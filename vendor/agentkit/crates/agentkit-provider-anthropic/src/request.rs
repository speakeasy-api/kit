use agentkit_core::{Item, ItemKind, Modality, Part, PartKind};
use agentkit_loop::{
    LoopError, PromptCacheBreakpoint, PromptCacheMode, PromptCacheRequest, PromptCacheRetention,
    PromptCacheStrategy, TurnRequest,
};
use agentkit_tools_core::ToolSpec;
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::config::{AnthropicConfig, ThinkingConfig};
use crate::media::{file_to_content, media_to_content};

/// Internal error produced while building a request body.
#[derive(Debug, Error)]
pub(crate) enum BuildError {
    #[error("unsupported content part {part_kind:?} on role {role:?}")]
    UnsupportedPart { role: ItemKind, part_kind: PartKind },
    #[error("unsupported modality: {0:?}")]
    UnsupportedModality(Modality),
    #[error("unsupported data reference: {0}")]
    UnsupportedDataRef(String),
    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("tool name {0:?} does not match ^[a-zA-Z0-9_-]{{1,64}}$")]
    InvalidToolName(String),
    #[error(
        "max_tokens ({max_tokens}) must be strictly greater than thinking.budget_tokens \
         ({budget_tokens}); raise max_tokens or lower the thinking budget"
    )]
    InvalidThinkingBudget { max_tokens: u32, budget_tokens: u32 },
    #[error("{0}")]
    CacheViolation(String),
    #[error("turn structured output conflicts with configured output_format")]
    StructuredOutputConflict,
    #[error("Anthropic cannot honor the requested generation controls")]
    UnsupportedGenerationControls,
}

impl From<BuildError> for LoopError {
    fn from(error: BuildError) -> Self {
        LoopError::Provider(error.to_string())
    }
}

/// Max `cache_control` breakpoints Anthropic accepts per request.
const MAX_CACHE_BREAKPOINTS: usize = 4;

/// Builds the Messages API request body for a turn.
pub(crate) fn build_request_body(
    config: &AnthropicConfig,
    request: &TurnRequest,
) -> Result<Value, BuildError> {
    validate_thinking_budget(config)?;
    if request.generation.max_output_tokens == Some(0)
        || request
            .generation
            .max_output_tokens
            .is_some_and(|maximum| maximum > config.max_tokens)
        || request
            .generation
            .temperature
            .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
        || (config.thinking.is_some() && request.generation.temperature.is_some())
    {
        return Err(BuildError::UnsupportedGenerationControls);
    }

    // Split the transcript up front into system blocks + interleaved messages.
    let mut system_blocks: Vec<Value> = Vec::new();
    let mut messages: Vec<Message> = Vec::new();

    for item in &request.transcript {
        match item.kind {
            ItemKind::System | ItemKind::Developer | ItemKind::Context => {
                extend_system_blocks(&mut system_blocks, item)?;
            }
            ItemKind::User => {
                let content = build_user_content(&item.parts)?;
                append_user_message(&mut messages, content);
            }
            ItemKind::Assistant => {
                let content = build_assistant_content(&item.parts)?;
                // Anthropic rejects assistant messages with an empty
                // content array (`messages.X.content: minimum 1 item`).
                // Skip when every part filtered out (blank text, summary-
                // less reasoning, etc.) rather than emitting `[]`.
                if !content.is_empty() {
                    messages.push(Message::Assistant { content });
                }
            }
            ItemKind::Tool => {
                let blocks = build_tool_result_blocks(&item.parts)?;
                append_user_message(&mut messages, blocks);
            }
            ItemKind::Notification => {
                // Out-of-band signal: emit as a user-role message with
                // the content wrapped in <system-reminder> so the model
                // reads it as a notification, not a user turn. Crucially
                // this stays in the messages stream — DO NOT hoist into
                // system_blocks — so its temporal position is preserved.
                let block = build_notification_block(&item.parts)?;
                append_user_message(&mut messages, vec![block]);
            }
        }
    }

    // Synthesize tools
    let mut tools: Vec<Value> = request
        .available_tools
        .iter()
        .map(build_tool_spec)
        .collect::<Result<_, _>>()?;
    for server_tool in &config.server_tools {
        tools.push(server_tool.to_tool_json());
    }

    // Apply prompt caching (may mutate tools/system/messages)
    apply_prompt_cache(&mut system_blocks, &mut messages, &mut tools, request)?;

    // Assemble body
    let mut body = Map::new();
    body.insert("model".into(), Value::String(config.model.clone()));
    body.insert(
        "max_tokens".into(),
        Value::from(
            request
                .generation
                .max_output_tokens
                .unwrap_or(config.max_tokens),
        ),
    );

    if !system_blocks.is_empty() {
        body.insert("system".into(), Value::Array(system_blocks));
    }
    body.insert(
        "messages".into(),
        Value::Array(messages.into_iter().map(Message::into_value).collect()),
    );
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }

    if let Some(choice) = &config.tool_choice {
        body.insert(
            "tool_choice".into(),
            choice.to_json(config.disable_parallel_tool_use),
        );
    }

    if let Some(temp) = request.generation.temperature.or(config.temperature) {
        body.insert("temperature".into(), json_number(temp as f64));
    }
    if let Some(top_p) = config.top_p {
        body.insert("top_p".into(), json_number(top_p as f64));
    }
    if let Some(top_k) = config.top_k {
        body.insert("top_k".into(), Value::from(top_k));
    }
    if let Some(stops) = request
        .generation
        .stop_sequences
        .as_ref()
        .or(config.stop_sequences.as_ref())
    {
        body.insert(
            "stop_sequences".into(),
            Value::Array(stops.iter().cloned().map(Value::String).collect()),
        );
    }
    if let Some(thinking) = &config.thinking {
        body.insert("thinking".into(), thinking.to_json());
    }
    if let Some(tier) = config.service_tier {
        body.insert("service_tier".into(), Value::String(tier.as_str().into()));
    }
    if let Some(user_id) = &config.metadata_user_id {
        body.insert("metadata".into(), json!({ "user_id": user_id }));
    }
    if let Some(container) = &config.container {
        body.insert("container".into(), Value::String(container.clone()));
    }
    if request.structured_output.is_some() && config.output_format.is_some() {
        return Err(BuildError::StructuredOutputConflict);
    }
    if let Some(format) = request.structured_output.as_ref() {
        let mut output = Map::new();
        output.insert(
            "format".into(),
            json!({
                "type": "json_schema",
                "schema": format.schema()
            }),
        );
        if let Some(effort) = config.output_effort {
            output.insert("effort".into(), Value::String(effort.as_str().into()));
        }
        body.insert("output_config".into(), Value::Object(output));
    } else if let Some(format) = &config.output_format {
        let mut oc = Map::new();
        oc.insert("format".into(), format.to_json());
        if let Some(effort) = config.output_effort {
            oc.insert("effort".into(), Value::String(effort.as_str().into()));
        }
        body.insert("output_config".into(), Value::Object(oc));
    } else if let Some(effort) = config.output_effort {
        body.insert("output_config".into(), json!({ "effort": effort.as_str() }));
    }
    if !config.mcp_servers.is_empty() {
        body.insert(
            "mcp_servers".into(),
            Value::Array(config.mcp_servers.iter().map(|s| s.0.clone()).collect()),
        );
    }

    body.insert("stream".into(), Value::Bool(config.streaming));

    Ok(Value::Object(body))
}

fn json_number(n: f64) -> Value {
    serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
}

/// Enforce the Anthropic constraint that `max_tokens` must be strictly greater
/// than `thinking.budget_tokens` when extended thinking is enabled.
fn validate_thinking_budget(config: &AnthropicConfig) -> Result<(), BuildError> {
    let Some(ThinkingConfig::Enabled { budget_tokens }) = &config.thinking else {
        return Ok(());
    };
    if config.max_tokens <= *budget_tokens {
        return Err(BuildError::InvalidThinkingBudget {
            max_tokens: config.max_tokens,
            budget_tokens: *budget_tokens,
        });
    }
    Ok(())
}

// --- System extraction ---

fn extend_system_blocks(blocks: &mut Vec<Value>, item: &Item) -> Result<(), BuildError> {
    for part in &item.parts {
        match part {
            Part::Text(text) => blocks.push(json!({ "type": "text", "text": text.text })),
            Part::Structured(structured) => blocks.push(json!({
                "type": "text",
                "text": serde_json::to_string_pretty(&structured.value)?,
            })),
            Part::Reasoning(reasoning) => {
                if let Some(summary) = &reasoning.summary {
                    blocks.push(json!({ "type": "text", "text": summary }));
                }
            }
            _ => {
                return Err(BuildError::UnsupportedPart {
                    role: item.kind,
                    part_kind: part_kind(part),
                });
            }
        }
    }
    Ok(())
}

// --- Notification (out-of-band side-channel) ---

fn build_notification_block(parts: &[Part]) -> Result<Value, BuildError> {
    let mut buf = String::new();
    for part in parts {
        match part {
            Part::Text(text) => buf.push_str(&text.text),
            Part::Structured(structured) => {
                buf.push_str(&serde_json::to_string_pretty(&structured.value)?);
            }
            Part::Reasoning(reasoning) => {
                if let Some(summary) = &reasoning.summary {
                    buf.push_str(summary);
                }
            }
            _ => {
                return Err(BuildError::UnsupportedPart {
                    role: ItemKind::Notification,
                    part_kind: part_kind(part),
                });
            }
        }
    }
    Ok(json!({
        "type": "text",
        "text": format!("<system-reminder>\n{buf}\n</system-reminder>"),
    }))
}

// --- User / tool-result message content ---

fn build_user_content(parts: &[Part]) -> Result<Vec<Value>, BuildError> {
    let mut blocks = Vec::new();
    for part in parts {
        match part {
            Part::Text(text) => blocks.push(json!({ "type": "text", "text": text.text })),
            Part::Structured(structured) => blocks.push(json!({
                "type": "text",
                "text": serde_json::to_string_pretty(&structured.value)?,
            })),
            Part::Reasoning(reasoning) => {
                if let Some(summary) = &reasoning.summary {
                    blocks.push(json!({ "type": "text", "text": summary }));
                }
            }
            Part::Media(media) => blocks.push(media_to_content(media)?),
            Part::File(file) => blocks.push(file_to_content(file)?),
            Part::ToolCall(_) | Part::ToolResult(_) | Part::Custom(_) => {
                return Err(BuildError::UnsupportedPart {
                    role: ItemKind::User,
                    part_kind: part_kind(part),
                });
            }
        }
    }
    Ok(blocks)
}

fn build_tool_result_blocks(parts: &[Part]) -> Result<Vec<Value>, BuildError> {
    let mut blocks = Vec::new();
    for part in parts {
        let Part::ToolResult(result) = part else {
            return Err(BuildError::UnsupportedPart {
                role: ItemKind::Tool,
                part_kind: part_kind(part),
            });
        };
        let content = tool_output_to_content(&result.output)?;
        let mut block = Map::new();
        block.insert("type".into(), Value::String("tool_result".into()));
        block.insert(
            "tool_use_id".into(),
            Value::String(result.call_id.0.clone()),
        );
        block.insert("content".into(), content);
        if result.is_error {
            block.insert("is_error".into(), Value::Bool(true));
        }
        blocks.push(Value::Object(block));
    }
    Ok(blocks)
}

fn tool_output_to_content(output: &agentkit_core::ToolOutput) -> Result<Value, BuildError> {
    use agentkit_core::ToolOutput;
    match output {
        ToolOutput::Text(text) => Ok(Value::String(text.clone())),
        ToolOutput::Structured(value) => Ok(Value::Array(vec![json!({
            "type": "text",
            "text": serde_json::to_string(&value)?,
        })])),
        ToolOutput::Parts(parts) => {
            let mut blocks = Vec::new();
            for part in parts {
                match part {
                    Part::Text(text) => blocks.push(json!({ "type": "text", "text": text.text })),
                    Part::Structured(structured) => blocks.push(json!({
                        "type": "text",
                        "text": serde_json::to_string(&structured.value)?,
                    })),
                    Part::Media(media) => blocks.push(media_to_content(media)?),
                    Part::File(file) => blocks.push(file_to_content(file)?),
                    _ => {
                        return Err(BuildError::UnsupportedPart {
                            role: ItemKind::Tool,
                            part_kind: part_kind(part),
                        });
                    }
                }
            }
            Ok(Value::Array(blocks))
        }
        ToolOutput::Files(files) => {
            let mut blocks = Vec::new();
            for file in files {
                blocks.push(file_to_content(file)?);
            }
            Ok(Value::Array(blocks))
        }
    }
}

// --- Assistant message content ---

fn build_assistant_content(parts: &[Part]) -> Result<Vec<Value>, BuildError> {
    // Anthropic requires thinking blocks first, then text/tool_use.
    let mut thinking_blocks = Vec::new();
    let mut content_blocks = Vec::new();

    for part in parts {
        match part {
            Part::Reasoning(reasoning) => {
                if reasoning.redacted {
                    let data = reasoning
                        .metadata
                        .get("anthropic.redacted_data")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    thinking_blocks.push(json!({ "type": "redacted_thinking", "data": data }));
                } else if let Some(summary) = &reasoning.summary {
                    let signature = reasoning
                        .metadata
                        .get("anthropic.thinking_signature")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    thinking_blocks.push(json!({
                        "type": "thinking",
                        "thinking": summary,
                        "signature": signature,
                    }));
                }
            }
            Part::Text(text) => {
                if !text.text.is_empty() {
                    content_blocks.push(json!({ "type": "text", "text": text.text }));
                }
            }
            Part::Structured(structured) => {
                content_blocks.push(json!({
                    "type": "text",
                    "text": serde_json::to_string(&structured.value)?,
                }));
            }
            Part::ToolCall(call) => {
                content_blocks.push(json!({
                    "type": "tool_use",
                    "id": call.id.0,
                    "name": call.name,
                    "input": call.input,
                }));
            }
            Part::Custom(custom) => {
                // Round-trip server tool blocks stashed by the response parser.
                if let Some(kind) = custom.kind.strip_prefix("anthropic.")
                    && let Some(value) = &custom.value
                {
                    let mut cloned = value.clone();
                    if let Some(obj) = cloned.as_object_mut()
                        && !obj.contains_key("type")
                    {
                        obj.insert("type".into(), Value::String(kind.to_string()));
                    }
                    content_blocks.push(cloned);
                    continue;
                }
                return Err(BuildError::UnsupportedPart {
                    role: ItemKind::Assistant,
                    part_kind: PartKind::Custom,
                });
            }
            Part::ToolResult(_) | Part::Media(_) | Part::File(_) => {
                return Err(BuildError::UnsupportedPart {
                    role: ItemKind::Assistant,
                    part_kind: part_kind(part),
                });
            }
        }
    }

    let mut combined = thinking_blocks;
    combined.extend(content_blocks);
    Ok(combined)
}

// --- Tools ---

fn build_tool_spec(spec: &ToolSpec) -> Result<Value, BuildError> {
    validate_tool_name(&spec.name.0)?;
    let mut body = Map::new();
    body.insert("name".into(), Value::String(spec.name.0.clone()));
    body.insert(
        "description".into(),
        Value::String(spec.description.clone()),
    );
    body.insert("input_schema".into(), spec.input_schema.clone());
    Ok(Value::Object(body))
}

fn validate_tool_name(name: &str) -> Result<(), BuildError> {
    if name.is_empty() || name.len() > 64 {
        return Err(BuildError::InvalidToolName(name.into()));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(BuildError::InvalidToolName(name.into()));
    }
    Ok(())
}

// --- Role alternation helpers ---

enum Message {
    User { content: Vec<Value> },
    Assistant { content: Vec<Value> },
}

impl Message {
    fn into_value(self) -> Value {
        match self {
            Self::User { content } => json!({ "role": "user", "content": content }),
            Self::Assistant { content } => json!({ "role": "assistant", "content": content }),
        }
    }
}

/// Append user content to the last message if it's already a user message,
/// otherwise start a new user message. Keeps tool_results and fresh user text
/// in the same message so role alternation holds.
fn append_user_message(messages: &mut Vec<Message>, mut content: Vec<Value>) {
    if let Some(Message::User { content: prev }) = messages.last_mut() {
        prev.append(&mut content);
    } else {
        messages.push(Message::User { content });
    }
}

// --- Prompt cache mapping ---

fn apply_prompt_cache(
    system_blocks: &mut [Value],
    messages: &mut [Message],
    tools: &mut [Value],
    request: &TurnRequest,
) -> Result<(), BuildError> {
    let Some(cache) = &request.cache else {
        return Ok(());
    };
    if matches!(cache.mode, PromptCacheMode::Disabled) {
        return Ok(());
    }

    let ttl = match cache.retention {
        Some(PromptCacheRetention::Extended) => Some("1h"),
        _ => None,
    };

    match &cache.strategy {
        PromptCacheStrategy::Automatic => {
            place_on_last_block(messages, system_blocks, ttl);
        }
        PromptCacheStrategy::Explicit { breakpoints } => {
            if matches!(cache.mode, PromptCacheMode::Required)
                && breakpoints.len() > MAX_CACHE_BREAKPOINTS
            {
                return Err(BuildError::CacheViolation(format!(
                    "Anthropic supports at most {MAX_CACHE_BREAKPOINTS} cache breakpoints"
                )));
            }
            let mut placed: usize = 0;
            for bp in breakpoints {
                if placed >= MAX_CACHE_BREAKPOINTS {
                    break;
                }
                if try_place_breakpoint(bp, system_blocks, messages, tools, ttl, cache)? {
                    placed += 1;
                }
            }
        }
    }

    Ok(())
}

fn place_on_last_block(
    messages: &mut [Message],
    system_blocks: &mut [Value],
    ttl: Option<&'static str>,
) -> bool {
    for message in messages.iter_mut().rev() {
        let content = match message {
            Message::User { content } | Message::Assistant { content } => content,
        };
        if let Some(block) = content.last_mut() {
            attach_cache_control(block, ttl);
            return true;
        }
    }
    if let Some(last) = system_blocks.last_mut() {
        attach_cache_control(last, ttl);
        return true;
    }
    false
}

fn try_place_breakpoint(
    bp: &PromptCacheBreakpoint,
    system_blocks: &mut [Value],
    messages: &mut [Message],
    tools: &mut [Value],
    ttl: Option<&'static str>,
    cache: &PromptCacheRequest,
) -> Result<bool, BuildError> {
    match bp {
        PromptCacheBreakpoint::ToolsEnd => {
            if let Some(last) = tools.last_mut() {
                attach_cache_control(last, ttl);
                Ok(true)
            } else if matches!(cache.mode, PromptCacheMode::Required) {
                Err(BuildError::CacheViolation(
                    "cannot apply ToolsEnd breakpoint: no tools configured".into(),
                ))
            } else {
                Ok(false)
            }
        }
        PromptCacheBreakpoint::TranscriptItemEnd { index } => {
            // agentkit transcript index -> our post-normalization message stream.
            // We don't have a one-to-one map (system items dropped, user items merged),
            // so we approximate by clamping to the message index range.
            if let Some(message) = messages.get_mut(*index) {
                let content = match message {
                    Message::User { content } | Message::Assistant { content } => content,
                };
                if let Some(last) = content.last_mut() {
                    attach_cache_control(last, ttl);
                    return Ok(true);
                }
            }
            // Fall back to placing on system blocks if the index targets a system item.
            if let Some(last) = system_blocks.last_mut() {
                attach_cache_control(last, ttl);
                return Ok(true);
            }
            Ok(false)
        }
        PromptCacheBreakpoint::TranscriptPartEnd {
            item_index,
            part_index,
        } => {
            if let Some(message) = messages.get_mut(*item_index) {
                let content = match message {
                    Message::User { content } | Message::Assistant { content } => content,
                };
                if let Some(block) = content.get_mut(*part_index) {
                    attach_cache_control(block, ttl);
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

fn attach_cache_control(block: &mut Value, ttl: Option<&str>) {
    let Some(obj) = block.as_object_mut() else {
        return;
    };
    let mut control = Map::new();
    control.insert("type".into(), Value::String("ephemeral".into()));
    if let Some(ttl) = ttl {
        control.insert("ttl".into(), Value::String(ttl.to_string()));
    }
    obj.insert("cache_control".into(), Value::Object(control));
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

#[cfg(test)]
mod tests {
    use agentkit_core::{
        Item, ItemKind, MetadataMap, Part, SessionId, TextPart, ToolCallPart, ToolOutput,
        ToolResultPart, TurnId,
    };
    use agentkit_loop::{
        PromptCacheBreakpoint, PromptCacheRequest, PromptCacheRetention, TurnRequest,
    };
    use agentkit_tools_core::{ToolName, ToolSpec};
    use serde_json::json;

    use super::*;
    use crate::config::AnthropicConfig;

    fn base_request(transcript: Vec<Item>) -> TurnRequest {
        TurnRequest {
            session_id: SessionId::new("s"),
            turn_id: TurnId::new("t"),
            transcript,
            available_tools: Vec::new(),
            cache: None,
            structured_output: None,
            generation: Default::default(),
            metadata: MetadataMap::new(),
        }
    }

    fn cfg() -> AnthropicConfig {
        AnthropicConfig::new("k", "claude-opus-4-7", 1024).unwrap()
    }

    #[test]
    fn system_extracted_to_top_level() {
        let transcript = vec![
            Item::text(ItemKind::System, "be concise"),
            Item::text(ItemKind::User, "hello"),
        ];
        let body = build_request_body(&cfg(), &base_request(transcript)).unwrap();
        assert_eq!(body["system"][0]["text"], "be concise");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn per_turn_generation_controls_override_the_exact_request_fields() {
        let mut request = base_request(vec![Item::text(ItemKind::User, "hello")]);
        request.generation = agentkit_loop::GenerationControls {
            max_output_tokens: Some(32),
            temperature: Some(0.25),
            stop_sequences: Some(vec!["STOP".into()]),
        };
        let body = build_request_body(&cfg(), &request).unwrap();
        assert_eq!(body["max_tokens"], 32);
        assert_eq!(body["temperature"], 0.25);
        assert_eq!(body["stop_sequences"], serde_json::json!(["STOP"]));
    }

    #[test]
    fn per_turn_structured_output_projects_to_anthropic_wire_format() {
        let mut request = base_request(vec![Item::text(ItemKind::User, "edit")]);
        request.structured_output = Some(
            agentkit_loop::StructuredOutputRequest::new(
                "kit.edit-ir-input",
                1,
                true,
                json!({"type": "object", "additionalProperties": false}),
            )
            .unwrap(),
        );

        let body = build_request_body(&cfg(), &request).unwrap();
        assert_eq!(
            body["output_config"],
            json!({
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object", "additionalProperties": false}
                }
            })
        );
        assert!(body.get("response_format").is_none());
    }

    #[test]
    fn tool_results_merge_into_user_message() {
        let transcript = vec![
            Item::text(ItemKind::User, "q"),
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
            Item::text(ItemKind::User, "now summarize"),
        ];
        let body = build_request_body(&cfg(), &base_request(transcript)).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[2]["role"], "user");
        let content = messages[2]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "now summarize");
    }

    #[test]
    fn notification_emits_user_message_with_system_reminder_wrap() {
        // A notification mid-conversation: must land in the messages
        // stream (not hoisted to system_blocks), wrapped in
        // <system-reminder>. Adjacent user content can merge with it
        // since both carry the user role.
        let transcript = vec![
            Item::new(
                ItemKind::Assistant,
                vec![Part::ToolCall(ToolCallPart::new(
                    "call-1",
                    "mcp_install",
                    json!({}),
                ))],
            ),
            Item::new(
                ItemKind::Tool,
                vec![Part::ToolResult(ToolResultPart::success(
                    "call-1",
                    ToolOutput::text("running in background"),
                ))],
            ),
            Item::text(ItemKind::Assistant, "ok, kicked it off"),
            Item::notification("Slack install completed: ok"),
        ];
        let body = build_request_body(&cfg(), &base_request(transcript)).unwrap();
        let messages = body["messages"].as_array().unwrap();
        // assistant -> tool_result-as-user -> assistant -> notification-as-user
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[3]["role"], "user");
        let text = messages[3]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("<system-reminder>"));
        assert!(text.contains("Slack install completed: ok"));
        assert!(text.ends_with("</system-reminder>"));
        // Critical: must NOT leak into top-level system blocks.
        let no_leak = body
            .get("system")
            .and_then(|s| s.as_array())
            .is_none_or(|arr| {
                arr.iter()
                    .all(|b| !b["text"].as_str().unwrap_or("").contains("Slack install"))
            });
        assert!(no_leak, "notification leaked into system blocks");
    }

    #[test]
    fn automatic_cache_places_single_breakpoint() {
        let transcript = vec![Item::text(ItemKind::User, "hi")];
        let mut req = base_request(transcript);
        req.cache = Some(PromptCacheRequest::automatic());
        let body = build_request_body(&cfg(), &req).unwrap();
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["cache_control"]["type"], "ephemeral");
        assert!(block["cache_control"].get("ttl").is_none());
    }

    #[test]
    fn extended_retention_sets_1h_ttl() {
        let transcript = vec![Item::text(ItemKind::User, "hi")];
        let mut req = base_request(transcript);
        req.cache =
            Some(PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Extended));
        let body = build_request_body(&cfg(), &req).unwrap();
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["ttl"],
            "1h"
        );
    }

    #[test]
    fn explicit_tools_end_breakpoint_targets_last_tool() {
        let transcript = vec![Item::text(ItemKind::User, "hi")];
        let mut req = base_request(transcript);
        req.available_tools = vec![
            ToolSpec::new(ToolName("alpha".into()), "a", json!({ "type": "object" })),
            ToolSpec::new(ToolName("beta".into()), "b", json!({ "type": "object" })),
        ];
        req.cache = Some(PromptCacheRequest::explicit([
            PromptCacheBreakpoint::ToolsEnd,
        ]));
        let body = build_request_body(&cfg(), &req).unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn explicit_breakpoints_over_cap_rejected_when_required() {
        let transcript = vec![Item::text(ItemKind::User, "hi")];
        let mut req = base_request(transcript);
        req.cache = Some(PromptCacheRequest::explicit_required([
            PromptCacheBreakpoint::transcript_item_end(0),
            PromptCacheBreakpoint::transcript_item_end(0),
            PromptCacheBreakpoint::transcript_item_end(0),
            PromptCacheBreakpoint::transcript_item_end(0),
            PromptCacheBreakpoint::transcript_item_end(0),
        ]));
        let result = build_request_body(&cfg(), &req);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("at most 4"), "err was: {err}");
    }

    #[test]
    fn rejects_invalid_tool_names() {
        let transcript = vec![Item::text(ItemKind::User, "hi")];
        let mut req = base_request(transcript);
        req.available_tools = vec![ToolSpec::new(
            ToolName("bad.name".into()),
            "",
            json!({ "type": "object" }),
        )];
        let err = build_request_body(&cfg(), &req).unwrap_err().to_string();
        assert!(err.contains("bad.name"));
    }

    #[test]
    fn rejects_thinking_budget_at_or_above_max_tokens() {
        let transcript = vec![Item::text(ItemKind::User, "hi")];
        let req = base_request(transcript);

        // Equal budget and max_tokens — what the user actually hit against Anthropic.
        let cfg_equal = AnthropicConfig::new("k", "claude-opus-4-7", 4096)
            .unwrap()
            .with_thinking(crate::config::ThinkingConfig::Enabled {
                budget_tokens: 4096,
            });
        let err = build_request_body(&cfg_equal, &req)
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be strictly greater"), "err was: {err}");

        // Budget greater than max_tokens — also invalid.
        let cfg_over = AnthropicConfig::new("k", "claude-opus-4-7", 1024)
            .unwrap()
            .with_thinking(crate::config::ThinkingConfig::Enabled {
                budget_tokens: 2048,
            });
        assert!(build_request_body(&cfg_over, &req).is_err());

        // Budget strictly below — accepted.
        let cfg_ok = AnthropicConfig::new("k", "claude-opus-4-7", 8192)
            .unwrap()
            .with_thinking(crate::config::ThinkingConfig::Enabled {
                budget_tokens: 4096,
            });
        assert!(build_request_body(&cfg_ok, &req).is_ok());
    }

    #[test]
    fn streaming_is_enabled_by_default() {
        let transcript = vec![Item::text(ItemKind::User, "hi")];
        let body = build_request_body(&cfg(), &base_request(transcript)).unwrap();
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn streaming_can_be_disabled_via_config() {
        let transcript = vec![Item::text(ItemKind::User, "hi")];
        let body =
            build_request_body(&cfg().with_streaming(false), &base_request(transcript)).unwrap();
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn empty_assistant_item_is_skipped() {
        // An assistant Item where every part filters away (blank text,
        // reasoning without summary) would emit `content: []`, which
        // Anthropic rejects. Skip the message.
        use agentkit_core::ReasoningPart;

        let blank_text = TextPart {
            text: String::new(),
            metadata: MetadataMap::new(),
        };
        let summaryless = ReasoningPart {
            summary: None,
            data: None,
            redacted: false,
            metadata: MetadataMap::new(),
        };
        let transcript = vec![
            Item::text(ItemKind::User, "hi"),
            Item::new(
                ItemKind::Assistant,
                vec![Part::Text(blank_text), Part::Reasoning(summaryless)],
            ),
            Item::text(ItemKind::User, "still there?"),
        ];
        let body = build_request_body(&cfg(), &base_request(transcript)).unwrap();
        let messages = body["messages"].as_array().unwrap();
        // Empty assistant collapsed; the two user messages merge into one
        // (consecutive user-role turns share a message).
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        let parts = messages[0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["text"], "hi");
        assert_eq!(parts[1]["text"], "still there?");
    }

    #[test]
    fn round_trips_thinking_signature() {
        let mut meta = MetadataMap::new();
        meta.insert(
            "anthropic.thinking_signature".into(),
            Value::String("sig-123".into()),
        );
        let reasoning = agentkit_core::ReasoningPart::summary("pondering").with_metadata(meta);
        let transcript = vec![
            Item::text(ItemKind::User, "q"),
            Item::new(
                ItemKind::Assistant,
                vec![
                    Part::Reasoning(reasoning),
                    Part::Text(TextPart::new("answer")),
                ],
            ),
        ];
        let body = build_request_body(&cfg(), &base_request(transcript)).unwrap();
        let assistant = &body["messages"][1];
        assert_eq!(assistant["content"][0]["type"], "thinking");
        assert_eq!(assistant["content"][0]["signature"], "sig-123");
        assert_eq!(assistant["content"][1]["type"], "text");
    }
}
