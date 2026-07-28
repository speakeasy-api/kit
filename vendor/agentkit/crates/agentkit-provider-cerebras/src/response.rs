//! Buffered Cerebras response → `VecDeque<ModelTurnEvent>`.
//!
//! Streaming responses go through [`crate::stream`]; this module is only used
//! when `CerebrasConfig::streaming` is `false`.

use std::collections::VecDeque;

use agentkit_core::{
    Delta, FinishReason, Item, ItemKind, MetadataMap, Part, ReasoningPart, TextPart, TokenUsage,
    ToolCallPart, Usage,
};
use agentkit_loop::{ModelTurnEvent, ModelTurnResult};
use serde_json::Value;

use crate::error::ResponseError;

/// Parses a full `/v1/chat/completions` JSON body into a sequence of
/// `ModelTurnEvent`s.
pub fn build_turn_from_response(body: &str) -> Result<VecDeque<ModelTurnEvent>, ResponseError> {
    let raw: Value = serde_json::from_str(body)
        .map_err(|e| ResponseError::Protocol(format!("parse error: {e}")))?;

    if let Some(err_obj) = raw.get("error") {
        let message = err_obj
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        let status_code = raw
            .get("status_code")
            .and_then(Value::as_u64)
            .map(|n| n as u16);
        return Err(ResponseError::StreamError {
            message,
            status_code,
        });
    }

    let choices = raw
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| ResponseError::Protocol("missing choices array".into()))?;

    let usage = parse_usage(&raw);

    let mut events = VecDeque::new();
    if let Some(usage) = usage.clone() {
        events.push_back(ModelTurnEvent::Usage(usage));
    }

    let mut output_items: Vec<Item> = Vec::new();
    let mut finish_reason = FinishReason::Completed;

    for choice in choices {
        let message = choice.get("message").unwrap_or(&Value::Null);
        let finish = choice.get("finish_reason").and_then(Value::as_str);
        if let Some(f) = finish {
            finish_reason = map_finish_reason(f);
        }

        let mut parts: Vec<Part> = Vec::new();

        if let Some(reasoning) = message.get("reasoning").and_then(Value::as_str)
            && !reasoning.is_empty()
        {
            parts.push(Part::Reasoning(ReasoningPart::summary(reasoning)));
        }

        if let Some(content) = message.get("content").and_then(Value::as_str)
            && !content.is_empty()
        {
            parts.push(Part::Text(TextPart::new(content)));
        }

        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                if let Some(part) = tool_call_to_part(call) {
                    if let Part::ToolCall(tc) = &part {
                        events.push_back(ModelTurnEvent::ToolCall(tc.clone()));
                    }
                    parts.push(part);
                }
            }
        }

        for part in &parts {
            events.push_back(ModelTurnEvent::Delta(Delta::CommitPart {
                part: part.clone(),
            }));
        }

        if !parts.is_empty() {
            output_items.push(Item {
                id: raw.get("id").and_then(Value::as_str).map(Into::into),
                kind: ItemKind::Assistant,
                parts,
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            });
        }
    }

    events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
        finish_reason,
        output_items,
        usage,
        metadata: MetadataMap::new(),
        model: raw.get("model").and_then(Value::as_str).map(str::to_owned),
        response_id: raw.get("id").and_then(Value::as_str).map(str::to_owned),
    }));

    Ok(events)
}

fn tool_call_to_part(call: &Value) -> Option<Part> {
    let id = call.get("id").and_then(Value::as_str)?.to_string();
    let function = call.get("function")?;
    let name = function.get("name").and_then(Value::as_str)?.to_string();
    let arguments_str = function
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let input: Value = serde_json::from_str(arguments_str).unwrap_or(Value::Null);
    Some(Part::ToolCall(ToolCallPart::new(id, name, input)))
}

/// Parses usage + time_info + service_tier + system_fingerprint off the
/// envelope, folding Cerebras-specific fields into `Usage.metadata` under the
/// `cerebras.` prefix.
pub(crate) fn parse_usage(raw: &Value) -> Option<Usage> {
    let usage_obj = raw.get("usage")?;
    let input_tokens = usage_obj
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage_obj
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let cached_input_tokens = usage_obj
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64);

    let completion_details = usage_obj.get("completion_tokens_details");
    let reasoning_tokens = completion_details
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64);
    let accepted = completion_details
        .and_then(|d| d.get("accepted_prediction_tokens"))
        .and_then(Value::as_u64);
    let rejected = completion_details
        .and_then(|d| d.get("rejected_prediction_tokens"))
        .and_then(Value::as_u64);

    let mut metadata = MetadataMap::new();
    if let Some(n) = accepted {
        metadata.insert("cerebras.accepted_prediction_tokens".into(), Value::from(n));
    }
    if let Some(n) = rejected {
        metadata.insert("cerebras.rejected_prediction_tokens".into(), Value::from(n));
    }
    if let Some(time_info) = raw.get("time_info") {
        metadata.insert("cerebras.time_info".into(), time_info.clone());
    }
    if let Some(tier) = raw.get("service_tier") {
        metadata.insert("cerebras.service_tier".into(), tier.clone());
    }
    if let Some(tier_used) = raw.get("service_tier_used") {
        metadata.insert("cerebras.service_tier_used".into(), tier_used.clone());
    }
    if let Some(fp) = raw.get("system_fingerprint") {
        metadata.insert("cerebras.system_fingerprint".into(), fp.clone());
    }

    Some(Usage {
        tokens: Some(TokenUsage {
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cached_input_tokens,
            cache_write_input_tokens: None,
        }),
        cost: None,
        metadata,
    })
}

/// Maps a Cerebras `finish_reason` string onto the core `FinishReason` enum.
pub(crate) fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" | "done" => FinishReason::Completed,
        "length" => FinishReason::MaxTokens,
        "tool_calls" => FinishReason::ToolCall,
        "content_filter" => FinishReason::Blocked,
        other => FinishReason::Other(other.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::Part;
    use serde_json::json;

    #[test]
    fn parses_content_only() {
        let body = json!({
            "id": "msg_1",
            "model": "gpt-oss-120b",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hello" },
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 2,
                "total_tokens": 12,
            }
        })
        .to_string();
        let events: Vec<_> = build_turn_from_response(&body)
            .unwrap()
            .into_iter()
            .collect();
        let ModelTurnEvent::Finished(result) = events.last().unwrap() else {
            panic!("last event must be Finished");
        };
        assert_eq!(result.finish_reason, FinishReason::Completed);
        match &result.output_items[0].parts[0] {
            Part::Text(t) => assert_eq!(t.text, "hello"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn parses_reasoning_and_tool_calls() {
        let body = json!({
            "id": "msg_2",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "",
                    "reasoning": "think",
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": { "name": "search", "arguments": "{\"q\":\"x\"}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 1,
                "completion_tokens": 1,
                "completion_tokens_details": { "reasoning_tokens": 4 }
            }
        })
        .to_string();
        let events: Vec<_> = build_turn_from_response(&body)
            .unwrap()
            .into_iter()
            .collect();
        let ModelTurnEvent::Finished(result) = events.last().unwrap() else {
            panic!("last event must be Finished");
        };
        assert_eq!(result.finish_reason, FinishReason::ToolCall);
        let parts = &result.output_items[0].parts;
        assert!(matches!(parts[0], Part::Reasoning(_)));
        assert!(matches!(parts[1], Part::ToolCall(_)));
        let tokens = result.usage.as_ref().unwrap().tokens.as_ref().unwrap();
        assert_eq!(tokens.reasoning_tokens, Some(4));
    }

    #[test]
    fn parses_cached_and_predicted_tokens() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "ok" },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 10,
                "prompt_tokens_details": { "cached_tokens": 50 },
                "completion_tokens_details": {
                    "accepted_prediction_tokens": 3,
                    "rejected_prediction_tokens": 2
                }
            },
            "time_info": { "total_time": 0.12 },
            "service_tier": "auto",
            "service_tier_used": "priority",
            "system_fingerprint": "fp-123"
        })
        .to_string();
        let events: Vec<_> = build_turn_from_response(&body)
            .unwrap()
            .into_iter()
            .collect();
        let ModelTurnEvent::Finished(result) = events.last().unwrap() else {
            panic!("last event must be Finished");
        };
        let usage = result.usage.as_ref().unwrap();
        assert_eq!(usage.tokens.as_ref().unwrap().cached_input_tokens, Some(50));
        assert_eq!(
            usage.metadata.get("cerebras.accepted_prediction_tokens"),
            Some(&Value::from(3u64))
        );
        assert_eq!(
            usage.metadata.get("cerebras.rejected_prediction_tokens"),
            Some(&Value::from(2u64))
        );
        assert!(usage.metadata.contains_key("cerebras.time_info"));
        assert_eq!(
            usage.metadata.get("cerebras.service_tier_used"),
            Some(&Value::String("priority".into()))
        );
        assert_eq!(
            usage.metadata.get("cerebras.system_fingerprint"),
            Some(&Value::String("fp-123".into()))
        );
    }

    #[test]
    fn all_finish_reasons_map_correctly() {
        assert_eq!(map_finish_reason("stop"), FinishReason::Completed);
        assert_eq!(map_finish_reason("done"), FinishReason::Completed);
        assert_eq!(map_finish_reason("length"), FinishReason::MaxTokens);
        assert_eq!(map_finish_reason("tool_calls"), FinishReason::ToolCall);
        assert_eq!(map_finish_reason("content_filter"), FinishReason::Blocked);
        assert!(matches!(
            map_finish_reason("novel"),
            FinishReason::Other(s) if s == "novel"
        ));
    }

    #[test]
    fn surface_top_level_error_as_stream_error() {
        let body = json!({
            "error": { "message": "rate limited", "type": "rate_limit_exceeded" },
            "status_code": 429
        })
        .to_string();
        let err = build_turn_from_response(&body).unwrap_err();
        match err {
            ResponseError::StreamError {
                message,
                status_code,
            } => {
                assert_eq!(message, "rate limited");
                assert_eq!(status_code, Some(429));
            }
            other => panic!("expected StreamError, got {other:?}"),
        }
    }
}
