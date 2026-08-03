pub(crate) mod discovery;
mod payload;

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
};

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::capabilities::{
    catalog::{CapabilityKind, CatalogSchemas},
    kernel::identity::{Digest, DigestAlgorithm},
    schema::{JSON_SCHEMA_2020_12, NormalizedSchema, ProjectionError, SchemaProjectionSet},
};

pub use discovery::{
    DiscoveredFeatures, DiscoveryError, FeatureListKind, McpCatalog, McpCatalogConfig,
    McpCatalogPolicy, McpCatalogPolicyKey, NegotiatedFeatureKinds, RefreshCoalescer, RefreshLimits,
    RefreshTicket,
};
pub use payload::{PayloadError, PayloadLimits, RawPayload};

const MAX_DESCRIPTOR_NAME_BYTES: usize = 256;
const MAX_RESOURCE_URI_BYTES: usize = 16 * 1024;
const MAX_TEMPLATE_VARIABLES: usize = 128;
const MAX_PROMPT_ARGUMENTS: usize = 256;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConfiguredServerIdentity(Arc<str>);

impl ConfiguredServerIdentity {
    pub fn new(value: impl AsRef<str>) -> Result<Self, FeatureError> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(FeatureError::InvalidDescriptor(
                "configured server identity",
            ));
        }
        Ok(Self(Arc::from(value)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub struct DescriptorSource {
    payload: RawPayload,
}

impl DescriptorSource {
    fn from_value(value: Value, limits: PayloadLimits) -> Result<Self, FeatureError> {
        Ok(Self {
            payload: RawPayload::from_value(value, limits)?,
        })
    }

    pub fn bytes(&self) -> &[u8] {
        self.payload.canonical_bytes()
    }

    pub const fn digest(&self) -> Digest {
        self.payload.canonical_digest()
    }

    pub const fn value(&self) -> &Value {
        self.payload.value()
    }
}

#[derive(Clone, Debug)]
pub struct ToolDescriptor {
    server: ConfiguredServerIdentity,
    name: String,
    title: Option<String>,
    description: Option<String>,
    input_schema: Value,
    output_schema: Option<Value>,
    /// Untrusted server advice retained for inspection, never policy authority.
    annotations: Option<Value>,
    source: DescriptorSource,
}

#[derive(Clone, Debug)]
pub struct ResourceDescriptor {
    server: ConfiguredServerIdentity,
    uri: String,
    name: String,
    title: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    size: Option<u64>,
    /// Untrusted server advice retained for inspection, never policy authority.
    annotations: Option<Value>,
    source: DescriptorSource,
}

#[derive(Clone, Debug)]
pub struct ResourceTemplateDescriptor {
    server: ConfiguredServerIdentity,
    uri_template: String,
    name: String,
    title: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    /// Untrusted server advice retained for inspection, never policy authority.
    annotations: Option<Value>,
    source: DescriptorSource,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct PromptArgument {
    name: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    required: Option<bool>,
}

impl PromptArgument {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn required(&self) -> bool {
        self.required == Some(true)
    }
}

#[derive(Clone, Debug)]
pub struct PromptDescriptor {
    server: ConfiguredServerIdentity,
    name: String,
    title: Option<String>,
    description: Option<String>,
    arguments: Vec<PromptArgument>,
    source: DescriptorSource,
}

#[derive(Clone, Debug)]
pub struct FeaturePage<T> {
    server: ConfiguredServerIdentity,
    items: Vec<T>,
    next_cursor: Option<String>,
    payload: RawPayload,
}

impl<T> FeaturePage<T> {
    pub const fn server(&self) -> &ConfiguredServerIdentity {
        &self.server
    }

    pub fn items(&self) -> &[T] {
        &self.items
    }

    pub fn into_items(self) -> Vec<T> {
        self.items
    }

    pub fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }

    pub const fn payload(&self) -> &RawPayload {
        &self.payload
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FeatureIdentity {
    Tool(ConfiguredServerIdentity, String),
    StaticResource(ConfiguredServerIdentity, String),
    ResourceTemplate(ConfiguredServerIdentity, String),
    Prompt(ConfiguredServerIdentity, String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeatureMetadataTrust {
    UntrustedServer,
}

#[derive(Clone, Debug)]
pub struct NormalizedFeature {
    kind: CapabilityKind,
    identity: FeatureIdentity,
    input: NormalizedSchema,
    output: Option<NormalizedSchema>,
    descriptor_digest: Digest,
    metadata_trust: FeatureMetadataTrust,
}

impl NormalizedFeature {
    pub const fn kind(&self) -> CapabilityKind {
        self.kind
    }

    pub const fn identity(&self) -> &FeatureIdentity {
        &self.identity
    }

    pub const fn input(&self) -> &NormalizedSchema {
        &self.input
    }

    pub const fn output(&self) -> Option<&NormalizedSchema> {
        self.output.as_ref()
    }

    pub const fn descriptor_digest(&self) -> Digest {
        self.descriptor_digest
    }

    pub const fn metadata_trust(&self) -> FeatureMetadataTrust {
        self.metadata_trust
    }

    pub fn catalog_schemas(&self) -> CatalogSchemas {
        CatalogSchemas::new(
            SchemaProjectionSet::new(self.input.clone()),
            self.output.clone().map(SchemaProjectionSet::new),
        )
    }
}

impl ToolDescriptor {
    pub const fn server(&self) -> &ConfiguredServerIdentity {
        &self.server
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    pub const fn output_schema(&self) -> Option<&Value> {
        self.output_schema.as_ref()
    }

    pub const fn annotations(&self) -> Option<&Value> {
        self.annotations.as_ref()
    }

    pub const fn source(&self) -> &DescriptorSource {
        &self.source
    }

    pub fn identity(&self) -> FeatureIdentity {
        FeatureIdentity::Tool(self.server.clone(), self.name.clone())
    }

    pub fn normalize(&self) -> Result<NormalizedFeature, FeatureError> {
        Ok(NormalizedFeature {
            kind: CapabilityKind::Tool,
            identity: FeatureIdentity::Tool(self.server.clone(), self.name.clone()),
            input: normalize_schema(&self.input_schema, documentation(self))?,
            output: self
                .output_schema
                .as_ref()
                .map(|schema| normalize_schema(schema, documentation(self)))
                .transpose()?,
            descriptor_digest: self.source.digest(),
            metadata_trust: FeatureMetadataTrust::UntrustedServer,
        })
    }
}

impl ResourceDescriptor {
    pub const fn server(&self) -> &ConfiguredServerIdentity {
        &self.server
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn mime_type(&self) -> Option<&str> {
        self.mime_type.as_deref()
    }

    pub const fn size(&self) -> Option<u64> {
        self.size
    }

    pub const fn annotations(&self) -> Option<&Value> {
        self.annotations.as_ref()
    }

    pub const fn source(&self) -> &DescriptorSource {
        &self.source
    }

    pub fn identity(&self) -> FeatureIdentity {
        FeatureIdentity::StaticResource(self.server.clone(), self.uri.clone())
    }

    pub fn normalize(&self) -> Result<NormalizedFeature, FeatureError> {
        validate_resource_uri(&self.uri)?;
        normalize_derived(
            CapabilityKind::Resource,
            FeatureIdentity::StaticResource(self.server.clone(), self.uri.clone()),
            static_resource_schema(&self.uri),
            self.description.as_deref().unwrap_or_default(),
            self.source.digest(),
        )
    }
}

impl ResourceTemplateDescriptor {
    pub const fn server(&self) -> &ConfiguredServerIdentity {
        &self.server
    }

    pub fn uri_template(&self) -> &str {
        &self.uri_template
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn mime_type(&self) -> Option<&str> {
        self.mime_type.as_deref()
    }

    pub const fn annotations(&self) -> Option<&Value> {
        self.annotations.as_ref()
    }

    pub const fn source(&self) -> &DescriptorSource {
        &self.source
    }

    pub fn identity(&self) -> FeatureIdentity {
        FeatureIdentity::ResourceTemplate(self.server.clone(), self.uri_template.clone())
    }

    pub fn normalize(&self) -> Result<NormalizedFeature, FeatureError> {
        validate_uri_template(&self.uri_template)?;
        normalize_derived(
            CapabilityKind::ResourceTemplate,
            FeatureIdentity::ResourceTemplate(self.server.clone(), self.uri_template.clone()),
            template_resource_schema(&self.uri_template)?,
            self.description.as_deref().unwrap_or_default(),
            self.source.digest(),
        )
    }
}

impl PromptDescriptor {
    pub const fn server(&self) -> &ConfiguredServerIdentity {
        &self.server
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    pub fn arguments(&self) -> &[PromptArgument] {
        &self.arguments
    }

    pub const fn source(&self) -> &DescriptorSource {
        &self.source
    }

    pub fn identity(&self) -> FeatureIdentity {
        FeatureIdentity::Prompt(self.server.clone(), self.name.clone())
    }

    pub fn normalize(&self) -> Result<NormalizedFeature, FeatureError> {
        let mut names = BTreeSet::new();
        let mut properties = Map::new();
        let mut required = Vec::new();
        for argument in &self.arguments {
            if argument.name.is_empty()
                || argument.name.len() > MAX_DESCRIPTOR_NAME_BYTES
                || argument.name.chars().any(char::is_control)
                || !names.insert(argument.name.clone())
            {
                return Err(FeatureError::InvalidDescriptor("prompt argument name"));
            }
            let mut property = Map::from_iter([("type".to_owned(), json!("string"))]);
            if let Some(title) = &argument.title {
                property.insert("title".to_owned(), json!(title));
            }
            if let Some(description) = &argument.description {
                property.insert("description".to_owned(), json!(description));
            }
            properties.insert(argument.name.clone(), Value::Object(property));
            if argument.required == Some(true) {
                required.push(argument.name.clone());
            }
        }
        normalize_derived(
            CapabilityKind::Prompt,
            FeatureIdentity::Prompt(self.server.clone(), self.name.clone()),
            json!({
                "$schema": JSON_SCHEMA_2020_12,
                "additionalProperties": false,
                "properties": properties,
                "required": required,
                "type": "object"
            }),
            self.description.as_deref().unwrap_or_default(),
            self.source.digest(),
        )
    }
}

pub fn decode_tools_page(
    server: &ConfiguredServerIdentity,
    bytes: impl AsRef<[u8]>,
    limits: PayloadLimits,
) -> Result<FeaturePage<ToolDescriptor>, FeatureError> {
    decode_page(server, bytes, limits, "tools", decode_tool)
}

pub fn decode_resources_page(
    server: &ConfiguredServerIdentity,
    bytes: impl AsRef<[u8]>,
    limits: PayloadLimits,
) -> Result<FeaturePage<ResourceDescriptor>, FeatureError> {
    decode_page(server, bytes, limits, "resources", decode_resource)
}

pub fn decode_resource_templates_page(
    server: &ConfiguredServerIdentity,
    bytes: impl AsRef<[u8]>,
    limits: PayloadLimits,
) -> Result<FeaturePage<ResourceTemplateDescriptor>, FeatureError> {
    decode_page(
        server,
        bytes,
        limits,
        "resourceTemplates",
        decode_resource_template,
    )
}

pub fn decode_prompts_page(
    server: &ConfiguredServerIdentity,
    bytes: impl AsRef<[u8]>,
    limits: PayloadLimits,
) -> Result<FeaturePage<PromptDescriptor>, FeatureError> {
    decode_page(server, bytes, limits, "prompts", decode_prompt)
}

#[derive(Debug)]
pub enum FeatureError {
    Payload(PayloadError),
    Schema(ProjectionError),
    MissingField(&'static str),
    InvalidDescriptor(&'static str),
}

impl fmt::Display for FeatureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Payload(error) => error.fmt(formatter),
            Self::Schema(error) => error.fmt(formatter),
            Self::MissingField(field) => write!(formatter, "MCP payload is missing {field}"),
            Self::InvalidDescriptor(field) => write!(formatter, "invalid MCP {field}"),
        }
    }
}

impl std::error::Error for FeatureError {}

impl From<PayloadError> for FeatureError {
    fn from(value: PayloadError) -> Self {
        Self::Payload(value)
    }
}

impl From<ProjectionError> for FeatureError {
    fn from(value: ProjectionError) -> Self {
        Self::Schema(value)
    }
}

fn decode_page<T>(
    server: &ConfiguredServerIdentity,
    bytes: impl AsRef<[u8]>,
    limits: PayloadLimits,
    field: &'static str,
    decode: fn(&ConfiguredServerIdentity, &Value, PayloadLimits) -> Result<T, FeatureError>,
) -> Result<FeaturePage<T>, FeatureError> {
    let payload = RawPayload::parse(bytes, limits)?;
    let result = result_object(payload.value())?;
    let items = result
        .get(field)
        .and_then(Value::as_array)
        .ok_or(FeatureError::MissingField(field))?
        .iter()
        .map(|value| decode(server, value, limits))
        .collect::<Result<Vec<_>, _>>()?;
    let next_cursor = optional_string(result, "nextCursor")?;
    Ok(FeaturePage {
        server: server.clone(),
        items,
        next_cursor,
        payload,
    })
}

fn decode_tool(
    server: &ConfiguredServerIdentity,
    value: &Value,
    limits: PayloadLimits,
) -> Result<ToolDescriptor, FeatureError> {
    let object = object(value)?;
    Ok(ToolDescriptor {
        server: server.clone(),
        name: required_bounded_string(object, "name", MAX_DESCRIPTOR_NAME_BYTES)?,
        title: optional_string(object, "title")?,
        description: optional_string(object, "description")?,
        input_schema: object
            .get("inputSchema")
            .filter(|schema| schema.is_object())
            .cloned()
            .ok_or(FeatureError::MissingField("inputSchema"))?,
        output_schema: object
            .get("outputSchema")
            .map(|schema| {
                schema
                    .is_object()
                    .then(|| schema.clone())
                    .ok_or(FeatureError::InvalidDescriptor("tool output schema"))
            })
            .transpose()?,
        annotations: optional_object_value(object, "annotations")?,
        source: DescriptorSource::from_value(value.clone(), limits)?,
    })
}

fn decode_resource(
    server: &ConfiguredServerIdentity,
    value: &Value,
    limits: PayloadLimits,
) -> Result<ResourceDescriptor, FeatureError> {
    let object = object(value)?;
    Ok(ResourceDescriptor {
        server: server.clone(),
        uri: required_bounded_string(object, "uri", MAX_RESOURCE_URI_BYTES)?,
        name: required_bounded_string(object, "name", MAX_DESCRIPTOR_NAME_BYTES)?,
        title: optional_string(object, "title")?,
        description: optional_string(object, "description")?,
        mime_type: optional_string(object, "mimeType")?,
        size: object
            .get("size")
            .map(|value| {
                value
                    .as_u64()
                    .ok_or(FeatureError::InvalidDescriptor("resource size"))
            })
            .transpose()?,
        annotations: optional_object_value(object, "annotations")?,
        source: DescriptorSource::from_value(value.clone(), limits)?,
    })
}

fn decode_resource_template(
    server: &ConfiguredServerIdentity,
    value: &Value,
    limits: PayloadLimits,
) -> Result<ResourceTemplateDescriptor, FeatureError> {
    let object = object(value)?;
    Ok(ResourceTemplateDescriptor {
        server: server.clone(),
        uri_template: required_bounded_string(object, "uriTemplate", MAX_RESOURCE_URI_BYTES)?,
        name: required_bounded_string(object, "name", MAX_DESCRIPTOR_NAME_BYTES)?,
        title: optional_string(object, "title")?,
        description: optional_string(object, "description")?,
        mime_type: optional_string(object, "mimeType")?,
        annotations: optional_object_value(object, "annotations")?,
        source: DescriptorSource::from_value(value.clone(), limits)?,
    })
}

fn decode_prompt(
    server: &ConfiguredServerIdentity,
    value: &Value,
    limits: PayloadLimits,
) -> Result<PromptDescriptor, FeatureError> {
    let object = object(value)?;
    let arguments = object
        .get("arguments")
        .map(|arguments| {
            if arguments
                .as_array()
                .is_none_or(|arguments| arguments.len() > MAX_PROMPT_ARGUMENTS)
            {
                return Err(FeatureError::InvalidDescriptor("prompt arguments"));
            }
            serde_json::from_value::<Vec<PromptArgument>>(arguments.clone())
                .map_err(|_| FeatureError::InvalidDescriptor("prompt arguments"))
        })
        .transpose()?
        .unwrap_or_default();
    Ok(PromptDescriptor {
        server: server.clone(),
        name: required_bounded_string(object, "name", MAX_DESCRIPTOR_NAME_BYTES)?,
        title: optional_string(object, "title")?,
        description: optional_string(object, "description")?,
        arguments,
        source: DescriptorSource::from_value(value.clone(), limits)?,
    })
}

fn normalize_schema(value: &Value, docs: &[u8]) -> Result<NormalizedSchema, FeatureError> {
    if !value.is_object() {
        return Err(FeatureError::InvalidDescriptor("schema"));
    }
    let dialect = match value.get("$schema") {
        None => JSON_SCHEMA_2020_12,
        Some(Value::String(dialect)) => dialect,
        Some(_) => return Err(FeatureError::InvalidDescriptor("schema dialect")),
    };
    let source =
        serde_json::to_vec(value).map_err(|_| FeatureError::InvalidDescriptor("schema"))?;
    let docs = frame_untrusted_metadata(&String::from_utf8_lossy(docs), 1024);
    Ok(NormalizedSchema::ingest(
        source,
        dialect,
        docs,
        DigestAlgorithm::Sha256,
    )?)
}

fn frame_untrusted_metadata(value: &str, maximum: usize) -> String {
    const PREFIX: &str = "UNTRUSTED_MCP_METADATA_JSON=";
    let mut body = String::from("\"");
    for character in value.chars() {
        let escaped = match character {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            character if character.is_control() => "\\uFFFD",
            _ => {
                if PREFIX.len() + 24 + body.len() + character.len_utf8() + 1 > maximum {
                    break;
                }
                body.push(character);
                continue;
            }
        };
        if PREFIX.len() + 24 + body.len() + escaped.len() + 1 > maximum {
            break;
        }
        body.push_str(escaped);
    }
    body.push('"');
    format!("{PREFIX}{}:{body}", body.len())
}

pub fn model_schema_projection(mut schema: Value) -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Array(values) => values.iter_mut().for_each(strip),
            Value::Object(values) => {
                values.retain(|name, _| {
                    !matches!(
                        name.as_str(),
                        "$comment"
                            | "default"
                            | "deprecated"
                            | "description"
                            | "examples"
                            | "readOnly"
                            | "title"
                            | "writeOnly"
                    ) && !name.starts_with("x-")
                });
                values.values_mut().for_each(strip);
            }
            _ => {}
        }
    }

    strip(&mut schema);
    schema
}

fn normalize_derived(
    kind: CapabilityKind,
    identity: FeatureIdentity,
    input: Value,
    documentation: &str,
    descriptor_digest: Digest,
) -> Result<NormalizedFeature, FeatureError> {
    Ok(NormalizedFeature {
        kind,
        identity,
        input: normalize_schema(&input, documentation.as_bytes())?,
        output: None,
        descriptor_digest,
        metadata_trust: FeatureMetadataTrust::UntrustedServer,
    })
}

fn static_resource_schema(uri: &str) -> Value {
    json!({
        "$schema": JSON_SCHEMA_2020_12,
        "additionalProperties": false,
        "properties": {"uri": {"const": uri, "type": "string"}},
        "required": ["uri"],
        "type": "object"
    })
}

fn validate_resource_uri(uri: &str) -> Result<(), FeatureError> {
    if uri.len() > MAX_RESOURCE_URI_BYTES
        || uri.chars().any(char::is_control)
        || url::Url::parse(uri).is_err()
    {
        return Err(FeatureError::InvalidDescriptor("resource URI"));
    }
    Ok(())
}

fn validate_uri_template(template: &str) -> Result<(), FeatureError> {
    if template.is_empty()
        || template.len() > MAX_RESOURCE_URI_BYTES
        || template
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(FeatureError::InvalidDescriptor("resource URI template"));
    }
    let scheme_end = template
        .find(':')
        .ok_or(FeatureError::InvalidDescriptor("resource URI template"))?;
    let scheme = &template[..scheme_end];
    if !scheme
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic())
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return Err(FeatureError::InvalidDescriptor("resource URI template"));
    }
    let bytes = template.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if !bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
                || !bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit)
            {
                return Err(FeatureError::InvalidDescriptor("resource URI template"));
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn template_resource_schema(template: &str) -> Result<Value, FeatureError> {
    let variables = template_variables(template)?;
    let properties = variables
        .iter()
        .map(|(name, scalar_only)| {
            (
                name.clone(),
                if *scalar_only {
                    json!({"type": "string"})
                } else {
                    json!({
                        "oneOf": [
                            {"type": "string"},
                            {"items": {"type": "string"}, "type": "array"},
                            {"additionalProperties": {"type": "string"}, "type": "object"}
                        ]
                    })
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    Ok(json!({
        "$schema": JSON_SCHEMA_2020_12,
        "additionalProperties": false,
        "properties": properties,
        "type": "object"
    }))
}

fn template_variables(template: &str) -> Result<Vec<(String, bool)>, FeatureError> {
    let mut output = BTreeMap::<String, bool>::new();
    let mut rest = template;
    loop {
        let start = rest.find('{');
        if rest
            .find('}')
            .is_some_and(|end| start.is_none_or(|start| end < start))
        {
            return Err(FeatureError::InvalidDescriptor("resource URI template"));
        }
        let Some(start) = start else {
            break;
        };
        rest = &rest[start + 1..];
        let end = rest
            .find('}')
            .ok_or(FeatureError::InvalidDescriptor("resource URI template"))?;
        let expression = &rest[..end];
        if expression.is_empty() || expression.contains('{') {
            return Err(FeatureError::InvalidDescriptor("resource URI template"));
        }
        let expression = expression
            .strip_prefix(['+', '#', '.', '/', ';', '?', '&'])
            .unwrap_or(expression);
        for variable in expression.split(',') {
            let exploded = variable.ends_with('*');
            let variable = variable.strip_suffix('*').unwrap_or(variable);
            let (variable, prefix) = variable
                .split_once(':')
                .map_or((variable, None), |(name, prefix)| (name, Some(prefix)));
            if !valid_template_variable(variable)
                || (exploded && prefix.is_some())
                || prefix.is_some_and(|prefix| {
                    prefix.is_empty()
                        || prefix.len() > 4
                        || prefix.starts_with('0')
                        || !prefix.bytes().all(|byte| byte.is_ascii_digit())
                })
            {
                return Err(FeatureError::InvalidDescriptor("resource URI template"));
            }
            output
                .entry(variable.to_owned())
                .and_modify(|scalar_only| *scalar_only |= prefix.is_some())
                .or_insert(prefix.is_some());
            if output.len() > MAX_TEMPLATE_VARIABLES {
                return Err(FeatureError::InvalidDescriptor(
                    "resource URI template variables",
                ));
            }
        }
        rest = &rest[end + 1..];
    }
    if rest.contains('}') {
        return Err(FeatureError::InvalidDescriptor("resource URI template"));
    }
    Ok(output.into_iter().collect())
}

fn valid_template_variable(value: &str) -> bool {
    if value.is_empty() || value.starts_with('.') || value.ends_with('.') {
        return false;
    }
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            byte if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.' => index += 1,
            b'%' if bytes.get(index + 1).is_some_and(u8::is_ascii_hexdigit)
                && bytes.get(index + 2).is_some_and(u8::is_ascii_hexdigit) =>
            {
                index += 3;
            }
            _ => return false,
        }
    }
    !value.contains("..")
}

fn result_object(value: &Value) -> Result<&Map<String, Value>, FeatureError> {
    let value = value.get("result").unwrap_or(value);
    object(value)
}

fn object(value: &Value) -> Result<&Map<String, Value>, FeatureError> {
    value
        .as_object()
        .ok_or(FeatureError::InvalidDescriptor("JSON object"))
}

fn required_string(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<String, FeatureError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or(FeatureError::MissingField(field))
}

fn required_bounded_string(
    object: &Map<String, Value>,
    field: &'static str,
    max_bytes: usize,
) -> Result<String, FeatureError> {
    let value = required_string(object, field)?;
    if value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(FeatureError::InvalidDescriptor(field));
    }
    Ok(value)
}

fn optional_string(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, FeatureError> {
    object
        .get(field)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or(FeatureError::InvalidDescriptor(field))
        })
        .transpose()
}

fn optional_object_value(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<Value>, FeatureError> {
    object
        .get(field)
        .map(|value| {
            value
                .is_object()
                .then(|| value.clone())
                .ok_or(FeatureError::InvalidDescriptor(field))
        })
        .transpose()
}

fn documentation(descriptor: &ToolDescriptor) -> &[u8] {
    descriptor
        .description
        .as_deref()
        .or(descriptor.title.as_deref())
        .unwrap_or_default()
        .as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> ConfiguredServerIdentity {
        ConfiguredServerIdentity::new("configured-test-server").unwrap()
    }

    #[test]
    fn four_kinds_normalize_with_dialect_output_absence_and_distinct_resources() {
        let tools = decode_tools_page(
            &server(),
            br#"{"tools":[{"name":"default","description":"docs","inputSchema":{"type":"object"}},{"name":"explicit","inputSchema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object"},"outputSchema":{"type":"object"}}]}"#,
            PayloadLimits::default(),
        )
        .unwrap();
        let default = tools.items[0].normalize().unwrap();
        assert_eq!(default.kind(), CapabilityKind::Tool);
        assert_eq!(default.input().source().dialect(), JSON_SCHEMA_2020_12);
        assert!(default.output().is_none());
        assert!(tools.items[1].normalize().unwrap().output().is_some());

        let resource = decode_resources_page(
            &server(),
            br#"{"resources":[{"uri":"file:///fixed","name":"fixed"}]}"#,
            PayloadLimits::default(),
        )
        .unwrap()
        .items
        .remove(0)
        .normalize()
        .unwrap();
        let template = decode_resource_templates_page(
            &server(),
            br#"{"resourceTemplates":[{"uriTemplate":"file:///{path}{?line}","name":"templated"}]}"#,
            PayloadLimits::default(),
        )
        .unwrap()
        .items
        .remove(0)
        .normalize()
        .unwrap();
        assert_eq!(resource.kind(), CapabilityKind::Resource);
        assert_eq!(template.kind(), CapabilityKind::ResourceTemplate);
        assert_ne!(resource.identity(), template.identity());
        assert!(template.input().value().get("required").is_none());

        let prompt = decode_prompts_page(
            &server(),
            br#"{"prompts":[{"name":"review","arguments":[{"name":"code","description":"Source","required":true},{"name":"tone"}]}]}"#,
            PayloadLimits::default(),
        )
        .unwrap()
        .items
        .remove(0)
        .normalize()
        .unwrap();
        assert_eq!(prompt.kind(), CapabilityKind::Prompt);
        assert_eq!(prompt.input().value()["additionalProperties"], false);
        assert_eq!(prompt.input().value()["required"], json!(["code"]));
    }

    #[test]
    fn schemas_fail_closed_and_descriptor_digest_covers_full_descriptor() {
        let unsupported = decode_tools_page(
            &server(),
            br#"{"tools":[{"name":"x","inputSchema":{"$schema":"draft-07","type":"object"}}]}"#,
            PayloadLimits::default(),
        )
        .unwrap();
        assert_eq!(
            unsupported.items[0]
                .normalize()
                .unwrap()
                .input()
                .source()
                .dialect(),
            "draft-07"
        );
        let mismatched = decode_tools_page(
            &server(),
            br#"{"tools":[{"name":"x","inputSchema":{"$schema":7,"type":"object"}}]}"#,
            PayloadLimits::default(),
        )
        .unwrap();
        assert!(mismatched.items[0].normalize().is_err());

        let first = decode_tools_page(
            &server(),
            br#"{"tools":[{"name":"x","description":"one","inputSchema":{"type":"object"},"future":{"kept":true}}]}"#,
            PayloadLimits::default(),
        )
        .unwrap();
        let second = decode_tools_page(
            &server(),
            br#"{"tools":[{"name":"x","description":"two","inputSchema":{"type":"object"},"future":{"kept":true}}]}"#,
            PayloadLimits::default(),
        )
        .unwrap();
        assert_ne!(
            first.items[0].source.digest(),
            second.items[0].source.digest()
        );
        assert_eq!(first.items[0].source.value()["future"]["kept"], true);
    }
}
