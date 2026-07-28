use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use agentkit_core::{
    FinishReason, Item, ItemKind, MetadataMap, Part, SessionId, ToolCallPart, ToolOutput,
    TurnCancellation, TurnId,
};
use agentkit_loop::{
    LoopError, LoopStep, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, ModelTurnResult,
    SessionConfig, TurnRequest,
};
use agentkit_tools_core::{
    AllowAllPermissions, OwnedToolContext, ToolExecutionOutcome, ToolExecutionScope, ToolExecutor,
    ToolRequest, ToolSpec,
};
use kit::{
    agent::{
        adapters::tool::{ToolBinding, ToolExecutorAdapter, ToolKernelContext},
        agentkit_bridge::mapping::from_agentkit_item,
        driver::{
            restart::{
                BoundarySnapshot, CommittedModelOutcome, EffectJournal, EffectJournalAppend,
                LoopRecord, RecoveryState, RestartProjection, SafeBoundary,
            },
            waiting::{WaitingKind, WaitingResolution, WaitingState},
        },
    },
    api::{
        auth::{
            contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        service::AttemptDriverClaim,
    },
    capabilities::kernel::{
        grant::{ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, EffectClass},
        identity::{
            CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
            CapabilityVersion, Digest, DigestAlgorithm,
        },
        invoke::{ApprovalState, CanonicalOutput, DispatchOutcome, RetrySafety},
    },
    domain::{
        config::{
            BudgetLayer, CONFIG_SCHEMA_VERSION, ConcurrencyLayer, ConfigLayer, Executor, Grant,
            LayerStack, Provider, RetentionLayer, RunConfigContext, RunConfigSnapshot,
        },
        events::{ApprovalDecision as DomainApprovalDecision, TraceId, UtcDateTime},
        ids::ApprovalId,
        ids::{
            AttemptId, CommandId, EventId, PrincipalId, ProjectId, RunId, ToolCallId, WorkspaceId,
        },
        lifecycle::{AttemptOwnership, AttemptState, FencingToken},
    },
    runtime::scheduler::{budget::RunBudget, limits::Spend, reserve::BudgetLedger},
    store::sqlite::idempotency::IdempotencyKey,
    test_support,
};

const UID: u32 = 831;

#[derive(Clone, Copy, Debug)]
enum Route {
    Valid,
    UnknownTool,
    ForeignRun,
    ForeignContextRun,
    ForeignContextTurn,
    SchemaDrift,
    ForgedCapability,
    ForgedEffect,
    WeakenedConstraints,
    ForeignWorkspace,
    ForeignProject,
    ForeignPrincipal,
    EmptyGrants,
    StaleFenceLow,
    StaleFenceHigh,
    ExhaustedBudget,
    PreCancelled,
    ApprovalPending,
    ApprovalDenied,
    DirectExecutorSeam,
    NestedExecutorSeam,
    TaskManagerOwnedSeam,
}

#[derive(Clone, Copy)]
enum Seam {
    Owned,
    Direct,
    Nested,
}

struct Harness {
    adapter: Arc<ToolExecutorAdapter>,
    request: ToolRequest,
    context: OwnedToolContext,
    directory: PathBuf,
    dispatches: Arc<AtomicUsize>,
    intent_before_dispatch: Arc<AtomicBool>,
    attempt: AttemptOwnership,
    authenticated: AuthenticatedPrincipal,
    run_id: RunId,
}

#[derive(Clone)]
struct NeverModel;

struct NeverSession;

struct NeverTurn(Option<ModelTurnEvent>);

impl ModelAdapter for NeverModel {
    type Session = NeverSession;

    fn start_session<'life0, 'async_trait>(
        &'life0 self,
        _config: SessionConfig,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Session, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(NeverSession) })
    }
}

impl ModelSession for NeverSession {
    type Turn = NeverTurn;

    fn begin_turn<'life0, 'async_trait>(
        &'life0 mut self,
        _request: TurnRequest,
        _cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Turn, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async {
            Ok(NeverTurn(Some(ModelTurnEvent::Finished(ModelTurnResult {
                finish_reason: FinishReason::Completed,
                output_items: vec![Item::text(ItemKind::Assistant, "complete")],
                usage: None,
                metadata: MetadataMap::new(),
                model: Some("completion".to_owned()),
                response_id: None,
            }))))
        })
    }
}

impl ModelTurn for NeverTurn {
    fn next_event<'life0, 'async_trait>(
        &'life0 mut self,
        _cancellation: Option<TurnCancellation>,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<ModelTurnEvent>, LoopError>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(self.0.take()) })
    }
}

fn harness(route: Route) -> Harness {
    harness_with_outcome(
        route,
        DispatchOutcome::Succeeded(CanonicalOutput {
            media_type: "application/json".to_owned(),
            body: br#"{"ok":true}"#.to_vec(),
        }),
    )
}

fn harness_with_outcome(route: Route, capability_outcome: DispatchOutcome) -> Harness {
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let workspace_id = WorkspaceId::generate().unwrap();
    let run_id = RunId::generate().unwrap();
    let authority = BTreeSet::from([Grant::WorkspaceRead]);
    let mut config = run_config(principal_id, project_id, run_id, authority.clone());
    let mut authenticated = authenticate(principal_id, project_id, authority.clone());
    let granted_capability = identity("read", b"implementation-v1");
    let mut capability = granted_capability.clone();
    let schema = Digest::of(DigestAlgorithm::Sha256, b"schema-v1");
    let mut discovered_schema = schema;
    let constraints = ArgumentConstraints::new([b"workspace=root".as_slice()]);
    let mut requested_constraints = constraints.clone();
    let mut effect = EffectClass::WorkspaceRead;
    let mut selected_workspace = workspace_id;
    let mut selected_project = project_id;
    let mut grants = CapabilityGrantSnapshot::new(
        &config,
        [CapabilityGrant::new(
            principal_id,
            project_id,
            workspace_id,
            granted_capability,
            schema,
            EffectClass::WorkspaceRead,
            constraints,
        )],
        DigestAlgorithm::Sha256,
    );
    let mut approval = ApprovalState::NotRequired;
    let mut fence_value = 17;
    let mut budget = RunBudget::new(100, 100, 100, 100, 100);
    let mut cancelled = false;
    let mut seam = Seam::Owned;

    match route {
        Route::SchemaDrift => discovered_schema = Digest::of(DigestAlgorithm::Sha256, b"drift"),
        Route::ForgedCapability => capability = identity("write", b"implementation-v2"),
        Route::ForgedEffect => effect = EffectClass::WorkspaceWrite,
        Route::WeakenedConstraints => requested_constraints = ArgumentConstraints::default(),
        Route::ForeignWorkspace => selected_workspace = WorkspaceId::generate().unwrap(),
        Route::ForeignProject => selected_project = ProjectId::generate().unwrap(),
        Route::ForeignPrincipal => {
            authenticated = authenticate(
                PrincipalId::generate().unwrap(),
                project_id,
                authority.clone(),
            )
        }
        Route::EmptyGrants => {
            grants = CapabilityGrantSnapshot::new(&config, [], DigestAlgorithm::Sha256)
        }
        Route::StaleFenceLow => fence_value = 16,
        Route::StaleFenceHigh => fence_value = 18,
        Route::ExhaustedBudget => budget = RunBudget::new(0, 0, 0, 0, 0),
        Route::PreCancelled => cancelled = true,
        Route::ApprovalPending => approval = ApprovalState::Pending,
        Route::ApprovalDenied => approval = ApprovalState::Denied,
        Route::DirectExecutorSeam => seam = Seam::Direct,
        Route::NestedExecutorSeam => seam = Seam::Nested,
        Route::TaskManagerOwnedSeam => seam = Seam::Owned,
        _ => {}
    }

    if matches!(route, Route::ForeignRun) {
        config = run_config(
            principal_id,
            project_id,
            RunId::generate().unwrap(),
            authority,
        );
    }
    let attempt = AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        principal_id,
        FencingToken::new(17),
    );
    let binding = ToolBinding::new(
        ToolSpec::new(
            "workspace.read",
            "read a workspace file",
            serde_json::json!({"type": "object"}),
        ),
        capability,
        discovered_schema,
        schema,
        effect,
        requested_constraints,
        Spend::new(2, 2, 0, 1, 0),
        RetrySafety::NonIdempotent,
        approval,
    );
    let directory = std::env::temp_dir().join(format!(
        "kit-tool-kernel-bypass-{}",
        EventId::generate().unwrap()
    ));
    std::fs::create_dir(&directory).unwrap();
    let database = directory.join("store.sqlite3");
    let mut store = test_support::open_sqlite_store(&database).unwrap();
    let claim = store
        .install_driver_claim_for_test(AttemptDriverClaim {
            run_id: config.run_id(),
            attempt_id: attempt.attempt_id,
            principal_id: attempt.principal_id,
            fence: attempt.fencing_token,
            lease_version: 1,
            expires_at_unix_micros: 0,
        })
        .unwrap();
    let dispatches = Arc::new(AtomicUsize::new(0));
    let intent_before_dispatch = Arc::new(AtomicBool::new(true));
    let callback_dispatches = Arc::clone(&dispatches);
    let callback_ordering = Arc::clone(&intent_before_dispatch);
    let callback_database = database.clone();
    let adapter = Arc::new(
        ToolExecutorAdapter::new(
            [binding],
            ToolKernelContext {
                authenticated: authenticated.clone(),
                config: config.clone(),
                grants,
                delegation: None,
                workspace_id: selected_workspace,
                project_id: selected_project,
                attempt,
                claim,
                current_fence: Arc::new(AtomicU64::new(fence_value)),
                cancellation: Arc::new(AtomicBool::new(cancelled)),
                cancellation_coordinator: Arc::new(
                    kit::executor::cancel::SqliteCancellationCoordinator::new(&database),
                ),
                budget: Arc::new(BudgetLedger::new(budget)),
            },
            store,
            move |_| {
                let events = test_support::open_sqlite_store(&callback_database)
                    .unwrap()
                    .events()
                    .unwrap();
                callback_ordering.fetch_and(
                    events.len() >= 2
                        && events[events.len() - 2].event.event_type.as_str()
                            == "capability.invocation_intent"
                        && events.last().is_some_and(|event| {
                            event.event.event_type.as_str() == "capability.invocation_dispatched"
                        }),
                    Ordering::SeqCst,
                );
                callback_dispatches.fetch_add(1, Ordering::SeqCst);
                capability_outcome.clone()
            },
        )
        .unwrap(),
    );
    let metadata = MetadataMap::new();
    let mut tool_name = "workspace.read";
    let request_run = if matches!(route, Route::ForeignRun) {
        run_id
    } else {
        config.run_id()
    };
    let mut context_run = request_run;
    let request_turn = TurnId::new("turn-a");
    let mut context_turn = request_turn.clone();

    match route {
        Route::UnknownTool => tool_name = "workspace.direct-read",
        Route::ForeignContextRun => context_run = RunId::generate().unwrap(),
        Route::ForeignContextTurn => context_turn = TurnId::new("turn-b"),
        _ => {}
    }
    let request = ToolRequest::new(
        "provider-call-a",
        tool_name,
        serde_json::json!({"path": "README.md"}),
        request_run.to_string(),
        request_turn,
    )
    .with_metadata(metadata.clone());
    let context = OwnedToolContext {
        session_id: SessionId::new(context_run.to_string()),
        turn_id: context_turn,
        metadata,
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
        execution_scope: if matches!(seam, Seam::Nested) {
            Some(ToolExecutionScope {
                executor: adapter.clone(),
                session_id: SessionId::new(context_run.to_string()),
                turn_id: TurnId::new("turn-a"),
                permissions: Arc::new(AllowAllPermissions),
                resources: Arc::new(()),
                cancellation: None,
            })
        } else {
            None
        },
        approved_request: None,
    };
    Harness {
        adapter,
        request,
        context,
        directory,
        dispatches,
        intent_before_dispatch,
        attempt,
        authenticated,
        run_id: config.run_id(),
    }
}

#[tokio::test]
async fn direct_nested_and_task_manager_seams_still_execute_through_the_kernel() {
    for route in [
        Route::DirectExecutorSeam,
        Route::NestedExecutorSeam,
        Route::TaskManagerOwnedSeam,
    ] {
        let mut harness = harness(route);
        let outcome = execute(&mut harness, route).await;
        assert!(matches!(outcome, ToolExecutionOutcome::Completed(_)));
        assert_eq!(harness.dispatches.load(Ordering::SeqCst), 1);
        let directory = harness.directory.clone();
        drop(harness);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

async fn execute(harness: &mut Harness, route: Route) -> ToolExecutionOutcome {
    match route {
        Route::DirectExecutorSeam => {
            let mut context = harness.context.borrowed();
            harness
                .adapter
                .execute(harness.request.clone(), &mut context)
                .await
        }
        Route::NestedExecutorSeam => {
            harness
                .context
                .execution_scope
                .as_ref()
                .unwrap()
                .execute_child(harness.request.clone())
                .await
        }
        _ => {
            harness
                .adapter
                .execute_owned(harness.request.clone(), harness.context.clone())
                .await
        }
    }
}

#[tokio::test]
async fn authorization_budget_and_cancellation_routes_cannot_bypass_the_kernel() {
    let routes = [
        Route::UnknownTool,
        Route::ForeignRun,
        Route::ForeignContextRun,
        Route::ForeignContextTurn,
        Route::SchemaDrift,
        Route::ForgedCapability,
        Route::ForgedEffect,
        Route::WeakenedConstraints,
        Route::ForeignWorkspace,
        Route::ForeignProject,
        Route::ForeignPrincipal,
        Route::EmptyGrants,
        Route::StaleFenceLow,
        Route::StaleFenceHigh,
        Route::ExhaustedBudget,
        Route::PreCancelled,
        Route::ApprovalPending,
        Route::ApprovalDenied,
    ];

    for route in routes {
        let mut harness = harness(route);
        let outcome = execute(&mut harness, route).await;
        assert_eq!(
            harness.dispatches.load(Ordering::SeqCst),
            0,
            "bypass route dispatched: {route:?}"
        );
        assert!(
            !matches!(outcome, ToolExecutionOutcome::Completed(_)),
            "bypass route completed: {route:?}"
        );
        let directory = harness.directory.clone();
        drop(harness);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test]
async fn successful_dispatch_observes_committed_intent_and_returns_after_durable_outcome() {
    let harness = harness(Route::Valid);
    let outcome = harness
        .adapter
        .execute_owned(harness.request.clone(), harness.context.clone())
        .await;
    assert!(
        matches!(outcome, ToolExecutionOutcome::Completed(_)),
        "unexpected outcome: {outcome:?}"
    );
    assert_eq!(harness.dispatches.load(Ordering::SeqCst), 1);
    assert!(harness.intent_before_dispatch.load(Ordering::SeqCst));
    let database = harness.directory.join("store.sqlite3");
    let events = test_support::open_sqlite_store(database)
        .unwrap()
        .events()
        .unwrap();
    let events = capability_events(&events);
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[0].event.event_type.as_str(),
        "capability.invocation_intent"
    );
    assert_eq!(
        events[1].event.event_type.as_str(),
        "capability.invocation_dispatched"
    );
    assert_eq!(
        events[2].event.event_type.as_str(),
        "capability.invocation_outcome"
    );
    let directory = harness.directory.clone();
    drop(harness);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn provider_metadata_cannot_supply_correlation_or_authority() {
    let mut harness = harness(Route::Valid);
    harness.request.metadata = BTreeMap::from([
        ("kit.invocation_id".to_owned(), serde_json::json!("forged")),
        ("kit.approval".to_owned(), serde_json::json!("not_required")),
        ("kit.effect".to_owned(), serde_json::json!("workspace_read")),
    ]);
    harness.context.metadata = harness.request.metadata.clone();
    let outcome = harness
        .adapter
        .execute_owned(harness.request.clone(), harness.context.clone())
        .await;
    assert!(matches!(outcome, ToolExecutionOutcome::Completed(_)));
    let events = test_support::open_sqlite_store(harness.directory.join("store.sqlite3"))
        .unwrap()
        .events()
        .unwrap();
    assert!(
        events
            .iter()
            .all(|event| { !String::from_utf8_lossy(&event.event.payload).contains("forged") })
    );
    let directory = harness.directory.clone();
    drop(harness);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn oversized_and_schema_invalid_inputs_stop_before_dispatch() {
    for input in [
        serde_json::json!(["not-an-object"]),
        serde_json::json!({"payload": "x".repeat(1024 * 1024)}),
    ] {
        let mut harness = harness(Route::Valid);
        harness.request.input = input;
        let outcome = harness
            .adapter
            .execute_owned(harness.request.clone(), harness.context.clone())
            .await;
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::FailedBeforeInvocation(_)
        ));
        assert_eq!(harness.dispatches.load(Ordering::SeqCst), 0);
        let directory = harness.directory.clone();
        drop(harness);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test]
async fn approval_is_durable_and_resume_uses_a_fresh_kernel_invocation() {
    let harness = harness(Route::ApprovalPending);
    let interrupted = harness
        .adapter
        .execute_owned(harness.request.clone(), harness.context.clone())
        .await;
    let ToolExecutionOutcome::Interrupted(agentkit_tools_core::ToolInterruption::ApprovalRequired(
        approval,
    )) = interrupted
    else {
        panic!("expected approval interruption")
    };
    let replayed = harness
        .adapter
        .execute_owned(harness.request.clone(), harness.context.clone())
        .await;
    let ToolExecutionOutcome::Interrupted(agentkit_tools_core::ToolInterruption::ApprovalRequired(
        replayed_approval,
    )) = replayed
    else {
        panic!("expected replayed approval interruption")
    };
    assert_eq!(replayed_approval, approval);
    let completed = harness
        .adapter
        .execute_approved_owned(harness.request.clone(), &approval, harness.context.clone())
        .await;
    assert!(matches!(completed, ToolExecutionOutcome::Completed(_)));
    let completed_again = harness
        .adapter
        .execute_approved_owned(harness.request.clone(), &approval, harness.context.clone())
        .await;
    assert!(matches!(
        completed_again,
        ToolExecutionOutcome::Completed(_)
    ));
    assert_eq!(harness.dispatches.load(Ordering::SeqCst), 1);
    let events = test_support::open_sqlite_store(harness.directory.join("store.sqlite3"))
        .unwrap()
        .events()
        .unwrap();
    let events = capability_events(&events);
    assert_eq!(events.len(), 5);
    assert_eq!(
        events
            .iter()
            .map(|event| event.event.event_type.as_str())
            .collect::<Vec<_>>(),
        [
            "capability.invocation_intent",
            "capability.invocation_outcome",
            "capability.invocation_intent",
            "capability.invocation_dispatched",
            "capability.invocation_outcome",
        ]
    );
    let directory = harness.directory.clone();
    drop(harness);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn approved_restart_reconstructs_agentkit_and_executes_once() {
    let harness = harness(Route::ApprovalPending);
    let tool_call = ToolCallPart {
        id: harness.request.call_id.clone(),
        name: harness.request.tool_name.0.clone(),
        input: harness.request.input.clone(),
        metadata: harness.request.metadata.clone(),
    };
    let model_result = ModelTurnResult {
        finish_reason: FinishReason::ToolCall,
        output_items: vec![Item::new(
            ItemKind::Assistant,
            vec![Part::ToolCall(tool_call)],
        )],
        usage: None,
        metadata: MetadataMap::new(),
        model: Some("replayed".to_owned()),
        response_id: None,
    };
    let user = Item::text(ItemKind::User, "read the file");
    let snapshot = BoundarySnapshot {
        boundary: SafeBoundary::AfterModelOutcome,
        transcript: std::iter::once(&user)
            .chain(model_result.output_items.iter())
            .map(from_agentkit_item)
            .collect(),
        resume_index: Some(0),
        model_outcome: Some(CommittedModelOutcome::from_agentkit(&model_result)),
    };
    append_test_journal(
        &harness,
        "approval-boundary",
        LoopRecord::Boundary(snapshot.clone()),
    );

    let interrupted = harness
        .adapter
        .execute_owned(harness.request.clone(), harness.context.clone())
        .await;
    let ToolExecutionOutcome::Interrupted(agentkit_tools_core::ToolInterruption::ApprovalRequired(
        _,
    )) = interrupted
    else {
        panic!("expected durable approval request")
    };
    assert_eq!(harness.dispatches.load(Ordering::SeqCst), 0);

    let waiting = WaitingState {
        wait_id: CommandId::generate().unwrap(),
        principal_id: harness.attempt.principal_id,
        kind: WaitingKind::Approval {
            approval_id: ApprovalId::generate().unwrap(),
            tool_call_id: ToolCallId::generate().unwrap(),
        },
        snapshot,
    };
    append_test_journal(
        &harness,
        "approval-waiting",
        LoopRecord::Waiting(waiting.clone()),
    );
    append_test_journal(
        &harness,
        "approval-resolution",
        waiting
            .resolve(
                &harness.authenticated,
                WaitingResolution::Approval {
                    decision: DomainApprovalDecision::Approved,
                },
            )
            .unwrap(),
    );

    let projection = kit::api::service::AttemptProjection {
        id: harness.attempt.attempt_id,
        run_id: harness.run_id,
        state: AttemptState::Executing,
        owner: harness.attempt,
        version: 1,
    };
    let events = test_support::open_sqlite_store(harness.directory.join("store.sqlite3"))
        .unwrap()
        .events()
        .unwrap();
    let RecoveryState::Ready(plan) = RestartProjection::reconstruct(&projection, &events).unwrap()
    else {
        panic!("approved resolution did not produce a restart plan")
    };
    let mut driver = plan
        .start(&projection, NeverModel, |builder| {
            builder.tool_executor(harness.adapter.clone())
        })
        .await
        .unwrap();
    assert!(matches!(
        driver.poll(&projection).await.unwrap(),
        LoopStep::Finished(_)
    ));
    assert_eq!(harness.dispatches.load(Ordering::SeqCst), 1);

    let events = test_support::open_sqlite_store(harness.directory.join("store.sqlite3"))
        .unwrap()
        .events()
        .unwrap();
    let RecoveryState::Ready(after_execution) =
        RestartProjection::reconstruct(&projection, &events).unwrap()
    else {
        panic!("completed approved invocation was not restartable")
    };
    assert!(after_execution.approved_tool.is_none());
    let directory = harness.directory.clone();
    drop(driver);
    drop(harness);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn auth_required_failure_is_durable_before_it_is_returned() {
    let harness = harness_with_outcome(
        Route::Valid,
        DispatchOutcome::Failed {
            code: "auth_required".to_owned(),
        },
    );
    let outcome = harness
        .adapter
        .execute_owned(harness.request.clone(), harness.context.clone())
        .await;
    assert!(matches!(
        outcome,
        ToolExecutionOutcome::Failed(agentkit_tools_core::ToolError::ExecutionFailed(code))
            if code == "auth_required"
    ));
    let events = test_support::open_sqlite_store(harness.directory.join("store.sqlite3"))
        .unwrap()
        .events()
        .unwrap();
    let events = capability_events(&events);
    assert_eq!(events.len(), 3);
    let payload: serde_json::Value = serde_json::from_slice(&events[2].event.payload).unwrap();
    assert_eq!(payload["result"]["status"], "failed");
    assert_eq!(payload["result"]["code"], "auth_required");
    let directory = harness.directory.clone();
    drop(harness);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn canonical_and_model_facing_outputs_are_bounded() {
    let presentation = harness_with_outcome(
        Route::Valid,
        DispatchOutcome::Succeeded(CanonicalOutput {
            media_type: "text/plain".to_owned(),
            body: vec![b'x'; 20 * 1024],
        }),
    );
    let outcome = presentation
        .adapter
        .execute_owned(presentation.request.clone(), presentation.context.clone())
        .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("bounded presentation did not complete")
    };
    assert!(matches!(
        result.result.output,
        ToolOutput::Text(ref text) if text.len() == 16 * 1024
    ));
    assert_eq!(
        result.result.metadata["kit.presentation_truncated"],
        serde_json::json!(true)
    );
    let directory = presentation.directory.clone();
    drop(presentation);
    std::fs::remove_dir_all(directory).unwrap();

    let canonical = harness_with_outcome(
        Route::Valid,
        DispatchOutcome::Succeeded(CanonicalOutput {
            media_type: "application/octet-stream".to_owned(),
            body: vec![0; 64 * 1024 + 1],
        }),
    );
    let outcome = canonical
        .adapter
        .execute_owned(canonical.request.clone(), canonical.context.clone())
        .await;
    assert!(matches!(
        outcome,
        ToolExecutionOutcome::Failed(agentkit_tools_core::ToolError::ExecutionFailed(code))
            if code == "tool_output_too_large"
    ));
    let events = test_support::open_sqlite_store(canonical.directory.join("store.sqlite3"))
        .unwrap()
        .events()
        .unwrap();
    let events = capability_events(&events);
    let payload: serde_json::Value =
        serde_json::from_slice(&events.last().unwrap().event.payload).unwrap();
    assert_eq!(payload["result"]["code"], "tool_output_too_large");
    assert!(payload["result"]["output"].is_null());
    let directory = canonical.directory.clone();
    drop(canonical);
    std::fs::remove_dir_all(directory).unwrap();
}

fn capability_events(
    events: &[kit::store::sqlite::append::StoredEvent],
) -> Vec<&kit::store::sqlite::append::StoredEvent> {
    events
        .iter()
        .filter(|event| {
            event
                .event
                .event_type
                .as_str()
                .starts_with("capability.invocation_")
        })
        .collect()
}

fn append_test_journal(harness: &Harness, key: &str, record: LoopRecord) {
    test_support::open_sqlite_store(harness.directory.join("store.sqlite3"))
        .unwrap()
        .append_effect(EffectJournalAppend {
            owner: harness.attempt,
            claim: None,
            idempotency_key: IdempotencyKey::parse(key).unwrap(),
            command_id: CommandId::generate().unwrap(),
            event_id: EventId::generate().unwrap(),
            occurred_at: UtcDateTime::parse("2026-07-22T12:00:00Z").unwrap(),
            trace_id: TraceId::parse("approval-restart").unwrap(),
            artifacts: Vec::new(),
            record,
        })
        .unwrap();
}

fn authenticate(
    principal_id: PrincipalId,
    project_id: ProjectId,
    authority: BTreeSet<Grant>,
) -> AuthenticatedPrincipal {
    LocalPeerAuthenticator::new(BTreeMap::from([(
        UID,
        GrantSnapshot::new(principal_id, project_id, authority),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(UID, 88, UID))
    .unwrap()
}

fn run_config(
    principal_id: PrincipalId,
    project_id: ProjectId,
    run_id: RunId,
    authority: BTreeSet<Grant>,
) -> RunConfigSnapshot {
    LayerStack {
        built_in: ConfigLayer {
            schema_version: CONFIG_SCHEMA_VERSION,
            budgets: BudgetLayer {
                max_tokens: Some(100),
                max_cost_microusd: Some(100),
                max_turns: Some(100),
            },
            concurrency: ConcurrencyLayer {
                max_runs: Some(2),
                max_tools: Some(2),
            },
            retention: RetentionLayer {
                event_days: Some(7),
                artifact_days: Some(7),
            },
            provider: Some(Provider::Anthropic),
            executor: Some(Executor::Local),
            grammar_edit: Some(Default::default()),
            grants: Some(authority.clone()),
        },
        user: None,
        project: None,
        run: None,
        experiment: None,
    }
    .materialize(
        RunConfigContext {
            principal_id,
            project_id,
            run_id,
        },
        &authority,
    )
    .unwrap()
}

fn identity(name: &str, implementation: &[u8]) -> CapabilityIdentity {
    CapabilityIdentity::new(
        CapabilitySource::new("native").unwrap(),
        CapabilityNamespace::new("kit.workspace").unwrap(),
        CapabilityName::new(name).unwrap(),
        CapabilityVersion::new("1.0.0").unwrap(),
        Digest::of(DigestAlgorithm::Blake3, implementation),
    )
}

#[test]
fn adapter_has_no_alternate_dispatcher_or_basic_executor_path() {
    let source = include_str!("../../src/agent/adapters/tool.rs");
    assert!(!source.contains("BasicToolExecutor"));
    assert!(!source.contains("pub struct Dispatcher"));
    assert!(!source.contains("pub fn dispatch"));
    assert_eq!(
        source.matches("OrchestratedNativeInvocation::new(").count(),
        1
    );
    assert!(!source.contains("invoke::invoke("));
}
