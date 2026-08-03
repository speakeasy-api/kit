use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use kit::test_support;
use kit::{
    api::auth::{
        contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot},
        local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
    },
    api::service::AttemptDriverClaim,
    capabilities::{
        broker::{
            BrokerError, BrokerInvocation, BrokerOutcome, BrokerRuntime, invoke as broker_invoke,
        },
        kernel::{
            grant::{ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, EffectClass},
            identity::{
                CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
                CapabilityVersion, Digest, DigestAlgorithm,
            },
            invoke::{
                ApprovalState, AuthorizedInvocation, CanonicalOutput, DispatchOutcome,
                InvocationCrashPoint, InvocationEnvelope, InvocationResult, InvocationStatus,
                InvokeError, RetrySafety,
            },
        },
        schema::{JSON_SCHEMA_2020_12, NormalizedSchema},
    },
    domain::{
        config::{
            BudgetLayer, CONFIG_SCHEMA_VERSION, ConcurrencyLayer, ConfigLayer, Executor, Grant,
            LayerStack, Provider, RetentionLayer, RunConfigContext, RunConfigSnapshot,
        },
        events::{TraceId, UtcDateTime},
        ids::{
            AttemptId, CommandId, EventId, PrincipalId, ProjectId, RunId, ToolCallId, WorkspaceId,
        },
        lifecycle::{AttemptOwnership, FencingToken},
    },
    runtime::scheduler::{budget::RunBudget, limits::Spend, reserve::BudgetLedger},
    store::sqlite::append::SqliteStore,
    store::sqlite::idempotency::IdempotencyKey,
};

const UID: u32 = 501;

struct TestDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("kit-cap-invoke-{}", EventId::generate().unwrap()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("store.sqlite3");
        Self { directory, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

struct Fixture {
    store: SqliteStore,
    _database: TestDatabase,
    authenticated: AuthenticatedPrincipal,
    config: RunConfigSnapshot,
    grants: CapabilityGrantSnapshot,
    capability: CapabilityIdentity,
    schema: Digest,
    normalized_schema: NormalizedSchema,
    discovered_schema: Digest,
    constraints: ArgumentConstraints,
    arguments: Vec<u8>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    invocation_id: ToolCallId,
    key: IdempotencyKey,
    budget: BudgetLedger,
    cancellation: Arc<AtomicBool>,
    fence: Arc<AtomicU64>,
    attempt: AttemptOwnership,
    claim: Option<AttemptDriverClaim>,
    command_id: CommandId,
    intent_event_id: EventId,
    outcome_event_id: EventId,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
}

impl Fixture {
    fn new() -> Self {
        let principal_id = PrincipalId::generate().unwrap();
        let project_id = ProjectId::generate().unwrap();
        let workspace_id = WorkspaceId::generate().unwrap();
        let authority = BTreeSet::from([Grant::WorkspaceRead]);
        let config = config(principal_id, project_id, authority.clone());
        let authenticated = LocalPeerAuthenticator::new(BTreeMap::from([(
            UID,
            GrantSnapshot::new(principal_id, project_id, authority),
        )]))
        .authenticate(&LocalPeerObservation::from_transport(UID, 42, UID))
        .unwrap();
        let capability = identity("read");
        let normalized_schema = NormalizedSchema::ingest(
            br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"}"#,
            JSON_SCHEMA_2020_12,
            b"read schema",
            DigestAlgorithm::Sha256,
        )
        .unwrap();
        let schema = normalized_schema.source().normalized_digest();
        let constraints = ArgumentConstraints::new([b"workspace=root".as_slice()]);
        let grants = CapabilityGrantSnapshot::new(
            &config,
            [CapabilityGrant::new(
                principal_id,
                project_id,
                workspace_id,
                capability.clone(),
                schema,
                EffectClass::WorkspaceRead,
                constraints.clone(),
            )],
            DigestAlgorithm::Sha256,
        );
        let database = TestDatabase::new();
        let mut store = test_support::open_sqlite_store(database.path()).unwrap();
        let attempt = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal_id,
            FencingToken::new(7),
        );
        let claim = store
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id: config.run_id(),
                attempt_id: attempt.attempt_id,
                principal_id,
                fence: attempt.fencing_token,
                lease_version: 1,
                expires_at_unix_micros: 0,
            })
            .unwrap();
        Self {
            store,
            _database: database,
            authenticated,
            config,
            grants,
            capability,
            schema,
            normalized_schema,
            discovered_schema: schema,
            constraints,
            arguments: br#"{"path":"README.md"}"#.to_vec(),
            workspace_id,
            project_id,
            invocation_id: ToolCallId::generate().unwrap(),
            key: IdempotencyKey::parse("invoke-key").unwrap(),
            budget: BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100)),
            cancellation: Arc::new(AtomicBool::new(false)),
            fence: Arc::new(AtomicU64::new(7)),
            attempt,
            claim: Some(claim),
            command_id: CommandId::generate().unwrap(),
            intent_event_id: EventId::generate().unwrap(),
            outcome_event_id: EventId::generate().unwrap(),
            occurred_at: UtcDateTime::parse("2026-07-22T12:00:00Z").unwrap(),
            trace_id: TraceId::parse("trace-cap-invoke").unwrap(),
        }
    }
}

fn call(
    fixture: &mut Fixture,
    retry_safety: RetrySafety,
    approval: ApprovalState,
    crash_at: Option<InvocationCrashPoint>,
    backend: &mut dyn FnMut(&AuthorizedInvocation) -> DispatchOutcome,
) -> Result<InvocationResult, BrokerError> {
    let Fixture {
        store,
        authenticated,
        config,
        grants,
        capability,
        schema,
        normalized_schema,
        discovered_schema,
        constraints,
        arguments,
        workspace_id,
        project_id,
        invocation_id,
        key,
        budget,
        cancellation,
        fence,
        attempt,
        claim,
        command_id,
        intent_event_id,
        outcome_event_id,
        occurred_at,
        trace_id,
        ..
    } = fixture;
    let envelope = InvocationEnvelope {
        authenticated,
        config,
        grants,
        delegation: None,
        extension: kit::capabilities::kernel::grant_ext::RequestExtension::default(),
        capability,
        discovered_schema_digest: *discovered_schema,
        bound_schema_digest: *schema,
        effect: EffectClass::WorkspaceRead,
        argument_constraints: constraints,
        arguments,
        workspace_id: *workspace_id,
        project_id: *project_id,
        invocation_id: *invocation_id,
        idempotency_key: key,
        reservation: Spend::new(3, 4, 0, 1, 0),
        retry_safety,
        approval,
        cancellation,
        attempt: *attempt,
        driver_claim: *claim,
        current_fence: fence,
        command_id: *command_id,
        intent_event_id: *intent_event_id,
        outcome_event_id: *outcome_event_id,
        occurred_at,
        trace_id,
    };
    let runtime = BrokerRuntime::new(store, budget, backend);
    let runtime = match crash_at {
        Some(point) => runtime.with_crash_at(point),
        None => runtime,
    };
    broker_invoke(
        BrokerInvocation::generic(envelope, normalized_schema),
        runtime,
    )
    .map(|outcome| match outcome {
        BrokerOutcome::Completed(result) => result.invocation,
        BrokerOutcome::AuthRequired(_) => unreachable!("direct invocation cannot require auth"),
    })
}

fn config(
    principal_id: PrincipalId,
    project_id: ProjectId,
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
            run_id: RunId::generate().unwrap(),
        },
        &authority,
    )
    .unwrap()
}

fn identity(name: &str) -> CapabilityIdentity {
    CapabilityIdentity::new(
        CapabilitySource::new("native").unwrap(),
        CapabilityNamespace::new("kit.workspace").unwrap(),
        CapabilityName::new(name).unwrap(),
        CapabilityVersion::new("1.0.0").unwrap(),
        Digest::of(DigestAlgorithm::Blake3, b"workspace implementation v1"),
    )
}

fn success(_: &AuthorizedInvocation) -> DispatchOutcome {
    DispatchOutcome::Succeeded(CanonicalOutput {
        media_type: "application/json".to_owned(),
        body: br#"{"bytes":12}"#.to_vec(),
        artifact_digests: Vec::new(),
    })
}

#[test]
fn intent_precedes_dispatch_and_structured_outcome_is_durable() {
    let mut fixture = Fixture::new();
    let mut dispatches = 0;
    let expected_capability = fixture.capability.clone();
    let expected_schema = fixture.schema;
    let expected_invocation = fixture.invocation_id;
    let expected_attempt = fixture.attempt;
    let mut backend = |authorized: &AuthorizedInvocation| {
        dispatches += 1;
        assert_eq!(authorized.capability(), &expected_capability);
        assert_eq!(authorized.schema_digest(), expected_schema);
        assert_eq!(authorized.effect(), EffectClass::WorkspaceRead);
        assert_eq!(authorized.arguments(), br#"{"path":"README.md"}"#);
        assert_eq!(authorized.invocation_id(), expected_invocation);
        assert_eq!(authorized.idempotency_key(), "invoke-key");
        assert_eq!(authorized.attempt(), expected_attempt);
        success(authorized)
    };
    let result = call(
        &mut fixture,
        RetrySafety::Idempotent,
        ApprovalState::NotRequired,
        None,
        &mut backend,
    )
    .unwrap();

    assert_eq!(dispatches, 1);
    assert_eq!(result.canonical.status, InvocationStatus::Succeeded);
    assert_eq!(result.presentation, None);
    assert!(!result.replayed);
    assert_eq!(fixture.budget.totals().reserved, Spend::ZERO);
    assert_eq!(fixture.budget.totals().committed, Spend::new(3, 4, 0, 1, 0));
    let events = fixture.store.events().unwrap();
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
    let intent: serde_json::Value = serde_json::from_slice(&events[0].event.payload).unwrap();
    assert_eq!(intent["arguments_digest"].as_str().unwrap().len(), 71);
    assert!(intent.get("arguments").is_none());
    let outcome: serde_json::Value = serde_json::from_slice(&events[2].event.payload).unwrap();
    assert_eq!(outcome["result"]["status"], "succeeded");
}

#[test]
fn terminal_idempotency_replays_without_dispatch_or_double_charge() {
    let mut fixture = Fixture::new();
    let first = call(
        &mut fixture,
        RetrySafety::Idempotent,
        ApprovalState::NotRequired,
        None,
        &mut success,
    )
    .unwrap();
    let mut dispatches = 0;
    let second = call(
        &mut fixture,
        RetrySafety::Idempotent,
        ApprovalState::NotRequired,
        None,
        &mut |_| {
            dispatches += 1;
            DispatchOutcome::Failed {
                code: "must-not-run".to_owned(),
            }
        },
    )
    .unwrap();

    assert_eq!(dispatches, 0);
    assert_eq!(second.canonical, first.canonical);
    assert!(second.replayed);
    assert_eq!(fixture.store.events().unwrap().len(), 3);
    assert_eq!(fixture.budget.totals().committed, Spend::new(3, 4, 0, 1, 0));
}

#[test]
fn explicit_unknown_completion_is_durable_charged_and_never_redispatched() {
    let mut fixture = Fixture::new();
    let first = call(
        &mut fixture,
        RetrySafety::NonIdempotent,
        ApprovalState::NotRequired,
        None,
        &mut |_| DispatchOutcome::OutcomeUnknown {
            code: "transport_timeout".to_owned(),
        },
    )
    .unwrap();
    assert_eq!(first.canonical.status, InvocationStatus::OutcomeUnknown);
    assert_eq!(first.canonical.code.as_deref(), Some("transport_timeout"));
    assert!(first.canonical.charged);
    assert_eq!(fixture.budget.totals().reserved, Spend::ZERO);

    let mut dispatches = 0;
    let replay = call(
        &mut fixture,
        RetrySafety::NonIdempotent,
        ApprovalState::NotRequired,
        None,
        &mut |_| {
            dispatches += 1;
            panic!("persisted unknown outcome was redispatched")
        },
    )
    .unwrap();
    assert_eq!(dispatches, 0);
    assert_eq!(replay.canonical, first.canonical);
    assert!(replay.replayed);
    assert_eq!(fixture.store.events().unwrap().len(), 3);
}

#[test]
fn approval_and_cancellation_interrupt_before_dispatch_and_release_budget() {
    for (approval, cancel, status) in [
        (
            ApprovalState::Pending,
            false,
            InvocationStatus::ApprovalRequired,
        ),
        (
            ApprovalState::Denied,
            false,
            InvocationStatus::ApprovalDenied,
        ),
        (ApprovalState::Approved, true, InvocationStatus::Cancelled),
    ] {
        let mut fixture = Fixture::new();
        fixture.cancellation.store(cancel, Ordering::Release);
        let mut dispatches = 0;
        let result = call(
            &mut fixture,
            RetrySafety::Idempotent,
            approval,
            None,
            &mut |_| {
                dispatches += 1;
                panic!("backend must not be called")
            },
        )
        .unwrap();
        assert_eq!(dispatches, 0);
        assert_eq!(result.canonical.status, status);
        assert!(!result.canonical.charged);
        assert_eq!(fixture.budget.totals().reserved, Spend::ZERO);
        assert_eq!(fixture.budget.totals().committed, Spend::ZERO);
        assert_eq!(fixture.store.events().unwrap().len(), 2);
    }
}

#[test]
fn stale_fence_never_completes_a_dispatched_effect_as_success() {
    let mut fixture = Fixture::new();
    let fence = Arc::clone(&fixture.fence);
    let mut backend = |authorized: &AuthorizedInvocation| {
        fence.store(8, Ordering::Release);
        success(authorized)
    };
    let result = call(
        &mut fixture,
        RetrySafety::Idempotent,
        ApprovalState::NotRequired,
        None,
        &mut backend,
    )
    .unwrap();
    assert_eq!(result.canonical.status, InvocationStatus::OutcomeUnknown);
    assert_eq!(
        result.canonical.code.as_deref(),
        Some("stale_fence_after_dispatch")
    );
    assert!(result.canonical.charged);

    let mut stale = Fixture::new();
    stale.fence.store(8, Ordering::Release);
    let error = call(
        &mut stale,
        RetrySafety::Idempotent,
        ApprovalState::NotRequired,
        None,
        &mut success,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        BrokerError::Invoke(InvokeError::StaleFence)
    ));
    assert!(stale.store.events().unwrap().is_empty());
}

#[test]
fn crash_points_never_invent_success_or_blindly_redispatch() {
    for point in [
        InvocationCrashPoint::BeforeIntent,
        InvocationCrashPoint::BetweenIntentAndDispatch,
        InvocationCrashPoint::AfterDispatch,
        InvocationCrashPoint::BeforeOutcome,
    ] {
        let mut fixture = Fixture::new();
        let mut dispatches = 0;
        let mut backend = |authorized: &AuthorizedInvocation| {
            dispatches += 1;
            success(authorized)
        };
        let error = call(
            &mut fixture,
            RetrySafety::NonIdempotent,
            ApprovalState::NotRequired,
            Some(point),
            &mut backend,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            BrokerError::Invoke(InvokeError::InjectedCrash(found)) if found == point
        ));
        assert_eq!(
            dispatches,
            usize::from(matches!(
                point,
                InvocationCrashPoint::AfterDispatch | InvocationCrashPoint::BeforeOutcome
            ))
        );
        let events = fixture.store.events().unwrap();
        if point == InvocationCrashPoint::BeforeIntent {
            assert!(events.is_empty());
            assert_eq!(fixture.budget.totals().reserved, Spend::ZERO);
        } else {
            assert_eq!(
                events.len(),
                if point == InvocationCrashPoint::BetweenIntentAndDispatch {
                    2
                } else {
                    3
                }
            );
            let outcome: serde_json::Value =
                serde_json::from_slice(&events.last().unwrap().event.payload).unwrap();
            assert_eq!(outcome["result"]["status"], "outcome_unknown");
        }
    }
}

#[test]
fn panic_after_intent_fences_non_idempotent_recovery_to_unknown() {
    let mut fixture = Fixture::new();
    let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = call(
            &mut fixture,
            RetrySafety::NonIdempotent,
            ApprovalState::NotRequired,
            None,
            &mut |_| panic!("simulated process loss during dispatch"),
        );
    }));
    assert!(first.is_err());
    assert_eq!(fixture.store.events().unwrap().len(), 2);

    let mut dispatches = 0;
    let recovered = call(
        &mut fixture,
        RetrySafety::NonIdempotent,
        ApprovalState::NotRequired,
        None,
        &mut |_| {
            dispatches += 1;
            DispatchOutcome::Failed {
                code: "must-not-run".to_owned(),
            }
        },
    )
    .unwrap();
    assert_eq!(dispatches, 0);
    assert_eq!(recovered.canonical.status, InvocationStatus::OutcomeUnknown);
    assert_eq!(
        recovered.canonical.code.as_deref(),
        Some("recovery_requires_reconciliation")
    );
    assert!(recovered.canonical.charged);
    assert_eq!(fixture.store.events().unwrap().len(), 3);
}

#[test]
fn schema_arguments_authority_and_budget_fail_before_dispatch() {
    let mut schema = Fixture::new();
    let different = Digest::of(DigestAlgorithm::Sha256, b"different");
    schema.discovered_schema = different;
    assert!(matches!(
        call(
            &mut schema,
            RetrySafety::Idempotent,
            ApprovalState::NotRequired,
            None,
            &mut success,
        ),
        Err(BrokerError::Invoke(InvokeError::SchemaBindingMismatch))
    ));

    let mut arguments = Fixture::new();
    arguments.arguments = b"not-json".to_vec();
    assert!(matches!(
        call(
            &mut arguments,
            RetrySafety::Idempotent,
            ApprovalState::NotRequired,
            None,
            &mut success,
        ),
        Err(BrokerError::Invoke(InvokeError::InvalidArguments))
    ));

    let mut authority = Fixture::new();
    authority.capability = identity("write");
    assert!(matches!(
        call(
            &mut authority,
            RetrySafety::Idempotent,
            ApprovalState::NotRequired,
            None,
            &mut success,
        ),
        Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(_)))
    ));

    let mut budget = Fixture::new();
    budget.budget = BudgetLedger::new(RunBudget::new(1, 1, 1, 1, 1));
    assert!(matches!(
        call(
            &mut budget,
            RetrySafety::Idempotent,
            ApprovalState::NotRequired,
            None,
            &mut success,
        ),
        Err(BrokerError::Invoke(InvokeError::Budget(_)))
    ));
    assert!(budget.store.events().unwrap().is_empty());
}

#[test]
fn missing_driver_claim_is_a_typed_denial() {
    let mut fixture = Fixture::new();
    fixture.claim = None;
    assert!(matches!(
        call(
            &mut fixture,
            RetrySafety::Idempotent,
            ApprovalState::NotRequired,
            None,
            &mut success,
        ),
        Err(BrokerError::Invoke(InvokeError::MissingDriverClaim))
    ));
    assert!(fixture.store.events().unwrap().is_empty());
}
