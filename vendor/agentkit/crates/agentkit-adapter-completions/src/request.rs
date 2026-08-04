use agentkit_core::{Item, ItemKind, Part, PartKind};
use agentkit_loop::TurnRequest;
use agentkit_tools_core::ToolSpec;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::CompletionsProvider;
use crate::error::CompletionsError;
use crate::media::{file_to_content, media_to_content};

pub(crate) fn build_request_body<P: CompletionsProvider>(
    provider: &P,
    request: &TurnRequest,
) -> Result<Value, CompletionsError> {
    let mut body = Map::new();

    // Provider-supplied config (model, temperature, parallel_tool_calls, …)
    // is the base; everything else is layered on top.
    let config_value =
        serde_json::to_value(provider.config()).map_err(CompletionsError::Serialize)?;
    if let Value::Object(fields) = config_value {
        for (key, value) in fields {
            body.insert(key, value);
        }
    }
    provider
        .apply_generation_controls(&mut body, &request.generation)
        .map_err(|error| CompletionsError::Protocol(error.to_string()))?;

    let mut messages = build_messages(&request.transcript)?;
    if provider.requires_alternating_roles() {
        merge_consecutive_user_messages(&mut messages);
    }
    body.insert(
        "messages".into(),
        serde_json::to_value(&messages).map_err(CompletionsError::Serialize)?,
    );

    let streaming = provider.streaming();
    body.insert("stream".into(), Value::Bool(streaming));
    if streaming {
        provider
            .apply_stream_options(&mut body)
            .map_err(|error| CompletionsError::Protocol(error.to_string()))?;
    }

    let tools = build_tools(&request.available_tools)?;
    if !tools.is_empty() {
        body.insert(
            "tools".into(),
            serde_json::to_value(&tools).map_err(CompletionsError::Serialize)?,
        );
    }

    body.insert("user".into(), Value::String(request.session_id.0.clone()));

    provider
        .apply_prompt_cache(&mut body, request)
        .map_err(|error| CompletionsError::Protocol(error.to_string()))?;

    Ok(Value::Object(body))
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ToolDefinition {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ToolFunction,
}

#[derive(Serialize)]
struct ToolFunction {
    name: String,
    description: String,
    parameters: Value,
}

fn build_tools(tool_specs: &[ToolSpec]) -> Result<Vec<ToolDefinition>, CompletionsError> {
    tool_specs
        .iter()
        .map(|spec| {
            validate_tool_name(&spec.name.0)?;
            Ok(ToolDefinition {
                kind: "function",
                function: ToolFunction {
                    name: spec.name.0.clone(),
                    description: spec.description.clone(),
                    parameters: spec.input_schema.clone(),
                },
            })
        })
        .collect()
}

/// Tool names must match `^[a-zA-Z0-9_-]{1,64}$` for OpenAI-compatible
/// chat completions providers — both OpenAI and Anthropic enforce this
/// regex server-side, and OpenAI returns a 400 when violated.
fn validate_tool_name(name: &str) -> Result<(), CompletionsError> {
    if name.is_empty() || name.len() > 64 {
        return Err(CompletionsError::InvalidToolName(name.into()));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(CompletionsError::InvalidToolName(name.into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(tag = "role", rename_all = "lowercase")]
enum ChatMessage {
    System {
        content: String,
    },
    Developer {
        content: String,
    },
    User {
        content: UserContent,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<AssistantContent>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Serialize)]
#[serde(untagged)]
enum UserContent {
    Text(String),
    Parts(Vec<Value>),
}

#[derive(Serialize)]
#[serde(untagged)]
enum AssistantContent {
    Text(String),
    Parts(Vec<TextPart>),
}

#[derive(Serialize)]
struct TextPart {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

#[derive(Serialize)]
struct ToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: ToolCallFunction,
}

#[derive(Serialize)]
struct ToolCallFunction {
    name: String,
    arguments: String,
}

fn build_messages(transcript: &[Item]) -> Result<Vec<ChatMessage>, CompletionsError> {
    let mut messages = Vec::new();

    for item in transcript {
        match item.kind {
            ItemKind::Tool => {
                for part in &item.parts {
                    let Part::ToolResult(result) = part else {
                        return Err(CompletionsError::UnsupportedPart {
                            role: item.kind,
                            part_kind: part_kind(part),
                        });
                    };
                    messages.push(ChatMessage::Tool {
                        tool_call_id: result.call_id.0.clone(),
                        content: tool_output_to_string(&result.output),
                    });
                }
            }
            ItemKind::System | ItemKind::Context => {
                messages.push(ChatMessage::System {
                    content: stringify_parts(&item.parts, item.kind)?,
                });
            }
            ItemKind::Developer => {
                messages.push(ChatMessage::Developer {
                    content: stringify_parts(&item.parts, item.kind)?,
                });
            }
            ItemKind::User => {
                messages.push(ChatMessage::User {
                    content: build_user_content(&item.parts)?,
                });
            }
            ItemKind::Notification => {
                messages.push(ChatMessage::User {
                    content: UserContent::Text(wrap_notification(&stringify_parts(
                        &item.parts,
                        item.kind,
                    )?)),
                });
            }
            ItemKind::Assistant => {
                if let Some(message) = build_assistant_message(item)? {
                    messages.push(message);
                }
            }
        }
    }

    Ok(messages)
}

/// Wrap a notification's plain text in `<system-reminder>` so the model
/// reads it as a side-channel signal rather than a user turn. Same
/// convention as Anthropic's reference harness.
fn wrap_notification(text: &str) -> String {
    format!("<system-reminder>\n{text}\n</system-reminder>")
}

/// Build an assistant message, returning `None` when the item collapses to
/// no content and no tool calls. Emitting such an item produces
/// `{"content": null, "tool_calls": []}` which OpenAI rejects with
/// "Either content or tool_calls must be set".
fn build_assistant_message(item: &Item) -> Result<Option<ChatMessage>, CompletionsError> {
    let mut tool_calls = Vec::new();
    let mut content_parts: Vec<TextPart> = Vec::new();

    for part in &item.parts {
        match part {
            Part::Text(text) => {
                if !text.text.is_empty() {
                    content_parts.push(TextPart {
                        kind: "text",
                        text: text.text.clone(),
                    });
                }
            }
            Part::Structured(structured) => {
                content_parts.push(TextPart {
                    kind: "text",
                    text: serde_json::to_string(&structured.value)
                        .map_err(CompletionsError::Serialize)?,
                });
            }
            Part::Reasoning(reasoning) => {
                if let Some(summary) = &reasoning.summary {
                    content_parts.push(TextPart {
                        kind: "text",
                        text: summary.clone(),
                    });
                }
            }
            Part::ToolCall(call) => {
                tool_calls.push(ToolCall {
                    id: call.id.0.clone(),
                    kind: "function",
                    function: ToolCallFunction {
                        name: call.name.clone(),
                        arguments: serde_json::to_string(&call.input)
                            .map_err(CompletionsError::Serialize)?,
                    },
                });
            }
            Part::ToolResult(_) | Part::Media(_) | Part::File(_) | Part::Custom(_) => {
                return Err(CompletionsError::UnsupportedPart {
                    role: item.kind,
                    part_kind: part_kind(part),
                });
            }
        }
    }

    let content = match content_parts.len() {
        0 => None,
        1 => Some(AssistantContent::Text(
            content_parts.pop().expect("len == 1").text,
        )),
        _ => Some(AssistantContent::Parts(content_parts)),
    };

    if content.is_none() && tool_calls.is_empty() {
        return Ok(None);
    }

    Ok(Some(ChatMessage::Assistant {
        content,
        tool_calls,
    }))
}

fn build_user_content(parts: &[Part]) -> Result<UserContent, CompletionsError> {
    let mut content = Vec::new();

    for part in parts {
        match part {
            Part::Text(text) => content.push(serde_json::json!({
                "type": "text",
                "text": text.text,
            })),
            Part::Structured(structured) => content.push(serde_json::json!({
                "type": "text",
                "text": serde_json::to_string_pretty(&structured.value)
                    .map_err(CompletionsError::Serialize)?,
            })),
            Part::Reasoning(reasoning) => {
                if let Some(summary) = &reasoning.summary {
                    content.push(serde_json::json!({
                        "type": "text",
                        "text": summary,
                    }));
                }
            }
            Part::Media(media) => content.push(media_to_content(media)?),
            Part::File(file) => content.push(file_to_content(file)?),
            Part::ToolCall(_) | Part::ToolResult(_) | Part::Custom(_) => {
                return Err(CompletionsError::UnsupportedPart {
                    role: ItemKind::User,
                    part_kind: part_kind(part),
                });
            }
        }
    }

    if content.len() == 1 && content[0]["type"] == "text" {
        Ok(UserContent::Text(
            content[0]["text"].as_str().unwrap_or_default().to_string(),
        ))
    } else {
        Ok(UserContent::Parts(content))
    }
}

fn stringify_parts(parts: &[Part], role: ItemKind) -> Result<String, CompletionsError> {
    let mut segments = Vec::new();

    for part in parts {
        match part {
            Part::Text(text) => segments.push(text.text.clone()),
            Part::Structured(structured) => segments.push(
                serde_json::to_string_pretty(&structured.value)
                    .map_err(CompletionsError::Serialize)?,
            ),
            Part::Reasoning(reasoning) => {
                if let Some(summary) = &reasoning.summary {
                    segments.push(summary.clone());
                }
            }
            _ => {
                return Err(CompletionsError::UnsupportedPart {
                    role,
                    part_kind: part_kind(part),
                });
            }
        }
    }

    Ok(segments.join("\n\n"))
}

fn tool_output_to_string(output: &agentkit_core::ToolOutput) -> String {
    match output {
        agentkit_core::ToolOutput::Text(text) => text.clone(),
        agentkit_core::ToolOutput::Structured(value) => value.to_string(),
        agentkit_core::ToolOutput::Parts(parts) => {
            serde_json::to_string(parts).unwrap_or_else(|_| "[]".into())
        }
        agentkit_core::ToolOutput::Files(files) => {
            serde_json::to_string(files).unwrap_or_else(|_| "[]".into())
        }
    }
}

fn part_kind(part: &Part) -> PartKind {
    match part {
        Part::Text(_) => PartKind::Text,
        Part::Media(_) => PartKind::Media,
        Part::File(_) => PartKind::File,
        Part::Structured(_) => PartKind::Structured,
        Part::Reasoning(_) => PartKind::Reasoning,
        Part::ToolCall(_) => PartKind::ToolCall,
        Part::ToolResult(_) => PartKind::ToolResult,
        Part::Custom(_) => PartKind::Custom,
    }
}

// ---------------------------------------------------------------------------
// Strict alternation: merge adjacent user-role messages
// ---------------------------------------------------------------------------

/// Merge consecutive `user`-role messages into a single message. Required
/// for providers that enforce strict `user`/`assistant` alternation in
/// their chat templates — notably vLLM-served Mistral
/// (https://github.com/vllm-project/vllm/issues/6862) and the Mistral
/// hosted API. Tool messages are left untouched: they participate in the
/// `assistant → tool* → assistant` pattern that those templates do allow.
fn merge_consecutive_user_messages(messages: &mut Vec<ChatMessage>) {
    let mut merged: Vec<ChatMessage> = Vec::with_capacity(messages.len());

    for message in messages.drain(..) {
        match (merged.last_mut(), message) {
            (Some(ChatMessage::User { content: prev }), ChatMessage::User { content: next }) => {
                merge_user_content(prev, next);
            }
            (_, message) => merged.push(message),
        }
    }

    *messages = merged;
}

fn merge_user_content(prev: &mut UserContent, next: UserContent) {
    let prev_parts =
        take_user_content_as_parts(std::mem::replace(prev, UserContent::Parts(Vec::new())));
    let next_parts = take_user_content_as_parts(next);
    let mut combined = prev_parts;
    combined.extend(next_parts);
    *prev = UserContent::Parts(combined);
}

fn take_user_content_as_parts(content: UserContent) -> Vec<Value> {
    match content {
        UserContent::Text(text) => vec![serde_json::json!({
            "type": "text",
            "text": text,
        })],
        UserContent::Parts(parts) => parts,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use agentkit_core::{
        Item, ItemKind, MetadataMap, Part, ReasoningPart, SessionId, TextPart as CoreTextPart,
        ToolCallPart, ToolOutput, ToolResultPart, TurnId,
    };
    use agentkit_loop::{LoopError, PromptCacheRequest, PromptCacheStrategy, TurnRequest};
    use agentkit_tools_core::{ToolName, ToolSpec};
    use serde::Serialize;
    use serde_json::json;

    use super::*;

    #[derive(Clone, Serialize)]
    struct TestConfig {
        model: String,
    }

    #[derive(Clone)]
    struct TestProvider {
        strict_alternation: bool,
        streaming: bool,
    }

    impl TestProvider {
        fn lenient() -> Self {
            Self {
                strict_alternation: false,
                streaming: false,
            }
        }
        fn strict() -> Self {
            Self {
                strict_alternation: true,
                streaming: false,
            }
        }
        fn streaming() -> Self {
            Self {
                strict_alternation: false,
                streaming: true,
            }
        }
    }

    impl CompletionsProvider for TestProvider {
        type Config = TestConfig;

        fn provider_name(&self) -> &str {
            "test"
        }

        fn endpoint_url(&self) -> &str {
            "https://example.test/v1/chat/completions"
        }

        fn config(&self) -> &Self::Config {
            static CONFIG: OnceLock<TestConfig> = OnceLock::new();
            CONFIG.get_or_init(|| TestConfig {
                model: "test-model".into(),
            })
        }

        fn requires_alternating_roles(&self) -> bool {
            self.strict_alternation
        }

        fn streaming(&self) -> bool {
            self.streaming
        }

        fn apply_prompt_cache(
            &self,
            body: &mut serde_json::Map<String, Value>,
            request: &TurnRequest,
        ) -> Result<(), LoopError> {
            if request.cache.is_some() {
                body.insert("cache_hook".into(), Value::Bool(true));
            }
            Ok(())
        }

        fn apply_stream_options(
            &self,
            body: &mut serde_json::Map<String, Value>,
        ) -> Result<(), LoopError> {
            body.insert("stream_options_hook".into(), Value::Bool(true));
            Ok(())
        }
    }

    fn turn_request(transcript: Vec<Item>, available_tools: Vec<ToolSpec>) -> TurnRequest {
        TurnRequest {
            session_id: SessionId::new("session"),
            turn_id: TurnId::new("turn-1"),
            transcript,
            available_tools,
            cache: None,
            structured_output: None,
            generation: Default::default(),
            metadata: MetadataMap::new(),
        }
    }

    #[test]
    fn applies_provider_cache_hook() {
        let body = build_request_body(
            &TestProvider::lenient(),
            &TurnRequest {
                cache: Some(PromptCacheRequest::best_effort(
                    PromptCacheStrategy::Automatic,
                )),
                ..turn_request(vec![Item::text(ItemKind::User, "hi")], Vec::new())
            },
        )
        .unwrap();

        assert_eq!(body.get("cache_hook"), Some(&Value::Bool(true)));
    }

    #[test]
    fn streaming_provider_sets_stream_true_and_applies_options() {
        let body = build_request_body(
            &TestProvider::streaming(),
            &turn_request(vec![Item::text(ItemKind::User, "hi")], Vec::new()),
        )
        .unwrap();

        assert_eq!(body.get("stream"), Some(&Value::Bool(true)));
        assert_eq!(body.get("stream_options_hook"), Some(&Value::Bool(true)));
    }

    #[test]
    fn buffered_provider_sets_stream_false() {
        let body = build_request_body(
            &TestProvider::lenient(),
            &turn_request(vec![Item::text(ItemKind::User, "hi")], Vec::new()),
        )
        .unwrap();

        assert_eq!(body.get("stream"), Some(&Value::Bool(false)));
        assert!(body.get("stream_options_hook").is_none());
    }

    #[test]
    fn notification_renders_as_user_role_with_system_reminder() {
        let body = build_request_body(
            &TestProvider::lenient(),
            &turn_request(
                vec![Item::notification("background task done: ok")],
                Vec::new(),
            ),
        )
        .unwrap();

        let messages = body.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        let text = messages[0]["content"].as_str().unwrap();
        assert!(text.starts_with("<system-reminder>"));
        assert!(text.ends_with("</system-reminder>"));
        assert!(text.contains("background task done: ok"));
    }

    #[test]
    fn assistant_text_only_omits_tool_calls_field() {
        // OpenAI rejects assistant messages that include `tool_calls: []`
        // (code: empty_array). Verify we omit the field entirely when
        // there are no tool calls.
        let body = build_request_body(
            &TestProvider::lenient(),
            &turn_request(
                vec![
                    Item::text(ItemKind::User, "hi"),
                    Item::text(ItemKind::Assistant, "hello"),
                ],
                Vec::new(),
            ),
        )
        .unwrap();

        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"], "hello");
        assert!(
            assistant.get("tool_calls").is_none(),
            "tool_calls must be omitted when empty, got {assistant}",
        );
    }

    #[test]
    fn assistant_tool_call_omits_content_field() {
        // Conversely, when there's only a tool call and no text, omit
        // `content` rather than emitting `content: null`. Keeps the wire
        // payload minimal and avoids tripping strict OpenAI-compatible
        // shims that reject explicit-null content.
        let body = build_request_body(
            &TestProvider::lenient(),
            &turn_request(
                vec![
                    Item::text(ItemKind::User, "go"),
                    Item::new(
                        ItemKind::Assistant,
                        vec![Part::ToolCall(ToolCallPart::new(
                            "call-1",
                            "search",
                            json!({ "q": "x" }),
                        ))],
                    ),
                ],
                Vec::new(),
            ),
        )
        .unwrap();

        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert!(
            assistant.get("content").is_none(),
            "content must be omitted when assistant has only tool calls, got {assistant}",
        );
        assert_eq!(assistant["tool_calls"][0]["id"], "call-1");
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "search");
    }

    #[test]
    fn empty_assistant_item_is_skipped() {
        // An assistant Item whose parts all filter to empty (e.g. blank
        // text + reasoning without summary) would produce `{content:null,
        // tool_calls:[]}` — invalid for OpenAI. Skip the message instead.
        let blank_text = CoreTextPart {
            text: String::new(),
            metadata: MetadataMap::new(),
        };
        let summaryless = ReasoningPart {
            summary: None,
            data: None,
            redacted: false,
            metadata: MetadataMap::new(),
        };

        let body = build_request_body(
            &TestProvider::lenient(),
            &turn_request(
                vec![
                    Item::text(ItemKind::User, "hi"),
                    Item::new(
                        ItemKind::Assistant,
                        vec![Part::Text(blank_text), Part::Reasoning(summaryless)],
                    ),
                    Item::text(ItemKind::User, "still there?"),
                ],
                Vec::new(),
            ),
        )
        .unwrap();

        let messages = body["messages"].as_array().unwrap();
        // empty assistant collapsed away — only the two user messages remain
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn rejects_invalid_tool_name() {
        let err = build_request_body(
            &TestProvider::lenient(),
            &turn_request(
                vec![Item::text(ItemKind::User, "hi")],
                vec![ToolSpec::new(
                    ToolName("bad.name".into()),
                    "",
                    json!({ "type": "object" }),
                )],
            ),
        )
        .unwrap_err();

        assert!(matches!(err, CompletionsError::InvalidToolName(ref name) if name == "bad.name"));
    }

    #[test]
    fn omits_tools_field_when_no_tools() {
        let body = build_request_body(
            &TestProvider::lenient(),
            &turn_request(vec![Item::text(ItemKind::User, "hi")], Vec::new()),
        )
        .unwrap();

        assert!(body.get("tools").is_none());
    }

    #[test]
    fn strict_alternation_merges_adjacent_user_messages() {
        // With strict alternation, the user → notification sequence must
        // collapse into a single user message. Verifies the merge path
        // used for vLLM-served Mistral and Perplexity routes.
        let body = build_request_body(
            &TestProvider::strict(),
            &turn_request(
                vec![
                    Item::text(ItemKind::User, "first"),
                    Item::notification("background event"),
                    Item::text(ItemKind::Assistant, "ack"),
                    Item::text(ItemKind::User, "follow-up A"),
                    Item::text(ItemKind::User, "follow-up B"),
                ],
                Vec::new(),
            ),
        )
        .unwrap();

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert!(messages[0]["content"].is_array());
        let parts = messages[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "first");
        assert!(
            parts[1]["text"]
                .as_str()
                .unwrap()
                .contains("<system-reminder>")
        );

        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[2]["role"], "user");
        let parts = messages[2]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "follow-up A");
        assert_eq!(parts[1]["text"], "follow-up B");
    }

    #[test]
    fn lenient_provider_keeps_consecutive_user_messages_separate() {
        let body = build_request_body(
            &TestProvider::lenient(),
            &turn_request(
                vec![
                    Item::text(ItemKind::User, "one"),
                    Item::text(ItemKind::User, "two"),
                ],
                Vec::new(),
            ),
        )
        .unwrap();

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn tool_message_round_trips() {
        let body = build_request_body(
            &TestProvider::lenient(),
            &turn_request(
                vec![
                    Item::text(ItemKind::User, "go"),
                    Item::new(
                        ItemKind::Assistant,
                        vec![Part::ToolCall(ToolCallPart::new(
                            "call-1",
                            "search",
                            json!({ "q": "x" }),
                        ))],
                    ),
                    Item::new(
                        ItemKind::Tool,
                        vec![Part::ToolResult(ToolResultPart::success(
                            "call-1",
                            ToolOutput::text("hit"),
                        ))],
                    ),
                ],
                Vec::new(),
            ),
        )
        .unwrap();

        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call-1");
        assert_eq!(messages[2]["content"], "hit");
    }
}
