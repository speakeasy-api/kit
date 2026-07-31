use std::collections::BTreeSet;

use kit::capabilities::{
    kernel::identity::{Digest, DigestAlgorithm},
    native::NativeCatalog,
    schema::{
        JSON_SCHEMA_2020_12, NormalizedSchema, ProjectionError, ProjectionProfile,
        ProjectionTarget, SchemaProjectionSet,
    },
};

fn source() -> Vec<u8> {
    br#"{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "required": ["path"],
  "properties": {
    "path": {"default": "src/lib.rs", "description": "Author path docs", "minLength": 1, "type": "string"},
    "mode": {"enum": ["read", "write"], "type": "string"}
  },
  "description": "Author root docs",
  "additionalProperties": false,
  "type": "object"
}"#
        .to_vec()
}

fn keywords() -> BTreeSet<String> {
    [
        "$schema",
        "additionalProperties",
        "default",
        "description",
        "enum",
        "minLength",
        "properties",
        "required",
        "type",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn target(model: &str) -> ProjectionTarget {
    ProjectionTarget::new("fixture", model, "fixture-adapter@1", 1).unwrap()
}

fn profile(model: &str) -> ProjectionProfile {
    ProjectionProfile::new(
        target(model),
        JSON_SCHEMA_2020_12,
        keywords(),
        serde_json::Value::Bool(true),
        1024 * 1024,
        DigestAlgorithm::Sha256,
    )
    .unwrap()
}

fn normalized() -> NormalizedSchema {
    NormalizedSchema::ingest(
        source(),
        JSON_SCHEMA_2020_12,
        b"Out-of-band author documentation\r\n",
        DigestAlgorithm::Sha256,
    )
    .unwrap()
}

#[test]
fn native_schema_dialect() {
    for descriptor in NativeCatalog::all() {
        assert_eq!(descriptor.schema().dialect(), JSON_SCHEMA_2020_12);
        let value: serde_json::Value =
            serde_json::from_slice(descriptor.schema().normalized_bytes()).unwrap();
        assert_eq!(value["$schema"], JSON_SCHEMA_2020_12);
        jsonschema::draft202012::options().build(&value).unwrap();
    }
}

#[test]
fn schema_source_preservation() {
    let source = source();
    let docs = b"Out-of-band author documentation\r\n".to_vec();
    let schema = NormalizedSchema::ingest(
        source.clone(),
        JSON_SCHEMA_2020_12,
        docs.clone(),
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    assert_eq!(schema.source().source_bytes(), source);
    assert_eq!(schema.source().dialect(), JSON_SCHEMA_2020_12);
    assert_eq!(schema.source().documentation(), docs);
}

#[test]
fn schema_normalized_preservation() {
    let first = normalized();
    let value: serde_json::Value = serde_json::from_slice(&source()).unwrap();
    let second = NormalizedSchema::ingest(
        serde_json::to_vec_pretty(&value).unwrap(),
        JSON_SCHEMA_2020_12,
        b"Out-of-band author documentation\r\n",
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    assert_eq!(
        first.source().normalized_bytes(),
        second.source().normalized_bytes()
    );
    assert_eq!(
        first.source().normalized_digest(),
        second.source().normalized_digest()
    );
}

#[test]
fn schema_projection_preservation() {
    let mut projections = SchemaProjectionSet::new(normalized());
    let first_target = target("model-a");
    let first = projections.project(&profile("model-a")).unwrap().clone();
    let second = projections.project(&profile("model-b")).unwrap().clone();
    assert_eq!(first.bytes(), first.schema().source().normalized_bytes());
    assert_eq!(second.bytes(), second.schema().source().normalized_bytes());
    assert_eq!(first.schema().source().source_bytes(), source());
    assert_eq!(first.schema().source().dialect(), JSON_SCHEMA_2020_12);
    assert_eq!(first.target().provider(), "fixture");
    assert_eq!(first.target().model(), "model-a");
    assert_eq!(first.target().adapter(), "fixture-adapter@1");
    assert_eq!(first.target().profile_version(), 1);
    assert_eq!(
        projections.projection(&first_target).unwrap().digest(),
        first.digest()
    );
    assert_eq!(
        projections.project(&profile("model-a")).unwrap().digest(),
        first.digest()
    );
    let conflicting = ProjectionProfile::new(
        target("model-a"),
        JSON_SCHEMA_2020_12,
        keywords().into_iter().chain(["title".to_owned()]).collect(),
        serde_json::Value::Bool(true),
        1024 * 1024,
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    assert_eq!(
        projections.project(&conflicting).unwrap_err(),
        ProjectionError::ProjectionConflict
    );
    assert_eq!(projections.len(), 2);
}

#[test]
fn schema_form_digests() {
    let mut projections = SchemaProjectionSet::new(normalized());
    let projection = projections.project(&profile("model-a")).unwrap();
    let schema = projection.schema();
    assert_eq!(
        schema.source().source_digest(),
        Digest::of(DigestAlgorithm::Sha256, schema.source().source_bytes())
    );
    assert_eq!(
        schema.source().normalized_digest(),
        Digest::of(DigestAlgorithm::Sha256, schema.source().normalized_bytes())
    );
    assert_eq!(
        schema.dialect_digest(),
        Digest::of(DigestAlgorithm::Sha256, JSON_SCHEMA_2020_12.as_bytes())
    );
    assert_eq!(
        schema.documentation_digest(),
        Digest::of(DigestAlgorithm::Sha256, schema.source().documentation())
    );
    assert_eq!(
        projection.digest(),
        Digest::of(DigestAlgorithm::Sha256, projection.bytes())
    );
    assert_ne!(projection.profile_digest(), projection.digest());
}

#[test]
fn schema_author_docs() {
    let mut projections = SchemaProjectionSet::new(normalized());
    let projection = projections.project(&profile("model-a")).unwrap();
    assert_eq!(
        projection.schema().source().documentation(),
        b"Out-of-band author documentation\r\n"
    );
    assert_eq!(projection.value()["description"], "Author root docs");
    assert_eq!(
        projection.value()["properties"]["path"]["description"],
        "Author path docs"
    );
    assert_eq!(
        projection.value()["properties"]["path"]["default"],
        "src/lib.rs"
    );
    assert_eq!(projection.value()["required"], serde_json::json!(["path"]));
}

#[test]
fn unsupported_constraints_reject_without_silent_drops() {
    let unsupported = [
        "allOf",
        "anyOf",
        "const",
        "contains",
        "dependentRequired",
        "dependentSchemas",
        "else",
        "format",
        "if",
        "items",
        "maximum",
        "minimum",
        "not",
        "oneOf",
        "pattern",
        "prefixItems",
        "then",
        "unevaluatedProperties",
        "uniqueItems",
        "$ref",
        "$dynamicRef",
    ];
    for keyword in unsupported {
        let schema = serde_json::json!({
            "$schema": JSON_SCHEMA_2020_12,
            "type": "object",
            "properties": {"value": {"type": "string", (keyword): constraint_value(keyword)}}
        });
        let normalized = NormalizedSchema::ingest(
            serde_json::to_vec(&schema).unwrap(),
            JSON_SCHEMA_2020_12,
            b"docs",
            DigestAlgorithm::Sha256,
        )
        .unwrap();
        let source_digest = normalized.source().source_digest();
        let mut projections = SchemaProjectionSet::new(normalized);
        let error = projections.project(&profile("model-a")).unwrap_err();
        assert!(matches!(
            &error,
            ProjectionError::UnsupportedConstraint { keyword: rejected, .. } if rejected == keyword
        ));
        assert!(projections.is_empty());
        assert_eq!(projections.project(&profile("model-b")).unwrap_err(), error);
        let accepting = ProjectionProfile::new(
            target("all-keywords"),
            JSON_SCHEMA_2020_12,
            keywords().into_iter().chain([keyword.to_owned()]).collect(),
            serde_json::Value::Bool(true),
            1024 * 1024,
            DigestAlgorithm::Sha256,
        );
        if matches!(keyword, "$ref" | "$dynamicRef") {
            assert_eq!(accepting.unwrap_err(), ProjectionError::InvalidProfile);
            continue;
        }
        assert_eq!(
            projections
                .project(&accepting.unwrap())
                .unwrap()
                .schema()
                .source()
                .source_digest(),
            source_digest
        );
    }

    assert_eq!(
        NormalizedSchema::ingest(
            br#"{"type":"string","type":"number"}"#,
            JSON_SCHEMA_2020_12,
            Vec::new(),
            DigestAlgorithm::Sha256,
        )
        .unwrap_err(),
        ProjectionError::InvalidJson
    );

    let data_keywords = serde_json::json!({
        "$schema": JSON_SCHEMA_2020_12,
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "default": {"pattern": "data, not a schema"}}
        }
    });
    let mut projections = SchemaProjectionSet::new(
        NormalizedSchema::ingest(
            serde_json::to_vec(&data_keywords).unwrap(),
            JSON_SCHEMA_2020_12,
            b"docs",
            DigestAlgorithm::Sha256,
        )
        .unwrap(),
    );
    let data_profile = ProjectionProfile::new(
        target("data-keywords"),
        JSON_SCHEMA_2020_12,
        ["$schema", "default", "properties", "type"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        serde_json::Value::Bool(true),
        1024 * 1024,
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    assert!(projections.project(&data_profile).is_ok());

    let decimals = NormalizedSchema::ingest(
        br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","minimum":0.1,"multipleOf":1.2300}"#,
        JSON_SCHEMA_2020_12,
        b"docs",
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    assert_eq!(decimals.value()["minimum"], serde_json::json!(0.1));
    assert_eq!(decimals.value()["multipleOf"], serde_json::json!(1.23));
    for lossy in [
        br#"{"minimum":0.10000000000000001}"#.as_slice(),
        br#"{"minimum":18446744073709551617}"#.as_slice(),
        br#"{"minimum":1e-400}"#.as_slice(),
    ] {
        assert_eq!(
            NormalizedSchema::ingest(lossy, JSON_SCHEMA_2020_12, b"docs", DigestAlgorithm::Sha256,)
                .unwrap_err(),
            ProjectionError::InvalidJson
        );
    }

    let scalar_profile = ProjectionProfile::new(
        target("scalar-only"),
        JSON_SCHEMA_2020_12,
        ["$schema", "type"].into_iter().map(str::to_owned).collect(),
        serde_json::json!({
            "type": "object",
            "properties": {"type": {"type": "string"}}
        }),
        1024 * 1024,
        DigestAlgorithm::Sha256,
    )
    .unwrap();
    let mut boolean = SchemaProjectionSet::new(
        NormalizedSchema::ingest(
            b"true",
            JSON_SCHEMA_2020_12,
            b"docs",
            DigestAlgorithm::Sha256,
        )
        .unwrap(),
    );
    assert_eq!(
        boolean.project(&scalar_profile).unwrap_err(),
        ProjectionError::UnsupportedSchemaForm
    );
    let mut type_array = SchemaProjectionSet::new(
        NormalizedSchema::ingest(
            br#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":["string","null"]}"#,
            JSON_SCHEMA_2020_12,
            b"docs",
            DigestAlgorithm::Sha256,
        )
        .unwrap(),
    );
    assert_eq!(
        type_array.project(&scalar_profile).unwrap_err(),
        ProjectionError::UnsupportedSchemaForm
    );

    let mut bounded = SchemaProjectionSet::new(normalized());
    for index in 0..64 {
        bounded
            .project(&profile(&format!("bounded-{index}")))
            .unwrap();
    }
    assert_eq!(
        bounded.project(&profile("bounded-overflow")).unwrap_err(),
        ProjectionError::LimitExceeded
    );
}

fn constraint_value(keyword: &str) -> serde_json::Value {
    match keyword {
        "allOf" | "anyOf" | "oneOf" | "prefixItems" => serde_json::json!([{"type":"string"}]),
        "const" => serde_json::json!("value"),
        "contains" | "items" | "not" | "if" | "then" | "else" => {
            serde_json::json!({"type":"string"})
        }
        "dependentRequired" => serde_json::json!({"value":["other"]}),
        "dependentSchemas" => serde_json::json!({"value":{"type":"string"}}),
        "format" => serde_json::json!("uri"),
        "maximum" | "minimum" => serde_json::json!(1),
        "pattern" => serde_json::json!("^value$"),
        "unevaluatedProperties" | "uniqueItems" => serde_json::json!(false),
        "$ref" | "$dynamicRef" => serde_json::json!("#"),
        _ => unreachable!(),
    }
}
