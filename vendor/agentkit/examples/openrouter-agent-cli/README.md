# `openrouter-agent-cli`

One-shot example that combines:

- `agentkit-loop`
- `agentkit-provider-openrouter`
- `agentkit-context`
- `agentkit-tool-fs`
- `agentkit-tool-shell`
- `agentkit-mcp`
- `agentkit-compaction`
- `agentkit-reporting`

## Run

Repository inspection:

```bash
cargo run -p openrouter-agent-cli -- \
  "Use fs_read_file on ./Cargo.toml and return only the workspace member count as an integer."
```

Context-aware run:

```bash
cargo run -p openrouter-agent-cli -- \
  --context-root examples/openrouter-context-agent/fixtures/project \
  "From the loaded context only, return the project codename. Return only the codename."
```

Context + MCP:

```bash
cargo run -p openrouter-agent-cli -- \
  --context-root examples/openrouter-context-agent/fixtures/project \
  --mcp-mock \
  "Return a JSON object with keys codename and secret. Read codename from context and secret from the MCP tool. Do not include prose."
```

## Notes

- The example loads environment variables from the workspace `.env`.
- The shell tool is intentionally constrained to simple read-only commands.
- `--mcp-mock` starts a local stdio MCP server by re-executing the example binary with `--serve-mock-mcp`.
- The compaction setup uses a built-in strategy pipeline: drop reasoning, drop failed tool results, then keep recent items while preserving system and context items.
