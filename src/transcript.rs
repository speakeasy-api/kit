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

const OPENAI_RESPONSES_CONTINUATION: &str = "openai.responses.continuation.v1";
const OPENAI_SUBSCRIPTION_CONTINUATION: &str = "openai.subscription.v1";

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

/// Removes provider continuation state that is bound to the source session.
///
/// A fork has a new durable identity, so replaying opaque continuation state from
/// the source would violate the provider's session binding. Generated assistant
/// images cannot be encoded without that continuation and are omitted as well.
pub(crate) fn sanitize_forked_transcript(transcript: &mut [Item]) {
    for item in transcript {
        let assistant = item.kind == ItemKind::Assistant;
        item.parts.retain_mut(|part| {
            let is_media = matches!(part, Part::Media(_));
            let metadata = match part {
                Part::ToolCall(call) => Some(&mut call.metadata),
                Part::Reasoning(reasoning) => Some(&mut reasoning.metadata),
                Part::Media(media) => Some(&mut media.metadata),
                _ => None,
            };
            let Some(metadata) = metadata else {
                return true;
            };
            metadata.remove(OPENAI_RESPONSES_CONTINUATION);
            metadata.remove(OPENAI_SUBSCRIPTION_CONTINUATION);
            !(assistant && is_media)
        });
    }
}

/// Validates the ordered tool protocol at a checkout boundary without repair.
///
/// Each assistant item with calls opens one batch; only tool results may follow until the
/// batch is complete. Parallel results may arrive in any order, but call IDs
/// cannot be reused anywhere in the prefix. An empty prefix is valid.
///
/// Also rejects role/part combinations unsupported by both the Completions and
/// Responses adapters. This is their union, not a promise that every provider
/// can encode the prefix: user files and reasoning remain Completions-compatible.
/// Assistant media is allowed because the shared fork sanitizer removes it.
/// Provider-specific payload, data-reference, metadata, and size validation
/// remains with the sanitizer and adapters. This does not select candidates.
pub(crate) fn validate_checkout_prefix(transcript: &[Item]) -> Result<(), String> {
    let mut seen = HashSet::new();
    let mut pending = HashSet::new();
    for (index, item) in transcript.iter().enumerate() {
        if !pending.is_empty() && item.kind != ItemKind::Tool {
            return Err(format!(
                "checkout prefix item {index} interrupts a pending tool-call batch"
            ));
        }
        if item.kind == ItemKind::Tool && item.parts.is_empty() {
            return Err(format!("checkout prefix item {index} has no tool results"));
        }
        for part in &item.parts {
            match part {
                Part::ToolCall(call) => {
                    if item.kind != ItemKind::Assistant {
                        return Err(format!(
                            "checkout prefix item {index} has a tool call outside an assistant item"
                        ));
                    }
                    let id = call.id.0.as_str();
                    if id.trim().is_empty() {
                        return Err(format!(
                            "checkout prefix item {index} has a blank tool-call ID"
                        ));
                    }
                    if !seen.insert(id) {
                        return Err(format!(
                            "checkout prefix item {index} reuses a tool-call ID"
                        ));
                    }
                    pending.insert(id);
                }
                Part::ToolResult(result) => {
                    if item.kind != ItemKind::Tool {
                        return Err(format!(
                            "checkout prefix item {index} has a tool result outside a tool item"
                        ));
                    }
                    let id = result.call_id.0.as_str();
                    if id.trim().is_empty() {
                        return Err(format!(
                            "checkout prefix item {index} has a blank tool-result ID"
                        ));
                    }
                    if !pending.remove(id) {
                        return Err(format!(
                            "checkout prefix item {index} has a duplicate or orphan tool result"
                        ));
                    }
                }
                _ if item.kind == ItemKind::Tool => {
                    return Err(format!(
                        "checkout prefix item {index} has non-result content in a tool item"
                    ));
                }
                Part::Custom(_) => {
                    return Err(format!(
                        "checkout prefix item {index} has unsupported custom content"
                    ));
                }
                Part::File(_) if item.kind != ItemKind::User => {
                    return Err(format!(
                        "checkout prefix item {index} has file content outside a user item"
                    ));
                }
                Part::Media(_) if !matches!(item.kind, ItemKind::User | ItemKind::Assistant) => {
                    return Err(format!(
                        "checkout prefix item {index} has media outside a user or assistant item"
                    ));
                }
                Part::Media(media)
                    if item.kind == ItemKind::User
                        && matches!(
                            media.modality,
                            agentkit_core::Modality::Video | agentkit_core::Modality::Binary
                        ) =>
                {
                    return Err(format!(
                        "checkout prefix item {index} has an unsupported user media modality"
                    ));
                }
                _ => {}
            }
        }
    }
    if !pending.is_empty() {
        return Err("checkout prefix ends with unanswered tool calls".into());
    }
    Ok(())
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
    fn checkout_accepts_empty_and_closed_parallel_batches_without_changes() {
        assert_eq!(validate_checkout_prefix(&[]), Ok(()));
        let parallel = Item::new(
            ItemKind::Assistant,
            vec![call("a").parts.remove(0), call("b").parts.remove(0)],
        );
        for results in [
            vec![result("a"), result("b")],
            vec![result("b"), result("a")],
            vec![Item::new(
                ItemKind::Tool,
                vec![result("b").parts.remove(0), result("a").parts.remove(0)],
            )],
        ] {
            let mut transcript = vec![Item::text(ItemKind::User, "go"), parallel.clone()];
            transcript.extend(results);
            transcript.extend([call("c"), result("c")]);
            transcript.push(Item::text(ItemKind::Assistant, "done"));
            let before = transcript.clone();
            assert_eq!(validate_checkout_prefix(&transcript), Ok(()));
            assert_eq!(transcript, before);
        }
    }

    #[test]
    fn checkout_rejects_invalid_order_roles_ids_and_boundaries() {
        let cases = [
            ("unanswered", vec![call("a")]),
            ("orphan", vec![result("a")]),
            ("before call", vec![result("a"), call("a")]),
            (
                "duplicate result",
                vec![call("a"), result("a"), result("a")],
            ),
            (
                "reused ID",
                vec![call("a"), result("a"), call("a"), result("a")],
            ),
            ("blank call", vec![call(" \t"), result(" \t")]),
            ("blank result", vec![call("a"), result("\n")]),
            ("empty call ID", vec![call(""), result("")]),
            ("empty result ID", vec![result("")]),
            (
                "nested batch",
                vec![call("a"), call("b"), result("b"), result("a")],
            ),
            ("wrong result ID", vec![call("a"), result("b")]),
            ("empty tool item", vec![Item::new(ItemKind::Tool, vec![])]),
            ("tool text", vec![Item::text(ItemKind::Tool, "done")]),
            (
                "same-item duplicate calls",
                vec![
                    Item::new(
                        ItemKind::Assistant,
                        vec![call("a").parts.remove(0), call("a").parts.remove(0)],
                    ),
                    result("a"),
                ],
            ),
            (
                "same-item duplicate results",
                vec![
                    call("a"),
                    Item::new(
                        ItemKind::Tool,
                        vec![result("a").parts.remove(0), result("a").parts.remove(0)],
                    ),
                ],
            ),
            (
                "partial parallel batch",
                vec![
                    Item::new(
                        ItemKind::Assistant,
                        vec![call("a").parts.remove(0), call("b").parts.remove(0)],
                    ),
                    result("a"),
                ],
            ),
            (
                "mixed tool content",
                vec![
                    call("a"),
                    Item::new(
                        ItemKind::Tool,
                        vec![result("a").parts.remove(0), Part::text("extra")],
                    ),
                ],
            ),
        ];
        for (name, transcript) in cases {
            assert!(validate_checkout_prefix(&transcript).is_err(), "{name}");
        }
        for kind in [
            ItemKind::System,
            ItemKind::Developer,
            ItemKind::User,
            ItemKind::Assistant,
            ItemKind::Tool,
            ItemKind::Context,
            ItemKind::Notification,
        ] {
            if kind != ItemKind::Assistant {
                let mut invalid_call = call("a");
                invalid_call.kind = kind;
                assert!(validate_checkout_prefix(&[invalid_call, result("a")]).is_err());
            }
            if kind != ItemKind::Tool {
                let mut invalid_result = result("a");
                invalid_result.kind = kind;
                assert!(validate_checkout_prefix(&[call("a"), invalid_result]).is_err());
            }
            // Even an empty intervening item cannot hide an interrupted batch.
            assert!(
                validate_checkout_prefix(&[call("a"), Item::new(kind, vec![]), result("a"),])
                    .is_err()
            );
        }
    }

    #[test]
    fn checkout_preserves_inherited_reasoning_and_media_policy() {
        let mut transcript = vec![
            Item::new(
                ItemKind::User,
                vec![Part::media(
                    agentkit_core::Modality::Image,
                    "image/png",
                    agentkit_core::DataRef::InlineBytes(vec![1]),
                )],
            ),
            Item::new(
                ItemKind::Assistant,
                vec![
                    Part::reasoning("summary"),
                    Part::media(
                        agentkit_core::Modality::Image,
                        "image/png",
                        agentkit_core::DataRef::InlineBytes(vec![2]),
                    ),
                    Part::text("done"),
                ],
            ),
        ];
        let before = transcript.clone();
        assert_eq!(validate_checkout_prefix(&transcript), Ok(()));
        assert_eq!(transcript, before);
        sanitize_forked_transcript(&mut transcript);
        assert_eq!(validate_checkout_prefix(&transcript), Ok(()));
        assert_eq!(transcript[0], before[0]);
        assert_eq!(
            transcript[1].parts,
            vec![Part::reasoning("summary"), Part::text("done")]
        );
    }

    #[test]
    fn checkout_checks_common_role_part_encoding_constraints() {
        use agentkit_core::{CustomPart, DataRef, Modality};

        for kind in [
            ItemKind::System,
            ItemKind::Developer,
            ItemKind::User,
            ItemKind::Assistant,
            ItemKind::Tool,
            ItemKind::Context,
            ItemKind::Notification,
        ] {
            for (part, supported) in [
                (Part::Custom(CustomPart::new("opaque")), false),
                (
                    Part::file(DataRef::InlineBytes(vec![1])),
                    kind == ItemKind::User,
                ),
                (Part::reasoning("summary"), kind != ItemKind::Tool),
                (
                    Part::structured(json!({"ok": true})),
                    kind != ItemKind::Tool,
                ),
                (Part::text("hello"), kind != ItemKind::Tool),
                (
                    Part::media(Modality::Image, "image/png", DataRef::InlineBytes(vec![1])),
                    matches!(kind, ItemKind::User | ItemKind::Assistant),
                ),
                (
                    Part::media(Modality::Audio, "audio/wav", DataRef::InlineBytes(vec![1])),
                    matches!(kind, ItemKind::User | ItemKind::Assistant),
                ),
                (
                    Part::media(Modality::Video, "video/mp4", DataRef::InlineBytes(vec![1])),
                    kind == ItemKind::Assistant, // Removed by the shared sanitizer.
                ),
                (
                    Part::media(
                        Modality::Binary,
                        "application/octet-stream",
                        DataRef::InlineBytes(vec![1]),
                    ),
                    kind == ItemKind::Assistant,
                ),
            ] {
                let item = Item::new(kind, vec![part]);
                assert_eq!(
                    validate_checkout_prefix(std::slice::from_ref(&item)).is_ok(),
                    supported,
                    "{item:?}",
                );
            }
        }
    }

    #[test]
    fn fork_removes_session_bound_continuations_and_generated_images() {
        let mut continuation = MetadataMap::new();
        continuation.insert(
            OPENAI_RESPONSES_CONTINUATION.into(),
            json!({ "session_id": "source" }),
        );
        continuation.insert(
            OPENAI_SUBSCRIPTION_CONTINUATION.into(),
            json!({ "session_id": "source" }),
        );
        continuation.insert("preserved".into(), true.into());
        let user_media = Part::media(
            agentkit_core::Modality::Image,
            "image/png",
            agentkit_core::DataRef::InlineBytes(vec![3]),
        );
        let mut transcript = vec![
            Item::new(
                ItemKind::Assistant,
                vec![
                    Part::ToolCall(
                        ToolCallPart::new("call-1", "compose", json!({}))
                            .with_metadata(continuation.clone()),
                    ),
                    Part::media(
                        agentkit_core::Modality::Image,
                        "image/png",
                        agentkit_core::DataRef::InlineBytes(vec![1]),
                    ),
                    Part::Media(
                        agentkit_core::MediaPart::new(
                            agentkit_core::Modality::Image,
                            "image/png",
                            agentkit_core::DataRef::InlineBytes(vec![2]),
                        )
                        .with_metadata(continuation),
                    ),
                ],
            ),
            Item::new(ItemKind::User, vec![user_media.clone()]),
        ];

        sanitize_forked_transcript(&mut transcript);

        assert_eq!(transcript[0].parts.len(), 1);
        let Part::ToolCall(call) = &transcript[0].parts[0] else {
            panic!("expected tool call");
        };
        assert_eq!(call.metadata.len(), 1);
        assert_eq!(call.metadata["preserved"], true);
        assert_eq!(transcript[1].parts, vec![user_media]);
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
