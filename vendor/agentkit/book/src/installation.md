# Installation

## Requirements

- Rust 1.92 or later (workspace edition 2024)

## Adding agentkit to your project

```sh
cargo add agentkit
```

Or add it to your `Cargo.toml`:

```toml
[dependencies]
agentkit = "0.10.2"
```

## Minimal dependency set

By default, agentkit enables: `core`, `capabilities`, `tools`, `loop`, and `reporting`. The `loop` feature transitively pulls in `task-manager`.

To keep your build lean, disable defaults and pick only what you need:

```toml
[dependencies]
agentkit = { version = "0.10.2", default-features = false, features = ["core", "loop"] }
```

Provider adapters and MCP integration are opt-in features:

```toml
[dependencies]
agentkit = { version = "0.10.2", features = ["provider-anthropic", "mcp", "tool-fs", "tool-shell"] }
```

See the [Feature flags reference](./feature-flags.md) for the full list.

## Building from source

```sh
git clone https://github.com/danielkov/agentkit.git
cd agentkit
cargo build
```

## Running the examples

Most examples use OpenRouter as the model provider. Create a `.env` file in the repo root:

```env
OPENROUTER_API_KEY=your_key_here
OPENROUTER_MODEL=anthropic/claude-sonnet-4.5
```

Then run any example:

```sh
cargo run -p openrouter-chat -- "hello"
```

For the Anthropic provider, the `anthropic-chat` example demonstrates streaming, server tools, and extended thinking:

```env
ANTHROPIC_API_KEY=your_key_here
ANTHROPIC_MODEL=claude-opus-4-7
ANTHROPIC_MAX_TOKENS=4096
```

```sh
cargo run -p anthropic-chat -- --web-search 3 --thinking 2048
```

For the Cerebras provider, the `cerebras-chat` REPL covers the chat path and the `cerebras-batch` CLI covers the Files + Batch API:

```env
CEREBRAS_API_KEY=your_key_here
CEREBRAS_MODEL=gpt-oss-120b
```

```sh
cargo run -p cerebras-chat -- --reasoning-effort medium --compression msgpack+gzip
cargo run -p cerebras-batch -- run ./prompts.json
```

The full set of bundled examples:

- `openrouter-chat` — minimal REPL against OpenRouter
- `openrouter-agent-cli` — agent loop with tools
- `openrouter-coding-agent` — coding agent with filesystem + shell
- `openrouter-compaction-agent` — compaction strategies
- `openrouter-context-agent` — `AGENTS.md` and skills loading
- `openrouter-context-window-compaction` — semantic compaction triggers
- `openrouter-macro-tool` — `#[tool]` macro
- `openrouter-mcp-tool` — federated MCP tools
- `openrouter-parallel-agent` — parallel/background tool execution
- `openrouter-session-persistence` — resumable sessions
- `openrouter-subagent-tool` — subagent-as-tool composition
- `openrouter-acp-trio` — three agents exposed as in-memory ACP endpoints, calling each other over the Agent Client Protocol
- `anthropic-chat` — streaming, server tools, extended thinking
- `cerebras-chat`, `cerebras-batch` — Cerebras chat + batch
- `mcp-dynamic-auth`, `mcp-reference-interop` — MCP transport + auth

The examples are referenced throughout this book. Each chapter points to the relevant example that exercises the concepts being discussed.
