use std::{collections::BTreeMap, fs, path::Path};

use kit::agent::extensions::{
    CompatibilityRange, ConfigSource, ContentDigest, ContractVersion, ExtensionConfigLayer,
    ExtensionConfigStack, ExtensionDescriptor, ExtensionError, ExtensionIdentity, ExtensionPoint,
    ExtensionReference, ExtensionRegistry, ExtensionVersion, HOST_CONTRACT_VERSION,
    OUT_OF_PROCESS_PROTOCOLS, built_in_descriptors, validate_contracts,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};

#[derive(Deserialize)]
struct CompatibilityManifest {
    schema_version: u16,
    host_contract_version: ContractVersion,
    descriptor_schema_version: u16,
    effective_config_schema_version: u16,
    compatibility_range: CompatibilityRange,
    trust_boundary: TrustBoundary,
    vendor_pins: VendorPins,
    built_ins: Vec<BuiltInPin>,
}

#[derive(Deserialize)]
struct TrustBoundary {
    in_process: String,
    token_visibility: String,
    dynamic_library_loading: String,
    untrusted_protocols: Vec<String>,
}

#[derive(Deserialize)]
struct VendorPins {
    agentkit: AgentkitPin,
}

#[derive(Deserialize)]
struct AgentkitPin {
    version: String,
    commit: String,
    snapshot_sha256: String,
    source: String,
}

#[derive(Deserialize)]
struct BuiltInPin {
    extension_point: ExtensionPoint,
    identity: String,
    version: String,
    schema_digest: String,
    implementation_digest: String,
    source: String,
}

fn manifest() -> CompatibilityManifest {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/compatibility/ext-m002.yaml");
    serde_yaml::from_slice(
        &fs::read(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
}

fn descriptor(
    point: ExtensionPoint,
    identity: &str,
    version: &str,
    schema: &[u8],
) -> ExtensionDescriptor {
    ExtensionDescriptor::new(
        point,
        ExtensionIdentity::parse(identity).unwrap(),
        ExtensionVersion::parse(version).unwrap(),
        CompatibilityRange::new(ContractVersion::new(1, 0, 0), ContractVersion::new(2, 0, 0)),
        ContentDigest::sha256(schema),
        ContentDigest::sha256(b"implementation"),
    )
    .unwrap()
}

fn built_in_registry() -> ExtensionRegistry {
    ExtensionRegistry::from_descriptors(built_in_descriptors()).unwrap()
}

#[test]
fn every_m002_extension_point_has_a_versioned_compatible_pin() {
    let manifest = manifest();
    let registry = built_in_registry();

    assert_eq!(manifest.schema_version, 1);
    assert_eq!(manifest.descriptor_schema_version, 1);
    assert_eq!(
        manifest.effective_config_schema_version,
        kit::domain::config::CONFIG_SCHEMA_VERSION as u16
    );
    assert_eq!(manifest.host_contract_version, HOST_CONTRACT_VERSION);
    assert!(manifest.compatibility_range.contains(HOST_CONTRACT_VERSION));
    assert_eq!(registry.descriptors().len(), ExtensionPoint::ALL.len());
    assert_eq!(manifest.built_ins.len(), ExtensionPoint::ALL.len());

    let pins = manifest
        .built_ins
        .into_iter()
        .map(|pin| (pin.extension_point, pin))
        .collect::<BTreeMap<_, _>>();
    for point in ExtensionPoint::ALL {
        let registered = registry
            .descriptor_for(point)
            .unwrap_or_else(|| panic!("missing registered {point:?}"));
        let pin = &pins[&point];
        assert_eq!(registered.identity().as_str(), pin.identity);
        assert_eq!(registered.version().as_str(), pin.version);
        assert_eq!(registered.schema_digest().as_str(), pin.schema_digest);
        assert_eq!(
            registered.implementation_digest().as_str(),
            pin.implementation_digest
        );
        assert!(registered.compatibility().contains(HOST_CONTRACT_VERSION));
        assert!(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(&pin.source)
                .is_file()
        );
    }

    assert_eq!(manifest.vendor_pins.agentkit.version, "0.10.2");
    assert_eq!(
        manifest.vendor_pins.agentkit.commit,
        "c3926f1c4f3c945d400c8b6ef039da1f84826fcd"
    );
    assert_eq!(
        manifest.vendor_pins.agentkit.snapshot_sha256,
        "3cc4569be6990cd88265f9e3d5d2c057c1cfd4eefad5da4ff0ece4150d758077"
    );
    assert_eq!(
        manifest.vendor_pins.agentkit.source,
        "vendor/agentkit/SNAPSHOT-METADATA.yaml"
    );
}

#[test]
fn duplicate_identity_version_and_schema_drift_fail_closed() {
    let original = descriptor(
        ExtensionPoint::ModelAdapter,
        "example.model_adapter",
        "1.4.0",
        b"provider-schema-v1",
    );
    assert!(matches!(
        validate_contracts([original.clone(), original.clone()]),
        Err(ExtensionError::DuplicateIdentityVersion(extension))
            if extension == original.reference()
    ));

    let drifted = descriptor(
        ExtensionPoint::ModelAdapter,
        "example.model_adapter",
        "1.4.0",
        b"provider-schema-mutated",
    );
    assert!(matches!(
        validate_contracts([original.clone(), drifted]),
        Err(ExtensionError::SchemaDrift { extension, .. })
            if extension == original.reference()
    ));

    let registry = built_in_registry();
    let built_in = registry
        .descriptor_for(ExtensionPoint::ModelAdapter)
        .unwrap();
    assert!(matches!(
        registry.assert_schema(&built_in.reference(), &ContentDigest::sha256(b"changed")),
        Err(ExtensionError::SchemaDrift { .. })
    ));
}

#[test]
fn newer_contract_versions_fail_closed_and_additive_fields_roundtrip() {
    let original = descriptor(
        ExtensionPoint::PromptModule,
        "example.prompt",
        "7.1.2",
        b"prompt-schema-v1",
    );
    let mut encoded = serde_json::to_value(&original).unwrap();
    encoded["vendor_metadata"] = json!({"feature_revision": 8, "modes": ["safe"]});
    let decoded: ExtensionDescriptor = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(
        decoded.additional_fields()["vendor_metadata"],
        encoded["vendor_metadata"]
    );
    assert_eq!(serde_json::to_value(&decoded).unwrap(), encoded);
    validate_contracts([decoded]).unwrap();

    let mut newer_schema = serde_json::to_value(&original).unwrap();
    newer_schema["schema_version"] = json!(2);
    let newer_schema: ExtensionDescriptor = serde_json::from_value(newer_schema).unwrap();
    assert!(matches!(
        validate_contracts([newer_schema]),
        Err(ExtensionError::UnsupportedDescriptorSchemaVersion { found: 2 })
    ));

    let mut newer_contract = serde_json::to_value(&original).unwrap();
    newer_contract["compatibility"] = json!({
        "minimum": "2.0.0",
        "maximum_exclusive": "3.0.0"
    });
    let newer_contract: ExtensionDescriptor = serde_json::from_value(newer_contract).unwrap();
    assert!(matches!(
        validate_contracts([newer_contract]),
        Err(ExtensionError::IncompatibleContract { host, .. })
            if host == HOST_CONTRACT_VERSION
    ));

    let config = json!({
        "schema_version": 1,
        "selections": {},
        "deployment_metadata": {"region": "test"}
    });
    let config_layer: ExtensionConfigLayer = serde_json::from_value(config.clone()).unwrap();
    assert_eq!(
        config_layer.additional_fields()["deployment_metadata"],
        config["deployment_metadata"]
    );
    assert_eq!(serde_json::to_value(config_layer).unwrap(), config);
}

#[test]
fn effective_extension_selection_is_immutable_digest_bound_and_provenanced() {
    let registry = built_in_registry();
    let model_adapter = registry
        .descriptor_for(ExtensionPoint::ModelAdapter)
        .unwrap()
        .reference();
    let mut stack = ExtensionConfigStack::built_ins();
    let mut user = ExtensionConfigLayer::empty();
    user.select(ExtensionPoint::ModelAdapter, model_adapter.clone());
    stack.user = Some(user);

    let effective = stack.materialize(&registry).unwrap();
    assert_eq!(effective.selections().len(), ExtensionPoint::ALL.len());
    assert_eq!(effective.provenance().len(), ExtensionPoint::ALL.len());
    assert_eq!(
        effective.selection(ExtensionPoint::ModelAdapter),
        &model_adapter
    );
    assert_eq!(
        effective.source(ExtensionPoint::ModelAdapter),
        ConfigSource::User
    );
    for point in [ExtensionPoint::PromptModule, ExtensionPoint::CostTable] {
        assert_eq!(effective.source(point), ConfigSource::BuiltIn);
    }
    assert_eq!(
        Sha256::digest(effective.canonical_bytes()).as_slice(),
        effective.digest()
    );

    stack.user.as_mut().unwrap().selections.clear();
    assert_eq!(
        effective.selection(ExtensionPoint::ModelAdapter),
        &model_adapter
    );
    assert_eq!(
        effective.source(ExtensionPoint::ModelAdapter),
        ConfigSource::User
    );
    assert_eq!(
        ExtensionConfigStack::built_ins()
            .materialize(&registry)
            .unwrap()
            .digest_hex(),
        ExtensionConfigStack::built_ins()
            .materialize(&registry)
            .unwrap()
            .digest_hex()
    );
}

#[test]
fn extension_config_rejects_newer_schema_unknown_selection_and_point_mismatch() {
    let registry = built_in_registry();
    let mut newer = ExtensionConfigStack::built_ins();
    let mut newer_layer = ExtensionConfigLayer::empty();
    newer_layer.schema_version = 2;
    newer.user = Some(newer_layer);
    assert!(matches!(
        newer.materialize(&registry),
        Err(ExtensionError::UnsupportedConfigSchemaVersion {
            source: ConfigSource::User,
            found: 2
        })
    ));

    let mut unknown = ExtensionConfigStack::built_ins();
    let mut layer = ExtensionConfigLayer::empty();
    layer.select(
        ExtensionPoint::ModelAdapter,
        ExtensionReference::new(
            ExtensionIdentity::parse("missing.provider").unwrap(),
            ExtensionVersion::parse("1.0.0").unwrap(),
        ),
    );
    unknown.run = Some(layer);
    assert!(matches!(
        unknown.materialize(&registry),
        Err(ExtensionError::UnknownSelection(_))
    ));

    let mut mismatched = ExtensionConfigStack::built_ins();
    let prompt_module = registry
        .descriptor_for(ExtensionPoint::PromptModule)
        .unwrap()
        .reference();
    mismatched
        .built_in
        .select(ExtensionPoint::ModelAdapter, prompt_module);
    assert!(matches!(
        mismatched.materialize(&registry),
        Err(ExtensionError::ExtensionPointConflict { .. })
    ));
}

#[test]
fn untrusted_in_process_registration_is_rejected_with_protocol_route() {
    let registry = built_in_registry();
    let untrusted = descriptor(
        ExtensionPoint::CostTable,
        "third_party.costs",
        "2026.7.0",
        b"third-party-cost-schema",
    );
    assert!(matches!(
        registry.reject_untrusted_in_process(&untrusted),
        Err(ExtensionError::OutOfProcessRequired {
            extension,
            protocols
        }) if extension == untrusted.reference() && protocols == OUT_OF_PROCESS_PROTOCOLS
    ));

    let manifest = manifest();
    assert_eq!(manifest.trust_boundary.in_process, "trusted_token_required");
    assert_eq!(manifest.trust_boundary.token_visibility, "crate_private");
    assert_eq!(
        manifest.trust_boundary.dynamic_library_loading,
        "prohibited"
    );
    assert_eq!(
        manifest.trust_boundary.untrusted_protocols,
        ["mcp", "acp", "a2a", "kit_plugin"]
    );
    let source = include_str!("../../src/agent/extensions/registry.rs");
    assert!(source.contains("pub(crate) struct TrustedExtensionToken"));
    assert!(!source.contains("libloading"));
    assert!(!source.contains("dlopen"));
}
