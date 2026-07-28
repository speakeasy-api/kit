# agentkit-task-manager

<p align="center">
  <a href="https://crates.io/crates/agentkit-task-manager"><img src="https://img.shields.io/crates/v/agentkit-task-manager.svg?logo=rust" alt="Crates.io" /></a>
  <a href="https://docs.rs/agentkit-task-manager"><img src="https://img.shields.io/docsrs/agentkit-task-manager?logo=docsdotrs" alt="Documentation" /></a>
  <a href="https://github.com/danielkov/agentkit/blob/main/LICENSE"><img src="https://img.shields.io/crates/l/agentkit-task-manager.svg" alt="License" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/MSRV-1.92-blue?logo=rust" alt="MSRV" /></a>
</p>

Task scheduling abstractions for tool execution in [agentkit](https://crates.io/crates/agentkit).

A `TaskManager` decides whether each tool call runs inline (foreground), in the
background, or starts foreground and detaches after a timeout. The agent loop
talks to a single `TaskManagerHandle` and receives `TaskEvent`s as tasks make
progress, complete, or get cancelled.

## What it provides

- `TaskManager` trait + two implementations:
  - `SimpleTaskManager` -- executes every tool call inline, no spawning
  - `AsyncTaskManager` -- spawns tokio tasks and supports background work,
    cancellation, and detachment
- `TaskRoutingPolicy` for deciding `Foreground` vs `Background` vs
  `ForegroundThenDetachAfter(Duration)` per request
- `TaskManagerHandle` for cancellation, listing running/completed tasks,
  and draining results out-of-band
- `TaskEvent` stream covering `Started`, `Detached`, `Completed`,
  `Cancelled`, `Failed`, and `ContinueRequested`

## Quick start

`SimpleTaskManager` is the smallest useful implementation. It runs each tool
call to completion before returning a `TaskResolution`:

```rust,no_run
use std::sync::Arc;

use agentkit_core::{MetadataMap, SessionId, ToolCallId, TurnId};
use agentkit_task_manager::{
    SimpleTaskManager, TaskLaunchRequest, TaskManager, TaskStartContext,
};
use agentkit_tools_core::{
    BasicToolExecutor, OwnedToolContext, PermissionChecker, PermissionDecision,
    PermissionRequest, ToolExecutor, ToolName, ToolRegistry, ToolRequest,
};
use serde_json::json;

struct AllowAll;
impl PermissionChecker for AllowAll {
    fn evaluate(&self, _: &dyn PermissionRequest) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

# async fn run() -> Result<(), Box<dyn std::error::Error>> {
let manager = SimpleTaskManager::new();
let executor: Arc<dyn ToolExecutor> =
    Arc::new(BasicToolExecutor::from_registry(ToolRegistry::new()));

let request = ToolRequest {
    call_id: ToolCallId::new("call-1"),
    tool_name: ToolName::new("my_tool"),
    input: json!({}),
    session_id: SessionId::new("session-1"),
    turn_id: TurnId::new("turn-1"),
    metadata: MetadataMap::new(),
};

let ctx = TaskStartContext {
    executor,
    tool_context: OwnedToolContext {
        session_id: request.session_id.clone(),
        turn_id: request.turn_id.clone(),
        metadata: MetadataMap::new(),
        permissions: Arc::new(AllowAll),
        resources: Arc::new(()),
        cancellation: None,
    },
};

let _outcome = manager
    .start_task(TaskLaunchRequest::plain(None, request), ctx)
    .await?;
# Ok(())
# }
```

## Routing tasks to the background

`AsyncTaskManager` accepts a `TaskRoutingPolicy` that decides per-request
whether work runs in the foreground (blocking the turn) or in the background
(letting the turn continue while the task runs):

```rust,no_run
use std::time::Duration;

use agentkit_task_manager::{AsyncTaskManager, RoutingDecision};
use agentkit_tools_core::ToolRequest;

let manager = AsyncTaskManager::new().routing(|request: &ToolRequest| {
    match request.tool_name.0.as_str() {
        "long_running_search" => RoutingDecision::Background,
        "interactive_step"     => RoutingDecision::ForegroundThenDetachAfter(
            Duration::from_secs(5),
        ),
        _                      => RoutingDecision::Foreground,
    }
});
# let _ = manager;
```

The handle returned by `manager.handle()` is what callers use to observe
events (`next_event`), cancel by id (`cancel`), tweak per-task delivery
(`set_delivery_mode`, `set_continue_policy`), and drain background results
delivered out-of-band (`drain_ready_items`).
