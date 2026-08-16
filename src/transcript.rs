//! Repair for stored transcripts that lost a tool result.
//!
//! The loop answers every call it abandons, so a transcript Kit writes is
//! sound when it leaves memory. Nothing defends it after that: a session file
//! is plain JSONL a user can edit or truncate, a disk can lose the tail of a
//! write, and either leaves a tool call with no result. Providers reject that
//! history outright — OpenAI answers "No tool output found for function call
//! ..." — so the session is stranded for good unless the pair is completed.
//! Nothing about that is provider-specific: the agent loop enforces the same
//! invariant on every transcript mutation.
//!
//! The missing half is reconstructed rather than dropped: the call is part of
//! what the model already said, and removing it would rewrite history the
//! model can see. A result recording the absent outcome is the truthful pair.

use std::collections::HashSet;

use agentkit_core::{
    Item, ItemKind, MetadataMap, Part, Timestamp, ToolCallId, ToolOutput, ToolResultPart,
};
use serde_json::Value;

const MISSING: &str = "No result for this tool call survived in the stored transcript. The work \
                       it started may or may not have completed; check before assuming either \
                       way.";

/// Answers every tool call the stored transcript left open, in place.
///
/// Each synthesized result is inserted directly after the item holding its
/// call, since a result may not precede the call it answers. The synthesized
/// items are returned in transcript order so a caller holding the transcript's
/// write lock can persist them.
pub fn repair_unanswered_tool_calls(transcript: &mut Vec<Item>) -> Vec<Item> {
    if !has_unanswered_tool_calls(transcript) {
        return Vec::new();
    }
    let answered = answered_calls(transcript)
        .into_iter()
        .map(str::to_owned)
        .collect::<HashSet<_>>();
    let mut synthesized = Vec::new();
    let mut repaired = Vec::with_capacity(transcript.len() + 1);
    for item in transcript.drain(..) {
        let missing = item
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::ToolCall(call) if !answered.contains(call.id.0.as_str()) => {
                    Some(missing_result_part(call.id.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        repaired.push(item);
        if !missing.is_empty() {
            let result = Item::new(ItemKind::Tool, missing).with_created_at(Timestamp::now());
            synthesized.push(result.clone());
            repaired.push(result);
        }
    }
    *transcript = repaired;
    synthesized
}

/// Reports whether any tool call in `transcript` is still unanswered.
#[must_use]
pub fn has_unanswered_tool_calls(transcript: &[Item]) -> bool {
    let answered = answered_calls(transcript);
    transcript
        .iter()
        .flat_map(|item| &item.parts)
        .any(|part| match part {
            Part::ToolCall(call) => !answered.contains(call.id.0.as_str()),
            _ => false,
        })
}

fn missing_result_part(call_id: impl Into<ToolCallId>) -> Part {
    Part::ToolResult(
        ToolResultPart::error(call_id, ToolOutput::text(MISSING))
            .with_metadata(missing_result_metadata()),
    )
}

fn answered_calls(transcript: &[Item]) -> HashSet<&str> {
    transcript
        .iter()
        .flat_map(|item| &item.parts)
        .filter_map(|part| match part {
            Part::ToolResult(result) => Some(result.call_id.0.as_str()),
            _ => None,
        })
        .collect()
}

fn missing_result_metadata() -> MetadataMap {
    let mut metadata = MetadataMap::new();
    metadata.insert("kit.tool_result.repaired".to_owned(), Value::Bool(true));
    metadata
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::ToolCallPart;
    use serde_json::json;

    fn call(id: &str) -> Item {
        Item::new(
            ItemKind::Assistant,
            vec![Part::ToolCall(ToolCallPart::new(id, "compose", json!({})))],
        )
    }

    fn result(id: &str) -> Item {
        Item::new(
            ItemKind::Tool,
            vec![Part::ToolResult(ToolResultPart::success(
                id,
                ToolOutput::text("done"),
            ))],
        )
    }

    #[test]
    fn answers_an_unanswered_call_in_place() {
        let mut transcript = vec![
            Item::text(ItemKind::User, "go"),
            call("call-1"),
            Item::text(ItemKind::User, "still there?"),
        ];

        let synthesized = repair_unanswered_tool_calls(&mut transcript);

        assert_eq!(synthesized.len(), 1);
        assert!(synthesized[0].created_at.is_some());
        assert_eq!(synthesized[0].created_at, transcript[2].created_at);
        assert_eq!(
            transcript.iter().map(|item| item.kind).collect::<Vec<_>>(),
            [
                ItemKind::User,
                ItemKind::Assistant,
                ItemKind::Tool,
                ItemKind::User
            ],
            "the result must directly follow the call it answers"
        );
        let Part::ToolResult(repaired) = &transcript[2].parts[0] else {
            panic!("expected a tool result");
        };
        assert_eq!(repaired.call_id.0, "call-1");
        assert!(repaired.is_error);
        assert!(!has_unanswered_tool_calls(&transcript));
    }

    #[test]
    fn leaves_answered_calls_alone() {
        let mut transcript = vec![call("call-1"), result("call-1")];
        let before = transcript.clone();

        assert!(repair_unanswered_tool_calls(&mut transcript).is_empty());

        assert_eq!(transcript, before);
    }

    #[test]
    fn answers_every_call_of_a_parallel_round() {
        let mut transcript = vec![
            Item::new(
                ItemKind::Assistant,
                vec![
                    Part::ToolCall(ToolCallPart::new("call-1", "compose", json!({}))),
                    Part::ToolCall(ToolCallPart::new("call-2", "compose", json!({}))),
                ],
            ),
            result("call-2"),
        ];

        let synthesized = repair_unanswered_tool_calls(&mut transcript);

        assert_eq!(synthesized.len(), 1, "only the unanswered call is repaired");
        assert!(!has_unanswered_tool_calls(&transcript));
    }
}
