use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use agentkit_core::{
    CostUsage, Delta, FinishReason, Item, ItemKind, MetadataMap, Part, PartId, PartKind,
    ReasoningPart, TextPart, TokenUsage, TurnCancellation, Usage,
};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, ModelTurnResult,
    SessionConfig, TurnRequest,
};
use kit::{
    agent::{
        adapters::model::{
            DurableModelAdapter, ModelCrashPoint, ModelPolicy, ModelSecurity, ProviderIdempotency,
        },
        agentkit_bridge::mapping::from_agentkit_item,
        driver::restart::{
            BoundarySnapshot, LoopCommit, LoopRecord, RecoveryState, RestartProjection,
            SafeBoundary, append_loop_record,
        },
    },
    api::{
        auth::{
            contract::{Authenticator, GrantSnapshot},
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
    },
    domain::{
        config::{
            BudgetLayer, CONFIG_SCHEMA_VERSION, ConcurrencyLayer, ConfigLayer, Executor, Grant,
            LayerStack, Provider, RetentionLayer, RunConfigContext,
        },
        events::{TraceId, UtcDateTime},
        ids::{AttemptId, CommandId, EventId, PrincipalId, ProjectId, RunId, WorkspaceId},
        lifecycle::{AttemptOwnership, AttemptState, FencingToken},
    },
    runtime::scheduler::{
        AdmissionKind, DurableScheduler, ReservationRequest, SchedulerError, limits::Spend,
        reserve::ReservationId,
    },
    store::sqlite::idempotency::IdempotencyKey,
    test_support,
};

const UID: u32 = 501;

#[derive(Clone, Default)]
struct FakeAdapter {
    state: Arc<FakeState>,
}

#[derive(Default)]
struct FakeState {
    scripts: Mutex<VecDeque<VecDeque<ModelTurnEvent>>>,
    dispatches: AtomicUsize,
}

impl FakeAdapter {
    fn with_events(events: impl IntoIterator<Item = ModelTurnEvent>) -> Self {
        let adapter = Self::default();
        adapter
            .state
            .scripts
            .lock()
            .unwrap()
            .push_back(events.into_iter().collect());
        adapter
    }

    fn dispatches(&self) -> usize {
        self.state.dispatches.load(Ordering::SeqCst)
    }
}

struct FakeSession {
    state: Arc<FakeState>,
}

struct FakeTurn {
    events: VecDeque<ModelTurnEvent>,
}

#[derive(Clone)]
struct HangingAdapter;

struct HangingSession;

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

    fn provider_name(&self) -> Option<&str> {
        Some("fake")
    }
}

impl ModelSession for FakeSession {
    type Turn = FakeTurn;

    fn begin_turn<'life0, 'async_trait>(
        &'life0 mut self,
        _request: TurnRequest,
        _cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Turn, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.state.dispatches.fetch_add(1, Ordering::SeqCst);
            let events = self
                .state
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| LoopError::Provider("unexpected fake dispatch".to_owned()))?;
            Ok(FakeTurn { events })
        })
    }

    fn model_name(&self) -> Option<&str> {
        Some("fake-model")
    }

    fn prepare_turn(&mut self, request: &mut TurnRequest) -> Result<(), LoopError> {
        if request.generation.temperature.is_some() {
            Err(LoopError::Unsupported(
                "temperature is unsupported".to_owned(),
            ))
        } else {
            Ok(())
        }
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

impl ModelAdapter for HangingAdapter {
    type Session = HangingSession;

    fn start_session<'life0, 'async_trait>(
        &'life0 self,
        _config: SessionConfig,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Session, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(HangingSession) })
    }

    fn provider_name(&self) -> Option<&str> {
        Some("hanging")
    }
}

impl ModelSession for HangingSession {
    type Turn = FakeTurn;

    fn begin_turn<'life0, 'async_trait>(
        &'life0 mut self,
        _request: TurnRequest,
        _cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Turn, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(std::future::pending())
    }

    fn model_name(&self) -> Option<&str> {
        Some("fake-model")
    }
}

struct TestDatabase {
    root: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kit-model-intent-outcome-{}-{}",
            std::process::id(),
            EventId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("state.sqlite3");
        drop(test_support::open_service_store(&path).unwrap());
        Self { root, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Fixture {
    database: TestDatabase,
    scheduler: DurableScheduler,
    security: ModelSecurity,
    policy: ModelPolicy,
    fake: FakeAdapter,
}

impl Fixture {
    fn new(events: impl IntoIterator<Item = ModelTurnEvent>) -> Self {
        let database = TestDatabase::new();
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let workspace = WorkspaceId::generate().unwrap();
        let run = RunId::generate().unwrap();
        let authority = BTreeSet::from([Grant::ModelCall]);
        let config = LayerStack {
            built_in: ConfigLayer {
                schema_version: CONFIG_SCHEMA_VERSION,
                budgets: BudgetLayer {
                    max_tokens: Some(1_000),
                    max_cost_microusd: Some(1_000),
                    max_turns: Some(10),
                },
                concurrency: ConcurrencyLayer {
                    max_runs: Some(2),
                    max_tools: Some(2),
                },
                retention: RetentionLayer {
                    event_days: Some(7),
                    artifact_days: Some(7),
                },
                provider: Some(Provider::OpenAi),
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
                principal_id: principal,
                project_id: project,
                run_id: run,
            },
            &authority,
        )
        .unwrap();
        let authenticated = LocalPeerAuthenticator::new(BTreeMap::from([(
            UID,
            GrantSnapshot::new(principal, project, authority),
        )]))
        .authenticate(&LocalPeerObservation::from_transport(UID, 42, UID))
        .unwrap();
        let capability = CapabilityIdentity::new(
            CapabilitySource::new("native").unwrap(),
            CapabilityNamespace::new("kit.model").unwrap(),
            CapabilityName::new("call").unwrap(),
            CapabilityVersion::new("1.0.0").unwrap(),
            Digest::of(DigestAlgorithm::Blake3, b"durable model adapter"),
        );
        let schema = Digest::of(DigestAlgorithm::Sha256, b"model call schema");
        let constraints = ArgumentConstraints::default();
        let grants = CapabilityGrantSnapshot::new(
            &config,
            [CapabilityGrant::new(
                principal,
                project,
                workspace,
                capability.clone(),
                schema,
                EffectClass::ModelCall,
                constraints.clone(),
            )],
            DigestAlgorithm::Sha256,
        );
        let attempt = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal,
            FencingToken::new(7),
        );
        let scheduler = DurableScheduler::open(database.path()).unwrap();
        scheduler
            .register_run_with_snapshot(run, principal, "model-run", &config)
            .unwrap();
        scheduler.admit_run(run).unwrap();
        let claim = test_support::open_sqlite_store(database.path())
            .unwrap()
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id: run,
                attempt_id: attempt.attempt_id,
                principal_id: attempt.principal_id,
                fence: attempt.fencing_token,
                lease_version: 1,
                expires_at_unix_micros: 0,
            })
            .unwrap();

        Self {
            database,
            scheduler,
            security: ModelSecurity {
                authenticated,
                config,
                grants,
                delegation: None,
                capability,
                schema_digest: schema,
                argument_constraints: constraints,
                workspace_id: workspace,
                attempt,
                claim,
            },
            policy: ModelPolicy {
                reservation: Spend::new(10, 100, 1, 0, 0),
                provider_idempotency: ProviderIdempotency::Unproven,
                max_buffered_bytes: 1024 * 1024,
                max_delta_bytes: 4,
                detached: false,
                unbounded_output_allowance: 0,
            },
            fake: FakeAdapter::with_events(events),
        }
    }

    fn adapter(&self, crash: Option<ModelCrashPoint>) -> DurableModelAdapter<FakeAdapter> {
        let adapter = DurableModelAdapter::new(
            self.fake.clone(),
            test_support::open_sqlite_store(self.database.path()).unwrap(),
            self.scheduler.clone(),
            self.security.clone(),
            self.policy,
            UtcDateTime::parse("2026-07-22T12:00:00Z").unwrap(),
            TraceId::parse("model-intent-outcome").unwrap(),
        );
        match crash {
            Some(point) => adapter.with_crash_at(point),
            None => adapter,
        }
    }

    fn events(&self) -> Vec<kit::store::sqlite::append::StoredEvent> {
        test_support::open_sqlite_store(self.database.path())
            .unwrap()
            .events()
            .unwrap()
    }

    fn projection(&self) -> kit::api::service::AttemptProjection {
        kit::api::service::AttemptProjection {
            id: self.security.attempt.attempt_id,
            run_id: self.security.config.run_id(),
            state: AttemptState::Executing,
            owner: self.security.attempt,
            version: 1,
        }
    }

    fn seed_boundary(&self) {
        let input = Item::text(ItemKind::User, "hello");
        append_loop_record(
            &mut test_support::open_sqlite_store(self.database.path()).unwrap(),
            LoopCommit {
                owner: self.security.attempt,
                claim: None,
                expected_stream_version: 0,
                idempotency_key: IdempotencyKey::parse("model-test-boundary").unwrap(),
                command_id: CommandId::generate().unwrap(),
                event_id: EventId::generate().unwrap(),
                occurred_at: UtcDateTime::parse("2026-07-22T12:00:00Z").unwrap(),
                trace_id: TraceId::parse("model-intent-outcome").unwrap(),
                artifacts: Vec::new(),
                record: LoopRecord::Boundary(BoundarySnapshot {
                    boundary: SafeBoundary::BeforeModelDispatch,
                    transcript: vec![from_agentkit_item(&input)],
                    resume_index: Some(0),
                    model_outcome: None,
                }),
            },
        )
        .unwrap();
    }
}

fn result() -> ModelTurnResult {
    ModelTurnResult {
        finish_reason: FinishReason::Completed,
        output_items: vec![Item::new(
            ItemKind::Assistant,
            vec![
                Part::Reasoning(ReasoningPart::summary("private chain")),
                Part::Text(TextPart::new("public answer")),
            ],
        )],
        usage: Some(Usage::new(TokenUsage::new(12, 3).with_reasoning_tokens(5))),
        metadata: MetadataMap::new(),
        model: Some("fake-model".to_owned()),
        response_id: Some("response-1".to_owned()),
    }
}

fn request() -> TurnRequest {
    TurnRequest {
        session_id: agentkit_core::SessionId::new("attempt-session"),
        turn_id: agentkit_core::TurnId::new("turn-0"),
        transcript: vec![Item::text(ItemKind::User, "hello")],
        available_tools: Vec::new(),
        cache: None,
        structured_output: None,
        generation: Default::default(),
        metadata: MetadataMap::new(),
    }
}

async fn begin(
    adapter: DurableModelAdapter<FakeAdapter>,
) -> Result<<DurableModelAdapter<FakeAdapter> as ModelAdapter>::Session, LoopError> {
    adapter
        .start_session(SessionConfig::new("attempt-session"))
        .await
}

async fn drain<S: ModelSession>(session: &mut S) -> Result<Vec<ModelTurnEvent>, LoopError> {
    drain_request(session, request()).await
}

async fn drain_request<S: ModelSession>(
    session: &mut S,
    request: TurnRequest,
) -> Result<Vec<ModelTurnEvent>, LoopError> {
    let mut turn = session.begin_turn(request, None).await?;
    let mut events = Vec::new();
    while let Some(event) = turn.next_event(None).await? {
        events.push(event);
    }
    Ok(events)
}

#[tokio::test]
async fn replay_rejects_a_changed_projected_request_digest() {
    let fixture = Fixture::new([ModelTurnEvent::Finished(result())]);
    let mut crashed = begin(fixture.adapter(Some(ModelCrashPoint::BetweenIntentAndDispatch)))
        .await
        .unwrap();
    assert!(drain(&mut crashed).await.is_err());
    assert_eq!(fixture.fake.dispatches(), 0);

    fixture.scheduler.reconcile_startup().unwrap();
    let mut restarted = begin(fixture.adapter(None)).await.unwrap();
    let mut changed = request();
    changed
        .transcript
        .push(Item::text(ItemKind::Developer, "changed projection"));
    let error = drain_request(&mut restarted, changed).await.unwrap_err();
    assert!(error.to_string().contains("invalid durable model record"));
    assert_eq!(fixture.fake.dispatches(), 0);
}

/// A completed turn replayed after a driver-claim lease renewal must resume
/// cleanly. The replay re-appends the outcome journal record as crash
/// repair, and the journal digest binds the claim and a recomputed boundary
/// snapshot — the existing terminal record under the identity key must be
/// accepted, never reported as "a different request".
#[tokio::test]
async fn replay_after_lease_renewal_repairs_the_outcome_journal() {
    let mut fixture = Fixture::new([ModelTurnEvent::Finished(result())]);
    fixture.seed_boundary();
    let mut first = begin(fixture.adapter(None)).await.unwrap();
    drain(&mut first).await.unwrap();
    assert_eq!(fixture.fake.dispatches(), 1);

    let renewed = test_support::open_sqlite_store(fixture.database.path())
        .unwrap()
        .install_driver_claim_for_test(AttemptDriverClaim {
            run_id: fixture.security.config.run_id(),
            attempt_id: fixture.security.attempt.attempt_id,
            principal_id: fixture.security.attempt.principal_id,
            fence: fixture.security.attempt.fencing_token,
            lease_version: 2,
            expires_at_unix_micros: 0,
        })
        .unwrap();
    fixture.security.claim = renewed;

    let mut replayed = begin(fixture.adapter(None)).await.unwrap();
    let events = drain(&mut replayed).await.unwrap();
    assert!(!events.is_empty());
    // Replay, not re-execution: the provider is never dispatched again.
    assert_eq!(fixture.fake.dispatches(), 1);
}

#[tokio::test]
async fn schema_bytes_can_exhaust_model_budget_before_intent_or_dispatch() {
    let fixture = Fixture::new([ModelTurnEvent::Finished(result())]);
    let mut session = begin(fixture.adapter(None)).await.unwrap();
    let mut request = request();
    request.structured_output = Some(
        agentkit_loop::StructuredOutputRequest::new(
            "large",
            1,
            true,
            serde_json::json!({
                "type": "object",
                "properties": {"payload": {"const": "x".repeat(12_000)}}
            }),
        )
        .unwrap(),
    );
    assert!(drain_request(&mut session, request).await.is_err());
    assert_eq!(fixture.fake.dispatches(), 0);
    assert!(fixture.events().is_empty());
}

#[tokio::test]
async fn intent_budget_and_outcome_are_committed_before_bounded_visible_stream() {
    let text = PartId::new("text");
    let reasoning = PartId::new("reasoning");
    let fixture = Fixture::new([
        ModelTurnEvent::Delta(Delta::BeginPart {
            part_id: reasoning.clone(),
            kind: PartKind::Reasoning,
        }),
        ModelTurnEvent::Delta(Delta::AppendText {
            part_id: reasoning,
            chunk: "hidden thought".to_owned(),
        }),
        ModelTurnEvent::Delta(Delta::BeginPart {
            part_id: text.clone(),
            kind: PartKind::Text,
        }),
        ModelTurnEvent::Delta(Delta::AppendText {
            part_id: text,
            chunk: "abcdefghij".to_owned(),
        }),
        ModelTurnEvent::Finished(result()),
    ]);
    let mut session = begin(fixture.adapter(None)).await.unwrap();
    let visible = drain(&mut session).await.unwrap();

    assert_eq!(fixture.fake.dispatches(), 1);
    assert!(visible.iter().all(|event| {
        match event {
            ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => {
                chunk.len() <= fixture.policy.max_delta_bytes && !chunk.contains("hidden")
            }
            ModelTurnEvent::Delta(Delta::BeginPart { kind, .. }) => *kind != PartKind::Reasoning,
            ModelTurnEvent::Delta(Delta::CommitPart { part }) => {
                !matches!(part, Part::Reasoning(_))
            }
            ModelTurnEvent::Finished(result) => result
                .output_items
                .iter()
                .flat_map(|item| &item.parts)
                .all(|part| !matches!(part, Part::Reasoning(_))),
            _ => true,
        }
    }));
    assert_eq!(
        fixture
            .scheduler
            .totals(fixture.security.config.run_id())
            .unwrap()
            .committed,
        fixture.policy.reservation
    );

    let events = fixture
        .events()
        .into_iter()
        .filter(|event| event.event.event_type.as_str().starts_with("model_call."))
        .collect::<Vec<_>>();
    assert_eq!(
        events
            .iter()
            .map(|event| event.event.event_type.as_str())
            .collect::<Vec<_>>(),
        [INTENT_EVENT, "model_call.dispatched", "model_call.outcome"]
    );
    let intent: serde_json::Value = serde_json::from_slice(&events[0].event.payload).unwrap();
    for field in [
        "prompt_snapshot_digest",
        "config_snapshot_digest",
        "model_snapshot_digest",
    ] {
        assert!(intent[field].as_str().unwrap().starts_with("sha256:"));
    }
    let outcome: serde_json::Value = serde_json::from_slice(&events[2].event.payload).unwrap();
    assert_eq!(outcome["status"], "succeeded");
    assert_eq!(outcome["usage"]["tokens"]["input_tokens"], 12);
}

const INTENT_EVENT: &str = "model_call.intent";

#[tokio::test]
async fn crash_windows_reconcile_without_duplicate_dispatch_or_invented_success() {
    for point in [
        ModelCrashPoint::BeforeIntent,
        ModelCrashPoint::BetweenIntentAndDispatch,
        ModelCrashPoint::AfterDispatch,
        ModelCrashPoint::BeforeOutcome,
        ModelCrashPoint::AfterOutcome,
    ] {
        let fixture = Fixture::new([ModelTurnEvent::Finished(result())]);
        fixture.seed_boundary();
        let mut crashed = begin(fixture.adapter(Some(point))).await.unwrap();
        let first = drain(&mut crashed).await;
        assert!(first.is_err());

        let all_events_after_crash = fixture.events();
        let events_after_crash = all_events_after_crash
            .iter()
            .filter(|event| event.event.event_type.as_str().starts_with("model_call."))
            .collect::<Vec<_>>();
        match point {
            ModelCrashPoint::BeforeIntent => {
                assert!(events_after_crash.is_empty());
                assert_eq!(fixture.fake.dispatches(), 0);
                assert!(matches!(
                    RestartProjection::reconstruct(&fixture.projection(), &all_events_after_crash)
                        .unwrap(),
                    RecoveryState::Ready(_)
                ));
                continue;
            }
            ModelCrashPoint::BetweenIntentAndDispatch => {
                assert_eq!(events_after_crash.len(), 1);
                assert_eq!(fixture.fake.dispatches(), 0);
                assert!(matches!(
                    RestartProjection::reconstruct(&fixture.projection(), &all_events_after_crash)
                        .unwrap(),
                    RecoveryState::Ready(_)
                ));
            }
            ModelCrashPoint::AfterDispatch | ModelCrashPoint::BeforeOutcome => {
                assert_eq!(events_after_crash.len(), 3);
                assert_eq!(fixture.fake.dispatches(), 1);
                let outcome: serde_json::Value =
                    serde_json::from_slice(&events_after_crash[2].event.payload).unwrap();
                assert_eq!(outcome["status"], "outcome_unknown");
                assert!(matches!(
                    RestartProjection::reconstruct(
                        &fixture.projection(),
                        &all_events_after_crash
                    )
                    .unwrap(),
                    RecoveryState::OutcomeUnknown(outcomes)
                        if outcomes.len() == 1
                ));
            }
            ModelCrashPoint::AfterOutcome => {
                assert_eq!(events_after_crash.len(), 3);
                assert_eq!(fixture.fake.dispatches(), 1);
                let outcome: serde_json::Value =
                    serde_json::from_slice(&events_after_crash[2].event.payload).unwrap();
                assert_eq!(outcome["status"], "succeeded");
                assert_eq!(outcome["settlement"]["cost_microusd"], 10);
                assert!(matches!(
                    RestartProjection::reconstruct(&fixture.projection(), &all_events_after_crash)
                        .unwrap(),
                    RecoveryState::Ready(_)
                ));
            }
        }

        fixture.scheduler.reconcile_startup().unwrap();
        let mut restarted = begin(fixture.adapter(None)).await.unwrap();
        let recovered = drain(&mut restarted).await;
        if matches!(
            point,
            ModelCrashPoint::BetweenIntentAndDispatch | ModelCrashPoint::AfterOutcome
        ) {
            assert!(recovered.is_ok());
            assert_eq!(fixture.fake.dispatches(), 1);
            let events = fixture
                .events()
                .into_iter()
                .filter(|event| event.event.event_type.as_str().starts_with("model_call."))
                .collect::<Vec<_>>();
            let outcome: serde_json::Value =
                serde_json::from_slice(&events.last().unwrap().event.payload).unwrap();
            assert_eq!(outcome["status"], "succeeded");
        } else {
            assert!(recovered.is_err());
            assert_eq!(fixture.fake.dispatches(), 1);
            let events = fixture
                .events()
                .into_iter()
                .filter(|event| event.event.event_type.as_str().starts_with("model_call."))
                .collect::<Vec<_>>();
            assert_eq!(events.len(), 3);
            let outcome: serde_json::Value =
                serde_json::from_slice(&events[2].event.payload).unwrap();
            assert_eq!(outcome["status"], "outcome_unknown");
        }
    }
}

#[tokio::test]
async fn stale_owner_is_rejected_before_budget_intent_or_provider() {
    let fixture = Fixture::new([ModelTurnEvent::Finished(result())]);
    test_support::open_sqlite_store(fixture.database.path())
        .unwrap()
        .install_driver_claim_for_test(AttemptDriverClaim {
            attempt_id: AttemptId::generate().unwrap(),
            fence: FencingToken::new(8),
            lease_version: 2,
            ..fixture.security.claim
        })
        .unwrap();
    assert!(begin(fixture.adapter(None)).await.is_err());
    assert_eq!(fixture.fake.dispatches(), 0);
    assert!(fixture.events().is_empty());
    assert_eq!(
        fixture
            .scheduler
            .totals(fixture.security.config.run_id())
            .unwrap(),
        kit::runtime::scheduler::reserve::BudgetTotals {
            committed: Spend::ZERO,
            reserved: Spend::ZERO,
        }
    );
}

#[tokio::test]
async fn detached_model_call_requires_the_exact_model_grant_decision() {
    let mut fixture = Fixture::new([ModelTurnEvent::Finished(result())]);
    fixture.policy.detached = true;
    fixture.security.authenticated = LocalPeerAuthenticator::new(BTreeMap::from([(
        UID,
        GrantSnapshot::new(
            fixture.security.attempt.principal_id,
            fixture.security.config.project_id(),
            [],
        ),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(UID, 43, UID))
    .unwrap();

    assert!(begin(fixture.adapter(None)).await.is_err());
    assert_eq!(fixture.fake.dispatches(), 0);
    assert!(fixture.events().is_empty());
}

#[tokio::test]
async fn rejected_detached_output_never_enters_durable_model_records() {
    let secret = "provider-secret-that-must-not-persist";
    let mut rejected = result();
    rejected.output_items = vec![Item::text(ItemKind::Assistant, secret)];
    let mut fixture = Fixture::new([ModelTurnEvent::Finished(rejected)]);
    fixture.policy.detached = true;
    let adapter = fixture.adapter(None).with_outcome_validator(Arc::new(|_| {
        Err(LoopError::Provider("rejected by Kit validator".to_owned()))
    }));
    let mut session = begin(adapter).await.unwrap();
    assert!(drain(&mut session).await.is_err());

    let connection = rusqlite::Connection::open(fixture.database.path()).unwrap();
    let mut statement = connection
        .prepare("SELECT response FROM idempotency")
        .unwrap();
    let records = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(!records.is_empty());
    assert!(records.iter().all(|record| {
        !record
            .windows(secret.len())
            .any(|bytes| bytes == secret.as_bytes())
    }));
    let outcomes = fixture
        .events()
        .into_iter()
        .filter(|event| event.event.event_type.as_str() == "model_call.outcome")
        .collect::<Vec<_>>();
    assert_eq!(outcomes.len(), 1);
    let outcome: serde_json::Value = serde_json::from_slice(&outcomes[0].event.payload).unwrap();
    assert_eq!(outcome["status"], "outcome_unknown");
    assert_eq!(outcome["settlement"]["cost_microusd"], 10);
}

#[tokio::test]
async fn detached_actual_overage_replays_the_same_failure_without_output_or_redispatch() {
    let mut overage = result();
    overage.usage =
        Some(Usage::new(TokenUsage::new(120, 30)).with_cost(CostUsage::new(0.000_011, "USD")));
    let mut fixture = Fixture::new([ModelTurnEvent::Finished(overage)]);
    fixture.policy.detached = true;

    let mut first = begin(fixture.adapter(None)).await.unwrap();
    let first_error = drain(&mut first).await.unwrap_err().to_string();
    assert!(
        first_error.contains("actual usage exceeded"),
        "{first_error}"
    );
    assert_eq!(fixture.fake.dispatches(), 1);

    let outcome = fixture
        .events()
        .into_iter()
        .find(|event| event.event.event_type.as_str() == "model_call.outcome")
        .unwrap();
    let outcome: serde_json::Value = serde_json::from_slice(&outcome.event.payload).unwrap();
    let reservation = kit::runtime::scheduler::reserve::ReservationId::new(
        u128::from_str_radix(outcome["reservation_id"].as_str().unwrap(), 16).unwrap(),
    );
    let snapshot = fixture.scheduler.snapshot(reservation).unwrap();
    assert_eq!(
        snapshot.status(),
        kit::runtime::scheduler::reserve::ReservationStatus::ActualOverage
    );
    assert_eq!(snapshot.spend(), Spend::new(11, 150, 1, 0, 0));

    let mut replay = begin(fixture.adapter(None)).await.unwrap();
    let replay_error = drain(&mut replay).await.unwrap_err().to_string();
    assert!(
        replay_error.contains("actual usage exceeded"),
        "{replay_error}"
    );
    assert_eq!(fixture.fake.dispatches(), 1);
    assert_eq!(
        fixture
            .scheduler
            .totals(fixture.security.config.run_id())
            .unwrap()
            .committed,
        Spend::new(11, 150, 1, 0, 0)
    );
}

#[tokio::test]
async fn after_outcome_recovery_permanently_blocks_spend_after_actual_overage() {
    let mut overage = result();
    overage.usage =
        Some(Usage::new(TokenUsage::new(120, 30)).with_cost(CostUsage::new(0.000_011, "USD")));
    let mut fixture = Fixture::new([ModelTurnEvent::Finished(overage)]);
    fixture.policy.detached = true;

    let mut crashed = begin(fixture.adapter(Some(ModelCrashPoint::AfterOutcome)))
        .await
        .unwrap();
    assert!(drain(&mut crashed).await.is_err());
    assert_eq!(fixture.fake.dispatches(), 1);

    fixture.scheduler.reconcile_startup().unwrap();
    let future_spend = |id| ReservationRequest {
        id: ReservationId::new(id),
        run_id: fixture.security.config.run_id(),
        principal_id: fixture.security.attempt.principal_id,
        attempt: Some(fixture.security.attempt),
        idempotency_key: format!("future-spend-{id}"),
        kind: AdmissionKind::Model,
        spend: Spend::new(1, 1, 1, 0, 0),
    };
    assert!(matches!(
        fixture.scheduler.reserve(&future_spend(u128::MAX)),
        Err(SchedulerError::BudgetBlocked)
    ));

    let reopened = DurableScheduler::open(fixture.database.path()).unwrap();
    assert!(matches!(
        reopened.reserve(&future_spend(u128::MAX - 1)),
        Err(SchedulerError::BudgetBlocked)
    ));
}

#[tokio::test]
async fn live_cancellation_aborts_a_provider_that_ignores_cancellation() {
    let mut fixture = Fixture::new([]);
    fixture.policy.detached = true;
    let adapter = DurableModelAdapter::new(
        HangingAdapter,
        test_support::open_sqlite_store(fixture.database.path()).unwrap(),
        fixture.scheduler.clone(),
        fixture.security.clone(),
        fixture.policy,
        UtcDateTime::parse("2026-07-22T12:00:00Z").unwrap(),
        TraceId::parse("model-cancellation").unwrap(),
    );
    let mut session = adapter
        .start_session(SessionConfig::new("attempt-session"))
        .await
        .unwrap();
    let controller = agentkit_core::CancellationController::new();
    let cancellation = TurnCancellation::new(controller.handle());
    let interrupt = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        controller.interrupt();
    });
    assert!(
        session
            .begin_turn(request(), Some(cancellation))
            .await
            .is_err()
    );
    interrupt.await.unwrap();
    assert_eq!(
        fixture
            .scheduler
            .totals(fixture.security.config.run_id())
            .unwrap()
            .committed,
        fixture.policy.reservation
    );
    let outcomes = fixture
        .events()
        .into_iter()
        .filter(|event| event.event.event_type.as_str() == "model_call.outcome")
        .collect::<Vec<_>>();
    assert_eq!(outcomes.len(), 1);
    let outcome: serde_json::Value = serde_json::from_slice(&outcomes[0].event.payload).unwrap();
    assert_eq!(outcome["status"], "outcome_unknown");
}

#[tokio::test]
async fn unsupported_generation_controls_fail_before_intent_or_reservation() {
    let mut fixture = Fixture::new([]);
    fixture.policy.detached = true;
    let mut session = fixture
        .adapter(None)
        .start_session(SessionConfig::new("attempt-session"))
        .await
        .unwrap();
    let mut unsupported = request();
    unsupported.generation.temperature = Some(0.5);
    assert!(session.begin_turn(unsupported, None).await.is_err());
    assert!(fixture.events().is_empty());
    assert_eq!(
        fixture
            .scheduler
            .totals(fixture.security.config.run_id())
            .unwrap(),
        kit::runtime::scheduler::reserve::BudgetTotals {
            committed: Spend::ZERO,
            reserved: Spend::ZERO,
        }
    );
}
