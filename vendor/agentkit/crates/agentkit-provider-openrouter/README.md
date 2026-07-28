# agentkit-provider-openrouter

<p align="center">
  <a href="https://crates.io/crates/agentkit-provider-openrouter"><img src="https://img.shields.io/crates/v/agentkit-provider-openrouter.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-provider-openrouter"><img src="https://img.shields.io/docsrs/agentkit-provider-openrouter?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-provider-openrouter.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

OpenRouter model adapter for `agentkit-loop`.

This crate translates between agentkit transcript primitives and OpenRouter chat completion requests, including:

- session and turn adapters
- tool declaration and tool-call decoding
- SSE streaming of text, reasoning, tool calls, and generated image parts
- multimodal user content mapping (images, audio)
- usage and finish-reason normalization
- environment-based configuration helpers

Use it when OpenRouter is the backing model provider for your agent runtime.

## Configuration

Create a config with `OpenRouterConfig::new(api_key, model)` and chain `.with_*()` builders for optional parameters. Streaming is enabled by default; use `.with_streaming(false)` to force the buffered response path. Alternatively, `OpenRouterConfig::from_env()` reads from environment variables:

| Variable                           | Required | Default                                         |
| ---------------------------------- | -------- | ----------------------------------------------- |
| `OPENROUTER_API_KEY`               | yes      | --                                              |
| `OPENROUTER_MODEL`                 | no       | `openrouter/auto`                               |
| `OPENROUTER_BASE_URL`              | no       | `https://openrouter.ai/api/v1/chat/completions` |
| `OPENROUTER_APP_NAME`              | no       | --                                              |
| `OPENROUTER_SITE_URL`              | no       | --                                              |
| `OPENROUTER_MAX_COMPLETION_TOKENS` | no       | --                                              |
| `OPENROUTER_TEMPERATURE`           | no       | --                                              |

## Examples

### Minimal chat agent

```rust,no_run
use agentkit_loop::{Agent, PromptCacheRequest, PromptCacheRetention, SessionConfig};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
let config = OpenRouterConfig::new("sk-or-v1-...", "anthropic/claude-sonnet-4.5")
    .with_app_name("my-agent");
let adapter = OpenRouterAdapter::new(config)?;

let agent = Agent::builder()
    .model(adapter)
    .build()?;

let mut driver = agent
    .start(
        SessionConfig::new("demo").with_cache(
            PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
        ),
    )
    .await?;

let step = driver.next().await?;
println!("{step:?}");
# Ok(())
# }
```

### With model parameters

```rust,no_run
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let config = OpenRouterConfig::new("sk-or-v1-...", "anthropic/claude-sonnet-4.5")
    .with_temperature(0.0)
    .with_max_completion_tokens(4096)
    .with_app_name("my-agent")
    .with_site_url("https://example.com");

let adapter = OpenRouterAdapter::new(config)?;
# Ok(())
# }
```

### Environment-based configuration with overrides

```rust,no_run
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let config = OpenRouterConfig::from_env()?
    .with_temperature(0.0)
    .with_max_completion_tokens(512)
    .with_extra_body_value("top_p", 0.95);

let adapter = OpenRouterAdapter::new(config)?;
# Ok(())
# }
```
