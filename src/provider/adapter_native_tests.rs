#![allow(clippy::disallowed_methods, clippy::disallowed_macros)]
use super::*;
use agentkit_core::{Item, ItemKind, MediaPart, MetadataMap, SessionId, TurnId};
use agentkit_http::{
    HeaderMap, Http, HttpClient, HttpError, HttpRequest, HttpResponse, StatusCode,
};
use base64::Engine as _;
use serde_json::json;

fn info(input: &[&str], output: &[&str], tools: bool) -> OpenRouterModelInfo {
    parse_openrouter_model(
        &json!({"data": [{"id":"test/image", "architecture": {
        "input_modalities": input, "output_modalities": output
    }, "supported_parameters": if tools { vec!["tools"] } else { vec![] }}]}),
        "test/image",
    )
    .unwrap()
}

#[test]
fn exact_catalog_capabilities_not_names_or_custom_endpoints() {
    let config = OpenRouterConfig::new("test", "test/image");
    let image = info(&["image", "text"], &["image", "text"], true);
    assert!(
        native_image_capability(&config, Some(&image))
            .unwrap()
            .unwrap()
            .image_input
    );
    assert!(native_image_capability(&config, None).unwrap().is_none());
    assert!(
        native_image_capability(&config, Some(&info(&["text"], &["text"], true)))
            .unwrap()
            .is_none()
    );
    assert!(native_image_capability(&config, Some(&info(&["text"], &["image"], false))).is_err());
    let custom = config.with_base_url("https://custom.invalid/chat/completions");
    assert!(
        native_image_capability(&custom, Some(&image))
            .unwrap()
            .is_none()
    );
    assert!(
        parse_openrouter_model(&json!({"data":[{"id":"different/image"}]}), "test/image").is_none()
    );
}

fn png() -> Vec<u8> {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(2, 2)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

fn image_part(uri: String) -> Part {
    Part::Media(MediaPart::new(
        Modality::Image,
        "image/*",
        DataRef::Uri(uri),
    ))
}

#[test]
fn normalization_validates_bytes_and_never_fetches_remote_media() {
    let bytes = png();
    let mut part = image_part(format!(
        "data:image/*;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ));
    assert_eq!(normalize_native_part(&mut part).unwrap(), (bytes.len(), 4));
    let Part::Media(media) = part else { panic!() };
    assert_eq!(media.mime_type, "image/png");
    assert_eq!(media.data, DataRef::InlineBytes(bytes));
    for uri in [
        "https://example.invalid/image.png",
        "file:///tmp/image.png",
        "data:image/png;base64,???",
        "data:image/png;base64,YWJj",
        "data:image/gif;base64,YWJj",
    ] {
        let error = normalize_native_part(&mut image_part(uri.into()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("native-image-malformed"), "{error}");
    }
    let mut wrong_mime = image_part(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png())
    ));
    assert!(normalize_native_part(&mut wrong_mime).is_err());
    let oversized = format!(
        "data:image/png;base64,{}",
        "A".repeat(MAX_NATIVE_IMAGE_BYTES.div_ceil(3) * 4 + 4)
    );
    assert!(
        normalize_native_part(&mut image_part(oversized))
            .unwrap_err()
            .to_string()
            .contains("native-image-too-large")
    );
}

struct FakeHttp {
    body: Vec<bytes::Bytes>,
    sent: tokio::sync::mpsc::UnboundedSender<Value>,
}

#[async_trait]
impl HttpClient for FakeHttp {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.sent
            .send(serde_json::from_slice(request.body.as_ref().unwrap()).unwrap())
            .unwrap();
        Ok(HttpResponse::new(
            StatusCode::OK,
            HeaderMap::new(),
            request.url,
            Box::pin(futures_util::stream::iter(
                self.body.clone().into_iter().map(Ok),
            )),
        ))
    }
}

fn request(with_image: bool) -> TurnRequest {
    let mut parts = vec![Part::text("make a sticker")];
    if with_image {
        parts.push(Part::Media(MediaPart::new(
            Modality::Image,
            "image/png",
            DataRef::InlineBytes(png()),
        )));
    }
    TurnRequest {
        session_id: SessionId::new("native"),
        turn_id: TurnId::new("turn"),
        transcript: vec![Item::new(ItemKind::User, parts)],
        available_tools: vec![agentkit_tools_core::ToolSpec {
            name: "compose".into(),
            description: "Execute a program".into(),
            input_schema: json!({"type":"object"}),
            output_schema: None,
            annotations: Default::default(),
            metadata: MetadataMap::new(),
        }],
        cache: None,
        metadata: MetadataMap::new(),
    }
}

async fn session(
    body: Vec<bytes::Bytes>,
    image_input: bool,
) -> (KitSession, tokio::sync::mpsc::UnboundedReceiver<Value>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let capability = NativeImageCapability {
        image_input,
        modalities: vec!["image".into(), "text".into()],
    };
    let config =
        native_generation_config(OpenRouterConfig::new("test", "test/image"), &capability).unwrap();
    let adapter = CompletionsAdapter::with_client(
        OpenRouterProvider::from(config),
        Http::new(BoundedImageClient {
            inner: Http::new(FakeHttp { body, sent: tx }),
        }),
    )
    .with_resilience(native_generation_resilience());
    let inner = adapter
        .start_session(SessionConfig::new("native"))
        .await
        .unwrap();
    (
        KitSession::OpenRouter(OpenRouterKitSession {
            inner,
            context_window: None,
            native: Some(capability),
        }),
        rx,
    )
}

fn response(images: Value) -> bytes::Bytes {
    serde_json::to_vec(
        &json!({"id":"gen-test", "model":"test/image", "choices":[{"index":0,"message":{
        "role":"assistant", "content":"{\"sticker\":true}", "images":images
    }, "finish_reason":"stop"}]}),
    )
    .unwrap()
    .into()
}

#[tokio::test]
async fn native_http_request_and_completed_bytes_preserve_real_text_without_labels() {
    let bytes = png();
    let uri = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    );
    let (mut session, mut sent) =
        session(vec![response(json!([{"image_url":{"url":uri}}]))], true).await;
    let mut turn = session.begin_turn(request(true), None).await.unwrap();
    let wire = sent.try_recv().unwrap();
    assert_eq!(wire["modalities"], json!(["image", "text"]));
    assert_eq!(wire["stream"], false);
    assert_eq!(wire["provider"]["require_parameters"], true);
    assert_eq!(wire["tools"][0]["function"]["name"], "compose");
    assert!(
        wire["messages"][0]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|value| value["type"] == "image_url")
    );
    let mut finished = false;
    let mut committed_image = false;
    while let Some(event) = turn.next_event(None).await.unwrap() {
        match event {
            ModelTurnEvent::Delta(Delta::CommitPart {
                part: Part::Media(media),
            }) => {
                assert_eq!(media.mime_type, "image/png");
                assert_eq!(media.data, DataRef::InlineBytes(bytes.clone()));
                committed_image = true;
            }
            ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => {
                assert!(!chunk.contains("[Image"))
            }
            ModelTurnEvent::Finished(result) => {
                let parts: Vec<_> = result
                    .output_items
                    .iter()
                    .flat_map(|item| &item.parts)
                    .collect();
                assert!(parts.iter().any(
                    |part| matches!(part, Part::Text(text) if text.text == "{\"sticker\":true}")
                ));
                assert!(parts.iter().any(|part| matches!(part, Part::Media(media) if media.data == DataRef::InlineBytes(bytes.clone()) && media.mime_type == "image/png")));
                finished = true;
            }
            _ => {}
        }
    }
    assert!(finished);
    assert!(committed_image);
    assert!(
        sent.try_recv().is_err(),
        "successful generation must not be replayed"
    );
}

#[tokio::test]
async fn unsupported_image_input_fails_before_http() {
    let (mut session, mut sent) = session(vec![], false).await;
    assert!(
        session
            .begin_turn(request(true), None)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("does not support image input")
    );
    assert!(sent.try_recv().is_err());
}

#[tokio::test]
async fn transport_bounds_raw_json_and_rejects_malformed_siblings() {
    let chunk = bytes::Bytes::from(vec![b' '; 1024 * 1024]);
    let (mut session, _sent) = session(vec![chunk; 25], true).await;
    // Keep capture channel alive: the fake is an actual HTTP boundary, not instrumentation.
    let error = session
        .begin_turn(request(false), None)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("native-image-response-too-large"), "{error}");
    for images in [
        json!([{}]),
        json!([{"image_url":{"url":"https://example.invalid/x"}}]),
        json!({}),
    ] {
        let (mut session, _sent) = self::session(vec![response(images)], true).await;
        let error = session
            .begin_turn(request(false), None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("native-image-malformed"), "{error}");
    }
}

#[tokio::test]
async fn custom_and_text_models_do_not_send_generation_modalities() {
    for config in [
        OpenRouterConfig::new("test", "test/image")
            .with_base_url("https://custom.invalid/chat/completions"),
        OpenRouterConfig::new("test", "test/text"),
    ] {
        let reported = if config.model == "test/text" {
            info(&["text"], &["text"], true)
        } else {
            info(&["image", "text"], &["image", "text"], true)
        };
        assert!(
            native_image_capability(&config, Some(&reported))
                .unwrap()
                .is_none()
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let adapter = CompletionsAdapter::with_client(
            OpenRouterProvider::from(config.with_streaming(false)),
            Http::new(FakeHttp {
                body: vec![response(json!([]))],
                sent: tx,
            }),
        );
        let mut session = adapter
            .start_session(SessionConfig::new("native"))
            .await
            .unwrap();
        let mut turn = session.begin_turn(request(false), None).await.unwrap();
        let wire = rx.try_recv().unwrap();
        assert!(wire.get("modalities").is_none());
        assert_eq!(wire["tools"][0]["function"]["name"], "compose");
        while let Some(event) = turn.next_event(None).await.unwrap() {
            if let ModelTurnEvent::Finished(result) = event {
                assert!(result.output_items.iter().flat_map(|item| &item.parts).any(
                    |part| matches!(part, Part::Text(text) if text.text == "{\"sticker\":true}")
                ));
            }
        }
    }
}

#[tokio::test]
async fn malformed_content_images_and_sse_are_strict_transport_errors() {
    let malformed_content =
        json!({"choices":[{"message":{"role":"assistant","content":[{"type":"image_url"}]}}]});
    for body in [
        serde_json::to_vec(&malformed_content).unwrap(),
        b"data: {\"choices\":[]}\n\n".to_vec(),
        br#"{"choices":[]}"#.to_vec(),
    ] {
        let (mut session, _sent) = session(vec![body.into()], true).await;
        let error = session
            .begin_turn(request(false), None)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("native-image-malformed"), "{error}");
    }
}

#[test]
fn excessive_dimensions_are_size_errors_not_malformed_media() {
    let mut bytes = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgba8(8193, 1)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    let uri = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())
    );
    let error = normalize_native_part(&mut image_part(uri))
        .unwrap_err()
        .to_string();
    assert!(error.contains("native-image-too-large"), "{error}");
}

fn endpoint_document(model: &str, endpoints: Value) -> Value {
    json!({"data":{"id":model,"architecture":{"input_modalities":["image","text"],"output_modalities":["image","text"]},"endpoints":endpoints}})
}

struct EndpointHttp {
    body: Vec<bytes::Bytes>,
    status: StatusCode,
    sent: tokio::sync::mpsc::UnboundedSender<String>,
}

#[async_trait]
impl HttpClient for EndpointHttp {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        assert!(request.body.is_none());
        self.sent.send(request.url.clone()).unwrap();
        Ok(HttpResponse::new(
            self.status,
            HeaderMap::new(),
            request.url,
            Box::pin(futures_util::stream::iter(
                self.body.clone().into_iter().map(Ok),
            )),
        ))
    }
}

fn endpoint_client(
    body: Vec<bytes::Bytes>,
    status: StatusCode,
) -> (Http, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let (sent, received) = tokio::sync::mpsc::unbounded_channel();
    (Http::new(EndpointHttp { body, status, sent }), received)
}

#[tokio::test]
async fn routing_selectors_with_empty_endpoints_remain_legacy_without_name_heuristics() {
    for model in [
        "openrouter/auto",
        "openrouter/auto-beta",
        "other/arbitrary-router",
    ] {
        let document = endpoint_document(model, json!([]));
        let (client, mut requests) = endpoint_client(
            vec![serde_json::to_vec(&document).unwrap().into()],
            StatusCode::OK,
        );
        let config = OpenRouterConfig::new("test", model);
        // Empty endpoints remain legacy even when the aggregate entry lacks tools.
        let catalog = info(&["image", "text"], &["image", "text"], false);
        assert!(
            discover_native_image(&client, &config, Some(&catalog))
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            requests.try_recv().unwrap(),
            format!("https://openrouter.ai/api/v1/models/{model}/endpoints")
        );
    }
}

#[tokio::test]
async fn concrete_mixed_endpoints_enable_generation_and_preserve_routing_privacy() {
    let catalog = info(&["image", "text"], &["image", "text"], true);
    let document = endpoint_document(
        "test/image",
        json!([
            {"supported_parameters":["temperature"]},
            {"supported_parameters":["tools","tool_choice"]}
        ]),
    );
    let (client, _requests) = endpoint_client(
        vec![serde_json::to_vec(&document).unwrap().into()],
        StatusCode::OK,
    );
    let config = OpenRouterConfig::new("test", "test/image").with_extra_body_value("provider", json!({
        "data_collection":"deny", "order":["Google AI Studio"], "allow_fallbacks":false, "require_parameters":false
    }));
    let capability = discover_native_image(&client, &config, Some(&catalog))
        .await
        .unwrap()
        .unwrap();
    let native = native_generation_config(config, &capability).unwrap();
    assert_eq!(native.extra_body["modalities"], json!(["image", "text"]));
    assert_eq!(
        native.extra_body["provider"],
        json!({
            "data_collection":"deny", "order":["Google AI Studio"], "allow_fallbacks":false, "require_parameters":true
        })
    );
    let invalid =
        OpenRouterConfig::new("test", "test/image").with_extra_body_value("provider", "invalid");
    assert!(native_generation_config(invalid, &capability).is_err());
}

#[tokio::test]
async fn endpoint_ineligibility_malformed_and_failed_discovery_do_not_fabricate_capability() {
    let config = OpenRouterConfig::new("test", "test/image");
    let catalog = info(&["image", "text"], &["image", "text"], true);
    let no_tools = endpoint_document(
        "test/image",
        json!([{"supported_parameters":["temperature"]}]),
    );
    let mut wrong_architecture = endpoint_document("test/image", json!([]));
    wrong_architecture["data"]["architecture"]["output_modalities"] = json!(["text"]);
    let mut too_many = endpoint_document("test/image", json!([]));
    too_many["data"]["endpoints"] = json!(vec![
        json!({"supported_parameters":["tools"]});
        MAX_NATIVE_ENDPOINTS + 1
    ]);
    let cases = [
        (no_tools, "native-image-ineligible"),
        (
            endpoint_document("wrong/model", json!([])),
            "native-image-discovery",
        ),
        (wrong_architecture, "native-image-discovery"),
        (
            endpoint_document("test/image", json!([{}])),
            "native-image-discovery",
        ),
        (
            endpoint_document(
                "test/image",
                json!([{"supported_parameters":vec!["tools"; MAX_CAPABILITY_VALUES + 1]}]),
            ),
            "native-image-discovery",
        ),
        (too_many, "native-image-discovery"),
        (
            json!({"data":{"id":"test/image"}}),
            "native-image-discovery",
        ),
    ];
    for (document, code) in cases {
        let (client, _requests) = endpoint_client(
            vec![serde_json::to_vec(&document).unwrap().into()],
            StatusCode::OK,
        );
        let error = discover_native_image(&client, &config, Some(&catalog))
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains(code), "{error}");
    }
    for (body, status) in [
        (vec![bytes::Bytes::from_static(b"not JSON")], StatusCode::OK),
        (
            vec![
                bytes::Bytes::from(vec![b' '; MAX_MODELS_BYTES]),
                bytes::Bytes::from_static(b"x"),
            ],
            StatusCode::OK,
        ),
        (vec![], StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let (client, _requests) = endpoint_client(body, status);
        assert!(
            discover_native_image(&client, &config, Some(&catalog))
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("native-image-discovery")
        );
    }
}

#[tokio::test]
async fn custom_and_nonimage_routes_never_fetch_official_endpoints() {
    let (client, mut requests) = endpoint_client(vec![], StatusCode::SERVICE_UNAVAILABLE);
    let config = OpenRouterConfig::new("test", "test/image");
    let catalog = info(&["image", "text"], &["image", "text"], true);
    let custom = config
        .clone()
        .with_base_url("https://custom.invalid/chat/completions");
    assert!(
        discover_native_image(&client, &custom, Some(&catalog))
            .await
            .unwrap()
            .is_none()
    );
    let text = info(&["text"], &["text"], true);
    assert!(
        discover_native_image(&client, &config, Some(&text))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        discover_native_image(&client, &config, None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(requests.try_recv().is_err());
}

#[test]
fn endpoint_model_paths_cannot_inject_query_fragment_or_traversal() {
    for model in [
        "author/model?x=y",
        "author/model#fragment",
        "author/../model",
        "author//model",
        "author/%2e%2e",
        "model",
        "author/.",
    ] {
        assert!(native_endpoints_url(model).is_err(), "{model}");
    }
    assert_eq!(
        native_endpoints_url("author/model:free").unwrap().as_str(),
        "https://openrouter.ai/api/v1/models/author/model:free/endpoints"
    );
}

struct FailedEndpointHttp;
#[async_trait]
impl HttpClient for FailedEndpointHttp {
    async fn execute(&self, _request: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::Other("fixture transport unavailable".into()))
    }
}

#[tokio::test]
async fn failed_endpoint_transport_is_unknown_not_legacy_or_ineligible() {
    let client = Http::new(FailedEndpointHttp);
    let config = OpenRouterConfig::new("test", "test/image");
    let catalog = info(&["image", "text"], &["image", "text"], true);
    let error = discover_native_image(&client, &config, Some(&catalog))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("native-image-discovery"));
    assert!(error.contains("eligibility is unknown"));
}

async fn completed_items(turn: &mut KitTurn) -> Vec<Item> {
    let mut output = None;
    while let Some(event) = turn.next_event(None).await.unwrap() {
        if let ModelTurnEvent::Finished(result) = event {
            output = Some(result.output_items);
        }
    }
    output.expect("completed native turn")
}

fn assert_historical_image_wire(wire: &Value, parallel_tools: bool) {
    let messages = wire["messages"].as_array().unwrap();
    let image_position = messages
        .iter()
        .position(|message| {
            message["role"] == "user"
                && message["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|part| part["type"] == "image_url"))
        })
        .expect("historical generated image must be encoded as actual image input");
    let image = messages[image_position]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|part| part["type"] == "image_url")
        .unwrap();
    assert_eq!(
        image["image_url"]["url"],
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png())
        )
    );
    assert!(messages.iter().any(
        |message| message["role"] == "assistant" && message["content"] == "{\"sticker\":true}"
    ));
    if parallel_tools {
        let positions: Vec<_> = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message["role"] == "tool")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(positions.len(), 2);
        assert!(positions.iter().all(|index| *index < image_position));
        assert_eq!(messages[positions[0]]["tool_call_id"], "a");
        assert_eq!(messages[positions[1]]["tool_call_id"], "b");
    }
}

#[tokio::test]
async fn native_generated_history_supports_second_prompt_tool_roundtrip_and_restored_sessions() {
    use agentkit_core::{ToolOutput, ToolResultPart};
    for parallel_tools in [false, true] {
        let uri = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png())
        );
        let mut body: Value =
            serde_json::from_slice(&response(json!([{"image_url":{"url":uri}}]))).unwrap();
        if parallel_tools {
            body["choices"][0]["finish_reason"] = json!("tool_calls");
            body["choices"][0]["message"]["tool_calls"] = json!([
                {"id":"a","type":"function","function":{"name":"compose","arguments":"{}"}},
                {"id":"b","type":"function","function":{"name":"compose","arguments":"{}"}}
            ]);
        }
        let response_bytes: bytes::Bytes = serde_json::to_vec(&body).unwrap().into();
        let (mut live, mut sent) = session(vec![response_bytes.clone()], true).await;
        let first_request = request(false);
        let mut first = live.begin_turn(first_request.clone(), None).await.unwrap();
        sent.try_recv().unwrap();
        let first_output = completed_items(&mut first).await;
        let original_output = serde_json::to_value(&first_output).unwrap();
        let mut continuation = first_request;
        continuation.transcript.extend(first_output.clone());
        if parallel_tools {
            for id in ["a", "b"] {
                continuation.transcript.push(Item::new(
                    ItemKind::Tool,
                    vec![Part::ToolResult(ToolResultPart::success(
                        id,
                        ToolOutput::Text(format!("result {id}")),
                    ))],
                ));
            }
        }
        continuation.transcript.push(Item::new(
            ItemKind::User,
            vec![Part::text("Refine that sticker")],
        ));
        let history = serde_json::to_value(&continuation).unwrap();
        let mut second = live.begin_turn(continuation.clone(), None).await.unwrap();
        assert_historical_image_wire(&sent.try_recv().unwrap(), parallel_tools);
        completed_items(&mut second).await;
        assert_eq!(
            serde_json::to_value(&first_output).unwrap(),
            original_output
        );
        assert_eq!(serde_json::to_value(&continuation).unwrap(), history);

        // Fork/resume reconstruct new provider sessions from canonical history,
        // not a provider-private cache or the already projected outbound copy.
        for _ in ["fork", "resume"] {
            let restored: TurnRequest = serde_json::from_value(history.clone()).unwrap();
            let (mut reconstructed, mut sent) = session(vec![response_bytes.clone()], true).await;
            let mut turn = reconstructed.begin_turn(restored, None).await.unwrap();
            assert_historical_image_wire(&sent.try_recv().unwrap(), parallel_tools);
            completed_items(&mut turn).await;
        }
    }
}

#[tokio::test]
async fn image_only_assistant_history_has_no_empty_outbound_assistant_message() {
    let mut history = request(false);
    history.transcript.push(Item::new(
        ItemKind::Assistant,
        vec![Part::Media(MediaPart::new(
            Modality::Image,
            "image/png",
            DataRef::InlineBytes(png()),
        ))],
    ));
    let original = history.clone();
    let (mut session, mut sent) = session(vec![response(json!([]))], true).await;
    session.begin_turn(history.clone(), None).await.unwrap();
    let wire = sent.try_recv().unwrap();
    assert!(
        wire["messages"]
            .as_array()
            .unwrap()
            .iter()
            .all(|message| message["role"] != "assistant")
    );
    assert_eq!(history, original);
}

#[test]
fn historical_assistant_projection_is_bounded_and_waits_for_all_tool_results() {
    use agentkit_core::ToolCallPart;
    let image = Part::Media(MediaPart::new(
        Modality::Image,
        "image/png",
        DataRef::InlineBytes(png()),
    ));
    let mut history = request(false);
    history.transcript.push(Item::new(
        ItemKind::Assistant,
        vec![
            image.clone(),
            Part::ToolCall(ToolCallPart::new("pending", "compose", json!({}))),
        ],
    ));
    assert!(
        project_tool_output_images(history.clone(), false)
            .unwrap_err()
            .to_string()
            .contains("outstanding tool calls")
    );
    history.transcript.last_mut().unwrap().parts = vec![image; 9];
    assert!(
        project_tool_output_images(history, false)
            .unwrap_err()
            .to_string()
            .contains("historical assistant images exceed")
    );
}

#[test]
fn native_generation_policy_is_finite_and_never_replays_ambiguous_billable_work() {
    let policy = native_generation_resilience();
    assert_eq!(policy.max_retries, 0);
    assert_eq!(policy.attempt_timeout, Some(Duration::from_secs(300)));
    assert_eq!(policy.stream_idle_timeout, Some(Duration::from_secs(300)));
    assert_eq!(policy.retry_budget, Duration::from_secs(310));
    assert_eq!(NATIVE_GENERATION_TIMEOUT, Duration::from_secs(300));
}

struct PendingGenerationHttp(tokio::sync::mpsc::UnboundedSender<()>);
#[async_trait]
impl HttpClient for PendingGenerationHttp {
    async fn execute(&self, _request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.0.send(()).unwrap();
        futures_util::future::pending().await
    }
}

struct AmbiguousGenerationHttp(tokio::sync::mpsc::UnboundedSender<()>);
#[async_trait]
impl HttpClient for AmbiguousGenerationHttp {
    async fn execute(&self, _request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.0.send(()).unwrap();
        Err(HttpError::Timeout {
            operation: "fixture accepted generation",
            timeout: NATIVE_GENERATION_TIMEOUT,
        })
    }
}

async fn session_with_http(http: Http) -> KitSession {
    let capability = NativeImageCapability {
        image_input: true,
        modalities: vec!["image".into(), "text".into()],
    };
    let config =
        native_generation_config(OpenRouterConfig::new("test", "test/image"), &capability).unwrap();
    let adapter = CompletionsAdapter::with_client(
        OpenRouterProvider::from(config),
        Http::new(BoundedImageClient { inner: http }),
    )
    .with_resilience(native_generation_resilience());
    let inner = adapter
        .start_session(SessionConfig::new("native"))
        .await
        .unwrap();
    KitSession::OpenRouter(OpenRouterKitSession {
        inner,
        context_window: None,
        native: Some(capability),
    })
}

#[tokio::test]
async fn native_pending_http_is_cancellable_and_ambiguous_timeout_is_not_replayed() {
    // This is only a deadlock guard; no wall-clock performance assertion or sleep.
    tokio::time::timeout(Duration::from_secs(5), async {
        let (started, mut received) = tokio::sync::mpsc::unbounded_channel();
        let mut session = session_with_http(Http::new(PendingGenerationHttp(started))).await;
        let controller = agentkit_core::CancellationController::new();
        let checkpoint = controller.handle().checkpoint();
        let (result, _) = tokio::join!(
            session.begin_turn(request(false), Some(checkpoint)),
            async {
                received.recv().await.unwrap();
                controller.interrupt();
            }
        );
        assert!(matches!(result, Err(LoopError::Cancelled)));
        assert!(received.try_recv().is_err());

        let (started, mut received) = tokio::sync::mpsc::unbounded_channel();
        let mut session = session_with_http(Http::new(AmbiguousGenerationHttp(started))).await;
        assert!(session.begin_turn(request(false), None).await.is_err());
        received.try_recv().unwrap();
        assert!(
            received.try_recv().is_err(),
            "ambiguous billable generation must not be retried"
        );
    })
    .await
    .unwrap();
}
