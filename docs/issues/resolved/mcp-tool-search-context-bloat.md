# MCP tool search can flood the model context (resolved)

`tool_search` treated a natural-language query as an OR-style ranked keyword bag. A tool or server was included when any query term matched. It searched every already-connected server, returned up to 20 full tool schemas per matching server, and backfilled a matching server to 20 tools even when only a weak query term matched its name or description. There was no global result-count or serialized-size budget.

This could produce unexpectedly large results after `tool_search({ query: "mcp" })` had initialized every server. In one observed session, a later search for Gmail capabilities matched seven connected servers through generic terms such as `search` and `email`. Returning `result.servers` from `compose` added 103 full tool definitions and 266,552 serialized bytes to the transcript, increasing provider-reported context use from 6,571 to 71,078 tokens.

## Resolution (implemented in 0.1.76)

Discovery was redesigned in `src/tools/mcp.rs`:

- All configured servers begin eager background initialization at runtime startup, restoring stored OAuth credentials in parallel and settling all connections concurrently through `McpServerManager::connect_servers_settled`. Interactive OAuth is never started implicitly; `AuthRequired` challenges and per-server statuses are preserved.
- Every `tool_search` reloads and diffs the config, then initializes and awaits any still-uninitialized servers through the same primitive (`initialize_servers`) before searching. Query-driven initialization and per-server tool backfill were removed; `auth` initializes its target through the same primitive.
- The exact query `mcp` (normalized, case-insensitive) returns a compact server list — name, bounded description, status, bounded optional error, and `available_tool_count` (null unless connected) — with no schemas. It drops tail entries to stay within the response cap and reports `total_servers`, `returned_servers`, and `truncated`.
- Normal searches rank all connected tools globally with high precision: the existing scoring plus a coverage/name gate (every term must match, or the name must match), an absolute score floor of 60, and a relative floor of a third of the top score. At most 5 full results are returned globally (score descending, name ascending), grouped by server with `available_tool_count`, `matched_tool_count`, `returned_tool_count`, and `truncated`; strongly matching servers that need authentication or failed to connect appear without tools. The response carries top-level `total_matched`, `total_returned`, and `truncated`, omits `output_schema` from results, and enforces a 32,768-byte serialized cap by dropping the lowest-ranked status-only groups before the lowest-ranked tools.
