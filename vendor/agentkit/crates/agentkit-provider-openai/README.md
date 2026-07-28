# agentkit-provider-openai

<p align="center">
  <a href="https://crates.io/crates/agentkit-provider-openai"><img src="https://img.shields.io/crates/v/agentkit-provider-openai.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-provider-openai"><img src="https://img.shields.io/docsrs/agentkit-provider-openai?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-provider-openai.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

OpenAI model adapter for the agentkit agent loop.

This crate provides `OpenAIAdapter` and `OpenAIConfig` for connecting the
agent loop to the [OpenAI](https://platform.openai.com) chat completions API.
It handles request translation, response normalization, usage reporting, and
prompt cache integration for OpenAI-backed sessions. Streaming is enabled by
default; use `.with_streaming(false)` to force the buffered response path.

Applications that want an OpenAI-powered agent will usually use this crate
through the umbrella `agentkit` crate's `provider-openai` feature, or depend on
it directly when assembling a smaller runtime.

## Configuration

Create a config with `OpenAIConfig::new(api_key, model)` and chain `.with_*()` builders for optional parameters. Alternatively, `OpenAIConfig::from_env()` reads from environment variables:

| Variable          | Required | Default                                      |
| ----------------- | -------- | -------------------------------------------- |
| `OPENAI_API_KEY`  | yes      | --                                           |
| `OPENAI_MODEL`    | no       | `gpt-4o`                                     |
| `OPENAI_BASE_URL` | no       | `https://api.openai.com/v1/chat/completions` |

## Examples

### Minimal chat agent

```rust,no_run
use agentkit_loop::{Agent, SessionConfig};
use agentkit_provider_openai::{OpenAIAdapter, OpenAIConfig};

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
let config = OpenAIConfig::new("sk-...", "gpt-4o");
let adapter = OpenAIAdapter::new(config)?;

let agent = Agent::builder()
    .model(adapter)
    .build()?;

let mut driver = agent
    .start(SessionConfig::new("demo"))
    .await?;

let step = driver.next().await?;
println!("{step:?}");
# Ok(())
# }
```

### With model parameters

```rust,no_run
use agentkit_provider_openai::{OpenAIAdapter, OpenAIConfig};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let config = OpenAIConfig::new("sk-...", "gpt-4o")
    .with_temperature(0.0)
    .with_max_completion_tokens(4096)
    .with_frequency_penalty(0.5);

let adapter = OpenAIAdapter::new(config)?;
# Ok(())
# }
```

### Environment-based configuration with overrides

```rust,no_run
use agentkit_provider_openai::{OpenAIAdapter, OpenAIConfig};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let config = OpenAIConfig::from_env()?
    .with_temperature(0.0)
    .with_max_completion_tokens(512);

let adapter = OpenAIAdapter::new(config)?;
# Ok(())
# }
```

### Custom base URL (Azure OpenAI, proxies, etc.)

```rust,no_run
use agentkit_provider_openai::{OpenAIAdapter, OpenAIConfig};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let config = OpenAIConfig::new("sk-...", "gpt-4o")
    .with_base_url("https://my-resource.openai.azure.com/openai/deployments/gpt-4o/chat/completions?api-version=2024-02-15-preview");

let adapter = OpenAIAdapter::new(config)?;
# Ok(())
# }
```
