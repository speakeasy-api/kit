//! OpenAI-compatible SSE chunk translator.

use std::collections::BTreeMap;
use std::sync::Arc;

use agentkit_core::{
    DataRef, Delta, FinishReason, Item, ItemKind, MediaPart, MetadataMap, Modality, Part, PartId,
    PartKind, ReasoningPart, TextPart, ToolCallPart, Usage,
};
use agentkit_loop::{ModelTurnEvent, ModelTurnResult};
use serde_json::{Value, json};

use crate::error::CompletionsError;
use crate::response;
pub(crate) use crate::sse::{SseDecoder, SseEvent};

pub(crate) type PostprocessResponse =
    Arc<dyn Fn(&mut Option<Usage>, &mut MetadataMap, &Value) + Send + Sync>;

struct ChoiceState {
    content_part_id: Option<PartId>,
    content_open: bool,
    content_emitted: bool,
    content_buffer: String,
    reasoning_part_id: Option<PartId>,
    reasoning_open: bool,
    reasoning_emitted: bool,
    reasoning_buffer: String,
    refusal_buffer: String,
    tool_calls: BTreeMap<u32, ToolCallAccum>,
    /// Generated images delivered via `delta.images[]` chunks. Each chunk carries
    /// a complete `data:image/...;base64,...` URL (the gateway convention does not
    /// stream image payloads partially), so each entry is pushed as a complete
    /// `Part::Media` and accumulates for inclusion in the finalize output_items.
    images: Vec<Part>,
    finish_reason: Option<FinishReason>,
    finish_reason_raw: Option<String>,
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
            refusal_buffer: String::new(),
            tool_calls: BTreeMap::new(),
            images: Vec::new(),
            finish_reason: None,
            finish_reason_raw: None,
        }
    }
}

struct ToolCallAccum {
    id: Option<String>,
    name: String,
    arguments: String,
    emitted: bool,
    part_id: PartId,
    open: bool,
}

impl ToolCallAccum {
    fn new(choice_index: u32, tool_index: u32) -> Self {
        Self {
            id: None,
            name: String::new(),
            arguments: String::new(),
            emitted: false,
            part_id: PartId::new(format!("part-{choice_index}-tool-{tool_index}")),
            open: false,
        }
    }
}

pub(crate) struct EventTranslator {
    choices: BTreeMap<u32, ChoiceState>,
    terminal_usage: Option<Usage>,
    terminal_usage_raw: Option<Value>,
    message_id: Option<String>,
    model: Option<String>,
    system_fingerprint: Option<String>,
    finished: bool,
}

impl EventTranslator {
    pub(crate) fn new() -> Self {
        Self {
            choices: BTreeMap::new(),
            terminal_usage: None,
            terminal_usage_raw: None,
            message_id: None,
            model: None,
            system_fingerprint: None,
            finished: false,
        }
    }

    pub(crate) fn handle(
        &mut self,
        event: &SseEvent,
        postprocess: &PostprocessResponse,
    ) -> Result<Vec<ModelTurnEvent>, CompletionsError> {
        if self.finished {
            return Ok(Vec::new());
        }
        if event.name.as_deref() == Some("error") {
            return Err(parse_error_frame(&event.data));
        }
        if event.name.is_some() && event.name.as_deref() != Some("error") {
            return Ok(Vec::new());
        }
        if event.data.trim() == "[DONE]" {
            return self.finalize(postprocess);
        }

        let json: Value = serde_json::from_str(&event.data)
            .map_err(|e| CompletionsError::Protocol(format!("invalid SSE JSON: {e}")))?;

        if let Some(error) = json.get("error") {
            return Err(parse_error_value(error, json.get("status_code")));
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
        if let Some(fingerprint) = json.get("system_fingerprint").and_then(Value::as_str) {
            self.system_fingerprint = Some(fingerprint.to_string());
        }
        if let Some(raw_usage) = json.get("usage").filter(|usage| !usage.is_null()) {
            self.terminal_usage_raw = Some(raw_usage.clone());
            let parsed = serde_json::from_value(raw_usage.clone())
                .ok()
                .and_then(|usage| response::map_usage(Some(usage)));
            if parsed.is_some() {
                self.terminal_usage = parsed;
            }
        }

        let mut out = Vec::new();
        let Some(choices) = json.get("choices").and_then(Value::as_array) else {
            return Ok(out);
        };
        for choice in choices {
            let index = choice.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
            let state = self.choices.entry(index).or_insert_with(ChoiceState::new);
            let delta = choice.get("delta").unwrap_or(&Value::Null);

            if let Some(reasoning) = delta
                .get("reasoning")
                .or_else(|| delta.get("reasoning_content"))
                .and_then(Value::as_str)
                && !reasoning.is_empty()
            {
                if !state.reasoning_open {
                    state.reasoning_open = true;
                    let part_id = PartId::new(format!("part-{index}-reasoning"));
                    state.reasoning_part_id = Some(part_id.clone());
                    out.push(ModelTurnEvent::Delta(Delta::BeginPart {
                        part_id,
                        kind: PartKind::Reasoning,
                    }));
                }
                state.reasoning_buffer.push_str(reasoning);
                if let Some(part_id) = &state.reasoning_part_id {
                    out.push(ModelTurnEvent::Delta(Delta::AppendText {
                        part_id: part_id.clone(),
                        chunk: reasoning.to_string(),
                    }));
                }
            }

            if let Some(content) = delta.get("content").and_then(Value::as_str)
                && !content.is_empty()
            {
                if !state.content_open {
                    state.content_open = true;
                    let part_id = PartId::new(format!("part-{index}-content"));
                    state.content_part_id = Some(part_id.clone());
                    out.push(ModelTurnEvent::Delta(Delta::BeginPart {
                        part_id,
                        kind: PartKind::Text,
                    }));
                }
                state.content_buffer.push_str(content);
                if let Some(part_id) = &state.content_part_id {
                    out.push(ModelTurnEvent::Delta(Delta::AppendText {
                        part_id: part_id.clone(),
                        chunk: content.to_string(),
                    }));
                }
            }

            if let Some(refusal) = delta.get("refusal").and_then(Value::as_str) {
                state.refusal_buffer.push_str(refusal);
            }

            // De facto gateway convention for image output (see ResponseMessage::images
            // in response.rs). Each chunk carries a complete image_url; emit Begin+Commit
            // back-to-back since there's nothing to stream incrementally.
            if let Some(images) = delta.get("images").and_then(Value::as_array) {
                for image in images {
                    let Some(url) = image.pointer("/image_url/url").and_then(Value::as_str) else {
                        continue;
                    };
                    let image_index = state.images.len();
                    let part_id = PartId::new(format!("part-{index}-image-{image_index}"));
                    let part = Part::Media(MediaPart {
                        modality: Modality::Image,
                        mime_type: "image/*".into(),
                        data: DataRef::Uri(url.to_string()),
                        metadata: MetadataMap::new(),
                    });
                    out.push(ModelTurnEvent::Delta(Delta::BeginPart {
                        part_id,
                        kind: PartKind::Media,
                    }));
                    out.push(ModelTurnEvent::Delta(Delta::CommitPart {
                        part: part.clone(),
                    }));
                    state.images.push(part);
                }
            }

            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    let tool_index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as u32;
                    let accum = state
                        .tool_calls
                        .entry(tool_index)
                        .or_insert_with(|| ToolCallAccum::new(index, tool_index));
                    if !accum.open {
                        accum.open = true;
                        out.push(ModelTurnEvent::Delta(Delta::BeginPart {
                            part_id: accum.part_id.clone(),
                            kind: PartKind::ToolCall,
                        }));
                    }
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        accum.id = Some(id.to_string());
                    }
                    if let Some(function) = call.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            accum.name.push_str(name);
                        }
                        if let Some(arguments) = function.get("arguments").and_then(Value::as_str)
                            && !arguments.is_empty()
                        {
                            accum.arguments.push_str(arguments);
                            out.push(ModelTurnEvent::Delta(Delta::AppendText {
                                part_id: accum.part_id.clone(),
                                chunk: arguments.to_string(),
                            }));
                        }
                    }
                }
            }

            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                state.finish_reason_raw = Some(reason.to_string());
                state.finish_reason = Some(response::map_finish_reason(Some(reason)));
                out.extend(commit_choice(state));
                out.extend(flush_tool_calls(state)?);
            }
        }

        Ok(out)
    }

    pub(crate) fn is_done(&self) -> bool {
        self.finished
    }

    fn finalize(
        &mut self,
        postprocess: &PostprocessResponse,
    ) -> Result<Vec<ModelTurnEvent>, CompletionsError> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;

        let mut events = Vec::new();
        let mut output_items = Vec::new();
        let mut aggregate_finish: Option<FinishReason> = None;
        let mut raw_choices = Vec::new();

        for (index, mut state) in std::mem::take(&mut self.choices) {
            events.extend(commit_choice(&mut state));
            events.extend(flush_tool_calls(&mut state)?);
            if aggregate_finish.is_none() {
                aggregate_finish = state.finish_reason.clone();
            }
            let mut parts = Vec::new();
            if state.reasoning_emitted && !state.reasoning_buffer.is_empty() {
                parts.push(Part::Reasoning(ReasoningPart::summary(
                    state.reasoning_buffer.clone(),
                )));
            }
            if state.content_emitted && !state.content_buffer.is_empty() {
                parts.push(Part::Text(TextPart::new(state.content_buffer.clone())));
            }
            parts.extend(state.images.iter().cloned());
            let mut raw_tool_calls = Vec::new();
            for (tool_index, accum) in state.tool_calls {
                if !accum.emitted {
                    continue;
                }
                let id = accum
                    .id
                    .unwrap_or_else(|| format!("call-{index}-{tool_index}"));
                let call = ToolCallPart::new(
                    id.clone(),
                    accum.name.clone(),
                    response::parse_tool_arguments(&accum.arguments)?,
                );
                parts.push(Part::ToolCall(call));
                raw_tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": accum.name,
                        "arguments": accum.arguments,
                    }
                }));
            }

            let mut raw_message = serde_json::Map::new();
            raw_message.insert("role".into(), Value::String("assistant".into()));
            raw_message.insert(
                "content".into(),
                Value::String(state.content_buffer.clone()),
            );
            if !state.reasoning_buffer.is_empty() {
                raw_message.insert("reasoning".into(), Value::String(state.reasoning_buffer));
            }
            if !state.refusal_buffer.is_empty() {
                raw_message.insert("refusal".into(), Value::String(state.refusal_buffer));
            }
            if !state.images.is_empty() {
                let images_raw: Vec<Value> = state
                    .images
                    .iter()
                    .filter_map(|p| match p {
                        Part::Media(media) => match &media.data {
                            DataRef::Uri(url) => Some(json!({
                                "type": "image_url",
                                "image_url": { "url": url },
                            })),
                            _ => None,
                        },
                        _ => None,
                    })
                    .collect();
                if !images_raw.is_empty() {
                    raw_message.insert("images".into(), Value::Array(images_raw));
                }
            }
            if !raw_tool_calls.is_empty() {
                raw_message.insert("tool_calls".into(), Value::Array(raw_tool_calls));
            }
            raw_choices.push(json!({
                "index": index,
                "finish_reason": state.finish_reason_raw,
                "message": Value::Object(raw_message),
            }));

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

        let raw = self.synthetic_raw_response(raw_choices);
        let mut usage = self.terminal_usage.clone();
        let mut metadata = MetadataMap::new();
        postprocess(&mut usage, &mut metadata, &raw);

        if let Some(usage) = usage.clone() {
            events.push(ModelTurnEvent::Usage(usage.clone()));
        }
        for item in &mut output_items {
            item.metadata = metadata.clone();
        }
        events.push(ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason: aggregate_finish.unwrap_or(FinishReason::Completed),
            output_items,
            usage,
            metadata: MetadataMap::new(),
            model: self.model.clone(),
            response_id: self.message_id.clone(),
        }));
        Ok(events)
    }

    fn synthetic_raw_response(&self, choices: Vec<Value>) -> Value {
        let mut raw = serde_json::Map::new();
        if let Some(id) = &self.message_id {
            raw.insert("id".into(), Value::String(id.clone()));
        }
        if let Some(model) = &self.model {
            raw.insert("model".into(), Value::String(model.clone()));
        }
        if let Some(fingerprint) = &self.system_fingerprint {
            raw.insert(
                "system_fingerprint".into(),
                Value::String(fingerprint.clone()),
            );
        }
        raw.insert("choices".into(), Value::Array(choices));
        if let Some(usage) = &self.terminal_usage_raw {
            raw.insert("usage".into(), usage.clone());
        }
        Value::Object(raw)
    }
}

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

fn flush_tool_calls(state: &mut ChoiceState) -> Result<Vec<ModelTurnEvent>, CompletionsError> {
    let mut out = Vec::new();
    for accum in state.tool_calls.values_mut() {
        if accum.emitted {
            continue;
        }
        let id = accum.id.clone().unwrap_or_default();
        let input = response::parse_tool_arguments(if accum.arguments.trim().is_empty() {
            "{}"
        } else {
            &accum.arguments
        })?;
        let call = ToolCallPart::new(id, accum.name.clone(), input);
        accum.emitted = true;
        out.push(ModelTurnEvent::ToolCall(call.clone()));
        out.push(ModelTurnEvent::Delta(Delta::CommitPart {
            part: Part::ToolCall(call),
        }));
    }
    Ok(out)
}

fn parse_error_frame(data: &str) -> CompletionsError {
    let Ok(json): Result<Value, _> = serde_json::from_str(data) else {
        return CompletionsError::Protocol(format!("malformed error frame: {data}"));
    };
    parse_error_value(json.get("error").unwrap_or(&json), json.get("status_code"))
}

fn parse_error_value(error: &Value, status_code: Option<&Value>) -> CompletionsError {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("unknown stream error");
    let status = status_code.and_then(Value::as_u64);
    match status {
        Some(status) => CompletionsError::Protocol(format!("stream error ({status}): {message}")),
        None => CompletionsError::Protocol(format!("stream error: {message}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn postprocess_noop() -> PostprocessResponse {
        Arc::new(|_, _, _| {})
    }

    fn translate(stream: &str) -> Result<Vec<ModelTurnEvent>, CompletionsError> {
        let mut decoder = SseDecoder::new();
        let mut translator = EventTranslator::new();
        let postprocess = postprocess_noop();
        let mut out = Vec::new();
        for event in decoder.feed(stream) {
            out.extend(translator.handle(&event, &postprocess)?);
        }
        Ok(out)
    }

    #[test]
    fn text_stream_emits_append_and_finished() {
        let stream = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let events = translate(stream).unwrap();
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => Some(chunk.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hi there");
        assert!(matches!(
            events.last(),
            Some(ModelTurnEvent::Finished(ModelTurnResult {
                finish_reason: FinishReason::Completed,
                ..
            }))
        ));
    }

    #[test]
    fn tool_call_arguments_accumulate() {
        let stream = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"rust\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let events = translate(stream).unwrap();
        let call = events.iter().find_map(|event| match event {
            ModelTurnEvent::ToolCall(call) => Some(call),
            _ => None,
        });
        assert_eq!(call.unwrap().input["q"], "rust");
    }

    #[test]
    fn delta_images_emit_media_parts_and_finalize() {
        // Mirrors the streaming shape documented by Vercel AI Gateway / OpenRouter
        // for image-output models (Nano Banana family, GPT-5 image variants):
        // images arrive on `delta.images[]` rather than `delta.content`.
        let stream = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"images\":[{\"type\":\"image_url\",\"image_url\":{\"url\":\"data:image/png;base64,AAA\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let events = translate(stream).unwrap();

        let begin_count = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    ModelTurnEvent::Delta(Delta::BeginPart {
                        kind: PartKind::Media,
                        ..
                    })
                )
            })
            .count();
        assert_eq!(begin_count, 1, "expected one BeginPart for the image");

        let media_commit = events.iter().find_map(|e| match e {
            ModelTurnEvent::Delta(Delta::CommitPart {
                part: Part::Media(media),
            }) => Some(media),
            _ => None,
        });
        let media = media_commit.expect("CommitPart for media");
        assert_eq!(media.modality, Modality::Image);
        match &media.data {
            DataRef::Uri(url) => {
                assert!(url.starts_with("data:image/png;base64,"));
            }
            other => panic!("expected DataRef::Uri, got {other:?}"),
        }

        let finished = events.iter().rev().find_map(|e| match e {
            ModelTurnEvent::Finished(result) => Some(result),
            _ => None,
        });
        let finished = finished.expect("Finished event");
        let assistant = finished
            .output_items
            .iter()
            .find(|i| i.kind == ItemKind::Assistant)
            .expect("assistant item in output_items");
        let has_media = assistant
            .parts
            .iter()
            .any(|p| matches!(p, Part::Media(m) if m.modality == Modality::Image));
        assert!(has_media, "assistant output_items should carry the image");
    }

    #[test]
    fn postprocess_receives_synthetic_raw_response() {
        let stream = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"cost\":0.01}}\n\n",
            "data: [DONE]\n\n",
        );
        let mut decoder = SseDecoder::new();
        let mut translator = EventTranslator::new();
        let postprocess: PostprocessResponse = Arc::new(|usage, metadata, raw| {
            assert_eq!(raw["usage"]["cost"], 0.01);
            if let Some(usage) = usage {
                usage
                    .metadata
                    .insert("seen".into(), Value::Bool(raw.get("id").is_some()));
            }
            metadata.insert("model".into(), raw["model"].clone());
        });
        let mut events = Vec::new();
        for event in decoder.feed(stream) {
            events.extend(translator.handle(&event, &postprocess).unwrap());
        }
        let finished = events.iter().find_map(|event| match event {
            ModelTurnEvent::Finished(result) => Some(result),
            _ => None,
        });
        let item = &finished.unwrap().output_items[0];
        assert_eq!(item.metadata["model"], "m");
    }
}
