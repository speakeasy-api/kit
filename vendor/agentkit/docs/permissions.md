# agentkit permissions design

## Purpose

Permissions are a shared policy layer for anything the agent may try to do that has safety implications.

In `agentkit`, that primarily means:

- local shell execution
- filesystem operations
- MCP server connection and invocation
- custom tools with side effects

The permissions design should answer three questions consistently:

1. Is this action allowed?
2. If not automatically allowed, does it need host approval?
3. If denied or interrupted, how is that surfaced back to the loop and the host?

This policy layer should be reusable across tool types instead of reimplemented separately by each tool crate.

## Non-goals

The permissions layer should not:

- implement sandboxing or isolation
- enforce OS-level security guarantees by itself
- own host UX for approval prompts
- encode provider-specific rules
- duplicate tool execution logic

It is a policy and decision layer, not a security boundary on its own.

That distinction matters. A deny rule in `agentkit` is useful, but it is not equivalent to a hardened sandbox.

## Design principles

### 1. Permissions are request-based

Policy should be evaluated against a normalized description of what is being proposed, not against the implementation details of a specific tool.

For built-in capabilities, `agentkit` can provide standard request types.
For custom tools, hosts should be able to define first-class custom permission request types.

This makes policy portable across:

- built-in tools
- MCP-backed tools
- custom tools

### 2. The default outcome model is ternary

A permission check should not collapse to just allow/deny.

The correct base outcomes are:

- allow
- deny
- require approval

That matches real agent UX much better than a simple boolean.

### 3. Policy is host-owned

`agentkit` should define the interfaces and default helpers.

The host application should decide:

- default-deny versus default-allow posture
- path allowlists/denylists
- shell command rules
- MCP server trust
- whether approvals are enabled

### 4. Policy evaluation should be side-effect free

Permission checks should happen before execution whenever possible.

That means tools should expose preflight actions and the executor should evaluate them before performing side effects.

This is especially important for:

- shell
- filesystem writes/deletes
- MCP auth scope use

### 5. Denials and approvals are first-class outcomes

Permission denials should be structured and reportable.

Approval requirements should be structured interruptions that the loop can surface to the host.

Neither should be smuggled through generic string errors.

## Main boundary

The clean separation is:

- `agentkit-tools-core` defines tool execution contracts and permission preflight hooks
- the permissions layer defines how proposed requests are evaluated
- the loop translates approval-requiring outcomes into blocking loop interrupts
- the host resolves approvals and configures policy

So permissions live logically between tool preflight and actual execution.

## Core model

## 1. Permission requests

This is the core input to policy.

The right v1 design is a hybrid:

```rust
pub trait PermissionRequest: Send + Sync {
    fn kind(&self) -> &'static str;
    fn summary(&self) -> String;
    fn metadata(&self) -> &MetadataMap;
    fn as_any(&self) -> &dyn Any;
}
```

And `agentkit` should provide built-in request types such as:

```rust
pub enum StandardPermissionRequest {
    Shell(ShellPermissionRequest),
    FileSystem(FileSystemPermissionRequest),
    Mcp(McpPermissionRequest),
}
```

Recommended rule:

- the permission input should be trait-based
- built-in request families should still ship as standard concrete types
- custom tools may define their own request types implementing the trait directly

This gives custom tools first-class permission semantics without giving up a good built-in story.

## 2. Decisions

Recommended base decision type:

```rust
pub enum PermissionDecision {
    Allow,
    Deny(PermissionDenial),
    RequireApproval(ApprovalRequest),
}
```

Where:

```rust
pub struct PermissionDenial {
    pub code: PermissionCode,
    pub message: String,
    pub metadata: MetadataMap,
}
```

`PermissionCode` should be normalized and machine-usable.

Suggested categories:

- `PathNotAllowed`
- `CommandNotAllowed`
- `NetworkNotAllowed`
- `ServerNotTrusted`
- `AuthScopeNotAllowed`
- `CustomPolicyDenied`
- `UnknownAction`

This lets the host and reporters reason about denials without parsing strings.

## 3. Approval requests

Approvals should reuse the loop’s blocking interrupt system, but the permission layer should define the information needed to ask for approval.

Recommended shape:

```rust
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub request_kind: String,
    pub reason: ApprovalReason,
    pub summary: String,
    pub metadata: MetadataMap,
}
```

Recommended `ApprovalReason` categories:

- `PolicyRequiresConfirmation`
- `EscalatedRisk`
- `UnknownTarget`
- `SensitivePath`
- `SensitiveCommand`
- `SensitiveServer`
- `SensitiveAuthScope`

The goal is to provide enough structure for hosts to render useful approval UX without locking `agentkit` into one UI model.

## 4. Approval resolution

The host’s reply should be explicit.

Recommended shape:

```rust
pub enum ApprovalDecision {
    Approve,
    Deny { reason: Option<String> },
}
```

This is intentionally simple for v1.

The important part is that:

- policy can require approval
- the host decides
- the loop resumes with that answer

## Permission checker

The base trait should be small and deterministic.

Recommended shape:

```rust
pub trait PermissionChecker: Send + Sync {
    fn evaluate(&self, request: &dyn PermissionRequest) -> PermissionDecision;
}
```

This should stay synchronous in v1.

Reason:

- simple policy checks are local and cheap
- keeps the executor straightforward
- avoids dragging approval or external state into policy evaluation itself

If a host wants external policy engines later, that can be added as an adapter, but the base contract should remain simple.

## Policy composition

Hosts will want layered rules.

Examples:

- global default-deny
- allow reads under workspace root
- require approval for writes outside workspace root
- deny dangerous shell commands entirely
- allow certain MCP servers

So the permission system should support composition.

Recommended primitive:

```rust
pub trait PermissionPolicy {
    fn evaluate(&self, request: &dyn PermissionRequest) -> PolicyMatch;
}
```

With an aggregator:

```rust
pub struct CompositePermissionChecker {
    policies: Vec<Box<dyn PermissionPolicy>>,
    fallback: PermissionDecision,
}
```

Where `PolicyMatch` can distinguish:

- no opinion
- allow
- deny
- require approval

This gives a clean way to build layered policies without hardcoding one rule engine.

## Policy precedence

The permission layer needs deterministic precedence.

Recommended v1 order:

1. explicit deny wins
2. explicit require-approval wins over allow
3. explicit allow wins over no-opinion
4. fallback decision applies if no policy matches

This is simple and matches common safety expectations.

If a host wants something more complex later, that should be a deliberate extension, not the default.

## Built-in policy helpers

`agentkit` should probably provide helper policy implementations, even if hosts can always write their own.

Recommended v1 helpers:

- `PathPolicy`
- `CommandPolicy`
- `McpServerPolicy`
- `CustomKindPolicy`

### `PathPolicy`

Purpose:

- allow, deny, or require approval for filesystem paths

Typical capabilities:

- workspace root allowlist
- protected path denylist
- read-only subtrees
- approval outside trusted roots

### `CommandPolicy`

Purpose:

- allow, deny, or require approval for shell actions

Typical capabilities:

- executable allowlist/denylist
- cwd restrictions
- env var restrictions
- approval for destructive commands

### `McpServerPolicy`

Purpose:

- govern MCP server connection and usage

Typical capabilities:

- trusted server allowlist
- auth-scope restrictions
- prompt/resource access restrictions
- approval for remote or unknown servers

### `CustomKindPolicy`

Purpose:

- handle `ProposedToolAction::Custom`

Typical capabilities:

- allow known custom action kinds
- deny unknown kinds
- require approval for broad categories

## Structured action types

The built-in request families should be rich enough for policy without forcing tools to leak implementation details.

## `ShellPermissionRequest`

Recommended fields:

- executable
- argv
- cwd
- env keys used
- timeout hint

Important note:

- policy usually does not need the full environment values
- env key names are often enough for policy decisions

## `FileSystemPermissionRequest`

Recommended variants:

- `Read { path }`
- `Write { path }`
- `Edit { path }`
- `Delete { path }`
- `Move { from, to }`
- `List { path }`
- `CreateDir { path }`

This should be enough to express common coding-agent filesystem behavior clearly.

## `McpPermissionRequest`

Recommended variants:

- `Connect { server_id }`
- `InvokeTool { server_id, tool_name }`
- `ReadResource { server_id, resource_id }`
- `FetchPrompt { server_id, prompt_id }`
- `UseAuthScope { server_id, scope }`

This matches the MCP surface designed in [mcp.md](./mcp.md).

## Custom permission requests

Custom tools should be able to define custom request types directly.

Example:

```rust
pub struct DeployPermissionRequest {
    pub environment: String,
    pub service: String,
    pub metadata: MetadataMap,
}

impl PermissionRequest for DeployPermissionRequest {
    fn kind(&self) -> &'static str { "myapp.deploy" }
    fn summary(&self) -> String { format!("Deploy {} to {}", self.service, self.environment) }
    fn metadata(&self) -> &MetadataMap { &self.metadata }
    fn as_any(&self) -> &dyn Any { self }
}
```

This is what makes custom tools feel first-class:

- they do not need to squeeze themselves into `Custom { kind, payload }`
- host policies can reason about their native fields directly
- built-in and custom permissions share the same checker interface

The tradeoff is that generic policies cannot automatically understand every custom request type.

That is acceptable.

The intended layering is:

- generic policies operate on `kind()` and metadata
- specialized host policies may downcast through `as_any()` for richer handling

## Approval policy versus hard deny

One important host-level choice is when to require approval versus deny outright.

Recommended heuristic:

- deny when an action is categorically disallowed
- require approval when an action is risky but may be legitimate

Examples:

- `rm -rf /` -> deny
- write outside workspace root -> require approval
- connect to unknown remote MCP server -> require approval
- read a secret system file -> deny

This distinction should be reflected in helper policies and examples.

## Integration with tool execution

The ideal flow is:

1. tool receives request
2. tool exposes one or more preflight permission requests
3. executor evaluates each request through the permission checker
4. if any request is denied, execution stops with a structured denial
5. if any request requires approval, execution stops with a structured interruption
6. otherwise the tool executes

This is why permissions belong conceptually in the tool execution layer, not buried inside individual tools.

## Multiple actions per tool call

Some tool calls imply more than one permission-relevant request.

Examples:

- a shell tool may execute a command in a specific cwd with certain env keys
- a complex filesystem tool may move then delete
- an MCP tool may require both server access and an auth scope

Recommended v1 policy:

- evaluate all proposed requests before side effects
- if any deny, deny the whole call
- else if any require approval, interrupt the whole call
- else allow the call

This keeps the model simple and conservative.

## Approval caching

Hosts may eventually want “approve once” or “approve for this session” semantics.

V1 should not build a complicated cache into the core permission model.

Instead:

- hosts may implement approval caching inside their `PermissionChecker`
- or by wrapping the checker with a memoizing adapter

That keeps the base model simple.

## Auditability

The permission system should be easy to observe.

At minimum, reporting should be able to see:

- which request was proposed
- which decision was made
- whether approval was required
- whether the host approved or denied

This suggests that loop/tool events should include permission-related event variants or metadata.

Examples:

- `PermissionEvaluated`
- `ApprovalRequired`
- `ApprovalResolved`
- `PermissionDenied`

The exact event placement belongs to the loop/reporting layers, but the permissions design should require that these outcomes are observable.

## Suggested public API shape

Recommended first-pass types:

```rust
pub trait PermissionChecker { /* ... */ }
pub trait PermissionPolicy { /* ... */ }
pub trait PermissionRequest { /* ... */ }

pub enum PermissionDecision { /* ... */ }
pub enum PolicyMatch { /* ... */ }
pub struct PermissionDenial { /* ... */ }
pub struct ApprovalRequest { /* ... */ }
pub enum ApprovalDecision { /* ... */ }

pub struct CompositePermissionChecker { /* ... */ }
pub struct PathPolicy { /* ... */ }
pub struct CommandPolicy { /* ... */ }
pub struct McpServerPolicy { /* ... */ }
pub struct CustomKindPolicy { /* ... */ }
```

And built-in request families:

```rust
pub enum StandardPermissionRequest { /* ... */ }
pub struct ShellPermissionRequest { /* ... */ }
pub struct FileSystemPermissionRequest { /* ... */ }
pub struct McpPermissionRequest { /* ... */ }
```

## Where this should live

For v1, these abstractions can live in `agentkit-tools-core`.

That is consistent with the current tools design, since permissions are primarily used by the tool executor path.

If the policy layer grows beyond tool execution later, it may justify its own crate such as `agentkit-policy`.

For now, splitting it out would likely add more ceremony than value.

## What does not belong here

These concerns should stay elsewhere:

- UI rendering for approvals
- transcript/reporting implementations
- shell/file/MCP execution code
- host identity/session management beyond what policy needs
- OS sandboxing

The permission layer should stay focused on policy decisions and structured outcomes.

## What we should validate early

Before locking the permissions API, prove:

1. shell, filesystem, and MCP requests can all be expressed cleanly as built-in permission request types
2. simple host policies are easy to write
3. layered policies compose without surprising precedence
4. approval-required outcomes resume cleanly through the loop
5. denials are rich enough for good reporting without becoming overcomplicated
6. custom tools can participate with native custom request types and without awkward enum wrapping

If any of those fail, the action model or the policy composition design is still wrong.
