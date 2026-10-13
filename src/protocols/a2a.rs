use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use a2a_protocol_server::{
    AgentExecutor, EventEmitter, JsonRpcDispatcher, RequestHandlerBuilder,
    request_context::RequestContext, streaming::EventQueueWriter,
};
use a2a_protocol_types::{
    agent_card::{AgentCapabilities, AgentCard, AgentInterface, AgentSkill},
    error::A2aResult,
    message::{Message, MessageId, MessageRole, Part},
    security::{HttpAuthSecurityScheme, SecurityRequirement, SecurityScheme, StringList},
    task::TaskState,
};

use sha2::{Digest as _, Sha256};
use tracing::Instrument as _;

use crate::runtime::Runtime;

pub(crate) struct Dispatcher {
    inner: JsonRpcDispatcher,
    sessions: Option<crate::protocols::acp::SessionRegistry>,
}

impl Dispatcher {
    pub(crate) async fn dispatch(
        &self,
        request: axum::http::Request<hyper::body::Incoming>,
    ) -> axum::http::Response<axum::body::Body> {
        use axum::{
            body::{Body, to_bytes},
            http::{Method, Response, StatusCode},
        };
        let Some(sessions) = &self.sessions else {
            return self.inner.dispatch(request).await.map(Body::new);
        };
        if request.method() != Method::POST {
            return self.inner.dispatch(request).await.map(Body::new);
        }
        let bytes = match to_bytes(Body::new(request.into_body()), 4 * 1024 * 1024).await {
            Ok(bytes) => bytes,
            Err(_) => {
                let mut response =
                    Response::new(Body::from("A2A request body exceeds admission limit"));
                *response.status_mut() = StatusCode::PAYLOAD_TOO_LARGE;
                return response;
            }
        };
        let value = match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(value) => value,
            Err(_) => {
                return rpc_response(
                    serde_json::Value::Null,
                    Err((-32700, "Invalid JSON".into())),
                );
            }
        };
        let id = value
            .get("id")
            .filter(|id| id.is_null() || id.is_string() || id.is_number())
            .cloned()
            .unwrap_or_default();
        rpc_response(id, admit_message(sessions, value).await)
    }
}

fn rpc_response(
    id: serde_json::Value,
    result: Result<Message, (i32, String)>,
) -> axum::http::Response<axum::body::Body> {
    use serde_json::{Map, Value};
    let result = result.and_then(|message| {
        serde_json::to_value(a2a_protocol_types::responses::SendMessageResponse::Message(
            message,
        ))
        .map_err(|error| (-32603, error.to_string()))
    });
    let mut value = Map::from_iter([
        ("jsonrpc".into(), Value::String("2.0".into())),
        ("id".into(), id),
    ]);
    match result {
        Ok(message) => {
            value.insert("result".into(), message);
        }
        Err((code, message)) => {
            value.insert(
                "error".into(),
                Value::Object(Map::from_iter([
                    ("code".into(), Value::from(code)),
                    ("message".into(), Value::String(message)),
                ])),
            );
        }
    }
    let value = Value::Object(value);
    let mut response = axum::http::Response::new(axum::body::Body::from(value.to_string()));
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    response
}

async fn admit_message(
    sessions: &crate::protocols::acp::SessionRegistry,
    request: serde_json::Value,
) -> Result<Message, (i32, String)> {
    if request["jsonrpc"] != "2.0"
        || !request.is_object()
        || !request
            .get("id")
            .is_some_and(|id| id.is_null() || id.is_string() || id.is_number())
    {
        return Err((-32600, "Invalid JSON-RPC request".into()));
    }
    let Some(method) = request["method"].as_str() else {
        return Err((-32600, "JSON-RPC method must be a string".into()));
    };
    if !matches!(method, "SendMessage" | "message/send") {
        return Err((-32601, "Session-bound A2A supports SendMessage admission acknowledgments, not task tracking or streaming".into()));
    }
    let params: a2a_protocol_types::params::MessageSendParams =
        serde_json::from_value(request["params"].clone())
            .map_err(|error| (-32602, error.to_string()))?;
    if params.configuration.as_ref().is_some_and(|configuration| {
        configuration.return_immediately == Some(true)
            || configuration.task_push_notification_config.is_some()
    }) {
        return Err((-32602, "Session-bound A2A requires a direct acknowledgment: returnImmediately=false and no push notification configuration".into()));
    }
    if params.message.role != MessageRole::User || params.message.task_id.is_some() {
        return Err((
            -32602,
            "Admission requires a new user message without a taskId".into(),
        ));
    }
    if params
        .message
        .parts
        .iter()
        .any(|part| part.text_content().is_none())
    {
        return Err((
            -32602,
            "Session admission supports text parts only; nontext parts are not accepted".into(),
        ));
    }
    let prompt = params
        .message
        .parts
        .iter()
        .filter_map(Part::text_content)
        .collect::<Vec<_>>()
        .join("\n");
    if prompt.trim().is_empty() {
        return Err((-32602, "A2A request must contain a text part".into()));
    }
    let acknowledgment = sessions
        .admit_a2a(prompt)
        .await
        .map_err(|error| (-32603, error))?;
    Ok(Message {
        id: MessageId::new(format!("{}-admission", params.message.id)),
        role: MessageRole::Agent,
        parts: vec![Part::text(acknowledgment)],
        task_id: None,
        context_id: params.message.context_id,
        reference_task_ids: None,
        extensions: None,
        metadata: Some(serde_json::Value::Object(serde_json::Map::from_iter([(
            "kind".into(),
            serde_json::Value::String("admission_acknowledgment".into()),
        )]))),
    })
}

struct KitAgent {
    runtime: Arc<Runtime>,
}

impl AgentExecutor for KitAgent {
    fn execute<'a>(
        &'a self,
        context: &'a RequestContext,
        queue: &'a dyn EventQueueWriter,
    ) -> Pin<Box<dyn Future<Output = A2aResult<()>> + Send + 'a>> {
        Box::pin(
            async move {
                let emit = EventEmitter::new(context, queue);

                let prompt = context
                    .message
                    .parts
                    .iter()
                    .filter_map(Part::text_content)
                    .collect::<Vec<_>>()
                    .join("\n");
                if prompt.trim().is_empty() {
                    emit.artifact(
                        "error",
                        vec![Part::text("A2A request must contain a text part")],
                        None,
                        Some(true),
                    )
                    .await?;
                    emit.status(TaskState::Failed).await?;
                    return Ok(());
                }
                emit.status(TaskState::Working).await?;
                match self
                    .runtime
                    .run_cancelled(prompt, 0, Some(context.cancellation_token.clone()))
                    .await
                {
                    Ok(output) => {
                        emit.artifact("result", vec![Part::text(output)], None, Some(true))
                            .await?;
                        emit.status(TaskState::Completed).await?;
                    }
                    Err(error) => {
                        let session_id = a2a_session_id(context);
                        let rendered = crate::fatal::render_loop_error(&error);
                        let rendered = match crate::fatal::record_loop_error(
                            &session_id,
                            crate::fatal::Surface::A2a,
                            &error,
                        ) {
                            Ok(Some(path)) => {
                                eprintln!(
                                    "stored fatal error log for {session_id}: {}",
                                    path.display()
                                );
                                rendered
                            }
                            Ok(None) => rendered,
                            Err(log_error) => {
                                eprintln!(
                                    "could not store fatal error log for {session_id}: {log_error}"
                                );
                                rendered
                            }
                        };
                        emit.artifact("error", vec![Part::text(rendered)], None, Some(true))
                            .await?;
                        emit.status(TaskState::Failed).await?;
                    }
                }
                Ok(())
            }
            .instrument(crate::telemetry::error_spans::operation("a2a")),
        )
    }
}

fn a2a_session_id(context: &RequestContext) -> String {
    let mut digest = Sha256::new();
    digest.update(context.task_id.0.as_bytes());
    digest.update([0]);
    digest.update(context.context_id.as_bytes());
    let digest = digest.finalize();
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("a2a-{encoded}")
}

pub(crate) fn dispatcher(
    runtime: Arc<Runtime>,
    bound: std::net::SocketAddr,
    authenticated: bool,
    sessions: Option<crate::protocols::acp::SessionRegistry>,
) -> Result<Dispatcher, Box<dyn std::error::Error>> {
    let url = format!("http://{bound}");
    let (security_schemes, security_requirements) = if authenticated {
        let name = "bearer".to_string();
        (
            Some(HashMap::from([(
                name.clone(),
                SecurityScheme::Http(HttpAuthSecurityScheme {
                    scheme: "bearer".into(),
                    bearer_format: None,
                    description: Some("Bearer token loaded by the Kit server".into()),
                }),
            )])),
            Some(vec![SecurityRequirement {
                schemes: HashMap::from([(name, StringList { list: Vec::new() })]),
            }]),
        )
    } else {
        (None, None)
    };
    let card = AgentCard {
        url: None,
        name: "Kit".into(),
        description: if sessions.is_some() {
            "Admit text into the bound interactive ACP v2 session. SendMessage returns an admission acknowledgment, not task completion or model output.".into()
        } else {
            "Coding agent runtime".into()
        },
        version: env!("CARGO_PKG_VERSION").into(),
        supported_interfaces: vec![AgentInterface {
            url,
            protocol_binding: "JSONRPC".into(),
            protocol_version: "1.0".into(),
            tenant: None,
        }],
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![AgentSkill {
            id: "coding".into(),
            name: "Coding".into(),
            description: "Inspect, edit, and run code in the configured directory".into(),
            tags: vec!["coding".into()],
            examples: None,
            input_modes: None,
            output_modes: None,
            security_requirements: None,
        }],
        capabilities: AgentCapabilities::none(),
        provider: None,
        icon_url: None,
        documentation_url: None,
        security_schemes,
        security_requirements,
        signatures: None,
    };
    let handler = Arc::new(
        RequestHandlerBuilder::new(KitAgent { runtime })
            .with_agent_card(card)
            .build()?,
    );
    Ok(Dispatcher {
        inner: JsonRpcDispatcher::new(handler),
        sessions,
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use serde_json::json;

    fn send_request() -> serde_json::Value {
        json!({"jsonrpc": "2.0", "id": 1, "method": "SendMessage", "params": {
            "message": {"messageId": "inbound", "role": "ROLE_USER", "parts": [{"text": "request"}]}
        }})
    }

    #[tokio::test]
    async fn a2a_direct_acknowledgment_is_a_message_not_a_completed_task() {
        let registry = super::super::acp::a2a_test_support::bound_registry(Arc::new(|_prompt| {
            Box::pin(async { Ok("Accepted into the bound ACP session; not completion".into()) })
        }));
        let acknowledgment = admit_message(&registry, send_request()).await.unwrap();
        assert_eq!(acknowledgment.role, MessageRole::Agent);
        assert_eq!(acknowledgment.task_id, None);
        assert!(acknowledgment.text().unwrap().contains("not completion"));
        assert_eq!(
            acknowledgment.metadata.unwrap()["kind"],
            "admission_acknowledgment"
        );
    }

    #[tokio::test]
    async fn a2a_admission_rejects_task_modes_and_missing_session_without_execution() {
        let registry = crate::protocols::acp::SessionRegistry::new();
        let mut request = send_request();
        request["params"]["configuration"] = json!({"returnImmediately": true});
        assert_eq!(
            admit_message(&registry, request).await.unwrap_err().0,
            -32602
        );
        let mut request = send_request();
        request["method"] = json!("SendStreamingMessage");
        assert_eq!(
            admit_message(&registry, request).await.unwrap_err().0,
            -32601
        );
        let mut request = send_request();
        request["params"]["message"]["taskId"] = json!("existing-task");
        assert_eq!(
            admit_message(&registry, request).await.unwrap_err().0,
            -32602
        );
        assert!(
            admit_message(&registry, send_request())
                .await
                .unwrap_err()
                .1
                .contains("no bound")
        );
        let mut mixed = send_request();
        mixed["params"]["message"]["parts"]
            .as_array_mut()
            .unwrap()
            .push(json!({"data": {"ignored": true}}));
        assert_eq!(admit_message(&registry, mixed).await.unwrap_err().0, -32602);
        let mut malformed = send_request();
        malformed["method"] = json!(42);
        assert_eq!(
            admit_message(&registry, malformed).await.unwrap_err().0,
            -32600
        );
        let registry = super::super::acp::a2a_test_support::bound_registry(Arc::new(|_prompt| {
            Box::pin(async { Err("ACP admission failed".into()) })
        }));
        assert_eq!(
            admit_message(&registry, send_request()).await.unwrap_err(),
            (-32603, "ACP admission failed".into())
        );
    }
}
