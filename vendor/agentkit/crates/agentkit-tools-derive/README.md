# agentkit-tools-derive

<p align="center">
  <a href="https://crates.io/crates/agentkit-tools-derive"><img src="https://img.shields.io/crates/v/agentkit-tools-derive.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-tools-derive"><img src="https://img.shields.io/docsrs/agentkit-tools-derive?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-tools-derive.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Procedural macros for declaring agentkit tools.

The `#[tool]` attribute turns either an async function **or** an `impl`
block on a struct into an `agentkit_tools_core::Tool` implementation. The
free-function form synthesises a unit struct named after the function;
the impl-block form preserves the existing struct so the tool can hold
state (channels, HTTP clients, caches).

## Free function

```rust,ignore
use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{ToolError, ToolResult};
use agentkit_tools_derive::tool;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(JsonSchema, Deserialize)]
struct WeatherInput {
    /// City name to look up.
    location: String,
}

/// Fetch the current weather for a location.
#[tool(read_only)]
async fn get_weather(input: WeatherInput) -> Result<ToolResult, ToolError> {
    Ok(ToolResult::new(ToolResultPart::success(
        // The macro overwrites `call_id` with the request's id; any
        // placeholder value is fine here.
        agentkit_core::ToolCallId::default(),
        ToolOutput::text(format!("sunny in {}", input.location)),
    )))
}

// Registers as `get_weather` in the catalog.
let registry = agentkit_tools_core::ToolRegistry::new().with(get_weather);
```

## Stateful impl block

```rust,ignore
use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{ToolError, ToolResult};
use agentkit_tools_derive::tool;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;

#[derive(JsonSchema, Deserialize)]
struct ReconnectInput { server_id: String }

pub struct Reconnector { cmd_tx: mpsc::Sender<String> }

#[tool(idempotent)]
impl Reconnector {
    /// Disconnect and reconnect a registered MCP server.
    async fn run(&self, input: ReconnectInput) -> Result<ToolResult, ToolError> {
        self.cmd_tx
            .send(input.server_id)
            .await
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        Ok(ToolResult::new(ToolResultPart::success(
            agentkit_core::ToolCallId::default(),
            ToolOutput::text("reconnected"),
        )))
    }
}
```

## Recognised arguments

- `name = "literal"` — overrides the tool name (defaults to the
  function/method identifier).
- `description = "literal"` — sets the description (defaults to the first
  doc-comment line).
- Annotation flags — each maps to a field on `ToolAnnotations` and may be
  bare (sets `true`) or take an explicit boolean: `read_only`,
  `destructive`, `idempotent`, `needs_approval`, `supports_streaming`.

The input struct must implement `schemars::JsonSchema` and
`serde::Deserialize`; the JSON Schema is derived at registration time via
`agentkit_tools_core::tool_spec_for`.
