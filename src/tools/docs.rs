use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

#[derive(Clone)]
pub struct DocsTool {
    spec: ToolSpec,
}

impl DocsTool {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                ToolName::new("docs"),
                "Search the version-matched Kit documentation bundled in this binary for questions or troubleshooting about Kit itself.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "minLength": 1,
                            "maxLength": 512,
                            "description": "A free-text Kit question, error message, or feature name."
                        }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
            )
            .with_output_schema(json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "version": {"type": "string"},
                    "matches": {
                        "type": "array",
                        "maxItems": 5,
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": {"type": "string", "maxLength": 256},
                                "title": {"type": "string", "maxLength": 256},
                                "section": {"type": "string", "maxLength": 256},
                                "score": {"type": "integer"},
                                "content": {"type": "string", "maxLength": 1800}
                            },
                            "required": ["path", "title", "section", "score", "content"],
                            "additionalProperties": false
                        }
                    },
                    "truncated": {"type": "boolean"}
                },
                "required": ["query", "version", "matches", "truncated"],
                "additionalProperties": false
            }))
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
