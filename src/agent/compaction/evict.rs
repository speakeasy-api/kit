//! Deterministic bounded transcript eviction (M009-W03).
//!
//! Eviction is pure. It borrows a transcript, works on a copy, and returns a
//! candidate plus a candidate digest and an eviction digest. The source
//! transcript, durable events, and artifacts are never mutated, and no
//! summarization or other semantic compaction happens here.
//!
//! Every category except reasoning is driven by explicit typed facts supplied
//! by the caller from authoritative state. Prose, tool names, and model
//! authority are never inspected. Reasoning parts carry no eviction fact: they
//! are stripped unconditionally, as is the typed reasoning-token usage field,
//! so no reasoning material reaches the candidate, any digest, or the removal
//! manifest.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::{self, Write},
};

use agentkit_core::{
    DataRef, FilePart, FinishReason, Item, Part, ToolCallId, ToolOutput, ToolResultPart, Usage,
};
use agentkit_loop::MutationPoint;
use serde::Serialize;
use serde_json::Value;

use crate::{
    agent::{
        agentkit_bridge::mapping::{CanonicalItem, from_agentkit_item},
        compaction::states::CheckpointMutationPoint,
        context::estimate_tokens,
        driver::restart::EffectStatus,
    },
    domain::events::ContentDigest,
};

/// Bound on the nesting depth this module supports at all, independent of the
/// caller's limit, so recursive traversal can never grow the stack unbounded.
pub const MAX_SUPPORTED_PART_DEPTH: usize = 8;
pub const MAX_SUPPORTED_JSON_DEPTH: usize = 64;
pub const MAX_TOOL_CALL_ID_BYTES: usize = 256;
pub const MAX_FACT_KEY_BYTES: usize = 256;

/// Algorithm version bound into both digests. Any change to category semantics,
/// ordering, or digest serialization requires a new version.
pub const EVICTION_ALGORITHM_VERSION: u16 = 4;

const CANDIDATE_DIGEST_DOMAIN: &[u8] = b"KIT-COMPACT-EVICT-CANDIDATE\0";
const EVICTION_DIGEST_DOMAIN: &[u8] = b"KIT-COMPACT-EVICT-MANIFEST\0";
const TOOL_CALL_ID_DIGEST_DOMAIN: &[u8] = b"KIT-COMPACT-EVICT-TOOL-CALL-ID\0";

/// Explicit caller-supplied bounds. Flat counts and nonreasoning structure are
/// checked before candidate cloning; exact serialization is checked afterward
/// on the reasoning-free projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EvictionLimits {
    pub max_items: usize,
    pub max_visited_parts: usize,
    pub max_part_depth: usize,
    pub max_json_depth: usize,
    pub max_facts: usize,
    pub max_canonical_bytes: usize,
    pub max_tool_output_bytes: usize,
    pub max_token_estimate: usize,
}

impl EvictionLimits {
    fn validate(self) -> Result<(), EvictionError> {
        if self.max_items == 0
            || self.max_visited_parts == 0
            || self.max_part_depth == 0
            || self.max_part_depth > MAX_SUPPORTED_PART_DEPTH
            || self.max_json_depth == 0
            || self.max_json_depth > MAX_SUPPORTED_JSON_DEPTH
            || self.max_canonical_bytes == 0
            || self.max_tool_output_bytes == 0
            || self.max_token_estimate == 0
        {
            return Err(EvictionError::InvalidLimits);
        }
        Ok(())
    }
}

/// Which explicit bound a rejected input exceeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvictionLimit {
    Items,
    VisitedParts,
    PartDepth,
    JsonDepth,
    Facts,
    CanonicalBytes,
    ToolOutputBytes,
    TokenEstimate,
}

impl fmt::Display for EvictionLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Items => "transcript item count",
            Self::VisitedParts => "recursively visited part count",
            Self::PartDepth => "part nesting depth",
            Self::JsonDepth => "JSON nesting depth",
            Self::Facts => "eviction fact count",
            Self::CanonicalBytes => "canonical transcript bytes",
            Self::ToolOutputBytes => "tool output bytes",
            Self::TokenEstimate => "transcript token estimate",
        })
    }
}

/// Bounded explicit equivalence or map key. Control, bidirectional-override,
/// and zero-width characters are rejected so a key cannot spoof diagnostics.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FactKey(String);

impl FactKey {
    pub fn parse(value: &str) -> Result<Self, EvictionError> {
        if value.is_empty()
            || value.len() > MAX_FACT_KEY_BYTES
            || !value
                .chars()
                .all(|character| character == ' ' || character.is_ascii_graphic())
        {
            return Err(EvictionError::InvalidFactKey);
        }
        Ok(Self(value.to_owned()))
    }
}

/// Why an operation's transcript material is evictable. Each variant carries
/// the explicit key the deterministic rule needs; nothing is inferred.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FactClassification {
    /// Raw log output whose durable record lives outside model context.
    StaleRawLog,
    /// A repository or workspace map for `map_key` at `generation`.
    SupersededMap { map_key: FactKey, generation: u64 },
    /// A read whose content is identified by `equivalence_key`.
    DuplicateRead { equivalence_key: FactKey },
    /// A command that completed successfully and carries no active failure.
    SuccessfulCommandNoise,
}

impl FactClassification {
    const fn category(&self) -> EvictionCategory {
        match self {
            Self::StaleRawLog => EvictionCategory::StaleRawLog,
            Self::SupersededMap { .. } => EvictionCategory::SupersededMap,
            Self::DuplicateRead { .. } => EvictionCategory::DuplicateRead,
            Self::SuccessfulCommandNoise => EvictionCategory::SuccessfulCommandNoise,
        }
    }
}

/// One explicit typed fact about one tool call, supplied from authoritative
/// state rather than derived from transcript prose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationFact {
    pub tool_call_id: ToolCallId,
    pub outcome: EffectStatus,
    pub classification: FactClassification,
}

/// The deterministic category order: stale raw log, superseded map, duplicate
/// read, reasoning part, successful command noise. The derived `Ord` matches
/// the execution order.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvictionCategory {
    StaleRawLog,
    SupersededMap,
    DuplicateRead,
    ReasoningPart,
    SuccessfulCommandNoise,
}

impl EvictionCategory {
    pub const ORDER: [Self; 5] = [
        Self::StaleRawLog,
        Self::SupersededMap,
        Self::DuplicateRead,
        Self::ReasoningPart,
        Self::SuccessfulCommandNoise,
    ];
}

/// One removed part, addressed by its position in the source transcript. The
/// entry carries no removed content, so no reasoning byte can reach it.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct RemovedPart {
    pub category: EvictionCategory,
    pub item_index: usize,
    pub part_path: Vec<usize>,
    pub tool_call_id_digest: Option<ContentDigest>,
}

/// A source-relative address of one part, nested paths included.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PartPath {
    item_index: usize,
    part_path: Vec<usize>,
}

impl PartPath {
    fn new(item_index: usize, prefix: &[usize]) -> Self {
        Self {
            item_index,
            part_path: prefix.to_vec(),
        }
    }

    fn has_ancestor_in(&self, paths: &BTreeSet<Self>) -> bool {
        let mut ancestor = self.clone();
        while ancestor.part_path.len() > 1 {
            ancestor.part_path.pop();
            if paths.contains(&ancestor) {
                return true;
            }
        }
        false
    }
}

/// One atomic removal unit: a tool call and the tool result it pairs with are
/// always removed together, so eviction never retains an orphan.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct EvictionUnit {
    category: EvictionCategory,
    call_path: PartPath,
    call_id: ToolCallId,
    result_path: PartPath,
}

/// One eviction request over borrowed inputs.
#[derive(Clone, Copy)]
pub struct EvictionRequest<'a> {
    pub transcript: &'a [Item],
    pub facts: &'a [OperationFact],
    pub mutation_point: CheckpointMutationPoint,
    pub target_tokens: usize,
    pub limits: EvictionLimits,
}

/// The eviction result. Both variants carry the full plan; `Insufficient`
/// means every eligible unit was consumed and the protected remainder still
/// exceeds the target, so the caller must escalate rather than discard facts.
#[derive(Clone, Debug, PartialEq)]
pub enum EvictionOutcome {
    Fits(EvictionPlan),
    Insufficient(EvictionPlan),
}

impl EvictionOutcome {
    pub const fn fits(&self) -> bool {
        matches!(self, Self::Fits(_))
    }

    pub const fn plan(&self) -> &EvictionPlan {
        match self {
            Self::Fits(plan) | Self::Insufficient(plan) => plan,
        }
    }
}

/// The candidate transcript, its digests, and the ordered removal manifest.
#[derive(Clone, PartialEq)]
pub struct EvictionPlan {
    mutation_point: CheckpointMutationPoint,
    target_tokens: usize,
    estimated_tokens: usize,
    input_digest: ContentDigest,
    candidate_digest: ContentDigest,
    eviction_digest: ContentDigest,
    candidate: Vec<Item>,
    removed: Vec<RemovedPart>,
}

impl fmt::Debug for EvictionPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvictionPlan")
            .field("mutation_point", &self.mutation_point)
            .field("target_tokens", &self.target_tokens)
            .field("estimated_tokens", &self.estimated_tokens)
            .field("input_digest", &self.input_digest)
            .field("candidate_digest", &self.candidate_digest)
            .field("eviction_digest", &self.eviction_digest)
            .field("candidate_items", &self.candidate.len())
            .field("removed", &self.removed)
            .finish()
    }
}

impl EvictionPlan {
    pub const fn mutation_point(&self) -> CheckpointMutationPoint {
        self.mutation_point
    }

    pub const fn target_tokens(&self) -> usize {
        self.target_tokens
    }

    pub const fn estimated_tokens(&self) -> usize {
        self.estimated_tokens
    }

    pub const fn input_digest(&self) -> &ContentDigest {
        &self.input_digest
    }

    pub const fn candidate_digest(&self) -> &ContentDigest {
        &self.candidate_digest
    }

    pub const fn eviction_digest(&self) -> &ContentDigest {
        &self.eviction_digest
    }

    pub fn candidate(&self) -> &[Item] {
        &self.candidate
    }

    pub fn removed(&self) -> &[RemovedPart] {
        &self.removed
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvictionError {
    InvalidLimits,
    LimitExceeded(EvictionLimit),
    UnsupportedMutationPoint,
    InvalidToolCallId,
    InvalidFactKey,
    DuplicateFact,
    NonSuccessfulCommandNoise(EffectStatus),
    FactOutcomeMismatch,
    DuplicateToolCall,
    DuplicateToolResult,
    OrphanToolResult,
    OutOfOrderToolResult,
    InFlightToolCall,
    CandidatePairInvariant,
    CanonicalSerialization,
}

impl fmt::Display for EvictionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => f.write_str("eviction limits are unsupported"),
            Self::LimitExceeded(limit) => write!(f, "eviction input exceeds its {limit} bound"),
            Self::UnsupportedMutationPoint => {
                f.write_str("eviction runs only after a tool result or turn end")
            }
            Self::InvalidToolCallId => write!(
                f,
                "tool call id must contain 1 to {MAX_TOOL_CALL_ID_BYTES} UTF-8 bytes"
            ),
            Self::InvalidFactKey => write!(
                f,
                "fact key must contain 1 to {MAX_FACT_KEY_BYTES} visible ASCII characters or spaces"
            ),
            Self::DuplicateFact => f.write_str("one tool call carries at most one eviction fact"),
            Self::NonSuccessfulCommandNoise(outcome) => {
                write!(
                    f,
                    "a {} operation is never successful command noise",
                    effect_status_name(*outcome)
                )
            }
            Self::FactOutcomeMismatch => {
                f.write_str("a succeeded eviction fact cannot name an error result")
            }
            Self::DuplicateToolCall => f.write_str("tool call id is not unique in the transcript"),
            Self::DuplicateToolResult => f.write_str("tool call id has more than one tool result"),
            Self::OrphanToolResult => f.write_str("tool result has no matching tool call"),
            Self::OutOfOrderToolResult => {
                f.write_str("tool result does not follow its tool call in the transcript")
            }
            Self::InFlightToolCall => f.write_str("tool call has no tool result yet"),
            Self::CandidatePairInvariant => {
                f.write_str("candidate transcript broke a tool call/result pair")
            }
            Self::CanonicalSerialization => {
                f.write_str("transcript has no canonical serialization")
            }
        }
    }
}

impl std::error::Error for EvictionError {}

/// Eviction accepts only the two W02 checkpoint mutation points; every other
/// upstream mutation point fails closed.
pub fn accept_mutation_point(
    point: MutationPoint,
) -> Result<CheckpointMutationPoint, EvictionError> {
    CheckpointMutationPoint::from_agentkit(point)
        .map_err(|_| EvictionError::UnsupportedMutationPoint)
}

/// Deterministically evict low-risk material until the token estimate meets
/// the target, leaving the source transcript byte-identical.
pub fn evict(request: &EvictionRequest<'_>) -> Result<EvictionOutcome, EvictionError> {
    let limits = request.limits;
    limits.validate()?;
    if request.transcript.len() > limits.max_items {
        return Err(EvictionError::LimitExceeded(EvictionLimit::Items));
    }
    if request.facts.len() > limits.max_facts {
        return Err(EvictionError::LimitExceeded(EvictionLimit::Facts));
    }
    let facts = validate_facts(request.facts)?;

    let inspection = inspect(request.transcript, &limits)?;
    validate_pairs(&inspection)?;

    // Preflight bounded every nonreasoning field before this first clone.
    // Threshold estimates omit reasoning without putting it in the physical
    // removal set before its category executes.
    let mut estimation_removed: BTreeSet<PartPath> = inspection.reasoning.iter().cloned().collect();
    let estimation_candidate = build_candidate(
        request.transcript,
        &estimation_removed,
        limits.max_part_depth,
    )?;
    let estimation_canonical = projection_canonical_bytes(&estimation_candidate, &limits)?;
    if token_estimate(&estimation_canonical)? > limits.max_token_estimate {
        return Err(EvictionError::LimitExceeded(EvictionLimit::TokenEstimate));
    }
    let units = plan_units(&facts, &inspection)?;
    let mut estimation = Some((estimation_candidate, estimation_canonical));

    let estimation_canonical = &estimation
        .as_ref()
        .expect("estimation is present before reasoning executes")
        .1;
    let input_digest = transcript_digest(estimation_canonical);
    let mut actual_removed = BTreeSet::new();
    let mut manifest = Vec::new();
    let mut estimated_tokens = token_estimate(estimation_canonical)?;
    let mut materialized = None;

    for category in EvictionCategory::ORDER {
        let mut category_manifest = Vec::new();
        if category == EvictionCategory::ReasoningPart {
            for path in &inspection.reasoning {
                let has_removed_ancestor = path.has_ancestor_in(&actual_removed);
                actual_removed.insert(path.clone());
                if !has_removed_ancestor {
                    category_manifest.push((path, None));
                }
            }
            materialized = estimation.take();
        } else {
            for unit in units.iter().filter(|unit| unit.category == category) {
                if estimated_tokens <= request.target_tokens {
                    break;
                }
                actual_removed.insert(unit.call_path.clone());
                actual_removed.insert(unit.result_path.clone());
                estimation_removed.insert(unit.call_path.clone());
                estimation_removed.insert(unit.result_path.clone());
                category_manifest.push((&unit.call_path, Some(&unit.call_id)));
                category_manifest.push((&unit.result_path, Some(&unit.call_id)));
                let removed = if category < EvictionCategory::ReasoningPart {
                    &estimation_removed
                } else {
                    &actual_removed
                };
                let candidate =
                    build_candidate(request.transcript, removed, limits.max_part_depth)?;
                let candidate_canonical = projection_canonical_bytes(&candidate, &limits)?;
                estimated_tokens = token_estimate(&candidate_canonical)?;
                if category < EvictionCategory::ReasoningPart {
                    estimation = Some((candidate, candidate_canonical));
                } else {
                    materialized = Some((candidate, candidate_canonical));
                }
            }
        }

        // Selection stays pair-atomic; only this executed category's manifest
        // entries are canonicalized by source path and opaque call id.
        category_manifest.sort();
        manifest.extend(
            category_manifest.into_iter().map(|(path, call_id)| {
                removed_part(category, path, call_id.map(tool_call_id_digest))
            }),
        );
    }

    let (candidate, candidate_canonical) =
        materialized.expect("reasoning is present in the eviction category order");

    validate_pairs(&inspect(&candidate, &limits)?)
        .map_err(|_| EvictionError::CandidatePairInvariant)?;

    let candidate_digest = transcript_digest(&candidate_canonical);
    let eviction_digest = eviction_digest(
        &input_digest,
        &candidate_digest,
        request.mutation_point,
        request.target_tokens,
        &manifest,
    )?;

    let plan = EvictionPlan {
        mutation_point: request.mutation_point,
        target_tokens: request.target_tokens,
        estimated_tokens,
        input_digest,
        candidate_digest,
        eviction_digest,
        candidate,
        removed: manifest,
    };
    Ok(if estimated_tokens <= request.target_tokens {
        EvictionOutcome::Fits(plan)
    } else {
        EvictionOutcome::Insufficient(plan)
    })
}

#[derive(Default)]
struct Inspection {
    calls: BTreeMap<ToolCallId, InspectedCall>,
    results: BTreeMap<ToolCallId, InspectedResult>,
    reasoning: Vec<PartPath>,
    visited_parts: usize,
    visited_json_nodes: usize,
    payload_bytes: usize,
}

struct InspectedCall {
    path: PartPath,
}

struct InspectedResult {
    path: PartPath,
    is_error: bool,
}

/// Bounded recursive traversal. Depth and visited-part counters are checked
/// before each part is examined, so no clone or recursion is unbounded.
fn inspect(transcript: &[Item], limits: &EvictionLimits) -> Result<Inspection, EvictionError> {
    let mut inspection = Inspection::default();
    let mut output_budgets = Vec::new();
    for (item_index, item) in transcript.iter().enumerate() {
        account_payload(1, limits, &mut inspection, &mut output_budgets)?;
        if let Some(id) = &item.id {
            account_payload(id.0.len(), limits, &mut inspection, &mut output_budgets)?;
        }
        inspect_metadata(&item.metadata, limits, &mut inspection, &mut output_budgets)?;
        if let Some(usage) = &item.usage {
            account_payload(1, limits, &mut inspection, &mut output_budgets)?;
            if usage
                .cost
                .as_ref()
                .is_some_and(|cost| !cost.amount.is_finite())
            {
                return Err(EvictionError::CanonicalSerialization);
            }
            if let Some(cost) = &usage.cost {
                account_payload(
                    cost.currency.len(),
                    limits,
                    &mut inspection,
                    &mut output_budgets,
                )?;
                if let Some(provider_amount) = &cost.provider_amount {
                    account_payload(
                        provider_amount.len(),
                        limits,
                        &mut inspection,
                        &mut output_budgets,
                    )?;
                }
            }
            inspect_metadata(
                &usage.metadata,
                limits,
                &mut inspection,
                &mut output_budgets,
            )?;
        }
        if let Some(FinishReason::Other(reason)) = &item.finish_reason {
            account_payload(reason.len(), limits, &mut inspection, &mut output_budgets)?;
        }
        let mut prefix = Vec::new();
        inspect_parts(
            &item.parts,
            item_index,
            &mut prefix,
            1,
            limits,
            &mut inspection,
            &mut output_budgets,
        )?;
    }
    Ok(inspection)
}

fn inspect_parts(
    parts: &[Part],
    item_index: usize,
    prefix: &mut Vec<usize>,
    depth: usize,
    limits: &EvictionLimits,
    inspection: &mut Inspection,
    output_budgets: &mut Vec<usize>,
) -> Result<(), EvictionError> {
    if parts.is_empty() {
        return Ok(());
    }
    if depth > limits.max_part_depth {
        return Err(EvictionError::LimitExceeded(EvictionLimit::PartDepth));
    }
    for (part_index, part) in parts.iter().enumerate() {
        inspection.visited_parts = inspection
            .visited_parts
            .checked_add(1)
            .ok_or(EvictionError::LimitExceeded(EvictionLimit::VisitedParts))?;
        if inspection.visited_parts > limits.max_visited_parts {
            return Err(EvictionError::LimitExceeded(EvictionLimit::VisitedParts));
        }
        prefix.push(part_index);
        if matches!(part, Part::Reasoning(_)) {
            inspection.reasoning.push(PartPath::new(item_index, prefix));
            prefix.pop();
            continue;
        }
        account_payload(1, limits, inspection, output_budgets)?;
        let protocol_level = prefix.len() == 1;
        match part {
            Part::Text(part) => {
                account_payload(part.text.len(), limits, inspection, output_budgets)?;
                inspect_metadata(&part.metadata, limits, inspection, output_budgets)?;
            }
            Part::Media(part) => {
                account_payload(part.mime_type.len(), limits, inspection, output_budgets)?;
                inspect_data_ref(&part.data, limits, inspection, output_budgets)?;
                inspect_metadata(&part.metadata, limits, inspection, output_budgets)?;
            }
            Part::File(part) => {
                inspect_file(part, limits, inspection, output_budgets)?;
            }
            Part::Structured(part) => {
                inspect_json(&part.value, limits, inspection, output_budgets)?;
                if let Some(schema) = &part.schema {
                    inspect_json(schema, limits, inspection, output_budgets)?;
                }
                inspect_metadata(&part.metadata, limits, inspection, output_budgets)?;
            }
            Part::Reasoning(_) => {
                unreachable!("reasoning parts are handled without payload access")
            }
            Part::ToolCall(call) => {
                account_payload(call.id.0.len(), limits, inspection, output_budgets)?;
                account_payload(call.name.len(), limits, inspection, output_budgets)?;
                inspect_json(&call.input, limits, inspection, output_budgets)?;
                inspect_metadata(&call.metadata, limits, inspection, output_budgets)?;
                if protocol_level {
                    let call_id = tool_call_id(&call.id)?;
                    if inspection
                        .calls
                        .insert(
                            call_id,
                            InspectedCall {
                                path: PartPath::new(item_index, prefix),
                            },
                        )
                        .is_some()
                    {
                        return Err(EvictionError::DuplicateToolCall);
                    }
                }
            }
            Part::ToolResult(result) => {
                account_payload(result.call_id.0.len(), limits, inspection, output_budgets)?;
                inspect_metadata(&result.metadata, limits, inspection, output_budgets)?;
                if protocol_level {
                    let call_id = tool_call_id(&result.call_id)?;
                    if inspection
                        .results
                        .insert(
                            call_id,
                            InspectedResult {
                                path: PartPath::new(item_index, prefix),
                                is_error: result.is_error,
                            },
                        )
                        .is_some()
                    {
                        return Err(EvictionError::DuplicateToolResult);
                    }
                }
                output_budgets.push(0);
                account_payload(1, limits, inspection, output_budgets)?;
                if let ToolOutput::Parts(nested) = &result.output {
                    let nested_depth = depth
                        .checked_add(1)
                        .ok_or(EvictionError::LimitExceeded(EvictionLimit::PartDepth))?;
                    inspect_parts(
                        nested,
                        item_index,
                        prefix,
                        nested_depth,
                        limits,
                        inspection,
                        output_budgets,
                    )?;
                } else if let ToolOutput::Structured(value) = &result.output {
                    inspect_json(value, limits, inspection, output_budgets)?;
                } else if let ToolOutput::Text(text) = &result.output {
                    account_payload(text.len(), limits, inspection, output_budgets)?;
                } else if let ToolOutput::Files(files) = &result.output {
                    if files.len() > limits.max_tool_output_bytes {
                        return Err(EvictionError::LimitExceeded(EvictionLimit::ToolOutputBytes));
                    }
                    for file in files {
                        inspect_file(file, limits, inspection, output_budgets)?;
                    }
                }
                output_budgets.pop();
            }
            Part::Custom(part) => {
                account_payload(part.kind.len(), limits, inspection, output_budgets)?;
                if let Some(data) = &part.data {
                    inspect_data_ref(data, limits, inspection, output_budgets)?;
                }
                if let Some(value) = &part.value {
                    inspect_json(value, limits, inspection, output_budgets)?;
                }
                inspect_metadata(&part.metadata, limits, inspection, output_budgets)?;
            }
        }
        prefix.pop();
    }
    Ok(())
}

fn inspect_metadata(
    metadata: &BTreeMap<String, Value>,
    limits: &EvictionLimits,
    inspection: &mut Inspection,
    output_budgets: &mut [usize],
) -> Result<(), EvictionError> {
    for (key, value) in metadata {
        account_payload(key.len(), limits, inspection, output_budgets)?;
        inspect_json(value, limits, inspection, output_budgets)?;
    }
    Ok(())
}

fn inspect_file(
    file: &FilePart,
    limits: &EvictionLimits,
    inspection: &mut Inspection,
    output_budgets: &mut [usize],
) -> Result<(), EvictionError> {
    account_payload(1, limits, inspection, output_budgets)?;
    if let Some(name) = &file.name {
        account_payload(name.len(), limits, inspection, output_budgets)?;
    }
    if let Some(mime_type) = &file.mime_type {
        account_payload(mime_type.len(), limits, inspection, output_budgets)?;
    }
    inspect_data_ref(&file.data, limits, inspection, output_budgets)?;
    inspect_metadata(&file.metadata, limits, inspection, output_budgets)
}

fn inspect_data_ref(
    data: &DataRef,
    limits: &EvictionLimits,
    inspection: &mut Inspection,
    output_budgets: &mut [usize],
) -> Result<(), EvictionError> {
    account_payload(1, limits, inspection, output_budgets)?;
    let bytes = match data {
        DataRef::InlineText(value) | DataRef::Uri(value) => value.len(),
        DataRef::InlineBytes(value) => value.len(),
        DataRef::Handle(value) => value.0.len(),
    };
    account_payload(bytes, limits, inspection, output_budgets)
}

fn account_payload(
    bytes: usize,
    limits: &EvictionLimits,
    inspection: &mut Inspection,
    output_budgets: &mut [usize],
) -> Result<(), EvictionError> {
    inspection.payload_bytes = inspection
        .payload_bytes
        .checked_add(bytes)
        .ok_or(EvictionError::LimitExceeded(EvictionLimit::CanonicalBytes))?;
    if inspection.payload_bytes > limits.max_canonical_bytes {
        return Err(EvictionError::LimitExceeded(EvictionLimit::CanonicalBytes));
    }
    for budget in output_budgets {
        *budget = budget
            .checked_add(bytes)
            .ok_or(EvictionError::LimitExceeded(EvictionLimit::ToolOutputBytes))?;
        if *budget > limits.max_tool_output_bytes {
            return Err(EvictionError::LimitExceeded(EvictionLimit::ToolOutputBytes));
        }
    }
    Ok(())
}

/// Iterative preflight keeps serde and canonical mapping away from unbounded
/// JSON recursion. The canonical-byte limit also bounds scheduled JSON nodes.
fn inspect_json(
    value: &Value,
    limits: &EvictionLimits,
    inspection: &mut Inspection,
    output_budgets: &mut [usize],
) -> Result<(), EvictionError> {
    let mut stack = vec![(value, 1_usize)];
    while let Some((value, depth)) = stack.pop() {
        if depth > limits.max_json_depth {
            return Err(EvictionError::LimitExceeded(EvictionLimit::JsonDepth));
        }
        inspection.visited_json_nodes = inspection
            .visited_json_nodes
            .checked_add(1)
            .ok_or(EvictionError::LimitExceeded(EvictionLimit::CanonicalBytes))?;
        if inspection.visited_json_nodes > limits.max_canonical_bytes {
            return Err(EvictionError::LimitExceeded(EvictionLimit::CanonicalBytes));
        }
        account_payload(1, limits, inspection, output_budgets)?;
        let children: &[Value] = match value {
            Value::Array(values) => values,
            Value::Object(values) => {
                for key in values.keys() {
                    account_payload(key.len(), limits, inspection, output_budgets)?;
                }
                let child_count = values.len();
                let scheduled = inspection
                    .visited_json_nodes
                    .checked_add(stack.len())
                    .and_then(|count| count.checked_add(child_count))
                    .ok_or(EvictionError::LimitExceeded(EvictionLimit::CanonicalBytes))?;
                if scheduled > limits.max_canonical_bytes {
                    return Err(EvictionError::LimitExceeded(EvictionLimit::CanonicalBytes));
                }
                let child_depth = depth
                    .checked_add(1)
                    .ok_or(EvictionError::LimitExceeded(EvictionLimit::JsonDepth))?;
                stack
                    .try_reserve(child_count)
                    .map_err(|_| EvictionError::LimitExceeded(EvictionLimit::CanonicalBytes))?;
                stack.extend(values.values().map(|value| (value, child_depth)));
                continue;
            }
            Value::String(value) => {
                account_payload(value.len(), limits, inspection, output_budgets)?;
                continue;
            }
            _ => continue,
        };
        let child_count = children.len();
        let scheduled = inspection
            .visited_json_nodes
            .checked_add(stack.len())
            .and_then(|count| count.checked_add(child_count))
            .ok_or(EvictionError::LimitExceeded(EvictionLimit::CanonicalBytes))?;
        if scheduled > limits.max_canonical_bytes {
            return Err(EvictionError::LimitExceeded(EvictionLimit::CanonicalBytes));
        }
        let child_depth = depth
            .checked_add(1)
            .ok_or(EvictionError::LimitExceeded(EvictionLimit::JsonDepth))?;
        stack
            .try_reserve(child_count)
            .map_err(|_| EvictionError::LimitExceeded(EvictionLimit::CanonicalBytes))?;
        stack.extend(children.iter().map(|value| (value, child_depth)));
    }
    Ok(())
}

/// Every tool call id is unique and pairs with exactly one tool result at a
/// strictly later transcript position. Malformed, duplicate, orphaned,
/// out-of-order, and in-flight pairs fail closed.
fn validate_pairs(inspection: &Inspection) -> Result<(), EvictionError> {
    for (call_id, result) in &inspection.results {
        let call_path = inspection
            .calls
            .get(call_id)
            .ok_or(EvictionError::OrphanToolResult)?;
        if call_path.path >= result.path {
            return Err(EvictionError::OutOfOrderToolResult);
        }
    }
    for call_id in inspection.calls.keys() {
        if !inspection.results.contains_key(call_id) {
            return Err(EvictionError::InFlightToolCall);
        }
    }
    Ok(())
}

fn tool_call_id(id: &ToolCallId) -> Result<ToolCallId, EvictionError> {
    if id.0.is_empty() || id.0.len() > MAX_TOOL_CALL_ID_BYTES {
        return Err(EvictionError::InvalidToolCallId);
    }
    Ok(id.clone())
}

struct ValidatedFact<'a> {
    fact: &'a OperationFact,
    call_id: ToolCallId,
}

fn validate_facts(facts: &[OperationFact]) -> Result<Vec<ValidatedFact<'_>>, EvictionError> {
    let mut seen = BTreeSet::new();
    let mut validated = Vec::with_capacity(facts.len());
    for fact in facts {
        let call_id = tool_call_id(&fact.tool_call_id)?;
        if !seen.insert(call_id.clone()) {
            return Err(EvictionError::DuplicateFact);
        }
        if matches!(
            fact.classification,
            FactClassification::SuccessfulCommandNoise
        ) && !effect_succeeded(fact.outcome)
        {
            return Err(EvictionError::NonSuccessfulCommandNoise(fact.outcome));
        }
        validated.push(ValidatedFact { fact, call_id });
    }
    Ok(validated)
}

/// Resolve explicit facts into ordered atomic removal units.
///
/// A fact whose operation did not succeed is never eligible, and a fact whose
/// tool call is absent from this transcript has no effect, so re-applying the
/// same fact set to a candidate is a fixed point.
fn plan_units(
    facts: &[ValidatedFact<'_>],
    inspection: &Inspection,
) -> Result<Vec<EvictionUnit>, EvictionError> {
    let mut units = Vec::new();
    let mut reads: BTreeMap<FactKey, Vec<EvictionUnit>> = BTreeMap::new();
    let mut maps: BTreeMap<FactKey, Vec<(u64, EvictionUnit)>> = BTreeMap::new();

    for validated in facts {
        let fact = validated.fact;
        let call_id = &validated.call_id;
        if !effect_succeeded(fact.outcome) {
            continue;
        }
        if inspection
            .results
            .get(call_id)
            .is_some_and(|result| result.is_error)
        {
            return Err(EvictionError::FactOutcomeMismatch);
        }
        let Some(unit) = resolve_unit(fact.classification.category(), call_id, inspection) else {
            continue;
        };
        match &fact.classification {
            FactClassification::StaleRawLog | FactClassification::SuccessfulCommandNoise => {
                units.push(unit);
            }
            FactClassification::DuplicateRead { equivalence_key } => {
                reads.entry(equivalence_key.clone()).or_default().push(unit)
            }
            FactClassification::SupersededMap {
                map_key,
                generation,
            } => maps
                .entry(map_key.clone())
                .or_default()
                .push((*generation, unit)),
        }
    }

    // A duplicate-read group retains its newest completed occurrence.
    for mut group in reads.into_values() {
        group.sort_by(|left, right| {
            (&left.result_path, &left.call_id).cmp(&(&right.result_path, &right.call_id))
        });
        group.pop();
        units.extend(group);
    }

    // A superseded-map group retains its newest generation, then its latest
    // completed occurrence.
    for mut group in maps.into_values() {
        group.sort_by(|(left_generation, left), (right_generation, right)| {
            (left_generation, &left.result_path, &left.call_id).cmp(&(
                right_generation,
                &right.result_path,
                &right.call_id,
            ))
        });
        group.pop();
        units.extend(group.into_iter().map(|(_, unit)| unit));
    }

    units.sort();
    Ok(units)
}

fn resolve_unit(
    category: EvictionCategory,
    call_id: &ToolCallId,
    inspection: &Inspection,
) -> Option<EvictionUnit> {
    let call = inspection.calls.get(call_id)?;
    let result = inspection.results.get(call_id)?;
    let call_path = call.path.clone();
    let result_path = result.path.clone();
    Some(EvictionUnit {
        category,
        call_path,
        call_id: call_id.clone(),
        result_path,
    })
}

fn removed_part(
    category: EvictionCategory,
    path: &PartPath,
    tool_call_id_digest: Option<ContentDigest>,
) -> RemovedPart {
    RemovedPart {
        category,
        item_index: path.item_index,
        part_path: path.part_path.clone(),
        tool_call_id_digest,
    }
}

/// Build the candidate from the source and the source-relative removal set, so
/// removal is idempotent and no index shifts as parts disappear. Item envelopes
/// are retained even when every part is removed.
fn build_candidate(
    transcript: &[Item],
    removed: &BTreeSet<PartPath>,
    max_depth: usize,
) -> Result<Vec<Item>, EvictionError> {
    let mut candidate = Vec::with_capacity(transcript.len());
    for (item_index, item) in transcript.iter().enumerate() {
        let mut prefix = Vec::new();
        let parts = retain_parts(&item.parts, item_index, &mut prefix, 1, max_depth, removed)?;
        candidate.push(Item {
            id: item.id.clone(),
            kind: item.kind,
            parts,
            metadata: item.metadata.clone(),
            usage: reasoning_free_usage(&item.usage),
            finish_reason: item.finish_reason.clone(),
            created_at: item.created_at,
        });
    }
    Ok(candidate)
}

fn reasoning_free_usage(usage: &Option<Usage>) -> Option<Usage> {
    let mut usage = usage.clone();
    if let Some(tokens) = usage.as_mut().and_then(|usage| usage.tokens.as_mut()) {
        tokens.reasoning_tokens = None;
    }
    usage
}

fn retain_parts(
    parts: &[Part],
    item_index: usize,
    prefix: &mut Vec<usize>,
    depth: usize,
    max_depth: usize,
    removed: &BTreeSet<PartPath>,
) -> Result<Vec<Part>, EvictionError> {
    if parts.is_empty() {
        return Ok(Vec::new());
    }
    if depth > max_depth {
        return Err(EvictionError::LimitExceeded(EvictionLimit::PartDepth));
    }
    let mut kept = Vec::with_capacity(parts.len());
    for (part_index, part) in parts.iter().enumerate() {
        prefix.push(part_index);
        if !removed.contains(&PartPath::new(item_index, prefix)) {
            kept.push(match part {
                Part::ToolResult(result) => match &result.output {
                    ToolOutput::Parts(nested) => {
                        let nested_depth = depth
                            .checked_add(1)
                            .ok_or(EvictionError::LimitExceeded(EvictionLimit::PartDepth))?;
                        Part::ToolResult(ToolResultPart {
                            call_id: result.call_id.clone(),
                            output: ToolOutput::Parts(retain_parts(
                                nested,
                                item_index,
                                prefix,
                                nested_depth,
                                max_depth,
                                removed,
                            )?),
                            is_error: result.is_error,
                            metadata: result.metadata.clone(),
                        })
                    }
                    _ => part.clone(),
                },
                _ => part.clone(),
            });
        }
        prefix.pop();
    }
    Ok(kept)
}

fn projection_canonical_bytes(
    items: &[Item],
    limits: &EvictionLimits,
) -> Result<Vec<u8>, EvictionError> {
    validate_projection_tool_outputs(items, limits)?;
    canonical_bytes(items, limits.max_canonical_bytes)
}

/// Every individual output is measured exactly after reasoning is removed.
/// The separate counters keep this validation bounded even for nested outputs.
fn validate_projection_tool_outputs(
    items: &[Item],
    limits: &EvictionLimits,
) -> Result<(), EvictionError> {
    let mut visited_parts = 0_usize;
    for item in items {
        validate_projection_tool_output_parts(&item.parts, 1, limits, &mut visited_parts)?;
    }
    Ok(())
}

fn validate_projection_tool_output_parts(
    parts: &[Part],
    depth: usize,
    limits: &EvictionLimits,
    visited_parts: &mut usize,
) -> Result<(), EvictionError> {
    if parts.is_empty() {
        return Ok(());
    }
    if depth > limits.max_part_depth {
        return Err(EvictionError::LimitExceeded(EvictionLimit::PartDepth));
    }
    for part in parts {
        *visited_parts = visited_parts
            .checked_add(1)
            .ok_or(EvictionError::LimitExceeded(EvictionLimit::VisitedParts))?;
        if *visited_parts > limits.max_visited_parts {
            return Err(EvictionError::LimitExceeded(EvictionLimit::VisitedParts));
        }
        let Part::ToolResult(result) = part else {
            continue;
        };
        bounded_json_len(
            &result.output,
            limits.max_tool_output_bytes,
            EvictionLimit::ToolOutputBytes,
        )?;
        if let ToolOutput::Parts(nested) = &result.output {
            let nested_depth = depth
                .checked_add(1)
                .ok_or(EvictionError::LimitExceeded(EvictionLimit::PartDepth))?;
            validate_projection_tool_output_parts(nested, nested_depth, limits, visited_parts)?;
        }
    }
    Ok(())
}

/// Canonical bytes come from the existing agentkit bridge mapping, so the
/// digest never depends on an ad hoc encoding of upstream types.
fn canonical_bytes(items: &[Item], limit: usize) -> Result<Vec<u8>, EvictionError> {
    let canonical: Vec<CanonicalItem> = items.iter().map(from_agentkit_item).collect();
    bounded_json_bytes(&canonical, limit, EvictionLimit::CanonicalBytes)
}

fn token_estimate(canonical: &[u8]) -> Result<usize, EvictionError> {
    let canonical =
        std::str::from_utf8(canonical).map_err(|_| EvictionError::CanonicalSerialization)?;
    Ok(estimate_tokens(canonical))
}

const fn effect_succeeded(status: EffectStatus) -> bool {
    matches!(status, EffectStatus::Succeeded)
}

const fn effect_status_name(status: EffectStatus) -> &'static str {
    match status {
        EffectStatus::Succeeded => "succeeded",
        EffectStatus::Failed => "failed",
        EffectStatus::Cancelled => "cancelled",
        EffectStatus::AuthRequired => "auth_required",
        EffectStatus::OutcomeUnknown => "outcome_unknown",
    }
}

fn transcript_digest(canonical: &[u8]) -> ContentDigest {
    let mut hash = blake3::Hasher::new();
    hash.update(CANDIDATE_DIGEST_DOMAIN);
    hash.update(&EVICTION_ALGORITHM_VERSION.to_le_bytes());
    hash.update(canonical);
    digest(hash)
}

fn tool_call_id_digest(call_id: &ToolCallId) -> ContentDigest {
    let mut hash = blake3::Hasher::new();
    hash.update(TOOL_CALL_ID_DIGEST_DOMAIN);
    hash.update(&EVICTION_ALGORITHM_VERSION.to_le_bytes());
    hash.update(call_id.0.as_bytes());
    digest(hash)
}

#[derive(Serialize)]
struct EvictionDigestInput<'a> {
    algorithm_version: u16,
    input_digest: &'a ContentDigest,
    candidate_digest: &'a ContentDigest,
    mutation_point: CheckpointMutationPoint,
    target_tokens: usize,
    ordered_removals: &'a [RemovedPart],
}

fn eviction_digest(
    input_digest: &ContentDigest,
    candidate_digest: &ContentDigest,
    mutation_point: CheckpointMutationPoint,
    target_tokens: usize,
    removed: &[RemovedPart],
) -> Result<ContentDigest, EvictionError> {
    let mut hash = blake3::Hasher::new();
    hash.update(EVICTION_DIGEST_DOMAIN);
    let mut writer = DigestWriter {
        hash: &mut hash,
        bytes: 0,
    };
    serde_json::to_writer(
        &mut writer,
        &EvictionDigestInput {
            algorithm_version: EVICTION_ALGORITHM_VERSION,
            input_digest,
            candidate_digest,
            mutation_point,
            target_tokens,
            ordered_removals: removed,
        },
    )
    .map_err(|_| EvictionError::CanonicalSerialization)?;
    Ok(digest(hash))
}

struct CappedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

struct CappedCounter {
    bytes: usize,
    limit: usize,
    exceeded: bool,
}

impl Write for CappedCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next) = self.bytes.checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("canonical byte limit exceeded"));
        };
        if next > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("canonical byte limit exceeded"));
        }
        self.bytes = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounded_json_len<T: Serialize + ?Sized>(
    value: &T,
    limit: usize,
    limit_kind: EvictionLimit,
) -> Result<usize, EvictionError> {
    let mut writer = CappedCounter {
        bytes: 0,
        limit,
        exceeded: false,
    };
    if serde_json::to_writer(&mut writer, value).is_err() {
        return if writer.exceeded {
            Err(EvictionError::LimitExceeded(limit_kind))
        } else {
            Err(EvictionError::CanonicalSerialization)
        };
    }
    Ok(writer.bytes)
}

impl CappedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(4096)),
            limit,
            exceeded: false,
        }
    }
}

impl Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(next) = self.bytes.len().checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("canonical byte limit exceeded"));
        };
        if next > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("canonical byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounded_json_bytes<T: Serialize + ?Sized>(
    value: &T,
    limit: usize,
    limit_kind: EvictionLimit,
) -> Result<Vec<u8>, EvictionError> {
    let mut writer = CappedWriter::new(limit);
    if serde_json::to_writer(&mut writer, value).is_err() {
        return if writer.exceeded {
            Err(EvictionError::LimitExceeded(limit_kind))
        } else {
            Err(EvictionError::CanonicalSerialization)
        };
    }
    Ok(writer.bytes)
}

struct DigestWriter<'a> {
    hash: &'a mut blake3::Hasher,
    bytes: usize,
}

impl Write for DigestWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("eviction digest input length overflow"))?;
        self.hash.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn digest(hash: blake3::Hasher) -> ContentDigest {
    ContentDigest::parse(&format!("blake3:{}", hash.finalize().to_hex()))
        .expect("blake3 hexadecimal digests are canonical content digests")
}
