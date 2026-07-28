//! Cerebras SSE → `ModelTurnEvent` translator.
//!
//! Cerebras streams chat-completion chunks that look like OpenAI's: each frame
//! carries `choices[i].delta` with `content`, `reasoning`, and/or
//! `tool_calls[].function.{name,arguments}` fragments. The terminator is
//! `data: [DONE]`. Errors arrive either as `event: error` frames or as
//! unnamed frames whose JSON carries a top-level `error` key.

use std::collections::BTreeMap;

use agentkit_core::{
    Delta, FinishReason, Item, ItemKind, MetadataMap, Part, PartId, PartKind, ReasoningPart,
    TextPart, ToolCallPart, Usage,
};
use agentkit_loop::{ModelTurnEvent, ModelTurnResult};
use serde_json::Value;

use crate::error::ResponseError;
pub(crate) use crate::sse::{SseDecoder, SseEvent};

/// Per-choice assembly state.
struct ChoiceState {
    content_part_id: Option<PartId>,
    content_open: bool,
    content_emitted: bool,
    content_buffer: String,
    reasoning_part_id: Option<PartId>,
    reasoning_open: bool,
    reasoning_emitted: bool,
    reasoning_buffer: String,
    tool_calls: BTreeMap<u32, ToolCallAccum>,
    finish_reason: Option<FinishReason>,
}

impl ChoiceState {
    fn new() -> Self {
        Self {
            content_part_id: None,
            content_open: false,
            content_emitted: false,
            content_buffer: String::new(),
            reasoning_part_id: None,
            reasoning_open: false,
            reasoning_emitted: false,
            reasoning_buffer: String::new(),
            tool_calls: BTreeMap::new(),
            finish_reason: None,
        }
    }
}

/// In-flight accumulator for a single streamed tool call.
struct ToolCallAccum {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
    emitted: bool,
}

impl ToolCallAccum {
    fn new() -> Self {
        Self {
            id: None,
            name: None,
            arguments: String::new(),
            emitted: false,
        }
    }
}

/// Stateful translator that consumes decoded SSE frames and emits
/// `ModelTurnEvent`s. One instance handles exactly one streaming turn.
pub(crate) struct EventTranslator {
    choices: BTreeMap<u32, ChoiceState>,
    terminal_usage: Option<Usage>,
    message_id: Option<String>,
    model: Option<String>,
    finished: bool,
}

impl EventTranslator {
    pub(crate) fn new() -> Self {
        Self {
            choices: BTreeMap::new(),
            terminal_usage: None,
            message_id: None,
            model: None,
            finished: false,
        }
    }

    /// Translates a single SSE record into zero or more `ModelTurnEvent`s.
    pub(crate) fn handle(
        &mut self,
        event: &SseEvent,
    ) -> Result<Vec<ModelTurnEvent>, ResponseError> {
        if self.finished {
            return Ok(Vec::new());
        }
        // Error dispatch ----------------------------------------------------
        if event.name.as_deref() == Some("error") {
            return Err(Self::parse_error_frame(&event.data));
        }
        if event.name.is_some() && event.name.as_deref() != Some("error") {
            // Unknown named event — forward compat, silently ignore.
            return Ok(Vec::new());
        }
        // Terminator -------------------------------------------------------
        if event.data.trim() == "[DONE]" {
            return Ok(self.finalize());
        }
        // Parse the JSON payload -------------------------------------------
        let json: Value = serde_json::from_str(&event.data)
            .map_err(|e| ResponseError::Protocol(format!("invalid SSE JSON: {e}")))?;

        // Unnamed frames whose JSON has a top-level `error` key are treated
        // as terminal errors too.
        if let Some(err_obj) = json.get("error") {
            let message = err_obj
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            let status_code = json
                .get("status_code")
                .and_then(Value::as_u64)
                .map(|n| n as u16);
            return Err(ResponseError::StreamError {
                message,
                status_code,
            });
        }

        if let Some(id) = json.get("id").and_then(Value::as_str)
            && self.message_id.is_none()
        {
            self.message_id = Some(id.to_string());
        }

        if let Some(model) = json.get("model").and_then(Value::as_str)
            && self.model.is_none()
        {
            self.model = Some(model.to_string());
        }

        // Terminal usage frames arrive on the same envelope; the last chunk
        // carries `usage` + `time_info` alongside a `done` finish reason.
        if json.get("usage").is_some()
            && let Some(usage) = crate::response::parse_usage(&json)
        {
            self.terminal_usage = Some(usage);
        }

        let mut out = Vec::new();
        let Some(choices) = json.get("choices").and_then(Value::as_array) else {
            return Ok(out);
        };
        for choice in choices {
            let index = choice.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
            let state = self.choices.entry(index).or_insert_with(ChoiceState::new);
            let delta = choice.get("delta").unwrap_or(&Value::Null);

            // content ---------------------------------------------------
            if let Some(content) = delta.get("content").and_then(Value::as_str)
                && !content.is_empty()
            {
                if !state.content_open {
                    state.content_open = true;
                    let pid = PartId::new(format!("part-{index}-content"));
                    state.content_part_id = Some(pid.clone());
                    out.push(ModelTurnEvent::Delta(Delta::BeginPart {
                        part_id: pid,
                        kind: PartKind::Text,
                    }));
                }
                state.content_buffer.push_str(content);
                if let Some(pid) = &state.content_part_id {
                    out.push(ModelTurnEvent::Delta(Delta::AppendText {
                        part_id: pid.clone(),
                        chunk: content.to_string(),
                    }));
                }
            }

            // reasoning -------------------------------------------------
            if let Some(reasoning) = delta.get("reasoning").and_then(Value::as_str)
                && !reasoning.is_empty()
            {
                if !state.reasoning_open {
                    state.reasoning_open = true;
                    let pid = PartId::new(format!("part-{index}-reasoning"));
                    state.reasoning_part_id = Some(pid.clone());
                    out.push(ModelTurnEvent::Delta(Delta::BeginPart {
                        part_id: pid,
                        kind: PartKind::Reasoning,
                    }));
                }
                state.reasoning_buffer.push_str(reasoning);
                if let Some(pid) = &state.reasoning_part_id {
                    out.push(ModelTurnEvent::Delta(Delta::AppendText {
                        part_id: pid.clone(),
                        chunk: reasoning.to_string(),
                    }));
                }
            }

            // tool_calls ------------------------------------------------
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    let idx = call.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let accum = state
                        .tool_calls
                        .entry(idx)
                        .or_insert_with(ToolCallAccum::new);
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        accum.id = Some(id.to_string());
                    }
                    if let Some(function) = call.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            let existing = accum.name.get_or_insert_with(String::new);
                            existing.push_str(name);
                        }
                        if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                            accum.arguments.push_str(args);
                        }
                    }
                }
            }

            // finish_reason -------------------------------------------
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                state.finish_reason = Some(crate::response::map_finish_reason(reason));
                // Commit content/reasoning as soon as we see the finish.
                out.extend(commit_choice(state));
                // Flush any ready tool calls.
                let flushed = flush_tool_calls(state)?;
                out.extend(flushed);
            }
        }

        Ok(out)
    }

    /// True once the translator has emitted its terminal `Finished`.
    pub(crate) fn is_done(&self) -> bool {
        self.finished
    }

    fn parse_error_frame(data: &str) -> ResponseError {
        let Ok(json): Result<Value, _> = serde_json::from_str(data) else {
            return ResponseError::Protocol(format!("malformed error frame: {data}"));
        };
        let err_obj = json.get("error").unwrap_or(&json);
        let message = err_obj
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();
        let status_code = json
            .get("status_code")
            .and_then(Value::as_u64)
            .map(|n| n as u16);
        ResponseError::StreamError {
            message,
            status_code,
        }
    }

    fn finalize(&mut self) -> Vec<ModelTurnEvent> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;

        let mut events = Vec::new();
        if let Some(usage) = self.terminal_usage.clone() {
            events.push(ModelTurnEvent::Usage(usage));
        }

        // Commit any still-open content/reasoning buffers and emit tool calls
        // whose `arguments` stayed unparsed until [DONE].
        let mut aggregate_finish: Option<FinishReason> = None;
        let mut output_items: Vec<Item> = Vec::new();
        for (_index, state) in std::mem::take(&mut self.choices) {
            let mut state = state;
            events.extend(commit_choice(&mut state));
            match flush_tool_calls(&mut state) {
                Ok(flushed) => events.extend(flushed),
                Err(e) => {
                    events.push(ModelTurnEvent::Finished(ModelTurnResult {
                        finish_reason: FinishReason::Error,
                        output_items: Vec::new(),
                        usage: self.terminal_usage.clone(),
                        metadata: {
                            let mut m = MetadataMap::new();
                            m.insert("cerebras.error".into(), Value::String(e.to_string()));
                            m
                        },
                        model: self.model.clone(),
                        response_id: self.message_id.clone(),
                    }));
                    return events;
                }
            }
            if aggregate_finish.is_none() {
                aggregate_finish = state.finish_reason.clone();
            }
            // Assemble a per-choice Item for the terminal result.
            let mut parts: Vec<Part> = Vec::new();
            if state.reasoning_emitted && !state.reasoning_buffer.is_empty() {
                parts.push(Part::Reasoning(ReasoningPart::summary(
                    state.reasoning_buffer.clone(),
                )));
            }
            if state.content_emitted && !state.content_buffer.is_empty() {
                parts.push(Part::Text(TextPart::new(state.content_buffer.clone())));
            }
            for (_i, accum) in state.tool_calls {
                if !accum.emitted {
                    continue;
                }
                let input: Value = if accum.arguments.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str(&accum.arguments).unwrap_or(Value::Null)
                };
                let id = accum.id.unwrap_or_default();
                let name = accum.name.unwrap_or_default();
                parts.push(Part::ToolCall(ToolCallPart::new(id, name, input)));
            }
            if !parts.is_empty() {
                output_items.push(Item {
                    id: self.message_id.clone().map(Into::into),
                    kind: ItemKind::Assistant,
                    parts,
                    metadata: MetadataMap::new(),
                    usage: None,
                    finish_reason: None,
                    created_at: None,
                });
            }
        }

        events.push(ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason: aggregate_finish.unwrap_or(FinishReason::Completed),
            output_items,
            usage: self.terminal_usage.clone(),
            metadata: MetadataMap::new(),
            model: self.model.clone(),
            response_id: self.message_id.clone(),
        }));
        events
    }
}

/// Emits `CommitPart` for any open content/reasoning buffers on a single
/// choice. Idempotent — called both when `finish_reason` arrives mid-stream
/// and again at `[DONE]`.
fn commit_choice(state: &mut ChoiceState) -> Vec<ModelTurnEvent> {
    let mut out = Vec::new();
    if state.reasoning_open && !state.reasoning_emitted {
        out.push(ModelTurnEvent::Delta(Delta::CommitPart {
            part: Part::Reasoning(ReasoningPart::summary(state.reasoning_buffer.clone())),
        }));
        state.reasoning_emitted = true;
    }
    if state.content_open && !state.content_emitted {
        out.push(ModelTurnEvent::Delta(Delta::CommitPart {
            part: Part::Text(TextPart::new(state.content_buffer.clone())),
        }));
        state.content_emitted = true;
    }
    out
}

/// Emits `ToolCall` events for every accumulator whose `arguments` buffer
/// parses as JSON. Returns a `ResponseError::Protocol` if arguments fail to
/// parse — we don't want to silently swallow a malformed tool invocation.
fn flush_tool_calls(state: &mut ChoiceState) -> Result<Vec<ModelTurnEvent>, ResponseError> {
    let mut out = Vec::new();
    for accum in state.tool_calls.values_mut() {
        if accum.emitted {
            continue;
        }
        let input: Value = if accum.arguments.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(&accum.arguments).map_err(|e| {
                ResponseError::Protocol(format!(
                    "failed to parse streamed tool_call arguments: {e}"
                ))
            })?
        };
        let id = accum.id.clone().unwrap_or_default();
        let name = accum.name.clone().unwrap_or_default();
        let call = ToolCallPart::new(id, name, input);
        accum.emitted = true;
        out.push(ModelTurnEvent::ToolCall(call.clone()));
        out.push(ModelTurnEvent::Delta(Delta::CommitPart {
            part: Part::ToolCall(call),
        }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::Part;

    fn translate(stream: &str) -> Result<Vec<ModelTurnEvent>, ResponseError> {
        let mut decoder = SseDecoder::new();
        let mut translator = EventTranslator::new();
        let mut out = Vec::new();
        for event in decoder.feed(stream) {
            out.extend(translator.handle(&event)?);
        }
        Ok(out)
    }

    #[test]
    fn text_only_stream_finishes_with_usage() {
        let stream = concat!(
            "data: {\"id\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"done\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2},\"time_info\":{\"total_time\":0.1}}\n\n",
            "data: [DONE]\n\n",
        );
        let events = translate(stream).unwrap();
        let appended: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => Some(chunk.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(appended, vec!["hi", " there"]);
        let ModelTurnEvent::Finished(result) = events.last().unwrap() else {
            panic!("expected Finished");
        };
        assert_eq!(result.finish_reason, FinishReason::Completed);
        let usage = result.usage.as_ref().unwrap();
        assert!(usage.metadata.contains_key("cerebras.time_info"));
        let item = &result.output_items[0];
        match &item.parts[0] {
            Part::Text(t) => assert_eq!(t.text, "hi there"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_arguments_accumulate_across_fragments() {
        let stream = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c-1\",\"type\":\"function\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"rust\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let events = translate(stream).unwrap();
        let tool_calls: Vec<&ToolCallPart> = events
            .iter()
            .filter_map(|e| match e {
                ModelTurnEvent::ToolCall(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "search");
        assert_eq!(tool_calls[0].input, serde_json::json!({ "q": "rust" }));
    }

    #[test]
    fn reasoning_precedes_content() {
        let stream = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"reasoning\":\"think\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ans\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"done\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let events = translate(stream).unwrap();
        let kinds: Vec<PartKind> = events
            .iter()
            .filter_map(|e| match e {
                ModelTurnEvent::Delta(Delta::BeginPart { kind, .. }) => Some(*kind),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, vec![PartKind::Reasoning, PartKind::Text]);
    }

    #[test]
    fn cached_tokens_surface_on_usage() {
        let stream = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"done\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1,\"prompt_tokens_details\":{\"cached_tokens\":7}}}\n\n",
            "data: [DONE]\n\n",
        );
        let events = translate(stream).unwrap();
        let ModelTurnEvent::Finished(result) = events.last().unwrap() else {
            panic!("expected Finished");
        };
        let tokens = result.usage.as_ref().unwrap().tokens.as_ref().unwrap();
        assert_eq!(tokens.cached_input_tokens, Some(7));
    }

    #[test]
    fn prediction_and_reasoning_tokens_route_correctly() {
        let stream = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"done\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":5,\"completion_tokens_details\":{\"reasoning_tokens\":3,\"accepted_prediction_tokens\":2,\"rejected_prediction_tokens\":1}}}\n\n",
            "data: [DONE]\n\n",
        );
        let events = translate(stream).unwrap();
        let ModelTurnEvent::Finished(result) = events.last().unwrap() else {
            panic!("expected Finished");
        };
        let usage = result.usage.as_ref().unwrap();
        assert_eq!(usage.tokens.as_ref().unwrap().reasoning_tokens, Some(3));
        assert_eq!(
            usage.metadata.get("cerebras.accepted_prediction_tokens"),
            Some(&Value::from(2u64))
        );
        assert_eq!(
            usage.metadata.get("cerebras.rejected_prediction_tokens"),
            Some(&Value::from(1u64))
        );
    }

    #[test]
    fn event_error_frame_surfaces_stream_error() {
        let stream =
            "event: error\ndata: {\"error\":{\"message\":\"boom\"},\"status_code\":429}\n\n";
        let err = translate(stream).unwrap_err();
        match err {
            ResponseError::StreamError {
                message,
                status_code,
            } => {
                assert_eq!(message, "boom");
                assert_eq!(status_code, Some(429));
            }
            other => panic!("expected StreamError, got {other:?}"),
        }
    }

    #[test]
    fn unnamed_error_frame_surfaces_stream_error() {
        let stream = "data: {\"error\":{\"message\":\"oops\"}}\n\n";
        let err = translate(stream).unwrap_err();
        assert!(matches!(err, ResponseError::StreamError { .. }));
    }

    #[test]
    fn unknown_named_event_ignored() {
        let stream = concat!(
            "event: heartbeat\ndata: {}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":\"done\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let events = translate(stream).unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ModelTurnEvent::Finished(_)))
        );
    }

    #[test]
    fn malformed_data_json_returns_protocol_error() {
        let stream = "data: not-json\n\n";
        let err = translate(stream).unwrap_err();
        assert!(matches!(err, ResponseError::Protocol(_)));
    }

    #[test]
    fn all_finish_reasons_map() {
        for (wire, expected) in [
            ("stop", FinishReason::Completed),
            ("done", FinishReason::Completed),
            ("length", FinishReason::MaxTokens),
            ("tool_calls", FinishReason::ToolCall),
            ("content_filter", FinishReason::Blocked),
        ] {
            let stream = format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"x\"}},\"finish_reason\":\"{wire}\"}}]}}\n\ndata: [DONE]\n\n",
            );
            let events = translate(&stream).unwrap();
            let ModelTurnEvent::Finished(result) = events.last().unwrap() else {
                panic!("missing Finished for {wire}");
            };
            assert_eq!(result.finish_reason, expected, "mismatch on {wire}");
        }
    }
}
