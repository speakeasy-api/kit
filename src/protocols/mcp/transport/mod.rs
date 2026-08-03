mod http;
mod stdio;

use std::{
    fmt, io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use agentkit_mcp::{
    CallToolResult, GetPromptResult, McpConnection, McpError, McpHttpClient, McpPrompt,
    McpProtocolVersion, McpResource, McpTool, PINNED_PROTOCOL_VERSION, ReadResourceResult,
    kit_authorized_initialize_arguments,
};
use serde_json::Value;

use crate::{
    capabilities::broker::{
        BrokerError, BrokerInvocation,
        transport_auth::{self, TransportAuthState},
    },
    store::sqlite::append::SqliteStore,
};

pub use http::{
    HttpCredentialBroker, HttpCredentialError, HttpSecretContext, StreamableHttpOutcome,
    connect_streamable_http, resolve_streamable_http_auth, resume_streamable_http,
};
pub use stdio::{
    OwnedStdioLaunchError, OwnedStdioLimits, OwnedStdioProcess, OwnedStdioProcessService,
    SandboxedStdioLauncher, connect_stdio,
};

pub const PROTOCOL_REVISION: McpProtocolVersion = PINNED_PROTOCOL_VERSION;

fn validate_initialize_arguments(request: &BrokerInvocation<'_>) -> Result<(), TransportError> {
    let arguments = serde_json::from_slice::<Value>(request.arguments())
        .map_err(|_| BrokerError::InvalidArguments)?;
    if arguments == kit_authorized_initialize_arguments() {
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
}

pub struct ReadyConnection {
    connection: McpConnection,
    request_timeout: Duration,
    close_timeout: Duration,
    operations: Arc<OperationGate>,
    cleanup: Option<Arc<dyn McpHttpClient>>,
    can_reinitialize: bool,
}

impl ReadyConnection {
    fn new(
        connection: McpConnection,
        limits: TransportLimits,
        operations: Arc<OperationGate>,
        cleanup: Option<Arc<dyn McpHttpClient>>,
        can_reinitialize: bool,
    ) -> Result<Self, TransportError> {
        if connection.negotiated_protocol_version().as_ref() != Some(&PROTOCOL_REVISION) {
            return Err(TransportError::ProtocolVersionRefused);
        }
        operations.ready.store(true, Ordering::Release);
        Ok(Self {
            connection,
            request_timeout: limits.request_timeout(),
            close_timeout: limits.close_timeout(),
            operations,
            cleanup,
            can_reinitialize,
        })
    }

    pub fn negotiated_protocol_version(&self) -> McpProtocolVersion {
        PROTOCOL_REVISION
    }

    fn authorize(
        &self,
        request: &BrokerInvocation<'_>,
        operation: &str,
        arguments: Value,
        store: &mut SqliteStore,
    ) -> Result<transport_auth::TransportDispatch, TransportError> {
        let expected = serde_json::from_slice::<Value>(request.arguments())
            .map_err(|_| BrokerError::InvalidArguments)?;
        if expected != arguments {
            return Err(BrokerError::InvalidArguments.into());
        }
        let operation = transport_auth::TransportOperation::parse(operation)?;
        let binding = self.operations.binding()?;
        let (authorization, replay) =
            authorize_ready_operation(request, &operation, &binding, store)?;
        self.operations.install(authorization)?;
        let dispatch =
            match transport_auth::begin_dispatch(request, &operation, &binding, replay, store) {
                Ok(dispatch) => dispatch,
                Err(error) => {
                    self.operations.clear();
                    return Err(error.into());
                }
            };
        Ok(dispatch)
    }

    fn finish_operation<T>(
        &self,
        result: Result<T, McpError>,
        dispatch: transport_auth::TransportDispatch,
        request: &BrokerInvocation<'_>,
        store: &mut SqliteStore,
    ) -> Result<T, TransportError> {
        match result {
            Ok(value) => {
                let persisted = transport_auth::finish_dispatch(
                    request,
                    dispatch,
                    transport_auth::TransportDispatchOutcome::Completed,
                    store,
                );
                self.operations.clear();
                persisted?;
                Ok(value)
            }
            Err(McpError::AuthRequired(challenge)) => {
                let (kind, operation, scope) = http::auth_challenge(&challenge)?;
                let persisted = transport_auth::interrupt_dispatch(
                    request,
                    dispatch,
                    kind,
                    &operation,
                    scope.as_deref(),
                    store,
                );
                self.operations.clear();
                let challenge = persisted?;
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
                self.operations.clear();
                persisted?;
                Err(typed.unwrap_or_else(|| error.into()))
            }
        }
    }

    fn finish_timed_operation<T>(
        &self,
        result: Result<Result<T, McpError>, tokio::time::error::Elapsed>,
        dispatch: transport_auth::TransportDispatch,
        request: &BrokerInvocation<'_>,
        store: &mut SqliteStore,
        operation: &'static str,
    ) -> Result<T, TransportError> {
        match result {
            Ok(result) => self.finish_operation(result, dispatch, request, store),
            Err(_) => {
                let persisted = transport_auth::finish_dispatch(
                    request,
                    dispatch,
                    transport_auth::TransportDispatchOutcome::OutcomeUnknown,
                    store,
                );
                self.operations.clear();
                persisted?;
                Err(TransportError::Timeout(operation))
            }
        }
    }

    pub async fn list_tools_page(
        &self,
        request: &BrokerInvocation<'_>,
        cursor: Option<String>,
        store: &mut SqliteStore,
    ) -> Result<(Vec<McpTool>, Option<String>), TransportError> {
        let arguments = cursor.as_ref().map_or_else(
            || serde_json::json!({}),
            |cursor| serde_json::json!({"cursor": cursor}),
        );
        let dispatch = self.authorize(request, "tools/list", arguments, store)?;
        let result = tokio::time::timeout(
            self.request_timeout,
            self.connection.list_tools_page(cursor),
        )
        .await;
        self.finish_timed_operation(result, dispatch, request, store, "tools/list response")
    }

    pub async fn list_resources_page(
        &self,
        request: &BrokerInvocation<'_>,
        cursor: Option<String>,
        store: &mut SqliteStore,
    ) -> Result<(Vec<McpResource>, Option<String>), TransportError> {
        let arguments = cursor.as_ref().map_or_else(
            || serde_json::json!({}),
            |cursor| serde_json::json!({"cursor": cursor}),
        );
        let dispatch = self.authorize(request, "resources/list", arguments, store)?;
        let result = tokio::time::timeout(
            self.request_timeout,
            self.connection.list_resources_page(cursor),
        )
        .await;
        self.finish_timed_operation(result, dispatch, request, store, "resources/list response")
    }

    pub async fn list_prompts_page(
        &self,
        request: &BrokerInvocation<'_>,
        cursor: Option<String>,
        store: &mut SqliteStore,
    ) -> Result<(Vec<McpPrompt>, Option<String>), TransportError> {
        let arguments = cursor.as_ref().map_or_else(
            || serde_json::json!({}),
            |cursor| serde_json::json!({"cursor": cursor}),
        );
        let dispatch = self.authorize(request, "prompts/list", arguments, store)?;
        let result = tokio::time::timeout(
            self.request_timeout,
            self.connection.list_prompts_page(cursor),
        )
        .await;
        self.finish_timed_operation(result, dispatch, request, store, "prompts/list response")
    }

    pub async fn call_tool(
        &self,
        request: &BrokerInvocation<'_>,
        name: &str,
        arguments: Value,
        store: &mut SqliteStore,
    ) -> Result<CallToolResult, TransportError> {
        let mut wire =
            serde_json::Map::from_iter([("name".to_owned(), Value::String(name.to_owned()))]);
        if !arguments.is_null() {
            wire.insert("arguments".to_owned(), arguments.clone());
        }
        let dispatch = self.authorize(request, "tools/call", Value::Object(wire), store)?;
        let result = tokio::time::timeout(
            self.request_timeout,
            self.connection.call_tool(name, arguments),
        )
        .await;
        self.finish_timed_operation(result, dispatch, request, store, "tools/call response")
    }

    pub async fn read_resource(
        &self,
        request: &BrokerInvocation<'_>,
        uri: &str,
        store: &mut SqliteStore,
    ) -> Result<ReadResourceResult, TransportError> {
        let dispatch = self.authorize(
            request,
            "resources/read",
            serde_json::json!({"uri": uri}),
            store,
        )?;
        let result =
            tokio::time::timeout(self.request_timeout, self.connection.read_resource(uri)).await;
        self.finish_timed_operation(result, dispatch, request, store, "resources/read response")
    }

    pub async fn get_prompt(
        &self,
        request: &BrokerInvocation<'_>,
        name: &str,
        arguments: Value,
        store: &mut SqliteStore,
    ) -> Result<GetPromptResult, TransportError> {
        let mut wire =
            serde_json::Map::from_iter([("name".to_owned(), Value::String(name.to_owned()))]);
        if !arguments.is_null() {
            wire.insert("arguments".to_owned(), arguments.clone());
        }
        let dispatch = self.authorize(request, "prompts/get", Value::Object(wire), store)?;
        let result = tokio::time::timeout(
            self.request_timeout,
            self.connection.get_prompt(name, arguments),
        )
        .await;
        self.finish_timed_operation(result, dispatch, request, store, "prompts/get response")
    }

    pub async fn reinitialize_expired_session(
        &self,
        request: &BrokerInvocation<'_>,
        store: &mut SqliteStore,
    ) -> Result<(), TransportError> {
        if !self.can_reinitialize {
            return Err(TransportError::AuthorizationMismatch);
        }
        validate_initialize_arguments(request)?;
        let arguments = serde_json::from_slice(request.arguments())
            .map_err(|_| BrokerError::InvalidArguments)?;
        let dispatch = self.authorize(request, "initialize", arguments, store)?;
        let result = tokio::time::timeout(
            self.request_timeout,
            self.connection.reinitialize_authorized(),
        )
        .await;
        self.finish_timed_operation(result, dispatch, request, store, "initialize response")
    }

    pub async fn close(
        self,
        request: &BrokerInvocation<'_>,
        store: &mut SqliteStore,
    ) -> Result<(), TransportError> {
        let dispatch = self.authorize(request, "session/delete", serde_json::json!({}), store)?;
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
        self.finish_operation(result, dispatch, request, store)
    }
}

pub(crate) fn authorize_ready_operation(
    request: &BrokerInvocation<'_>,
    operation: &transport_auth::TransportOperation,
    binding: &transport_auth::TransportBinding,
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
        transport_auth::authorize_replay(request, operation, binding, store)?
    } else {
        transport_auth::authorize(request, operation, binding, store)?
    };
    Ok((authorization, replay))
}

struct OperationGate {
    ready: AtomicBool,
    initialized_followup: AtomicBool,
    message_sent: AtomicBool,
    next: Mutex<Option<Arc<transport_auth::TransportAuthorization>>>,
    failure: Mutex<Option<TransportFailure>>,
    binding: Mutex<Option<transport_auth::TransportBinding>>,
    connection: Mutex<Option<Arc<transport_auth::TransportAuthorization>>>,
}

enum TransportFailure {
    InvalidHeader,
    ResponseTooLarge,
    SseEventTooLarge,
    Credential(HttpCredentialError),
    MissingProtocolVersion(agentkit_mcp::McpServerId),
    StdioParse(String),
    StdioTimeout,
}

impl OperationGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ready: AtomicBool::new(false),
            initialized_followup: AtomicBool::new(false),
            message_sent: AtomicBool::new(false),
            next: Mutex::new(None),
            failure: Mutex::new(None),
            binding: Mutex::new(None),
            connection: Mutex::new(None),
        })
    }

    fn install(
        &self,
        authorization: transport_auth::TransportAuthorization,
    ) -> Result<(), TransportError> {
        let mut next = self
            .next
            .lock()
            .map_err(|_| TransportError::AuthorizationMismatch)?;
        if next.is_some() {
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
        if self.binding()? != *authorization.binding() {
            return Err(TransportError::AuthorizationMismatch);
        }
        self.message_sent.store(false, Ordering::Release);
        *next = Some(Arc::new(authorization));
        Ok(())
    }

    fn authorize_message<T: serde::Serialize>(
        &self,
        message: &T,
    ) -> Result<Arc<transport_auth::TransportAuthorization>, TransportError> {
        let message =
            serde_json::to_value(message).map_err(|_| TransportError::AuthorizationMismatch)?;
        let method = message.get("method").and_then(Value::as_str);
        let method = method.ok_or(TransportError::AuthorizationMismatch)?;
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
        let params = message
            .get("params")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        if authorization.operation().as_str() != method || arguments != params {
            return Err(TransportError::AuthorizationMismatch);
        }
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

    fn clear(&self) {
        if let Ok(mut next) = self.next.lock() {
            *next = None;
        }
        self.initialized_followup.store(false, Ordering::Release);
        self.message_sent.store(false, Ordering::Release);
    }

    fn fail(&self, failure: TransportFailure) {
        if let Ok(mut slot) = self.failure.lock()
            && slot.is_none()
        {
            *slot = Some(failure);
        }
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
            })
    }
}

#[derive(Debug)]
pub enum TransportError {
    InvalidLimits,
    InvalidEndpoint,
    AuthorizationMismatch,
    PolicyAuthorizationMismatch,
    InvalidHeader,
    ResponseTooLarge,
    SseEventTooLarge,
    ProtocolVersionRefused,
    OwnedProcessUnavailable,
    Timeout(&'static str),
    Broker(BrokerError),
    Agentkit(Box<McpError>),
    Io(io::Error),
    Credential(HttpCredentialError),
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
            Self::PolicyAuthorizationMismatch => {
                formatter.write_str("MCP HTTP endpoint does not match policy authorization")
            }
            Self::InvalidHeader => formatter.write_str("invalid or oversized MCP HTTP header"),
            Self::ResponseTooLarge => formatter.write_str("MCP HTTP response exceeds its bound"),
            Self::SseEventTooLarge => formatter.write_str("MCP SSE event exceeds its bound"),
            Self::ProtocolVersionRefused => {
                formatter.write_str("MCP server did not negotiate protocol revision 2025-11-25")
            }
            Self::OwnedProcessUnavailable => {
                formatter.write_str("durable MCP owned-process service is unavailable")
            }
            Self::Timeout(operation) => write!(formatter, "MCP {operation} timed out"),
            Self::Broker(error) => error.fmt(formatter),
            Self::Agentkit(error) => error.fmt(formatter),
            Self::Io(error) => error.fmt(formatter),
            Self::Credential(error) => error.fmt(formatter),
            Self::AuthRequired(_) => formatter.write_str("MCP operation requires authorization"),
        }
    }
}

impl std::error::Error for TransportError {}

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

#[cfg(test)]
mod tests {
    use super::*;

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
            gate.authorize_message(&serde_json::json!({"method":"tools/call","params":arguments}))
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
            gate.authorize_message(&serde_json::json!({"method":"tools/call","params":arguments}))
                .is_ok()
        );
        assert!(
            gate.authorize_message(&serde_json::json!({"method":"tools/call","params":{}}))
                .is_err()
        );
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
                "method":"prompts/get",
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
    fn ready_gate_rejects_capability_and_endpoint_confusion() {
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
        assert!(
            gate.install(
                transport_auth::TransportAuthorization::for_test_capability_binding(
                    transport_auth::TransportOperation::parse("tools/call").unwrap(),
                    "write",
                    binding,
                ),
            )
            .is_err()
        );
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
                "method":"initialize",
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
}
