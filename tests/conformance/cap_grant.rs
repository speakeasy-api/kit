use std::collections::{BTreeMap, BTreeSet};

use kit::{
    api::auth::{
        contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot},
        local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
    },
    capabilities::kernel::{
        grant::{
            ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, DelegationSnapshot,
            EffectClass, GrantReasonCode, GrantRequest, authorized, decide,
        },
        identity::{
            CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
            CapabilityVersion, Digest, DigestAlgorithm, SourceSchema,
        },
    },
    domain::{
        config::{
            BudgetLayer, CONFIG_SCHEMA_VERSION, ConcurrencyLayer, ConfigLayer, Executor, Grant,
            LayerStack, Provider, RetentionLayer, RunConfigContext, RunConfigSnapshot,
        },
        ids::{PrincipalId, ProjectId, RunId, WorkspaceId},
    },
};
use sha2::{Digest as _, Sha256};

const UID: u32 = 501;

fn set(values: &[Grant]) -> BTreeSet<Grant> {
    values.iter().copied().collect()
}

fn config(
    principal_id: PrincipalId,
    project_id: ProjectId,
    authority: BTreeSet<Grant>,
    max_tokens: u64,
) -> RunConfigSnapshot {
    let built_in = ConfigLayer {
        schema_version: CONFIG_SCHEMA_VERSION,
        budgets: BudgetLayer {
            max_tokens: Some(max_tokens),
            max_cost_microusd: Some(10_000),
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
    };
    LayerStack {
        built_in,
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

fn authenticate(
    principal_id: PrincipalId,
    project_id: ProjectId,
    authority: BTreeSet<Grant>,
) -> AuthenticatedPrincipal {
    LocalPeerAuthenticator::new(BTreeMap::from([(
        UID,
        GrantSnapshot::new(principal_id, project_id, authority),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(UID, 42, UID))
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

#[test]
fn source_schema_bytes_dialect_docs_and_digests_are_preserved() {
    let source = br#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "type": "object",
  "properties": {"path": {"type": "string", "description": "Keep exactly."}},
  "required": ["path"],
  "unevaluatedProperties": false
}"#
    .to_vec();
    let normalized = br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","properties":{"path":{"description":"Keep exactly.","type":"string"}},"required":["path"],"type":"object","unevaluatedProperties":false}"#.to_vec();
    let docs = b"Author docs\r\n\xff remain byte-exact.".to_vec();
    let dialect = "https://json-schema.org/draft/2020-12/schema";
    let schema = SourceSchema::new(
        source.clone(),
        dialect,
        docs.clone(),
        normalized.clone(),
        DigestAlgorithm::Sha256,
    )
    .unwrap();

    assert_eq!(schema.source_bytes(), source);
    assert_eq!(schema.dialect(), dialect);
    assert_eq!(schema.documentation(), docs);
    assert_eq!(schema.normalized_bytes(), normalized);
    assert_eq!(schema.source_digest().algorithm(), DigestAlgorithm::Sha256);
    assert_eq!(
        schema.source_digest().as_bytes(),
        Sha256::digest(&source)[..]
    );
    assert_eq!(
        schema.normalized_digest().as_bytes(),
        Sha256::digest(&normalized)[..]
    );
    assert_eq!(
        Digest::of(DigestAlgorithm::Blake3, b"abc").hex(),
        "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
    );

    let same = identity("read");
    let changed_implementation = CapabilityIdentity::new(
        same.source().clone(),
        same.namespace().clone(),
        same.name().clone(),
        same.version().clone(),
        Digest::of(DigestAlgorithm::Blake3, b"workspace implementation v2"),
    );
    assert_ne!(same, changed_implementation);
}

#[test]
fn grant_is_the_intersection_of_auth_config_rule_and_delegation() {
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let workspace_id = WorkspaceId::generate().unwrap();
    let coarse = set(&[Grant::WorkspaceRead, Grant::WorkspaceWrite]);
    let config = config(principal_id, project_id, coarse.clone(), 1_000);
    let authenticated = authenticate(principal_id, project_id, coarse);
    let capability = identity("read");
    let schema_digest = Digest::of(DigestAlgorithm::Sha256, b"read schema");
    let root_constraints = ArgumentConstraints::new([b"workspace=root".as_slice()]);
    let delegated_constraints =
        ArgumentConstraints::new([b"workspace=root".as_slice(), b"extension=rs".as_slice()]);
    let root_grants = CapabilityGrantSnapshot::new(
        &config,
        [CapabilityGrant::new(
            principal_id,
            project_id,
            workspace_id,
            capability.clone(),
            schema_digest,
            EffectClass::WorkspaceRead,
            root_constraints.clone(),
        )],
        DigestAlgorithm::Sha256,
    );
    let delegated_grants = CapabilityGrantSnapshot::new(
        &config,
        [CapabilityGrant::new(
            principal_id,
            project_id,
            workspace_id,
            capability.clone(),
            schema_digest,
            EffectClass::WorkspaceRead,
            delegated_constraints.clone(),
        )],
        DigestAlgorithm::Sha256,
    );
    let delegation = DelegationSnapshot::new(vec![principal_id], 1, delegated_grants).unwrap();

    let allowed = decide(GrantRequest {
        authenticated: &authenticated,
        capability: &capability,
        schema_digest,
        effect: EffectClass::WorkspaceRead,
        argument_constraints: &delegated_constraints,
        workspace_id,
        project_id,
        config: &config,
        grants: &root_grants,
        delegation: Some(&delegation),
    });
    assert!(allowed.is_allowed());
    assert_eq!(allowed.reason(), GrantReasonCode::Granted);
    assert_eq!(
        allowed.binding_inputs().grant_snapshot_digest(),
        root_grants.digest()
    );
    assert_eq!(
        allowed.binding_inputs().config_snapshot_digest(),
        config.digest()
    );
    assert_eq!(
        allowed.binding_inputs().delegation_digest(),
        Some(delegation.digest())
    );

    let denied = decide(GrantRequest {
        authenticated: &authenticated,
        capability: &capability,
        schema_digest,
        effect: EffectClass::WorkspaceRead,
        argument_constraints: &root_constraints,
        workspace_id,
        project_id,
        config: &config,
        grants: &root_grants,
        delegation: Some(&delegation),
    });
    assert!(!denied.is_allowed());
    assert_eq!(denied.reason(), GrantReasonCode::DelegationDenied);
}

#[test]
fn decision_replays_identically_and_later_config_cannot_expand_it() {
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let workspace_id = WorkspaceId::generate().unwrap();
    let authenticated_authority = set(&[Grant::WorkspaceRead, Grant::WorkspaceWrite]);
    let authenticated = authenticate(principal_id, project_id, authenticated_authority.clone());
    let original = config(
        principal_id,
        project_id,
        set(&[Grant::WorkspaceRead]),
        1_000,
    );
    let later_expanded = config(principal_id, project_id, authenticated_authority, 2_000);
    let capability = identity("write");
    let schema_digest = Digest::of(DigestAlgorithm::Blake3, b"write schema");
    let constraints = ArgumentConstraints::default();
    let grants = CapabilityGrantSnapshot::new(
        &original,
        [CapabilityGrant::new(
            principal_id,
            project_id,
            workspace_id,
            capability.clone(),
            schema_digest,
            EffectClass::WorkspaceWrite,
            constraints.clone(),
        )],
        DigestAlgorithm::Blake3,
    );
    let evaluate = |config: &RunConfigSnapshot| {
        decide(GrantRequest {
            authenticated: &authenticated,
            capability: &capability,
            schema_digest,
            effect: EffectClass::WorkspaceWrite,
            argument_constraints: &constraints,
            workspace_id,
            project_id,
            config,
            grants: &grants,
            delegation: None,
        })
    };

    let expected = evaluate(&original);
    assert_eq!(expected.reason(), GrantReasonCode::EffectNotConfigured);
    for _ in 0..1_000 {
        assert_eq!(evaluate(&original), expected);
    }
    assert_eq!(
        evaluate(&later_expanded).reason(),
        GrantReasonCode::ConfigurationSnapshotChanged
    );
}

#[test]
fn deny_by_default_keeps_unauthorized_capabilities_absent() {
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let workspace_id = WorkspaceId::generate().unwrap();
    let authority = set(&[Grant::WorkspaceRead]);
    let config = config(principal_id, project_id, authority.clone(), 1_000);
    let authenticated = authenticate(principal_id, project_id, authority);
    let visible = identity("visible");
    let forbidden = identity("forbidden");
    let schema_digest = Digest::of(DigestAlgorithm::Sha256, b"catalog schema");
    let constraints = ArgumentConstraints::default();
    let grants = CapabilityGrantSnapshot::new(
        &config,
        [CapabilityGrant::new(
            principal_id,
            project_id,
            workspace_id,
            visible.clone(),
            schema_digest,
            EffectClass::WorkspaceRead,
            constraints.clone(),
        )],
        DigestAlgorithm::Sha256,
    );

    let discovered = [&visible, &forbidden]
        .into_iter()
        .filter_map(|capability| {
            authorized(GrantRequest {
                authenticated: &authenticated,
                capability,
                schema_digest,
                effect: EffectClass::WorkspaceRead,
                argument_constraints: &constraints,
                workspace_id,
                project_id,
                config: &config,
                grants: &grants,
                delegation: None,
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(discovered.len(), 1);
    assert_eq!(discovered[0].capability(), &visible);

    let denied = decide(GrantRequest {
        authenticated: &authenticated,
        capability: &forbidden,
        schema_digest,
        effect: EffectClass::WorkspaceRead,
        argument_constraints: &constraints,
        workspace_id,
        project_id,
        config: &config,
        grants: &grants,
        delegation: None,
    });
    assert_eq!(denied.reason(), GrantReasonCode::NoMatchingGrant);
}
