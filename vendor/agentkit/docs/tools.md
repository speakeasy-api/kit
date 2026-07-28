# agentkit-tools-core design

## Purpose

`agentkit-tools-core` is the tool execution contract crate.

It should define:

- how tools are described to models
- how the loop invokes tools
- how permissions and approval requirements are expressed
- how tool results and execution failures are surfaced
- how native tools and MCP-backed tools fit the same registry

It should make custom tools easy to add while giving built-in tools a shared safety model.

## Non-goals

`agentkit-tools-core` should not own:

- shell command execution details
- filesystem implementations
- MCP transport or process lifecycle
- loop control flow
- provider-specific tool calling formats
- prompt/context loading

It defines the execution boundary, not the individual tool products.

This should also remain the primary user-facing extension mechanism for v1.

If users want to add custom behavior, the first answer should be "implement a tool", not "implement a capability provider".

## Dependency direction

Recommended direction:

- `agentkit-core` defines normalized tool call and tool result value types
- `agentkit-capabilities` defines the lower-level invocable/resource/prompt layer
- `agentkit-tools-core` defines tool traits, registry, execution context, and permission contracts on top of invocables
- `agentkit-loop` depends on `agentkit-tools-core` to execute tool calls
- built-in tool crates depend on `agentkit-tools-core`
- `agentkit-mcp` adapts MCP tools/resources/prompts through these abstractions where appropriate

That gives one tool system instead of a separate path for every integration style.

## Design principles

### 1. Tool description and tool execution are different concerns

Every tool has at least two different representations:

- a model-facing description
- an executable implementation

Those should be separate, even if one type happens to implement both.

The model-facing part is mostly stable:

- name
- description
- input schema

The execution part is operational:

- parse input
- check policy
- run action
- produce output

Separating them keeps the registry flexible and makes testing easier.

### 2. Permissions are first-class, not ad hoc

Every serious agent tool system eventually needs:

- allow/deny checks
- approval requirements
- scoped execution context
- clear error reasons

This should not be reinvented independently by shell, filesystem, and MCP tools.

It should also not make custom tools feel second-class.

### 3. Tools should return normalized rich results

Tool output should not be reduced immediately to a plain string.

The loop and provider adapters will often need richer forms:

- text
- structured JSON
- file references
- mixed content parts

`agentkit-core` already defines the value layer for tool calls/results. `agentkit-tools-core` should preserve that richness at execution time.

### 4. Simple custom tools should remain easy

The safety model cannot come at the cost of making custom tools painful to implement.

The target should be:

- one trait for basic tools
- optional extension traits or hooks for advanced policy-aware tools

### 5. Built-in and remote tools should feel uniform

A host should be able to register:

- a native Rust tool
- a shell tool
- a filesystem tool
- an MCP tool proxy

through the same registry and with the same loop-facing execution path.

### 6. Tools are a specialization, not the lowest layer

The lower-level `agentkit-capabilities` layer should sit underneath tools.

That means:

- tools are invocables with model-facing schema and permission semantics
- MCP tools can share the same invocable base
- MCP resources and prompts do not need to be distorted into tool traits

### 7. Custom tools should be able to define native permission requests

Custom tools should not be forced to squeeze their safety model into a catch-all enum variant.

They should be able to expose first-class permission request types through the shared permission-request trait.

## Core concepts

## 1. Tool specification

The model-facing description should be explicit and serializable.

Recommended shape:

```rust
pub struct ToolSpec {
    pub name: ToolName,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub output_schema: Option<serde_json::Value>,
    pub annotations: ToolAnnotations,
    pub metadata: MetadataMap,
}
```

`output_schema` declares the JSON shape the tool returns. Provider tool-call APIs do not carry an output schema in their declarations, so it is not surfaced verbatim to the model. Hosts and composing tools may render it into the description or use it for validation; `agentkit-tool-compose` surfaces it through its Lua `tools()` helper so generated scripts can target the right return shape. Set it with `ToolSpec::with_output_schema(schema)`.

Recommended `ToolAnnotations` fields:

- `read_only_hint`
- `destructive_hint`
- `idempotent_hint`
- `needs_approval_hint`
- `supports_streaming_hint`

These are hints, not guarantees. The actual enforcement comes from policy.

Why keep them:

- useful for model guidance
- useful for UI presentation
- useful for host-side policy defaults

## 2. Tool identity

Tool names should be first-class and validated.

Recommended newtype:

```rust
pub struct ToolName(String);
```

Reasons:

- avoid accidental whitespace/case inconsistencies
- centralize validation rules
- allow future namespacing conventions

Suggested convention:

- lowercase ASCII
- `-`, `_`, and `.` allowed
- namespaced patterns such as `fs_read_file` or `mcp.github.search`

## 3. Tool trait

The base tool trait should be small.

Conceptually, `Tool` should build on the lower-level `Invocable` abstraction from `agentkit-capabilities`.

Recommended shape:

```rust
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;

    async fn invoke(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext,
    ) -> Result<ToolResult, ToolError>;

    // Optional. Default wraps `invoke`'s Result into
    // ToolExecutionOutcome::Completed / ::Failed. Override only when a
    // composing tool needs to propagate a nested `Interrupted` outcome
    // (e.g. a child approval) up to the loop.
    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext,
    ) -> ToolExecutionOutcome { /* ... */ }
}
```

Where:

- `ToolRequest` wraps the normalized `ToolCallPart` input plus execution metadata
- `ToolContext` gives access to scoped runtime capabilities
- `ToolResult` wraps normalized `ToolResultPart` output plus execution metadata

This is enough for most tools.

## 4. Tool request

Recommended `ToolRequest` contents:

- `call_id`
- `tool_name`
- raw JSON input
- session ID
- turn ID
- invocation metadata

Optional metadata may include:

- originating model/provider
- host-supplied request tags
- trace IDs

This lets tools make context-aware decisions without depending on loop internals.

## 5. Tool context

`ToolContext` is the operational context the loop gives to tools.

It should contain only what tools actually need.

Recommended responsibilities:

- expose permission and approval facilities
- expose scoped environment information
- expose cancellation state if supported
- expose host-provided shared resources

Possible shape:

```rust
pub struct ToolContext<'a> {
    pub session_id: &'a SessionId,
    pub turn_id: &'a TurnId,
    pub permissions: &'a dyn PermissionChecker,
    pub resources: &'a dyn ToolResources,
    pub cancellation: Option<TurnCancellation>,
    pub execution_scope: Option<ToolExecutionScope>,
    pub approved_request: Option<ApprovalRequest>,
}
```

The important boundary is that tools do not reach directly into the loop. They get a narrow execution context.

`execution_scope` is an owned scope (executor + session + turn + permissions + resources + cancellation) that lets a composing tool invoke other tools through the same execution path. Call `scope.execute_child(request)` for a normal nested call and `scope.execute_approved_child(request, approval)` to resume a child past a recorded approval. Both return `ToolExecutionOutcome`, so a nested approval interruption can propagate up through the parent's `invoke_outcome` instead of being collapsed into an error.

`approved_request` is the `ApprovalRequest` currently being resumed when the invocation is the result of host approval. The `BasicToolExecutor` sets it on entry to `execute_approved` and restores the previous value on return.

## 6. Tool registry

The registry should be straightforward and deterministic.

Recommended shape:

```rust
pub struct ToolRegistry {
    tools: BTreeMap<ToolName, Arc<dyn Tool>>,
}
```

Recommended operations:

- register one tool
- register many tools
- get tool by name
- iterate specs
- freeze registry for execution

The loop needs two things from the registry:

- a stable list of tool specs to expose to the model
- a name lookup for invocation

## 7. Tool executor boundary

The loop should not know every detail of policy evaluation and execution plumbing.

It should call into a higher-level execution boundary such as:

```rust
pub trait ToolExecutor {
    async fn execute(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext,
    ) -> ToolExecutionOutcome;
}
```

Why this extra layer is useful:

- registry lookup
- permission checks
- approval determination
- metrics and timing
- tool invocation
- error normalization

can all live behind one call.

This gives you a place to centralize safety logic instead of duplicating it in every tool crate.

## 8. Execution outcome

Tool execution needs more than success/failure.

Recommended shape:

```rust
pub enum ToolExecutionOutcome {
    Completed(ToolResult),
    Interrupted(ToolInterruption),
    Failed(ToolError),
}
```

Where `ToolInterruption` captures blocking cases such as:

- approval required
- auth required
- missing host capability

This matters because not every tool execution failure is an actual error.

In many cases, the correct result is:

- emit an approval-required event
- return a loop interrupt
- resume the exact same tool execution later

## 9. Permissions

Permissions should have a shared contract.

Recommended base trait:

```rust
pub trait PermissionChecker: Send + Sync {
    fn evaluate(&self, action: &ProposedToolAction) -> PermissionDecision;
}
```

Recommended decision type:

```rust
pub enum PermissionDecision {
    Allow,
    Deny { reason: String },
    RequireApproval(ApprovalRequest),
}
```

This is the main unifying concept for safety.

Built-in tools should describe their risky behavior as normalized `ProposedToolAction` values that the permission checker can evaluate.

Examples:

- run shell command
- read file
- write file
- delete file
- connect to MCP server
- invoke remote MCP tool with auth scope

## 10. Preflight permission requests

Tools need a way to explain what they are about to do before they do it.

The permission layer should expose a trait-based request model.

That means tools should be able to preflight one or more `PermissionRequest` values before execution.

Built-in tools should use built-in request families.
Custom tools may define their own request types.

## 11. Approval and auth

Tool execution may need to stop for reasons beyond generic approval.

Two important cases:

- explicit host approval
- external auth completion

Recommended approach:

- represent both as `ToolInterruption`
- let the loop translate those into blocking `LoopInterrupt`s
- let the host resume execution after satisfying the requirement

Possible shape:

```rust
pub enum ToolInterruption {
    ApprovalRequired(ApprovalRequest),
    AuthRequired(AuthRequest),
}
```

This is important for MCP, where auth is a first-class concern.

## 12. Tool result

The execution layer should preserve both semantic output and operational metadata.

Recommended shape:

```rust
pub struct ToolResult {
    pub result: ToolResultPart,
    pub duration: Option<Duration>,
    pub metadata: MetadataMap,
}
```

This allows:

- normalized transcript integration via `ToolResultPart`
- operational reporting via metadata and duration

## 13. Tool errors

Tool errors should be categorized clearly.

Recommended categories:

- invalid input
- permission denied
- execution failed
- unavailable
- interrupted
- internal tool error

The error type should be expressive enough for reporting and policy, but not so detailed that every tool invents a unique taxonomy.

## Custom tool ergonomics

Custom tools should not be forced to implement the entire safety stack themselves.

Recommended ergonomics:

- implement `Tool`
- optionally expose a preflight `ProposedToolAction`
- let the executor perform default permission handling

A simple custom tool should be possible with:

```rust
pub struct EchoTool;

impl Tool for EchoTool {
    fn spec(&self) -> &ToolSpec { /* ... */ }

    async fn invoke(
        &self,
        request: ToolRequest,
        _ctx: &mut ToolContext,
    ) -> Result<ToolResult, ToolError> {
        /* ... */
    }
}
```

Advanced tools can opt into richer action descriptions for better policy support.

## Preflight actions

To support shared permission logic cleanly, tools should optionally expose what they plan to do before they execute it.

Recommended extension trait:

```rust
pub trait PreflightTool: Tool {
    fn proposed_requests(
        &self,
        request: &ToolRequest,
    ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError>;
}
```

This lets the executor:

- inspect intended requests
- run permission checks
- raise approval interrupts
- avoid partially executing side effects before approval

This is especially important for shell, filesystem, and MCP tools.

## Built-in tool crate boundaries

V1 built-ins should be separate crates using the same contracts.

Mandatory built-in focus for the first public release:

- filesystem tools
- a simple and low-friction path for implementing custom tools

Other built-ins can follow once the base extension path feels good in code.

## `agentkit-tool-shell`

Responsibilities:

- command execution
- cwd scoping
- environment filtering
- timeout handling
- optional PTY support if added later

Should emit structured `ShellPermissionRequest` preflight values such as:

- command line
- cwd
- env policy summary

Should not own the global approval model.

## `agentkit-tool-fs`

Responsibilities:

- read file
- write file
- edit file
- list directory
- create/delete/move operations as scoped by policy

Should emit structured `FileSystemPermissionRequest` values such as:

- read path
- write path
- delete path
- move path

Should rely on shared permission decisions rather than inline one-off checks where possible.

## `agentkit-mcp`

MCP is broader than ordinary local tools, but the tool-facing path should still align.

Responsibilities:

- discover MCP tools
- surface MCP tool specs
- invoke MCP tools through the same execution path
- surface auth and capability interruptions
- expose MCP resources and prompts through adjacent abstractions

Not everything in MCP is a tool, so the crate will likely have additional abstractions for:

- resources
- prompts
- server/session management

But MCP tools themselves should still fit naturally into the same registry/executor boundary.

## Tool schema generation

V1 should keep schema generation simple.

Recommended approach:

- tools provide JSON Schema explicitly
- helper macros or derive support can come later

Reason:

- avoids locking into one schema generation library too early
- keeps the base contract explicit
- makes MCP and custom remote tools easier to adapt

If ergonomic schema helpers are needed later, add them as an optional companion crate.

## Streaming tool results

V1 should not require streaming tool outputs.

However, the design should leave room for it later through:

- `supports_streaming_hint`
- future `ToolEvent` or streamed result extensions

For now:

- tools are request/response
- the loop handles tool execution as an atomic step from the model's perspective

That is enough for the first version.

## Suggested public API shape

Recommended first-pass types:

```rust
pub struct ToolSpec { /* ... */ }
pub struct ToolRequest { /* ... */ }
pub struct ToolResult { /* ... */ }
pub struct ToolRegistry { /* ... */ }
pub struct ToolContext<'a> { /* ... */ }

pub trait Tool { /* ... */ }
pub trait PreflightTool: Tool { /* ... */ }
pub trait ToolExecutor { /* ... */ }
pub trait PermissionChecker { /* ... */ }

pub enum ToolExecutionOutcome { /* ... */ }
pub enum ProposedToolAction { /* ... */ }
pub enum PermissionDecision { /* ... */ }
pub enum ToolInterruption { /* ... */ }
```

This is enough to support:

- custom tools
- built-in tools
- shared permission checks
- approval interrupts
- MCP-backed tools

## Suggested feature flags

At the crate level:

- `std`
- `executor`
- `permissions`

The base trait/types should stay as small as possible.

If executor plumbing becomes too opinionated, it can be split into a sibling crate later. But for v1, keeping registry plus execution policy together is likely simpler.

## What does not belong here

These concerns should stay elsewhere:

- transcript item/content types: `agentkit-core`
- loop interrupts and turn lifecycle: `agentkit-loop`
- terminal/logging output: `agentkit-reporting`
- actual shell/process code: `agentkit-tool-shell`
- actual filesystem code: `agentkit-tool-fs`
- context loading: `agentkit-context`

The tool crate should be the narrow middle layer between orchestration and specific tool implementations.

## Suggested module layout

```text
agentkit-tools-core/
  src/
    lib.rs
    spec.rs
    tool.rs
    request.rs
    result.rs
    registry.rs
    executor.rs
    context.rs
    permissions.rs
    action.rs
    interrupt.rs
    error.rs
```

Module intent:

- `spec.rs`: `ToolSpec`, `ToolName`, annotations
- `tool.rs`: `Tool`, `PreflightTool`
- `request.rs`: `ToolRequest`
- `result.rs`: execution result wrapper
- `registry.rs`: registration and lookup
- `executor.rs`: execution orchestration
- `context.rs`: `ToolContext` and host resources
- `permissions.rs`: permission traits and decisions
- `action.rs`: `ProposedToolAction` and built-in action types
- `interrupt.rs`: `ToolInterruption`, auth/approval requests
- `error.rs`: tool-layer errors

## What we should validate early

Before locking the tools API, prove:

1. a minimal custom tool can be implemented with very little boilerplate
2. shell and filesystem tools can describe preflight actions before side effects
3. permission checks can produce allow, deny, and approval-required outcomes cleanly
4. MCP-backed tools can be surfaced through the same registry and executor path
5. tool results can carry structured output without collapsing to plain text
6. the loop can pause and resume around a tool interruption without special-casing one tool type

If any of those are awkward, the boundary is probably still wrong.
