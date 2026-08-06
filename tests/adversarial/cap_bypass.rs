use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64},
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
        broker::{BrokerInvocation, BrokerOutcome, BrokerRuntime, invoke as broker_invoke},
        kernel::{
            grant::{ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, EffectClass},
            identity::{
                CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
                CapabilityVersion, Digest, DigestAlgorithm,
            },
            invoke::{
                ApprovalState, AuthorizedInvocation, CanonicalOutput, DispatchOutcome,
                InvocationEnvelope, InvocationStatus, RetrySafety,
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
    store::sqlite::{append::SqliteStore, idempotency::IdempotencyKey},
};

const UID: u32 = 712;

#[derive(Clone, Copy, Debug)]
enum BypassRoute {
    DiscoverySchemaDrift,
    EmptyArguments,
    TextArguments,
    TrailingArguments,
    InvalidUtf8Arguments,
    StaleFenceLow,
    StaleFenceHigh,
    ForgedSource,
    ForgedNamespace,
    ForgedName,
    ForgedVersion,
    ForgedImplementation,
    UngrantedSchema,
    ModelEffect,
    WriteEffect,
    ProcessEffect,
    NetworkEffect,
    MissingArgumentConstraint,
    ForeignWorkspace,
    ForeignProject,
    ForeignPrincipalSnapshot,
    ForeignConfigSnapshot,
    EmptyGrantSnapshot,
    ExhaustedBudget,
    ApprovalPending,
    ApprovalDenied,
    PreCancelled,
}

struct Fixture {
    store: SqliteStore,
    directory: PathBuf,
    authenticated: AuthenticatedPrincipal,
    config: RunConfigSnapshot,
    grants: CapabilityGrantSnapshot,
    capability: CapabilityIdentity,
    discovered_schema: Digest,
    bound_schema: Digest,
    normalized_schema: NormalizedSchema,
    effect: EffectClass,
    constraints: ArgumentConstraints,
    arguments: Vec<u8>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    budget: BudgetLedger,
    approval: ApprovalState,
    cancellation: Arc<AtomicBool>,
    fence: Arc<AtomicU64>,
    attempt: AttemptOwnership,
    claim: AttemptDriverClaim,
    invocation_id: ToolCallId,
    key: IdempotencyKey,
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
        let authenticated = authenticate(principal_id, project_id, authority);
        let capability = identity(
            "native",
            "kit.workspace",
            "read",
            "1.0.0",
            b"implementation-v1",
        );
        let normalized_schema = NormalizedSchema::ingest(
            br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"}"#,
            JSON_SCHEMA_2020_12,
            b"bypass schema",
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
        let directory =
            std::env::temp_dir().join(format!("kit-cap-bypass-{}", EventId::generate().unwrap()));
        std::fs::create_dir(&directory).unwrap();
        let mut store = test_support::open_sqlite_store(directory.join("store.sqlite3")).unwrap();
        let attempt = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal_id,
            FencingToken::new(11),
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
            directory,
            authenticated,
            config,
            grants,
            capability,
            discovered_schema: schema,
            bound_schema: schema,
            normalized_schema,
            effect: EffectClass::WorkspaceRead,
            constraints,
            arguments: br#"{"path":"README.md"}"#.to_vec(),
            workspace_id,
            project_id,
            budget: BudgetLedger::new(RunBudget::new(10, 10, 10, 10, 10)),
            approval: ApprovalState::NotRequired,
            cancellation: Arc::new(AtomicBool::new(false)),
            fence: Arc::new(AtomicU64::new(11)),
            attempt,
            claim,
            invocation_id: ToolCallId::generate().unwrap(),
            key: IdempotencyKey::parse("bypass-key").unwrap(),
            command_id: CommandId::generate().unwrap(),
            intent_event_id: EventId::generate().unwrap(),
            outcome_event_id: EventId::generate().unwrap(),
            occurred_at: UtcDateTime::parse("2026-07-22T12:00:00Z").unwrap(),
            trace_id: TraceId::parse("trace-cap-bypass").unwrap(),
        }
    }

    fn apply(&mut self, route: BypassRoute) {
        match route {
            BypassRoute::DiscoverySchemaDrift => {
                self.discovered_schema = Digest::of(DigestAlgorithm::Sha256, b"drift")
            }
            BypassRoute::EmptyArguments => self.arguments.clear(),
            BypassRoute::TextArguments => self.arguments = b"not-json".to_vec(),
            BypassRoute::TrailingArguments => self.arguments = b"{}{}".to_vec(),
            BypassRoute::InvalidUtf8Arguments => self.arguments = vec![0xff],
            BypassRoute::StaleFenceLow => self.fence = Arc::new(AtomicU64::new(10)),
            BypassRoute::StaleFenceHigh => self.fence = Arc::new(AtomicU64::new(12)),
            BypassRoute::ForgedSource => {
                self.capability = identity(
                    "extension",
                    "kit.workspace",
                    "read",
                    "1.0.0",
                    b"implementation-v1",
                )
            }
            BypassRoute::ForgedNamespace => {
                self.capability = identity(
                    "native",
                    "kit.process",
                    "read",
                    "1.0.0",
                    b"implementation-v1",
                )
            }
            BypassRoute::ForgedName => {
                self.capability = identity(
                    "native",
                    "kit.workspace",
                    "write",
                    "1.0.0",
                    b"implementation-v1",
                )
            }
            BypassRoute::ForgedVersion => {
                self.capability = identity(
                    "native",
                    "kit.workspace",
                    "read",
                    "2.0.0",
                    b"implementation-v1",
                )
            }
            BypassRoute::ForgedImplementation => {
                self.capability = identity(
                    "native",
                    "kit.workspace",
                    "read",
                    "1.0.0",
                    b"implementation-v2",
                )
            }
            BypassRoute::UngrantedSchema => {
                let schema = Digest::of(DigestAlgorithm::Sha256, b"schema-v2");
                self.discovered_schema = schema;
                self.bound_schema = schema;
            }
            BypassRoute::ModelEffect => self.effect = EffectClass::ModelCall,
            BypassRoute::WriteEffect => self.effect = EffectClass::WorkspaceWrite,
            BypassRoute::ProcessEffect => self.effect = EffectClass::ProcessSpawn,
            BypassRoute::NetworkEffect => self.effect = EffectClass::NetworkEgress,
            BypassRoute::MissingArgumentConstraint => {
                self.constraints = ArgumentConstraints::default()
            }
            BypassRoute::ForeignWorkspace => self.workspace_id = WorkspaceId::generate().unwrap(),
            BypassRoute::ForeignProject => self.project_id = ProjectId::generate().unwrap(),
            BypassRoute::ForeignPrincipalSnapshot => {
                self.authenticated = authenticate(
                    PrincipalId::generate().unwrap(),
                    self.project_id,
                    BTreeSet::from([Grant::WorkspaceRead]),
                )
            }
            BypassRoute::ForeignConfigSnapshot => {
                self.config = config(
                    self.authenticated.principal_id(),
                    ProjectId::generate().unwrap(),
                    BTreeSet::from([Grant::WorkspaceRead]),
                )
            }
            BypassRoute::EmptyGrantSnapshot => {
                self.grants =
                    CapabilityGrantSnapshot::new(&self.config, [], DigestAlgorithm::Sha256)
            }
            BypassRoute::ExhaustedBudget => {
                self.budget = BudgetLedger::new(RunBudget::new(1, 1, 1, 1, 1))
            }
            BypassRoute::ApprovalPending => self.approval = ApprovalState::Pending,
            BypassRoute::ApprovalDenied => self.approval = ApprovalState::Denied,
            BypassRoute::PreCancelled => self.cancellation = Arc::new(AtomicBool::new(true)),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn run(
    fixture: &mut Fixture,
    backend: &mut dyn FnMut(&AuthorizedInvocation) -> DispatchOutcome,
) -> Result<InvocationStatus, String> {
    let Fixture {
        store,
        authenticated,
        config,
        grants,
        capability,
        discovered_schema,
        bound_schema,
        normalized_schema,
        effect,
        constraints,
        arguments,
        workspace_id,
        project_id,
        budget,
        approval,
        cancellation,
        fence,
        attempt,
        claim,
        invocation_id,
        key,
        command_id,
        intent_event_id,
        outcome_event_id,
        occurred_at,
        trace_id,
        ..
    } = fixture;
    let request = BrokerInvocation::generic(
        InvocationEnvelope {
            authenticated,
            config,
            grants,
            delegation: None,
            extension: kit::capabilities::kernel::grant_ext::RequestExtension::default(),
            capability,
            discovered_schema_digest: *discovered_schema,
            bound_schema_digest: *bound_schema,
            effect: *effect,
            argument_constraints: constraints,
            arguments,
            workspace_id: *workspace_id,
            project_id: *project_id,
            invocation_id: *invocation_id,
            idempotency_key: key,
            reservation: Spend::new(2, 2, 0, 2, 0),
            retry_safety: RetrySafety::NonIdempotent,
            approval: *approval,
            cancellation,
            attempt: *attempt,
            driver_claim: Some(*claim),
            current_fence: fence,
            command_id: *command_id,
            intent_event_id: *intent_event_id,
            outcome_event_id: *outcome_event_id,
            occurred_at,
            trace_id,
            learning: None,
        },
        normalized_schema,
    );
    broker_invoke(request, BrokerRuntime::new(store, budget, backend))
        .map(|outcome| match outcome {
            BrokerOutcome::Completed(result) => result.invocation.canonical.status,
            BrokerOutcome::AuthRequired(_) => unreachable!("direct invocation cannot require auth"),
        })
        .map_err(|error| error.to_string())
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
    .authenticate(&LocalPeerObservation::from_transport(UID, 99, UID))
    .unwrap()
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
                max_tokens: Some(10),
                max_cost_microusd: Some(10),
                max_turns: Some(10),
            },
            concurrency: ConcurrencyLayer {
                max_runs: Some(1),
                max_tools: Some(1),
            },
            retention: RetentionLayer {
                event_days: Some(1),
                artifact_days: Some(1),
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

fn identity(
    source: &str,
    namespace: &str,
    name: &str,
    version: &str,
    implementation: &[u8],
) -> CapabilityIdentity {
    CapabilityIdentity::new(
        CapabilitySource::new(source).unwrap(),
        CapabilityNamespace::new(namespace).unwrap(),
        CapabilityName::new(name).unwrap(),
        CapabilityVersion::new(version).unwrap(),
        Digest::of(DigestAlgorithm::Blake3, implementation),
    )
}

#[test]
fn at_least_twenty_bypass_routes_cannot_reach_dispatch() {
    let routes = [
        BypassRoute::DiscoverySchemaDrift,
        BypassRoute::EmptyArguments,
        BypassRoute::TextArguments,
        BypassRoute::TrailingArguments,
        BypassRoute::InvalidUtf8Arguments,
        BypassRoute::StaleFenceLow,
        BypassRoute::StaleFenceHigh,
        BypassRoute::ForgedSource,
        BypassRoute::ForgedNamespace,
        BypassRoute::ForgedName,
        BypassRoute::ForgedVersion,
        BypassRoute::ForgedImplementation,
        BypassRoute::UngrantedSchema,
        BypassRoute::ModelEffect,
        BypassRoute::WriteEffect,
        BypassRoute::ProcessEffect,
        BypassRoute::NetworkEffect,
        BypassRoute::MissingArgumentConstraint,
        BypassRoute::ForeignWorkspace,
        BypassRoute::ForeignProject,
        BypassRoute::ForeignPrincipalSnapshot,
        BypassRoute::ForeignConfigSnapshot,
        BypassRoute::EmptyGrantSnapshot,
        BypassRoute::ExhaustedBudget,
        BypassRoute::ApprovalPending,
        BypassRoute::ApprovalDenied,
        BypassRoute::PreCancelled,
    ];
    assert!(routes.len() >= 20);

    for route in routes {
        let mut fixture = Fixture::new();
        fixture.apply(route);
        let mut dispatches = 0;
        let result = run(&mut fixture, &mut |_| {
            dispatches += 1;
            DispatchOutcome::Succeeded(CanonicalOutput {
                media_type: "application/json".to_owned(),
                body: b"{}".to_vec(),
                artifact_digests: Vec::new(),
            })
        });
        assert_eq!(dispatches, 0, "bypass route reached dispatcher: {route:?}");
        assert!(
            !matches!(result, Ok(InvocationStatus::Succeeded)),
            "bypass route succeeded: {route:?}"
        );
    }
}

#[test]
fn dispatcher_and_authorized_token_construction_remain_private() {
    let source = include_str!("../../src/capabilities/kernel/invoke.rs");
    let broker = include_str!("../../src/capabilities/broker/mod.rs");
    assert!(!source.contains("pub struct Dispatcher"));
    assert!(!source.contains("pub fn dispatch"));
    assert!(!source.contains("pub capability: CapabilityIdentity"));
    assert!(!source.contains("pub arguments: Vec<u8>"));
    assert_eq!(source.matches("pub(crate) fn invoke(").count(), 1);
    assert!(source.contains("pub(crate) struct InvocationRuntime"));
    assert!(broker.contains("pub fn native("));
    assert!(!broker.contains("fn direct("));
}
