use std::cmp::Ordering;
use std::fmt::Write;

pub const FALLBACK_TOKEN_BUDGET: usize = 8_192;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ContextLayer {
    CanonicalPrompt,
    Repository,
    Checkpoint,
    Task,
    RecentTranscript,
    RetrievedEvidence,
    ToolResultDelta,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ContextPriority {
    Requirement,
    ActiveFailure,
    ChangedFile,
    UnresolvedDecision,
    Current,
    OldRawToolOutput,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ContextVisibility {
    Model,
    PrivateReasoning,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ContextItem {
    pub sequence: u64,
    pub content: String,
    pub artifact_handle: Option<String>,
    pub visibility: ContextVisibility,
}

impl ContextItem {
    pub fn model(sequence: u64, content: impl Into<String>) -> Self {
        Self {
            sequence,
            content: content.into(),
            artifact_handle: None,
            visibility: ContextVisibility::Model,
        }
    }

    pub fn artifact(
        sequence: u64,
        content: impl Into<String>,
        artifact_handle: impl Into<String>,
    ) -> Self {
        Self {
            sequence,
            content: content.into(),
            artifact_handle: Some(artifact_handle.into()),
            visibility: ContextVisibility::Model,
        }
    }

    pub fn private_reasoning(sequence: u64, content: impl Into<String>) -> Self {
        Self {
            sequence,
            content: content.into(),
            artifact_handle: None,
            visibility: ContextVisibility::PrivateReasoning,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContextBlock {
    pub layer: ContextLayer,
    pub priority: ContextPriority,
    pub source_handle: String,
    pub revision: String,
    pub retrieval_reason: String,
    pub relevance_score: Option<f64>,
    pub items: Vec<ContextItem>,
}

impl ContextBlock {
    pub fn new(
        layer: ContextLayer,
        priority: ContextPriority,
        source_handle: impl Into<String>,
        revision: impl Into<String>,
        retrieval_reason: impl Into<String>,
        items: Vec<ContextItem>,
    ) -> Self {
        Self {
            layer,
            priority,
            source_handle: source_handle.into(),
            revision: revision.into(),
            retrieval_reason: retrieval_reason.into(),
            relevance_score: None,
            items,
        }
    }

    pub fn with_relevance_score(mut self, relevance_score: Option<f64>) -> Self {
        self.relevance_score = relevance_score.filter(|score| score.is_finite());
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectionLimits {
    pub fallback_token_budget: usize,
    pub max_blocks: usize,
    pub max_items_per_block: usize,
    pub max_source_handle_bytes: usize,
    pub max_revision_bytes: usize,
    pub max_retrieval_reason_bytes: usize,
    pub max_score_bytes: usize,
    pub max_inline_bytes: usize,
    pub max_preview_bytes: usize,
    pub max_artifact_handle_bytes: usize,
}

impl Default for ProjectionLimits {
    fn default() -> Self {
        Self {
            fallback_token_budget: FALLBACK_TOKEN_BUDGET,
            max_blocks: 128,
            max_items_per_block: 32,
            max_source_handle_bytes: 512,
            max_revision_bytes: 256,
            max_retrieval_reason_bytes: 1_024,
            max_score_bytes: 32,
            max_inline_bytes: 4_096,
            max_preview_bytes: 512,
            max_artifact_handle_bytes: 512,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContextField {
    SourceHandle,
    Revision,
    RetrievalReason,
    RelevanceScore,
    ArtifactHandle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectionRejectionReason {
    FieldTooLarge {
        field: ContextField,
        actual_bytes: usize,
        max_bytes: usize,
    },
    NoModelVisibleItems,
    TokenBudgetExceeded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionRejection {
    pub layer: ContextLayer,
    pub priority: ContextPriority,
    pub reason: ProjectionRejectionReason,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlockProvenance {
    pub source_handle: String,
    pub revision: String,
    pub retrieval_reason: String,
    pub estimated_tokens: usize,
    pub relevance_score: Option<f64>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProjectedItem {
    pub sequence: u64,
    pub preview: String,
    pub artifact_handle: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProjectedBlock {
    pub layer: ContextLayer,
    pub priority: ContextPriority,
    pub provenance: BlockProvenance,
    pub items: Vec<ProjectedItem>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContextProjection {
    pub blocks: Vec<ProjectedBlock>,
    pub rejections: Vec<ProjectionRejection>,
    pub token_budget: usize,
    pub estimated_tokens: usize,
    pub used_fallback_budget: bool,
}

pub fn estimate_tokens(value: &str) -> usize {
    value.len().div_ceil(4)
}

/// Projects an already-canonical prompt without wrapping or duplicating its bytes.
pub fn project_canonical_prompt(
    source_handle: impl Into<String>,
    revision: impl Into<String>,
    rendered: &str,
    token_budget: usize,
) -> Result<ContextProjection, ProjectionRejectionReason> {
    let estimated_tokens = estimate_tokens(rendered);
    if estimated_tokens > token_budget {
        return Err(ProjectionRejectionReason::TokenBudgetExceeded);
    }
    let source_handle = source_handle.into();
    Ok(ContextProjection {
        blocks: vec![ProjectedBlock {
            layer: ContextLayer::CanonicalPrompt,
            priority: ContextPriority::Requirement,
            provenance: BlockProvenance {
                source_handle,
                revision: revision.into(),
                retrieval_reason: "canonical model request".to_owned(),
                estimated_tokens,
                relevance_score: None,
            },
            items: vec![ProjectedItem {
                sequence: 0,
                preview: rendered.to_owned(),
                artifact_handle: None,
            }],
        }],
        rejections: Vec::new(),
        token_budget,
        estimated_tokens,
        used_fallback_budget: false,
    })
}

pub fn render_context(projection: &ContextProjection) -> String {
    render_blocks(projection.blocks.iter())
}

pub fn project(
    source: &[ContextBlock],
    learned_token_budget: Option<usize>,
    limits: ProjectionLimits,
) -> ContextProjection {
    let (token_budget, used_fallback_budget) = learned_token_budget
        .map_or((limits.fallback_token_budget, true), |budget| {
            (budget, false)
        });
    let mut candidates = source.to_vec();
    candidates.sort_by(admission_order);

    let mut blocks = Vec::new();
    let mut rejections = Vec::new();
    for candidate in candidates {
        if blocks.len() == limits.max_blocks {
            break;
        }
        let layer = candidate.layer;
        let priority = candidate.priority;
        match prepare_block(candidate, limits) {
            Ok(mut block) => match fit_block(&blocks, &mut block, token_budget, limits) {
                Ok(()) => blocks.push(block),
                Err(reason) => rejections.push(ProjectionRejection {
                    layer,
                    priority,
                    reason,
                }),
            },
            Err(reason) => rejections.push(ProjectionRejection {
                layer,
                priority,
                reason,
            }),
        }
    }

    blocks.sort_by(render_order);
    let estimated_tokens = estimate_tokens(&render_blocks(blocks.iter()));
    assert!(estimated_tokens <= token_budget);
    ContextProjection {
        blocks,
        rejections,
        token_budget,
        estimated_tokens,
        used_fallback_budget,
    }
}

fn prepare_block(
    block: ContextBlock,
    limits: ProjectionLimits,
) -> Result<ProjectedBlock, ProjectionRejectionReason> {
    check_len(
        ContextField::SourceHandle,
        &block.source_handle,
        limits.max_source_handle_bytes,
    )?;
    check_len(
        ContextField::Revision,
        &block.revision,
        limits.max_revision_bytes,
    )?;
    check_len(
        ContextField::RetrievalReason,
        &block.retrieval_reason,
        limits.max_retrieval_reason_bytes,
    )?;
    let relevance_score = block.relevance_score.filter(|score| score.is_finite());
    if let Some(score) = relevance_score {
        check_len(
            ContextField::RelevanceScore,
            &score.to_string(),
            limits.max_score_bytes,
        )?;
    }

    let mut source_items = block
        .items
        .into_iter()
        .filter(|item| item.visibility == ContextVisibility::Model)
        .collect::<Vec<_>>();
    source_items.sort();
    source_items.truncate(limits.max_items_per_block);

    let items = source_items
        .into_iter()
        .map(|item| project_item(item, &block.source_handle, limits))
        .collect::<Result<Vec<_>, _>>()?;
    if items.is_empty() {
        return Err(ProjectionRejectionReason::NoModelVisibleItems);
    }

    let mut projected = ProjectedBlock {
        layer: block.layer,
        priority: block.priority,
        provenance: BlockProvenance {
            source_handle: block.source_handle,
            revision: block.revision,
            retrieval_reason: block.retrieval_reason,
            estimated_tokens: 0,
            relevance_score,
        },
        items,
    };
    refresh_block_estimate(&mut projected);
    Ok(projected)
}

fn project_item(
    item: ContextItem,
    source_handle: &str,
    limits: ProjectionLimits,
) -> Result<ProjectedItem, ProjectionRejectionReason> {
    let represented_as_artifact =
        item.artifact_handle.is_some() || item.content.len() > limits.max_inline_bytes;
    let preview_limit = if represented_as_artifact {
        limits.max_preview_bytes
    } else {
        limits.max_inline_bytes
    };
    let artifact_handle = if represented_as_artifact {
        Some(
            item.artifact_handle
                .unwrap_or_else(|| source_handle.to_owned()),
        )
    } else {
        None
    };
    if let Some(handle) = artifact_handle.as_deref() {
        check_len(
            ContextField::ArtifactHandle,
            handle,
            limits.max_artifact_handle_bytes,
        )?;
    }

    Ok(ProjectedItem {
        sequence: item.sequence,
        preview: truncate_utf8(&item.content, preview_limit).to_owned(),
        artifact_handle,
    })
}

fn fit_block(
    admitted: &[ProjectedBlock],
    block: &mut ProjectedBlock,
    token_budget: usize,
    limits: ProjectionLimits,
) -> Result<(), ProjectionRejectionReason> {
    while block.items.len() > 1 && combined_tokens(admitted, block) > token_budget {
        block.items.pop();
        refresh_block_estimate(block);
    }
    if combined_tokens(admitted, block) <= token_budget {
        return Ok(());
    }

    if block.items[0].artifact_handle.is_none() {
        check_len(
            ContextField::ArtifactHandle,
            &block.provenance.source_handle,
            limits.max_artifact_handle_bytes,
        )?;
        block.items[0].artifact_handle = Some(block.provenance.source_handle.clone());
        let preview_boundary =
            floor_char_boundary(&block.items[0].preview, limits.max_preview_bytes);
        block.items[0].preview.truncate(preview_boundary);
    }

    let preview = block.items[0].preview.clone();
    let mut boundaries = preview
        .char_indices()
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    boundaries.push(preview.len());
    let mut low = 0;
    let mut high = boundaries.len() - 1;
    while low < high {
        let middle = (low + high).div_ceil(2);
        block.items[0].preview = preview[..boundaries[middle]].to_owned();
        refresh_block_estimate(block);
        if combined_tokens(admitted, block) <= token_budget {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    block.items[0].preview = preview[..boundaries[low]].to_owned();
    refresh_block_estimate(block);

    (combined_tokens(admitted, block) <= token_budget)
        .then_some(())
        .ok_or(ProjectionRejectionReason::TokenBudgetExceeded)
}

fn check_len(
    field: ContextField,
    value: &str,
    max_bytes: usize,
) -> Result<(), ProjectionRejectionReason> {
    (value.len() <= max_bytes)
        .then_some(())
        .ok_or(ProjectionRejectionReason::FieldTooLarge {
            field,
            actual_bytes: value.len(),
            max_bytes,
        })
}

fn combined_tokens(admitted: &[ProjectedBlock], candidate: &ProjectedBlock) -> usize {
    estimate_tokens(&render_blocks(
        admitted.iter().chain(std::iter::once(candidate)),
    ))
}

fn refresh_block_estimate(block: &mut ProjectedBlock) {
    block.provenance.estimated_tokens = 0;
    loop {
        let estimate = estimate_tokens(&render_block(block));
        if estimate == block.provenance.estimated_tokens {
            return;
        }
        block.provenance.estimated_tokens = estimate;
    }
}

fn render_blocks<'a>(blocks: impl Iterator<Item = &'a ProjectedBlock>) -> String {
    let mut blocks = blocks.collect::<Vec<_>>();
    blocks.sort_by(|left, right| render_order(left, right));
    if blocks.is_empty() {
        return String::new();
    }

    let mut rendered = String::from("{\"blocks\":[");
    for (index, block) in blocks.into_iter().enumerate() {
        if index != 0 {
            rendered.push(',');
        }
        rendered.push_str(&render_block(block));
    }
    rendered.push_str("]}");
    rendered
}

fn render_block(block: &ProjectedBlock) -> String {
    let mut rendered = String::from("{\"layer\":");
    write_json_string(&mut rendered, layer_name(block.layer));
    rendered.push_str(",\"priority\":");
    write_json_string(&mut rendered, priority_name(block.priority));
    rendered.push_str(",\"provenance\":{\"source_handle\":");
    write_json_string(&mut rendered, &block.provenance.source_handle);
    rendered.push_str(",\"revision\":");
    write_json_string(&mut rendered, &block.provenance.revision);
    rendered.push_str(",\"retrieval_reason\":");
    write_json_string(&mut rendered, &block.provenance.retrieval_reason);
    rendered.push_str(",\"estimated_tokens\":");
    write!(rendered, "{}", block.provenance.estimated_tokens)
        .expect("writing to String cannot fail");
    rendered.push_str(",\"relevance_score\":");
    match block.provenance.relevance_score {
        Some(score) => write!(rendered, "{score}").expect("writing to String cannot fail"),
        None => rendered.push_str("null"),
    }
    rendered.push_str("},\"items\":[");
    for (index, item) in block.items.iter().enumerate() {
        if index != 0 {
            rendered.push(',');
        }
        rendered.push_str("{\"sequence\":");
        write!(rendered, "{}", item.sequence).expect("writing to String cannot fail");
        rendered.push_str(",\"preview\":");
        write_json_string(&mut rendered, &item.preview);
        rendered.push_str(",\"artifact_handle\":");
        match item.artifact_handle.as_deref() {
            Some(handle) => write_json_string(&mut rendered, handle),
            None => rendered.push_str("null"),
        }
        rendered.push('}');
    }
    rendered.push_str("]}");
    rendered
}

fn write_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{00}'..='\u{1f}' => {
                write!(output, "\\u{:04x}", character as u32)
                    .expect("writing to String cannot fail");
            }
            _ => output.push(character),
        }
    }
    output.push('"');
}

fn layer_name(layer: ContextLayer) -> &'static str {
    match layer {
        ContextLayer::CanonicalPrompt => "canonical_prompt",
        ContextLayer::Repository => "repository",
        ContextLayer::Checkpoint => "checkpoint",
        ContextLayer::Task => "task",
        ContextLayer::RecentTranscript => "recent_transcript",
        ContextLayer::RetrievedEvidence => "retrieved_evidence",
        ContextLayer::ToolResultDelta => "tool_result_delta",
    }
}

fn priority_name(priority: ContextPriority) -> &'static str {
    match priority {
        ContextPriority::Requirement => "requirement",
        ContextPriority::ActiveFailure => "active_failure",
        ContextPriority::ChangedFile => "changed_file",
        ContextPriority::UnresolvedDecision => "unresolved_decision",
        ContextPriority::Current => "current",
        ContextPriority::OldRawToolOutput => "old_raw_tool_output",
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    &value[..floor_char_boundary(value, max_bytes)]
}

fn floor_char_boundary(value: &str, max_bytes: usize) -> usize {
    let mut boundary = max_bytes.min(value.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

fn admission_order(left: &ContextBlock, right: &ContextBlock) -> Ordering {
    left.priority
        .cmp(&right.priority)
        .then_with(|| score_order(left.relevance_score, right.relevance_score))
        .then_with(|| left.layer.cmp(&right.layer))
        .then_with(|| left.source_handle.cmp(&right.source_handle))
        .then_with(|| left.revision.cmp(&right.revision))
        .then_with(|| left.retrieval_reason.cmp(&right.retrieval_reason))
        .then_with(|| left.items.cmp(&right.items))
}

fn render_order(left: &ProjectedBlock, right: &ProjectedBlock) -> Ordering {
    left.layer
        .cmp(&right.layer)
        .then_with(|| left.priority.cmp(&right.priority))
        .then_with(|| {
            score_order(
                left.provenance.relevance_score,
                right.provenance.relevance_score,
            )
        })
        .then_with(|| {
            left.provenance
                .source_handle
                .cmp(&right.provenance.source_handle)
        })
        .then_with(|| left.provenance.revision.cmp(&right.provenance.revision))
        .then_with(|| {
            left.provenance
                .retrieval_reason
                .cmp(&right.provenance.retrieval_reason)
        })
        .then_with(|| {
            left.items
                .iter()
                .map(|item| item.sequence)
                .cmp(right.items.iter().map(|item| item.sequence))
        })
        .then_with(|| left.items.cmp(&right.items))
}

fn score_order(left: Option<f64>, right: Option<f64>) -> Ordering {
    match (
        left.filter(|score| score.is_finite()),
        right.filter(|score| score.is_finite()),
    ) {
        (Some(left), Some(right)) => right.total_cmp(&left),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}
