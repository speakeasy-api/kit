# Driving the loop

This chapter walks through the `LoopDriver` — the runtime heart of the agent. We'll trace a complete turn from input submission through model invocation, tool execution, and final result.

## The driver API

The `LoopDriver` is generic over the model session type:

```rust
pub struct LoopDriver<S: ModelSession> {
    session_id: SessionId,
    session: Option<S>,
    tool_executor: Arc<dyn ToolExecutor>,
    task_manager: Arc<dyn TaskManager>,
    permissions: Arc<dyn PermissionChecker>,
    resources: Arc<dyn ToolResources>,
    cancellation: Option<CancellationHandle>,
    mutators: Vec<Arc<dyn LoopMutator>>,
    observers: Vec<Arc<dyn LoopObserver>>,
    transcript_observers: Vec<Arc<dyn TranscriptObserver>>,
    transcript: Vec<Item>,
    pending_input: Vec<Item>,
    pending_approvals: BTreeMap<ToolCallId, PendingApprovalToolCall>,
    active_tool_round: Option<ActiveToolRound>,
    next_turn_index: u64,
    /* … */
}
```

The public API is narrow:

```rust
impl<S: ModelSession> LoopDriver<S> {
    pub async fn next(&mut self) -> Result<LoopStep, LoopError>;
    pub fn resolve_approval_for(&mut self, call_id: ToolCallId, decision: ApprovalDecision)
        -> Result<(), LoopError>;
    pub fn set_next_turn_cache(&mut self, cache: PromptCacheRequest) -> Result<(), LoopError>;
    pub fn snapshot(&self) -> LoopSnapshot;
}
```

There is no `submit_input` on the driver. The prior transcript is preloaded via `AgentBuilder::transcript` as passive starting state, and an opening user turn for one-shot calls is preloaded via `AgentBuilder::input`. After that, every user turn is supplied through the `InputRequest` and `ToolRoundInfo` handles surfaced on cooperative interrupts. Funnelling every transcript mutation through the driver itself preserves the `&mut LoopDriver` invariant — no other task or thread can race with `next()`.

The host code is a simple loop. With nothing preloaded as input, the first call to `next()` yields `AwaitingInput`:

```rust
let agent = Agent::builder()
    .model(adapter)
    .transcript(vec![system_item])
    .build()?;

let mut driver = agent.start(session_config).await?;

loop {
    match driver.next().await? {
        LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => {
            req.submit(&mut driver, read_user_input()?)?;
        }
        LoopStep::Interrupt(interrupt) => handle_interrupt(interrupt),
        LoopStep::Finished(result) => break,
    }
}
```

### State machine semantics

`next()` is the only async method. It advances the driver through its internal state machine until it hits a yield point — either a finished turn or an interrupt. There is no polling, no callback registration, and no event queue to drain.

```text
Driver state machine:

       Agent::builder()
         .transcript(prior)        // passive, optional
         .input(opening_turn)      // optional one-shot opener
         .build()?
         .start(cfg)
                      │
                      ▼  (transcript & pending input baked in)
  ┌─────────────────────────────────┐
  │         Has pending input?      │
  │                                 │
  │  yes ──▶ merge into transcript  │
  │  no  ──▶ AwaitingInput         ─┼──▶ Interrupt (cooperative)
  └─────────────┬───────────────────┘     host: req.submit(...) or drop
                │
                ▼
  ┌─────────────────────────────────┐
  │      Model turn                 │
  │                                 │
  │  stream events from model       │
  │  collect tool calls             │
  │  emit AgentEvents to observers  │
  └─────────────┬───────────────────┘
                │
                ▼
  ┌─────────────────────────────────┐
  │      Tool calls present?        │
  │                                 │
  │  no  ──▶ Finished(TurnResult)  ─┼──▶ return
  │  yes ──▶ permission preflight   │
  └─────────────┬───────────────────┘
                │
                ▼
  ┌─────────────────────────────────┐
  │    Any require approval?        │
  │                                 │
  │  yes ──▶ ApprovalRequest       ─┼──▶ Interrupt (blocking)
  │  no  ──▶ execute tools          │
  └─────────────┬───────────────────┘
                │
                ▼
       append tool results
                │
                ▼
       run mutators at MutationPoint::AfterToolResult
                │
                ▼
  ┌─────────────────────────────────┐
  │   AfterToolResult              ─┼──▶ Interrupt (cooperative)
  │                                 │    host: info.submit(...) to
  │   host calls next() to resume  ◀┼─── interject, then next() to
  └─────────────┬───────────────────┘    resume into the next model turn
                │
                ▼
       go to "Model turn" ◀─── automatic tool roundtrip


When a turn ends (Finished, Interrupt, Cancelled), mutators run again at
MutationPoint::AfterTurnEnded before the next user turn begins.
```

The host cannot call `next()` twice without resolving an outstanding _blocking_ interrupt (`ApprovalRequest`) — that's a state error. Cooperative interrupts (`AwaitingInput`, `AfterToolResult`) require no resolution; calling `next()` again resumes the loop as described in the diagram. The driver forces the host to deal with blocking interrupts before proceeding so an approval request can never be silently skipped.

## Anatomy of a turn

Here's what happens inside `next()`, step by step:

### 1. Merge input

Pending items — submitted through an `InputRequest` / `ToolRoundInfo` handle — are appended to the working transcript. The driver emits `AgentEvent::InputAccepted` to observers. (The transcript handed to `Agent::start` is loaded passively at session creation and is not re-merged here.)

```text
Before:
  transcript: [System, Context, User("hello"), Assistant("Hi!")]
  pending:    [User("Read main.rs")]

After merge:
  transcript: [System, Context, User("hello"), Assistant("Hi!"), User("Read main.rs")]
  pending:    []
```

### 2. Construct TurnRequest

The loop builds a `TurnRequest` from the working transcript and tool registry:

```rust
TurnRequest {
    session_id: self.session_id.clone(),
    turn_id: TurnId::new(format!("turn-{}", self.next_turn_index)),
    transcript: self.transcript.clone(),
    available_tools: self.tool_executor.specs(),
    metadata: MetadataMap::new(),
    cache: self.next_turn_cache.take().or_else(|| self.default_cache.clone()),
}
```

### 3. Start model turn

`session.begin_turn(request, cancellation)` sends the transcript to the provider and returns a streaming turn handle.

### 4. Stream model output

The driver polls `turn.next_event()` in a loop:

```text
Loop:
  next_event() ──▶ Some(Delta(BeginPart))       ──▶ emit ContentDelta to observers
  next_event() ──▶ Some(Delta(AppendText))      ──▶ emit ContentDelta to observers
  next_event() ──▶ Some(Delta(CommitPart))      ──▶ emit ContentDelta to observers
  next_event() ──▶ Some(ToolCall(ToolCallPart)) ──▶ collect for execution
  next_event() ──▶ Some(Usage(Usage))           ──▶ emit UsageUpdated to observers
  next_event() ──▶ Some(Finished(result))       ──▶ break
```

### 5. Execute tools

If the model requested tool calls (indicated by `FinishReason::ToolCall`):

1. The driver constructs a `ToolRequest` for each `ToolCallPart`
2. Each request goes through the task manager for scheduling
3. The task manager routes each tool call (foreground, background, or foreground-then-detach)
4. The executor runs permission preflight on each tool
5. If any tool requires approval → the driver surfaces `LoopStep::Interrupt(ApprovalRequest)`
6. Otherwise → tools execute and results are appended to the transcript as `ToolResultPart`s

Auth challenges from MCP-backed tools are not loop interrupts. They surface as `ToolError::AuthRequired(AuthRequest)` from the tool, the driver records the failure on the transcript, and the host completes the auth flow out-of-band via [`McpServerManager::resolve_auth`](./ch17-mcp.md). The next tool call reconnects with the new credentials.

### 6. Run AfterToolResult mutators

After tool results are appended, the driver runs every registered `LoopMutator` at `MutationPoint::AfterToolResult`. Mutators decide for themselves whether to fire (compaction triggers, redaction rules, etc.). If any mutator dirtied the transcript, the loop validates protocol invariants (tool_use ↔ tool_result pairing); a violation is a hard `LoopError::Mutator` failure rather than letting the next request blow up at the provider.

### 7. Tool roundtrip

The driver yields `LoopStep::Interrupt(AfterToolResult(info))` before invoking the model again. The host has a chance to call `info.submit(&mut driver, items)?` to interject a user message at this boundary — the resulting transcript `[..., tool_call, tool_result, user]` is valid for the next model call. Calling `next()` again resumes the turn into the next model call (back to step 3). The model sees the tool results (and any injected message) and may request more tools or produce a final response.

### 8. Return result

When the model finishes without pending tool calls, the driver returns:

```rust
LoopStep::Finished(TurnResult {
    turn_id,
    finish_reason: FinishReason::Completed,
    items: /* assistant items from this turn */,
    usage: /* accumulated usage */,
    metadata: MetadataMap::new(),
})
```

### Multiple tool roundtrips per user turn

A single user message can trigger many tool roundtrips. Between each one the driver yields `AfterToolResult` back to the host:

```text
User: "Add error handling to src/parser.rs"

  Model call 1: ToolCall(fs_read_file)
                execute → result appended
  ──▶ next() returns Interrupt(AfterToolResult)
      (host may info.submit(...) or just call next())

  Model call 2: ToolCall(fs_replace_in_file)
                execute → result appended
  ──▶ next() returns Interrupt(AfterToolResult)

  Model call 3: ToolCall(shell_exec("cargo check"))
                execute → result appended
  ──▶ next() returns Interrupt(AfterToolResult)

  Model call 4: Text("I've added error handling...")
                no tool calls
  ──▶ next() returns Finished(TurnResult)

Host sees: four calls to next(), three cooperative yields, one Finished.
```

From the host's perspective, each tool round ends with a cooperative yield. Non-interactive callers match `AfterToolResult` with `continue` and see essentially one "turn" delivered as a final `TurnResult`; interactive callers can interject user input at each boundary without cancelling the turn. Either way, the model chains tool calls without the host having to mediate each call — only the round boundaries are exposed.

## Event delivery during a turn

While the driver processes a turn, non-blocking events are delivered to observers synchronously:

```rust
pub trait LoopObserver: Send + Sync {
    fn handle_event(&self, event: ObservedEvent);
}
```

`ObservedEvent` wraps the `AgentEvent` in a session-addressed envelope (`session_id` + `event`), so a single shared observer can route events from every session it watches. Observers take `&self` and store mutable state behind interior mutability (`Mutex`, atomics, channels). The driver shares each observer as `Arc<dyn LoopObserver>` so a single configured `Agent` can mint multiple sessions over its lifetime.

The full event taxonomy (`AgentEvent` is `#[non_exhaustive]` — keep a wildcard arm):

| Event                      | When it fires                                                                          |
| -------------------------- | -------------------------------------------------------------------------------------- |
| `RunStarted`               | `Agent::start()` completes                                                             |
| `TurnStarted`              | Before each model turn begins                                                          |
| `InputAccepted`            | The driver merges pending input into the transcript                                    |
| `ContentDelta(Delta)`      | Model streams a delta                                                                  |
| `ToolCallRequested`        | Model requests a tool call                                                             |
| `ToolExecutionStarted`     | A tool call is about to execute, after policy and approval checks                      |
| `ToolExecutionProgress`    | Non-terminal tool progress (e.g. background detachment placeholder)                    |
| `ToolResultReceived`       | A terminal tool result lands in the transcript (foreground or background)              |
| `ApprovalRequired`         | A tool requires approval                                                               |
| `ApprovalResolved`         | An approval interrupt is resolved                                                      |
| `ToolCatalogChanged`       | A federated tool source's catalog changed; the next request will see the new tool list |
| `MutationStarted`          | A `LoopMutator` is about to run at a `MutationPoint`                                   |
| `MutationFinished`         | A `LoopMutator` finished; `dirty` indicates whether the transcript was modified        |
| `UsageUpdated(Usage)`      | Token usage reported                                                                   |
| `Warning(String)`          | Non-fatal issue (recovered tool error, etc.)                                           |
| `RunFailed(String)`        | Unrecoverable error                                                                    |
| `TurnFinished(TurnResult)` | A turn completes                                                                       |

Observers are called inline, synchronously, in registration order. The loop task blocks briefly for each observer call. This is acceptable because observers should be fast — write to stderr, increment a counter, append to a buffer. Expensive processing should happen asynchronously behind a channel adapter.

For loss-free transcript reconstruction (persistence, replication, audit), the driver also fans out to a separate `TranscriptObserver` channel that fires once per `Item` appended — a session-addressed `TranscriptEvent` carrying the item, in transcript order. `LoopObserver` alone is not sufficient for this — content deltas span partial parts and historically tool results were appended without an event at all. Mutator-driven rewrites do **not** fire `on_transcript_event`; those are signaled by `AgentEvent::MutationFinished`. Register via `AgentBuilder::transcript_observer`.

## Building the agent

The `Agent` is built with a builder:

```rust
let agent = Agent::builder()
    .model(adapter)                          // required
    .add_tool_source(registry)               // optional; call again to federate
    .permissions(checker)                    // default: allow all
    .resources(resources)                    // default: ()
    .task_manager(manager)                   // default: SimpleTaskManager
    .cancellation(cancellation_handle)       // default: none
    .compaction(config)                      // default: none
    .observer(reporter)                      // default: none
    .transcript_observer(persistence)        // default: none
    .transcript(vec![system_item])           // default: empty
    .input(vec![first_user_turn])            // default: empty (one-shot opener)
    .build()?;

let mut driver = agent.start(session_config).await?;
```

The builder validates that a model adapter is set. Everything else has sensible defaults:

| Field                  | Default               | Effect                            |
| ---------------------- | --------------------- | --------------------------------- |
| `tool_sources`         | `[]`                  | Model can't call any tools        |
| `permissions`          | `AllowAllPermissions` | Every tool call is auto-approved  |
| `resources`            | `()`                  | No shared resources               |
| `task_manager`         | `SimpleTaskManager`   | Sequential, inline tool execution |
| `cancellation`         | `None`                | No cancellation support           |
| `compaction`           | `None`                | Transcript grows without bounds   |
| `observers`            | `[]`                  | No event reporting                |
| `transcript_observers` | `[]`                  | No transcript persistence hook    |

`Agent::start()` consumes the agent and returns a `LoopDriver` with the supplied transcript loaded passively. The first call to `next()` yields `AwaitingInput`; the host supplies the first user turn via `InputRequest::submit`, and the driver dispatches the model on the next `next()`. The agent's immutable configuration (adapter, tool sources, permissions) is moved into the driver. Multiple drivers can be created from the same `Agent` type by cloning it first.

### Tool sources federate

`add_tool_source` accepts any `ToolSource`. Sources are walked in registration order; the default `CollisionPolicy` is `FirstWins`. A typical interactive agent stitches three together:

```rust
let agent = Agent::builder()
    .model(adapter)
    .add_tool_source(native_registry)             // frozen built-ins
    .add_tool_source(mcp_manager.source())        // CatalogReader from McpServerManager
    .add_tool_source(skill_watcher.reader())      // dynamic_catalog reader
    .build()?;
```

Dynamic sources publish `ToolCatalogEvent`s; the driver re-snapshots the available tools at each model call boundary and emits `AgentEvent::ToolCatalogChanged` so observers can log what changed.

## Snapshots

The driver exposes a read-only snapshot for inspection or persistence:

```rust
let snapshot: LoopSnapshot = driver.snapshot();
// snapshot.session_id, snapshot.transcript, snapshot.pending_input
```

This is useful for debugging (inspect the transcript mid-session), persistence (serialize and resume later), and testing (assert on transcript state).

## Cancellation

If the host connects a `CancellationHandle` (e.g. wired to a Ctrl-C handler), the driver creates `TurnCancellation` checkpoints and passes them to model turns and tool executions:

```text
Host wires Ctrl-C:

  ctrlc::set_handler(move || controller.interrupt());

Driver flow:

  1. checkpoint = cancellation.checkpoint()
  2. session.begin_turn(request, Some(checkpoint.clone()))
  3. turn.next_event(Some(checkpoint.clone()))
     └── if cancelled → LoopError::Cancelled
  4. tool.invoke(request, ctx)  // ctx.cancellation = Some(checkpoint)
     └── if cancelled → ToolError::Cancelled
```

When cancellation fires, the current turn ends with `FinishReason::Cancelled`. The driver adds metadata (`agentkit.interrupted: true`, `agentkit.interrupt_reason: "user_cancelled"`) to the turn result so the host can distinguish cancellation from normal completion.

> **Example:** [`openrouter-coding-agent`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-coding-agent) demonstrates a driver executing filesystem tool calls across multiple roundtrips in a single turn.
>
> **Crate:** [`agentkit-loop`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-loop) — the `Agent`, `AgentBuilder`, `LoopDriver`, `LoopStep`, `TurnResult`, and `LoopSnapshot` types.
