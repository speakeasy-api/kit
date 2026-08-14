use std::sync::Arc;

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crate::runtime::Runtime;

#[derive(Clone)]
pub struct SubagentTool {
    runtime: Arc<Runtime>,
    depth: usize,
    spec: ToolSpec,
}

impl SubagentTool {
    pub fn new(runtime: Arc<Runtime>, depth: usize) -> Self {
        Self {
            runtime,
            depth,
            spec: ToolSpec::new(
                ToolName::new("subagent"),
                "Run a fresh local coding agent in the same directory and return its final response.",
                json!({
                    "type": "object",
                    "properties": {"prompt": {"type": "string"}},
                    "required": ["prompt"],
                    "additionalProperties": false
                }),
            )
            .with_annotations(ToolAnnotations::new()),
        }
    }
}

#[derive(Deserialize)]
struct Input {
    prompt: String,
}

#[async_trait]
impl Tool for SubagentTool {
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
        if self.depth >= self.runtime.max_subagent_depth() {
            return Err(ToolError::ExecutionFailed(format!(
                "subagent depth limit ({}) reached",
                self.runtime.max_subagent_depth()
            )));
        }
        let output = self
            .runtime
            .run(input.prompt, self.depth + 1)
            .await
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::text(output),
        )))
    }
}
