use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use agentkit_core::{
    FinishReason, Item, ItemKind, MetadataMap, Part, TextPart, ToolCallId as AgentkitToolCallId,
    ToolCallPart, ToolOutput, ToolResultPart, TurnCancellation,
};
use agentkit_loop::{
    LoopError, LoopInterrupt, LoopStep, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent,
    ModelTurnResult, SessionConfig, TurnRequest,
};
use agentkit_tools_core::{
    ToolContext, ToolExecutionOutcome, ToolExecutor, ToolRequest, ToolResult, ToolSpec,
};
use kit::{
    agent::{
        agentkit_bridge::mapping::from_agentkit_item,
        driver::{
            restart::{
                BoundarySnapshot, CommittedModelOutcome, LoopCommit, LoopRecord, RecoveryState,
                RestartProjection, SafeBoundary, append_loop_record,
            },
            waiting::{WaitingKind, WaitingResolution, WaitingState},
        },
    },
    api::{
        auth::{
            contract::{Authenticator, GrantSnapshot},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        service::AttemptProjection,
    },
    domain::{
        config::Grant,
        events::{ApprovalDecision, TraceId, UtcDateTime},
        ids::{
            ApprovalId, AttemptId, CommandId, EventId, PrincipalId, ProjectId, RunId, ToolCallId,
        },
        lifecycle::{AttemptOwnership, AttemptState, FencingToken},
    },
    store::sqlite::{append::SqliteStore, idempotency::IdempotencyKey},
    test_support::open_sqlite_store,
};

const UID: u32 = 501;

#[derive(Clone, Default)]
struct FakeAdapter {
    state: Arc<FakeState>,
}

#[derive(Default)]
struct FakeState {
    scripts: Mutex<VecDeque<ModelTurnResult>>,
    requests: Mutex<Vec<TurnRequest>>,
    dispatches: AtomicUsize,
}

impl FakeAdapter {
    fn with_result(result: ModelTurnResult) -> Self {
        let adapter = Self::default();
        adapter.state.scripts.lock().unwrap().push_back(result);
        adapter
    }

    fn dispatches(&self) -> usize {
        self.state.dispatches.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<TurnRequest> {
        self.state.requests.lock().unwrap().clone()
    }
}

struct FakeSession {
    state: Arc<FakeState>,
}

struct FakeTurn {
    events: VecDeque<ModelTurnEvent>,
}

#[derive(Clone)]
struct CountingTool {
    calls: Arc<AtomicUsize>,
    spec: ToolSpec,
}

impl CountingTool {
    fn new() -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            spec: ToolSpec::new("echo", "echo once", serde_json::json!({"type": "object"})),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ToolExecutor for CountingTool {
    fn specs(&self) -> Vec<ToolSpec> {
        vec![self.spec.clone()]
    }

    fn execute<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        request: ToolRequest,
        _ctx: &'life1 mut ToolContext<'life2>,
    ) -> Pin<Box<dyn Future<Output = ToolExecutionOutcome> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ToolExecutionOutcome::Completed(ToolResult::new(ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Text("executed-once".into()),
                is_error: false,
                metadata: MetadataMap::new(),
            }))
        })
    }
}

impl ModelAdapter for FakeAdapter {
    type Session = FakeSession;

    fn start_session<'life0, 'async_trait>(
        &'life0 self,
        _config: SessionConfig,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Session, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            Ok(FakeSession {
                state: Arc::clone(&self.state),
            })
        })
    }
}

impl ModelSession for FakeSession {
    type Turn = FakeTurn;

    fn begin_turn<'life0, 'async_trait>(
        &'life0 mut self,
        request: TurnRequest,
        _cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Turn, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.state.dispatches.fetch_add(1, Ordering::SeqCst);
            self.state.requests.lock().unwrap().push(request);
            let result = self
                .state
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| LoopError::Provider("unexpected fake provider dispatch".into()))?;
            Ok(FakeTurn {
                events: VecDeque::from([ModelTurnEvent::Finished(result)]),
            })
        })
    }
}

impl ModelTurn for FakeTurn {
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
        Box::pin(async move { Ok(self.events.pop_front()) })
    }
}

struct Journal {
    path: PathBuf,
    store: SqliteStore,
}

impl Journal {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kit-loop-restart-{}-{}.sqlite3",
            std::process::id(),
            EventId::generate().unwrap()
        ));
        Self {
            store: open_sqlite_store(&path).unwrap(),
            path,
        }
    }

    fn append(&mut self, owner: AttemptOwnership, version: u64, record: LoopRecord) {
        append_loop_record(&mut self.store, commit(owner, version, record)).unwrap();
    }

    fn events(&self) -> Vec<kit::store::sqlite::append::StoredEvent> {
        self.store.events().unwrap()
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(self.path.with_extension("sqlite3-shm"));
    }
}

fn new_owner() -> AttemptOwnership {
    AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        PrincipalId::generate().unwrap(),
        FencingToken::new(7),
    )
}

fn make_projection(owner: AttemptOwnership) -> AttemptProjection {
    AttemptProjection {
        id: owner.attempt_id,
        run_id: RunId::generate().unwrap(),
        state: AttemptState::Executing,
        owner,
        version: 3,
    }
}

fn commit(owner: AttemptOwnership, version: u64, record: LoopRecord) -> LoopCommit {
    LoopCommit {
        owner,
        claim: None,
        expected_stream_version: version,
        idempotency_key: IdempotencyKey::parse(&format!(
            "loop-{}-{}",
            version,
            CommandId::generate().unwrap()
        ))
        .unwrap(),
        command_id: CommandId::generate().unwrap(),
        event_id: EventId::generate().unwrap(),
        occurred_at: UtcDateTime::parse("2026-07-22T12:00:00Z").unwrap(),
        trace_id: TraceId::parse("loop-restart-test").unwrap(),
        artifacts: Vec::new(),
        record,
    }
}

fn text(kind: ItemKind, value: &str) -> Item {
    Item::new(kind, vec![Part::Text(TextPart::new(value))])
}

fn result(value: &str) -> ModelTurnResult {
    ModelTurnResult {
        finish_reason: FinishReason::Completed,
        output_items: vec![text(ItemKind::Assistant, value)],
        usage: None,
        metadata: MetadataMap::new(),
        model: Some("fake".into()),
        response_id: None,
    }
}

fn tool_call_result(call: ToolCallPart) -> ModelTurnResult {
    ModelTurnResult {
        finish_reason: FinishReason::ToolCall,
        output_items: vec![Item::new(ItemKind::Assistant, vec![Part::ToolCall(call)])],
        usage: None,
        metadata: MetadataMap::new(),
        model: Some("fake".into()),
        response_id: None,
    }
}

fn canonical(items: &[Item]) -> Vec<kit::agent::agentkit_bridge::mapping::CanonicalItem> {
    items.iter().map(from_agentkit_item).collect()
}

fn boundary(
    kind: SafeBoundary,
    transcript: &[Item],
    resume_index: Option<usize>,
    model_outcome: Option<&ModelTurnResult>,
) -> BoundarySnapshot {
    BoundarySnapshot {
        boundary: kind,
        transcript: canonical(transcript),
        resume_index,
        model_outcome: model_outcome.map(CommittedModelOutcome::from_agentkit),
    }
}

fn ready(state: RecoveryState) -> kit::agent::driver::restart::RestartPlan {
    match state {
        RecoveryState::Ready(plan) => plan,
        other => panic!("expected ready restart, got {other:?}"),
    }
}

fn authenticated(principal: PrincipalId) -> kit::api::auth::contract::AuthenticatedPrincipal {
    let project = ProjectId::generate().unwrap();
    let authenticator = LocalPeerAuthenticator::new(BTreeMap::from([(
        UID,
        GrantSnapshot::new(principal, project, [Grant::WorkspaceRead]),
    )]));
    authenticator
        .authenticate(&LocalPeerObservation::from_transport(UID, 42, UID))
        .unwrap()
}

#[tokio::test]
async fn every_safe_boundary_restarts_without_duplicate_provider_or_transcript_items() {
    // Before model dispatch: pending user input is consumed exactly once.
    let owner = new_owner();
    let projection = make_projection(owner);
    let input = text(ItemKind::User, "before");
    let mut journal = Journal::new();
    journal.append(
        owner,
        0,
        LoopRecord::Boundary(boundary(
            SafeBoundary::BeforeModelDispatch,
            std::slice::from_ref(&input),
            Some(0),
            None,
        )),
    );
    let adapter = FakeAdapter::with_result(result("fresh"));
    let mut driver = ready(RestartProjection::reconstruct(&projection, &journal.events()).unwrap())
        .start(&projection, adapter.clone(), |builder| builder)
        .await
        .unwrap();
    assert!(matches!(
        driver.poll(&projection).await.unwrap(),
        LoopStep::Finished(_)
    ));
    assert_eq!(adapter.dispatches(), 1);
    assert_eq!(adapter.requests()[0].transcript.len(), 1);
    assert_eq!(adapter.requests()[0].transcript[0].kind, input.kind);
    assert_eq!(adapter.requests()[0].transcript[0].parts, input.parts);

    // After model outcome: the committed result is replayed inside LoopDriver.
    let owner = new_owner();
    let projection = make_projection(owner);
    let user = text(ItemKind::User, "model");
    let call = ToolCallPart {
        id: AgentkitToolCallId::new("committed-call"),
        name: "echo".into(),
        input: serde_json::json!({"value": "committed"}),
        metadata: MetadataMap::new(),
    };
    let outcome = tool_call_result(call);
    let transcript = [user.clone(), outcome.output_items[0].clone()];
    let mut journal = Journal::new();
    journal.append(
        owner,
        0,
        LoopRecord::Boundary(boundary(
            SafeBoundary::AfterModelOutcome,
            &transcript,
            Some(0),
            Some(&outcome),
        )),
    );
    let adapter = FakeAdapter::default();
    let tool = CountingTool::new();
    let mut driver = ready(RestartProjection::reconstruct(&projection, &journal.events()).unwrap())
        .start(&projection, adapter.clone(), |builder| {
            builder.tool_executor(tool.clone())
        })
        .await
        .unwrap();
    assert!(matches!(
        driver.poll(&projection).await.unwrap(),
        LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_))
    ));
    assert_eq!(adapter.dispatches(), 0);
    assert_eq!(tool.calls(), 1);

    // After tool outcome: committed tool results enter as pending loop input.
    let owner = new_owner();
    let projection = make_projection(owner);
    let call = ToolCallPart {
        id: AgentkitToolCallId::new("call-1"),
        name: "echo".into(),
        input: serde_json::json!({"value": "x"}),
        metadata: MetadataMap::new(),
    };
    let transcript = vec![
        text(ItemKind::User, "tool"),
        Item::new(ItemKind::Assistant, vec![Part::ToolCall(call.clone())]),
        Item::new(
            ItemKind::Tool,
            vec![Part::ToolResult(ToolResultPart {
                call_id: call.id,
                output: ToolOutput::Text("once".into()),
                is_error: false,
                metadata: MetadataMap::new(),
            })],
        ),
    ];
    let mut journal = Journal::new();
    journal.append(
        owner,
        0,
        LoopRecord::Boundary(boundary(
            SafeBoundary::AfterToolOutcome,
            &transcript,
            Some(2),
            None,
        )),
    );
    let adapter = FakeAdapter::with_result(result("after-tool"));
    let mut driver = ready(RestartProjection::reconstruct(&projection, &journal.events()).unwrap())
        .start(&projection, adapter.clone(), |builder| builder)
        .await
        .unwrap();
    assert!(matches!(
        driver.poll(&projection).await.unwrap(),
        LoopStep::Finished(_)
    ));
    assert_eq!(adapter.dispatches(), 1);
    assert_eq!(adapter.requests()[0].transcript.len(), transcript.len());
    assert_eq!(
        adapter.requests()[0]
            .transcript
            .iter()
            .map(|item| (&item.kind, &item.parts))
            .collect::<Vec<_>>(),
        transcript
            .iter()
            .map(|item| (&item.kind, &item.parts))
            .collect::<Vec<_>>()
    );

    // Turn end is passive and waits without dispatching anything.
    let owner = new_owner();
    let projection = make_projection(owner);
    let transcript = [
        text(ItemKind::User, "done"),
        text(ItemKind::Assistant, "ok"),
    ];
    let mut journal = Journal::new();
    journal.append(
        owner,
        0,
        LoopRecord::Boundary(boundary(SafeBoundary::TurnEnd, &transcript, None, None)),
    );
    let adapter = FakeAdapter::default();
    let mut driver = ready(RestartProjection::reconstruct(&projection, &journal.events()).unwrap())
        .start(&projection, adapter.clone(), |builder| builder)
        .await
        .unwrap();
    assert!(matches!(
        driver.poll(&projection).await.unwrap(),
        LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_))
    ));
    assert_eq!(adapter.dispatches(), 0);
}

#[tokio::test]
async fn input_approval_and_auth_waits_survive_and_require_authenticated_resolution() {
    let owner = new_owner();
    let projection = make_projection(owner);
    let principal = authenticated(owner.principal_id);

    let input_wait = WaitingState {
        wait_id: CommandId::generate().unwrap(),
        principal_id: owner.principal_id,
        kind: WaitingKind::Input,
        snapshot: boundary(SafeBoundary::TurnEnd, &[], None, None),
    };
    let mut journal = Journal::new();
    journal.append(owner, 0, LoopRecord::Waiting(input_wait.clone()));
    assert!(matches!(
        RestartProjection::reconstruct(&projection, &journal.events()).unwrap(),
        RecoveryState::Waiting(_)
    ));
    journal.append(
        owner,
        1,
        input_wait
            .resolve_input(&principal, vec![text(ItemKind::User, "resume-input")])
            .unwrap(),
    );
    let adapter = FakeAdapter::with_result(result("input-resumed"));
    let mut driver = ready(RestartProjection::reconstruct(&projection, &journal.events()).unwrap())
        .start(&projection, adapter.clone(), |builder| builder)
        .await
        .unwrap();
    assert!(matches!(
        driver.poll(&projection).await.unwrap(),
        LoopStep::Finished(_)
    ));
    assert_eq!(adapter.dispatches(), 1);

    let owner = new_owner();
    let projection = make_projection(owner);
    let principal = authenticated(owner.principal_id);
    let tool_call_id = ToolCallId::generate().unwrap();
    let call = ToolCallPart {
        id: AgentkitToolCallId::new(tool_call_id.to_string()),
        name: "echo".into(),
        input: serde_json::json!({}),
        metadata: MetadataMap::new(),
    };
    let model_outcome = tool_call_result(call);
    let approval_transcript = [
        text(ItemKind::User, "needs approval"),
        model_outcome.output_items[0].clone(),
    ];
    let approval_wait = WaitingState {
        wait_id: CommandId::generate().unwrap(),
        principal_id: owner.principal_id,
        kind: WaitingKind::Approval {
            approval_id: ApprovalId::generate().unwrap(),
            tool_call_id,
        },
        snapshot: boundary(
            SafeBoundary::AfterModelOutcome,
            &approval_transcript,
            Some(0),
            Some(&model_outcome),
        ),
    };
    let mut journal = Journal::new();
    journal.append(owner, 0, LoopRecord::Waiting(approval_wait.clone()));
    assert!(matches!(
        RestartProjection::reconstruct(&projection, &journal.events()).unwrap(),
        RecoveryState::Waiting(_)
    ));
    journal.append(
        owner,
        1,
        approval_wait
            .resolve(
                &principal,
                WaitingResolution::Approval {
                    decision: ApprovalDecision::Denied,
                },
            )
            .unwrap(),
    );
    let adapter = FakeAdapter::with_result(result("approval-denied-observed"));
    let mut driver = ready(RestartProjection::reconstruct(&projection, &journal.events()).unwrap())
        .start(&projection, adapter.clone(), |builder| builder)
        .await
        .unwrap();
    assert!(matches!(
        driver.poll(&projection).await.unwrap(),
        LoopStep::Finished(_)
    ));
    assert_eq!(adapter.dispatches(), 1);
    assert!(matches!(
        adapter.requests()[0].transcript[2].parts[0],
        Part::ToolResult(ToolResultPart { is_error: true, .. })
    ));

    let owner = new_owner();
    let projection = make_projection(owner);
    let principal = authenticated(owner.principal_id);
    let auth_wait = WaitingState {
        wait_id: CommandId::generate().unwrap(),
        principal_id: owner.principal_id,
        kind: WaitingKind::Auth {
            run_id: projection.run_id,
            scope: "provider.read".into(),
        },
        snapshot: boundary(
            SafeBoundary::BeforeModelDispatch,
            &[text(ItemKind::User, "needs auth")],
            Some(0),
            None,
        ),
    };
    let mut journal = Journal::new();
    journal.append(owner, 0, LoopRecord::Waiting(auth_wait.clone()));
    assert!(matches!(
        RestartProjection::reconstruct(&projection, &journal.events()).unwrap(),
        RecoveryState::Waiting(_)
    ));
    journal.append(
        owner,
        1,
        auth_wait
            .resolve(&principal, WaitingResolution::Auth { granted: true })
            .unwrap(),
    );
    let adapter = FakeAdapter::with_result(result("auth-resumed"));
    let mut driver = ready(RestartProjection::reconstruct(&projection, &journal.events()).unwrap())
        .start(&projection, adapter.clone(), |builder| builder)
        .await
        .unwrap();
    assert!(matches!(
        driver.poll(&projection).await.unwrap(),
        LoopStep::Finished(_)
    ));
    assert_eq!(adapter.dispatches(), 1);

    let mut denied = Journal::new();
    denied.append(owner, 0, LoopRecord::Waiting(auth_wait.clone()));
    denied.append(
        owner,
        1,
        auth_wait
            .resolve(&principal, WaitingResolution::Auth { granted: false })
            .unwrap(),
    );
    assert!(matches!(
        RestartProjection::reconstruct(&projection, &denied.events()).unwrap(),
        RecoveryState::Cancelled(_)
    ));

    let other = authenticated(PrincipalId::generate().unwrap());
    assert!(matches!(
        WaitingState {
            wait_id: CommandId::generate().unwrap(),
            principal_id: owner.principal_id,
            kind: WaitingKind::Input,
            snapshot: boundary(SafeBoundary::TurnEnd, &[], None, None),
        }
        .resolve_input(&other, vec![]),
        Err(kit::agent::driver::waiting::WaitingError::Unauthorized)
    ));
}

#[test]
fn cancellation_is_committed_before_agentkit_observes_it() {
    let owner = new_owner();
    let projection = make_projection(owner);
    let mut journal = Journal::new();
    journal.append(
        owner,
        0,
        LoopRecord::Boundary(boundary(
            SafeBoundary::BeforeModelDispatch,
            &[text(ItemKind::User, "cancel")],
            Some(0),
            None,
        )),
    );
    journal.append(owner, 1, LoopRecord::CancellationRequested);
    assert!(matches!(
        RestartProjection::reconstruct(&projection, &journal.events()).unwrap(),
        RecoveryState::Cancelled(_)
    ));

    let owner = new_owner();
    let projection = make_projection(owner);
    let waiting = WaitingState {
        wait_id: CommandId::generate().unwrap(),
        principal_id: owner.principal_id,
        kind: WaitingKind::Approval {
            approval_id: ApprovalId::generate().unwrap(),
            tool_call_id: ToolCallId::generate().unwrap(),
        },
        snapshot: boundary(SafeBoundary::TurnEnd, &[], None, None),
    };
    let mut journal = Journal::new();
    journal.append(owner, 0, LoopRecord::Waiting(waiting));
    journal.append(owner, 1, LoopRecord::CancellationRequested);
    assert!(matches!(
        RestartProjection::reconstruct(&projection, &journal.events()).unwrap(),
        RecoveryState::Cancelled(_)
    ));
}

#[tokio::test]
async fn test_only_unclaimed_drivers_do_not_supply_ownership_authority() {
    let owner = new_owner();
    let projection = make_projection(owner);
    let snapshot = boundary(SafeBoundary::TurnEnd, &[], None, None);

    let first_adapter = FakeAdapter::default();
    let mut first = kit::agent::driver::restart::RestartPlan {
        owner,
        claim: None,
        snapshot: snapshot.clone(),
        approved_tool: None,
    }
    .start(&projection, first_adapter, |builder| builder)
    .await
    .unwrap();
    let second_adapter = FakeAdapter::default();
    let second = kit::agent::driver::restart::RestartPlan {
        owner,
        claim: None,
        snapshot,
        approved_tool: None,
    }
    .start(&projection, second_adapter, |builder| builder)
    .await;

    assert!(second.is_ok());
    drop(second);
    let committed = AtomicUsize::new(0);
    assert!(matches!(
        first
            .commit(&projection, || {
                committed.fetch_add(1, Ordering::SeqCst);
                Ok::<_, std::convert::Infallible>(())
            })
            .await,
        Ok(())
    ));
    assert_eq!(committed.load(Ordering::SeqCst), 1);
    assert!(matches!(
        first.poll(&projection).await.unwrap(),
        LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_))
    ));
    let mut revoked = projection.clone();
    revoked.state = AttemptState::Quiescing;
    revoked.version += 1;
    first.revoke(&revoked).await.unwrap();

    let replacement_owner = AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        owner.principal_id,
        FencingToken::new(owner.fencing_token.get() + 1),
    );
    let mut replacement_projection = make_projection(replacement_owner);
    replacement_projection.run_id = projection.run_id;
    let replacement = kit::agent::driver::restart::RestartPlan {
        owner: replacement_owner,
        claim: None,
        snapshot: boundary(SafeBoundary::TurnEnd, &[], None, None),
        approved_tool: None,
    }
    .start(&replacement_projection, FakeAdapter::default(), |builder| {
        builder
    })
    .await;
    assert!(replacement.is_ok());
}
