//! Cerebras completions transport and legacy OpenRouter-endpoint compatibility.
use agentkit_adapter_completions::CompletionsProvider;
use agentkit_core::{MetadataMap, Usage};
use agentkit_http::{Authentication, HttpRequestBuilder, ResilienceConfig, StatusCode};
use agentkit_loop::{LoopError, TurnRequest};
use agentkit_provider_openrouter::{OpenRouterConfig, OpenRouterProvider, OpenRouterRequestConfig};
use serde_json::{Map, Value};

#[derive(Clone)]
pub(super) struct CerebrasCompatibleOpenRouter(OpenRouterProvider, bool);

pub(super) const COMPLETIONS_URL: &str = "https://api.cerebras.ai/v1/chat/completions";

// Official shared-model catalog: https://inference-docs.cerebras.ai/models/overview.
// Context counts are conservative free-tier baselines from rounded documentation,
// not exact architectural maxima or a promise of account-specific allowances.
pub(super) const MODELS: &[(&str, u64)] = &[("gpt-oss-120b", 65_000), ("qwen-3.8-27b", 64_000)];

pub(super) fn context_window(model: &str) -> Option<u64> {
    MODELS
        .iter()
        .find_map(|(id, limit)| (*id == model).then_some(*limit))
}

impl CerebrasCompatibleOpenRouter {
    pub(super) fn dedicated(config: OpenRouterConfig) -> Self {
        Self(config.into(), true)
    }

    fn is_cerebras(&self) -> bool {
        self.1
            || url::Url::parse(self.endpoint_url())
                .ok()
                .is_some_and(|url| url.host_str() == Some("api.cerebras.ai"))
    }
}

impl From<OpenRouterConfig> for CerebrasCompatibleOpenRouter {
    fn from(config: OpenRouterConfig) -> Self {
        Self(config.into(), false)
    }
}

impl CompletionsProvider for CerebrasCompatibleOpenRouter {
    type Config = OpenRouterRequestConfig;

    fn provider_name(&self) -> &str {
        if self.1 {
            "cerebras"
        } else {
            self.0.provider_name()
        }
    }
    fn endpoint_url(&self) -> &str {
        self.0.endpoint_url()
    }
    fn config(&self) -> &Self::Config {
        self.0.config()
    }
    fn preprocess_request(&self, builder: HttpRequestBuilder) -> HttpRequestBuilder {
        if self.1 {
            builder.header("User-Agent", concat!("kit/", env!("CARGO_PKG_VERSION")))
        } else {
            self.0.preprocess_request(builder)
        }
    }
    fn authentication(&self) -> Option<Authentication> {
        self.0.authentication()
    }
    fn resilience_config(&self) -> Option<ResilienceConfig> {
        self.0.resilience_config()
    }
    fn streaming(&self) -> bool {
        self.0.streaming()
    }
    fn apply_stream_options(&self, body: &mut Map<String, Value>) -> Result<(), LoopError> {
        if self.is_cerebras() {
            // Cerebras includes usage in its final chunk and does not accept
            // OpenRouter's stream_options extension.
            body.remove("stream_options");
            Ok(())
        } else {
            self.0.apply_stream_options(body)
        }
    }
    fn apply_prompt_cache(
        &self,
        body: &mut Map<String, Value>,
        request: &TurnRequest,
    ) -> Result<(), LoopError> {
        if self.is_cerebras() {
            normalize_system_messages(body);
            if let Some(reasoning) = body.remove("reasoning")
                && let Some(effort) = reasoning.get("effort")
            {
                body.entry("reasoning_effort")
                    .or_insert_with(|| effort.clone());
            }
            // Prompt caching is automatic; OpenRouter cache_control annotations
            // are not part of Cerebras's request contract.
            Ok(())
        } else {
            self.0.apply_prompt_cache(body, request)
        }
    }

    fn preprocess_response(&self, status: StatusCode, body: &str) -> Result<(), LoopError> {
        if !self.is_cerebras() {
            return self.0.preprocess_response(status, body);
        }
        let error = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|value| value.get("error").cloned());
        if !status.is_success() || error.is_some() {
            // Never include untrusted upstream text: servers can echo bearer keys,
            // prompts, or nested provider metadata in their error payloads.
            let rate_limited = status.as_u16() == 429
                || error
                    .as_ref()
                    .and_then(|e| e.get("code"))
                    .is_some_and(|code| {
                        code == 429 || code == "429" || code == "rate_limit_exceeded"
                    });
            let kind = if rate_limited {
                "rate limited"
            } else {
                "request failed"
            };
            return Err(LoopError::Provider(format!(
                "Cerebras {kind} (HTTP {})",
                status.as_u16()
            )));
        }
        Ok(())
    }
    fn postprocess_response(
        &self,
        usage: &mut Option<Usage>,
        metadata: &mut MetadataMap,
        raw: &Value,
    ) {
        if self.1 {
            if let Some(model) = raw.get("model").and_then(Value::as_str) {
                metadata.insert("cerebras.model".into(), Value::String(model.into()));
            }
        } else {
            self.0.postprocess_response(usage, metadata, raw)
        }
    }
}

// Agentkit maps both System and Context items to system messages. Cerebras
// permits a system message only at index zero, so combine the initial block
// without hoisting later instructions or mutating the persisted transcript.
fn normalize_system_messages(body: &mut Map<String, Value>) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let count = messages
        .iter()
        .take_while(|message| message["role"] == "system")
        .count();
    // Later system/context instructions stay at their conversational position,
    // but must use a supported role (Cerebras allows system only at index zero).
    for message in messages.iter_mut().skip(count.max(1)) {
        if message["role"] == "system" {
            message["role"] = Value::String("user".into());
        }
    }
    if count < 2 {
        return;
    }
    // Be conservative with non-text content (for example explicit cache blocks):
    // do not silently discard structure or message-level fields.
    let contents: Option<Vec<&str>> = messages[..count]
        .iter()
        .map(|message| {
            (message.as_object()?.len() == 2).then_some(())?;
            message["content"].as_str()
        })
        .collect();
    let Some(contents) = contents else {
        for message in messages.iter_mut().take(count).skip(1) {
            message["role"] = Value::String("user".into());
        }
        return;
    };
    let content = contents.join("\n\n");
    messages[0]["content"] = Value::String(content);
    messages.drain(1..count);
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_adapter_completions::CompletionsAdapter;
    use agentkit_core::{Item, ItemKind, SessionId, TurnId};
    use agentkit_loop::{ModelAdapter, ModelSession, SessionConfig};
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    fn request() -> TurnRequest {
        TurnRequest {
            session_id: SessionId::new("cerebras-test"),
            turn_id: TurnId::new("turn"),
            transcript: vec![
                Item::text(ItemKind::System, "instructions"),
                Item::text(ItemKind::User, "hello"),
            ],
            available_tools: vec![agentkit_tools_core::ToolSpec {
                name: "lookup".into(),
                description: "Look up x".into(),
                input_schema: json!({"type":"object", "properties":{"x":{"type":"integer"}},"required":["x"]}),
                output_schema: None,
                annotations: Default::default(),
                metadata: MetadataMap::new(),
            }],
            cache: None,
            metadata: MetadataMap::new(),
        }
    }

    fn mock_adapter(
        status: u16,
        response: String,
    ) -> (
        CompletionsAdapter<CerebrasCompatibleOpenRouter>,
        std::thread::JoinHandle<Value>,
    ) {
        mock_adapter_attempts(status, response, 1)
    }

    fn mock_adapter_attempts(
        status: u16,
        response: String,
        attempts: usize,
    ) -> (
        CompletionsAdapter<CerebrasCompatibleOpenRouter>,
        std::thread::JoinHandle<Value>,
    ) {
        mock_adapter_responses(vec![(status, response); attempts])
    }

    fn mock_adapter_responses(
        responses: Vec<(u16, String)>,
    ) -> (
        CompletionsAdapter<CerebrasCompatibleOpenRouter>,
        std::thread::JoinHandle<Value>,
    ) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let attempts = responses.len();
            let requests: Vec<Value> = responses.into_iter().map(|(status, response)| {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let body = loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                let size: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= end + 4 + size {
                    assert!(
                        headers
                            .to_lowercase()
                            .contains("authorization: bearer synthetic-key")
                    );
                    assert!(!headers.to_lowercase().contains("http-referer:"));
                    break serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + size])
                        .unwrap();
                }
            };
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            body
            }).collect();
            if attempts == 1 {
                requests.into_iter().next().unwrap()
            } else {
                json!(requests)
            }
        });
        let mut config = OpenRouterConfig::new("synthetic-key", "qwen-3.8-27b")
            .with_base_url(format!("http://{address}/v1/chat/completions"));
        config
            .extra_body
            .insert("reasoning_effort".into(), json!("low"));
        config.max_completion_tokens = Some(512);
        config.parallel_tool_calls = Some(true);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        (
            CompletionsAdapter::with_client(
                CerebrasCompatibleOpenRouter::dedicated(config),
                agentkit_http::Http::new(client),
            ),
            server,
        )
    }

    #[tokio::test]
    async fn dedicated_streaming_text_tools_and_usage() {
        use agentkit_loop::{ModelTurn, ModelTurnEvent};
        let chunks = [
            json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"lookup","arguments":"{\"x\":"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":4,"total_tokens":16}}),
        ];
        let response = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n";
        let (adapter, server) = mock_adapter(200, response);
        let mut session = adapter
            .start_session(SessionConfig::new("cerebras-test"))
            .await
            .unwrap();
        assert_eq!(session.provider_name(), Some("cerebras"));
        let turn = session.begin_turn(request(), None).await.unwrap();
        let mut turn = super::super::adapter::KitTurn::cerebras_for_test(turn, "qwen-3.8-27b");
        let mut events = Vec::new();
        while let Some(event) = turn.next_event(None).await.unwrap() {
            events.push(event);
        }
        let rendered = format!("{events:?}");
        assert!(rendered.contains("Hello"), "{rendered}");
        assert!(
            events.iter().any(
                |event| matches!(event, ModelTurnEvent::ToolCall(call) if call.name == "lookup")
            )
        );
        assert!(events.iter().any(
            |event| matches!(event, ModelTurnEvent::Usage(usage) if usage.tokens.as_ref().is_some_and(|tokens| tokens.input_tokens == 12))
        ));
        for event in &events {
            if let ModelTurnEvent::Usage(usage) = event {
                assert_eq!(usage.metadata["context_window"], 64_000);
                assert_eq!(usage.metadata["cerebras.context_length"], 64_000);
                assert!(!usage.metadata.contains_key("openrouter.context_length"));
            }
            if let ModelTurnEvent::Finished(result) = event
                && let Some(usage) = &result.usage
            {
                assert_eq!(usage.metadata["context_window"], 64_000);
            }
        }
        let body = server.join().unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning_effort"], "low");
        assert_eq!(body["max_completion_tokens"], 512);
        assert_eq!(body["parallel_tool_calls"], true);
        assert!(body.get("stream_options").is_none());
        assert!(body.get("reasoning").is_none());
        assert_eq!(body["tools"][0]["function"]["name"], "lookup");
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["required"],
            json!(["x"])
        );
    }

    #[tokio::test]
    async fn parallel_tool_calls_round_trip_with_later_compaction_context() {
        use agentkit_core::{Part, ToolOutput, ToolResultPart};
        use agentkit_loop::{ModelTurn, ModelTurnEvent};
        let chunks = [
            json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
                {"index":0,"id":"call-first","type":"function","function":{"name":"lookup","arguments":"{\"x\":"}},
                {"index":1,"id":"call-second","type":"function","function":{"name":"lookup","arguments":"{"}}
            ]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":1,"function":{"arguments":"\"x\":2"}},
                {"index":0,"function":{"arguments":"1"}}
            ]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"}"}},
                {"index":1,"function":{"arguments":"}"}}
            ]},"finish_reason":"tool_calls"}]}),
        ];
        let first_response = chunks
            .iter()
            .map(|chunk| format!("data: {chunk}\n\n"))
            .collect::<String>()
            + "data: [DONE]\n\n";
        let final_chunk = json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"Both lookups completed."},"finish_reason":"stop"}]});
        let (adapter, server) = mock_adapter_responses(vec![
            (200, first_response),
            (200, format!("data: {final_chunk}\n\ndata: [DONE]\n\n")),
        ]);
        let mut session = adapter
            .start_session(SessionConfig::new("cerebras-test"))
            .await
            .unwrap();
        let mut turn = session.begin_turn(request(), None).await.unwrap();
        let mut calls = Vec::new();
        while let Some(event) = turn.next_event(None).await.unwrap() {
            if let ModelTurnEvent::ToolCall(call) = event {
                calls.push(call);
            }
        }
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id.to_string(), "call-first");
        assert_eq!(calls[0].input, json!({"x":1}));
        assert_eq!(calls[1].id.to_string(), "call-second");
        assert_eq!(calls[1].input, json!({"x":2}));

        let mut follow_up = request();
        follow_up.turn_id = TurnId::new("turn-two");
        follow_up.transcript.push(Item::new(
            ItemKind::Assistant,
            calls.iter().cloned().map(Part::ToolCall).collect(),
        ));
        for call in &calls {
            follow_up.transcript.push(Item::new(
                ItemKind::Tool,
                vec![Part::ToolResult(ToolResultPart::success(
                    call.id.clone(),
                    ToolOutput::text(format!("result for {}", call.input["x"])),
                ))],
            ));
        }
        follow_up.transcript.push(Item::text(
            ItemKind::Context,
            "Compaction summary: preserve both lookup results.",
        ));
        let mut turn = session.begin_turn(follow_up, None).await.unwrap();
        let mut final_items = Vec::new();
        while let Some(event) = turn.next_event(None).await.unwrap() {
            if let ModelTurnEvent::Finished(result) = event {
                final_items.extend(result.output_items);
            }
        }
        assert!(final_items.iter().flat_map(|item| &item.parts).any(
            |part| matches!(part, Part::Text(text) if text.text == "Both lookups completed.")
        ));
        let bodies = server.join().unwrap();
        assert_eq!(bodies.as_array().unwrap().len(), 2);
        assert_eq!(bodies[0]["parallel_tool_calls"], true);
        let messages = bodies[1]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 6);
        assert_eq!(messages[0]["role"], "system");
        assert!(
            messages
                .iter()
                .skip(1)
                .all(|message| message["role"] != "system")
        );
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call-first");
        assert_eq!(messages[2]["tool_calls"][1]["id"], "call-second");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call-first");
        assert_eq!(messages[3]["content"], "result for 1");
        assert_eq!(messages[4]["role"], "tool");
        assert_eq!(messages[4]["tool_call_id"], "call-second");
        assert_eq!(messages[4]["content"], "result for 2");
        assert_eq!(messages[5]["role"], "user");
        assert_eq!(
            messages[5]["content"],
            "Compaction summary: preserve both lookup results."
        );
    }

    #[test]
    fn dedicated_errors_and_metadata_have_cerebras_identity() {
        let provider =
            CerebrasCompatibleOpenRouter::dedicated(OpenRouterConfig::new("synthetic-key", "test"));
        for (status, body) in [
            (StatusCode::UNAUTHORIZED, "synthetic-key"),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "<html>private prompt</html>",
            ),
            (
                StatusCode::OK,
                r#"{"error":{"code":"bad_request","message":"synthetic-key"}}"#,
            ),
        ] {
            let error = provider
                .preprocess_response(status, body)
                .unwrap_err()
                .to_string();
            assert!(error.contains("Cerebras"));
            assert!(!error.contains("synthetic-key"));
            assert!(!error.contains("private prompt"));
        }
        let mut metadata = MetadataMap::new();
        provider.postprocess_response(&mut None, &mut metadata, &json!({"model":"test"}));
        assert_eq!(metadata["cerebras.model"], "test");
        assert!(!metadata.contains_key("openrouter.model"));
    }

    #[tokio::test]
    async fn dedicated_http_errors_never_echo_upstream_secrets() {
        for code in [json!(429), json!("429"), json!("rate_limit_exceeded")] {
            let (adapter, server) = mock_adapter(429, json!({"error":{"code":code,"message":"synthetic-key", "metadata":{"raw":"private prompt"}}}).to_string());
            let mut session = adapter
                .start_session(SessionConfig::new("cerebras-test"))
                .await
                .unwrap();
            let error = match session.begin_turn(request(), None).await {
                Ok(_) => panic!("expected error"),
                Err(error) => error.to_string(),
            };
            assert!(error.contains("Cerebras rate limited"), "{error}");
            assert!(!error.contains("synthetic-key"));
            assert!(!error.contains("private prompt"));
            assert!(!error.contains("OpenRouter"));
            server.join().unwrap();
        }
    }

    #[tokio::test]
    async fn dedicated_http_rate_limit_retries_are_bounded_and_sanitized() {
        let (adapter, server) = mock_adapter_attempts(
            429,
            json!({"error":{"code":"429", "message":"synthetic-key private prompt"}}).to_string(),
            3,
        );
        let adapter = adapter.with_resilience(ResilienceConfig {
            max_retries: 2,
            initial_backoff: std::time::Duration::from_millis(1),
            max_backoff: std::time::Duration::from_millis(1),
            ..Default::default()
        });
        let mut session = adapter
            .start_session(SessionConfig::new("cerebras-test"))
            .await
            .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session.begin_turn(request(), None),
        )
        .await
        .unwrap();
        let error = match result {
            Ok(_) => panic!("expected HTTP error"),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("Cerebras rate limited (HTTP 429)"),
            "{error}"
        );
        assert!(!error.contains("synthetic-key"));
        assert!(!error.contains("private prompt"));
        let requests = server.join().unwrap();
        assert_eq!(requests.as_array().unwrap().len(), 3);
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[1], requests[2]);
    }

    #[test]
    fn catalog_context_limits_are_conservative_free_tier_baselines() {
        assert_eq!(
            MODELS,
            &[("gpt-oss-120b", 65_000), ("qwen-3.8-27b", 64_000)]
        );
        assert_eq!(context_window("gpt-oss-120b"), Some(65_000));
        assert_eq!(context_window("qwen-3.8-27b"), Some(64_000));
        assert_eq!(context_window("custom-model"), None);
        assert_eq!(context_window("llama-3.3-70b"), None);
    }

    #[tokio::test]
    async fn dedicated_stream_errors_are_sanitized() {
        use agentkit_loop::ModelTurn;
        let (adapter, server) = mock_adapter(
            200,
            "event: error\ndata: {\"error\":{\"message\":\"synthetic-key private prompt\"}}\n\n"
                .into(),
        );
        let mut session = adapter
            .start_session(SessionConfig::new("cerebras-test"))
            .await
            .unwrap();
        let turn = session.begin_turn(request(), None).await.unwrap();
        let mut turn = super::super::adapter::KitTurn::cerebras_for_test(turn, "qwen-3.8-27b");
        let error = turn.next_event(None).await.unwrap_err().to_string();
        assert!(error.contains("Cerebras"));
        assert!(!error.contains("synthetic-key"));
        assert!(!error.contains("private prompt"));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn dedicated_cancellation_stops_stream_consumption() {
        use agentkit_core::CancellationController;
        use agentkit_loop::ModelTurn;
        let (adapter, server) = mock_adapter(200, "data: [DONE]\n\n".into());
        let mut session = adapter
            .start_session(SessionConfig::new("cerebras-test"))
            .await
            .unwrap();
        let mut turn = session.begin_turn(request(), None).await.unwrap();
        let controller = CancellationController::new();
        let cancellation = controller.handle().checkpoint();
        controller.interrupt();
        assert!(matches!(
            turn.next_event(Some(cancellation)).await,
            Err(LoopError::Cancelled)
        ));
        server.join().unwrap();
    }

    // Real Agentkit serialization + HTTP, using synthetic messages and a dummy key only.
    async fn wire_messages(host: &str) -> Value {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let body = loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                let size: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= end + 4 + size {
                    assert_eq!(
                        headers.lines().next(),
                        Some("POST /v1/chat/completions HTTP/1.1")
                    );
                    break serde_json::from_slice::<Value>(&bytes[end + 4..end + 4 + size])
                        .unwrap();
                }
            };
            let response = json!({"id":"test", "object":"chat.completion", "created":1,
                "model":"qwen-3.8-27b", "choices":[{"index":0, "message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]}).to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            body["messages"].clone()
        });
        let config = OpenRouterConfig::new("local-test-key", "qwen-3.8-27b")
            .with_base_url(format!(
                "http://{host}:{}/v1/chat/completions",
                address.port()
            ))
            .with_streaming(false);
        let client = reqwest::Client::builder()
            .no_proxy()
            .resolve(host, address)
            .build()
            .unwrap();
        let adapter = CompletionsAdapter::with_client(
            CerebrasCompatibleOpenRouter::from(config),
            agentkit_http::Http::new(client),
        );
        let mut session = adapter
            .start_session(SessionConfig::new("cerebras-test"))
            .await
            .unwrap();
        session
            .begin_turn(
                TurnRequest {
                    session_id: SessionId::new("cerebras-test"),
                    turn_id: TurnId::new("turn"),
                    transcript: vec![
                        Item::text(ItemKind::System, "system"),
                        Item::text(ItemKind::Context, "AGENTS instructions"),
                        Item::text(ItemKind::User, "hello"),
                    ],
                    available_tools: Vec::new(),
                    cache: None,
                    metadata: MetadataMap::new(),
                },
                None,
            )
            .await
            .unwrap();
        server.join().unwrap()
    }

    #[tokio::test]
    async fn cerebras_coalesces_leading_system_messages_on_wire() {
        let messages = wire_messages("api.cerebras.ai").await;
        assert_eq!(
            messages,
            json!([
                {"role":"system", "content":"system\n\nAGENTS instructions"},
                {"role":"user", "content":"hello"}
            ])
        );
    }

    #[test]
    fn normalization_preserves_later_instructions_and_tool_order() {
        let mut tail = json!([
            {"role":"user", "content":"question"},
            {"role":"assistant", "tool_calls":[{"id":"call-1", "type":"function", "function":{"name":"tool", "arguments":"{}"}}]},
            {"role":"tool", "tool_call_id":"call-1", "content":"result"},
            {"role":"system", "content":"later instruction"},
            {"role":"system", "content":"another later instruction"}
        ]);
        let mut messages = vec![
            json!({"role":"system", "content":"first"}),
            json!({"role":"system", "content":"second"}),
            json!({"role":"system", "content":"third"}),
        ];
        messages.extend(tail.as_array().unwrap().iter().cloned());
        let mut body = json!({"messages":messages}).as_object().unwrap().clone();
        normalize_system_messages(&mut body);
        assert_eq!(body["messages"][0]["content"], "first\n\nsecond\n\nthird");
        tail[3]["role"] = json!("user");
        tail[4]["role"] = json!("user");
        assert_eq!(
            &body["messages"].as_array().unwrap()[1..],
            tail.as_array().unwrap()
        );
    }

    #[test]
    fn normalization_preserves_structured_content_and_normalizes_later_roles() {
        for mut messages in [
            json!([]),
            json!([{"role":"system", "content":"only"}]),
            json!([{"role":"user", "content":"first"}, {"role":"system", "content":"later"}]),
            json!([{"role":"system", "content":"first"}, {"role":"developer", "content":"second"}, {"role":"system", "content":"later"}]),
            json!([{"role":"system", "content":[{"type":"text", "text":"first", "cache_control":{"type":"ephemeral"}}]}, {"role":"system", "content":"second"}]),
            json!([{"role":"system", "content":"first", "name":"named"}, {"role":"system", "content":"second"}]),
        ] {
            let mut body = json!({"messages": messages}).as_object().unwrap().clone();
            normalize_system_messages(&mut body);
            for message in messages.as_array_mut().unwrap().iter_mut().skip(1) {
                if message["role"] == "system" {
                    message["role"] = json!("user");
                }
            }
            assert_eq!(body["messages"], messages);
        }
    }

    #[tokio::test]
    async fn other_openrouter_endpoints_keep_separate_system_messages() {
        for host in ["openrouter.ai", "api.cerebras.ai.example.com", "localhost"] {
            let messages = wire_messages(host).await;
            assert_eq!(messages.as_array().unwrap().len(), 3);
            assert_eq!(messages[0]["content"], "system");
            assert_eq!(messages[1]["content"], "AGENTS instructions");
        }
    }
}
