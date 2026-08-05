mod http;
mod invocation;
mod stdio;

use std::{
    collections::BTreeMap,
    fmt, io,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use agentkit_mcp::{
    CallToolResult, GetPromptResult, McpAuthorizedReinitializeCandidate, McpConnection, McpError,
    McpHttpClient, McpProtocolVersion, McpServerEvent, PINNED_PROTOCOL_VERSION, ReadResourceResult,
};
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::protocols::mcp::egress::McpResponseScanner;
use crate::{
    capabilities::broker::{
        self, BrokerError, BrokerInvocation, BrokerOutcome, BrokerPrepareOutcome,
        OwnedBrokerInvocation,
        transport_auth::{self, TransportAuthState},
    },
    capabilities::kernel::{
        identity::{Digest, DigestAlgorithm},
        invoke::InvocationCrashPoint,
    },
    capabilities::{kernel::invoke::InvocationEnvelope, registration::BoundRegistrationCall},
    protocols::mcp::features::{
        ConfiguredServerIdentity, DiscoveredFeatures, DiscoveryError, FeatureError,
        FeatureListKind, FeaturePage, McpCatalog, NegotiatedFeatureKinds, PayloadError,
        PayloadLimits, PromptDescriptor, RawPayload, RefreshCoalescer, RefreshLimits,
        ResourceDescriptor, ResourceTemplateDescriptor, ToolDescriptor, decode_prompts_page,
        decode_resource_templates_page, decode_resources_page, decode_tools_page,
    },
    runtime::scheduler::reserve::BudgetLedger,
    store::artifacts::ArtifactStore,
    store::sqlite::append::SqliteStore,
};

pub use crate::protocols::mcp::egress::{
    HttpCredentialBroker, HttpCredentialError, HttpSecretContext,
};
pub(crate) use http::EnvironmentHttpCredentialBroker;
pub use http::{
    StreamableHttpOutcome, connect_streamable_http, resolve_streamable_http_auth,
    resume_streamable_http,
};
use http::{connect_streamable_http_with_handler, resume_streamable_http_with_handler};
use invocation::{
    McpInvocationResult, McpOperation, NormalizedMcpResult, normalize_invocation_result,
};
pub use invocation::{McpResultError, McpResultPolicy};
#[cfg(not(windows))]
pub(crate) use stdio::ProductionStdioProfiles;
use stdio::connect_stdio_with_handler;
pub use stdio::{
    OwnedStdioEnvironment, OwnedStdioLaunchError, OwnedStdioLimits, OwnedStdioProcess,
    OwnedStdioProcessLaunch, OwnedStdioProcessService, OwnedStdioProfileError,
    OwnedStdioProfileProvider, SandboxedStdioLauncher, connect_stdio,
};

pub const PROTOCOL_REVISION: McpProtocolVersion = PINNED_PROTOCOL_VERSION;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn connect_configured_streamable_http(
    server_id: &str,
    endpoint: &str,
    request: &BrokerInvocation<'_>,
    policy: &crate::domain::egress::EgressPolicy,
    credentials: Arc<dyn HttpCredentialBroker>,
    store: &mut SqliteStore,
    limits: TransportLimits,
    handler: agentkit_mcp::McpHandlerConfig,
) -> Result<StreamableHttpOutcome, TransportError> {
    connect_streamable_http_with_handler(
        agentkit_mcp::McpServerId::new(server_id),
        endpoint,
        request,
        policy,
        credentials,
        store,
        limits,
        handler,
    )
    .await
}

pub(crate) async fn connect_configured_stdio(
    server_id: &str,
    profile: &str,
    request: &BrokerInvocation<'_>,
    prepare: impl FnOnce() -> Result<SandboxedStdioLauncher, OwnedStdioProfileError>,
    store: &mut SqliteStore,
    limits: TransportLimits,
    handler: agentkit_mcp::McpHandlerConfig,
) -> Result<ReadyConnection, TransportError> {
    connect_stdio_with_handler(
        agentkit_mcp::McpServerId::new(server_id),
        profile,
        request,
        prepare,
        store,
        limits,
        handler,
    )
    .await
}

pub(crate) fn resolve_configured_streamable_http_auth(
    server_id: &str,
    endpoint: &str,
    request: &BrokerInvocation<'_>,
    actor: &crate::api::auth::contract::AuthenticatedPrincipal,
    resolution: crate::capabilities::broker::AuthResolution,
    expected: &crate::agent::driver::restart::ResolvedMcpBootstrapAuth,
    store: &mut SqliteStore,
) -> Result<(), TransportError> {
    transport_auth::resume_expected(
        request,
        actor,
        server_id,
        "http",
        endpoint,
        resolution,
        Some((
            expected.challenge_id,
            expected.challenge_kind,
            expected.challenge_generation,
        )),
        store,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn resume_configured_streamable_http(
    server_id: &str,
    endpoint: &str,
    request: &BrokerInvocation<'_>,
    policy: &crate::domain::egress::EgressPolicy,
    credentials: Arc<dyn HttpCredentialBroker>,
    store: &mut SqliteStore,
    limits: TransportLimits,
    handler: agentkit_mcp::McpHandlerConfig,
) -> Result<ReadyConnection, TransportError> {
    resume_streamable_http_with_handler(
        agentkit_mcp::McpServerId::new(server_id),
        endpoint,
        request,
        policy,
        credentials,
        store,
        limits,
        handler,
    )
    .await
}

fn validate_initialize_arguments(request: &BrokerInvocation<'_>) -> Result<(), TransportError> {
    let arguments =
        serde_json::from_slice::<rmcp::model::InitializeRequestParams>(request.arguments())
            .map_err(|_| BrokerError::InvalidArguments)?;
    let capabilities = &arguments.capabilities;
    if arguments.protocol_version == PINNED_PROTOCOL_VERSION
        && arguments.client_info.name == "agentkit-mcp"
        && !arguments.client_info.version.is_empty()
        && arguments.client_info.version.len() <= 64
        && arguments.meta.is_none()
        && capabilities.experimental.is_none()
        && capabilities.extensions.is_none()
        && capabilities.tasks.is_none()
        && capabilities
            .roots
            .as_ref()
            .is_none_or(|roots| roots.list_changed != Some(true))
        && capabilities
            .sampling
            .as_ref()
            .is_none_or(|sampling| sampling.tools.is_none() && sampling.context.is_none())
        && capabilities
            .elicitation
            .as_ref()
            .is_none_or(|elicitation| elicitation.form.is_some() || elicitation.url.is_some())
    {
        Ok(())
    } else {
        Err(BrokerError::InvalidArguments.into())
    }
}

const MAX_CONFIGURED_BYTES: usize = 16 * 1024 * 1024;
const MAX_CONFIGURED_HEADERS: usize = 256;
const MAX_CONFIGURED_CHANNEL_CAPACITY: usize = 4096;
const MAX_CONFIGURED_DURATION: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransportLimits {
    max_json_bytes: usize,
    max_sse_event_bytes: usize,
    max_session_id_bytes: usize,
    max_event_id_bytes: usize,
    max_header_bytes: usize,
    max_headers: usize,
    channel_capacity: usize,
    max_sse_reconnects: usize,
    max_sse_retry_millis: u64,
    request_timeout: Duration,
    connect_timeout: Duration,
    close_timeout: Duration,
}

impl Default for TransportLimits {
    fn default() -> Self {
        Self {
            max_json_bytes: 4 * 1024 * 1024,
            max_sse_event_bytes: 4 * 1024 * 1024,
            max_session_id_bytes: 1024,
            max_event_id_bytes: 1024,
            max_header_bytes: 32 * 1024,
            max_headers: 64,
            channel_capacity: 64,
            max_sse_reconnects: 3,
            max_sse_retry_millis: 30_000,
            request_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(30),
            close_timeout: Duration::from_secs(5),
        }
    }
}

impl TransportLimits {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        max_json_bytes: usize,
        max_sse_event_bytes: usize,
        max_session_id_bytes: usize,
        max_event_id_bytes: usize,
        max_header_bytes: usize,
        max_headers: usize,
        channel_capacity: usize,
        max_sse_reconnects: usize,
        max_sse_retry_millis: u64,
        request_timeout: Duration,
        connect_timeout: Duration,
        close_timeout: Duration,
    ) -> Result<Self, TransportError> {
        let limits = Self {
            max_json_bytes,
            max_sse_event_bytes,
            max_session_id_bytes,
            max_event_id_bytes,
            max_header_bytes,
            max_headers,
            channel_capacity,
            max_sse_reconnects,
            max_sse_retry_millis,
            request_timeout,
            connect_timeout,
            close_timeout,
        };
        if [
            max_json_bytes,
            max_sse_event_bytes,
            max_session_id_bytes,
            max_event_id_bytes,
            max_header_bytes,
        ]
        .into_iter()
        .any(|value| value == 0 || value > MAX_CONFIGURED_BYTES)
            || max_headers == 0
            || max_headers > MAX_CONFIGURED_HEADERS
            || channel_capacity == 0
            || channel_capacity > MAX_CONFIGURED_CHANNEL_CAPACITY
            || max_sse_reconnects > 64
            || max_sse_retry_millis == 0
            || max_sse_retry_millis > MAX_CONFIGURED_DURATION.as_millis() as u64
            || [request_timeout, connect_timeout, close_timeout]
                .into_iter()
                .any(|duration| duration.is_zero() || duration > MAX_CONFIGURED_DURATION)
        {
            return Err(TransportError::InvalidLimits);
        }
        Ok(limits)
    }

    pub const fn max_json_bytes(self) -> usize {
        self.max_json_bytes
    }

    pub const fn max_sse_event_bytes(self) -> usize {
        self.max_sse_event_bytes
    }

    pub const fn max_session_id_bytes(self) -> usize {
        self.max_session_id_bytes
    }

    pub const fn max_event_id_bytes(self) -> usize {
        self.max_event_id_bytes
    }

    pub const fn max_header_bytes(self) -> usize {
        self.max_header_bytes
    }

    pub const fn max_headers(self) -> usize {
        self.max_headers
    }

    pub const fn channel_capacity(self) -> usize {
        self.channel_capacity
    }

    pub const fn max_sse_reconnects(self) -> usize {
        self.max_sse_reconnects
    }

    pub const fn max_sse_retry_millis(self) -> u64 {
        self.max_sse_retry_millis
    }

    pub const fn request_timeout(self) -> Duration {
        self.request_timeout
    }

    pub const fn connect_timeout(self) -> Duration {
        self.connect_timeout
    }

    pub const fn close_timeout(self) -> Duration {
        self.close_timeout
    }

    pub const fn payload_limits(self) -> PayloadLimits {
        PayloadLimits::with_max_bytes(self.max_json_bytes)
    }
}

pub struct ReadyConnection {
    connection: McpConnection,
    configured_server: ConfiguredServerIdentity,
    request_timeout: Duration,
    close_timeout: Duration,
    payload_limits: PayloadLimits,
    operations: Arc<OperationGate>,
    cleanup: Option<Arc<dyn McpHttpClient>>,
    stdio_reaper: Option<Arc<dyn OwnedStdioProcess>>,
    can_reinitialize: bool,
    negotiated: Arc<RwLock<NegotiatedFeatureKinds>>,
    retired: AtomicBool,
    serial: OperationQueue,
    lifecycle_authority: Option<Arc<LifecycleAuthority>>,
    responders: Option<crate::protocols::mcp::responders::ResponderInstallation>,
}

struct LifecycleAuthority(OwnedBrokerInvocation);

impl LifecycleAuthority {
    fn capture(request: &BrokerInvocation<'_>) -> Self {
        Self(OwnedBrokerInvocation::capture(request))
    }

    fn mint(&self) -> Result<OwnedBrokerInvocation, BrokerError> {
        self.0.mint()
    }
}

struct RegisteredConnection {
    generation: u64,
    connection: Arc<ReadyConnection>,
}

#[derive(Default)]
pub struct ReadyConnectionRegistry {
    connections: RwLock<BTreeMap<String, RegisteredConnection>>,
    generation: AtomicU64,
}

pub struct McpCapabilityRuntime {
    catalog: Arc<RwLock<McpCatalog>>,
    connections: Arc<ReadyConnectionRegistry>,
    lifecycle: Arc<RwLock<()>>,
    stopped: AtomicBool,
    retained_retired: Mutex<Vec<Arc<ReadyConnection>>>,
    authority: RwLock<
        BTreeMap<
            String,
            (
                crate::capabilities::kernel::grant_ext::GrantExtension,
                crate::capabilities::kernel::grant_ext::RequestExtension,
            ),
        >,
    >,
}

#[derive(Clone)]
pub struct McpRuntimeServer {
    catalog: crate::protocols::mcp::features::McpCatalogConfig,
    discovered: DiscoveredFeatures,
    connection: Arc<ReadyConnection>,
    grant_extension: crate::capabilities::kernel::grant_ext::GrantExtension,
    request_extension: crate::capabilities::kernel::grant_ext::RequestExtension,
}

impl McpRuntimeServer {
    pub fn new(
        catalog: crate::protocols::mcp::features::McpCatalogConfig,
        discovered: DiscoveredFeatures,
        connection: Arc<ReadyConnection>,
    ) -> Result<Self, TransportError> {
        if catalog.server() != discovered.server()
            || discovered.server() != &connection.configured_server
            || connection.lifecycle_authority.is_none()
        {
            return Err(TransportError::AuthorizationMismatch);
        }
        Ok(Self {
            catalog,
            discovered,
            connection,
            grant_extension: Default::default(),
            request_extension: Default::default(),
        })
    }

    pub fn with_authority(
        mut self,
        grant: crate::capabilities::kernel::grant_ext::GrantExtension,
        request: crate::capabilities::kernel::grant_ext::RequestExtension,
    ) -> Self {
        self.grant_extension = grant;
        self.request_extension = request;
        self
    }
}

impl McpCapabilityRuntime {
    pub fn new(catalog: McpCatalog) -> Self {
        Self {
            catalog: Arc::new(RwLock::new(catalog)),
            connections: Arc::new(ReadyConnectionRegistry::default()),
            lifecycle: Arc::new(RwLock::new(())),
            stopped: AtomicBool::new(false),
            retained_retired: Mutex::new(Vec::new()),
            authority: RwLock::new(BTreeMap::new()),
        }
    }

    pub fn from_configured_servers(
        catalog: McpCatalog,
        servers: impl IntoIterator<Item = McpRuntimeServer>,
    ) -> Result<Self, TransportError> {
        let runtime = Self::new(catalog);
        for server in servers {
            let identity = server.catalog.server().as_str().to_owned();
            runtime.install(server.catalog, server.discovered, server.connection)?;
            runtime
                .authority
                .write()
                .map_err(|_| TransportError::AuthorizationMismatch)?
                .insert(identity, (server.grant_extension, server.request_extension));
        }
        for registered in runtime
            .connections
            .connections
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .values()
        {
            registered.connection.arm_responders();
        }
        Ok(runtime)
    }

    pub fn authority_for(
        &self,
        server: &str,
    ) -> Result<
        (
            crate::capabilities::kernel::grant_ext::GrantExtension,
            crate::capabilities::kernel::grant_ext::RequestExtension,
        ),
        TransportError,
    > {
        self.authority
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .get(server)
            .cloned()
            .ok_or(TransportError::AuthorizationMismatch)
    }

    #[cfg(test)]
    pub(crate) fn catalog_snapshot(
        &self,
    ) -> Result<crate::capabilities::catalog::CatalogSnapshot, TransportError> {
        let _lifecycle = self
            .lifecycle
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        Ok(self
            .catalog
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .snapshot_owned())
    }

    pub fn catalog_snapshot_for(
        &self,
        principal_id: crate::domain::ids::PrincipalId,
        project_id: crate::domain::ids::ProjectId,
        workspace_id: crate::domain::ids::WorkspaceId,
        workspace_revision: Option<&str>,
    ) -> Result<crate::capabilities::catalog::CatalogSnapshot, TransportError> {
        let _lifecycle = self
            .lifecycle
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        let connections = self
            .connections
            .connections
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        self.catalog
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .snapshot()
            .filtered(|entry| {
                let Some(target) = entry.external_target() else {
                    return true;
                };
                connections
                    .get(target.configured_server())
                    .is_some_and(|registered| {
                        registered
                            .connection
                            .operations
                            .binding()
                            .is_ok_and(|binding| {
                                binding.owned_by(
                                    &principal_id.to_string(),
                                    &project_id.to_string(),
                                    &workspace_id.to_string(),
                                    workspace_revision,
                                )
                            })
                    })
            })
            .map_err(|_| TransportError::AuthorizationMismatch)
    }

    fn install(
        &self,
        config: crate::protocols::mcp::features::McpCatalogConfig,
        discovered: DiscoveredFeatures,
        connection: Arc<ReadyConnection>,
    ) -> Result<(u64, Option<Arc<ReadyConnection>>), TransportError> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(TransportError::ConnectionRetired);
        }
        let _lifecycle = self
            .lifecycle
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        if discovered.server() != &connection.configured_server {
            return Err(TransportError::AuthorizationMismatch);
        }
        let mut candidate = self
            .catalog
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .clone();
        candidate.publish(config, discovered)?;
        let mut catalog = self
            .catalog
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        let mut connections = self
            .connections
            .connections
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        let generation = self.connections.next_generation()?;
        let server = connection.configured_server.as_str().to_owned();
        let replaced = connections
            .insert(
                server,
                RegisteredConnection {
                    generation,
                    connection,
                },
            )
            .map(|registered| registered.connection);
        *catalog = candidate;
        if let Some(replaced) = &replaced {
            replaced.disarm_responders();
            replaced.retire();
        }
        Ok((generation, replaced))
    }

    pub fn remove(
        &self,
        server: &ConfiguredServerIdentity,
        generation: u64,
    ) -> Result<Option<Arc<ReadyConnection>>, TransportError> {
        let _lifecycle = self
            .lifecycle
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        let removed = self.connections.remove(server, generation)?;
        if let Some(connection) = &removed {
            connection.disarm_responders();
            self.catalog
                .write()
                .map_err(|_| TransportError::AuthorizationMismatch)?
                .remove(server)?;
        }
        Ok(removed)
    }

    #[allow(clippy::await_holding_lock)]
    pub async fn invoke_registered<'a>(
        &self,
        call: &'a BoundRegistrationCall,
        envelope: InvocationEnvelope<'a>,
        store: &mut SqliteStore,
        budget: &BudgetLedger,
        artifacts: &ArtifactStore,
        policy: &McpResultPolicy,
    ) -> Result<BrokerOutcome, TransportError> {
        let context = call.context();
        let binding = context.binding();
        let server = McpOperation::configured_server(binding)?;
        let envelope = envelope.bind_external(
            context.capability(),
            context.schema_digest(),
            context.effect(),
            context.retry_safety(),
            context.input_bytes(),
        );
        let request = BrokerInvocation::bound_external(envelope, &context)?;
        // Exact durable outcomes survive catalog removal, but replay still runs
        // current run authority and artifact-owner checks and never dispatches.
        if let Some(result) = broker::replay(&request, store, budget, artifacts)? {
            return Ok(BrokerOutcome::Completed(result));
        }
        let _lifecycle = self
            .lifecycle
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        {
            let catalog = self
                .catalog
                .read()
                .map_err(|_| TransportError::AuthorizationMismatch)?;
            let current = catalog
                .snapshot()
                .get_identity(binding.pinned_entry().identity())
                .ok_or(TransportError::BindingExpired)?;
            if current.digest() != binding.entry_digest()
                || current.availability() == crate::capabilities::catalog::Availability::Unavailable
            {
                return Err(TransportError::BindingExpired);
            }
        }
        let (_generation, connection) = self.connections.get_registered(server)?;
        connection
            .invoke_bound(request, store, budget, artifacts, policy)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub fn resolve_registered_auth<'a>(
        &self,
        call: &'a BoundRegistrationCall,
        envelope: InvocationEnvelope<'a>,
        actor: &crate::api::auth::contract::AuthenticatedPrincipal,
        resolution: crate::capabilities::broker::AuthResolution,
        current: &crate::capabilities::broker::AuthChallenge,
        challenge_id: crate::domain::ids::ApprovalId,
        challenge_kind: crate::capabilities::broker::AuthChallengeKind,
        challenge_generation: u64,
        store: &mut SqliteStore,
    ) -> Result<bool, TransportError> {
        let _lifecycle = self
            .lifecycle
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        let context = call.context();
        let binding = context.binding();
        let server = McpOperation::configured_server(binding)?;
        let connection = self.connections.get(server)?;
        let envelope = envelope.bind_external(
            context.capability(),
            context.schema_digest(),
            context.effect(),
            context.retry_safety(),
            context.input_bytes(),
        );
        let request = BrokerInvocation::bound_external(envelope, &context)?;
        if current.challenge_id != challenge_id
            || current.kind != challenge_kind
            || current.generation != challenge_generation
        {
            return Ok(false);
        }
        match challenge_kind {
            crate::capabilities::broker::AuthChallengeKind::Broker => {
                broker::resolve_auth_expected(
                    &request,
                    actor,
                    resolution,
                    challenge_id,
                    challenge_kind,
                    challenge_generation,
                    store,
                )?;
                Ok(true)
            }
            crate::capabilities::broker::AuthChallengeKind::Transport => {
                let transport_binding = connection.operations.binding()?;
                let TransportAuthState::Pending(challenge) =
                    transport_auth::state(&request, &transport_binding, store)?
                else {
                    return Err(BrokerError::InvalidAuthState.into());
                };
                if challenge.challenge.challenge_id != challenge_id
                    || challenge.challenge.kind != challenge_kind
                    || challenge.challenge.generation != challenge_generation
                {
                    return Err(BrokerError::InvalidAuthState.into());
                }
                transport_auth::resume_bound_expected(
                    &request,
                    actor,
                    &transport_binding,
                    resolution,
                    (
                        challenge_id,
                        match challenge_kind {
                            crate::capabilities::broker::AuthChallengeKind::Broker => "broker",
                            crate::capabilities::broker::AuthChallengeKind::Transport => {
                                "transport"
                            }
                        },
                        challenge_generation,
                    ),
                    store,
                )?;
                Ok(true)
            }
        }
    }

    pub async fn drive_refresh<'a, F>(
        &self,
        server: &ConfiguredServerIdentity,
        generation: u64,
        limits: RefreshLimits,
        authorize_page: &mut F,
        store: &mut SqliteStore,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), TransportError>
    where
        F: FnMut(&'static str, Option<&str>) -> Result<BrokerInvocation<'a>, TransportError>,
    {
        let connection = self.connections.get(server.as_str())?;
        if self.connections.generation(server)? != Some(generation) {
            return Ok(());
        }
        let mut driver = connection.refresh_driver(limits);
        let result = driver
            .run(
                &connection,
                &self.catalog,
                &self.connections,
                &self.lifecycle,
                generation,
                authorize_page,
                store,
                shutdown,
            )
            .await;
        if let Err(error) = result {
            let _ = self.retire_and_close_owned(server, generation, store).await;
            return Err(error);
        }
        Ok(())
    }

    pub async fn drive_refresh_owned(
        &self,
        server: &ConfiguredServerIdentity,
        generation: u64,
        limits: RefreshLimits,
        store: &mut SqliteStore,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), TransportError> {
        let connection = self.connections.get(server.as_str())?;
        if self.connections.generation(server)? != Some(generation) {
            return Ok(());
        }
        let mut driver = connection.refresh_driver(limits);
        let result = driver
            .run_owned(
                &connection,
                &self.catalog,
                &self.connections,
                &self.lifecycle,
                generation,
                store,
                shutdown,
            )
            .await;
        if let Err(error) = result {
            let _ = self.retire_and_close_owned(server, generation, store).await;
            return Err(error);
        }
        Ok(())
    }

    pub fn refresh_registrations(
        &self,
    ) -> Result<Vec<(ConfiguredServerIdentity, u64)>, TransportError> {
        self.connections
            .connections
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .values()
            .map(|registered| {
                Ok((
                    registered.connection.configured_server.clone(),
                    registered.generation,
                ))
            })
            .collect()
    }

    pub async fn remove_and_close(
        &self,
        server: &ConfiguredServerIdentity,
        generation: u64,
        request: &BrokerInvocation<'_>,
        store: &mut SqliteStore,
    ) -> Result<bool, TransportError> {
        let Some(connection) = self.remove(server, generation)? else {
            return Ok(false);
        };
        connection.close(request, store).await?;
        Ok(true)
    }

    pub(crate) async fn retire_and_close_owned(
        &self,
        server: &ConfiguredServerIdentity,
        generation: u64,
        store: &mut SqliteStore,
    ) -> Result<bool, TransportError> {
        let Some(connection) = self.remove(server, generation)? else {
            return Ok(false);
        };
        let result = connection.close_owned(store).await;
        if let Err(error) = result {
            self.retained_retired
                .lock()
                .map_err(|_| TransportError::AuthorizationMismatch)?
                .push(connection);
            return Err(error);
        }
        Ok(true)
    }

    pub(crate) fn refresh_is_current(
        &self,
        server: &ConfiguredServerIdentity,
        generation: u64,
    ) -> Result<bool, TransportError> {
        Ok(self.connections.generation(server)? == Some(generation))
    }

    pub async fn replace_and_close(
        &self,
        server: McpRuntimeServer,
        store: &mut SqliteStore,
    ) -> Result<u64, TransportError> {
        let identity = server.catalog.server().as_str().to_owned();
        let (generation, replaced) =
            self.install(server.catalog, server.discovered, server.connection)?;
        self.authority
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .insert(
                identity.clone(),
                (server.grant_extension, server.request_extension),
            );
        self.connections.get(&identity)?.arm_responders();
        if let Some(replaced) = replaced {
            let result = replaced.close_owned(store).await;
            if let Err(error) = result {
                self.retained_retired
                    .lock()
                    .map_err(|_| TransportError::AuthorizationMismatch)?
                    .push(replaced);
                return Err(error);
            }
        }
        Ok(generation)
    }

    pub(crate) fn retire_for_revision_change(&self) -> Result<(), TransportError> {
        let _lifecycle = self
            .lifecycle
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        let connections = self.connections.drain();
        let mut catalog = self
            .catalog
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        for connection in &connections {
            connection.disarm_responders();
            connection.retire();
            catalog.remove(&connection.configured_server)?;
        }
        self.retained_retired
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .extend(connections);
        Ok(())
    }

    pub async fn shutdown(&self, store: &mut SqliteStore) -> Result<(), TransportError> {
        self.stopped.store(true, Ordering::Release);
        let connections = {
            let _lifecycle = self
                .lifecycle
                .write()
                .map_err(|_| TransportError::AuthorizationMismatch)?;
            let connections = self.connections.drain();
            let mut catalog = self
                .catalog
                .write()
                .map_err(|_| TransportError::AuthorizationMismatch)?;
            for connection in &connections {
                catalog.remove(&connection.configured_server)?;
            }
            connections
        };
        let mut failure = None;
        let mut connections = connections;
        connections.extend(
            self.retained_retired
                .lock()
                .map_err(|_| TransportError::AuthorizationMismatch)?
                .drain(..),
        );
        for connection in connections {
            connection.disarm_responders();
            connection.retire();
            let result = connection.close_owned(store).await;
            if failure.is_none() {
                failure = result.err();
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl ReadyConnectionRegistry {
    fn drain(&self) -> Vec<Arc<ReadyConnection>> {
        self.connections
            .write()
            .map(|mut connections| {
                std::mem::take(&mut *connections)
                    .into_values()
                    .map(|registered| registered.connection)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn register(&self, connection: Arc<ReadyConnection>) -> Result<(), TransportError> {
        let mut connections = self
            .connections
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        let server = connection.configured_server.as_str().to_owned();
        if connections.contains_key(&server) {
            return Err(TransportError::AuthorizationMismatch);
        }
        let generation = self.next_generation()?;
        connections.insert(
            server,
            RegisteredConnection {
                generation,
                connection,
            },
        );
        Ok(())
    }

    pub fn replace(
        &self,
        connection: Arc<ReadyConnection>,
    ) -> Result<(u64, Option<Arc<ReadyConnection>>), TransportError> {
        let generation = self.next_generation()?;
        let server = connection.configured_server.as_str().to_owned();
        let replaced = self
            .connections
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .insert(
                server,
                RegisteredConnection {
                    generation,
                    connection,
                },
            )
            .map(|registered| registered.connection);
        if let Some(replaced) = &replaced {
            replaced.disarm_responders();
            replaced.retire();
        }
        Ok((generation, replaced))
    }

    pub fn remove(
        &self,
        server: &ConfiguredServerIdentity,
        generation: u64,
    ) -> Result<Option<Arc<ReadyConnection>>, TransportError> {
        let mut connections = self
            .connections
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        if connections
            .get(server.as_str())
            .is_none_or(|registered| registered.generation != generation)
        {
            return Ok(None);
        }
        let removed = connections
            .remove(server.as_str())
            .map(|registered| registered.connection);
        if let Some(removed) = &removed {
            removed.retire();
        }
        Ok(removed)
    }

    pub fn generation(
        &self,
        server: &ConfiguredServerIdentity,
    ) -> Result<Option<u64>, TransportError> {
        Ok(self
            .connections
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .get(server.as_str())
            .map(|registered| registered.generation))
    }

    fn get(&self, server: &str) -> Result<Arc<ReadyConnection>, TransportError> {
        self.connections
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .get(server)
            .map(|registered| Arc::clone(&registered.connection))
            .ok_or(TransportError::AuthorizationMismatch)
    }

    fn get_registered(&self, server: &str) -> Result<(u64, Arc<ReadyConnection>), TransportError> {
        self.connections
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .get(server)
            .map(|registered| (registered.generation, Arc::clone(&registered.connection)))
            .ok_or(TransportError::AuthorizationMismatch)
    }

    fn next_generation(&self) -> Result<u64, TransportError> {
        self.generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
            .map(|generation| generation + 1)
            .map_err(|_| TransportError::AuthorizationMismatch)
    }
}

impl ReadyConnection {
    fn new(
        connection: McpConnection,
        configured_server: ConfiguredServerIdentity,
        limits: TransportLimits,
        operations: Arc<OperationGate>,
        cleanup: Option<Arc<dyn McpHttpClient>>,
        stdio_reaper: Option<Arc<dyn OwnedStdioProcess>>,
        can_reinitialize: bool,
    ) -> Result<Self, TransportError> {
        if connection.negotiated_protocol_version().as_ref() != Some(&PROTOCOL_REVISION) {
            return Err(TransportError::ProtocolVersionRefused);
        }
        let negotiated = feature_kinds(connection.capabilities());
        operations.set_notification_support(&negotiated)?;
        let negotiated = Arc::new(RwLock::new(negotiated));
        operations.ready.store(true, Ordering::Release);
        Ok(Self {
            connection,
            configured_server,
            request_timeout: limits.request_timeout(),
            close_timeout: limits.close_timeout(),
            payload_limits: limits.payload_limits(),
            operations,
            cleanup,
            stdio_reaper,
            can_reinitialize,
            negotiated,
            retired: AtomicBool::new(false),
            serial: OperationQueue::new(),
            lifecycle_authority: None,
            responders: None,
        })
    }

    fn with_lifecycle_authority(mut self, request: &BrokerInvocation<'_>) -> Self {
        self.lifecycle_authority = Some(Arc::new(LifecycleAuthority::capture(request)));
        self
    }

    pub(crate) fn with_responders(
        mut self,
        responders: crate::protocols::mcp::responders::ResponderInstallation,
    ) -> Self {
        self.operations
            .set_responder_scanner(responders.secret_scanner());
        self.responders = Some(responders);
        self
    }

    fn arm_responders(&self) {
        if let Some(responders) = &self.responders {
            responders.arm();
        }
    }

    fn disarm_responders(&self) {
        if let Some(responders) = &self.responders {
            responders.disarm();
        }
    }

    fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.operations.clear_response_scanners();
    }

    #[cfg(test)]
    fn set_lifecycle_authority(&mut self, request: &BrokerInvocation<'_>) {
        self.lifecycle_authority = Some(Arc::new(LifecycleAuthority::capture(request)));
    }

    fn lifecycle_request(&self) -> Result<Arc<OwnedBrokerInvocation>, TransportError> {
        self.lifecycle_authority
            .as_ref()
            .ok_or(TransportError::AuthorizationMismatch)?
            .mint()
            .map(Arc::new)
            .map_err(Into::into)
    }

    pub fn negotiated_protocol_version(&self) -> McpProtocolVersion {
        PROTOCOL_REVISION
    }

    pub fn negotiated_feature_kinds(&self) -> NegotiatedFeatureKinds {
        self.negotiated
            .read()
            .expect("MCP negotiated capabilities lock poisoned")
            .clone()
    }

    pub fn subscribe_feature_events(&self) -> tokio::sync::broadcast::Receiver<McpServerEvent> {
        self.connection.subscribe_events()
    }

    pub fn refresh_driver(&self, limits: RefreshLimits) -> McpRefreshDriver {
        McpRefreshDriver {
            events: self.subscribe_feature_events(),
            server: self.configured_server.clone(),
            negotiated: Arc::clone(&self.negotiated),
            coalescer: RefreshCoalescer::new(limits),
            started: Instant::now(),
        }
    }

    fn authorize(
        &self,
        request: &BrokerInvocation<'_>,
        operation: &str,
        arguments: Value,
        store: &mut SqliteStore,
    ) -> Result<(transport_auth::TransportDispatch, u64), TransportError> {
        self.authorize_inner(request, operation, arguments, store, false)
    }

    fn authorize_inner(
        &self,
        request: &BrokerInvocation<'_>,
        operation: &str,
        arguments: Value,
        store: &mut SqliteStore,
        allow_retired: bool,
    ) -> Result<(transport_auth::TransportDispatch, u64), TransportError> {
        if !allow_retired && self.retired.load(Ordering::Acquire) {
            return Err(TransportError::ConnectionRetired);
        }
        let operation = transport_auth::TransportOperation::parse(operation)?;
        let argument_bytes =
            serde_json::to_vec(&arguments).map_err(|_| BrokerError::InvalidArguments)?;
        let binding = self.operations.binding()?.with_request(request);
        let (authorization, replay) =
            authorize_ready_operation(request, &operation, &binding, &argument_bytes, store)?;
        let generation = self.operations.install(authorization)?;
        let dispatch =
            match transport_auth::begin_dispatch(request, &operation, &binding, replay, store) {
                Ok(dispatch) => dispatch,
                Err(error) => {
                    self.operations.clear_generation(generation)?;
                    return Err(error.into());
                }
            };
        Ok((dispatch, generation))
    }

    fn finish_operation<T>(
        &self,
        result: Result<T, McpError>,
        dispatch: transport_auth::TransportDispatch,
        generation: u64,
        request: &BrokerInvocation<'_>,
        store: &mut SqliteStore,
    ) -> Result<(T, Option<RawPayload>), TransportError> {
        match result {
            Ok(value) => {
                let cancelled = request.cancelled() && !request.lifecycle_shutdown();
                let persisted = transport_auth::finish_dispatch(
                    request,
                    dispatch,
                    if cancelled {
                        transport_auth::TransportDispatchOutcome::OutcomeUnknown
                    } else {
                        transport_auth::TransportDispatchOutcome::Completed
                    },
                    store,
                );
                let capture = self.operations.clear_generation(generation);
                persisted?;
                let capture = capture?;
                if cancelled {
                    Err(TransportError::Cancelled)
                } else {
                    Ok((value, capture))
                }
            }
            Err(McpError::AuthRequired(challenge)) => {
                let (kind, operation, scope) = match http::auth_challenge(&challenge) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        let persisted = transport_auth::finish_dispatch(
                            request,
                            dispatch,
                            transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                            store,
                        );
                        let capture = self.operations.clear_generation(generation);
                        persisted?;
                        capture?;
                        return Err(error);
                    }
                };
                let persisted = transport_auth::interrupt_dispatch(
                    request,
                    dispatch,
                    kind,
                    &operation,
                    scope.as_deref(),
                    store,
                );
                let capture = self.operations.clear_generation(generation);
                let challenge = persisted?;
                capture?;
                Err(TransportError::AuthRequired(Box::new(challenge)))
            }
            Err(error) => {
                let typed = self.operations.take_failure();
                let persisted = transport_auth::finish_dispatch(
                    request,
                    dispatch,
                    transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                    store,
                );
                let capture = self.operations.clear_generation(generation);
                persisted?;
                capture?;
                Err(typed.unwrap_or_else(|| error.into()))
            }
        }
    }

    fn finish_timed_operation<T>(
        &self,
        result: Result<Result<T, McpError>, tokio::time::error::Elapsed>,
        dispatch: transport_auth::TransportDispatch,
        generation: u64,
        request: &BrokerInvocation<'_>,
        store: &mut SqliteStore,
        operation: &'static str,
    ) -> Result<(T, Option<RawPayload>), TransportError> {
        match result {
            Ok(result) => self.finish_operation(result, dispatch, generation, request, store),
            Err(_) => {
                let persisted = transport_auth::finish_dispatch(
                    request,
                    dispatch,
                    transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                    store,
                );
                let capture = self.operations.clear_generation(generation);
                persisted?;
                capture?;
                Err(TransportError::Timeout(operation))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_with_session_retry<T, F, Fut>(
        &self,
        result: Result<Result<T, McpError>, tokio::time::error::Elapsed>,
        dispatch: transport_auth::TransportDispatch,
        generation: u64,
        request: &BrokerInvocation<'_>,
        store: &mut SqliteStore,
        operation: &'static str,
        retry: F,
    ) -> Result<(T, Option<RawPayload>), TransportError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, McpError>>,
    {
        if matches!(result, Ok(Err(_)))
            && matches!(
                self.operations.take_failure(),
                Some(TransportError::SessionExpired)
            )
        {
            if request.retry_safety()
                == crate::capabilities::kernel::invoke::RetrySafety::NonIdempotent
            {
                transport_auth::finish_dispatch(
                    request,
                    dispatch,
                    transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                    store,
                )?;
                self.operations.clear_generation(generation)?;
                return Err(TransportError::SessionExpired);
            }
            let authorization = self.operations.current_authorization()?;
            self.operations.clear_generation(generation)?;
            let initialize_arguments = serde_json::to_vec(&self.connection.initialize_arguments())
                .expect("static MCP initialize arguments serialize");
            let initialize_request = request.transport_initialize(&initialize_arguments);
            if let Err(error) = self
                .reinitialize_expired_session_inner(&initialize_request, store)
                .await
            {
                transport_auth::finish_dispatch(
                    request,
                    dispatch,
                    transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                    store,
                )?;
                return Err(error);
            }
            let generation = self.operations.install((*authorization).clone())?;
            return self.finish_timed_operation(
                tokio::time::timeout(self.request_timeout, retry()).await,
                dispatch,
                generation,
                request,
                store,
                operation,
            );
        }
        self.finish_timed_operation(result, dispatch, generation, request, store, operation)
    }

    pub async fn list_tools_page(
        &self,
        request: &BrokerInvocation<'_>,
        cursor: Option<String>,
        store: &mut SqliteStore,
    ) -> Result<FeaturePage<ToolDescriptor>, TransportError> {
        let _permit = self.serial.acquire(request, self.request_timeout).await?;
        let arguments = cursor.as_ref().map_or_else(
            || serde_json::json!({}),
            |cursor| serde_json::json!({"cursor": cursor}),
        );
        let (dispatch, generation) = self.authorize(request, "tools/list", arguments, store)?;
        let result = tokio::time::timeout(
            self.request_timeout,
            self.connection.list_tools_page(cursor),
        )
        .await;
        let (_, payload) = self.finish_timed_operation(
            result,
            dispatch,
            generation,
            request,
            store,
            "tools/list response",
        )?;
        let payload = payload.ok_or(TransportError::MissingPayload)?;
        Ok(decode_tools_page(
            &self.configured_server,
            payload.source_bytes(),
            self.payload_limits,
        )?)
    }

    pub async fn list_resources_page(
        &self,
        request: &BrokerInvocation<'_>,
        cursor: Option<String>,
        store: &mut SqliteStore,
    ) -> Result<FeaturePage<ResourceDescriptor>, TransportError> {
        let _permit = self.serial.acquire(request, self.request_timeout).await?;
        let arguments = cursor.as_ref().map_or_else(
            || serde_json::json!({}),
            |cursor| serde_json::json!({"cursor": cursor}),
        );
        let (dispatch, generation) = self.authorize(request, "resources/list", arguments, store)?;
        let result = tokio::time::timeout(
            self.request_timeout,
            self.connection.list_resources_page(cursor),
        )
        .await;
        let (_, payload) = self.finish_timed_operation(
            result,
            dispatch,
            generation,
            request,
            store,
            "resources/list response",
        )?;
        let payload = payload.ok_or(TransportError::MissingPayload)?;
        Ok(decode_resources_page(
            &self.configured_server,
            payload.source_bytes(),
            self.payload_limits,
        )?)
    }

    pub async fn list_resource_templates_page(
        &self,
        request: &BrokerInvocation<'_>,
        cursor: Option<String>,
        store: &mut SqliteStore,
    ) -> Result<FeaturePage<ResourceTemplateDescriptor>, TransportError> {
        let _permit = self.serial.acquire(request, self.request_timeout).await?;
        let arguments = cursor.as_ref().map_or_else(
            || serde_json::json!({}),
            |cursor| serde_json::json!({"cursor": cursor}),
        );
        let (dispatch, generation) =
            self.authorize(request, "resources/templates/list", arguments, store)?;
        let result = tokio::time::timeout(
            self.request_timeout,
            self.connection.list_resource_templates_page(cursor),
        )
        .await;
        let (_, payload) = self.finish_timed_operation(
            result,
            dispatch,
            generation,
            request,
            store,
            "resources/templates/list response",
        )?;
        let payload = payload.ok_or(TransportError::MissingPayload)?;
        Ok(decode_resource_templates_page(
            &self.configured_server,
            payload.source_bytes(),
            self.payload_limits,
        )?)
    }

    pub async fn list_prompts_page(
        &self,
        request: &BrokerInvocation<'_>,
        cursor: Option<String>,
        store: &mut SqliteStore,
    ) -> Result<FeaturePage<PromptDescriptor>, TransportError> {
        let _permit = self.serial.acquire(request, self.request_timeout).await?;
        let arguments = cursor.as_ref().map_or_else(
            || serde_json::json!({}),
            |cursor| serde_json::json!({"cursor": cursor}),
        );
        let (dispatch, generation) = self.authorize(request, "prompts/list", arguments, store)?;
        let result = tokio::time::timeout(
            self.request_timeout,
            self.connection.list_prompts_page(cursor),
        )
        .await;
        let (_, payload) = self.finish_timed_operation(
            result,
            dispatch,
            generation,
            request,
            store,
            "prompts/list response",
        )?;
        let payload = payload.ok_or(TransportError::MissingPayload)?;
        Ok(decode_prompts_page(
            &self.configured_server,
            payload.source_bytes(),
            self.payload_limits,
        )?)
    }

    pub async fn discover_features<'a, F>(
        &self,
        authorize_page: &mut F,
        store: &mut SqliteStore,
    ) -> Result<DiscoveredFeatures, TransportError>
    where
        F: FnMut(&'static str, Option<&str>) -> Result<BrokerInvocation<'a>, TransportError>,
    {
        self.collect_features(self.negotiated_feature_kinds(), authorize_page, store)
            .await
    }

    pub(crate) async fn discover_features_owned(
        &self,
        store: &mut SqliteStore,
    ) -> Result<DiscoveredFeatures, TransportError> {
        let negotiated = self.negotiated_feature_kinds();
        let mut parts = Vec::new();
        for kind in negotiated.iter() {
            parts.push(self.refresh_features_owned(kind, store, || false).await?);
        }
        DiscoveredFeatures::combine(self.configured_server.clone(), negotiated, parts)
            .map_err(Into::into)
    }

    pub async fn refresh_features<'a, F>(
        &self,
        kind: FeatureListKind,
        authorize_page: &mut F,
        store: &mut SqliteStore,
    ) -> Result<DiscoveredFeatures, TransportError>
    where
        F: FnMut(&'static str, Option<&str>) -> Result<BrokerInvocation<'a>, TransportError>,
    {
        if !self.negotiated_feature_kinds().contains(kind) {
            return Err(DiscoveryError::UnnegotiatedKind(kind).into());
        }
        self.collect_features(NegotiatedFeatureKinds::new([kind]), authorize_page, store)
            .await
    }

    async fn refresh_features_owned(
        &self,
        kind: FeatureListKind,
        store: &mut SqliteStore,
        cancelled: impl Fn() -> bool,
    ) -> Result<DiscoveredFeatures, TransportError> {
        if !self.negotiated_feature_kinds().contains(kind) {
            return Err(DiscoveryError::UnnegotiatedKind(kind).into());
        }
        let requested = NegotiatedFeatureKinds::new([kind]);
        let mut pages = 0_usize;
        let mut entries = 0_usize;
        let mut payload_bytes = 0_usize;
        let mut tools = Vec::new();
        let mut resources = Vec::new();
        let mut resource_templates = Vec::new();
        let mut prompts = Vec::new();

        if kind == FeatureListKind::Tools {
            let mut cursor = None::<String>;
            let mut seen = std::collections::BTreeSet::new();
            loop {
                if cancelled() {
                    return Err(TransportError::Cancelled);
                }
                checked_page(&mut pages)?;
                let owner = self.lifecycle_request()?;
                let page = self
                    .list_tools_page(&owner.invocation(), cursor.clone(), store)
                    .await?;
                checked_discovery_page(&page, &mut entries, &mut payload_bytes)?;
                let next = crate::protocols::mcp::features::discovery::validated_next_cursor(
                    cursor.as_deref(),
                    page.next_cursor(),
                    &mut seen,
                )?;
                tools.push(page);
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
        } else if kind == FeatureListKind::Resources {
            let mut cursor = None::<String>;
            let mut seen = std::collections::BTreeSet::new();
            loop {
                if cancelled() {
                    return Err(TransportError::Cancelled);
                }
                checked_page(&mut pages)?;
                let owner = self.lifecycle_request()?;
                let page = self
                    .list_resources_page(&owner.invocation(), cursor.clone(), store)
                    .await?;
                checked_discovery_page(&page, &mut entries, &mut payload_bytes)?;
                let next = crate::protocols::mcp::features::discovery::validated_next_cursor(
                    cursor.as_deref(),
                    page.next_cursor(),
                    &mut seen,
                )?;
                resources.push(page);
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
            let mut cursor = None::<String>;
            let mut seen = std::collections::BTreeSet::new();
            loop {
                if cancelled() {
                    return Err(TransportError::Cancelled);
                }
                checked_page(&mut pages)?;
                let owner = self.lifecycle_request()?;
                let page = self
                    .list_resource_templates_page(&owner.invocation(), cursor.clone(), store)
                    .await?;
                checked_discovery_page(&page, &mut entries, &mut payload_bytes)?;
                let next = crate::protocols::mcp::features::discovery::validated_next_cursor(
                    cursor.as_deref(),
                    page.next_cursor(),
                    &mut seen,
                )?;
                resource_templates.push(page);
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
        } else if kind == FeatureListKind::Prompts {
            let mut cursor = None::<String>;
            let mut seen = std::collections::BTreeSet::new();
            loop {
                if cancelled() {
                    return Err(TransportError::Cancelled);
                }
                checked_page(&mut pages)?;
                let owner = self.lifecycle_request()?;
                let page = self
                    .list_prompts_page(&owner.invocation(), cursor.clone(), store)
                    .await?;
                checked_discovery_page(&page, &mut entries, &mut payload_bytes)?;
                let next = crate::protocols::mcp::features::discovery::validated_next_cursor(
                    cursor.as_deref(),
                    page.next_cursor(),
                    &mut seen,
                )?;
                prompts.push(page);
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        if cancelled() {
            return Err(TransportError::Cancelled);
        }
        Ok(DiscoveredFeatures::from_pages(
            self.configured_server.clone(),
            requested,
            tools,
            resources,
            resource_templates,
            prompts,
        )?)
    }

    async fn refresh_features_until<'a, F, C>(
        &self,
        kind: FeatureListKind,
        authorize_page: &mut F,
        store: &mut SqliteStore,
        cancelled: C,
    ) -> Result<DiscoveredFeatures, TransportError>
    where
        F: FnMut(&'static str, Option<&str>) -> Result<BrokerInvocation<'a>, TransportError>,
        C: Fn() -> bool,
    {
        if !self.negotiated_feature_kinds().contains(kind) {
            return Err(DiscoveryError::UnnegotiatedKind(kind).into());
        }
        self.collect_features_until(
            NegotiatedFeatureKinds::new([kind]),
            authorize_page,
            store,
            cancelled,
        )
        .await
    }

    async fn collect_features<'a, F>(
        &self,
        requested: NegotiatedFeatureKinds,
        authorize_page: &mut F,
        store: &mut SqliteStore,
    ) -> Result<DiscoveredFeatures, TransportError>
    where
        F: FnMut(&'static str, Option<&str>) -> Result<BrokerInvocation<'a>, TransportError>,
    {
        self.collect_features_until(requested, authorize_page, store, || false)
            .await
    }

    async fn collect_features_until<'a, F, C>(
        &self,
        requested: NegotiatedFeatureKinds,
        authorize_page: &mut F,
        store: &mut SqliteStore,
        cancelled: C,
    ) -> Result<DiscoveredFeatures, TransportError>
    where
        F: FnMut(&'static str, Option<&str>) -> Result<BrokerInvocation<'a>, TransportError>,
        C: Fn() -> bool,
    {
        let mut pages = 0_usize;
        let mut entries = 0_usize;
        let mut payload_bytes = 0_usize;
        let mut tools = Vec::new();
        let mut resources = Vec::new();
        let mut resource_templates = Vec::new();
        let mut prompts = Vec::new();

        if requested.contains(FeatureListKind::Tools) {
            let mut cursor = None::<String>;
            let mut seen = std::collections::BTreeSet::new();
            loop {
                if cancelled() {
                    return Err(TransportError::Cancelled);
                }
                checked_page(&mut pages)?;
                let request = authorize_page("tools/list", cursor.as_deref())?;
                let page = self.list_tools_page(&request, cursor.clone(), store).await;
                if cancelled() {
                    return Err(TransportError::Cancelled);
                }
                let page = page?;
                checked_discovery_page(&page, &mut entries, &mut payload_bytes)?;
                let next = crate::protocols::mcp::features::discovery::validated_next_cursor(
                    cursor.as_deref(),
                    page.next_cursor(),
                    &mut seen,
                )?;
                tools.push(page);
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        if requested.contains(FeatureListKind::Resources) {
            let mut cursor = None::<String>;
            let mut seen = std::collections::BTreeSet::new();
            loop {
                if cancelled() {
                    return Err(TransportError::Cancelled);
                }
                checked_page(&mut pages)?;
                let request = authorize_page("resources/list", cursor.as_deref())?;
                let page = self
                    .list_resources_page(&request, cursor.clone(), store)
                    .await;
                if cancelled() {
                    return Err(TransportError::Cancelled);
                }
                let page = page?;
                checked_discovery_page(&page, &mut entries, &mut payload_bytes)?;
                let next = crate::protocols::mcp::features::discovery::validated_next_cursor(
                    cursor.as_deref(),
                    page.next_cursor(),
                    &mut seen,
                )?;
                resources.push(page);
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }

            let mut cursor = None::<String>;
            let mut seen = std::collections::BTreeSet::new();
            loop {
                if cancelled() {
                    return Err(TransportError::Cancelled);
                }
                checked_page(&mut pages)?;
                let request = authorize_page("resources/templates/list", cursor.as_deref())?;
                let page = self
                    .list_resource_templates_page(&request, cursor.clone(), store)
                    .await;
                if cancelled() {
                    return Err(TransportError::Cancelled);
                }
                let page = page?;
                checked_discovery_page(&page, &mut entries, &mut payload_bytes)?;
                let next = crate::protocols::mcp::features::discovery::validated_next_cursor(
                    cursor.as_deref(),
                    page.next_cursor(),
                    &mut seen,
                )?;
                resource_templates.push(page);
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        if requested.contains(FeatureListKind::Prompts) {
            let mut cursor = None::<String>;
            let mut seen = std::collections::BTreeSet::new();
            loop {
                if cancelled() {
                    return Err(TransportError::Cancelled);
                }
                checked_page(&mut pages)?;
                let request = authorize_page("prompts/list", cursor.as_deref())?;
                let page = self
                    .list_prompts_page(&request, cursor.clone(), store)
                    .await;
                if cancelled() {
                    return Err(TransportError::Cancelled);
                }
                let page = page?;
                checked_discovery_page(&page, &mut entries, &mut payload_bytes)?;
                let next = crate::protocols::mcp::features::discovery::validated_next_cursor(
                    cursor.as_deref(),
                    page.next_cursor(),
                    &mut seen,
                )?;
                prompts.push(page);
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        Ok(DiscoveredFeatures::from_pages(
            self.configured_server.clone(),
            requested,
            tools,
            resources,
            resource_templates,
            prompts,
        )?)
    }

    fn finish_interrupted_tool_call(
        &self,
        request: &BrokerInvocation<'_>,
        dispatch: transport_auth::TransportDispatch,
        generation: Option<u64>,
        store: &mut SqliteStore,
        outcome: transport_auth::TransportDispatchOutcome,
        error: TransportError,
    ) -> BoundOperationError {
        let persisted = transport_auth::finish_dispatch(request, dispatch, outcome, store)
            .map_err(TransportError::from);
        let cleared = generation
            .map(|generation| self.operations.clear_generation(generation))
            .transpose();
        BoundOperationError::after_dispatch(
            persisted.err().or_else(|| cleared.err()).unwrap_or(error),
        )
    }

    async fn call_tool_with_url_elicitation(
        &self,
        request: &BrokerInvocation<'_>,
        name: &str,
        arguments: Value,
        store: &mut SqliteStore,
    ) -> Result<CallToolResult, BoundOperationError> {
        let mut wire =
            serde_json::Map::from_iter([("name".to_owned(), Value::String(name.to_owned()))]);
        if !arguments.is_null() {
            wire.insert("arguments".to_owned(), arguments.clone());
        }
        let operation_arguments = Value::Object(wire);
        let (dispatch, generation) = self
            .authorize(request, "tools/call", operation_arguments.clone(), store)
            .map_err(BoundOperationError::before_dispatch)?;
        let first = tokio::time::timeout(
            self.request_timeout,
            self.connection.call_tool(name, arguments.clone()),
        )
        .await;
        let Ok(Err(McpError::Invocation(agentkit_mcp::McpInvocationError::UrlElicitation {
            message,
            data: Some(data),
            raw_data,
        }))) = first
        else {
            return self
                .finish_with_session_retry(
                    first,
                    dispatch,
                    generation,
                    request,
                    store,
                    "tools/call response",
                    || self.connection.call_tool(name, arguments),
                )
                .await
                .map(|(value, _)| value)
                .map_err(BoundOperationError::after_dispatch);
        };
        let Some(responders) = self.responders.as_ref() else {
            return Err(self.finish_interrupted_tool_call(
                request,
                dispatch,
                Some(generation),
                store,
                transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                TransportError::UrlElicitationUnavailable,
            ));
        };
        let error_response_digest = match self.operations.exact_terminal_url_elicitation_digest(
            generation,
            &message,
            raw_data.as_ref(),
        ) {
            Ok(digest) => digest,
            Err(error) => {
                return Err(self.finish_interrupted_tool_call(
                    request,
                    dispatch,
                    Some(generation),
                    store,
                    transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                    error,
                ));
            }
        };
        let resolution = match responders
            .await_invocation_url(
                request,
                generation,
                error_response_digest,
                &message,
                &data.url,
                &data.elicitation_id,
                raw_data.as_ref(),
            )
            .await
        {
            Ok(resolution) => resolution,
            Err(_) => {
                return Err(self.finish_interrupted_tool_call(
                    request,
                    dispatch,
                    Some(generation),
                    store,
                    transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                    TransportError::UrlElicitationUnavailable,
                ));
            }
        };
        if resolution.action != crate::domain::mcp_callback::McpCallbackAction::Accept {
            return Err(self.finish_interrupted_tool_call(
                request,
                dispatch,
                Some(generation),
                store,
                transport_auth::TransportDispatchOutcome::Completed,
                TransportError::UrlElicitationDeclined,
            ));
        }
        if request.retry_safety() == crate::capabilities::kernel::invoke::RetrySafety::NonIdempotent
            || !resolution.authorizes_retry(
                request,
                self.configured_server.as_str(),
                generation,
                error_response_digest,
                &data.url,
            )
        {
            resolution.finish(false);
            return Err(self.finish_interrupted_tool_call(
                request,
                dispatch,
                Some(generation),
                store,
                transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                TransportError::UrlElicitationUnavailable,
            ));
        }
        if let Err(error) = request.preflight_transport_retry(store) {
            resolution.finish(false);
            return Err(self.finish_interrupted_tool_call(
                request,
                dispatch,
                Some(generation),
                store,
                transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                error.into(),
            ));
        }
        let operation = transport_auth::TransportOperation::parse("tools/call")
            .map_err(|error| BoundOperationError::after_dispatch(error.into()))?;
        let argument_bytes = serde_json::to_vec(&operation_arguments).map_err(|_| {
            BoundOperationError::after_dispatch(TransportError::AuthorizationMismatch)
        })?;
        let binding = self
            .operations
            .binding()
            .map_err(BoundOperationError::after_dispatch)?
            .with_request(request);
        let authorization = match authorize_ready_operation(
            request,
            &operation,
            &binding,
            &argument_bytes,
            store,
        ) {
            Ok((authorization, _)) => authorization,
            Err(error) => {
                resolution.finish(false);
                return Err(self.finish_interrupted_tool_call(
                    request,
                    dispatch,
                    Some(generation),
                    store,
                    transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                    error,
                ));
            }
        };
        if let Err(error) = self.operations.clear_generation(generation) {
            resolution.finish(false);
            return Err(self.finish_interrupted_tool_call(
                request,
                dispatch,
                Some(generation),
                store,
                transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                error,
            ));
        }
        let generation = match self.operations.install(authorization) {
            Ok(generation) => generation,
            Err(error) => {
                resolution.finish(false);
                return Err(self.finish_interrupted_tool_call(
                    request,
                    dispatch,
                    None,
                    store,
                    transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                    error,
                ));
            }
        };
        let retry = self
            .finish_timed_operation(
                tokio::time::timeout(
                    self.request_timeout,
                    self.connection.call_tool(name, arguments),
                )
                .await,
                dispatch,
                generation,
                request,
                store,
                "tools/call response",
            )
            .map(|(value, _)| value)
            .map_err(BoundOperationError::after_dispatch);
        resolution.finish(retry.is_ok());
        retry
    }

    pub async fn invoke_bound(
        &self,
        request: BrokerInvocation<'_>,
        store: &mut SqliteStore,
        budget: &BudgetLedger,
        artifacts: &ArtifactStore,
        policy: &McpResultPolicy,
    ) -> Result<BrokerOutcome, TransportError> {
        let _permit = self.serial.acquire(&request, self.request_timeout).await?;
        self.invoke_bound_inner(request, store, budget, artifacts, policy, None)
            .await
    }

    #[cfg(debug_assertions)]
    pub async fn invoke_bound_with_crash_at(
        &self,
        request: BrokerInvocation<'_>,
        store: &mut SqliteStore,
        budget: &BudgetLedger,
        artifacts: &ArtifactStore,
        policy: &McpResultPolicy,
        crash_at: InvocationCrashPoint,
    ) -> Result<BrokerOutcome, TransportError> {
        let _permit = self.serial.acquire(&request, self.request_timeout).await?;
        self.invoke_bound_inner(request, store, budget, artifacts, policy, Some(crash_at))
            .await
    }

    async fn invoke_bound_inner(
        &self,
        request: BrokerInvocation<'_>,
        store: &mut SqliteStore,
        budget: &BudgetLedger,
        artifacts: &ArtifactStore,
        policy: &McpResultPolicy,
        crash_at: Option<InvocationCrashPoint>,
    ) -> Result<BrokerOutcome, TransportError> {
        let binding = request.binding().ok_or(BrokerError::BindingMismatch)?;
        if McpOperation::configured_server(binding)? != self.configured_server.as_str() {
            return Err(BrokerError::BindingMismatch.into());
        }
        let input = serde_json::from_slice(request.arguments())
            .map_err(|_| BrokerError::InvalidArguments)?;
        let operation =
            McpOperation::from_binding(binding, &input, self.payload_limits.max_bytes())?;
        let transport_operation = transport_auth::TransportOperation::parse(operation.method())?;
        let transport_binding = self.operations.binding()?;
        // Catalog state is not replay authority. The broker first verifies the
        // current principal/run/workspace and persisted artifact ownership.
        if let Some(result) = broker::replay(&request, store, budget, artifacts)? {
            return Ok(BrokerOutcome::Completed(result));
        }
        #[derive(Clone, Copy)]
        enum ResumeState {
            Fresh,
            Resume,
            Denied,
            Unknown,
        }
        let resume = match transport_auth::state(&request, &transport_binding, store) {
            Ok(TransportAuthState::Absent) | Err(BrokerError::AuthNotRequired) => {
                ResumeState::Fresh
            }
            Ok(TransportAuthState::Pending(challenge)) => {
                if challenge.operation != transport_operation {
                    return Err(BrokerError::ReplayNotAuthorized.into());
                }
                return Ok(BrokerOutcome::AuthRequired(challenge.challenge));
            }
            Ok(TransportAuthState::Granted(challenge)) => {
                if challenge.operation != transport_operation {
                    return Err(BrokerError::ReplayNotAuthorized.into());
                }
                ResumeState::Resume
            }
            Ok(TransportAuthState::Denied) => ResumeState::Denied,
            Ok(TransportAuthState::Replayed) => ResumeState::Unknown,
            Err(_) => ResumeState::Unknown,
        };
        let prepared = match if matches!(resume, ResumeState::Fresh) {
            broker::prepare(&request, store, budget, crash_at)
        } else {
            broker::prepare_resuming_transport(&request, store, budget, crash_at)
        }? {
            BrokerPrepareOutcome::Authorized(prepared) => *prepared,
            BrokerPrepareOutcome::Completed(result) => {
                return Ok(BrokerOutcome::Completed(*result));
            }
            BrokerPrepareOutcome::AuthRequired(challenge) => {
                return Ok(BrokerOutcome::AuthRequired(*challenge));
            }
        };
        if matches!(resume, ResumeState::Denied | ResumeState::Unknown) {
            return broker::complete(
                &request,
                prepared,
                if matches!(resume, ResumeState::Denied) {
                    crate::capabilities::kernel::invoke::DispatchOutcome::Failed {
                        code: "mcp.auth_denied".to_owned(),
                    }
                } else {
                    crate::capabilities::kernel::invoke::DispatchOutcome::OutcomeUnknown {
                        code: "mcp.auth_replay_state_unknown".to_owned(),
                    }
                },
                store,
                budget,
                crash_at,
            )
            .map(BrokerOutcome::Completed)
            .map_err(Into::into);
        }
        enum Completion {
            Kernel(crate::capabilities::kernel::invoke::DispatchOutcome),
            External(NormalizedMcpResult),
        }
        let completion = if prepared.kernel().arguments() != request.arguments() {
            Completion::Kernel(
                crate::capabilities::kernel::invoke::DispatchOutcome::Failed {
                    code: "mcp.binding_mismatch".to_owned(),
                },
            )
        } else if matches!(resume, ResumeState::Resume)
            && request.retry_safety()
                == crate::capabilities::kernel::invoke::RetrySafety::NonIdempotent
        {
            Completion::Kernel(
                crate::capabilities::kernel::invoke::DispatchOutcome::OutcomeUnknown {
                    code: "mcp.non_idempotent_auth_interrupted".to_owned(),
                },
            )
        } else {
            let result = match &operation {
                McpOperation::Tool { name, arguments } => self
                    .call_tool_with_url_elicitation(&request, name, arguments.clone(), store)
                    .await
                    .map(McpInvocationResult::Tool),
                McpOperation::Resource { uri } => self
                    .read_resource(&request, uri, store)
                    .await
                    .map(McpInvocationResult::Resource),
                McpOperation::Prompt { name, arguments } => self
                    .get_prompt(&request, name, arguments.clone(), store)
                    .await
                    .map(McpInvocationResult::Prompt),
            };
            match result {
                Ok(result) => match prepared.result_authority() {
                    Some(authority) => match normalize_invocation_result(
                        binding,
                        result.as_typed(),
                        artifacts,
                        authority,
                        policy,
                    ) {
                        Ok(normalized) => Completion::External(normalized),
                        Err(_) => Completion::Kernel(
                            crate::capabilities::kernel::invoke::DispatchOutcome::Failed {
                                code: "mcp.result_normalization_failed".to_owned(),
                            },
                        ),
                    },
                    None => Completion::Kernel(
                        crate::capabilities::kernel::invoke::DispatchOutcome::Failed {
                            code: "mcp.result_authority_missing".to_owned(),
                        },
                    ),
                },
                Err(BoundOperationError {
                    error: TransportError::AuthRequired(challenge),
                    ..
                }) => return Ok(BrokerOutcome::AuthRequired(challenge.challenge)),
                Err(BoundOperationError {
                    error: TransportError::BindingExpired,
                    ..
                }) => Completion::Kernel(
                    crate::capabilities::kernel::invoke::DispatchOutcome::Failed {
                        code: "mcp.stale_binding".to_owned(),
                    },
                ),
                Err(error) => Completion::Kernel(if error.dispatched {
                    crate::capabilities::kernel::invoke::DispatchOutcome::OutcomeUnknown {
                        code: error.error.completion_code().to_owned(),
                    }
                } else {
                    crate::capabilities::kernel::invoke::DispatchOutcome::Failed {
                        code: error.error.completion_code().to_owned(),
                    }
                }),
            }
        };
        match completion {
            Completion::Kernel(dispatched) => {
                broker::complete(&request, prepared, dispatched, store, budget, crash_at)
            }
            Completion::External(normalized) => broker::complete_external(
                &request,
                prepared,
                (
                    normalized.dispatch_outcome(),
                    normalized.presentation().clone(),
                    normalized.publication().clone(),
                ),
                artifacts,
                store,
                budget,
                crash_at,
            ),
        }
        .map(BrokerOutcome::Completed)
        .map_err(Into::into)
    }

    async fn read_resource(
        &self,
        request: &BrokerInvocation<'_>,
        uri: &str,
        store: &mut SqliteStore,
    ) -> Result<ReadResourceResult, BoundOperationError> {
        let (dispatch, generation) = self
            .authorize(
                request,
                "resources/read",
                serde_json::json!({"uri": uri}),
                store,
            )
            .map_err(BoundOperationError::before_dispatch)?;
        let result =
            tokio::time::timeout(self.request_timeout, self.connection.read_resource(uri)).await;
        self.finish_with_session_retry(
            result,
            dispatch,
            generation,
            request,
            store,
            "resources/read response",
            || self.connection.read_resource(uri),
        )
        .await
        .map(|(value, _)| value)
        .map_err(BoundOperationError::after_dispatch)
    }

    async fn get_prompt(
        &self,
        request: &BrokerInvocation<'_>,
        name: &str,
        arguments: Value,
        store: &mut SqliteStore,
    ) -> Result<GetPromptResult, BoundOperationError> {
        let mut wire =
            serde_json::Map::from_iter([("name".to_owned(), Value::String(name.to_owned()))]);
        if !arguments.is_null() {
            wire.insert("arguments".to_owned(), arguments.clone());
        }
        let (dispatch, generation) = self
            .authorize(request, "prompts/get", Value::Object(wire), store)
            .map_err(BoundOperationError::before_dispatch)?;
        let result = tokio::time::timeout(
            self.request_timeout,
            self.connection.get_prompt(name, arguments.clone()),
        )
        .await;
        self.finish_with_session_retry(
            result,
            dispatch,
            generation,
            request,
            store,
            "prompts/get response",
            || self.connection.get_prompt(name, arguments),
        )
        .await
        .map(|(value, _)| value)
        .map_err(BoundOperationError::after_dispatch)
    }

    pub async fn reinitialize_expired_session(
        &self,
        request: &BrokerInvocation<'_>,
        store: &mut SqliteStore,
    ) -> Result<(), TransportError> {
        let _permit = self.serial.acquire(request, self.request_timeout).await?;
        self.reinitialize_expired_session_inner(request, store)
            .await
    }

    async fn reinitialize_expired_session_inner(
        &self,
        request: &BrokerInvocation<'_>,
        store: &mut SqliteStore,
    ) -> Result<(), TransportError> {
        if !self.can_reinitialize {
            return Err(TransportError::AuthorizationMismatch);
        }
        validate_initialize_arguments(request)?;
        let arguments = self.connection.initialize_arguments();
        let (dispatch, generation) = self.authorize(request, "initialize", arguments, store)?;
        let result = tokio::time::timeout(
            self.request_timeout,
            self.connection.begin_reinitialize_authorized(),
        )
        .await;
        let (candidate, _) = self.finish_timed_operation(
            result,
            dispatch,
            generation,
            request,
            store,
            "initialize response",
        )?;
        if let Err(error) = self
            .validate_reinitialized_binding(request, store, &candidate)
            .await
        {
            candidate.abort().await?;
            return Err(error);
        }
        let negotiated = feature_kinds(candidate.capabilities());
        candidate.commit(|| self.arm_responders()).await;
        self.operations.set_notification_support(&negotiated)?;
        *self
            .negotiated
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)? = negotiated;
        Ok(())
    }

    async fn validate_reinitialized_binding(
        &self,
        request: &BrokerInvocation<'_>,
        store: &mut SqliteStore,
        candidate: &McpAuthorizedReinitializeCandidate<'_>,
    ) -> Result<(), TransportError> {
        let binding = request.binding().ok_or(BrokerError::BindingMismatch)?;
        let target = binding
            .pinned_entry()
            .external_target()
            .ok_or(BrokerError::BindingMismatch)?;
        let owner = self.lifecycle_request()?;
        let owner = owner.invocation();
        let mut cursor = None::<String>;
        let mut seen = std::collections::BTreeSet::new();
        loop {
            let arguments = cursor.as_ref().map_or_else(
                || serde_json::json!({}),
                |cursor| serde_json::json!({"cursor": cursor}),
            );
            let method = match target.kind() {
                crate::capabilities::catalog::CapabilityKind::Tool => "tools/list",
                crate::capabilities::catalog::CapabilityKind::Resource => {
                    if target.remote().contains("{") {
                        "resources/templates/list"
                    } else {
                        "resources/list"
                    }
                }
                crate::capabilities::catalog::CapabilityKind::Prompt => "prompts/list",
                crate::capabilities::catalog::CapabilityKind::ResourceTemplate => {
                    "resources/templates/list"
                }
            };
            let (dispatch, generation) = self.authorize(&owner, method, arguments, store)?;
            let payload = match target.kind() {
                crate::capabilities::catalog::CapabilityKind::Tool => {
                    let result = tokio::time::timeout(
                        self.request_timeout,
                        candidate.list_tools_page(cursor.clone()),
                    )
                    .await;
                    self.finish_timed_operation(
                        result,
                        dispatch,
                        generation,
                        &owner,
                        store,
                        "tools/list response",
                    )?
                    .1
                }
                crate::capabilities::catalog::CapabilityKind::Resource => {
                    let result = tokio::time::timeout(
                        self.request_timeout,
                        candidate.list_resources_page(cursor.clone()),
                    )
                    .await;
                    self.finish_timed_operation(
                        result,
                        dispatch,
                        generation,
                        &owner,
                        store,
                        "resources/list response",
                    )?
                    .1
                }
                crate::capabilities::catalog::CapabilityKind::ResourceTemplate => {
                    let result = tokio::time::timeout(
                        self.request_timeout,
                        candidate.list_resource_templates_page(cursor.clone()),
                    )
                    .await;
                    self.finish_timed_operation(
                        result,
                        dispatch,
                        generation,
                        &owner,
                        store,
                        "resource templates/list response",
                    )?
                    .1
                }
                crate::capabilities::catalog::CapabilityKind::Prompt => {
                    let result = tokio::time::timeout(
                        self.request_timeout,
                        candidate.list_prompts_page(cursor.clone()),
                    )
                    .await;
                    self.finish_timed_operation(
                        result,
                        dispatch,
                        generation,
                        &owner,
                        store,
                        "prompts/list response",
                    )?
                    .1
                }
            }
            .ok_or(TransportError::MissingPayload)?;
            let (found, next) = match target.kind() {
                crate::capabilities::catalog::CapabilityKind::Tool => {
                    let page = decode_tools_page(
                        &self.configured_server,
                        payload.source_bytes(),
                        self.payload_limits,
                    )?;
                    (
                        page.items().iter().any(|descriptor| {
                            descriptor.name() == target.remote()
                                && descriptor.normalize().is_ok_and(|feature| {
                                    feature.descriptor_digest() == target.descriptor_digest()
                                })
                        }),
                        page.next_cursor().map(str::to_owned),
                    )
                }
                crate::capabilities::catalog::CapabilityKind::Resource => {
                    let page = decode_resources_page(
                        &self.configured_server,
                        payload.source_bytes(),
                        self.payload_limits,
                    )?;
                    (
                        page.items().iter().any(|descriptor| {
                            descriptor.uri() == target.remote()
                                && descriptor.normalize().is_ok_and(|feature| {
                                    feature.descriptor_digest() == target.descriptor_digest()
                                })
                        }),
                        page.next_cursor().map(str::to_owned),
                    )
                }
                crate::capabilities::catalog::CapabilityKind::ResourceTemplate => {
                    let page = decode_resource_templates_page(
                        &self.configured_server,
                        payload.source_bytes(),
                        self.payload_limits,
                    )?;
                    (
                        page.items().iter().any(|descriptor| {
                            descriptor.uri_template() == target.remote()
                                && descriptor.normalize().is_ok_and(|feature| {
                                    feature.descriptor_digest() == target.descriptor_digest()
                                })
                        }),
                        page.next_cursor().map(str::to_owned),
                    )
                }
                crate::capabilities::catalog::CapabilityKind::Prompt => {
                    let page = decode_prompts_page(
                        &self.configured_server,
                        payload.source_bytes(),
                        self.payload_limits,
                    )?;
                    (
                        page.items().iter().any(|descriptor| {
                            descriptor.name() == target.remote()
                                && descriptor.normalize().is_ok_and(|feature| {
                                    feature.descriptor_digest() == target.descriptor_digest()
                                })
                        }),
                        page.next_cursor().map(str::to_owned),
                    )
                }
            };
            if found {
                return Ok(());
            }
            cursor = crate::protocols::mcp::features::discovery::validated_next_cursor(
                cursor.as_deref(),
                next.as_deref(),
                &mut seen,
            )?;
            if cursor.is_none() {
                return Err(TransportError::BindingExpired);
            }
        }
    }

    pub async fn close(
        &self,
        request: &BrokerInvocation<'_>,
        store: &mut SqliteStore,
    ) -> Result<(), TransportError> {
        self.operations.clear_response_scanners();
        self.disarm_responders();
        let _permit = self.serial.acquire(request, self.close_timeout).await?;
        let (dispatch, generation) = self.authorize_inner(
            request,
            "session/delete",
            serde_json::json!({}),
            store,
            true,
        )?;
        let deadline = tokio::time::Instant::now() + self.close_timeout;
        let service_deadline = if self.cleanup.is_some() {
            tokio::time::Instant::now() + self.close_timeout / 2
        } else {
            deadline
        };
        let close = tokio::time::timeout_at(service_deadline, self.connection.close()).await;
        let mut result = match close {
            Ok(result) => result,
            Err(_) => Err(McpError::Transport("MCP close timed out".to_owned())),
        };
        if let Some(cleanup) = &self.cleanup {
            let cleanup =
                match tokio::time::timeout_at(deadline, cleanup.close_open_sessions()).await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(McpError::Transport(error.to_string())),
                    Err(_) => Err(McpError::Transport(
                        "MCP session cleanup timed out".to_owned(),
                    )),
                };
            if cleanup.is_err() {
                result = cleanup;
            }
        }
        let result = self
            .finish_operation(result, dispatch, generation, request, store)
            .map(|(value, _)| value);
        self.operations.clear_response_scanners();
        result
    }

    pub(crate) async fn close_owned(&self, store: &mut SqliteStore) -> Result<(), TransportError> {
        self.operations.clear_response_scanners();
        let result = match self.lifecycle_request() {
            Ok(owner) => self.close(&owner.shutdown_invocation(), store).await,
            Err(error) => Err(error),
        };
        let Some(reaper) = &self.stdio_reaper else {
            return result;
        };
        match (
            result,
            tokio::time::timeout(self.close_timeout, reaper.close_and_reap()).await,
        ) {
            (Ok(()), Ok(Ok(()))) => Ok(()),
            (Err(primary), Ok(Ok(()))) => Err(primary),
            (primary, cleanup) => Err(TransportError::Cleanup {
                primary: Box::new(primary.err().unwrap_or_else(|| {
                    TransportError::Io(io::Error::other("MCP stdio protocol close failed"))
                })),
                cleanup: match cleanup {
                    Ok(Err(error)) => error,
                    Err(_) => {
                        io::Error::new(io::ErrorKind::TimedOut, "MCP stdio cleanup timed out")
                    }
                    Ok(Ok(())) => unreachable!(),
                },
            }),
        }
    }
}

pub struct McpRefreshDriver {
    events: tokio::sync::broadcast::Receiver<McpServerEvent>,
    server: ConfiguredServerIdentity,
    negotiated: Arc<RwLock<NegotiatedFeatureKinds>>,
    coalescer: RefreshCoalescer,
    started: Instant,
}

const MAX_REFRESH_FAILURES: u8 = 5;

impl McpRefreshDriver {
    #[allow(clippy::too_many_arguments)]
    pub async fn run<'a, F>(
        &mut self,
        connection: &ReadyConnection,
        catalog: &Arc<RwLock<McpCatalog>>,
        registry: &ReadyConnectionRegistry,
        lifecycle: &Arc<RwLock<()>>,
        connection_generation: u64,
        authorize_page: &mut F,
        store: &mut SqliteStore,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), TransportError>
    where
        F: FnMut(&'static str, Option<&str>) -> Result<BrokerInvocation<'a>, TransportError>,
    {
        loop {
            let now = self.now_millis();
            for ticket in self.coalescer.take_ready(now) {
                if *shutdown.borrow() {
                    return Ok(());
                }
                if registry.generation(ticket.server())? != Some(connection_generation) {
                    return Ok(());
                }
                if !connection
                    .negotiated_feature_kinds()
                    .supports_list_changed(ticket.kind())
                {
                    self.coalescer.complete(&ticket, true, self.now_millis());
                    continue;
                }
                let refreshed = connection
                    .refresh_features_until(ticket.kind(), authorize_page, store, || {
                        *shutdown.borrow()
                    })
                    .await;
                if matches!(refreshed, Err(TransportError::Cancelled)) {
                    self.coalescer.complete(&ticket, false, self.now_millis());
                    return Ok(());
                }
                let succeeded = match refreshed {
                    Ok(features) => {
                        let published = (|| {
                            let _lifecycle = lifecycle
                                .write()
                                .map_err(|_| TransportError::AuthorizationMismatch)?;
                            if registry.generation(ticket.server())? != Some(connection_generation)
                            {
                                return Ok(false);
                            }
                            catalog
                                .write()
                                .map_err(|_| TransportError::AuthorizationMismatch)?
                                .refresh_kind_until(
                                    ticket.server(),
                                    ticket.kind(),
                                    &features,
                                    || *shutdown.borrow(),
                                )
                                .map_err(TransportError::from)
                        })();
                        match published {
                            Ok(published) => published,
                            Err(error) => {
                                return fail_refresh_closed(
                                    connection,
                                    catalog,
                                    registry,
                                    lifecycle,
                                    connection_generation,
                                    error,
                                );
                            }
                        }
                    }
                    Err(error) if error.transient_refresh() => false,
                    Err(error) => {
                        return fail_refresh_closed(
                            connection,
                            catalog,
                            registry,
                            lifecycle,
                            connection_generation,
                            error,
                        );
                    }
                };
                if *shutdown.borrow() {
                    self.coalescer.complete(&ticket, false, self.now_millis());
                    return Ok(());
                }
                self.coalescer
                    .complete(&ticket, succeeded, self.now_millis());
                if !succeeded && self.coalescer.failures(&ticket) >= MAX_REFRESH_FAILURES {
                    return fail_refresh_closed(
                        connection,
                        catalog,
                        registry,
                        lifecycle,
                        connection_generation,
                        TransportError::RefreshRetriesExhausted,
                    );
                }
            }

            if *shutdown.borrow() {
                return Ok(());
            }
            if let Some(due) = self.coalescer.next_due_millis() {
                let delay = Duration::from_millis(due.saturating_sub(self.now_millis()));
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return Ok(());
                        }
                    }
                    event = self.events.recv() => {
                        if !self.handle_event(event) {
                            return fail_refresh_closed(
                                connection, catalog, registry, lifecycle,
                                connection_generation, TransportError::RefreshClosed,
                            );
                        }
                    }
                    () = tokio::time::sleep(delay) => {}
                }
            } else {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return Ok(());
                        }
                    }
                    event = self.events.recv() => {
                        if !self.handle_event(event) {
                            return fail_refresh_closed(
                                connection, catalog, registry, lifecycle,
                                connection_generation, TransportError::RefreshClosed,
                            );
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_owned(
        &mut self,
        connection: &ReadyConnection,
        catalog: &Arc<RwLock<McpCatalog>>,
        registry: &ReadyConnectionRegistry,
        lifecycle: &Arc<RwLock<()>>,
        connection_generation: u64,
        store: &mut SqliteStore,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), TransportError> {
        loop {
            let now = self.now_millis();
            for ticket in self.coalescer.take_ready(now) {
                if *shutdown.borrow()
                    || registry.generation(ticket.server())? != Some(connection_generation)
                {
                    return Ok(());
                }
                if !connection
                    .negotiated_feature_kinds()
                    .supports_list_changed(ticket.kind())
                {
                    self.coalescer.complete(&ticket, true, self.now_millis());
                    continue;
                }
                let refreshed = connection
                    .refresh_features_owned(ticket.kind(), store, || *shutdown.borrow())
                    .await;
                if matches!(refreshed, Err(TransportError::Cancelled)) {
                    self.coalescer.complete(&ticket, false, self.now_millis());
                    return Ok(());
                }
                let succeeded = match refreshed {
                    Ok(features) => {
                        let published = (|| {
                            let _lifecycle = lifecycle
                                .write()
                                .map_err(|_| TransportError::AuthorizationMismatch)?;
                            if registry.generation(ticket.server())? != Some(connection_generation)
                            {
                                return Ok(false);
                            }
                            catalog
                                .write()
                                .map_err(|_| TransportError::AuthorizationMismatch)?
                                .refresh_kind_until(
                                    ticket.server(),
                                    ticket.kind(),
                                    &features,
                                    || *shutdown.borrow(),
                                )
                                .map_err(TransportError::from)
                        })();
                        match published {
                            Ok(published) => published,
                            Err(error) => {
                                return fail_refresh_closed(
                                    connection,
                                    catalog,
                                    registry,
                                    lifecycle,
                                    connection_generation,
                                    error,
                                );
                            }
                        }
                    }
                    Err(error) if error.transient_refresh() => false,
                    Err(error) => {
                        return fail_refresh_closed(
                            connection,
                            catalog,
                            registry,
                            lifecycle,
                            connection_generation,
                            error,
                        );
                    }
                };
                if *shutdown.borrow() {
                    self.coalescer.complete(&ticket, false, self.now_millis());
                    return Ok(());
                }
                self.coalescer
                    .complete(&ticket, succeeded, self.now_millis());
                if !succeeded && self.coalescer.failures(&ticket) >= MAX_REFRESH_FAILURES {
                    return fail_refresh_closed(
                        connection,
                        catalog,
                        registry,
                        lifecycle,
                        connection_generation,
                        TransportError::RefreshRetriesExhausted,
                    );
                }
            }

            if *shutdown.borrow() {
                return Ok(());
            }
            if let Some(due) = self.coalescer.next_due_millis() {
                let delay = Duration::from_millis(due.saturating_sub(self.now_millis()));
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return Ok(());
                        }
                    }
                    event = self.events.recv() => {
                        if !self.handle_event(event) {
                            return fail_refresh_closed(
                                connection, catalog, registry, lifecycle,
                                connection_generation, TransportError::RefreshClosed,
                            );
                        }
                    }
                    () = tokio::time::sleep(delay) => {}
                }
            } else {
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() {
                            return Ok(());
                        }
                    }
                    event = self.events.recv() => {
                        if !self.handle_event(event) {
                            return fail_refresh_closed(
                                connection, catalog, registry, lifecycle,
                                connection_generation, TransportError::RefreshClosed,
                            );
                        }
                    }
                }
            }
        }
    }

    fn handle_event(
        &mut self,
        event: Result<McpServerEvent, tokio::sync::broadcast::error::RecvError>,
    ) -> bool {
        let now = self.now_millis();
        let negotiated = self
            .negotiated
            .read()
            .expect("MCP negotiated capabilities lock poisoned")
            .clone();
        match event {
            Ok(McpServerEvent::ToolListChanged) => {
                self.coalescer.notify(
                    self.server.clone(),
                    FeatureListKind::Tools,
                    &negotiated,
                    now,
                );
            }
            Ok(McpServerEvent::ResourceListChanged) => {
                self.coalescer.notify(
                    self.server.clone(),
                    FeatureListKind::Resources,
                    &negotiated,
                    now,
                );
            }
            Ok(McpServerEvent::PromptListChanged) => {
                self.coalescer.notify(
                    self.server.clone(),
                    FeatureListKind::Prompts,
                    &negotiated,
                    now,
                );
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                self.coalescer
                    .mark_lagged(self.server.clone(), &negotiated, now)
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return false,
        }
        true
    }

    fn now_millis(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

fn fail_refresh_closed(
    connection: &ReadyConnection,
    catalog: &Arc<RwLock<McpCatalog>>,
    registry: &ReadyConnectionRegistry,
    lifecycle: &Arc<RwLock<()>>,
    connection_generation: u64,
    error: TransportError,
) -> Result<(), TransportError> {
    let _lifecycle = lifecycle
        .write()
        .map_err(|_| TransportError::AuthorizationMismatch)?;
    if registry.generation(&connection.configured_server)? == Some(connection_generation) {
        connection.retire();
        catalog
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .mark_unavailable(&connection.configured_server)?;
    }
    Err(error)
}

fn feature_kinds(capabilities: agentkit_mcp::McpServerCapabilities) -> NegotiatedFeatureKinds {
    let available = [
        capabilities
            .tools
            .is_some()
            .then_some(FeatureListKind::Tools),
        capabilities
            .resources
            .is_some()
            .then_some(FeatureListKind::Resources),
        capabilities
            .prompts
            .is_some()
            .then_some(FeatureListKind::Prompts),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let list_changed = [
        capabilities.tools.as_ref().and_then(|capability| {
            capability
                .list_changed
                .map(|value| (FeatureListKind::Tools, value))
        }),
        capabilities.resources.as_ref().and_then(|capability| {
            capability
                .list_changed
                .map(|value| (FeatureListKind::Resources, value))
        }),
        capabilities.prompts.as_ref().and_then(|capability| {
            capability
                .list_changed
                .map(|value| (FeatureListKind::Prompts, value))
        }),
    ]
    .into_iter()
    .flatten();
    NegotiatedFeatureKinds::with_list_changed_values(available, list_changed)
}

pub(crate) fn authorize_ready_operation(
    request: &BrokerInvocation<'_>,
    operation: &transport_auth::TransportOperation,
    binding: &transport_auth::TransportBinding,
    arguments: &[u8],
    store: &mut SqliteStore,
) -> Result<(transport_auth::TransportAuthorization, bool), TransportError> {
    let replay = match transport_auth::state(request, binding, store) {
        Ok(TransportAuthState::Absent) | Err(BrokerError::AuthNotRequired) => false,
        Ok(TransportAuthState::Pending(challenge)) => {
            return Err(TransportError::AuthRequired(Box::new(challenge)));
        }
        Ok(TransportAuthState::Granted(challenge)) => {
            if challenge.operation != *operation {
                return Err(BrokerError::ReplayNotAuthorized.into());
            }
            true
        }
        Ok(TransportAuthState::Denied) => return Err(BrokerError::AuthDenied.into()),
        Ok(TransportAuthState::Replayed) => {
            return Err(BrokerError::ReplayPermitConsumed.into());
        }
        Err(error) => return Err(error.into()),
    };
    let authorization = if replay {
        transport_auth::authorize_operation_replay(request, operation, binding, arguments, store)?
    } else {
        transport_auth::authorize_operation(request, operation, binding, arguments, store)?
    };
    Ok((authorization, replay))
}

fn checked_page(pages: &mut usize) -> Result<(), TransportError> {
    *pages = pages.checked_add(1).ok_or(DiscoveryError::PageLimit)?;
    if *pages > crate::protocols::mcp::features::discovery::MAX_DISCOVERY_PAGES {
        return Err(DiscoveryError::PageLimit.into());
    }
    Ok(())
}

fn checked_discovery_page<T>(
    page: &FeaturePage<T>,
    entries: &mut usize,
    payload_bytes: &mut usize,
) -> Result<(), TransportError> {
    *entries = entries
        .checked_add(page.items().len())
        .ok_or(DiscoveryError::EntryLimit)?;
    if *entries > crate::capabilities::catalog::MAX_CATALOG_ENTRIES {
        return Err(DiscoveryError::EntryLimit.into());
    }
    *payload_bytes = payload_bytes
        .checked_add(
            page.payload()
                .accounted_bytes()
                .ok_or(DiscoveryError::PayloadLimit)?,
        )
        .ok_or(DiscoveryError::PayloadLimit)?;
    if *payload_bytes > crate::capabilities::catalog::MAX_CATALOG_PAYLOAD_BYTES {
        return Err(DiscoveryError::PayloadLimit.into());
    }
    Ok(())
}

struct OperationQueue {
    active: Arc<Semaphore>,
    waiting: Arc<Semaphore>,
}

impl OperationQueue {
    fn new() -> Self {
        Self {
            active: Arc::new(Semaphore::new(1)),
            waiting: Arc::new(Semaphore::new(1)),
        }
    }

    async fn acquire(
        &self,
        request: &BrokerInvocation<'_>,
        timeout: Duration,
    ) -> Result<OwnedSemaphorePermit, TransportError> {
        if let Ok(permit) = Arc::clone(&self.active).try_acquire_owned() {
            return Ok(permit);
        }
        let _waiting = Arc::clone(&self.waiting)
            .try_acquire_owned()
            .map_err(|_| TransportError::OperationQueueFull)?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            tokio::select! {
                permit = Arc::clone(&self.active).acquire_owned() => {
                    return permit.map_err(|_| TransportError::ConnectionRetired);
                }
                () = tokio::time::sleep_until(deadline) => {
                    return Err(TransportError::Timeout("operation queue"));
                }
                () = tokio::time::sleep(Duration::from_millis(1)) => {
                    if request.cancelled() && !request.lifecycle_shutdown() {
                        return Err(TransportError::Cancelled);
                    }
                }
            }
        }
    }
}

struct OperationGate {
    ready: AtomicBool,
    initialized_followup: AtomicBool,
    message_sent: AtomicBool,
    next: Mutex<Option<Arc<transport_auth::TransportAuthorization>>>,
    failure: Mutex<Option<TransportFailure>>,
    response: Mutex<OperationResponse>,
    callback_scanners: Mutex<BTreeMap<String, Arc<McpResponseScanner>>>,
    notification_support: RwLock<Option<NotificationSupport>>,
    responder_scanner: Mutex<Option<Arc<crate::protocols::mcp::responders::CallbackSecretScanner>>>,
    binding: Mutex<Option<transport_auth::TransportBinding>>,
    connection: Mutex<Option<Arc<transport_auth::TransportAuthorization>>>,
}

#[derive(Clone, Copy, Default)]
struct NotificationSupport {
    tools_changed: bool,
    resources_changed: bool,
    prompts_changed: bool,
}

impl NotificationSupport {
    fn from_capabilities(capabilities: &rmcp::model::ServerCapabilities) -> Self {
        Self {
            tools_changed: capabilities
                .tools
                .as_ref()
                .is_some_and(|capability| capability.list_changed == Some(true)),
            resources_changed: capabilities
                .resources
                .as_ref()
                .is_some_and(|capability| capability.list_changed == Some(true)),
            prompts_changed: capabilities
                .prompts
                .as_ref()
                .is_some_and(|capability| capability.list_changed == Some(true)),
        }
    }

    fn from_negotiated(negotiated: &NegotiatedFeatureKinds) -> Self {
        Self {
            tools_changed: negotiated.supports_list_changed(FeatureListKind::Tools),
            resources_changed: negotiated.supports_list_changed(FeatureListKind::Resources),
            prompts_changed: negotiated.supports_list_changed(FeatureListKind::Prompts),
        }
    }
}

#[derive(Default)]
struct OperationResponse {
    generation: u64,
    active: Option<u64>,
    payload: Option<RawPayload>,
    response_id: Option<Value>,
}

enum TransportFailure {
    InvalidHeader,
    ResponseTooLarge,
    SseEventTooLarge,
    Credential(HttpCredentialError),
    Egress(crate::protocols::mcp::egress::McpEgressError),
    HttpTimeout,
    MissingProtocolVersion(agentkit_mcp::McpServerId),
    StdioParse(String),
    StdioTimeout,
    Payload(PayloadError),
    SensitivePayload,
    SessionExpired,
}

struct BoundOperationError {
    error: TransportError,
    dispatched: bool,
}

impl BoundOperationError {
    fn before_dispatch(error: TransportError) -> Self {
        Self {
            error,
            dispatched: false,
        }
    }

    fn after_dispatch(error: TransportError) -> Self {
        Self {
            error,
            dispatched: true,
        }
    }
}

impl OperationGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ready: AtomicBool::new(false),
            initialized_followup: AtomicBool::new(false),
            message_sent: AtomicBool::new(false),
            next: Mutex::new(None),
            failure: Mutex::new(None),
            response: Mutex::new(OperationResponse::default()),
            callback_scanners: Mutex::new(BTreeMap::new()),
            notification_support: RwLock::new(None),
            responder_scanner: Mutex::new(None),
            binding: Mutex::new(None),
            connection: Mutex::new(None),
        })
    }

    fn install(
        &self,
        authorization: transport_auth::TransportAuthorization,
    ) -> Result<u64, TransportError> {
        let mut response = self
            .response
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        let mut next = self
            .next
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        let mut binding = self
            .binding
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        if next.is_some() || response.active.is_some() {
            return Err(TransportError::AuthorizationMismatch);
        }
        if let Some(connection) = self
            .connection
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .as_ref()
            && !authorization.same_connection(connection)
        {
            return Err(TransportError::AuthorizationMismatch);
        }
        if binding
            .as_ref()
            .is_none_or(|current| !current.same_connection(authorization.binding()))
        {
            return Err(TransportError::AuthorizationMismatch);
        }
        *binding = Some(authorization.binding().clone());
        self.message_sent.store(false, Ordering::Release);
        response.generation = response
            .generation
            .checked_add(1)
            .ok_or(TransportError::AuthorizationMismatch)?;
        let generation = response.generation;
        response.active = Some(generation);
        response.payload = None;
        response.response_id = None;
        *next = Some(Arc::new(authorization));
        Ok(generation)
    }

    fn authorize_message<T: serde::Serialize>(
        &self,
        message: &T,
    ) -> Result<Arc<transport_auth::TransportAuthorization>, TransportError> {
        let message =
            serde_json::to_value(message).map_err(|_| TransportError::AuthorizationMismatch)?;
        let method = message.get("method").and_then(Value::as_str);
        let Some(method) = method else {
            if agentkit_mcp::has_responder_delivery_permit(&message) {
                return self
                    .connection
                    .lock()
                    .map_err(|_| TransportError::AuthorizationMismatch)?
                    .clone()
                    .ok_or(TransportError::AuthorizationMismatch);
            }
            return Err(TransportError::AuthorizationMismatch);
        };
        if method == "notifications/initialized"
            && self.initialized_followup.swap(false, Ordering::AcqRel)
        {
            if message.get("params").is_some() {
                return Err(TransportError::AuthorizationMismatch);
            }
            return self.current_authorization();
        }
        if !self.ready.load(Ordering::Acquire) && method != "initialize" {
            return Err(TransportError::AuthorizationMismatch);
        }
        let authorization = self
            .next
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .clone()
            .ok_or(TransportError::AuthorizationMismatch)?;
        if self.message_sent.swap(true, Ordering::AcqRel) {
            return Err(TransportError::AuthorizationMismatch);
        }
        let arguments = serde_json::from_slice::<Value>(authorization.arguments())
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        let mut params = message
            .get("params")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(object) = params.as_object_mut()
            && let Some(metadata) = object.remove("_meta")
            && metadata.as_object().is_none_or(|metadata| {
                metadata.len() != 1
                    || metadata
                        .get("progressToken")
                        .is_none_or(|token| !(token.is_string() || token.is_number()))
            })
        {
            return Err(TransportError::AuthorizationMismatch);
        }
        if authorization.operation().as_str() != method || arguments != params {
            return Err(TransportError::AuthorizationMismatch);
        }
        let response_id = message
            .get("id")
            .filter(|id| id.is_string() || id.is_number())
            .cloned()
            .ok_or(TransportError::AuthorizationMismatch)?;
        let mut response = self
            .response
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        if response.active.is_none() {
            return Err(TransportError::AuthorizationMismatch);
        }
        response.response_id = Some(response_id);
        if method == "initialize" {
            self.initialized_followup.store(true, Ordering::Release);
        }
        Ok(authorization)
    }

    fn current_authorization(
        &self,
    ) -> Result<Arc<transport_auth::TransportAuthorization>, TransportError> {
        self.next
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .clone()
            .ok_or(TransportError::AuthorizationMismatch)
    }

    fn exact_terminal_url_elicitation_digest(
        &self,
        generation: u64,
        message: &str,
        raw_data: Option<&Value>,
    ) -> Result<Digest, TransportError> {
        let response = self
            .response
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        let payload = response
            .payload
            .as_ref()
            .filter(|_| response.active == Some(generation))
            .ok_or(TransportError::AuthorizationMismatch)?;
        let value = payload.value();
        let object = value
            .as_object()
            .filter(|object| {
                object.len() == 3
                    && object.contains_key("jsonrpc")
                    && object.contains_key("id")
                    && object.contains_key("error")
            })
            .ok_or(TransportError::AuthorizationMismatch)?;
        let error = object
            .get("error")
            .and_then(Value::as_object)
            .filter(|error| {
                error.len() == 3
                    && error.get("code").and_then(Value::as_i64) == Some(-32042)
                    && error.get("message").and_then(Value::as_str) == Some(message)
                    && error.get("data") == raw_data
            })
            .ok_or(TransportError::AuthorizationMismatch)?;
        let _ = error;
        Ok(Digest::of(
            DigestAlgorithm::Sha256,
            payload.canonical_bytes(),
        ))
    }

    fn bind_connection(
        &self,
        authorization: Arc<transport_auth::TransportAuthorization>,
    ) -> Result<(), TransportError> {
        *self
            .connection
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)? = Some(authorization);
        Ok(())
    }

    fn set_binding(&self, binding: transport_auth::TransportBinding) -> Result<(), TransportError> {
        *self
            .binding
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)? = Some(binding);
        Ok(())
    }

    fn binding(&self) -> Result<transport_auth::TransportBinding, TransportError> {
        self.binding
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .clone()
            .ok_or(TransportError::AuthorizationMismatch)
    }

    fn clear_generation(&self, generation: u64) -> Result<Option<RawPayload>, TransportError> {
        let mut response = self
            .response
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        if response.active != Some(generation) {
            return Err(TransportError::AuthorizationMismatch);
        }
        *self
            .next
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)? = None;
        self.initialized_followup.store(false, Ordering::Release);
        self.message_sent.store(false, Ordering::Release);
        response.response_id = None;
        response.active = None;
        Ok(response.payload.take())
    }

    #[cfg(test)]
    fn clear(&self) {
        let generation = self.response.lock().ok().and_then(|state| state.active);
        if let Some(generation) = generation {
            let _ = self.clear_generation(generation);
        }
    }

    fn fail(&self, failure: TransportFailure) {
        if let Ok(mut slot) = self.failure.lock()
            && slot.is_none()
        {
            *slot = Some(failure);
        }
    }

    fn set_responder_scanner(
        &self,
        scanner: Arc<crate::protocols::mcp::responders::CallbackSecretScanner>,
    ) {
        *self
            .responder_scanner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(scanner);
    }

    fn set_notification_support(
        &self,
        negotiated: &NegotiatedFeatureKinds,
    ) -> Result<(), TransportError> {
        *self
            .notification_support
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)? =
            Some(NotificationSupport::from_negotiated(negotiated));
        Ok(())
    }

    fn bind_callback_scanner(
        &self,
        payload: &RawPayload,
        scanner: Arc<McpResponseScanner>,
    ) -> Result<(), TransportError> {
        let value = payload.value();
        if value.get("method").and_then(Value::as_str).is_none() {
            self.bind_initial_notification_support(value)?;
            return Ok(());
        }
        let Some(id) = value
            .get("id")
            .filter(|id| id.is_string() || id.is_number())
        else {
            return self.validate_notification(value);
        };
        let key = serde_json::to_string(id).map_err(|_| TransportError::AuthorizationMismatch)?;
        let mut scanners = self
            .callback_scanners
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        if scanners.contains_key(&key) {
            return Err(TransportError::AuthorizationMismatch);
        }
        scanners.insert(key, scanner);
        Ok(())
    }

    fn bind_initial_notification_support(&self, value: &Value) -> Result<(), TransportError> {
        if self.ready.load(Ordering::Acquire)
            || self
                .next
                .lock()
                .map_err(|_| TransportError::AuthorizationMismatch)?
                .as_ref()
                .is_none_or(|authorization| authorization.operation().as_str() != "initialize")
        {
            return Ok(());
        }
        let response = self
            .response
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        if value.get("id") != response.response_id.as_ref() {
            return Ok(());
        }
        let Some(result) = value.get("result").cloned() else {
            return Ok(());
        };
        let Ok(result) = serde_json::from_value::<rmcp::model::InitializeResult>(result) else {
            return Ok(());
        };
        if result.protocol_version != PROTOCOL_REVISION {
            return Ok(());
        }
        *self
            .notification_support
            .write()
            .map_err(|_| TransportError::AuthorizationMismatch)? =
            Some(NotificationSupport::from_capabilities(&result.capabilities));
        Ok(())
    }

    fn validate_notification(&self, value: &Value) -> Result<(), TransportError> {
        let notification =
            match serde_json::from_value::<rmcp::model::ServerJsonRpcMessage>(value.clone())
                .map_err(|_| TransportError::AuthorizationMismatch)?
            {
                rmcp::model::ServerJsonRpcMessage::Notification(notification) => {
                    notification.notification
                }
                _ => return Err(TransportError::AuthorizationMismatch),
            };
        let support = self
            .notification_support
            .read()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .ok_or(TransportError::AuthorizationMismatch)?;
        let accepted = match notification {
            rmcp::model::ServerNotification::ToolListChangedNotification(_) => {
                support.tools_changed && value.get("params").is_none()
            }
            rmcp::model::ServerNotification::ResourceListChangedNotification(_) => {
                support.resources_changed && value.get("params").is_none()
            }
            rmcp::model::ServerNotification::PromptListChangedNotification(_) => {
                support.prompts_changed && value.get("params").is_none()
            }
            rmcp::model::ServerNotification::ProgressNotification(_)
            | rmcp::model::ServerNotification::CancelledNotification(_) => true,
            _ => false,
        };
        accepted
            .then_some(())
            .ok_or(TransportError::AuthorizationMismatch)
    }

    fn scan_callback_response(&self, value: &Value, bytes: &[u8]) -> Result<(), TransportError> {
        let id = value
            .get("id")
            .filter(|id| id.is_string() || id.is_number())
            .ok_or(TransportError::AuthorizationMismatch)?;
        let key = serde_json::to_string(id).map_err(|_| TransportError::AuthorizationMismatch)?;
        let scanner = self
            .callback_scanners
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .remove(&key)
            .ok_or(TransportError::AuthorizationMismatch)?;
        let response_public = self
            .responder_scanner
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?
            .as_ref()
            .is_some_and(|scanner| {
                crate::protocols::mcp::responders::callback_value_public_to(scanner, value)
            });
        if scanner
            .scan_callback(bytes)
            .map_err(|_| TransportError::AuthorizationMismatch)?
            || !response_public
        {
            self.fail(TransportFailure::SensitivePayload);
            Err(TransportError::SensitivePayload)
        } else {
            Ok(())
        }
    }

    fn clear_response_scanners(&self) {
        self.callback_scanners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        *self
            .responder_scanner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    fn capture_payload(&self, payload: RawPayload) -> Result<bool, TransportError> {
        let mut response = self
            .response
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        if response.active.is_none() || payload.value().get("id") != response.response_id.as_ref() {
            return Ok(false);
        }
        if response.payload.is_some() {
            return Err(TransportError::AuthorizationMismatch);
        }
        response.payload = Some(payload);
        Ok(true)
    }

    fn take_failure(&self) -> Option<TransportError> {
        self.failure
            .lock()
            .ok()?
            .take()
            .map(|failure| match failure {
                TransportFailure::InvalidHeader => TransportError::InvalidHeader,
                TransportFailure::ResponseTooLarge => TransportError::ResponseTooLarge,
                TransportFailure::SseEventTooLarge => TransportError::SseEventTooLarge,
                TransportFailure::Credential(error) => TransportError::Credential(error),
                TransportFailure::Egress(error) => TransportError::Egress(error),
                TransportFailure::HttpTimeout => TransportError::Timeout("HTTP request"),
                TransportFailure::MissingProtocolVersion(server) => {
                    TransportError::Agentkit(Box::new(McpError::UnsupportedProtocolVersion {
                        server,
                        expected: PINNED_PROTOCOL_VERSION,
                        negotiated: None,
                    }))
                }
                TransportFailure::StdioParse(error) => TransportError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid MCP stdio response: {error}"),
                )),
                TransportFailure::StdioTimeout => TransportError::Timeout("stdio response"),
                TransportFailure::Payload(error) => TransportError::Payload(error),
                TransportFailure::SensitivePayload => TransportError::SensitivePayload,
                TransportFailure::SessionExpired => TransportError::SessionExpired,
            })
    }
}

fn is_terminal_url_elicitation(payload: &RawPayload) -> bool {
    payload.value()["error"]["code"].as_i64() == Some(-32042)
}

#[derive(Debug)]
pub enum TransportError {
    InvalidLimits,
    InvalidEndpoint,
    AuthorizationMismatch,
    ConnectionRetired,
    BindingExpired,
    OperationQueueFull,
    PolicyAuthorizationMismatch,
    InvalidHeader,
    ResponseTooLarge,
    SseEventTooLarge,
    ProtocolVersionRefused,
    MissingPayload,
    OwnedProcessUnavailable,
    Cancelled,
    RefreshClosed,
    RefreshRetriesExhausted,
    Timeout(&'static str),
    Broker(BrokerError),
    Agentkit(Box<McpError>),
    Io(io::Error),
    Cleanup {
        primary: Box<TransportError>,
        cleanup: io::Error,
    },
    Credential(HttpCredentialError),
    Egress(crate::protocols::mcp::egress::McpEgressError),
    Payload(PayloadError),
    Feature(FeatureError),
    Discovery(DiscoveryError),
    Result(McpResultError),
    SensitivePayload,
    SessionExpired,
    UrlElicitation {
        message: String,
        url: String,
        elicitation_id: String,
        raw_data: Option<Value>,
    },
    UrlElicitationUnavailable,
    UrlElicitationDeclined,
    AuthRequired(Box<transport_auth::TransportAuthChallenge>),
}

impl fmt::Display for TransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("invalid MCP transport limits"),
            Self::InvalidEndpoint => formatter.write_str("invalid MCP HTTP endpoint"),
            Self::AuthorizationMismatch => {
                formatter.write_str("MCP transport authorization does not match the operation")
            }
            Self::ConnectionRetired => {
                formatter.write_str("MCP connection was replaced or removed")
            }
            Self::BindingExpired => formatter.write_str("MCP capability binding expired"),
            Self::OperationQueueFull => formatter.write_str("MCP operation queue is full"),
            Self::PolicyAuthorizationMismatch => {
                formatter.write_str("MCP HTTP endpoint does not match policy authorization")
            }
            Self::InvalidHeader => formatter.write_str("invalid or oversized MCP HTTP header"),
            Self::ResponseTooLarge => formatter.write_str("MCP HTTP response exceeds its bound"),
            Self::SseEventTooLarge => formatter.write_str("MCP SSE event exceeds its bound"),
            Self::ProtocolVersionRefused => {
                formatter.write_str("MCP server did not negotiate protocol revision 2025-11-25")
            }
            Self::MissingPayload => formatter.write_str("MCP feature response payload is missing"),
            Self::OwnedProcessUnavailable => {
                formatter.write_str("durable MCP owned-process service is unavailable")
            }
            Self::Cancelled => formatter.write_str("MCP refresh was cancelled"),
            Self::RefreshClosed => formatter.write_str("MCP refresh event stream closed"),
            Self::RefreshRetriesExhausted => {
                formatter.write_str("MCP refresh retry limit exhausted")
            }
            Self::Timeout(operation) => write!(formatter, "MCP {operation} timed out"),
            Self::Broker(error) => error.fmt(formatter),
            Self::Agentkit(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
            Self::Cleanup { primary, cleanup } => {
                write!(formatter, "{primary}; cleanup: {cleanup}")
            }
            Self::Credential(error) => error.fmt(formatter),
            Self::Egress(error) => error.fmt(formatter),
            Self::Payload(error) => error.fmt(formatter),
            Self::Feature(error) => error.fmt(formatter),
            Self::Discovery(error) => error.fmt(formatter),
            Self::Result(error) => error.fmt(formatter),
            Self::SensitivePayload => {
                formatter.write_str("MCP response contains protected credential material")
            }
            Self::SessionExpired => formatter.write_str("MCP session expired"),
            Self::UrlElicitation { .. } => {
                formatter.write_str("MCP URL elicitation requires authenticated resolution")
            }
            Self::UrlElicitationUnavailable => {
                formatter.write_str("MCP URL elicitation outcome is unknown")
            }
            Self::UrlElicitationDeclined => formatter.write_str("MCP URL elicitation declined"),
            Self::AuthRequired(_) => formatter.write_str("MCP operation requires authorization"),
        }
    }
}

impl std::error::Error for TransportError {}

impl TransportError {
    fn transient_refresh(&self) -> bool {
        matches!(
            self,
            Self::Timeout(_)
                | Self::Io(_)
                | Self::OperationQueueFull
                | Self::Credential(HttpCredentialError::Unavailable)
                | Self::Cleanup { .. }
        )
    }

    fn completion_code(&self) -> &'static str {
        match self {
            Self::Timeout(_) => "mcp.transport_timeout",
            Self::AuthRequired(_) => "mcp.transport_auth_interrupted",
            Self::SensitivePayload => "mcp.sensitive_payload",
            Self::SessionExpired => "mcp.session_expired",
            Self::UrlElicitation { .. } => "mcp.url_elicitation_required",
            Self::UrlElicitationUnavailable => "mcp.url_elicitation_outcome_unknown",
            Self::UrlElicitationDeclined => "mcp.url_elicitation_declined",
            Self::Credential(_) => "mcp.credential_failed",
            Self::Egress(_) => "mcp.egress_denied",
            Self::Payload(_) | Self::Result(_) => "mcp.invalid_response",
            Self::Cleanup { primary, .. } => primary.completion_code(),
            Self::Broker(_)
            | Self::AuthorizationMismatch
            | Self::ConnectionRetired
            | Self::BindingExpired
            | Self::PolicyAuthorizationMismatch => "mcp.authorization_failed",
            Self::OperationQueueFull => "mcp.operation_queue_full",
            Self::Cancelled => "mcp.cancelled",
            Self::RefreshClosed => "mcp.refresh_closed",
            Self::RefreshRetriesExhausted => "mcp.refresh_retries_exhausted",
            Self::InvalidLimits
            | Self::InvalidEndpoint
            | Self::InvalidHeader
            | Self::ResponseTooLarge
            | Self::SseEventTooLarge
            | Self::ProtocolVersionRefused
            | Self::MissingPayload
            | Self::OwnedProcessUnavailable
            | Self::Agentkit(_)
            | Self::Io(_)
            | Self::Feature(_)
            | Self::Discovery(_) => "mcp.transport_failed",
        }
    }
}

impl From<BrokerError> for TransportError {
    fn from(value: BrokerError) -> Self {
        Self::Broker(value)
    }
}

impl From<io::Error> for TransportError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<McpError> for TransportError {
    fn from(value: McpError) -> Self {
        Self::Agentkit(Box::new(value))
    }
}

impl From<FeatureError> for TransportError {
    fn from(value: FeatureError) -> Self {
        Self::Feature(value)
    }
}

impl From<DiscoveryError> for TransportError {
    fn from(value: DiscoveryError) -> Self {
        Self::Discovery(value)
    }
}

impl From<McpResultError> for TransportError {
    fn from(value: McpResultError) -> Self {
        Self::Result(value)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        future::Future,
        sync::atomic::{AtomicU64, AtomicUsize},
    };

    use agentkit_mcp::{McpHandlerConfig, McpServerId, kit_authorized_initialize_arguments};
    use rmcp::{
        RoleClient,
        service::{RxJsonRpcMessage, TxJsonRpcMessage},
        transport::Transport,
    };
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        api::{
            auth::{
                contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot},
                local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
            },
            service::AttemptDriverClaim,
        },
        capabilities::{
            catalog::{Availability, CatalogSnapshot, CatalogSource, SourceKind, TrustDomain},
            discovery::DiscoverySession,
            kernel::{
                grant::{
                    ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, EffectClass,
                },
                grant_ext::RequestExtension,
                identity::{
                    CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
                    CapabilityVersion, Digest, DigestAlgorithm,
                },
                invoke::{ApprovalState, RetrySafety},
            },
            registration::{
                BindingRegistry, PortableInvokeCall, ProviderCapabilityContract, RegistrationCall,
                ValidatedProjectionSupport,
            },
            schema::{JSON_SCHEMA_2020_12, NormalizedSchema, ProjectionProfile, ProjectionTarget},
        },
        domain::{
            config::{
                BudgetLayer, CONFIG_SCHEMA_VERSION, ConcurrencyLayer, ConfigLayer, Executor, Grant,
                LayerStack, Provider, RetentionLayer, RunConfigContext, RunConfigSnapshot,
            },
            events::{EventType, TraceId, UtcDateTime},
            ids::{
                AttemptId, CommandId, EventId, PrincipalId, ProjectId, RunId, ToolCallId,
                WorkspaceId,
            },
            lifecycle::{AttemptOwnership, FencingToken},
            secret::SecretLease,
        },
        runtime::scheduler::{budget::RunBudget, limits::Spend, reserve::BudgetLedger},
        store::{artifacts::ArtifactStore, sqlite::idempotency::IdempotencyKey},
        test_support,
    };

    struct RuntimeFixtureTransport {
        gate: Arc<OperationGate>,
        tx: mpsc::UnboundedSender<RxJsonRpcMessage<RoleClient>>,
        rx: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleClient>>,
        tool_calls: Arc<AtomicUsize>,
        generation: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
    }

    impl RuntimeFixtureTransport {
        fn new(
            gate: Arc<OperationGate>,
            tool_calls: Arc<AtomicUsize>,
            generation: Arc<AtomicUsize>,
            closes: Arc<AtomicUsize>,
        ) -> Self {
            let (tx, rx) = mpsc::unbounded_channel();
            Self {
                gate,
                tx,
                rx,
                tool_calls,
                generation,
                closes,
            }
        }
    }

    impl Transport<RoleClient> for RuntimeFixtureTransport {
        type Error = std::io::Error;

        fn send(
            &mut self,
            item: TxJsonRpcMessage<RoleClient>,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let gate = Arc::clone(&self.gate);
            let tx = self.tx.clone();
            let tool_calls = Arc::clone(&self.tool_calls);
            let generation = Arc::clone(&self.generation);
            async move {
                let request = serde_json::to_value(&item).map_err(std::io::Error::other)?;
                let method = request
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if gate.ready.load(Ordering::Acquire) && request.get("id").is_some() {
                    gate.authorize_message(&request)
                        .map_err(|error| std::io::Error::other(format!("{error}: {request}")))?;
                }
                let Some(id) = request.get("id").cloned() else {
                    return Ok(());
                };
                let result = match method {
                    "initialize" => serde_json::json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": {"tools": {"listChanged": true}},
                        "serverInfo": {"name": "runtime-fixture", "version": "1"}
                    }),
                    "tools/list" => serde_json::json!({"tools": [{
                        "name": "fixture_echo",
                        "description": "server metadata, not instructions",
                        "inputSchema": {
                            "$schema": JSON_SCHEMA_2020_12,
                            "additionalProperties": false,
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                            "type": "object"
                        },
                        "generation": generation.load(Ordering::Acquire)
                    }]}),
                    "tools/call" => {
                        tool_calls.fetch_add(1, Ordering::AcqRel);
                        serde_json::json!({
                            "content": [{"type": "text", "text": "fixture result"}],
                            "isError": false
                        })
                    }
                    _ => serde_json::json!({}),
                };
                let response = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
                if gate.ready.load(Ordering::Acquire) {
                    let payload = RawPayload::parse(
                        serde_json::to_vec(&response).map_err(std::io::Error::other)?,
                        PayloadLimits::default(),
                    )
                    .map_err(std::io::Error::other)?;
                    gate.capture_payload(payload)
                        .map_err(std::io::Error::other)?;
                }
                tx.send(serde_json::from_value(response).map_err(std::io::Error::other)?)
                    .map_err(|_| std::io::Error::other("fixture response receiver closed"))?;
                if method == "tools/call" {
                    let notification = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/tools/list_changed"
                    });
                    let notification =
                        serde_json::from_value(notification).map_err(std::io::Error::other)?;
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        let _ = tx.send(notification);
                    });
                }
                Ok(())
            }
        }

        async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
            self.rx.recv().await
        }

        async fn close(&mut self) -> Result<(), Self::Error> {
            self.closes.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    struct UrlAcceptanceResolver {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl crate::domain::egress::EgressResolver for UrlAcceptanceResolver {
        async fn resolve(
            &self,
            host: &str,
            _port: u16,
        ) -> Result<Vec<std::net::IpAddr>, crate::domain::egress::Denial> {
            self.calls.lock().unwrap().push(host.to_owned());
            match host {
                "mcp-a.example" => Ok(vec!["8.8.8.8".parse().unwrap()]),
                "mcp-b.example" => Ok(vec!["1.1.1.1".parse().unwrap()]),
                _ => Err(crate::domain::egress::Denial::ResolverUnavailable),
            }
        }
    }

    struct UrlAcceptanceCredentials;

    #[async_trait::async_trait]
    impl crate::protocols::mcp::egress::HttpCredentialBroker for UrlAcceptanceCredentials {
        async fn authorize_and_resolve(
            &self,
            _handle: &crate::domain::secret::SecretHandle,
            _context: &crate::protocols::mcp::egress::HttpSecretContext<'_>,
        ) -> Result<SecretLease, crate::protocols::mcp::egress::HttpCredentialError> {
            Ok(SecretLease::new(b"credential-canary".to_vec()))
        }
    }

    #[derive(Default)]
    struct UrlAcceptanceDialer {
        initialize_redirected: AtomicBool,
        tool_calls: Mutex<BTreeMap<String, usize>>,
        observations: Mutex<Vec<(String, String, Vec<std::net::IpAddr>)>>,
    }

    #[async_trait::async_trait]
    impl crate::protocols::mcp::egress::EgressDialer for UrlAcceptanceDialer {
        async fn send(
            &self,
            request: reqwest::Request,
            authorization: &crate::domain::egress::Authorization,
            _limits: crate::protocols::mcp::egress::McpEgressLimits,
        ) -> Result<crate::protocols::mcp::egress::EgressDialResponse, std::io::Error> {
            let url = request.url().to_string();
            self.observations.lock().unwrap().push((
                url.clone(),
                authorization.destination().host(),
                authorization.resolved_addresses().collect(),
            ));
            let body = request
                .body()
                .and_then(reqwest::Body::as_bytes)
                .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok());
            let method = body
                .as_ref()
                .and_then(|body| body.get("method"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let mut response = ::http::Response::builder();
            let peer: std::net::IpAddr = if url.starts_with("https://mcp-b.example") {
                "1.1.1.1"
            } else {
                "8.8.8.8"
            }
            .parse()
            .unwrap();
            if method.as_deref() == Some("initialize")
                && !self.initialize_redirected.swap(true, Ordering::AcqRel)
            {
                response = response
                    .status(::http::StatusCode::TEMPORARY_REDIRECT)
                    .header(::http::header::LOCATION, "https://mcp-b.example/mcp");
                return Ok(crate::protocols::mcp::egress::EgressDialResponse {
                    response: response.body(bytes::Bytes::new()).unwrap().into(),
                    peer: Some(peer),
                });
            }
            let Some(body) = body else {
                return Ok(crate::protocols::mcp::egress::EgressDialResponse {
                    response: response
                        .status(::http::StatusCode::ACCEPTED)
                        .body(bytes::Bytes::new())
                        .unwrap()
                        .into(),
                    peer: Some(peer),
                });
            };
            let Some(id) = body.get("id").cloned() else {
                return Ok(crate::protocols::mcp::egress::EgressDialResponse {
                    response: response
                        .status(::http::StatusCode::ACCEPTED)
                        .body(bytes::Bytes::new())
                        .unwrap()
                        .into(),
                    peer: Some(peer),
                });
            };
            let payload = match method.as_deref() {
                Some("initialize") => serde_json::json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "result":{
                        "protocolVersion":"2025-11-25",
                        "capabilities":{"tools":{}},
                        "serverInfo":{"name":"url-acceptance","version":"1"}
                    }
                }),
                Some("tools/call") => {
                    let scenario = body["params"]["arguments"]["scenario"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let call = {
                        let mut calls = self.tool_calls.lock().unwrap();
                        let call = calls.entry(scenario.clone()).or_default();
                        *call += 1;
                        *call
                    };
                    if scenario == "accepted" && call == 2 {
                        serde_json::json!({
                            "jsonrpc":"2.0","id":id,
                            "result":{"content":[{"type":"text","text":"authorized"}],"isError":false}
                        })
                    } else {
                        let message = match scenario.as_str() {
                            "secret" => "credential-canary",
                            "encoded_secret" => "Y3JlZGVudGlhbC1jYW5hcnk=",
                            _ => "Authenticate with OAuth, then authorize access",
                        };
                        serde_json::json!({
                            "jsonrpc":"2.0","id":id,
                            "error":{
                                "code":-32042,
                                "message":message,
                                "data":{
                                    "url":"https://auth.example.com/authorize",
                                    "elicitationId":format!("{scenario}-{call}")
                                }
                            }
                        })
                    }
                }
                _ => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{}}),
            };
            let bytes = serde_json::to_vec(&payload).unwrap();
            Ok(crate::protocols::mcp::egress::EgressDialResponse {
                response: response
                    .status(::http::StatusCode::OK)
                    .header(::http::header::CONTENT_TYPE, "application/json")
                    .body(bytes::Bytes::from(bytes))
                    .unwrap()
                    .into(),
                peer: Some(peer),
            })
        }
    }

    struct RuntimeInputs {
        authenticated: AuthenticatedPrincipal,
        config: RunConfigSnapshot,
        constraints: ArgumentConstraints,
        workspace: WorkspaceId,
        project: ProjectId,
        attempt: AttemptOwnership,
        claim: AttemptDriverClaim,
        occurred_at: UtcDateTime,
        trace: TraceId,
        fence: Arc<AtomicU64>,
        cancellation: Arc<AtomicBool>,
    }

    impl RuntimeInputs {
        fn new() -> Self {
            Self::with_authority(BTreeSet::from([Grant::WorkspaceRead]))
        }

        fn with_authority(authority: BTreeSet<Grant>) -> Self {
            let principal = PrincipalId::generate().unwrap();
            let project = ProjectId::generate().unwrap();
            let workspace = WorkspaceId::generate().unwrap();
            let config = LayerStack {
                built_in: ConfigLayer {
                    schema_version: CONFIG_SCHEMA_VERSION,
                    budgets: BudgetLayer {
                        max_tokens: Some(100),
                        max_cost_microusd: Some(100),
                        max_turns: Some(100),
                    },
                    concurrency: ConcurrencyLayer {
                        max_runs: Some(2),
                        max_tools: Some(10),
                    },
                    retention: RetentionLayer {
                        event_days: Some(7),
                        artifact_days: Some(7),
                    },
                    provider: Some(Provider::Anthropic),
                    executor: Some(Executor::Local),
                    grammar_edit: Some(Default::default()),
                    grants: Some(authority.clone()),
                },
                user: None,
                project: None,
                run: None,
                experiment: None,
            }
            .materialize(
                RunConfigContext {
                    principal_id: principal,
                    project_id: project,
                    run_id: RunId::generate().unwrap(),
                },
                &authority,
            )
            .unwrap();
            let authenticated = LocalPeerAuthenticator::new(BTreeMap::from([(
                501,
                GrantSnapshot::new(principal, project, authority),
            )]))
            .authenticate(&LocalPeerObservation::from_transport(501, 1, 501))
            .unwrap();
            let attempt = AttemptOwnership::new(
                AttemptId::generate().unwrap(),
                principal,
                FencingToken::new(7),
            );
            let claim = AttemptDriverClaim {
                run_id: config.run_id(),
                attempt_id: attempt.attempt_id,
                principal_id: principal,
                fence: attempt.fencing_token,
                lease_version: 1,
                expires_at_unix_micros: 0,
            };
            Self {
                authenticated,
                config,
                constraints: ArgumentConstraints::default(),
                workspace,
                project,
                attempt,
                claim,
                occurred_at: UtcDateTime::parse("2026-08-03T12:00:00Z").unwrap(),
                trace: TraceId::parse("mcp-production-runtime").unwrap(),
                fence: Arc::new(AtomicU64::new(7)),
                cancellation: Arc::new(AtomicBool::new(false)),
            }
        }

        fn envelope<'a>(
            &'a self,
            grants: &'a CapabilityGrantSnapshot,
            capability: &'a CapabilityIdentity,
            schema: Digest,
            arguments: &'a [u8],
            invocation: ToolCallId,
            key: &'a IdempotencyKey,
        ) -> InvocationEnvelope<'a> {
            InvocationEnvelope {
                authenticated: &self.authenticated,
                config: &self.config,
                grants,
                delegation: None,
                extension: RequestExtension::default(),
                capability,
                discovered_schema_digest: schema,
                bound_schema_digest: schema,
                effect: EffectClass::WorkspaceRead,
                argument_constraints: &self.constraints,
                arguments,
                workspace_id: self.workspace,
                project_id: self.project,
                invocation_id: invocation,
                idempotency_key: key,
                reservation: Spend::new(0, 0, 0, 1, 0),
                retry_safety: RetrySafety::Idempotent,
                approval: ApprovalState::NotRequired,
                cancellation: &self.cancellation,
                attempt: self.attempt,
                driver_claim: Some(self.claim),
                current_fence: &self.fence,
                command_id: CommandId::generate().unwrap(),
                intent_event_id: EventId::generate().unwrap(),
                outcome_event_id: EventId::generate().unwrap(),
                occurred_at: &self.occurred_at,
                trace_id: &self.trace,
            }
        }
    }

    async fn runtime_fixture_connection(
        server: &ConfiguredServerIdentity,
        tool_calls: Arc<AtomicUsize>,
        generation: Arc<AtomicUsize>,
        closes: Arc<AtomicUsize>,
    ) -> Arc<ReadyConnection> {
        let gate = OperationGate::new();
        let connection = McpConnection::connect_kit_authorized_transport(
            McpServerId::new(server.as_str()),
            RuntimeFixtureTransport::new(Arc::clone(&gate), tool_calls, generation, closes),
            McpHandlerConfig::new(),
        )
        .await
        .unwrap();
        Arc::new(
            ReadyConnection::new(
                connection,
                server.clone(),
                TransportLimits::default(),
                gate,
                None,
                None,
                false,
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn production_runtime_load_registration_broker_refresh_replay_and_reconnect() {
        use crate::protocols::mcp::features::{
            McpCatalogConfig, McpCatalogPolicy, McpCatalogPolicyKey,
        };

        let root = std::env::temp_dir().join(format!(
            "kit-mcp-production-runtime-{}",
            EventId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("events.sqlite3");
        let artifacts = ArtifactStore::open(root.join("artifacts")).unwrap();
        let mut store = test_support::open_sqlite_store(&database).unwrap();
        let inputs = RuntimeInputs::new();
        store.install_driver_claim_for_test(inputs.claim).unwrap();

        let server = ConfiguredServerIdentity::new("production-runtime").unwrap();
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let fixture_generation = Arc::new(AtomicUsize::new(0));
        let closes = Arc::new(AtomicUsize::new(0));
        let mut connection = runtime_fixture_connection(
            &server,
            Arc::clone(&tool_calls),
            Arc::clone(&fixture_generation),
            Arc::clone(&closes),
        )
        .await;

        let discovery_schema = NormalizedSchema::ingest(
            br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"}"#,
            JSON_SCHEMA_2020_12,
            b"authorized MCP discovery",
            DigestAlgorithm::Sha256,
        )
        .unwrap();
        let discovery_capability = CapabilityIdentity::new(
            CapabilitySource::new("mcp-runtime").unwrap(),
            CapabilityNamespace::new("mcp.runtime").unwrap(),
            CapabilityName::new("discover").unwrap(),
            CapabilityVersion::new("1").unwrap(),
            Digest::of(DigestAlgorithm::Sha256, b"mcp runtime discovery"),
        );
        let discovery_grants = CapabilityGrantSnapshot::new(
            &inputs.config,
            [CapabilityGrant::new(
                inputs.authenticated.principal_id(),
                inputs.project,
                inputs.workspace,
                discovery_capability.clone(),
                discovery_schema.source().normalized_digest(),
                EffectClass::WorkspaceRead,
                inputs.constraints.clone(),
            )],
            DigestAlgorithm::Sha256,
        );
        let discovery_key = IdempotencyKey::parse("mcp-runtime-discovery").unwrap();
        let discovery_invocation = ToolCallId::generate().unwrap();
        let discovery_request = || {
            BrokerInvocation::generic(
                inputs.envelope(
                    &discovery_grants,
                    &discovery_capability,
                    discovery_schema.source().normalized_digest(),
                    b"{}",
                    discovery_invocation,
                    &discovery_key,
                ),
                &discovery_schema,
            )
        };
        connection
            .operations
            .set_binding(transport_auth::TransportBinding::new(
                &discovery_request(),
                server.as_str(),
                "stdio",
                "memory",
                None,
            ))
            .unwrap();
        let mut authorize_page = |_: &'static str, _: Option<&str>| Ok(discovery_request());
        let discovered = connection
            .discover_features(&mut authorize_page, &mut store)
            .await
            .unwrap();
        assert_eq!(discovered.tools().len(), 1);
        Arc::get_mut(&mut connection)
            .unwrap()
            .set_lifecycle_authority(&discovery_request());

        let feature = discovered.tools()[0].normalize().unwrap();
        let policy = McpCatalogPolicy::new(
            EffectClass::WorkspaceRead,
            RetrySafety::Idempotent,
            [Grant::WorkspaceRead],
            Vec::<String>::new(),
            Availability::Available,
        );
        let catalog_config = McpCatalogConfig::new(
            server.clone(),
            CatalogSource::new(
                SourceKind::Mcp,
                CapabilitySource::new("mcp-production-runtime").unwrap(),
                TrustDomain::new("mcp-production-runtime").unwrap(),
            )
            .unwrap(),
            CapabilityNamespace::new("mcp.production").unwrap(),
            CapabilityVersion::new("1").unwrap(),
            BTreeMap::from([(
                McpCatalogPolicyKey::new(
                    feature.identity().clone(),
                    feature.kind(),
                    feature.descriptor_digest(),
                ),
                policy,
            )]),
        )
        .unwrap();
        let runtime = McpCapabilityRuntime::from_configured_servers(
            McpCatalog::new(CatalogSnapshot::new([], DigestAlgorithm::Sha256).unwrap()),
            [McpRuntimeServer::new(
                catalog_config.clone(),
                discovered.clone(),
                Arc::clone(&connection),
            )
            .unwrap()],
        )
        .unwrap();
        let first_generation = runtime.refresh_registrations().unwrap()[0].1;

        let snapshot = runtime.catalog_snapshot().unwrap();
        let entry = snapshot.entries()[0].clone();
        assert!(
            entry
                .search()
                .summary()
                .starts_with("UNTRUSTED_MCP_METADATA_JSON=")
        );
        let grants = CapabilityGrantSnapshot::new(
            &inputs.config,
            [CapabilityGrant::new(
                inputs.authenticated.principal_id(),
                inputs.project,
                inputs.workspace,
                entry.identity().clone(),
                entry
                    .schemas()
                    .input()
                    .schema()
                    .source()
                    .normalized_digest(),
                EffectClass::WorkspaceRead,
                inputs.constraints.clone(),
            )],
            DigestAlgorithm::Sha256,
        );
        let session = DiscoverySession::new(
            &snapshot,
            &inputs.authenticated,
            &inputs.config,
            &grants,
            None,
            inputs.workspace,
            inputs.project,
            &inputs.constraints,
            RequestExtension::default(),
        );
        let inspection = session
            .inspect(session.search("fixture_echo", 1).unwrap()[0].handle())
            .unwrap();
        let binding = Arc::new(session.bind(&inspection).unwrap());
        let registry = BindingRegistry::new([Arc::clone(&binding)]).unwrap();
        let profile = ProjectionProfile::new(
            ProjectionTarget::new("fixture", "model", "runtime", 1).unwrap(),
            JSON_SCHEMA_2020_12,
            BTreeSet::from([
                "$schema".to_owned(),
                "additionalProperties".to_owned(),
                "maxLength".to_owned(),
                "maximum".to_owned(),
                "minLength".to_owned(),
                "minimum".to_owned(),
                "pattern".to_owned(),
                "properties".to_owned(),
                "required".to_owned(),
                "type".to_owned(),
            ]),
            Value::Bool(true),
            1024 * 1024,
            DigestAlgorithm::Sha256,
        )
        .unwrap();
        let provider = ProviderCapabilityContract::portable(
            ValidatedProjectionSupport::validate(&profile).unwrap(),
        );
        let plan = registry.plan(&provider, &session).unwrap();
        assert!(
            plan.eager_tools()
                .iter()
                .any(|tool| tool.name.0 == "tools_invoke")
        );
        let call = plan
            .invoke(
                &registry,
                &session,
                RegistrationCall::Portable(PortableInvokeCall::new(
                    serde_json::to_vec(&serde_json::json!({
                        "binding_id": binding.id().to_string(),
                        "input": {"text": "hello"}
                    }))
                    .unwrap(),
                )),
            )
            .unwrap();

        fixture_generation.store(1, Ordering::Release);
        let invocation = ToolCallId::generate().unwrap();
        let invocation_key = IdempotencyKey::parse("mcp-runtime-invoke").unwrap();
        let invoke = || {
            inputs.envelope(
                &grants,
                entry.identity(),
                binding.input_schema_digest(),
                call.input_bytes(),
                invocation,
                &invocation_key,
            )
        };
        let budget = BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100));
        let first = tokio::time::timeout(
            Duration::from_secs(2),
            runtime.invoke_registered(
                &call,
                invoke(),
                &mut store,
                &budget,
                &artifacts,
                &McpResultPolicy::default(),
            ),
        )
        .await
        .expect("fixture invocation timed out")
        .unwrap();
        let first = match first {
            BrokerOutcome::Completed(result) => result,
            BrokerOutcome::AuthRequired(_) => panic!("fixture unexpectedly required auth"),
        };
        assert_eq!(tool_calls.load(Ordering::Acquire), 1);
        let first_presentation = first.presentation.clone().unwrap();
        assert!(first_presentation.body().contains("fixture result"));
        assert!(!first_presentation.body().contains("END UNTRUSTED MCP DATA"));

        let replayed = tokio::time::timeout(
            Duration::from_secs(2),
            runtime.invoke_registered(
                &call,
                invoke(),
                &mut store,
                &budget,
                &artifacts,
                &McpResultPolicy::default(),
            ),
        )
        .await
        .expect("fixture replay timed out")
        .unwrap();
        let replayed = match replayed {
            BrokerOutcome::Completed(result) => result,
            BrokerOutcome::AuthRequired(_) => panic!("replay unexpectedly required auth"),
        };
        assert_eq!(tool_calls.load(Ordering::Acquire), 1);
        assert_eq!(replayed.presentation, Some(first_presentation));

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        {
            let refresh = runtime.drive_refresh_owned(
                &server,
                first_generation,
                RefreshLimits::default(),
                &mut store,
                &mut shutdown_rx,
            );
            tokio::pin!(refresh);
            tokio::select! {
                result = &mut refresh => panic!("refresh stopped before publication: {result:?}"),
                () = async {
                    loop {
                        if runtime.catalog_snapshot().unwrap().entries().is_empty() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                } => {}
                () = tokio::time::sleep(Duration::from_secs(2)) => {
                    panic!("listChanged refresh was not published")
                }
            }
            shutdown_tx.send(true).unwrap();
            refresh.await.unwrap();
        }

        let mut replacement = runtime_fixture_connection(
            &server,
            Arc::clone(&tool_calls),
            Arc::clone(&fixture_generation),
            Arc::clone(&closes),
        )
        .await;
        Arc::get_mut(&mut replacement)
            .unwrap()
            .set_lifecycle_authority(&discovery_request());
        replacement
            .operations
            .set_binding(transport_auth::TransportBinding::new(
                &discovery_request(),
                server.as_str(),
                "stdio",
                "memory",
                None,
            ))
            .unwrap();
        let second_generation = runtime
            .replace_and_close(
                McpRuntimeServer::new(catalog_config, discovered, replacement).unwrap(),
                &mut store,
            )
            .await
            .unwrap();
        assert!(second_generation > first_generation);
        assert_eq!(closes.load(Ordering::Acquire), 1);
        assert!(runtime.remove(&server, first_generation).unwrap().is_none());
        assert_eq!(
            runtime.connections.generation(&server).unwrap(),
            Some(second_generation)
        );

        runtime.shutdown(&mut store).await.unwrap();
        assert!(runtime.catalog_snapshot().unwrap().entries().is_empty());

        let removed_binding_replay = runtime
            .invoke_registered(
                &call,
                invoke(),
                &mut store,
                &budget,
                &artifacts,
                &McpResultPolicy::default(),
            )
            .await
            .unwrap();
        assert!(matches!(
            removed_binding_replay,
            BrokerOutcome::Completed(_)
        ));
        assert_eq!(tool_calls.load(Ordering::Acquire), 1);

        let other_principal = PrincipalId::generate().unwrap();
        let other = LocalPeerAuthenticator::new(BTreeMap::from([(
            502,
            GrantSnapshot::new(
                other_principal,
                inputs.project,
                BTreeSet::from([Grant::WorkspaceRead]),
            ),
        )]))
        .authenticate(&LocalPeerObservation::from_transport(502, 2, 502))
        .unwrap();
        let mut cross_owner = invoke();
        cross_owner.authenticated = &other;
        assert!(
            runtime
                .invoke_registered(
                    &call,
                    cross_owner,
                    &mut store,
                    &budget,
                    &artifacts,
                    &McpResultPolicy::default(),
                )
                .await
                .is_err()
        );
        assert_eq!(tool_calls.load(Ordering::Acquire), 1);

        let event_types = store
            .events()
            .unwrap()
            .into_iter()
            .map(|stored| stored.event.event_type)
            .collect::<Vec<EventType>>();
        assert!(
            event_types
                .iter()
                .any(|event| event.as_str() == "capability.invocation_outcome")
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn production_url_elicitation_broker_connector_callback_and_retry_acceptance() {
        use crate::{
            capabilities::kernel::{
                grant_ext::{EgressConstraint, GrantExtension, RequestExtension},
                invoke::{CanonicalOutput, DispatchOutcome},
            },
            domain::{
                egress::{CredentialHandle, DestinationGrant, EgressPolicy},
                mcp_callback::{McpCallbackAction, McpCallbackState},
                secret::SecretHandle,
            },
            protocols::mcp::{
                config::{
                    McpOwnerConfig, McpResponderConfig, McpServerConfig, McpTransportConfig,
                    McpUrlElicitationResponderConfig, McpUrlOriginConfig,
                },
                responders::{
                    CallbackSecretRegistry, ResponderAuthority, ResponderOutcomes, SourceRootProof,
                },
            },
            store::sqlite::mcp_callback::McpCallbackStore,
        };

        let root = std::env::temp_dir().join(format!(
            "kit-mcp-url-production-{}",
            EventId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("events.sqlite3");
        let artifacts = Arc::new(ArtifactStore::open(root.join("artifacts")).unwrap());
        let project_root = root.join("project");
        std::fs::create_dir(&project_root).unwrap();
        let project_root = std::fs::canonicalize(project_root).unwrap();
        let workspace = crate::workspace::revision::ManagedWorkspace::open(&project_root).unwrap();
        let workspace_revision = workspace.current_revision().unwrap().id().to_string();
        let mut store = test_support::open_sqlite_store(&database).unwrap();
        let mut inputs = RuntimeInputs::with_authority(BTreeSet::from([
            Grant::WorkspaceRead,
            Grant::NetworkEgress,
        ]));
        inputs.claim.expires_at_unix_micros = i64::MAX;
        store.install_driver_claim_for_test(inputs.claim).unwrap();
        let credential = SecretHandle::parse("env:URL_ACCEPTANCE").unwrap();
        let initial =
            EgressConstraint::new("https", "mcp-a.example", 443, credential.clone()).unwrap();
        let redirect =
            EgressConstraint::new("https", "mcp-b.example", 443, credential.clone()).unwrap();
        let extension = RequestExtension::new(Some(initial.clone()), Some(credential.clone()))
            .with_egresses([redirect.clone()])
            .unwrap()
            .with_workspace_revision(&workspace_revision);
        let server_config = McpServerConfig {
            id: "url-production".to_owned(),
            transport: McpTransportConfig::Http {
                endpoint: "https://mcp-a.example/mcp".to_owned(),
            },
            owner: McpOwnerConfig {
                principal_id: inputs.authenticated.principal_id(),
                project_id: inputs.project,
                workspace_id: Some(inputs.workspace),
            },
            source: "url-production".to_owned(),
            trust_domain: "url-production".to_owned(),
            namespace: "mcp.url.production".to_owned(),
            version: "1".to_owned(),
            credential_handle: Some(credential.clone()),
            credential_scope: None,
            egress: None,
            descriptors: Vec::new(),
            responders: McpResponderConfig {
                url_elicitation: Some(McpUrlElicitationResponderConfig {
                    timeout_millis: 2_000,
                    max_message_bytes: 256,
                    max_url_bytes: 1024,
                    max_elicitation_id_bytes: 128,
                    max_response_bytes: 1024,
                    allowed_origins: vec![McpUrlOriginConfig {
                        scheme: "https".to_owned(),
                        host: "auth.example.com".to_owned(),
                        port: 443,
                    }],
                }),
                ..McpResponderConfig::default()
            },
        };
        let scanner_secret = Arc::new(SecretLease::new(b"credential-canary".to_vec()));
        let outcomes = ResponderOutcomes::default()
            .with_secret_scope(
                &CallbackSecretRegistry::default(),
                inputs.authenticated.principal_id(),
                inputs.project,
                inputs.config.run_id(),
                inputs.attempt.attempt_id,
                server_config.id.as_str(),
                [credential.identifier()],
                &[scanner_secret],
            )
            .unwrap()
            .with_default_elicitation(
                &server_config,
                &database,
                Arc::clone(&artifacts),
                &project_root,
                inputs.authenticated.principal_id(),
                inputs.project,
                inputs.attempt,
                inputs.claim,
                inputs.workspace,
                &workspace_revision,
                1,
                Arc::clone(&inputs.cancellation),
            )
            .unwrap();
        let responders = crate::protocols::mcp::responders::install(
            &server_config,
            ResponderAuthority::new(
                inputs.attempt,
                inputs.claim,
                Arc::clone(&inputs.fence),
                Arc::new(AtomicU64::new(inputs.claim.lease_version)),
                Arc::new(|| true),
                server_config.id.as_str(),
                Arc::new(BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100))),
                Arc::clone(&inputs.cancellation),
                Arc::new(|_| true),
            ),
            &outcomes,
            SourceRootProof::issue(&server_config, &root).unwrap(),
            TransportLimits::default().channel_capacity(),
        )
        .unwrap();
        let initialize_arguments = responders.handler_config().initialize_arguments();
        let lifecycle = OwnedBrokerInvocation::run_lifecycle(
            &server_config.id,
            initialize_arguments,
            &inputs.authenticated,
            &inputs.config,
            inputs.workspace,
            extension.clone(),
            inputs.attempt,
            inputs.claim,
            Arc::clone(&inputs.fence),
            Arc::clone(&inputs.cancellation),
            inputs.occurred_at.clone(),
        )
        .unwrap();
        let resolver = Arc::new(UrlAcceptanceResolver {
            calls: Mutex::new(Vec::new()),
        });
        let dialer = Arc::new(UrlAcceptanceDialer::default());
        let policy = EgressPolicy::new([
            DestinationGrant::new(
                "https",
                "mcp-a.example",
                443,
                CredentialHandle::new(credential.identifier()).unwrap(),
            )
            .unwrap(),
            DestinationGrant::new(
                "https",
                "mcp-b.example",
                443,
                CredentialHandle::new(credential.identifier()).unwrap(),
            )
            .unwrap(),
        ])
        .with_resolver(resolver.clone());
        let lifecycle_request = lifecycle.invocation();
        let connection = match http::connect_streamable_http_with_handler_and_dialer(
            agentkit_mcp::McpServerId::new(&server_config.id),
            "https://mcp-a.example/mcp",
            &lifecycle_request,
            &policy,
            Arc::new(UrlAcceptanceCredentials),
            &mut store,
            TransportLimits::default(),
            responders.handler_config(),
            Some(dialer.clone()),
        )
        .await
        .unwrap()
        {
            StreamableHttpOutcome::Ready(connection) => (*connection).with_responders(responders),
            StreamableHttpOutcome::AuthRequired(_) => {
                panic!("initialize unexpectedly required auth")
            }
        };
        connection.arm_responders();

        let schema = NormalizedSchema::ingest(
            br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"}"#,
            JSON_SCHEMA_2020_12,
            b"URL acceptance operation",
            DigestAlgorithm::Sha256,
        )
        .unwrap();
        let capability = CapabilityIdentity::new(
            CapabilitySource::new("mcp-url-production").unwrap(),
            CapabilityNamespace::new("mcp.url.production").unwrap(),
            CapabilityName::new("authenticate").unwrap(),
            CapabilityVersion::new("1").unwrap(),
            Digest::of(DigestAlgorithm::Sha256, b"mcp url production"),
        );
        let grants = CapabilityGrantSnapshot::new(
            &inputs.config,
            [CapabilityGrant::new(
                inputs.authenticated.principal_id(),
                inputs.project,
                inputs.workspace,
                capability.clone(),
                schema.source().normalized_digest(),
                EffectClass::WorkspaceRead,
                inputs.constraints.clone(),
            )
            .with_extension(
                GrantExtension::new([initial, redirect], [credential.clone()], 0).unwrap(),
            )],
            DigestAlgorithm::Sha256,
        );
        let callback_store = McpCallbackStore::open(&database).unwrap();
        let budget = BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100));

        for (index, scenario, retry_safety, action, expected_ok, expected_calls) in [
            (
                1_u8,
                "accepted",
                RetrySafety::Idempotent,
                Some(McpCallbackAction::Accept),
                true,
                2,
            ),
            (
                2,
                "declined",
                RetrySafety::Idempotent,
                Some(McpCallbackAction::Decline),
                false,
                1,
            ),
            (
                3,
                "cancelled",
                RetrySafety::Idempotent,
                Some(McpCallbackAction::Cancel),
                false,
                1,
            ),
            (
                4,
                "non_idempotent",
                RetrySafety::NonIdempotent,
                Some(McpCallbackAction::Accept),
                false,
                1,
            ),
            (5, "secret", RetrySafety::Idempotent, None, false, 1),
            (6, "encoded_secret", RetrySafety::Idempotent, None, false, 1),
        ] {
            let key = IdempotencyKey::parse(&format!("url-production-{scenario}")).unwrap();
            let invocation = ToolCallId::generate().unwrap();
            let mut envelope = inputs.envelope(
                &grants,
                &capability,
                schema.source().normalized_digest(),
                b"{}",
                invocation,
                &key,
            );
            envelope.extension = extension.clone();
            envelope.retry_safety = retry_safety;
            let request = BrokerInvocation::generic(envelope, &schema);
            let invoke = async {
                let prepared = match broker::prepare(&request, &mut store, &budget, None).unwrap() {
                    BrokerPrepareOutcome::Authorized(prepared) => *prepared,
                    _ => panic!("broker did not authorize {scenario}"),
                };
                let result = connection
                    .call_tool_with_url_elicitation(
                        &request,
                        "authenticate",
                        serde_json::json!({"scenario":scenario}),
                        &mut store,
                    )
                    .await;
                broker::complete(
                    &request,
                    prepared,
                    if result.is_ok() {
                        DispatchOutcome::Succeeded(CanonicalOutput {
                            media_type: "application/json".to_owned(),
                            body: b"{}".to_vec(),
                            artifact_digests: Vec::new(),
                        })
                    } else {
                        DispatchOutcome::OutcomeUnknown {
                            code: "url_acceptance_expected".to_owned(),
                        }
                    },
                    &mut store,
                    &budget,
                    None,
                )
                .unwrap();
                result
            };
            let result = if let Some(action) = action {
                tokio::pin!(invoke);
                let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
                let callback = loop {
                    let pending = callback_store.pending(inputs.project).unwrap();
                    if let Some(callback) = pending.into_iter().find(|callback| {
                        callback.request["elicitation_id"]
                            .as_str()
                            .is_some_and(|id| id.starts_with(scenario))
                    }) {
                        break callback;
                    }
                    tokio::select! {
                        result = &mut invoke => {
                            panic!("invocation ended before durable callback for {scenario}: {:?}", result.err().map(|error| error.error));
                        }
                        () = tokio::time::sleep(Duration::from_millis(5)) => {}
                    }
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "durable callback not created for {scenario}"
                    );
                };
                callback_store.authority_live(&callback).unwrap();
                assert_eq!(
                    workspace.current_revision().unwrap().id().to_string(),
                    callback.workspace_revision
                );
                let resolution_key = IdempotencyKey::parse(&format!("resolve-{scenario}")).unwrap();
                callback_store
                    .reserve_resolution(
                        inputs.authenticated.principal_id(),
                        inputs.project,
                        callback.id,
                        &resolution_key,
                        [index; 32],
                        callback.version,
                        callback.challenge_generation,
                        &callback.schema_digest,
                    )
                    .unwrap();
                callback_store
                    .resolve_with_recheck(
                        inputs.authenticated.principal_id(),
                        inputs.project,
                        &resolution_key,
                        callback.id,
                        callback.version,
                        callback.challenge_generation,
                        &callback.schema_digest,
                        action,
                        Vec::new(),
                        [index; 32],
                        &callback,
                        &|_| true,
                        &workspace_revision,
                    )
                    .unwrap();
                let callback_id = callback.id;
                let result = invoke.await;
                let callback = callback_store.get(callback_id).unwrap();
                assert_eq!(callback.action, Some(action));
                assert!(matches!(
                    callback.state,
                    McpCallbackState::Delivered | McpCallbackState::DeliveryUnknown
                ));
                result
            } else {
                invoke.await
            };
            assert_eq!(result.is_ok(), expected_ok, "scenario {scenario}");
            assert_eq!(
                dialer.tool_calls.lock().unwrap().get(scenario).copied(),
                Some(expected_calls),
                "scenario {scenario} retried incorrectly"
            );
        }

        {
            let observations = dialer.observations.lock().unwrap();
            assert_eq!(observations[0].0, "https://mcp-a.example/mcp");
            assert_eq!(observations[1].0, "https://mcp-b.example/mcp");
            for (url, host, addresses) in observations.iter() {
                let expected = if url.starts_with("https://mcp-b.example") {
                    ("mcp-b.example", "1.1.1.1")
                } else {
                    ("mcp-a.example", "8.8.8.8")
                };
                assert_eq!(host, expected.0);
                assert_eq!(
                    addresses,
                    &[expected.1.parse::<std::net::IpAddr>().unwrap()]
                );
            }
        }
        {
            let resolutions = resolver.calls.lock().unwrap();
            assert!(resolutions.iter().any(|host| host == "mcp-a.example"));
            assert!(resolutions.iter().any(|host| host == "mcp-b.example"));
        }
        connection
            .close(&lifecycle_request, &mut store)
            .await
            .unwrap();
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn post_ready_gate_requires_one_exact_operation_and_arguments_per_message() {
        let gate = OperationGate::new();
        gate.set_binding(transport_auth::TransportBinding::for_test(
            "test-server",
            "http",
            "http://127.0.0.1/mcp",
            None,
        ))
        .unwrap();
        gate.ready.store(true, Ordering::Release);
        let operation = transport_auth::TransportOperation::parse("tools/call").unwrap();
        let arguments = serde_json::json!({"name":"read","arguments":{"path":"README.md"}});

        assert!(
            gate.authorize_message(
                &serde_json::json!({"id":1,"method":"tools/call","params":arguments})
            )
            .is_err()
        );
        gate.install(transport_auth::TransportAuthorization::for_test_arguments(
            operation.clone(),
            arguments.clone(),
        ))
        .unwrap();
        assert!(
            gate.authorize_message(&serde_json::json!({
                "method":"tools/call",
                "params":{"name":"read","arguments":{"path":"other"}}
            }))
            .is_err()
        );
        gate.clear();
        gate.install(transport_auth::TransportAuthorization::for_test_arguments(
            operation,
            arguments.clone(),
        ))
        .unwrap();
        assert!(
            gate.authorize_message(&serde_json::json!({"jsonrpc":"2.0","id":1,"result":{}}))
                .is_err()
        );
        assert!(
            gate.authorize_message(
                &serde_json::json!({"id":1,"method":"tools/call","params":arguments})
            )
            .is_ok()
        );
        assert!(
            gate.authorize_message(&serde_json::json!({"id":2,"method":"tools/call","params":{}}))
                .is_err()
        );
    }

    #[test]
    fn url_retry_rotates_the_one_message_authorization_generation() {
        let gate = OperationGate::new();
        gate.set_binding(transport_auth::TransportBinding::for_test(
            "test-server",
            "http",
            "http://127.0.0.1/mcp",
            None,
        ))
        .unwrap();
        gate.ready.store(true, Ordering::Release);
        let authorization = transport_auth::TransportAuthorization::for_test_arguments(
            transport_auth::TransportOperation::parse("tools/call").unwrap(),
            serde_json::json!({"name":"read","arguments":{"path":"README.md"}}),
        );
        let first = gate.install(authorization).unwrap();
        let request = serde_json::json!({
            "id": 1,
            "method":"tools/call",
            "params":{"name":"read","arguments":{"path":"README.md"}}
        });
        assert!(gate.authorize_message(&request).is_ok());
        assert!(gate.authorize_message(&request).is_err());

        gate.clear_generation(first).unwrap();
        let second = gate
            .install(transport_auth::TransportAuthorization::for_test_arguments(
                transport_auth::TransportOperation::parse("tools/call").unwrap(),
                serde_json::json!({"name":"read","arguments":{"path":"README.md"}}),
            ))
            .unwrap();
        assert!(gate.authorize_message(&request).is_ok());
        gate.clear_generation(second).unwrap();
    }

    #[test]
    fn only_exact_captured_terminal_32042_response_mints_undispatched_digest() {
        let data = serde_json::json!({
            "url":"https://auth.example/complete",
            "elicitationId":"challenge"
        });
        for (code, extra, accepted) in [
            (-32042, false, true),
            (-32041, false, false),
            (-32042, true, false),
        ] {
            let gate = OperationGate::new();
            gate.set_binding(transport_auth::TransportBinding::for_test(
                "test-server",
                "http",
                "http://127.0.0.1/mcp",
                None,
            ))
            .unwrap();
            gate.ready.store(true, Ordering::Release);
            let generation = gate
                .install(transport_auth::TransportAuthorization::for_test_arguments(
                    transport_auth::TransportOperation::parse("tools/call").unwrap(),
                    serde_json::json!({"name":"read"}),
                ))
                .unwrap();
            gate.authorize_message(&serde_json::json!({
                "jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"read"}
            }))
            .unwrap();
            let mut error = serde_json::json!({
                "code":code,
                "message":"authenticate",
                "data":data
            });
            if extra {
                error["effect"] = Value::String("unknown".to_owned());
            }
            let payload = RawPayload::from_value(
                serde_json::json!({"jsonrpc":"2.0","id":7,"error":error}),
                PayloadLimits::default(),
            )
            .unwrap();
            assert!(gate.capture_payload(payload).unwrap());
            assert_eq!(
                gate.exact_terminal_url_elicitation_digest(
                    generation,
                    "authenticate",
                    Some(&data),
                )
                .is_ok(),
                accepted
            );
        }
    }

    #[test]
    fn callback_response_uses_its_owning_stream_and_configured_scanners() {
        let gate = OperationGate::new();
        gate.set_responder_scanner(Arc::new(
            crate::protocols::mcp::responders::CallbackSecretScanner::new([
                "configured-secret".into()
            ]),
        ));
        let first = Arc::new(McpResponseScanner::new(&[SecretLease::new(
            b"first-stream-secret".to_vec(),
        )]));
        let second = Arc::new(McpResponseScanner::new(&[SecretLease::new(
            b"second-stream-secret".to_vec(),
        )]));
        for (id, scanner) in [(1, first), (2, second)] {
            let payload = RawPayload::from_value(
                serde_json::json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "method":"roots/list",
                    "params":{}
                }),
                PayloadLimits::default(),
            )
            .unwrap();
            gate.bind_callback_scanner(&payload, scanner).unwrap();
        }

        let first_response = serde_json::json!({
            "jsonrpc":"2.0",
            "id":1,
            "result":{"roots":[{"name":"first-stream-secret"}]}
        });
        assert!(matches!(
            gate.scan_callback_response(
                &first_response,
                &serde_json::to_vec(&first_response).unwrap()
            ),
            Err(TransportError::SensitivePayload)
        ));

        let second_response = serde_json::json!({
            "jsonrpc":"2.0",
            "id":2,
            "result":{"roots":[{"name":"configured-secret"}]}
        });
        assert!(matches!(
            gate.scan_callback_response(
                &second_response,
                &serde_json::to_vec(&second_response).unwrap()
            ),
            Err(TransportError::SensitivePayload)
        ));
    }

    #[test]
    fn callback_binding_accepts_only_typed_negotiated_idless_notifications() {
        let gate = OperationGate::new();
        gate.set_binding(transport_auth::TransportBinding::for_test(
            "test-server",
            "http",
            "http://127.0.0.1/mcp",
            None,
        ))
        .unwrap();
        gate.install(
            transport_auth::TransportAuthorization::for_test_bound_arguments_binding(
                transport_auth::TransportOperation::parse("initialize").unwrap(),
                kit_authorized_initialize_arguments(),
                None,
                None,
                transport_auth::TransportBinding::for_test(
                    "test-server",
                    "http",
                    "http://127.0.0.1/mcp",
                    None,
                ),
            ),
        )
        .unwrap();
        gate.authorize_message(&serde_json::json!({
            "jsonrpc":"2.0",
            "id":1,
            "method":"initialize",
            "params":kit_authorized_initialize_arguments()
        }))
        .unwrap();
        let scanner = Arc::new(McpResponseScanner::new(&[]));
        let initialize = RawPayload::from_value(
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":1,
                "result":{
                    "protocolVersion":"2025-11-25",
                    "capabilities":{"tools":{"listChanged":true},"resources":{}},
                    "serverInfo":{"name":"notification-test","version":"1"}
                }
            }),
            PayloadLimits::default(),
        )
        .unwrap();
        gate.bind_callback_scanner(&initialize, Arc::clone(&scanner))
            .unwrap();

        for notification in [
            serde_json::json!({
                "jsonrpc":"2.0",
                "method":"notifications/tools/list_changed"
            }),
            serde_json::json!({
                "jsonrpc":"2.0",
                "method":"notifications/progress",
                "params":{"progressToken":"refresh","progress":1}
            }),
            serde_json::json!({
                "jsonrpc":"2.0",
                "method":"notifications/cancelled",
                "params":{"requestId":7,"reason":"stopped"}
            }),
        ] {
            assert!(!agentkit_mcp::has_responder_delivery_permit(&notification));
            gate.bind_callback_scanner(
                &RawPayload::from_value(notification, PayloadLimits::default()).unwrap(),
                Arc::clone(&scanner),
            )
            .unwrap();
        }
        assert_eq!(Arc::strong_count(&scanner), 1);

        for notification in [
            serde_json::json!({
                "jsonrpc":"2.0",
                "method":"notifications/resources/list_changed"
            }),
            serde_json::json!({
                "jsonrpc":"2.0",
                "method":"notifications/tools/list_changed",
                "params":{}
            }),
            serde_json::json!({
                "jsonrpc":"2.0",
                "method":"notifications/progress",
                "params":{"progress":1}
            }),
            serde_json::json!({
                "jsonrpc":"2.0",
                "method":"notifications/unknown"
            }),
        ] {
            assert!(
                gate.bind_callback_scanner(
                    &RawPayload::from_value(notification, PayloadLimits::default()).unwrap(),
                    Arc::clone(&scanner),
                )
                .is_err()
            );
        }
        assert_eq!(Arc::strong_count(&scanner), 1);
    }

    #[test]
    fn connection_close_state_drops_unanswered_callback_scanners() {
        let gate = OperationGate::new();
        let scanner = Arc::new(McpResponseScanner::new(&[SecretLease::new(
            b"stream-secret".to_vec(),
        )]));
        let payload = RawPayload::from_value(
            serde_json::json!({
                "jsonrpc":"2.0",
                "id":1,
                "method":"roots/list",
                "params":{}
            }),
            PayloadLimits::default(),
        )
        .unwrap();
        gate.bind_callback_scanner(&payload, Arc::clone(&scanner))
            .unwrap();
        assert_eq!(Arc::strong_count(&scanner), 2);
        gate.clear_response_scanners();
        assert_eq!(Arc::strong_count(&scanner), 1);
    }

    #[test]
    fn optional_arguments_match_wire_omission_not_explicit_null() {
        let gate = OperationGate::new();
        gate.set_binding(transport_auth::TransportBinding::for_test(
            "test-server",
            "http",
            "http://127.0.0.1/mcp",
            None,
        ))
        .unwrap();
        gate.ready.store(true, Ordering::Release);
        let operation = transport_auth::TransportOperation::parse("prompts/get").unwrap();
        let omitted = serde_json::json!({"name":"summary"});
        gate.install(transport_auth::TransportAuthorization::for_test_arguments(
            operation.clone(),
            omitted.clone(),
        ))
        .unwrap();
        assert!(
            gate.authorize_message(&serde_json::json!({
                "id":1,"method":"prompts/get",
                "params":omitted
            }))
            .is_ok()
        );

        gate.clear();
        gate.install(transport_auth::TransportAuthorization::for_test_arguments(
            operation,
            serde_json::json!({"name":"summary","arguments":null}),
        ))
        .unwrap();
        assert!(
            gate.authorize_message(&serde_json::json!({
                "method":"prompts/get",
                "params":{"name":"summary"}
            }))
            .is_err()
        );
    }

    #[test]
    fn payload_capture_ignores_stale_response_ids() {
        let gate = OperationGate::new();
        gate.set_binding(transport_auth::TransportBinding::for_test(
            "test-server",
            "http",
            "http://127.0.0.1/mcp",
            None,
        ))
        .unwrap();
        gate.ready.store(true, Ordering::Release);
        let generation = gate
            .install(transport_auth::TransportAuthorization::for_test_arguments(
                transport_auth::TransportOperation::parse("tools/list").unwrap(),
                serde_json::json!({}),
            ))
            .unwrap();
        gate.authorize_message(&serde_json::json!({"id":7,"method":"tools/list","params":{}}))
            .unwrap();

        let stale = RawPayload::parse(
            br#"{"id":6,"result":{"tools":[]}}"#,
            PayloadLimits::default(),
        )
        .unwrap();
        assert!(!gate.capture_payload(stale).unwrap());
        let current = RawPayload::parse(
            br#"{"id":7,"result":{"tools":[]}}"#,
            PayloadLimits::default(),
        )
        .unwrap();
        assert!(gate.capture_payload(current).unwrap());
        assert_eq!(
            gate.clear_generation(generation).unwrap().unwrap().value()["id"],
            7
        );
    }

    #[test]
    fn concurrent_list_completion_cannot_clear_or_replace_another_generation() {
        let gate = OperationGate::new();
        gate.set_binding(transport_auth::TransportBinding::for_test(
            "test-server",
            "http",
            "http://127.0.0.1/mcp",
            None,
        ))
        .unwrap();
        gate.ready.store(true, Ordering::Release);
        let first = gate
            .install(transport_auth::TransportAuthorization::for_test_arguments(
                transport_auth::TransportOperation::parse("tools/list").unwrap(),
                serde_json::json!({}),
            ))
            .unwrap();
        gate.authorize_message(&serde_json::json!({"id":1,"method":"tools/list","params":{}}))
            .unwrap();
        gate.capture_payload(
            RawPayload::parse(
                br#"{"id":1,"result":{"tools":[{"name":"first","inputSchema":{}}]}}"#,
                PayloadLimits::default(),
            )
            .unwrap(),
        )
        .unwrap();

        assert!(
            gate.install(transport_auth::TransportAuthorization::for_test_arguments(
                transport_auth::TransportOperation::parse("tools/list").unwrap(),
                serde_json::json!({}),
            ))
            .is_err()
        );
        let first_payload = gate.clear_generation(first).unwrap().unwrap();
        assert_eq!(first_payload.value()["result"]["tools"][0]["name"], "first");

        let second = gate
            .install(transport_auth::TransportAuthorization::for_test_arguments(
                transport_auth::TransportOperation::parse("tools/list").unwrap(),
                serde_json::json!({}),
            ))
            .unwrap();
        assert!(gate.clear_generation(first).is_err());
        gate.authorize_message(&serde_json::json!({"id":2,"method":"tools/list","params":{}}))
            .unwrap();
        gate.capture_payload(
            RawPayload::parse(
                br#"{"id":2,"result":{"tools":[{"name":"second","inputSchema":{}}]}}"#,
                PayloadLimits::default(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            gate.clear_generation(second).unwrap().unwrap().value()["result"]["tools"][0]["name"],
            "second"
        );
    }

    #[test]
    fn ready_gate_allows_broker_bound_capabilities_but_rejects_endpoint_confusion() {
        let gate = OperationGate::new();
        let binding = transport_auth::TransportBinding::for_test(
            "server-a",
            "http",
            "https://a.example/mcp",
            Some("session-a".to_owned()),
        );
        gate.set_binding(binding.clone()).unwrap();
        let initial = Arc::new(
            transport_auth::TransportAuthorization::for_test_capability_binding(
                transport_auth::TransportOperation::parse("tools/call").unwrap(),
                "read",
                binding.clone(),
            ),
        );
        gate.bind_connection(initial).unwrap();
        let generation = gate
            .install(
                transport_auth::TransportAuthorization::for_test_capability_binding(
                    transport_auth::TransportOperation::parse("tools/call").unwrap(),
                    "write",
                    binding,
                ),
            )
            .unwrap();
        gate.clear_generation(generation).unwrap();
        gate.set_binding(transport_auth::TransportBinding::for_test(
            "server-b",
            "http",
            "https://b.example/mcp",
            Some("session-a".to_owned()),
        ))
        .unwrap();
        assert!(
            gate.install(
                transport_auth::TransportAuthorization::for_test_capability_binding(
                    transport_auth::TransportOperation::parse("tools/call").unwrap(),
                    "read",
                    transport_auth::TransportBinding::for_test(
                        "server-b",
                        "http",
                        "https://b.example/mcp",
                        Some("session-a".to_owned()),
                    ),
                ),
            )
            .is_err()
        );
    }

    #[test]
    fn initialize_and_initialized_require_exact_canonical_params() {
        let gate = OperationGate::new();
        gate.set_binding(transport_auth::TransportBinding::for_test(
            "test-server",
            "http",
            "http://127.0.0.1/mcp",
            None,
        ))
        .unwrap();
        gate.install(
            transport_auth::TransportAuthorization::for_test_bound_arguments_binding(
                transport_auth::TransportOperation::parse("initialize").unwrap(),
                kit_authorized_initialize_arguments(),
                None,
                None,
                transport_auth::TransportBinding::for_test(
                    "test-server",
                    "http",
                    "http://127.0.0.1/mcp",
                    None,
                ),
            ),
        )
        .unwrap();
        assert!(
            gate.authorize_message(&serde_json::json!({
                "id":1,"method":"initialize",
                "params":kit_authorized_initialize_arguments()
            }))
            .is_ok()
        );
        assert!(
            gate.authorize_message(&serde_json::json!({
                "method":"notifications/initialized",
                "params":{}
            }))
            .is_err()
        );

        gate.initialized_followup.store(true, Ordering::Release);
        assert!(
            gate.authorize_message(&serde_json::json!({
                "method":"notifications/initialized"
            }))
            .is_ok()
        );
    }

    #[test]
    fn refresh_driver_rejects_unsolicited_kinds_and_lag_marks_only_enabled_kinds() {
        let (_events, receiver) = tokio::sync::broadcast::channel(4);
        let server = ConfiguredServerIdentity::new("driver-test").unwrap();
        let mut driver = McpRefreshDriver {
            events: receiver,
            server: server.clone(),
            negotiated: Arc::new(RwLock::new(
                NegotiatedFeatureKinds::with_list_changed_values(
                    [FeatureListKind::Tools, FeatureListKind::Resources],
                    [
                        (FeatureListKind::Tools, false),
                        (FeatureListKind::Resources, true),
                    ],
                ),
            )),
            coalescer: RefreshCoalescer::new(RefreshLimits::default()),
            started: Instant::now(),
        };

        assert!(driver.handle_event(Ok(McpServerEvent::ToolListChanged)));
        assert_eq!(driver.coalescer.pending_kinds(), 0);
        assert!(driver.handle_event(Err(tokio::sync::broadcast::error::RecvError::Lagged(1))));
        assert_eq!(driver.coalescer.pending_kinds(), 1);
        assert!(driver.handle_event(Ok(McpServerEvent::ResourceListChanged)));
        assert_eq!(driver.coalescer.pending_kinds(), 1);

        *driver.negotiated.write().unwrap() = NegotiatedFeatureKinds::with_list_changed(
            [FeatureListKind::Tools],
            [FeatureListKind::Tools],
        );
        assert!(driver.handle_event(Ok(McpServerEvent::ToolListChanged)));
        assert_eq!(driver.coalescer.pending_kinds(), 2);
        assert!(driver.handle_event(Ok(McpServerEvent::ResourceListChanged)));
        assert_eq!(driver.coalescer.pending_kinds(), 2);
    }
}
