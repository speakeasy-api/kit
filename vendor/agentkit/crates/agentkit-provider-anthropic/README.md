# agentkit-provider-anthropic

<p align="center">
  <a href="https://crates.io/crates/agentkit-provider-anthropic"><img src="https://img.shields.io/crates/v/agentkit-provider-anthropic.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-provider-anthropic"><img src="https://img.shields.io/docsrs/agentkit-provider-anthropic?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-provider-anthropic.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Anthropic Messages API provider for [`agentkit`](https://github.com/danielkov/agentkit).

Connects the agent loop to `https://api.anthropic.com/v1/messages` (or any
compatible endpoint). Supports streaming, prompt caching, extended thinking,
and server-side tools (web search, web fetch, code execution).

## Quick start

```rust,ignore
use agentkit_loop::{Agent, SessionConfig};
use agentkit_provider_anthropic::{AnthropicAdapter, AnthropicConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = AnthropicConfig::from_env()?;
    let adapter = AnthropicAdapter::new(config)?;

    let agent = Agent::builder().model(adapter).build()?;
    let _driver = agent.start(SessionConfig::new("demo")).await?;
    Ok(())
}
```

## Environment variables

| Variable               | Required | Default                                     |
| ---------------------- | -------- | ------------------------------------------- |
| `ANTHROPIC_API_KEY`    | one of   | —                                           |
| `ANTHROPIC_AUTH_TOKEN` | one of   | — (bearer token; takes precedence if set)   |
| `ANTHROPIC_MODEL`      | yes      | —                                           |
| `ANTHROPIC_MAX_TOKENS` | yes      | — (required per-request by the API)         |
| `ANTHROPIC_BASE_URL`   | no       | `https://api.anthropic.com/v1/messages`     |
| `ANTHROPIC_VERSION`    | no       | `2023-06-01`                                |
| `ANTHROPIC_BETA`       | no       | comma-separated list of anthropic-beta tags |

## Server tools

Known server tools (`WebSearchTool`, `WebFetchTool`, `CodeExecutionTool`, etc.)
implement the `ServerTool` trait. `RawServerTool` passes through any tool
type the crate does not yet cover.
