# Compose MCP tool calls can remain open indefinitely

A nested research run left at least 25 MCP calls marked as running for more than 50 minutes. Six ACP grandchildren had each appended an assistant `compose` call but no tool result, while their processes remained alive and idle. Most outstanding programs were concurrent batches of Exa search or fetch calls; one also contained bounded `shell` work, but the outer `compose` could not finish while its MCP sibling remained pending. Boundary retries did not help because they retry failures but do not impose a deadline on one attempt.

Kit applied a 20-second MCP connection timeout, but `McpTool::dispatch` waited on `execution_scope.execute_child(...)` without an execution timeout. A connected remote server that never completed a tool request could therefore hold a compose iteration, its parent ACP turn, and the whole ancestor compose graph open until the user cancelled the turn.

## Resolution

MCP tool calls now have a 60-second deadline. The optional `timeout_seconds` input accepts values from 1 through 3600 for calls expected to return after that default; omission remains the default and the JSON Schema does not supply a value. The deadline covers waiting for a serialized server-operation gate and remote execution. A timeout releases the local wait and warns callers to inspect remote state before retrying side effects.

Relevant implementation: `src/tools/mcp.rs` (`McpTool::dispatch`).
