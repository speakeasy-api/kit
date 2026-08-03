use std::collections::{BTreeMap, BTreeSet};

use kit::{
    api::auth::{
        contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot},
        local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
    },
    capabilities::{
        catalog::{
            Availability, CapabilityKind, CatalogAuthority, CatalogEntry, CatalogSchemas,
            CatalogSearch, CatalogSnapshot, CatalogSource, CostStats, LatencyStats,
            ReliabilityStats, SideEffects, SourceKind, TrustDomain,
        },
        discovery::{BindingExpired, DiscoveryHandle, DiscoverySession},
        kernel::{
            grant::{ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, EffectClass},
            grant_ext::RequestExtension,
            identity::{
                CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
                CapabilityVersion, Digest, DigestAlgorithm,
            },
            invoke::RetrySafety,
        },
        schema::{JSON_SCHEMA_2020_12, NormalizedSchema, SchemaProjectionSet},
    },
    domain::{
        config::{
            BudgetLayer, CONFIG_SCHEMA_VERSION, ConcurrencyLayer, ConfigLayer, Executor, Grant,
            LayerStack, Provider, RetentionLayer, RunConfigContext, RunConfigSnapshot,
        },
        ids::{PrincipalId, ProjectId, RunId, WorkspaceId},
    },
};

const UID: u32 = 907;

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

fn authenticate(
    principal_id: PrincipalId,
    project_id: ProjectId,
    authority: BTreeSet<Grant>,
) -> AuthenticatedPrincipal {
    LocalPeerAuthenticator::new(BTreeMap::from([(
        UID,
        GrantSnapshot::new(principal_id, project_id, authority),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(UID, 1, UID))
    .unwrap()
}

fn schema(canary: &str) -> NormalizedSchema {
    NormalizedSchema::ingest(
        serde_json::to_vec(&serde_json::json!({
            "$schema": JSON_SCHEMA_2020_12,
            "title": canary,
            "type": "object"
        }))
        .unwrap(),
        JSON_SCHEMA_2020_12,
        format!("documentation-{canary}"),
        DigestAlgorithm::Sha256,
    )
    .unwrap()
}

fn forbidden_entry(index: usize) -> CatalogEntry {
    forbidden_entry_of_kind(index, CapabilityKind::Tool)
}

fn forbidden_entry_of_kind(index: usize, kind: CapabilityKind) -> CatalogEntry {
    let canary = format!("forbidden-{index:03}");
    let source_id = CapabilitySource::new("forbidden-source-canary").unwrap();
    CatalogEntry::new(
        CapabilityIdentity::new(
            source_id.clone(),
            CapabilityNamespace::new("forbidden.namespace.canary").unwrap(),
            CapabilityName::new(canary.clone()).unwrap(),
            CapabilityVersion::new("1.0.0").unwrap(),
            Digest::of(DigestAlgorithm::Blake3, canary.as_bytes()),
        ),
        CatalogSource::new(
            SourceKind::Mcp,
            source_id,
            TrustDomain::new("forbidden-trust-canary").unwrap(),
        )
        .unwrap(),
        kind,
        CatalogSchemas::new(
            SchemaProjectionSet::new(schema(&format!("schema-{canary}"))),
            Some(SchemaProjectionSet::new(schema("forbidden-output-canary"))),
        ),
        CatalogSearch::new(format!("summary-{canary}"), [format!("term-{canary}")]).unwrap(),
        SideEffects::new(EffectClass::WorkspaceRead, RetrySafety::Idempotent),
        CatalogAuthority::new(
            [Grant::WorkspaceRead],
            (index % 2 == 1).then(|| format!("scope-{canary}")),
        )
        .unwrap(),
        Availability::Available,
        ReliabilityStats::default(),
        LatencyStats::Unobserved,
        CostStats::Unobserved,
    )
    .unwrap()
}

fn grant(
    principal_id: PrincipalId,
    project_id: ProjectId,
    workspace_id: WorkspaceId,
    entry: &CatalogEntry,
) -> CapabilityGrant {
    CapabilityGrant::new(
        principal_id,
        project_id,
        workspace_id,
        entry.identity().clone(),
        entry
            .schemas()
            .input()
            .schema()
            .source()
            .normalized_digest(),
        EffectClass::WorkspaceRead,
        ArgumentConstraints::default(),
    )
}

#[test]
fn catalog_visibility() {
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let workspace_id = WorkspaceId::generate().unwrap();
    let authority = BTreeSet::from([Grant::WorkspaceRead]);
    let config = config(principal_id, project_id, authority.clone());
    let authenticated = authenticate(principal_id, project_id, authority);
    let catalog =
        CatalogSnapshot::new((0..512).map(forbidden_entry), DigestAlgorithm::Sha256).unwrap();
    let constraints = ArgumentConstraints::default();
    let denied_grants = CapabilityGrantSnapshot::new(&config, [], DigestAlgorithm::Sha256);
    let denied = DiscoverySession::new(
        &catalog,
        &authenticated,
        &config,
        &denied_grants,
        None,
        workspace_id,
        project_id,
        &constraints,
        RequestExtension::default(),
    );

    for index in 0..512 {
        let canary = format!("forbidden-{index:03}");
        for query in [
            canary.clone(),
            format!("summary-{canary}"),
            format!("term-{canary}"),
            format!("schema-{canary}"),
            format!("documentation-schema-{canary}"),
            format!("scope-{canary}"),
        ] {
            assert!(denied.search(&query, 100).unwrap().is_empty());
        }
    }
    for canary in [
        "forbidden-source-canary",
        "forbidden-trust-canary",
        "forbidden.namespace.canary",
        "forbidden-output-canary",
    ] {
        assert!(denied.search(canary, 100).unwrap().is_empty());
    }

    let privileged_grants = CapabilityGrantSnapshot::new(
        &config,
        catalog
            .entries()
            .iter()
            .map(|entry| grant(principal_id, project_id, workspace_id, entry)),
        DigestAlgorithm::Sha256,
    );
    let privileged = DiscoverySession::new(
        &catalog,
        &authenticated,
        &config,
        &privileged_grants,
        None,
        workspace_id,
        project_id,
        &constraints,
        RequestExtension::default(),
    );
    let unknown = DiscoveryHandle::from_bytes([0xa5; 32]);
    assert!(denied.inspect(unknown).is_none());
    let mut handle_probes = 0;
    for index in (0..512).step_by(2) {
        let result = privileged
            .search(&format!("forbidden-{index:03}"), 1)
            .unwrap()
            .remove(0);
        let known_forbidden = result.handle();
        assert!(denied.inspect(known_forbidden).is_none());
        assert_eq!(
            denied.inspect(known_forbidden).is_some(),
            denied.inspect(unknown).is_some()
        );
        let mut forged = known_forbidden.as_bytes();
        forged[index % forged.len()] ^= 0xff;
        let forged = DiscoveryHandle::from_bytes(forged);
        assert!(denied.inspect(forged).is_none());
        assert!(privileged.inspect(forged).is_none());
        handle_probes += 3;
    }
    assert!(handle_probes >= 512);
    for index in (1..512).step_by(2) {
        assert_eq!(
            privileged
                .search(&format!("forbidden-{index:03}"), 1)
                .unwrap()
                .len(),
            1
        );
    }

    let allowed_result = privileged.search("forbidden-000", 1).unwrap().remove(0);
    let inspection = privileged.inspect(allowed_result.handle()).unwrap();
    let binding = privileged.bind(&inspection).unwrap();
    let replacement_catalog = CatalogSnapshot::new(
        (0..512)
            .map(forbidden_entry)
            .chain(std::iter::once(forbidden_entry(513))),
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    let replacement_grants = CapabilityGrantSnapshot::new(
        &config,
        replacement_catalog
            .entries()
            .iter()
            .filter(|entry| entry.identity().name().as_str() != "forbidden-513")
            .map(|entry| grant(principal_id, project_id, workspace_id, entry)),
        DigestAlgorithm::Sha256,
    );
    let replacement = DiscoverySession::new(
        &replacement_catalog,
        &authenticated,
        &config,
        &replacement_grants,
        None,
        workspace_id,
        project_id,
        &constraints,
        RequestExtension::default(),
    );
    let replacement_result = replacement.search("forbidden-000", 1).unwrap().remove(0);
    assert_eq!(replacement_result.handle(), allowed_result.handle());
    assert!(matches!(
        binding.validate(&replacement),
        Err(BindingExpired)
    ));
    for _ in 0..100 {
        let changed_grants = CapabilityGrantSnapshot::new(&config, [], DigestAlgorithm::Sha256);
        let changed = DiscoverySession::new(
            &catalog,
            &authenticated,
            &config,
            &changed_grants,
            None,
            workspace_id,
            project_id,
            &constraints,
            RequestExtension::default(),
        );
        assert!(matches!(binding.validate(&changed), Err(BindingExpired)));
    }
}

#[test]
fn five_hundred_unauthorized_probes_per_mcp_kind_reveal_nothing() {
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let workspace_id = WorkspaceId::generate().unwrap();
    let authority = BTreeSet::from([Grant::WorkspaceRead]);
    let config = config(principal_id, project_id, authority.clone());
    let authenticated = authenticate(principal_id, project_id, authority);
    let constraints = ArgumentConstraints::default();
    for kind in [
        CapabilityKind::Tool,
        CapabilityKind::Resource,
        CapabilityKind::ResourceTemplate,
        CapabilityKind::Prompt,
    ] {
        let catalog =
            CatalogSnapshot::new([forbidden_entry_of_kind(0, kind)], DigestAlgorithm::Sha256)
                .unwrap();
        let denied_grants = CapabilityGrantSnapshot::new(&config, [], DigestAlgorithm::Sha256);
        let allowed_grants = CapabilityGrantSnapshot::new(
            &config,
            catalog
                .entries()
                .iter()
                .map(|entry| grant(principal_id, project_id, workspace_id, entry)),
            DigestAlgorithm::Sha256,
        );
        let denied = DiscoverySession::new(
            &catalog,
            &authenticated,
            &config,
            &denied_grants,
            None,
            workspace_id,
            project_id,
            &constraints,
            RequestExtension::default(),
        );
        let allowed = DiscoverySession::new(
            &catalog,
            &authenticated,
            &config,
            &allowed_grants,
            None,
            workspace_id,
            project_id,
            &constraints,
            RequestExtension::default(),
        );
        let handle = allowed.search("forbidden-000", 1).unwrap()[0].handle();
        for _ in 0..500 {
            assert!(denied.search("forbidden-000", 1).unwrap().is_empty());
            assert!(denied.inspect(handle).is_none());
        }
    }
}
