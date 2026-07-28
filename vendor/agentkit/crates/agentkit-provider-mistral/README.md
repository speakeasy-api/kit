# agentkit-provider-mistral

<p align="center">
  <a href="https://crates.io/crates/agentkit-provider-mistral"><img src="https://img.shields.io/crates/v/agentkit-provider-mistral.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-provider-mistral"><img src="https://img.shields.io/docsrs/agentkit-provider-mistral?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-provider-mistral.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Mistral model adapter for the agentkit agent loop.

This crate provides `MistralAdapter` and `MistralConfig` for connecting the
agent loop to the [Mistral AI](https://mistral.ai) chat completions API. It
handles request translation, response normalization, and usage reporting for
Mistral-backed sessions. Streaming is enabled by default; use
`.with_streaming(false)` to force the buffered response path.

Note: Mistral uses `max_tokens` instead of the `max_completion_tokens` field
used by most other OpenAI-compatible APIs.

## Configuration

Create a config with `MistralConfig::new(api_key, model)` and chain `.with_*()` builders for optional parameters. Alternatively, `MistralConfig::from_env()` reads from environment variables:

| Variable           | Required | Default                                      |
| ------------------ | -------- | -------------------------------------------- |
| `MISTRAL_API_KEY`  | yes      | --                                           |
| `MISTRAL_MODEL`    | no       | `mistral-small-latest`                       |
| `MISTRAL_BASE_URL` | no       | `https://api.mistral.ai/v1/chat/completions` |

## Examples

### Minimal chat agent

```rust,no_run
use agentkit_loop::{Agent, SessionConfig};
use agentkit_provider_mistral::{MistralAdapter, MistralConfig};

# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
let config = MistralConfig::new("sk-...", "mistral-large-latest");
let adapter = MistralAdapter::new(config)?;

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
use agentkit_provider_mistral::{MistralAdapter, MistralConfig};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let config = MistralConfig::new("sk-...", "mistral-large-latest")
    .with_temperature(0.0)
    .with_max_tokens(4096);

let adapter = MistralAdapter::new(config)?;
# Ok(())
# }
```

### Environment-based configuration with overrides

```rust,no_run
use agentkit_provider_mistral::{MistralAdapter, MistralConfig};

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let config = MistralConfig::from_env()?
    .with_temperature(0.0)
    .with_max_tokens(512);

let adapter = MistralAdapter::new(config)?;
# Ok(())
# }
```
