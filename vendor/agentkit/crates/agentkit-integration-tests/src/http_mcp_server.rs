//! Real-HTTP rmcp server with a mutable tool list, bound to a random port.
//!
//! Spawns a [`StreamableHttpService`] on `127.0.0.1:0` and runs an MCP
//! [`ServerHandler`] backed by a shared `Vec<rmcp::model::Tool>`. Tests can
//! mutate that list at runtime ([`HttpServerHandle::add_tool`],
//! [`HttpServerHandle::remove_tool`]) and emit a `tools/list_changed`
//! notification ([`HttpServerHandle::notify_tool_list_changed`]) so the full
//! agentkit ↔ rmcp ↔ HTTP path can be exercised end-to-end.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::{
    RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResult, Content, ErrorData as RmcpError,
        InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool, ToolsCapability,
    },
    service::{NotificationContext, Peer, RequestContext},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use tokio::{sync::oneshot, task::JoinHandle};

/// MCP server handler with a mutable tool list. Each tool returns a fixed
/// "ok:<name>" response so call-side assertions can verify routing.
#[derive(Clone)]
pub struct MutableMcpServer {
    tools: Arc<Mutex<Vec<Tool>>>,
    initialize_delay: Arc<Mutex<Option<Duration>>>,
    list_tools_delay: Arc<Mutex<Option<Duration>>>,
    peer_slot: Arc<Mutex<Option<Peer<RoleServer>>>>,
    /// Records every `call_tool` invocation as `(name, json_arguments)`.
    pub call_log: Arc<Mutex<Vec<(String, serde_json::Value)>>>,
}

impl MutableMcpServer {
    fn new(initial_tools: Vec<Tool>) -> Self {
        Self {
            tools: Arc::new(Mutex::new(initial_tools)),
            initialize_delay: Arc::new(Mutex::new(None)),
            list_tools_delay: Arc::new(Mutex::new(None)),
            peer_slot: Arc::new(Mutex::new(None)),
            call_log: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl ServerHandler for MutableMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools_with(ToolsCapability {
                    list_changed: Some(true),
                })
                .build(),
        )
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, RmcpError> {
        let delay = *self.initialize_delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        Ok(self.get_info())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, RmcpError> {
        let delay = *self.list_tools_delay.lock().unwrap();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let tools = self.tools.lock().unwrap().clone();
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, RmcpError> {
        let name = request.name.to_string();
        let arguments = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or(serde_json::Value::Null);
        self.call_log
            .lock()
            .unwrap()
            .push((name.clone(), arguments));

        let known = self
            .tools
            .lock()
            .unwrap()
            .iter()
            .any(|tool| tool.name.as_ref() == name);
        if !known {
            return Err(RmcpError::invalid_params(
                format!("unknown tool: {name}"),
                None,
            ));
        }

        Ok(CallToolResult::success(vec![Content::text(format!(
            "ok:{name}"
        ))]))
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        *self.peer_slot.lock().unwrap() = Some(context.peer.clone());
    }
}

/// Handle to a live HTTP MCP server. Drops cleanly when the test ends:
/// the spawned task is aborted, the listener is dropped, and the address
/// is freed.
pub struct HttpServerHandle {
    /// `http://127.0.0.1:<port>/mcp` — feed straight into
    /// [`agentkit_mcp::McpServerConfig::streamable_http`].
    pub url: String,
    /// The handler whose tool list backs the served catalog. Mutate via
    /// [`HttpServerHandle::add_tool`] / [`HttpServerHandle::remove_tool`].
    pub server: MutableMcpServer,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl HttpServerHandle {
    /// Replaces the served tool list. Does not emit `tools/list_changed`;
    /// pair with [`Self::notify_tool_list_changed`] when the test wants the
    /// client to learn about the change.
    pub fn set_tools(&self, tools: Vec<Tool>) {
        *self.server.tools.lock().unwrap() = tools;
    }

    /// Delays every `initialize` response by `delay`. Useful for exercising
    /// client-side connect timeout behavior against a server that accepts
    /// the transport but stalls the MCP handshake.
    pub fn set_initialize_delay(&self, delay: Option<Duration>) {
        *self.server.initialize_delay.lock().unwrap() = delay;
    }

    /// Delays every `tools/list` response by `delay`. Useful for exercising
    /// client-side discovery timeout behavior.
    pub fn set_list_tools_delay(&self, delay: Option<Duration>) {
        *self.server.list_tools_delay.lock().unwrap() = delay;
    }

    /// Appends a tool to the served list.
    pub fn add_tool(&self, tool: Tool) {
        self.server.tools.lock().unwrap().push(tool);
    }

    /// Removes a tool by name. Returns `true` if a matching tool was found.
    pub fn remove_tool(&self, name: &str) -> bool {
        let mut tools = self.server.tools.lock().unwrap();
        let before = tools.len();
        tools.retain(|tool| tool.name.as_ref() != name);
        tools.len() != before
    }

    /// Kills the server immediately, freeing the port. Subsequent client
    /// requests — including the session `DELETE` a graceful close issues —
    /// fail at the transport level. Useful for exercising close/disconnect
    /// error paths against a server that has gone away.
    pub fn shutdown(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.join.take() {
            handle.abort();
        }
    }

    /// Emits `notifications/tools/list_changed` on the live MCP session.
    /// Returns an error if the client hasn't completed the handshake yet
    /// (no peer has been stashed) or the underlying notify fails.
    pub async fn notify_tool_list_changed(&self) -> Result<(), String> {
        let peer = self
            .server
            .peer_slot
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "no MCP peer registered yet (client not initialized)".to_string())?;
        peer.notify_tool_list_changed()
            .await
            .map_err(|e| e.to_string())
    }
}

impl Drop for HttpServerHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.join.take() {
            handle.abort();
        }
    }
}

/// Spawns an HTTP MCP server on a random local port. Resolves once the
/// listener is bound; the URL in the returned handle is immediately usable.
pub async fn spawn_http_mcp(initial_tools: Vec<Tool>) -> HttpServerHandle {
    let server = MutableMcpServer::new(initial_tools);
    let server_for_factory = server.clone();
    let service = StreamableHttpService::new(
        move || Ok(server_for_factory.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind random local port");
    let addr = listener.local_addr().expect("listener has local_addr");
    let url = format!("http://127.0.0.1:{}/mcp", addr.port());

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    HttpServerHandle {
        url,
        server,
        shutdown_tx: Some(shutdown_tx),
        join: Some(join),
    }
}

/// Convenience for building an [`rmcp::model::Tool`] with a trivial
/// `{"type":"object"}` input schema.
pub fn simple_tool(name: &'static str, description: &'static str) -> Tool {
    let schema: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(r#"{"type":"object","properties":{},"additionalProperties":true}"#)
            .expect("valid schema literal");
    Tool::new(name, description, Arc::new(schema))
}
