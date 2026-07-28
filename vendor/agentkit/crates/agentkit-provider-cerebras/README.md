# agentkit-provider-cerebras

<p align="center">
  <a href="https://crates.io/crates/agentkit-provider-cerebras"><img src="https://img.shields.io/crates/v/agentkit-provider-cerebras.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-provider-cerebras"><img src="https://img.shields.io/docsrs/agentkit-provider-cerebras?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-provider-cerebras.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Cerebras Inference API adapter for the [agentkit](https://crates.io/crates/agentkit) agent loop.

Implements the `ModelAdapter` trait directly against Cerebras'
`/v1/chat/completions` endpoint. Streaming is on by default. Experimental
features are gated behind Cargo features.

## Quick start

```rust,ignore
use agentkit_loop::{Agent, SessionConfig};
use agentkit_provider_cerebras::{CerebrasAdapter, CerebrasConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = CerebrasConfig::from_env()?;
    let adapter = CerebrasAdapter::new(config)?;
    let agent = Agent::builder().model(adapter).build()?;
    let _driver = agent.start(SessionConfig::new("demo")).await?;
    Ok(())
}
```

## Environment

| Variable                         | Required | Default                      |
| -------------------------------- | -------- | ---------------------------- |
| `CEREBRAS_API_KEY`               | yes      | —                            |
| `CEREBRAS_MODEL`                 | yes      | —                            |
| `CEREBRAS_BASE_URL`              | no       | `https://api.cerebras.ai/v1` |
| `CEREBRAS_VERSION_PATCH`         | no       | —                            |
| `CEREBRAS_MAX_COMPLETION_TOKENS` | no       | —                            |

## Features

| Feature               | Purpose                                                     |
| --------------------- | ----------------------------------------------------------- |
| `streaming` (default) | SSE streaming of responses                                  |
| `compression`         | msgpack / gzip request compression                          |
| `predicted-outputs`   | `prediction` parameter (preview)                            |
| `service-tiers`       | `service_tier` + `queue_threshold` header (private preview) |
| `batch`               | Files + Batch async bulk inference API                      |
| `experimental`        | Enables all preview/private-preview features                |

## Retry

`agentkit-http` is a trait façade without a retry layer. Callers wanting retry
build their `Http` with the `reqwest-middleware-client` feature +
`reqwest-retry` and pass it via `CerebrasAdapter::with_client`.

## Metadata keys

Cerebras-specific metadata rides on `Usage.metadata` under the `cerebras.`
prefix:

- `cerebras.accepted_prediction_tokens`
- `cerebras.rejected_prediction_tokens`
- `cerebras.time_info` — `{queue_time, prompt_time, completion_time, total_time, created}`
- `cerebras.service_tier`
- `cerebras.service_tier_used`
- `cerebras.system_fingerprint`
