//! In-memory rmcp server for end-to-end tests.
//!
//! Spawns a tokio duplex pair and runs a real rmcp [`ServerHandler`] on one
//! end. The other end is wrapped in an [`McpConnection`] so the test stack
//! exercises the full agentkit ↔ rmcp ↔ MCP-server data path without
//! spawning a child process or opening a socket.
//!
//! The server's tool list is configurable per call so tests can verify
//! catalog changes (connect, refresh, disconnect).

use std::sync::{Arc, Mutex};

use agentkit_mcp::{McpConnection, McpHandlerConfig, McpServerId};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, ErrorData as RmcpError, ServerCapabilities, ServerInfo,
};
use rmcp::service::{Peer, RoleServer};
use rmcp::{ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use tokio::sync::oneshot;

/// Echo + multiply tool surface. Two tools is the minimum to assert that
/// MCP-side tools land in the agent's advertised catalog with both their
/// names visible.
#[derive(Clone)]
pub struct DemoServer {
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
    /// Records every `multiply` invocation for tests to inspect.
    pub call_log: Arc<Mutex<Vec<(i64, i64)>>>,
}

impl Default for DemoServer {
    fn default() -> Self {
        Self {
            tool_router: Self::tool_router(),
            call_log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EchoArgs {
    text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MultiplyArgs {
    a: i64,
    b: i64,
}

#[tool_router]
impl DemoServer {
    #[tool(description = "Echoes the supplied text back as the tool result.")]
    async fn echo(
        &self,
        Parameters(EchoArgs { text }): Parameters<EchoArgs>,
    ) -> Result<CallToolResult, RmcpError> {
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "Multiplies two integers and returns the product as text.")]
    async fn multiply(
        &self,
        Parameters(MultiplyArgs { a, b }): Parameters<MultiplyArgs>,
    ) -> Result<CallToolResult, RmcpError> {
        self.call_log.lock().unwrap().push((a, b));
        Ok(CallToolResult::success(vec![Content::text(
            (a * b).to_string(),
        )]))
    }
}

#[tool_handler]
impl ServerHandler for DemoServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }
}

/// Handle to a live in-memory MCP connection.
pub struct InMemoryServer {
    pub connection: McpConnection,
    pub server: DemoServer,
    /// Server-side rmcp peer; held so the test can drop it explicitly to
    /// simulate disconnection.
    pub peer: Peer<RoleServer>,
}

/// Spin up a [`DemoServer`] over a tokio duplex pipe and return a connected
/// [`McpConnection`]. The returned [`Peer`] is held so the server task stays
/// alive for the lifetime of the test.
pub async fn spawn_in_memory(server_id: impl Into<String>) -> InMemoryServer {
    spawn_in_memory_with_handler(server_id, McpHandlerConfig::new()).await
}

/// Variant that accepts a custom [`McpHandlerConfig`] (auth, sampling,
/// elicitation responders, etc.).
pub async fn spawn_in_memory_with_handler(
    server_id: impl Into<String>,
    handler_config: McpHandlerConfig,
) -> InMemoryServer {
    let (handler, channels) = handler_config.build();
    let (server_io, client_io) = tokio::io::duplex(8 * 1024);
    let server = DemoServer::default();
    let server_for_task = server.clone();
    let (peer_tx, peer_rx) = oneshot::channel();
    tokio::spawn(async move {
        match server_for_task.serve(server_io).await {
            Ok(running) => {
                let _ = peer_tx.send(running.peer().clone());
                let _ = running.waiting().await;
            }
            Err(error) => {
                eprintln!("in-memory MCP server init failed: {error:?}");
            }
        }
    });

    let service = handler
        .serve(client_io)
        .await
        .expect("MCP client init succeeds");
    let peer = peer_rx.await.expect("server peer arrives");

    let connection = McpConnection::from_running_service_with_events(
        McpServerId::new(server_id),
        service,
        channels.notifications,
        channels.events,
    );

    InMemoryServer {
        connection,
        server,
        peer,
    }
}
