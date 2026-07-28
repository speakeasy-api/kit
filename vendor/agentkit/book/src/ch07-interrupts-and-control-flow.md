# Interrupts and control flow

The loop runs autonomously until it hits a yield point that requires or invites host action. Some yields are _blocking_ (the loop genuinely cannot proceed without an answer), others are _cooperative_ (the host may interject but can also ignore the yield and call `next()` again to resume). Both are surfaced through the same `LoopStep::Interrupt` channel. This chapter covers each variant, the blocking/cooperative distinction, and how hosts resolve or pass through them.

## The interrupt model

```rust
pub enum LoopStep {
    Interrupt(LoopInterrupt),
    Finished(TurnResult),
}

pub enum LoopInterrupt {
    ApprovalRequest(PendingApproval),
    AwaitingInput(InputRequest),
    AfterToolResult(ToolRoundInfo),
}

impl LoopInterrupt {
    /// `true` for variants that must be resolved before the loop can
    /// make progress: `ApprovalRequest`.  `false` for cooperative
    /// yields the host may ignore: `AwaitingInput`, `AfterToolResult`.
    pub fn is_blocking(&self) -> bool { /* ... */ }
}
```

Each variant represents a different reason control returned to the host. `ApprovalRequest` and `AwaitingInput` carry handle types (`PendingApproval`, `InputRequest`) with ergonomic resolution methods. `AfterToolResult` carries a `ToolRoundInfo` snapshot (`session_id`, `turn_id`, `transcript_len`) and exposes the same `submit(...)` shape as `InputRequest`, so the host can interject a user message without cancelling the turn — or just call `next()` again to continue.

> **MCP auth is not an interrupt.** Earlier versions of agentkit included a fourth `AuthRequest(PendingAuth)` variant. Auth is now handled outside the loop: tool adapters return `ToolError::AuthRequired(AuthRequest)` and the host completes the flow via [`McpServerManager::resolve_auth`](./ch17-mcp.md). The next tool call reconnects with the resolved credentials. Keeping auth out of the loop's interrupt set keeps the state machine small and lets non-MCP hosts ignore the concept entirely.

```text
Loop autonomy boundary:

  ┌──────────────────────────────────────────────────────┐
  │                Autonomous zone                       │
  │                                                      │
  │   model turn → stream deltas → collect tool calls    │
  │   permission check → tool execution → append result  │
  │   compaction → next model turn → ...                 │
  │                                                      │
  │   The loop runs here without host involvement.       │
  └──────────────────────────┬───────────────────────────┘
                             │
                    yield point (interrupt)
                             │
  ┌──────────────────────────▼───────────────────────────┐
  │                Host decision zone                    │
  │                                                      │
  │   blocking:    "Approve this shell command?"         │
  │   cooperative: "Type your next message"              │
  │   cooperative: "Tool round done — interject?"        │
  │                                                      │
  │   The host handles this, then calls next() again.    │
  └──────────────────────────────────────────────────────┘
```

## Approval interrupts

When a tool's permission policy returns `RequireApproval`, the loop pauses and surfaces the request:

```rust
pub struct ApprovalRequest {
    pub task_id: Option<TaskId>,
    pub call_id: Option<ToolCallId>,
    pub id: ApprovalId,
    pub request_kind: String,      // e.g. "filesystem.write", "shell.command"
    pub reason: ApprovalReason,
    pub summary: String,
    pub metadata: MetadataMap,
}
```

The `reason` field tells the host _why_ approval is needed:

```rust
pub enum ApprovalReason {
    PolicyRequiresConfirmation,   // Policy always requires approval for this kind
    EscalatedRisk,                // Operation flagged as higher risk than usual
    UnknownTarget,                // Target not recognised by any policy
    SensitivePath,                // Filesystem path outside the allowed set
    SensitiveCommand,             // Shell command not in the allow-list
    SensitiveServer,              // MCP server not in the trusted set
    SensitiveAuthScope,           // MCP auth scope not pre-approved
}
```

The host resolves using the `PendingApproval` handle:

```rust
match driver.next().await? {
    LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
        println!("Tool needs approval: {}", pending.request.summary);

        // Option 1: approve
        pending.approve(&mut driver)?;

        // Option 2: deny
        pending.deny(&mut driver)?;

        // Option 3: deny with reason (fed back to model)
        pending.deny_with_reason(&mut driver, "User declined")?;
    }
    ...
}
```

After resolution, the host calls `next()` again. If approved, the tool executes and the turn continues. If denied, the denial is reported back to the model as a tool error — the model sees the denial reason and can adjust its approach.

### The approval flow in detail

```text
1. Model emits ToolCall(fs_replace_in_file, { path: "/etc/hosts", ... })
                          │
2. Executor runs permission preflight
   └── PathPolicy: /etc/hosts is outside workspace → RequireApproval(SensitivePath)
                          │
3. Driver emits AgentEvent::ApprovalRequired to observers (for UI/logging)
                          │
4. Driver returns LoopStep::Interrupt(ApprovalRequest(PendingApproval { ... }))
                          │
                   ─── host decision ───
                          │
5a. host calls pending.approve(driver)
    └── tool executes → result appended → loop resumes
                    OR
5b. host calls pending.deny(driver)
    └── denial sent to model as ToolResultPart { is_error: true, output: "Permission denied: ..." }
    └── model sees the error and may try a different approach
```

### Multiple pending approvals

When the model requests several tool calls in a single turn, some may require approval while others don't. The driver surfaces one approval at a time, in the order the model emitted them:

```text
Model response: [ToolCall("fs.write", ...), ToolCall("shell_exec", ...), ToolCall("fs.read", ...)]

Permission check:
  fs.write     → RequireApproval (outside workspace)
  shell_exec   → RequireApproval (unknown command)
  fs.read      → Allow

next() → Interrupt(ApprovalRequest for fs.write)
  host approves
next() → Interrupt(ApprovalRequest for shell_exec)
  host denies
next() → tools execute (fs.write runs, shell_exec denied, fs.read runs)
       → results appended, loop continues
```

The driver tracks pending approvals in a `BTreeMap<ToolCallId, PendingApprovalToolCall>` with a `VecDeque` for ordering. Each approval is surfaced individually, but they belong to the same tool round — the driver only starts tool execution once all pending approvals are resolved.

### Why interrupts, not callbacks

An alternative design would pass a callback or channel into the tool executor. agentkit uses interrupts instead because:

1. **Explicit control flow** — the host's main loop always knows what state the driver is in. There's no hidden state machine running in the background.
2. **No hidden concurrency** — approval doesn't happen on a background thread while the loop keeps running. The loop is genuinely paused.
3. **Testability** — interrupt-based flows are easy to test: submit input, call `next()`, assert you get the expected interrupt, resolve it, call `next()` again. No mocking of async channels.
4. **Serializable state** — an interrupted driver can be snapshotted and resumed later, because the interrupt carries all state needed for resolution.

```text
Callback model (rejected):

  loop calls tool → tool calls approval_callback → callback calls host code
  └── Who owns the stack? Can the host do async work? What if the host panics?
      What if multiple tools need approval concurrently?

Interrupt model (adopted):

  loop calls tool → tool needs approval → loop returns Interrupt to host
  └── Host owns the stack. Host does whatever it needs. Calls next() when ready.
```

## Auth — handled outside the loop

MCP servers and external tools may require authentication. agentkit does **not** model auth as a loop interrupt. The flow:

1. A tool call surfaces `ToolError::AuthRequired(AuthRequest)`. The driver writes the failure to the transcript as a tool error and continues — the model sees that the call could not be completed.
2. The host (which owns the `McpServerManager`) reads the `AuthRequest` either from the tool error metadata or from `manager.resolve_auth_and_resume(...)`-style entry points, runs whatever auth flow it needs (OAuth, API key prompt, secret-store fetch), and submits the resolution.
3. Subsequent calls reconnect with the new credentials transparently.

```rust
manager
    .resolve_auth(AuthResolution::provided(request, credentials))
    .await?;
```

This keeps the loop state machine to three interrupts and lets hosts that don't use MCP ignore auth entirely. See [Chapter 17](./ch17-mcp.md) for the manager-side surface (`AuthRequest`, `AuthOperation`, `AuthResolution`, `McpAuthResponder`).

## Input interrupts

When the model finishes a turn and the loop has no pending input, it returns `AwaitingInput`:

```rust
pub struct InputRequest {
    pub session_id: SessionId,
    pub reason: String,
}
```

The host reads the next user message and submits it:

```rust
LoopStep::Interrupt(LoopInterrupt::AwaitingInput(pending)) => {
    let user_message = read_line()?;
    pending.submit(&mut driver, vec![user_item(user_message)])?;
}
```

This is the most common interrupt in an interactive session. The pattern is: model finishes → host gets `AwaitingInput` → host reads user input → host calls `submit` → host calls `next()` → loop runs another turn.

## After-tool-result yields

A single user message can drive many tool rounds before the model produces a final reply. Between each round — after every tool call in the previous assistant message has a matching tool result in the transcript, and before the driver invokes the model again — the loop yields control to the host:

```rust
pub struct ToolRoundInfo {
    pub session_id:     SessionId,
    pub turn_id:        TurnId,
    pub transcript_len: usize,
}
```

Unlike approval, this yield requires no resolution. The host has three choices:

```rust
LoopStep::Interrupt(LoopInterrupt::AfterToolResult(info)) => {
    // 1. Ignore: just loop back to next().  The turn resumes with the
    //    existing transcript into the next model call.
    // 2. Interject: submit a user message that the next model call will
    //    see as part of the transcript.
    info.submit(&mut driver, vec![Item::text(ItemKind::User, "also: be concise")])?;
    // 3. Cancel: call cancellation.interrupt() and then drain the turn.
}
```

The invariant maintained at this point is that the transcript ends in a valid `[…, assistant(tool_call…), tool_result(…)]` sequence — adding a user message here produces `[…, tool_call, tool_result, user]` which every major provider accepts as the prompt for the next assistant response.

### Why yield if resolution is optional?

Interactive agents frequently need to let the user type ahead during a slow turn ("wait, also be concise", "actually, skip the benchmarks"). Without a yield point, the only ways to interject are:

- cancel the whole turn and restart with the combined message — loses progress and burns tokens;
- inject into the next `TurnRequest.transcript` via a `ModelAdapter` wrapper — works, but requires a parallel buffer and post-turn reconciliation with the driver's own transcript;
- hold the driver under a mutex and submit input from another task — violates the `&mut self` invariant and requires `unsafe` or a lock wrapper.

`AfterToolResult` solves this natively: the driver is not mid-`next()` at the yield, so `info.submit(&mut driver, items)` is callable in the normal way, and the transcript the model sees is always the single canonical one owned by the driver. The handle is consumed when used, so the same yield cannot accept input twice.

### Hosts that don't care about interjection

Non-interactive callers (batch jobs, tests, subagents) match the arm with `continue`:

```rust
loop {
    match driver.next().await? {
        LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
        LoopStep::Finished(result) => break handle_result(result),
        // …blocking interrupts handled as usual
    }
}
```

## Interrupt ordering and state safety

The driver enforces strict state transitions:

```text
Valid transitions:

  Agent::builder()
    .transcript(prior)        // optional, default empty
    .input(opening_turn)      // optional, default empty
    .build()?
    .start(cfg)         ──▶ next() ──▶ Interrupt(Awaiting)        ──▶ req.submit(driver, first_input)   ──▶ next()
                                                  ──▶ Finished
                                                  ──▶ Interrupt(Approval)        ──▶ pending.approve/deny()  ──▶ next()
                                                  ──▶ Interrupt(AfterToolResult) ──▶ [info.submit(driver, …)] ──▶ next()
                                                                                    (submit is optional)

Invalid (state errors):

  next() while an approval is pending                    → LoopError::InvalidState
  resolve_approval() with no pending approval            → LoopError::InvalidState
  resolve_approval() for a ToolCallId that doesn't exist → LoopError::InvalidState
```

Blocking interrupts (`ApprovalRequest`) must be resolved before `next()` can run again; calling `next()` with an unresolved approval returns `LoopError::InvalidState`. Cooperative interrupts (`AwaitingInput`, `AfterToolResult`) impose no such constraint — the host calls `next()` when ready, with or without an intervening `submit`. These constraints prevent subtle bugs where the host accidentally skips or duplicates a resolution that actually matters.

## The event/interrupt duality

Some actions are reported both as non-blocking observations _and_ as blocking interrupts:

| Observer receives                   | Host receives                                   |
| ----------------------------------- | ----------------------------------------------- |
| `AgentEvent::ApprovalRequired(req)` | `LoopStep::Interrupt(ApprovalRequest(pending))` |
| `AgentEvent::TurnFinished(result)`  | `LoopStep::Finished(result)`                    |

This duplication is intentional. The event is for observability — a reporter logs it, a UI updates a status indicator. The interrupt is for control flow — the host must answer it before the loop can continue. These are different concerns served by different mechanisms.

A reporter that displays "Waiting for approval..." needs the event. The host code that prompts the user needs the interrupt. Neither should have to reach into the other's channel.

## Practical patterns

### Auto-approve by policy

If your permission policy already knows which operations are safe, it returns `Allow` instead of `RequireApproval`. The loop never interrupts for those operations. Configure your policies conservatively and expand allowlists as you build confidence.

### Session-scoped approvals

A host can maintain a session-local allowlist. When the user approves a command like `cargo build`, add it to the allowlist. On subsequent approval interrupts, check the allowlist before prompting:

```rust
LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
    if session_allowlist.contains(&pending.request.request_kind) {
        pending.approve(&mut driver)?;
    } else {
        let decision = prompt_user(&pending.request)?;
        if decision == "always" {
            session_allowlist.insert(pending.request.request_kind.clone());
        }
        // resolve based on decision
    }
}
```

### Headless operation

For non-interactive agents (CI, background jobs), either configure permissive policies or auto-approve everything:

```rust
LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
    pending.approve(&mut driver)?;
}
```

The approval system still runs — it's just that the policy answers "yes" to everything. The events are still emitted, so audit logging captures every approved operation.

> **Example:** [`openrouter-coding-agent`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-coding-agent) handles approval interrupts for filesystem writes in its main loop.
>
> **Crate:** [`agentkit-loop`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-loop) — `LoopStep`, `LoopInterrupt`, `PendingApproval`, `InputRequest`, `ToolRoundInfo`. Approval types come from [`agentkit-tools-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tools-core); MCP `AuthRequest` / `AuthResolution` live in [`agentkit-mcp`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-mcp).
