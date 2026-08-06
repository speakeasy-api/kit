use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64},
    },
};

use kit::{
    agent::accounting::SchedulerDebit,
    api::auth::{
        contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot},
        local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
    },
    api::service::AttemptDriverClaim,
    capabilities::{
        broker::{
            AuthResolution, BrokerAuthRequirement, BrokerError, BrokerInvocation, BrokerOutcome,
            BrokerResult, BrokerRuntime, invoke, resolve_auth,
        },
        kernel::{
            grant::{
                ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, DelegationSnapshot,
                EffectClass, GrantDecision, GrantRequest, decide,
            },
            grant_ext::{GrantExtension, RequestExtension},
            identity::{
                CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
                CapabilityVersion, Digest, DigestAlgorithm,
            },
            invoke::{
                ApprovalState, AuthorizedInvocation, CanonicalOutput, DispatchOutcome,
                InvocationEnvelope, RetrySafety,
            },
        },
        native::NativeCatalog,
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
        secret::SecretHandle,
    },
    runtime::scheduler::{
        budget::RunBudget,
        limits::Spend,
        reserve::{BudgetLedger, ReservationId, ReservationStatus},
    },
    store::sqlite::{append::StoredEvent, idempotency::IdempotencyKey},
    test_support,
};

const UID: u32 = 501;
const SCHEMA: &[u8] = br#"{
    "$schema":"https://json-schema.org/draft/2020-12/schema",
    "type":"object",
    "properties":{"path":{"type":"string","minLength":1}},
    "required":["path"],
    "additionalProperties":false
}"#;
const NATIVE_VALID_ARGUMENTS: &[u8] = br#"{"expected_revision":"r:0000000000000000000000000000000000000000000000000000000000000000","path":"README.md","range":{"kind":"full"}}"#;
const NATIVE_INVALID_ARGUMENTS: &[u8] = br#"{"expected_revision":"r:0000000000000000000000000000000000000000000000000000000000000000","path":"","range":{"kind":"full"}}"#;

struct TestDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("kit-broker-{}", EventId::generate().unwrap()));
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

#[derive(Clone)]
struct Inputs {
    authenticated: AuthenticatedPrincipal,
    config: RunConfigSnapshot,
    grants: CapabilityGrantSnapshot,
    capability: CapabilityIdentity,
    schema_digest: Digest,
    effect: EffectClass,
    reservation: Spend,
    retry_safety: RetrySafety,
    approval: ApprovalState,
    constraints: ArgumentConstraints,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    invocation_id: ToolCallId,
    key: IdempotencyKey,
    attempt: AttemptOwnership,
    command_id: CommandId,
    intent_event_id: EventId,
    outcome_event_id: EventId,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
    auth_credential: SecretHandle,
    claim: AttemptDriverClaim,
}

#[derive(Clone, Copy)]
enum TestPath {
    Direct,
    Generic,
    External,
    Nested,
}

struct RunOutcome {
    result: Result<BrokerResult, BrokerError>,
    events: Vec<StoredEvent>,
    committed: Spend,
    reserved: Spend,
    dispatches: usize,
}

impl Inputs {
    fn new() -> (Self, NormalizedSchema) {
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
        let schema = NormalizedSchema::ingest(
            SCHEMA,
            JSON_SCHEMA_2020_12,
            b"path arguments",
            DigestAlgorithm::Sha256,
        )
        .unwrap();
        let schema_digest = schema.source().normalized_digest();
        let capability = identity();
        let constraints = ArgumentConstraints::new([b"workspace=root".as_slice()]);
        let auth_credential = SecretHandle::parse("local-keychain:item-7").unwrap();
        let extension = GrantExtension::new([], [auth_credential.clone()], 1).unwrap();
        let grants = CapabilityGrantSnapshot::new(
            &config,
            [CapabilityGrant::new(
                principal_id,
                project_id,
                workspace_id,
                capability.clone(),
                schema_digest,
                EffectClass::WorkspaceRead,
                constraints.clone(),
            )
            .with_extension(extension)],
            DigestAlgorithm::Sha256,
        );
        let attempt = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal_id,
            FencingToken::new(7),
        );
        let claim = AttemptDriverClaim {
            run_id: config.run_id(),
            attempt_id: attempt.attempt_id,
            principal_id,
            fence: attempt.fencing_token,
            lease_version: 1,
            expires_at_unix_micros: 0,
        };
        (
            Self {
                authenticated,
                config,
                grants,
                capability,
                schema_digest,
                effect: EffectClass::WorkspaceRead,
                reservation: Spend::new(3, 4, 0, 1, 0),
                retry_safety: RetrySafety::Idempotent,
                approval: ApprovalState::NotRequired,
                constraints,
                workspace_id,
                project_id,
                invocation_id: ToolCallId::generate().unwrap(),
                key: IdempotencyKey::parse("broker-key").unwrap(),
                attempt,
                command_id: CommandId::generate().unwrap(),
                intent_event_id: EventId::generate().unwrap(),
                outcome_event_id: EventId::generate().unwrap(),
                occurred_at: UtcDateTime::parse("2026-07-31T12:00:00Z").unwrap(),
                trace_id: TraceId::parse("trace-broker-paths").unwrap(),
                auth_credential,
                claim,
            },
            schema,
        )
    }

    fn native() -> (Self, NormalizedSchema) {
        let (mut inputs, _) = Self::new();
        let descriptor = NativeCatalog::by_canonical_name("kit.read").unwrap();
        let schema = descriptor.normalized_schema().clone();
        inputs.capability = descriptor.identity().clone();
        inputs.schema_digest = descriptor.schema().normalized_digest();
        inputs.effect = descriptor.effect();
        inputs.reservation = descriptor.reservation();
        inputs.retry_safety = descriptor.retry_safety();
        inputs.approval = descriptor.approval();
        inputs.grants = CapabilityGrantSnapshot::new(
            &inputs.config,
            [CapabilityGrant::new(
                inputs.authenticated.principal_id(),
                inputs.project_id,
                inputs.workspace_id,
                inputs.capability.clone(),
                inputs.schema_digest,
                inputs.effect,
                inputs.constraints.clone(),
            )],
            DigestAlgorithm::Sha256,
        );
        (inputs, schema)
    }

    fn policy(&self, delegation: Option<&DelegationSnapshot>) -> GrantDecision {
        decide(GrantRequest {
            authenticated: &self.authenticated,
            capability: &self.capability,
            schema_digest: self.schema_digest,
            effect: self.effect,
            argument_constraints: &self.constraints,
            workspace_id: self.workspace_id,
            project_id: self.project_id,
            config: &self.config,
            grants: &self.grants,
            delegation,
            extension: RequestExtension::default(),
        })
    }
}

fn run(
    inputs: &Inputs,
    path: TestPath,
    arguments: &[u8],
    schema: &NormalizedSchema,
    delegation: Option<&DelegationSnapshot>,
) -> RunOutcome {
    run_with_preexisting(inputs, path, arguments, schema, delegation, None)
}

fn run_with_preexisting(
    inputs: &Inputs,
    path: TestPath,
    arguments: &[u8],
    schema: &NormalizedSchema,
    delegation: Option<&DelegationSnapshot>,
    preexisting: Option<ReservationId>,
) -> RunOutcome {
    let database = TestDatabase::new();
    let mut store = test_support::open_sqlite_store(database.path()).unwrap();
    install_claim(&mut store, inputs);
    let budget = BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100));
    let reservation = inputs.reservation;
    if let Some(id) = preexisting {
        budget.reserve(id, reservation).unwrap();
        budget.commit(id).unwrap();
    }
    let cancellation = Arc::new(AtomicBool::new(false));
    let fence = Arc::new(AtomicU64::new(7));
    let envelope = InvocationEnvelope {
        authenticated: &inputs.authenticated,
        config: &inputs.config,
        grants: &inputs.grants,
        delegation,
        extension: RequestExtension::default(),
        capability: &inputs.capability,
        discovered_schema_digest: inputs.schema_digest,
        bound_schema_digest: inputs.schema_digest,
        effect: inputs.effect,
        argument_constraints: &inputs.constraints,
        arguments,
        workspace_id: inputs.workspace_id,
        project_id: inputs.project_id,
        invocation_id: inputs.invocation_id,
        idempotency_key: &inputs.key,
        reservation,
        retry_safety: inputs.retry_safety,
        approval: inputs.approval,
        cancellation: &cancellation,
        attempt: inputs.attempt,
        driver_claim: Some(inputs.claim),
        current_fence: &fence,
        command_id: inputs.command_id,
        intent_event_id: inputs.intent_event_id,
        outcome_event_id: inputs.outcome_event_id,
        occurred_at: &inputs.occurred_at,
        trace_id: &inputs.trace_id,
        learning: None,
    };
    let request = match path {
        TestPath::Direct => BrokerInvocation::native(envelope),
        TestPath::Generic => Ok(BrokerInvocation::generic(envelope, schema)),
        TestPath::External => Ok(BrokerInvocation::external(envelope, schema)),
        TestPath::Nested => Ok(BrokerInvocation::nested(
            envelope,
            schema,
            delegation.expect("nested test path requires delegation"),
        )),
    };
    let mut dispatches = 0;
    let mut backend = |authorized: &AuthorizedInvocation| {
        dispatches += 1;
        success(authorized)
    };
    let result = request
        .and_then(|request| {
            invoke(
                request,
                BrokerRuntime::new(&mut store, &budget, &mut backend),
            )
        })
        .map(|outcome| match outcome {
            BrokerOutcome::Completed(result) => result,
            BrokerOutcome::AuthRequired(_) => panic!("test invocation has no auth requirement"),
        });
    let totals = budget.totals();
    RunOutcome {
        result,
        events: store.events().unwrap(),
        committed: totals.committed,
        reserved: totals.reserved,
        dispatches,
    }
}

#[test]
fn broker_accounting_uses_the_invocations_exact_reservation() {
    let (inputs, schema) = Inputs::native();
    let decoy = ReservationId::new(0);
    let outcome = run_with_preexisting(
        &inputs,
        TestPath::Direct,
        NATIVE_VALID_ARGUMENTS,
        &schema,
        None,
        Some(decoy),
    );
    let result = outcome.result.unwrap();
    let intent: serde_json::Value =
        serde_json::from_slice(&outcome.events[0].event.payload).unwrap();

    assert_ne!(result.invocation.reservation.id(), decoy);
    assert_eq!(
        result.invocation.reservation.id().get().to_string(),
        intent["reservation_id"]
    );
    assert_eq!(result.invocation.reservation.spend(), inputs.reservation);
    assert_eq!(
        result.invocation.reservation.status(),
        ReservationStatus::Debited
    );
    assert_eq!(
        result.accounting.reservation_debit,
        SchedulerDebit {
            cost_microusd: 0,
            tokens: 0,
            turns: 0,
            tools: 1,
            processes: 0,
        }
    );
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

fn identity() -> CapabilityIdentity {
    CapabilityIdentity::new(
        CapabilitySource::new("native").unwrap(),
        CapabilityNamespace::new("fixture.files").unwrap(),
        CapabilityName::new("read").unwrap(),
        CapabilityVersion::new("1.0.0").unwrap(),
        Digest::of(DigestAlgorithm::Blake3, b"broker fixture implementation"),
    )
}

fn success(_: &AuthorizedInvocation) -> DispatchOutcome {
    DispatchOutcome::Succeeded(CanonicalOutput {
        media_type: "application/json".to_owned(),
        body: br#"{"bytes":12}"#.to_vec(),
        artifact_digests: Vec::new(),
    })
}

fn install_claim(store: &mut kit::store::sqlite::append::SqliteStore, inputs: &Inputs) {
    store.install_driver_claim_for_test(inputs.claim).unwrap();
}

fn auth_request<'a>(
    inputs: &'a Inputs,
    schema: &'a NormalizedSchema,
    arguments: &'a [u8],
    cancellation: &'a Arc<AtomicBool>,
    fence: &'a Arc<AtomicU64>,
) -> BrokerInvocation<'a> {
    auth_request_with(
        inputs,
        schema,
        arguments,
        cancellation,
        fence,
        &inputs.auth_credential,
        &inputs.auth_credential,
        Spend::new(3, 4, 0, 1, 0),
        ApprovalState::NotRequired,
    )
}

#[allow(clippy::too_many_arguments)]
fn auth_request_with<'a>(
    inputs: &'a Inputs,
    schema: &'a NormalizedSchema,
    arguments: &'a [u8],
    cancellation: &'a Arc<AtomicBool>,
    fence: &'a Arc<AtomicU64>,
    request_credential: &SecretHandle,
    required_credential: &SecretHandle,
    reservation: Spend,
    approval: ApprovalState,
) -> BrokerInvocation<'a> {
    BrokerInvocation::external(
        InvocationEnvelope {
            authenticated: &inputs.authenticated,
            config: &inputs.config,
            grants: &inputs.grants,
            delegation: None,
            extension: RequestExtension::new(None, Some(request_credential.clone())),
            capability: &inputs.capability,
            discovered_schema_digest: inputs.schema_digest,
            bound_schema_digest: inputs.schema_digest,
            effect: EffectClass::WorkspaceRead,
            argument_constraints: &inputs.constraints,
            arguments,
            workspace_id: inputs.workspace_id,
            project_id: inputs.project_id,
            invocation_id: inputs.invocation_id,
            idempotency_key: &inputs.key,
            reservation,
            retry_safety: RetrySafety::Idempotent,
            approval,
            cancellation,
            attempt: inputs.attempt,
            driver_claim: Some(inputs.claim),
            current_fence: fence,
            command_id: inputs.command_id,
            intent_event_id: inputs.intent_event_id,
            outcome_event_id: inputs.outcome_event_id,
            occurred_at: &inputs.occurred_at,
            trace_id: &inputs.trace_id,
            learning: None,
        },
        schema,
    )
    .with_auth_requirement(
        BrokerAuthRequirement::new("workspace.read:path")
            .unwrap()
            .with_credential_id(required_credential.clone()),
    )
}

#[test]
fn broker_paths_direct_and_generic_are_field_wise_equal() {
    let (inputs, schema) = Inputs::native();
    let direct_policy = inputs.policy(None);
    let generic_policy = inputs.policy(None);
    let direct = run(
        &inputs,
        TestPath::Direct,
        NATIVE_VALID_ARGUMENTS,
        &schema,
        None,
    );
    let generic = run(
        &inputs,
        TestPath::Generic,
        NATIVE_VALID_ARGUMENTS,
        &schema,
        None,
    );
    let direct_result = direct.result.unwrap();
    let generic_result = generic.result.unwrap();

    assert_eq!(direct_policy, generic_policy);
    assert!(direct_policy.is_allowed());
    assert_eq!(direct_result.invocation, generic_result.invocation);
    assert_eq!(
        direct_result.invocation.reservation.spend(),
        inputs.reservation
    );
    assert_eq!(
        direct_result.invocation.reservation.status(),
        ReservationStatus::Debited
    );
    assert_eq!(direct_result.accounting, generic_result.accounting);
    assert_eq!(direct.events, generic.events);
    assert!(
        direct
            .events
            .iter()
            .all(|event| event.event.trace_id == inputs.trace_id)
    );
    assert_eq!(direct.committed, generic.committed);
    assert_eq!(direct.committed, inputs.reservation);
    assert_eq!(direct.reserved, Spend::ZERO);
    assert_eq!(direct.dispatches, 1);
    assert_eq!(
        direct_result.accounting.reservation_debit,
        SchedulerDebit {
            cost_microusd: 0,
            tokens: 0,
            turns: 0,
            tools: 1,
            processes: 0,
        }
    );
    assert_eq!(
        direct_result.accounting.categories.tool.billed_calls,
        Some(1)
    );
}

#[test]
fn broker_paths_direct_and_generic_schema_denials_have_zero_effects() {
    let (inputs, schema) = Inputs::native();
    let direct = run(
        &inputs,
        TestPath::Direct,
        NATIVE_INVALID_ARGUMENTS,
        &schema,
        None,
    );
    let generic = run(
        &inputs,
        TestPath::Generic,
        NATIVE_INVALID_ARGUMENTS,
        &schema,
        None,
    );

    assert!(matches!(direct.result, Err(BrokerError::InvalidArguments)));
    assert!(matches!(generic.result, Err(BrokerError::InvalidArguments)));
    for outcome in [direct, generic] {
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.committed, Spend::ZERO);
        assert_eq!(outcome.reserved, Spend::ZERO);
        assert_eq!(outcome.dispatches, 0);
    }
}

#[test]
fn broker_native_rejects_unknown_and_non_exact_identities() {
    let (inputs, schema) = Inputs::native();
    let exact = inputs.capability.clone();
    let identities = [
        CapabilityIdentity::new(
            CapabilitySource::new("native").unwrap(),
            CapabilityNamespace::new("kit.native").unwrap(),
            CapabilityName::new("unknown").unwrap(),
            CapabilityVersion::new("1.0.0").unwrap(),
            exact.implementation_digest(),
        ),
        CapabilityIdentity::new(
            CapabilitySource::new("native").unwrap(),
            CapabilityNamespace::new("kit.native").unwrap(),
            CapabilityName::new("read").unwrap(),
            CapabilityVersion::new("1.0.1").unwrap(),
            exact.implementation_digest(),
        ),
    ];

    for capability in identities {
        let mut non_exact = inputs.clone();
        non_exact.capability = capability;
        let outcome = run(
            &non_exact,
            TestPath::Direct,
            NATIVE_VALID_ARGUMENTS,
            &schema,
            None,
        );
        assert!(matches!(
            outcome.result,
            Err(BrokerError::NativeCapabilityBinding)
        ));
        assert!(outcome.events.is_empty());
        assert_eq!(outcome.committed, Spend::ZERO);
        assert_eq!(outcome.reserved, Spend::ZERO);
        assert_eq!(outcome.dispatches, 0);
    }
}

#[test]
fn broker_paths_cloned_schema_retains_validation() {
    let (inputs, schema) = Inputs::new();
    let schema = schema.clone();

    assert!(
        run(
            &inputs,
            TestPath::External,
            br#"{"path":"README.md"}"#,
            &schema,
            None,
        )
        .result
        .is_ok()
    );
    assert!(matches!(
        run(
            &inputs,
            TestPath::External,
            br#"{"path":""}"#,
            &schema,
            None,
        )
        .result,
        Err(BrokerError::InvalidArguments)
    ));
}

#[test]
fn broker_paths_external_validates_pinned_schema_before_effects() {
    let (inputs, schema) = Inputs::new();
    let valid = run(
        &inputs,
        TestPath::External,
        br#"{"path":"README.md"}"#,
        &schema,
        None,
    );
    assert!(valid.result.is_ok());
    assert_eq!(valid.events.len(), 3);

    let invalid = run(
        &inputs,
        TestPath::External,
        br#"{"path":""}"#,
        &schema,
        None,
    );
    assert!(matches!(invalid.result, Err(BrokerError::InvalidArguments)));
    assert!(invalid.events.is_empty());
    assert_eq!(invalid.committed, Spend::ZERO);
    assert_eq!(invalid.reserved, Spend::ZERO);
    assert_eq!(invalid.dispatches, 0);

    let different = NormalizedSchema::ingest(
        br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"array"}"#,
        JSON_SCHEMA_2020_12,
        b"different fixture",
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    let mismatched = run(
        &inputs,
        TestPath::External,
        br#"{"path":"README.md"}"#,
        &different,
        None,
    );
    assert!(matches!(
        mismatched.result,
        Err(BrokerError::SchemaBindingMismatch)
    ));
    assert!(mismatched.events.is_empty());
    assert_eq!(mismatched.dispatches, 0);

    let unsupported = NormalizedSchema::ingest(
        br#"{"type":"object"}"#,
        "https://json-schema.org/draft/2019-09/schema",
        b"unsupported fixture",
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    let mut unsupported_inputs = inputs.clone();
    unsupported_inputs.schema_digest = unsupported.source().normalized_digest();
    unsupported_inputs.grants = CapabilityGrantSnapshot::new(
        &unsupported_inputs.config,
        [CapabilityGrant::new(
            unsupported_inputs.authenticated.principal_id(),
            unsupported_inputs.project_id,
            unsupported_inputs.workspace_id,
            unsupported_inputs.capability.clone(),
            unsupported_inputs.schema_digest,
            unsupported_inputs.effect,
            unsupported_inputs.constraints.clone(),
        )],
        DigestAlgorithm::Sha256,
    );
    let unsupported = run(
        &unsupported_inputs,
        TestPath::External,
        br#"{"path":"README.md"}"#,
        &unsupported,
        None,
    );
    assert!(matches!(
        unsupported.result,
        Err(BrokerError::UnsupportedValidation)
    ));
    assert!(unsupported.events.is_empty());
    assert_eq!(unsupported.dispatches, 0);
}

#[test]
fn broker_paths_nested_requires_and_uses_real_delegation() {
    let (inputs, schema) = Inputs::new();
    let root = PrincipalId::generate().unwrap();
    let delegation = DelegationSnapshot::new(
        vec![root, inputs.authenticated.principal_id()],
        1,
        inputs.grants.clone(),
    )
    .unwrap();
    assert!(inputs.policy(Some(&delegation)).is_allowed());
    let nested = run(
        &inputs,
        TestPath::Nested,
        br#"{"path":"README.md"}"#,
        &schema,
        Some(&delegation),
    );
    assert!(nested.result.is_ok());
    assert_eq!(nested.events.len(), 3);
    assert_eq!(nested.dispatches, 1);
}

#[test]
fn broker_rejects_invalid_auth_inputs_before_challenge() {
    let (inputs, schema) = Inputs::new();
    let database = TestDatabase::new();
    let cancellation = Arc::new(AtomicBool::new(false));
    let fence = Arc::new(AtomicU64::new(7));
    let budget = BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100));
    let mut store = test_support::open_sqlite_store(database.path()).unwrap();
    install_claim(&mut store, &inputs);
    let mut backend = |_: &AuthorizedInvocation| success_unchecked();
    let mismatched = SecretHandle::parse("local-keychain:different").unwrap();

    assert!(matches!(
        invoke(
            auth_request_with(
                &inputs,
                &schema,
                br#"{"path":"README.md"}"#,
                &cancellation,
                &fence,
                &inputs.auth_credential,
                &mismatched,
                Spend::new(3, 4, 0, 1, 0),
                ApprovalState::NotRequired,
            ),
            BrokerRuntime::new(&mut store, &budget, &mut backend),
        ),
        Err(BrokerError::AuthCredentialMismatch)
    ));
    assert!(matches!(
        invoke(
            auth_request_with(
                &inputs,
                &schema,
                br#"{"path":"README.md"}"#,
                &cancellation,
                &fence,
                &inputs.auth_credential,
                &inputs.auth_credential,
                Spend::ZERO,
                ApprovalState::NotRequired,
            ),
            BrokerRuntime::new(&mut store, &budget, &mut backend),
        ),
        Err(BrokerError::ToolReservationRequired)
    ));
    let mut wrong_owner = inputs.clone();
    wrong_owner.attempt = AttemptOwnership::new(
        wrong_owner.attempt.attempt_id,
        PrincipalId::generate().unwrap(),
        wrong_owner.attempt.fencing_token,
    );
    assert!(matches!(
        invoke(
            auth_request(
                &wrong_owner,
                &schema,
                br#"{"path":"README.md"}"#,
                &cancellation,
                &fence,
            ),
            BrokerRuntime::new(&mut store, &budget, &mut backend),
        ),
        Err(BrokerError::Invoke(
            kit::capabilities::kernel::invoke::InvokeError::StaleFence
        ))
    ));
    assert!(store.events().unwrap().is_empty());
}

#[test]
fn cancelled_broker_invocation_never_creates_or_resolves_auth() {
    let (inputs, schema) = Inputs::new();
    let database = TestDatabase::new();
    let cancellation = Arc::new(AtomicBool::new(true));
    let fence = Arc::new(AtomicU64::new(7));
    let budget = BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100));
    let mut store = test_support::open_sqlite_store(database.path()).unwrap();
    install_claim(&mut store, &inputs);
    let mut backend = |_: &AuthorizedInvocation| panic!("cancelled invocation dispatched");

    let outcome = invoke(
        auth_request(
            &inputs,
            &schema,
            br#"{"path":"README.md"}"#,
            &cancellation,
            &fence,
        ),
        BrokerRuntime::new(&mut store, &budget, &mut backend),
    )
    .unwrap();
    let BrokerOutcome::Completed(result) = outcome else {
        panic!("cancelled invocation created an auth challenge");
    };
    assert_eq!(
        result.invocation.canonical.status,
        kit::capabilities::kernel::invoke::InvocationStatus::Cancelled
    );
    assert!(store.events().unwrap().iter().all(|event| {
        !event
            .event
            .event_type
            .as_str()
            .starts_with("capability.broker_auth")
    }));
    assert!(matches!(
        resolve_auth(
            &auth_request(
                &inputs,
                &schema,
                br#"{"path":"README.md"}"#,
                &cancellation,
                &fence,
            ),
            &inputs.authenticated,
            AuthResolution::Granted,
            &mut store,
        ),
        Err(BrokerError::AuthResolutionCancelled)
    ));
}

#[test]
fn pending_and_denied_approval_skip_broker_auth() {
    for (approval, expected) in [
        (
            ApprovalState::Pending,
            kit::capabilities::kernel::invoke::InvocationStatus::ApprovalRequired,
        ),
        (
            ApprovalState::Denied,
            kit::capabilities::kernel::invoke::InvocationStatus::ApprovalDenied,
        ),
    ] {
        let (inputs, schema) = Inputs::new();
        let database = TestDatabase::new();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let budget = BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100));
        let mut store = test_support::open_sqlite_store(database.path()).unwrap();
        install_claim(&mut store, &inputs);
        let mut backend = |_: &AuthorizedInvocation| panic!("approval interruption dispatched");
        let request = auth_request_with(
            &inputs,
            &schema,
            br#"{"path":"README.md"}"#,
            &cancellation,
            &fence,
            &inputs.auth_credential,
            &inputs.auth_credential,
            Spend::new(3, 4, 0, 1, 0),
            approval,
        );

        let BrokerOutcome::Completed(result) = invoke(
            request,
            BrokerRuntime::new(&mut store, &budget, &mut backend),
        )
        .unwrap() else {
            panic!("approval interruption created auth challenge");
        };
        assert_eq!(result.invocation.canonical.status, expected);
        assert!(store.events().unwrap().iter().all(|event| {
            !event
                .event
                .event_type
                .as_str()
                .starts_with("capability.broker_auth")
        }));
    }
}

#[test]
fn broker_auth_ids_are_scoped_beyond_invocation_id() {
    let (first_inputs, first_schema) = Inputs::new();
    let (mut second_inputs, second_schema) = Inputs::new();
    second_inputs.invocation_id = first_inputs.invocation_id;
    let cancellation = Arc::new(AtomicBool::new(false));
    let fence = Arc::new(AtomicU64::new(7));
    let budget = BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100));
    let first_database = TestDatabase::new();
    let second_database = TestDatabase::new();
    let mut first_store = test_support::open_sqlite_store(first_database.path()).unwrap();
    let mut second_store = test_support::open_sqlite_store(second_database.path()).unwrap();
    install_claim(&mut first_store, &first_inputs);
    install_claim(&mut second_store, &second_inputs);
    let mut backend = |_: &AuthorizedInvocation| success_unchecked();

    let first = invoke(
        auth_request(
            &first_inputs,
            &first_schema,
            br#"{"path":"README.md"}"#,
            &cancellation,
            &fence,
        ),
        BrokerRuntime::new(&mut first_store, &budget, &mut backend),
    )
    .unwrap();
    let second = invoke(
        auth_request(
            &second_inputs,
            &second_schema,
            br#"{"path":"README.md"}"#,
            &cancellation,
            &fence,
        ),
        BrokerRuntime::new(&mut second_store, &budget, &mut backend),
    )
    .unwrap();
    let BrokerOutcome::AuthRequired(first) = first else {
        panic!("first auth challenge missing");
    };
    let BrokerOutcome::AuthRequired(second) = second else {
        panic!("second auth challenge missing");
    };

    assert_ne!(first.challenge_id, second.challenge_id);
    assert_ne!(
        first_store.events().unwrap()[0].event.id,
        second_store.events().unwrap()[0].event.id
    );
}

#[test]
fn broker_auth_is_bound_durable_and_precedes_kernel_effects() {
    let (inputs, schema) = Inputs::new();
    let database = TestDatabase::new();
    let cancellation = Arc::new(AtomicBool::new(false));
    let fence = Arc::new(AtomicU64::new(7));
    let arguments = br#"{"path":"README.md"}"#;
    let budget = BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100));
    let dispatches = Cell::new(0);

    let mut store = test_support::open_sqlite_store(database.path()).unwrap();
    install_claim(&mut store, &inputs);
    let mut backend = |_: &AuthorizedInvocation| {
        dispatches.set(dispatches.get() + 1);
        success_unchecked()
    };
    let first = invoke(
        auth_request(&inputs, &schema, arguments, &cancellation, &fence),
        BrokerRuntime::new(&mut store, &budget, &mut backend),
    )
    .unwrap();
    let challenge = match first {
        BrokerOutcome::AuthRequired(challenge) => challenge,
        BrokerOutcome::Completed(_) => panic!("auth challenge was bypassed"),
    };
    assert_eq!(challenge.scope, "workspace.read:path");
    assert_eq!(
        challenge
            .credential_id
            .as_ref()
            .map(SecretHandle::identifier),
        Some("local-keychain:item-7")
    );
    assert_eq!(dispatches.get(), 0);
    assert_eq!(budget.totals().reserved, Spend::ZERO);
    assert_eq!(budget.totals().committed, Spend::ZERO);
    let events = store.events().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].event.event_type.as_str(),
        "capability.broker_auth_challenged"
    );
    let payload: serde_json::Value = serde_json::from_slice(&events[0].event.payload).unwrap();
    assert_eq!(
        payload["principal_id"],
        inputs.authenticated.principal_id().to_string()
    );
    assert_eq!(payload["project_id"], inputs.project_id.to_string());
    assert_eq!(payload["invocation_id"], inputs.invocation_id.to_string());
    assert_eq!(payload["schema_digest"], inputs.schema_digest.to_string());
    assert_eq!(payload["trace_id"], inputs.trace_id.as_str());
    assert!(
        payload["decision_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert!(
        payload["request_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    drop(store);

    let mut store = test_support::open_sqlite_store(database.path()).unwrap();
    let wrong = authenticated_for(UID + 1, PrincipalId::generate().unwrap(), inputs.project_id);
    assert!(matches!(
        resolve_auth(
            &auth_request(&inputs, &schema, arguments, &cancellation, &fence),
            &wrong,
            AuthResolution::Granted,
            &mut store,
        ),
        Err(BrokerError::AuthPrincipalMismatch)
    ));
    assert!(matches!(
        resolve_auth(
            &auth_request(
                &inputs,
                &schema,
                br#"{"path":"different.md"}"#,
                &cancellation,
                &fence,
            ),
            &inputs.authenticated,
            AuthResolution::Granted,
            &mut store,
        ),
        Err(BrokerError::InvalidAuthState)
    ));
    let mut changed_grants = inputs.clone();
    changed_grants.grants = CapabilityGrantSnapshot::new(
        &changed_grants.config,
        [CapabilityGrant::new(
            changed_grants.authenticated.principal_id(),
            changed_grants.project_id,
            changed_grants.workspace_id,
            changed_grants.capability.clone(),
            changed_grants.schema_digest,
            EffectClass::WorkspaceRead,
            changed_grants.constraints.clone(),
        )
        .with_extension(
            GrantExtension::new([], [changed_grants.auth_credential.clone()], 1).unwrap(),
        )],
        DigestAlgorithm::Blake3,
    );
    assert!(matches!(
        resolve_auth(
            &auth_request(&changed_grants, &schema, arguments, &cancellation, &fence,),
            &changed_grants.authenticated,
            AuthResolution::Granted,
            &mut store,
        ),
        Err(BrokerError::InvalidAuthState)
    ));
    let changed_schema = NormalizedSchema::ingest(
        br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#,
        JSON_SCHEMA_2020_12,
        b"changed path arguments",
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    let mut changed_schema_inputs = inputs.clone();
    changed_schema_inputs.schema_digest = changed_schema.source().normalized_digest();
    changed_schema_inputs.grants = CapabilityGrantSnapshot::new(
        &changed_schema_inputs.config,
        [CapabilityGrant::new(
            changed_schema_inputs.authenticated.principal_id(),
            changed_schema_inputs.project_id,
            changed_schema_inputs.workspace_id,
            changed_schema_inputs.capability.clone(),
            changed_schema_inputs.schema_digest,
            EffectClass::WorkspaceRead,
            changed_schema_inputs.constraints.clone(),
        )
        .with_extension(
            GrantExtension::new([], [changed_schema_inputs.auth_credential.clone()], 1).unwrap(),
        )],
        DigestAlgorithm::Sha256,
    );
    assert!(matches!(
        resolve_auth(
            &auth_request(
                &changed_schema_inputs,
                &changed_schema,
                arguments,
                &cancellation,
                &fence,
            ),
            &changed_schema_inputs.authenticated,
            AuthResolution::Granted,
            &mut store,
        ),
        Err(BrokerError::InvalidAuthState)
    ));
    store.quiesce_driver_claim(inputs.claim).unwrap();
    resolve_auth(
        &auth_request(&inputs, &schema, arguments, &cancellation, &fence),
        &inputs.authenticated,
        AuthResolution::Granted,
        &mut store,
    )
    .unwrap();
    resolve_auth(
        &auth_request(&inputs, &schema, arguments, &cancellation, &fence),
        &inputs.authenticated,
        AuthResolution::Granted,
        &mut store,
    )
    .unwrap();
    assert!(matches!(
        resolve_auth(
            &auth_request(&inputs, &schema, arguments, &cancellation, &fence),
            &inputs.authenticated,
            AuthResolution::Denied,
            &mut store,
        ),
        Err(BrokerError::AuthStore(
            kit::store::sqlite::append::StoreError::IdempotencyConflict(_)
        ))
    ));
    assert_eq!(store.events().unwrap().len(), 2);
    drop(store);

    let mut store = test_support::open_sqlite_store(database.path()).unwrap();
    install_claim(&mut store, &inputs);
    let completed = invoke(
        auth_request(&inputs, &schema, arguments, &cancellation, &fence),
        BrokerRuntime::new(&mut store, &budget, &mut backend),
    )
    .unwrap();
    let completed = match completed {
        BrokerOutcome::Completed(result) => result,
        BrokerOutcome::AuthRequired(_) => panic!("durable grant was lost"),
    };
    assert_eq!(
        completed.invocation.canonical.output.unwrap().body,
        br#"{"bytes":12}"#
    );
    assert_eq!(dispatches.get(), 1);
    assert_eq!(budget.totals().committed, Spend::new(3, 4, 0, 1, 0));
    assert_eq!(budget.totals().reserved, Spend::ZERO);
    let event_types = store
        .events()
        .unwrap()
        .into_iter()
        .map(|event| event.event.event_type.as_str().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        event_types,
        [
            "capability.broker_auth_challenged",
            "capability.broker_auth_resolved",
            "capability.invocation_intent",
            "capability.invocation_dispatched",
            "capability.invocation_outcome",
        ]
    );
}

#[test]
fn broker_auth_denial_and_corruption_fail_closed() {
    let (inputs, schema) = Inputs::new();
    let cancellation = Arc::new(AtomicBool::new(false));
    let fence = Arc::new(AtomicU64::new(7));
    let arguments = br#"{"path":"README.md"}"#;

    let mut grant_denied = inputs.clone();
    grant_denied.grants =
        CapabilityGrantSnapshot::new(&grant_denied.config, [], DigestAlgorithm::Sha256);
    let grant_denied_db = TestDatabase::new();
    let mut store = test_support::open_sqlite_store(grant_denied_db.path()).unwrap();
    install_claim(&mut store, &grant_denied);
    let budget = BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100));
    let mut backend = |_: &AuthorizedInvocation| success_unchecked();
    assert!(matches!(
        invoke(
            auth_request(&grant_denied, &schema, arguments, &cancellation, &fence,),
            BrokerRuntime::new(&mut store, &budget, &mut backend),
        ),
        Err(BrokerError::Invoke(
            kit::capabilities::kernel::invoke::InvokeError::AuthorizationDenied(_)
        ))
    ));
    assert!(store.events().unwrap().is_empty());

    let denied_db = TestDatabase::new();
    let mut store = test_support::open_sqlite_store(denied_db.path()).unwrap();
    install_claim(&mut store, &inputs);
    assert!(matches!(
        invoke(
            auth_request(&inputs, &schema, arguments, &cancellation, &fence),
            BrokerRuntime::new(&mut store, &budget, &mut backend),
        ),
        Ok(BrokerOutcome::AuthRequired(_))
    ));
    resolve_auth(
        &auth_request(&inputs, &schema, arguments, &cancellation, &fence),
        &inputs.authenticated,
        AuthResolution::Denied,
        &mut store,
    )
    .unwrap();
    assert!(matches!(
        invoke(
            auth_request(&inputs, &schema, arguments, &cancellation, &fence),
            BrokerRuntime::new(&mut store, &budget, &mut backend),
        ),
        Err(BrokerError::AuthDenied)
    ));
    assert_eq!(budget.totals().committed, Spend::ZERO);

    let corrupt_db = TestDatabase::new();
    let mut store = test_support::open_sqlite_store(corrupt_db.path()).unwrap();
    install_claim(&mut store, &inputs);
    assert!(matches!(
        invoke(
            auth_request(&inputs, &schema, arguments, &cancellation, &fence),
            BrokerRuntime::new(&mut store, &budget, &mut backend),
        ),
        Ok(BrokerOutcome::AuthRequired(_))
    ));
    drop(store);
    let connection = rusqlite::Connection::open(corrupt_db.path()).unwrap();
    connection
        .execute(
            "UPDATE idempotency SET response = ?1 WHERE command_name = ?2",
            rusqlite::params![
                b"{\"schema_version\":2}".as_slice(),
                "capability.broker_auth.challenge"
            ],
        )
        .unwrap();
    drop(connection);
    let mut store = test_support::open_sqlite_store(corrupt_db.path()).unwrap();
    assert!(matches!(
        invoke(
            auth_request(&inputs, &schema, arguments, &cancellation, &fence),
            BrokerRuntime::new(&mut store, &budget, &mut backend),
        ),
        Err(BrokerError::InvalidAuthState)
    ));
}

fn authenticated_for(
    uid: u32,
    principal: PrincipalId,
    project: ProjectId,
) -> AuthenticatedPrincipal {
    LocalPeerAuthenticator::new(BTreeMap::from([(
        uid,
        GrantSnapshot::new(principal, project, [Grant::WorkspaceRead]),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(uid, 42, uid))
    .unwrap()
}

fn success_unchecked() -> DispatchOutcome {
    DispatchOutcome::Succeeded(CanonicalOutput {
        media_type: "application/json".to_owned(),
        body: br#"{"bytes":12}"#.to_vec(),
        artifact_digests: Vec::new(),
    })
}
