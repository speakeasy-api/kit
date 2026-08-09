use std::{cell::Cell, collections::BTreeSet, rc::Rc};

use kit::{
    agent::accounting::MoneyMicros,
    capabilities::{
        catalog::{
            Availability, CapabilityKind, CatalogAuthority, CatalogEntry, CatalogError,
            CatalogLimit, CatalogSchemas, CatalogSearch, CatalogSnapshot, CatalogSource, CostStats,
            LatencyStats, MAX_AUTH_SCOPES, MAX_CATALOG_ENTRIES, MAX_CATALOG_ENTRY_PAYLOAD_BYTES,
            MAX_CATALOG_PAYLOAD_BYTES, MAX_CATALOG_SOURCES, MAX_CATALOG_TEXT_BYTES,
            MAX_SEARCH_TERMS, MAX_SUMMARY_BYTES, ReliabilityStats, SideEffects, SourceKind,
            TrustDomain,
        },
        kernel::{
            grant::EffectClass,
            identity::{
                CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
                CapabilityVersion, Digest, DigestAlgorithm,
            },
            invoke::RetrySafety,
        },
        native::NativeCatalog,
        schema::{
            JSON_SCHEMA_2020_12, NormalizedSchema, ProjectionProfile, ProjectionTarget,
            SchemaProjectionSet,
        },
    },
    domain::config::Grant,
};

fn schema(label: &str) -> NormalizedSchema {
    let source = serde_json::to_vec(&serde_json::json!({
        "$schema": JSON_SCHEMA_2020_12,
        "title": label,
        "type": "object"
    }))
    .unwrap();
    NormalizedSchema::ingest(
        source,
        JSON_SCHEMA_2020_12,
        format!("{label} docs"),
        DigestAlgorithm::Sha256,
    )
    .unwrap()
}

fn fixture(
    source_id: &str,
    kind: SourceKind,
    name: &str,
    version: &str,
    implementation: &str,
) -> CatalogEntry {
    fixture_with(
        source_id,
        kind,
        name,
        version,
        implementation,
        "fixture summary",
        "fixture-trust",
        "input",
        Availability::Available,
        ReliabilityStats::default(),
        LatencyStats::Unobserved,
        CostStats::Unobserved,
        EffectClass::WorkspaceRead,
        RetrySafety::Idempotent,
        [Grant::WorkspaceRead],
        ["scope.read"],
    )
}

#[allow(clippy::too_many_arguments)]
fn fixture_with<G, S>(
    source_id: &str,
    kind: SourceKind,
    name: &str,
    version: &str,
    implementation: &str,
    summary: &str,
    trust: &str,
    schema_label: &str,
    availability: Availability,
    reliability: ReliabilityStats,
    latency: LatencyStats,
    cost: CostStats,
    effect: EffectClass,
    retry: RetrySafety,
    grants: G,
    scopes: S,
) -> CatalogEntry
where
    G: IntoIterator<Item = Grant>,
    S: IntoIterator<Item = &'static str>,
{
    let source_id = CapabilitySource::new(source_id).unwrap();
    let identity = CapabilityIdentity::new(
        source_id.clone(),
        CapabilityNamespace::new("fixture.tools").unwrap(),
        CapabilityName::new(name).unwrap(),
        CapabilityVersion::new(version).unwrap(),
        Digest::of(DigestAlgorithm::Blake3, implementation.as_bytes()),
    );
    let schemas = CatalogSchemas::new(
        SchemaProjectionSet::new(schema(schema_label)),
        Some(SchemaProjectionSet::new(schema("output"))),
    );
    CatalogEntry::new(
        identity,
        CatalogSource::new(kind, source_id, TrustDomain::new(trust).unwrap()).unwrap(),
        CapabilityKind::Tool,
        schemas,
        CatalogSearch::new(summary, ["fixture", name]).unwrap(),
        SideEffects::new(effect, retry),
        CatalogAuthority::new(grants, scopes).unwrap(),
        availability,
        reliability,
        latency,
        cost,
    )
    .unwrap()
}

fn rebuild(
    base: &CatalogEntry,
    schemas: CatalogSchemas,
    side_effects: SideEffects,
    authority: CatalogAuthority,
    reliability: ReliabilityStats,
    latency: LatencyStats,
    cost: CostStats,
) -> Result<CatalogEntry, CatalogError> {
    CatalogEntry::new(
        base.identity().clone(),
        base.source().clone(),
        base.kind(),
        schemas,
        base.search().clone(),
        side_effects,
        authority,
        base.availability(),
        reliability,
        latency,
        cost,
    )
}

#[test]
fn native_catalog_entries_preserve_complete_metadata() {
    let snapshot = CatalogSnapshot::from_native(DigestAlgorithm::Sha256).unwrap();
    assert_eq!(snapshot.entries().len(), 5);
    for descriptor in NativeCatalog::all() {
        let entry = snapshot.get_identity(descriptor.identity()).unwrap();
        assert_eq!(entry.identity(), descriptor.identity());
        assert_eq!(entry.source().kind(), SourceKind::Native);
        assert_eq!(entry.kind(), CapabilityKind::Tool);
        assert_eq!(entry.source().id(), descriptor.identity().source());
        assert!(!entry.source().trust_domain().as_str().is_empty());
        assert_eq!(
            entry.schemas().input().schema().source(),
            descriptor.normalized_schema().source()
        );
        assert!(entry.schemas().input().is_empty());
        assert!(entry.schemas().output().unwrap().is_empty());
        let output: serde_json::Value = serde_json::from_slice(
            entry
                .schemas()
                .output()
                .unwrap()
                .schema()
                .source()
                .normalized_bytes(),
        )
        .unwrap();
        assert_eq!(Some(&output), descriptor.spec().output_schema.as_ref());
        assert_eq!(
            entry.authority().required_grants(),
            &descriptor
                .required_grants()
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
        );
        assert!(entry.authority().auth_scopes().is_empty());
        assert_eq!(entry.side_effects().effect(), descriptor.effect());
        assert_eq!(
            entry.side_effects().retry_safety(),
            descriptor.retry_safety()
        );
        assert_eq!(entry.availability(), Availability::Available);
        assert_eq!(entry.reliability(), ReliabilityStats::default());
        assert_eq!(entry.latency(), LatencyStats::Unobserved);
        assert_eq!(entry.cost(), &CostStats::Unobserved);
        assert_eq!(entry.version(), descriptor.identity().version());
        assert!(!entry.search().summary().contains("Example:"));
        assert!(!entry.search().terms().is_empty());
        assert_eq!(entry.digests().schema(), entry.schemas().digest());
        assert_eq!(
            entry.digests().implementation(),
            descriptor.identity().implementation_digest()
        );
    }
}

#[test]
fn catalog_preserves_capability_kinds_and_absent_declared_outputs() {
    let entries = [
        CapabilityKind::Tool,
        CapabilityKind::Resource,
        CapabilityKind::ResourceTemplate,
        CapabilityKind::Prompt,
    ]
    .into_iter()
    .enumerate()
    .map(|(index, capability_kind)| {
        let source_id = CapabilitySource::new(format!("kind-{index}")).unwrap();
        CatalogEntry::new(
            CapabilityIdentity::new(
                source_id.clone(),
                CapabilityNamespace::new("fixture.kinds").unwrap(),
                CapabilityName::new(format!("kind-{index}")).unwrap(),
                CapabilityVersion::new("1").unwrap(),
                Digest::of(DigestAlgorithm::Sha256, format!("kind-{index}").as_bytes()),
            ),
            CatalogSource::new(
                SourceKind::Mcp,
                source_id,
                TrustDomain::new("fixture-trust").unwrap(),
            )
            .unwrap(),
            capability_kind,
            CatalogSchemas::new(SchemaProjectionSet::new(schema("input")), None),
            CatalogSearch::new("fixture kind", [format!("kind-{index}")]).unwrap(),
            SideEffects::new(EffectClass::WorkspaceRead, RetrySafety::Idempotent),
            CatalogAuthority::new([Grant::WorkspaceRead], Vec::<String>::new()).unwrap(),
            Availability::Available,
            ReliabilityStats::default(),
            LatencyStats::Unobserved,
            CostStats::Unobserved,
        )
        .unwrap()
    })
    .collect::<Vec<_>>();
    let snapshot = CatalogSnapshot::new(entries, DigestAlgorithm::Sha256).unwrap();
    assert_eq!(
        snapshot
            .entries()
            .iter()
            .map(|entry| entry.kind())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            CapabilityKind::Tool,
            CapabilityKind::Resource,
            CapabilityKind::ResourceTemplate,
            CapabilityKind::Prompt,
        ])
    );
    assert!(
        snapshot
            .entries()
            .iter()
            .all(|entry| entry.schemas().output().is_none())
    );
}

#[test]
fn catalog_represents_all_six_source_kinds() {
    let kinds = [
        SourceKind::Native,
        SourceKind::ProjectPlugin,
        SourceKind::Mcp,
        SourceKind::Acp,
        SourceKind::A2a,
        SourceKind::ProviderNative,
    ];
    let entries = kinds
        .into_iter()
        .enumerate()
        .map(|(index, kind)| {
            fixture(
                &format!("source-{index}"),
                kind,
                &format!("tool-{index}"),
                "1",
                "implementation",
            )
        })
        .collect::<Vec<_>>();
    let snapshot = CatalogSnapshot::new(entries, DigestAlgorithm::Sha256).unwrap();
    assert_eq!(
        snapshot
            .entries()
            .iter()
            .map(|entry| entry.source().kind())
            .collect::<BTreeSet<_>>(),
        kinds.into_iter().collect()
    );
}

#[test]
fn collision_key_excludes_source_and_implementation() {
    let first = fixture("one", SourceKind::Mcp, "same", "1", "first");
    let changed_source = fixture("two", SourceKind::Acp, "same", "1", "second");
    assert!(matches!(
        CatalogSnapshot::new([first.clone(), changed_source], DigestAlgorithm::Sha256),
        Err(CatalogError::IdentityCollision(_))
    ));
    let same_source = fixture("one", SourceKind::Mcp, "same", "1", "changed");
    assert!(matches!(
        CatalogSnapshot::new([first.clone(), same_source], DigestAlgorithm::Sha256),
        Err(CatalogError::DuplicateIdentity(_))
    ));
    let newer = fixture("two", SourceKind::Acp, "same", "2", "second");
    assert_eq!(
        CatalogSnapshot::new([first, newer], DigestAlgorithm::Sha256)
            .unwrap()
            .entries()
            .len(),
        2
    );
}

#[test]
fn source_ids_bind_to_exact_metadata_and_replacement_cannot_be_poisoned() {
    let trusted = fixture("shared", SourceKind::Mcp, "first", "1", "trusted");
    let conflicting = fixture_with(
        "shared",
        SourceKind::Acp,
        "second",
        "1",
        "conflicting",
        "fixture summary",
        "other-trust",
        "input",
        Availability::Available,
        ReliabilityStats::default(),
        LatencyStats::Unobserved,
        CostStats::Unobserved,
        EffectClass::WorkspaceRead,
        RetrySafety::Idempotent,
        [Grant::WorkspaceRead],
        ["scope.read"],
    );
    assert_eq!(
        CatalogSnapshot::new(
            [trusted.clone(), conflicting.clone()],
            DigestAlgorithm::Sha256
        )
        .unwrap_err(),
        CatalogError::SourceConflict
    );

    let old = CatalogSnapshot::new([trusted.clone()], DigestAlgorithm::Sha256).unwrap();
    assert_eq!(
        old.replace_source(conflicting.source(), [conflicting.clone()])
            .unwrap_err(),
        CatalogError::SourceConflict
    );
    assert_eq!(old.entries()[0].identity(), trusted.identity());
    assert_eq!(
        old.replace_source(trusted.source(), [conflicting.clone()])
            .unwrap_err(),
        CatalogError::SourceMismatch
    );
    assert_eq!(old.entries()[0].source(), trusted.source());

    let empty = old
        .replace_source(trusted.source(), std::iter::empty())
        .unwrap();
    assert!(empty.entries().is_empty());
    assert_eq!(
        empty
            .replace_source(conflicting.source(), std::iter::empty())
            .unwrap_err(),
        CatalogError::SourceConflict
    );
}

#[test]
fn digest_is_order_independent_and_covers_every_metadata_group() {
    let first = fixture("one", SourceKind::Mcp, "first", "1", "first");
    let second = fixture("two", SourceKind::Acp, "second", "1", "second");
    let ordered =
        CatalogSnapshot::new([first.clone(), second.clone()], DigestAlgorithm::Sha256).unwrap();
    let reversed = CatalogSnapshot::new([second, first], DigestAlgorithm::Sha256).unwrap();
    assert_eq!(ordered.digest(), reversed.digest());
    let baseline = CatalogSnapshot::new(
        [fixture("one", SourceKind::Mcp, "first", "1", "first")],
        DigestAlgorithm::Sha256,
    )
    .unwrap();

    let variants = [
        fixture("one", SourceKind::Mcp, "first", "2", "first"),
        fixture_with(
            "one",
            SourceKind::ProjectPlugin,
            "first",
            "1",
            "first",
            "fixture summary",
            "other-trust",
            "input",
            Availability::Available,
            ReliabilityStats::default(),
            LatencyStats::Unobserved,
            CostStats::Unobserved,
            EffectClass::WorkspaceRead,
            RetrySafety::Idempotent,
            [Grant::WorkspaceRead],
            ["scope.read"],
        ),
        fixture_with(
            "one",
            SourceKind::Mcp,
            "first",
            "1",
            "first",
            "fixture summary",
            "fixture-trust",
            "changed schema",
            Availability::Available,
            ReliabilityStats::default(),
            LatencyStats::Unobserved,
            CostStats::Unobserved,
            EffectClass::WorkspaceRead,
            RetrySafety::Idempotent,
            [Grant::WorkspaceRead],
            ["scope.read"],
        ),
        fixture_with(
            "one",
            SourceKind::Mcp,
            "first",
            "1",
            "first",
            "changed summary",
            "fixture-trust",
            "input",
            Availability::Available,
            ReliabilityStats::default(),
            LatencyStats::Unobserved,
            CostStats::Unobserved,
            EffectClass::WorkspaceRead,
            RetrySafety::Idempotent,
            [Grant::WorkspaceRead],
            ["scope.read"],
        ),
        fixture(
            "one",
            SourceKind::Mcp,
            "first",
            "1",
            "changed implementation",
        ),
        fixture_with(
            "one",
            SourceKind::Mcp,
            "first",
            "1",
            "first",
            "fixture summary",
            "fixture-trust",
            "input",
            Availability::Available,
            ReliabilityStats::default(),
            LatencyStats::Unobserved,
            CostStats::Unobserved,
            EffectClass::WorkspaceWrite,
            RetrySafety::NonIdempotent,
            [Grant::WorkspaceWrite],
            ["scope.read"],
        ),
        fixture_with(
            "one",
            SourceKind::Mcp,
            "first",
            "1",
            "first",
            "fixture summary",
            "fixture-trust",
            "input",
            Availability::Available,
            ReliabilityStats::default(),
            LatencyStats::Unobserved,
            CostStats::Unobserved,
            EffectClass::WorkspaceRead,
            RetrySafety::Idempotent,
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
            ["scope.write"],
        ),
        fixture_with(
            "one",
            SourceKind::Mcp,
            "first",
            "1",
            "first",
            "fixture summary",
            "fixture-trust",
            "input",
            Availability::Degraded,
            ReliabilityStats::new(1, 0, 1, 0, 0).unwrap(),
            LatencyStats::measured(1, 10, 10, 10).unwrap(),
            CostStats::measured(
                1,
                MoneyMicros::new("USD", 1).unwrap(),
                MoneyMicros::new("USD", 1).unwrap(),
                MoneyMicros::new("USD", 1).unwrap(),
            )
            .unwrap(),
            EffectClass::WorkspaceRead,
            RetrySafety::Idempotent,
            [Grant::WorkspaceRead],
            ["scope.read"],
        ),
    ];
    for variant in variants {
        assert_ne!(
            baseline.digest(),
            CatalogSnapshot::new([variant], DigestAlgorithm::Sha256)
                .unwrap()
                .digest()
        );
    }
}

#[test]
fn compositional_schema_digest_covers_exact_forms_and_projection_metadata() {
    fn ingest(source: &[u8], dialect: &str, docs: &[u8]) -> NormalizedSchema {
        NormalizedSchema::ingest(source, dialect, docs, DigestAlgorithm::Sha256).unwrap()
    }

    fn schemas(input: NormalizedSchema) -> CatalogSchemas {
        CatalogSchemas::new(
            SchemaProjectionSet::new(input),
            Some(SchemaProjectionSet::new(schema("output"))),
        )
    }

    fn entry_digest(base: &CatalogEntry, schemas: CatalogSchemas) -> Digest {
        rebuild(
            base,
            schemas,
            base.side_effects(),
            base.authority().clone(),
            base.reliability(),
            base.latency(),
            base.cost().clone(),
        )
        .unwrap()
        .digest()
    }

    let base = fixture("one", SourceKind::Mcp, "first", "1", "first");
    let compact = br#"{"title":"input","type":"object"}"#;
    let pretty = b"{\n  \"title\": \"input\",\n  \"type\": \"object\"\n}";
    let baseline_schema = ingest(compact, "fixture-dialect", b"docs");
    let baseline_normalized = baseline_schema.source().normalized_bytes().to_vec();
    let baseline = entry_digest(&base, schemas(baseline_schema));
    let source_changed = ingest(pretty, "fixture-dialect", b"docs");
    assert_eq!(
        source_changed.source().normalized_bytes(),
        baseline_normalized
    );
    assert_ne!(baseline, entry_digest(&base, schemas(source_changed)));
    assert_ne!(
        baseline,
        entry_digest(&base, schemas(ingest(compact, "other-dialect", b"docs")))
    );
    assert_ne!(
        baseline,
        entry_digest(
            &base,
            schemas(ingest(compact, "fixture-dialect", b"other docs"))
        )
    );
    assert_ne!(
        baseline,
        entry_digest(
            &base,
            schemas(ingest(
                br#"{"title":"input","type":"string"}"#,
                "fixture-dialect",
                b"docs"
            ))
        )
    );

    let unprojected = schemas(schema("input"));
    let unprojected_digest = entry_digest(&base, unprojected);
    let mut projected = SchemaProjectionSet::new(schema("input"));
    let profile = ProjectionProfile::new(
        ProjectionTarget::new("fixture", "model", "adapter", 1).unwrap(),
        JSON_SCHEMA_2020_12,
        ["$schema", "title", "type"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        serde_json::Value::Bool(true),
        1024 * 1024,
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    projected.project(&profile).unwrap();
    assert_ne!(
        unprojected_digest,
        entry_digest(
            &base,
            CatalogSchemas::new(projected, Some(SchemaProjectionSet::new(schema("output"))))
        )
    );
}

#[test]
fn replace_source_is_atomic_and_updates_operational_metadata() {
    let old_entry = fixture("one", SourceKind::Mcp, "first", "1", "first");
    let other_entry = fixture("other", SourceKind::Acp, "second", "1", "other");
    let old =
        CatalogSnapshot::new([old_entry.clone(), other_entry], DigestAlgorithm::Sha256).unwrap();
    let replacement = fixture_with(
        "one",
        SourceKind::Mcp,
        "first",
        "1",
        "first",
        "fixture summary",
        "fixture-trust",
        "input",
        Availability::Unavailable,
        ReliabilityStats::new(2, 1, 1, 0, 0).unwrap(),
        LatencyStats::measured(2, 10, 20, 30).unwrap(),
        CostStats::Unobserved,
        EffectClass::WorkspaceRead,
        RetrySafety::Idempotent,
        [Grant::WorkspaceRead],
        ["scope.read"],
    );
    let source = old_entry.source().clone();
    let new = old.replace_source(&source, [replacement]).unwrap();
    assert_eq!(old.entries()[0].availability(), Availability::Available);
    assert_eq!(new.entries()[0].availability(), Availability::Unavailable);
    assert_ne!(old.digest(), new.digest());

    let colliding = fixture("one", SourceKind::Mcp, "second", "1", "replacement");
    assert!(matches!(
        old.replace_source(&source, [colliding]),
        Err(CatalogError::IdentityCollision(_))
    ));
    assert_eq!(old.entries()[0].availability(), Availability::Available);

    let consumed = Rc::new(Cell::new(0));
    let observed = Rc::clone(&consumed);
    let replacements = std::iter::from_fn(move || {
        let index = observed.get();
        observed.set(index + 1);
        Some(fixture(
            "one",
            SourceKind::Mcp,
            "replacement",
            &index.to_string(),
            "implementation",
        ))
    });
    assert_eq!(
        old.replace_source(&source, replacements).unwrap_err(),
        CatalogError::LimitExceeded(CatalogLimit::Entries)
    );
    assert_eq!(consumed.get(), MAX_CATALOG_ENTRIES);
    assert_eq!(old.entries().len(), 2);
    assert_eq!(old.entries()[0].availability(), Availability::Available);
}

#[test]
fn catalog_bounds_count_raw_items_before_deduplication() {
    assert!(matches!(
        CatalogSearch::new("x".repeat(MAX_SUMMARY_BYTES + 1), ["x"]),
        Err(CatalogError::TextTooLong("summary"))
    ));
    assert!(matches!(
        TrustDomain::new("x".repeat(MAX_CATALOG_TEXT_BYTES + 1)),
        Err(CatalogError::TextTooLong("trust domain"))
    ));
    assert!(matches!(
        CatalogSource::new(
            SourceKind::Mcp,
            CapabilitySource::new("x".repeat(MAX_CATALOG_TEXT_BYTES + 1)).unwrap(),
            TrustDomain::new("trust").unwrap(),
        ),
        Err(CatalogError::TextTooLong("source"))
    ));
    assert!(matches!(
        CatalogSearch::new("summary", ["x".repeat(MAX_CATALOG_TEXT_BYTES + 1)]),
        Err(CatalogError::TextTooLong("search term"))
    ));
    assert!(matches!(
        CatalogAuthority::new([], ["x".repeat(MAX_CATALOG_TEXT_BYTES + 1)]),
        Err(CatalogError::TextTooLong("auth scope"))
    ));

    let base = fixture("identity", SourceKind::Mcp, "tool", "1", "implementation");
    for (namespace, name, version, field) in [
        (
            "x".repeat(MAX_CATALOG_TEXT_BYTES + 1),
            "tool".to_owned(),
            "1".to_owned(),
            "namespace",
        ),
        (
            "fixture.tools".to_owned(),
            "x".repeat(MAX_CATALOG_TEXT_BYTES + 1),
            "1".to_owned(),
            "name",
        ),
        (
            "fixture.tools".to_owned(),
            "tool".to_owned(),
            "x".repeat(MAX_CATALOG_TEXT_BYTES + 1),
            "version",
        ),
    ] {
        let identity = CapabilityIdentity::new(
            base.identity().source().clone(),
            CapabilityNamespace::new(namespace).unwrap(),
            CapabilityName::new(name).unwrap(),
            CapabilityVersion::new(version).unwrap(),
            base.identity().implementation_digest(),
        );
        assert!(matches!(
            CatalogEntry::new(
                identity.clone(),
                base.source().clone(),
                base.kind(),
                base.schemas().clone(),
                base.search().clone(),
                base.side_effects(),
                base.authority().clone(),
                base.availability(),
                base.reliability(),
                base.latency(),
                base.cost().clone(),
            ),
            Err(CatalogError::TextTooLong(rejected)) if rejected == field
        ));
    }

    let consumed = Rc::new(Cell::new(0));
    let observed = Rc::clone(&consumed);
    let duplicate_terms = std::iter::from_fn(move || {
        observed.set(observed.get() + 1);
        Some("same")
    });
    assert_eq!(
        CatalogSearch::new("summary", duplicate_terms).unwrap_err(),
        CatalogError::LimitExceeded(CatalogLimit::SearchTerms)
    );
    assert_eq!(consumed.get(), MAX_SEARCH_TERMS + 1);

    let consumed = Rc::new(Cell::new(0));
    let observed = Rc::clone(&consumed);
    let duplicate_scopes = std::iter::from_fn(move || {
        observed.set(observed.get() + 1);
        Some("same")
    });
    assert_eq!(
        CatalogAuthority::new([], duplicate_scopes).unwrap_err(),
        CatalogError::LimitExceeded(CatalogLimit::AuthScopes)
    );
    assert_eq!(consumed.get(), MAX_AUTH_SCOPES + 1);

    let consumed = Rc::new(Cell::new(0));
    let observed = Rc::clone(&consumed);
    let entries = std::iter::from_fn(move || {
        let index = observed.get();
        observed.set(index + 1);
        Some(fixture(
            "bounded",
            SourceKind::Mcp,
            "tool",
            &index.to_string(),
            "implementation",
        ))
    });
    assert!(matches!(
        CatalogSnapshot::new(entries, DigestAlgorithm::Sha256),
        Err(CatalogError::LimitExceeded(CatalogLimit::Entries))
    ));
    assert_eq!(consumed.get(), MAX_CATALOG_ENTRIES + 1);

    let duplicate = fixture("duplicate", SourceKind::Mcp, "tool", "1", "implementation");
    assert!(matches!(
        CatalogSnapshot::new(std::iter::repeat(duplicate), DigestAlgorithm::Sha256),
        Err(CatalogError::DuplicateIdentity(_))
    ));

    let sources = (0..=MAX_CATALOG_SOURCES).map(|index| {
        fixture(
            &format!("source-{index}"),
            SourceKind::Mcp,
            &format!("tool-{index}"),
            "1",
            "implementation",
        )
    });
    assert!(matches!(
        CatalogSnapshot::new(sources, DigestAlgorithm::Sha256),
        Err(CatalogError::LimitExceeded(CatalogLimit::Sources))
    ));
}

#[test]
fn catalog_payload_limits_reject_before_publication() {
    let base = fixture("bounded", SourceKind::Mcp, "tool", "1", "implementation");
    let large_source = serde_json::to_vec(&vec![serde_json::Value::Null; 99_999]).unwrap();
    let large_input = NormalizedSchema::ingest(
        &large_source,
        "fixture-dialect",
        b"docs",
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    let large_output = NormalizedSchema::ingest(
        &large_source,
        "fixture-dialect",
        b"docs",
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    assert_eq!(
        rebuild(
            &base,
            CatalogSchemas::new(
                SchemaProjectionSet::new(large_input),
                Some(SchemaProjectionSet::new(large_output)),
            ),
            base.side_effects(),
            base.authority().clone(),
            base.reliability(),
            base.latency(),
            base.cost().clone(),
        )
        .unwrap_err(),
        CatalogError::LimitExceeded(CatalogLimit::EntryPayloadBytes)
    );

    let terms = (0..MAX_SEARCH_TERMS)
        .map(|index| format!("{index:02}{}", "t".repeat(MAX_CATALOG_TEXT_BYTES - 2)))
        .collect::<Vec<_>>();
    let scopes = (0..MAX_AUTH_SCOPES)
        .map(|index| format!("{index:02}{}", "s".repeat(MAX_CATALOG_TEXT_BYTES - 2)))
        .collect::<Vec<_>>();
    let search = CatalogSearch::new("x".repeat(MAX_SUMMARY_BYTES), terms).unwrap();
    let authority = CatalogAuthority::new([Grant::WorkspaceRead], scopes).unwrap();
    let source = base.source().clone();
    let schemas = base.schemas().clone();
    let make_entry = move |index: usize| {
        CatalogEntry::new(
            CapabilityIdentity::new(
                source.id().clone(),
                CapabilityNamespace::new("fixture.tools").unwrap(),
                CapabilityName::new("retained").unwrap(),
                CapabilityVersion::new(format!("{index:04}")).unwrap(),
                Digest::of(DigestAlgorithm::Blake3, b"implementation"),
            ),
            source.clone(),
            CapabilityKind::Tool,
            schemas.clone(),
            search.clone(),
            SideEffects::new(EffectClass::WorkspaceRead, RetrySafety::Idempotent),
            authority.clone(),
            Availability::Available,
            ReliabilityStats::default(),
            LatencyStats::Unobserved,
            CostStats::Unobserved,
        )
        .unwrap()
    };
    let payload_per_entry = make_entry(0).payload_bytes();
    assert!(payload_per_entry <= MAX_CATALOG_ENTRY_PAYLOAD_BYTES);
    let expected_consumed = MAX_CATALOG_PAYLOAD_BYTES / payload_per_entry + 1;
    assert!(expected_consumed <= MAX_CATALOG_ENTRIES);
    let consumed = Rc::new(Cell::new(0));
    let observed = Rc::clone(&consumed);
    let entries = std::iter::from_fn(move || {
        let index = observed.get();
        observed.set(index + 1);
        Some(make_entry(index))
    });
    assert_eq!(
        CatalogSnapshot::new(entries, DigestAlgorithm::Sha256).unwrap_err(),
        CatalogError::LimitExceeded(CatalogLimit::PayloadBytes)
    );
    assert_eq!(consumed.get(), expected_consumed);
}

#[test]
fn malformed_statistics_currency_and_source_reject() {
    assert_eq!(
        ReliabilityStats::new(2, 1, 0, 0, 0).unwrap_err(),
        CatalogError::InvalidStatistics
    );
    assert_eq!(
        LatencyStats::measured(0, 0, 0, 0).unwrap_err(),
        CatalogError::InvalidStatistics
    );
    assert_eq!(
        LatencyStats::measured(2, 20, 10, 30).unwrap_err(),
        CatalogError::InvalidStatistics
    );
    assert_eq!(
        CostStats::measured(
            1,
            MoneyMicros::new("USD", 1).unwrap(),
            MoneyMicros::new("EUR", 1).unwrap(),
            MoneyMicros::new("USD", 1).unwrap(),
        )
        .unwrap_err(),
        CatalogError::CurrencyMismatch
    );
    assert_eq!(
        CostStats::measured(
            1,
            MoneyMicros {
                currency: "usd".to_owned(),
                micros: 1,
            },
            MoneyMicros {
                currency: "usd".to_owned(),
                micros: 1,
            },
            MoneyMicros {
                currency: "usd".to_owned(),
                micros: 1,
            },
        )
        .unwrap_err(),
        CatalogError::InvalidCurrency
    );

    let entry = fixture("one", SourceKind::Mcp, "first", "1", "first");
    let identity = entry.identity().clone();
    let schemas = entry.schemas().clone();
    let bad_source = CatalogSource::new(
        SourceKind::Mcp,
        CapabilitySource::new("other").unwrap(),
        TrustDomain::new("trust").unwrap(),
    )
    .unwrap();
    assert!(matches!(
        CatalogEntry::new(
            identity.clone(),
            bad_source,
            entry.kind(),
            schemas.clone(),
            entry.search().clone(),
            entry.side_effects(),
            entry.authority().clone(),
            entry.availability(),
            entry.reliability(),
            entry.latency(),
            entry.cost().clone(),
        ),
        Err(CatalogError::SourceMismatch)
    ));
}

#[test]
fn entry_revalidates_direct_statistics_variants_and_authority() {
    let observed = fixture_with(
        "one",
        SourceKind::Mcp,
        "first",
        "1",
        "first",
        "fixture summary",
        "fixture-trust",
        "input",
        Availability::Available,
        ReliabilityStats::new(1, 1, 0, 0, 0).unwrap(),
        LatencyStats::Unobserved,
        CostStats::Unobserved,
        EffectClass::WorkspaceRead,
        RetrySafety::Idempotent,
        [Grant::WorkspaceRead],
        ["scope.read"],
    );
    let invalid_latency = LatencyStats::Measured {
        samples: 1,
        minimum_micros: 20,
        maximum_micros: 10,
        total_micros: 10,
    };
    assert_eq!(
        rebuild(
            &observed,
            observed.schemas().clone(),
            observed.side_effects(),
            observed.authority().clone(),
            observed.reliability(),
            invalid_latency,
            CostStats::Unobserved,
        )
        .unwrap_err(),
        CatalogError::InvalidStatistics
    );
    let too_many_samples = LatencyStats::Measured {
        samples: 2,
        minimum_micros: 1,
        maximum_micros: 1,
        total_micros: 2,
    };
    assert_eq!(
        rebuild(
            &observed,
            observed.schemas().clone(),
            observed.side_effects(),
            observed.authority().clone(),
            observed.reliability(),
            too_many_samples,
            CostStats::Unobserved,
        )
        .unwrap_err(),
        CatalogError::InvalidStatistics
    );

    for cost in [
        CostStats::Measured {
            samples: 1,
            minimum: MoneyMicros {
                currency: "usd".to_owned(),
                micros: 1,
            },
            maximum: MoneyMicros {
                currency: "usd".to_owned(),
                micros: 1,
            },
            total: MoneyMicros {
                currency: "usd".to_owned(),
                micros: 1,
            },
        },
        CostStats::Measured {
            samples: 1,
            minimum: MoneyMicros::new("USD", 1).unwrap(),
            maximum: MoneyMicros::new("EUR", 1).unwrap(),
            total: MoneyMicros::new("USD", 1).unwrap(),
        },
        CostStats::Measured {
            samples: 1,
            minimum: MoneyMicros::new("USD", 2).unwrap(),
            maximum: MoneyMicros::new("USD", 1).unwrap(),
            total: MoneyMicros::new("USD", 1).unwrap(),
        },
        CostStats::Measured {
            samples: 2,
            minimum: MoneyMicros::new("USD", 1).unwrap(),
            maximum: MoneyMicros::new("USD", 1).unwrap(),
            total: MoneyMicros::new("USD", 2).unwrap(),
        },
    ] {
        assert!(matches!(
            rebuild(
                &observed,
                observed.schemas().clone(),
                observed.side_effects(),
                observed.authority().clone(),
                observed.reliability(),
                LatencyStats::Unobserved,
                cost,
            ),
            Err(CatalogError::InvalidCurrency
                | CatalogError::CurrencyMismatch
                | CatalogError::InvalidStatistics)
        ));
    }

    assert_eq!(
        rebuild(
            &observed,
            observed.schemas().clone(),
            SideEffects::new(EffectClass::WorkspaceWrite, RetrySafety::NonIdempotent),
            CatalogAuthority::new([Grant::WorkspaceRead], Vec::<String>::new()).unwrap(),
            observed.reliability(),
            LatencyStats::Unobserved,
            CostStats::Unobserved,
        )
        .unwrap_err(),
        CatalogError::AuthorityMismatch
    );
}
