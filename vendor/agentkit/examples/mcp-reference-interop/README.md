# `mcp-reference-interop`

Live Streamable HTTP interop checks for `agentkit-mcp` against the official Rust
MCP SDK, `rmcp`.

This example does not mock the transport layer. It launches two `rmcp`-backed
reference servers and probes them through the real `agentkit-mcp` client:

- stateful Streamable HTTP with SSE responses
- stateless Streamable HTTP with JSON responses

Each probe verifies:

- `initialize` and capability discovery
- tool listing and `tools/call`
- resource listing and `resources/read`
- prompt listing and `prompts/get`
- negotiated protocol header propagation
- stateful session header reuse and `DELETE` on close
- interrupted SSE response resume via `Last-Event-ID`

## Run

Probe the stateful server:

```bash
cargo run -p mcp-reference-interop -- stateful
```

Probe the stateless JSON-response server:

```bash
cargo run -p mcp-reference-interop -- stateless
```

## Live Tests

The integration tests are self-contained and run with Cargo alone.

Run the stateful reference-implementation interop test:

```bash
cargo test -p mcp-reference-interop rust_sdk_stateful_streamable_http_interop -- --nocapture
```

Run the stateless JSON-response reference-implementation interop test:

```bash
cargo test -p mcp-reference-interop rust_sdk_stateless_json_streamable_http_interop -- --nocapture
```

Run the full transport/lifecycle coverage:

```bash
cargo test -p mcp-reference-interop -- --nocapture
```

## Upstream Targets

- Rust SDK crate: `rmcp = 1.3.0`
- Upstream repository: <https://github.com/modelcontextprotocol/rust-sdk>
