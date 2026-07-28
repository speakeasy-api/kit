//! Anthropic SSE-event-to-`ModelTurnEvent` translator.
//!
//! The sibling [`crate::sse`] module turns a chunked response body into
//! `(event_name, data)` records; this module is the stateful translator that
//! consumes those records and emits agentkit-native [`ModelTurnEvent`]s with
//! incremental [`Delta`] payloads. Re-exports [`SseDecoder`] for convenience
//! so the streaming adapter only needs one import.
//!
//! The translator does no I/O — it's driven from the consumer side by the
//! streaming `AnthropicTurn` implementation in the crate root.

use std::collections::BTreeMap;

use agentkit_core::{
    CustomPart, Delta, FinishReason, Item, ItemKind, MetadataMap, Part, PartId, PartKind,
    ReasoningPart, TextPart, TokenUsage, ToolCallPart, Usage,
};
use agentkit_loop::{LoopError, ModelTurnEvent, ModelTurnResult};
use serde_json::{Value, json};

pub(crate) use crate::sse::{SseDecoder, SseEvent};

/// State tracked per `content_block` index while a block is in flight.
#[derive(Debug)]
enum BlockState {
    Text {
        part_id: PartId,
        buffer: String,
    },
    Reasoning {
        part_id: PartId,
        buffer: String,
        signature: String,
    },
    RedactedReasoning {
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        partial_input: String,
    },
    /// Server-tool or other block types we round-trip as opaque parts.
    Other {
        kind: String,
        raw: Value,
    },
}

/// Stateful SSE-event-to-`ModelTurnEvent` translator.
///
/// One instance handles a single Messages API turn. Feed it each decoded
/// [`SseEvent`] via [`EventTranslator::handle`]; the returned vector contains
/// the translated `ModelTurnEvent`s (zero, one, or many per SSE event).
pub(crate) struct EventTranslator {
    blocks: BTreeMap<usize, BlockState>,
    /// Parts committed so far, in content-block-index order. Used to assemble
    /// the final `Item` emitted with the `Finished` event.
    committed_parts: Vec<(usize, Part)>,
    /// Accumulated usage. Anthropic sends `input_tokens` in `message_start`
    /// and progressively updates `output_tokens` in `message_delta`.
    usage: Option<Usage>,
    /// Message-level metadata harvested from `message_start` / `message_delta`.
    metadata: MetadataMap,
    /// Message id from `message_start`.
    message_id: Option<String>,
    /// Model identifier from `message_start`.
    model: Option<String>,
    /// Final stop reason from `message_delta`.
    stop_reason: Option<String>,
    /// True once a terminal event has been emitted; further SSE events are
    /// ignored defensively.
    finished: bool,
}

impl EventTranslator {
    pub(crate) fn new() -> Self {
        Self {
            blocks: BTreeMap::new(),
            committed_parts: Vec::new(),
            usage: None,
            metadata: MetadataMap::new(),
            message_id: None,
            model: None,
            stop_reason: None,
            finished: false,
        }
    }

    /// Translates a single SSE event into zero or more `ModelTurnEvent`s.
    ///
    /// Protocol errors (malformed JSON, missing required fields) are surfaced
    /// as [`LoopError::Provider`]. Known event kinds we don't care about
    /// (`ping`, unknown names) silently yield an empty vector.
    pub(crate) fn handle(&mut self, event: &SseEvent) -> Result<Vec<ModelTurnEvent>, LoopError> {
        if self.finished {
            return Ok(Vec::new());
        }
        match event.name.as_str() {
            "message_start" => self.on_message_start(&event.data),
            "content_block_start" => self.on_block_start(&event.data),
            "content_block_delta" => self.on_block_delta(&event.data),
            "content_block_stop" => self.on_block_stop(&event.data),
            "message_delta" => self.on_message_delta(&event.data),
            "message_stop" => self.on_message_stop(),
            "error" => Err(LoopError::Provider(format!(
                "Anthropic stream error: {}",
                event.data
            ))),
            "ping" | "" => Ok(Vec::new()),
            _ => Ok(Vec::new()),
        }
    }

    /// Returns true once the translator has emitted its terminal `Finished`
    /// event and no further events should be dispatched.
    pub(crate) fn is_done(&self) -> bool {
        self.finished
    }

    // --- individual event handlers ---

    fn on_message_start(&mut self, data: &str) -> Result<Vec<ModelTurnEvent>, LoopError> {
        let value: Value = parse_json(data)?;
        let message = value
            .get("message")
            .ok_or_else(|| protocol("message_start missing message"))?;
        if let Some(id) = message.get("id").and_then(Value::as_str) {
            self.message_id = Some(id.to_string());
        }
        if let Some(model) = message.get("model").and_then(Value::as_str) {
            self.model = Some(model.to_string());
            self.metadata
                .insert("anthropic.model".into(), Value::String(model.into()));
        }
        let usage = parse_usage(message.get("usage"));
        self.usage = usage.clone();
        let mut events = Vec::new();
        if let Some(usage) = usage {
            events.push(ModelTurnEvent::Usage(usage));
        }
        Ok(events)
    }

    fn on_block_start(&mut self, data: &str) -> Result<Vec<ModelTurnEvent>, LoopError> {
        let value: Value = parse_json(data)?;
        let index = value
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| protocol("content_block_start missing index"))?
            as usize;
        let block = value
            .get("content_block")
            .ok_or_else(|| protocol("content_block_start missing content_block"))?;
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol("content_block_start missing type"))?;
        let part_id = PartId::new(format!("part-{index}"));

        let (state, part_kind) = match kind {
            "text" => {
                let initial = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                (
                    BlockState::Text {
                        part_id: part_id.clone(),
                        buffer: initial,
                    },
                    PartKind::Text,
                )
            }
            "thinking" => {
                let initial = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let signature = block
                    .get("signature")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                (
                    BlockState::Reasoning {
                        part_id: part_id.clone(),
                        buffer: initial,
                        signature,
                    },
                    PartKind::Reasoning,
                )
            }
            "redacted_thinking" => {
                let data = block
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                (BlockState::RedactedReasoning { data }, PartKind::Reasoning)
            }
            "tool_use" => {
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| protocol("tool_use block missing id"))?
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| protocol("tool_use block missing name"))?
                    .to_string();
                // The start event carries an empty `input: {}` placeholder;
                // the real payload arrives as `input_json_delta` chunks we
                // accumulate into `partial_input` and parse at block stop.
                (
                    BlockState::ToolUse {
                        id,
                        name,
                        partial_input: String::new(),
                    },
                    PartKind::ToolCall,
                )
            }
            other => (
                BlockState::Other {
                    kind: other.to_string(),
                    raw: block.clone(),
                },
                PartKind::Custom,
            ),
        };

        self.blocks.insert(index, state);
        Ok(vec![ModelTurnEvent::Delta(Delta::BeginPart {
            part_id,
            kind: part_kind,
        })])
    }

    fn on_block_delta(&mut self, data: &str) -> Result<Vec<ModelTurnEvent>, LoopError> {
        let value: Value = parse_json(data)?;
        let index = value
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| protocol("content_block_delta missing index"))?
            as usize;
        let delta = value
            .get("delta")
            .ok_or_else(|| protocol("content_block_delta missing delta"))?;
        let delta_type = delta
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| protocol("content_block_delta missing delta.type"))?;

        let state = self
            .blocks
            .get_mut(&index)
            .ok_or_else(|| protocol("content_block_delta for unknown index"))?;

        let mut out = Vec::new();
        match (delta_type, state) {
            ("text_delta", BlockState::Text { part_id, buffer }) => {
                let chunk = delta
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !chunk.is_empty() {
                    buffer.push_str(chunk);
                    out.push(ModelTurnEvent::Delta(Delta::AppendText {
                        part_id: part_id.clone(),
                        chunk: chunk.to_string(),
                    }));
                }
            }
            (
                "thinking_delta",
                BlockState::Reasoning {
                    part_id, buffer, ..
                },
            ) => {
                let chunk = delta
                    .get("thinking")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !chunk.is_empty() {
                    buffer.push_str(chunk);
                    out.push(ModelTurnEvent::Delta(Delta::AppendText {
                        part_id: part_id.clone(),
                        chunk: chunk.to_string(),
                    }));
                }
            }
            ("signature_delta", BlockState::Reasoning { signature, .. }) => {
                // Signatures are accumulated and attached at content_block_stop
                // so the committed ReasoningPart round-trips correctly.
                if let Some(chunk) = delta.get("signature").and_then(Value::as_str) {
                    signature.push_str(chunk);
                }
            }
            ("input_json_delta", BlockState::ToolUse { partial_input, .. }) => {
                // Anthropic streams tool input as chunked JSON text; we
                // accumulate and parse at content_block_stop.
                if let Some(chunk) = delta.get("partial_json").and_then(Value::as_str) {
                    partial_input.push_str(chunk);
                }
            }
            ("citations_delta", _) => {
                // Citations are attached to text blocks as metadata; we
                // currently drop them silently rather than invent an agentkit
                // representation. They're also reflected in the final Item's
                // block structure if the provider includes them there.
            }
            (other, state) => {
                // Defensive: unrecognized delta types on a known block we
                // stash on Other/Custom blocks verbatim.
                if let BlockState::Other { raw, .. } = state {
                    merge_unknown_delta(raw, other, delta);
                }
            }
        }
        Ok(out)
    }

    fn on_block_stop(&mut self, data: &str) -> Result<Vec<ModelTurnEvent>, LoopError> {
        let value: Value = parse_json(data)?;
        let index = value
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| protocol("content_block_stop missing index"))?
            as usize;
        let Some(state) = self.blocks.remove(&index) else {
            return Ok(Vec::new());
        };

        let mut events = Vec::new();
        let part = match state {
            BlockState::Text { buffer, .. } => Part::Text(TextPart::new(buffer)),
            BlockState::Reasoning {
                buffer, signature, ..
            } => {
                let mut meta = MetadataMap::new();
                meta.insert(
                    "anthropic.thinking_signature".into(),
                    Value::String(signature),
                );
                Part::Reasoning(ReasoningPart::summary(buffer).with_metadata(meta))
            }
            BlockState::RedactedReasoning { data } => {
                let mut meta = MetadataMap::new();
                meta.insert("anthropic.redacted_data".into(), Value::String(data));
                Part::Reasoning(ReasoningPart {
                    summary: None,
                    data: None,
                    redacted: true,
                    metadata: meta,
                })
            }
            BlockState::ToolUse {
                id,
                name,
                partial_input,
            } => {
                // If no input deltas arrived the partial buffer is empty; the
                // Anthropic API treats `{}` as the equivalent empty-object
                // default, so parse that instead of erroring out.
                let input = if partial_input.trim().is_empty() {
                    Value::Object(Default::default())
                } else {
                    serde_json::from_str::<Value>(&partial_input).map_err(|e| {
                        LoopError::Provider(format!(
                            "failed to parse streamed tool_use input JSON: {e}"
                        ))
                    })?
                };
                let call = ToolCallPart::new(id, name, input);
                events.push(ModelTurnEvent::ToolCall(call.clone()));
                Part::ToolCall(call)
            }
            BlockState::Other { kind, raw } => {
                Part::Custom(CustomPart::new(format!("anthropic.{kind}")).with_value(raw))
            }
        };
        self.committed_parts.push((index, part.clone()));
        events.push(ModelTurnEvent::Delta(Delta::CommitPart { part }));
        Ok(events)
    }

    fn on_message_delta(&mut self, data: &str) -> Result<Vec<ModelTurnEvent>, LoopError> {
        let value: Value = parse_json(data)?;
        if let Some(delta) = value.get("delta") {
            if let Some(reason) = delta.get("stop_reason").and_then(Value::as_str) {
                self.stop_reason = Some(reason.to_string());
                self.metadata
                    .insert("anthropic.stop_reason".into(), Value::String(reason.into()));
            }
            if let Some(seq) = delta.get("stop_sequence").and_then(Value::as_str) {
                self.metadata
                    .insert("anthropic.stop_sequence".into(), Value::String(seq.into()));
            }
        }
        if let Some(usage) = parse_usage(value.get("usage")) {
            let merged = merge_usage(self.usage.take(), usage);
            self.usage = Some(merged.clone());
            return Ok(vec![ModelTurnEvent::Usage(merged)]);
        }
        Ok(Vec::new())
    }

    fn on_message_stop(&mut self) -> Result<Vec<ModelTurnEvent>, LoopError> {
        self.finished = true;
        let finish_reason = map_stop_reason(self.stop_reason.as_deref());
        // Sort committed parts by their content-block index to preserve the
        // author-intended ordering (thinking-before-text-before-tools).
        self.committed_parts.sort_by_key(|(idx, _)| *idx);
        let parts: Vec<Part> = self.committed_parts.drain(..).map(|(_, p)| p).collect();

        let response_id = self.message_id.take();
        let output_items = if parts.is_empty() {
            Vec::new()
        } else {
            vec![Item {
                id: response_id.clone().map(Into::into),
                kind: ItemKind::Assistant,
                parts,
                metadata: std::mem::take(&mut self.metadata),
                usage: None,
                finish_reason: None,
                created_at: None,
            }]
        };

        Ok(vec![ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason,
            output_items,
            usage: self.usage.clone(),
            metadata: MetadataMap::new(),
            model: self.model.take(),
            response_id,
        })])
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_json(data: &str) -> Result<Value, LoopError> {
    serde_json::from_str(data)
        .map_err(|e| LoopError::Provider(format!("invalid Anthropic SSE JSON: {e}")))
}

fn protocol(msg: &str) -> LoopError {
    LoopError::Provider(format!("Anthropic SSE protocol error: {msg}"))
}

fn merge_unknown_delta(raw: &mut Value, delta_type: &str, delta: &Value) {
    let obj = match raw.as_object_mut() {
        Some(o) => o,
        None => return,
    };
    // Collect unknown deltas into an `agentkit.deltas` array so the
    // round-tripped Custom part at least carries them.
    let entry = obj
        .entry("agentkit.deltas")
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(arr) = entry.as_array_mut() {
        arr.push(json!({ "type": delta_type, "delta": delta.clone() }));
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

/// Merges a later usage report into an earlier one. Anthropic reports
/// `input_tokens` on `message_start` and updates `output_tokens` on
/// `message_delta`; the merged record should reflect the newest numbers for
/// each direction while retaining whichever fields weren't re-sent.
fn merge_usage(prev: Option<Usage>, next: Usage) -> Usage {
    let Some(prev) = prev else {
        return next;
    };
    let tokens = match (prev.tokens, next.tokens) {
        (Some(a), Some(b)) => Some(TokenUsage {
            input_tokens: if b.input_tokens > 0 {
                b.input_tokens
            } else {
                a.input_tokens
            },
            output_tokens: if b.output_tokens > 0 {
                b.output_tokens
            } else {
                a.output_tokens
            },
            reasoning_tokens: b.reasoning_tokens.or(a.reasoning_tokens),
            cached_input_tokens: b.cached_input_tokens.or(a.cached_input_tokens),
            cache_write_input_tokens: b.cache_write_input_tokens.or(a.cache_write_input_tokens),
        }),
        (a, b) => b.or(a),
    };
    let mut metadata = prev.metadata;
    for (k, v) in next.metadata {
        metadata.insert(k, v);
    }
    Usage {
        tokens,
        cost: next.cost.or(prev.cost),
        metadata,
    }
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(stream: &str) -> Vec<ModelTurnEvent> {
        let mut decoder = SseDecoder::new();
        let mut translator = EventTranslator::new();
        let mut out = Vec::new();
        for event in decoder.feed(stream) {
            out.extend(translator.handle(&event).expect("translation failed"));
        }
        out
    }

    #[test]
    fn translates_text_turn_end_to_end() {
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-opus-4-7\",\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\", world\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let events = translate(stream);

        // Expected: Usage, BeginPart, AppendText, AppendText, CommitPart, Usage, Finished
        assert!(matches!(events[0], ModelTurnEvent::Usage(_)));
        match &events[1] {
            ModelTurnEvent::Delta(Delta::BeginPart { part_id, kind }) => {
                assert_eq!(part_id.0, "part-0");
                assert_eq!(*kind, PartKind::Text);
            }
            other => panic!("expected BeginPart, got {other:?}"),
        }
        let appended: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => Some(chunk.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(appended, vec!["Hello", ", world"]);

        let finished = events.last().expect("at least one event");
        let ModelTurnEvent::Finished(result) = finished else {
            panic!("last event should be Finished, got {finished:?}");
        };
        assert_eq!(result.finish_reason, FinishReason::Completed);
        assert_eq!(result.output_items.len(), 1);
        let item = &result.output_items[0];
        match &item.parts[0] {
            Part::Text(text) => assert_eq!(text.text, "Hello, world"),
            other => panic!("expected text, got {other:?}"),
        }
        // Final usage should carry merged tokens.
        let tokens = result.usage.as_ref().unwrap().tokens.as_ref().unwrap();
        assert_eq!(tokens.input_tokens, 10);
        assert_eq!(tokens.output_tokens, 5);
    }

    #[test]
    fn translates_thinking_and_tool_use() {
        let stream = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"model\":\"claude-opus-4-7\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"let me\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\" think\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"abc\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool-1\",\"name\":\"search\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"q\\\":\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"rust\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"index\":1}\n\n",
            "event: message_delta\n",
            "data: {\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":20}}\n\n",
            "event: message_stop\n",
            "data: {}\n\n",
        );
        let events = translate(stream);

        // Thinking part committed with signature metadata.
        let reasoning_commit = events
            .iter()
            .find_map(|e| match e {
                ModelTurnEvent::Delta(Delta::CommitPart {
                    part: Part::Reasoning(r),
                }) => Some(r),
                _ => None,
            })
            .expect("reasoning part committed");
        assert_eq!(reasoning_commit.summary.as_deref(), Some("let me think"));
        assert_eq!(
            reasoning_commit.metadata["anthropic.thinking_signature"],
            "sig-abc"
        );

        // Exactly one tool-call event emitted.
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

        // Finished carries both parts with tool-use stop reason.
        let ModelTurnEvent::Finished(result) = events.last().unwrap() else {
            panic!("missing Finished");
        };
        assert_eq!(result.finish_reason, FinishReason::ToolCall);
        let item = &result.output_items[0];
        assert_eq!(item.parts.len(), 2);
        assert!(matches!(item.parts[0], Part::Reasoning(_)));
        assert!(matches!(item.parts[1], Part::ToolCall(_)));
    }

    #[test]
    fn server_tool_block_round_trips_as_custom() {
        let stream = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"id\":\"msg_3\",\"model\":\"claude-opus-4-7\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\n",
            "data: {\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"s-1\",\"name\":\"web_search\",\"input\":{}}}\n\n",
            "event: content_block_stop\n",
            "data: {\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"delta\":{\"stop_reason\":\"end_turn\"}}\n\n",
            "event: message_stop\n",
            "data: {}\n\n",
        );
        let events = translate(stream);
        let ModelTurnEvent::Finished(result) = events.last().unwrap() else {
            panic!("missing Finished");
        };
        match &result.output_items[0].parts[0] {
            Part::Custom(c) => assert_eq!(c.kind, "anthropic.server_tool_use"),
            other => panic!("expected custom, got {other:?}"),
        }
    }

    #[test]
    fn error_event_becomes_provider_error() {
        let mut decoder = SseDecoder::new();
        let mut translator = EventTranslator::new();
        let events = decoder.feed("event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"slow down\"}}\n\n");
        assert_eq!(events.len(), 1);
        let err = translator.handle(&events[0]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("overloaded_error"), "got {msg}");
    }

    #[test]
    fn ping_and_unknown_events_are_ignored() {
        let mut decoder = SseDecoder::new();
        let mut translator = EventTranslator::new();
        for sse in decoder.feed("event: ping\ndata: {}\n\nevent: novel_event\ndata: {}\n\n") {
            let out = translator.handle(&sse).unwrap();
            assert!(out.is_empty());
        }
    }

    #[test]
    fn tool_use_with_empty_input_parses_as_empty_object() {
        let stream = concat!(
            "event: message_start\n",
            "data: {\"message\":{\"id\":\"msg_4\",\"model\":\"m\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool-7\",\"name\":\"noop\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"index\":0}\n\n",
            "event: message_delta\n",
            "data: {\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\n",
            "data: {}\n\n",
        );
        let events = translate(stream);
        let tool_call = events
            .iter()
            .find_map(|e| match e {
                ModelTurnEvent::ToolCall(c) => Some(c),
                _ => None,
            })
            .unwrap();
        assert_eq!(tool_call.input, serde_json::json!({}));
    }

    #[test]
    fn finished_is_idempotent_after_message_stop() {
        let mut dec = SseDecoder::new();
        let mut tr = EventTranslator::new();
        for e in dec.feed("event: message_stop\ndata: {}\n\n") {
            tr.handle(&e).unwrap();
        }
        assert!(tr.is_done());
        // Further events after terminal stop are silently dropped.
        for e in dec.feed("event: content_block_start\ndata: {\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n") {
            let out = tr.handle(&e).unwrap();
            assert!(out.is_empty());
        }
    }
}
