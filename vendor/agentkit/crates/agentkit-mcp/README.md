# agentkit-mcp

<p align="center">
  <a href="https://crates.io/crates/agentkit-mcp"><img src="https://img.shields.io/crates/v/agentkit-mcp.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-mcp"><img src="https://img.shields.io/docsrs/agentkit-mcp?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-mcp.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Model Context Protocol integration for agentkit, built on top of the official [`rmcp`](https://crates.io/crates/rmcp) Rust SDK.

This crate covers:

- **stdio** and **Streamable HTTP** transports (driven by rmcp)
- multi-server lifecycle, discovery, and catalog diffing via `McpServerManager`
- adapters that surface MCP servers as agentkit tools and capabilities
- pluggable client-side responders for `sampling/createMessage`, `elicitation/create`, and `roots/list`
- a broadcast subscription for server-pushed events: progress, logging, resource updates, list-changed, cancellation
- auth replay for MCP operations that fail with an authentication challenge

The wire-protocol types — `CallToolResult`, `ReadResourceResult`, `GetPromptResult`, `Content`, `RawContent`, `ToolAnnotations`, `Prompt`, etc. — are re-exported from `rmcp` directly. There is no parallel agentkit-side type vocabulary to maintain.

## Why this matters

Re-exporting [`rmcp::model`](https://docs.rs/rmcp/latest/rmcp/model/index.html) keeps agentkit-mcp in lockstep with the MCP spec — new fields, content variants, capability flags, server-initiated requests, and notification payloads land in agentkit the moment `rmcp` ships them. No second source of truth to drift.

- **Spec:** [modelcontextprotocol.io](https://modelcontextprotocol.io)
- **Rust SDK:** [`rmcp` on crates.io](https://crates.io/crates/rmcp)

The same applies to transports: any future rmcp transport is reachable through `McpConnection::from_running_service_with_events` without touching the built-in `McpTransportBinding` enum.

## Configuring and connecting MCP servers

Register one or more MCP server configurations with `McpServerManager`, then connect them. Each connected server is represented by an `McpServerHandle` that holds the live connection and the discovery snapshot.

```rust,no_run
use agentkit_mcp::{
    McpServerConfig, McpServerManager, McpTransportBinding, StdioTransportConfig,
};

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
let mut manager = McpServerManager::new()
    .with_server(McpServerConfig::new(
        "filesystem",
        McpTransportBinding::Stdio(
            StdioTransportConfig::new("npx")
                .with_arg("-y")
                .with_arg("@modelcontextprotocol/server-filesystem"),
        ),
    ))
    .with_server(McpServerConfig::new(
        "github",
        McpTransportBinding::Stdio(
            StdioTransportConfig::new("npx")
                .with_arg("-y")
                .with_arg("@modelcontextprotocol/server-github")
                .with_env("GITHUB_TOKEN", "ghp_..."),
        ),
    ));

let handles = manager.connect_all().await?;
println!("connected {} MCP server(s)", handles.len());
# Ok(())
# }
```

Use `connect_all_settled().await` when startup should be best effort: it attempts every registered server concurrently, installs successful connections into the manager, and returns each failed server with its own `McpError`.

For slow or unresponsive servers, register with `with_server_options(config, McpServerOptions::new().with_timeout(duration))`. The timeout bounds connection establishment — transport setup and the MCP initialize handshake — together with initial discovery, and bounds refresh discovery on its own; exceeding it returns `McpError::Timeout`.

## Discovering tools

After connecting, each server's capabilities are available through its discovery snapshot. The `tools`/`resources`/`prompts` fields hold the raw rmcp types — pattern-match on them directly for `output_schema`, `annotations`, `mime_type`, and friends.

```rust,no_run
use agentkit_mcp::{
    McpServerConfig, McpServerManager, McpServerId, McpTransportBinding,
    StdioTransportConfig,
};

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let mut manager = McpServerManager::new().with_server(McpServerConfig::new(
#     "filesystem",
#     McpTransportBinding::Stdio(StdioTransportConfig::new("npx")
#         .with_arg("-y").with_arg("@modelcontextprotocol/server-filesystem")),
# ));
# manager.connect_all().await?;
let handle = manager.connected_server(&McpServerId::new("filesystem")).unwrap();
for tool in &handle.snapshot().tools {
    println!("  {} - {}", tool.name, tool.description.as_deref().unwrap_or(""));
}

let registry = manager.tool_registry();
for spec in registry.specs() {
    println!("{}", spec.name); // e.g. "mcp_filesystem_read_file"
}
# Ok(())
# }
```

## Using MCP tools in an agent

The tool registry and capability provider produced by `McpServerManager` plug straight into the agentkit agent loop. Tools are namespaced as `mcp_<server_id>_<tool_name>` by default.

```rust,no_run
use agentkit_mcp::{
    McpServerConfig, McpServerManager, McpTransportBinding, StdioTransportConfig,
};

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let mut manager = McpServerManager::new().with_server(McpServerConfig::new(
#     "filesystem",
#     McpTransportBinding::Stdio(StdioTransportConfig::new("npx")
#         .with_arg("-y").with_arg("@modelcontextprotocol/server-filesystem")),
# ));
# manager.connect_all().await?;
let tool_registry = manager.tool_registry();
let capability_provider = manager.capability_provider();
# Ok(())
# }
```

Pick a different convention — strip the prefix, replace it with dots, anything — by installing an `McpToolNamespace::Custom` strategy:

```rust,no_run
use agentkit_mcp::{McpServerManager, McpToolNamespace};

let manager = McpServerManager::new().with_namespace(McpToolNamespace::custom(
    |server, name| format!("remote.{server}.{name}"),
));
```

## Streamable HTTP transport

For modern remote MCP servers exposed over HTTP, use the Streamable HTTP transport. The bearer token (or any custom header) is set declaratively on the binding — rmcp drives the JSON/SSE response handling, session header propagation, and resumption with `Last-Event-ID`.

```rust,no_run
use agentkit_mcp::{
    McpServerConfig, McpServerManager, McpTransportBinding, StreamableHttpTransportConfig,
};

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
let mut manager = McpServerManager::new().with_server(McpServerConfig::new(
    "remote",
    McpTransportBinding::StreamableHttp(
        StreamableHttpTransportConfig::new("https://mcp.example.com/mcp")
            .with_bearer_token("tok_abc123"),
    ),
));

let handles = manager.connect_all().await?;
# Ok(())
# }
```

To rotate the bearer at runtime, drive an `AuthResolution::Provided { credentials, .. }` through `McpServerManager::resolve_auth` (or `McpConnection::resolve_auth` directly). The next operation reconnects with the new credentials.

## Sampling, elicitation, and roots

Servers can issue requests _back_ into the client: `sampling/createMessage` (ask the host LLM to generate), `elicitation/create` (ask the user for input), `roots/list` (enumerate workspace roots in scope). Wire in trait implementations to handle each:

```rust,no_run
use std::sync::Arc;
use async_trait::async_trait;
use agentkit_mcp::{
    McpCreateMessageRequestParams, McpCreateMessageResult, McpError, McpHandlerConfig, McpRoot,
    McpRootsProvider, McpSamplingMessage, McpSamplingResponder, McpServerManager,
};

struct HostSampling;
#[async_trait]
impl McpSamplingResponder for HostSampling {
    async fn create_message(
        &self,
        _params: McpCreateMessageRequestParams,
    ) -> Result<McpCreateMessageResult, McpError> {
        Ok(McpCreateMessageResult::new(
            McpSamplingMessage::assistant_text("(host LLM response)"),
            "host-model".into(),
        ))
    }
}

struct StaticRoots;
#[async_trait]
impl McpRootsProvider for StaticRoots {
    async fn list_roots(&self) -> Result<Vec<McpRoot>, McpError> {
        Ok(vec![McpRoot::new("file:///workspace").with_name("workspace")])
    }
}

let manager = McpServerManager::new().with_handler_config(
    McpHandlerConfig::new()
        .with_sampling_responder(Arc::new(HostSampling))
        .with_roots_provider(Arc::new(StaticRoots)),
);
```

`McpElicitationResponder` follows the same shape. The handler advertises the corresponding `ClientCapabilities` entry only when a responder is installed — servers that probe `client.capabilities.sampling` will see the host opt in.

## Subscribing to server events

`McpConnection::subscribe_events` returns a `tokio::sync::broadcast::Receiver<McpServerEvent>` that surfaces every push notification the server sends:

- `Progress` — keyed by the `progress_token` the client issued in a request
- `Logging` — `notifications/message`; throttled by `set_logging_level`
- `ResourceUpdated` — for URIs the client subscribed to via `subscribe_resource`
- `ToolListChanged` / `ResourceListChanged` / `PromptListChanged`
- `Cancelled` — server-initiated cancellation of an in-flight request

```rust,no_run
use agentkit_mcp::{McpConnection, McpServerEvent};

# async fn watch(connection: &McpConnection) -> Result<(), Box<dyn std::error::Error>> {
let mut events = connection.subscribe_events();
connection.subscribe_resource("memo:welcome").await?;
while let Ok(event) = events.recv().await {
    match event {
        McpServerEvent::Progress(progress) => println!("progress: {}", progress.progress),
        McpServerEvent::Logging(message) => println!("log: {message:?}"),
        McpServerEvent::ResourceUpdated(updated) => println!("updated: {}", updated.uri),
        other => println!("event: {other:?}"),
    }
}
# Ok(())
# }
```

Catalog list-changed events are _also_ delivered through the legacy `McpServerNotification` mpsc receiver consumed by `McpServerManager::refresh_changed_catalogs`. The two channels coexist: events for live UI/observability, mpsc for re-discovery.

## Lifecycle management

```rust,no_run
use agentkit_mcp::{
    McpServerConfig, McpServerManager, McpServerId, McpTransportBinding,
    StdioTransportConfig,
};

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# let mut manager = McpServerManager::new().with_server(McpServerConfig::new(
#     "filesystem",
#     McpTransportBinding::Stdio(StdioTransportConfig::new("npx")
#         .with_arg("-y").with_arg("@modelcontextprotocol/server-filesystem")),
# ));
let server_id = McpServerId::new("filesystem");

let handle = manager.connect_server(&server_id).await?;
let snapshot = manager.refresh_server(&server_id).await?;
println!("now has {} tools", snapshot.tools.len());
manager.disconnect_server(&server_id).await?;
# Ok(())
# }
```

`manager.refresh_changed_catalogs()` drains pending list-changed notifications across every connection and re-runs discovery for each affected server, returning the diffs as `McpCatalogEvent`s.

## Custom transports

When you need a transport rmcp supports but `McpTransportBinding` does not (in-memory pipes, websockets, custom IO), build the rmcp `RunningService` directly and adopt it:

```rust,no_run
use agentkit_mcp::{McpConnection, McpHandlerConfig, McpServerId};
use rmcp::ServiceExt;

# async fn adopt(client_io: tokio::io::DuplexStream) -> Result<(), Box<dyn std::error::Error>> {
let (handler, channels) = McpHandlerConfig::new().build();
let service = handler.serve(client_io).await?;
let connection = McpConnection::from_running_service_with_events(
    McpServerId::new("in-memory"),
    service,
    channels.notifications,
    channels.events,
);
# Ok(())
# }
```
