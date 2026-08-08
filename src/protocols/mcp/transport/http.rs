use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use agentkit_mcp::{
    AuthOperation, ClientJsonRpcMessage, McpConnection, McpError, McpHandlerConfig, McpHttpClient,
    McpServerConfig, McpServerId, McpSse, McpSseError, McpSseStream, McpStreamableHttpError,
    McpStreamableHttpPostResponse, McpTransportBinding, StreamableHttpTransportConfig,
};
use bytes::Bytes;
use futures_util::{StreamExt, stream::BoxStream};
#[cfg(test)]
use http::header::AUTHORIZATION;
use http::{
    HeaderName, HeaderValue, StatusCode,
    header::{ACCEPT, CONTENT_TYPE, WWW_AUTHENTICATE},
};
use rmcp::model::ServerJsonRpcMessage;
use serde_json::Value;

use crate::{
    api::auth::contract::AuthenticatedPrincipal,
    capabilities::broker::{
        AuthResolution, BrokerError, BrokerInvocation,
        transport_auth::{
            self, TransportAuthChallenge, TransportAuthKind, TransportAuthState,
            TransportAuthorization, TransportBinding, TransportOperation,
        },
    },
    domain::{
        egress::{Authorization as EgressAuthorization, CredentialHandle, EgressPolicy},
        ids::{PrincipalId, ProjectId, WorkspaceId},
        secret::{SecretHandle, SecretLease},
    },
    protocols::mcp::egress::{
        EgressDialer, HttpCredentialBroker, HttpCredentialError, HttpSecretContext,
        McpEgressConnector, McpEgressLimits, McpEgressRequest, McpEgressResponse,
        McpResponseScanner,
    },
    store::sqlite::append::SqliteStore,
};

use super::{OperationGate, ReadyConnection, TransportError, TransportFailure, TransportLimits};
use crate::protocols::mcp::features::{ConfiguredServerIdentity, RawPayload};

const INITIALIZE_OPERATION: &str = "initialize";
const PROTOCOL_HEADER: &str = "mcp-protocol-version";
const PROTOCOL_REVISION_HEADER: &str = "2025-11-25";
const SESSION_HEADER: &str = "mcp-session-id";
const LAST_EVENT_ID_HEADER: &str = "last-event-id";
const JSON_CONTENT_TYPE: &str = "application/json";
const SSE_CONTENT_TYPE: &str = "text/event-stream";
type ActiveHttpLeases = BTreeMap<(String, String), Vec<(String, Arc<SecretLease>)>>;

pub(crate) struct EnvironmentHttpCredentialBroker {
    principal_id: PrincipalId,
    project_id: ProjectId,
    workspace_id: WorkspaceId,
    credentials: BTreeMap<SecretHandle, crate::protocols::mcp::config::McpCredentialScopeConfig>,
    callback_scanner: Option<Arc<crate::protocols::mcp::responders::CallbackSecretScanner>>,
    custody: Option<(crate::domain::secret::SecretCustody, String)>,
    lease_sequence: AtomicU64,
    active_leases: Mutex<ActiveHttpLeases>,
}

impl EnvironmentHttpCredentialBroker {
    pub(crate) fn new(
        principal_id: PrincipalId,
        project_id: ProjectId,
        workspace_id: WorkspaceId,
        credentials: BTreeMap<
            SecretHandle,
            crate::protocols::mcp::config::McpCredentialScopeConfig,
        >,
    ) -> Self {
        Self {
            principal_id,
            project_id,
            workspace_id,
            credentials,
            callback_scanner: None,
            custody: None,
            lease_sequence: AtomicU64::new(0),
            active_leases: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn with_callback_scanner(
        mut self,
        scanner: Arc<crate::protocols::mcp::responders::CallbackSecretScanner>,
    ) -> Self {
        self.callback_scanner = Some(scanner);
        self
    }

    pub(crate) fn with_custody(
        mut self,
        custody: crate::domain::secret::SecretCustody,
        owner: impl Into<String>,
    ) -> Self {
        self.custody = Some((custody, owner.into()));
        self
    }
}

#[async_trait::async_trait]
impl HttpCredentialBroker for EnvironmentHttpCredentialBroker {
    async fn authorize_and_resolve(
        &self,
        handle: &SecretHandle,
        context: &HttpSecretContext<'_>,
    ) -> Result<Arc<SecretLease>, HttpCredentialError> {
        if context.principal_id() != self.principal_id.to_string()
            || context.project_id() != self.project_id.to_string()
            || context.workspace_id() != self.workspace_id.to_string()
        {
            return Err(HttpCredentialError::Denied);
        }
        let scope = self
            .credentials
            .get(handle)
            .ok_or(HttpCredentialError::Denied)?;
        if matches!(
            scope,
            crate::protocols::mcp::config::McpCredentialScopeConfig::Workspace { workspace_id }
                if *workspace_id != self.workspace_id
        ) {
            return Err(HttpCredentialError::Denied);
        }
        let variable = handle
            .identifier()
            .strip_prefix("env:")
            .ok_or(HttpCredentialError::Denied)?;
        let value = std::env::var(variable).map_err(|_| HttpCredentialError::Unavailable)?;
        if value.is_empty() {
            return Err(HttpCredentialError::Invalid);
        }
        let lease = Arc::new(SecretLease::new(value.into_bytes()));
        if let Some(scanner) = &self.callback_scanner {
            scanner.add_secret(&lease);
        }
        if let Some((custody, owner)) = &self.custody {
            let source = format!(
                "http:{}:{}:{}",
                context.invocation_id(),
                handle.identifier(),
                self.lease_sequence.fetch_add(1, Ordering::Relaxed)
            );
            let mut active = self
                .active_leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            custody.register(owner.clone(), source.clone(), Arc::clone(&lease));
            active
                .entry((
                    context.invocation_id().to_owned(),
                    handle.identifier().to_owned(),
                ))
                .or_default()
                .push((source, Arc::clone(&lease)));
        }
        Ok(lease)
    }

    fn revoke(&self, invocation_id: &str, handle: &SecretHandle, lease: &Arc<SecretLease>) {
        if let Some((custody, owner)) = &self.custody {
            let key = (invocation_id.to_owned(), handle.identifier().to_owned());
            let mut active = self
                .active_leases
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(sources) = active.get_mut(&key) {
                if let Some(index) = sources
                    .iter()
                    .position(|(_, registered)| Arc::ptr_eq(registered, lease))
                {
                    let (source, _) = sources.remove(index);
                    custody.remove(owner, &source);
                }
                if sources.is_empty() {
                    active.remove(&key);
                }
            }
        }
    }
}

pub enum StreamableHttpOutcome {
    Ready(Box<ReadyConnection>),
    AuthRequired(Box<TransportAuthChallenge>),
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn connect_streamable_http_with_handler(
    server_id: McpServerId,
    endpoint: &str,
    request: &BrokerInvocation<'_>,
    policy: &EgressPolicy,
    credentials: Arc<dyn HttpCredentialBroker>,
    store: &mut SqliteStore,
    limits: TransportLimits,
    handler: McpHandlerConfig,
) -> Result<StreamableHttpOutcome, TransportError> {
    connect_streamable_http_with_handler_and_dialer(
        server_id,
        endpoint,
        request,
        policy,
        credentials,
        store,
        limits,
        handler,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn connect_streamable_http_with_handler_and_dialer(
    server_id: McpServerId,
    endpoint: &str,
    request: &BrokerInvocation<'_>,
    policy: &EgressPolicy,
    credentials: Arc<dyn HttpCredentialBroker>,
    store: &mut SqliteStore,
    limits: TransportLimits,
    handler: McpHandlerConfig,
    dialer: Option<Arc<dyn EgressDialer>>,
) -> Result<StreamableHttpOutcome, TransportError> {
    let deadline = tokio::time::Instant::now() + limits.request_timeout();
    super::validate_initialize_arguments(request)?;
    let endpoint = EgressPolicy::canonical_url(endpoint)
        .map_err(|_| TransportError::InvalidEndpoint)?
        .to_string();
    let operation = TransportOperation::parse(INITIALIZE_OPERATION)?;
    let binding = TransportBinding::new(request, server_id.to_string(), "http", &endpoint, None);
    let authorization = transport_auth::authorize(request, &operation, &binding, store)?;
    let credential = authorization
        .credential()
        .ok_or(TransportError::PolicyAuthorizationMismatch)?;
    let handle = CredentialHandle::new(credential.identifier().to_owned())
        .map_err(|_| TransportError::PolicyAuthorizationMismatch)?;
    let policy_authorization =
        tokio::time::timeout_at(deadline, policy.resolve_initial(&endpoint, &handle))
            .await
            .map_err(|_| TransportError::Timeout("HTTP initialize"))?
            .map_err(|_| TransportError::PolicyAuthorizationMismatch)?;
    validate_policy(&endpoint, &policy_authorization, &authorization, limits)?;
    match transport_auth::state(request, &binding, store) {
        Ok(TransportAuthState::Absent) | Err(BrokerError::AuthNotRequired) => {}
        Ok(TransportAuthState::Pending(challenge)) => {
            return Ok(StreamableHttpOutcome::AuthRequired(Box::new(challenge)));
        }
        Ok(TransportAuthState::Granted(_)) => return Err(BrokerError::ReplayNotAuthorized.into()),
        Ok(TransportAuthState::Denied) => return Err(BrokerError::AuthDenied.into()),
        Ok(TransportAuthState::Replayed) => return Err(BrokerError::ReplayPermitConsumed.into()),
        Err(error) => return Err(error.into()),
    }
    connect_once(
        server_id,
        &endpoint,
        request,
        authorization,
        policy.clone(),
        &policy_authorization,
        credentials,
        store,
        limits,
        false,
        handler,
        dialer,
        deadline,
    )
    .await
}

pub async fn connect_streamable_http(
    server_id: McpServerId,
    endpoint: &str,
    request: &BrokerInvocation<'_>,
    policy: &EgressPolicy,
    credentials: Arc<dyn HttpCredentialBroker>,
    store: &mut SqliteStore,
    limits: TransportLimits,
) -> Result<StreamableHttpOutcome, TransportError> {
    connect_streamable_http_with_handler(
        server_id,
        endpoint,
        request,
        policy,
        credentials,
        store,
        limits,
        McpHandlerConfig::new().with_events_capacity(limits.channel_capacity()),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn resume_streamable_http_with_handler(
    server_id: McpServerId,
    endpoint: &str,
    request: &BrokerInvocation<'_>,
    policy: &EgressPolicy,
    credentials: Arc<dyn HttpCredentialBroker>,
    store: &mut SqliteStore,
    limits: TransportLimits,
    handler: McpHandlerConfig,
) -> Result<ReadyConnection, TransportError> {
    let deadline = tokio::time::Instant::now() + limits.request_timeout();
    super::validate_initialize_arguments(request)?;
    let endpoint = EgressPolicy::canonical_url(endpoint)
        .map_err(|_| TransportError::InvalidEndpoint)?
        .to_string();
    let operation = TransportOperation::parse(INITIALIZE_OPERATION)?;
    let binding = TransportBinding::new(request, server_id.to_string(), "http", &endpoint, None);
    let authorization = transport_auth::authorize_replay(request, &operation, &binding, store)?;
    let credential = authorization
        .credential()
        .ok_or(TransportError::PolicyAuthorizationMismatch)?;
    let handle = CredentialHandle::new(credential.identifier().to_owned())
        .map_err(|_| TransportError::PolicyAuthorizationMismatch)?;
    let policy_authorization =
        tokio::time::timeout_at(deadline, policy.resolve_initial(&endpoint, &handle))
            .await
            .map_err(|_| TransportError::Timeout("HTTP initialize"))?
            .map_err(|_| TransportError::PolicyAuthorizationMismatch)?;
    validate_policy(&endpoint, &policy_authorization, &authorization, limits)?;
    match connect_once(
        server_id,
        &endpoint,
        request,
        authorization,
        policy.clone(),
        &policy_authorization,
        credentials,
        store,
        limits,
        true,
        handler,
        None,
        deadline,
    )
    .await?
    {
        StreamableHttpOutcome::Ready(connection) => Ok(*connection),
        StreamableHttpOutcome::AuthRequired(_) => Err(BrokerError::RepeatedAuthChallenge.into()),
    }
}

pub async fn resume_streamable_http(
    server_id: McpServerId,
    endpoint: &str,
    request: &BrokerInvocation<'_>,
    policy: &EgressPolicy,
    credentials: Arc<dyn HttpCredentialBroker>,
    store: &mut SqliteStore,
    limits: TransportLimits,
) -> Result<ReadyConnection, TransportError> {
    resume_streamable_http_with_handler(
        server_id,
        endpoint,
        request,
        policy,
        credentials,
        store,
        limits,
        McpHandlerConfig::new().with_events_capacity(limits.channel_capacity()),
    )
    .await
}

pub fn resolve_streamable_http_auth(
    server_id: &McpServerId,
    endpoint: &str,
    request: &BrokerInvocation<'_>,
    actor: &AuthenticatedPrincipal,
    resolution: AuthResolution,
    store: &mut SqliteStore,
) -> Result<(), TransportError> {
    let endpoint = EgressPolicy::canonical_url(endpoint)
        .map_err(|_| TransportError::InvalidEndpoint)?
        .to_string();
    transport_auth::resume(
        request,
        actor,
        &server_id.to_string(),
        "http",
        &endpoint,
        resolution,
        store,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn connect_once(
    server_id: McpServerId,
    endpoint: &str,
    request: &BrokerInvocation<'_>,
    authorization: TransportAuthorization,
    egress_policy: EgressPolicy,
    policy: &EgressAuthorization,
    credentials: Arc<dyn HttpCredentialBroker>,
    store: &mut SqliteStore,
    limits: TransportLimits,
    replay: bool,
    handler: McpHandlerConfig,
    dialer: Option<Arc<dyn EgressDialer>>,
    deadline: tokio::time::Instant,
) -> Result<StreamableHttpOutcome, TransportError> {
    let configured_server = ConfiguredServerIdentity::new(server_id.to_string())?;
    let operations = OperationGate::new();
    operations.set_binding(authorization.binding().clone())?;
    let generation = operations.install(authorization.clone())?;
    let dispatch_binding = authorization.binding().clone();
    let client = Arc::new(AuthorizedHttpClient::new(
        &server_id,
        endpoint,
        authorization,
        egress_policy,
        policy,
        request.workspace_id(),
        Arc::clone(&operations),
        credentials,
        limits,
        dialer,
        deadline,
    )?);
    let binding = StreamableHttpTransportConfig::new(endpoint)
        .with_http_client(client.clone())
        .with_max_sse_reconnects(limits.max_sse_reconnects())
        .with_channel_buffer_capacity(limits.channel_capacity())
        .map_err(TransportError::from)?;
    let config = McpServerConfig::new(
        server_id.to_string(),
        McpTransportBinding::StreamableHttp(binding),
    );
    let operation = TransportOperation::parse(INITIALIZE_OPERATION)?;
    let dispatch =
        match transport_auth::begin_dispatch(request, &operation, &dispatch_binding, replay, store)
        {
            Ok(dispatch) => dispatch,
            Err(error) => {
                operations.clear_generation(generation)?;
                return Err(error.into());
            }
        };
    let result = match tokio::time::timeout_at(
        deadline,
        McpConnection::connect_authorized_http(&config, handler),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            let cleanup =
                tokio::time::timeout(limits.close_timeout(), client.close_open_sessions()).await;
            let persisted = transport_auth::finish_dispatch(
                request,
                dispatch,
                transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                store,
            );
            let cleared = operations.clear_generation(generation);
            persisted?;
            cleared?;
            let primary = TransportError::Timeout("HTTP initialize");
            return match cleanup {
                Ok(Ok(())) => Err(primary),
                Ok(Err(error)) => Err(TransportError::Cleanup {
                    primary: Box::new(primary),
                    cleanup: std::io::Error::other(error),
                }),
                Err(_) => Err(TransportError::Cleanup {
                    primary: Box::new(primary),
                    cleanup: std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "MCP HTTP session cleanup timed out",
                    ),
                }),
            };
        }
    };
    if request.cancelled() {
        let cleanup =
            tokio::time::timeout(limits.close_timeout(), client.close_open_sessions()).await;
        let persisted = transport_auth::finish_dispatch(
            request,
            dispatch,
            transport_auth::TransportDispatchOutcome::OutcomeUnknown,
            store,
        );
        let cleared = operations.clear_generation(generation);
        persisted?;
        cleared?;
        return match cleanup {
            Ok(Ok(())) => Err(TransportError::Cancelled),
            Ok(Err(error)) => Err(TransportError::Cleanup {
                primary: Box::new(TransportError::Cancelled),
                cleanup: std::io::Error::other(error),
            }),
            Err(_) => Err(TransportError::Cleanup {
                primary: Box::new(TransportError::Cancelled),
                cleanup: std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "MCP HTTP session cleanup timed out",
                ),
            }),
        };
    }
    if result.is_err()
        && let Some(error) = operations.take_failure()
    {
        let cleanup =
            tokio::time::timeout(limits.close_timeout(), client.close_open_sessions()).await;
        let persisted = transport_auth::finish_dispatch(
            request,
            dispatch,
            transport_auth::TransportDispatchOutcome::OutcomeUnknown,
            store,
        );
        let cleared = operations.clear_generation(generation);
        persisted?;
        cleared?;
        return match cleanup {
            Ok(Ok(())) => Err(error),
            Ok(Err(cleanup)) => Err(TransportError::Cleanup {
                primary: Box::new(error),
                cleanup: std::io::Error::other(cleanup),
            }),
            Err(_) => Err(TransportError::Cleanup {
                primary: Box::new(error),
                cleanup: std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "MCP HTTP session cleanup timed out",
                ),
            }),
        };
    }
    match result {
        Ok(connection) => {
            let ready = (|| {
                transport_auth::finish_dispatch(
                    request,
                    dispatch,
                    transport_auth::TransportDispatchOutcome::Completed,
                    store,
                )?;
                let authorization = operations.current_authorization()?;
                operations.bind_connection(authorization)?;
                operations.clear_generation(generation)?;
                ReadyConnection::new(
                    connection,
                    configured_server,
                    limits,
                    Arc::clone(&operations),
                    Some(client.clone()),
                    None,
                    true,
                )
                .map(|connection| connection.with_lifecycle_authority(request))
                .map(Box::new)
                .map(StreamableHttpOutcome::Ready)
            })();
            match ready {
                Ok(connection) => Ok(connection),
                Err(primary) => {
                    let _ = operations.clear_generation(generation);
                    Err(cleanup_http_error(client, limits.close_timeout(), primary).await)
                }
            }
        }
        Err(McpError::AuthRequired(challenge)) => {
            let (kind, operation, scope) = auth_challenge(&challenge)?;
            let challenge = transport_auth::interrupt_dispatch(
                request,
                dispatch,
                kind,
                &operation,
                scope.as_deref(),
                store,
            )?;
            operations.clear_generation(generation)?;
            Ok(StreamableHttpOutcome::AuthRequired(Box::new(challenge)))
        }
        Err(error) => {
            let primary = TransportError::from(error);
            let cleanup =
                tokio::time::timeout(limits.close_timeout(), client.close_open_sessions()).await;
            let persisted = transport_auth::finish_dispatch(
                request,
                dispatch,
                transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                store,
            );
            let cleared = operations.clear_generation(generation);
            persisted?;
            cleared?;
            match cleanup {
                Ok(Ok(())) => Err(primary),
                Ok(Err(error)) => Err(TransportError::Cleanup {
                    primary: Box::new(primary),
                    cleanup: std::io::Error::other(error),
                }),
                Err(_) => Err(TransportError::Cleanup {
                    primary: Box::new(primary),
                    cleanup: std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "MCP HTTP session cleanup timed out",
                    ),
                }),
            }
        }
    }
}

async fn cleanup_http_error(
    client: Arc<AuthorizedHttpClient>,
    timeout: std::time::Duration,
    primary: TransportError,
) -> TransportError {
    match tokio::time::timeout(timeout, client.close_open_sessions()).await {
        Ok(Ok(())) => primary,
        Ok(Err(error)) => TransportError::Cleanup {
            primary: Box::new(primary),
            cleanup: std::io::Error::other(error),
        },
        Err(_) => TransportError::Cleanup {
            primary: Box::new(primary),
            cleanup: std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "MCP HTTP session cleanup timed out",
            ),
        },
    }
}

pub(super) fn auth_challenge(
    challenge: &agentkit_mcp::AuthRequest,
) -> Result<(TransportAuthKind, TransportOperation, Option<String>), TransportError> {
    let method = match &challenge.operation {
        AuthOperation::McpConnect { .. } => INITIALIZE_OPERATION,
        AuthOperation::McpToolCall { .. } => "tools/call",
        AuthOperation::McpResourceRead { .. } => "resources/read",
        AuthOperation::McpPromptGet { .. } => "prompts/get",
        AuthOperation::McpOther { method, .. } => method,
    };
    if challenge
        .challenge
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(|metadata| metadata != method)
    {
        return Err(BrokerError::InvalidTransportOperation.into());
    }
    let operation = TransportOperation::parse(method)?;
    let insufficient = challenge
        .challenge
        .get("insufficient_scope")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let scope = challenge
        .challenge
        .get("required_scope")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            challenge
                .challenge
                .get("www_authenticate")
                .and_then(Value::as_str)
                .and_then(extract_scope)
        });
    Ok((
        if insufficient {
            TransportAuthKind::Forbidden
        } else {
            TransportAuthKind::Unauthorized
        },
        operation,
        scope,
    ))
}

fn extract_scope(header: &str) -> Option<String> {
    let start = header.to_ascii_lowercase().find("scope=")? + "scope=".len();
    let rest = &header[start..];
    if let Some(rest) = rest.strip_prefix('"') {
        return rest.split_once('"').map(|(scope, _)| scope.to_owned());
    }
    let end = rest
        .find(|character: char| character == ',' || character == ';' || character.is_whitespace())
        .unwrap_or(rest.len());
    (end > 0).then(|| rest[..end].to_owned())
}

fn validate_policy(
    endpoint: &str,
    policy: &EgressAuthorization,
    authorization: &TransportAuthorization,
    limits: TransportLimits,
) -> Result<(), TransportError> {
    if endpoint.is_empty() || endpoint.len() > limits.max_header_bytes() {
        return Err(TransportError::InvalidEndpoint);
    }
    let parsed = url::Url::parse(endpoint).map_err(|_| TransportError::InvalidEndpoint)?;
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return Err(TransportError::InvalidEndpoint);
    }
    let host = parsed.host_str().ok_or(TransportError::InvalidEndpoint)?;
    if parsed.scheme() != "https" && !insecure_test_endpoint(&parsed) {
        return Err(TransportError::InvalidEndpoint);
    }
    let port = parsed
        .port_or_known_default()
        .ok_or(TransportError::InvalidEndpoint)?;
    let egress = authorization
        .egress()
        .ok_or(TransportError::PolicyAuthorizationMismatch)?;
    let credential = authorization
        .credential()
        .ok_or(TransportError::PolicyAuthorizationMismatch)?;
    let scheme_matches = match egress.scheme() {
        crate::domain::egress::Scheme::Http => parsed.scheme() == "http",
        crate::domain::egress::Scheme::Https => parsed.scheme() == "https",
    };
    let destination = policy.destination();
    if !scheme_matches
        || !host.eq_ignore_ascii_case(egress.host())
        || port != egress.port()
        || destination.scheme() != egress.scheme()
        || !destination.host().eq_ignore_ascii_case(egress.host())
        || destination.port() != egress.port()
        || policy.credential().as_str() != credential.identifier()
        || authorization.operation().as_str() != INITIALIZE_OPERATION
    {
        return Err(TransportError::PolicyAuthorizationMismatch);
    }
    Ok(())
}

#[cfg(test)]
fn insecure_test_endpoint(endpoint: &url::Url) -> bool {
    endpoint.scheme() == "http"
        && endpoint
            .host()
            .is_some_and(|host| matches!(host, url::Host::Ipv4(ip) if ip.is_loopback()))
}

#[cfg(not(test))]
const fn insecure_test_endpoint(_: &url::Url) -> bool {
    false
}

struct AuthorizedHttpClient {
    authorization: TransportAuthorization,
    endpoint: String,
    connector: McpEgressConnector,
    limits: TransportLimits,
    sessions: Mutex<Option<String>>,
    cleanup_sessions: Mutex<BTreeSet<String>>,
    operations: Arc<OperationGate>,
    server_id: McpServerId,
    workspace_id: String,
    initial_deadline: Mutex<Option<tokio::time::Instant>>,
}

impl AuthorizedHttpClient {
    #[allow(clippy::too_many_arguments)]
    fn new(
        server_id: &McpServerId,
        endpoint: &str,
        authorization: TransportAuthorization,
        egress_policy: EgressPolicy,
        policy: &EgressAuthorization,
        workspace_id: WorkspaceId,
        operations: Arc<OperationGate>,
        credentials: Arc<dyn HttpCredentialBroker>,
        limits: TransportLimits,
        dialer: Option<Arc<dyn EgressDialer>>,
        initial_deadline: tokio::time::Instant,
    ) -> Result<Self, TransportError> {
        if policy.resolved_addresses().next().is_none() {
            return Err(TransportError::PolicyAuthorizationMismatch);
        }
        let egress_limits = McpEgressLimits {
            max_location_bytes: limits.max_header_bytes(),
            max_headers: limits.max_headers(),
            max_header_bytes: limits.max_header_bytes(),
            request_timeout: limits.request_timeout(),
            connect_timeout: limits.connect_timeout(),
        };
        let connector = if let Some(dialer) = dialer {
            McpEgressConnector::with_dialer(egress_policy, credentials, dialer, egress_limits)
        } else {
            McpEgressConnector::new(egress_policy, credentials, egress_limits)
        }
        .with_initial_authorization(policy.clone());
        Ok(Self {
            authorization,
            endpoint: endpoint.to_owned(),
            connector,
            limits,
            sessions: Mutex::new(None),
            cleanup_sessions: Mutex::new(BTreeSet::new()),
            operations,
            server_id: server_id.clone(),
            workspace_id: workspace_id.to_string(),
            initial_deadline: Mutex::new(Some(initial_deadline)),
        })
    }

    fn check_uri(&self, uri: &str) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
        if uri == self.endpoint {
            Ok(())
        } else {
            Err(unexpected("HTTP endpoint changed after authorization"))
        }
    }

    fn check_response_headers(
        &self,
        response: &reqwest::Response,
    ) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
        if crate::protocols::mcp::egress::check_headers(
            response.headers(),
            McpEgressLimits {
                max_location_bytes: self.limits.max_header_bytes(),
                max_headers: self.limits.max_headers(),
                max_header_bytes: self.limits.max_header_bytes(),
                request_timeout: self.limits.request_timeout(),
                connect_timeout: self.limits.connect_timeout(),
            },
        )
        .is_err()
        {
            self.operations.fail(TransportFailure::InvalidHeader);
            return Err(unexpected("MCP HTTP response headers exceed bound"));
        }
        Ok(())
    }

    fn expire_session(&self, expired: &str) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
        let mut established = self
            .sessions
            .lock()
            .map_err(|_| unexpected("MCP session state is unavailable"))?;
        if established.as_deref() == Some(expired) {
            *established = None;
        }
        self.cleanup_sessions
            .lock()
            .map_err(|_| unexpected("MCP session cleanup state is unavailable"))?
            .remove(expired);
        Ok(())
    }

    fn bind_session(
        &self,
        session: Option<String>,
    ) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
        self.operations
            .set_binding(self.authorization.binding().with_session(session))
            .map_err(|_| unexpected("MCP session binding is unavailable"))
    }

    fn headers(
        &self,
        mut headers: http::HeaderMap,
        mut custom: HashMap<HeaderName, HeaderValue>,
        require_protocol: bool,
    ) -> Result<http::HeaderMap, McpStreamableHttpError<reqwest::Error>> {
        let protocol = custom.remove(&HeaderName::from_static(PROTOCOL_HEADER));
        if require_protocol {
            let protocol = protocol.ok_or_else(|| {
                self.operations.fail(TransportFailure::InvalidHeader);
                unexpected("missing MCP protocol header")
            })?;
            if protocol != HeaderValue::from_static(PROTOCOL_REVISION_HEADER) {
                self.operations.fail(TransportFailure::InvalidHeader);
                return Err(unexpected("invalid MCP protocol header"));
            }
            headers.insert(HeaderName::from_static(PROTOCOL_HEADER), protocol);
        } else if protocol.is_some() {
            self.operations.fail(TransportFailure::InvalidHeader);
            return Err(unexpected("initialize request carried a protocol header"));
        }
        let mut bytes = 0usize;
        for (name, value) in custom {
            bytes = bytes
                .checked_add(name.as_str().len() + value.as_bytes().len())
                .ok_or_else(|| unexpected("MCP HTTP headers exceed bound"))?;
            if bytes > self.limits.max_header_bytes()
                || matches!(
                    name.as_str(),
                    "accept"
                        | "authorization"
                        | "cookie"
                        | "proxy-authorization"
                        | SESSION_HEADER
                        | LAST_EVENT_ID_HEADER
                )
            {
                self.operations.fail(TransportFailure::InvalidHeader);
                return Err(unexpected("reserved or oversized MCP HTTP header"));
            }
            headers.insert(name, value);
        }
        Ok(headers)
    }

    async fn send(
        &self,
        method: http::Method,
        url: &str,
        headers: http::HeaderMap,
        body: Bytes,
        authorization: &TransportAuthorization,
    ) -> Result<McpEgressResponse, McpStreamableHttpError<reqwest::Error>> {
        let deadline = self
            .initial_deadline
            .lock()
            .map_err(|_| unexpected("MCP HTTP deadline state is unavailable"))?
            .take()
            .unwrap_or_else(|| tokio::time::Instant::now() + self.limits.request_timeout());
        let response = self
            .connector
            .execute_before(
                McpEgressRequest {
                    method,
                    url: url.to_owned(),
                    headers,
                    body,
                },
                authorization.principal_id(),
                authorization.project_id(),
                &self.workspace_id,
                authorization.invocation_id(),
                authorization.decision_digest(),
                authorization.request_digest(),
                authorization.scope(),
                authorization.operation().as_str(),
                deadline,
            )
            .await
            .map_err(|error| {
                self.operations.fail(match error {
                    crate::protocols::mcp::egress::McpEgressError::InvalidHeader => {
                        TransportFailure::InvalidHeader
                    }
                    crate::protocols::mcp::egress::McpEgressError::Credential(error) => {
                        TransportFailure::Credential(error)
                    }
                    crate::protocols::mcp::egress::McpEgressError::Timeout => {
                        TransportFailure::HttpTimeout
                    }
                    error => TransportFailure::Egress(error),
                });
                unexpected("MCP egress connector denied request")
            })?;
        if response.redirects() > 0 && response.response().headers().contains_key(SESSION_HEADER) {
            self.operations.fail(TransportFailure::InvalidHeader);
            return Err(unexpected(
                "redirected MCP response carried session authority",
            ));
        }
        Ok(response)
    }

    fn check_authorization(
        &self,
        authorization: &TransportAuthorization,
        session_id: Option<&str>,
    ) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
        let current_session = self
            .sessions
            .lock()
            .map_err(|_| unexpected("MCP session state is unavailable"))?
            .clone();
        if authorization.principal_id() != self.authorization.principal_id()
            || authorization.project_id() != self.authorization.project_id()
            || authorization.credential() != self.authorization.credential()
            || authorization.egress() != self.authorization.egress()
            || session_id != current_session.as_deref()
        {
            return Err(unexpected("MCP operation authorization context changed"));
        }
        Ok(())
    }

    fn session(&self, value: &str) -> Result<HeaderValue, McpStreamableHttpError<reqwest::Error>> {
        if value.is_empty()
            || value.len() > self.limits.max_session_id_bytes()
            || value.bytes().any(|byte| !byte.is_ascii_graphic())
        {
            return Err(unexpected("invalid MCP session id"));
        }
        HeaderValue::from_str(value).map_err(|_| unexpected("invalid MCP session id"))
    }

    fn event_id(&self, value: &str) -> Result<HeaderValue, McpStreamableHttpError<reqwest::Error>> {
        if value.is_empty() || value.len() > self.limits.max_event_id_bytes() {
            return Err(unexpected("invalid MCP resume event id"));
        }
        HeaderValue::from_str(value).map_err(|_| unexpected("invalid MCP resume event id"))
    }
}

#[async_trait::async_trait]
impl McpHttpClient for AuthorizedHttpClient {
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<McpStreamableHttpPostResponse, McpStreamableHttpError<reqwest::Error>> {
        self.check_uri(&uri)?;
        let authorization = self
            .operations
            .authorize_message(&message)
            .map_err(|_| unexpected("MCP operation was not broker-authorized"))?;
        self.check_authorization(&authorization, session_id.as_deref())?;
        let is_initialize = matches!(
            &message,
            ClientJsonRpcMessage::Request(request)
                if matches!(
                    &request.request,
                    rmcp::model::ClientRequest::InitializeRequest(_)
                )
        );
        let body = serde_json::to_vec(&message)?;
        if body.len() > self.limits.max_json_bytes() {
            self.operations.fail(TransportFailure::ResponseTooLarge);
            return Err(unexpected("outbound MCP JSON exceeds bound"));
        }
        if agentkit_mcp::has_responder_delivery_permit(&message) {
            let value = serde_json::to_value(&message)?;
            self.operations
                .scan_callback_response(&value, &body)
                .map_err(|_| unexpected("MCP callback response contains credential material"))?;
        }
        let mut headers = http::HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(JSON_CONTENT_TYPE));
        headers = self.headers(headers, custom_headers, !is_initialize)?;
        if let Some(session_id) = &session_id {
            headers.insert(
                HeaderName::from_static(SESSION_HEADER),
                self.session(session_id)?,
            );
        }
        let response = self
            .send(
                http::Method::POST,
                uri.as_ref(),
                headers,
                Bytes::from(body),
                &authorization,
            )
            .await?;
        self.check_response_headers(response.response())?;
        response_auth_error(response.response(), self.limits)?;
        let deadline = response.deadline();
        let status = response.status();
        let is_request = matches!(&message, ClientJsonRpcMessage::Request(_));
        if status == StatusCode::NOT_FOUND && session_id.is_some() {
            let expired = session_id.as_deref().expect("checked above");
            self.expire_session(expired)?;
            self.bind_session(None)?;
            self.operations.fail(TransportFailure::SessionExpired);
            return Err(McpStreamableHttpError::SessionExpired);
        }
        let content_type = content_type(&response);
        let response_session = response_session(&response, self.limits)?;
        if is_initialize && let Some(session) = &response_session {
            self.cleanup_sessions
                .lock()
                .map_err(|_| unexpected("MCP session cleanup state is unavailable"))?
                .insert(session.clone());
        }
        if let Some(session) = &response_session {
            let mut established = self
                .sessions
                .lock()
                .map_err(|_| unexpected("MCP session state is unavailable"))?;
            match established.as_deref() {
                None if is_initialize => {
                    *established = Some(session.clone());
                    self.bind_session(Some(session.clone()))?;
                }
                Some(current) if !is_initialize && current == session => {}
                _ => return Err(unexpected("conflicting MCP session id")),
            }
        }
        if !is_request {
            if status != StatusCode::ACCEPTED
                || content_type.is_some()
                || !bounded_body(
                    response,
                    self.limits.max_json_bytes(),
                    &self.operations,
                    deadline,
                )
                .await?
                .bytes
                .is_empty()
            {
                return Err(unexpected(
                    "MCP notification/response requires an empty 202 acknowledgement",
                ));
            }
            return Ok(McpStreamableHttpPostResponse::Accepted);
        }
        if !status.is_success() {
            let _ = bounded_body(
                response,
                self.limits.max_json_bytes(),
                &self.operations,
                deadline,
            )
            .await?;
            return Err(unexpected("MCP HTTP request failed"));
        }
        match content_type.as_deref().and_then(parse_media_type) {
            Some(JSON_CONTENT_TYPE) => {
                let body = bounded_body(
                    response,
                    self.limits.max_json_bytes(),
                    &self.operations,
                    deadline,
                )
                .await?;
                let payload = RawPayload::parse(&body.bytes, self.limits.payload_limits())
                    .map_err(|error| {
                        self.operations.fail(TransportFailure::Payload(error));
                        unexpected("invalid bounded MCP JSON response")
                    })?;
                scan_canonical(&body.scanner, &payload, &self.operations)?;
                self.operations
                    .bind_callback_scanner(&payload, Arc::clone(&body.scanner))
                    .map_err(|_| unexpected("conflicting MCP callback request"))?;
                if payload.value().get("result").is_some()
                    || super::is_terminal_url_elicitation(&payload)
                {
                    self.operations
                        .capture_payload(payload.clone())
                        .map_err(|_| unexpected("conflicting MCP response payload"))?;
                }
                let message = serde_json::from_value(payload.value().clone())?;
                if is_initialize
                    && !matches!(
                        &message,
                        ServerJsonRpcMessage::Response(response)
                            if matches!(response.result, rmcp::model::ServerResult::InitializeResult(_))
                    )
                {
                    return Err(unexpected("initialize did not return InitializeResult"));
                }
                Ok(McpStreamableHttpPostResponse::Json(
                    message,
                    response_session,
                ))
            }
            Some(SSE_CONTENT_TYPE) => {
                let mut stream = bounded_sse(
                    response,
                    self.limits,
                    Arc::clone(&self.operations),
                    self.server_id.clone(),
                );
                if is_initialize {
                    let first = tokio::time::timeout_at(deadline, stream.next())
                        .await
                        .map_err(|_| {
                            self.operations.fail(TransportFailure::HttpTimeout);
                            unexpected("MCP HTTP initialize timed out")
                        })?
                        .ok_or_else(|| unexpected("initialize SSE stream ended without a result"))?
                        .map_err(|_| unexpected("initialize SSE stream failed"))?;
                    stream = futures_util::stream::once(async move { Ok(first) })
                        .chain(stream)
                        .boxed();
                }
                Ok(McpStreamableHttpPostResponse::Sse(stream, response_session))
            }
            _ => Err(McpStreamableHttpError::UnexpectedContentType(content_type)),
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
        self.check_uri(&uri)?;
        let authorization = self
            .operations
            .current_authorization()
            .map_err(|_| unexpected("MCP session deletion was not broker-authorized"))?;
        self.check_authorization(&authorization, Some(session_id.as_ref()))?;
        let mut headers = http::HeaderMap::new();
        headers.insert(
            HeaderName::from_static(SESSION_HEADER),
            self.session(&session_id)?,
        );
        headers = self.headers(headers, custom_headers, true)?;
        let response = self
            .send(
                http::Method::DELETE,
                uri.as_ref(),
                headers,
                Bytes::new(),
                &authorization,
            )
            .await?;
        self.check_response_headers(response.response())?;
        response_auth_error(response.response(), self.limits)?;
        let deadline = response.deadline();
        if response.status() == StatusCode::NOT_FOUND {
            self.expire_session(&session_id)?;
            self.bind_session(None)?;
            self.operations.fail(TransportFailure::SessionExpired);
            return Err(McpStreamableHttpError::SessionExpired);
        }
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            let mut session = self
                .sessions
                .lock()
                .map_err(|_| unexpected("MCP session state is unavailable"))?;
            if session.as_deref() == Some(session_id.as_ref()) {
                *session = None;
            }
            self.cleanup_sessions
                .lock()
                .map_err(|_| unexpected("MCP session cleanup state is unavailable"))?
                .remove(session_id.as_ref());
            self.bind_session(None)?;
            return Ok(());
        }
        if !response.status().is_success() {
            return Err(unexpected("MCP session deletion failed"));
        }
        let _ = bounded_body(
            response,
            self.limits.max_json_bytes(),
            &self.operations,
            deadline,
        )
        .await?;
        let mut session = self
            .sessions
            .lock()
            .map_err(|_| unexpected("MCP session state is unavailable"))?;
        if session.as_deref() == Some(session_id.as_ref()) {
            *session = None;
        }
        self.cleanup_sessions
            .lock()
            .map_err(|_| unexpected("MCP session cleanup state is unavailable"))?
            .remove(session_id.as_ref());
        self.bind_session(None)?;
        Ok(())
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<McpSseStream, McpStreamableHttpError<reqwest::Error>> {
        self.check_uri(&uri)?;
        let authorization = self
            .operations
            .current_authorization()
            .map_err(|_| unexpected("MCP SSE reconnect was not broker-authorized"))?;
        self.check_authorization(&authorization, Some(session_id.as_ref()))?;
        let mut headers = http::HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static(SSE_CONTENT_TYPE));
        headers.insert(
            HeaderName::from_static(SESSION_HEADER),
            self.session(&session_id)?,
        );
        if let Some(event_id) = last_event_id.filter(|id| !id.is_empty()) {
            headers.insert(
                HeaderName::from_static(LAST_EVENT_ID_HEADER),
                self.event_id(&event_id)?,
            );
        }
        headers = self.headers(headers, custom_headers, true)?;
        let response = self
            .send(
                http::Method::GET,
                uri.as_ref(),
                headers,
                Bytes::new(),
                &authorization,
            )
            .await?;
        self.check_response_headers(response.response())?;
        response_auth_error(response.response(), self.limits)?;
        if response.status() == StatusCode::NOT_FOUND {
            self.expire_session(&session_id)?;
            self.bind_session(None)?;
            self.operations.fail(TransportFailure::SessionExpired);
            return Err(McpStreamableHttpError::SessionExpired);
        }
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(McpStreamableHttpError::ServerDoesNotSupportSse);
        }
        if !response.status().is_success() {
            return Err(unexpected("MCP SSE request failed"));
        }
        match content_type(&response)
            .as_deref()
            .and_then(parse_media_type)
        {
            Some(SSE_CONTENT_TYPE) => Ok(bounded_sse(
                response,
                self.limits,
                Arc::clone(&self.operations),
                self.server_id.clone(),
            )),
            other => Err(McpStreamableHttpError::UnexpectedContentType(
                other.map(str::to_owned),
            )),
        }
    }

    async fn close_open_sessions(&self) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
        loop {
            let Some(session) = self
                .cleanup_sessions
                .lock()
                .map_err(|_| unexpected("MCP session cleanup state is unavailable"))?
                .iter()
                .next()
                .cloned()
            else {
                return Ok(());
            };
            let mut headers = HashMap::new();
            headers.insert(
                HeaderName::from_static(PROTOCOL_HEADER),
                HeaderValue::from_static(PROTOCOL_REVISION_HEADER),
            );
            let authorization = self
                .operations
                .current_authorization()
                .map_err(|_| unexpected("MCP session cleanup was not broker-authorized"))?;
            self.check_authorization(&authorization, Some(&session))?;
            let mut request_headers = http::HeaderMap::new();
            request_headers.insert(
                HeaderName::from_static(SESSION_HEADER),
                self.session(&session)?,
            );
            request_headers = self.headers(request_headers, headers, true)?;
            let response = self
                .send(
                    http::Method::DELETE,
                    &self.endpoint,
                    request_headers,
                    Bytes::new(),
                    &authorization,
                )
                .await?;
            self.check_response_headers(response.response())?;
            response_auth_error(response.response(), self.limits)?;
            let deadline = response.deadline();
            if response.status() == StatusCode::NOT_FOUND {
                self.expire_session(&session)?;
                self.bind_session(None)?;
                return Err(McpStreamableHttpError::SessionExpired);
            }
            if response.status() != StatusCode::METHOD_NOT_ALLOWED
                && !response.status().is_success()
            {
                return Err(unexpected("MCP failed-initialize session cleanup failed"));
            }
            let _ = bounded_body(
                response,
                self.limits.max_json_bytes(),
                &self.operations,
                deadline,
            )
            .await?;
            *self
                .sessions
                .lock()
                .map_err(|_| unexpected("MCP session state is unavailable"))? = None;
            self.cleanup_sessions
                .lock()
                .map_err(|_| unexpected("MCP session cleanup state is unavailable"))?
                .remove(&session);
            self.bind_session(None)?;
        }
    }
}

fn response_auth_error(
    response: &reqwest::Response,
    limits: TransportLimits,
) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
    if !matches!(
        response.status(),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        return Ok(());
    }
    let challenge = response
        .headers()
        .get(WWW_AUTHENTICATE)
        .map(|value| value.to_str().map(str::to_owned))
        .transpose()
        .map_err(|_| unexpected("invalid WWW-Authenticate header"))?
        .unwrap_or_else(|| "Bearer".to_owned());
    if challenge.len() > limits.max_header_bytes() {
        return Err(unexpected("WWW-Authenticate header exceeds bound"));
    }
    if response.status() == StatusCode::UNAUTHORIZED {
        Err(McpStreamableHttpError::AuthRequired(
            rmcp::transport::streamable_http_client::AuthRequiredError::new(challenge),
        ))
    } else {
        let scope = extract_scope(&challenge);
        Err(McpStreamableHttpError::InsufficientScope(
            rmcp::transport::streamable_http_client::InsufficientScopeError::new(challenge, scope),
        ))
    }
}

fn content_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn parse_media_type(value: &str) -> Option<&str> {
    let (media_type, mut parameters) = value.split_once(';').unwrap_or((value, ""));
    let media_type = media_type.trim();
    if !media_type.eq_ignore_ascii_case(JSON_CONTENT_TYPE)
        && !media_type.eq_ignore_ascii_case(SSE_CONTENT_TYPE)
    {
        return None;
    }
    while !parameters.trim().is_empty() {
        parameters = parameters.trim_start();
        let name_end = parameters.bytes().position(|byte| !is_token(byte))?;
        if name_end == 0 {
            return None;
        }
        parameters = parameters[name_end..].trim_start();
        parameters = parameters.strip_prefix('=')?.trim_start();
        if let Some(rest) = parameters.strip_prefix('"') {
            let mut escaped = false;
            let mut end = None;
            for (index, character) in rest.char_indices() {
                if matches!(character, '\r' | '\n') {
                    return None;
                }
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    end = Some(index + 1);
                    break;
                }
            }
            parameters = &rest[end?..];
        } else {
            let end = parameters
                .bytes()
                .position(|byte| !is_token(byte))
                .unwrap_or(parameters.len());
            if end == 0 {
                return None;
            }
            parameters = &parameters[end..];
        }
        parameters = parameters.trim_start();
        if parameters.is_empty() {
            break;
        }
        parameters = parameters.strip_prefix(';')?;
    }
    if media_type.eq_ignore_ascii_case(JSON_CONTENT_TYPE) {
        Some(JSON_CONTENT_TYPE)
    } else {
        Some(SSE_CONTENT_TYPE)
    }
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn response_session(
    response: &reqwest::Response,
    limits: TransportLimits,
) -> Result<Option<String>, McpStreamableHttpError<reqwest::Error>> {
    response
        .headers()
        .get(SESSION_HEADER)
        .map(|value| {
            let value = value
                .to_str()
                .map_err(|_| unexpected("invalid MCP session id"))?;
            if value.is_empty()
                || value.len() > limits.max_session_id_bytes()
                || value.bytes().any(|byte| !byte.is_ascii_graphic())
            {
                return Err(unexpected("invalid MCP session id"));
            }
            Ok(value.to_owned())
        })
        .transpose()
}

async fn bounded_body(
    response: McpEgressResponse,
    max: usize,
    operations: &OperationGate,
    deadline: tokio::time::Instant,
) -> Result<ScannedBody, McpStreamableHttpError<reqwest::Error>> {
    if response
        .content_length()
        .is_some_and(|length| length > max as u64)
    {
        operations.fail(TransportFailure::ResponseTooLarge);
        return Err(unexpected("MCP HTTP body exceeds bound"));
    }
    let (response, _, scanner) = response.into_parts();
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = tokio::time::timeout_at(deadline, stream.next())
        .await
        .map_err(|_| {
            operations.fail(TransportFailure::HttpTimeout);
            unexpected("MCP HTTP body timed out")
        })?
    {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > max {
            operations.fail(TransportFailure::ResponseTooLarge);
            return Err(unexpected("MCP HTTP body exceeds bound"));
        }
        scan_ingress(&scanner, &chunk, operations)?;
        body.extend_from_slice(&chunk);
    }
    Ok(ScannedBody {
        bytes: body,
        scanner,
    })
}

struct ScannedBody {
    bytes: Vec<u8>,
    scanner: Arc<McpResponseScanner>,
}

fn scan_ingress(
    scanner: &McpResponseScanner,
    bytes: &[u8],
    operations: &OperationGate,
) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
    if scanner
        .scan_ingress(bytes)
        .map_err(|_| unexpected("credential scanner is unavailable"))?
    {
        operations.fail(TransportFailure::SensitivePayload);
        Err(unexpected("MCP HTTP response contains credential material"))
    } else {
        Ok(())
    }
}

fn scan_canonical(
    scanner: &McpResponseScanner,
    payload: &RawPayload,
    operations: &OperationGate,
) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
    if scanner
        .scan_canonical(payload.canonical_bytes())
        .map_err(|_| unexpected("credential scanner is unavailable"))?
    {
        operations.fail(TransportFailure::SensitivePayload);
        Err(unexpected("MCP HTTP response contains credential material"))
    } else {
        Ok(())
    }
}

fn unexpected(message: &'static str) -> McpStreamableHttpError<reqwest::Error> {
    McpStreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(message))
}

fn bounded_sse(
    response: McpEgressResponse,
    limits: TransportLimits,
    operations: Arc<OperationGate>,
    _server_id: McpServerId,
) -> McpSseStream {
    let (response, _, scanner) = response.into_parts();
    let input: BoxStream<'static, Result<Bytes, reqwest::Error>> = response.bytes_stream().boxed();
    futures_util::stream::unfold(
        SseState::new(input, limits, Arc::clone(&operations), scanner),
        |mut state| async move { state.next().await.map(|event| (event, state)) },
    )
    .boxed()
}

struct SseState {
    input: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    chunk: Bytes,
    offset: usize,
    line: Vec<u8>,
    current: McpSse,
    event_bytes: usize,
    limits: TransportLimits,
    pending_cr: bool,
    bom_index: Option<usize>,
    terminal: bool,
    operations: Arc<OperationGate>,
    scanner: Arc<McpResponseScanner>,
}

impl SseState {
    fn new(
        input: BoxStream<'static, Result<Bytes, reqwest::Error>>,
        limits: TransportLimits,
        operations: Arc<OperationGate>,
        scanner: Arc<McpResponseScanner>,
    ) -> Self {
        Self {
            input,
            chunk: Bytes::new(),
            offset: 0,
            line: Vec::new(),
            current: McpSse::default(),
            event_bytes: 0,
            limits,
            pending_cr: false,
            bom_index: Some(0),
            terminal: false,
            operations,
            scanner,
        }
    }

    async fn next(&mut self) -> Option<Result<McpSse, McpSseError>> {
        if self.terminal {
            return None;
        }
        loop {
            if self.offset == self.chunk.len() {
                match self.input.next().await {
                    Some(Ok(chunk)) => {
                        if let Err(error) = scan_ingress(&self.scanner, &chunk, &self.operations) {
                            self.terminal = true;
                            return Some(Err(McpSseError::Body(Box::new(error))));
                        }
                        self.chunk = chunk;
                        self.offset = 0;
                    }
                    Some(Err(error)) => {
                        self.terminal = true;
                        return Some(Err(McpSseError::Body(Box::new(error))));
                    }
                    None => {
                        self.terminal = true;
                        return None;
                    }
                }
                if self.chunk.is_empty() {
                    continue;
                }
            }
            let byte = self.chunk[self.offset];
            self.offset += 1;
            if let Some(index) = self.bom_index {
                const BOM: &[u8] = b"\xef\xbb\xbf";
                if byte == BOM[index] {
                    self.bom_index = (index + 1 < BOM.len()).then_some(index + 1);
                    continue;
                }
                self.line.extend_from_slice(&BOM[..index]);
                self.bom_index = None;
            }
            self.event_bytes = self.event_bytes.saturating_add(1);
            if self.event_bytes > self.limits.max_sse_event_bytes() {
                self.operations.fail(TransportFailure::SseEventTooLarge);
                self.terminal = true;
                return Some(Err(bound_sse_error()));
            }
            if self.pending_cr {
                self.pending_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if matches!(byte, b'\r' | b'\n') {
                self.pending_cr = byte == b'\r';
                let empty = self.line.is_empty();
                if let Err(error) = self.finish_line() {
                    self.terminal = true;
                    return Some(Err(error));
                }
                if empty {
                    self.event_bytes = 0;
                    if let Some(event) = self.take_event() {
                        if let Some(data) = &event.data
                            && data.trim_start().starts_with(['{', '['])
                        {
                            let payload = match RawPayload::parse(
                                data.as_bytes(),
                                self.limits.payload_limits(),
                            ) {
                                Ok(payload) => payload,
                                Err(error) => {
                                    self.operations.fail(TransportFailure::Payload(error));
                                    self.terminal = true;
                                    return Some(Err(McpSseError::Body(Box::new(error))));
                                }
                            };
                            if scan_canonical(&self.scanner, &payload, &self.operations).is_err() {
                                self.terminal = true;
                                return Some(Err(McpSseError::Body(Box::new(SsePayloadConflict))));
                            }
                            if self
                                .operations
                                .bind_callback_scanner(&payload, Arc::clone(&self.scanner))
                                .is_err()
                            {
                                self.terminal = true;
                                return Some(Err(McpSseError::Body(Box::new(SsePayloadConflict))));
                            }
                            if (payload.value().get("result").is_some()
                                || super::is_terminal_url_elicitation(&payload))
                                && self.operations.capture_payload(payload).is_err()
                            {
                                self.terminal = true;
                                return Some(Err(McpSseError::Body(Box::new(SsePayloadConflict))));
                            }
                        }
                        return Some(Ok(event));
                    }
                }
            } else {
                self.line.push(byte);
                if self.line.len() > self.limits.max_sse_event_bytes() {
                    self.operations.fail(TransportFailure::SseEventTooLarge);
                    self.terminal = true;
                    return Some(Err(bound_sse_error()));
                }
            }
        }
    }

    fn finish_line(&mut self) -> Result<(), McpSseError> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() || line.first() == Some(&b':') {
            return Ok(());
        }
        let colon = line.iter().position(|byte| *byte == b':');
        let (field, mut value) = match colon {
            Some(index) => (&line[..index], &line[index + 1..]),
            None => (&line[..], &[][..]),
        };
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        match field {
            b"data" => {
                // SSE decodes malformed UTF-8 with replacement before JSON parsing.
                let value = String::from_utf8_lossy(value);
                let had_data = self.current.data.is_some();
                let data = self.current.data.get_or_insert_with(String::new);
                if had_data {
                    data.push('\n');
                }
                data.push_str(&value);
            }
            b"event" => {
                self.current.event = Some(String::from_utf8_lossy(value).into_owned());
            }
            b"id" if !value.contains(&0) => {
                let value = String::from_utf8_lossy(value);
                if value.len() > self.limits.max_event_id_bytes() {
                    return Err(bound_sse_error());
                }
                self.current.id = Some(value.into_owned());
            }
            b"retry" => {
                let value = String::from_utf8_lossy(value);
                if let Ok(retry) = value.parse::<u64>()
                    && retry <= self.limits.max_sse_retry_millis()
                {
                    self.current.retry = Some(retry);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn take_event(&mut self) -> Option<McpSse> {
        let event = std::mem::take(&mut self.current);
        (event.data.is_some()
            || event.id.is_some()
            || event.retry.is_some()
            || event.event.is_some())
        .then_some(event)
    }
}

#[derive(Debug)]
struct SseBoundError;

impl fmt::Display for SseBoundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MCP SSE event exceeds bound")
    }
}

impl std::error::Error for SseBoundError {}

fn bound_sse_error() -> McpSseError {
    McpSseError::Body(Box::new(SseBoundError))
}

#[derive(Debug)]
struct SsePayloadConflict;

impl fmt::Display for SsePayloadConflict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("conflicting MCP SSE response payload")
    }
}

impl std::error::Error for SsePayloadConflict {}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr, SocketAddr},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use agentkit_mcp::{McpError, PINNED_PROTOCOL_VERSION};
    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, Method},
        response::{IntoResponse, Response},
        routing::any,
    };
    use tokio::{net::TcpListener, sync::Mutex};

    use super::*;
    use crate::{
        capabilities::{
            broker::transport_auth::{TransportAuthorization, TransportOperation},
            kernel::grant_ext::EgressConstraint,
        },
        domain::{
            egress::{
                CredentialHandle as EgressCredentialHandle, DestinationGrant, EgressPolicy,
                ResolverObservation,
            },
            secret::SecretHandle,
        },
    };

    #[derive(Clone)]
    struct RequestRecord {
        method: Method,
        rpc_method: Option<String>,
        headers: HeaderMap,
    }

    #[derive(Clone)]
    struct MockState {
        version: &'static str,
        records: Arc<Mutex<Vec<RequestRecord>>>,
        oversized_sse: bool,
        auth_status: Option<StatusCode>,
        not_found_method: Option<&'static str>,
        initialize_sse: bool,
        extra_headers: usize,
    }

    struct CredentialBroker {
        resolutions: AtomicUsize,
    }

    struct ActiveCredentialBroker;

    #[async_trait::async_trait]
    impl HttpCredentialBroker for ActiveCredentialBroker {
        async fn authorize_and_resolve(
            &self,
            _handle: &SecretHandle,
            _context: &HttpSecretContext<'_>,
        ) -> Result<Arc<SecretLease>, HttpCredentialError> {
            Ok(Arc::new(SecretLease::new(
                b"active-hop-credential".to_vec(),
            )))
        }
    }

    struct StaticBodyDialer(Bytes);

    #[async_trait::async_trait]
    impl EgressDialer for StaticBodyDialer {
        async fn send(
            &self,
            request: reqwest::Request,
            _authorization: &EgressAuthorization,
            _limits: McpEgressLimits,
        ) -> Result<crate::protocols::mcp::egress::EgressDialResponse, std::io::Error> {
            assert_eq!(
                request.headers()[AUTHORIZATION],
                "Bearer active-hop-credential"
            );
            Ok(crate::protocols::mcp::egress::EgressDialResponse {
                response: ::http::Response::builder()
                    .status(StatusCode::OK)
                    .header(CONTENT_TYPE, JSON_CONTENT_TYPE)
                    .body(reqwest::Body::from(self.0.clone()))
                    .unwrap()
                    .into(),
                peer: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            })
        }
    }

    fn response_scanner(secret: &[u8]) -> Arc<McpResponseScanner> {
        Arc::new(McpResponseScanner::new(&[Arc::new(SecretLease::new(
            secret.to_vec(),
        ))]))
    }

    #[async_trait::async_trait]
    impl HttpCredentialBroker for CredentialBroker {
        async fn authorize_and_resolve(
            &self,
            _handle: &SecretHandle,
            context: &HttpSecretContext<'_>,
        ) -> Result<Arc<SecretLease>, HttpCredentialError> {
            assert!(!context.operation().is_empty());
            self.resolutions.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(SecretLease::new(b"test-token".to_vec())))
        }
    }

    #[test]
    fn overlapping_http_registrations_revoke_their_exact_custody_sources() {
        let custody = crate::domain::secret::SecretCustody::default();
        let owner = "http-overlap".to_owned();
        let handle = SecretHandle::parse("env:KIT_TEST_HTTP_SECRET").unwrap();
        let broker = EnvironmentHttpCredentialBroker::new(
            PrincipalId::generate().unwrap(),
            ProjectId::generate().unwrap(),
            WorkspaceId::generate().unwrap(),
            BTreeMap::new(),
        )
        .with_custody(custody.clone(), owner.clone());
        let first = Arc::new(SecretLease::new("first-overlap-secret"));
        let second = Arc::new(SecretLease::new("second-overlap-secret"));
        custody.register(&owner, "first", Arc::clone(&first));
        custody.register(&owner, "second", Arc::clone(&second));
        broker.active_leases.lock().unwrap().insert(
            ("invocation".to_owned(), handle.identifier().to_owned()),
            vec![
                ("first".to_owned(), Arc::clone(&first)),
                ("second".to_owned(), Arc::clone(&second)),
            ],
        );

        broker.revoke("invocation", &handle, &first);
        assert!(!custody.contains(first.expose()));
        assert!(custody.contains(second.expose()));
        broker.revoke("invocation", &handle, &second);
        assert!(custody.leases().is_empty());
        assert!(broker.active_leases.lock().unwrap().is_empty());
    }

    struct RotatingCredentialBroker {
        resolutions: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl HttpCredentialBroker for RotatingCredentialBroker {
        async fn authorize_and_resolve(
            &self,
            _handle: &SecretHandle,
            _context: &HttpSecretContext<'_>,
        ) -> Result<Arc<SecretLease>, HttpCredentialError> {
            let secret = match self.resolutions.fetch_add(1, Ordering::SeqCst) {
                0 => b"sse-credential".as_slice(),
                1 => b"post-credential".as_slice(),
                _ => return Err(HttpCredentialError::Unavailable),
            };
            Ok(Arc::new(SecretLease::new(secret.to_vec())))
        }
    }

    struct ConcurrentResponseDialer {
        calls: AtomicUsize,
        sse: std::sync::Mutex<
            Option<tokio::sync::mpsc::UnboundedSender<Result<Bytes, std::io::Error>>>,
        >,
        post_body: Bytes,
    }

    #[derive(Default)]
    struct NotificationSseDialer {
        sse: std::sync::Mutex<
            Option<tokio::sync::mpsc::UnboundedSender<Result<Bytes, std::io::Error>>>,
        >,
        list_calls: AtomicUsize,
    }

    impl NotificationSseDialer {
        async fn notify(&self, notification: Value) {
            let bytes = Bytes::from(format!("data: {notification}\n\n"));
            loop {
                if let Some(sender) = self.sse.lock().unwrap().clone()
                    && sender.send(Ok(bytes.clone())).is_ok()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }

    #[async_trait::async_trait]
    impl EgressDialer for NotificationSseDialer {
        async fn send(
            &self,
            request: reqwest::Request,
            _authorization: &EgressAuthorization,
            _limits: McpEgressLimits,
        ) -> Result<crate::protocols::mcp::egress::EgressDialResponse, std::io::Error> {
            assert_eq!(
                request.headers()[AUTHORIZATION],
                "Bearer active-hop-credential"
            );
            let http_method = request.method().clone();
            let body = request
                .body()
                .and_then(reqwest::Body::as_bytes)
                .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok());
            let method = body
                .as_ref()
                .and_then(|body| body.get("method"))
                .and_then(Value::as_str);
            let response = if http_method == Method::GET {
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                *self.sse.lock().unwrap() = Some(tx);
                let stream = futures_util::stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|chunk| (chunk, rx))
                });
                ::http::Response::builder()
                    .status(StatusCode::OK)
                    .header(CONTENT_TYPE, SSE_CONTENT_TYPE)
                    .body(reqwest::Body::wrap_stream(stream))
                    .unwrap()
                    .into()
            } else {
                match method {
                    Some("initialize") => {
                        let id = body.as_ref().unwrap()["id"].clone();
                        let initialize = serde_json::json!({
                            "jsonrpc":"2.0",
                            "id":id,
                            "result":{
                                "protocolVersion":"2025-11-25",
                                "capabilities":{"tools":{"listChanged":true}},
                                "serverInfo":{"name":"notification-sse","version":"1"}
                            }
                        });
                        ::http::Response::builder()
                            .status(StatusCode::OK)
                            .header(CONTENT_TYPE, JSON_CONTENT_TYPE)
                            .header(SESSION_HEADER, "notification-session")
                            .body(reqwest::Body::from(initialize.to_string()))
                            .unwrap()
                            .into()
                    }
                    Some("notifications/initialized") => ::http::Response::builder()
                        .status(StatusCode::ACCEPTED)
                        .body(reqwest::Body::default())
                        .unwrap()
                        .into(),
                    Some("tools/list") => {
                        self.list_calls.fetch_add(1, Ordering::SeqCst);
                        let id = body.as_ref().unwrap()["id"].clone();
                        let response = serde_json::json!({
                            "jsonrpc":"2.0",
                            "id":id,
                            "result":{"tools":[{
                                "name":"refreshed_tool",
                                "inputSchema":{"type":"object"}
                            }]}
                        });
                        ::http::Response::builder()
                            .status(StatusCode::OK)
                            .header(CONTENT_TYPE, JSON_CONTENT_TYPE)
                            .body(reqwest::Body::from(response.to_string()))
                            .unwrap()
                            .into()
                    }
                    _ => ::http::Response::builder()
                        .status(StatusCode::ACCEPTED)
                        .body(reqwest::Body::default())
                        .unwrap()
                        .into(),
                }
            };
            Ok(crate::protocols::mcp::egress::EgressDialResponse {
                response,
                peer: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            })
        }
    }

    impl ConcurrentResponseDialer {
        fn new(post_body: impl Into<Bytes>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                sse: std::sync::Mutex::new(None),
                post_body: post_body.into(),
            }
        }

        fn send_sse(&self, bytes: &'static [u8]) {
            self.sse
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .send(Ok(Bytes::from_static(bytes)))
                .unwrap();
        }
    }

    #[async_trait::async_trait]
    impl EgressDialer for ConcurrentResponseDialer {
        async fn send(
            &self,
            request: reqwest::Request,
            _authorization: &EgressAuthorization,
            _limits: McpEgressLimits,
        ) -> Result<crate::protocols::mcp::egress::EgressDialResponse, std::io::Error> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let authorization = request
                .headers()
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok());
            let response = if call == 0 {
                assert_eq!(authorization, Some("Bearer sse-credential"));
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                *self.sse.lock().unwrap() = Some(tx);
                let body = reqwest::Body::wrap_stream(futures_util::stream::unfold(
                    rx,
                    |mut rx| async move { rx.recv().await.map(|chunk| (chunk, rx)) },
                ));
                ::http::Response::builder()
                    .status(StatusCode::OK)
                    .header(CONTENT_TYPE, SSE_CONTENT_TYPE)
                    .body(body)
                    .unwrap()
                    .into()
            } else {
                assert_eq!(call, 1);
                assert_eq!(authorization, Some("Bearer post-credential"));
                ::http::Response::builder()
                    .status(StatusCode::OK)
                    .header(CONTENT_TYPE, JSON_CONTENT_TYPE)
                    .body(reqwest::Body::from(self.post_body.clone()))
                    .unwrap()
                    .into()
            };
            Ok(crate::protocols::mcp::egress::EgressDialResponse {
                response,
                peer: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            })
        }
    }

    async fn spawn_mock(state: MockState) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new().route("/mcp", any(handle)).with_state(state);
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        address
    }

    async fn handle(
        State(state): State<MockState>,
        method: Method,
        headers: HeaderMap,
        body: String,
    ) -> Response {
        let message = serde_json::from_str::<Value>(&body).ok();
        let rpc_method = message
            .as_ref()
            .and_then(|value| value.get("method"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        state.records.lock().await.push(RequestRecord {
            method: method.clone(),
            rpc_method: rpc_method.clone(),
            headers,
        });
        if method == Method::POST
            && let Some(status) = state.auth_status
        {
            return (status, [(WWW_AUTHENTICATE, "Bearer scope=\"mcp.connect\"")]).into_response();
        }
        if (method == Method::POST && rpc_method.as_deref() == state.not_found_method)
            || (method == Method::GET && state.not_found_method == Some("GET"))
            || (method == Method::DELETE && state.not_found_method == Some("DELETE"))
        {
            return StatusCode::NOT_FOUND.into_response();
        }
        match method {
            Method::GET if state.oversized_sse => (
                StatusCode::OK,
                [(CONTENT_TYPE, SSE_CONTENT_TYPE)],
                format!("id: resume-1\ndata: {}\n\n", "x".repeat(256)),
            )
                .into_response(),
            Method::GET => StatusCode::METHOD_NOT_ALLOWED.into_response(),
            Method::DELETE => StatusCode::OK.into_response(),
            Method::POST if rpc_method.as_deref() == Some("initialize") => {
                let id = message.as_ref().and_then(|value| value.get("id")).unwrap();
                let mut result = serde_json::json!({
                    "protocolVersion":state.version,
                    "capabilities":{},
                    "serverInfo":{"name":"kit-http-test","version":"1"}
                });
                if state.version == "missing" {
                    result.as_object_mut().unwrap().remove("protocolVersion");
                }
                let response = serde_json::json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "result":result
                })
                .to_string();
                let mut response = if state.initialize_sse {
                    (
                        StatusCode::OK,
                        [
                            (CONTENT_TYPE, HeaderValue::from_static(SSE_CONTENT_TYPE)),
                            (
                                HeaderName::from_static(SESSION_HEADER),
                                HeaderValue::from_static("bounded-session"),
                            ),
                        ],
                        format!("data: {response}\n\n"),
                    )
                        .into_response()
                } else {
                    (
                        StatusCode::OK,
                        [
                            (CONTENT_TYPE, HeaderValue::from_static(JSON_CONTENT_TYPE)),
                            (
                                HeaderName::from_static(SESSION_HEADER),
                                HeaderValue::from_static("bounded-session"),
                            ),
                        ],
                        response,
                    )
                        .into_response()
                };
                for index in 0..state.extra_headers {
                    response.headers_mut().insert(
                        HeaderName::from_bytes(format!("x-extra-{index}").as_bytes()).unwrap(),
                        HeaderValue::from_static("bounded"),
                    );
                }
                response
            }
            Method::POST => StatusCode::ACCEPTED.into_response(),
            _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
        }
    }

    async fn duplicate_initialize(body: String) -> Response {
        let message: Value = serde_json::from_str(&body).unwrap();
        let id = &message["id"];
        (
            StatusCode::OK,
            [(CONTENT_TYPE, JSON_CONTENT_TYPE)],
            format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2025-11-25","capabilities":{{}},"capabilities":{{}},"serverInfo":{{"name":"duplicate","version":"1"}}}}}}"#
            ),
        )
            .into_response()
    }

    fn authorization(endpoint: &str, credential: &SecretHandle) -> TransportAuthorization {
        TransportAuthorization::for_test_bound_arguments_binding(
            TransportOperation::parse(INITIALIZE_OPERATION).unwrap(),
            agentkit_mcp::kit_authorized_initialize_arguments(),
            Some(credential.clone()),
            None,
            TransportBinding::for_test("http-test", "http", endpoint, None),
        )
    }

    fn client(
        endpoint: &str,
        credential: &SecretHandle,
        broker: Arc<CredentialBroker>,
        limits: TransportLimits,
    ) -> Arc<AuthorizedHttpClient> {
        client_with_dialer(endpoint, credential, broker, limits, None)
    }

    fn client_with_dialer(
        endpoint: &str,
        credential: &SecretHandle,
        broker: Arc<dyn HttpCredentialBroker>,
        limits: TransportLimits,
        dialer: Option<Arc<dyn EgressDialer>>,
    ) -> Arc<AuthorizedHttpClient> {
        let parsed = url::Url::parse(endpoint).unwrap();
        let address = parsed.host_str().unwrap().parse::<IpAddr>().unwrap();
        let policy = EgressAuthorization::for_test(
            crate::domain::egress::Scheme::Http,
            parsed.host_str().unwrap(),
            parsed.port_or_known_default().unwrap(),
            EgressCredentialHandle::new(credential.identifier()).unwrap(),
            [address],
        );
        let operations = OperationGate::new();
        let authorization = authorization(endpoint, credential);
        operations
            .set_binding(authorization.binding().clone())
            .unwrap();
        operations.install(authorization.clone()).unwrap();
        Arc::new(
            AuthorizedHttpClient::new(
                &McpServerId::new("http-test"),
                endpoint,
                authorization,
                EgressPolicy::new([]),
                &policy,
                WorkspaceId::parse("workspace_00000000000000000000000001").unwrap(),
                operations,
                broker,
                limits,
                dialer,
                tokio::time::Instant::now() + limits.request_timeout(),
            )
            .unwrap(),
        )
    }

    fn concurrent_client(
        endpoint: &str,
        credential: &SecretHandle,
        broker: Arc<RotatingCredentialBroker>,
        dialer: Arc<ConcurrentResponseDialer>,
    ) -> AuthorizedHttpClient {
        let parsed = url::Url::parse(endpoint).unwrap();
        let address = parsed.host_str().unwrap().parse::<IpAddr>().unwrap();
        let policy = EgressAuthorization::for_test(
            crate::domain::egress::Scheme::Http,
            parsed.host_str().unwrap(),
            parsed.port_or_known_default().unwrap(),
            EgressCredentialHandle::new(credential.identifier()).unwrap(),
            [address],
        );
        let operations = OperationGate::new();
        let authorization = authorization(endpoint, credential);
        operations
            .set_binding(authorization.binding().clone())
            .unwrap();
        operations
            .bind_connection(Arc::new(authorization.clone()))
            .unwrap();
        let limits = TransportLimits::default();
        AuthorizedHttpClient::new(
            &McpServerId::new("http-test"),
            endpoint,
            authorization,
            EgressPolicy::new([]),
            &policy,
            WorkspaceId::parse("workspace_00000000000000000000000001").unwrap(),
            operations,
            broker,
            limits,
            Some(dialer),
            tokio::time::Instant::now() + limits.request_timeout(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn long_lived_sse_keeps_its_scanner_while_concurrent_post_rotates_credentials() {
        let endpoint = "http://127.0.0.1:43210/mcp";
        let credential = SecretHandle::parse("test:rotating-http").unwrap();
        let broker = Arc::new(RotatingCredentialBroker {
            resolutions: AtomicUsize::new(0),
        });
        let dialer = Arc::new(ConcurrentResponseDialer::new(br#"{}"#.as_slice()));
        let http = concurrent_client(endpoint, &credential, broker, Arc::clone(&dialer));
        let authorization = http.authorization.clone();

        let sse = http
            .send(
                http::Method::GET,
                endpoint,
                http::HeaderMap::new(),
                Bytes::new(),
                &authorization,
            )
            .await
            .unwrap();
        let mut stream = bounded_sse(
            sse,
            TransportLimits::default(),
            Arc::clone(&http.operations),
            McpServerId::new("http-test"),
        );
        let post = http
            .send(
                http::Method::POST,
                endpoint,
                http::HeaderMap::new(),
                Bytes::new(),
                &authorization,
            )
            .await
            .unwrap();
        let deadline = post.deadline();
        bounded_body(post, 1024, &http.operations, deadline)
            .await
            .unwrap();

        dialer.send_sse(b"data: post-credential\n\n");
        assert_eq!(
            stream.next().await.unwrap().unwrap().data.as_deref(),
            Some("post-credential")
        );
        dialer.send_sse(b"data: Bearer sse-credential\n\n");
        assert!(matches!(stream.next().await, Some(Err(_))));
        assert!(matches!(
            http.operations.take_failure(),
            Some(TransportError::SensitivePayload)
        ));
    }

    #[tokio::test]
    async fn rotating_post_reflection_is_rejected_without_ending_live_sse() {
        let endpoint = "http://127.0.0.1:43210/mcp";
        let credential = SecretHandle::parse("test:rotating-http").unwrap();
        let broker = Arc::new(RotatingCredentialBroker {
            resolutions: AtomicUsize::new(0),
        });
        let dialer = Arc::new(ConcurrentResponseDialer::new(
            br#"{"token":"post-credential"}"#.as_slice(),
        ));
        let http = concurrent_client(endpoint, &credential, broker, Arc::clone(&dialer));
        let authorization = http.authorization.clone();

        let sse = http
            .send(
                http::Method::GET,
                endpoint,
                http::HeaderMap::new(),
                Bytes::new(),
                &authorization,
            )
            .await
            .unwrap();
        let mut stream = bounded_sse(
            sse,
            TransportLimits::default(),
            Arc::clone(&http.operations),
            McpServerId::new("http-test"),
        );
        let post = http
            .send(
                http::Method::POST,
                endpoint,
                http::HeaderMap::new(),
                Bytes::new(),
                &authorization,
            )
            .await
            .unwrap();
        let deadline = post.deadline();
        assert!(
            bounded_body(post, 1024, &http.operations, deadline)
                .await
                .is_err()
        );

        dialer.send_sse(b"data: still-live\n\n");
        assert_eq!(
            stream.next().await.unwrap().unwrap().data.as_deref(),
            Some("still-live")
        );
    }

    #[tokio::test]
    async fn active_http_credential_scanner_rejects_raw_percent_and_base64_reflection() {
        let endpoint = "http://127.0.0.1:43210/mcp";
        let credential = SecretHandle::parse("test:active-http").unwrap();
        let initialize: ClientJsonRpcMessage = serde_json::from_value(serde_json::json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":agentkit_mcp::kit_authorized_initialize_arguments()
        }))
        .unwrap();
        let response = |name: &str| {
            Bytes::from(
                serde_json::json!({
                    "jsonrpc":"2.0",
                    "id":1,
                    "result":{
                        "protocolVersion":"2025-11-25",
                        "capabilities":{},
                        "serverInfo":{"name":name,"version":"1"}
                    }
                })
                .to_string(),
            )
        };

        for (encoding, reflected) in [
            ("raw", "active-hop-credential"),
            ("mixed percent", "active%2dhop-cred%65ntial"),
            ("base64", "YWN0aXZlLWhvcC1jcmVkZW50aWFs"),
            ("nested base64", "WVdOMGFYWmxMV2h2Y0MxamNtVmtaVzUwYVdGcw=="),
        ] {
            let http = client_with_dialer(
                endpoint,
                &credential,
                Arc::new(ActiveCredentialBroker),
                TransportLimits::default(),
                Some(Arc::new(StaticBodyDialer(response(reflected)))),
            );
            assert!(
                http.post_message(
                    Arc::from(endpoint),
                    initialize.clone(),
                    None,
                    None,
                    HashMap::new(),
                )
                .await
                .is_err(),
                "{encoding} reflection reached typed parsing",
            );
            assert!(matches!(
                http.operations.take_failure(),
                Some(TransportError::SensitivePayload)
            ));
            assert!(
                http.operations.response.lock().unwrap().payload.is_none(),
                "{encoding} reflection reached model/artifact capture",
            );
        }

        let http = client_with_dialer(
            endpoint,
            &credential,
            Arc::new(ActiveCredentialBroker),
            TransportLimits::default(),
            Some(Arc::new(StaticBodyDialer(response("public-control")))),
        );
        assert!(matches!(
            http.post_message(Arc::from(endpoint), initialize, None, None, HashMap::new(),)
                .await,
            Ok(McpStreamableHttpPostResponse::Json(..))
        ));
        assert!(http.operations.response.lock().unwrap().payload.is_some());
    }

    #[tokio::test]
    async fn production_http_sse_list_changed_reaches_refresh_and_rejects_credential_notification()
    {
        let endpoint = "http://127.0.0.1:43210/mcp";
        let credential = SecretHandle::parse("test:notification-sse").unwrap();
        let dialer = Arc::new(NotificationSseDialer::default());
        let http = client_with_dialer(
            endpoint,
            &credential,
            Arc::new(ActiveCredentialBroker),
            TransportLimits::default(),
            Some(dialer.clone()),
        );
        let connection = McpConnection::connect_authorized_http(
            &McpServerConfig::new(
                "notification-sse",
                McpTransportBinding::StreamableHttp(
                    StreamableHttpTransportConfig::new(endpoint).with_http_client(http.clone()),
                ),
            ),
            McpHandlerConfig::new(),
        )
        .await
        .unwrap();
        let authorization = http.operations.current_authorization().unwrap();
        http.operations.bind_connection(authorization).unwrap();
        http.operations.clear();
        http.operations.ready.store(true, Ordering::Release);

        let mut events = connection.subscribe_events();
        dialer
            .notify(serde_json::json!({
                "jsonrpc":"2.0",
                "method":"notifications/tools/list_changed"
            }))
            .await;
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap()
                .unwrap(),
            agentkit_mcp::McpServerEvent::ToolListChanged
        ));

        http.operations
            .install(operation_authorization(
                &http,
                "tools/list",
                serde_json::json!({}),
                Some(credential),
            ))
            .unwrap();
        let (tools, cursor) = connection.list_tools_page(None).await.unwrap();
        assert_eq!(cursor, None);
        assert_eq!(tools[0].name, "refreshed_tool");
        assert_eq!(dialer.list_calls.load(Ordering::SeqCst), 1);

        dialer
            .notify(serde_json::json!({
                "jsonrpc":"2.0",
                "method":"notifications/progress",
                "params":{
                    "progressToken":"refresh",
                    "progress":1,
                    "message":"active-hop-credential"
                }
            }))
            .await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if matches!(
                    http.operations.take_failure(),
                    Some(TransportError::SensitivePayload)
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn environment_broker_checks_exact_workspace_before_lazy_secret_lookup() {
        let principal = PrincipalId::parse("principal_00000000000000000000000001").unwrap();
        let project = ProjectId::parse("project_00000000000000000000000001").unwrap();
        let workspace = WorkspaceId::parse("workspace_00000000000000000000000001").unwrap();
        let other = WorkspaceId::parse("workspace_00000000000000000000000002").unwrap();
        let handle = SecretHandle::parse("env:KIT_MCP_MUST_NOT_BE_LOADED").unwrap();
        let broker = EnvironmentHttpCredentialBroker::new(
            principal,
            project,
            workspace,
            BTreeMap::from([(
                handle.clone(),
                crate::protocols::mcp::config::McpCredentialScopeConfig::Workspace {
                    workspace_id: workspace,
                },
            )]),
        );
        let result = broker
            .authorize_and_resolve(
                &handle,
                &HttpSecretContext {
                    principal_id: &principal.to_string(),
                    project_id: &project.to_string(),
                    workspace_id: &other.to_string(),
                    invocation_id: "invocation",
                    decision_digest: "decision",
                    request_digest: "request",
                    scope: None,
                    operation: "tools/call",
                    endpoint: "https://example.com/mcp",
                    destination_digest: "destination",
                    hop: 0,
                },
            )
            .await;
        assert_eq!(result.unwrap_err(), HttpCredentialError::Denied);
    }

    fn operation_authorization(
        http: &AuthorizedHttpClient,
        operation: &str,
        arguments: Value,
        credential: Option<SecretHandle>,
    ) -> TransportAuthorization {
        TransportAuthorization::for_test_bound_arguments_binding(
            TransportOperation::parse(operation).unwrap(),
            arguments,
            credential,
            None,
            http.operations.binding().unwrap(),
        )
    }

    #[tokio::test]
    async fn http_and_sse_reject_duplicate_feature_payloads_before_typed_parsing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/mcp", any(duplicate_initialize)),
            )
            .await
            .unwrap();
        });
        let endpoint = format!("http://{address}/mcp");
        let credential = SecretHandle::parse("test:mcp-http-duplicate").unwrap();
        let http = client(
            &endpoint,
            &credential,
            Arc::new(CredentialBroker {
                resolutions: AtomicUsize::new(0),
            }),
            TransportLimits::default(),
        );
        assert!(
            McpConnection::connect_authorized_http(
                &McpServerConfig::new(
                    "duplicate-http",
                    McpTransportBinding::StreamableHttp(
                        StreamableHttpTransportConfig::new(&endpoint)
                            .with_http_client(http.clone()),
                    ),
                ),
                McpHandlerConfig::new()
            )
            .await
            .is_err()
        );
        assert!(matches!(
            http.operations.take_failure(),
            Some(TransportError::Payload(
                crate::protocols::mcp::features::PayloadError::DuplicateKey
            ))
        ));

        let operations = OperationGate::new();
        let input: BoxStream<'static, Result<Bytes, reqwest::Error>> =
            futures_util::stream::once(async {
                Ok(Bytes::from_static(
                    b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"tools\":[],\"tools\":[]}}\n\n",
                ))
            })
            .boxed();
        let mut state = SseState::new(
            input,
            TransportLimits::default(),
            Arc::clone(&operations),
            response_scanner(b"not-reflected"),
        );
        assert!(state.next().await.unwrap().is_err());
        assert!(matches!(
            operations.take_failure(),
            Some(TransportError::Payload(
                crate::protocols::mcp::features::PayloadError::DuplicateKey
            ))
        ));
    }

    #[tokio::test]
    async fn streamable_http_carries_exact_session_and_protocol_headers() {
        let state = MockState {
            version: "2025-11-25",
            records: Arc::new(Mutex::new(Vec::new())),
            oversized_sse: false,
            auth_status: None,
            not_found_method: None,
            initialize_sse: false,
            extra_headers: 0,
        };
        let address = spawn_mock(state.clone()).await;
        let endpoint = format!("http://{address}/mcp");
        let credential = SecretHandle::parse("test:mcp-http").unwrap();
        let broker = Arc::new(CredentialBroker {
            resolutions: AtomicUsize::new(0),
        });
        let http = client(
            &endpoint,
            &credential,
            Arc::clone(&broker),
            TransportLimits::default(),
        );
        let connection = McpConnection::connect_authorized_http(
            &McpServerConfig::new(
                "header-test",
                McpTransportBinding::StreamableHttp(
                    StreamableHttpTransportConfig::new(&endpoint).with_http_client(http),
                ),
            ),
            McpHandlerConfig::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            connection.negotiated_protocol_version(),
            Some(PINNED_PROTOCOL_VERSION)
        );
        connection.close().await.unwrap();

        let records = state.records.lock().await;
        let initialize = records
            .iter()
            .find(|record| record.rpc_method.as_deref() == Some("initialize"))
            .unwrap();
        assert!(initialize.headers.get(PROTOCOL_HEADER).is_none());
        assert!(initialize.headers.get(SESSION_HEADER).is_none());
        for record in records.iter().filter(|record| {
            record.rpc_method.as_deref() != Some("initialize")
                || matches!(record.method, Method::GET | Method::DELETE)
        }) {
            assert_eq!(record.headers[PROTOCOL_HEADER], "2025-11-25");
            assert_eq!(record.headers[SESSION_HEADER], "bounded-session");
            assert_eq!(record.headers[AUTHORIZATION], "Bearer test-token");
        }
        assert!(broker.resolutions.load(Ordering::SeqCst) >= records.len());
    }

    #[tokio::test]
    async fn streamable_http_refuses_old_revision_and_bounds_sse_resume() {
        let state = MockState {
            version: "2025-06-18",
            records: Arc::new(Mutex::new(Vec::new())),
            oversized_sse: true,
            auth_status: None,
            not_found_method: None,
            initialize_sse: true,
            extra_headers: 0,
        };
        let address = spawn_mock(state.clone()).await;
        let endpoint = format!("http://{address}/mcp");
        let credential = SecretHandle::parse("test:mcp-http").unwrap();
        let broker = Arc::new(CredentialBroker {
            resolutions: AtomicUsize::new(0),
        });
        let http = client(
            &endpoint,
            &credential,
            Arc::clone(&broker),
            TransportLimits::default(),
        );
        let error = match McpConnection::connect_authorized_http(
            &McpServerConfig::new(
                "old-http",
                McpTransportBinding::StreamableHttp(
                    StreamableHttpTransportConfig::new(&endpoint).with_http_client(http.clone()),
                ),
            ),
            McpHandlerConfig::new(),
        )
        .await
        {
            Ok(_) => panic!("older revision must be refused"),
            Err(error) => error,
        };
        assert!(matches!(error, McpError::UnsupportedProtocolVersion { .. }));
        let limits = TransportLimits {
            max_sse_event_bytes: 64,
            ..TransportLimits::default()
        };
        let http = client(&endpoint, &credential, broker, limits);
        *http.sessions.lock().unwrap() = Some("bounded-session".to_owned());

        let mut headers = HashMap::new();
        headers.insert(
            HeaderName::from_static(PROTOCOL_HEADER),
            HeaderValue::from_static("2025-11-25"),
        );
        let mut stream = http
            .get_stream(
                Arc::from(endpoint),
                Arc::from("bounded-session"),
                Some("resume-0".to_owned()),
                None,
                headers,
            )
            .await
            .unwrap();
        assert!(matches!(
            stream.next().await,
            Some(Err(McpSseError::Body(_)))
        ));
        let records = state.records.lock().await;
        let resumed = records
            .iter()
            .rev()
            .find(|record| record.method == Method::GET)
            .unwrap();
        assert_eq!(resumed.headers[LAST_EVENT_ID_HEADER], "resume-0");
        assert_eq!(resumed.headers[PROTOCOL_HEADER], "2025-11-25");
        assert_eq!(resumed.headers[SESSION_HEADER], "bounded-session");
    }

    #[tokio::test]
    async fn unauthorized_and_forbidden_are_typed_auth_interruptions() {
        for (status, insufficient) in [
            (StatusCode::UNAUTHORIZED, false),
            (StatusCode::FORBIDDEN, true),
        ] {
            let state = MockState {
                version: "2025-11-25",
                records: Arc::new(Mutex::new(Vec::new())),
                oversized_sse: false,
                auth_status: Some(status),
                not_found_method: None,
                initialize_sse: false,
                extra_headers: 0,
            };
            let address = spawn_mock(state).await;
            let endpoint = format!("http://{address}/mcp");
            let credential = SecretHandle::parse("test:mcp-http").unwrap();
            let broker = Arc::new(CredentialBroker {
                resolutions: AtomicUsize::new(0),
            });
            let http = client(&endpoint, &credential, broker, TransportLimits::default());
            let error = match McpConnection::connect_authorized_http(
                &McpServerConfig::new(
                    "auth-http",
                    McpTransportBinding::StreamableHttp(
                        StreamableHttpTransportConfig::new(&endpoint).with_http_client(http),
                    ),
                ),
                McpHandlerConfig::new(),
            )
            .await
            {
                Ok(_) => panic!("HTTP auth status must interrupt"),
                Err(error) => error,
            };
            let McpError::AuthRequired(challenge) = error else {
                panic!("HTTP auth status must produce typed AuthRequired");
            };
            assert_eq!(
                challenge
                    .challenge
                    .get("insufficient_scope")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                insufficient
            );
            if insufficient {
                assert_eq!(
                    challenge
                        .challenge
                        .get("required_scope")
                        .and_then(Value::as_str),
                    Some("mcp.connect")
                );
            }
        }
    }

    #[tokio::test]
    async fn post_ready_sse_requires_current_same_context_authorization() {
        let state = MockState {
            version: "2025-11-25",
            records: Arc::new(Mutex::new(Vec::new())),
            oversized_sse: false,
            auth_status: None,
            not_found_method: None,
            initialize_sse: false,
            extra_headers: 0,
        };
        let address = spawn_mock(state.clone()).await;
        let endpoint = format!("http://{address}/mcp");
        let credential = SecretHandle::parse("test:mcp-http").unwrap();
        let broker = Arc::new(CredentialBroker {
            resolutions: AtomicUsize::new(0),
        });
        let http = client(
            &endpoint,
            &credential,
            Arc::clone(&broker),
            TransportLimits::default(),
        );
        *http.sessions.lock().unwrap() = Some("bounded-session".to_owned());
        http.bind_session(Some("bounded-session".to_owned()))
            .unwrap();
        http.operations.clear();
        http.operations.ready.store(true, Ordering::Release);
        http.operations
            .install(operation_authorization(
                &http,
                "tools/call",
                serde_json::json!({}),
                Some(credential.clone()),
            ))
            .unwrap();
        let mut headers = HashMap::new();
        headers.insert(
            HeaderName::from_static(PROTOCOL_HEADER),
            HeaderValue::from_static(PROTOCOL_REVISION_HEADER),
        );
        assert!(matches!(
            http.get_stream(
                Arc::from(endpoint.clone()),
                Arc::from("bounded-session"),
                Some("resume-1".to_owned()),
                None,
                headers.clone(),
            )
            .await,
            Err(McpStreamableHttpError::ServerDoesNotSupportSse)
        ));
        assert_eq!(broker.resolutions.load(Ordering::SeqCst), 1);

        http.operations.clear();
        assert!(matches!(
            http.get_stream(
                Arc::from(endpoint),
                Arc::from("bounded-session"),
                Some("resume-1".to_owned()),
                None,
                headers,
            )
            .await,
            Err(McpStreamableHttpError::UnexpectedServerResponse(_))
        ));
        assert_eq!(broker.resolutions.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn initialized_and_delete_require_current_operation_authorization() {
        let state = MockState {
            version: "2025-11-25",
            records: Arc::new(Mutex::new(Vec::new())),
            oversized_sse: false,
            auth_status: None,
            not_found_method: None,
            initialize_sse: false,
            extra_headers: 0,
        };
        let address = spawn_mock(state).await;
        let endpoint = format!("http://{address}/mcp");
        let credential = SecretHandle::parse("test:mcp-http").unwrap();
        let broker = Arc::new(CredentialBroker {
            resolutions: AtomicUsize::new(0),
        });
        let http = client(
            &endpoint,
            &credential,
            Arc::clone(&broker),
            TransportLimits::default(),
        );
        let initialize = serde_json::from_value(serde_json::json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":agentkit_mcp::kit_authorized_initialize_arguments()
        }))
        .unwrap();
        http.post_message(
            Arc::from(endpoint.clone()),
            initialize,
            None,
            None,
            HashMap::new(),
        )
        .await
        .unwrap();

        let initialized = serde_json::from_value(serde_json::json!({
            "jsonrpc":"2.0",
            "method":"notifications/initialized"
        }))
        .unwrap();
        let headers = HashMap::from([(
            HeaderName::from_static(PROTOCOL_HEADER),
            HeaderValue::from_static(PROTOCOL_REVISION_HEADER),
        )]);
        assert!(matches!(
            http.post_message(
                Arc::from(endpoint.clone()),
                initialized,
                Some(Arc::from("bounded-session")),
                None,
                headers.clone(),
            )
            .await,
            Ok(McpStreamableHttpPostResponse::Accepted)
        ));
        assert_eq!(broker.resolutions.load(Ordering::SeqCst), 2);

        http.operations.clear();
        assert!(matches!(
            http.delete_session(
                Arc::from(endpoint.clone()),
                Arc::from("bounded-session"),
                None,
                headers.clone(),
            )
            .await,
            Err(McpStreamableHttpError::UnexpectedServerResponse(_))
        ));
        assert_eq!(broker.resolutions.load(Ordering::SeqCst), 2);

        http.operations
            .install(operation_authorization(
                &http,
                "session/delete",
                serde_json::json!({}),
                Some(credential),
            ))
            .unwrap();
        http.delete_session(
            Arc::from(endpoint),
            Arc::from("bounded-session"),
            None,
            headers,
        )
        .await
        .unwrap();
        assert_eq!(broker.resolutions.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn cross_context_proof_is_rejected_before_credential_resolution() {
        let endpoint = "http://127.0.0.1:9/mcp";
        let credential = SecretHandle::parse("test:mcp-http").unwrap();
        let broker = Arc::new(CredentialBroker {
            resolutions: AtomicUsize::new(0),
        });
        let http = client(
            endpoint,
            &credential,
            Arc::clone(&broker),
            TransportLimits::default(),
        );
        *http.sessions.lock().unwrap() = Some("bounded-session".to_owned());
        http.bind_session(Some("bounded-session".to_owned()))
            .unwrap();
        http.operations.clear();
        http.operations.ready.store(true, Ordering::Release);
        http.operations
            .install(operation_authorization(
                &http,
                "tools/call",
                serde_json::json!({"name":"read"}),
                None,
            ))
            .unwrap();
        let message = serde_json::from_value(serde_json::json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"tools/call",
            "params":{"name":"read"}
        }))
        .unwrap();
        let mut headers = HashMap::new();
        headers.insert(
            HeaderName::from_static(PROTOCOL_HEADER),
            HeaderValue::from_static(PROTOCOL_REVISION_HEADER),
        );
        assert!(matches!(
            http.post_message(
                Arc::from(endpoint),
                message,
                Some(Arc::from("bounded-session")),
                None,
                headers,
            )
            .await,
            Err(McpStreamableHttpError::UnexpectedServerResponse(_))
        ));
        assert_eq!(broker.resolutions.load(Ordering::SeqCst), 0);

        http.operations.clear();
        assert!(
            http.operations
                .install(TransportAuthorization::for_test_bound_arguments_binding(
                    TransportOperation::parse("tools/call").unwrap(),
                    serde_json::json!({"name":"read"}),
                    Some(credential),
                    None,
                    TransportBinding::for_test(
                        "http-test",
                        "http",
                        "http://127.0.0.1:10/mcp",
                        Some("bounded-session".to_owned()),
                    ),
                ))
                .is_err()
        );
        assert_eq!(broker.resolutions.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn session_404_expires_old_state_before_one_fresh_initialize() {
        let state = MockState {
            version: "2025-11-25",
            records: Arc::new(Mutex::new(Vec::new())),
            oversized_sse: false,
            auth_status: None,
            not_found_method: Some("resources/read"),
            initialize_sse: false,
            extra_headers: 0,
        };
        let address = spawn_mock(state).await;
        let endpoint = format!("http://{address}/mcp");
        let credential = SecretHandle::parse("test:mcp-http").unwrap();
        let broker = Arc::new(CredentialBroker {
            resolutions: AtomicUsize::new(0),
        });
        let http = client(&endpoint, &credential, broker, TransportLimits::default());
        *http.sessions.lock().unwrap() = Some("old-session".to_owned());
        http.bind_session(Some("old-session".to_owned())).unwrap();
        http.cleanup_sessions
            .lock()
            .unwrap()
            .insert("old-session".to_owned());
        http.operations.ready.store(true, Ordering::Release);
        http.operations.clear();
        http.operations
            .install(operation_authorization(
                &http,
                "resources/read",
                serde_json::json!({"uri":"memo:test"}),
                Some(credential),
            ))
            .unwrap();
        let message = serde_json::from_value(serde_json::json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"resources/read",
            "params":{"uri":"memo:test"}
        }))
        .unwrap();
        let mut headers = HashMap::new();
        headers.insert(
            HeaderName::from_static(PROTOCOL_HEADER),
            HeaderValue::from_static(PROTOCOL_REVISION_HEADER),
        );
        assert!(matches!(
            http.post_message(
                Arc::from(endpoint),
                message,
                Some(Arc::from("old-session")),
                None,
                headers,
            )
            .await,
            Err(McpStreamableHttpError::SessionExpired)
        ));
        assert!(http.sessions.lock().unwrap().is_none());
        assert!(http.cleanup_sessions.lock().unwrap().is_empty());

        http.operations.ready.store(false, Ordering::Release);
        http.operations.clear();
        http.bind_session(None).unwrap();
        http.operations
            .install(authorization(
                &format!("http://{address}/mcp"),
                &SecretHandle::parse("test:mcp-http").unwrap(),
            ))
            .unwrap();
        let initialize: ClientJsonRpcMessage = serde_json::from_value(serde_json::json!({
            "jsonrpc":"2.0",
            "id":2,
            "method":"initialize",
            "params":agentkit_mcp::kit_authorized_initialize_arguments()
        }))
        .unwrap();
        http.post_message(
            Arc::from(format!("http://{address}/mcp")),
            initialize.clone(),
            None,
            None,
            HashMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            http.sessions.lock().unwrap().as_deref(),
            Some("bounded-session")
        );
        assert!(matches!(
            http.post_message(
                Arc::from(format!("http://{address}/mcp")),
                initialize,
                None,
                None,
                HashMap::new(),
            )
            .await,
            Err(McpStreamableHttpError::UnexpectedServerResponse(_))
        ));
    }

    #[tokio::test]
    async fn missing_revision_is_typed_and_failed_session_is_closed() {
        let state = MockState {
            version: "missing",
            records: Arc::new(Mutex::new(Vec::new())),
            oversized_sse: false,
            auth_status: None,
            not_found_method: None,
            initialize_sse: false,
            extra_headers: 0,
        };
        let address = spawn_mock(state.clone()).await;
        let endpoint = format!("http://{address}/mcp");
        let credential = SecretHandle::parse("test:mcp-http").unwrap();
        let broker = Arc::new(CredentialBroker {
            resolutions: AtomicUsize::new(0),
        });
        let http = client(&endpoint, &credential, broker, TransportLimits::default());
        let error = match McpConnection::connect_authorized_http(
            &McpServerConfig::new(
                "missing-http",
                McpTransportBinding::StreamableHttp(
                    StreamableHttpTransportConfig::new(&endpoint).with_http_client(http),
                ),
            ),
            McpHandlerConfig::new(),
        )
        .await
        {
            Ok(_) => panic!("missing revision must be refused"),
            Err(error) => error,
        };
        assert!(
            matches!(
                &error,
                McpError::UnsupportedProtocolVersion {
                    expected,
                    negotiated: None,
                    ..
                } if *expected == PINNED_PROTOCOL_VERSION
            ),
            "unexpected missing-version error: {error:?}"
        );
        assert!(
            state
                .records
                .lock()
                .await
                .iter()
                .any(|record| record.method == Method::DELETE)
        );
    }

    #[tokio::test]
    async fn aggregate_response_headers_are_bounded_before_use() {
        let state = MockState {
            version: "2025-11-25",
            records: Arc::new(Mutex::new(Vec::new())),
            oversized_sse: false,
            auth_status: None,
            not_found_method: None,
            initialize_sse: false,
            extra_headers: TransportLimits::default().max_headers() + 1,
        };
        let address = spawn_mock(state).await;
        let endpoint = format!("http://{address}/mcp");
        let credential = SecretHandle::parse("test:mcp-http").unwrap();
        let http = client(
            &endpoint,
            &credential,
            Arc::new(CredentialBroker {
                resolutions: AtomicUsize::new(0),
            }),
            TransportLimits::default(),
        );
        assert!(
            McpConnection::connect_authorized_http(
                &McpServerConfig::new(
                    "header-overflow",
                    McpTransportBinding::StreamableHttp(
                        StreamableHttpTransportConfig::new(&endpoint)
                            .with_http_client(http.clone()),
                    ),
                ),
                McpHandlerConfig::new()
            )
            .await
            .is_err()
        );
        assert!(matches!(
            http.operations.take_failure(),
            Some(TransportError::InvalidHeader)
        ));
    }

    #[tokio::test]
    async fn get_and_delete_report_and_clear_expired_sessions() {
        for method in ["GET", "DELETE"] {
            let state = MockState {
                version: "2025-11-25",
                records: Arc::new(Mutex::new(Vec::new())),
                oversized_sse: false,
                auth_status: None,
                not_found_method: Some(method),
                initialize_sse: false,
                extra_headers: 0,
            };
            let address = spawn_mock(state).await;
            let endpoint = format!("http://{address}/mcp");
            let credential = SecretHandle::parse("test:mcp-http").unwrap();
            let http = client(
                &endpoint,
                &credential,
                Arc::new(CredentialBroker {
                    resolutions: AtomicUsize::new(0),
                }),
                TransportLimits::default(),
            );
            http.operations.clear();
            *http.sessions.lock().unwrap() = Some("expired".to_owned());
            http.bind_session(Some("expired".to_owned())).unwrap();
            http.operations.ready.store(true, Ordering::Release);
            http.operations
                .install(operation_authorization(
                    &http,
                    "tools/list",
                    serde_json::json!({}),
                    Some(credential),
                ))
                .unwrap();
            let mut headers = HashMap::from([(
                HeaderName::from_static(PROTOCOL_HEADER),
                HeaderValue::from_static(PROTOCOL_REVISION_HEADER),
            )]);
            let result = if method == "GET" {
                http.get_stream(
                    Arc::from(endpoint),
                    Arc::from("expired"),
                    None,
                    None,
                    headers,
                )
                .await
                .map(|_| ())
            } else {
                http.delete_session(
                    Arc::from(endpoint),
                    Arc::from("expired"),
                    None,
                    std::mem::take(&mut headers),
                )
                .await
            };
            assert!(matches!(
                result,
                Err(McpStreamableHttpError::SessionExpired)
            ));
            assert!(http.sessions.lock().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn reqwest_protocol_failures_are_never_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&accepts);
        tokio::spawn(async move {
            while let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(Duration::from_millis(250), listener.accept()).await
            {
                observed.fetch_add(1, Ordering::SeqCst);
                let mut request = [0u8; 4096];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut request).await;
                drop(socket);
            }
        });
        let endpoint = format!("http://{address}/mcp");
        let credential = SecretHandle::parse("test:mcp-http").unwrap();
        let http = client(
            &endpoint,
            &credential,
            Arc::new(CredentialBroker {
                resolutions: AtomicUsize::new(0),
            }),
            TransportLimits::default(),
        );
        let message = serde_json::from_value(serde_json::json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":agentkit_mcp::kit_authorized_initialize_arguments()
        }))
        .unwrap();
        assert!(
            http.post_message(Arc::from(endpoint), message, None, None, HashMap::new())
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(accepts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn http_construction_requires_matching_egress_policy_authorization() {
        let credential = SecretHandle::parse("test:mcp-http").unwrap();
        let egress =
            EgressConstraint::new("https", "example.com", 443, credential.clone()).unwrap();
        let grant = DestinationGrant::new(
            "https",
            "example.com",
            443,
            EgressCredentialHandle::new(credential.identifier()).unwrap(),
        )
        .unwrap();
        let policy = EgressPolicy::new([grant]);
        let authorization = policy
            .authorize_initial(
                "https://example.com/mcp",
                &EgressCredentialHandle::new(credential.identifier()).unwrap(),
                &ResolverObservation::new([IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]),
            )
            .unwrap();
        let transport = TransportAuthorization::for_test(
            TransportOperation::parse(INITIALIZE_OPERATION).unwrap(),
            Some(credential),
            Some(egress),
        );
        assert!(
            validate_policy(
                "https://example.com/mcp",
                &authorization,
                &transport,
                TransportLimits::default(),
            )
            .is_ok()
        );
        assert!(matches!(
            validate_policy(
                "https://other.example/mcp",
                &authorization,
                &transport,
                TransportLimits::default(),
            ),
            Err(TransportError::PolicyAuthorizationMismatch)
        ));
    }

    #[tokio::test]
    async fn later_sse_callbacks_reject_bearer_reflection() {
        for method in ["sampling/createMessage", "elicitation/create", "roots/list"] {
            let operations = OperationGate::new();
            let event = format!(
                "data: {{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"{method}\",\"params\":{{\"message\":\"Bearer credential-canary\"}}}}\n\n"
            );
            let input: BoxStream<'static, Result<Bytes, reqwest::Error>> =
                futures_util::stream::once(async move { Ok(Bytes::from(event)) }).boxed();
            let mut state = SseState::new(
                input,
                TransportLimits::default(),
                Arc::clone(&operations),
                response_scanner(b"credential-canary"),
            );
            assert!(matches!(
                state.next().await,
                Some(Err(McpSseError::Body(_)))
            ));
            assert!(matches!(
                operations.take_failure(),
                Some(TransportError::SensitivePayload)
            ));
        }
    }

    #[tokio::test]
    async fn sse_parser_follows_bom_eof_replacement_retry_and_empty_id_rules() {
        let input: BoxStream<'static, Result<Bytes, reqwest::Error>> = futures_util::stream::iter([
            Ok(Bytes::from_static(b"\xef")),
            Ok(Bytes::from_static(
                b"\xbb\xbfevent: old\nevent: message\nid: old\nid:\nretry: nope\nretry: 12\ndata:\ndata: value\xff\n\n",
            )),
        ])
        .boxed();
        let operations = OperationGate::new();
        let mut state = SseState::new(
            input,
            TransportLimits::default(),
            operations,
            response_scanner(b"not-reflected"),
        );
        let event = state.next().await.unwrap().unwrap();
        assert_eq!(event.event.as_deref(), Some("message"));
        assert_eq!(event.id.as_deref(), Some(""));
        assert_eq!(event.retry, Some(12));
        assert_eq!(event.data.as_deref(), Some("\nvalue\u{fffd}"));

        let input: BoxStream<'static, Result<Bytes, reqwest::Error>> =
            futures_util::stream::iter([Ok(Bytes::from_static(b"data: incomplete"))]).boxed();
        let operations = OperationGate::new();
        let mut state = SseState::new(
            input,
            TransportLimits::default(),
            operations,
            response_scanner(b"not-reflected"),
        );
        assert!(state.next().await.is_none());

        let input: BoxStream<'static, Result<Bytes, reqwest::Error>> =
            futures_util::stream::iter([Ok(Bytes::from_static(b"id: control\nretry: 10\n\n"))])
                .boxed();
        let operations = OperationGate::new();
        let mut state = SseState::new(
            input,
            TransportLimits::default(),
            operations,
            response_scanner(b"not-reflected"),
        );
        let control = state.next().await.unwrap().unwrap();
        assert_eq!(control.id.as_deref(), Some("control"));
        assert_eq!(control.retry, Some(10));
        assert!(control.data.is_none());
    }
}
