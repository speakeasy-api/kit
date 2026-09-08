#![allow(clippy::disallowed_methods, clippy::disallowed_macros)]
use super::*;
use agentkit_core::{Item, ItemKind, MetadataMap, SessionId, ToolCallPart, ToolResultPart, TurnId};
use agentkit_http::{
    HeaderMap, Http, HttpClient, HttpError, HttpRequest, HttpResponse, StatusCode,
};
use serde_json::json;

fn request() -> TurnRequest {
    let image = Part::Media(
        agentkit_core::MediaPart::new(
            Modality::Image,
            "image/png",
            DataRef::InlineBytes(vec![1, 2, 3]),
        )
        .with_metadata(MetadataMap::from_iter([(
            "position".into(),
            json!("$.image"),
        )])),
    );
    TurnRequest {
        session_id: SessionId::new("projection"),
        turn_id: TurnId::new("replay"),
        transcript: vec![
            Item::new(
                ItemKind::Assistant,
                vec![Part::ToolCall(ToolCallPart::new("a", "compose", json!({})))],
            ),
            Item::new(
                ItemKind::Assistant,
                vec![Part::ToolCall(ToolCallPart::new("b", "shell", json!({})))],
            ),
            Item::new(
                ItemKind::Tool,
                vec![Part::ToolResult(ToolResultPart::success(
                    "a",
                    ToolOutput::Parts(vec![
                        Part::structured(json!({"answer": 42})),
                        Part::text("original output"),
                        Part::ToolResult(ToolResultPart::success(
                            "nested",
                            ToolOutput::Parts(vec![Part::text("$.image"), image]),
                        )),
                    ]),
                ))],
            ),
            Item::new(
                ItemKind::Tool,
                vec![Part::ToolResult(ToolResultPart::success(
                    "b",
                    ToolOutput::Text("parallel result".into()),
                ))],
            ),
            Item::text(ItemKind::User, "continue"),
        ],
        available_tools: vec![],
        cache: None,
        metadata: MetadataMap::new(),
    }
}

#[test]
fn projection_preserves_canonical_native_metadata_and_replay() {
    let canonical = request();
    let original = serde_json::to_value(&canonical.transcript).unwrap();
    let projected = project_tool_output_images(canonical.clone(), false).unwrap();
    assert_eq!(
        serde_json::to_value(&canonical.transcript).unwrap(),
        original
    );
    let native = project_tool_output_images(canonical, true).unwrap();
    assert_eq!(native.transcript.len(), 5);
    assert!(
        serde_json::to_string(&native.transcript)
            .unwrap()
            .contains("InlineBytes")
    );
    assert_eq!(projected.transcript.len(), 6);
    assert_eq!(projected.transcript[3].kind, ItemKind::Tool);
    let image = projected.transcript[4]
        .parts
        .iter()
        .find_map(|p| match p {
            Part::Media(m) => Some(m),
            _ => None,
        })
        .unwrap();
    assert_eq!(image.metadata["position"], "$.image");
    let first = serde_json::to_value(&projected.transcript).unwrap();
    let replay = project_tool_output_images(projected, false).unwrap();
    assert_eq!(serde_json::to_value(replay.transcript).unwrap(), first);
}

#[test]
fn whole_item_batch_keeps_all_results_before_multiple_direct_images() {
    let mut request = request();
    let second_call = request.transcript.remove(1);
    request.transcript[0].parts.extend(second_call.parts);
    let second_result = request.transcript.remove(2);
    request.transcript[1].parts.extend(second_result.parts);
    let Part::ToolResult(result) = &mut request.transcript[1].parts[1] else {
        panic!()
    };
    result.output = ToolOutput::Parts(vec![
        Part::text("second image label"),
        Part::media(
            Modality::Image,
            "image/jpeg",
            DataRef::InlineBytes(vec![4, 5, 6]),
        ),
        Part::text("trailing text"),
    ]);
    let projected = project_tool_output_images(request, false).unwrap();
    assert_eq!(projected.transcript.len(), 4);
    assert_eq!(projected.transcript[1].parts.len(), 2);
    assert_eq!(projected.transcript[1].kind, ItemKind::Tool);
    assert_eq!(projected.transcript[2].kind, ItemKind::User);
    let images: Vec<_> = projected.transcript[2]
        .parts
        .iter()
        .filter_map(|p| match p {
            Part::Media(m) => Some(m),
            _ => None,
        })
        .collect();
    assert_eq!(images.len(), 2);
    assert_eq!(images[0].data, DataRef::InlineBytes(vec![1, 2, 3]));
    assert_eq!(images[1].data, DataRef::InlineBytes(vec![4, 5, 6]));
    let tool = serde_json::to_string(&projected.transcript[1]).unwrap();
    assert!(tool.contains("trailing text"));
    assert!(tool.contains("second image label"));
}

#[test]
fn unfinished_batch_does_not_deliver_images_between_results() {
    let mut request = request();
    request.transcript.remove(3);
    assert!(
        project_tool_output_images(request, false)
            .unwrap_err()
            .to_string()
            .contains("all outstanding tool calls")
    );
}

// Genuine HTTP boundary: inspect the bytes emitted by the pinned public
// Completions encoder, also used by OpenRouter (a CompletionsSession alias).
struct CaptureClient(tokio::sync::mpsc::UnboundedSender<Value>);

#[async_trait]
impl HttpClient for CaptureClient {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.0
            .send(serde_json::from_slice(&request.body.unwrap()).unwrap())
            .unwrap();
        Ok(HttpResponse::new(
            StatusCode::BAD_REQUEST,
            HeaderMap::new(),
            request.url,
            Box::pin(futures_util::stream::empty()),
        ))
    }
}

async fn capture_wire<P: CompletionsProvider + 'static>(
    provider: P,
    request: TurnRequest,
) -> Value {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let adapter = CompletionsAdapter::with_client(provider, Http::new(CaptureClient(tx)));
    let mut session = adapter
        .start_session(SessionConfig::new("projection"))
        .await
        .unwrap();
    assert!(session.begin_turn(request, None).await.is_err());
    rx.try_recv().expect("request must reach the HTTP encoder")
}

async fn assert_wire<P: CompletionsProvider + 'static>(provider: P, request: TurnRequest) {
    let body = capture_wire(provider, request).await;
    let messages = body["messages"].as_array().unwrap();
    let roles: Vec<_> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        ["assistant", "assistant", "tool", "tool", "user", "user"]
    );
    assert_eq!(messages[2]["tool_call_id"], "a");
    assert_eq!(messages[3]["tool_call_id"], "b");
    let output = messages[2]["content"].as_str().unwrap();
    assert!(output.contains("original output"));
    assert!(output.contains("answer"));
    assert!(!output.contains("inline_bytes"));
    let content = messages[4]["content"].as_array().unwrap();
    assert!(content.iter().any(|p| p["text"] == "$.image"));
    let images: Vec<_> = content
        .iter()
        .filter(|p| p["type"] == "image_url")
        .collect();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0]["image_url"]["url"], "data:image/png;base64,AQID");
    assert_eq!(messages[5]["content"], "continue");
}

#[tokio::test]
async fn completions_and_openrouter_encode_parallel_mixed_replay_as_user_images() {
    let projected = project_tool_output_images(request(), false).unwrap();
    let speakeasy = SpeakeasyProvider {
        openrouter: OpenRouterProvider::from(OpenRouterConfig::new("test", "test/model")),
        api_key: "test".into(),
        project: "test".into(),
        chat_id: None,
    };
    assert_wire(speakeasy, projected.clone()).await;
    assert_wire(
        OpenRouterProvider::from(OpenRouterConfig::new("test", "test/model")),
        projected,
    )
    .await;
}

#[test]
fn nested_results_encode_with_private_responses_in_both_modes() {
    use agentkit_http::Authentication;
    use agentkit_provider_openai::OpenAIResponsesConfig;
    for native in [false, true] {
        let projected = project_tool_output_images(request(), native).unwrap();
        let config =
            OpenAIResponsesConfig::chatgpt_private("gpt-5.4", Authentication::bearer("test"));
        let wire = config.encode_request(&projected).unwrap();
        let text = serde_json::to_string(&wire).unwrap();
        assert!(text.contains("nested_tool_result"));
        assert!(text.contains("original output"));
        assert!(text.contains("is_error"));
        assert_eq!(text.matches("data:image/png;base64,AQID").count(), 1);
        assert!(!text.contains("InlineBytes"));
    }
}

// Exact maybe_convert_detached representation from patched agentkit-loop
// 8e4ee26. That private method retains arbitrary item/result metadata and adds
// no dedicated marker. It emits a summary followed by serialized results.
fn detached_request() -> TurnRequest {
    let mut request = request();
    let Part::ToolResult(result) = &request.transcript[2].parts[0] else {
        panic!()
    };
    let mut result = result.clone();
    result
        .metadata
        .insert("diagnostic".into(), json!("preserve me"));
    result.is_error = true;
    request.transcript[2].parts = vec![Part::ToolResult(ToolResultPart::success("a", ToolOutput::Text(
        "Tool compose is now running in the background. The result will be delivered when it completes.".into()
    )))];
    let mut notification = Item::new(
        ItemKind::Notification,
        vec![
            Part::text(
                "Background tool results: 1 total, 1 failed, 1 with metadata. a failed: parts payload (3 parts)",
            ),
            Part::structured(serde_json::to_value(result).unwrap()),
        ],
    );
    notification
        .metadata
        .insert("delivery".into(), json!("deferred"));
    // Another foreground call is still outstanding when notification arrives.
    request.transcript.insert(3, notification);
    request
}

#[tokio::test]
async fn detached_notifications_deliver_once_even_on_native_transport() {
    use agentkit_http::Authentication;
    use agentkit_provider_openai::OpenAIResponsesConfig;
    for native in [false, true] {
        let request = detached_request();
        let original = serde_json::to_value(&request.transcript).unwrap();
        let projected = project_tool_output_images(request.clone(), native).unwrap();
        assert_eq!(serde_json::to_value(&request.transcript).unwrap(), original);
        assert_eq!(projected.transcript[3].kind, ItemKind::Notification);
        assert_eq!(projected.transcript[3].metadata["delivery"], "deferred");
        assert_eq!(projected.transcript[4].kind, ItemKind::Tool);
        assert_eq!(
            projected.transcript[5].metadata["kit.projected_tool_images"],
            true
        );
        let note = serde_json::to_string(&projected.transcript[3]).unwrap();
        assert!(!note.contains("InlineBytes"));
        assert!(note.contains("preserve me"));
        assert!(note.contains("$.image"));
        let wire =
            OpenAIResponsesConfig::chatgpt_private("gpt-5.4", Authentication::bearer("test"))
                .encode_request(&projected)
                .unwrap();
        let text = serde_json::to_string(&wire).unwrap();
        assert_eq!(text.matches("data:image/png;base64,AQID").count(), 1);
        assert_eq!(
            wire["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|i| i["type"] == "function_call_output")
                .count(),
            2
        );
        assert!(!text.contains("InlineBytes"));
        let body = capture_wire(
            OpenRouterProvider::from(OpenRouterConfig::new("test", "test/model")),
            projected.clone(),
        )
        .await;
        assert_eq!(
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["role"] == "tool")
                .count(),
            2
        );
        assert_eq!(
            body.to_string()
                .matches("data:image/png;base64,AQID")
                .count(),
            1
        );
        assert!(!body.to_string().contains("InlineBytes"));
        let replay = project_tool_output_images(projected.clone(), native).unwrap();
        assert_eq!(
            serde_json::to_value(replay.transcript).unwrap(),
            serde_json::to_value(projected.transcript).unwrap()
        );
    }
}

#[test]
fn arbitrary_structured_results_are_not_detached_notifications() {
    let mut request = detached_request();
    request.transcript[3].parts[0] = Part::text("Unrelated notification");
    let expected = request.transcript[3].clone();
    let projected = project_tool_output_images(request, false).unwrap();
    assert_eq!(projected.transcript[3], expected);
}

#[test]
fn detached_output_traversal_is_bounded_in_both_modes() {
    for native in [false, true] {
        let mut request = detached_request();
        let Part::Structured(result) = &mut request.transcript[3].parts[1] else {
            panic!()
        };
        let mut nested = json!({"Text": {"text": "deep", "metadata": {}}});
        for _ in 0..65 {
            nested = json!({"ToolResult": {
                "call_id": "nested", "is_error": false, "metadata": {},
                "output": {"Parts": [nested]},
            }});
        }
        result.value["output"] = json!({"Parts": [nested]});
        assert!(
            project_tool_output_images(request, native)
                .unwrap_err()
                .to_string()
                .contains("traversal budget")
        );
        let mut request = detached_request();
        let Part::Structured(result) = &mut request.transcript[3].parts[1] else {
            panic!()
        };
        result.value["output"] =
            json!({"Parts": vec![json!({"Text": {"text": "wide", "metadata": {}}}); 100_001]});
        assert!(
            project_tool_output_images(request, native)
                .unwrap_err()
                .to_string()
                .contains("traversal budget")
        );
    }
}
