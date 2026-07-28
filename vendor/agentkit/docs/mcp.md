# agentkit-mcp design

## Purpose

`agentkit-mcp` is the MCP integration crate.

It makes MCP servers usable from `agentkit` without forcing the rest of the stack to understand MCP transport details. The crate is built on top of the official [`rmcp`](https://crates.io/crates/rmcp) Rust SDK — wire-protocol types are re-exported as-is and there is no parallel agentkit-side type vocabulary.

It owns:

- MCP server configuration and lifecycle
- transport/session management (delegated to rmcp)
- auth handshakes and auth-required interruptions
- discovery of MCP tools, resources, and prompts
- adaptation of MCP tools into the shared tool system
- access APIs for MCP resources and prompts via the `agentkit-capabilities` `ResourceProvider`/`PromptProvider` traits
- pluggable client-side responders for `sampling/createMessage`, `elicitation/create`, and `roots/list`
- a broadcast subscription for server-pushed events: progress, logging, resource updates, list-changed, cancellation

It does not pretend that all of MCP is a tool.

## Non-goals

`agentkit-mcp` does not own:

- the main loop driver
- the generic tool registry contract
- shell/process execution outside MCP server management
- host UI for auth or approval
- long-term caching or persistence
- provider-specific model logic

This crate integrates MCP into `agentkit`; it does not replace the rest of the architecture.

## Design principles

### 1. MCP tools plug into the normal tool system

If an MCP server exposes tools, those appear as ordinary `ToolSpec`s and execute through the same `ToolExecutor` path as native tools.

The loop sees one unified tool flow:

- tool discovered
- tool spec exposed to model
- tool called
- permission checked
- auth/approval interruption surfaced if needed
- tool result returned

### 2. MCP resources and prompts stay first-class MCP concepts

Resources and prompts are not tools. They expose through the lower-level `ResourceProvider` / `PromptProvider` capability traits rather than awkward fake tools such as `mcp.read_resource` or `mcp.get_prompt`. That preserves MCP's structure and avoids flattening unlike concepts into one trait.

### 3. Auth is an interruption, not hidden retry logic

MCP often involves auth or capability negotiation. `agentkit-mcp` surfaces auth requirements explicitly so the host can resolve them.

This aligns with the loop/tool interruption model:

- tool invocation may interrupt with `ToolError::AuthRequired(AuthRequest)`
- server/session startup may interrupt with `McpError::AuthRequired(AuthRequest)`
- the host resolves the auth flow via `McpServerManager::resolve_auth(...)`
- the same operation can then resume

### 4. Transport details stay inside this crate (and rmcp)

The rest of `agentkit` does not care whether an MCP server is reached via stdio, HTTP, or in-memory pipes. Built-in transports:

- **stdio** (rmcp `TokioChildProcess`)
- **Streamable HTTP** (rmcp `StreamableHttpClientTransport`)

For transports rmcp supports but the built-in `McpTransportBinding` enum does not (in-memory pipes for tests, websockets, custom IO), hosts construct the rmcp `RunningService` themselves and adopt it through `McpConnection::from_running_service_with_events`. Pair the service with the channels returned by `McpHandlerConfig::new().build()` so list-change notifications and `McpServerEvent` subscribers stay observable.

### 5. Discovery is explicit and cacheable

MCP server capabilities may change over time, but hosts should not be forced to re-discover on every loop step. The crate supports:

- explicit discovery via `McpConnection::discover()` / `McpServerManager::refresh_server(...)`
- stable `McpDiscoverySnapshot`s of tools/resources/prompts (rmcp wire types)
- coalesced re-discovery of changed catalogs via `McpServerManager::refresh_changed_catalogs()`, driven by server-pushed `notifications/*/list_changed`

### 6. MCP implements the lower-level capability layer

`agentkit-mcp` builds on `agentkit-capabilities`, not a parallel universe:

- MCP tools wrap into `ToolInvocableAdapter` invocables
- MCP resources implement `ResourceProvider`
- MCP prompts implement `PromptProvider`

### 7. Bidirectional MCP is fully supported

MCP is bidirectional — servers can issue `sampling/createMessage`, `elicitation/create`, and `roots/list` requests back into the client. The host installs `McpSamplingResponder` / `McpElicitationResponder` / `McpRootsProvider` implementations and the client only advertises the matching `ClientCapabilities` entry when a responder is wired in.

Server-pushed notifications (progress, logging, resource updates, list-changed, cancellation) are delivered as `McpServerEvent` over a `tokio::sync::broadcast` channel obtained via `McpConnection::subscribe_events`. List-changed events are _also_ delivered through the legacy `McpServerNotification` mpsc receiver consumed by `refresh_changed_catalogs` — the two channels coexist: events for live UI/observability, mpsc for catalog re-sync.

## Main boundary

The clean separation:

- `agentkit-capabilities` owns the lower-level invocable/resource/prompt contracts
- `agentkit-tools-core` owns generic tool execution contracts
- `agentkit-mcp` adapts MCP tools into those contracts
- `agentkit-mcp` separately exposes MCP resources/prompts/server lifecycle/auth/responders/events
- `rmcp` owns the wire protocol, transports, and the JSON-RPC framing

So:

- MCP tools participate in the shared tool registry
- MCP resources/prompts do not get forced into the tool system
- Wire-protocol types are not re-wrapped — `CallToolResult`, `ReadResourceResult`, `Content`, `ToolAnnotations`, etc. flow through unchanged

## Core concepts

### 1. Server configuration

```rust,ignore
pub struct McpServerConfig {
    pub id: McpServerId,
    pub transport: McpTransportBinding,
    pub metadata: MetadataMap,
}

pub enum McpTransportBinding {
    Stdio(StdioTransportConfig),
    StreamableHttp(StreamableHttpTransportConfig),
}
```

Both bindings are thin agentkit-owned config structs that drive the corresponding rmcp transport at connect time. Auth on Streamable HTTP is set declaratively via `with_bearer_token` / `with_header`; rotate with `McpServerManager::resolve_auth` (which currently triggers a reconnect with the new credentials).

### 2. Server manager

`McpServerManager` is the subsystem that owns MCP server lifecycle. One per host.

```rust,ignore
let manager = McpServerManager::new()
    .with_server(config_a)
    .with_server(config_b)
    .with_namespace(McpToolNamespace::Default)
    .with_handler_config(
        McpHandlerConfig::new()
            .with_sampling_responder(Arc::new(host_sampling))
            .with_elicitation_responder(Arc::new(prompt_user))
            .with_roots_provider(Arc::new(workspace_roots)),
    );

manager.connect_all().await?;
let registry = manager.tool_registry();
let provider = manager.capability_provider();
```

For best-effort startup, `connect_all_settled().await` attempts every registered server concurrently, installs the successful connections, and returns per-server failures without short-circuiting on the first error.

Connecting can be bounded per server with `with_server_options(config, McpServerOptions::new().with_timeout(duration))`. On connect paths (`connect_server`, `connect_all`, `connect_all_settled`) the timeout covers connection establishment — transport setup and the MCP initialize handshake — together with initial discovery; on `refresh_server` and changed-catalog refreshes it bounds discovery alone. Timeouts surface as `McpError::Timeout`.

`McpConnection` is the live handle to one configured MCP server. It owns:

- the rmcp `RunningService` (negotiated transport + session)
- negotiated server capabilities
- auth state
- the events broadcast sender + catalog notification receiver

### 3. Discovery snapshot

```rust,ignore
pub struct McpDiscoverySnapshot {
    pub server_id: McpServerId,
    pub tools: Vec<McpTool>,        // = rmcp::model::Tool
    pub resources: Vec<McpResource>, // = rmcp::model::Resource
    pub prompts: Vec<McpPrompt>,     // = rmcp::model::Prompt
    pub metadata: MetadataMap,
}
```

Pattern-matching directly against the rmcp types gives access to `output_schema`, `annotations`, `mime_type`, prompt arguments, etc. without a wrapping layer.

### 4. Tool adaptation and namespacing

`McpToolAdapter` wraps an MCP tool as a `Tool` implementation. Tool names are namespaced as `mcp_<server_id>_<tool_name>` by default. Hosts override the strategy with `McpToolNamespace::None` (strip the prefix) or `McpToolNamespace::Custom(...)` (e.g. `remote.<server>.<tool>`).

### 5. Capability provider

`McpCapabilityProvider` builds invocables for tools (via `ToolInvocableAdapter`), `McpResourceHandle`s for resources, and `McpPromptHandle`s for prompts — surfacing all three through the `CapabilityProvider` trait that the agent loop and context system already consume.

### 6. Auth replay

`McpServerManager::resolve_auth_and_resume(resolution)` resolves credentials and replays the operation that triggered the original challenge. The connection-level `McpConnection::replay_auth_operation(operation)` is also exposed for hosts that drive auth without going through the manager.

### 7. Server events

```rust,ignore
pub enum McpServerEvent {
    Progress(McpProgressNotificationParam),
    Logging(McpLoggingMessageNotificationParam),
    ResourceUpdated(McpResourceUpdatedNotificationParam),
    ToolListChanged,
    ResourceListChanged,
    PromptListChanged,
    Cancelled(McpCancelledNotificationParam),
}
```

Subscribers fan out from `McpConnection::subscribe_events()`. `McpConnection::set_logging_level(...)` negotiates the minimum severity the server emits; `subscribe_resource(uri)` / `unsubscribe_resource(uri)` toggle per-URI watch on resources that support it.

### 8. Errors

`McpError` covers transport failure, protocol errors, auth challenges (`AuthRequired(Box<AuthRequest>)`), invocation errors, and unknown-server lookup misses. It implements `From<McpError> for rmcp::model::ErrorData` so responders can return agentkit errors and have them surface to the server as JSON-RPC errors.

## What we validated

The Phase 1-4 rebuild proved out:

1. one MCP server can be connected and discovered from config alone
2. discovered MCP tools register into `ToolRegistry` with collision-safe names
3. MCP tool invocation interrupts for auth and resumes cleanly
4. MCP resources are read without going through the tool path
5. MCP prompts are fetched and turned into capability `PromptContents`
6. server-specific metadata survives adaptation without polluting the generic tool model
7. server-initiated `sampling/createMessage`, `elicitation/create`, and `roots/list` are dispatched to host-supplied responders
8. server-pushed progress/logging/resource-updated/list-changed/cancellation events reach broadcast subscribers
