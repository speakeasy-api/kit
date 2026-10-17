use super::super::test_support::{hooks, intercept, text_message};
use super::*;
use agentkit_core::{MetadataMap, SessionId, TurnId};
use serde_json::json;
use std::collections::VecDeque;

#[derive(Clone)]
struct EchoModel;

#[async_trait]
impl ModelAdapter for EchoModel {
    type Session = Self;
    async fn start_session(&self, _: SessionConfig) -> Result<Self, LoopError> {
        Ok(self.clone())
    }
}

#[async_trait]
impl ModelSession for EchoModel {
    type Turn = Events;
    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        _: Option<TurnCancellation>,
    ) -> Result<Events, LoopError> {
        if request.metadata.contains_key("fail") {
            return Err(LoopError::Provider("provider must not be called".into()));
        }
        let text = request
            .transcript
            .iter()
            .flat_map(|item| &item.parts)
            .filter_map(super::super::part_text)
            .collect::<Vec<_>>()
            .join("|");
        let mut events = VecDeque::from([ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![Item::text(ItemKind::Assistant, text)],
            usage: None,
            metadata: Default::default(),
            model: None,
            response_id: None,
        })]);
        if request.metadata.contains_key("stream") {
            events.push_front(ModelTurnEvent::Delta(agentkit_core::Delta::AppendText {
                part_id: agentkit_core::PartId::new("part"),
                chunk: "original".into(),
            }));
        }
        Ok(Events(events))
    }
}

struct Events(VecDeque<ModelTurnEvent>);
#[async_trait]
impl ModelTurn for Events {
    async fn next_event(
        &mut self,
        _: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        Ok(self.0.pop_front())
    }
}

fn request() -> TurnRequest {
    TurnRequest {
        session_id: SessionId::new("model-effects"),
        turn_id: TurnId::new("turn"),
        transcript: vec![Item::text(ItemKind::User, "original")],
        available_tools: vec![],
        cache: None,
        metadata: MetadataMap::new(),
    }
}

async fn session(hooks: Arc<Hooks>) -> HookedSession<EchoModel> {
    HookedAdapter::new(EchoModel, Some(hooks), SessionStartInputTrigger::Startup)
        .start_session(SessionConfig::new("model-effects"))
        .await
        .unwrap()
}

fn text(event: Option<ModelTurnEvent>) -> String {
    let Some(ModelTurnEvent::Finished(result)) = event else {
        panic!("expected finished")
    };
    result
        .output_items
        .iter()
        .flat_map(|item| &item.parts)
        .filter_map(super::super::part_text)
        .collect()
}

#[tokio::test]
async fn request_replacement_reaches_provider() {
    let (hooks, backend) = hooks(vec![intercept(&["model.request.before"])]);
    backend.respond("model.request.before", json!([{"type":"modify", "target":"request", "operation":"replace", "value":text_message("edited")} ]));
    let mut turn = session(hooks)
        .await
        .begin_turn(request(), None)
        .await
        .unwrap();
    assert_eq!(text(turn.next_event(None).await.unwrap()), "edited");
}

#[tokio::test]
async fn supplied_result_bypasses_provider() {
    let (hooks, backend) = hooks(vec![
        intercept(&["model.request.before"]),
        super::super::test_support::observe(&["model.response.after"]),
    ]);
    let mut messages = text_message("cached");
    messages[0]["role"] = json!("assistant");
    backend.respond(
        "model.request.before",
        json!([{"type":"return", "value":messages}]),
    );
    let mut request = request();
    request.metadata.insert("fail".into(), json!(true));
    let mut turn = session(hooks)
        .await
        .begin_turn(request, None)
        .await
        .unwrap();
    assert_eq!(text(turn.next_event(None).await.unwrap()), "cached");
    assert_eq!(
        backend.events("model.response.after")[0]["execution"]["reason"],
        "supplied_result"
    );
    let response = &backend.events("model.response.after")[0];
    assert!(response.get("usage").is_none());
    assert!(response.get("latencyMs").is_none());
}

#[tokio::test]
async fn response_replacement_is_final_output() {
    let (hooks, backend) = hooks(vec![intercept(&["model.response.after"])]);
    let mut messages = text_message("safe");
    messages[0]["role"] = json!("assistant");
    backend.respond(
        "model.response.after",
        json!([{"type":"modify", "target":"response", "operation":"replace", "value":messages}]),
    );
    let mut request = request();
    request.metadata.insert("stream".into(), json!(true));
    let mut turn = session(hooks)
        .await
        .begin_turn(request, None)
        .await
        .unwrap();
    assert_eq!(text(turn.next_event(None).await.unwrap()), "safe");
}

#[tokio::test]
async fn response_stop_withholds_output() {
    let (hooks, backend) = hooks(vec![intercept(&["model.response.after"])]);
    backend.respond(
        "model.response.after",
        json!([{"type":"flow", "operation":"stop", "reason":"policy"}]),
    );
    let mut request = request();
    request.metadata.insert("stream".into(), json!(true));
    let mut turn = session(hooks)
        .await
        .begin_turn(request, None)
        .await
        .unwrap();
    assert!(turn.next_event(None).await.is_err());
}

#[tokio::test]
async fn immediate_injection_reaches_provider() {
    let (hooks, backend) = hooks(vec![intercept(&["model.request.before"])]);
    backend.respond("model.request.before", json!([{"type":"inject", "target":"context", "operation":"append", "deliverAt":"now", "value":text_message("context")}]));
    let mut turn = session(hooks)
        .await
        .begin_turn(request(), None)
        .await
        .unwrap();
    assert_eq!(
        text(turn.next_event(None).await.unwrap()),
        "original|context"
    );
}

#[test]
fn unknown_content_is_not_reinterpreted_as_text() {
    let mut messages = text_message("not executable");
    messages[0]["parts"][0]["category"] = json!("tool-call");
    let messages: agenthooksprotocol::CanonicalMessages = serde_json::from_value(messages).unwrap();
    assert!(canonical_items(messages.0.iter().map(Box::as_ref), &[]).is_err());
}

#[tokio::test]
async fn observe_only_preserves_streaming() {
    let (hooks, _) = hooks(vec![super::super::test_support::observe(&[
        "model.response.after",
    ])]);
    let mut request = request();
    request.metadata.insert("stream".into(), json!(true));
    let mut turn = session(hooks)
        .await
        .begin_turn(request, None)
        .await
        .unwrap();
    assert!(matches!(
        turn.next_event(None).await.unwrap(),
        Some(ModelTurnEvent::Delta(_))
    ));
    assert_eq!(text(turn.next_event(None).await.unwrap()), "original");
}

#[test]
fn changing_text_retains_unchanged_native_tool_call() {
    let original = Item::new(
        ItemKind::Assistant,
        vec![
            Part::text("original"),
            Part::ToolCall(agentkit_core::ToolCallPart::new(
                "call",
                "echo",
                json!({"text":"safe"}),
            )),
        ],
    );
    let projected = super::super::message(&original, 0);
    let mut message = projected.metadata;
    message.parts = projected
        .parts
        .into_iter()
        .map(|part| match part {
            agenthooksprotocol::inputs::PartInput::Text(body) => {
                Box::new(ContentItem::TextBodyPart(Box::new(body)))
            }
            _ => panic!("expected text projection"),
        })
        .collect();
    let ContentItem::TextBodyPart(body) = message.parts[0].as_mut() else {
        panic!("text")
    };
    body.text = "edited".into();
    let result = canonical_items([&message], std::slice::from_ref(&original)).unwrap();
    assert_eq!(result[0].parts[0], Part::text("edited"));
    assert_eq!(result[0].parts[1], original.parts[1]);
}

#[test]
fn opaque_reasoning_survives_noop_and_rejects_unsafe_sibling_edits() {
    let original = Item::new(
        ItemKind::Assistant,
        vec![
            Part::Reasoning(agentkit_core::ReasoningPart {
                summary: None,
                data: Some(agentkit_core::DataRef::inline_bytes(vec![1, 2, 3])),
                redacted: false,
                metadata: MetadataMap::new(),
            }),
            Part::text("visible"),
        ],
    );
    let projected = super::super::message(&original, 0);
    let mut message = projected.metadata;
    message.parts = projected
        .parts
        .into_iter()
        .map(|part| match part {
            agenthooksprotocol::inputs::PartInput::Text(body) => {
                Box::new(ContentItem::TextBodyPart(Box::new(body)))
            }
            _ => panic!("expected text projection"),
        })
        .collect();
    assert_eq!(message.parts.len(), 1);
    assert_eq!(
        canonical_items([&message], std::slice::from_ref(&original)).unwrap(),
        vec![original.clone()]
    );
    let ContentItem::TextBodyPart(body) = message.parts[0].as_mut() else {
        panic!("text")
    };
    body.text = "edited".into();
    let error = canonical_items([&message], &[original]).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("opaque unprojected provider content")
    );
}

#[derive(Clone)]
struct ToolModel(bool);

#[async_trait]
impl ModelAdapter for ToolModel {
    type Session = Self;
    async fn start_session(&self, _: SessionConfig) -> Result<Self, LoopError> {
        Ok(self.clone())
    }
}

#[async_trait]
impl ModelSession for ToolModel {
    type Turn = Events;
    async fn begin_turn(
        &mut self,
        _: TurnRequest,
        _: Option<TurnCancellation>,
    ) -> Result<Events, LoopError> {
        let mut events = VecDeque::new();
        let output_items = if self.0 {
            let call = agentkit_core::ToolCallPart::new(
                "approved-call",
                "echo",
                json!({"text":"approved"}),
            );
            events.push_back(ModelTurnEvent::ToolCall(call.clone()));
            vec![Item::new(ItemKind::Assistant, vec![Part::ToolCall(call)])]
        } else {
            vec![Item::text(ItemKind::Assistant, "done")]
        };
        events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason: if self.0 {
                FinishReason::ToolCall
            } else {
                FinishReason::Completed
            },
            output_items,
            usage: Some(agentkit_core::Usage::new(agentkit_core::TokenUsage::new(
                2, 3,
            ))),
            metadata: MetadataMap::from_iter([("sentinel".into(), json!("retained"))]),
            model: Some("fake-model".into()),
            response_id: Some("fake-response".into()),
        }));
        self.0 = false;
        Ok(Events(events))
    }
}

struct RecordingTool {
    spec: agentkit_tools_core::ToolSpec,
    calls: tokio::sync::mpsc::UnboundedSender<agentkit_tools_core::ToolRequest>,
}

#[async_trait]
impl agentkit_tools_core::Tool for RecordingTool {
    fn spec(&self) -> &agentkit_tools_core::ToolSpec {
        &self.spec
    }
    async fn invoke(
        &self,
        request: agentkit_tools_core::ToolRequest,
        _: &mut agentkit_tools_core::ToolContext<'_>,
    ) -> Result<agentkit_tools_core::ToolResult, agentkit_tools_core::ToolError> {
        self.calls.send(request.clone()).unwrap();
        Ok(agentkit_tools_core::ToolResult::new(
            agentkit_core::ToolResultPart::success(
                request.call_id,
                agentkit_core::ToolOutput::text("tool executed"),
            ),
        ))
    }
}

#[tokio::test]
async fn real_loop_executes_only_approved_tool_calls_once() {
    use agentkit_loop::{Agent, LoopInterrupt, LoopStep};
    use agentkit_tools_core::{ToolName, ToolRegistry, ToolSpec};
    for policy in ["allow", "observe", "remove", "stop"] {
        let subscription = if policy == "observe" {
            super::super::test_support::observe(&["model.response.after"])
        } else {
            intercept(&["model.response.after"])
        };
        let (hooks, backend) = hooks(vec![subscription]);
        match policy {
            "remove" => {
                let mut messages = text_message("safe replacement");
                messages[0]["role"] = json!("assistant");
                backend.respond("model.response.after", json!([{"type":"modify", "target":"response", "operation":"replace", "value":messages}]));
            }
            "stop" => backend.respond(
                "model.response.after",
                json!([{"type":"flow", "operation":"stop", "reason":"blocked"}]),
            ),
            _ => {}
        }
        let (calls, mut received) = tokio::sync::mpsc::unbounded_channel();
        let mut tools = ToolRegistry::new();
        tools.register_arc(Arc::new(RecordingTool {
            spec: ToolSpec::new(ToolName::new("echo"), "echo", json!({"type":"object"})),
            calls,
        }));
        let mut driver = Agent::builder()
            .model(HookedAdapter::new(
                ToolModel(true),
                Some(hooks),
                SessionStartInputTrigger::Startup,
            ))
            .add_tool_source(tools)
            .input(vec![Item::text(ItemKind::User, "run echo")])
            .build()
            .unwrap()
            .start(SessionConfig::new(format!("tool-{policy}")))
            .await
            .unwrap();
        let step = driver.next().await;
        if matches!(policy, "allow" | "observe") {
            assert!(matches!(
                step.unwrap(),
                LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_))
            ));
            let call = received.try_recv().expect("approved tool must execute");
            assert_eq!(call.call_id.to_string(), "approved-call");
            assert_eq!(call.input, json!({"text":"approved"}));
            assert!(matches!(
                driver.next().await.unwrap(),
                LoopStep::Finished(_)
            ));
        } else if policy == "stop" {
            assert!(step.is_err());
        } else {
            assert!(matches!(step.unwrap(), LoopStep::Finished(_)));
        }
        assert!(
            received.try_recv().is_err(),
            "no removed, blocked, or duplicate calls: {policy}"
        );
    }
}

#[tokio::test]
async fn approved_replay_preserves_result_and_cancellation_discards_queue() {
    for cancel in [false, true] {
        let (hooks, _) = hooks(vec![intercept(&["model.response.after"])]);
        let mut session = HookedAdapter::new(
            ToolModel(true),
            Some(hooks),
            SessionStartInputTrigger::Startup,
        )
        .start_session(SessionConfig::new("model-effects"))
        .await
        .unwrap();
        let mut turn = session.begin_turn(request(), None).await.unwrap();
        assert!(matches!(
            turn.next_event(None).await.unwrap(),
            Some(ModelTurnEvent::ToolCall(_))
        ));
        if cancel {
            let controller = agentkit_core::CancellationController::new();
            let token = controller.handle().checkpoint();
            controller.interrupt();
            assert!(matches!(
                turn.next_event(Some(token)).await,
                Err(LoopError::Cancelled)
            ));
            assert!(turn.next_event(None).await.unwrap().is_none());
        } else {
            let Some(ModelTurnEvent::Finished(result)) = turn.next_event(None).await.unwrap()
            else {
                panic!("approved final result")
            };
            assert_eq!(
                result.usage,
                Some(agentkit_core::Usage::new(agentkit_core::TokenUsage::new(
                    2, 3
                )))
            );
            assert_eq!(result.metadata["sentinel"], "retained");
            assert_eq!(result.model.as_deref(), Some("fake-model"));
            assert_eq!(result.response_id.as_deref(), Some("fake-response"));
            assert!(turn.next_event(None).await.unwrap().is_none());
        }
    }
}

#[test]
fn text_identity_suffixes_do_not_define_descriptor_semantics() {
    for suffix in ["data", "name"] {
        let mut messages = text_message("legitimate text");
        messages[0]["parts"][0]["id"] = json!(format!("new/{suffix}"));
        let messages: agenthooksprotocol::CanonicalMessages =
            serde_json::from_value(messages).unwrap();
        let result = canonical_items(messages.0.iter().map(Box::as_ref), &[]).unwrap();
        assert_eq!(result[0].parts, vec![Part::text("legitimate text")]);
    }
    for category in ["file", "media", "custom"] {
        let mut messages = text_message("descriptor");
        messages[0]["parts"][0]["category"] = json!(category);
        let messages: agenthooksprotocol::CanonicalMessages =
            serde_json::from_value(messages).unwrap();
        assert!(canonical_items(messages.0.iter().map(Box::as_ref), &[]).is_err());
    }
}

#[test]
fn native_parts_cannot_be_reordered_or_repeated() {
    let original = Item::new(
        ItemKind::Assistant,
        vec![
            Part::ToolCall(agentkit_core::ToolCallPart::new(
                "first",
                "echo",
                json!({"text":"first"}),
            )),
            Part::ToolCall(agentkit_core::ToolCallPart::new(
                "second",
                "echo",
                json!({"text":"second"}),
            )),
        ],
    );
    let projected = super::super::message(&original, 0);
    let mut message = projected.metadata;
    message.parts = projected
        .parts
        .into_iter()
        .map(|part| match part {
            agenthooksprotocol::inputs::PartInput::Text(body) => {
                Box::new(ContentItem::TextBodyPart(Box::new(body)))
            }
            _ => panic!("expected text projection"),
        })
        .collect();
    assert_eq!(
        canonical_items([&message], std::slice::from_ref(&original)).unwrap(),
        vec![original.clone()]
    );
    assert!(canonical_items([&message, &message], std::slice::from_ref(&original)).is_err());
    message.parts.reverse();
    assert!(canonical_items([&message], std::slice::from_ref(&original)).is_err());
    message.parts = vec![message.parts[0].clone(), message.parts[0].clone()];
    assert!(canonical_items([&message], &[original]).is_err());
}
