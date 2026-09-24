#![allow(clippy::disallowed_methods, clippy::disallowed_macros)]
//! Wire-level coverage for fallback images with authentication-bound replay.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use agentkit_core::{Item, SessionId, ToolResultPart, TurnId};
use agentkit_http::{Bytes, Http, StatusCode};
use serde_json::json;
use tokio::sync::mpsc;

// Fake only the external HTTP boundary; decoding, authentication binding,
// projection, normalization, and request encoding remain production code.
struct ContinuationHttp {
    requests: mpsc::UnboundedSender<HttpRequest>,
}

#[async_trait]
impl HttpClient for ContinuationHttp {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        let url = request.url.clone();
        self.requests.send(request).unwrap();
        let items = [
            json!({"id":"reason-1","type":"reasoning","summary":[{"type":"summary_text","text":"Inspect both results"}],"encrypted_content":"opaque-reasoning"}),
            json!({"id":"call-item-image","type":"function_call","call_id":"call-image","name":"compose","arguments":"{}"}),
            json!({"id":"call-item-text","type":"function_call","call_id":"call-text","name":"compose","arguments":"{}"}),
        ];
        let mut events = vec![
            json!({"type":"response.created","response":{"id":"response-1","model":"gpt-6-astra"}}),
        ];
        for (index, item) in items.into_iter().enumerate() {
            events.push(json!({"type":"response.output_item.added","output_index":index,"item":{"id":item["id"],"type":item["type"]}}));
            if item["type"] == "reasoning" {
                events.push(json!({"type":"response.reasoning_summary_part.added","item_id":item["id"],"output_index":index,"summary_index":0,"part":{"type":"summary_text"}}));
                events.push(json!({"type":"response.reasoning_summary_text.delta","item_id":item["id"],"output_index":index,"summary_index":0,"delta":"Inspect both results"}));
                events.push(json!({"type":"response.reasoning_summary_text.done","item_id":item["id"],"output_index":index,"summary_index":0,"text":"Inspect both results"}));
                events.push(json!({"type":"response.reasoning_summary_part.done","item_id":item["id"],"output_index":index,"summary_index":0,"part":item["summary"][0]}));
            } else {
                events.push(json!({"type":"response.function_call_arguments.delta","item_id":item["id"],"output_index":index,"delta":"{}"}));
                events.push(json!({"type":"response.function_call_arguments.done","item_id":item["id"],"output_index":index,"arguments":"{}"}));
            }
            events
                .push(json!({"type":"response.output_item.done","output_index":index,"item":item}));
        }
        events.push(json!({"type":"response.completed","response":{"id":"response-1","model":"gpt-6-astra","usage":{"input_tokens":1,"output_tokens":1}}}));
        let body = events
            .into_iter()
            .enumerate()
            .map(|(index, mut event)| {
                event["sequence_number"] = json!(index + 1);
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect::<String>();
        Ok(HttpResponse::new(
            StatusCode::OK,
            HeaderMap::from_iter([(
                agentkit_http::header::CONTENT_TYPE,
                HeaderValue::from_static("text/event-stream"),
            )]),
            url,
            Box::pin(futures_util::stream::once(
                async move { Ok(Bytes::from(body)) },
            )),
        ))
    }
}

async fn collect_output(
    session: &mut OpenAiSubscriptionSession,
    request: TurnRequest,
) -> Vec<Item> {
    let mut turn = session.begin_turn(request, None).await.unwrap();
    while let Some(event) = turn.next_event(None).await.unwrap() {
        if let ModelTurnEvent::Finished(result) = event {
            return result.output_items;
        }
    }
    panic!("response must finish");
}

#[tokio::test]
async fn authenticated_continuation_replays_parallel_results_with_fallback_images() {
    let (sender, mut requests) = mpsc::unbounded_channel();
    let config = OpenAIResponsesConfig::chatgpt_private(
        "gpt-6-astra",
        Authentication::bearer("test-image-replay-key"),
    );
    let adapter = OpenAIResponsesAdapter::with_client(
        config,
        Http::new(ContinuationHttp { requests: sender }),
    );
    let mut session = OpenAiSubscriptionSession {
        inner: adapter
            .start_session(SessionConfig::new("image-replay"))
            .await
            .unwrap(),
        context_window: None,
        // No legacy metadata is injected: the real adapter creates and validates
        // its current authentication-bound continuation metadata.
        authentication_binding: "unused-legacy-binding".into(),
    };
    let mut canonical = TurnRequest {
        session_id: SessionId::new("image-replay"),
        turn_id: TurnId::new("initial"),
        transcript: vec![Item::text(ItemKind::User, "Inspect both results")],
        available_tools: Vec::new(),
        cache: None,
        metadata: MetadataMap::new(),
    };
    let output = collect_output(&mut session, canonical.clone()).await;
    let initial = requests.try_recv().unwrap();
    assert_eq!(
        initial.headers[agentkit_http::header::AUTHORIZATION],
        "Bearer test-image-replay-key"
    );
    let metadata: Vec<_> = output
        .iter()
        .flat_map(|item| &item.parts)
        .filter_map(|part| match part {
            Part::Reasoning(reasoning) => reasoning.metadata.get(CONTINUATION_METADATA),
            Part::ToolCall(call) => call.metadata.get(CONTINUATION_METADATA),
            _ => None,
        })
        .collect();
    assert_eq!(metadata.len(), 3);
    for value in metadata {
        assert!(!value["authentication_binding"].as_str().unwrap().is_empty());
        assert_eq!(value["session_id"], "image-replay");
    }
    canonical.transcript.extend(output);
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, Rgb([12, 34, 56])))
        .write_to(&mut png, ImageFormat::Png)
        .unwrap();
    let expected_url = format!("data:image/png;base64,{}", BASE64.encode(png.get_ref()));
    canonical.transcript.push(Item::new(
        ItemKind::Tool,
        vec![Part::ToolResult(ToolResultPart::success(
            "call-image",
            ToolOutput::Parts(vec![
                Part::text("Selected screenshot: $.image"),
                Part::media(
                    Modality::Image,
                    "image/png",
                    DataRef::InlineBytes(png.into_inner()),
                ),
            ]),
        ))],
    ));
    canonical.transcript.push(Item::new(
        ItemKind::Tool,
        vec![Part::ToolResult(ToolResultPart::success(
            "call-text",
            ToolOutput::Text("Second parallel result".into()),
        ))],
    ));
    let original = serde_json::to_value(&canonical.transcript).unwrap();
    let mut encoded = Vec::new();
    for turn_id in ["continuation", "replay"] {
        // A fresh provider session proves replay does not depend on hidden
        // per-session request state or a persisted synthetic user turn.
        session.inner = adapter
            .start_session(SessionConfig::new("image-replay"))
            .await
            .unwrap();
        let mut request = canonical.clone();
        request.turn_id = TurnId::new(turn_id);
        collect_output(&mut session, request).await;
        let captured = requests.try_recv().unwrap();
        let wire: Value = serde_json::from_slice(captured.body.as_ref().unwrap()).unwrap();
        assert_eq!(wire["model"], "gpt-6-astra");
        let input = wire["input"].as_array().unwrap();
        let reasoning = input
            .iter()
            .find(|item| item["type"] == "reasoning")
            .unwrap();
        assert_eq!(reasoning["id"], "reason-1");
        assert_eq!(reasoning["encrypted_content"], "opaque-reasoning");
        let calls: Vec<_> = input
            .iter()
            .enumerate()
            .filter(|(_, item)| item["type"] == "function_call")
            .collect();
        let results: Vec<_> = input
            .iter()
            .enumerate()
            .filter(|(_, item)| item["type"] == "function_call_output")
            .collect();
        assert_eq!(calls.len(), 2);
        assert_eq!(results.len(), 2);
        for (index, (call_id, item_id)) in [
            ("call-image", "call-item-image"),
            ("call-text", "call-item-text"),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(calls[index].1["call_id"], call_id);
            assert_eq!(calls[index].1["id"], item_id);
            assert_eq!(results[index].1["call_id"], call_id);
            assert!(calls[index].0 < results[0].0);
        }
        assert!(
            results[0].1["output"]
                .as_str()
                .unwrap()
                .contains("Selected screenshot: $.image")
        );
        assert_eq!(results[1].1["output"], "Second parallel result");
        let images: Vec<_> = input
            .iter()
            .enumerate()
            .flat_map(|(index, item)| {
                item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|part| part["type"] == "input_image")
                    .map(move |part| (index, item, part))
            })
            .collect();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].0, results[1].0 + 1);
        assert_eq!(images[0].1["role"], "user");
        assert_eq!(images[0].2["image_url"], expected_url);
        assert_eq!(
            serde_json::to_value(&canonical.transcript).unwrap(),
            original
        );
        encoded.push(wire["input"].clone());
    }
    assert_eq!(encoded[0], encoded[1]);
    assert!(requests.try_recv().is_err());
}
