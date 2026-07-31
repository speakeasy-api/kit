use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};

use crate::capabilities::kernel::identity::{Digest, DigestAlgorithm, SourceSchema, put_bytes};

pub const SCHEMA_PROJECTION_VERSION: u16 = 1;
pub const JSON_SCHEMA_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";
const MAX_SCHEMA_BYTES: usize = 1024 * 1024;
const MAX_DOCUMENTATION_BYTES: usize = 64 * 1024;
const MAX_TARGET_BYTES: usize = 128;
const MAX_PROFILE_KEYWORDS: usize = 256;
const MAX_FORM_CONTRACT_BYTES: usize = 64 * 1024;
const MAX_SCHEMA_DEPTH: usize = 128;
const MAX_SCHEMA_NODES: usize = 100_000;
const MAX_PROJECTIONS: usize = 64;
const MAX_PROJECTION_ATTEMPTS: usize = 128;
const MAX_POINTER_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct NormalizedSchema {
    source: SourceSchema,
    value: Value,
    dialect_digest: Digest,
    documentation_digest: Digest,
}

impl NormalizedSchema {
    pub fn ingest(
        source: impl AsRef<[u8]>,
        dialect: impl AsRef<str>,
        documentation: impl AsRef<[u8]>,
        algorithm: DigestAlgorithm,
    ) -> Result<Self, ProjectionError> {
        let source = source.as_ref();
        let dialect = dialect.as_ref();
        let documentation = documentation.as_ref();
        if source.is_empty()
            || source.len() > MAX_SCHEMA_BYTES
            || dialect.is_empty()
            || dialect.len() > MAX_TARGET_BYTES
            || documentation.len() > MAX_DOCUMENTATION_BYTES
        {
            return Err(ProjectionError::InvalidInput);
        }
        preflight_nodes(source)?;
        let value = serde_json::from_slice::<UniqueValue>(source)
            .map_err(|_| ProjectionError::InvalidJson)?
            .0;
        let mut nodes = 0;
        validate_shape(&value, 0, &mut nodes)?;
        if let Some(embedded) = value.get("$schema")
            && embedded.as_str() != Some(dialect)
        {
            return Err(ProjectionError::DialectMismatch);
        }
        if dialect == JSON_SCHEMA_2020_12 {
            jsonschema::draft202012::options()
                .build(&value)
                .map_err(|_| ProjectionError::InvalidSchema)?;
        }
        let normalized = serde_json::to_vec(&value).map_err(|_| ProjectionError::InvalidJson)?;
        let source_schema = SourceSchema::new(
            Arc::<[u8]>::from(source),
            Arc::<str>::from(dialect),
            Arc::<[u8]>::from(documentation),
            normalized,
            algorithm,
        )
        .map_err(|_| ProjectionError::InvalidInput)?;
        Ok(Self {
            dialect_digest: Digest::of(algorithm, dialect.as_bytes()),
            documentation_digest: Digest::of(algorithm, documentation),
            source: source_schema,
            value,
        })
    }

    pub const fn source(&self) -> &SourceSchema {
        &self.source
    }

    pub const fn value(&self) -> &Value {
        &self.value
    }

    pub const fn dialect_digest(&self) -> Digest {
        self.dialect_digest
    }

    pub const fn documentation_digest(&self) -> Digest {
        self.documentation_digest
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProjectionTarget {
    provider: String,
    model: String,
    adapter: String,
    profile_version: u16,
}

impl ProjectionTarget {
    pub fn new(
        provider: impl AsRef<str>,
        model: impl AsRef<str>,
        adapter: impl AsRef<str>,
        profile_version: u16,
    ) -> Result<Self, ProjectionError> {
        let provider = provider.as_ref();
        let model = model.as_ref();
        let adapter = adapter.as_ref();
        if profile_version == 0
            || [provider, model, adapter]
                .into_iter()
                .any(|part| !valid_text(part, MAX_TARGET_BYTES))
        {
            return Err(ProjectionError::InvalidTarget);
        }
        let target = Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
            adapter: adapter.to_owned(),
            profile_version,
        };
        Ok(target)
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    pub const fn profile_version(&self) -> u16 {
        self.profile_version
    }
}

#[derive(Clone, Debug)]
pub struct ProjectionProfile {
    target: ProjectionTarget,
    dialect: String,
    keywords: BTreeSet<String>,
    form_validator: jsonschema::Validator,
    max_schema_bytes: usize,
    digest: Digest,
}

impl ProjectionProfile {
    pub fn new(
        target: ProjectionTarget,
        dialect: impl AsRef<str>,
        keywords: BTreeSet<String>,
        form_contract: Value,
        max_schema_bytes: usize,
        algorithm: DigestAlgorithm,
    ) -> Result<Self, ProjectionError> {
        let dialect = dialect.as_ref();
        let mut form_nodes = 0;
        validate_shape(&form_contract, 0, &mut form_nodes)?;
        if json_weight(&form_contract, MAX_FORM_CONTRACT_BYTES)? > MAX_FORM_CONTRACT_BYTES {
            return Err(ProjectionError::InvalidProfile);
        }
        let form_contract = canonical_value(form_contract);
        let form_contract_bytes =
            serde_json::to_vec(&form_contract).map_err(|_| ProjectionError::InvalidProfile)?;
        if !valid_text(dialect, MAX_TARGET_BYTES)
            || keywords.is_empty()
            || keywords.len() > MAX_PROFILE_KEYWORDS
            || keywords
                .iter()
                .any(|keyword| !KNOWN_KEYWORDS.contains(&keyword.as_str()))
            || keywords.contains("$ref")
            || keywords.contains("$dynamicRef")
            || max_schema_bytes == 0
            || max_schema_bytes > MAX_SCHEMA_BYTES
            || form_contract_bytes.len() > MAX_FORM_CONTRACT_BYTES
        {
            return Err(ProjectionError::InvalidProfile);
        }
        let form_validator = jsonschema::draft202012::options()
            .build(&form_contract)
            .map_err(|_| ProjectionError::InvalidProfile)?;
        let mut canonical = Vec::new();
        canonical.extend_from_slice(&SCHEMA_PROJECTION_VERSION.to_be_bytes());
        put_bytes(&mut canonical, target.provider.as_bytes());
        put_bytes(&mut canonical, target.model.as_bytes());
        put_bytes(&mut canonical, target.adapter.as_bytes());
        canonical.extend_from_slice(&target.profile_version.to_be_bytes());
        put_bytes(&mut canonical, dialect.as_bytes());
        canonical.extend_from_slice(&(max_schema_bytes as u64).to_be_bytes());
        put_bytes(&mut canonical, &form_contract_bytes);
        for keyword in &keywords {
            put_bytes(&mut canonical, keyword.as_bytes());
        }
        Ok(Self {
            target,
            dialect: dialect.to_owned(),
            keywords,
            form_validator,
            max_schema_bytes,
            digest: Digest::of(algorithm, &canonical),
        })
    }

    pub const fn target(&self) -> &ProjectionTarget {
        &self.target
    }

    pub const fn digest(&self) -> Digest {
        self.digest
    }
}

#[derive(Clone, Debug)]
pub struct ProviderSchemaProjection {
    schema: Arc<NormalizedSchema>,
    target: ProjectionTarget,
    profile_digest: Digest,
    digest: Digest,
}

impl ProviderSchemaProjection {
    pub fn schema(&self) -> &NormalizedSchema {
        &self.schema
    }

    pub const fn target(&self) -> &ProjectionTarget {
        &self.target
    }

    pub const fn profile_digest(&self) -> Digest {
        self.profile_digest
    }

    pub fn bytes(&self) -> &[u8] {
        self.schema.source().normalized_bytes()
    }

    pub fn value(&self) -> &Value {
        self.schema.value()
    }

    pub const fn digest(&self) -> Digest {
        self.digest
    }
}

pub struct SchemaProjectionSet {
    schema: Arc<NormalizedSchema>,
    projections: BTreeMap<ProjectionTarget, ProviderSchemaProjection>,
    attempts: usize,
}

impl SchemaProjectionSet {
    pub fn new(schema: NormalizedSchema) -> Self {
        Self {
            schema: Arc::new(schema),
            projections: BTreeMap::new(),
            attempts: 0,
        }
    }

    pub fn project(
        &mut self,
        profile: &ProjectionProfile,
    ) -> Result<&ProviderSchemaProjection, ProjectionError> {
        if let Some(digest) = self
            .projections
            .get(profile.target())
            .map(ProviderSchemaProjection::profile_digest)
        {
            if digest != profile.digest() {
                return Err(ProjectionError::ProjectionConflict);
            }
            return Ok(self
                .projections
                .get(profile.target())
                .expect("existing projection exists"));
        }
        if self.schema.source().dialect() != profile.dialect
            || profile.dialect != JSON_SCHEMA_2020_12
        {
            return Err(ProjectionError::UnsupportedDialect);
        }
        if self.schema.source().normalized_bytes().len() > profile.max_schema_bytes {
            return Err(ProjectionError::SchemaTooLarge);
        }
        if self.projections.len() >= MAX_PROJECTIONS {
            return Err(ProjectionError::LimitExceeded);
        }
        if self.attempts >= MAX_PROJECTION_ATTEMPTS {
            return Err(ProjectionError::LimitExceeded);
        }
        self.attempts += 1;
        if !profile.form_validator.is_valid(self.schema.value()) {
            return Err(ProjectionError::UnsupportedSchemaForm);
        }
        walk_schema(self.schema.value(), "", &profile.keywords)?;
        let projection = ProviderSchemaProjection {
            schema: Arc::clone(&self.schema),
            target: profile.target.clone(),
            profile_digest: profile.digest,
            digest: self.schema.source().normalized_digest(),
        };
        self.projections.insert(profile.target.clone(), projection);
        Ok(self
            .projections
            .get(profile.target())
            .expect("inserted projection exists"))
    }

    pub fn projection(&self, target: &ProjectionTarget) -> Option<&ProviderSchemaProjection> {
        self.projections.get(target)
    }

    pub fn len(&self) -> usize {
        self.projections.len()
    }

    pub fn is_empty(&self) -> bool {
        self.projections.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectionError {
    InvalidInput,
    InvalidJson,
    InvalidSchema,
    DialectMismatch,
    InvalidTarget,
    InvalidProfile,
    UnsupportedDialect,
    UnsupportedSchemaForm,
    UnsupportedConstraint { pointer: String, keyword: String },
    SchemaTooLarge,
    ProjectionConflict,
    LimitExceeded,
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedConstraint { pointer, keyword } => {
                write!(
                    formatter,
                    "unsupported schema constraint {keyword} at {pointer}"
                )
            }
            other => formatter.write_str(match other {
                Self::InvalidInput => "schema input is invalid",
                Self::InvalidJson => "schema JSON is invalid or contains duplicate keys",
                Self::InvalidSchema => "JSON Schema is invalid",
                Self::DialectMismatch => "declared and embedded schema dialects differ",
                Self::InvalidTarget => "projection target is invalid",
                Self::InvalidProfile => "projection profile is invalid",
                Self::UnsupportedDialect => {
                    "projection profile does not support the schema dialect"
                }
                Self::UnsupportedSchemaForm => {
                    "projection profile does not support this schema form"
                }
                Self::SchemaTooLarge => "schema exceeds the projection profile byte limit",
                Self::ProjectionConflict => "projection target already exists",
                Self::LimitExceeded => "schema depth or node limit exceeded",
                Self::UnsupportedConstraint { .. } => unreachable!(),
            }),
        }
    }
}

impl std::error::Error for ProjectionError {}

fn walk_schema(
    value: &Value,
    pointer: &str,
    supported: &BTreeSet<String>,
) -> Result<(), ProjectionError> {
    let Some(object) = value.as_object() else {
        return Ok(());
    };
    for (keyword, child) in object {
        let keyword_pointer = join_pointer(pointer, keyword)?;
        if !supported.contains(keyword) {
            return Err(ProjectionError::UnsupportedConstraint {
                pointer: keyword_pointer,
                keyword: keyword.clone(),
            });
        }
        match keyword.as_str() {
            "properties" | "patternProperties" | "$defs" | "dependentSchemas" => {
                if let Some(schemas) = child.as_object() {
                    for (name, schema) in schemas {
                        walk_schema(schema, &join_pointer(&keyword_pointer, name)?, supported)?;
                    }
                }
            }
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                if let Some(schemas) = child.as_array() {
                    for (index, schema) in schemas.iter().enumerate() {
                        let index_pointer = format!("{keyword_pointer}/{index}");
                        if index_pointer.len() > MAX_POINTER_BYTES {
                            return Err(ProjectionError::LimitExceeded);
                        }
                        walk_schema(schema, &index_pointer, supported)?;
                    }
                }
            }
            "additionalProperties"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "items"
            | "contains"
            | "not"
            | "if"
            | "then"
            | "else"
            | "propertyNames"
            | "contentSchema" => {
                walk_schema(child, &keyword_pointer, supported)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn preflight_nodes(bytes: &[u8]) -> Result<(), ProjectionError> {
    let mut nodes = 1_usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
        } else if byte == b'"' {
            in_string = true;
        } else if matches!(byte, b'-' | b'0'..=b'9') {
            let start = index;
            index += 1;
            while index < bytes.len()
                && matches!(bytes[index], b'+' | b'-' | b'.' | b'0'..=b'9' | b'E' | b'e')
            {
                index += 1;
            }
            if !number_is_lossless(&bytes[start..index]) {
                return Err(ProjectionError::InvalidJson);
            }
            continue;
        } else if matches!(byte, b'{' | b'[' | b',') {
            nodes = nodes.checked_add(1).ok_or(ProjectionError::LimitExceeded)?;
            if nodes > MAX_SCHEMA_NODES {
                return Err(ProjectionError::LimitExceeded);
            }
        }
        index += 1;
    }
    Ok(())
}

fn number_is_lossless(bytes: &[u8]) -> bool {
    let Ok(value) = std::str::from_utf8(bytes) else {
        return false;
    };
    if !value.contains(['.', 'e', 'E']) {
        return if value.starts_with('-') {
            value.parse::<i64>().is_ok()
        } else {
            value.parse::<u64>().is_ok()
        };
    }
    let Ok(parsed) = value.parse::<f64>() else {
        return false;
    };
    parsed.is_finite()
        && canonical_decimal(value)
            .zip(canonical_decimal(&parsed.to_string()))
            .is_some_and(|(source, normalized)| source == normalized)
}

fn canonical_decimal(value: &str) -> Option<(bool, String, i64)> {
    let (negative, value) = value
        .strip_prefix('-')
        .map_or((false, value), |value| (true, value));
    let (mantissa, exponent) = value
        .split_once(['e', 'E'])
        .map_or(Some((value, 0_i64)), |(mantissa, exponent)| {
            exponent.parse::<i64>().ok().map(|value| (mantissa, value))
        })?;
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return Some((false, "0".to_owned(), 0));
    }
    let trimmed = digits.trim_end_matches('0');
    let removed = digits.len().checked_sub(trimmed.len())?;
    let exponent = exponent
        .checked_sub(i64::try_from(fraction.len()).ok()?)?
        .checked_add(i64::try_from(removed).ok()?)?;
    Some((negative, trimmed.to_owned(), exponent))
}

fn validate_shape(value: &Value, depth: usize, nodes: &mut usize) -> Result<(), ProjectionError> {
    *nodes = nodes.checked_add(1).ok_or(ProjectionError::LimitExceeded)?;
    if depth > MAX_SCHEMA_DEPTH || *nodes > MAX_SCHEMA_NODES {
        return Err(ProjectionError::LimitExceeded);
    }
    match value {
        Value::Array(values) => {
            for value in values {
                validate_shape(value, depth + 1, nodes)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_shape(value, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn json_weight(value: &Value, limit: usize) -> Result<usize, ProjectionError> {
    fn add(total: &mut usize, value: usize, limit: usize) -> Result<(), ProjectionError> {
        *total = total
            .checked_add(value)
            .ok_or(ProjectionError::InvalidProfile)?;
        if *total > limit {
            return Err(ProjectionError::InvalidProfile);
        }
        Ok(())
    }

    fn visit(value: &Value, total: &mut usize, limit: usize) -> Result<(), ProjectionError> {
        match value {
            Value::Null => add(total, 4, limit),
            Value::Bool(_) => add(total, 5, limit),
            Value::Number(number) => add(total, number.to_string().len(), limit),
            Value::String(value) => add(total, value.len().saturating_mul(6) + 2, limit),
            Value::Array(values) => {
                add(total, values.len() + 2, limit)?;
                for value in values {
                    visit(value, total, limit)?;
                }
                Ok(())
            }
            Value::Object(values) => {
                add(total, values.len() + 2, limit)?;
                for (key, value) in values {
                    add(total, key.len().saturating_mul(6) + 3, limit)?;
                    visit(value, total, limit)?;
                }
                Ok(())
            }
        }
    }

    let mut total = 0;
    visit(value, &mut total, limit)?;
    Ok(total)
}

fn canonical_value(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_value).collect()),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(key, value)| (key, canonical_value(value)))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .collect(),
        ),
        value => value,
    }
}

fn join_pointer(base: &str, part: &str) -> Result<String, ProjectionError> {
    if base
        .len()
        .saturating_add(1)
        .saturating_add(part.len().saturating_mul(2))
        > MAX_POINTER_BYTES
    {
        return Err(ProjectionError::LimitExceeded);
    }
    Ok(format!(
        "{base}/{}",
        part.replace('~', "~0").replace('/', "~1")
    ))
}

fn valid_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty() && value.len() <= max_bytes && !value.chars().any(char::is_control)
}

const KNOWN_KEYWORDS: &[&str] = &[
    "$anchor",
    "$comment",
    "$defs",
    "$dynamicAnchor",
    "$dynamicRef",
    "$id",
    "$ref",
    "$schema",
    "$vocabulary",
    "additionalProperties",
    "allOf",
    "anyOf",
    "const",
    "contains",
    "contentEncoding",
    "contentMediaType",
    "contentSchema",
    "default",
    "dependentRequired",
    "dependentSchemas",
    "deprecated",
    "description",
    "else",
    "enum",
    "examples",
    "exclusiveMaximum",
    "exclusiveMinimum",
    "format",
    "if",
    "items",
    "maxContains",
    "maxItems",
    "maxLength",
    "maxProperties",
    "maximum",
    "minContains",
    "minItems",
    "minLength",
    "minProperties",
    "minimum",
    "multipleOf",
    "not",
    "oneOf",
    "pattern",
    "patternProperties",
    "prefixItems",
    "properties",
    "propertyNames",
    "readOnly",
    "required",
    "then",
    "title",
    "type",
    "unevaluatedItems",
    "unevaluatedProperties",
    "uniqueItems",
    "writeOnly",
];

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueVisitor)
    }
}

struct UniqueVisitor;

impl<'de> Visitor<'de> for UniqueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(Number::from(value))))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(Number::from(value))))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .map(UniqueValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<UniqueValue>()? {
            values.push(value.0);
        }
        Ok(UniqueValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut input: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = BTreeMap::new();
        while let Some((key, value)) = input.next_entry::<String, UniqueValue>()? {
            if values.insert(key, value.0).is_some() {
                return Err(serde::de::Error::custom("duplicate JSON object key"));
            }
        }
        Ok(UniqueValue(Value::Object(
            values.into_iter().collect::<Map<_, _>>(),
        )))
    }
}
