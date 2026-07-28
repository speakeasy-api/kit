# agentkit-loop design

## Purpose

`agentkit-loop` is the execution crate.

It owns the mechanics of running an agent session:

- accepting host input
- invoking the configured model session
- observing streamed model output
- dispatching tool calls
- pausing on blocking interrupts
- notifying reporters about non-blocking activity
- maintaining enough in-memory state to continue the conversation correctly

It should be the crate that turns `agentkit-core` data types into a running agent.

## Non-goals

`agentkit-loop` should not own:

- provider SDK implementations
- shell or filesystem execution
- MCP transport details
- long-term transcript persistence
- UI concerns
- prompt authoring conventions
- reporter implementations

It coordinates these concerns, but it should not absorb them.

## Dependency policy

`agentkit-loop` should stay runtime-agnostic.

That means:

- no direct dependency on `tokio`
- depend only on portable async primitives such as `Future`, `Stream`, and synchronization types that are runtime-independent
- let built-in tools and MCP process management use `tokio` in their own crates

This gives you a portable execution core while still allowing runtime-specific leaf crates.

Built-in tools and MCP transports may depend on runtime-specific crates behind their own features, but the loop crate itself should not.

## Core design principles

### 1. The driver returns only blocking control points

The driver API should not yield every event.

It should yield only things the host must answer before the session can continue:

- approval requests
- requests for more input
- final turn completion

Everything else should go to reporters or event sinks.

### 2. The loop owns orchestration, not policy

The loop should know how to:

- run the state machine
- sequence model and tool interactions
- pause and resume

The host should decide:

- what to do with approval requests
- what system/developer/context items to include
- what tools are registered
- what gets logged or displayed
- whether to persist transcript snapshots

### 3. The loop works in normalized types at its boundaries

Provider adapters may keep native request and response types internally.

But the loop should operate in terms of normalized `agentkit-core` types at its boundaries:

- input items
- streamed deltas
- tool calls
- tool results
- finish reasons
- usage

That keeps the orchestration logic provider-neutral.

### 4. Reporting is observation, not control

Reporters should receive structured events about what happened.

They should not:

- decide whether the loop can continue
- inject control flow
- become mandatory for correctness

If a host wants to render streamed text or store a transcript, it attaches one or more reporters.

## Public mental model

There should be three layers:

1. `Agent`
2. `LoopDriver`
3. `ModelSession`

`Agent` is the assembled configuration.

It contains:

- one model adapter
- zero or more context sources
- one tool registry
- one approval policy or interrupt policy
- zero or more event sinks
- zero or more `LoopMutator`s (e.g. compaction)

`LoopDriver` is the mutable runtime instance.

It owns:

- the active model session
- the working transcript
- pending host input
- pending interrupt state
- current turn bookkeeping

`ModelSession` is the adapter-owned interface for one provider-backed conversation.

This shape supports both:

- stateless providers, which reconstruct context on every turn
- stateful providers, which keep remote session state

## Main API sketch

Recommended host-facing shape:

```rust
let agent = Agent::builder()
    .model(my_adapter)
    .tools(my_tools)
    .context(my_context)
    .reporter(my_reporter)
    .build()?;

let mut driver = agent.start()?;

driver.submit_input(user_items)?;

loop {
    match driver.next().await? {
        LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(req)) => {
            let decision = ask_user(req).await?;
            driver.resolve_approval(decision)?;
        }
        LoopStep::Interrupt(LoopInterrupt::AuthRequest(_)) => {
            // Obtain credentials and resolve; omitted here.
            break;
        }
        LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
            let input = read_input().await?;
            driver.submit_input(input)?;
        }
        LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {
            // Optional mid-turn interjection point.  Non-interactive
            // callers just loop; interactive callers may submit_input.
        }
        LoopStep::Finished(turn) => {
            println!("turn complete: {:?}", turn.finish_reason);
            break;
        }
    }
}
```

The important rule is that `next()` should not return ordinary reporting events.

## Driver contract

Recommended interface:

```rust
pub struct LoopDriver { /* private */ }

impl LoopDriver {
    pub fn submit_input(&mut self, input: Vec<Item>) -> Result<(), LoopError>;
    pub fn resolve_approval(&mut self, decision: ApprovalDecision) -> Result<(), LoopError>;
    pub async fn next(&mut self) -> Result<LoopStep, LoopError>;
    pub fn snapshot(&self) -> LoopSnapshot;
}
```

Semantics:

- `submit_input` queues user or host-provided items for the next runnable turn
- `resolve_approval` resumes the driver after a blocking approval interrupt
- `next` advances internal work until it hits the next blocking interrupt or finishes a turn
- `snapshot` exposes inspectable state without handing transcript ownership to the loop permanently

`next()` should be resumable and idempotent with respect to state transitions:

- calling `next()` twice without answering an outstanding interrupt should return an error
- calling `resolve_approval()` when no approval is pending should return an error
- calling `submit_input()` while the loop is blocked on approval or auth errors with `LoopError::InvalidState`; submitting during a cooperative yield (`AwaitingInput`, `AfterToolResult`) is allowed and is the intended path for mid-turn interjection

## LoopStep and interrupts

Shape:

```rust
pub enum LoopStep {
    Interrupt(LoopInterrupt),
    Finished(TurnResult),
}

pub enum LoopInterrupt {
    ApprovalRequest(PendingApproval),       // blocking
    AuthRequest(PendingAuth),               // blocking
    AwaitingInput(InputRequest),            // cooperative
    AfterToolResult(ToolRoundInfo),         // cooperative
}
```

Why this is the right boundary:

- `ApprovalRequest` means the loop cannot continue safely without a host decision.
- `AuthRequest` means a tool needs credentials before it can execute.
- `AwaitingInput` means the current turn boundary has been reached and the loop needs more user/host input.
- `AfterToolResult` means a tool round just finished; the host may interject a user message before the next model call by calling `submit_input`, or just call `next()` again to resume.
- `Finished` means the requested turn completed and the host takes control back.

Blocking variants must be resolved before `next()` can run again; cooperative variants impose no such constraint. `LoopInterrupt::is_blocking()` surfaces the distinction.

## Approval model

Approval should be an interrupt, not a callback hidden deep inside execution.

That matches your intended style and makes the host's responsibility explicit.

Recommended types:

```rust
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub reason: ApprovalReason,
    pub action: ProposedAction,
    pub context: ApprovalContext,
}

pub enum ApprovalDecision {
    Approve,
    Deny { reason: Option<String> },
}
```

Typical approval reasons:

- shell command exceeds policy
- filesystem write touches a protected path
- MCP server requests auth or elevated access
- a host-defined policy requires confirmation for a specific tool

The loop should not know how approval is displayed. It only knows it needs an answer.

## Event delivery

Non-blocking activity should be delivered through event sinks.

Recommended minimal contract:

```rust
pub trait LoopObserver: Send + Sync {
    fn handle_event(&self, event: ObservedEvent);
}
```

`ObservedEvent` carries the `session_id` plus the `AgentEvent`. Observers take `&self` and hold mutable state behind interior mutability (`Mutex`, atomics, channels). The driver shares each observer as `Arc<dyn LoopObserver>` so a single configured agent can mint multiple sessions over its lifetime (e.g. an outer agent that uses an inner sub-agent for compaction).

V1 should make this synchronous.

Reason:

- it keeps `agentkit-loop` runtime-agnostic
- it keeps event ordering deterministic
- it avoids forcing async trait machinery into the observer boundary
- it keeps the loop crate simple and easy to reason about

That means the loop calls observers inline as part of its ordinary execution.

This can be richer in later versions, but the conceptual boundary should stay the same.

`agentkit-loop` should define the event stream it emits.
`agentkit-reporting` should provide observer implementations that log, print, aggregate usage, or serialize events.

That keeps the direction of dependencies clean:

- `agentkit-loop` defines events and observer contract
- `agentkit-reporting` depends on `agentkit-loop`, not the other way around

Buffered or async fanout, if needed later, should be built as an adapter layer on top of this synchronous observer contract rather than making the base loop API more complex.

## AgentEvent

`AgentEvent` should capture non-blocking state changes and observations.
The enum is non-exhaustive: observers and replay tools should keep a wildcard
arm so new stream events, such as tool execution lifecycle updates, do not
break ingestion.

Recommended categories:

- `RunStarted`
- `TurnStarted`
- `InputAccepted`
- `ContentDelta`
- `ToolCallRequested`
- `ToolExecutionStarted`
- `ToolExecutionProgress`
- `ToolResultReceived`
- `ApprovalRequired`
- `ApprovalResolved`
- `MutationStarted`
- `MutationFinished`
- `UsageUpdated`
- `Warning`
- `RunFailed`
- `TurnFinished`

Notes:

- `ApprovalRequired` may be both reported as an event and surfaced as an interrupt
- the event is for observability
- the interrupt is for control flow

This duplication is acceptable because they serve different purposes.

## Model adapter boundary

`agentkit-loop` should own the operational model interfaces.

Recommended first-pass traits:

```rust
pub trait ModelAdapter {
    type Session: ModelSession;

    async fn start_session(
        &self,
        config: SessionConfig,
    ) -> Result<Self::Session, LoopError>;

    // Optional. Lowercase provider identifier used to populate
    // `gen_ai.provider.name` on the `agent.turn` and `chat` spans.
    // Defaults to `None`.
    fn provider_name(&self) -> Option<&str> { None }
}

pub trait ModelSession {
    type Turn: ModelTurn;

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
    ) -> Result<Self::Turn, LoopError>;

    // Optional. Model identifier used to populate `gen_ai.request.model`
    // on the loop's `chat` span. Defaults to `None`.
    fn model_name(&self) -> Option<&str> { None }
}

pub trait ModelTurn {
    async fn next_event(&mut self) -> Result<Option<ModelTurnEvent>, LoopError>;
}
```

This three-level split matters:

- `ModelAdapter` creates the provider session
- `ModelSession` begins a turn
- `ModelTurn` yields streamed provider output until completion

It fits both:

- stateless HTTP providers
- stateful websocket or remote-session providers

## ModelTurnEvent

The model turn stream should emit normalized operational events, not provider wire fragments.

Recommended shape:

```rust
pub enum ModelTurnEvent {
    Delta(ContentDelta),
    ToolCall(ToolCallPart),
    Usage(Usage),
    Finished(ModelTurnResult),
}
```

Where:

- `Delta` carries streamed content fragments
- `ToolCall` surfaces a complete tool invocation request
- `Usage` provides incremental or final usage updates
- `Finished` includes normalized finish reason and optional final assistant items

The loop consumes these events, updates its transcript, calls observers, and decides what to do next.

## TurnRequest and TurnResult

Recommended `TurnRequest` contents:

- session ID
- turn ID
- effective transcript or transcript delta
- active tool specs
- provider-facing turn options

Recommended `TurnResult` contents:

- turn ID
- finish reason
- items appended during the turn
- usage summary
- interruption summary if applicable

The loop should own `TurnRequest` construction so the host does not need to manually rebuild model-facing state every turn.

## Transcript ownership

The loop needs a working transcript to function correctly.

That does not mean it should own persistence.

Recommended boundary:

- the loop owns the in-memory active transcript for the running session
- hosts can inspect snapshots or receive full events
- hosts decide whether and where to persist

This keeps the loop usable while avoiding a storage opinion.

## Tool integration

The loop should depend on a tool execution abstraction from `agentkit-tools-core`.

Conceptually:

- the model emits `ToolCallPart`
- the loop checks policy and approval requirements
- the loop executes the tool through the tool registry/executor
- the loop appends a `ToolResultPart` to the working transcript
- the loop continues the same turn or starts the next model round as needed

The loop should support multiple tool roundtrips inside a single host-visible turn.

That is important for coding-agent behavior, where one user message may trigger many tool invocations before the assistant is ready to yield.

## Suggested internal state machine

Internally, the driver will need more states than the host sees.

Recommended internal states:

- `Idle`
- `Ready`
- `RunningModelTurn`
- `WaitingForApproval`
- `ExecutingTool`
- `RunningMutators`
- `FinishedTurn`
- `Failed`

The host-facing API should still collapse this to:

- interrupt
- finished

The value of this split is that the loop can remain expressive internally without leaking complexity into the public contract.

## Turn lifecycle

Recommended execution flow:

1. host submits input items
2. driver merges input into working transcript
3. driver runs registered `LoopMutator`s at `MutationPoint::AfterTurnEnded` / `AfterToolResult` (compaction, redaction, repair); the loop validates transcript invariants if any mutator dirtied the cursor
4. driver constructs `TurnRequest`
5. driver starts a provider turn
6. driver forwards streamed deltas to observers
7. if the model emits a tool call:
   - check permission policy
   - if approval is required, emit event and return interrupt
   - otherwise execute tool
   - append `ToolResultPart`
   - continue model roundtrip
8. when the model finishes:
   - append final assistant items
   - emit usage and turn-finished events
   - return `LoopStep::Finished`
9. if the host wants another user turn:
   - host calls `submit_input`
   - loop continues
10. if no input is available when the host asks the driver to continue:
    - return `LoopInterrupt::AwaitingInput`

## Error handling

`LoopError` should represent operational failures:

- invalid driver state
- provider failure
- tool execution failure when not representable as a tool result
- observer failure if configured as fatal
- mutator failure (compaction, redaction, repair) — including transcript invariant violations

V1 should default to making reporter failures non-fatal.

Reason:

- logging should not usually break the agent
- usage aggregation should not usually break the agent
- synchronous observers should remain observational, not control-bearing

If a host wants strict behavior, that can be a configuration option.

## Cancellation and timeouts

The loop crate should support cancellation and timeouts conceptually, but keep them minimal in v1.

V1 should support:

- host-requested cancellation of an active turn
- tool execution timeout propagation from tool crates
- provider timeout propagation from adapters

V1 should not try to define one global timeout system for every subsystem.

## Suggested module layout

```text
agentkit-loop/
  src/
    lib.rs
    agent.rs
    builder.rs
    driver.rs
    interrupt.rs
    event.rs
    observer.rs
    model.rs
    turn.rs
    state.rs
    error.rs
```

Module intent:

- `agent.rs`: assembled loop configuration
- `builder.rs`: ergonomic construction
- `driver.rs`: `LoopDriver`
- `interrupt.rs`: `LoopStep`, `LoopInterrupt`, approval types
- `event.rs`: `AgentEvent`
- `observer.rs`: observer contract and fanout
- `model.rs`: adapter/session/turn traits
- `turn.rs`: `TurnRequest`, `TurnResult`, execution bookkeeping
- `state.rs`: internal state machine
- `error.rs`: loop-specific errors

## What we should validate early

Before locking the public API, prove these with tests and a fake provider:

1. the driver can pause on approval and resume correctly
2. non-blocking deltas can be observed without changing control flow
3. a single user turn can contain multiple tool roundtrips
4. a stateless adapter and a stateful adapter both fit the same session/turn traits
5. transcript snapshots are sufficient for external persistence

If any of those feel awkward, the loop abstraction is still wrong.
