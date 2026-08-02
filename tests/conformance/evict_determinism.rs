use std::collections::{BTreeMap, BTreeSet};

use agentkit_core::{
    CostUsage, DataRef, FilePart, Item, ItemKind, Part, ReasoningPart, TextPart, TokenUsage,
    ToolCallPart, ToolOutput, ToolResultPart, Usage,
};
use agentkit_loop::MutationPoint;
use kit::agent::compaction::{
    evict::{
        EvictionCategory, EvictionError, EvictionLimit, EvictionLimits, EvictionOutcome,
        EvictionPlan, EvictionRequest, FactClassification, FactKey, MAX_FACT_KEY_BYTES,
        MAX_SUPPORTED_JSON_DEPTH, MAX_SUPPORTED_PART_DEPTH, MAX_TOOL_CALL_ID_BYTES, OperationFact,
        accept_mutation_point, evict,
    },
    states::CheckpointMutationPoint,
};
use kit::agent::driver::restart::EffectStatus;
use serde_json::json;

const REASONING: &str = "private chain of thought that must never leave the loop";

fn limits() -> EvictionLimits {
    EvictionLimits {
        max_items: 256,
        max_visited_parts: 1024,
        max_part_depth: 4,
        max_json_depth: 16,
        max_facts: 64,
        max_canonical_bytes: 1_048_576,
        max_tool_output_bytes: 65_536,
        max_token_estimate: 262_144,
    }
}

fn request<'a>(
    transcript: &'a [Item],
    facts: &'a [OperationFact],
    target_tokens: usize,
) -> EvictionRequest<'a> {
    EvictionRequest {
        transcript,
        facts,
        mutation_point: CheckpointMutationPoint::AfterToolResult,
        target_tokens,
        limits: limits(),
    }
}

fn text_item(kind: ItemKind, text: &str) -> Item {
    Item::text(kind, text)
}

fn reasoning(summary: &str) -> Part {
    Part::Reasoning(ReasoningPart::summary(summary))
}

fn call(id: &str, name: &str) -> Part {
    Part::ToolCall(ToolCallPart::new(id, name, json!({ "id": id })))
}

fn result(id: &str, body: &str) -> Part {
    Part::ToolResult(ToolResultPart::success(id, ToolOutput::text(body)))
}

fn error_result(id: &str, body: &str) -> Part {
    Part::ToolResult(ToolResultPart::error(id, ToolOutput::text(body)))
}

fn assistant(parts: Vec<Part>) -> Item {
    Item::new(ItemKind::Assistant, parts)
}

fn tool(parts: Vec<Part>) -> Item {
    Item::new(ItemKind::Tool, parts)
}

fn key(value: &str) -> FactKey {
    FactKey::parse(value).unwrap()
}

fn fact(id: &str, outcome: EffectStatus, classification: FactClassification) -> OperationFact {
    OperationFact {
        tool_call_id: id.into(),
        outcome,
        classification,
    }
}

/// One tool call and its result, with a payload large enough that removing the
/// pair visibly moves the token estimate.
fn exchange(id: &str, name: &str, body: &str) -> [Item; 2] {
    [
        assistant(vec![call(id, name)]),
        tool(vec![result(id, &body.repeat(40))]),
    ]
}

fn transcript() -> Vec<Item> {
    let mut items = vec![
        text_item(ItemKind::User, "implement the checkpoint state contract"),
        assistant(vec![reasoning(REASONING), call("call-log", "shell_exec")]),
        tool(vec![result("call-log", &"raw log line\n".repeat(40))]),
    ];
    items.extend(exchange(
        "call-map-1",
        "repo_map",
        "workspace map generation one\n",
    ));
    items.extend(exchange(
        "call-map-2",
        "repo_map",
        "workspace map generation two\n",
    ));
    items.extend(exchange("call-read-1", "fs_read_file", "file body bytes\n"));
    items.extend(exchange("call-read-2", "fs_read_file", "file body bytes\n"));
    items.extend(exchange("call-check", "shell_exec", "check passed\n"));
    items.push(text_item(
        ItemKind::Assistant,
        "active failure: parser rejects nested maps",
    ));
    items
}

fn facts() -> Vec<OperationFact> {
    vec![
        fact(
            "call-log",
            EffectStatus::Succeeded,
            FactClassification::StaleRawLog,
        ),
        fact(
            "call-map-1",
            EffectStatus::Succeeded,
            FactClassification::SupersededMap {
                map_key: key("workspace"),
                generation: 1,
            },
        ),
        fact(
            "call-map-2",
            EffectStatus::Succeeded,
            FactClassification::SupersededMap {
                map_key: key("workspace"),
                generation: 2,
            },
        ),
        fact(
            "call-read-1",
            EffectStatus::Succeeded,
            FactClassification::DuplicateRead {
                equivalence_key: key("blake3:file-body"),
            },
        ),
        fact(
            "call-read-2",
            EffectStatus::Succeeded,
            FactClassification::DuplicateRead {
                equivalence_key: key("blake3:file-body"),
            },
        ),
        fact(
            "call-check",
            EffectStatus::Succeeded,
            FactClassification::SuccessfulCommandNoise,
        ),
    ]
}

/// The estimate of a transcript once reasoning is stripped and nothing else is
/// evicted, used to derive exact targets without hard-coded byte counts.
fn baseline_tokens(transcript: &[Item]) -> usize {
    evict(&request(transcript, &[], usize::MAX))
        .unwrap()
        .plan()
        .estimated_tokens()
}

fn top_level_call_ids(items: &[Item]) -> BTreeSet<String> {
    items
        .iter()
        .flat_map(|item| &item.parts)
        .filter_map(|part| match part {
            Part::ToolCall(call) => Some(call.id.0.clone()),
            _ => None,
        })
        .collect()
}

fn evicted_call_ids(source: &[Item], plan: &EvictionPlan) -> BTreeSet<String> {
    &top_level_call_ids(source) - &top_level_call_ids(plan.candidate())
}

fn categories(plan: &EvictionPlan) -> Vec<EvictionCategory> {
    plan.removed().iter().map(|entry| entry.category).collect()
}

fn rendered(items: &[Item]) -> String {
    serde_json::to_string(items).unwrap()
}

#[test]
fn kit_compact_800_eviction_never_mutates_its_source_transcript() {
    let source = transcript();
    let untouched = rendered(&source);
    let facts = facts();

    let outcome = evict(&request(&source, &facts, 0)).unwrap();
    let plan = outcome.plan();

    assert_eq!(rendered(&source), untouched);
    assert_eq!(source, transcript());
    assert!(!plan.removed().is_empty());
    assert_ne!(plan.candidate_digest(), plan.input_digest());
    assert_eq!(plan.input_digest(), &baseline_input_digest(&source));

    assert_eq!(plan.candidate().len(), source.len());
    for (item, original) in plan.candidate().iter().zip(&source) {
        assert_eq!(item.id, original.id);
        assert_eq!(item.kind, original.kind);
        assert_eq!(item.metadata, original.metadata);
        assert_eq!(item.usage, original.usage);
        assert_eq!(item.finish_reason, original.finish_reason);
        assert_eq!(item.created_at, original.created_at);
        assert!(item.parts.iter().all(|part| original.parts.contains(part)));
    }
}

fn baseline_input_digest(transcript: &[Item]) -> kit::domain::events::ContentDigest {
    evict(&request(transcript, &[], usize::MAX))
        .unwrap()
        .plan()
        .input_digest()
        .clone()
}

#[test]
fn kit_compact_823_evicts_every_category_in_the_declared_order() {
    let source = transcript();
    let facts = facts();
    let outcome = evict(&request(&source, &facts, 0)).unwrap();
    let plan = outcome.plan();

    let ordered = categories(plan);
    assert_eq!(
        ordered,
        vec![
            EvictionCategory::StaleRawLog,
            EvictionCategory::StaleRawLog,
            EvictionCategory::SupersededMap,
            EvictionCategory::SupersededMap,
            EvictionCategory::DuplicateRead,
            EvictionCategory::DuplicateRead,
            EvictionCategory::ReasoningPart,
            EvictionCategory::SuccessfulCommandNoise,
            EvictionCategory::SuccessfulCommandNoise,
        ]
    );

    let distinct = ordered.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(
        distinct.into_iter().collect::<Vec<_>>(),
        EvictionCategory::ORDER.to_vec()
    );

    // Stale log, older map generation, older duplicate read and successful
    // command noise go; the newest map, newest read and the active failure stay.
    assert_eq!(
        evicted_call_ids(&source, plan),
        ["call-check", "call-log", "call-map-1", "call-read-1"]
            .map(str::to_owned)
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
    let candidate = rendered(plan.candidate());
    assert!(candidate.contains("workspace map generation two"));
    assert!(candidate.contains("active failure"));
    assert!(!candidate.contains("workspace map generation one"));
    assert!(!candidate.contains("raw log line"));
    assert!(!candidate.contains("check passed"));
}

#[test]
fn within_a_category_removals_order_by_position_then_path_then_call_id() {
    let source = transcript();
    let facts = facts();
    let plan = evict(&request(&source, &facts, 0)).unwrap();
    let plan = plan.plan();

    for window in plan.removed().windows(2) {
        let (left, right) = (&window[0], &window[1]);
        let left_key = (
            left.category,
            left.item_index,
            left.part_path.clone(),
            left.tool_call_id_digest.clone(),
        );
        let right_key = (
            right.category,
            right.item_index,
            right.part_path.clone(),
            right.tool_call_id_digest.clone(),
        );
        assert!(left_key <= right_key);
    }
}

#[test]
fn overlapping_pairs_keep_global_manifest_order_and_atomic_identity() {
    let source = vec![
        assistant(vec![call("call-z", "shell_exec")]),
        assistant(vec![call("call-a", "shell_exec")]),
        tool(vec![result("call-z", "first completed output")]),
        tool(vec![result("call-a", "second completed output")]),
    ];
    let facts = vec![
        fact(
            "call-z",
            EffectStatus::Succeeded,
            FactClassification::StaleRawLog,
        ),
        fact(
            "call-a",
            EffectStatus::Succeeded,
            FactClassification::StaleRawLog,
        ),
    ];
    let outcome = evict(&request(&source, &facts, 0)).unwrap();
    let plan = outcome.plan();
    let removed = plan.removed();

    assert_eq!(categories(plan), vec![EvictionCategory::StaleRawLog; 4]);
    assert_eq!(
        removed
            .iter()
            .map(|entry| (entry.item_index, entry.part_path.clone()))
            .collect::<Vec<_>>(),
        vec![(0, vec![0]), (1, vec![0]), (2, vec![0]), (3, vec![0])]
    );
    assert_eq!(
        removed[0].tool_call_id_digest,
        removed[2].tool_call_id_digest
    );
    assert_eq!(
        removed[1].tool_call_id_digest,
        removed[3].tool_call_id_digest
    );
    assert_ne!(
        removed[0].tool_call_id_digest,
        removed[1].tool_call_id_digest
    );
    assert!(plan.candidate().iter().all(|item| item.parts.is_empty()));
}

#[test]
fn only_the_two_checkpoint_mutation_points_are_accepted() {
    assert_eq!(
        accept_mutation_point(MutationPoint::AfterToolResult),
        Ok(CheckpointMutationPoint::AfterToolResult)
    );
    assert_eq!(
        accept_mutation_point(MutationPoint::AfterTurnEnded),
        Ok(CheckpointMutationPoint::AfterTurnEnded)
    );

    let source = transcript();
    let facts = facts();
    for point in [
        CheckpointMutationPoint::AfterToolResult,
        CheckpointMutationPoint::AfterTurnEnded,
    ] {
        let mut request = request(&source, &facts, 0);
        request.mutation_point = point;
        let plan = evict(&request).unwrap();
        assert_eq!(plan.plan().mutation_point(), point);
    }

    // The mutation point is bound into the eviction digest, not the candidate.
    let mut turn_end = request(&source, &facts, 0);
    turn_end.mutation_point = CheckpointMutationPoint::AfterTurnEnded;
    let after_tool = evict(&request(&source, &facts, 0)).unwrap();
    let after_turn = evict(&turn_end).unwrap();
    assert_eq!(
        after_tool.plan().candidate_digest(),
        after_turn.plan().candidate_digest()
    );
    assert_ne!(
        after_tool.plan().eviction_digest(),
        after_turn.plan().eviction_digest()
    );
}

#[test]
fn every_reasoning_part_is_stripped_including_nested_tool_output_parts() {
    let source = vec![
        text_item(ItemKind::User, "goal"),
        assistant(vec![reasoning(REASONING), call("call-1", "shell_exec")]),
        tool(vec![Part::ToolResult(ToolResultPart::success(
            "call-1",
            ToolOutput::Parts(vec![
                Part::text("tool output text"),
                reasoning("nested private reasoning"),
            ]),
        ))]),
    ];

    let outcome = evict(&request(&source, &[], usize::MAX)).unwrap();
    let plan = outcome.plan();

    assert_eq!(categories(plan), vec![EvictionCategory::ReasoningPart; 2]);
    assert_eq!(plan.removed()[0].part_path, vec![0]);
    assert_eq!(plan.removed()[1].part_path, vec![0, 1]);
    assert!(
        plan.removed()
            .iter()
            .all(|entry| entry.tool_call_id_digest.is_none())
    );

    let candidate = rendered(plan.candidate());
    assert!(!candidate.contains(REASONING));
    assert!(!candidate.contains("nested private reasoning"));
    assert!(!candidate.contains("Reasoning"));
    assert!(candidate.contains("tool output text"));
    let debug = format!("{plan:?}");
    assert!(debug.contains("ReasoningPart"));
    assert!(!debug.contains(REASONING));
    assert!(!debug.contains("nested private reasoning"));
}

#[test]
fn reasoning_bytes_reach_neither_digest() {
    let with_one = vec![
        text_item(ItemKind::User, "goal"),
        assistant(vec![reasoning("first hidden rationale")]),
    ];
    let with_other = vec![
        text_item(ItemKind::User, "goal"),
        assistant(vec![reasoning(
            "a completely different and much longer hidden rationale",
        )]),
    ];

    let left = evict(&request(&with_one, &[], usize::MAX)).unwrap();
    let right = evict(&request(&with_other, &[], usize::MAX)).unwrap();

    assert_eq!(
        left.plan().candidate_digest(),
        right.plan().candidate_digest()
    );
    assert_eq!(
        left.plan().eviction_digest(),
        right.plan().eviction_digest()
    );
    assert_eq!(left.plan().input_digest(), right.plan().input_digest());
    assert_eq!(left.plan().input_digest(), left.plan().candidate_digest());
}

#[test]
fn typed_reasoning_usage_does_not_change_candidates_thresholds_or_digests() {
    fn source_with(reasoning_tokens: Option<u64>) -> Vec<Item> {
        let mut source = transcript();
        let mut tokens = TokenUsage::new(1_024, 128)
            .with_cached_input_tokens(512)
            .with_cache_write_input_tokens(64);
        if let Some(reasoning_tokens) = reasoning_tokens {
            tokens = tokens.with_reasoning_tokens(reasoning_tokens);
        }
        source[0].usage = Some(
            Usage::new(tokens)
                .with_cost(
                    CostUsage::new(1.25, "USD").with_provider_amount("provider-cost-display"),
                )
                .with_metadata(BTreeMap::from([(
                    "accounting-source".to_owned(),
                    json!("provider"),
                )])),
        );
        source
    }

    let absent = source_with(None);
    let low = source_with(Some(1));
    let high = source_with(Some(u64::MAX));
    let facts = facts();
    let target = baseline_tokens(&absent) - 1;

    let baseline = evict(&request(&absent, &facts, target)).unwrap();
    for source in [&low, &high] {
        let outcome = evict(&request(source, &facts, target)).unwrap();
        assert_eq!(baseline.fits(), outcome.fits());
        assert_eq!(
            baseline.plan().estimated_tokens(),
            outcome.plan().estimated_tokens()
        );
        assert_eq!(baseline.plan().removed(), outcome.plan().removed());
        assert_eq!(baseline.plan().candidate(), outcome.plan().candidate());
        assert_eq!(
            baseline.plan().input_digest(),
            outcome.plan().input_digest()
        );
        assert_eq!(
            baseline.plan().candidate_digest(),
            outcome.plan().candidate_digest()
        );
        assert_eq!(
            baseline.plan().eviction_digest(),
            outcome.plan().eviction_digest()
        );
    }

    let mut expected_usage = low[0].usage.clone().unwrap();
    expected_usage.tokens.as_mut().unwrap().reasoning_tokens = None;
    assert_eq!(
        baseline.plan().candidate()[0].usage.as_ref(),
        Some(&expected_usage)
    );
    assert_eq!(
        absent[0]
            .usage
            .as_ref()
            .unwrap()
            .tokens
            .as_ref()
            .unwrap()
            .reasoning_tokens,
        None
    );
    assert_eq!(
        low[0]
            .usage
            .as_ref()
            .unwrap()
            .tokens
            .as_ref()
            .unwrap()
            .reasoning_tokens,
        Some(1)
    );
    assert_eq!(
        high[0]
            .usage
            .as_ref()
            .unwrap()
            .tokens
            .as_ref()
            .unwrap()
            .reasoning_tokens,
        Some(u64::MAX)
    );
}

#[test]
fn errors_do_not_reflect_reasoning_or_attacker_strings() {
    let secret = "hidden rationale must not enter diagnostics";
    let attacker_id = "attacker-controlled-call-id";
    let source = vec![
        assistant(vec![reasoning(secret)]),
        tool(vec![result(attacker_id, "orphan result")]),
    ];

    let error = evict(&request(&source, &[], 0)).unwrap_err();
    let diagnostic = format!("{error:?} {error}");
    assert!(!diagnostic.contains(secret));
    assert!(!diagnostic.contains(attacker_id));

    let hostile_key = "attacker\u{202e}key";
    let error = FactKey::parse(hostile_key).unwrap_err();
    let diagnostic = format!("{error:?} {error}");
    assert!(!diagnostic.contains(hostile_key));
}

#[test]
fn plans_do_not_expose_candidate_content_or_raw_tool_call_ids() {
    let secret = "retained user secret";
    let provider_id = "provider-sensitive-call-id";
    let source = vec![
        text_item(ItemKind::User, secret),
        assistant(vec![call(provider_id, "shell_exec")]),
        tool(vec![result(provider_id, "sensitive output")]),
    ];
    let facts = vec![fact(
        provider_id,
        EffectStatus::Succeeded,
        FactClassification::StaleRawLog,
    )];

    let outcome = evict(&request(&source, &facts, 0)).unwrap();
    let plan = outcome.plan();
    let debug = format!("{outcome:?}");
    let manifest = serde_json::to_string(plan.removed()).unwrap();

    assert!(!debug.contains(secret));
    assert!(!debug.contains("sensitive output"));
    assert!(!debug.contains(provider_id));
    assert!(!manifest.contains(provider_id));
    assert!(
        plan.removed()
            .iter()
            .all(|entry| entry.tool_call_id_digest.is_some())
    );
}

#[test]
fn non_successful_operations_are_never_successful_command_noise() {
    let source = transcript();
    for outcome in [
        EffectStatus::Failed,
        EffectStatus::Cancelled,
        EffectStatus::OutcomeUnknown,
    ] {
        let facts = vec![fact(
            "call-check",
            outcome,
            FactClassification::SuccessfulCommandNoise,
        )];
        assert_eq!(
            evict(&request(&source, &facts, 0)),
            Err(EvictionError::NonSuccessfulCommandNoise(outcome))
        );
    }
}

#[test]
fn an_error_result_cannot_be_reclassified_as_successful_noise() {
    let source = vec![
        assistant(vec![call("call-error", "shell_exec")]),
        tool(vec![error_result("call-error", "command failed")]),
    ];
    let untouched = rendered(&source);
    let facts = vec![fact(
        "call-error",
        EffectStatus::Succeeded,
        FactClassification::SuccessfulCommandNoise,
    )];

    assert_eq!(
        evict(&request(&source, &facts, 0)),
        Err(EvictionError::FactOutcomeMismatch)
    );
    assert_eq!(rendered(&source), untouched);
}

#[test]
fn non_successful_operations_are_retained_in_every_other_category() {
    let source = transcript();
    for outcome in [
        EffectStatus::Failed,
        EffectStatus::Cancelled,
        EffectStatus::OutcomeUnknown,
    ] {
        let facts = vec![
            fact("call-log", outcome, FactClassification::StaleRawLog),
            fact(
                "call-read-1",
                outcome,
                FactClassification::DuplicateRead {
                    equivalence_key: key("blake3:file-body"),
                },
            ),
            fact(
                "call-read-2",
                outcome,
                FactClassification::DuplicateRead {
                    equivalence_key: key("blake3:file-body"),
                },
            ),
            fact(
                "call-map-1",
                outcome,
                FactClassification::SupersededMap {
                    map_key: key("workspace"),
                    generation: 1,
                },
            ),
            fact(
                "call-map-2",
                outcome,
                FactClassification::SupersededMap {
                    map_key: key("workspace"),
                    generation: 2,
                },
            ),
        ];
        let plan = evict(&request(&source, &facts, 0)).unwrap();
        assert!(evicted_call_ids(&source, plan.plan()).is_empty());
        assert!(!plan.fits());
    }
}

#[test]
fn duplicate_reads_need_one_equivalence_key_and_retain_the_newest() {
    let source = transcript();

    let shared = vec![
        fact(
            "call-read-1",
            EffectStatus::Succeeded,
            FactClassification::DuplicateRead {
                equivalence_key: key("blake3:file-body"),
            },
        ),
        fact(
            "call-read-2",
            EffectStatus::Succeeded,
            FactClassification::DuplicateRead {
                equivalence_key: key("blake3:file-body"),
            },
        ),
    ];
    let plan = evict(&request(&source, &shared, 0)).unwrap();
    assert_eq!(
        evicted_call_ids(&source, plan.plan()),
        ["call-read-1".to_owned()].into_iter().collect()
    );

    let distinct = vec![
        fact(
            "call-read-1",
            EffectStatus::Succeeded,
            FactClassification::DuplicateRead {
                equivalence_key: key("blake3:file-body-a"),
            },
        ),
        fact(
            "call-read-2",
            EffectStatus::Succeeded,
            FactClassification::DuplicateRead {
                equivalence_key: key("blake3:file-body-b"),
            },
        ),
    ];
    let plan = evict(&request(&source, &distinct, 0)).unwrap();
    assert!(evicted_call_ids(&source, plan.plan()).is_empty());

    let reverse_completion = vec![
        assistant(vec![call("call-a", "fs_read_file")]),
        assistant(vec![call("call-b", "fs_read_file")]),
        tool(vec![result("call-b", "same bytes")]),
        tool(vec![result("call-a", "same bytes")]),
    ];
    let facts = vec![
        fact(
            "call-a",
            EffectStatus::Succeeded,
            FactClassification::DuplicateRead {
                equivalence_key: key("same-content"),
            },
        ),
        fact(
            "call-b",
            EffectStatus::Succeeded,
            FactClassification::DuplicateRead {
                equivalence_key: key("same-content"),
            },
        ),
    ];
    let plan = evict(&request(&reverse_completion, &facts, 0)).unwrap();
    assert_eq!(
        evicted_call_ids(&reverse_completion, plan.plan()),
        ["call-b".to_owned()].into_iter().collect(),
        "the latest completed read is retained even when calls overlap"
    );
}

#[test]
fn superseded_maps_retain_the_newest_generation_then_the_latest_completion() {
    let source = transcript();

    let generations = vec![
        fact(
            "call-map-1",
            EffectStatus::Succeeded,
            FactClassification::SupersededMap {
                map_key: key("workspace"),
                generation: 9,
            },
        ),
        fact(
            "call-map-2",
            EffectStatus::Succeeded,
            FactClassification::SupersededMap {
                map_key: key("workspace"),
                generation: 2,
            },
        ),
    ];
    let plan = evict(&request(&source, &generations, 0)).unwrap();
    assert_eq!(
        evicted_call_ids(&source, plan.plan()),
        ["call-map-2".to_owned()].into_iter().collect(),
        "the newest generation is retained regardless of transcript position"
    );

    let tied = vec![
        fact(
            "call-map-1",
            EffectStatus::Succeeded,
            FactClassification::SupersededMap {
                map_key: key("workspace"),
                generation: 4,
            },
        ),
        fact(
            "call-map-2",
            EffectStatus::Succeeded,
            FactClassification::SupersededMap {
                map_key: key("workspace"),
                generation: 4,
            },
        ),
    ];
    let plan = evict(&request(&source, &tied, 0)).unwrap();
    assert_eq!(
        evicted_call_ids(&source, plan.plan()),
        ["call-map-1".to_owned()].into_iter().collect(),
        "an equal generation falls back to the latest transcript occurrence"
    );

    let reverse_completion = vec![
        assistant(vec![call("call-map-a", "repo_map")]),
        assistant(vec![call("call-map-b", "repo_map")]),
        tool(vec![result("call-map-b", "generation four")]),
        tool(vec![result("call-map-a", "generation four")]),
    ];
    let tied = vec![
        fact(
            "call-map-a",
            EffectStatus::Succeeded,
            FactClassification::SupersededMap {
                map_key: key("workspace"),
                generation: 4,
            },
        ),
        fact(
            "call-map-b",
            EffectStatus::Succeeded,
            FactClassification::SupersededMap {
                map_key: key("workspace"),
                generation: 4,
            },
        ),
    ];
    let plan = evict(&request(&reverse_completion, &tied, 0)).unwrap();
    assert_eq!(
        evicted_call_ids(&reverse_completion, plan.plan()),
        ["call-map-b".to_owned()].into_iter().collect(),
        "an equal generation retains the latest completion even when calls overlap"
    );

    let separate = vec![
        fact(
            "call-map-1",
            EffectStatus::Succeeded,
            FactClassification::SupersededMap {
                map_key: key("workspace"),
                generation: 1,
            },
        ),
        fact(
            "call-map-2",
            EffectStatus::Succeeded,
            FactClassification::SupersededMap {
                map_key: key("vendor"),
                generation: 1,
            },
        ),
    ];
    let plan = evict(&request(&source, &separate, 0)).unwrap();
    assert!(evicted_call_ids(&source, plan.plan()).is_empty());
}

#[test]
fn every_pair_is_removed_atomically_and_leaves_no_orphan() {
    let source = transcript();
    let facts = facts();
    let outcome = evict(&request(&source, &facts, 0)).unwrap();
    let plan = outcome.plan();

    for call_id_digest in plan
        .removed()
        .iter()
        .filter_map(|entry| entry.tool_call_id_digest.as_ref())
        .collect::<BTreeSet<_>>()
    {
        let entries = plan
            .removed()
            .iter()
            .filter(|entry| entry.tool_call_id_digest.as_ref() == Some(call_id_digest))
            .count();
        assert_eq!(entries, 2, "each call digest must lose its call and result");
    }

    let candidate = rendered(plan.candidate());
    for call_id in evicted_call_ids(&source, plan) {
        assert!(!candidate.contains(&call_id));
    }

    // The candidate re-validates as a complete pairing.
    assert!(evict(&request(plan.candidate(), &[], usize::MAX)).is_ok());
}

#[test]
fn removing_final_parts_preserves_item_envelopes() {
    let mut metadata = BTreeMap::new();
    metadata.insert("provenance".to_owned(), json!("provider"));
    let call_item = assistant(vec![call("call-only", "shell_exec")])
        .with_id("assistant-message")
        .with_metadata(metadata.clone())
        .with_usage(Usage::default().with_cost(CostUsage::new(1.25, "USD")));
    let result_item = tool(vec![result("call-only", "completed")])
        .with_id("tool-message")
        .with_metadata(metadata);
    let source = vec![call_item, result_item];
    let facts = vec![fact(
        "call-only",
        EffectStatus::Succeeded,
        FactClassification::StaleRawLog,
    )];

    let plan = evict(&request(&source, &facts, 0)).unwrap();
    assert_eq!(plan.plan().candidate().len(), source.len());
    for (candidate, original) in plan.plan().candidate().iter().zip(&source) {
        assert!(candidate.parts.is_empty());
        assert_eq!(candidate.id, original.id);
        assert_eq!(candidate.metadata, original.metadata);
        assert_eq!(candidate.usage, original.usage);
    }
}

#[test]
fn nested_tool_output_parts_are_content_not_transcript_protocol() {
    let source = vec![
        assistant(vec![call("call-outer", "shell_exec")]),
        tool(vec![Part::ToolResult(ToolResultPart::success(
            "call-outer",
            ToolOutput::Parts(vec![
                call("call-inner", "nested"),
                result("call-inner", "inner result"),
                reasoning("nested private reasoning"),
            ]),
        ))]),
    ];
    let facts = vec![
        fact(
            "call-outer",
            EffectStatus::Succeeded,
            FactClassification::StaleRawLog,
        ),
        fact(
            "call-inner",
            EffectStatus::Succeeded,
            FactClassification::StaleRawLog,
        ),
    ];

    let outcome = evict(&request(&source, &facts, 0)).unwrap();
    assert!(matches!(outcome, EvictionOutcome::Insufficient(_)));
    assert_eq!(
        evicted_call_ids(&source, outcome.plan()),
        ["call-outer".to_owned()].into_iter().collect()
    );
    assert_eq!(
        categories(outcome.plan()),
        vec![EvictionCategory::StaleRawLog, EvictionCategory::StaleRawLog]
    );

    let candidate = rendered(outcome.plan().candidate());
    assert!(!candidate.contains("call-outer"));
    assert!(!candidate.contains("call-inner"));
    assert!(!candidate.contains("nested private reasoning"));
    let reapplied = evict(&request(outcome.plan().candidate(), &facts, 0)).unwrap();
    assert!(reapplied.plan().removed().is_empty());
    assert_eq!(reapplied.plan().candidate(), outcome.plan().candidate());
}

#[test]
fn malformed_duplicate_orphan_out_of_order_and_in_flight_pairs_fail_closed() {
    let paired = vec![
        assistant(vec![call("call-1", "shell_exec")]),
        tool(vec![result("call-1", "ok")]),
    ];
    assert!(evict(&request(&paired, &[], usize::MAX)).is_ok());

    let duplicate_call = vec![
        assistant(vec![
            call("call-1", "shell_exec"),
            call("call-1", "shell_exec"),
        ]),
        tool(vec![result("call-1", "ok")]),
    ];
    assert_eq!(
        evict(&request(&duplicate_call, &[], usize::MAX)),
        Err(EvictionError::DuplicateToolCall)
    );

    let duplicate_result = vec![
        assistant(vec![call("call-1", "shell_exec")]),
        tool(vec![result("call-1", "ok"), result("call-1", "ok")]),
    ];
    assert_eq!(
        evict(&request(&duplicate_result, &[], usize::MAX)),
        Err(EvictionError::DuplicateToolResult)
    );

    let orphan = vec![tool(vec![result("call-9", "ok")])];
    assert_eq!(
        evict(&request(&orphan, &[], usize::MAX)),
        Err(EvictionError::OrphanToolResult)
    );

    let out_of_order = vec![
        tool(vec![result("call-1", "ok")]),
        assistant(vec![call("call-1", "shell_exec")]),
    ];
    assert_eq!(
        evict(&request(&out_of_order, &[], usize::MAX)),
        Err(EvictionError::OutOfOrderToolResult)
    );

    let same_item = vec![assistant(vec![
        result("call-1", "ok"),
        call("call-1", "shell_exec"),
    ])];
    assert_eq!(
        evict(&request(&same_item, &[], usize::MAX)),
        Err(EvictionError::OutOfOrderToolResult)
    );

    let in_flight = vec![assistant(vec![call("call-1", "shell_exec")])];
    assert_eq!(
        evict(&request(&in_flight, &[], usize::MAX)),
        Err(EvictionError::InFlightToolCall)
    );

    let cross_scope = vec![
        assistant(vec![call("call-cross", "shell_exec")]),
        assistant(vec![call("call-outer", "shell_exec")]),
        tool(vec![Part::ToolResult(ToolResultPart::success(
            "call-outer",
            ToolOutput::Parts(vec![result("call-cross", "wrong container")]),
        ))]),
    ];
    assert_eq!(
        evict(&request(&cross_scope, &[], usize::MAX)),
        Err(EvictionError::InFlightToolCall)
    );

    let cross_item_nested_scope = vec![
        assistant(vec![call("call-outer-a", "shell_exec")]),
        tool(vec![Part::ToolResult(ToolResultPart::success(
            "call-outer-a",
            ToolOutput::Parts(vec![call("call-cross", "nested")]),
        ))]),
        assistant(vec![call("call-outer-b", "shell_exec")]),
        tool(vec![Part::ToolResult(ToolResultPart::success(
            "call-outer-b",
            ToolOutput::Parts(vec![result("call-cross", "wrong nested container")]),
        ))]),
    ];
    assert!(evict(&request(&cross_item_nested_scope, &[], usize::MAX)).is_ok());

    let oversized = "c".repeat(MAX_TOOL_CALL_ID_BYTES + 1);
    for malformed in ["", oversized.as_str()] {
        let items = vec![
            assistant(vec![call(malformed, "shell_exec")]),
            tool(vec![result(malformed, "ok")]),
        ];
        assert_eq!(
            evict(&request(&items, &[], usize::MAX)),
            Err(EvictionError::InvalidToolCallId)
        );
    }

    let unicode = vec![
        assistant(vec![call("工具-调用-1", "shell_exec")]),
        tool(vec![result("工具-调用-1", "ok")]),
    ];
    assert!(evict(&request(&unicode, &[], usize::MAX)).is_ok());
}

#[test]
fn facts_are_bounded_unique_and_explicitly_keyed() {
    let source = transcript();

    let duplicated = vec![
        fact(
            "call-log",
            EffectStatus::Succeeded,
            FactClassification::StaleRawLog,
        ),
        fact(
            "call-log",
            EffectStatus::Succeeded,
            FactClassification::SuccessfulCommandNoise,
        ),
    ];
    assert_eq!(
        evict(&request(&source, &duplicated, 0)),
        Err(EvictionError::DuplicateFact)
    );

    let oversized = "k".repeat(MAX_FACT_KEY_BYTES + 1);
    for malformed in ["", oversized.as_str(), "key\u{0007}"] {
        assert_eq!(
            FactKey::parse(malformed),
            Err(EvictionError::InvalidFactKey)
        );
    }

    // A fact for a call that is not in this transcript has no effect.
    let absent = vec![fact(
        "call-absent",
        EffectStatus::Succeeded,
        FactClassification::StaleRawLog,
    )];
    let plan = evict(&request(&source, &absent, 0)).unwrap();
    assert!(evicted_call_ids(&source, plan.plan()).is_empty());
}

#[test]
fn payload_bytes_are_bounded_before_serialization() {
    let attacker = "x".repeat(1024);
    let mut metadata = BTreeMap::new();
    metadata.insert(attacker.clone(), json!(null));
    let fixtures = vec![
        vec![text_item(ItemKind::User, &attacker)],
        vec![Item::new(
            ItemKind::User,
            vec![Part::File(FilePart::named(
                attacker.clone(),
                DataRef::uri(attacker.clone()),
            ))],
        )],
        vec![text_item(ItemKind::User, "bounded").with_metadata(metadata)],
        vec![
            text_item(ItemKind::Assistant, "bounded")
                .with_usage(Usage::default().with_cost(CostUsage::new(1.0, attacker.clone()))),
        ],
    ];

    for source in fixtures {
        let mut bounded = request(&source, &[], usize::MAX);
        bounded.limits = EvictionLimits {
            max_canonical_bytes: 32,
            ..limits()
        };
        assert_eq!(
            evict(&bounded),
            Err(EvictionError::LimitExceeded(EvictionLimit::CanonicalBytes))
        );
    }

    let output = vec![
        assistant(vec![call("call-output", "shell_exec")]),
        tool(vec![result("call-output", &attacker)]),
    ];
    let mut bounded = request(&output, &[], usize::MAX);
    bounded.limits = EvictionLimits {
        max_tool_output_bytes: 32,
        ..limits()
    };
    assert_eq!(
        evict(&bounded),
        Err(EvictionError::LimitExceeded(EvictionLimit::ToolOutputBytes))
    );

    let envelope_only = vec![Item::new(ItemKind::User, Vec::new())];
    let mut exact_canonical = request(&envelope_only, &[], usize::MAX);
    exact_canonical.limits = EvictionLimits {
        max_canonical_bytes: 1,
        ..limits()
    };
    assert_eq!(
        evict(&exact_canonical),
        Err(EvictionError::LimitExceeded(EvictionLimit::CanonicalBytes)),
        "the exact canonical envelope is bounded after preflight"
    );
}

#[test]
fn nested_reasoning_payload_is_excluded_before_every_limit_check() {
    let private = "private reasoning payload ".repeat(1_024);
    let mut deep_metadata = json!("private leaf");
    for _ in 0..8 {
        deep_metadata = json!({ "nested": deep_metadata });
    }
    let private_reasoning = Part::Reasoning(ReasoningPart {
        summary: Some(private.clone()),
        data: Some(DataRef::inline_text(private.clone())),
        redacted: false,
        metadata: BTreeMap::from([
            ("large".to_owned(), json!(private.clone())),
            ("deep".to_owned(), deep_metadata.clone()),
        ]),
    });
    let private_source = vec![
        assistant(vec![call("call-private", "shell_exec")]),
        tool(vec![Part::ToolResult(ToolResultPart::success(
            "call-private",
            ToolOutput::Parts(vec![Part::text("visible output"), private_reasoning]),
        ))]),
    ];
    let mut bounded_private = request(&private_source, &[], usize::MAX);
    bounded_private.limits = EvictionLimits {
        max_json_depth: 4,
        max_canonical_bytes: 2_048,
        max_tool_output_bytes: 256,
        ..limits()
    };

    let private_outcome = evict(&bounded_private).unwrap();
    let candidate = rendered(private_outcome.plan().candidate());
    assert!(candidate.contains("visible output"));
    assert!(!candidate.contains("private reasoning payload"));
    assert!(!candidate.contains("private leaf"));

    let mut part_bounded = bounded_private;
    part_bounded.limits.max_visited_parts = 3;
    assert_eq!(
        evict(&part_bounded),
        Err(EvictionError::LimitExceeded(EvictionLimit::VisitedParts)),
        "reasoning still consumes one bounded part and path entry"
    );

    let depth_source = vec![
        assistant(vec![call("call-depth", "shell_exec")]),
        tool(vec![Part::ToolResult(ToolResultPart::success(
            "call-depth",
            ToolOutput::Parts(vec![reasoning("nested private depth")]),
        ))]),
    ];
    let mut depth_bounded = request(&depth_source, &[], usize::MAX);
    depth_bounded.limits.max_part_depth = 1;
    assert_eq!(
        evict(&depth_bounded),
        Err(EvictionError::LimitExceeded(EvictionLimit::PartDepth)),
        "reasoning still consumes bounded nesting depth"
    );

    for nonreasoning in [
        Part::text(private.clone()),
        Part::File(FilePart::new(DataRef::inline_text(private.clone()))),
    ] {
        let source = vec![
            assistant(vec![call("call-large", "shell_exec")]),
            tool(vec![Part::ToolResult(ToolResultPart::success(
                "call-large",
                ToolOutput::Parts(vec![nonreasoning]),
            ))]),
        ];
        let mut bounded = request(&source, &[], usize::MAX);
        bounded.limits.max_tool_output_bytes = 256;
        assert_eq!(
            evict(&bounded),
            Err(EvictionError::LimitExceeded(EvictionLimit::ToolOutputBytes))
        );
    }

    let deep_source = vec![
        assistant(vec![call("call-deep", "shell_exec")]),
        tool(vec![Part::ToolResult(ToolResultPart::success(
            "call-deep",
            ToolOutput::Parts(vec![Part::Text(
                TextPart::new("visible output")
                    .with_metadata(BTreeMap::from([("deep".to_owned(), deep_metadata)])),
            )]),
        ))]),
    ];
    let mut json_depth_bounded = request(&deep_source, &[], usize::MAX);
    json_depth_bounded.limits.max_json_depth = 4;
    assert_eq!(
        evict(&json_depth_bounded),
        Err(EvictionError::LimitExceeded(EvictionLimit::JsonDepth))
    );

    let exact_output_source = vec![
        assistant(vec![call("call-exact", "shell_exec")]),
        tool(vec![result("call-exact", "")]),
    ];
    let mut exact_output = request(&exact_output_source, &[], usize::MAX);
    exact_output.limits.max_tool_output_bytes = 1;
    assert_eq!(
        evict(&exact_output),
        Err(EvictionError::LimitExceeded(EvictionLimit::ToolOutputBytes)),
        "exact ToolOutput serialization is checked after bounded preflight"
    );
}

#[test]
fn every_explicit_bound_is_validated_before_any_work() {
    let source = transcript();
    let facts = facts();

    for invalid in [
        EvictionLimits {
            max_items: 0,
            ..limits()
        },
        EvictionLimits {
            max_visited_parts: 0,
            ..limits()
        },
        EvictionLimits {
            max_part_depth: 0,
            ..limits()
        },
        EvictionLimits {
            max_part_depth: MAX_SUPPORTED_PART_DEPTH + 1,
            ..limits()
        },
        EvictionLimits {
            max_json_depth: 0,
            ..limits()
        },
        EvictionLimits {
            max_json_depth: MAX_SUPPORTED_JSON_DEPTH + 1,
            ..limits()
        },
        EvictionLimits {
            max_canonical_bytes: 0,
            ..limits()
        },
        EvictionLimits {
            max_tool_output_bytes: 0,
            ..limits()
        },
        EvictionLimits {
            max_token_estimate: 0,
            ..limits()
        },
    ] {
        let mut request = request(&source, &facts, 0);
        request.limits = invalid;
        assert_eq!(evict(&request), Err(EvictionError::InvalidLimits));
    }

    let nested = vec![
        assistant(vec![call("call-1", "shell_exec")]),
        tool(vec![Part::ToolResult(ToolResultPart::success(
            "call-1",
            ToolOutput::Parts(vec![Part::text("nested")]),
        ))]),
    ];

    let empty_nested = vec![
        assistant(vec![call("call-empty", "shell_exec")]),
        tool(vec![Part::ToolResult(ToolResultPart::success(
            "call-empty",
            ToolOutput::Parts(Vec::new()),
        ))]),
    ];
    let mut one_level = request(&empty_nested, &[], usize::MAX);
    one_level.limits = EvictionLimits {
        max_part_depth: 1,
        ..limits()
    };
    assert!(evict(&one_level).is_ok());

    for (limits, expected) in [
        (
            EvictionLimits {
                max_items: 1,
                ..limits()
            },
            EvictionLimit::Items,
        ),
        (
            EvictionLimits {
                max_visited_parts: 1,
                ..limits()
            },
            EvictionLimit::VisitedParts,
        ),
        (
            EvictionLimits {
                max_part_depth: 1,
                ..limits()
            },
            EvictionLimit::PartDepth,
        ),
        (
            EvictionLimits {
                max_tool_output_bytes: 4,
                ..limits()
            },
            EvictionLimit::ToolOutputBytes,
        ),
        (
            EvictionLimits {
                max_canonical_bytes: 16,
                ..limits()
            },
            EvictionLimit::CanonicalBytes,
        ),
        (
            EvictionLimits {
                max_token_estimate: 1,
                ..limits()
            },
            EvictionLimit::TokenEstimate,
        ),
    ] {
        let mut request = request(&nested, &[], usize::MAX);
        request.limits = limits;
        assert_eq!(evict(&request), Err(EvictionError::LimitExceeded(expected)));
    }

    let mut bounded_facts = request(&source, &facts, 0);
    bounded_facts.limits = EvictionLimits {
        max_facts: 1,
        ..limits()
    };
    assert_eq!(
        evict(&bounded_facts),
        Err(EvictionError::LimitExceeded(EvictionLimit::Facts))
    );

    let malformed = vec![tool(vec![result("orphan", "must not be inspected")])];
    let mut early_fact_bound = request(&malformed, &facts, 0);
    early_fact_bound.limits = EvictionLimits {
        max_facts: 1,
        ..limits()
    };
    assert_eq!(
        evict(&early_fact_bound),
        Err(EvictionError::LimitExceeded(EvictionLimit::Facts)),
        "fact count is rejected before transcript traversal"
    );

    let empty_facts: Vec<OperationFact> = Vec::new();
    let mut zero_fact_limit = request(&source, &empty_facts, usize::MAX);
    zero_fact_limit.limits = EvictionLimits {
        max_facts: 0,
        ..limits()
    };
    assert!(evict(&zero_fact_limit).is_ok());

    let file_output = vec![
        assistant(vec![call("call-files", "read_many")]),
        tool(vec![Part::ToolResult(ToolResultPart::success(
            "call-files",
            ToolOutput::files(
                (0..5)
                    .map(|_| FilePart::new(DataRef::inline_text("")))
                    .collect(),
            ),
        ))]),
    ];
    let mut bounded_files = request(&file_output, &[], usize::MAX);
    bounded_files.limits = EvictionLimits {
        max_tool_output_bytes: 4,
        ..limits()
    };
    assert_eq!(
        evict(&bounded_files),
        Err(EvictionError::LimitExceeded(EvictionLimit::ToolOutputBytes))
    );

    let mut deeply_nested = json!(null);
    for _ in 0..5 {
        deeply_nested = json!([deeply_nested]);
    }
    let deep_json = vec![Item::new(
        ItemKind::Assistant,
        vec![Part::structured(deeply_nested)],
    )];
    let mut json_depth = request(&deep_json, &[], usize::MAX);
    json_depth.limits = EvictionLimits {
        max_json_depth: 4,
        ..limits()
    };
    assert_eq!(
        evict(&json_depth),
        Err(EvictionError::LimitExceeded(EvictionLimit::JsonDepth))
    );

    let non_finite = vec![
        Item::text(ItemKind::Assistant, "answer")
            .with_usage(Usage::default().with_cost(CostUsage::new(f64::NAN, "USD"))),
    ];
    assert_eq!(
        evict(&request(&non_finite, &[], usize::MAX)),
        Err(EvictionError::CanonicalSerialization)
    );
}

#[test]
fn reasoning_free_thresholds_preserve_physical_category_execution() {
    let source = vec![
        text_item(ItemKind::User, "goal"),
        assistant(vec![reasoning(REASONING), call("call-log", "shell_exec")]),
        tool(vec![result("call-log", &"stale output ".repeat(40))]),
    ];
    let facts = vec![fact(
        "call-log",
        EffectStatus::Succeeded,
        FactClassification::StaleRawLog,
    )];
    let reasoning_free = baseline_tokens(&source);
    assert!(reasoning_free > 0);

    let needs_stale = evict(&request(&source, &facts, reasoning_free - 1)).unwrap();
    assert!(needs_stale.fits());
    assert_eq!(
        evicted_call_ids(&source, needs_stale.plan()),
        ["call-log".to_owned()].into_iter().collect()
    );
    assert_eq!(
        categories(needs_stale.plan()),
        vec![
            EvictionCategory::StaleRawLog,
            EvictionCategory::StaleRawLog,
            EvictionCategory::ReasoningPart,
        ]
    );
    let candidate = rendered(needs_stale.plan().candidate());
    assert!(!candidate.contains("stale output"));
    assert!(!candidate.contains(REASONING));

    let skips_stale = evict(&request(&source, &facts, reasoning_free)).unwrap();
    assert!(skips_stale.fits());
    assert!(evicted_call_ids(&source, skips_stale.plan()).is_empty());
    assert_eq!(
        categories(skips_stale.plan()),
        vec![EvictionCategory::ReasoningPart]
    );
    let candidate = rendered(skips_stale.plan().candidate());
    assert!(candidate.contains("stale output"));
    assert!(!candidate.contains(REASONING));
}

#[test]
fn eviction_stops_as_soon_as_the_estimate_meets_the_target() {
    let source = transcript();
    let facts = facts();
    let baseline = baseline_tokens(&source);

    let outcome = evict(&request(&source, &facts, baseline - 1)).unwrap();
    let plan = outcome.plan();
    assert!(outcome.fits());
    assert!(plan.estimated_tokens() < baseline);
    assert_eq!(
        evicted_call_ids(&source, plan),
        ["call-log".to_owned()].into_iter().collect(),
        "only the first category's first unit is needed"
    );
    assert_eq!(
        categories(plan),
        vec![
            EvictionCategory::StaleRawLog,
            EvictionCategory::StaleRawLog,
            EvictionCategory::ReasoningPart,
        ],
        "category order reflects thresholded execution, not a global manifest sort"
    );

    let full = evict(&request(&source, &facts, 0)).unwrap();
    assert_eq!(evicted_call_ids(&source, full.plan()).len(), 4);
}

#[test]
fn overflow_fixtures_fit_whenever_eligible_material_suffices() {
    let source = transcript();
    let facts = facts();
    let baseline = baseline_tokens(&source);
    let floor = evict(&request(&source, &facts, 0))
        .unwrap()
        .plan()
        .estimated_tokens();

    for target in floor..=baseline {
        let outcome = evict(&request(&source, &facts, target)).unwrap();
        assert!(outcome.fits(), "target {target} must fit");
        assert!(outcome.plan().estimated_tokens() <= target);
    }
}

#[test]
fn an_irreducible_transcript_returns_typed_insufficiency() {
    let source = vec![
        text_item(ItemKind::User, "goal and acceptance criteria"),
        assistant(vec![reasoning(REASONING), call("call-fail", "shell_exec")]),
        tool(vec![result("call-fail", &"compile error\n".repeat(40))]),
        text_item(ItemKind::Assistant, "next action: fix the parser"),
    ];
    let facts = vec![fact(
        "call-fail",
        EffectStatus::Failed,
        FactClassification::StaleRawLog,
    )];

    let outcome = evict(&request(&source, &facts, 1)).unwrap();
    assert!(matches!(outcome, EvictionOutcome::Insufficient(_)));
    let plan = outcome.plan();
    assert!(plan.estimated_tokens() > plan.target_tokens());
    assert!(evicted_call_ids(&source, plan).is_empty());

    let candidate = rendered(plan.candidate());
    assert!(candidate.contains("goal and acceptance criteria"));
    assert!(candidate.contains("compile error"));
    assert!(candidate.contains("next action: fix the parser"));
    assert!(!candidate.contains(REASONING));
}

#[test]
fn a_second_application_to_the_candidate_is_a_fixed_point() {
    let source = transcript();
    let facts = facts();

    for target in [0, 1, baseline_tokens(&source) - 1] {
        let first = evict(&request(&source, &facts, target)).unwrap();
        let second = evict(&request(first.plan().candidate(), &facts, target)).unwrap();

        assert!(second.plan().removed().is_empty());
        assert_eq!(second.plan().candidate(), first.plan().candidate());
        assert_eq!(
            second.plan().candidate_digest(),
            first.plan().candidate_digest()
        );
        assert_eq!(
            second.plan().input_digest(),
            first.plan().candidate_digest()
        );
        assert_eq!(second.fits(), first.fits());
    }
}

#[test]
fn one_thousand_reruns_produce_exactly_one_candidate_and_eviction_digest() {
    let source = transcript();
    let facts = facts();
    let mut candidates = BTreeSet::new();
    let mut evictions = BTreeSet::new();

    for _ in 0..1000 {
        let outcome = evict(&request(&source, &facts, 0)).unwrap();
        let plan = outcome.plan();
        candidates.insert(plan.candidate_digest().to_string());
        evictions.insert(plan.eviction_digest().to_string());
    }

    assert_eq!(candidates.len(), 1);
    assert_eq!(evictions.len(), 1);
}

#[test]
fn fact_input_order_does_not_change_the_plan_or_digests() {
    let source = transcript();
    let facts = facts();
    let mut reversed = facts.clone();
    reversed.reverse();

    let forward = evict(&request(&source, &facts, 0)).unwrap();
    let backward = evict(&request(&source, &reversed, 0)).unwrap();

    assert_eq!(forward.plan(), backward.plan());
}

#[test]
fn the_eviction_digest_binds_the_target_and_the_removal_manifest() {
    let source = transcript();
    let facts = facts();

    let full = evict(&request(&source, &facts, 0)).unwrap();
    let partial = evict(&request(&source, &facts, baseline_tokens(&source) - 1)).unwrap();

    assert_eq!(full.plan().input_digest(), partial.plan().input_digest());
    assert_ne!(
        full.plan().candidate_digest(),
        partial.plan().candidate_digest()
    );
    assert_ne!(
        full.plan().eviction_digest(),
        partial.plan().eviction_digest()
    );

    // Same candidate, different target: the eviction digest still differs.
    let empty = evict(&request(&source, &[], 0)).unwrap();
    let empty_high = evict(&request(&source, &[], 1)).unwrap();
    assert_eq!(
        empty.plan().candidate_digest(),
        empty_high.plan().candidate_digest()
    );
    assert_ne!(
        empty.plan().eviction_digest(),
        empty_high.plan().eviction_digest()
    );

    // Same input, target, and candidate with a different typed classification:
    // only the ordered removal manifest distinguishes these plans.
    let pair = exchange("call-only", "shell_exec", "completed output").to_vec();
    let stale = vec![fact(
        "call-only",
        EffectStatus::Succeeded,
        FactClassification::StaleRawLog,
    )];
    let noise = vec![fact(
        "call-only",
        EffectStatus::Succeeded,
        FactClassification::SuccessfulCommandNoise,
    )];
    let stale = evict(&request(&pair, &stale, 0)).unwrap();
    let noise = evict(&request(&pair, &noise, 0)).unwrap();
    assert_eq!(stale.plan().input_digest(), noise.plan().input_digest());
    assert_eq!(
        stale.plan().candidate_digest(),
        noise.plan().candidate_digest()
    );
    assert_ne!(stale.plan().removed(), noise.plan().removed());
    assert_ne!(
        stale.plan().eviction_digest(),
        noise.plan().eviction_digest()
    );
}

#[test]
fn near_threshold_selection_ignores_reasoning_summary_data_and_metadata() {
    fn source_with(reasoning: ReasoningPart) -> Vec<Item> {
        let mut source = transcript();
        source[1].parts[0] = Part::Reasoning(reasoning);
        source
    }

    let short_source = source_with(ReasoningPart {
        summary: Some("short private summary".to_owned()),
        data: Some(DataRef::inline_text("short private data")),
        redacted: false,
        metadata: BTreeMap::from([("private".to_owned(), json!("short metadata"))]),
    });
    let long_private = "long private reasoning ".repeat(2_048);
    let long_source = source_with(ReasoningPart {
        summary: Some(long_private.clone()),
        data: Some(DataRef::inline_text(format!("{long_private} data"))),
        redacted: false,
        metadata: BTreeMap::from([("private".to_owned(), json!(long_private))]),
    });
    let stale_fact = vec![fact(
        "call-log",
        EffectStatus::Succeeded,
        FactClassification::StaleRawLog,
    )];
    let target = evict(&request(&short_source, &stale_fact, 0))
        .unwrap()
        .plan()
        .estimated_tokens();
    let facts = facts();
    let short_before = short_source.clone();
    let long_before = long_source.clone();

    let short = evict(&request(&short_source, &facts, target)).unwrap();
    let long = evict(&request(&long_source, &facts, target)).unwrap();

    assert!(short.fits());
    assert!(long.fits());
    assert_eq!(short.plan().estimated_tokens(), target);
    assert_eq!(long.plan().estimated_tokens(), target);
    assert_eq!(short.plan().removed(), long.plan().removed());
    assert_eq!(short.plan().candidate(), long.plan().candidate());
    assert_eq!(short.plan().input_digest(), long.plan().input_digest());
    assert_eq!(
        short.plan().candidate_digest(),
        long.plan().candidate_digest()
    );
    assert_eq!(
        short.plan().eviction_digest(),
        long.plan().eviction_digest()
    );
    assert_eq!(
        categories(short.plan()),
        vec![
            EvictionCategory::StaleRawLog,
            EvictionCategory::StaleRawLog,
            EvictionCategory::ReasoningPart,
        ]
    );
    assert_eq!(short_source, short_before);
    assert_eq!(long_source, long_before);
}
