//! Narrow wire-format compatibility for Cerebras through the OpenRouter transport.
use agentkit_adapter_completions::CompletionsProvider;
use agentkit_core::{MetadataMap, Usage};
use agentkit_http::{Authentication, HttpRequestBuilder, ResilienceConfig, StatusCode};
use agentkit_loop::{LoopError, TurnRequest};
use agentkit_provider_openrouter::{OpenRouterConfig, OpenRouterProvider, OpenRouterRequestConfig};
use serde_json::{Map, Value};

#[derive(Clone)]
pub(super) struct CerebrasCompatibleOpenRouter(OpenRouterProvider);

impl From<OpenRouterConfig> for CerebrasCompatibleOpenRouter {
    fn from(config: OpenRouterConfig) -> Self {
        Self(config.into())
    }
}

impl CompletionsProvider for CerebrasCompatibleOpenRouter {
    type Config = OpenRouterRequestConfig;

    fn provider_name(&self) -> &str {
        self.0.provider_name()
    }
    fn endpoint_url(&self) -> &str {
        self.0.endpoint_url()
    }
    fn config(&self) -> &Self::Config {
        self.0.config()
    }
    fn preprocess_request(&self, builder: HttpRequestBuilder) -> HttpRequestBuilder {
        self.0.preprocess_request(builder)
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
        self.0.apply_stream_options(body)
    }
    fn apply_prompt_cache(
        &self,
        body: &mut Map<String, Value>,
        request: &TurnRequest,
    ) -> Result<(), LoopError> {
        self.0.apply_prompt_cache(body, request)?;
        if url::Url::parse(self.endpoint_url())
            .ok()
            .is_some_and(|url| url.host_str() == Some("api.cerebras.ai"))
        {
            coalesce_leading_system_messages(body);
        }
        Ok(())
    }
    fn preprocess_response(&self, status: StatusCode, body: &str) -> Result<(), LoopError> {
        self.0.preprocess_response(status, body)
    }
    fn postprocess_response(
        &self,
        usage: &mut Option<Usage>,
        metadata: &mut MetadataMap,
        raw: &Value,
    ) {
        self.0.postprocess_response(usage, metadata, raw)
    }
}

// Agentkit maps both System and Context items to system messages. Cerebras
// permits a system message only at index zero, so combine the initial block
// without hoisting later instructions or mutating the persisted transcript.
fn coalesce_leading_system_messages(body: &mut Map<String, Value>) {
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    let count = messages
        .iter()
        .take_while(|message| message["role"] == "system")
        .count();
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
    let Some(contents) = contents else { return };
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
        let tail = json!([
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
        coalesce_leading_system_messages(&mut body);
        assert_eq!(body["messages"][0]["content"], "first\n\nsecond\n\nthird");
        assert_eq!(
            &body["messages"].as_array().unwrap()[1..],
            tail.as_array().unwrap()
        );
    }

    #[test]
    fn normalization_leaves_nonleading_and_structured_messages_unchanged() {
        for messages in [
            json!([]),
            json!([{"role":"system", "content":"only"}]),
            json!([{"role":"user", "content":"first"}, {"role":"system", "content":"later"}]),
            json!([{"role":"system", "content":"first"}, {"role":"developer", "content":"second"}, {"role":"system", "content":"later"}]),
            json!([{"role":"system", "content":[{"type":"text", "text":"first", "cache_control":{"type":"ephemeral"}}]}, {"role":"system", "content":"second"}]),
            json!([{"role":"system", "content":"first", "name":"named"}, {"role":"system", "content":"second"}]),
        ] {
            let mut body = json!({"messages": messages}).as_object().unwrap().clone();
            let original = body.clone();
            coalesce_leading_system_messages(&mut body);
            assert_eq!(body, original);
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
