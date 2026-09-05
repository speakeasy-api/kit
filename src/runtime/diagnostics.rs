//! Carry producer identity through dependency-owned task and Runlet spawns.

use std::sync::Arc;

use agentkit_core::{TurnCancellation, TurnId};
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

pub(super) struct DiagnosticTaskManager(pub(super) AsyncTaskManager);

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
        let origin = DiagnosticScope::capture();
        ctx.executor = Arc::new(DiagnosticExecutor {
            inner: ctx.executor,
            origin: origin.clone(),
        });
        if let Some(scope) = &mut ctx.tool_context.execution_scope {
            // Runlet crosses spawn_blocking and Handle::block_on before invoking
            // this executor. Scoping only the outer compose future is insufficient.
            scope.executor = Arc::new(DiagnosticExecutor {
                inner: scope.executor.clone(),
                origin,
            });
        }
        self.0.start_task(request, ctx).await
    }

    async fn wait_for_turn(
        &self,
        turn_id: &TurnId,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<TurnTaskUpdate>, TaskManagerError> {
        self.0.wait_for_turn(turn_id, cancellation).await
    }

    async fn take_pending_loop_updates(&self) -> Result<PendingLoopUpdates, TaskManagerError> {
        self.0.take_pending_loop_updates().await
    }

    async fn wait_for_loop_update(&self) -> Result<(), TaskManagerError> {
        self.0.wait_for_loop_update().await
    }

    async fn on_turn_interrupted(&self, turn_id: &TurnId) -> Result<(), TaskManagerError> {
        self.0.on_turn_interrupted(turn_id).await
    }

    fn handle(&self) -> TaskManagerHandle {
        self.0.handle()
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
                        current += 1;
                    } else {
                        assert!(call.starts_with("old"), "{call}");
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
            let task = if mode == "unscoped" { start.await } else { events::scope_diagnostics("A", start).await };
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
            events::scope_diagnostics("A", manager.start_task(TaskLaunchRequest {
                task_id: None,
                request: request("current", "gate", json!({"wait": false})),
                kind: TaskLaunchKind::Plain,
            }, TaskStartContext { executor: executor.clone(), tool_context: context(executor) })).await.unwrap();
            completed(&manager).await;
        }).await.expect("barrier-controlled task timed out");
    }
}
