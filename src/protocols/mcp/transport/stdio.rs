use std::{future::Future, io, sync::Arc, time::Duration};

use agentkit_mcp::{McpConnection, McpHandlerConfig, McpServerId};
use rmcp::{
    RoleClient,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};

use crate::{
    capabilities::broker::{
        BrokerInvocation,
        transport_auth::{self, TransportBinding, TransportOperation},
    },
    executor::process::own::PreparedCommandToken,
    store::sqlite::append::SqliteStore,
};

use super::{OperationGate, ReadyConnection, TransportError, TransportFailure, TransportLimits};

#[derive(Clone, Copy, Debug)]
pub struct OwnedStdioLimits {
    max_frame_bytes: usize,
    io_timeout: Duration,
    close_timeout: Duration,
}

impl OwnedStdioLimits {
    pub const fn max_frame_bytes(self) -> usize {
        self.max_frame_bytes
    }

    pub const fn io_timeout(self) -> Duration {
        self.io_timeout
    }

    pub const fn close_timeout(self) -> Duration {
        self.close_timeout
    }
}

#[derive(Debug)]
pub enum OwnedStdioLaunchError {
    Unavailable,
    Io(io::Error),
}

impl std::fmt::Display for OwnedStdioLaunchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("durable owned-process service unavailable"),
            Self::Io(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for OwnedStdioLaunchError {}

/// Executor-owned process proxy. The executor service retains process-tree
/// custody and recovery ownership; dropping this proxy must never orphan it.
#[async_trait::async_trait]
pub trait OwnedStdioProcess: Send + Sync + 'static {
    async fn send_frame(&self, frame: &[u8]) -> io::Result<()>;
    async fn receive_frame(&self) -> io::Result<Option<Vec<u8>>>;
    async fn close_and_reap(&self) -> io::Result<()>;
}

/// Durable executor boundary for launching an already-issued process token.
/// Implementations must register recovery ownership before releasing the child.
#[async_trait::async_trait]
pub trait OwnedStdioProcessService: Send + Sync + 'static {
    async fn launch(
        &self,
        token: PreparedCommandToken,
        limits: OwnedStdioLimits,
    ) -> Result<Arc<dyn OwnedStdioProcess>, OwnedStdioLaunchError>;
}

/// Single-use launcher that can only submit an executor-issued process token
/// to the durable owned-process service.
pub struct SandboxedStdioLauncher {
    token: Option<PreparedCommandToken>,
    service: Arc<dyn OwnedStdioProcessService>,
}

impl SandboxedStdioLauncher {
    pub fn new(token: PreparedCommandToken, service: Arc<dyn OwnedStdioProcessService>) -> Self {
        Self {
            token: Some(token),
            service,
        }
    }

    fn binding(
        &self,
        request: &BrokerInvocation<'_>,
        server_id: &McpServerId,
    ) -> Result<TransportBinding, TransportError> {
        let token = self
            .token
            .as_ref()
            .ok_or(TransportError::AuthorizationMismatch)?;
        Ok(TransportBinding::new(
            request,
            server_id.to_string(),
            "stdio",
            token.stdio_identity(),
            None,
        ))
    }

    async fn launch(
        &mut self,
        limits: TransportLimits,
    ) -> Result<Arc<dyn OwnedStdioProcess>, TransportError> {
        let token = self
            .token
            .take()
            .ok_or(TransportError::AuthorizationMismatch)?;
        let limits = OwnedStdioLimits {
            max_frame_bytes: limits.max_json_bytes(),
            io_timeout: limits.request_timeout(),
            close_timeout: limits.close_timeout(),
        };
        match self.service.launch(token, limits).await {
            Ok(process) => Ok(process),
            Err(OwnedStdioLaunchError::Unavailable) => Err(TransportError::OwnedProcessUnavailable),
            Err(OwnedStdioLaunchError::Io(error)) => Err(TransportError::Io(error)),
        }
    }
}

pub async fn connect_stdio(
    server_id: McpServerId,
    request: &BrokerInvocation<'_>,
    launcher: &mut SandboxedStdioLauncher,
    store: &mut SqliteStore,
    limits: TransportLimits,
) -> Result<ReadyConnection, TransportError> {
    super::validate_initialize_arguments(request)?;
    let operation = TransportOperation::parse("initialize")?;
    let binding = launcher.binding(request, &server_id)?;
    let authorization = transport_auth::authorize(request, &operation, &binding, store)?;
    let operations = OperationGate::new();
    operations.set_binding(binding.clone())?;
    operations.install(authorization)?;
    let dispatch = match transport_auth::begin_dispatch(request, &operation, &binding, false, store)
    {
        Ok(dispatch) => dispatch,
        Err(error) => {
            operations.clear();
            return Err(error.into());
        }
    };
    let result =
        connect_stdio_authorized(server_id, launcher, limits, Arc::clone(&operations)).await;
    transport_auth::finish_dispatch(
        request,
        dispatch,
        if result.is_ok() {
            transport_auth::TransportDispatchOutcome::Completed
        } else {
            transport_auth::TransportDispatchOutcome::OutcomeUnknown
        },
        store,
    )?;
    result
}

async fn connect_stdio_authorized(
    server_id: McpServerId,
    launcher: &mut SandboxedStdioLauncher,
    limits: TransportLimits,
    operations: Arc<OperationGate>,
) -> Result<ReadyConnection, TransportError> {
    let process = tokio::time::timeout(limits.connect_timeout(), launcher.launch(limits))
        .await
        .map_err(|_| TransportError::Timeout("owned stdio launch"))??;
    connect_owned_transport(server_id, process, limits, operations).await
}

async fn connect_owned_transport(
    server_id: McpServerId,
    process: Arc<dyn OwnedStdioProcess>,
    limits: TransportLimits,
    operations: Arc<OperationGate>,
) -> Result<ReadyConnection, TransportError> {
    let transport =
        BoundedStdioTransport::new(process, limits, Arc::clone(&operations), server_id.clone());
    let result = tokio::time::timeout(
        limits.connect_timeout(),
        McpConnection::connect_kit_authorized_transport(
            server_id,
            transport,
            McpHandlerConfig::new().with_events_capacity(limits.channel_capacity()),
        ),
    )
    .await
    .map_err(|_| TransportError::Timeout("stdio initialize"))?;
    let connection = match result {
        Ok(connection) => connection,
        Err(error) => return Err(operations.take_failure().unwrap_or_else(|| error.into())),
    };
    let authorization = operations.current_authorization()?;
    operations.bind_connection(authorization)?;
    operations.clear();
    ReadyConnection::new(connection, limits, operations, None, false)
}

struct BoundedStdioTransport {
    process: Arc<dyn OwnedStdioProcess>,
    max_frame_bytes: usize,
    io_timeout: Duration,
    close_timeout: Duration,
    operations: Arc<OperationGate>,
    server_id: McpServerId,
}

impl BoundedStdioTransport {
    fn new(
        process: Arc<dyn OwnedStdioProcess>,
        limits: TransportLimits,
        operations: Arc<OperationGate>,
        server_id: McpServerId,
    ) -> Self {
        Self {
            process,
            max_frame_bytes: limits.max_json_bytes(),
            io_timeout: limits.request_timeout(),
            close_timeout: limits.close_timeout(),
            operations,
            server_id,
        }
    }
}

impl Transport<RoleClient> for BoundedStdioTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let authorized = self.operations.authorize_message(&item);
        let max = self.max_frame_bytes;
        let timeout = self.io_timeout;
        let process = Arc::clone(&self.process);
        async move {
            authorized
                .map(|_| ())
                .map_err(|error| io::Error::new(io::ErrorKind::PermissionDenied, error))?;
            let frame = serde_json::to_vec(&item).map_err(invalid_data)?;
            if frame.len() > max {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "outbound MCP stdio frame exceeds bound",
                ));
            }
            tokio::time::timeout(timeout, process.send_frame(&frame))
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "MCP stdio write timed out"))?
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let frame = match tokio::time::timeout(self.io_timeout, self.process.receive_frame()).await
        {
            Ok(Ok(Some(frame))) if frame.len() <= self.max_frame_bytes => frame,
            Ok(Ok(Some(_))) => {
                self.operations.fail(TransportFailure::ResponseTooLarge);
                return None;
            }
            Ok(Ok(None)) => return None,
            Ok(Err(error)) => {
                self.operations
                    .fail(TransportFailure::StdioParse(error.to_string()));
                return None;
            }
            Err(_) => {
                self.operations.fail(TransportFailure::StdioTimeout);
                return None;
            }
        };
        let value: serde_json::Value = match serde_json::from_slice(&frame) {
            Ok(value) => value,
            Err(error) => {
                self.operations
                    .fail(TransportFailure::StdioParse(error.to_string()));
                return None;
            }
        };
        if value
            .get("result")
            .and_then(serde_json::Value::as_object)
            .is_some_and(|result| {
                result.contains_key("capabilities")
                    && result.contains_key("serverInfo")
                    && !result.contains_key("protocolVersion")
            })
        {
            self.operations
                .fail(TransportFailure::MissingProtocolVersion(
                    self.server_id.clone(),
                ));
            return None;
        }
        match serde_json::from_value(value) {
            Ok(message) => Some(message),
            Err(error) => {
                self.operations
                    .fail(TransportFailure::StdioParse(error.to_string()));
                None
            }
        }
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        tokio::time::timeout(self.close_timeout, self.process.close_and_reap())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "MCP stdio close timed out"))?
    }
}

fn invalid_data(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::broker::transport_auth::{TransportAuthorization, TransportOperation};

    struct MissingVersionProcess {
        response: tokio::sync::Mutex<Option<Vec<u8>>>,
    }

    #[async_trait::async_trait]
    impl OwnedStdioProcess for MissingVersionProcess {
        async fn send_frame(&self, frame: &[u8]) -> io::Result<()> {
            let request: serde_json::Value = serde_json::from_slice(frame).map_err(invalid_data)?;
            if request.get("method").and_then(serde_json::Value::as_str) == Some("initialize") {
                *self.response.lock().await = Some(
                    serde_json::to_vec(&serde_json::json!({
                        "jsonrpc":"2.0",
                        "id":request["id"],
                        "result":{
                            "capabilities":{},
                            "serverInfo":{"name":"missing-version","version":"1"}
                        }
                    }))
                    .unwrap(),
                );
            }
            Ok(())
        }

        async fn receive_frame(&self) -> io::Result<Option<Vec<u8>>> {
            Ok(self.response.lock().await.take())
        }

        async fn close_and_reap(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn production_adapter_preserves_missing_version_as_typed_refusal() {
        let server_id = McpServerId::new("missing-version");
        let operations = OperationGate::new();
        let binding =
            TransportBinding::for_test(server_id.to_string(), "stdio", "owned-process-token", None);
        operations.set_binding(binding.clone()).unwrap();
        operations
            .install(TransportAuthorization::for_test_bound_arguments_binding(
                TransportOperation::parse("initialize").unwrap(),
                agentkit_mcp::kit_authorized_initialize_arguments(),
                None,
                None,
                binding,
            ))
            .unwrap();
        let error = match connect_owned_transport(
            server_id,
            Arc::new(MissingVersionProcess {
                response: tokio::sync::Mutex::new(None),
            }),
            TransportLimits::default(),
            operations,
        )
        .await
        {
            Ok(_) => panic!("missing protocol version connected"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            TransportError::Agentkit(inner)
                if matches!(*inner, agentkit_mcp::McpError::UnsupportedProtocolVersion { negotiated: None, .. })
        ));
    }
}
