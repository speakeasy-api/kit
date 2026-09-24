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
use futures_util::future::{Either, select};
use serde::Deserialize;
use serde_json::{Map, Value};

static NEXT_MESSAGE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct A2aTool {
    spec: ToolSpec,
}

impl A2aTool {
    pub fn new() -> Self {
        let input_schema = Value::Object(Map::from_iter([
            ("type".into(), Value::from("object")),
            (
                "properties".into(),
                Value::Object(Map::from_iter([
                    (
                        "url".into(),
                        Value::Object(Map::from_iter([("type".into(), Value::from("string"))])),
                    ),
                    (
                        "prompt".into(),
                        Value::Object(Map::from_iter([("type".into(), Value::from("string"))])),
                    ),
                ])),
            ),
            (
                "required".into(),
                Value::Array(vec![Value::from("url"), Value::from("prompt")]),
            ),
            ("additionalProperties".into(), Value::from(false)),
        ]));
        let output_schema = Value::Object(Map::from_iter([
            ("type".into(), Value::from("object")),
            (
                "oneOf".into(),
                Value::Array(vec![
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("object")),
                        (
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "task".into(),
                                Value::Object(Map::from_iter([(
                                    "type".into(),
                                    Value::from("object"),
                                )])),
                            )])),
                        ),
                        ("required".into(), Value::Array(vec![Value::from("task")])),
                        ("additionalProperties".into(), Value::from(false)),
                    ])),
                    Value::Object(Map::from_iter([
                        ("type".into(), Value::from("object")),
                        (
                            "properties".into(),
                            Value::Object(Map::from_iter([(
                                "message".into(),
                                Value::Object(Map::from_iter([(
                                    "type".into(),
                                    Value::from("object"),
                                )])),
                            )])),
                        ),
                        (
                            "required".into(),
                            Value::Array(vec![Value::from("message")]),
                        ),
                        ("additionalProperties".into(), Value::from(false)),
                    ])),
                ]),
            ),
        ]));
        Self {
            spec: ToolSpec::new(
                ToolName::new("a2a"),
                "Send a text task to a remote A2A v1 agent.",
                input_schema,
            )
            .with_output_schema(output_schema)
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
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let cancellation = context.cancellation.clone();
        let input: Input = serde_json::from_value(request.input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let client = ClientBuilder::new(&input.url)
            .build()
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        let sent = client.send_message(MessageSendParams {
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
        });
        // A remote agent can take as long as it likes; an interrupted turn
        // cannot wait for it.
        let interrupted = async {
            match &cancellation {
                Some(cancellation) => cancellation.cancelled().await,
                None => std::future::pending().await,
            }
        };
        // This one-shot race chooses the response when both are ready, one of
        // the outcomes the previous unbiased select allowed. Drop the losing
        // future before returning; no work is spawned or detached here.
        let response = match select(std::pin::pin!(sent), std::pin::pin!(interrupted)).await {
            Either::Left((response, _)) => {
                response.map_err(|error| ToolError::ExecutionFailed(error.to_string()))?
            }
            Either::Right(((), _)) => return Err(ToolError::Cancelled),
        };
        let value = serde_json::to_value(response)
            .map_err(|error| ToolError::ExecutionFailed(error.to_string()))?;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(value),
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use agentkit_core::{CancellationController, MetadataMap, SessionId, TurnId};
    use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext};
    use tokio::io::AsyncReadExt as _;

    use super::*;

    #[test]
    fn cancellation_drops_pending_remote_request() -> Result<(), Box<dyn std::error::Error>> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let controller = CancellationController::new();
                let context = OwnedToolContext {
                    session_id: SessionId::new("session"),
                    turn_id: TurnId::new("turn"),
                    metadata: MetadataMap::new(),
                    permissions: Arc::new(AllowAllPermissions),
                    resources: Arc::new(()),
                    cancellation: Some(controller.handle().checkpoint()),
                    execution_scope: None,
                    approved_request: None,
                };
                let request = ToolRequest::new(
                    "call",
                    "a2a",
                    Value::Object(Map::from_iter([
                        (
                            "url".into(),
                            Value::from(format!("http://{}", listener.local_addr()?)),
                        ),
                        ("prompt".into(), Value::from("wait for cancellation")),
                    ])),
                    "session",
                    "turn",
                );
                let tool = A2aTool::new();
                let mut borrowed = context.borrowed();
                let invocation = tool.invoke(request, &mut borrowed);
                let server = async {
                    let (mut socket, _) = listener.accept().await?;
                    let mut buffer = [0; 1024];
                    if socket.read(&mut buffer).await? == 0 {
                        return Err(std::io::Error::other("missing request"));
                    }
                    // Interrupt only after the real HTTP request reaches the peer.
                    // Never reply: completion must come from cancellation, not HTTP.
                    controller.interrupt();
                    while socket.read(&mut buffer).await? != 0 {}
                    Ok::<_, std::io::Error>(())
                };
                let (result, peer_closed) = tokio::time::timeout(
                    Duration::from_secs(5),
                    futures_util::future::join(invocation, server),
                )
                .await?;
                peer_closed?;
                if !matches!(result, Err(ToolError::Cancelled)) {
                    return Err(format!("expected cancellation, got {result:?}").into());
                }
                Ok(())
            })
    }
}
