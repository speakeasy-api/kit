# agentkit-capabilities

<p align="center">
  <a href="https://crates.io/crates/agentkit-capabilities"><img src="https://img.shields.io/crates/v/agentkit-capabilities.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-capabilities"><img src="https://img.shields.io/docsrs/agentkit-capabilities?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-capabilities.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Common abstractions for exposing tools, resources, and prompts as model-facing capabilities within the [agentkit](https://github.com/danielkov/agentkit) ecosystem.

This crate provides:

- The **`Invocable` trait** -- the core interface for anything the model can call during a conversation turn
- **`ResourceProvider`** and **`PromptProvider`** traits for surfacing data and prompt templates
- **`CapabilityProvider`** for bundling capabilities from a single source (e.g. an MCP server)
- Shared **`CapabilityContext`** that carries session/turn identifiers across invocations
- Common **error types** used by all capability adapters

## When to use this crate

Use `agentkit-capabilities` when you want to expose non-tool functionality through a
consistent interface that can participate in sessions and turns. For example:

- Wrapping an MCP server so its tools, resources, and prompts appear in the agentic loop
- Building a custom capability that doesn't fit the `Tool` trait in `agentkit-tools-core`
- Creating a `CapabilityProvider` that aggregates multiple backends behind a single facade

If you are building a standard tool (file I/O, shell commands, etc.), you probably want
`agentkit-tools-core` instead -- it provides a higher-level `Tool` trait with approval
flows, annotations, and execution context.

## Example: implementing a custom `Invocable`

```rust
use agentkit_capabilities::{
    CapabilityContext, CapabilityError, CapabilityName, Invocable, InvocableOutput,
    InvocableRequest, InvocableResult, InvocableSpec,
};
use async_trait::async_trait;
use serde_json::json;

/// A capability that searches a codebase and returns matching lines.
struct CodeSearch {
    spec: InvocableSpec,
}

impl CodeSearch {
    fn new() -> Self {
        Self {
            spec: InvocableSpec::new(
                CapabilityName::new("code_search"),
                "Search the codebase for a regex pattern",
                json!({
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string" },
                        "path":    { "type": "string" }
                    },
                    "required": ["pattern"]
                }),
            ),
        }
    }
}

#[async_trait]
impl Invocable for CodeSearch {
    fn spec(&self) -> &InvocableSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: InvocableRequest,
        _ctx: &mut CapabilityContext<'_>,
    ) -> Result<InvocableResult, CapabilityError> {
        let pattern = request.input["pattern"]
            .as_str()
            .ok_or_else(|| CapabilityError::InvalidInput("missing 'pattern'".into()))?;

        // In a real implementation you would run the search here.
        let results = format!("Found 3 matches for `{pattern}`");

        Ok(InvocableResult::text(results))
    }
}
```

## Example: bundling capabilities with `CapabilityProvider`

```rust
use std::sync::Arc;
use agentkit_capabilities::{
    CapabilityProvider, Invocable, PromptProvider, ResourceProvider,
};

/// Groups all capabilities exposed by a single backend.
struct MyBackend {
    invocables: Vec<Arc<dyn Invocable>>,
}

impl CapabilityProvider for MyBackend {
    fn invocables(&self) -> Vec<Arc<dyn Invocable>> {
        self.invocables.clone()
    }

    fn resources(&self) -> Vec<Arc<dyn ResourceProvider>> {
        // This backend does not expose any resources.
        vec![]
    }

    fn prompts(&self) -> Vec<Arc<dyn PromptProvider>> {
        // This backend does not expose any prompt templates.
        vec![]
    }
}
```
