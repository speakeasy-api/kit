use std::sync::atomic::{AtomicU64, Ordering};

use a2a_protocol_client::ClientBuilder;
use a2a_protocol_types::{
    message::{Message, MessageId, MessageRole, Part},
    params::MessageSendParams,
};
use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

static NEXT_MESSAGE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct A2aTool {
    spec: ToolSpec,
}

impl A2aTool {
    pub fn new() -> Self {
        Self {
            spec: ToolSpec::new(
                ToolName::new("a2a"),
                "Send a text task to a remote A2A v1 agent.",
                json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string"},
                        "prompt": {"type": "string"}
                    },
                    "required": ["url", "prompt"],
                    "additionalProperties": false
                }),
            )
            .with_annotations(ToolAnnotations::new()),
        }
    }
}

impl Default for A2aTool {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct Input {
    url: String,
    prompt: String,
}

#[async_trait]
impl Tool for A2aTool {
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
        let client = ClientBuilder::new(&input.url)
            .build()
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        let response = client
            .send_message(MessageSendParams {
                tenant: None,
                message: Message {
                    id: MessageId::new(format!(
                        "kit-{}",
                        NEXT_MESSAGE.fetch_add(1, Ordering::Relaxed)
                    )),
                    role: MessageRole::User,
                    parts: vec![Part::text(input.prompt)],
                    task_id: None,
                    context_id: None,
                    reference_task_ids: None,
                    extensions: None,
                    metadata: None,
                },
                configuration: None,
                metadata: None,
            })
            .await
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        let value = serde_json::to_value(response)
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(value),
        )))
    }
}
