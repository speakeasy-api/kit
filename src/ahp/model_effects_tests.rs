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
