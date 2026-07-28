use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use jsonschema::{Registry, Validator};
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value, json};
use sha2::{Digest, Sha256};

use crate::fixtures_selftest::repos;

const SCHEMA_NAMES: [&str; 7] = [
    "budget",
    "cache-condition",
    "environment",
    "grader",
    "outcome",
    "task",
    "trial",
];

fn manifest_dir(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("eval/manifests")
        .join(relative)
}

struct StrictJson;

impl<'de> DeserializeSeed<'de> for StrictJson {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(StrictJson)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut entries: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut object = Map::new();
        while let Some(key) = entries.next_key::<String>()? {
            if object.contains_key(&key) {
                return Err(de::Error::custom(format_args!("duplicate key {key:?}")));
            }
            object.insert(key, entries.next_value_seed(StrictJson)?);
        }
        Ok(Value::Object(object))
    }
}

fn parse_json(bytes: &[u8]) -> serde_json::Result<Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = StrictJson.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

fn read_json(path: &Path) -> Value {
    parse_json(
        &fs::read(path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display())),
    )
    .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
}

fn json_files(directory: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
        .map(|entry| entry.expect("failed to read directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    paths
}

fn schemas() -> BTreeMap<String, Value> {
    json_files(&manifest_dir("schema/v1"))
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".schema.json"))
                .unwrap_or_else(|| panic!("unexpected schema filename: {}", path.display()))
                .to_owned();
            (name, read_json(&path))
        })
        .collect()
}

fn examples() -> BTreeMap<String, Value> {
    json_files(&manifest_dir("examples"))
        .into_iter()
        .map(|path| {
            let name = path
                .file_stem()
                .and_then(|name| name.to_str())
                .expect("example must have a UTF-8 filename")
                .to_owned();
            (name, read_json(&path))
        })
        .collect()
}

fn registry(schemas: &BTreeMap<String, Value>) -> Registry<'static> {
    Registry::new()
        .extend(schemas.values().map(|schema| {
            (
                schema["$id"].as_str().expect("schema must have an $id"),
                schema.clone(),
            )
        }))
        .expect("schema IDs must be valid URIs")
        .prepare()
        .expect("schema references must resolve")
}

fn validator(name: &str, schemas: &BTreeMap<String, Value>, registry: &Registry) -> Validator {
    jsonschema::draft202012::options()
        .with_registry(registry)
        .should_validate_formats(true)
        .build(&schemas[name])
        .unwrap_or_else(|error| panic!("failed to compile {name} schema: {error}"))
}

fn canonical_identity_digest(identity: &Value) -> String {
    let field = |name| {
        identity[name]
            .as_str()
            .unwrap_or_else(|| panic!("identity.{name} must be a string"))
    };
    let canonical = format!(
        "kit-trial-identity-v1\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
        field("randomization_id"),
        identity["attempt"]
            .as_u64()
            .expect("identity.attempt must be unsigned"),
        field("task_id"),
        field("environment_id"),
        field("budget_id"),
        field("cache_condition_id"),
        field("grader_id"),
        field("config_id"),
    );
    format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()))
}

fn load_trial(
    instance: &Value,
    schemas: &BTreeMap<String, Value>,
    registry: &Registry,
    repository_policy: &repos::RepositorySourcePolicy,
) -> Result<(), String> {
    validator("trial", schemas, registry)
        .validate(instance)
        .map_err(|error| error.to_string())?;

    let identity = &instance["identity"];
    for (identity_field, component, component_field) in [
        ("task_id", "task", "task_id"),
        ("environment_id", "environment", "environment_id"),
        ("budget_id", "budget", "budget_id"),
        (
            "cache_condition_id",
            "cache_condition",
            "cache_condition_id",
        ),
        ("grader_id", "grader", "grader_id"),
    ] {
        if identity[identity_field] != instance[component][component_field] {
            return Err(format!(
                "identity.{identity_field} does not match {component}.{component_field}"
            ));
        }
    }
    if identity["canonical_digest"] != canonical_identity_digest(identity) {
        return Err(
            "identity.canonical_digest does not match canonical identity inputs".to_owned(),
        );
    }

    let repository = &instance["task"]["repository"];
    let source = repository["source"]
        .as_str()
        .ok_or_else(|| "repository.source is missing".to_owned())?;
    let location = if source == "local_fixture" {
        repository["fixture"].as_str()
    } else {
        repository["url"].as_str()
    }
    .ok_or_else(|| "repository location is missing".to_owned())?;
    repository_policy
        .authorize(source, location, repository["fixture_grant"].as_str())
        .map_err(|error| format!("repository source denied: {error:?}"))
}

#[test]
fn eval_manifest_schemas_and_examples_conform() {
    let schemas = schemas();
    let examples = examples();
    let expected: BTreeSet<_> = SCHEMA_NAMES.into_iter().map(str::to_owned).collect();

    assert_eq!(schemas.keys().cloned().collect::<BTreeSet<_>>(), expected);
    assert_eq!(examples.keys().cloned().collect::<BTreeSet<_>>(), expected);

    for (name, schema) in &schemas {
        jsonschema::draft202012::meta::validate(schema)
            .unwrap_or_else(|error| panic!("{name} is not valid JSON Schema 2020-12: {error}"));
    }

    let registry = registry(&schemas);
    for (name, example) in &examples {
        validator(name, &schemas, &registry)
            .validate(example)
            .unwrap_or_else(|error| panic!("{name} example is invalid: {error}"));
    }

    let trial = &examples["trial"];
    for component in ["task", "environment", "budget", "cache_condition", "grader"] {
        let example_name = component.replace('_', "-");
        assert_eq!(trial[component], examples[&example_name]);
    }
}

#[test]
fn eval_manifest_duplicate_keys_are_rejected_before_validation() {
    for (name, key) in [
        ("duplicate-root-key", "task_id"),
        ("duplicate-nested-key", "source"),
    ] {
        let path = manifest_dir(&format!("invalid/{name}.json"));
        let bytes = fs::read(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let error = match parse_json(&bytes) {
            Ok(_) => panic!("{} unexpectedly parsed", path.display()),
            Err(error) => error,
        };
        let diagnostic = error.to_string();

        assert!(
            diagnostic.contains(&format!("duplicate key {key:?}")),
            "unexpected diagnostic for {}: {diagnostic}",
            path.display()
        );
        assert!(
            diagnostic.contains(" at line ") && diagnostic.contains(" column "),
            "diagnostic lacks a source location for {}: {diagnostic}",
            path.display()
        );
    }
}

#[test]
fn eval_manifest_trial_identity_is_immutable_and_outcome_is_separate() {
    let schemas = schemas();
    let examples = examples();
    let registry = registry(&schemas);
    let trial = &examples["trial"];

    load_trial(
        trial,
        &schemas,
        &registry,
        &repos::RepositorySourcePolicy::default(),
    )
    .expect("empty trial must load before execution");
    assert!(!trial["trial_id"].as_str().unwrap().is_empty());
    assert_eq!(
        trial["identity"]["canonical_digest"],
        canonical_identity_digest(&trial["identity"])
    );
    assert!(trial.get("outcome").is_none());

    let outcome = &examples["outcome"];
    assert_eq!(outcome["trial_id"], trial["trial_id"]);
    assert_eq!(outcome["status"], "pending");
    assert_eq!(outcome["provider_request_ids"], json!([]));
    assert!(outcome.get("result").is_none());
}

#[test]
fn eval_manifest_invalid_identity_and_state_are_rejected() {
    let schemas = schemas();
    let examples = examples();
    let registry = registry(&schemas);
    let trial_validator = validator("trial", &schemas, &registry);
    let outcome_validator = validator("outcome", &schemas, &registry);
    let mut invalid = Vec::new();

    let mut missing_trial_id = examples["trial"].clone();
    missing_trial_id.as_object_mut().unwrap().remove("trial_id");
    invalid.push(missing_trial_id);

    let mut embedded_outcome = examples["trial"].clone();
    embedded_outcome["outcome"] = examples["outcome"].clone();
    invalid.push(embedded_outcome);

    let mut wrong_version = examples["trial"].clone();
    wrong_version["task"]["schema_version"] = json!("2.0");
    invalid.push(wrong_version);

    let mut missing_nested_identity = examples["trial"].clone();
    missing_nested_identity["environment"]
        .as_object_mut()
        .unwrap()
        .remove("image_digest");
    invalid.push(missing_nested_identity);

    let mut unknown_nested_field = examples["trial"].clone();
    unknown_nested_field["grader"]["last_modified"] = json!("now");
    invalid.push(unknown_nested_field);

    for field in ["gold_patch_digest", "harness_config_digest"] {
        let mut missing_grader_pin = examples["trial"].clone();
        missing_grader_pin["grader"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        invalid.push(missing_grader_pin);
    }

    let mut invalid_budget = examples["trial"].clone();
    invalid_budget["budget"]["limits"]["tokens"] = json!(-1);
    invalid.push(invalid_budget);

    let mut incomplete_warm_cache = examples["trial"].clone();
    incomplete_warm_cache["cache_condition"]["prompt"] = json!({ "state": "warm" });
    invalid.push(incomplete_warm_cache);

    let mut remote_with_fixture_grant = examples["trial"].clone();
    remote_with_fixture_grant["task"]["repository"]["fixture_grant"] = json!("fixture-approved");
    invalid.push(remote_with_fixture_grant);

    for (index, instance) in invalid.iter().enumerate() {
        assert!(
            !trial_validator.is_valid(instance),
            "invalid trial case {index} unexpectedly validated"
        );
    }

    let mut completed_without_result = examples["outcome"].clone();
    completed_without_result["status"] = json!("completed");
    completed_without_result["provider_request_ids"] = json!(["request-1"]);
    assert!(!outcome_validator.is_valid(&completed_without_result));

    let mut pending_with_result = examples["outcome"].clone();
    pending_with_result["result"] = json!({});
    assert!(!outcome_validator.is_valid(&pending_with_result));

    let mut pending_with_request = examples["outcome"].clone();
    pending_with_request["provider_request_ids"] = json!(["request-1"]);
    assert!(!outcome_validator.is_valid(&pending_with_request));
}

#[test]
fn eval_manifest_semantic_loader_rejects_identity_and_repository_bypasses() {
    let schemas = schemas();
    let examples = examples();
    let registry = registry(&schemas);
    let policy = repos::RepositorySourcePolicy::default();
    let trial = &examples["trial"];
    let original_digest = canonical_identity_digest(&trial["identity"]);

    for (identity_field, component, component_field) in [
        ("task_id", "task", "task_id"),
        ("environment_id", "environment", "environment_id"),
        ("budget_id", "budget", "budget_id"),
        (
            "cache_condition_id",
            "cache_condition",
            "cache_condition_id",
        ),
        ("grader_id", "grader", "grader_id"),
    ] {
        let mut mismatch = trial.clone();
        mismatch[component][component_field] = json!("substituted-component");
        assert!(
            load_trial(&mismatch, &schemas, &registry, &policy).is_err(),
            "mismatched {component_field} was accepted"
        );

        let mut changed_identity = trial["identity"].clone();
        changed_identity[identity_field] = json!("substituted-identity");
        assert_ne!(
            canonical_identity_digest(&changed_identity),
            original_digest
        );
    }

    for identity_field in ["config_id", "randomization_id"] {
        let mut changed = trial.clone();
        changed["identity"][identity_field] = json!("substituted-identity");
        assert_ne!(
            canonical_identity_digest(&changed["identity"]),
            original_digest
        );
        assert!(load_trial(&changed, &schemas, &registry, &policy).is_err());
    }
    let mut changed_attempt = trial.clone();
    changed_attempt["identity"]["attempt"] = json!(1);
    assert_ne!(
        canonical_identity_digest(&changed_attempt["identity"]),
        original_digest
    );
    assert!(load_trial(&changed_attempt, &schemas, &registry, &policy).is_err());

    for (source, url) in [
        ("https", "file:///tmp/repo"),
        ("https", "https://attacker@example.com/repo"),
        ("https", "https://127.0.0.1/repo"),
        ("ssh", "ssh://[::1]/repo"),
    ] {
        let mut hostile = trial.clone();
        hostile["task"]["repository"]["source"] = json!(source);
        hostile["task"]["repository"]["url"] = json!(url);
        assert!(
            load_trial(&hostile, &schemas, &registry, &policy).is_err(),
            "repository bypass {url} was accepted"
        );
    }

    let fixture_policy = repos::RepositorySourcePolicy::new(["fixture-approved"]);
    let mut granted_private = trial.clone();
    granted_private["task"]["repository"] = json!({
        "source": "https",
        "url": "https://127.0.0.1/repo",
        "commit": "0123456789abcdef0123456789abcdef01234567",
        "fixture_grant": "fixture-approved"
    });
    assert!(load_trial(&granted_private, &schemas, &registry, &fixture_policy).is_err());
}
