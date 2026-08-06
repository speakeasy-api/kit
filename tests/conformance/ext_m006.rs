use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, RwLock},
};

use kit::{
    agent::extensions::{
        CompatibilityRange, ContentDigest, ContractVersion, ExtensionIdentity, ExtensionVersion,
    },
    api::auth::{
        contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot},
        local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
    },
    api::service::WorkerStore,
    capabilities::extensions::{
        CAPABILITY_EXTENSION_HOST_VERSION, CapabilityExtensionRegistry, ExtensionContract,
        ExtensionKind, ExtensionMetadata, ExtensionProtocol, ExtensionRoute, ExtensionScope,
        ExtensionState, MAX_EXTENSION_ENTRIES, MAX_EXTENSION_ENTRIES_PER_PROJECT,
        MAX_EXTENSION_PROJECT_SNAPSHOT_BYTES, MAX_EXTENSION_SNAPSHOT_BYTES,
        MAX_EXTENSION_TEXT_BYTES, REGISTRY_FORMAT_VERSION, RegistrationOutcome, RegistryError,
        TrustClassification, built_in_contracts, canonical_schema_digest, implementation_merkle,
    },
    domain::{
        config::Grant,
        ids::{PrincipalId, ProjectId},
    },
    protocols::mcp::config::{mcp_lifecycle_schema_digest, mcp_runtime_implementation_digest},
    store::sqlite::append::ExtensionRegistryCommit,
    test_support,
};
use serde::{Deserialize, Serialize};

const UID: u32 = 906;

#[derive(Deserialize)]
struct Manifest {
    schema_version: u16,
    milestone: String,
    registry_format_version: u16,
    host_contract_version: ContractVersion,
    compatibility_range: CompatibilityRange,
    bounds: Bounds,
    trust_boundary: TrustBoundary,
    protocols: Protocols,
    extension_kinds: Vec<ExtensionKind>,
    built_ins: Vec<BuiltIn>,
    mcp_runtime: RuntimePin,
}

#[derive(Deserialize)]
struct Bounds {
    global_entries: usize,
    project_entries: usize,
    global_snapshot_bytes: usize,
    project_snapshot_bytes: usize,
    metadata_text_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustBoundary {
    in_process: String,
    restored_trusted: String,
    restored_untrusted: String,
    untrusted_in_process: String,
    dynamic_library_loading: String,
    out_of_process: String,
    authority: String,
    stdio_identity: StdioIdentity,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum StdioIdentity {
    ExactVerifiedExecutableSha256WithDescriptorOnlyLaunch,
}

#[derive(Deserialize)]
struct Protocols {
    mcp: ProtocolPin,
    acp: ProtocolPin,
    a2a: ProtocolPin,
    kit_plugin: KitPluginPin,
}

#[derive(Deserialize)]
struct ProtocolPin {
    version: String,
    pin: String,
}

#[derive(Deserialize)]
struct KitPluginPin {
    version: u16,
    scope: String,
}

#[derive(Deserialize)]
struct BuiltIn {
    kind: ExtensionKind,
    identity: String,
    version: String,
    trust: TrustClassification,
    route: ExtensionRoute,
    compatibility: CompatibilityRange,
    schema_digest: String,
    implementation_digest: String,
    schema_artifact: String,
    implementation_sources: Vec<String>,
}

#[derive(Deserialize)]
struct RuntimePin {
    protocol_version: String,
    schema_digest: String,
    implementation_digest: String,
    schema_artifact: String,
    implementation_sources: Vec<String>,
}

#[derive(Serialize)]
struct Snapshot<'a> {
    format_version: u16,
    revision: u64,
    principal_id: PrincipalId,
    project_id: ProjectId,
    entries: Vec<SnapshotEntry<'a>>,
}

#[derive(Serialize)]
struct SnapshotEntry<'a> {
    scope: ExtensionScope,
    contract: &'a ExtensionContract,
    state: ExtensionState,
}

fn manifest() -> Manifest {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/compatibility/ext-m006.yaml");
    serde_yaml::from_slice(&fs::read(&path).unwrap()).unwrap()
}

fn authenticate(principal: PrincipalId, project: ProjectId) -> AuthenticatedPrincipal {
    LocalPeerAuthenticator::new(BTreeMap::from([(
        UID,
        GrantSnapshot::new(
            principal,
            project,
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
        ),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(UID, 71, UID))
    .unwrap()
}

fn untrusted(index: usize, kind: ExtensionKind, project: ProjectId) -> ExtensionContract {
    ExtensionContract::untrusted(
        kind,
        ExtensionIdentity::parse(format!("third-party.ext-{index}")).unwrap(),
        ExtensionVersion::parse("1.0.0").unwrap(),
        ContentDigest::sha256(format!("schema-{index}-{project}").as_bytes()),
        ContentDigest::sha256(format!("implementation-{index}-{project}").as_bytes()),
        CompatibilityRange::new(
            CAPABILITY_EXTENSION_HOST_VERSION,
            ContractVersion::new(2, 0, 0),
        ),
        ExtensionProtocol::Mcp,
        format!("route-{index}"),
        ContentDigest::sha256(format!("profile-{index}").as_bytes()).to_string(),
        ExtensionMetadata::default(),
    )
    .unwrap()
}

fn digest_sources(paths: &[String]) -> ContentDigest {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let files = paths
        .iter()
        .map(|path| {
            let bytes = fs::read(root.join(path)).unwrap();
            (path.clone(), bytes)
        })
        .collect::<Vec<_>>();
    let borrowed = files
        .iter()
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
        .collect::<Vec<_>>();
    implementation_merkle(&borrowed)
}

#[test]
fn ext_m006_manifest_pins_bounds_protocols_and_nonpersistent_authority() {
    let manifest = manifest();
    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.milestone, "M006-W11");
    assert_eq!(manifest.registry_format_version, REGISTRY_FORMAT_VERSION);
    assert_eq!(
        manifest.host_contract_version,
        CAPABILITY_EXTENSION_HOST_VERSION
    );
    assert!(
        manifest
            .compatibility_range
            .contains(CAPABILITY_EXTENSION_HOST_VERSION)
    );
    assert_eq!(manifest.bounds.global_entries, MAX_EXTENSION_ENTRIES);
    assert_eq!(
        manifest.bounds.project_entries,
        MAX_EXTENSION_ENTRIES_PER_PROJECT
    );
    assert_eq!(
        manifest.bounds.global_snapshot_bytes,
        MAX_EXTENSION_SNAPSHOT_BYTES
    );
    assert_eq!(
        manifest.bounds.project_snapshot_bytes,
        MAX_EXTENSION_PROJECT_SNAPSHOT_BYTES
    );
    assert_eq!(
        manifest.bounds.metadata_text_bytes,
        MAX_EXTENSION_TEXT_BYTES
    );
    assert_eq!(manifest.extension_kinds, ExtensionKind::ALL);
    assert_eq!(
        manifest.trust_boundary.in_process,
        "build_attested_token_required"
    );
    assert_eq!(
        manifest.trust_boundary.restored_trusted,
        "inactive_until_build_reattested"
    );
    assert_eq!(
        manifest.trust_boundary.restored_untrusted,
        "fresh_broker_authorization_required"
    );
    assert_eq!(
        manifest.trust_boundary.untrusted_in_process,
        "rejected_at_validation"
    );
    assert_eq!(
        manifest.trust_boundary.dynamic_library_loading,
        "prohibited"
    );
    assert_eq!(
        manifest.trust_boundary.out_of_process,
        "current_exact_broker_decision_plus_sandbox"
    );
    assert_eq!(manifest.trust_boundary.authority, "broker_kernel_only");
    assert_eq!(
        manifest.trust_boundary.stdio_identity,
        StdioIdentity::ExactVerifiedExecutableSha256WithDescriptorOnlyLaunch
    );
    assert_eq!(
        (
            manifest.protocols.mcp.version.as_str(),
            manifest.protocols.mcp.pin.as_str()
        ),
        ("2025-11-25", "agentkit_mcp::PINNED_PROTOCOL_VERSION")
    );
    assert_eq!(
        (
            manifest.protocols.acp.version.as_str(),
            manifest.protocols.acp.pin.as_str()
        ),
        ("v1", "protocol.acp.wire")
    );
    assert_eq!(
        (
            manifest.protocols.a2a.version.as_str(),
            manifest.protocols.a2a.pin.as_str()
        ),
        ("1.0.0", "protocol.a2a.wire")
    );
    assert_eq!(manifest.protocols.kit_plugin.version, 1);
    assert_eq!(
        manifest.protocols.kit_plugin.scope,
        "narrow_sandboxed_extension_route"
    );
}

#[test]
fn ext_m006_stdio_identity_is_required_and_cannot_weaken_to_path_identity() {
    const FIELD: &str =
        "  stdio_identity: exact_verified_executable_sha256_with_descriptor_only_launch\n";
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/compatibility/ext-m006.yaml");
    let source = fs::read_to_string(path).unwrap();
    for invalid in [
        source.replace(FIELD, ""),
        source.replace("stdio_identity", "stdio_identitiy"),
        source.replace(
            "exact_verified_executable_sha256_with_descriptor_only_launch",
            "verified_executable_path_sha256",
        ),
    ] {
        assert!(serde_yaml::from_str::<Manifest>(&invalid).is_err());
    }
}

#[test]
fn ext_m006_built_in_and_mcp_pins_hash_exact_artifacts_and_runtime_sources() {
    let manifest = manifest();
    let pins = manifest
        .built_ins
        .into_iter()
        .map(|pin| (pin.identity.clone(), pin))
        .collect::<BTreeMap<_, _>>();
    for contract in built_in_contracts() {
        let pin = &pins[contract.identity().as_str()];
        assert_eq!(contract.kind(), pin.kind);
        assert_eq!(contract.version().as_str(), pin.version);
        assert_eq!(contract.trust(), pin.trust);
        assert_eq!(contract.route(), &pin.route);
        assert_eq!(contract.compatibility(), pin.compatibility);
        assert_eq!(contract.schema_digest().as_str(), pin.schema_digest);
        assert_eq!(
            contract.implementation_digest().as_str(),
            pin.implementation_digest
        );
        assert_eq!(
            canonical_schema_digest(
                &fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(&pin.schema_artifact))
                    .unwrap()
            )
            .as_str(),
            pin.schema_digest
        );
        assert_eq!(
            digest_sources(&pin.implementation_sources).as_str(),
            pin.implementation_digest
        );
    }
    assert_eq!(manifest.mcp_runtime.protocol_version, "2025-11-25");
    assert_eq!(
        mcp_lifecycle_schema_digest().as_str(),
        manifest.mcp_runtime.schema_digest
    );
    assert_eq!(
        mcp_runtime_implementation_digest().as_str(),
        manifest.mcp_runtime.implementation_digest
    );
    assert_eq!(
        canonical_schema_digest(
            &fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR")).join(&manifest.mcp_runtime.schema_artifact)
            )
            .unwrap()
        )
        .as_str(),
        manifest.mcp_runtime.schema_digest
    );
    assert_eq!(
        digest_sources(&manifest.mcp_runtime.implementation_sources).as_str(),
        manifest.mcp_runtime.implementation_digest
    );
}

#[test]
fn ext_m006_project_snapshot_restores_contracts_but_never_live_trust() {
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let authenticated = authenticate(principal, project);
    let scope = ExtensionScope::new(principal, project);
    let mut registry = CapabilityExtensionRegistry::default();
    let contract = untrusted(0, ExtensionKind::McpServer, project);
    registry
        .register_untrusted(&authenticated, project, contract.clone())
        .unwrap();
    let bytes = registry
        .canonical_project_bytes(&authenticated, project)
        .unwrap();
    let restored = CapabilityExtensionRegistry::from_project_bytes(&authenticated, &bytes).unwrap();
    assert_eq!(
        restored
            .canonical_project_bytes(&authenticated, project)
            .unwrap(),
        bytes
    );
    assert_eq!(
        restored.project_digest(&authenticated, project).unwrap(),
        registry.project_digest(&authenticated, project).unwrap()
    );
    assert_eq!(
        restored
            .entries_for_project(&authenticated, project)
            .unwrap()
            .len(),
        1
    );

    let trusted = built_in_contracts().into_iter().next().unwrap();
    let trusted_bytes = serde_json::to_vec(&Snapshot {
        format_version: REGISTRY_FORMAT_VERSION,
        revision: 0,
        principal_id: principal,
        project_id: project,
        entries: vec![SnapshotEntry {
            scope,
            contract: &trusted,
            state: ExtensionState::Active,
        }],
    })
    .unwrap();
    let restored_trusted =
        CapabilityExtensionRegistry::from_project_bytes(&authenticated, &trusted_bytes).unwrap();
    assert!(
        !restored_trusted
            .trusted_attested(&authenticated, project, &trusted.reference())
            .unwrap()
    );

    let other_project = ProjectId::generate().unwrap();
    assert!(matches!(
        restored.entries_for_project(&authenticated, other_project),
        Err(RegistryError::ProjectUnauthorized)
    ));
    assert!(matches!(
        CapabilityExtensionRegistry::from_project_bytes(
            &authenticated,
            &serde_json::to_vec(&Snapshot {
                format_version: REGISTRY_FORMAT_VERSION,
                revision: 0,
                principal_id: principal,
                project_id: other_project,
                entries: vec![],
            })
            .unwrap()
        ),
        Err(RegistryError::ProjectUnauthorized)
    ));
}

#[test]
fn ext_m006_public_contract_construction_cannot_select_trust_or_in_process() {
    let project = ProjectId::generate().unwrap();
    let contract = untrusted(0, ExtensionKind::McpServer, project);
    assert_eq!(contract.trust(), TrustClassification::Untrusted);
    assert!(matches!(
        contract.route(),
        ExtensionRoute::OutOfProcess {
            protocol: ExtensionProtocol::Mcp,
            ..
        }
    ));
}

#[test]
fn ext_m006_mutations_are_bounded_idempotent_and_restore_rejects_bad_graphs() {
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let authenticated = authenticate(principal, project);
    let scope = ExtensionScope::new(principal, project);
    let mut registry = CapabilityExtensionRegistry::default();
    let first = untrusted(0, ExtensionKind::McpServer, project);
    let second = untrusted(1, ExtensionKind::McpServer, project);
    let third = untrusted(2, ExtensionKind::McpServer, project);
    assert_eq!(
        registry
            .register_untrusted(&authenticated, project, first.clone())
            .unwrap(),
        RegistrationOutcome::Inserted
    );
    assert_eq!(
        registry
            .register_untrusted(&authenticated, project, first.clone())
            .unwrap(),
        RegistrationOutcome::Existing
    );
    registry
        .register_untrusted(&authenticated, project, second.clone())
        .unwrap();
    registry
        .register_untrusted(&authenticated, project, third.clone())
        .unwrap();
    assert!(matches!(
        registry.supersede(
            &authenticated,
            project,
            &first.reference(),
            &first.reference()
        ),
        Err(RegistryError::SelfSupersede)
    ));
    registry
        .supersede(
            &authenticated,
            project,
            &first.reference(),
            &second.reference(),
        )
        .unwrap();
    registry
        .supersede(
            &authenticated,
            project,
            &first.reference(),
            &second.reference(),
        )
        .unwrap();
    registry
        .revoke(&authenticated, project, &third.reference())
        .unwrap();
    registry
        .revoke(&authenticated, project, &third.reference())
        .unwrap();

    let dangling = serde_json::to_vec(&Snapshot {
        format_version: REGISTRY_FORMAT_VERSION,
        revision: 0,
        principal_id: principal,
        project_id: project,
        entries: vec![SnapshotEntry {
            scope,
            contract: &first,
            state: ExtensionState::Superseded {
                by: second.reference(),
            },
        }],
    })
    .unwrap();
    assert!(matches!(
        CapabilityExtensionRegistry::from_project_bytes(&authenticated, &dangling),
        Err(RegistryError::InvalidLifecycleGraph)
    ));

    let mut bounded = CapabilityExtensionRegistry::default();
    for index in 0..MAX_EXTENSION_ENTRIES_PER_PROJECT {
        bounded
            .register_untrusted(
                &authenticated,
                project,
                untrusted(index, ExtensionKind::McpServer, project),
            )
            .unwrap();
    }
    assert!(
        matches!(bounded.register_untrusted(&authenticated, project, untrusted(MAX_EXTENSION_ENTRIES_PER_PROJECT, ExtensionKind::McpServer, project)), Err(RegistryError::ProjectLimitExceeded(id)) if id == project)
    );
}

#[test]
fn ext_m006_shared_registry_is_durable_and_principal_isolated() {
    let principal = PrincipalId::generate().unwrap();
    let other_principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let authenticated = authenticate(principal, project);
    let other = authenticate(other_principal, project);
    let database =
        std::env::temp_dir().join(format!("kit-ext-registry-{}-{project}", std::process::id()));
    let service = test_support::open_service_store(&database).unwrap();
    let mut store = service.worker_append_store().unwrap();
    let shared = Arc::new(RwLock::new(CapabilityExtensionRegistry::default()));
    let contract = untrusted(0, ExtensionKind::McpServer, project);
    CapabilityExtensionRegistry::register_untrusted_durable(
        &shared,
        &authenticated,
        project,
        contract.clone(),
        &mut store,
    )
    .unwrap();
    let scope = ExtensionScope::new(principal, project);
    assert!(
        shared
            .read()
            .unwrap()
            .ensure_untrusted_active(scope, &contract.reference())
            .is_ok()
    );
    assert_eq!(
        shared
            .read()
            .unwrap()
            .entries_for_project(&other, project)
            .unwrap()
            .len(),
        0
    );
    assert!(matches!(
        shared
            .read()
            .unwrap()
            .contract(&other, project, &contract.reference()),
        Err(RegistryError::UnknownExtension(_))
    ));

    let snapshots = store.extension_registry_snapshots().unwrap();
    assert_eq!(snapshots.len(), 1);
    assert!(matches!(
        CapabilityExtensionRegistry::from_project_bytes(&other, &snapshots[0].1),
        Err(RegistryError::ProjectUnauthorized)
    ));
    let restored =
        CapabilityExtensionRegistry::from_project_bytes(&authenticated, &snapshots[0].1).unwrap();
    assert!(
        restored
            .ensure_untrusted_active(scope, &contract.reference())
            .is_ok()
    );
    let shared = Arc::new(RwLock::new(restored));
    for index in 1..MAX_EXTENSION_ENTRIES_PER_PROJECT {
        CapabilityExtensionRegistry::register_untrusted_durable(
            &shared,
            &authenticated,
            project,
            untrusted(index, ExtensionKind::McpServer, project),
            &mut store,
        )
        .unwrap();
    }
    assert!(matches!(
        CapabilityExtensionRegistry::register_untrusted_durable(
            &shared,
            &authenticated,
            project,
            untrusted(
                MAX_EXTENSION_ENTRIES_PER_PROJECT,
                ExtensionKind::McpServer,
                project
            ),
            &mut store,
        ),
        Err(RegistryError::ProjectLimitExceeded(id)) if id == project
    ));

    CapabilityExtensionRegistry::revoke_durable(
        &shared,
        &authenticated,
        project,
        &contract.reference(),
        &mut store,
    )
    .unwrap();
    assert!(matches!(
        shared
            .read()
            .unwrap()
            .ensure_untrusted_active(scope, &contract.reference()),
        Err(RegistryError::Revoked(_))
    ));
    let snapshots = store.extension_registry_snapshots().unwrap();
    let restored =
        CapabilityExtensionRegistry::from_project_bytes(&authenticated, &snapshots[0].1).unwrap();
    assert!(matches!(
        restored.ensure_untrusted_active(scope, &contract.reference()),
        Err(RegistryError::Revoked(_))
    ));
    drop(store);
    drop(service);
    let _ = fs::remove_file(database);
}

#[test]
fn ext_m006_project_quota_is_isolated_by_principal() {
    let project = ProjectId::generate().unwrap();
    let first_principal = PrincipalId::generate().unwrap();
    let second_principal = PrincipalId::generate().unwrap();
    let first = authenticate(first_principal, project);
    let second = authenticate(second_principal, project);
    let mut registry = CapabilityExtensionRegistry::default();

    for index in 0..MAX_EXTENSION_ENTRIES_PER_PROJECT {
        registry
            .register_untrusted(
                &first,
                project,
                untrusted(index, ExtensionKind::McpServer, project),
            )
            .unwrap();
        registry
            .register_untrusted(
                &second,
                project,
                untrusted(
                    index + MAX_EXTENSION_ENTRIES_PER_PROJECT,
                    ExtensionKind::McpServer,
                    project,
                ),
            )
            .unwrap();
    }

    assert_eq!(
        registry.entries_for_project(&first, project).unwrap().len(),
        MAX_EXTENSION_ENTRIES_PER_PROJECT
    );
    assert_eq!(
        registry
            .entries_for_project(&second, project)
            .unwrap()
            .len(),
        MAX_EXTENSION_ENTRIES_PER_PROJECT
    );
}

#[test]
fn ext_m006_stale_registry_owner_merges_without_resurrecting_revoke() {
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let authenticated = authenticate(principal, project);
    let database = std::env::temp_dir().join(format!(
        "kit-ext-registry-cas-{}-{project}",
        std::process::id()
    ));
    let service = test_support::open_service_store(&database).unwrap();
    let mut first_store = service.worker_append_store().unwrap();
    let mut stale_register_store = service.worker_append_store().unwrap();
    let mut stale_resolve_store = service.worker_append_store().unwrap();
    let mut stale_launch_store = service.worker_append_store().unwrap();
    let first_contract = untrusted(0, ExtensionKind::McpServer, project);
    let second_contract = untrusted(1, ExtensionKind::McpServer, project);
    let first_owner = Arc::new(RwLock::new(CapabilityExtensionRegistry::default()));
    CapabilityExtensionRegistry::register_untrusted_durable(
        &first_owner,
        &authenticated,
        project,
        first_contract.clone(),
        &mut first_store,
    )
    .unwrap();
    let stale_bytes = first_store.extension_registry_snapshots().unwrap()[0]
        .1
        .clone();
    let stale_register_owner = Arc::new(RwLock::new(
        CapabilityExtensionRegistry::from_project_bytes(&authenticated, &stale_bytes).unwrap(),
    ));
    let stale_resolve_owner = Arc::new(RwLock::new(
        CapabilityExtensionRegistry::from_project_bytes(&authenticated, &stale_bytes).unwrap(),
    ));
    let stale_launch_owner = Arc::new(RwLock::new(
        CapabilityExtensionRegistry::from_project_bytes(&authenticated, &stale_bytes).unwrap(),
    ));

    CapabilityExtensionRegistry::revoke_durable(
        &first_owner,
        &authenticated,
        project,
        &first_contract.reference(),
        &mut first_store,
    )
    .unwrap();
    assert!(matches!(
        CapabilityExtensionRegistry::register_untrusted_durable(
            &stale_register_owner,
            &authenticated,
            project,
            first_contract.clone(),
            &mut stale_register_store,
        ),
        Err(RegistryError::ContractConflict(_))
    ));
    assert!(matches!(
        CapabilityExtensionRegistry::ensure_untrusted_active_durable(
            &stale_resolve_owner,
            ExtensionScope::new(principal, project),
            &first_contract.reference(),
            &mut stale_resolve_store,
        ),
        Err(RegistryError::Revoked(_))
    ));
    assert!(matches!(
        CapabilityExtensionRegistry::ensure_untrusted_active_durable(
            &stale_launch_owner,
            ExtensionScope::new(principal, project),
            &first_contract.reference(),
            &mut stale_launch_store,
        ),
        Err(RegistryError::Revoked(_))
    ));

    CapabilityExtensionRegistry::register_untrusted_durable(
        &stale_register_owner,
        &authenticated,
        project,
        second_contract.clone(),
        &mut stale_register_store,
    )
    .unwrap();

    let snapshots = stale_register_store.extension_registry_snapshots().unwrap();
    assert_eq!(snapshots[0].0, 3);
    let restored =
        CapabilityExtensionRegistry::from_project_bytes(&authenticated, &snapshots[0].1).unwrap();
    let scope = ExtensionScope::new(principal, project);
    assert!(matches!(
        restored.ensure_untrusted_active(scope, &first_contract.reference()),
        Err(RegistryError::Revoked(_))
    ));
    restored
        .ensure_untrusted_active(scope, &second_contract.reference())
        .unwrap();

    drop(stale_launch_store);
    drop(stale_resolve_store);
    drop(stale_register_store);
    drop(first_store);
    drop(service);
    let _ = fs::remove_file(database);
}

#[test]
fn ext_m006_concurrent_scopes_share_global_quota_revision() {
    let database = std::env::temp_dir().join(format!(
        "kit-ext-registry-global-cas-{}-{}",
        std::process::id(),
        ProjectId::generate().unwrap()
    ));
    let service = test_support::open_service_store(&database).unwrap();
    drop(service);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let threads = (0..2)
        .map(|_| {
            let database = database.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let principal = PrincipalId::generate().unwrap();
                let project = ProjectId::generate().unwrap();
                let service = test_support::open_service_store(&database).unwrap();
                let mut store = service.worker_append_store().unwrap();
                barrier.wait();
                loop {
                    let (revision, _) = store.extension_registry_state().unwrap();
                    let outcome = store
                        .persist_extension_registry_snapshot(
                            principal,
                            project,
                            revision,
                            revision + 1,
                            b"scope-snapshot",
                            1,
                            1,
                            1024,
                        )
                        .unwrap();
                    if outcome != ExtensionRegistryCommit::Stale {
                        return outcome;
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    let outcomes = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ExtensionRegistryCommit::Committed)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ExtensionRegistryCommit::LimitExceeded)
            .count(),
        1
    );
    let service = test_support::open_service_store(&database).unwrap();
    let mut store = service.worker_append_store().unwrap();
    let (revision, snapshots) = store.extension_registry_state().unwrap();
    assert_eq!(revision, 1);
    assert_eq!(snapshots.len(), 1);
    drop(store);
    drop(service);
    let _ = fs::remove_file(database);
}
