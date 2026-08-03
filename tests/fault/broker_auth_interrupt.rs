use std::{
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
            AuthResolution, BrokerAuthRequirement, BrokerInvocation, BrokerOutcome, BrokerRuntime,
            invoke, resolve_auth,
        },
        kernel::{
            grant::{ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, EffectClass},
            grant_ext::{GrantExtension, RequestExtension},
            identity::{
                CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
                CapabilityVersion, Digest, DigestAlgorithm,
            },
            invoke::{
                ApprovalState, AuthorizedInvocation, CanonicalInvocationResult, CanonicalOutput,
                DispatchOutcome, InvocationEnvelope, InvocationStatus, RetrySafety,
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
        secret::SecretHandle,
    },
    runtime::scheduler::{budget::RunBudget, limits::Spend, reserve::BudgetLedger},
    store::sqlite::idempotency::IdempotencyKey,
    test_support,
};

const UID: u32 = 501;
const ARGUMENTS: &[u8] = br#"{"path":"README.md"}"#;
const SCHEMA: &[u8] = br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#;

struct TestDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new(iteration: usize) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "kit-broker-auth-fault-{iteration}-{}",
            EventId::generate().unwrap()
        ));
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

struct Inputs {
    authenticated: AuthenticatedPrincipal,
    config: RunConfigSnapshot,
    grants: CapabilityGrantSnapshot,
    capability: CapabilityIdentity,
    schema: Digest,
    normalized_schema: NormalizedSchema,
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

impl Inputs {
    fn new(iteration: usize) -> Self {
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
        let capability = CapabilityIdentity::new(
            CapabilitySource::new("external").unwrap(),
            CapabilityNamespace::new("fixture.files").unwrap(),
            CapabilityName::new("read").unwrap(),
            CapabilityVersion::new("1.0.0").unwrap(),
            Digest::of(DigestAlgorithm::Blake3, b"fault broker implementation"),
        );
        let normalized_schema = NormalizedSchema::ingest(
            SCHEMA,
            JSON_SCHEMA_2020_12,
            b"fault broker schema",
            DigestAlgorithm::Sha256,
        )
        .unwrap();
        let schema = normalized_schema.source().normalized_digest();
        let constraints = ArgumentConstraints::new([b"workspace=root".as_slice()]);
        let auth_credential = SecretHandle::parse("fault-keychain:item").unwrap();
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
            )
            .with_extension(GrantExtension::new([], [auth_credential.clone()], 0).unwrap())],
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
        Self {
            authenticated,
            config,
            grants,
            capability,
            schema,
            normalized_schema,
            constraints,
            workspace_id,
            project_id,
            invocation_id: ToolCallId::generate().unwrap(),
            key: IdempotencyKey::parse(&format!("broker-fault-{iteration}")).unwrap(),
            attempt,
            command_id: CommandId::generate().unwrap(),
            intent_event_id: EventId::generate().unwrap(),
            outcome_event_id: EventId::generate().unwrap(),
            occurred_at: UtcDateTime::parse("2026-07-31T12:00:00Z").unwrap(),
            trace_id: TraceId::parse(&format!("trace-broker-auth-fault-{iteration}")).unwrap(),
            auth_credential,
            claim,
        }
    }

    fn request<'a>(
        &'a self,
        cancellation: &'a Arc<AtomicBool>,
        fence: &'a Arc<AtomicU64>,
    ) -> BrokerInvocation<'a> {
        BrokerInvocation::generic(
            InvocationEnvelope {
                authenticated: &self.authenticated,
                config: &self.config,
                grants: &self.grants,
                delegation: None,
                extension: RequestExtension::new(None, Some(self.auth_credential.clone())),
                capability: &self.capability,
                discovered_schema_digest: self.schema,
                bound_schema_digest: self.schema,
                effect: EffectClass::WorkspaceRead,
                argument_constraints: &self.constraints,
                arguments: ARGUMENTS,
                workspace_id: self.workspace_id,
                project_id: self.project_id,
                invocation_id: self.invocation_id,
                idempotency_key: &self.key,
                reservation: Spend::new(3, 4, 0, 1, 0),
                retry_safety: RetrySafety::Idempotent,
                approval: ApprovalState::NotRequired,
                cancellation,
                attempt: self.attempt,
                driver_claim: Some(self.claim),
                current_fence: fence,
                command_id: self.command_id,
                intent_event_id: self.intent_event_id,
                outcome_event_id: self.outcome_event_id,
                occurred_at: &self.occurred_at,
                trace_id: &self.trace_id,
            },
            &self.normalized_schema,
        )
        .with_auth_requirement(
            BrokerAuthRequirement::new("workspace.read:path")
                .unwrap()
                .with_credential_id(self.auth_credential.clone()),
        )
    }
}

#[test]
fn broker_auth_interrupt_is_durable_for_exactly_100_restarts() {
    for iteration in 0..100 {
        let database = TestDatabase::new(iteration);
        let inputs = Inputs::new(iteration);

        let mut store = test_support::open_sqlite_store(database.path()).unwrap();
        store.install_driver_claim_for_test(inputs.claim).unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let budget = BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100));
        let mut backend = |_: &AuthorizedInvocation| panic!("challenge phase dispatched");
        assert!(matches!(
            invoke(
                inputs.request(&cancellation, &fence),
                BrokerRuntime::new(&mut store, &budget, &mut backend),
            ),
            Ok(BrokerOutcome::AuthRequired(_))
        ));
        assert_eq!(budget.totals().reserved, Spend::ZERO);
        assert_eq!(budget.totals().committed, Spend::ZERO);
        let events = store.events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event.event_type.as_str(),
            "capability.broker_auth_challenged"
        );
        drop(store);

        let mut store = test_support::open_sqlite_store(database.path()).unwrap();
        store.install_driver_claim_for_test(inputs.claim).unwrap();
        store.quiesce_driver_claim(inputs.claim).unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        resolve_auth(
            &inputs.request(&cancellation, &fence),
            &inputs.authenticated,
            AuthResolution::Granted,
            &mut store,
        )
        .unwrap();
        assert_eq!(store.events().unwrap().len(), 2);
        drop(store);

        let mut store = test_support::open_sqlite_store(database.path()).unwrap();
        store.install_driver_claim_for_test(inputs.claim).unwrap();
        let cancellation = Arc::new(AtomicBool::new(false));
        let fence = Arc::new(AtomicU64::new(7));
        let budget = BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100));
        let mut dispatches = 0;
        let mut backend = |_: &AuthorizedInvocation| {
            dispatches += 1;
            DispatchOutcome::Succeeded(CanonicalOutput {
                media_type: "application/json".to_owned(),
                body: br#"{"bytes":12}"#.to_vec(),
                artifact_digests: Vec::new(),
            })
        };
        let result = invoke(
            inputs.request(&cancellation, &fence),
            BrokerRuntime::new(&mut store, &budget, &mut backend),
        )
        .unwrap();
        let result = match result {
            BrokerOutcome::Completed(result) => result,
            BrokerOutcome::AuthRequired(_) => panic!("granted auth did not resume"),
        };
        assert_eq!(
            result.invocation.canonical,
            CanonicalInvocationResult {
                status: InvocationStatus::Succeeded,
                output: Some(CanonicalOutput {
                    media_type: "application/json".to_owned(),
                    body: br#"{"bytes":12}"#.to_vec(),
                    artifact_digests: Vec::new(),
                }),
                code: None,
                charged: true,
            }
        );
        assert_eq!(dispatches, 1);
        assert_eq!(budget.totals().reserved, Spend::ZERO);
        assert_eq!(budget.totals().committed, Spend::new(3, 4, 0, 1, 0));
        assert_eq!(
            result.accounting.reservation_debit,
            SchedulerDebit {
                cost_microusd: 3,
                tokens: 4,
                turns: 0,
                tools: 1,
                processes: 0,
            }
        );
        assert_eq!(
            store
                .events()
                .unwrap()
                .into_iter()
                .map(|event| event.event.event_type.as_str().to_owned())
                .collect::<Vec<_>>(),
            [
                "capability.broker_auth_challenged",
                "capability.broker_auth_resolved",
                "capability.invocation_intent",
                "capability.invocation_dispatched",
                "capability.invocation_outcome",
            ]
        );
    }
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
