#[path = "../../src/agent/agentkit_bridge/mapping.rs"]
mod mapping;

use agentkit_core::{
    CancellationController, CostUsage, DataRef, Delta, FilePart, Item, ItemKind, MediaPart,
    MetadataMap, Modality, Part, PartId, PartKind, ReasoningPart, StructuredPart, TextPart,
    TokenUsage, ToolCallPart, ToolOutput, ToolResultPart, Usage,
};
use agentkit_loop::{
    InputRequest, LoopInterrupt, PendingApproval, PostValidationCheckpoint,
    PostValidationCheckpointCursor, PostValidationCheckpointHook, ToolRoundInfo, TranscriptEvent,
};
use agentkit_tools_core::{ApprovalReason, ApprovalRequest, ToolInterruption};
use mapping::{
    AGENTKIT_BASE_COMMIT, AGENTKIT_BASE_TREE, AGENTKIT_DIRTY_OVERLAY_SHA256,
    AGENTKIT_EXCLUDED_PATHS_SHA256, AGENTKIT_SNAPSHOT_SHA256, APPROVAL_REASON_VARIANTS,
    CanonicalCancellationState, CanonicalDelta, CanonicalInterruptKind, DATA_REF_VARIANTS,
    DELTA_VARIANTS, DeltaMapper, FINISH_REASON_VARIANTS, ITEM_KIND_VARIANTS,
    LOOP_INTERRUPT_VARIANTS, MODALITY_VARIANTS, PART_KIND_VARIANTS, PART_VARIANTS,
    RUNLET_SNAPSHOT_SHA256, TOOL_INTERRUPTION_VARIANTS, TOOL_OUTPUT_VARIANTS, from_agentkit_item,
    from_agentkit_usage, from_loop_interrupt, from_tool_interruption, from_transcript_event,
    from_turn_cancellation, to_agentkit_item,
};
use serde_json::json;

const CORE_SOURCE: &str = include_str!("../../vendor/agentkit/crates/agentkit-core/src/lib.rs");
const LOOP_SOURCE: &str = include_str!("../../vendor/agentkit/crates/agentkit-loop/src/lib.rs");
const TOOLS_SOURCE: &str =
    include_str!("../../vendor/agentkit/crates/agentkit-tools-core/src/lib.rs");
const SNAPSHOT: &str = include_str!("../../docs/compatibility/pins/agentkit-snapshot.yaml");
const PREFLIGHT: &str = include_str!("../../docs/decisions/PRE-0002-agentkit-pin.md");
const VENDOR_SNAPSHOT: &str = include_str!("../../vendor/agentkit/SNAPSHOT-METADATA.yaml");

#[test]
fn upstream_variant_counts_and_transcript_shape_are_pinned() {
    for (source, name, expected) in [
        (CORE_SOURCE, "ItemKind", ITEM_KIND_VARIANTS),
        (CORE_SOURCE, "Part", PART_VARIANTS),
        (CORE_SOURCE, "PartKind", PART_KIND_VARIANTS),
        (CORE_SOURCE, "Modality", MODALITY_VARIANTS),
        (CORE_SOURCE, "DataRef", DATA_REF_VARIANTS),
        (CORE_SOURCE, "ToolOutput", TOOL_OUTPUT_VARIANTS),
        (CORE_SOURCE, "Delta", DELTA_VARIANTS),
        (CORE_SOURCE, "FinishReason", FINISH_REASON_VARIANTS),
        (LOOP_SOURCE, "LoopInterrupt", LOOP_INTERRUPT_VARIANTS),
        (TOOLS_SOURCE, "ToolInterruption", TOOL_INTERRUPTION_VARIANTS),
        (TOOLS_SOURCE, "ApprovalReason", APPROVAL_REASON_VARIANTS),
    ] {
        assert_eq!(enum_variant_count(source, name), expected, "{name}");
    }
    let transcript = source_item(LOOP_SOURCE, "pub struct TranscriptEvent");
    assert!(transcript.contains("pub session_id: &'a SessionId"));
    assert!(transcript.contains("pub item: &'a Item"));
}

#[test]
fn post_validation_checkpoint_hook_is_pinned() {
    assert!(std::mem::size_of::<PostValidationCheckpoint<'static>>() > 0);
    let _: Option<&dyn PostValidationCheckpointHook> = None;
    let cursor = PostValidationCheckpointCursor::new("attempt", 9, "driver", 6, 7);
    assert_eq!(cursor.attempt_id(), "attempt");
    assert_eq!(cursor.fence(), 9);
    assert_eq!(cursor.driver_id(), "driver");
    assert_eq!(cursor.durable_head_sequence(), 6);
    assert_eq!(cursor.next_sequence(), 7);
    for field in [
        "pub id:",
        "pub session_id:",
        "pub turn_id:",
        "pub point:",
        "pub transcript:",
        "pub base_transcript:",
        "pub expected_previous_sequence:",
    ] {
        assert!(source_item(LOOP_SOURCE, "pub struct PostValidationCheckpoint").contains(field));
    }

    let validate = LOOP_SOURCE
        .find("validate_transcript_invariants(cursor.as_slice())?")
        .unwrap();
    let checkpoint = LOOP_SOURCE.find("hook.checkpoint(").unwrap();
    let promote = LOOP_SOURCE
        .find("self.transcript = pending.candidate")
        .unwrap();
    let dispatch = LOOP_SOURCE.find("let request = TurnRequest").unwrap();
    assert!(validate < checkpoint && checkpoint < promote && promote < dispatch);
}

#[test]
fn every_item_and_nested_variant_maps_without_hidden_reasoning() {
    let metadata = MetadataMap::from([("visible".into(), json!(true))]);
    let parts = vec![
        Part::Text(TextPart::new("text").with_metadata(metadata.clone())),
        Part::Media(MediaPart::new(
            Modality::Image,
            "image/png",
            DataRef::InlineBytes(vec![1, 2]),
        )),
        Part::File(
            FilePart::named("a.txt", DataRef::Uri("https://example.invalid/a".into()))
                .with_mime_type("text/plain"),
        ),
        Part::Structured(StructuredPart::new(json!({"ok": true})).with_schema(json!({}))),
        Part::Reasoning(ReasoningPart {
            summary: Some("public summary".into()),
            data: Some(DataRef::InlineText("hidden chain of thought".into())),
            redacted: false,
            metadata: MetadataMap::from([("thinking".into(), json!("hidden metadata"))]),
        }),
        Part::ToolCall(ToolCallPart::new("call", "read", json!({"path": "a.txt"}))),
        Part::ToolResult(ToolResultPart::success(
            "call",
            ToolOutput::Parts(vec![Part::Text(TextPart::new("result"))]),
        )),
        Part::Custom(agentkit_core::CustomPart::new("extension").with_value(json!({"x": 1}))),
    ];
    let usage = Usage::new(TokenUsage::new(10, 3).with_reasoning_tokens(2))
        .with_cost(CostUsage::new(0.25, "USD"));

    for kind in [
        ItemKind::System,
        ItemKind::Developer,
        ItemKind::User,
        ItemKind::Assistant,
        ItemKind::Tool,
        ItemKind::Context,
        ItemKind::Notification,
    ] {
        let source = Item::new(kind, parts.clone()).with_usage(usage.clone());
        let canonical = from_agentkit_item(&source);
        let wire = serde_json::to_string(&canonical).unwrap();
        assert!(!wire.contains("hidden chain of thought"));
        assert!(!wire.contains("hidden metadata"));
        assert!(wire.contains("public summary"));

        let round_trip = to_agentkit_item(&canonical);
        assert_eq!(round_trip.kind, source.kind);
        assert_eq!(round_trip.parts.len(), source.parts.len());
        let Part::Reasoning(reasoning) = &round_trip.parts[4] else {
            panic!("reasoning part changed kind");
        };
        assert_eq!(reasoning.summary.as_deref(), Some("public summary"));
        assert_eq!(reasoning.data, None);
        assert!(reasoning.metadata.is_empty());
    }
}

#[test]
fn all_deltas_map_and_reasoning_stream_content_is_suppressed() {
    let mut mapper = DeltaMapper::default();
    let visible = PartId::new("visible");
    let hidden = PartId::new("hidden");
    let deltas = [
        Delta::BeginPart {
            part_id: visible.clone(),
            kind: PartKind::Text,
        },
        Delta::AppendText {
            part_id: visible.clone(),
            chunk: "hello".into(),
        },
        Delta::AppendBytes {
            part_id: visible.clone(),
            chunk: vec![1],
        },
        Delta::ReplaceStructured {
            part_id: visible.clone(),
            value: json!({"done": true}),
        },
        Delta::SetMetadata {
            part_id: visible,
            metadata: MetadataMap::new(),
        },
        Delta::CommitPart {
            part: Part::Text(TextPart::new("hello")),
        },
    ];
    assert!(matches!(
        mapper.map(&deltas[0]),
        CanonicalDelta::BeginPart { .. }
    ));
    assert!(matches!(
        mapper.map(&deltas[1]),
        CanonicalDelta::AppendText { .. }
    ));
    assert!(matches!(
        mapper.map(&deltas[2]),
        CanonicalDelta::AppendBytes { .. }
    ));
    assert!(matches!(
        mapper.map(&deltas[3]),
        CanonicalDelta::ReplaceStructured { .. }
    ));
    assert!(matches!(
        mapper.map(&deltas[4]),
        CanonicalDelta::SetMetadata { .. }
    ));
    assert!(matches!(
        mapper.map(&deltas[5]),
        CanonicalDelta::CommitPart { .. }
    ));

    assert!(matches!(
        mapper.map(&Delta::BeginPart {
            part_id: hidden.clone(),
            kind: PartKind::Reasoning,
        }),
        CanonicalDelta::ReasoningSuppressed { .. }
    ));
    let mapped = mapper.map(&Delta::AppendText {
        part_id: hidden,
        chunk: "secret reasoning".into(),
    });
    assert!(matches!(mapped, CanonicalDelta::ReasoningSuppressed { .. }));
    assert!(
        !serde_json::to_string(&mapped)
            .unwrap()
            .contains("secret reasoning")
    );
}

#[test]
fn transcript_interrupt_cancellation_and_usage_semantics_are_explicit() {
    let item = Item::text(ItemKind::User, "hello");
    let event = from_transcript_event(TranscriptEvent {
        session_id: &agentkit_core::SessionId::new("session"),
        item: &item,
    });
    assert_eq!(event.session_id, "session");

    let approval = LoopInterrupt::ApprovalRequest(PendingApproval {
        request: ApprovalRequest::new(
            "approval",
            "filesystem",
            ApprovalReason::SensitivePath,
            "allow read",
        )
        .with_call_id("call"),
    });
    let input = LoopInterrupt::AwaitingInput(InputRequest {
        session_id: agentkit_core::SessionId::new("session"),
        reason: "input needed".into(),
    });
    let round = LoopInterrupt::AfterToolResult(ToolRoundInfo {
        session_id: agentkit_core::SessionId::new("session"),
        turn_id: agentkit_core::TurnId::new("turn"),
        transcript_len: 4,
    });
    assert_eq!(
        from_loop_interrupt(&approval).kind,
        CanonicalInterruptKind::Approval
    );
    assert_eq!(
        from_loop_interrupt(&input).kind,
        CanonicalInterruptKind::Input
    );
    assert_eq!(
        from_loop_interrupt(&round).kind,
        CanonicalInterruptKind::ToolRoundBoundary
    );
    let ToolInterruption::ApprovalRequired(request) =
        ToolInterruption::ApprovalRequired(ApprovalRequest::new(
            "tool-approval",
            "shell",
            ApprovalReason::SensitiveCommand,
            "allow command",
        ));
    assert_eq!(
        from_tool_interruption(&ToolInterruption::ApprovalRequired(request)).kind,
        CanonicalInterruptKind::Approval
    );

    assert_eq!(
        from_turn_cancellation(None).state,
        CanonicalCancellationState::Unavailable
    );
    let controller = CancellationController::new();
    let checkpoint = controller.handle().checkpoint();
    assert_eq!(
        from_turn_cancellation(Some(&checkpoint)).state,
        CanonicalCancellationState::Active
    );
    controller.interrupt();
    assert_eq!(
        from_turn_cancellation(Some(&checkpoint)).state,
        CanonicalCancellationState::CancellationRequested
    );

    let unavailable = from_agentkit_usage(&Usage::default());
    assert_eq!(unavailable.input_tokens, None);
    assert_eq!(unavailable.uncached_input_tokens, None);
    assert_eq!(unavailable.tool_calls, None);
    assert_eq!(unavailable.compute_time_ms, None);
    let wire = serde_json::to_value(unavailable).unwrap();
    assert!(wire["input_tokens"].is_null());
    assert!(wire["uncached_input_tokens"].is_null());
    assert!(wire["tool_calls"].is_null());
}

#[test]
fn pin_identity_equals_preflight_snapshot() {
    for digest in [
        AGENTKIT_BASE_COMMIT,
        AGENTKIT_BASE_TREE,
        AGENTKIT_DIRTY_OVERLAY_SHA256,
        AGENTKIT_EXCLUDED_PATHS_SHA256,
        AGENTKIT_SNAPSHOT_SHA256,
        RUNLET_SNAPSHOT_SHA256,
    ] {
        assert!(SNAPSHOT.contains(digest), "snapshot is missing {digest}");
        assert!(PREFLIGHT.contains(digest), "preflight is missing {digest}");
        assert!(
            VENDOR_SNAPSHOT.contains(digest),
            "vendor snapshot is missing {digest}"
        );
    }
}

fn source_item<'a>(source: &'a str, declaration: &str) -> &'a str {
    let start = source
        .find(declaration)
        .unwrap_or_else(|| panic!("missing {declaration}"));
    let body = &source[start..];
    let open = body.find('{').unwrap();
    let mut depth = 0usize;
    for (offset, byte) in body[open..].bytes().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &body[..open + offset + 1];
                }
            }
            _ => {}
        }
    }
    panic!("unterminated {declaration}")
}

fn enum_variant_count(source: &str, name: &str) -> usize {
    let item = source_item(source, &format!("pub enum {name}"));
    let mut braces = 0usize;
    let mut parentheses = 0usize;
    let mut brackets = 0usize;
    let mut count = 0usize;
    for line in item.lines() {
        let code = line.split_once("//").map_or(line, |(code, _)| code);
        for byte in code.bytes() {
            match byte {
                b'{' => braces += 1,
                b'}' => braces -= 1,
                b'(' => parentheses += 1,
                b')' => parentheses -= 1,
                b'[' => brackets += 1,
                b']' => brackets -= 1,
                b',' if braces == 1 && parentheses == 0 && brackets == 0 => count += 1,
                _ => {}
            }
        }
    }
    count
}
