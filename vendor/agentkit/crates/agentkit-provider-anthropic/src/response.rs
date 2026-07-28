use std::collections::VecDeque;

use agentkit_core::{
    CustomPart, Delta, FinishReason, Item, ItemKind, MetadataMap, Part, ReasoningPart, TextPart,
    TokenUsage, ToolCallPart, Usage,
};
use agentkit_loop::{ModelTurnEvent, ModelTurnResult};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum ResponseError {
    #[error("invalid Anthropic response: {0}")]
    Protocol(String),
}

/// Parses a Messages API response body into a stream of `ModelTurnEvent`s.
pub(crate) fn build_turn_from_response(
    body: &str,
) -> Result<VecDeque<ModelTurnEvent>, ResponseError> {
    let raw: Value = serde_json::from_str(body)
        .map_err(|e| ResponseError::Protocol(format!("parse error: {e}")))?;

    let content = raw
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| ResponseError::Protocol("missing content array".into()))?;

    let stop_reason = raw.get("stop_reason").and_then(Value::as_str);
    let stop_sequence = raw.get("stop_sequence").and_then(Value::as_str);
    let message_id = raw.get("id").and_then(Value::as_str).map(str::to_string);
    let model = raw.get("model").and_then(Value::as_str);

    let mut metadata = MetadataMap::new();
    if let Some(model) = model {
        metadata.insert("anthropic.model".into(), Value::String(model.into()));
    }
    if let Some(seq) = stop_sequence {
        metadata.insert("anthropic.stop_sequence".into(), Value::String(seq.into()));
    }
    if let Some(reason) = stop_reason {
        metadata.insert("anthropic.stop_reason".into(), Value::String(reason.into()));
    }
    if let Some(container) = raw.get("container") {
        metadata.insert("anthropic.container".into(), container.clone());
    }

    let usage = parse_usage(raw.get("usage"));

    let mut parts: Vec<Part> = Vec::new();
    for block in content {
        if let Some(part) = block_to_part(block)? {
            parts.push(part);
        }
    }

    let mut events = VecDeque::new();

    if let Some(usage) = usage.clone() {
        events.push_back(ModelTurnEvent::Usage(usage));
    }

    for part in &parts {
        if let Part::ToolCall(call) = part {
            events.push_back(ModelTurnEvent::ToolCall(call.clone()));
        }
    }

    let finish_reason = map_stop_reason(stop_reason);

    if parts.is_empty() {
        events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason,
            output_items: Vec::new(),
            usage,
            metadata: MetadataMap::new(),
            model: model.map(str::to_owned),
            response_id: message_id,
        }));
        return Ok(events);
    }

    for part in &parts {
        events.push_back(ModelTurnEvent::Delta(Delta::CommitPart {
            part: part.clone(),
        }));
    }

    let item = Item {
        id: message_id.clone().map(Into::into),
        kind: ItemKind::Assistant,
        parts,
        metadata,
        usage: None,
        finish_reason: None,
        created_at: None,
    };

    events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
        finish_reason,
        output_items: vec![item],
        usage,
        metadata: MetadataMap::new(),
        model: model.map(str::to_owned),
        response_id: message_id,
    }));

    Ok(events)
}

fn block_to_part(block: &Value) -> Result<Option<Part>, ResponseError> {
    let kind = block
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| ResponseError::Protocol("content block missing type".into()))?;

    match kind {
        "text" => {
            let text = block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Ok(Some(Part::Text(TextPart::new(text))))
        }
        "thinking" => {
            let summary = block
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let signature = block
                .get("signature")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut meta = MetadataMap::new();
            meta.insert(
                "anthropic.thinking_signature".into(),
                Value::String(signature),
            );
            Ok(Some(Part::Reasoning(
                ReasoningPart::summary(summary).with_metadata(meta),
            )))
        }
        "redacted_thinking" => {
            let data = block
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut meta = MetadataMap::new();
            meta.insert("anthropic.redacted_data".into(), Value::String(data));
            Ok(Some(Part::Reasoning(ReasoningPart {
                summary: None,
                data: None,
                redacted: true,
                metadata: meta,
            })))
        }
        "tool_use" => {
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| ResponseError::Protocol("tool_use missing id".into()))?
                .to_string();
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| ResponseError::Protocol("tool_use missing name".into()))?
                .to_string();
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            Ok(Some(Part::ToolCall(ToolCallPart::new(id, name, input))))
        }
        // Server tool blocks and anything else — round-trip as opaque custom parts
        // so the loop can re-send them on the next turn.
        other => Ok(Some(Part::Custom(
            CustomPart::new(format!("anthropic.{other}")).with_value(block.clone()),
        ))),
    }
}

fn parse_usage(value: Option<&Value>) -> Option<Usage> {
    let value = value?;
    let input = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = value.get("cache_read_input_tokens").and_then(Value::as_u64);
    let cache_write = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);

    let mut metadata = MetadataMap::new();
    if let Some(creation) = value.get("cache_creation") {
        metadata.insert("anthropic.cache_creation".into(), creation.clone());
    }
    if let Some(tier) = value.get("service_tier") {
        metadata.insert("anthropic.service_tier".into(), tier.clone());
    }
    if let Some(server_tool_use) = value.get("server_tool_use") {
        metadata.insert("anthropic.server_tool_use".into(), server_tool_use.clone());
    }

    Some(Usage {
        tokens: Some(TokenUsage {
            input_tokens: input,
            output_tokens: output,
            reasoning_tokens: None,
            cached_input_tokens: cached,
            cache_write_input_tokens: cache_write,
        }),
        cost: None,
        metadata,
    })
}

fn map_stop_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("end_turn") => FinishReason::Completed,
        Some("tool_use") => FinishReason::ToolCall,
        Some("max_tokens") => FinishReason::MaxTokens,
        Some("stop_sequence") => FinishReason::Completed,
        Some("refusal") => FinishReason::Blocked,
        Some("pause_turn") => FinishReason::Other("pause_turn".into()),
        Some(other) => FinishReason::Other(other.into()),
        None => FinishReason::Completed,
    }
}

#[cfg(test)]
mod tests {
    use agentkit_core::{Part, TokenUsage};
    use serde_json::json;

    use super::*;

    #[test]
    fn parses_text_and_usage() {
        let body = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-4-7",
            "stop_reason": "end_turn",
            "content": [{ "type": "text", "text": "hello" }],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_creation_input_tokens": 2,
                "cache_read_input_tokens": 3
            }
        })
        .to_string();

        let events = build_turn_from_response(&body).unwrap();
        let mut events: Vec<_> = events.into_iter().collect();
        assert!(
            matches!(events.remove(0), ModelTurnEvent::Usage(u) if u.tokens == Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                reasoning_tokens: None,
                cached_input_tokens: Some(3),
                cache_write_input_tokens: Some(2),
            }))
        );
        let finished = events.pop().unwrap();
        match finished {
            ModelTurnEvent::Finished(result) => {
                assert_eq!(result.finish_reason, FinishReason::Completed);
                let item = &result.output_items[0];
                assert!(matches!(item.parts[0], Part::Text(_)));
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn parses_tool_use_and_thinking() {
        let body = json!({
            "id": "msg_2",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus-4-7",
            "stop_reason": "tool_use",
            "content": [
                { "type": "thinking", "thinking": "...", "signature": "sig-1" },
                { "type": "tool_use", "id": "tool-1", "name": "search", "input": { "q": "x" } }
            ],
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })
        .to_string();

        let events: Vec<_> = build_turn_from_response(&body)
            .unwrap()
            .into_iter()
            .collect();
        let tool_events: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ModelTurnEvent::ToolCall(_)))
            .collect();
        assert_eq!(tool_events.len(), 1);
        let finished = events.last().unwrap();
        let ModelTurnEvent::Finished(result) = finished else {
            panic!("last event should be Finished");
        };
        assert_eq!(result.finish_reason, FinishReason::ToolCall);
        let item = &result.output_items[0];
        match &item.parts[0] {
            Part::Reasoning(r) => {
                assert_eq!(r.metadata["anthropic.thinking_signature"], "sig-1");
            }
            other => panic!("expected reasoning, got {other:?}"),
        }
    }

    #[test]
    fn server_tool_blocks_round_trip_as_custom() {
        let body = json!({
            "id": "msg_3",
            "role": "assistant",
            "model": "claude-opus-4-7",
            "stop_reason": "end_turn",
            "content": [
                { "type": "server_tool_use", "id": "s-1", "name": "web_search", "input": {} }
            ],
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })
        .to_string();
        let events: Vec<_> = build_turn_from_response(&body)
            .unwrap()
            .into_iter()
            .collect();
        let ModelTurnEvent::Finished(result) = events.last().unwrap() else {
            panic!("missing Finished");
        };
        match &result.output_items[0].parts[0] {
            Part::Custom(c) => assert_eq!(c.kind, "anthropic.server_tool_use"),
            other => panic!("expected custom, got {other:?}"),
        }
    }
}
