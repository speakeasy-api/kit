# `openrouter-mcp-tool`

One-shot OpenRouter example that proves MCP tools can be surfaced through `agentkit-mcp` and called by the root agent.

The example spawns a local mock MCP stdio server inside the same binary. The root agent does **not** know the sealed launch code. The only way to obtain it is by calling the MCP tool `mcp.mock.reveal_secret`.

## Run

```bash
cargo run -p openrouter-mcp-tool -- \
  "Retrieve the sealed launch code via the MCP tool and return only the code."
```

By default the secret is `LANTERN-SECRET-93B7`. You can override it:

```bash
MCP_SECRET="OWL-SECRET-1182" cargo run -p openrouter-mcp-tool -- \
  "Retrieve the sealed launch code via the MCP tool and return only the code."
```

## Prove It With A Live Test

This package includes an ignored live integration-style test that asserts:

- the root agent called `mcp.mock.reveal_secret`
- the final root-agent output contains the secret

Run it with:

```bash
cargo test -p openrouter-mcp-tool root_agent_retrieves_secret_via_mcp_tool -- --ignored --nocapture
```
