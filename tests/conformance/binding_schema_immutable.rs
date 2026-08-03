use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use kit::{
    api::auth::{
        contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot},
        local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
    },
    capabilities::{
        catalog::{
            Availability, CapabilityKind, CatalogAuthority, CatalogEntry, CatalogSchemas, CatalogSearch,
            CatalogSnapshot, CatalogSource, CostStats, LatencyStats, ReliabilityStats, SideEffects,
            SourceKind, TrustDomain,
        },
        discovery::{
            BindingExpired, DiscoveryHandle, DiscoverySession, MAX_SEARCH_QUERY_BYTES,
            MAX_SEARCH_RESULTS, SearchError,
        },
        kernel::{
            grant::{
                ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, DelegationSnapshot,
                EffectClass,
            },
            grant_ext::{GrantExtension, RequestExtension},
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
        secret::SecretHandle,
    },
};

const UID: u32 = 806;

fn authority(values: &[Grant]) -> BTreeSet<Grant> {
    values.iter().copied().collect()
}

fn discovery_config(
    principal_id: PrincipalId,
    project_id: ProjectId,
    run_id: RunId,
    grants: BTreeSet<Grant>,
    max_tokens: u64,
) -> RunConfigSnapshot {
    LayerStack {
        built_in: ConfigLayer {
            schema_version: CONFIG_SCHEMA_VERSION,
            budgets: BudgetLayer {
                max_tokens: Some(max_tokens),
                max_cost_microusd: Some(100),
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
            grants: Some(grants.clone()),
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
            run_id,
        },
        &grants,
    )
    .unwrap()
}

fn authenticate(
    principal_id: PrincipalId,
    project_id: ProjectId,
    grants: BTreeSet<Grant>,
) -> AuthenticatedPrincipal {
    LocalPeerAuthenticator::new(BTreeMap::from([(
        UID,
        GrantSnapshot::new(principal_id, project_id, grants),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(UID, 1, UID))
    .unwrap()
}

fn schema(label: &str) -> NormalizedSchema {
    NormalizedSchema::ingest(
        serde_json::to_vec(&serde_json::json!({
            "$schema": JSON_SCHEMA_2020_12,
            "title": label,
            "type": "object",
            "properties": {"value": {"const": label}}
        }))
        .unwrap(),
        JSON_SCHEMA_2020_12,
        format!("exact documentation for {label}"),
        DigestAlgorithm::Sha256,
    )
    .unwrap()
}

fn entry(
    name: &str,
    schema_label: &str,
    required: &[Grant],
    scopes: &[&str],
) -> CatalogEntry {
    entry_of_kind(
        name,
        schema_label,
        required,
        scopes,
        CapabilityKind::Tool,
    )
}

fn entry_of_kind(
    name: &str,
    schema_label: &str,
    required: &[Grant],
    scopes: &[&str],
    kind: CapabilityKind,
) -> CatalogEntry {
    let source_id = CapabilitySource::new("discovery-source").unwrap();
    CatalogEntry::new(
        CapabilityIdentity::new(
            source_id.clone(),
            CapabilityNamespace::new("kit.discovery").unwrap(),
            CapabilityName::new(name).unwrap(),
            CapabilityVersion::new("1.0.0").unwrap(),
            Digest::of(DigestAlgorithm::Blake3, format!("implementation:{name}").as_bytes()),
        ),
        CatalogSource::new(
            SourceKind::Mcp,
            source_id,
            TrustDomain::new("discovery-trust").unwrap(),
        )
        .unwrap(),
        kind,
        CatalogSchemas::new(
            SchemaProjectionSet::new(schema(schema_label)),
            Some(SchemaProjectionSet::new(schema("output"))),
        ),
        CatalogSearch::new(format!("summary for {name}"), ["discovery", name]).unwrap(),
        SideEffects::new(EffectClass::WorkspaceRead, RetrySafety::Idempotent),
        CatalogAuthority::new(required.iter().copied(), scopes.iter().copied()).unwrap(),
        Availability::Available,
        ReliabilityStats::default(),
        LatencyStats::Unobserved,
        CostStats::Unobserved,
    )
    .unwrap()
}

#[test]
fn binding_id_covers_semantic_kind_and_full_entry_digest() {
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let workspace_id = WorkspaceId::generate().unwrap();
    let authority = authority(&[Grant::WorkspaceRead]);
    let config = discovery_config(
        principal_id,
        project_id,
        RunId::generate().unwrap(),
        authority.clone(),
        100,
    );
    let authenticated = authenticate(principal_id, project_id, authority);
    let constraints = ArgumentConstraints::default();
    let bind = |kind| {
        let catalog = CatalogSnapshot::new(
            [entry_of_kind(
                "semantic",
                "same-schema",
                &[Grant::WorkspaceRead],
                &[],
                kind,
            )],
            DigestAlgorithm::Sha256,
        )
        .unwrap();
        let grants = CapabilityGrantSnapshot::new(
            &config,
            [grant(
                principal_id,
                project_id,
                workspace_id,
                &catalog.entries()[0],
                constraints.clone(),
                GrantExtension::default(),
            )],
            DigestAlgorithm::Sha256,
        );
        let session = DiscoverySession::new(
            &catalog,
            &authenticated,
            &config,
            &grants,
            None,
            workspace_id,
            project_id,
            &constraints,
            RequestExtension::default(),
        );
        let result = session.search("semantic", 1).unwrap().remove(0);
        session.bind(&session.inspect(result.handle()).unwrap()).unwrap().id()
    };

    assert_ne!(bind(CapabilityKind::Tool), bind(CapabilityKind::Resource));
}

fn grant(
    principal_id: PrincipalId,
    project_id: ProjectId,
    workspace_id: WorkspaceId,
    entry: &CatalogEntry,
    constraints: ArgumentConstraints,
    extension: GrantExtension,
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
        entry.side_effects().effect(),
        constraints,
    )
    .with_extension(extension)
}

#[test]
fn binding_schema_immutable() {
    for iteration in 0..100 {
        let principal_id = PrincipalId::generate().unwrap();
        let project_id = ProjectId::generate().unwrap();
        let workspace_id = WorkspaceId::generate().unwrap();
        let coarse = authority(&[Grant::WorkspaceRead]);
        let config = discovery_config(
            principal_id,
            project_id,
            RunId::generate().unwrap(),
            coarse.clone(),
            100,
        );
        let authenticated = authenticate(principal_id, project_id, coarse);
        let original_entry = entry("immutable", "input-v1", &[Grant::WorkspaceRead], &[]);
        let original_catalog =
            CatalogSnapshot::new([original_entry], DigestAlgorithm::Sha256).unwrap();
        let constraints = ArgumentConstraints::default();
        let original_grants = CapabilityGrantSnapshot::new(
            &config,
            [grant(
                principal_id,
                project_id,
                workspace_id,
                &original_catalog.entries()[0],
                constraints.clone(),
                GrantExtension::default(),
            )],
            DigestAlgorithm::Sha256,
        );
        let original_session = DiscoverySession::new(
            &original_catalog,
            &authenticated,
            &config,
            &original_grants,
            None,
            workspace_id,
            project_id,
            &constraints,
            RequestExtension::default(),
        );
        let result = original_session.search("immutable", 1).unwrap().remove(0);
        let inspection = original_session.inspect(result.handle()).unwrap();
        let binding = original_session.bind(&inspection).unwrap();
        let old_id = binding.id();
        let old_schema = binding.input_schema_digest();
        assert_eq!(binding.catalog_digest(), original_catalog.digest());
        assert!(Arc::ptr_eq(
            binding.pinned_entry(),
            &original_catalog.entries()[0]
        ));

        let changed = entry(
            "immutable",
            &format!("input-v2-{iteration}"),
            &[Grant::WorkspaceRead],
            &[],
        );
        let changed_catalog = CatalogSnapshot::new([changed], DigestAlgorithm::Sha256).unwrap();
        let changed_entry = changed_catalog
            .entries()
            .iter()
            .find(|candidate| candidate.identity().name().as_str() == "immutable")
            .unwrap();
        let changed_grants = CapabilityGrantSnapshot::new(
            &config,
            [grant(
                principal_id,
                project_id,
                workspace_id,
                changed_entry,
                constraints.clone(),
                GrantExtension::default(),
            )],
            DigestAlgorithm::Sha256,
        );
        let changed_session = DiscoverySession::new(
            &changed_catalog,
            &authenticated,
            &config,
            &changed_grants,
            None,
            workspace_id,
            project_id,
            &constraints,
            RequestExtension::default(),
        );

        assert_eq!(binding.id(), old_id);
        assert_eq!(binding.input_schema_digest(), old_schema);
        assert!(Arc::ptr_eq(
            binding.pinned_entry(),
            &original_catalog.entries()[0]
        ));
        assert_eq!(
            binding
                .pinned_entry()
                .schemas()
                .input()
                .schema()
                .source()
                .documentation(),
            b"exact documentation for input-v1"
        );
        assert!(matches!(
            binding.validate(&changed_session),
            Err(BindingExpired)
        ));

        let changed_result = changed_session.search("immutable", 1).unwrap().remove(0);
        let changed_inspection = changed_session.inspect(changed_result.handle()).unwrap();
        let changed_binding = changed_session.bind(&changed_inspection).unwrap();
        assert_ne!(changed_binding.id(), old_id);
        assert_ne!(changed_binding.input_schema_digest(), old_schema);
    }

    for iteration in 0..100 {
        let principal_id = PrincipalId::generate().unwrap();
        let project_id = ProjectId::generate().unwrap();
        let workspace_id = WorkspaceId::generate().unwrap();
        let coarse = authority(&[Grant::WorkspaceRead]);
        let config = discovery_config(
            principal_id,
            project_id,
            RunId::generate().unwrap(),
            coarse.clone(),
            100,
        );
        let authenticated = authenticate(principal_id, project_id, coarse);
        let original_catalog = CatalogSnapshot::new(
            [
                entry("immutable", "input-v1", &[Grant::WorkspaceRead], &[]),
                entry(
                    &format!("visible-{iteration}"),
                    "visible-input",
                    &[Grant::WorkspaceRead],
                    &[],
                ),
            ],
            DigestAlgorithm::Sha256,
        )
        .unwrap();
        let constraints = ArgumentConstraints::default();
        let grants = CapabilityGrantSnapshot::new(
            &config,
            original_catalog.entries().iter().map(|entry| {
                grant(
                    principal_id,
                    project_id,
                    workspace_id,
                    entry,
                    constraints.clone(),
                    GrantExtension::default(),
                )
            }),
            DigestAlgorithm::Sha256,
        );
        let original_session = DiscoverySession::new(
            &original_catalog,
            &authenticated,
            &config,
            &grants,
            None,
            workspace_id,
            project_id,
            &constraints,
            RequestExtension::default(),
        );
        let result = original_session.search("immutable", 1).unwrap().remove(0);
        let inspection = original_session.inspect(result.handle()).unwrap();
        let binding = original_session.bind(&inspection).unwrap();
        let old_id = binding.id();
        let old_schema = binding.input_schema_digest();
        assert_eq!(binding.catalog_digest(), original_catalog.digest());
        assert!(Arc::ptr_eq(
            binding.pinned_entry(),
            original_catalog
                .entries()
                .iter()
                .find(|entry| entry.identity().name().as_str() == "immutable")
                .unwrap()
        ));

        let changed_catalog = CatalogSnapshot::new(
            [entry(
                "immutable",
                "input-v1",
                &[Grant::WorkspaceRead],
                &[],
            )],
            DigestAlgorithm::Sha256,
        )
        .unwrap();
        let changed_session = DiscoverySession::new(
            &changed_catalog,
            &authenticated,
            &config,
            &grants,
            None,
            workspace_id,
            project_id,
            &constraints,
            RequestExtension::default(),
        );

        assert_eq!(binding.id(), old_id);
        assert_eq!(binding.input_schema_digest(), old_schema);
        assert!(Arc::ptr_eq(
            binding.pinned_entry(),
            original_catalog
                .entries()
                .iter()
                .find(|entry| entry.identity().name().as_str() == "immutable")
                .unwrap()
        ));
        assert!(matches!(
            binding.validate(&changed_session),
            Err(BindingExpired)
        ));
        let changed_result = changed_session.search("immutable", 1).unwrap().remove(0);
        let changed_inspection = changed_session.inspect(changed_result.handle()).unwrap();
        let changed_binding = changed_session.bind(&changed_inspection).unwrap();
        assert_eq!(changed_binding.id(), old_id);
        assert_eq!(changed_binding.input_schema_digest(), old_schema);
        assert_ne!(
            changed_binding.catalog_digest(),
            binding.catalog_digest()
        );
        assert!(!Arc::ptr_eq(
            changed_binding.pinned_entry(),
            binding.pinned_entry()
        ));
        assert!(Arc::ptr_eq(
            changed_binding.pinned_entry(),
            &changed_catalog.entries()[0]
        ));
    }
}

#[test]
fn capability_discovery() {
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let workspace_id = WorkspaceId::generate().unwrap();
    let read = authority(&[Grant::WorkspaceRead]);
    let run_id = RunId::generate().unwrap();
    let config = discovery_config(principal_id, project_id, run_id, read.clone(), 100);
    let authenticated = authenticate(principal_id, project_id, read);
    let first = entry("alpha", "alpha-input", &[Grant::WorkspaceRead], &[]);
    let second = entry("beta", "beta-input", &[Grant::WorkspaceRead], &[]);
    let scoped = entry(
        "scoped-canary",
        "scoped-input",
        &[Grant::WorkspaceRead],
        &["secret.scope"],
    );
    let narrowed = entry(
        "narrowed-canary",
        "narrowed-input",
        &[Grant::WorkspaceRead, Grant::WorkspaceWrite],
        &[],
    );
    let catalog = CatalogSnapshot::new(
        [first, second, scoped, narrowed],
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    let constraints = ArgumentConstraints::default();
    let credential = SecretHandle::parse("discovery-credential").unwrap();
    let grant_extension = GrantExtension::new([], [credential.clone()], 1).unwrap();
    let grants = CapabilityGrantSnapshot::new(
        &config,
        catalog.entries().iter().map(|entry| {
            grant(
                principal_id,
                project_id,
                workspace_id,
                entry,
                constraints.clone(),
                grant_extension.clone(),
            )
        }),
        DigestAlgorithm::Sha256,
    );
    let session = DiscoverySession::new(
        &catalog,
        &authenticated,
        &config,
        &grants,
        None,
        workspace_id,
        project_id,
        &constraints,
        RequestExtension::default(),
    );

    assert!(matches!(
        session.search("", 1),
        Err(SearchError::EmptyQuery)
    ));
    assert!(matches!(
        session.search(&"x".repeat(MAX_SEARCH_QUERY_BYTES + 1), 1),
        Err(SearchError::QueryTooLong)
    ));
    assert!(matches!(
        session.search("alpha", 0),
        Err(SearchError::InvalidLimit)
    ));
    assert!(matches!(
        session.search("alpha", MAX_SEARCH_RESULTS + 1),
        Err(SearchError::InvalidLimit)
    ));
    assert_eq!(session.search("scoped-canary", 10).unwrap().len(), 1);
    assert!(session.search("narrowed-canary", 10).unwrap().is_empty());

    let ordered = session.search("summary", 10).unwrap();
    assert_eq!(ordered.len(), 3);
    assert_eq!(ordered[0].identity().name().as_str(), "alpha");
    assert_eq!(ordered[1].identity().name().as_str(), "beta");
    assert_eq!(ordered[2].identity().name().as_str(), "scoped-canary");
    assert_eq!(ordered[0].summary(), "summary for alpha");
    let inspection = session.inspect(ordered[0].handle()).unwrap();
    assert_eq!(inspection.definition().identity(), ordered[0].identity());
    let binding = session.bind(&inspection).unwrap();
    assert_eq!(session.bind(&inspection).unwrap().id(), binding.id());
    let validated = binding.validate(&session).unwrap();
    assert_eq!(validated.entry().identity(), ordered[0].identity());
    assert_eq!(validated.input_schema_digest(), binding.input_schema_digest());
    assert!(
        session
            .inspect(DiscoveryHandle::from_bytes([0x5a; 32]))
            .is_none()
    );

    let extra = entry("extra", "extra-input", &[Grant::WorkspaceRead], &[]);
    let changed_grants = CapabilityGrantSnapshot::new(
        &config,
        catalog
            .entries()
            .iter()
            .map(|entry| {
                grant(
                    principal_id,
                    project_id,
                    workspace_id,
                    entry,
                    constraints.clone(),
                    grant_extension.clone(),
                )
            })
            .chain([grant(
                principal_id,
                project_id,
                workspace_id,
                &extra,
                constraints.clone(),
                GrantExtension::default(),
            )]),
        DigestAlgorithm::Sha256,
    );
    let changed_grant_session = DiscoverySession::new(
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
    assert!(matches!(
        binding.validate(&changed_grant_session),
        Err(BindingExpired)
    ));

    let changed_config = discovery_config(
        principal_id,
        project_id,
        run_id,
        authority(&[Grant::WorkspaceRead]),
        101,
    );
    let changed_config_grants = CapabilityGrantSnapshot::new(
        &changed_config,
        catalog.entries().iter().map(|entry| {
            grant(
                principal_id,
                project_id,
                workspace_id,
                entry,
                constraints.clone(),
                grant_extension.clone(),
            )
        }),
        DigestAlgorithm::Sha256,
    );
    let changed_config_session = DiscoverySession::new(
        &catalog,
        &authenticated,
        &changed_config,
        &changed_config_grants,
        None,
        workspace_id,
        project_id,
        &constraints,
        RequestExtension::default(),
    );
    assert!(matches!(
        binding.validate(&changed_config_session),
        Err(BindingExpired)
    ));

    let changed_authenticated = authenticate(
        principal_id,
        project_id,
        authority(&[Grant::WorkspaceRead, Grant::WorkspaceWrite]),
    );
    let changed_auth_session = DiscoverySession::new(
        &catalog,
        &changed_authenticated,
        &config,
        &grants,
        None,
        workspace_id,
        project_id,
        &constraints,
        RequestExtension::default(),
    );
    assert!(matches!(
        binding.validate(&changed_auth_session),
        Err(BindingExpired)
    ));

    let delegation = DelegationSnapshot::new(vec![principal_id], 1, grants.clone()).unwrap();
    let delegated_session = DiscoverySession::new(
        &catalog,
        &authenticated,
        &config,
        &grants,
        Some(&delegation),
        workspace_id,
        project_id,
        &constraints,
        RequestExtension::default(),
    );
    assert!(matches!(
        binding.validate(&delegated_session),
        Err(BindingExpired)
    ));

    let changed_constraints = ArgumentConstraints::new([b"path=README.md".as_slice()]);
    let changed_policy_session = DiscoverySession::new(
        &catalog,
        &authenticated,
        &config,
        &grants,
        None,
        workspace_id,
        project_id,
        &changed_constraints,
        RequestExtension::default(),
    );
    assert!(matches!(
        binding.validate(&changed_policy_session),
        Err(BindingExpired)
    ));

    let changed_extension_session = DiscoverySession::new(
        &catalog,
        &authenticated,
        &config,
        &grants,
        None,
        workspace_id,
        project_id,
        &constraints,
        RequestExtension::new(None, Some(credential)),
    );
    assert!(matches!(
        binding.validate(&changed_extension_session),
        Err(BindingExpired)
    ));
}
