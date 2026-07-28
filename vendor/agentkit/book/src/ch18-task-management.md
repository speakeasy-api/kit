# Task management and parallelism

When an agent calls multiple tools in a single turn, running them sequentially wastes time. When a shell command takes 30 seconds, the agent shouldn't be blocked waiting. This chapter covers `agentkit-task-manager`: how tool calls are scheduled, routed, and delivered.

## The problem

The default behavior is sequential: tool calls execute one at a time on the current task. This is correct and simple, but it becomes a bottleneck when:

- The model requests multiple independent tool calls
- A shell command runs for a long time
- You want to start background work while the model continues

## TaskManager trait

```rust
#[async_trait]
pub trait TaskManager {
    async fn start_task(&self, request: TaskLaunchRequest, ctx: TaskStartContext)
        -> Result<TaskStartOutcome, TaskManagerError>;

    async fn wait_for_turn(&self, turn_id: &TurnId, cancellation: Option<TurnCancellation>)
        -> Result<Option<TurnTaskUpdate>, TaskManagerError>;

    async fn take_pending_loop_updates(&self)
        -> Result<PendingLoopUpdates, TaskManagerError>;

    async fn on_turn_interrupted(&self, turn_id: &TurnId)
        -> Result<(), TaskManagerError>;

    fn handle(&self) -> TaskManagerHandle;
}
```

## SimpleTaskManager (default)

Runs every tool call inline. No Tokio dependency. No concurrency. Returns the result before the driver continues. This is the default when no task manager is configured.

## AsyncTaskManager

Spawns each tool call as a Tokio task. Tasks are classified through a `TaskRoutingPolicy`:

```rust
pub enum RoutingDecision {
    Foreground,
    Background,
    ForegroundThenDetachAfter(Duration),
}
```

- **Foreground** — blocks the current turn until resolved
- **Background** — runs independently, results delivered later
- **ForegroundThenDetachAfter(Duration)** — starts foreground, automatically promotes to background if it hasn't finished within the timeout

### Routing policies

Implement `TaskRoutingPolicy` or use a closure:

```rust
let task_manager = AsyncTaskManager::new().routing(|req: &ToolRequest| {
    if req.tool_name.0 == "shell_exec" {
        RoutingDecision::ForegroundThenDetachAfter(Duration::from_secs(5))
    } else {
        RoutingDecision::Foreground
    }
});
```

This lets you make filesystem tools synchronous (fast, no overhead) while giving shell commands a timeout before they detach.

## Task lifecycle events

The `TaskManagerHandle` provides an event stream:

```rust
pub enum TaskEvent {
    Started(TaskSnapshot),
    Detached(TaskSnapshot),
    Completed(TaskSnapshot, ToolResultPart),
    Cancelled(TaskSnapshot),
    Failed(TaskSnapshot, ToolError),
    ContinueRequested,
}
```

Host code can subscribe to these events for progress reporting, UI updates, or manual task management:

```rust
let handle = task_manager.handle();
// List running tasks, cancel tasks, drain results, subscribe to events
```

## Integration with the loop

The loop driver integrates with the task manager transparently:

1. Tool call arrives from the model
2. Driver asks the task manager to start a task
3. If `TaskStartOutcome::Ready` — result is immediately available
4. If `TaskStartOutcome::Pending` — driver waits for foreground tasks via `wait_for_turn()`
5. Background task results are picked up via `take_pending_loop_updates()` on the next iteration
6. Background results are injected into the transcript as tool results

## The detach-after-timeout pattern

`ForegroundThenDetachAfter` deserves special attention. It solves a common problem: you want the model to wait for a command's output, but you don't want a slow command to block the entire turn.

```text
ForegroundThenDetachAfter(5s) — two possible outcomes:

Fast command (< 5s):

  t=0s  Task starts (foreground)
  t=3s  Command finishes → result returned immediately
        └── Model sees output, continues normally
        └── Identical to pure Foreground routing


Slow command (> 5s):

  t=0s  Task starts (foreground)
  t=5s  Timeout expires → task promoted to background
        └── Model receives: "Task detached (still running)"
        └── Model continues its turn (reads files, etc.)
  t=30s Command finishes → result stored
        └── On next turn, driver picks up the result
        └── Result injected into transcript as a tool result
```

This is the right default for shell commands in a coding agent:

- `cargo check` (2 seconds) → foreground, model sees the output immediately
- `cargo test` (30 seconds) → detaches after 5s, model continues working
- `ls` (instant) → foreground, practically no delay

### How background results re-enter the loop

When the driver starts a new turn, it calls `task_manager.take_pending_loop_updates()`. Any completed background tasks have their results injected into the transcript before the model sees it. The driver fires `AgentEvent::ToolExecutionProgress(_)` for the detachment placeholder (a non-terminal update) and `AgentEvent::ToolResultReceived(_)` for the eventual result, so observers can render the bg-tool transition in the UI:

```text
Turn N:
  Model: ToolCall(shell_exec, "cargo test")
  Task manager: starts foreground, detaches after 5s
  Driver appends a placeholder tool result ("task detached")
  AgentEvent::ToolExecutionProgress(call_id=42, is_error=false)
  Model: ToolCall(fs_read_file, "src/test_results.rs")  ← continues working
  Turn ends

Turn N+1:
  take_pending_loop_updates() → cargo test finished: "3 tests passed"
  Driver appends the real result and fires
  AgentEvent::ToolResultReceived(call_id=42, …)         ← same call_id
  Model: "All 3 tests pass. Here's what I changed..."
```

Correlate the two events by `call_id`. Hosts that need a fully reconstructable transcript (persistence, audit) should also register a `TranscriptObserver` — it fires once per `Item` appended, so the placeholder and the real result both appear in transcript order.

## Task lifecycle

```text
              ┌─────────────────┐
              │     Started     │
              └────────┬────────┘
                       │
        ┌──────────────┼─────────────┐
        │              │             │
  Foreground    FG then detach   Background
        │              │             │
        │         ┌────▼─────┐       │
        │         │ timeout? │       │
        │         └──┬───┬───┘       │
        │         no │   │ yes       │
        │            │   │           │
  ┌─────▼────────────▼┐  │  ┌────────▼──────┐
  │    Foreground     │  │  │  Background   │
  │    (blocks turn)  │  │  │  (async)      │
  └─────────┬─────────┘  │  └──────┬────────┘
            │            │         │
            │     ┌──────▼─────┐   │
            │     │  Detached  │   │
            │     │  (async)   │   │
            │     └──────┬─────┘   │
            │            │         │
       ┌────▼────────────▼─────────▼────┐
       │       Completed / Failed       │
       └────────────────────────────────┘
```

## Choosing a routing strategy

| Scenario                                | Recommended routing                  | Why                                                 |
| --------------------------------------- | ------------------------------------ | --------------------------------------------------- |
| File read/write                         | `Foreground`                         | Fast, order matters, model needs result immediately |
| Short shell commands (ls, git status)   | `Foreground`                         | Fast enough that detach overhead isn't worth it     |
| Build commands (cargo build, npm build) | `ForegroundThenDetachAfter(5-10s)`   | May be fast, may be slow — let the timeout decide   |
| Test suites                             | `ForegroundThenDetachAfter(5s)`      | Often slow, model can do other work while waiting   |
| Long-running servers                    | `Background`                         | Model shouldn't wait at all                         |
| Independent parallel tool calls         | `Foreground` (with AsyncTaskManager) | AsyncTaskManager runs foreground tasks concurrently |

> **Example:** [`openrouter-parallel-agent`](https://github.com/danielkov/agentkit/tree/main/examples/openrouter-parallel-agent) uses `AsyncTaskManager` with `ForegroundThenDetachAfter` routing for shell tools and foreground routing for filesystem tools. The `TaskManagerHandle` event stream is printed to stderr.
>
> **Crate:** [`agentkit-task-manager`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-task-manager) — depends on [`agentkit-tools-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tools-core), [`agentkit-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-core), and [tokio](https://tokio.rs).
