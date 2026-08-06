use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead as _, Write as _},
    sync::Arc,
};

use kit::{
    agent::extensions::{
        CompatibilityRange, ContentDigest, ContractVersion, ExtensionIdentity, ExtensionVersion,
    },
    api::{
        auth::{
            contract::{Authenticator, GrantSnapshot},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        service::WorkerStore,
    },
    capabilities::{
        catalog::{Availability, CapabilityKind},
        extensions::{
            CAPABILITY_EXTENSION_HOST_VERSION, CapabilityExtensionRegistry, ExtensionContract,
            ExtensionKind, ExtensionMetadata, ExtensionProtocol, RegistryError,
            SharedCapabilityExtensionRegistry, TrustClassification,
        },
        kernel::{grant::EffectClass, invoke::RetrySafety},
    },
    domain::{
        config::Grant,
        ids::{PrincipalId, ProjectId},
    },
    executor::profile::{
        Architecture, EgressGrant, EgressTransport, ExecutorProfile, Platform, ProfileSpec,
        ResourceLimits, TrustTier,
    },
    protocols::mcp::{
        config::{McpDescriptorPolicyConfig, McpOwnerConfig, McpServerConfig, McpTransportConfig},
        features::{ConfiguredServerIdentity, PayloadLimits, decode_tools_page},
    },
    test_support,
};

fn tool_descriptor() -> serde_json::Value {
    serde_json::json!({
        "name":"fixture_echo",
        "description":"Echo fixture text.",
        "inputSchema":{
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "additionalProperties":false,
            "properties":{"text":{"type":"string"}},
            "required":["text"],
            "type":"object"
        }
    })
}

fn restricted_profile() -> ProfileSpec {
    ProfileSpec::isolated(
        TrustTier::Restricted,
        if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Linux
        },
        if cfg!(target_arch = "aarch64") {
            Architecture::Aarch64
        } else {
            Architecture::X86_64
        },
        ResourceLimits::new(
            10_000,
            256 * 1024 * 1024,
            16,
            16 * 1024 * 1024,
            64 * 1024 * 1024,
            64 * 1024 * 1024,
            16 * 1024 * 1024,
            30_000,
        ),
    )
}

fn server(
    principal_id: PrincipalId,
    project_id: ProjectId,
    profile: ProfileSpec,
) -> McpServerConfig {
    let identity = ConfiguredServerIdentity::new("registry-fixture").unwrap();
    let descriptor_digest = decode_tools_page(
        &identity,
        serde_json::to_vec(
            &serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"tools":[tool_descriptor()]}}),
        )
        .unwrap(),
        PayloadLimits::default(),
    )
    .unwrap()
    .items()[0]
        .normalize()
        .unwrap()
        .descriptor_digest();
    let profile_digest = ExecutorProfile::new(profile.clone())
        .unwrap()
        .digest()
        .to_string();
    McpServerConfig {
        id: "registry-fixture".to_owned(),
        transport: McpTransportConfig::Stdio {
            owned_process_profile: "memory".to_owned(),
            argv: vec![
                std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ],
            profile: Box::new(profile),
            profile_digest,
            environment: BTreeMap::new(),
        },
        owner: McpOwnerConfig {
            principal_id,
            project_id,
            workspace_id: None,
        },
        source: "mcp.registry-fixture".to_owned(),
        trust_domain: "local".to_owned(),
        namespace: "fixture".to_owned(),
        version: "1".to_owned(),
        credential_handle: None,
        credential_scope: None,
        egress: None,
        descriptors: vec![McpDescriptorPolicyConfig {
            kind: CapabilityKind::Tool,
            remote: "fixture_echo".to_owned(),
            descriptor_digest,
            effect: EffectClass::ProcessSpawn,
            retry_safety: RetrySafety::Idempotent,
            required_grants: BTreeSet::from([Grant::ProcessSpawn]),
            auth_scopes: BTreeSet::new(),
            availability: Availability::Available,
        }],
        responders: Default::default(),
    }
}

#[test]
fn untrusted_ext_same_contract_authority_expansion_is_denied_before_launch() {
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let root = std::env::temp_dir().join(format!(
        "kit-untrusted-ext-registry-{}-{project_id}",
        std::process::id()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("state.sqlite3");
    let service = test_support::open_service_store(&database).unwrap();
    let mut store = service.worker_append_store().unwrap();
    let registry: SharedCapabilityExtensionRegistry =
        Arc::new(std::sync::RwLock::new(Default::default()));
    let configured = server(principal_id, project_id, restricted_profile());
    let base_contract = configured.extension_contract().unwrap();
    assert_eq!(
        base_contract.implementation_digest(),
        &ContentDigest::sha256(&fs::read(std::env::current_exe().unwrap()).unwrap())
    );
    let authenticated = LocalPeerAuthenticator::new(BTreeMap::from([(
        1002,
        GrantSnapshot::new(
            principal_id,
            project_id,
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
        ),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(1002, 1, 1002))
    .unwrap();
    CapabilityExtensionRegistry::register_untrusted_durable(
        &registry,
        &authenticated,
        project_id,
        base_contract.clone(),
        &mut store,
    )
    .unwrap();

    let mut expanded = restricted_profile();
    expanded
        .egress
        .insert(EgressGrant::new("example.com", 443, EgressTransport::Tcp).unwrap());
    let expanded = server(principal_id, project_id, expanded)
        .extension_contract()
        .unwrap();
    let mut changed_argv = configured.clone();
    let McpTransportConfig::Stdio { argv, .. } = &mut changed_argv.transport else {
        unreachable!()
    };
    argv.push("--changed-argv".to_owned());
    let changed_argv = changed_argv.extension_contract().unwrap();
    assert_eq!(
        base_contract.implementation_digest(),
        changed_argv.implementation_digest()
    );

    let changed_binary_path = root.join("changed-mcp-binary");
    fs::copy(std::env::current_exe().unwrap(), &changed_binary_path).unwrap();
    let mut changed_bytes = fs::read(&changed_binary_path).unwrap();
    changed_bytes.push(0);
    fs::write(&changed_binary_path, changed_bytes).unwrap();
    let mut changed_binary = configured;
    let McpTransportConfig::Stdio { argv, .. } = &mut changed_binary.transport else {
        unreachable!()
    };
    argv[0] = changed_binary_path.to_string_lossy().into_owned();
    let changed_binary = changed_binary.extension_contract().unwrap();
    assert_ne!(
        base_contract.implementation_digest(),
        changed_binary.implementation_digest()
    );
    assert!(matches!(
        CapabilityExtensionRegistry::register_untrusted_durable(
            &registry,
            &authenticated,
            project_id,
            changed_binary,
            &mut store,
        ),
        Err(RegistryError::ContractConflict(_))
    ));

    assert!(matches!(
        CapabilityExtensionRegistry::register_untrusted_durable(
            &registry,
            &authenticated,
            project_id,
            expanded.clone(),
            &mut store,
        ),
        Err(RegistryError::ContractConflict(reference)) if reference == expanded.reference()
    ));
    drop(store);
    drop(service);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn untrusted_ext_exactly_100_in_process_contract_loads_are_rejected_at_validation() {
    for index in 0..100 {
        let contract = ExtensionContract::untrusted(
            ExtensionKind::McpServer,
            ExtensionIdentity::parse(format!("third-party.mcp-{index}")).unwrap(),
            ExtensionVersion::parse("1.0.0").unwrap(),
            ContentDigest::sha256(b"schema"),
            ContentDigest::sha256(format!("implementation-{index}").as_bytes()),
            CompatibilityRange::new(
                CAPABILITY_EXTENSION_HOST_VERSION,
                ContractVersion::new(2, 0, 0),
            ),
            ExtensionProtocol::Mcp,
            format!("route-{index}"),
            ContentDigest::sha256(format!("profile-{index}").as_bytes()).to_string(),
            ExtensionMetadata::default(),
        )
        .unwrap();
        assert_eq!(contract.trust(), TrustClassification::Untrusted);
        let mut serialized = serde_json::to_value(contract).unwrap();
        serialized["route"] = serde_json::json!({"type":"in_process"});
        let error = serde_json::from_value::<ExtensionContract>(serialized)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("must run out of process"),
            "attempt {index}: {error}"
        );
    }
}

#[test]
fn untrusted_ext_actual_child_process_stdio_conformance_exactly_100() {
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_kit"))
        .arg("--kit-mcp-conformance-worker")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut invoke = |id: usize, method: &str, params: serde_json::Value| {
        serde_json::to_writer(
            &mut input,
            &serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )
        .unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
        let mut response = String::new();
        output.read_line(&mut response).unwrap();
        let response: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["id"], id);
        assert!(response.get("result").is_some());
    };
    invoke(1, "initialize", serde_json::json!({}));
    invoke(2, "tools/list", serde_json::json!({}));
    for index in 0..100 {
        invoke(
            index + 3,
            "tools/call",
            serde_json::json!({"name":"fixture_echo","arguments":{"text":"child"}}),
        );
    }
    drop(input);
    assert!(child.wait().unwrap().success());
}

#[test]
#[ignore = "requires an installed trusted Restricted/Hostile persistent stdio sandbox helper; local child conformance is not sandbox evidence"]
fn untrusted_ext_trusted_sandbox_external_evidence_exactly_100() {
    panic!("trusted sandbox external evidence is unavailable on this runner");
}
