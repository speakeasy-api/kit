# Writing custom tools

Custom tools are the primary extension mechanism in agentkit. This chapter shows how to implement tools from simple to sophisticated, including preflight actions, custom permission types, and shared resources.

## Happy path: `#[tool]` macro

For a stateless tool whose input is a serializable struct, the `#[tool]` attribute from `agentkit-tools-derive` generates the entire `Tool` impl from one async function:

```rust,ignore
use agentkit_core::{MetadataMap, ToolOutput, ToolResultPart};
use agentkit_tools_core::{ToolError, ToolRegistry, ToolResult};
use agentkit_tools_derive::tool;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(JsonSchema, Deserialize)]
struct WordCountInput {
    /// The text whose words should be counted.
    text: String,
}

/// Count the number of whitespace-separated words in a string.
#[tool]
async fn word_count(input: WordCountInput) -> Result<ToolResult, ToolError> {
    let count = input.text.split_whitespace().count();
    Ok(ToolResult {
        result: ToolResultPart {
            call_id: Default::default(),
            output: ToolOutput::Text(format!("{count} word(s)")),
            is_error: false,
            metadata: MetadataMap::new(),
        },
        duration: None,
        metadata: MetadataMap::new(),
    })
}

let registry = ToolRegistry::new().with(word_count);
```

What the macro generates:

- A unit struct named the same as the function (`word_count`), implementing `Tool`. Registering reads as `registry.with(word_count)`.
- A `ToolSpec` whose `input_schema` is derived from the input type via `schemars::JsonSchema`. The schema is built once at first access and cached in a `OnceLock`.
- `name` defaults to the function's identifier; override with `#[tool(name = "explicit_name")]`.
- `description` defaults to the function's first doc comment line; override with `#[tool(description = "...")]`.
- `invoke` decodes `request.input` into the input type using `serde_json` and calls the user body. Decode errors become `ToolError::InvalidInput`, which the loop surfaces as an error `ToolResult` so the model can correct itself on the next turn.

Requirements for the input type:

- Implements `schemars::JsonSchema` (for the `input_schema` field).
- Implements `serde::Deserialize` (for decoding `request.input`).

Requirements for the function:

- `async fn`, with an explicit return type of `Result<ToolResult, ToolError>` (or any compatible alias).
- Exactly one positional argument: the input struct. The argument name becomes a local binding inside the generated `invoke` body. `ToolContext` is not threaded into the macro shape — if your tool needs it, use the manual impl below.

Enable the macro by adding the two crates to your `Cargo.toml`:

```toml
[dependencies]
agentkit-tools-core = { version = "...", features = ["schemars"] }
agentkit-tools-derive = "..."
schemars = { version = "1", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
```

The companion runnable example lives at `examples/openrouter-macro-tool/`.

For tools that need access to `ToolContext`, hold per-instance state, propose preflight permission requests, or surface custom permission kinds, drop down to the manual `impl Tool` path described in the rest of this chapter.

## A minimal tool

```rust
use agentkit_tools_core::*;
use agentkit_core::*;
use async_trait::async_trait;
use serde_json::json;

pub struct EchoTool {
    spec: ToolSpec,
}

impl EchoTool {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec {
                name: ToolName::new("echo"),
                description: "Return the input unchanged".into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" }
                    },
                    "required": ["message"]
                }),
                annotations: ToolAnnotations {
                    read_only_hint: true,
                    ..Default::default()
                },
                metadata: MetadataMap::new(),
            },
        }
    }
}

#[async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let message = request.input["message"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidInput("missing message".into()))?;

        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Text(message.to_string()),
                is_error: false,
                metadata: MetadataMap::new(),
            },
            duration: None,
            metadata: MetadataMap::new(),
        })
    }
}
```

Register it:

```rust
let registry = ToolRegistry::new().with(EchoTool::new());
```

## Using ToolContext

Tools receive a `ToolContext` that provides access to the current session, permissions, cancellation state, and shared resources:

```rust
async fn invoke(&self, request: ToolRequest, ctx: &mut ToolContext<'_>) -> Result<ToolResult, ToolError> {
    // Check cancellation
    if let Some(ref cancel) = ctx.cancellation {
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
    }

    // Access shared resources
    let resources = ctx.resources;

    // Access session identity
    let session_id = ctx.capability.session_id;

    // Detect that the loop is resuming this call after a host approval
    if let Some(approval) = ctx.approved_request.as_ref() {
        // resume side of the work
    }

    // Invoke another tool through the same executor + permissions
    if let Some(scope) = ctx.execution_scope.clone() {
        let outcome = scope.execute_child(child_request).await;
    }

    // ...
}
```

`execution_scope` is the supported entry point for tools that compose other tools (see `agentkit-tool-compose` and the `Composing other tools` section below). `approved_request` is `Some` only while the loop is resuming this call past a host approval; the executor restores the previous value on return.

## Adding preflight permission requests

For tools with side effects, override `proposed_requests` on the `Tool` trait to expose proposed actions before execution:

```rust
impl Tool for DeployTool {
    fn spec(&self) -> &ToolSpec { &self.spec }

    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
        let env = request.input["environment"].as_str().unwrap_or("unknown");
        Ok(vec![Box::new(DeployPermissionRequest {
            environment: env.to_string(),
            service: "my-service".into(),
            metadata: MetadataMap::new(),
        })])
    }

    async fn invoke(&self, request: ToolRequest, ctx: &mut ToolContext<'_>)
        -> Result<ToolResult, ToolError> { /* ... */ }
}
```

The executor evaluates these before calling `invoke()`. If any are denied or require approval, execution stops before any side effects occur.

## Custom permission requests

Define your own permission request types:

```rust
pub struct DeployPermissionRequest {
    pub environment: String,
    pub service: String,
    pub metadata: MetadataMap,
}

impl PermissionRequest for DeployPermissionRequest {
    fn kind(&self) -> &'static str { "myapp.deploy" }
    fn summary(&self) -> String {
        format!("Deploy {} to {}", self.service, self.environment)
    }
    fn metadata(&self) -> &MetadataMap { &self.metadata }
    fn as_any(&self) -> &dyn Any { self }
}
```

Host policies can match on `kind()` generically, or downcast through `as_any()` for type-safe field access.

## Shared resources via ToolResources

If your tool needs session-scoped state (like the filesystem tools' read-before-write tracker), implement `ToolResources`:

```rust
pub trait ToolResources: Send + Sync {
    fn as_any(&self) -> &dyn Any;
}
```

Register resources when building the agent, and downcast in your tool's `invoke()` method.

## Tool composition patterns

### Nesting agents as tools

A powerful pattern: implement a tool that runs a nested agent loop. The outer agent calls the tool with a task description, the tool starts an inner agent, runs it to completion, and returns the result.

```text
Outer agent (orchestrator):
  Model: "I need to research this codebase and write a report"
  Model: ToolCall(subagent, { task: "Find all uses of unsafe code", tools: ["fs", "shell"] })
         │
         ▼
  Inner agent (researcher):
    Model: ToolCall(fs_read_file, { path: "src/lib.rs" })
    Model: ToolCall(shell_exec, { executable: "grep", argv: ["-r", "unsafe", "src/"] })
    Model: "Found 3 uses of unsafe in parser.rs, codec.rs, and ffi.rs..."
         │
         ▼
  Outer agent receives: "Found 3 uses of unsafe..."
  Model: "Based on my research, here's the report..."
```

The inner agent has its own transcript, tools, and session. It doesn't share state with the outer agent — this isolation prevents context pollution and makes the sub-agent's scope explicit.

The [`openrouter-subagent-tool`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-subagent-tool) example shows a complete implementation of this pattern.

### Composing other tools

When a tool needs to invoke other tools — not a nested agent loop, just direct calls — go through `ctx.execution_scope` rather than holding a reference to the registry or executor yourself. The scope routes nested calls through the same `ToolExecutor`, permission checks, output truncation, and cancellation token as the parent call.

```rust
async fn invoke_outcome(
    &self,
    request: ToolRequest,
    ctx: &mut ToolContext<'_>,
) -> ToolExecutionOutcome {
    let scope = match ctx.execution_scope.clone() {
        Some(scope) => scope,
        None => return ToolExecutionOutcome::Failed(
            ToolError::Internal("missing execution scope".into()),
        ),
    };

    let child = ToolRequest::new(/* ... */);
    match scope.execute_child(child).await {
        ToolExecutionOutcome::Completed(result) => /* ... */,
        // Propagate child approval interrupts back to the loop.
        ToolExecutionOutcome::Interrupted(i) => ToolExecutionOutcome::Interrupted(i),
        ToolExecutionOutcome::Failed(e) => ToolExecutionOutcome::Failed(e),
    }
}
```

Override `invoke_outcome` (not just `invoke`) so a child tool's `ApprovalRequired` interruption travels back to the loop and the host can decide. After approval, the loop replays your tool with `ctx.approved_request = Some(...)` set — use `scope.execute_approved_child(request, approval)` to advance the specific child call past the recorded approval.

The [`agentkit-tool-compose`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tool-compose) crate is the canonical example: it exposes a `compose` tool that runs sandboxed Lua scripts, and each `tool(name, input)` call inside the script becomes one `scope.execute_child(...)` round.

### Tool registries from crates

Organize related tools into crate-level `registry()` functions:

```rust
pub fn registry() -> ToolRegistry {
    ToolRegistry::new()
        .with(ToolA::default())
        .with(ToolB::default())
}
```

Host applications merge registries from multiple crates:

```rust
let registry = my_tools::registry()
    .merge(agentkit_tool_fs::registry())
    .merge(agentkit_tool_shell::registry());
```

### Stateful tools

Tools that need to maintain state across invocations (counters, caches, connection pools) should use `ToolResources`:

```rust
struct MyToolResources {
    cache: Mutex<HashMap<String, String>>,
    http_client: reqwest::Client,
}

impl ToolResources for MyToolResources {
    fn as_any(&self) -> &dyn Any { self }
}

// In your tool's invoke():
let resources = ctx.resources
    .as_any()
    .downcast_ref::<MyToolResources>()
    .expect("MyToolResources not registered");

let mut cache = resources.cache.lock().unwrap();
```

Register resources when building the agent:

```rust
let agent = Agent::builder()
    .model(adapter)
    .resources(MyToolResources::new())
    .build()?;
```

All tools in the session share the same `ToolResources` instance. This is how the filesystem tools share their read-before-write tracker — `FileSystemToolResources` implements `ToolResources` and is downcast in each tool's `invoke()`.

> **Example:** [`openrouter-subagent-tool`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-subagent-tool) implements a custom tool that runs a nested agent as a tool call.
>
> **Crate:** [`agentkit-tools-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tools-core) — `Tool`, `ToolRegistry`, `ToolResources`, `ToolContext`.
