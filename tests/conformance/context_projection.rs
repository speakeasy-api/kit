#[path = "../../src/agent/context/mod.rs"]
mod context;

use context::{
    ContextBlock, ContextField, ContextItem, ContextLayer, ContextPriority, ContextVisibility,
    FALLBACK_TOKEN_BUDGET, ProjectionLimits, ProjectionRejectionReason, estimate_tokens, project,
    project_canonical_prompt, render_context,
};

#[test]
fn canonical_prompt_projection_accounts_for_every_rendered_byte() {
    let rendered = "stable prefix\ndynamic task";
    let projection = project_canonical_prompt("prompt", "1", rendered, 100).unwrap();
    assert_eq!(projection.blocks.len(), 1);
    assert_eq!(projection.blocks[0].items[0].preview, rendered);
    assert_eq!(projection.estimated_tokens, estimate_tokens(rendered));
    assert!(projection.estimated_tokens <= projection.token_budget);
    assert!(project_canonical_prompt("prompt", "1", rendered, 1).is_err());
}

fn block(
    layer: ContextLayer,
    priority: ContextPriority,
    name: &str,
    content: impl Into<String>,
) -> ContextBlock {
    ContextBlock::new(
        layer,
        priority,
        format!("source:{name}"),
        "revision-7",
        format!("reason:{name}"),
        vec![ContextItem::model(0, content)],
    )
}

#[test]
fn projection_uses_rfc_layer_order_and_complete_provenance() {
    let source = vec![
        block(
            ContextLayer::ToolResultDelta,
            ContextPriority::Current,
            "delta",
            "delta",
        ),
        block(
            ContextLayer::Task,
            ContextPriority::Requirement,
            "task",
            "requirement",
        ),
        block(
            ContextLayer::CanonicalPrompt,
            ContextPriority::Current,
            "prompt",
            "policy and tools",
        ),
        block(
            ContextLayer::RetrievedEvidence,
            ContextPriority::Current,
            "evidence",
            "evidence",
        ),
        block(
            ContextLayer::Checkpoint,
            ContextPriority::ChangedFile,
            "checkpoint",
            "checkpoint",
        ),
        block(
            ContextLayer::RecentTranscript,
            ContextPriority::Current,
            "transcript",
            "recent",
        ),
        block(
            ContextLayer::Repository,
            ContextPriority::Current,
            "repository",
            "instructions and map",
        ),
    ];
    let projection = project(&source, Some(2_000), ProjectionLimits::default());

    assert_eq!(
        projection
            .blocks
            .iter()
            .map(|block| block.layer)
            .collect::<Vec<_>>(),
        vec![
            ContextLayer::CanonicalPrompt,
            ContextLayer::Repository,
            ContextLayer::Checkpoint,
            ContextLayer::Task,
            ContextLayer::RecentTranscript,
            ContextLayer::RetrievedEvidence,
            ContextLayer::ToolResultDelta,
        ]
    );
    assert!(projection.blocks.iter().all(|block| {
        !block.provenance.source_handle.is_empty()
            && !block.provenance.revision.is_empty()
            && !block.provenance.retrieval_reason.is_empty()
            && block.provenance.estimated_tokens > 0
    }));
    assert!(
        projection
            .blocks
            .iter()
            .all(|block| block.provenance.relevance_score.is_none())
    );
}

#[test]
fn priority_controls_admission_but_not_rfc_render_order() {
    let source = vec![
        block(
            ContextLayer::RecentTranscript,
            ContextPriority::OldRawToolOutput,
            "old",
            "old output",
        ),
        block(
            ContextLayer::Task,
            ContextPriority::Requirement,
            "requirement",
            "required",
        ),
        block(
            ContextLayer::ToolResultDelta,
            ContextPriority::ActiveFailure,
            "failure",
            "failed",
        ),
        block(
            ContextLayer::Checkpoint,
            ContextPriority::ChangedFile,
            "files",
            "src/lib.rs",
        ),
        block(
            ContextLayer::Checkpoint,
            ContextPriority::UnresolvedDecision,
            "decision",
            "choose API",
        ),
    ];
    let limits = ProjectionLimits {
        max_blocks: 4,
        ..ProjectionLimits::default()
    };
    let projection = project(&source, Some(1_000), limits);

    assert_eq!(projection.blocks.len(), 4);
    assert!(
        projection
            .blocks
            .iter()
            .all(|block| block.priority != ContextPriority::OldRawToolOutput)
    );
    assert!(
        projection
            .blocks
            .windows(2)
            .all(|pair| pair[0].layer <= pair[1].layer)
    );
}

#[test]
fn large_values_are_bounded_artifacts_and_private_reasoning_is_absent() {
    let source = vec![ContextBlock::new(
        ContextLayer::RetrievedEvidence,
        ContextPriority::Current,
        "artifact:source",
        "revision-9",
        "retrieved for failing symbol",
        vec![
            ContextItem::artifact(2, "a".repeat(10_000), "artifact:large"),
            ContextItem::model(1, "é".repeat(5_000)),
            ContextItem::private_reasoning(0, "hidden chain of thought"),
        ],
    )];
    let limits = ProjectionLimits {
        max_items_per_block: 2,
        max_inline_bytes: 64,
        max_preview_bytes: 17,
        ..ProjectionLimits::default()
    };
    let projection = project(&source, Some(1_000), limits);
    let items = &projection.blocks[0].items;

    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|item| item.preview.len() <= 17));
    assert_eq!(items[0].artifact_handle.as_deref(), Some("artifact:source"));
    assert_eq!(items[1].artifact_handle.as_deref(), Some("artifact:large"));
    assert!(!items.iter().any(|item| item.preview.contains("hidden")));
    assert_eq!(
        source[0].items[2].visibility,
        ContextVisibility::PrivateReasoning
    );
}

#[test]
fn fallback_budget_and_estimator_are_deterministic() {
    let limits = ProjectionLimits::default();
    let projection = project(
        &[block(
            ContextLayer::Task,
            ContextPriority::Requirement,
            "task",
            "12345",
        )],
        None,
        limits,
    );

    assert_eq!(estimate_tokens(""), 0);
    assert_eq!(estimate_tokens("12345"), 2);
    assert_eq!(projection.token_budget, FALLBACK_TOKEN_BUDGET);
    assert!(projection.used_fallback_budget);
    assert_eq!(
        projection.estimated_tokens,
        estimate_tokens(&render_context(&projection))
    );
}

#[test]
fn canonical_render_counts_labels_separators_escaping_and_provenance() {
    let source = vec![
        ContextBlock::new(
            ContextLayer::Task,
            ContextPriority::Requirement,
            "source:\"\\\n雪",
            "revision\t7",
            "because \"quoted\"\r\n",
            vec![ContextItem::artifact(
                u64::MAX,
                "payload \"\\\n\u{0001}🙂",
                "artifact:\"\\雪",
            )],
        )
        .with_relevance_score(Some(0.125)),
    ];
    let projection = project(&source, Some(1_000), ProjectionLimits::default());
    let rendered = render_context(&projection);

    assert_eq!(projection.blocks.len(), 1);
    assert!(rendered.starts_with("{\"blocks\":[{"));
    assert!(rendered.contains("\"source_handle\":\"source:\\\"\\\\\\n雪\""));
    assert!(rendered.contains("\\u0001"));
    assert!(rendered.contains("\"relevance_score\":0.125"));
    assert_eq!(projection.estimated_tokens, estimate_tokens(&rendered));
    assert!(projection.estimated_tokens <= projection.token_budget);
}

#[test]
fn oversized_provenance_and_artifact_handles_are_typed_rejections() {
    let limits = ProjectionLimits {
        max_source_handle_bytes: 8,
        max_revision_bytes: 8,
        max_retrieval_reason_bytes: 8,
        max_score_bytes: 4,
        max_artifact_handle_bytes: 8,
        ..ProjectionLimits::default()
    };
    let source = vec![
        ContextBlock::new(
            ContextLayer::Task,
            ContextPriority::Requirement,
            "s".repeat(1_000_000),
            "rev",
            "reason",
            vec![ContextItem::model(0, "content")],
        ),
        ContextBlock::new(
            ContextLayer::Task,
            ContextPriority::Requirement,
            "source",
            "rev",
            "reason",
            vec![ContextItem::artifact(0, "content", "a".repeat(9))],
        ),
        ContextBlock::new(
            ContextLayer::Task,
            ContextPriority::Requirement,
            "source",
            "rev",
            "reason",
            vec![ContextItem::model(0, "content")],
        )
        .with_relevance_score(Some(0.125)),
    ];
    let projection = project(&source, Some(1), limits);
    let rendered = render_context(&projection);

    assert!(projection.blocks.is_empty());
    assert!(rendered.is_empty());
    assert_eq!(projection.estimated_tokens, 0);
    assert!(projection.rejections.iter().any(|rejection| matches!(
        rejection.reason,
        ProjectionRejectionReason::FieldTooLarge {
            field: ContextField::SourceHandle,
            actual_bytes: 1_000_000,
            max_bytes: 8,
        }
    )));
    assert!(projection.rejections.iter().any(|rejection| matches!(
        rejection.reason,
        ProjectionRejectionReason::FieldTooLarge {
            field: ContextField::ArtifactHandle,
            actual_bytes: 9,
            max_bytes: 8,
        }
    )));
    assert!(projection.rejections.iter().any(|rejection| matches!(
        rejection.reason,
        ProjectionRejectionReason::FieldTooLarge {
            field: ContextField::RelevanceScore,
            max_bytes: 4,
            ..
        }
    )));
}

#[test]
fn one_thousand_randomized_projections_are_stable_and_within_budget() {
    let mut random = 0x6a09_e667_f3bc_c909_u64;
    for iteration in 0..1_000 {
        let mut source = (0..48)
            .map(|index| {
                random = random
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let layer = match random % 7 {
                    0 => ContextLayer::CanonicalPrompt,
                    1 => ContextLayer::Repository,
                    2 => ContextLayer::Checkpoint,
                    3 => ContextLayer::Task,
                    4 => ContextLayer::RecentTranscript,
                    5 => ContextLayer::RetrievedEvidence,
                    _ => ContextLayer::ToolResultDelta,
                };
                let priority = match (random >> 8) % 6 {
                    0 => ContextPriority::Requirement,
                    1 => ContextPriority::ActiveFailure,
                    2 => ContextPriority::ChangedFile,
                    3 => ContextPriority::UnresolvedDecision,
                    4 => ContextPriority::Current,
                    _ => ContextPriority::OldRawToolOutput,
                };
                let mut candidate = block(
                    layer,
                    priority,
                    &format!("{index:02}"),
                    format!(
                        "{}{}",
                        "x\"\\\n雪🙂\u{0002}".repeat((random as usize % 20) + 1),
                        index
                    ),
                )
                .with_relevance_score(
                    ((random & 1) == 0).then_some((random % 1_000) as f64 / 1_000.0),
                );
                candidate.revision = format!("rev:\"\\雪{index}");
                candidate.retrieval_reason = format!("reason\n🙂:{random}");
                if random.is_multiple_of(3) {
                    candidate.items.push(ContextItem::artifact(
                        1,
                        "artifact\u{0003}preview".repeat(20),
                        format!("artifact:\"\\{index}"),
                    ));
                }
                candidate
            })
            .collect::<Vec<_>>();
        for index in (1..source.len()).rev() {
            random = random
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            source.swap(index, random as usize % (index + 1));
        }

        let budget = iteration % 257;
        let limits = ProjectionLimits {
            max_blocks: 17,
            max_items_per_block: 3,
            max_inline_bytes: 96,
            max_preview_bytes: 24,
            ..ProjectionLimits::default()
        };
        let first = project(&source, Some(budget), limits);
        source.reverse();
        let second = project(&source, Some(budget), limits);
        let rendered = render_context(&first);

        assert!(first.estimated_tokens <= budget);
        assert_eq!(first.estimated_tokens, estimate_tokens(&rendered));
        assert!(first.blocks.len() <= limits.max_blocks);
        assert!(
            first
                .blocks
                .iter()
                .all(|block| block.items.len() <= limits.max_items_per_block)
        );
        assert_eq!(first, second);
    }
}
