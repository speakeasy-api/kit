use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agentkit_core::{
    Item, MetadataMap, TaskId, ToolCallId, ToolResultPart, TurnCancellation, TurnId,
};
use agentkit_tools_core::{
    ApprovalRequest, OwnedToolContext, ToolError, ToolExecutionOutcome, ToolExecutor, ToolRequest,
};
use async_trait::async_trait;
use thiserror::Error;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::task::JoinHandle;

pub const TOOL_RESULT_FAILURE_KIND_METADATA_KEY: &str = "agentkit.tool.failure_kind";
pub const TOOL_RESULT_FAILURE_KIND_PERMISSION_DENIED: &str = "permission_denied";
/// Marks a synthetic error result whose tool never began executing (failed
/// lookup, proposed-request error, or permission-checker denial). Distinct
/// from [`TOOL_RESULT_FAILURE_KIND_METADATA_KEY`]: a tool can fail with a
/// permission denial mid-execution, in which case it *did* start.
pub const TOOL_RESULT_NOT_STARTED_METADATA_KEY: &str = "agentkit.tool.not_started";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Foreground,
    Background,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinuePolicy {
    NotifyOnly,
    RequestContinue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryMode {
    ToLoop,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub id: TaskId,
    pub turn_id: TurnId,
    pub call_id: ToolCallId,
    pub tool_name: String,
    pub kind: TaskKind,
    pub metadata: MetadataMap,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaskEvent {
    Started(TaskSnapshot),
    Detached(TaskSnapshot),
    Completed(TaskSnapshot, ToolResultPart),
    Cancelled(TaskSnapshot),
    Failed(TaskSnapshot, ToolError),
    ContinueRequested,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskApproval {
    pub task_id: TaskId,
    pub tool_request: ToolRequest,
    pub approval: ApprovalRequest,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaskResolution {
    Item(Item),
    Approval(TaskApproval),
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaskStartOutcome {
    Ready(Box<TaskResolution>),
    Pending { task_id: TaskId, kind: TaskKind },
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnTaskUpdate {
    Resolution(Box<TaskResolution>),
    Detached(TaskSnapshot),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PendingLoopUpdates {
    pub resolutions: VecDeque<TaskResolution>,
}

/// How a task should be invoked. Mutually exclusive between plain execution
/// and resuming after approval.
#[derive(Clone, Debug, Default)]
pub enum TaskLaunchKind {
    /// Execute the tool with the active permission policy.
    #[default]
    Plain,
    /// Re-execute a previously-interrupted call after the user approved it.
    Approved(ApprovalRequest),
}

#[derive(Clone, Debug)]
pub struct TaskLaunchRequest {
    pub task_id: Option<TaskId>,
    pub request: ToolRequest,
    pub kind: TaskLaunchKind,
}

impl TaskLaunchRequest {
    /// Plain launch (no prior approval / auth).
    pub fn plain(task_id: Option<TaskId>, request: ToolRequest) -> Self {
        Self {
            task_id,
            request,
            kind: TaskLaunchKind::Plain,
        }
    }

    /// Resume after the user approved the call.
    pub fn approved(
        task_id: Option<TaskId>,
        request: ToolRequest,
        approval: ApprovalRequest,
    ) -> Self {
        Self {
            task_id,
            request,
            kind: TaskLaunchKind::Approved(approval),
        }
    }
}

#[derive(Clone)]
pub struct TaskStartContext {
    pub executor: Arc<dyn ToolExecutor>,
    pub tool_context: OwnedToolContext,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum TaskManagerError {
    #[error("task not found: {0}")]
    NotFound(TaskId),
    #[error("task manager internal error: {0}")]
    Internal(String),
}

pub trait TaskRoutingPolicy: Send + Sync {
    fn route(&self, request: &ToolRequest) -> RoutingDecision;
}

impl<F> TaskRoutingPolicy for F
where
    F: Fn(&ToolRequest) -> RoutingDecision + Send + Sync,
{
    fn route(&self, request: &ToolRequest) -> RoutingDecision {
        self(request)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutingDecision {
    Foreground,
    Background,
    ForegroundThenDetachAfter(Duration),
}

struct DefaultRoutingPolicy;

impl TaskRoutingPolicy for DefaultRoutingPolicy {
    fn route(&self, _request: &ToolRequest) -> RoutingDecision {
        RoutingDecision::Foreground
    }
}

#[async_trait]
pub trait TaskManager: Send + Sync {
    async fn start_task(
        &self,
        request: TaskLaunchRequest,
        ctx: TaskStartContext,
    ) -> Result<TaskStartOutcome, TaskManagerError>;

    async fn wait_for_turn(
        &self,
        turn_id: &TurnId,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<TurnTaskUpdate>, TaskManagerError>;

    async fn take_pending_loop_updates(&self) -> Result<PendingLoopUpdates, TaskManagerError>;

    async fn on_turn_interrupted(&self, turn_id: &TurnId) -> Result<(), TaskManagerError>;

    fn handle(&self) -> TaskManagerHandle;
}

#[async_trait]
trait TaskManagerControl: Send + Sync {
    async fn next_event(&self) -> Option<TaskEvent>;
    async fn cancel(&self, task_id: TaskId) -> Result<(), TaskManagerError>;
    async fn list_running(&self) -> Vec<TaskSnapshot>;
    async fn list_completed(&self) -> Vec<TaskSnapshot>;
    async fn drain_ready_items(&self) -> Vec<Item>;
    async fn set_continue_policy(
        &self,
        task_id: TaskId,
        policy: ContinuePolicy,
    ) -> Result<(), TaskManagerError>;
    async fn set_delivery_mode(
        &self,
        task_id: TaskId,
        mode: DeliveryMode,
    ) -> Result<(), TaskManagerError>;
    async fn wait_for_idle(&self);
}

#[derive(Clone)]
pub struct TaskManagerHandle {
    inner: Arc<dyn TaskManagerControl>,
}

impl TaskManagerHandle {
    pub async fn next_event(&self) -> Option<TaskEvent> {
        self.inner.next_event().await
    }

    pub async fn cancel(&self, task_id: TaskId) -> Result<(), TaskManagerError> {
        self.inner.cancel(task_id).await
    }

    pub async fn list_running(&self) -> Vec<TaskSnapshot> {
        self.inner.list_running().await
    }

    pub async fn list_completed(&self) -> Vec<TaskSnapshot> {
        self.inner.list_completed().await
    }

    pub async fn drain_ready_items(&self) -> Vec<Item> {
        self.inner.drain_ready_items().await
    }

    pub async fn set_continue_policy(
        &self,
        task_id: TaskId,
        policy: ContinuePolicy,
    ) -> Result<(), TaskManagerError> {
        self.inner.set_continue_policy(task_id, policy).await
    }

    pub async fn set_delivery_mode(
        &self,
        task_id: TaskId,
        mode: DeliveryMode,
    ) -> Result<(), TaskManagerError> {
        self.inner.set_delivery_mode(task_id, mode).await
    }

    /// Wait until all running tasks have completed.
    pub async fn wait_for_idle(&self) {
        self.inner.wait_for_idle().await
    }
}

pub struct SimpleTaskManager {
    state: Arc<HandleState>,
}

impl SimpleTaskManager {
    pub fn new() -> Self {
        Self {
            state: Arc::new(HandleState::default()),
        }
    }
}

impl Default for SimpleTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TaskManager for SimpleTaskManager {
    async fn start_task(
        &self,
        request: TaskLaunchRequest,
        ctx: TaskStartContext,
    ) -> Result<TaskStartOutcome, TaskManagerError> {
        let task_id = request
            .task_id
            .clone()
            .unwrap_or_else(|| self.state.next_task_id());
        let outcome = match &request.kind {
            TaskLaunchKind::Approved(approved) => {
                ctx.executor
                    .execute_approved_owned(request.request.clone(), approved, ctx.tool_context)
                    .await
            }
            TaskLaunchKind::Plain => {
                ctx.executor
                    .execute_owned(request.request.clone(), ctx.tool_context)
                    .await
            }
        };
        Ok(TaskStartOutcome::Ready(Box::new(
            map_outcome_to_resolution(Some(task_id), request.request, outcome),
        )))
    }

    async fn wait_for_turn(
        &self,
        _turn_id: &TurnId,
        _cancellation: Option<TurnCancellation>,
    ) -> Result<Option<TurnTaskUpdate>, TaskManagerError> {
        Ok(None)
    }

    async fn take_pending_loop_updates(&self) -> Result<PendingLoopUpdates, TaskManagerError> {
        Ok(PendingLoopUpdates::default())
    }

    async fn on_turn_interrupted(&self, _turn_id: &TurnId) -> Result<(), TaskManagerError> {
        Ok(())
    }

    fn handle(&self) -> TaskManagerHandle {
        TaskManagerHandle {
            inner: self.state.clone(),
        }
    }
}

#[derive(Default)]
struct HandleState {
    next_task_index: AtomicU64,
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<TaskEvent>>>,
}

impl HandleState {
    fn next_task_id(&self) -> TaskId {
        let next = self.next_task_index.fetch_add(1, Ordering::SeqCst) + 1;
        TaskId::new(format!("task-{}", next))
    }
}

#[async_trait]
impl TaskManagerControl for HandleState {
    async fn next_event(&self) -> Option<TaskEvent> {
        let mut rx = self.events_rx.lock().await;
        match rx.as_mut() {
            Some(inner) => inner.recv().await,
            None => None,
        }
    }

    async fn cancel(&self, task_id: TaskId) -> Result<(), TaskManagerError> {
        Err(TaskManagerError::NotFound(task_id))
    }

    async fn list_running(&self) -> Vec<TaskSnapshot> {
        Vec::new()
    }

    async fn list_completed(&self) -> Vec<TaskSnapshot> {
        Vec::new()
    }

    async fn drain_ready_items(&self) -> Vec<Item> {
        Vec::new()
    }

    async fn set_continue_policy(
        &self,
        task_id: TaskId,
        _policy: ContinuePolicy,
    ) -> Result<(), TaskManagerError> {
        Err(TaskManagerError::NotFound(task_id))
    }

    async fn set_delivery_mode(
        &self,
        task_id: TaskId,
        _mode: DeliveryMode,
    ) -> Result<(), TaskManagerError> {
        Err(TaskManagerError::NotFound(task_id))
    }

    async fn wait_for_idle(&self) {}
}

pub struct AsyncTaskManager {
    inner: Arc<AsyncInner>,
    routing: Arc<dyn TaskRoutingPolicy>,
}

impl AsyncTaskManager {
    pub fn new() -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(AsyncInner {
                state: Mutex::new(AsyncState::default()),
                host_event_tx: event_tx,
                host_event_rx: Mutex::new(event_rx),
                notify: Notify::new(),
            }),
            routing: Arc::new(DefaultRoutingPolicy),
        }
    }

    pub fn routing(mut self, policy: impl TaskRoutingPolicy + 'static) -> Self {
        self.routing = Arc::new(policy);
        self
    }
}

impl Default for AsyncTaskManager {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct AsyncState {
    next_task_index: u64,
    tasks: BTreeMap<TaskId, TaskRecord>,
    per_turn_running: BTreeMap<TurnId, usize>,
    per_turn_updates: BTreeMap<TurnId, VecDeque<TurnTaskUpdate>>,
    pending_loop_updates: VecDeque<TaskResolution>,
    manual_ready_items: Vec<Item>,
}

struct TaskRecord {
    snapshot: TaskSnapshot,
    continue_policy: ContinuePolicy,
    delivery_mode: DeliveryMode,
    running: bool,
    completed: bool,
    join: Option<JoinHandle<()>>,
}

struct AsyncInner {
    state: Mutex<AsyncState>,
    host_event_tx: mpsc::UnboundedSender<TaskEvent>,
    host_event_rx: Mutex<mpsc::UnboundedReceiver<TaskEvent>>,
    notify: Notify,
}

impl AsyncInner {
    async fn next_task_id(&self) -> TaskId {
        let mut state = self.state.lock().await;
        state.next_task_index += 1;
        TaskId::new(format!("task-{}", state.next_task_index))
    }
}

#[async_trait]
impl TaskManager for AsyncTaskManager {
    async fn start_task(
        &self,
        request: TaskLaunchRequest,
        ctx: TaskStartContext,
    ) -> Result<TaskStartOutcome, TaskManagerError> {
        let route = self.routing.route(&request.request);
        let task_id = match request.task_id.clone() {
            Some(existing) => existing,
            None => self.inner.next_task_id().await,
        };
        let initial_kind = match route {
            RoutingDecision::Background => TaskKind::Background,
            _ => TaskKind::Foreground,
        };
        let snapshot = TaskSnapshot {
            id: task_id.clone(),
            turn_id: request.request.turn_id.clone(),
            call_id: request.request.call_id.clone(),
            tool_name: request.request.tool_name.to_string(),
            kind: initial_kind,
            metadata: request.request.metadata.clone(),
        };
        let _ = self
            .inner
            .host_event_tx
            .send(TaskEvent::Started(snapshot.clone()));

        let mut state = self.inner.state.lock().await;
        state.tasks.insert(
            task_id.clone(),
            TaskRecord {
                snapshot: snapshot.clone(),
                continue_policy: ContinuePolicy::NotifyOnly,
                delivery_mode: DeliveryMode::ToLoop,
                running: true,
                completed: false,
                join: None,
            },
        );
        if initial_kind == TaskKind::Foreground {
            *state
                .per_turn_running
                .entry(snapshot.turn_id.clone())
                .or_default() += 1;
        }
        drop(state);

        let event_tx = self.inner.host_event_tx.clone();
        let inner = self.inner.clone();
        let task_id_for_future = task_id.clone();
        let turn_id = snapshot.turn_id.clone();
        let kind = request.kind.clone();
        let exec_request = request.request.clone();
        let owned_ctx = ctx.tool_context.clone();
        let executor = ctx.executor.clone();
        let route_copy = route;
        let join = tokio::spawn(async move {
            if let RoutingDecision::ForegroundThenDetachAfter(duration) = route_copy {
                let event_tx = event_tx.clone();
                let inner = inner.clone();
                let task_id = task_id_for_future.clone();
                let turn_id = turn_id.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(duration).await;
                    let mut state = inner.state.lock().await;
                    let snapshot = if let Some(record) = state.tasks.get_mut(&task_id)
                        && record.running
                        && record.snapshot.kind == TaskKind::Foreground
                    {
                        record.snapshot.kind = TaskKind::Background;
                        Some(record.snapshot.clone())
                    } else {
                        None
                    };
                    if let Some(snapshot) = snapshot {
                        if let Some(count) = state.per_turn_running.get_mut(&turn_id) {
                            *count = count.saturating_sub(1);
                            if *count == 0 {
                                state.per_turn_running.remove(&turn_id);
                            }
                        }
                        state
                            .per_turn_updates
                            .entry(turn_id.clone())
                            .or_default()
                            .push_back(TurnTaskUpdate::Detached(snapshot.clone()));
                        let _ = event_tx.send(TaskEvent::Detached(snapshot));
                        inner.notify.notify_waiters();
                    }
                });
            }

            let outcome = match &kind {
                TaskLaunchKind::Approved(approval) => {
                    executor
                        .execute_approved_owned(exec_request.clone(), approval, owned_ctx)
                        .await
                }
                TaskLaunchKind::Plain => {
                    executor
                        .execute_owned(exec_request.clone(), owned_ctx)
                        .await
                }
            };

            let resolution =
                map_outcome_to_resolution(Some(task_id_for_future.clone()), exec_request, outcome);
            let completed_result = match &resolution {
                TaskResolution::Item(item) => item.parts.iter().find_map(|part| match part {
                    agentkit_core::Part::ToolResult(result) => Some(result.clone()),
                    _ => None,
                }),
                TaskResolution::Approval(_) => None,
            };

            let (snapshot, should_request_continue) = {
                let mut state = inner.state.lock().await;
                let Some(record) = state.tasks.get_mut(&task_id_for_future) else {
                    return;
                };
                record.running = false;
                record.completed = true;
                let snapshot = record.snapshot.clone();
                let continue_policy = record.continue_policy;
                let delivery_mode = record.delivery_mode;
                let current_kind = snapshot.kind;

                if current_kind == TaskKind::Foreground {
                    if let Some(count) = state.per_turn_running.get_mut(&turn_id) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            state.per_turn_running.remove(&turn_id);
                        }
                    }
                    state
                        .per_turn_updates
                        .entry(turn_id.clone())
                        .or_default()
                        .push_back(TurnTaskUpdate::Resolution(Box::new(resolution.clone())));
                } else {
                    match &resolution {
                        TaskResolution::Item(_) if delivery_mode == DeliveryMode::ToLoop => {
                            state.pending_loop_updates.push_back(resolution.clone());
                        }
                        TaskResolution::Approval(_) if delivery_mode == DeliveryMode::ToLoop => {
                            state.pending_loop_updates.push_back(resolution.clone());
                        }
                        TaskResolution::Item(item) => {
                            state.manual_ready_items.push(item.clone());
                        }
                        TaskResolution::Approval(_) => {}
                    }
                }

                (
                    snapshot,
                    current_kind == TaskKind::Background
                        && delivery_mode == DeliveryMode::ToLoop
                        && continue_policy == ContinuePolicy::RequestContinue,
                )
            };

            if let Some(result) = completed_result {
                let _ = event_tx.send(TaskEvent::Completed(snapshot.clone(), result));
            }
            if should_request_continue {
                let _ = event_tx.send(TaskEvent::ContinueRequested);
            }
            inner.notify.notify_waiters();
        });

        let mut state = self.inner.state.lock().await;
        if let Some(record) = state.tasks.get_mut(&task_id) {
            record.join = Some(join);
        }
        Ok(TaskStartOutcome::Pending {
            task_id,
            kind: initial_kind,
        })
    }

    async fn wait_for_turn(
        &self,
        turn_id: &TurnId,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<TurnTaskUpdate>, TaskManagerError> {
        loop {
            {
                let mut state = self.inner.state.lock().await;
                if let Some(queue) = state.per_turn_updates.get_mut(turn_id)
                    && let Some(update) = queue.pop_front()
                {
                    return Ok(Some(update));
                }
                if state
                    .per_turn_running
                    .get(turn_id)
                    .copied()
                    .unwrap_or_default()
                    == 0
                {
                    return Ok(None);
                }
            }
            if cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled)
            {
                return Ok(None);
            }
            if let Some(cancellation) = cancellation.as_ref() {
                tokio::select! {
                    _ = self.inner.notify.notified() => {}
                    _ = cancellation.cancelled() => return Ok(None),
                }
            } else {
                self.inner.notify.notified().await;
            }
        }
    }

    async fn take_pending_loop_updates(&self) -> Result<PendingLoopUpdates, TaskManagerError> {
        let mut state = self.inner.state.lock().await;
        Ok(PendingLoopUpdates {
            resolutions: std::mem::take(&mut state.pending_loop_updates),
        })
    }

    async fn on_turn_interrupted(&self, turn_id: &TurnId) -> Result<(), TaskManagerError> {
        let mut state = self.inner.state.lock().await;
        let interrupted: Vec<TaskId> = state
            .tasks
            .iter()
            .filter_map(|(id, record)| {
                (record.snapshot.turn_id == *turn_id
                    && record.snapshot.kind == TaskKind::Foreground
                    && record.running)
                    .then_some(id.clone())
            })
            .collect();
        for task_id in interrupted {
            if let Some(record) = state.tasks.get_mut(&task_id) {
                record.running = false;
                if let Some(join) = record.join.take() {
                    join.abort();
                }
                let snapshot = record.snapshot.clone();
                let _ = self
                    .inner
                    .host_event_tx
                    .send(TaskEvent::Cancelled(snapshot));
            }
        }
        state.per_turn_running.remove(turn_id);
        self.inner.notify.notify_waiters();
        Ok(())
    }

    fn handle(&self) -> TaskManagerHandle {
        TaskManagerHandle {
            inner: self.inner.clone(),
        }
    }
}

#[async_trait]
impl TaskManagerControl for AsyncInner {
    async fn next_event(&self) -> Option<TaskEvent> {
        self.host_event_rx.lock().await.recv().await
    }

    async fn cancel(&self, task_id: TaskId) -> Result<(), TaskManagerError> {
        let mut state = self.state.lock().await;
        let record = state
            .tasks
            .get_mut(&task_id)
            .ok_or_else(|| TaskManagerError::NotFound(task_id.clone()))?;
        if let Some(join) = record.join.take() {
            join.abort();
        }
        record.running = false;
        let snapshot = record.snapshot.clone();
        if record.snapshot.kind == TaskKind::Foreground
            && let Some(count) = state.per_turn_running.get_mut(&snapshot.turn_id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.per_turn_running.remove(&snapshot.turn_id);
            }
        }
        let _ = self.host_event_tx.send(TaskEvent::Cancelled(snapshot));
        self.notify.notify_waiters();
        Ok(())
    }

    async fn list_running(&self) -> Vec<TaskSnapshot> {
        let state = self.state.lock().await;
        state
            .tasks
            .values()
            .filter(|record| record.running)
            .map(|record| record.snapshot.clone())
            .collect()
    }

    async fn list_completed(&self) -> Vec<TaskSnapshot> {
        let state = self.state.lock().await;
        state
            .tasks
            .values()
            .filter(|record| record.completed)
            .map(|record| record.snapshot.clone())
            .collect()
    }

    async fn drain_ready_items(&self) -> Vec<Item> {
        let mut state = self.state.lock().await;
        std::mem::take(&mut state.manual_ready_items)
    }

    async fn set_continue_policy(
        &self,
        task_id: TaskId,
        policy: ContinuePolicy,
    ) -> Result<(), TaskManagerError> {
        let mut state = self.state.lock().await;
        let record = state
            .tasks
            .get_mut(&task_id)
            .ok_or_else(|| TaskManagerError::NotFound(task_id.clone()))?;
        record.continue_policy = policy;
        Ok(())
    }

    async fn set_delivery_mode(
        &self,
        task_id: TaskId,
        mode: DeliveryMode,
    ) -> Result<(), TaskManagerError> {
        let mut state = self.state.lock().await;
        let record = state
            .tasks
            .get_mut(&task_id)
            .ok_or_else(|| TaskManagerError::NotFound(task_id.clone()))?;
        record.delivery_mode = mode;
        Ok(())
    }

    async fn wait_for_idle(&self) {
        loop {
            {
                let state = self.state.lock().await;
                if !state.tasks.values().any(|r| r.running) {
                    return;
                }
            }
            self.notify.notified().await;
        }
    }
}

fn map_outcome_to_resolution(
    task_id: Option<TaskId>,
    request: ToolRequest,
    outcome: ToolExecutionOutcome,
) -> TaskResolution {
    match outcome {
        ToolExecutionOutcome::Completed(result) => TaskResolution::Item(Item {
            id: None,
            kind: agentkit_core::ItemKind::Tool,
            parts: vec![agentkit_core::Part::ToolResult(result.result)],
            metadata: result.metadata,
            usage: None,
            finish_reason: None,
            created_at: None,
        }),
        ToolExecutionOutcome::Interrupted(
            agentkit_tools_core::ToolInterruption::ApprovalRequired(mut approval),
        ) => {
            let task_id = task_id.unwrap_or_default();
            approval.task_id = Some(task_id.clone());
            TaskResolution::Approval(TaskApproval {
                task_id,
                tool_request: request,
                approval,
            })
        }
        ToolExecutionOutcome::FailedBeforeInvocation(error) => {
            let mut metadata = request.metadata;
            metadata.insert(TOOL_RESULT_NOT_STARTED_METADATA_KEY.into(), true.into());
            if matches!(error, ToolError::PermissionDenied(_)) {
                metadata.insert(
                    TOOL_RESULT_FAILURE_KIND_METADATA_KEY.into(),
                    TOOL_RESULT_FAILURE_KIND_PERMISSION_DENIED.into(),
                );
            }
            TaskResolution::Item(Item {
                id: None,
                kind: agentkit_core::ItemKind::Tool,
                parts: vec![agentkit_core::Part::ToolResult(ToolResultPart {
                    call_id: request.call_id,
                    output: agentkit_core::ToolOutput::Text(error.to_string()),
                    is_error: true,
                    metadata,
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            })
        }
        ToolExecutionOutcome::Failed(error) => {
            let mut metadata = request.metadata;
            if matches!(error, ToolError::PermissionDenied(_)) {
                metadata.insert(
                    TOOL_RESULT_FAILURE_KIND_METADATA_KEY.into(),
                    TOOL_RESULT_FAILURE_KIND_PERMISSION_DENIED.into(),
                );
            }
            TaskResolution::Item(Item {
                id: None,
                kind: agentkit_core::ItemKind::Tool,
                parts: vec![agentkit_core::Part::ToolResult(ToolResultPart {
                    call_id: request.call_id,
                    output: agentkit_core::ToolOutput::Text(error.to_string()),
                    is_error: true,
                    metadata,
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc as StdArc;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    use agentkit_core::{
        CancellationController, ItemKind, Part, SessionId, ToolOutput, TurnCancellation,
    };
    use agentkit_tools_core::{
        ApprovalReason, PermissionChecker, PermissionDecision, ToolAnnotations, ToolInterruption,
        ToolName, ToolResult, ToolSpec,
    };
    use serde_json::json;
    use tokio::sync::Notify;
    use tokio::time::{Duration, timeout};

    use super::*;

    struct AllowAllPermissions;

    impl PermissionChecker for AllowAllPermissions {
        fn evaluate(
            &self,
            _request: &dyn agentkit_tools_core::PermissionRequest,
        ) -> PermissionDecision {
            PermissionDecision::Allow
        }
    }

    #[derive(Clone)]
    enum TestBehavior {
        Block {
            entered: StdArc<AtomicBool>,
            release: StdArc<Notify>,
            output: &'static str,
        },
        Approval,
    }

    #[derive(Clone)]
    struct TestExecutor {
        behaviors: BTreeMap<String, TestBehavior>,
    }

    impl TestExecutor {
        fn new(behaviors: impl IntoIterator<Item = (impl Into<String>, TestBehavior)>) -> Self {
            Self {
                behaviors: behaviors
                    .into_iter()
                    .map(|(name, behavior)| (name.into(), behavior))
                    .collect(),
            }
        }
    }

    #[async_trait]
    impl ToolExecutor for TestExecutor {
        fn specs(&self) -> Vec<ToolSpec> {
            self.behaviors
                .keys()
                .map(|name| ToolSpec {
                    name: ToolName::new(name),
                    description: format!("test tool {name}"),
                    input_schema: json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }),
                    output_schema: None,
                    annotations: ToolAnnotations::default(),
                    metadata: MetadataMap::new(),
                })
                .collect()
        }

        async fn execute(
            &self,
            request: ToolRequest,
            _ctx: &mut agentkit_tools_core::ToolContext<'_>,
        ) -> ToolExecutionOutcome {
            match self.behaviors.get(request.tool_name.0.as_str()) {
                Some(TestBehavior::Block {
                    entered,
                    release,
                    output,
                }) => {
                    entered.store(true, AtomicOrdering::SeqCst);
                    release.notified().await;
                    ToolExecutionOutcome::Completed(ToolResult {
                        result: ToolResultPart {
                            call_id: request.call_id,
                            output: ToolOutput::Text((*output).into()),
                            is_error: false,
                            metadata: request.metadata,
                        },
                        duration: None,
                        metadata: MetadataMap::new(),
                    })
                }
                Some(TestBehavior::Approval) => ToolExecutionOutcome::Interrupted(
                    ToolInterruption::ApprovalRequired(ApprovalRequest {
                        task_id: None,
                        call_id: Some(request.call_id.clone()),
                        id: "approval:test".into(),
                        request_kind: "tool.test".into(),
                        reason: ApprovalReason::SensitivePath,
                        summary: "requires approval".into(),
                        metadata: MetadataMap::new(),
                    }),
                ),
                None => ToolExecutionOutcome::Failed(ToolError::Unavailable(
                    request.tool_name.0.clone(),
                )),
            }
        }
    }

    struct NameRoutingPolicy {
        routes: BTreeMap<String, RoutingDecision>,
    }

    impl NameRoutingPolicy {
        fn new(routes: impl IntoIterator<Item = (impl Into<String>, RoutingDecision)>) -> Self {
            Self {
                routes: routes
                    .into_iter()
                    .map(|(name, decision)| (name.into(), decision))
                    .collect(),
            }
        }
    }

    impl TaskRoutingPolicy for NameRoutingPolicy {
        fn route(&self, request: &ToolRequest) -> RoutingDecision {
            self.routes
                .get(request.tool_name.0.as_str())
                .copied()
                .unwrap_or(RoutingDecision::Foreground)
        }
    }

    fn make_request(tool_name: &str, turn_id: &str, call_id: &str) -> ToolRequest {
        ToolRequest {
            call_id: ToolCallId::new(call_id),
            tool_name: ToolName::new(tool_name),
            input: json!({}),
            session_id: SessionId::new("session-1"),
            turn_id: TurnId::new(turn_id),
            metadata: MetadataMap::new(),
        }
    }

    fn make_context(
        executor: Arc<dyn ToolExecutor>,
        turn_id: &TurnId,
        cancellation: Option<TurnCancellation>,
    ) -> TaskStartContext {
        TaskStartContext {
            executor,
            tool_context: OwnedToolContext {
                session_id: SessionId::new("session-1"),
                turn_id: turn_id.clone(),
                metadata: MetadataMap::new(),
                permissions: Arc::new(AllowAllPermissions),
                resources: Arc::new(()),
                cancellation,
                execution_scope: None,
                approved_request: None,
            },
        }
    }

    async fn next_event(handle: &TaskManagerHandle) -> TaskEvent {
        timeout(Duration::from_secs(1), handle.next_event())
            .await
            .expect("timed out waiting for task event")
            .expect("task event stream ended unexpectedly")
    }

    async fn wait_until_entered(entered: &AtomicBool) {
        timeout(Duration::from_secs(1), async {
            while !entered.load(AtomicOrdering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task never entered execution");
    }

    #[tokio::test]
    async fn simple_task_manager_executes_inline_and_assigns_task_ids() {
        let manager = SimpleTaskManager::new();
        let executor: Arc<dyn ToolExecutor> = Arc::new(TestExecutor::new([(
            "needs-approval",
            TestBehavior::Approval,
        )]));
        let request = make_request("needs-approval", "turn-1", "call-1");

        let outcome = manager
            .start_task(
                TaskLaunchRequest {
                    task_id: None,
                    request: request.clone(),
                    kind: TaskLaunchKind::Plain,
                },
                make_context(executor, &request.turn_id, None),
            )
            .await
            .unwrap();

        match outcome {
            TaskStartOutcome::Ready(resolution) => match *resolution {
                TaskResolution::Approval(task) => {
                    assert!(!task.task_id.0.is_empty());
                    assert_eq!(task.approval.task_id.as_ref(), Some(&task.task_id));
                    assert_eq!(task.tool_request.call_id, request.call_id);
                }
                other => panic!("unexpected task resolution: {other:?}"),
            },
            other => panic!("unexpected start outcome: {other:?}"),
        }

        assert!(manager.handle().list_running().await.is_empty());
    }

    #[tokio::test]
    async fn async_manager_interrupt_cancels_foreground_only() {
        let fg_release = StdArc::new(Notify::new());
        let fg_entered = StdArc::new(AtomicBool::new(false));
        let bg_release = StdArc::new(Notify::new());
        let bg_entered = StdArc::new(AtomicBool::new(false));
        let executor: Arc<dyn ToolExecutor> = Arc::new(TestExecutor::new([
            (
                "foreground",
                TestBehavior::Block {
                    entered: fg_entered.clone(),
                    release: fg_release.clone(),
                    output: "foreground-done",
                },
            ),
            (
                "background",
                TestBehavior::Block {
                    entered: bg_entered.clone(),
                    release: bg_release.clone(),
                    output: "background-done",
                },
            ),
        ]));
        let manager = AsyncTaskManager::new().routing(NameRoutingPolicy::new([
            ("foreground", RoutingDecision::Foreground),
            ("background", RoutingDecision::Background),
        ]));
        let handle = manager.handle();
        let turn_id = TurnId::new("turn-1");

        let foreground = manager
            .start_task(
                TaskLaunchRequest {
                    task_id: None,
                    request: make_request("foreground", "turn-1", "call-fg"),
                    kind: TaskLaunchKind::Plain,
                },
                make_context(executor.clone(), &turn_id, None),
            )
            .await
            .unwrap();
        let background = manager
            .start_task(
                TaskLaunchRequest {
                    task_id: None,
                    request: make_request("background", "turn-1", "call-bg"),
                    kind: TaskLaunchKind::Plain,
                },
                make_context(executor.clone(), &turn_id, None),
            )
            .await
            .unwrap();

        assert!(matches!(
            foreground,
            TaskStartOutcome::Pending {
                kind: TaskKind::Foreground,
                ..
            }
        ));
        let background_id = match background {
            TaskStartOutcome::Pending {
                task_id,
                kind: TaskKind::Background,
            } => task_id,
            other => panic!("unexpected background outcome: {other:?}"),
        };

        let _ = next_event(&handle).await;
        let _ = next_event(&handle).await;
        wait_until_entered(fg_entered.as_ref()).await;
        wait_until_entered(bg_entered.as_ref()).await;

        manager.on_turn_interrupted(&turn_id).await.unwrap();

        match next_event(&handle).await {
            TaskEvent::Cancelled(snapshot) => assert_eq!(snapshot.tool_name, "foreground"),
            other => panic!("unexpected event after interrupt: {other:?}"),
        }

        let running = handle.list_running().await;
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, background_id);
        assert_eq!(running[0].tool_name, "background");

        bg_release.notify_waiters();
        match next_event(&handle).await {
            TaskEvent::Completed(snapshot, result) => {
                assert_eq!(snapshot.id, background_id);
                assert_eq!(result.output, ToolOutput::Text("background-done".into()));
            }
            other => panic!("unexpected completion event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn async_manager_can_cancel_background_tasks_by_id() {
        let release = StdArc::new(Notify::new());
        let entered = StdArc::new(AtomicBool::new(false));
        let executor: Arc<dyn ToolExecutor> = Arc::new(TestExecutor::new([(
            "background",
            TestBehavior::Block {
                entered: entered.clone(),
                release,
                output: "done",
            },
        )]));
        let manager = AsyncTaskManager::new().routing(NameRoutingPolicy::new([(
            "background",
            RoutingDecision::Background,
        )]));
        let handle = manager.handle();
        let request = make_request("background", "turn-1", "call-1");

        let task_id = match manager
            .start_task(
                TaskLaunchRequest {
                    task_id: None,
                    request: request.clone(),
                    kind: TaskLaunchKind::Plain,
                },
                make_context(executor, &request.turn_id, None),
            )
            .await
            .unwrap()
        {
            TaskStartOutcome::Pending { task_id, .. } => task_id,
            other => panic!("unexpected start outcome: {other:?}"),
        };

        let _ = next_event(&handle).await;
        wait_until_entered(entered.as_ref()).await;
        handle.cancel(task_id.clone()).await.unwrap();

        match next_event(&handle).await {
            TaskEvent::Cancelled(snapshot) => assert_eq!(snapshot.id, task_id),
            other => panic!("unexpected event after cancel: {other:?}"),
        }

        assert!(handle.list_running().await.is_empty());
    }

    #[tokio::test]
    async fn async_manager_manual_delivery_keeps_results_out_of_loop_updates() {
        let release = StdArc::new(Notify::new());
        let entered = StdArc::new(AtomicBool::new(false));
        let executor: Arc<dyn ToolExecutor> = Arc::new(TestExecutor::new([(
            "background",
            TestBehavior::Block {
                entered: entered.clone(),
                release: release.clone(),
                output: "manual-done",
            },
        )]));
        let manager = AsyncTaskManager::new().routing(NameRoutingPolicy::new([(
            "background",
            RoutingDecision::Background,
        )]));
        let handle = manager.handle();
        let request = make_request("background", "turn-1", "call-1");

        let task_id = match manager
            .start_task(
                TaskLaunchRequest {
                    task_id: None,
                    request: request.clone(),
                    kind: TaskLaunchKind::Plain,
                },
                make_context(executor, &request.turn_id, None),
            )
            .await
            .unwrap()
        {
            TaskStartOutcome::Pending { task_id, .. } => task_id,
            other => panic!("unexpected start outcome: {other:?}"),
        };

        let _ = next_event(&handle).await;
        wait_until_entered(entered.as_ref()).await;
        handle
            .set_continue_policy(task_id.clone(), ContinuePolicy::RequestContinue)
            .await
            .unwrap();
        handle
            .set_delivery_mode(task_id, DeliveryMode::Manual)
            .await
            .unwrap();

        release.notify_waiters();
        match next_event(&handle).await {
            TaskEvent::Completed(_, result) => {
                assert_eq!(result.output, ToolOutput::Text("manual-done".into()))
            }
            other => panic!("unexpected event: {other:?}"),
        }

        assert!(
            timeout(Duration::from_millis(50), handle.next_event())
                .await
                .is_err()
        );
        assert!(
            manager
                .take_pending_loop_updates()
                .await
                .unwrap()
                .resolutions
                .is_empty()
        );

        let ready_items = handle.drain_ready_items().await;
        assert_eq!(ready_items.len(), 1);
        assert_eq!(ready_items[0].kind, ItemKind::Tool);
        match &ready_items[0].parts[0] {
            Part::ToolResult(result) => {
                assert_eq!(result.output, ToolOutput::Text("manual-done".into()))
            }
            other => panic!("unexpected ready item: {other:?}"),
        }
    }

    #[tokio::test]
    async fn async_manager_to_loop_delivery_can_request_continue() {
        let release = StdArc::new(Notify::new());
        let entered = StdArc::new(AtomicBool::new(false));
        let executor: Arc<dyn ToolExecutor> = Arc::new(TestExecutor::new([(
            "background",
            TestBehavior::Block {
                entered: entered.clone(),
                release: release.clone(),
                output: "loop-done",
            },
        )]));
        let manager = AsyncTaskManager::new().routing(NameRoutingPolicy::new([(
            "background",
            RoutingDecision::Background,
        )]));
        let handle = manager.handle();
        let request = make_request("background", "turn-1", "call-1");

        let task_id = match manager
            .start_task(
                TaskLaunchRequest {
                    task_id: None,
                    request: request.clone(),
                    kind: TaskLaunchKind::Plain,
                },
                make_context(
                    executor,
                    &request.turn_id,
                    Some(TurnCancellation::new(
                        CancellationController::new().handle(),
                    )),
                ),
            )
            .await
            .unwrap()
        {
            TaskStartOutcome::Pending { task_id, .. } => task_id,
            other => panic!("unexpected start outcome: {other:?}"),
        };

        let _ = next_event(&handle).await;
        wait_until_entered(entered.as_ref()).await;
        handle
            .set_continue_policy(task_id, ContinuePolicy::RequestContinue)
            .await
            .unwrap();

        release.notify_waiters();
        match next_event(&handle).await {
            TaskEvent::Completed(_, result) => {
                assert_eq!(result.output, ToolOutput::Text("loop-done".into()))
            }
            other => panic!("unexpected completion event: {other:?}"),
        }
        match next_event(&handle).await {
            TaskEvent::ContinueRequested => {}
            other => panic!("unexpected follow-up event: {other:?}"),
        }

        let updates = manager.take_pending_loop_updates().await.unwrap();
        assert_eq!(updates.resolutions.len(), 1);
        assert!(handle.drain_ready_items().await.is_empty());
    }

    #[tokio::test]
    async fn wait_for_idle_returns_after_loop_updates_are_queued() {
        let release = StdArc::new(Notify::new());
        let entered = StdArc::new(AtomicBool::new(false));
        let executor: Arc<dyn ToolExecutor> = Arc::new(TestExecutor::new([(
            "background",
            TestBehavior::Block {
                entered: entered.clone(),
                release: release.clone(),
                output: "idle-done",
            },
        )]));
        let manager = AsyncTaskManager::new().routing(NameRoutingPolicy::new([(
            "background",
            RoutingDecision::Background,
        )]));
        let handle = manager.handle();
        let request = make_request("background", "turn-1", "call-1");

        let outcome = manager
            .start_task(
                TaskLaunchRequest {
                    task_id: None,
                    request: request.clone(),
                    kind: TaskLaunchKind::Plain,
                },
                make_context(executor, &request.turn_id, None),
            )
            .await
            .unwrap();
        assert!(matches!(outcome, TaskStartOutcome::Pending { .. }));

        let _ = next_event(&handle).await;
        wait_until_entered(entered.as_ref()).await;
        release.notify_waiters();

        timeout(Duration::from_secs(1), handle.wait_for_idle())
            .await
            .expect("wait_for_idle timed out");

        let updates = manager.take_pending_loop_updates().await.unwrap();
        assert_eq!(updates.resolutions.len(), 1);
        match &updates.resolutions[0] {
            TaskResolution::Item(item) => match &item.parts[0] {
                Part::ToolResult(result) => {
                    assert_eq!(result.call_id, request.call_id);
                    assert_eq!(result.output, ToolOutput::Text("idle-done".into()));
                }
                other => panic!("unexpected tool item: {other:?}"),
            },
            other => panic!("unexpected pending update: {other:?}"),
        }
    }
}
