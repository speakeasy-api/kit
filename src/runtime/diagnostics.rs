//! Carry producer identity through dependency-owned task and Runlet spawns.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use agentkit_core::{ToolCallId, TurnCancellation, TurnId};
use agentkit_loop::{AgentEvent, LoopObserver, ObservedEvent};
use agentkit_task_manager::{
    AsyncTaskManager, PendingLoopUpdates, TaskLaunchRequest, TaskManager, TaskManagerError,
    TaskManagerHandle, TaskStartContext, TaskStartOutcome, TurnTaskUpdate,
};
use agentkit_tools_core::{
    ApprovalRequest, OwnedToolContext, ToolCatalogEvent, ToolContext, ToolExecutionOutcome,
    ToolExecutor, ToolRequest, ToolSpec,
};
use async_trait::async_trait;

use crate::events::DiagnosticScope;

/// Ephemeral producer ownership, shared with the actor through BackgroundJobs.
/// Retain entries for the driver lifetime: approvals can restart the same call,
/// and a terminal event can precede consumption by the loop.
#[derive(Clone, Default)]
pub(crate) struct TaskOrigins(Arc<Mutex<TaskOriginState>>);

#[derive(Default)]
struct TaskOriginState {
    origins: HashMap<ToolCallId, DiagnosticScope>,
}

impl TaskOrigins {
    pub(crate) fn get(&self, call_id: &ToolCallId) -> Option<DiagnosticScope> {
        self.0
            .lock()
            .expect("task origins poisoned")
            .origins
            .get(call_id)
            .cloned()
    }

    fn proposed_origin(&self, call_id: &ToolCallId) -> DiagnosticScope {
        self.get(call_id).unwrap_or_else(DiagnosticScope::capture)
    }

    fn accepted(&self, call_id: ToolCallId, origin: DiagnosticScope) {
        let mut state = self.0.lock().expect("task origins poisoned");
        state.origins.entry(call_id).or_insert(origin);
    }
}

/// A coalesced continuation has an owner only when every cause agrees.
#[derive(Clone, Default)]
pub(crate) struct ContinuationOrigin(Option<DiagnosticScope>);

impl ContinuationOrigin {
    pub(crate) fn include(&mut self, origin: Option<DiagnosticScope>) {
        let origin = origin.unwrap_or_default();
        self.0 = Some(match self.0.take() {
            None => origin,
            Some(previous) if previous == origin => previous,
            Some(_) => DiagnosticScope::default(),
        });
    }

    pub(crate) fn scope(&self) -> DiagnosticScope {
        self.0.clone().unwrap_or_default()
    }
}

tokio::task_local! {
    static CONSUMPTION: ConsumptionScope;
}

/// Ownership of inputs actually presented to this drive. Task-manager handoff
/// alone is not consumption: LoopDriver buffers updates behind fresh input.
#[derive(Clone)]
pub(crate) struct ConsumptionScope {
    causes: Arc<Mutex<ContinuationOrigin>>,
    initial: DiagnosticScope,
    refresh_seed: Arc<std::sync::atomic::AtomicBool>,
}

impl ConsumptionScope {
    /// None admits background work with no assumed cause; Some seeds a real
    /// explicit request, or default() for unsolicited external input.
    pub(crate) fn new(seed: Option<DiagnosticScope>) -> Self {
        let initial = seed.clone().unwrap_or_default();
        Self {
            causes: Arc::new(Mutex::new(ContinuationOrigin(seed))),
            initial,
            refresh_seed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// v2 establishes local activation inside run_active_turn, after admission.
    pub(crate) fn explicit(origin: DiagnosticScope) -> Self {
        let scope = Self::new(Some(origin));
        scope
            .refresh_seed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        scope
    }

    pub(crate) fn bind_local_seed() {
        let _ = CONSUMPTION.try_with(|scope| {
            if scope
                .refresh_seed
                .swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                *scope.causes.lock().expect("consumption scope poisoned") =
                    ContinuationOrigin(Some(DiagnosticScope::capture()));
            }
            scope.current_scope().bind_consumed_origin();
        });
    }

    pub(crate) fn current_scope(&self) -> DiagnosticScope {
        self.causes
            .lock()
            .expect("consumption scope poisoned")
            .scope()
    }

    pub(crate) fn scope<F: std::future::Future>(
        &self,
        future: F,
    ) -> impl std::future::Future<Output = F::Output> + use<F> {
        CONSUMPTION.scope(self.clone(), self.initial.scope(future))
    }

    pub(crate) fn consumed(origin: Option<DiagnosticScope>) {
        let _ = CONSUMPTION.try_with(|scope| {
            let mut causes = scope.causes.lock().expect("consumption scope poisoned");
            causes.include(origin);
            // LoopDriver can begin the model in the SAME poll after emitting
            // ToolResultReceived. Rebind here, not at the next host step. This
            // changes no previously captured executor/descendant producer.
            causes.scope().bind_consumed_origin();
        });
    }
}

pub(crate) struct ConsumptionObserver(TaskOrigins);

impl ConsumptionObserver {
    pub(crate) fn new(origins: TaskOrigins) -> Self {
        Self(origins)
    }
}

impl LoopObserver for ConsumptionObserver {
    fn handle_event(&self, event: ObservedEvent) {
        let call_id = match &event.event {
            AgentEvent::ToolResultReceived(result) => Some(&result.call_id),
            AgentEvent::ApprovalRequired(approval) => approval.call_id.as_ref(),
            _ => None,
        };
        if let Some(call_id) = call_id {
            ConsumptionScope::consumed(self.0.get(call_id));
        }
    }
}

pub(crate) struct DiagnosticTaskManager {
    pub(super) inner: AsyncTaskManager,
    pub(crate) origins: TaskOrigins,
}

impl DiagnosticTaskManager {
    pub(crate) fn new(inner: AsyncTaskManager, origins: TaskOrigins) -> Self {
        Self { inner, origins }
    }
}

#[async_trait]
impl TaskManager for DiagnosticTaskManager {
    async fn start_task(
        &self,
        request: TaskLaunchRequest,
        mut ctx: TaskStartContext,
    ) -> Result<TaskStartOutcome, TaskManagerError> {
        // This runs inside the originating turn, before AsyncTaskManager spawns.
        // Never recover identity from session IDs: the same ID can be reactivated
        // while an old producer is still running.
        let call_id = request.request.call_id.clone();
        let origin = self.origins.proposed_origin(&call_id);
        ctx.executor = Arc::new(DiagnosticExecutor {
            inner: ctx.executor,
            origin: origin.clone(),
        });
        if let Some(scope) = &mut ctx.tool_context.execution_scope {
            // Runlet crosses spawn_blocking and Handle::block_on before invoking
            // this executor. Scoping only the outer compose future is insufficient.
            scope.executor = Arc::new(DiagnosticExecutor {
                inner: scope.executor.clone(),
                origin: origin.clone(),
            });
        }
        let outcome = self.inner.start_task(request, ctx).await?;
        // Rejected launches must not register phantom causes. The loop cannot
        // consume the resolution until this accepted outcome is returned.
        self.origins.accepted(call_id, origin);
        Ok(outcome)
    }

    async fn wait_for_turn(
        &self,
        turn_id: &TurnId,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<TurnTaskUpdate>, TaskManagerError> {
        self.inner.wait_for_turn(turn_id, cancellation).await
    }

    async fn take_pending_loop_updates(&self) -> Result<PendingLoopUpdates, TaskManagerError> {
        self.inner.take_pending_loop_updates().await
    }

    async fn wait_for_loop_update(&self) -> Result<(), TaskManagerError> {
        self.inner.wait_for_loop_update().await
    }

    async fn on_turn_interrupted(&self, turn_id: &TurnId) -> Result<(), TaskManagerError> {
        self.inner.on_turn_interrupted(turn_id).await
    }

    fn handle(&self) -> TaskManagerHandle {
        self.inner.handle()
    }
}

struct DiagnosticExecutor {
    inner: Arc<dyn ToolExecutor>,
    origin: DiagnosticScope,
}

#[async_trait]
impl ToolExecutor for DiagnosticExecutor {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs()
    }

    fn drain_catalog_events(&self) -> Vec<ToolCatalogEvent> {
        self.inner.drain_catalog_events()
    }

    async fn execute(
        &self,
        request: ToolRequest,
        ctx: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        self.origin.scope(self.inner.execute(request, ctx)).await
    }

    async fn execute_owned(
        &self,
        request: ToolRequest,
        ctx: OwnedToolContext,
    ) -> ToolExecutionOutcome {
        self.origin
            .scope(self.inner.execute_owned(request, ctx))
            .await
    }

    async fn execute_approved(
        &self,
        request: ToolRequest,
        approval: &ApprovalRequest,
        ctx: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        self.origin
            .scope(self.inner.execute_approved(request, approval, ctx))
            .await
    }

    async fn execute_approved_owned(
        &self,
        request: ToolRequest,
        approval: &ApprovalRequest,
        ctx: OwnedToolContext,
    ) -> ToolExecutionOutcome {
        self.origin
            .scope(self.inner.execute_approved_owned(request, approval, ctx))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::{process::Command, time::Duration};

    use agentkit_core::{MetadataMap, SessionId, ToolCallId, ToolOutput, ToolResultPart};
    use agentkit_task_manager::{TaskEvent, TaskLaunchKind};
    use agentkit_tool_compose::ComposeTool;
    use agentkit_tools_core::{
        ApprovalReason, BasicToolExecutor, PermissionChecker, PermissionDecision,
        PermissionRequest, Tool, ToolError, ToolExecutionScope, ToolName, ToolRegistry, ToolResult,
        ToolSource,
    };
    use serde_json::json;
    use tokio::sync::Barrier;

    use super::*;
    use crate::{events, tools::Observed};

    // The barriers belong to a real tool, not to the production propagation path.
    struct Gate {
        spec: ToolSpec,
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    }

    #[async_trait]
    impl Tool for Gate {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn proposed_requests(
            &self,
            request: &ToolRequest,
        ) -> Result<Vec<Box<dyn PermissionRequest>>, ToolError> {
            Ok(vec![Box::new(GatePermission {
                metadata: MetadataMap::new(),
                needs_approval: request.input["approved"] == true,
            })])
        }

        async fn invoke(
            &self,
            request: ToolRequest,
            ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, ToolError> {
            if request.input["approved"] == true {
                assert_eq!(ctx.approved_request.as_ref().unwrap().id, approval().id);
            }
            if request.input["wait"] == true {
                self.entered.wait().await;
                self.release.wait().await;
            }
            Ok(ToolResult::new(ToolResultPart::success(
                request.call_id,
                ToolOutput::structured(json!({"done": true})),
            )))
        }
    }

    struct GatePermission {
        metadata: MetadataMap,
        needs_approval: bool,
    }

    impl PermissionRequest for GatePermission {
        fn kind(&self) -> &'static str {
            "diagnostic.gate"
        }
        fn summary(&self) -> String {
            "run gate".into()
        }
        fn metadata(&self) -> &MetadataMap {
            &self.metadata
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    struct GatePermissions;

    impl PermissionChecker for GatePermissions {
        fn evaluate(&self, request: &dyn PermissionRequest) -> PermissionDecision {
            if request
                .as_any()
                .downcast_ref::<GatePermission>()
                .unwrap()
                .needs_approval
            {
                PermissionDecision::RequireApproval(approval())
            } else {
                PermissionDecision::Allow
            }
        }
    }

    fn approval() -> ApprovalRequest {
        ApprovalRequest::new(
            "diagnostic-approval",
            "diagnostic.gate",
            ApprovalReason::PolicyRequiresConfirmation,
            "run gate",
        )
    }

    fn context(executor: Arc<dyn ToolExecutor>) -> OwnedToolContext {
        let scope = ToolExecutionScope {
            executor,
            session_id: SessionId::new("A"),
            turn_id: TurnId::new("turn"),
            permissions: Arc::new(GatePermissions),
            resources: Arc::new(()),
            cancellation: None,
        };
        scope.nested_context(MetadataMap::new())
    }

    fn request(call: &str, tool: &str, input: serde_json::Value) -> ToolRequest {
        ToolRequest::new(
            ToolCallId::new(call),
            ToolName::new(tool),
            input,
            SessionId::new("A"),
            TurnId::new("turn"),
        )
    }

    async fn completed(manager: &DiagnosticTaskManager) {
        let handle = manager.handle();
        loop {
            match handle.next_event().await.expect("task event") {
                TaskEvent::Completed(_, result) => {
                    assert!(!result.is_error, "{result:?}");
                    return;
                }
                TaskEvent::Failed(_, error) => panic!("task failed: {error}"),
                TaskEvent::Cancelled(_) => panic!("task cancelled"),
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn task_origins_survive_delivery_and_approval_readmission() {
        let manager = crate::runtime::background_task_manager();
        let old = DiagnosticScope::with_operation(Some(
            events::DiagnosticOperation::new("old".into()).unwrap(),
        ));
        let new = DiagnosticScope::with_operation(Some(
            events::DiagnosticOperation::new("new".into()).unwrap(),
        ));
        let call_id = ToolCallId::new("retained-call");
        let tools = ToolRegistry::new().with(Gate {
            spec: ToolSpec::new("gate", "approval gate", json!({"type": "object"})),
            entered: Arc::new(Barrier::new(1)),
            release: Arc::new(Barrier::new(1)),
        });
        let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::new([
            Arc::new(tools) as Arc<dyn ToolSource>
        ]));
        let req = request("retained-call", "gate", json!({"approved": true}));
        old.scope(manager.start_task(
            TaskLaunchRequest::plain(None, req.clone()),
            TaskStartContext {
                executor: executor.clone(),
                tool_context: context(executor.clone()),
            },
        ))
        .await
        .unwrap();
        let Some(TurnTaskUpdate::Resolution(resolution)) = manager
            .wait_for_turn(&TurnId::new("turn"), None)
            .await
            .unwrap()
        else {
            panic!("approval resolution");
        };
        let agentkit_task_manager::TaskResolution::Approval(pending) = *resolution else {
            panic!("real permission checker must require approval");
        };
        assert_eq!(manager.origins.get(&call_id), Some(old.clone()));
        new.scope(manager.start_task(
            TaskLaunchRequest {
                task_id: Some(pending.task_id),
                request: req,
                kind: TaskLaunchKind::Approved(pending.approval),
            },
            TaskStartContext {
                executor: executor.clone(),
                tool_context: context(executor),
            },
        ))
        .await
        .unwrap();
        let Some(TurnTaskUpdate::Resolution(resolution)) = manager
            .wait_for_turn(&TurnId::new("turn"), None)
            .await
            .unwrap()
        else {
            panic!("approved completion");
        };
        assert!(matches!(
            *resolution,
            agentkit_task_manager::TaskResolution::Item(_)
        ));
        assert_eq!(manager.origins.get(&call_id), Some(old));
        assert!(
            manager
                .origins
                .get(&ToolCallId::new("never-accepted"))
                .is_none()
        );
    }

    #[test]
    fn dependency_spawns_keep_origin_on_stderr() {
        for mode in [
            "manager",
            "approved-manager",
            "runlet",
            "approved-runlet",
            "borrowed",
            "approved-borrowed",
            "owned",
            "approved-owned",
            "unscoped",
        ] {
            for switch in [false, true] {
                let output = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "runtime::diagnostics::tests::diagnostic_transport_child",
                        "--nocapture",
                    ])
                    .env(events::EVENTS_ENV, "1")
                    .env("KIT_DIAGNOSTIC_RUNTIME_TEST", mode)
                    .env("KIT_DIAGNOSTIC_SWITCH_TEST", switch.to_string())
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{mode}/{switch}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                let diagnostics = String::from_utf8(output.stderr)
                    .unwrap()
                    .lines()
                    .filter_map(events::parse_diagnostic)
                    .collect::<Vec<_>>();
                let markers = diagnostics
                    .iter()
                    .filter(|event| {
                        matches!(event.event, events::RuntimeEvent::SessionStarted { .. })
                    })
                    .collect::<Vec<_>>();
                let first = markers.first().unwrap().activation.clone();
                let latest = markers.last().unwrap().activation.clone();
                assert_ne!(first, latest);
                let expected = if mode == "unscoped" { None } else { first };
                let mut old_started = 0;
                let mut old_finished = 0;
                let mut current = 0;
                for diagnostic in &diagnostics {
                    let (call, started) = match &diagnostic.event {
                        events::RuntimeEvent::ChildStarted { call, .. } => (call, true),
                        events::RuntimeEvent::ChildFinished { call, ok, .. } => {
                            assert!(*ok);
                            (call, false)
                        }
                        _ => continue,
                    };
                    if call.starts_with("current") {
                        assert_eq!(diagnostic.activation, latest, "{mode}/{switch}");
                        assert_eq!(
                            diagnostic.operation.as_ref().map(|op| op.as_str()),
                            Some("current-operation")
                        );
                        current += 1;
                    } else {
                        assert!(call.starts_with("old"), "{call}");
                        assert_eq!(
                            diagnostic.operation.as_ref().map(|op| op.as_str()),
                            if mode == "unscoped" {
                                None
                            } else {
                                Some("old-operation")
                            }
                        );
                        assert_eq!(diagnostic.activation, expected, "{mode}/{switch}: {call}");
                        if started {
                            old_started += 1;
                        } else {
                            old_finished += 1;
                        }
                    }
                }
                let calls = if mode.contains("runlet") { 2 } else { 1 };
                assert_eq!(
                    (old_started, old_finished, current),
                    (calls, calls, 2),
                    "{mode}/{switch}"
                );
                // Delayed finishes and Runlet's dependent second start occur
                // after the final activation marker, not just before it.
                let last_marker = diagnostics
                    .iter()
                    .rposition(|event| {
                        matches!(event.event, events::RuntimeEvent::SessionStarted { .. })
                    })
                    .unwrap();
                assert!(diagnostics[last_marker + 1..].iter().any(|event| {
                    matches!(&event.event, events::RuntimeEvent::ChildFinished { call, .. } if call.starts_with("old"))
                }));
                if mode.contains("runlet") {
                    assert!(diagnostics[last_marker + 1..].iter().any(|event| {
                        matches!(&event.event, events::RuntimeEvent::ChildStarted { call, .. } if call.starts_with("old"))
                    }));
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn diagnostic_transport_child() {
        let Ok(mode) = std::env::var("KIT_DIAGNOSTIC_RUNTIME_TEST") else {
            return;
        };
        tokio::time::timeout(Duration::from_secs(15), async {
            let switch = std::env::var("KIT_DIAGNOSTIC_SWITCH_TEST").unwrap() == "true";
            let entered = Arc::new(Barrier::new(2));
            let release = Arc::new(Barrier::new(2));
            let mut children = ToolRegistry::new();
            children.register(Observed::new(Gate {
                spec: ToolSpec::new("gate", "barrier gate", json!({"type": "object"})),
                entered: entered.clone(),
                release: release.clone(),
            }));
            let compose = ComposeTool::wrap(children.clone())
                .with_backend(crate::runtime::HiddenRunletBackend(children));
            let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::new([
                Arc::new(compose) as Arc<dyn ToolSource>,
            ]));
            let manager = crate::runtime::background_task_manager();
            // The same real permission checker must interrupt an unapproved
            // call; approved executor entry points must resume, not bypass it.
            let denied = executor.execute_owned(
                request("approval-check", "gate", json!({"approved": true})),
                context(executor.clone()),
            ).await;
            assert!(matches!(denied, ToolExecutionOutcome::Interrupted(_)));
            events::activate_diagnostics("A");
            let approved = mode.starts_with("approved-");
            let runlet = mode.contains("runlet");
            let req = if runlet {
                request("old", "compose", json!({
                    "script": "first = gate({wait: true})\nreturn gate({wait: false, previous: first})",
                    "background": true,
                }))
            } else {
                request("old", "gate", json!({"wait": true, "approved": approved}))
            };
            let start = async {
                let ctx = context(executor.clone());
                if mode.contains("borrowed") || mode.ends_with("owned") {
                    let wrapped = DiagnosticExecutor {
                        inner: executor.clone(),
                        origin: DiagnosticScope::capture(),
                    };
                    let mode = mode.clone();
                    Some(tokio::spawn(async move {
                        let approval = approval();
                        match mode.as_str() {
                            "borrowed" => wrapped.execute(req, &mut ctx.borrowed()).await,
                            "approved-borrowed" => wrapped.execute_approved(req, &approval, &mut ctx.borrowed()).await,
                            "owned" => wrapped.execute_owned(req, ctx).await,
                            "approved-owned" => wrapped.execute_approved_owned(req, &approval, ctx).await,
                            _ => unreachable!(),
                        }
                    }))
                } else {
                    let outcome = manager.start_task(TaskLaunchRequest {
                        task_id: None,
                        request: req,
                        kind: if approved { TaskLaunchKind::Approved(approval()) } else { TaskLaunchKind::Plain },
                    }, TaskStartContext { executor: executor.clone(), tool_context: ctx }).await.unwrap();
                    assert!(matches!(outcome, TaskStartOutcome::Pending { .. }));
                    None
                }
            };
            let task = if mode == "unscoped" { start.await } else {
                DiagnosticScope::with_operation(Some(events::DiagnosticOperation::new("old-operation".into()).unwrap()))
                    .scope(async { events::scope_diagnostics("A", start).await }).await
            };
            entered.wait().await;
            if switch { events::activate_diagnostics("B"); }
            events::activate_diagnostics("A");
            release.wait().await;
            if let Some(task) = task {
                assert!(matches!(task.await.unwrap(), ToolExecutionOutcome::Completed(_)));
            } else {
                completed(&manager).await;
            }
            // A newly-created producer still gets the current activation.
            DiagnosticScope::with_operation(Some(events::DiagnosticOperation::new("current-operation".into()).unwrap())).scope(async { events::scope_diagnostics("A", manager.start_task(TaskLaunchRequest {
                task_id: None,
                request: request("current", "gate", json!({"wait": false})),
                kind: TaskLaunchKind::Plain,
            }, TaskStartContext { executor: executor.clone(), tool_context: context(executor) })).await }).await.unwrap();
            completed(&manager).await;
        }).await.expect("barrier-controlled task timed out");
    }
}
