# TaskManager Proposal For Parallel And Background Tool Execution

## Summary

This proposal keeps the current loop API and current interruption model intact by default.

The shape is:

1. add low-level lifecycle plumbing to capabilities/tools/core
2. add a new `TaskManager` abstraction that is independent of `agentkit-loop`
3. make `SimpleTaskManager` the default so current behavior remains unchanged
4. add an opt-in `AsyncTaskManager` for parallel foreground tasks and detached background tasks
5. let hosts talk to the task manager directly for task inspection and cancellation
6. continue the loop explicitly by feeding task-manager-produced items back into the existing loop

That gives us:

- 100% existing functionality and API surface by default
- no behavior changes unless a host opts into a non-default task manager
- a clean home for parallel/background execution without bloating the loop
- direct per-task cancellation without redefining loop interruption

## What Stays The Same

The existing host integration path remains valid:

- `Agent::builder()`
- `AgentBuilder::tools(...)`
- `AgentBuilder::cancellation(...)`
- `LoopDriver::submit_input(...)`
- `LoopDriver::next()`
- `LoopDriver::resolve_approval(...)`
- `LoopDriver::resolve_auth(...)`

Session-level interruption also remains exactly what it is today:

- host owns a `CancellationController`
- host calls `interrupt()` on that controller
- the loop and in-flight foreground tool work observe the shared cancellation handle

Default hosts should not need to change any code.

## Core Design

## 1. Add Low-Level Lifecycle Plumbing

The existing capability and tool execution surfaces are too request/response oriented to describe:

- task creation
- task state transitions
- foreground vs background work
- detached completion after the loop has yielded
- cancellation by task id

We should add low-level lifecycle plumbing, but keep it additive and non-breaking.

### New Core Types

```rust
pub struct TaskId(pub String);

pub enum TaskKind {
    Foreground,
    Background,
}

pub struct TaskSnapshot {
    pub id: TaskId,
    pub turn_id: TurnId,
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub kind: TaskKind,
    pub metadata: MetadataMap,
}

pub enum TaskEvent {
    Started(TaskSnapshot),
    Progress(TaskSnapshot),
    AwaitingApproval(TaskSnapshot, ApprovalRequest),
    AwaitingAuth(TaskSnapshot, AuthRequest),
    Completed(TaskSnapshot, ToolResultPart),
    Cancelled(TaskSnapshot, MetadataMap),
    Failed(TaskSnapshot, ToolError),
    ContinueRequested,
}
```

### Lifecycle Hook

```rust
pub trait TaskLifecycle: Send + Sync {
    fn event(&self, event: TaskEvent);
}
```

This is intentionally low-level plumbing.

It does not force tools to become background-aware.
It just gives the runtime a place to surface task lifecycles cleanly.

## 2. Add A New `TaskManager` Abstraction

The loop should not own parallel scheduling or detached background execution directly.
It should delegate tool-call execution to a task manager.

The task manager should be tool-agnostic.

That means:

- it does not need up-front knowledge of the tool registry
- it does not participate in model-facing tool advertisement
- it receives tool calls over time and decides how to run them
- the existing tool registry / tool executor remains the source of available tools

Suggested trait:

```rust
#[async_trait]
pub trait TaskManager: Send + Sync {
    async fn start_task(
        &self,
        request: ToolRequest,
        ctx: TaskStartContext<'_>,
    ) -> TaskStartOutcome;

    async fn poll_ready(&self) -> Vec<Item>;

    async fn on_turn_interrupted(
        &self,
        turn_id: TurnId,
    ) -> Result<(), TaskManagerError>;

    fn handle(&self) -> TaskManagerHandle;
}
```

Where:

```rust
pub enum TaskStartOutcome {
    Immediate(Item),
    Pending(TaskId),
}
```

Important point:

- `Immediate` means the task manager already has a loop-consumable result
- `Pending` means the task now exists in task manager state and may complete later

The loop does not need to know whether the task manager is sequential or async.
It only needs to consume either immediate results or later-ready items.

`on_turn_interrupted(...)` exists so the loop can notify the task manager that the current turn was cancelled by the shared `CancellationController`.

That allows the task manager to:

- drop queued foreground tasks for that turn
- reconcile running foreground task state with interruption
- keep background tasks alive

## 3. `SimpleTaskManager` Is The Default

This is critical.

`SimpleTaskManager` should exactly replicate current behavior:

- sequential execution
- no background tasks
- at most one blocking interrupt at a time
- no host-visible task surface beyond what already exists

It should not need a tool registry in its constructor.
The task manager is tool-agnostic.
The loop still owns the tool registry / executor and passes calls into the task manager as they happen.

Suggested defaulting:

```rust
let agent = Agent::builder()
    .model(model)
    .build()?;
```

should behave exactly like today because internally the builder injects:

```rust
SimpleTaskManager::new()
```

### Builder API

Add a non-breaking builder method:

```rust
impl<M> AgentBuilder<M>
where
    M: ModelAdapter,
{
    pub fn task_manager(mut self, manager: impl TaskManager + 'static) -> Self {
        self.task_manager = Some(Arc::new(manager));
        self
    }
}
```

If omitted:

- builder creates `SimpleTaskManager`
- all current examples keep working unchanged

## 4. `AsyncTaskManager` Is Opt-In

`AsyncTaskManager` adds:

- parallel foreground tasks
- detached background tasks
- direct host-facing task cancellation by `TaskId`
- result buffering
- configurable routing policy
- configurable continue policy

Opt-in host integration:

```rust
let cancellation = CancellationController::new();

let task_manager = AsyncTaskManager::new()
    .routing(MyRoutingPolicy);
let tasks = task_manager.handle();

let agent = Agent::builder()
    .model(model)
    .tools(tools)
    .cancellation(cancellation.handle())
    .task_manager(task_manager)
    .build()?;

let mut driver = agent.start(config).await?;
```

The loop API is unchanged.
The new host-facing control surface is `tasks`.

## User-Facing API

## Host API With `SimpleTaskManager`

This remains current usage:

```rust
let cancellation = CancellationController::new();

let agent = Agent::builder()
    .model(model)
    .tools(tools)
    .cancellation(cancellation.handle())
    .build()?;

let mut driver = agent.start(config).await?;
driver.submit_input(user_items)?;

match driver.next().await? {
    LoopStep::Finished(turn) => { /* unchanged */ }
    LoopStep::Interrupt(interrupt) => { /* unchanged */ }
}
```

Nothing new is required.

## Host API With `AsyncTaskManager`

The host gets a handle:

```rust
pub struct TaskManagerHandle { /* opaque */ }
```

and uses it directly:

```rust
impl TaskManagerHandle {
    pub async fn next_event(&self) -> Option<TaskEvent>;

    pub async fn cancel(&self, task_id: TaskId) -> Result<(), TaskManagerError>;

    pub async fn list_running(&self) -> Vec<TaskSnapshot>;

    pub async fn list_completed(&self) -> Vec<TaskSnapshot>;

    pub async fn drain_ready_items(&self) -> Vec<Item>;

    pub async fn set_continue_policy(
        &self,
        task_id: TaskId,
        policy: ContinuePolicy,
    ) -> Result<(), TaskManagerError>;
}
```

### Continue Policy

This controls whether the task manager asks the host to continue the loop immediately when a background result becomes ready.

```rust
pub enum ContinuePolicy {
    NotifyOnly,
    RequestContinue,
}
```

This is intentionally separate from result storage.

Background results are always retained in the task manager until the host drains them.
`ContinuePolicy` only controls whether the task manager emits `TaskEvent::ContinueRequested`.

That avoids conflating:

- where completed task results live
- whether loop continuation should happen now or later

There is no separate “manual mode”.
Manual behavior already exists whenever the host simply leaves results buffered until it wants them.

## Task Routing

The async task manager needs a configurable routing policy to decide how each tool call should be handled.

Suggested API:

```rust
pub trait TaskRoutingPolicy: Send + Sync {
    fn route(&self, request: &ToolRequest) -> RoutingDecision;
}

pub enum RoutingDecision {
    Foreground,
    Background,
    ForegroundThenDetachAfter(Duration),
}
```

The async task manager constructor should accept such a policy:

```rust
let task_manager = AsyncTaskManager::new()
    .routing(MyRoutingPolicy);
```

This supports the patterns we want:

- if a tool call takes over 5s, move it to background
- allow the agent to request explicit background execution through tool-call arguments or metadata
- let the implementer route by tool name, namespace, input shape, or any custom rule

Example:

```rust
struct MyRoutingPolicy;

impl TaskRoutingPolicy for MyRoutingPolicy {
    fn route(&self, request: &ToolRequest) -> RoutingDecision {
        if request.tool_name == "subagent.launch" {
            RoutingDecision::Background
        } else if request.tool_name == "shell_exec" {
            RoutingDecision::Foreground
        } else if request.metadata.get("background") == Some(&true.into()) {
            RoutingDecision::Background
        } else {
            RoutingDecision::ForegroundThenDetachAfter(Duration::from_secs(5))
        }
    }
}
```

This policy belongs in the task manager, not in the loop.

## Cancellation Semantics

These are the requirements:

- when the loop is interrupted, all in-flight parallel foreground tasks should be cancelled
- in-flight background tasks should not be cancelled
- background tasks should be cancellable individually

This proposal satisfies that by separating:

- loop/session interruption
- task cancellation

### Loop Interruption

Hosts continue to use the existing mechanism:

```rust
cancellation.interrupt();
```

Semantics with `AsyncTaskManager`:

- foreground tasks for the active turn are cancelled
- background tasks survive
- loop returns control to the host exactly as it does today

This means we do not invent a second session interruption API.

When the loop observes that the active turn has been interrupted, it should also notify the task manager:

```rust
task_manager.on_turn_interrupted(turn_id).await?;
```

That propagation is required so the task manager can clean up queued/running foreground work for the interrupted turn while leaving background work alone.

### Individual Background Task Cancellation

Hosts cancel background work through the task manager handle:

```rust
tasks.cancel(task_id).await?;
```

This is intentionally separate from `CancellationController`.

The controller remains the session/turn interruption mechanism.
The task manager handle becomes the task-level cancellation mechanism.

## How Continuation Works After The Loop Yields

Example:

1. loop starts
2. agent launches foreground tasks and background tasks
3. user interrupts the loop through `CancellationController`
4. foreground tasks are cancelled; background tasks continue
5. loop yields back to the host
6. a background task completes later

At this point the loop is not running.
That is fine.

The background completion should be surfaced through the task manager handle, not through `LoopDriver::next()`.

If the yielded loop is to continue, the host must continue it explicitly.

For that reason, the loop should gain a small non-breaking helper:

```rust
impl<S> LoopDriver<S>
where
    S: ModelSession,
{
    pub fn continue_with(&mut self, items: Vec<Item>) -> Result<(), LoopError>;
}
```

This is just a clearer host-facing alias for feeding non-user items back into the transcript before the next `next()` call.

### Policy A: Request Continue Immediately

Host opts a task into:

```rust
tasks
    .set_continue_policy(task_id.clone(), ContinuePolicy::RequestContinue)
    .await?;
```

Then when the task completes:

- `TaskManagerHandle::next_event()` yields `TaskEvent::Completed(...)`
- `TaskManagerHandle::next_event()` also yields `TaskEvent::ContinueRequested`
- host drains the ready items from the task manager
- host feeds them into the loop explicitly
- host calls `driver.next().await?` explicitly

Example:

```rust
if let Some(TaskEvent::ContinueRequested) = tasks.next_event().await {
    let items = tasks.drain_ready_items().await;
    if !items.is_empty() {
        driver.continue_with(items)?;
        let _ = driver.next().await?;
    }
}
```

Important point:

- the loop is still host-driven
- “auto resume” really means “the task manager requests continuation, and the host performs it”
- continuation is explicit

### Policy B: Keep Results Buffered Until The Next User Turn

Host opts a task into:

```rust
tasks
    .set_continue_policy(task_id.clone(), ContinuePolicy::NotifyOnly)
    .await?;
```

Then when the task completes:

- task manager buffers loop-consumable items
- no continue request is emitted
- nothing happens to the loop immediately
- next time the user sends a message, the host drains buffered items and submits them first

Example:

```rust
let buffered = tasks.drain_ready_items().await;
if !buffered.is_empty() {
    driver.continue_with(buffered)?;
}
driver.submit_input(user_items)?;
let step = driver.next().await?;
```

This fits the current loop API cleanly.

## How Task Output Feeds Back Into The Loop

The task manager should produce normal transcript-ready items:

```rust
pub async fn drain_ready_items(&self) -> Vec<Item>;
```

These items are then fed through the existing loop API:

```rust
driver.continue_with(items)?;
```

or, equivalently, through the existing generic input path:

```rust
driver.submit_input(items)?;
```

The loop does not need to know why those items exist.
It only needs to continue consuming transcript items.

## Approval And Auth

Approval and auth do not need a larger refactor for this proposal.

They should remain in the existing loop path.

That means:

- a task execution attempt may yield approval or auth
- the loop re-surfaces that through the existing `LoopInterrupt` API
- if the host rejects or cancels, the loop resumes normal operation exactly as it does today
- if the host approves or provides auth, the loop re-submits that tool call to the task manager

In other words:

- task managers do not need to own approval state
- task managers do not need to own auth state
- task managers only need to report that a task could not start or continue because approval/auth is required

For v1 of this proposal:

- keep the current approval/auth API
- add `TaskId` as a typed field on approval/auth interrupts

That is enough to correlate loop interrupts with task-manager state without redesigning the approval/auth flow.

## Suggested Public API

### New Builder Method

```rust
impl<M> AgentBuilder<M>
where
    M: ModelAdapter,
{
    pub fn task_manager(mut self, manager: impl TaskManager + 'static) -> Self;
}
```

### New Loop Convenience

```rust
impl<S> LoopDriver<S>
where
    S: ModelSession,
{
    pub fn continue_with(&mut self, items: Vec<Item>) -> Result<(), LoopError>;
}
```

### New Task Manager Types

```rust
pub trait TaskManager: Send + Sync { /* ... */ }

pub struct TaskManagerHandle { /* ... */ }

pub struct SimpleTaskManager { /* default */ }

pub struct AsyncTaskManager { /* opt-in */ }
```

### New AsyncTaskManager Host API

```rust
impl AsyncTaskManager {
    pub fn new() -> Self;
    pub fn routing(self, policy: impl TaskRoutingPolicy + 'static) -> Self;
    pub fn handle(&self) -> TaskManagerHandle;
}

impl TaskManagerHandle {
    pub async fn next_event(&self) -> Option<TaskEvent>;
    pub async fn cancel(&self, task_id: TaskId) -> Result<(), TaskManagerError>;
    pub async fn list_running(&self) -> Vec<TaskSnapshot>;
    pub async fn list_completed(&self) -> Vec<TaskSnapshot>;
    pub async fn drain_ready_items(&self) -> Vec<Item>;
    pub async fn set_continue_policy(
        &self,
        task_id: TaskId,
        policy: ContinuePolicy,
    ) -> Result<(), TaskManagerError>;
}
```

## Migration Story

### Existing Users

No changes:

```rust
let agent = Agent::builder()
    .model(model)
    .tools(tools)
    .build()?;
```

This uses `SimpleTaskManager` implicitly.

### Users Who Want Parallel And Background Tasks

They opt in:

```rust
let task_manager = AsyncTaskManager::new()
    .routing(MyRoutingPolicy);
let tasks = task_manager.handle();

let agent = Agent::builder()
    .model(model)
    .tools(tools)
    .task_manager(task_manager)
    .cancellation(cancellation.handle())
    .build()?;
```

Then they manage:

- direct task cancellation via `tasks.cancel(task_id)`
- continue policy via `tasks.set_continue_policy(...)`
- explicit loop continuation by draining `tasks.drain_ready_items()` into `driver.continue_with(...)`

## Decisions / Open Questions

1. Ordering can remain an implementation detail of the concrete task manager.
   For compatibility, `SimpleTaskManager` should preserve current order.
   For async managers, out-of-order completion is acceptable unless a host opts into ordering.
2. No strong opinion on whether `SimpleTaskManager` exposes a no-op handle or an empty live handle.
3. Routing should be host-configurable first, with optional tool-call argument or metadata hints.
4. Approval/auth interruptions should carry `TaskId` as a typed field.

## Recommendation

Implement this in two layers:

1. additive lifecycle plumbing in capabilities/tools/core
2. a new `TaskManager` abstraction with:
   - `SimpleTaskManager` as the default
   - `AsyncTaskManager` as the opt-in async implementation

This is the highest-leverage path because it preserves all existing behavior while creating a clean user-facing API for:

- per-task cancellation
- task inspection
- configurable foreground/background routing
- explicit loop continuation after detached task completion
