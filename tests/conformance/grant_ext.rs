use std::collections::{BTreeMap, BTreeSet};

use kit::{
    api::auth::{
        contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot},
        local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
    },
    capabilities::kernel::{
        grant::{
            ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, DelegationSnapshot,
            EffectClass, GrantReasonCode, GrantRequest, decide,
        },
        grant_ext::{EgressConstraint, GrantExtension, RequestExtension},
        identity::{
            CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
            CapabilityVersion, Digest, DigestAlgorithm,
        },
    },
    domain::{
        config::{
            BudgetLayer, CONFIG_SCHEMA_VERSION, ConcurrencyLayer, ConfigLayer, Executor, Grant,
            LayerStack, Provider, RetentionLayer, RunConfigContext, RunConfigSnapshot,
        },
        ids::{PrincipalId, ProjectId, RunId, WorkspaceId},
        secret::SecretHandle,
    },
};

fn make_config(
    principal: PrincipalId,
    project: ProjectId,
    authority: BTreeSet<Grant>,
) -> RunConfigSnapshot {
    LayerStack {
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
            principal_id: principal,
            project_id: project,
            run_id: RunId::generate().unwrap(),
        },
        &authority,
    )
    .unwrap()
}

fn authenticate(
    principal: PrincipalId,
    project: ProjectId,
    authority: BTreeSet<Grant>,
) -> AuthenticatedPrincipal {
    LocalPeerAuthenticator::new(BTreeMap::from([(
        501,
        GrantSnapshot::new(principal, project, authority),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(501, 42, 501))
    .unwrap()
}

fn capability() -> CapabilityIdentity {
    CapabilityIdentity::new(
        CapabilitySource::new("fixture").unwrap(),
        CapabilityNamespace::new("kit.fixture").unwrap(),
        CapabilityName::new("fetch").unwrap(),
        CapabilityVersion::new("1.0.0").unwrap(),
        Digest::of(DigestAlgorithm::Sha256, b"fixture implementation"),
    )
}

fn set(values: &[Grant]) -> BTreeSet<Grant> {
    values.iter().copied().collect()
}

#[test]
fn catalog_effect_egress_credential_and_depth_are_one_intersection() {
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let workspace = WorkspaceId::generate().unwrap();
    let authority = set(&[
        Grant::WorkspaceRead,
        Grant::WorkspaceWrite,
        Grant::NetworkEgress,
    ]);
    let config = make_config(principal, project, authority.clone());
    let authenticated = authenticate(principal, project, authority);
    let capability = capability();
    let schema = Digest::of(DigestAlgorithm::Sha256, b"schema");
    let credential = SecretHandle::parse("secret:provider-a").unwrap();
    let egress =
        EgressConstraint::new("HTTPS", "API.Example.COM.", 443, credential.clone()).unwrap();
    let extension = GrantExtension::new([egress.clone()], [credential.clone()], 0).unwrap();
    let grant_constraints = ArgumentConstraints::new([b"tenant=acme".as_slice()]);
    let requested_constraints =
        ArgumentConstraints::new([b"tenant=acme".as_slice(), b"page=2".as_slice()]);
    let grants = CapabilityGrantSnapshot::new(
        &config,
        [CapabilityGrant::new(
            principal,
            project,
            workspace,
            capability.clone(),
            schema,
            EffectClass::WorkspaceRead,
            grant_constraints,
        )
        .with_extension(extension.clone())],
        DigestAlgorithm::Sha256,
    );
    let request_extension = RequestExtension::new(Some(egress.clone()), Some(credential.clone()));
    let evaluate = |effect, constraints: &ArgumentConstraints, extension: RequestExtension| {
        decide(GrantRequest {
            authenticated: &authenticated,
            capability: &capability,
            schema_digest: schema,
            effect,
            argument_constraints: constraints,
            workspace_id: workspace,
            project_id: project,
            config: &config,
            grants: &grants,
            delegation: None,
            extension,
        })
    };

    let allowed = evaluate(
        EffectClass::WorkspaceRead,
        &requested_constraints,
        request_extension.clone(),
    );
    assert_eq!(allowed.reason(), GrantReasonCode::Granted);
    assert_eq!(allowed.binding_inputs().delegation_depth(), 0);
    assert_eq!(
        allowed
            .binding_inputs()
            .extension()
            .egress()
            .unwrap()
            .host(),
        "api.example.com"
    );

    let wrong_arguments = ArgumentConstraints::new([b"tenant=other".as_slice()]);
    assert_eq!(
        evaluate(
            EffectClass::WorkspaceRead,
            &wrong_arguments,
            request_extension.clone()
        )
        .reason(),
        GrantReasonCode::NoMatchingGrant
    );
    assert_eq!(
        evaluate(
            EffectClass::WorkspaceWrite,
            &requested_constraints,
            request_extension.clone()
        )
        .reason(),
        GrantReasonCode::NoMatchingGrant
    );
    let wrong_credential = SecretHandle::parse("secret:provider-b").unwrap();
    assert_eq!(
        evaluate(
            EffectClass::WorkspaceRead,
            &requested_constraints,
            RequestExtension::new(Some(egress.clone()), Some(wrong_credential))
        )
        .reason(),
        GrantReasonCode::NoMatchingGrant
    );

    let egress_without_credential_authority = CapabilityGrantSnapshot::new(
        &config,
        [CapabilityGrant::new(
            principal,
            project,
            workspace,
            capability.clone(),
            schema,
            EffectClass::WorkspaceRead,
            requested_constraints.clone(),
        )
        .with_extension(GrantExtension::new([egress.clone()], [], 0).unwrap())],
        DigestAlgorithm::Sha256,
    );
    assert_eq!(
        decide(GrantRequest {
            authenticated: &authenticated,
            capability: &capability,
            schema_digest: schema,
            effect: EffectClass::WorkspaceRead,
            argument_constraints: &requested_constraints,
            workspace_id: workspace,
            project_id: project,
            config: &config,
            grants: &egress_without_credential_authority,
            delegation: None,
            extension: RequestExtension::new(Some(egress.clone()), None),
        })
        .reason(),
        GrantReasonCode::NoMatchingGrant
    );

    let no_network = set(&[Grant::WorkspaceRead]);
    let no_network_config = make_config(principal, project, no_network.clone());
    let no_network_auth = authenticate(principal, project, no_network);
    let no_network_grants = CapabilityGrantSnapshot::new(
        &no_network_config,
        [CapabilityGrant::new(
            principal,
            project,
            workspace,
            capability.clone(),
            schema,
            EffectClass::WorkspaceRead,
            requested_constraints.clone(),
        )
        .with_extension(extension)],
        DigestAlgorithm::Sha256,
    );
    assert_eq!(
        decide(GrantRequest {
            authenticated: &no_network_auth,
            capability: &capability,
            schema_digest: schema,
            effect: EffectClass::WorkspaceRead,
            argument_constraints: &requested_constraints,
            workspace_id: workspace,
            project_id: project,
            config: &no_network_config,
            grants: &no_network_grants,
            delegation: None,
            extension: request_extension,
        })
        .reason(),
        GrantReasonCode::EffectNotAuthenticated
    );
}

#[test]
fn delegation_depth_violation_is_denied_by_decide() {
    let root = PrincipalId::generate().unwrap();
    let child = PrincipalId::generate().unwrap();
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let workspace = WorkspaceId::generate().unwrap();
    let authority = set(&[Grant::WorkspaceRead]);
    let config = make_config(principal, project, authority.clone());
    let authenticated = authenticate(principal, project, authority);
    let capability = capability();
    let schema = Digest::of(DigestAlgorithm::Sha256, b"schema");
    let constraints = ArgumentConstraints::default();
    let grant = |depth| {
        CapabilityGrant::new(
            principal,
            project,
            workspace,
            capability.clone(),
            schema,
            EffectClass::WorkspaceRead,
            constraints.clone(),
        )
        .with_extension(GrantExtension::new([], [], depth).unwrap())
    };
    let root_grants = CapabilityGrantSnapshot::new(&config, [grant(1)], DigestAlgorithm::Sha256);
    let delegated_grants =
        CapabilityGrantSnapshot::new(&config, [grant(2)], DigestAlgorithm::Sha256);
    assert!(DelegationSnapshot::new(vec![child, principal], 1, delegated_grants.clone()).is_ok());
    let delegation =
        DelegationSnapshot::new(vec![root, child, principal], 2, delegated_grants).unwrap();
    let decision = decide(GrantRequest {
        authenticated: &authenticated,
        capability: &capability,
        schema_digest: schema,
        effect: EffectClass::WorkspaceRead,
        argument_constraints: &constraints,
        workspace_id: workspace,
        project_id: project,
        config: &config,
        grants: &root_grants,
        delegation: Some(&delegation),
        extension: RequestExtension::default(),
    });
    assert_eq!(decision.reason(), GrantReasonCode::DelegationDepthExceeded);
    assert_eq!(decision.binding_inputs().delegation_depth(), 2);

    let other = PrincipalId::generate().unwrap();
    let other_config = make_config(other, project, set(&[Grant::WorkspaceRead]));
    let other_grants = CapabilityGrantSnapshot::new(
        &other_config,
        [CapabilityGrant::new(
            other,
            project,
            workspace,
            capability.clone(),
            schema,
            EffectClass::WorkspaceRead,
            constraints.clone(),
        )],
        DigestAlgorithm::Sha256,
    );
    let mismatched = DelegationSnapshot::new(vec![root, other], 1, other_grants).unwrap();
    let depth_zero_root =
        CapabilityGrantSnapshot::new(&config, [grant(0)], DigestAlgorithm::Sha256);
    assert_eq!(
        decide(GrantRequest {
            authenticated: &authenticated,
            capability: &capability,
            schema_digest: schema,
            effect: EffectClass::WorkspaceRead,
            argument_constraints: &constraints,
            workspace_id: workspace,
            project_id: project,
            config: &config,
            grants: &depth_zero_root,
            delegation: Some(&mismatched),
            extension: RequestExtension::default(),
        })
        .reason(),
        GrantReasonCode::DelegationPrincipalMismatch
    );
}

#[test]
fn extension_fields_change_snapshot_and_decision_digests() {
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let workspace = WorkspaceId::generate().unwrap();
    let authority = set(&[Grant::WorkspaceRead, Grant::NetworkEgress]);
    let config = make_config(principal, project, authority.clone());
    let authenticated = authenticate(principal, project, authority);
    let capability = capability();
    let schema = Digest::of(DigestAlgorithm::Sha256, b"schema");
    let constraints = ArgumentConstraints::default();
    let credential = SecretHandle::parse("secret:a").unwrap();
    let base = CapabilityGrant::new(
        principal,
        project,
        workspace,
        capability.clone(),
        schema,
        EffectClass::WorkspaceRead,
        constraints.clone(),
    );
    let direct = CapabilityGrantSnapshot::new(&config, [base.clone()], DigestAlgorithm::Sha256);
    let extended = CapabilityGrantSnapshot::new(
        &config,
        [base.with_extension(GrantExtension::new([], [credential.clone()], 1).unwrap())],
        DigestAlgorithm::Sha256,
    );
    assert_ne!(direct.digest(), extended.digest());

    let decide_with = |extension| {
        decide(GrantRequest {
            authenticated: &authenticated,
            capability: &capability,
            schema_digest: schema,
            effect: EffectClass::WorkspaceRead,
            argument_constraints: &constraints,
            workspace_id: workspace,
            project_id: project,
            config: &config,
            grants: &extended,
            delegation: None,
            extension,
        })
    };
    assert_ne!(
        decide_with(RequestExtension::default()).snapshot_digest(),
        decide_with(RequestExtension::new(None, Some(credential))).snapshot_digest()
    );
}

#[test]
fn exactly_one_capability_decision_entry_point() {
    let grant = include_str!("../../src/capabilities/kernel/grant.rs");
    let extension = include_str!("../../src/capabilities/kernel/grant_ext.rs");
    let invoke = include_str!("../../src/capabilities/kernel/invoke.rs");
    let public_entries = [grant, extension, invoke]
        .into_iter()
        .flat_map(str::lines)
        .filter(|line| {
            let line = line.trim_start();
            ["authorize", "authorized", "decide"]
                .into_iter()
                .any(|name| line.starts_with(&format!("pub fn {name}(")))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        public_entries,
        ["pub fn decide(request: GrantRequest<'_>) -> GrantDecision {"]
    );
}

#[test]
fn extension_limits_bound_even_infinite_input() {
    let credentials = (0_u64..).map(|index| {
        assert!(
            index <= 64,
            "constructor consumed beyond its rejection bound"
        );
        SecretHandle::parse(&format!("secret:{index}")).unwrap()
    });
    assert_eq!(
        GrantExtension::new([], credentials, 0),
        Err(kit::capabilities::kernel::grant_ext::GrantExtensionError::LimitExceeded)
    );

    let credential = SecretHandle::parse("secret:repeated").unwrap();
    assert_eq!(
        GrantExtension::new([], std::iter::repeat(credential), 0),
        Err(kit::capabilities::kernel::grant_ext::GrantExtensionError::LimitExceeded)
    );
}
