use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use kit::{
    api::service::AttemptDriverClaim,
    capabilities::{
        broker::{BrokerInvocation, BrokerRuntime, invoke as broker_invoke},
        kernel::{
            grant::{
                self, ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, EffectClass,
                GrantRequest,
            },
            identity::{CapabilityIdentity, Digest, DigestAlgorithm},
            invoke::{
                ApprovalState, AuthorizedInvocation, DispatchOutcome, InvocationEnvelope,
                RetrySafety,
            },
        },
        native::NativeCatalog,
        schema::NormalizedSchema,
    },
    domain::{
        events::{TraceId, UtcDateTime},
        ids::{AttemptId, CommandId, EventId, PrincipalId, ProjectId, ToolCallId, WorkspaceId},
        lifecycle::{AttemptOwnership, FencingToken},
    },
    runtime::scheduler::{budget::RunBudget, limits::Spend, reserve::BudgetLedger},
    store::sqlite::idempotency::IdempotencyKey,
    test_support,
};

#[test]
fn forty_eight_native_bypass_attempts_have_zero_effects() {
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let workspace = WorkspaceId::generate().unwrap();
    let (authenticated, _, config) = test_support::trusted_verification_context(principal, project);
    let effects = AtomicUsize::new(0);
    let mut attempts = 0;
    let directory = std::env::temp_dir().join(format!(
        "kit-native-bypass-{}",
        EventId::generate().unwrap()
    ));
    std::fs::create_dir(&directory).unwrap();
    let mut store = test_support::open_sqlite_store(directory.join("store.sqlite3")).unwrap();
    let budget = BudgetLedger::new(RunBudget::new(100, 100, 100, 100, 100));
    let cancellation = Arc::new(AtomicBool::new(false));
    let fence = Arc::new(AtomicU64::new(1));

    for descriptor in NativeCatalog::all() {
        let constraints = ArgumentConstraints::new([b"native-bound".as_slice()]);
        let valid = CapabilityGrant::new(
            principal,
            project,
            workspace,
            descriptor.identity().clone(),
            descriptor.schema().normalized_digest(),
            descriptor.effect(),
            constraints.clone(),
        );
        for attack in 0..8 {
            attempts += 1;
            let selected_authenticated = authenticated.clone();
            let mut selected_project = project;
            let mut selected_workspace = workspace;
            let mut capability: CapabilityIdentity = descriptor.identity().clone();
            let mut schema = descriptor.schema().normalized_digest();
            let mut effect = descriptor.effect();
            let mut requested_constraints = constraints.clone();
            let mut grants =
                CapabilityGrantSnapshot::new(&config, [valid.clone()], DigestAlgorithm::Sha256);
            match attack {
                0 => {
                    selected_project = ProjectId::generate().unwrap();
                }
                1 => selected_workspace = WorkspaceId::generate().unwrap(),
                2 => schema = Digest::of(DigestAlgorithm::Sha256, b"forged-schema"),
                3 => {
                    capability = NativeCatalog::all()
                        .iter()
                        .find(|other| other.tool() != descriptor.tool())
                        .unwrap()
                        .identity()
                        .clone();
                }
                4 => {
                    effect = if descriptor.effect() == EffectClass::WorkspaceRead {
                        EffectClass::WorkspaceWrite
                    } else {
                        EffectClass::WorkspaceRead
                    };
                }
                5 => grants = CapabilityGrantSnapshot::new(&config, [], DigestAlgorithm::Sha256),
                6 => requested_constraints = ArgumentConstraints::default(),
                7 => {
                    grants = CapabilityGrantSnapshot::new(
                        &config,
                        [CapabilityGrant::new(
                            PrincipalId::generate().unwrap(),
                            project,
                            workspace,
                            descriptor.identity().clone(),
                            descriptor.schema().normalized_digest(),
                            descriptor.effect(),
                            constraints.clone(),
                        )],
                        DigestAlgorithm::Sha256,
                    )
                }
                _ => unreachable!(),
            }
            assert!(
                !grant::decide(GrantRequest {
                    authenticated: &selected_authenticated,
                    capability: &capability,
                    schema_digest: schema,
                    effect,
                    argument_constraints: &requested_constraints,
                    workspace_id: selected_workspace,
                    project_id: selected_project,
                    config: &config,
                    grants: &grants,
                    delegation: None,
                    extension: kit::capabilities::kernel::grant_ext::RequestExtension::default(),
                })
                .is_allowed()
            );
            let owner = AttemptOwnership::new(
                AttemptId::generate().unwrap(),
                selected_authenticated.principal_id(),
                FencingToken::new(1),
            );
            let claim = store
                .install_driver_claim_for_test(AttemptDriverClaim {
                    run_id: config.run_id(),
                    attempt_id: owner.attempt_id,
                    principal_id: owner.principal_id,
                    fence: owner.fencing_token,
                    lease_version: u64::try_from(attempts).unwrap(),
                    expires_at_unix_micros: 0,
                })
                .unwrap();
            let invocation_id = ToolCallId::generate().unwrap();
            let key = IdempotencyKey::parse(&format!("native-bypass-{attempts}")).unwrap();
            let occurred_at = UtcDateTime::parse("2026-07-26T00:00:00Z").unwrap();
            let trace_id = TraceId::parse(&format!("native-bypass-{attempts}")).unwrap();
            let mut dispatch = |_: &AuthorizedInvocation| {
                effects.fetch_add(1, Ordering::SeqCst);
                DispatchOutcome::Failed {
                    code: "bypass_reached_dispatch".to_owned(),
                }
            };
            let normalized_schema = NormalizedSchema::ingest(
                descriptor.schema().source_bytes(),
                descriptor.schema().dialect(),
                descriptor.schema().documentation(),
                DigestAlgorithm::Sha256,
            )
            .unwrap();
            let request = BrokerInvocation::generic(
                InvocationEnvelope {
                    authenticated: &selected_authenticated,
                    config: &config,
                    grants: &grants,
                    delegation: None,
                    extension: kit::capabilities::kernel::grant_ext::RequestExtension::default(),
                    capability: &capability,
                    discovered_schema_digest: schema,
                    bound_schema_digest: schema,
                    effect,
                    argument_constraints: &requested_constraints,
                    arguments: b"{}",
                    workspace_id: selected_workspace,
                    project_id: selected_project,
                    invocation_id,
                    idempotency_key: &key,
                    reservation: Spend::new(0, 0, 0, 1, 0),
                    retry_safety: RetrySafety::Idempotent,
                    approval: ApprovalState::NotRequired,
                    cancellation: &cancellation,
                    attempt: owner,
                    driver_claim: Some(claim),
                    current_fence: &fence,
                    command_id: CommandId::generate().unwrap(),
                    intent_event_id: EventId::generate().unwrap(),
                    outcome_event_id: EventId::generate().unwrap(),
                    occurred_at: &occurred_at,
                    trace_id: &trace_id,
                },
                &normalized_schema,
            );
            let result = broker_invoke(
                request,
                BrokerRuntime::new(&mut store, &budget, &mut dispatch),
            );
            assert!(
                result.is_err(),
                "forged attempt {attempts} reached invocation"
            );
        }
    }
    assert_eq!(attempts, 48);
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn release_surface_cannot_name_native_dispatch_authority() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/capabilities/native/mod.rs"),
    )
    .unwrap();
    assert!(source.contains("pub(crate) mod dispatch"));
    assert!(!source.contains("pub mod dispatch;"));
}
