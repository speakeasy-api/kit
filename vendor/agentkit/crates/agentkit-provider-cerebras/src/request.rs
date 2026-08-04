//! Transcript → Cerebras `/v1/chat/completions` request-body converter.
//!
//! The single entry point, [`build_chat_body`], returns `(body, extra_headers)`
//! so header-bearing knobs (`queue_threshold`) can be attached by the caller.

use agentkit_core::{Item, ItemKind, Part, ToolCallPart, ToolOutput, ToolResultPart};
use agentkit_loop::TurnRequest;
use agentkit_tools_core::ToolSpec;
use serde_json::{Map, Value, json};

use crate::config::{CerebrasConfig, PartKindName};
use crate::error::BuildError;

/// Output of [`build_chat_body`]: request body + extra headers (collected for
/// the caller to merge into the outgoing HTTP request).
#[derive(Debug)]
pub struct BuiltRequest {
    /// JSON body.
    pub body: Value,
    /// Extra headers to attach. Currently only `queue_threshold` when service
    /// tiers are configured; the rest is plumbed via `CerebrasConfig`.
    pub extra_headers: Vec<(&'static str, String)>,
}

/// Builds a complete chat-completions request body and a small set of extra
/// headers.
pub fn build_chat_body(
    cfg: &CerebrasConfig,
    turn: &TurnRequest,
) -> Result<BuiltRequest, BuildError> {
    cfg.validate()?;

    #[cfg(feature = "predicted-outputs")]
    if let Some(_pred) = &cfg.prediction {
        if !turn.available_tools.is_empty() {
            return Err(BuildError::PredictionConflicts("tools"));
        }
        if matches!(cfg.logprobs, Some(true)) {
            return Err(BuildError::PredictionConflicts("logprobs"));
        }
    }

    let messages = build_messages(&turn.transcript)?;
    let tools = build_tools(&turn.available_tools, cfg.tool_strict)?;

    let mut body = Map::new();
    body.insert("model".into(), Value::String(cfg.model.clone()));
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), Value::Bool(cfg.streaming));

    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(choice) = &cfg.tool_choice {
        body.insert("tool_choice".into(), choice.to_json());
    }
    if let Some(ptc) = cfg.parallel_tool_calls {
        body.insert("parallel_tool_calls".into(), Value::Bool(ptc));
    }

    if let Some(v) = cfg.max_completion_tokens {
        body.insert("max_completion_tokens".into(), Value::from(v));
    }
    if let Some(v) = cfg.min_tokens {
        body.insert("min_tokens".into(), Value::from(v));
    }
    if let Some(v) = cfg.temperature {
        insert_number(&mut body, "temperature", v as f64);
    }
    if let Some(v) = cfg.top_p {
        insert_number(&mut body, "top_p", v as f64);
    }
    if let Some(v) = cfg.frequency_penalty {
        insert_number(&mut body, "frequency_penalty", v as f64);
    }
    if let Some(v) = cfg.presence_penalty {
        insert_number(&mut body, "presence_penalty", v as f64);
    }
    if let Some(stops) = &cfg.stop {
        body.insert(
            "stop".into(),
            Value::Array(stops.iter().cloned().map(Value::String).collect()),
        );
    }
    if let Some(seed) = cfg.seed {
        body.insert("seed".into(), Value::from(seed));
    }
    if let Some(bias) = &cfg.logit_bias {
        let mut obj = Map::new();
        for (k, v) in bias {
            obj.insert(k.clone(), Value::from(*v));
        }
        body.insert("logit_bias".into(), Value::Object(obj));
    }
    if let Some(flag) = cfg.logprobs {
        body.insert("logprobs".into(), Value::Bool(flag));
    }
    if let Some(v) = cfg.top_logprobs {
        body.insert("top_logprobs".into(), Value::from(v));
    }
    if let Some(user) = &cfg.user {
        body.insert("user".into(), Value::String(user.clone()));
    }

    if let Some(format) = &cfg.output_format {
        if let crate::config::OutputFormat::JsonSchema { schema, strict, .. } = format
            && *strict
        {
            validate_strict_schema(schema)?;
        }
        body.insert("response_format".into(), format.to_json());
    }

    if let Some(reasoning) = &cfg.reasoning {
        reasoning.apply(&mut body);
    }

    #[cfg(feature = "predicted-outputs")]
    if let Some(prediction) = &cfg.prediction {
        body.insert("prediction".into(), prediction.to_json());
    }

    #[allow(unused_mut)]
    let mut extra_headers: Vec<(&'static str, String)> = Vec::new();
    #[cfg(feature = "service-tiers")]
    {
        if let Some(tier) = cfg.service_tier {
            body.insert("service_tier".into(), Value::String(tier.as_str().into()));
        }
        if let Some(ms) = cfg.queue_threshold_ms {
            extra_headers.push(("queue_threshold", ms.to_string()));
        }
    }

    let mut full = Value::Object(body);
    if let Some(extra) = &cfg.extra_body {
        deep_merge(&mut full, extra.clone());
    }

    Ok(BuiltRequest {
        body: full,
        extra_headers,
    })
}

fn insert_number(body: &mut Map<String, Value>, key: &'static str, value: f64) {
    if let Some(n) = serde_json::Number::from_f64(value) {
        body.insert(key.into(), Value::Number(n));
    }
}

/// Merges `patch` into `target`, recursing through objects. Non-object values
/// replace whatever sat in `target`.
fn deep_merge(target: &mut Value, patch: Value) {
    match (target, patch) {
        (Value::Object(t), Value::Object(p)) => {
            for (k, v) in p {
                match t.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        t.insert(k, v);
                    }
                }
            }
        }
        (slot, patch) => {
            *slot = patch;
        }
    }
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

fn build_messages(transcript: &[Item]) -> Result<Vec<Value>, BuildError> {
    let mut out: Vec<Value> = Vec::new();
    for item in transcript {
        match item.kind {
            ItemKind::System => {
                out.push(role_text(item, "system")?);
            }
            ItemKind::Developer => {
                out.push(role_text(item, "developer")?);
            }
            ItemKind::Context => {
                // Treat Context items as system-level scaffolding. Cerebras
                // has no dedicated role for them.
                out.push(role_text(item, "system")?);
            }
            ItemKind::User => {
                out.push(build_user_message(item)?);
            }
            ItemKind::Assistant => {
                out.push(build_assistant_message(item)?);
            }
            ItemKind::Tool => {
                out.extend(build_tool_messages(item)?);
            }
            ItemKind::Notification => {
                // Side-channel signal — render as a user-role message
                // wrapped in <system-reminder>. Stays in temporal order
                // so the model sees it at the position it was emitted.
                out.push(build_notification_message(item)?);
            }
        }
    }
    Ok(out)
}

fn build_notification_message(item: &Item) -> Result<Value, BuildError> {
    let mut buf = String::new();
    for part in &item.parts {
        match part {
            Part::Text(t) => buf.push_str(&t.text),
            Part::Structured(s) => {
                buf.push_str(&serde_json::to_string(&s.value).map_err(BuildError::Serialize)?);
            }
            Part::Reasoning(r) => {
                if let Some(summary) = &r.summary {
                    buf.push_str(summary);
                }
            }
            _ => {
                return Err(BuildError::UnsupportedPart {
                    role: ItemKind::Notification,
                    part_kind: part_kind(part).into(),
                });
            }
        }
    }
    Ok(json!({
        "role": "user",
        "content": format!("<system-reminder>\n{buf}\n</system-reminder>"),
    }))
}

fn role_text(item: &Item, role: &'static str) -> Result<Value, BuildError> {
    let mut buf = String::new();
    for part in &item.parts {
        match part {
            Part::Text(t) => buf.push_str(&t.text),
            Part::Structured(s) => buf.push_str(&serde_json::to_string(&s.value)?),
            Part::Reasoning(r) => {
                if let Some(s) = &r.summary {
                    buf.push_str(s);
                }
            }
            other => {
                return Err(BuildError::UnsupportedPart {
                    role: item.kind,
                    part_kind: part_kind(other).into(),
                });
            }
        }
    }
    Ok(json!({
        "role": role,
        "content": buf,
    }))
}

fn build_user_message(item: &Item) -> Result<Value, BuildError> {
    // Text-only fast path — send `content` as a string. If any non-text part
    // appears we fall into the (future) content-array path and bail for now
    // since multimodal is explicitly out of scope per §4.2.
    let mut all_text = true;
    for part in &item.parts {
        if !matches!(
            part,
            Part::Text(_) | Part::Structured(_) | Part::Reasoning(_)
        ) {
            all_text = false;
            break;
        }
    }
    if !all_text {
        // Find the offending part for the error message.
        let offending = item
            .parts
            .iter()
            .find(|p| !matches!(p, Part::Text(_) | Part::Structured(_) | Part::Reasoning(_)))
            .expect("non-text part present per check above");
        return Err(BuildError::UnsupportedPart {
            role: ItemKind::User,
            part_kind: part_kind(offending).into(),
        });
    }
    role_text(item, "user")
}

fn build_assistant_message(item: &Item) -> Result<Value, BuildError> {
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for part in &item.parts {
        match part {
            Part::Text(t) => content.push_str(&t.text),
            Part::Structured(s) => content.push_str(&serde_json::to_string(&s.value)?),
            Part::Reasoning(r) => {
                if let Some(s) = &r.summary {
                    reasoning.push_str(s);
                }
            }
            Part::ToolCall(call) => tool_calls.push(tool_call_to_json(call)),
            other => {
                return Err(BuildError::UnsupportedPart {
                    role: ItemKind::Assistant,
                    part_kind: part_kind(other).into(),
                });
            }
        }
    }

    let mut msg = Map::new();
    msg.insert("role".into(), Value::String("assistant".into()));
    if !content.is_empty() {
        msg.insert("content".into(), Value::String(content));
    } else if tool_calls.is_empty() {
        msg.insert("content".into(), Value::String(String::new()));
    }
    if !reasoning.is_empty() {
        msg.insert("reasoning".into(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        msg.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    Ok(Value::Object(msg))
}

fn tool_call_to_json(call: &ToolCallPart) -> Value {
    // Cerebras expects `function.arguments` to be a JSON *string*.
    let arguments = serde_json::to_string(&call.input).unwrap_or_else(|_| "{}".into());
    json!({
        "id": call.id.0,
        "type": "function",
        "function": {
            "name": call.name,
            "arguments": arguments,
        },
    })
}

fn build_tool_messages(item: &Item) -> Result<Vec<Value>, BuildError> {
    let mut out = Vec::new();
    for part in &item.parts {
        let Part::ToolResult(result) = part else {
            return Err(BuildError::UnsupportedPart {
                role: ItemKind::Tool,
                part_kind: part_kind(part).into(),
            });
        };
        out.push(tool_result_to_message(result)?);
    }
    Ok(out)
}

fn tool_result_to_message(result: &ToolResultPart) -> Result<Value, BuildError> {
    let content = match &result.output {
        ToolOutput::Text(t) => t.clone(),
        ToolOutput::Structured(v) => serde_json::to_string(v)?,
        ToolOutput::Parts(parts) => {
            let mut buf = String::new();
            for part in parts {
                match part {
                    Part::Text(t) => buf.push_str(&t.text),
                    Part::Structured(s) => buf.push_str(&serde_json::to_string(&s.value)?),
                    other => {
                        return Err(BuildError::UnsupportedPart {
                            role: ItemKind::Tool,
                            part_kind: part_kind(other).into(),
                        });
                    }
                }
            }
            buf
        }
        ToolOutput::Files(_) => {
            return Err(BuildError::UnsupportedPart {
                role: ItemKind::Tool,
                part_kind: PartKindName::File,
            });
        }
    };
    Ok(json!({
        "role": "tool",
        "tool_call_id": result.call_id.0,
        "content": content,
    }))
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn build_tools(specs: &[ToolSpec], strict: bool) -> Result<Vec<Value>, BuildError> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        validate_tool_name(&spec.name.0)?;
        let mut function = Map::new();
        function.insert("name".into(), Value::String(spec.name.0.clone()));
        function.insert(
            "description".into(),
            Value::String(spec.description.clone()),
        );
        function.insert("parameters".into(), spec.input_schema.clone());
        if strict {
            function.insert("strict".into(), Value::Bool(true));
        }
        out.push(json!({
            "type": "function",
            "function": Value::Object(function),
        }));
    }
    Ok(out)
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

// ---------------------------------------------------------------------------
// Strict JSON-schema validation (§4 constraints)
// ---------------------------------------------------------------------------

fn validate_strict_schema(schema: &Value) -> Result<(), BuildError> {
    let schema_str = serde_json::to_string(schema)?;
    if schema_str.len() > 5000 {
        return Err(BuildError::SchemaViolation(format!(
            "schema serialized length {} exceeds 5000",
            schema_str.len()
        )));
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(BuildError::SchemaViolation(
            "root type must be \"object\"".into(),
        ));
    }
    if schema.get("additionalProperties") != Some(&Value::Bool(false)) {
        return Err(BuildError::SchemaViolation(
            "additionalProperties must be false at root".into(),
        ));
    }
    // Depth / prop / enum / forbidden-keyword scan.
    let mut counts = SchemaScan::default();
    walk_schema(schema, 0, &mut counts)?;
    if counts.depth > 10 {
        return Err(BuildError::SchemaViolation(format!(
            "nest depth {} exceeds 10",
            counts.depth
        )));
    }
    if counts.props > 500 {
        return Err(BuildError::SchemaViolation(format!(
            "property count {} exceeds 500",
            counts.props
        )));
    }
    if counts.enum_values > 500 {
        return Err(BuildError::SchemaViolation(format!(
            "enum value count {} exceeds 500",
            counts.enum_values
        )));
    }
    Ok(())
}

#[derive(Default)]
struct SchemaScan {
    depth: usize,
    props: usize,
    enum_values: usize,
}

fn walk_schema(schema: &Value, depth: usize, scan: &mut SchemaScan) -> Result<(), BuildError> {
    scan.depth = scan.depth.max(depth);
    let Some(obj) = schema.as_object() else {
        return Ok(());
    };
    for forbidden in ["pattern", "format", "minItems", "maxItems"] {
        if obj.contains_key(forbidden) {
            return Err(BuildError::SchemaViolation(format!(
                "unsupported keyword `{forbidden}` under strict mode"
            )));
        }
    }
    if let Some(reff) = obj.get("$ref").and_then(Value::as_str)
        && !reff.starts_with("#/$defs/")
    {
        return Err(BuildError::SchemaViolation(
            "only #/$defs/... references are allowed under strict mode".into(),
        ));
    }
    if let Some(enum_values) = obj.get("enum").and_then(Value::as_array) {
        scan.enum_values += enum_values.len();
    }
    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        scan.props += props.len();
        for (_k, v) in props {
            walk_schema(v, depth + 1, scan)?;
        }
    }
    if let Some(items) = obj.get("items") {
        walk_schema(items, depth + 1, scan)?;
    }
    if let Some(defs) = obj.get("$defs").and_then(Value::as_object) {
        for (_k, v) in defs {
            walk_schema(v, depth + 1, scan)?;
        }
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(arr) = obj.get(key).and_then(Value::as_array) {
            for v in arr {
                walk_schema(v, depth + 1, scan)?;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Part kind shim
// ---------------------------------------------------------------------------

fn part_kind(part: &Part) -> agentkit_core::PartKind {
    use agentkit_core::PartKind;
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
        Item, ItemKind, MetadataMap, Part, SessionId, ToolCallPart, ToolOutput, ToolResultPart,
        TurnId,
    };
    use agentkit_loop::TurnRequest;
    use agentkit_tools_core::{ToolName, ToolSpec};
    use serde_json::json;

    use super::*;
    use crate::config::{CerebrasConfig, OutputFormat, ToolChoice};

    fn cfg() -> CerebrasConfig {
        CerebrasConfig::new("k", "gpt-oss-120b").unwrap()
    }

    fn req(transcript: Vec<Item>, tools: Vec<ToolSpec>) -> TurnRequest {
        TurnRequest {
            session_id: SessionId::new("s"),
            turn_id: TurnId::new("t"),
            transcript,
            available_tools: tools,
            cache: None,
            structured_output: None,
            generation: Default::default(),
            metadata: MetadataMap::new(),
        }
    }

    #[test]
    fn plain_text_turn_builds() {
        let req = req(
            vec![
                Item::text(ItemKind::System, "be concise"),
                Item::text(ItemKind::User, "hi"),
            ],
            Vec::new(),
        );
        let out = build_chat_body(&cfg(), &req).unwrap();
        let body = out.body;
        assert_eq!(body["model"], "gpt-oss-120b");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "be concise");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "hi");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn assistant_tool_call_round_trips() {
        let req = req(
            vec![
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
            ],
            Vec::new(),
        );
        let body = build_chat_body(&cfg(), &req).unwrap().body;
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["tool_calls"][0]["type"], "function");
        assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "search");
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"],
            "{\"q\":\"x\"}"
        );
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call-1");
        assert_eq!(messages[2]["content"], "hit");
    }

    #[test]
    fn tool_spec_becomes_function_declaration() {
        let req = req(
            vec![Item::text(ItemKind::User, "hi")],
            vec![ToolSpec::new(
                ToolName("search".into()),
                "web search",
                json!({ "type": "object", "properties": { "q": { "type": "string" } } }),
            )],
        );
        let body = build_chat_body(&cfg().with_tool_strict(true), &req)
            .unwrap()
            .body;
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "search");
        assert_eq!(body["tools"][0]["function"]["strict"], true);
    }

    #[test]
    fn strict_json_schema_violations_rejected() {
        let req = req(vec![Item::text(ItemKind::User, "hi")], Vec::new());
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "pattern": "^[a-z]+$" }
            }
        });
        let cfg = cfg().with_output_format(OutputFormat::JsonSchema {
            schema,
            strict: true,
            name: None,
        });
        let err = build_chat_body(&cfg, &req).unwrap_err();
        assert!(matches!(err, BuildError::SchemaViolation(_)));
    }

    #[test]
    fn strict_json_schema_accepts_valid() {
        let req = req(vec![Item::text(ItemKind::User, "hi")], Vec::new());
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string" }
            },
            "required": ["name"]
        });
        let cfg = cfg().with_output_format(OutputFormat::JsonSchema {
            schema,
            strict: true,
            name: Some("p".into()),
        });
        let out = build_chat_body(&cfg, &req).unwrap();
        assert_eq!(out.body["response_format"]["type"], "json_schema");
    }

    #[test]
    fn tool_choice_serializes() {
        let req = req(
            vec![Item::text(ItemKind::User, "hi")],
            vec![ToolSpec::new(
                ToolName("t".into()),
                "",
                json!({"type":"object"}),
            )],
        );
        let out = build_chat_body(
            &cfg().with_tool_choice(ToolChoice::Function { name: "t".into() }),
            &req,
        )
        .unwrap();
        assert_eq!(out.body["tool_choice"]["type"], "function");
    }

    #[cfg(feature = "predicted-outputs")]
    #[test]
    fn prediction_conflicts_with_tools() {
        let req = req(
            vec![Item::text(ItemKind::User, "hi")],
            vec![ToolSpec::new(
                ToolName("t".into()),
                "",
                json!({"type":"object"}),
            )],
        );
        let cfg = cfg().with_prediction(crate::config::Prediction::Content("x".into()));
        let err = build_chat_body(&cfg, &req).unwrap_err();
        assert!(matches!(err, BuildError::PredictionConflicts("tools")));
    }

    #[cfg(feature = "service-tiers")]
    #[test]
    fn queue_threshold_emits_header() {
        let req = req(vec![Item::text(ItemKind::User, "hi")], Vec::new());
        let out = build_chat_body(&cfg().with_queue_threshold_ms(100), &req).unwrap();
        assert!(
            out.extra_headers
                .iter()
                .any(|(k, v)| *k == "queue_threshold" && v == "100")
        );
    }

    #[test]
    fn rejects_invalid_tool_name() {
        let req = req(
            vec![Item::text(ItemKind::User, "hi")],
            vec![ToolSpec::new(
                ToolName("bad.name".into()),
                "",
                json!({"type":"object"}),
            )],
        );
        let err = build_chat_body(&cfg(), &req).unwrap_err();
        assert!(matches!(err, BuildError::InvalidToolName(_)));
    }

    #[test]
    fn extra_body_deep_merges() {
        let req = req(vec![Item::text(ItemKind::User, "hi")], Vec::new());
        let cfg = cfg().with_extra_body(json!({ "custom_field": { "nested": 1 } }));
        let body = build_chat_body(&cfg, &req).unwrap().body;
        assert_eq!(body["custom_field"]["nested"], 1);
    }

    #[test]
    fn streaming_toggle_flows_into_body() {
        let req = req(vec![Item::text(ItemKind::User, "hi")], Vec::new());
        let body = build_chat_body(&cfg().with_streaming(false), &req)
            .unwrap()
            .body;
        assert_eq!(body["stream"], false);
    }
}
