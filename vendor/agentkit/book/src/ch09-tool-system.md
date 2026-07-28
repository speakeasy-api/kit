# Designing a tool system

This chapter covers `agentkit-tools-core`: the tool execution contract that connects the loop to actual functionality. We'll walk through the design decisions behind tool specs, the registry, the executor, and how tools bridge to the capability layer underneath.

## The tool trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError>;

    // Optional. Default delegates to `invoke` and folds `Result` into
    // `ToolExecutionOutcome::Completed` / `::Failed`.
    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome { /* ... */ }
}
```

A tool has two concerns: description and execution. The `spec()` method returns the model-facing description. The `invoke()` method does the work. Tools that compose other tools (see `agentkit-tool-compose`) can override `invoke_outcome` instead, so a nested approval interruption propagates back to the loop as `ToolExecutionOutcome::Interrupted` rather than collapsing into an error.

### ToolSpec

```rust
pub struct ToolSpec {
    pub name: ToolName,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub annotations: ToolAnnotations,
    pub metadata: MetadataMap,
}
```

`output_schema` is the JSON Schema describing what the tool returns. Provider tool-call APIs (Anthropic, OpenAI, Gemini) don't carry an output schema in their declarations, so it is **not** forwarded verbatim to the model. Hosts may render it into the description, and composing tools (e.g. `agentkit-tool-compose`) surface it so a generated script can target the correct return shape on the first try. Set it on a spec with `ToolSpec::with_output_schema(schema)`.

`ToolAnnotations` carry behavioral hints:

```rust
pub struct ToolAnnotations {
    pub read_only_hint: bool,
    pub destructive_hint: bool,
    pub idempotent_hint: bool,
    pub needs_approval_hint: bool,
    pub supports_streaming_hint: bool,
}
```

These are hints, not guarantees. The actual enforcement comes from the permission system. But they're useful for model guidance, UI presentation, and default policy decisions.

## ToolRequest and ToolResult

```rust
pub struct ToolRequest {
    pub call_id: ToolCallId,
    pub tool_name: ToolName,
    pub input: Value,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub metadata: MetadataMap,
}
```

The request carries everything the tool needs to execute in context. Session and turn IDs let tools make context-aware decisions without depending on loop internals.

```rust
pub struct ToolResult {
    pub result: ToolResultPart,
    pub duration: Option<Duration>,
    pub metadata: MetadataMap,
}
```

The result wraps a `ToolResultPart` (from `agentkit-core`) with execution metadata.

## ToolContext

```rust
pub struct ToolContext<'a> {
    pub capability: CapabilityContext<'a>,
    pub permissions: &'a dyn PermissionChecker,
    pub resources: &'a dyn ToolResources,
    pub cancellation: Option<TurnCancellation>,
    pub execution_scope: Option<ToolExecutionScope>,
    pub approved_request: Option<ApprovalRequest>,
}
```

The context gives tools access to permissions, shared resources (like filesystem policy state), and cancellation. Tools don't reach into the loop — they get a narrow execution context.

`execution_scope` is the owned handle a composing tool uses to invoke other tools through the same executor, permission checks, and cancellation token. It is `None` when no scope was installed (older host wiring, raw `Invocable` adapters). When present, `scope.execute_child(request)` and `scope.execute_approved_child(request, approval)` return `ToolExecutionOutcome` directly — so nested approval interrupts propagate out via `invoke_outcome` instead of being collapsed.

`approved_request` is the `ApprovalRequest` currently being resumed when the invocation is the result of host approval. Tools that need to know "is this an approval resume?" can read it; the executor sets and restores it automatically across the `execute_approved` path.

## The registry

```rust
pub struct ToolRegistry {
    tools: BTreeMap<ToolName, Arc<dyn Tool>>,
}
```

The registry is simple: register tools, look them up by name, iterate specs. It uses `BTreeMap` for deterministic ordering.

```rust
let registry = ToolRegistry::new()
    .with(ReadFileTool::default())
    .with(WriteFileTool::default())
    .with(ShellExecTool::default());
```

The builder pattern via `.with()` makes registration ergonomic. Registries from different tool crates can be merged:

```rust
let registry = agentkit_tool_fs::registry()
    .merge(agentkit_tool_shell::registry());
```

## The executor

The loop doesn't call tools directly. It goes through a `ToolExecutor`:

```rust
pub trait ToolExecutor {
    async fn execute(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome;
}
```

The executor handles:

1. Registry lookup
2. Permission preflight
3. Approval determination
4. Tool invocation
5. Error normalization

This centralized layer is where safety logic lives, rather than being duplicated in every tool.

### Execution outcomes

```rust
pub enum ToolExecutionOutcome {
    Completed(ToolResult),
    Interrupted(ToolInterruption),
    Failed(ToolError),
}

pub enum ToolInterruption {
    ApprovalRequired(ApprovalRequest),
}
```

Not every execution failure is an error. An approval-required outcome means the tool is valid but needs human confirmation. The loop translates this into a `LoopStep::Interrupt(ApprovalRequest(...))`. Auth challenges are not modelled as a `ToolInterruption` — MCP-backed tools surface them as `ToolError::AuthRequired(AuthRequest)` and the host completes the flow out-of-band via `McpServerManager::resolve_auth`.

## Preflight permission requests

Tools can expose what they plan to do before execution by overriding `proposed_requests` on the `Tool` trait:

```rust
fn proposed_requests(
    &self,
    request: &ToolRequest,
) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
    Ok(Vec::new()) // default: no permissions needed
}
```

This lets the executor inspect and evaluate permission requests before any side effects occur. This is especially important for shell commands and filesystem writes — you want to check policy before running `rm -rf`, not after.

## Bridging to capabilities

`ToolCapabilityProvider` wraps a `ToolRegistry` as a `CapabilityProvider`, making every registered tool available as an `Invocable`. This is how the loop presents tools to the model alongside MCP-backed capabilities through a single unified list.

## The execution flow

Putting it all together — here's the complete path from model tool call to result:

```text
Model emits ToolCallPart
       │
       ▼
┌──────────────────────────────────┐
│  ToolExecutor                    │
│                                  │
│  1. Registry lookup              │
│     ToolName → Arc<dyn Tool>     │
│     └── not found → ToolError    │
│                                  │
│  2. Preflight                    │
│     tool.proposed_requests()     │
│     → Vec<PermissionRequest>     │
│                                  │
│  3. Permission evaluation        │
│     for each PermissionRequest:  │
│     checker.evaluate(req)        │
│     ├── Allow → continue         │
│     ├── Deny → stop, return err  │
│     └── RequireApproval → stop,  │
│         return ToolInterruption  │
│                                  │
│  4. Invocation                   │
│     tool.invoke(request, ctx)    │
│     → ToolResult                 │
│                                  │
│  5. Error normalization          │
│     ToolError → ToolResult       │
│     with ToolResultPart          │
│     { is_error: true }           │
└──────────────────────────────────┘
       │
       ▼
ToolExecutionOutcome::Completed(ToolResult)
```

Tool errors (file not found, invalid JSON, network failure) are normalized into a `ToolResult` whose `ToolResultPart` has `is_error: true` — the model sees the error message as a tool result and can decide to retry, try differently, or report the failure. Errors don't crash the loop or propagate to the host.

## Design decisions

### Why separate Tool from Invocable?

Tools add model-facing schema and permission semantics on top of the base invocable contract. A raw `Invocable` doesn't have annotations, preflight actions, or a permission context. Tools are a specialization, not the lowest layer.

### Why ToolName as a newtype?

`ToolName` prevents accidental confusion with other string identifiers. It also centralizes validation and supports namespacing conventions like `fs_read_file` or `mcp.github.search`.

### Why [JSON Schema](https://json-schema.org) for input?

Explicit JSON Schema keeps the tool contract provider-neutral. Tools don't depend on derive macros or schema generation libraries that might not match every provider's expectations. The schema is a JSON `Value` — any valid JSON Schema works:

```rust
input_schema: json!({
    "type": "object",
    "properties": {
        "path": { "type": "string", "description": "File path to read" },
        "from": { "type": "integer", "description": "Start line (optional)" },
        "to": { "type": "integer", "description": "End line (optional)" }
    },
    "required": ["path"]
})
```

If ergonomic schema helpers are needed later (derive macros, builder APIs), they can be added as optional companions without changing the base contract.

### Why `BTreeMap` for the registry?

`ToolRegistry` uses `BTreeMap<ToolName, Arc<dyn Tool>>` rather than `HashMap` for deterministic tool ordering. When the model receives the tool list, the order is always the same — this matters for reproducibility and for providers that may be sensitive to tool ordering in the prompt.

> **Crate:** [`agentkit-tools-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tools-core) — depends on [`agentkit-capabilities`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-capabilities) and [`agentkit-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-core).
