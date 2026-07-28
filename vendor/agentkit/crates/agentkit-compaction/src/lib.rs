//! Transcript compaction primitives for reducing context size while preserving
//! useful state.
//!
//! This crate provides the building blocks for compacting an agent transcript
//! when it grows too large. The main concepts are:
//!
//! - **Compactors** ([`Compactor`]) decide *when* and *how* to compact in a
//!   single trait. Register one with the agent builder via
//!   [`AgentBuilderCompactorExt::compactor`].
//! - **Strategies** ([`CompactionStrategy`]) decide *how* the transcript is
//!   transformed: dropping reasoning, removing failed tool results, keeping
//!   only recent items, or summarising older items via a backend.
//! - **Pipelines** ([`CompactionPipeline`]) chain multiple strategies into a
//!   single pass.
//! - **Backends** ([`CompactionBackend`]) provide provider-backed
//!   summarisation for strategies that need it (e.g.
//!   [`SummarizeOlderStrategy`]).
//!
//! Wire a [`StrategyCompactor`] (bundling a trigger closure + strategy +
//! optional backend) into the loop, or implement [`Compactor`] directly for
//! stateful triggers.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use agentkit_core::{Item, ItemKind, MetadataMap, Part, SessionId, TurnCancellation};
use agentkit_loop::{
    Agent, AgentBuilder, AgentEvent, LoopCtx, LoopError, LoopMutator, LoopStep, ModelAdapter,
    MutationPoint, SessionConfig, TranscriptCursor,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The reason a compaction was triggered.
///
/// Returned by [`CompactionTrigger::should_compact`] and forwarded to
/// strategies so they can adapt their behaviour.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompactionReason {
    /// The transcript exceeded a configured item count.
    TranscriptTooLong,
    /// A caller explicitly requested compaction.
    Manual,
    /// An application-specific reason described by the inner string.
    Custom(String),
}

/// Input to a [`CompactionStrategy`]. Carries the transcript plus request
/// metadata so strategies can decide which items to keep, drop, or summarise.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionRequest {
    /// The transcript to compact.
    pub transcript: Vec<Item>,
    /// Why compaction was triggered.
    pub reason: CompactionReason,
    /// Arbitrary key-value metadata forwarded through the pipeline.
    pub metadata: MetadataMap,
}

impl CompactionRequest {
    /// Build a compaction request with empty metadata.
    pub fn new(transcript: Vec<Item>, reason: CompactionReason) -> Self {
        Self {
            transcript,
            reason,
            metadata: MetadataMap::new(),
        }
    }

    /// Replaces the request metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Output of a [`CompactionStrategy`].
///
/// Contains the compacted transcript along with metadata about what changed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompactionResult {
    /// The compacted transcript.
    pub transcript: Vec<Item>,
    /// How many items were removed or replaced during compaction.
    pub replaced_items: usize,
    /// Metadata produced by the strategy (e.g. summarisation statistics).
    pub metadata: MetadataMap,
}

impl CompactionResult {
    /// Builds a compaction result with empty metadata.
    pub fn new(transcript: Vec<Item>, replaced_items: usize) -> Self {
        Self {
            transcript,
            replaced_items,
            metadata: MetadataMap::new(),
        }
    }

    /// Replaces the result metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Request sent to a [`CompactionBackend`] asking it to summarise a set of
/// transcript items.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SummaryRequest {
    /// The transcript items to summarise.
    pub items: Vec<Item>,
    /// Why compaction was triggered.
    pub reason: CompactionReason,
    /// Arbitrary key-value metadata forwarded from the pipeline.
    pub metadata: MetadataMap,
}

impl SummaryRequest {
    /// Build a summary request with empty metadata.
    pub fn new(items: Vec<Item>, reason: CompactionReason) -> Self {
        Self {
            items,
            reason,
            metadata: MetadataMap::new(),
        }
    }

    /// Replaces the request metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Response from a [`CompactionBackend`] containing the summarised items.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SummaryResult {
    /// The summary items that replace the originals in the transcript.
    pub items: Vec<Item>,
    /// Metadata produced during summarisation (e.g. token counts).
    pub metadata: MetadataMap,
}

impl SummaryResult {
    /// Builds a summary result with empty metadata.
    pub fn new(items: Vec<Item>) -> Self {
        Self {
            items,
            metadata: MetadataMap::new(),
        }
    }

    /// Replaces the result metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Provider-backed summarisation service.
///
/// Implement this trait to connect a language model (or any other
/// summarisation service) so that strategies like [`SummarizeOlderStrategy`]
/// can condense older transcript items into a shorter summary.
///
/// # Errors
///
/// Implementations should return [`CompactionError::Failed`] when
/// summarisation cannot be completed, or [`CompactionError::Cancelled`] when
/// the cancellation token is signalled.
#[async_trait]
pub trait CompactionBackend: Send + Sync {
    /// Summarise the given items into a shorter set of replacement items.
    ///
    /// # Arguments
    ///
    /// * `request` - The items to summarise together with session context.
    /// * `cancellation` - An optional cancellation token; implementations
    ///   should check this periodically and bail early when cancelled.
    ///
    /// # Errors
    ///
    /// Returns [`CompactionError`] on failure or cancellation.
    async fn summarize(
        &self,
        request: SummaryRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<SummaryResult, CompactionError>;
}

/// High-level compaction primitive. Implementations decide whether and how
/// to compact, owning their own derived state (e.g. running token totals
/// behind interior mutability) so the framework doesn't need to plumb a
/// separate observer or shared atomic.
///
/// Wire a `Compactor` into the loop via [`AgentBuilderCompactorExt::compactor`],
/// which adapts it to a [`LoopMutator`] under the hood.
#[async_trait]
pub trait Compactor: Send + Sync {
    /// Decide whether to compact based on the current transcript and
    /// mutation point. Returning `None` is a no-op.
    fn should_compact(&self, transcript: &[Item], point: MutationPoint)
    -> Option<CompactionReason>;

    /// Produce the replacement transcript. Called only after
    /// [`should_compact`](Self::should_compact) returns `Some`.
    /// Implementations should respect `cancellation`.
    async fn compact(
        &self,
        transcript: &[Item],
        reason: CompactionReason,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Vec<Item>, CompactionError>;
}

/// Runtime context passed to each [`CompactionStrategy`] during execution.
///
/// Provides access to an optional [`CompactionBackend`] (needed by
/// [`SummarizeOlderStrategy`]), shared metadata, and a cancellation token
/// that strategies should respect.
pub struct CompactionContext<'a> {
    /// An optional backend for strategies that need to call an external
    /// summarisation service.
    pub backend: Option<&'a dyn CompactionBackend>,
    /// Cancellation token; strategies should check this and return
    /// [`CompactionError::Cancelled`] when signalled.
    pub cancellation: Option<TurnCancellation>,
}

impl<'a> CompactionContext<'a> {
    /// Build an empty context with no backend and no cancellation token.
    pub fn new() -> Self {
        Self {
            backend: None,
            cancellation: None,
        }
    }

    /// Attach a backend reference.
    pub fn with_backend(mut self, backend: &'a dyn CompactionBackend) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Attach a cancellation token.
    pub fn with_cancellation(mut self, cancellation: TurnCancellation) -> Self {
        self.cancellation = Some(cancellation);
        self
    }
}

impl Default for CompactionContext<'_> {
    fn default() -> Self {
        Self::new()
    }
}

/// A single compaction step that transforms a transcript.
///
/// Strategies are the core abstraction in this crate. Each strategy receives
/// the transcript inside a [`CompactionRequest`] and returns a
/// [`CompactionResult`] with the (possibly shorter) transcript.
///
/// Built-in strategies:
///
/// | Strategy | What it does |
/// |---|---|
/// | [`DropReasoningStrategy`] | Strips reasoning parts from items |
/// | [`DropFailedToolResultsStrategy`] | Removes errored tool results |
/// | [`KeepRecentStrategy`] | Keeps only the N most recent removable items |
/// | [`SummarizeOlderStrategy`] | Replaces older items with a backend-generated summary |
///
/// Use [`CompactionPipeline`] to chain multiple strategies together.
///
/// # Example
///
/// ```rust
/// use agentkit_compaction::DropReasoningStrategy;
///
/// // Strategies are composable via CompactionPipeline
/// let strategy = DropReasoningStrategy::new();
/// ```
#[async_trait]
pub trait CompactionStrategy: Send + Sync {
    /// Apply this strategy to the transcript in `request`.
    ///
    /// # Arguments
    ///
    /// * `request` - The transcript and session context to compact.
    /// * `ctx` - Runtime context providing the backend, metadata, and
    ///   cancellation token.
    ///
    /// # Errors
    ///
    /// Returns [`CompactionError`] on failure or cancellation.
    async fn apply(
        &self,
        request: CompactionRequest,
        ctx: &mut CompactionContext<'_>,
    ) -> Result<CompactionResult, CompactionError>;
}

/// An ordered sequence of [`CompactionStrategy`] steps executed one after
/// another.
///
/// Each strategy receives the transcript produced by the previous one,
/// creating a pipeline effect. The pipeline itself implements
/// [`CompactionStrategy`], so it can be nested or used anywhere a single
/// strategy is expected.
///
/// The pipeline checks the [`CompactionContext::cancellation`] token between
/// steps and returns [`CompactionError::Cancelled`] early if cancellation is
/// signalled.
///
/// # Example
///
/// ```rust
/// use agentkit_compaction::{
///     CompactionPipeline, DropFailedToolResultsStrategy,
///     DropReasoningStrategy, KeepRecentStrategy,
/// };
/// use agentkit_core::ItemKind;
///
/// let pipeline = CompactionPipeline::new()
///     .with_strategy(DropReasoningStrategy::new())
///     .with_strategy(DropFailedToolResultsStrategy::new())
///     .with_strategy(
///         KeepRecentStrategy::new(24)
///             .preserve_kind(ItemKind::System)
///             .preserve_kind(ItemKind::Context),
///     );
/// ```
#[derive(Clone, Default)]
pub struct CompactionPipeline {
    strategies: Vec<Arc<dyn CompactionStrategy>>,
}

impl CompactionPipeline {
    /// Create an empty pipeline with no strategies.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a strategy to the end of the pipeline.
    ///
    /// Strategies run in the order they are added.
    pub fn with_strategy(mut self, strategy: impl CompactionStrategy + 'static) -> Self {
        self.strategies.push(Arc::new(strategy));
        self
    }
}

#[async_trait]
impl CompactionStrategy for CompactionPipeline {
    async fn apply(
        &self,
        mut request: CompactionRequest,
        ctx: &mut CompactionContext<'_>,
    ) -> Result<CompactionResult, CompactionError> {
        let mut replaced_items = 0;
        let mut metadata = MetadataMap::new();

        for strategy in &self.strategies {
            if ctx
                .cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled)
            {
                return Err(CompactionError::Cancelled);
            }
            let result = strategy.apply(request.clone(), ctx).await?;
            request.transcript = result.transcript;
            replaced_items += result.replaced_items;
            metadata.extend(result.metadata);
        }

        Ok(CompactionResult {
            transcript: request.transcript,
            replaced_items,
            metadata,
        })
    }
}

/// Strategy that removes [`Part::Reasoning`] parts from every item.
///
/// Reasoning parts contain chain-of-thought content that is useful during
/// generation but rarely needed once the answer has been produced. Stripping
/// them reduces transcript size without losing user-visible content.
///
/// When `drop_empty_items` is `true` (the default), items that become empty
/// after reasoning removal are dropped entirely.
///
/// # Example
///
/// ```rust
/// use agentkit_compaction::DropReasoningStrategy;
///
/// let strategy = DropReasoningStrategy::new();
///
/// // Keep items that become empty after stripping reasoning:
/// let keep_empties = DropReasoningStrategy::new().drop_empty_items(false);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropReasoningStrategy {
    drop_empty_items: bool,
}

impl DropReasoningStrategy {
    /// Create a new strategy that drops reasoning parts and removes items
    /// that become empty as a result.
    pub fn new() -> Self {
        Self {
            drop_empty_items: true,
        }
    }

    /// Control whether items that become empty after reasoning removal are
    /// dropped from the transcript.
    ///
    /// Defaults to `true`.
    pub fn drop_empty_items(mut self, value: bool) -> Self {
        self.drop_empty_items = value;
        self
    }
}

#[async_trait]
impl CompactionStrategy for DropReasoningStrategy {
    async fn apply(
        &self,
        request: CompactionRequest,
        _ctx: &mut CompactionContext<'_>,
    ) -> Result<CompactionResult, CompactionError> {
        let mut transcript = Vec::with_capacity(request.transcript.len());
        let mut replaced_items = 0;

        for mut item in request.transcript {
            let original_len = item.parts.len();
            item.parts
                .retain(|part| !matches!(part, Part::Reasoning(_)));
            let changed = item.parts.len() != original_len;
            if item.parts.is_empty() && self.drop_empty_items {
                if changed {
                    replaced_items += 1;
                }
                continue;
            }
            if changed {
                replaced_items += 1;
            }
            transcript.push(item);
        }

        Ok(CompactionResult {
            transcript,
            replaced_items,
            metadata: MetadataMap::new(),
        })
    }
}

/// Strategy that removes [`Part::ToolResult`] parts where `is_error` is
/// `true`.
///
/// Failed tool invocations clutter the transcript and can confuse the model
/// on subsequent turns. This strategy strips those results while leaving
/// successful tool output intact.
///
/// When `drop_empty_items` is `true` (the default), items that become empty
/// after removal are dropped entirely.
///
/// # Example
///
/// ```rust
/// use agentkit_compaction::DropFailedToolResultsStrategy;
///
/// let strategy = DropFailedToolResultsStrategy::new();
///
/// // Keep items that become empty after stripping failed results:
/// let keep_empties = DropFailedToolResultsStrategy::new().drop_empty_items(false);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropFailedToolResultsStrategy {
    drop_empty_items: bool,
}

impl DropFailedToolResultsStrategy {
    /// Create a new strategy that drops failed tool results and removes
    /// items that become empty as a result.
    pub fn new() -> Self {
        Self {
            drop_empty_items: true,
        }
    }

    /// Control whether items that become empty after failed-result removal
    /// are dropped from the transcript.
    ///
    /// Defaults to `true`.
    pub fn drop_empty_items(mut self, value: bool) -> Self {
        self.drop_empty_items = value;
        self
    }
}

#[async_trait]
impl CompactionStrategy for DropFailedToolResultsStrategy {
    async fn apply(
        &self,
        request: CompactionRequest,
        _ctx: &mut CompactionContext<'_>,
    ) -> Result<CompactionResult, CompactionError> {
        let failed_call_ids = request
            .transcript
            .iter()
            .flat_map(|item| item.parts.iter())
            .filter_map(|part| match part {
                Part::ToolResult(result) if result.is_error => Some(result.call_id.clone()),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let mut transcript = Vec::with_capacity(request.transcript.len());
        let mut replaced_items = 0;

        for mut item in request.transcript {
            let original_len = item.parts.len();
            item.parts.retain(|part| {
                !matches!(part, Part::ToolResult(result) if result.is_error)
                    && !matches!(part, Part::ToolCall(call) if failed_call_ids.contains(&call.id))
            });
            let changed = item.parts.len() != original_len;
            if item.parts.is_empty() && self.drop_empty_items {
                if changed {
                    replaced_items += 1;
                }
                continue;
            }
            if changed {
                replaced_items += 1;
            }
            transcript.push(item);
        }

        Ok(CompactionResult {
            transcript,
            replaced_items,
            metadata: MetadataMap::new(),
        })
    }
}

/// Strategy that keeps only the `N` most recent removable items and drops
/// the rest.
///
/// Items whose [`ItemKind`] is in the `preserve_kinds` set are always
/// retained regardless of their position. This lets you protect system
/// prompts and context items while trimming older conversation turns.
///
/// # Example
///
/// ```rust
/// use agentkit_compaction::KeepRecentStrategy;
/// use agentkit_core::ItemKind;
///
/// let strategy = KeepRecentStrategy::new(16)
///     .preserve_kind(ItemKind::System)
///     .preserve_kind(ItemKind::Context);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepRecentStrategy {
    keep_last: usize,
    preserve_kinds: BTreeSet<ItemKind>,
}

impl KeepRecentStrategy {
    /// Create a strategy that keeps the last `keep_last` removable items.
    pub fn new(keep_last: usize) -> Self {
        Self {
            keep_last,
            preserve_kinds: BTreeSet::new(),
        }
    }

    /// Mark an [`ItemKind`] as preserved so that items of this kind are never
    /// dropped, regardless of their position in the transcript.
    pub fn preserve_kind(mut self, kind: ItemKind) -> Self {
        self.preserve_kinds.insert(kind);
        self
    }
}

#[async_trait]
impl CompactionStrategy for KeepRecentStrategy {
    async fn apply(
        &self,
        request: CompactionRequest,
        _ctx: &mut CompactionContext<'_>,
    ) -> Result<CompactionResult, CompactionError> {
        let removable = removable_indices(&request.transcript, &self.preserve_kinds);
        if removable.len() <= self.keep_last {
            return Ok(CompactionResult {
                transcript: request.transcript,
                replaced_items: 0,
                metadata: MetadataMap::new(),
            });
        }

        let keep_indices = removable
            .iter()
            .skip(removable.len() - self.keep_last)
            .copied()
            .collect::<BTreeSet<_>>();
        let keep_indices =
            expand_indices_to_tool_pairs(&request.transcript, keep_indices, &self.preserve_kinds);
        let replaced_items = removable
            .iter()
            .filter(|index| !keep_indices.contains(index))
            .count();
        let transcript = request
            .transcript
            .into_iter()
            .enumerate()
            .filter_map(|(index, item)| {
                (self.preserve_kinds.contains(&item.kind) || keep_indices.contains(&index))
                    .then_some(item)
            })
            .collect::<Vec<_>>();

        Ok(CompactionResult {
            transcript,
            replaced_items,
            metadata: MetadataMap::new(),
        })
    }
}

/// Strategy that replaces older transcript items with a backend-generated
/// summary.
///
/// The most recent `keep_last` removable items are kept verbatim. Everything
/// older (excluding items with a preserved [`ItemKind`]) is sent to the
/// configured [`CompactionBackend`] for summarisation. The summary items
/// replace the originals at their position in the transcript.
///
/// This strategy requires a backend. If [`CompactionContext::backend`] is
/// `None`, [`CompactionError::MissingBackend`] is returned.
///
/// # Example
///
/// ```rust
/// use agentkit_compaction::SummarizeOlderStrategy;
/// use agentkit_core::ItemKind;
///
/// let strategy = SummarizeOlderStrategy::new(8)
///     .preserve_kind(ItemKind::System);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SummarizeOlderStrategy {
    keep_last: usize,
    preserve_kinds: BTreeSet<ItemKind>,
}

impl SummarizeOlderStrategy {
    /// Create a strategy that keeps the last `keep_last` removable items and
    /// summarises everything older.
    pub fn new(keep_last: usize) -> Self {
        Self {
            keep_last,
            preserve_kinds: BTreeSet::new(),
        }
    }

    /// Mark an [`ItemKind`] as preserved so that items of this kind are never
    /// summarised, regardless of their position in the transcript.
    pub fn preserve_kind(mut self, kind: ItemKind) -> Self {
        self.preserve_kinds.insert(kind);
        self
    }
}

#[async_trait]
impl CompactionStrategy for SummarizeOlderStrategy {
    async fn apply(
        &self,
        request: CompactionRequest,
        ctx: &mut CompactionContext<'_>,
    ) -> Result<CompactionResult, CompactionError> {
        let Some(backend) = ctx.backend else {
            return Err(CompactionError::MissingBackend(
                "summarize strategy requires a compaction backend".into(),
            ));
        };

        let removable = removable_indices(&request.transcript, &self.preserve_kinds);
        if removable.len() <= self.keep_last {
            return Ok(CompactionResult {
                transcript: request.transcript,
                replaced_items: 0,
                metadata: MetadataMap::new(),
            });
        }

        let keep_indices = removable
            .iter()
            .skip(removable.len() - self.keep_last)
            .copied()
            .collect::<BTreeSet<_>>();
        let keep_indices =
            expand_indices_to_tool_pairs(&request.transcript, keep_indices, &self.preserve_kinds);
        let summary_indices = removable
            .iter()
            .copied()
            .filter(|index| !keep_indices.contains(index))
            .collect::<Vec<_>>();
        if summary_indices.is_empty() {
            return Ok(CompactionResult {
                transcript: request.transcript,
                replaced_items: 0,
                metadata: MetadataMap::new(),
            });
        }
        let first_summary_index = summary_indices[0];
        let summary_index_set = summary_indices.iter().copied().collect::<BTreeSet<_>>();
        let summary_items = summary_indices
            .iter()
            .map(|index| request.transcript[*index].clone())
            .collect::<Vec<_>>();
        let summary = backend
            .summarize(
                SummaryRequest {
                    items: summary_items,
                    reason: request.reason.clone(),
                    metadata: request.metadata.clone(),
                },
                ctx.cancellation.clone(),
            )
            .await?;

        let mut transcript = Vec::new();
        let mut inserted_summary = false;
        let mut summary_output = Some(summary.items);
        for (index, item) in request.transcript.into_iter().enumerate() {
            if summary_index_set.contains(&index) {
                if !inserted_summary && index == first_summary_index {
                    transcript.extend(summary_output.take().unwrap_or_default());
                    inserted_summary = true;
                }
                continue;
            }
            transcript.push(item);
        }

        Ok(CompactionResult {
            transcript,
            replaced_items: summary_indices.len(),
            metadata: summary.metadata,
        })
    }
}

fn removable_indices(transcript: &[Item], preserve_kinds: &BTreeSet<ItemKind>) -> Vec<usize> {
    transcript
        .iter()
        .enumerate()
        .filter_map(|(index, item)| (!preserve_kinds.contains(&item.kind)).then_some(index))
        .collect()
}

fn expand_indices_to_tool_pairs(
    transcript: &[Item],
    mut keep_indices: BTreeSet<usize>,
    preserve_kinds: &BTreeSet<ItemKind>,
) -> BTreeSet<usize> {
    keep_indices.extend(
        transcript
            .iter()
            .enumerate()
            .filter_map(|(index, item)| preserve_kinds.contains(&item.kind).then_some(index)),
    );

    let mut calls = HashMap::new();
    let mut results: HashMap<_, Vec<usize>> = HashMap::new();
    for (index, item) in transcript.iter().enumerate() {
        for part in &item.parts {
            match part {
                Part::ToolCall(call) => {
                    calls.entry(call.id.clone()).or_insert(index);
                }
                Part::ToolResult(result) => {
                    results
                        .entry(result.call_id.clone())
                        .or_default()
                        .push(index);
                }
                _ => {}
            }
        }
    }

    loop {
        let before_len = keep_indices.len();
        for (call_id, call_index) in &calls {
            if keep_indices.contains(call_index)
                && let Some(result_indices) = results.get(call_id)
            {
                keep_indices.extend(result_indices.iter().copied());
            }
        }
        for (call_id, result_indices) in &results {
            if result_indices
                .iter()
                .any(|result_index| keep_indices.contains(result_index))
                && let Some(call_index) = calls.get(call_id)
            {
                keep_indices.insert(*call_index);
            }
        }
        if keep_indices.len() == before_len {
            break;
        }
    }

    keep_indices
}

/// Errors that can occur during compaction.
#[derive(Debug, Error)]
pub enum CompactionError {
    /// The operation was cancelled via the [`TurnCancellation`] token.
    #[error("compaction cancelled")]
    Cancelled,
    /// A strategy that requires a [`CompactionBackend`] was invoked without
    /// one.
    #[error("missing compaction backend: {0}")]
    MissingBackend(String),
    /// A catch-all for other failures (e.g. backend errors).
    #[error("compaction failed: {0}")]
    Failed(String),
}

/// Adapts any [`Compactor`] to a [`LoopMutator`] so it can be registered
/// directly via [`AgentBuilder::mutator`]. Most callers reach this through
/// [`AgentBuilderCompactorExt::compactor`] rather than constructing it
/// directly.
///
/// `CompactorMutator` owns the telemetry contract: it emits
/// [`AgentEvent::MutationStarted`] before calling [`Compactor::compact`] and
/// [`AgentEvent::MutationFinished`] after, populating `metadata` with the
/// compaction reason and replaced item count.
pub struct CompactorMutator<C> {
    compactor: C,
    name: String,
}

impl<C: Compactor> CompactorMutator<C> {
    /// Wrap `compactor` with the default mutator label `"compactor"`.
    pub fn new(compactor: C) -> Self {
        Self {
            compactor,
            name: "compactor".into(),
        }
    }

    /// Override the mutator label that appears in
    /// [`AgentEvent::MutationStarted`]/[`AgentEvent::MutationFinished`].
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
}

#[async_trait]
impl<C: Compactor + 'static> LoopMutator for CompactorMutator<C> {
    async fn mutate(
        &self,
        cursor: &mut TranscriptCursor<'_>,
        ctx: LoopCtx<'_>,
    ) -> Result<(), LoopError> {
        let Some(reason) = self.compactor.should_compact(cursor.as_slice(), ctx.point) else {
            return Ok(());
        };

        ctx.emitter.emit(AgentEvent::MutationStarted {
            session_id: ctx.session_id.clone(),
            turn_id: ctx.turn_id.cloned(),
            mutator: self.name.clone(),
            point: ctx.point,
        });

        let before_len = cursor.len();
        let result = self
            .compactor
            .compact(cursor.as_slice(), reason.clone(), ctx.cancellation.clone())
            .await;

        let mut metadata = MetadataMap::new();
        metadata.insert("reason".into(), format!("{reason:?}").into());

        match result {
            Ok(new_items) => {
                let replaced = before_len.saturating_sub(new_items.len());
                metadata.insert("replaced_items".into(), (replaced as u64).into());
                **cursor = new_items;
                ctx.emitter.emit(AgentEvent::MutationFinished {
                    session_id: ctx.session_id.clone(),
                    turn_id: ctx.turn_id.cloned(),
                    mutator: self.name.clone(),
                    dirty: true,
                    metadata,
                });
                Ok(())
            }
            Err(err) => {
                metadata.insert("error".into(), err.to_string().into());
                ctx.emitter.emit(AgentEvent::MutationFinished {
                    session_id: ctx.session_id.clone(),
                    turn_id: ctx.turn_id.cloned(),
                    mutator: self.name.clone(),
                    dirty: false,
                    metadata,
                });
                match err {
                    CompactionError::Cancelled => Err(LoopError::Cancelled),
                    other => Err(LoopError::Mutator(other.to_string())),
                }
            }
        }
    }
}

/// Extension trait that adds [`compactor`](Self::compactor) to
/// [`AgentBuilder`], wrapping any [`Compactor`] in a [`CompactorMutator`]
/// and registering it via [`AgentBuilder::mutator`].
pub trait AgentBuilderCompactorExt<M: ModelAdapter>: Sized {
    /// Register `compactor` as a [`LoopMutator`].
    fn compactor<C: Compactor + 'static>(self, compactor: C) -> Self;
}

impl<M: ModelAdapter> AgentBuilderCompactorExt<M> for AgentBuilder<M> {
    fn compactor<C: Compactor + 'static>(self, compactor: C) -> Self {
        self.mutator(CompactorMutator::new(compactor))
    }
}

/// Boxed predicate driving [`StrategyCompactor`]: it inspects the transcript
/// and current [`MutationPoint`] and returns the reason to fire compaction,
/// or `None` to skip.
pub type TriggerFn = Box<dyn Fn(&[Item], MutationPoint) -> Option<CompactionReason> + Send + Sync>;

/// A reusable [`Compactor`] that bundles a trigger closure with a
/// [`CompactionStrategy`] (often a [`CompactionPipeline`]) and an optional
/// [`CompactionBackend`]. Use this when your trigger logic is a simple
/// predicate over the transcript; implement [`Compactor`] directly when you
/// need richer state (token meters, atomics, etc.).
///
/// # Example
///
/// ```rust
/// use agentkit_compaction::{
///     CompactionPipeline, CompactionReason, DropReasoningStrategy,
///     KeepRecentStrategy, StrategyCompactor,
/// };
/// use agentkit_core::ItemKind;
///
/// let compactor = StrategyCompactor::new(
///     |transcript: &[_], _point| {
///         (transcript.len() > 32).then_some(CompactionReason::TranscriptTooLong)
///     },
///     CompactionPipeline::new()
///         .with_strategy(DropReasoningStrategy::new())
///         .with_strategy(
///             KeepRecentStrategy::new(24)
///                 .preserve_kind(ItemKind::System)
///                 .preserve_kind(ItemKind::Context),
///         ),
/// );
/// ```
pub struct StrategyCompactor {
    trigger: TriggerFn,
    strategy: Arc<dyn CompactionStrategy>,
    backend: Option<Arc<dyn CompactionBackend>>,
    metadata: MetadataMap,
}

impl StrategyCompactor {
    /// Create a new compactor from a trigger closure and a strategy.
    ///
    /// The trigger receives the current transcript and [`MutationPoint`] and
    /// returns `Some(reason)` to fire compaction.
    pub fn new<T, S>(trigger: T, strategy: S) -> Self
    where
        T: Fn(&[Item], MutationPoint) -> Option<CompactionReason> + Send + Sync + 'static,
        S: CompactionStrategy + 'static,
    {
        Self {
            trigger: Box::new(trigger),
            strategy: Arc::new(strategy),
            backend: None,
            metadata: MetadataMap::new(),
        }
    }

    /// Start a builder for [`StrategyCompactor`].
    pub fn builder() -> StrategyCompactorBuilder {
        StrategyCompactorBuilder::default()
    }

    /// Attach a [`CompactionBackend`] for strategies that require
    /// summarisation (e.g. [`SummarizeOlderStrategy`]).
    pub fn with_backend(mut self, backend: impl CompactionBackend + 'static) -> Self {
        self.backend = Some(Arc::new(backend));
        self
    }

    /// Reuse an existing `Arc<dyn CompactionBackend>` (e.g. one already shared
    /// elsewhere) without re-wrapping.
    pub fn with_shared_backend(mut self, backend: Arc<dyn CompactionBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Set metadata forwarded to every strategy invocation.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

#[async_trait]
impl Compactor for StrategyCompactor {
    fn should_compact(
        &self,
        transcript: &[Item],
        point: MutationPoint,
    ) -> Option<CompactionReason> {
        (self.trigger)(transcript, point)
    }

    async fn compact(
        &self,
        transcript: &[Item],
        reason: CompactionReason,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Vec<Item>, CompactionError> {
        let request = CompactionRequest {
            transcript: transcript.to_vec(),
            reason,
            metadata: self.metadata.clone(),
        };
        let mut ctx = CompactionContext {
            backend: self.backend.as_deref(),
            cancellation,
        };
        let result = self.strategy.apply(request, &mut ctx).await?;
        Ok(result.transcript)
    }
}

/// Builder error for [`StrategyCompactor`].
#[derive(Debug, Error)]
pub enum StrategyCompactorBuildError {
    /// `trigger` was not provided.
    #[error("trigger is required")]
    MissingTrigger,
    /// `strategy` was not provided.
    #[error("strategy is required")]
    MissingStrategy,
}

/// Builder for [`StrategyCompactor`].
#[derive(Default)]
pub struct StrategyCompactorBuilder {
    trigger: Option<TriggerFn>,
    strategy: Option<Arc<dyn CompactionStrategy>>,
    backend: Option<Arc<dyn CompactionBackend>>,
    metadata: MetadataMap,
}

impl StrategyCompactorBuilder {
    /// Set the trigger closure.
    pub fn trigger<T>(mut self, trigger: T) -> Self
    where
        T: Fn(&[Item], MutationPoint) -> Option<CompactionReason> + Send + Sync + 'static,
    {
        self.trigger = Some(Box::new(trigger));
        self
    }

    /// Fire when the transcript exceeds `max_items`.
    pub fn item_count_trigger(self, max_items: usize) -> Self {
        self.trigger(move |transcript: &[Item], _point| {
            (transcript.len() > max_items).then_some(CompactionReason::TranscriptTooLong)
        })
    }

    /// Set the strategy.
    pub fn strategy(mut self, strategy: impl CompactionStrategy + 'static) -> Self {
        self.strategy = Some(Arc::new(strategy));
        self
    }

    /// Attach a backend for strategies that need summarisation.
    pub fn backend(mut self, backend: impl CompactionBackend + 'static) -> Self {
        self.backend = Some(Arc::new(backend));
        self
    }

    /// Reuse an existing `Arc<dyn CompactionBackend>`.
    pub fn shared_backend(mut self, backend: Arc<dyn CompactionBackend>) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Set metadata forwarded to every strategy invocation.
    pub fn metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }

    /// Build the configured [`StrategyCompactor`].
    pub fn build(self) -> Result<StrategyCompactor, StrategyCompactorBuildError> {
        Ok(StrategyCompactor {
            trigger: self
                .trigger
                .ok_or(StrategyCompactorBuildError::MissingTrigger)?,
            strategy: self
                .strategy
                .ok_or(StrategyCompactorBuildError::MissingStrategy)?,
            backend: self.backend,
            metadata: self.metadata,
        })
    }
}

const DEFAULT_COMPACTION_PROMPT: &str = "You are a compaction agent. Compress the \
transcript that follows into a durable context note for an assistant that has lost the \
original messages. Preserve every named person, every year and date, every place, every \
decision the assistant committed to, every tool the assistant invoked, and every \
actionable fact in the tool results. Drop chatter, narration, and chain-of-thought. \
Return only the compacted note as plain text.";

/// Build a trigger closure that fires when the most recent transcript item's
/// reported `usage.tokens.input_tokens` reaches `window * percent / 100`.
///
/// Only fires at [`MutationPoint::AfterTurnEnded`]; other points return
/// `None`. `percent` is clamped to `1..=100`.
///
/// Plug into [`StrategyCompactorBuilder::trigger`] (or use it directly as a
/// [`TriggerFn`]).
pub fn context_window_trigger(window: u64, percent: u32) -> TriggerFn {
    let percent = percent.clamp(1, 100);
    let threshold = window.saturating_mul(percent as u64) / 100;
    Box::new(move |transcript: &[Item], point: MutationPoint| {
        if point != MutationPoint::AfterTurnEnded {
            return None;
        }
        let last_input = transcript
            .iter()
            .rev()
            .find_map(|i| i.usage.as_ref()?.tokens.as_ref().map(|t| t.input_tokens))?;
        (last_input >= threshold).then(|| {
            CompactionReason::Custom(format!(
                "input_tokens={last_input} >= threshold={threshold} (window={window}, {percent}%)",
            ))
        })
    })
}

/// Build a trigger closure that fires when the transcript grows beyond
/// `max_items` items. Convenience matching
/// [`StrategyCompactorBuilder::item_count_trigger`].
pub fn item_count_trigger(max_items: usize) -> TriggerFn {
    Box::new(move |transcript: &[Item], _point: MutationPoint| {
        (transcript.len() > max_items).then_some(CompactionReason::TranscriptTooLong)
    })
}

/// Builder error for [`AgentCompactor`].
#[derive(Debug, Error)]
pub enum AgentCompactorBuildError {
    /// `agent` was not provided.
    #[error("agent is required")]
    MissingAgent,
    /// `session_id` was not provided.
    #[error("session_id is required")]
    MissingSessionId,
}

/// [`CompactionBackend`] that summarises items by running a nested loop over
/// a sub-agent.
///
/// Plug into any [`CompactionStrategy`] that needs a backend (e.g.
/// [`SummarizeOlderStrategy`]) via [`StrategyCompactorBuilder::backend`].
/// Pair with whatever trigger fits — see [`context_window_trigger`] for a
/// token-aware default.
pub struct AgentCompactor<M: ModelAdapter + Clone + 'static> {
    inner: Arc<Agent<M>>,
    session_id: SessionId,
    system_prompt: String,
}

impl<M: ModelAdapter + Clone + 'static> AgentCompactor<M> {
    /// Start a new builder. `agent` and `session_id` are required.
    pub fn builder() -> AgentCompactorBuilder<M> {
        AgentCompactorBuilder::new()
    }
}

/// Builder for [`AgentCompactor`].
pub struct AgentCompactorBuilder<M: ModelAdapter + Clone + 'static> {
    agent: Option<Arc<Agent<M>>>,
    session_id: Option<SessionId>,
    system_prompt: Option<String>,
}

impl<M: ModelAdapter + Clone + 'static> AgentCompactorBuilder<M> {
    fn new() -> Self {
        Self {
            agent: None,
            session_id: None,
            system_prompt: None,
        }
    }

    /// The sub-agent that runs nested summary turns.
    pub fn agent(mut self, agent: Arc<Agent<M>>) -> Self {
        self.agent = Some(agent);
        self
    }

    /// Session id passed to [`Agent::start`] for every nested compaction.
    pub fn session_id(mut self, id: SessionId) -> Self {
        self.session_id = Some(id);
        self
    }

    /// Override the system prompt used by the nested compaction agent.
    pub fn system_prompt(mut self, s: impl Into<String>) -> Self {
        self.system_prompt = Some(s.into());
        self
    }

    /// Build the configured [`AgentCompactor`].
    pub fn build(self) -> Result<AgentCompactor<M>, AgentCompactorBuildError> {
        Ok(AgentCompactor {
            inner: self.agent.ok_or(AgentCompactorBuildError::MissingAgent)?,
            session_id: self
                .session_id
                .ok_or(AgentCompactorBuildError::MissingSessionId)?,
            system_prompt: self
                .system_prompt
                .unwrap_or_else(|| DEFAULT_COMPACTION_PROMPT.into()),
        })
    }
}

#[async_trait]
impl<M: ModelAdapter + Clone + 'static> CompactionBackend for AgentCompactor<M> {
    async fn summarize(
        &self,
        request: SummaryRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<SummaryResult, CompactionError> {
        if cancellation
            .as_ref()
            .is_some_and(TurnCancellation::is_cancelled)
        {
            return Err(CompactionError::Cancelled);
        }

        let rendered = render_items_for_summary(&request.items);

        let driver_input = vec![
            Item::text(ItemKind::System, self.system_prompt.clone()),
            Item::text(
                ItemKind::User,
                format!(
                    "Compress the transcript below into a durable context note. \
                     Preserve names, places, dates, decisions, and tool outcomes.\n\n{rendered}"
                ),
            ),
        ];

        let mut driver = self
            .inner
            .start(SessionConfig::new(self.session_id.clone()))
            .await
            .map_err(|e| CompactionError::Failed(e.to_string()))?;
        driver
            .submit_input(driver_input)
            .map_err(|e| CompactionError::Failed(e.to_string()))?;

        let summary = run_compactor_to_completion(&mut driver)
            .await
            .map_err(CompactionError::Failed)?;

        Ok(SummaryResult {
            items: vec![Item::text(ItemKind::Context, summary)],
            metadata: MetadataMap::new(),
        })
    }
}

async fn run_compactor_to_completion<S>(
    driver: &mut agentkit_loop::LoopDriver<S>,
) -> Result<String, String>
where
    S: agentkit_loop::ModelSession,
{
    use agentkit_loop::LoopInterrupt;
    loop {
        let step = driver.next().await.map_err(|e| e.to_string())?;
        match step {
            LoopStep::Finished(result) => {
                let mut sections = Vec::new();
                for item in result.items {
                    if item.kind != ItemKind::Assistant {
                        continue;
                    }
                    for part in item.parts {
                        if let Part::Text(t) = part {
                            sections.push(t.text);
                        }
                    }
                }
                return Ok(sections.join("\n"));
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                return Err("compactor sub-agent unexpectedly awaiting input".into());
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(_)) => {
                return Err("compactor sub-agent unexpectedly required approval".into());
            }
        }
    }
}

fn render_items_for_summary(items: &[Item]) -> String {
    items
        .iter()
        .map(|item| {
            let kind = match item.kind {
                ItemKind::User => "USER",
                ItemKind::Assistant => "ASSISTANT",
                ItemKind::System => "SYSTEM",
                ItemKind::Developer => "DEVELOPER",
                ItemKind::Tool => "TOOL",
                ItemKind::Context => "CONTEXT",
                ItemKind::Notification => "NOTIFICATION",
            };
            let body = item
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text(t) => Some(t.text.clone()),
                    Part::Structured(v) => Some(v.value.to_string()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("[{kind}]\n{body}")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use agentkit_core::{
        CancellationController, Part, TextPart, ToolCallPart, ToolOutput, ToolResultPart,
    };

    use super::*;

    fn user_item(text: &str) -> Item {
        Item {
            id: None,
            kind: ItemKind::User,
            parts: vec![Part::Text(TextPart {
                text: text.into(),
                metadata: MetadataMap::new(),
            })],
            metadata: MetadataMap::new(),
            usage: None,
            finish_reason: None,
            created_at: None,
        }
    }

    fn assistant_with_reasoning() -> Item {
        Item {
            id: None,
            kind: ItemKind::Assistant,
            parts: vec![
                Part::Reasoning(agentkit_core::ReasoningPart {
                    summary: Some("think".into()),
                    data: None,
                    redacted: false,
                    metadata: MetadataMap::new(),
                }),
                Part::Text(TextPart {
                    text: "answer".into(),
                    metadata: MetadataMap::new(),
                }),
            ],
            metadata: MetadataMap::new(),
            usage: None,
            finish_reason: None,
            created_at: None,
        }
    }

    fn failed_tool_item() -> Item {
        Item {
            id: None,
            kind: ItemKind::Tool,
            parts: vec![Part::ToolResult(ToolResultPart {
                call_id: "call-1".into(),
                output: ToolOutput::Text("failed".into()),
                is_error: true,
                metadata: MetadataMap::new(),
            })],
            metadata: MetadataMap::new(),
            usage: None,
            finish_reason: None,
            created_at: None,
        }
    }

    fn tool_call_item(id: &str) -> Item {
        Item {
            id: None,
            kind: ItemKind::Assistant,
            parts: vec![Part::ToolCall(ToolCallPart {
                id: id.into(),
                name: "lookup".into(),
                input: serde_json::json!({}),
                metadata: MetadataMap::new(),
            })],
            metadata: MetadataMap::new(),
            usage: None,
            finish_reason: None,
            created_at: None,
        }
    }

    fn tool_result_item(id: &str, is_error: bool) -> Item {
        Item {
            id: None,
            kind: ItemKind::Tool,
            parts: vec![Part::ToolResult(ToolResultPart {
                call_id: id.into(),
                output: ToolOutput::Text("result".into()),
                is_error,
                metadata: MetadataMap::new(),
            })],
            metadata: MetadataMap::new(),
            usage: None,
            finish_reason: None,
            created_at: None,
        }
    }

    #[tokio::test]
    async fn pipeline_applies_local_strategies_in_order() {
        let request = CompactionRequest {
            transcript: vec![
                user_item("a"),
                assistant_with_reasoning(),
                failed_tool_item(),
                user_item("b"),
                user_item("c"),
            ],
            reason: CompactionReason::TranscriptTooLong,
            metadata: MetadataMap::new(),
        };
        let pipeline = CompactionPipeline::new()
            .with_strategy(DropReasoningStrategy::new())
            .with_strategy(DropFailedToolResultsStrategy::new())
            .with_strategy(
                KeepRecentStrategy::new(2)
                    .preserve_kind(ItemKind::System)
                    .preserve_kind(ItemKind::Context),
            );
        let mut ctx = CompactionContext {
            backend: None,
            cancellation: None,
        };

        let result = pipeline.apply(request, &mut ctx).await.unwrap();
        assert_eq!(result.transcript.len(), 2);
        assert!(result.replaced_items >= 2);
        assert!(result.transcript.iter().all(|item| {
            item.parts
                .iter()
                .all(|part| !matches!(part, Part::Reasoning(_)))
        }));
    }

    #[tokio::test]
    async fn keep_recent_preserves_tool_call_result_pairs() {
        let request = CompactionRequest {
            transcript: vec![
                user_item("old"),
                tool_call_item("call-1"),
                tool_result_item("call-1", false),
                user_item("recent"),
            ],
            reason: CompactionReason::TranscriptTooLong,
            metadata: MetadataMap::new(),
        };
        let strategy = KeepRecentStrategy::new(2);
        let mut ctx = CompactionContext {
            backend: None,
            cancellation: None,
        };

        let result = strategy.apply(request, &mut ctx).await.unwrap();
        assert_eq!(result.replaced_items, 1);
        assert_eq!(result.transcript.len(), 3);
        assert!(matches!(result.transcript[0].parts[0], Part::ToolCall(_)));
        assert!(matches!(result.transcript[1].parts[0], Part::ToolResult(_)));
    }

    #[tokio::test]
    async fn failed_tool_result_removal_drops_matching_tool_call() {
        let request = CompactionRequest {
            transcript: vec![
                tool_call_item("call-1"),
                tool_result_item("call-1", true),
                user_item("recent"),
            ],
            reason: CompactionReason::TranscriptTooLong,
            metadata: MetadataMap::new(),
        };
        let strategy = DropFailedToolResultsStrategy::new();
        let mut ctx = CompactionContext {
            backend: None,
            cancellation: None,
        };

        let result = strategy.apply(request, &mut ctx).await.unwrap();
        assert_eq!(result.replaced_items, 2);
        assert_eq!(result.transcript.len(), 1);
        assert!(matches!(result.transcript[0].kind, ItemKind::User));
    }

    struct FakeBackend;

    #[async_trait]
    impl CompactionBackend for FakeBackend {
        async fn summarize(
            &self,
            request: SummaryRequest,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<SummaryResult, CompactionError> {
            Ok(SummaryResult {
                items: vec![Item {
                    id: None,
                    kind: ItemKind::Context,
                    parts: vec![Part::Text(TextPart {
                        text: format!("summary of {} items", request.items.len()),
                        metadata: MetadataMap::new(),
                    })],
                    metadata: MetadataMap::new(),
                    usage: None,
                    finish_reason: None,
                    created_at: None,
                }],
                metadata: MetadataMap::new(),
            })
        }
    }

    #[tokio::test]
    async fn summarize_strategy_uses_backend() {
        let request = CompactionRequest {
            transcript: vec![user_item("a"), user_item("b"), user_item("c")],
            reason: CompactionReason::TranscriptTooLong,
            metadata: MetadataMap::new(),
        };
        let strategy = SummarizeOlderStrategy::new(1);
        let mut ctx = CompactionContext {
            backend: Some(&FakeBackend),
            cancellation: None,
        };

        let result = strategy.apply(request, &mut ctx).await.unwrap();
        assert_eq!(result.replaced_items, 2);
        assert_eq!(result.transcript.len(), 2);
        match &result.transcript[0].parts[0] {
            Part::Text(text) => assert_eq!(text.text, "summary of 2 items"),
            other => panic!("unexpected part: {other:?}"),
        }
    }

    #[tokio::test]
    async fn summarize_strategy_preserves_tool_call_result_pairs() {
        let request = CompactionRequest {
            transcript: vec![
                user_item("old"),
                tool_call_item("call-1"),
                tool_result_item("call-1", false),
                user_item("recent"),
            ],
            reason: CompactionReason::TranscriptTooLong,
            metadata: MetadataMap::new(),
        };
        let strategy = SummarizeOlderStrategy::new(2);
        let mut ctx = CompactionContext {
            backend: Some(&FakeBackend),
            cancellation: None,
        };

        let result = strategy.apply(request, &mut ctx).await.unwrap();
        assert_eq!(result.replaced_items, 1);
        assert_eq!(result.transcript.len(), 4);
        match &result.transcript[0].parts[0] {
            Part::Text(text) => assert_eq!(text.text, "summary of 1 items"),
            other => panic!("unexpected part: {other:?}"),
        }
        assert!(matches!(result.transcript[1].parts[0], Part::ToolCall(_)));
        assert!(matches!(result.transcript[2].parts[0], Part::ToolResult(_)));
    }

    #[tokio::test]
    async fn pipeline_stops_when_cancelled() {
        let controller = CancellationController::new();
        let checkpoint = controller.handle().checkpoint();
        controller.interrupt();
        let request = CompactionRequest {
            transcript: vec![user_item("a"), user_item("b"), user_item("c")],
            reason: CompactionReason::TranscriptTooLong,
            metadata: MetadataMap::new(),
        };
        let pipeline = CompactionPipeline::new().with_strategy(DropReasoningStrategy::new());
        let mut ctx = CompactionContext {
            backend: None,
            cancellation: Some(checkpoint),
        };

        let error = pipeline.apply(request, &mut ctx).await.unwrap_err();
        assert!(matches!(error, CompactionError::Cancelled));
    }
}
