# MCP tool deadlines do not send protocol cancellation

Kit's MCP wrapper can stop its local wait at the configured tool deadline, but the AgentKit MCP adapter exposes only an awaited `call_tool` future, not RMCP's `RequestHandle`. Dropping that future does not invoke `RequestHandle::cancel`, so Kit cannot send `notifications/cancelled` for the timed-out request. A server that never answers can retain the corresponding local RMCP pending request until the connection closes, and a remote side effect may still complete. Kit therefore warns callers to inspect remote state before retrying side effects.

A complete fix belongs at the AgentKit MCP boundary: expose a per-call deadline or cancellation-aware call API that uses RMCP `PeerRequestOptions`/`RequestHandle`, sends protocol cancellation on expiry, unregisters pending request state, and returns a typed timeout error. Kit can then delegate its `timeout_seconds` policy to that API instead of enforcing only the outer local deadline.

Relevant implementation: Kit `src/tools/mcp.rs` (`McpTool::dispatch`), AgentKit `agentkit-mcp` (`McpConnection::call_tool`), and RMCP `RequestHandle`.
