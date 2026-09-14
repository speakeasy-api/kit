//! Bounded, lossless discovery: the catalog remains canonical; projections never
//! enter the execution registry. References are content-derived, not stored state.
use super::*;

const PREVIEW_BYTES: usize = 512;
const INLINE_BYTES: usize = 4096;

fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Object(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

pub(super) struct DiscoveryScope {
    session: String,
    directory: PathBuf,
}

impl DiscoveryScope {
    pub(super) fn new(artifact_root: &Path, request: &ToolRequest) -> Self {
        Self {
            session: request.session_id.to_string(),
            directory: crate::artifacts::session_directory(
                &crate::artifacts::session_directory(artifact_root, &request.session_id.0),
                &request.call_id.0,
            ),
        }
    }

    #[cfg(test)]
    pub(super) fn testing() -> Self {
        Self {
            session: "test".into(),
            directory: std::env::temp_dir().join("kit-schema-tests"),
        }
    }

    fn reference(&self, server: &str, fingerprint: &[u8], spec: &ToolSpec) -> String {
        let schema = spec.input_schema.to_string();
        let mut digest = blake3::Hasher::new();
        for bytes in [
            self.session.as_bytes(),
            server.as_bytes(),
            fingerprint,
            spec.name.0.as_bytes(),
            spec.description.as_bytes(),
            schema.as_bytes(),
        ] {
            digest.update(&(bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        }
        format!("schema_v1_{}", digest.finalize().to_hex())
    }

    // All callers run on a blocking task. ArtifactTool enforces session ownership
    // and reads both disk-backed and resilient-filesystem retained bytes.
    fn text(&self, output: &mut Value, field: &str, text: &str) -> Result<(), ToolError> {
        let mut end = text.len().min(PREVIEW_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        output[field] = Value::from(&text[..end]);
        if end != text.len() {
            let digest = blake3::hash(text.as_bytes());
            let path = crate::artifacts::write(
                &self
                    .directory
                    .join(format!("schema-{}.txt", digest.to_hex())),
                text.as_bytes(),
            )
            .map_err(|error| {
                ToolError::ExecutionFailed(format!("could not retain schema text: {error}"))
            })?;
            output[format!("{field}_truncated")] = Value::Bool(true);
            output[format!("{field}_artifact")] = object([
                (
                    "path",
                    path.to_str()
                        .ok_or_else(|| {
                            ToolError::ExecutionFailed("schema artifact path is not UTF-8".into())
                        })?
                        .into(),
                ),
                ("total_bytes", text.len().into()),
            ]);
        }
        Ok(())
    }
}

pub(super) fn search_entry(
    scope: &DiscoveryScope,
    server: &str,
    fingerprint: &[u8],
    spec: &ToolSpec,
) -> Result<Value, ToolError> {
    let mut entry = Value::Object(Map::new());
    scope.text(&mut entry, "name", &spec.name.0)?;
    scope.text(&mut entry, "description", &spec.description)?;
    if spec.input_schema.to_string().len() <= INLINE_BYTES
        && !has_long_description(&spec.input_schema)
    {
        entry["input_schema"] = spec.input_schema.clone();
    } else {
        entry["schema_ref"] = Value::from(scope.reference(server, fingerprint, spec));
        entry["schema_incomplete"] = Value::Bool(true);
    }
    Ok(entry)
}

// A small schema may still contain disproportionately long prose. Defer it to
// the walker too, so nested descriptions use the same lossless artifact path.
fn has_long_description(value: &Value) -> bool {
    match value {
        Value::Object(values) => values.iter().any(|(key, value)| {
            (key == "description"
                && value
                    .as_str()
                    .is_some_and(|text| text.len() > PREVIEW_BYTES))
                || has_long_description(value)
        }),
        Value::Array(values) => values.iter().any(has_long_description),
        _ => false,
    }
}

#[derive(Clone)]
pub struct ToolSchema {
    runtime: McpRuntime,
    artifact_root: PathBuf,
    spec: ToolSpec,
}

impl ToolSchema {
    pub fn new(runtime: McpRuntime, artifact_root: PathBuf) -> Self {
        let string = object([("type", "string".into())]);
        let integer = object([("type", "integer".into())]);
        let boolean = object([("type", "boolean".into())]);
        let node = object([("type", "object".into())]);
        let input_schema = object([
            ("type", "object".into()),
            (
                "properties",
                object([
                    ("schema_ref", string.clone()),
                    (
                        "pointer",
                        object([("type", "string".into()), ("default", "".into())]),
                    ),
                    (
                        "offset",
                        object([
                            ("type", "integer".into()),
                            ("minimum", 0.into()),
                            ("default", 0.into()),
                        ]),
                    ),
                    (
                        "limit",
                        object([
                            ("type", "integer".into()),
                            ("minimum", 1.into()),
                            ("maximum", 32.into()),
                            ("default", 16.into()),
                        ]),
                    ),
                ]),
            ),
            ("required", vec![Value::from("schema_ref")].into()),
            ("additionalProperties", false.into()),
        ]);
        let output_schema = object([
            ("type", "object".into()),
            (
                "properties",
                object([
                    ("schema_ref", string.clone()),
                    ("pointer", string),
                    ("node", node.clone()),
                    (
                        "children",
                        object([("type", "array".into()), ("items", node)]),
                    ),
                    ("total_children", integer.clone()),
                    ("next_offset", integer),
                    ("incomplete", boolean.clone()),
                    ("children_incomplete", boolean),
                ]),
            ),
            (
                "required",
                [
                    "schema_ref",
                    "node",
                    "children",
                    "total_children",
                    "incomplete",
                    "children_incomplete",
                ]
                .into_iter()
                .map(Value::from)
                .collect::<Vec<_>>()
                .into(),
            ),
        ]);
        Self {
            runtime,
            artifact_root,
            spec: ToolSpec::new(ToolName::new("tool_schema"),
                "Walk a canonical MCP input schema using a session-scoped schema_ref from tool_search. JSON Pointer defaults to root (empty string). Returns shallow children with pointers; offset/limit page wide nodes. Follow child pointers to descend. References ($ref, $dynamicRef, etc.) remain literal, never recursively expanded. Incomplete projections are NOT executable schemas. Long descriptions and other strings have previews and artifact references; use artifact to read the full original text. Stale or out-of-scope references require a new tool_search.",
                input_schema).with_output_schema(output_schema),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaInput {
    schema_ref: String,
    #[serde(default)]
    pointer: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    16
}

#[async_trait]
impl Tool for ToolSchema {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: SchemaInput = serde_json::from_value(request.input.clone())
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        if !(1..=32).contains(&input.limit) {
            return Err(ToolError::InvalidInput(
                "limit must be between 1 and 32".into(),
            ));
        }
        let scope = DiscoveryScope::new(&self.artifact_root, &request);
        let (records, available) = self.runtime.discovery_catalog().await?;
        let output = tokio::task::spawn_blocking(move || {
            for (server, tools) in available {
                for spec in tools {
                    if scope.reference(&server, &records[&server].fingerprint, &spec)
                        == input.schema_ref
                    {
                        return walk(&scope, &spec.input_schema, &input);
                    }
                }
            }
            Err(ToolError::InvalidInput(
                "stale or out-of-scope schema_ref; run tool_search again".into(),
            ))
        })
        .await
        .map_err(|error| ToolError::ExecutionFailed(error.to_string()))??;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(output),
        )))
    }
}

fn summary(scope: &DiscoveryScope, value: &Value, pointer: &str) -> Result<Value, ToolError> {
    let kind = match value {
        Value::Object(_) => "object",
        Value::Array(_) => "array",
        Value::String(_) => "string",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::Null => "null",
    };
    let mut result = object([("kind", kind.into()), ("incomplete", false.into())]);
    scope.text(&mut result, "pointer", pointer)?;
    match value {
        Value::Object(children) => {
            result["child_count"] = children.len().into();
            result["incomplete"] = (!children.is_empty()).into();
            if let Some(Value::String(description)) = children.get("description") {
                scope.text(&mut result, "description", description)?;
            }
        }
        Value::Array(children) => {
            result["child_count"] = children.len().into();
            result["incomplete"] = (!children.is_empty()).into();
        }
        Value::String(text) => {
            let field = if pointer.ends_with("/description") {
                "description"
            } else {
                "value"
            };
            scope.text(&mut result, field, text)?;
            result["incomplete"] = (text.len() > PREVIEW_BYTES).into();
        }
        scalar => result["value"] = scalar.clone(),
    }
    result["incomplete"] =
        (result["incomplete"] == true || result["pointer_truncated"] == true).into();
    Ok(result)
}

fn walk(scope: &DiscoveryScope, schema: &Value, input: &SchemaInput) -> Result<Value, ToolError> {
    // serde_json's pointer walker accepts ~ escapes permissively; reject malformed
    // RFC 6901 tokens explicitly rather than silently addressing a different key.
    if (!input.pointer.is_empty() && !input.pointer.starts_with('/'))
        || input
            .pointer
            .split('~')
            .skip(1)
            .any(|part| !part.starts_with(['0', '1']))
    {
        return Err(ToolError::InvalidInput("invalid JSON Pointer".into()));
    }
    let node = schema
        .pointer(&input.pointer)
        .ok_or_else(|| ToolError::InvalidInput("JSON Pointer does not exist".into()))?;
    let total = match node {
        Value::Object(v) => v.len(),
        Value::Array(v) => v.len(),
        _ => 0,
    };
    if input.offset > total {
        return Err(ToolError::InvalidInput("offset exceeds child count".into()));
    }
    let mut output = object([
        ("schema_ref", input.schema_ref.clone().into()),
        ("node", summary(scope, node, &input.pointer)?),
        ("children", Value::Array(Vec::new())),
        ("total_children", total.into()),
        ("incomplete", false.into()),
        ("children_incomplete", false.into()),
    ]);
    scope.text(&mut output, "pointer", &input.pointer)?;
    let entries: Box<dyn Iterator<Item = (String, &Value)> + '_> = match node {
        Value::Object(values) => Box::new(
            values
                .iter()
                .skip(input.offset)
                .take(input.limit)
                .map(|(key, value)| (key.replace('~', "~0").replace('/', "~1"), value)),
        ),
        Value::Array(values) => Box::new(
            values
                .iter()
                .enumerate()
                .skip(input.offset)
                .take(input.limit)
                .map(|(index, value)| (index.to_string(), value)),
        ),
        _ => Box::new(std::iter::empty()),
    };
    let mut children = Vec::new();
    for (key, value) in entries {
        let child = summary(scope, value, &format!("{}/{key}", input.pointer))?;
        // Reserve envelope/continuation room. Each single summary is bounded;
        // escaped UTF-8 and artifact paths count toward the actual JSON budget.
        let used = output.to_string().len()
            + children
                .iter()
                .map(|v: &Value| v.to_string().len() + 1)
                .sum::<usize>();
        if used + child.to_string().len() + 256 > SEARCH_RESULT_BYTE_CAP {
            break;
        }
        children.push(child);
    }
    let next = input.offset + children.len();
    let children_incomplete = input.offset != 0 || next < total;
    output["incomplete"] = (output["pointer_truncated"] == true
        || children_incomplete
        || children.iter().any(|v| v["incomplete"] == true)
        || (total == 0 && output["node"]["incomplete"] == true))
        .into();
    output["children_incomplete"] = children_incomplete.into();
    output["children"] = children.into();
    if next < total {
        if next == input.offset {
            return Err(ToolError::ExecutionFailed(
                "schema child exceeds response budget".into(),
            ));
        }
        output["next_offset"] = next.into();
    }
    if output.to_string().len() > SEARCH_RESULT_BYTE_CAP {
        return Err(ToolError::ExecutionFailed(
            "schema response exceeds byte cap".into(),
        ));
    }
    Ok(output)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn schema_walk_bounds_escaped_strings_and_long_pointers() {
        let directory = tempfile::tempdir().unwrap();
        let scope = DiscoveryScope {
            session: "bounded".into(),
            directory: directory.path().to_path_buf(),
        };
        let key = format!("~/{}", "é\u{0001}".repeat(1000));
        let object = Value::Object(
            (0..40)
                .map(|index| {
                    (
                        format!("{key}{index}"),
                        json!({"description":"🦀\u{0001}".repeat(2000)}),
                    )
                })
                .collect(),
        );
        let input = SchemaInput {
            schema_ref: "ref".into(),
            pointer: String::new(),
            offset: 0,
            limit: 32,
        };
        let result = walk(&scope, &object, &input).unwrap();
        assert!(result.to_string().len() <= SEARCH_RESULT_BYTE_CAP);
        assert!(result["next_offset"].as_u64().unwrap() > 0);
        let child = &result["children"][0];
        assert_eq!(child["pointer_truncated"], true);
        assert_eq!(child["description_truncated"], true);
        assert_eq!(child["description_artifact"]["total_bytes"], 10000);
        assert_eq!(result["incomplete"], true);
        assert_eq!(object.as_object().unwrap().len(), 40);
    }

    #[test]
    fn schema_walk_marks_truncated_scalar_pointers_incomplete() {
        let directory = tempfile::tempdir().unwrap();
        let scope = DiscoveryScope {
            session: "pointer".into(),
            directory: directory.path().to_path_buf(),
        };
        let schema = json!({"x".repeat(600): true});
        let result = walk(
            &scope,
            &schema,
            &SchemaInput {
                schema_ref: "ref".into(),
                pointer: String::new(),
                offset: 0,
                limit: 16,
            },
        )
        .unwrap();
        assert_eq!(result["children"][0]["pointer_truncated"], true);
        assert_eq!(result["children"][0]["incomplete"], true);
        assert_eq!(result["incomplete"], true);
    }

    #[test]
    fn schema_walk_validates_pointers_and_offsets() {
        let scope = DiscoveryScope::testing();
        let schema = json!({"~/":{"enum":["one",false,null]},"ref":{"$ref":"#/ref"},"empty":{}});
        for pointer in ["not-root", "/~", "/~2", "/missing", "/~0~1/enum/01"] {
            assert!(
                walk(
                    &scope,
                    &schema,
                    &SchemaInput {
                        schema_ref: "ref".into(),
                        pointer: pointer.into(),
                        offset: 0,
                        limit: 16
                    }
                )
                .is_err(),
                "{pointer}"
            );
        }
        let result = walk(
            &scope,
            &schema,
            &SchemaInput {
                schema_ref: "ref".into(),
                pointer: "/~0~1/enum/1".into(),
                offset: 0,
                limit: 16,
            },
        )
        .unwrap();
        assert_eq!(result["node"]["value"], false);
        assert_eq!(result["incomplete"], false);
        let result = walk(
            &scope,
            &schema,
            &SchemaInput {
                schema_ref: "ref".into(),
                pointer: "/ref".into(),
                offset: 0,
                limit: 16,
            },
        )
        .unwrap();
        assert_eq!(result["children"][0]["value"], "#/ref");
        assert_eq!(result["incomplete"], false);
    }
}
