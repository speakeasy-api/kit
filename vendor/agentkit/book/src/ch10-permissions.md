# Permissions, approvals, and auth

Safety is the hardest problem in agent frameworks. An agent with shell access can delete your home directory. An agent with network access can exfiltrate data. The permission system is how you prevent this without making the agent useless.

This chapter covers the permission model, policy composition, and approval flow.

## The ternary decision model

A permission check produces one of three outcomes:

```rust
pub enum PermissionDecision {
    Allow,
    Deny(PermissionDenial),
    RequireApproval(ApprovalRequest),
}
```

This is not a boolean. The third outcome — "this might be okay, but a human needs to confirm" — is essential for practical agent use. Categorically denying all writes makes the agent unable to code. Categorically allowing all writes makes it dangerous. Requiring approval for writes outside the workspace is the useful middle ground.

## Permission requests

Policy is evaluated against a description of the proposed action, not against tool implementation details:

```rust
pub trait PermissionRequest: Send + Sync {
    fn kind(&self) -> &'static str;
    fn summary(&self) -> String;
    fn metadata(&self) -> &MetadataMap;
    fn as_any(&self) -> &dyn Any;
}
```

Built-in request types cover common scenarios:

- `ShellPermissionRequest` — executable, argv, cwd, env keys, timeout
- `FileSystemPermissionRequest` — Read, Write, Edit, Delete, Move, List, CreateDir
- `McpPermissionRequest` — Connect, InvokeTool, ReadResource, FetchPrompt, UseAuthScope

Custom tools can define their own request types by implementing the trait directly. This makes custom tools first-class — they don't have to squeeze into a generic `Custom { kind, payload }` variant.

## The permission checker

```rust
pub trait PermissionChecker: Send + Sync {
    fn evaluate(&self, request: &dyn PermissionRequest) -> PermissionDecision;
}
```

This is synchronous by design. Permission checks should be local and cheap. If a host needs external policy engines, they can build an adapter, but the base contract stays simple.

## Policy composition

Real hosts need layered rules. A single monolithic checker doesn't scale — you want separate rules for paths, commands, MCP servers, and custom actions, each maintained independently.

```rust
pub struct CompositePermissionChecker {
    policies: Vec<Box<dyn PermissionPolicy>>,
    fallback: PermissionDecision,
}
```

Each policy returns `PolicyMatch` — note the fourth option that `PermissionDecision` doesn't have:

```rust
pub enum PolicyMatch {
    NoOpinion,                    // "I don't handle this kind of request"
    Allow,
    Deny(PermissionDenial),
    RequireApproval(ApprovalRequest),
}
```

`NoOpinion` is what makes composition work. A `PathPolicy` returns `NoOpinion` for shell commands because it only understands filesystem paths. A `CommandPolicy` returns `NoOpinion` for filesystem operations. Each policy handles its domain and defers on everything else.

The evaluation algorithm:

```text
for each policy in registration order:
  match policy.evaluate(request):
    NoOpinion         → continue to next policy
    Allow             → record "saw allow", continue
    Deny(reason)      → STOP, return Deny immediately
    RequireApproval   → record it, continue

after all policies:
  if any Deny was seen     → return Deny         (already returned above)
  if any RequireApproval   → return RequireApproval
  if any Allow             → return Allow
  otherwise                → return fallback
```

Precedence rules:

1. **Explicit deny wins** — a single `Deny` short-circuits immediately
2. **Require-approval wins over allow** — if any policy says "ask the user", the user is asked
3. **Allow wins over no-opinion** — at least one policy must explicitly allow
4. **Fallback applies if no policy matches** — configurable (typically `Deny`)

### Built-in policies

- **`PathPolicy`** — workspace root allowlists, protected path denylists, read-only subtrees
- **`CommandPolicy`** — executable allowlists/denylists, cwd restrictions, env var restrictions
- **`McpServerPolicy`** — trusted server allowlists, auth-scope restrictions
- **`CustomKindPolicy`** — handles custom tool action kinds

### Composing policies — a practical example

```rust
let checker = CompositePermissionChecker::new(PermissionDecision::Deny(default_denial()))
    .with_policy(PathPolicy::new()
        .allow_root("/workspace")
        .read_only_root("/workspace/vendor")
        .protect_root("/workspace/.env")
        .protect_root("/workspace/secrets/"))
    .with_policy(CommandPolicy::new()
        .allow_executable("git")
        .allow_executable("cargo")
        .allow_executable("rustc")
        .deny_executable("rm")
        .require_approval_for_unknown(true))
    .with_policy(McpServerPolicy::new()
        .allow_server("github"));
```

Trace through some requests with this configuration:

```text
Request: FileSystem::Read("/workspace/src/main.rs")
  PathPolicy:    /workspace is allowed root → Allow
  CommandPolicy: NoOpinion (not a shell request)
  McpPolicy:     NoOpinion (not an MCP request)
  Result: Allow ✓

Request: FileSystem::Write("/workspace/.env")
  PathPolicy:    /workspace/.env is denied → Deny
  Result: Deny ✗ (short-circuit)

Request: FileSystem::Edit("/workspace/vendor/lib.rs")
  PathPolicy:    /workspace/vendor is read-only → Deny
  Result: Deny ✗ (short-circuit)

Request: Shell("curl", ["https://evil.com"])
  PathPolicy:    NoOpinion (not a filesystem request)
  CommandPolicy: "curl" is unknown, require_approval_for_unknown → RequireApproval
  McpPolicy:     NoOpinion
  Result: RequireApproval ⚠

Request: Shell("rm", ["-rf", "/"])
  PathPolicy:    NoOpinion
  CommandPolicy: "rm" is denied → Deny
  Result: Deny ✗ (short-circuit)

Request: Custom("deploy", {...})
  PathPolicy:    NoOpinion
  CommandPolicy: NoOpinion
  McpPolicy:     NoOpinion
  No policy matched → fallback: Deny ✗
```

## Execution integration

The permission flow integrates with tool execution:

1. Tool receives a request
2. Tool exposes preflight `PermissionRequest` values
3. Executor evaluates each request through the permission checker
4. If any are denied → execution stops with a structured denial
5. If any require approval → execution stops with an interrupt
6. Otherwise → the tool executes

Multiple actions per tool call are evaluated together: if any deny, the whole call is denied. If any require approval, the whole call is interrupted. This is conservative by design.

## Approval vs denial

The distinction matters:

- **Deny** when an action is categorically disallowed: `rm -rf /`, reading `/etc/shadow`
- **Require approval** when an action is risky but may be legitimate: writing outside the workspace, connecting to an unknown MCP server

Hosts should set this calibration through their policy configuration, not through agentkit defaults.

## Custom permission requests

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
    // ...
}
```

Generic policies operate on `kind()` and metadata. Specialized host policies can downcast through `as_any()` for richer handling. This layering lets custom tools participate in the permission system without compromising on type safety.

> **Crate:** Permission types live in [`agentkit-tools-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tools-core). Built-in policies are in the same crate. Tool crates like [`agentkit-tool-fs`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tool-fs) and [`agentkit-tool-shell`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tool-shell) define their specific request types.
