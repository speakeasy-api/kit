use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value};

#[derive(Clone)]
pub struct DocsTool {
    spec: ToolSpec,
}

impl DocsTool {
    pub fn new() -> Self {
        let input_schema = Value::Object(Map::from_iter([
            ("type".into(), Value::from("object")),
            (
                "properties".into(),
                Value::Object(Map::from_iter([(
                    "query".into(),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("string")),
                        ("minLength".into(), Value::from(1)),
                        ("maxLength".into(), Value::from(512)),
                        (
                            "description".into(),
                            Value::from(
                                "A free-text Kit question, error message, or feature name.",
                            ),
                        ),
                    ])),
                )])),
            ),
            ("required".into(), Value::Array(vec![Value::from("query")])),
            ("additionalProperties".into(), Value::from(false)),
        ]));
        let output_schema = Value::Object(Map::from_iter([
            ("type".into(), Value::from("object")),
            (
                "properties".into(),
                Value::Object(Map::from_iter([
                    (
                        "query".into(),
                        Value::Object(Map::from_iter([("type".into(), Value::from("string"))])),
                    ),
                    (
                        "version".into(),
                        Value::Object(Map::from_iter([("type".into(), Value::from("string"))])),
                    ),
                    (
                        "matches".into(),
                        Value::Object(Map::from_iter([
                            ("type".into(), Value::from("array")),
                            ("maxItems".into(), Value::from(5)),
                            (
                                "items".into(),
                                Value::Object(Map::from_iter([
                                    ("type".into(), Value::from("object")),
                                    (
                                        "properties".into(),
                                        Value::Object(Map::from_iter([
                                            (
                                                "path".into(),
                                                Value::Object(Map::from_iter([
                                                    ("type".into(), Value::from("string")),
                                                    ("maxLength".into(), Value::from(256)),
                                                ])),
                                            ),
                                            (
                                                "title".into(),
                                                Value::Object(Map::from_iter([
                                                    ("type".into(), Value::from("string")),
                                                    ("maxLength".into(), Value::from(256)),
                                                ])),
                                            ),
                                            (
                                                "section".into(),
                                                Value::Object(Map::from_iter([
                                                    ("type".into(), Value::from("string")),
                                                    ("maxLength".into(), Value::from(256)),
                                                ])),
                                            ),
                                            (
                                                "score".into(),
                                                Value::Object(Map::from_iter([(
                                                    "type".into(),
                                                    Value::from("integer"),
                                                )])),
                                            ),
                                            (
                                                "content".into(),
                                                Value::Object(Map::from_iter([
                                                    ("type".into(), Value::from("string")),
                                                    ("maxLength".into(), Value::from(1800)),
                                                ])),
                                            ),
                                        ])),
                                    ),
                                    (
                                        "required".into(),
                                        Value::Array(vec![
                                            Value::from("path"),
                                            Value::from("title"),
                                            Value::from("section"),
                                            Value::from("score"),
                                            Value::from("content"),
                                        ]),
                                    ),
                                    ("additionalProperties".into(), Value::from(false)),
                                ])),
                            ),
                        ])),
                    ),
                    (
                        "truncated".into(),
                        Value::Object(Map::from_iter([("type".into(), Value::from("boolean"))])),
                    ),
                ])),
            ),
            (
                "required".into(),
                Value::Array(vec![
                    Value::from("query"),
                    Value::from("version"),
                    Value::from("matches"),
                    Value::from("truncated"),
                ]),
            ),
            ("additionalProperties".into(), Value::from(false)),
        ]));
        Self {
            spec: ToolSpec::new(
                ToolName::new("docs"),
                "Search the version-matched Kit documentation bundled in this binary for questions or troubleshooting about Kit itself.",
                input_schema,
            )
            .with_output_schema(output_schema)
            .with_annotations(ToolAnnotations::new()),
        }
    }
}

impl Default for DocsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    query: String,
}

#[async_trait]
impl Tool for DocsTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: Input = serde_json::from_value(request.input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let result = crate::docs::bundled_search(&input.query)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let output =
            serde_json::to_value(result).map_err(|error| ToolError::Internal(error.to_string()))?;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(output),
        )))
    }
}
