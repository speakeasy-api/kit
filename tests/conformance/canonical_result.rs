mod canonical_result_contract {
use std::sync::Arc;

use kit::{
    capabilities::{
        discovery::BindingId,
        kernel::{
            identity::{
                CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
                CapabilityVersion, Digest, DigestAlgorithm,
            },
            invoke::InvocationStatus,
        },
        result::{
            CANONICAL_RESULT_SCHEMA_VERSION, CallProvenance, CallProvenanceInput, CanonicalResult,
            DelegationProvenance, MAX_CANONICAL_RESULT_BYTES, MAX_PRESENTATION_BYTES,
            MAX_RESULT_ARTIFACTS, MAX_RESULT_ERROR_CODE_BYTES, Presentation, PresentedResult,
            ResultError,
        },
    },
    domain::{
        events::TraceId,
        ids::{PrincipalId, ToolCallId},
    },
    runtime::scheduler::limits::Spend,
    store::{artifacts::ArtifactReference, sqlite::idempotency::IdempotencyKey},
};
use serde_json::{Map, Value, json};

fn artifact(seed: u64) -> ArtifactReference {
    ArtifactReference::parse(&format!("artifact-ref:{seed:064x}")).unwrap()
}

fn provenance_input(seed: u64) -> CallProvenanceInput {
    CallProvenanceInput {
        invocation_id: ToolCallId::parse("tool_call_00000000000000000000000001").unwrap(),
        principal_id: PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
        binding_id: BindingId::parse(&format!("binding_v1_{seed:064x}")).unwrap(),
        capability: CapabilityIdentity::new(
            CapabilitySource::new("fixture").unwrap(),
            CapabilityNamespace::new("kit.result").unwrap(),
            CapabilityName::new(format!("tool-{seed}")).unwrap(),
            CapabilityVersion::new("1.0.0").unwrap(),
            Digest::of(DigestAlgorithm::Blake3, format!("implementation-{seed}").as_bytes()),
        ),
        schema_digest: Digest::of(DigestAlgorithm::Sha256, format!("schema-{seed}").as_bytes()),
        authorization_snapshot_digest: Digest::of(
            DigestAlgorithm::Sha256,
            format!("authorization-{seed}").as_bytes(),
        ),
        grant_snapshot_digest: Digest::of(
            DigestAlgorithm::Blake3,
            format!("grant-{seed}").as_bytes(),
        ),
        trace_id: TraceId::parse(&format!("trace-result-{seed}")).unwrap(),
        idempotency_key: IdempotencyKey::parse(&format!("result-{seed}")).unwrap(),
        remaining_budget: Spend::new(seed + 1, seed + 2, seed + 3, seed + 4, seed + 5),
    }
}

fn provenance(seed: u64, nested: bool) -> CallProvenance {
    let input = provenance_input(seed);
    if nested {
        CallProvenance::nested(
            input,
            ToolCallId::parse("tool_call_00000000000000000000000002").unwrap(),
            DelegationProvenance::new(
                Digest::of(DigestAlgorithm::Sha256, format!("delegation-{seed}").as_bytes()),
                2,
                4,
            )
            .unwrap(),
        )
        .unwrap()
    } else {
        CallProvenance::direct(input).unwrap()
    }
}

fn fixture_content(seed: usize) -> Value {
    match seed % 8 {
        0 => json!(null),
        1 => json!(seed as u64),
        2 => json!(true),
        3 => json!("quotes: \" backslash: \\ newline:\n"),
        4 => json!("Zażółć gęślą jaźń 東京"),
        5 => json!([seed, "array", false, {"z": 1, "a": 2}]),
        6 => json!({"nested": {"z": seed, "a": [3, 2, 1]}, "first": 1}),
        _ => json!(18_446_744_073_709_551_615_u64),
    }
}

fn result(
    status: InvocationStatus,
    content: Option<Value>,
    error: Option<&str>,
    seed: u64,
    nested: bool,
    artifacts: impl IntoIterator<Item = ArtifactReference>,
) -> Result<CanonicalResult, ResultError> {
    let charged = match status {
        InvocationStatus::Succeeded | InvocationStatus::Failed => true,
        InvocationStatus::ApprovalRequired
        | InvocationStatus::ApprovalDenied
        | InvocationStatus::Cancelled => false,
        InvocationStatus::OutcomeUnknown => seed.is_multiple_of(2),
    };
    CanonicalResult::new(
        status,
        content,
        error,
        charged,
        artifacts,
        provenance(seed, nested),
    )
}

fn canonical_mutation(
    result: &CanonicalResult,
    mutate: impl FnOnce(&mut Map<String, Value>),
) -> Vec<u8> {
    let mut value: Value = serde_json::from_slice(result.canonical_bytes()).unwrap();
    mutate(value.as_object_mut().unwrap());
    serde_json::to_vec(&value).unwrap()
}

pub fn run() {
    let statuses = [
        InvocationStatus::Succeeded,
        InvocationStatus::Failed,
        InvocationStatus::ApprovalRequired,
        InvocationStatus::ApprovalDenied,
        InvocationStatus::Cancelled,
        InvocationStatus::OutcomeUnknown,
    ];

    for status in statuses {
        for charged in [false, true] {
            let succeeded = status == InvocationStatus::Succeeded;
            let candidate = CanonicalResult::new(
                status,
                succeeded.then_some(json!(true)),
                (!succeeded).then_some("charge_matrix"),
                charged,
                [],
                provenance(99, false),
            );
            let expected = match status {
                InvocationStatus::Succeeded | InvocationStatus::Failed => charged,
                InvocationStatus::ApprovalRequired
                | InvocationStatus::ApprovalDenied
                | InvocationStatus::Cancelled => !charged,
                InvocationStatus::OutcomeUnknown => true,
            };
            assert_eq!(candidate.is_ok(), expected, "{status:?} charged={charged}");
        }
    }

    for seed in 0..120_usize {
        let status = statuses[seed % statuses.len()];
        let succeeded = status == InvocationStatus::Succeeded;
        let artifacts = (0..seed % 5)
            .rev()
            .map(|index| artifact((seed * 10 + index) as u64));
        let original = result(
            status,
            succeeded.then(|| fixture_content(seed / statuses.len())),
            (!succeeded).then_some("fixture_error"),
            seed as u64 + 1,
            (seed / statuses.len()).is_multiple_of(2),
            artifacts,
        )
        .unwrap();
        let decoded = CanonicalResult::from_canonical_bytes(original.canonical_bytes()).unwrap();
        assert_eq!(decoded.canonical_bytes(), original.canonical_bytes());
        assert_eq!(decoded.digest(), original.digest());
        assert_eq!(decoded.status(), original.status());
        assert_eq!(decoded.content(), original.content());
        assert_eq!(decoded.error_code(), original.error_code());
        assert_eq!(decoded.charged(), original.charged());
        assert_eq!(decoded.artifacts(), original.artifacts());
        assert_eq!(decoded.provenance(), original.provenance());
    }

    let golden = result(
        InvocationStatus::Succeeded,
        Some(json!({"answer": 42})),
        None,
        1,
        false,
        [],
    )
    .unwrap();
    assert!(CanonicalResult::new(
        InvocationStatus::Succeeded,
        Some(json!(true)),
        None,
        true,
        [],
        provenance(2, false),
    )
    .is_ok());
    assert_eq!(
        std::str::from_utf8(golden.canonical_bytes()).unwrap(),
        "{\"artifacts\":[],\"authorization_snapshot_digest\":\"sha256:f05352651462d6aa55b472c0374d5b9bbfb87948f4dee5d28de1c4fe328c3ba5\",\"binding_id\":\"binding_v1_0000000000000000000000000000000000000000000000000000000000000001\",\"capability\":{\"implementation_digest\":\"blake3:ca3af6b1cfbc26108ab86d865bcca9a839c2b1175f3936a341b7e2919b449c42\",\"name\":\"tool-1\",\"namespace\":\"kit.result\",\"source\":\"fixture\",\"version\":\"1.0.0\"},\"charged\":true,\"content\":{\"answer\":42},\"delegation\":null,\"error_code\":null,\"grant_snapshot_digest\":\"blake3:1d7213ad476ca7bee962f5f3e8b2fc135804ab0e4ecaece6d34aefd869d2c79b\",\"idempotency_key\":\"result-1\",\"invocation_id\":\"tool_call_00000000000000000000000001\",\"parent_invocation_id\":null,\"principal_id\":\"principal_00000000000000000000000001\",\"remaining_budget\":{\"cost_microusd\":2,\"processes\":6,\"tokens\":3,\"tools\":5,\"turns\":4},\"schema_digest\":\"sha256:6f6150d77e1e6d8c6360638e4761edebbd140830e7d4dbce46c32ec3568b9e6e\",\"schema_version\":1,\"status\":\"succeeded\",\"trace_id\":\"trace-result-1\"}"
    );
    assert_eq!(
        golden.digest().to_string(),
        "sha256:20bbae2923166bedc77194255f102e39a457284b5e28889726ceecf626a0fa90"
    );
    assert_eq!(
        golden.digest().as_bytes(),
        [
            0x20, 0xbb, 0xae, 0x29, 0x23, 0x16, 0x6b, 0xed, 0xc7, 0x71, 0x94, 0x25, 0x5f, 0x10,
            0x2e, 0x39, 0xa4, 0x57, 0x28, 0x4b, 0x5e, 0x28, 0x88, 0x97, 0x26, 0xce, 0xec, 0xf6,
            0x26, 0xa0, 0xfa, 0x90,
        ]
    );

    let mut forward = Map::new();
    forward.insert("a".into(), json!({"c": 3, "b": 2}));
    forward.insert("z".into(), json!(1));
    let mut reverse = Map::new();
    reverse.insert("z".into(), json!(1));
    reverse.insert("a".into(), json!({"b": 2, "c": 3}));
    let ordered_a = result(
        InvocationStatus::Succeeded,
        Some(Value::Object(forward)),
        None,
        200,
        false,
        [],
    )
    .unwrap();
    let ordered_b = result(
        InvocationStatus::Succeeded,
        Some(Value::Object(reverse)),
        None,
        200,
        false,
        [],
    )
    .unwrap();
    assert_eq!(ordered_a.canonical_bytes(), ordered_b.canonical_bytes());
    assert_eq!(ordered_a.digest(), ordered_b.digest());

    let canonical = Arc::new(ordered_a);
    let presented = PresentedResult::new(
        Arc::clone(&canonical),
        Presentation::new(&canonical, "json", "1", "{}").unwrap(),
    )
    .unwrap();
    for presentation in [
        Presentation::new(&canonical, "json", "2", "{\"a\":1}").unwrap(),
        Presentation::new(&canonical, "text", "1", "plain").unwrap(),
        Presentation::new(&canonical, "table", "7", "a | b").unwrap(),
        Presentation::new(&canonical, "artifact", "1", "artifact-ref:handle").unwrap(),
        Presentation::new(&canonical, "toon", "3.3", "a: 1").unwrap(),
    ] {
        let changed = presented.with_presentation(presentation).unwrap();
        assert_eq!(changed.canonical().canonical_bytes(), canonical.canonical_bytes());
        assert_eq!(changed.canonical().digest(), canonical.digest());
    }
    let other = Arc::new(
        result(
            InvocationStatus::Succeeded,
            Some(json!(false)),
            None,
            201,
            false,
            [],
        )
        .unwrap(),
    );
    let mismatched = Presentation::new(&other, "text", "1", "wrong result").unwrap();
    assert!(matches!(
        PresentedResult::new(Arc::clone(&canonical), mismatched.clone()),
        Err(ResultError::InvalidPresentation)
    ));
    assert!(matches!(
        presented.with_presentation(mismatched),
        Err(ResultError::InvalidPresentation)
    ));

    let nested = result(
        InvocationStatus::Succeeded,
        Some(json!({"nested": true})),
        None,
        300,
        true,
        [artifact(9)],
    )
    .unwrap();
    let nested_roundtrip = CanonicalResult::from_canonical_bytes(nested.canonical_bytes()).unwrap();
    let call = nested_roundtrip.provenance();
    assert_eq!(call.invocation_id(), nested.provenance().invocation_id());
    assert_eq!(call.parent_invocation_id(), nested.provenance().parent_invocation_id());
    assert_eq!(call.principal_id(), nested.provenance().principal_id());
    assert_eq!(call.binding_id(), nested.provenance().binding_id());
    assert_eq!(call.capability(), nested.provenance().capability());
    assert_eq!(call.schema_digest(), nested.provenance().schema_digest());
    assert_eq!(
        call.authorization_snapshot_digest(),
        nested.provenance().authorization_snapshot_digest()
    );
    assert_eq!(
        call.grant_snapshot_digest(),
        nested.provenance().grant_snapshot_digest()
    );
    assert_eq!(call.delegation(), nested.provenance().delegation());
    assert_eq!(call.trace_id(), nested.provenance().trace_id());
    assert_eq!(call.idempotency_key(), nested.provenance().idempotency_key());
    assert_eq!(call.remaining_budget(), Spend::new(301, 302, 303, 304, 305));

    let empty = result(
        InvocationStatus::Succeeded,
        Some(json!("")),
        None,
        400,
        false,
        [],
    )
    .unwrap();
    let exact_payload = "x".repeat(MAX_CANONICAL_RESULT_BYTES - empty.canonical_bytes().len());
    let exact = result(
        InvocationStatus::Succeeded,
        Some(json!(exact_payload)),
        None,
        400,
        false,
        [],
    )
    .unwrap();
    assert_eq!(exact.canonical_bytes().len(), MAX_CANONICAL_RESULT_BYTES);
    assert_eq!(
        result(
            InvocationStatus::Succeeded,
            Some(json!("x".repeat(
                MAX_CANONICAL_RESULT_BYTES - empty.canonical_bytes().len() + 1
            ))),
            None,
            400,
            false,
            [],
        ),
        Err(ResultError::ResultTooLarge)
    );

    let artifacts_128: Vec<_> = (0..MAX_RESULT_ARTIFACTS as u64).map(artifact).collect();
    assert!(result(
        InvocationStatus::Succeeded,
        Some(json!(true)),
        None,
        500,
        false,
        artifacts_128.clone(),
    )
    .is_ok());
    assert_eq!(
        result(
            InvocationStatus::Succeeded,
            Some(json!(true)),
            None,
            500,
            false,
            (0..=MAX_RESULT_ARTIFACTS as u64).map(artifact),
        ),
        Err(ResultError::TooManyArtifacts)
    );
    assert_eq!(
        result(
            InvocationStatus::Succeeded,
            Some(json!(true)),
            None,
            500,
            false,
            [artifact(1), artifact(1)],
        ),
        Err(ResultError::DuplicateArtifact)
    );
    assert_eq!(
        result(
            InvocationStatus::Succeeded,
            Some(json!(true)),
            None,
            500,
            false,
            (0_u64..).map(artifact),
        ),
        Err(ResultError::TooManyArtifacts)
    );
    let malformed_artifact = canonical_mutation(&nested, |wire| {
        wire.insert("artifacts".into(), json!(["not-an-artifact"]));
    });
    assert_eq!(
        CanonicalResult::from_canonical_bytes(malformed_artifact),
        Err(ResultError::InvalidArtifact)
    );

    let error_256 = "e".repeat(MAX_RESULT_ERROR_CODE_BYTES);
    assert!(result(
        InvocationStatus::Failed,
        None,
        Some(&error_256),
        600,
        false,
        [],
    )
    .is_ok());
    let error_257 = format!("{error_256}e");
    assert_eq!(
        result(
            InvocationStatus::Failed,
            None,
            Some(&error_257),
            600,
            false,
            [],
        ),
        Err(ResultError::InvalidErrorCode)
    );
    for invalid in ["Uppercase", "has space", "control\n", "slash/code"] {
        assert_eq!(
            result(
                InvocationStatus::Failed,
                None,
                Some(invalid),
                600,
                false,
                [],
            ),
            Err(ResultError::InvalidErrorCode)
        );
    }

    let mut low = 0;
    let mut high = MAX_PRESENTATION_BYTES;
    while low < high {
        let middle = (low + high).div_ceil(2);
        if Presentation::new(&nested, "text", "1", "x".repeat(middle)).is_ok() {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    assert!(Presentation::new(&nested, "text", "1", "x".repeat(low)).is_ok());
    assert_eq!(
        Presentation::new(&nested, "text", "1", "x".repeat(low + 1)),
        Err(ResultError::PresentationTooLarge)
    );
    assert_eq!(
        Presentation::new(&nested, "toon", "3.2", "a: 1"),
        Err(ResultError::InvalidPresentation)
    );
    assert_eq!(
        Presentation::new(&nested, "bad\nname", "1", "body"),
        Err(ResultError::InvalidPresentation)
    );
    assert_eq!(
        Presentation::new(&nested, "xml", "1", "body"),
        Err(ResultError::InvalidPresentation)
    );

    let wrong_version = canonical_mutation(&nested, |wire| {
        wire.insert(
            "schema_version".into(),
            json!(CANONICAL_RESULT_SCHEMA_VERSION + 1),
        );
    });
    assert_eq!(
        CanonicalResult::from_canonical_bytes(wrong_version),
        Err(ResultError::UnsupportedVersion)
    );
    let future = canonical_mutation(&nested, |wire| {
        wire.insert("schema_version".into(), json!(2));
        wire.insert("status".into(), json!("future_terminal"));
        wire.insert("future_field".into(), json!({"accepted": true}));
    });
    assert_eq!(
        CanonicalResult::from_canonical_bytes(future),
        Err(ResultError::UnsupportedVersion)
    );
    let unknown_v1_status = canonical_mutation(&nested, |wire| {
        wire.insert("status".into(), json!("future_terminal"));
    });
    assert_eq!(
        CanonicalResult::from_canonical_bytes(unknown_v1_status),
        Err(ResultError::InvalidJson)
    );
    let mut very_deep = Value::Null;
    for _ in 0..10_000 {
        very_deep = Value::Array(vec![very_deep]);
    }
    assert_eq!(
        result(
            InvocationStatus::Succeeded,
            Some(very_deep),
            None,
            900,
            false,
            [],
        ),
        Err(ResultError::InvalidJson)
    );
    let unknown = canonical_mutation(&nested, |wire| {
        wire.insert("unknown".into(), json!(true));
    });
    assert_eq!(
        CanonicalResult::from_canonical_bytes(unknown),
        Err(ResultError::InvalidJson)
    );
    let mut whitespace = nested.canonical_bytes().to_vec();
    whitespace.push(b' ');
    assert_eq!(
        CanonicalResult::from_canonical_bytes(whitespace),
        Err(ResultError::NonCanonical)
    );
    let mut trailing = nested.canonical_bytes().to_vec();
    trailing.push(b'x');
    assert_eq!(
        CanonicalResult::from_canonical_bytes(trailing),
        Err(ResultError::InvalidJson)
    );
    let canonical_text = std::str::from_utf8(nested.canonical_bytes()).unwrap();
    let duplicate = format!("{{\"status\":\"succeeded\",{}", &canonical_text[1..]);
    assert_eq!(
        CanonicalResult::from_canonical_bytes(duplicate),
        Err(ResultError::InvalidJson)
    );

    let auth = nested
        .provenance()
        .authorization_snapshot_digest()
        .to_string();
    let canonical_prefix = format!(
        "{{\"artifacts\":[\"{}\"],\"authorization_snapshot_digest\":\"{auth}\",",
        artifact(9)
    );
    let noncanonical_prefix = format!(
        "{{\"authorization_snapshot_digest\":\"{auth}\",\"artifacts\":[\"{}\"],",
        artifact(9)
    );
    let noncanonical_order = canonical_text.replacen(&canonical_prefix, &noncanonical_prefix, 1);
    assert_ne!(noncanonical_order, canonical_text);
    assert_eq!(
        CanonicalResult::from_canonical_bytes(noncanonical_order),
        Err(ResultError::NonCanonical)
    );

    let numeric = result(
        InvocationStatus::Succeeded,
        Some(json!(1.0)),
        None,
        700,
        false,
        [],
    )
    .unwrap();
    let lossy_number = std::str::from_utf8(numeric.canonical_bytes())
        .unwrap()
        .replace("\"content\":1.0", "\"content\":1e0");
    assert_eq!(
        CanonicalResult::from_canonical_bytes(lossy_number),
        Err(ResultError::NonCanonical)
    );
    for source in ["1.2345678901234567", "1e100", "18446744073709551615"] {
        let value = serde_json::from_str(source).unwrap();
        let number = result(
            InvocationStatus::Succeeded,
            Some(value),
            None,
            700,
            false,
            [],
        )
        .unwrap();
        assert_eq!(
            CanonicalResult::from_canonical_bytes(number.canonical_bytes())
                .unwrap()
                .canonical_bytes(),
            number.canonical_bytes()
        );
    }
    for source in [
        "9007199254740993.0",
        "18446744073709551616",
        "1e309",
        "1e-400",
    ] {
        let invalid = std::str::from_utf8(numeric.canonical_bytes())
            .unwrap()
            .replace("\"content\":1.0", &format!("\"content\":{source}"));
        assert_eq!(
            CanonicalResult::from_canonical_bytes(invalid),
            Err(ResultError::InvalidJson),
            "{source}"
        );
    }

    for invalid in [
        canonical_mutation(&nested, |wire| {
            wire.insert("error_code".into(), json!("unexpected"));
        }),
        canonical_mutation(&nested, |wire| {
            wire.insert("status".into(), json!("failed"));
            wire.insert("content".into(), json!(true));
        }),
        canonical_mutation(&nested, |wire| {
            wire.insert("status".into(), json!("failed"));
            wire.insert("content".into(), Value::Null);
            wire.insert("error_code".into(), json!(""));
        }),
    ] {
        assert!(matches!(
            CanonicalResult::from_canonical_bytes(invalid),
            Err(ResultError::InvalidStatus | ResultError::InvalidErrorCode)
        ));
    }

    let direct = result(
        InvocationStatus::Succeeded,
        Some(json!(true)),
        None,
        800,
        false,
        [],
    )
    .unwrap();
    let direct_mismatch = canonical_mutation(&direct, |wire| {
        wire.insert(
            "parent_invocation_id".into(),
            json!("tool_call_00000000000000000000000002"),
        );
    });
    assert_eq!(
        CanonicalResult::from_canonical_bytes(direct_mismatch),
        Err(ResultError::InvalidProvenance)
    );
    let nested_mismatch = canonical_mutation(&nested, |wire| {
        wire.insert("parent_invocation_id".into(), Value::Null);
    });
    assert_eq!(
        CanonicalResult::from_canonical_bytes(nested_mismatch),
        Err(ResultError::InvalidProvenance)
    );
    let invalid_depth = canonical_mutation(&nested, |wire| {
        wire["delegation"]["depth"] = json!(5);
        wire["delegation"]["maximum_depth"] = json!(4);
    });
    assert_eq!(
        CanonicalResult::from_canonical_bytes(invalid_depth),
        Err(ResultError::InvalidProvenance)
    );

    let mut depth_63 = Value::Bool(true);
    for _ in 0..63 {
        depth_63 = Value::Array(vec![depth_63]);
    }
    assert!(result(
        InvocationStatus::Succeeded,
        Some(depth_63),
        None,
        900,
        false,
        [],
    )
    .is_ok());
    let mut depth_64 = Value::Bool(true);
    for _ in 0..64 {
        depth_64 = Value::Array(vec![depth_64]);
    }
    assert_eq!(
        result(
            InvocationStatus::Succeeded,
            Some(depth_64),
            None,
            900,
            false,
            [],
        ),
        Err(ResultError::InvalidJson)
    );
    let deep_source = std::str::from_utf8(direct.canonical_bytes())
        .unwrap()
        .replace(
            "\"content\":true",
            &format!("\"content\":{}true{}", "[".repeat(64), "]".repeat(64)),
        );
    assert_eq!(
        CanonicalResult::from_canonical_bytes(deep_source),
        Err(ResultError::InvalidJson)
    );
    assert_eq!(
        result(
            InvocationStatus::Succeeded,
            Some(Value::Array(vec![Value::Null; 100_001])),
            None,
            900,
            false,
            [],
        ),
        Err(ResultError::InvalidJson)
    );
    let mut invalid_status_deep = Value::Null;
    for _ in 0..10_000 {
        invalid_status_deep = Value::Array(vec![invalid_status_deep]);
    }
    assert_eq!(
        result(
            InvocationStatus::Failed,
            Some(invalid_status_deep),
            Some("invalid_status"),
            900,
            false,
            [],
        ),
        Err(ResultError::InvalidStatus)
    );

    let raw_control = std::str::from_utf8(direct.canonical_bytes())
        .unwrap()
        .replace("\"content\":true", "\"content\":\"bad\nstring\"");
    assert_eq!(
        CanonicalResult::from_canonical_bytes(raw_control),
        Err(ResultError::InvalidJson)
    );
    assert_eq!(
        CanonicalResult::from_canonical_bytes(br#"{"schema_version":1]"#),
        Err(ResultError::InvalidJson)
    );

    let self_parent_input = provenance_input(901);
    let self_parent = self_parent_input.invocation_id;
    assert_eq!(
        CallProvenance::nested(
            self_parent_input,
            self_parent,
            DelegationProvenance::new(
                Digest::of(DigestAlgorithm::Sha256, b"self-parent"),
                1,
                1,
            )
            .unwrap(),
        ),
        Err(ResultError::InvalidProvenance)
    );
    let self_parent_wire = canonical_mutation(&nested, |wire| {
        let invocation_id = wire["invocation_id"].clone();
        wire.insert("parent_invocation_id".into(), invocation_id);
    });
    assert_eq!(
        CanonicalResult::from_canonical_bytes(self_parent_wire),
        Err(ResultError::InvalidProvenance)
    );
}
}

#[test]
fn canonical_result() {
    canonical_result_contract::run();
}
