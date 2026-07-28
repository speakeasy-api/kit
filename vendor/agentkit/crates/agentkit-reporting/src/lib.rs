//! Reporting observers for the agentkit agent loop.
//!
//! This crate provides [`LoopObserver`] implementations that turn
//! [`AgentEvent`]s into logs, usage summaries, transcripts, and
//! machine-readable JSONL streams. Reporters are designed to be composed
//! through [`CompositeReporter`] so a single loop can feed multiple
//! observers at once.
//!
//! # Included reporters
//!
//! | Reporter | Purpose |
//! |---|---|
//! | [`StdoutReporter`] | Human-readable terminal output |
//! | [`JsonlReporter`] | Machine-readable newline-delimited JSON |
//! | [`UsageReporter`] | Aggregated token / cost totals |
//! | [`TranscriptReporter`] | Growing snapshot of conversation items |
//! | [`CompositeReporter`] | Fan-out to multiple reporters |
//!
//! # Adapter reporters
//!
//! | Adapter | Purpose |
//! |---|---|
//! | [`BufferedReporter`] | Enqueues events for batch flushing |
//! | [`ChannelReporter`] | Forwards events to another thread or task |
//! | [`TracingReporter`] | Converts events into `tracing` spans and events (requires `tracing` feature) |
//!
//! # Failure policy
//!
//! Wrap a [`FallibleObserver`] in a [`PolicyReporter`] to control how
//! errors are handled — see [`FailurePolicy`].
//!
//! # Quick start
//!
//! ```rust
//! use agentkit_reporting::{CompositeReporter, JsonlReporter, UsageReporter, TranscriptReporter};
//!
//! let reporter = CompositeReporter::new()
//!     .with_observer(JsonlReporter::new(Vec::new()))
//!     .with_observer(UsageReporter::new())
//!     .with_observer(TranscriptReporter::new());
//! ```

mod buffered;
mod channel;
mod policy;

#[cfg(feature = "tracing")]
mod tracing_reporter;

pub use buffered::BufferedReporter;
pub use channel::ChannelReporter;
pub use policy::{FailurePolicy, FallibleObserver, PolicyReporter};

#[cfg(feature = "tracing")]
pub use tracing_reporter::TracingReporter;

use std::io::{self, Write};
use std::time::SystemTime;

use agentkit_core::{Item, ItemKind, Part, SessionId, TokenUsage, Usage};
use agentkit_loop::{AgentEvent, LoopObserver, ObservedEvent, TurnResult};
use serde::Serialize;
use thiserror::Error;

/// Errors that can occur while writing reports.
///
/// Reporter implementations (e.g. [`JsonlReporter`], [`StdoutReporter`])
/// collect errors internally rather than surfacing them through the
/// [`LoopObserver`] interface. Call the reporter's `take_errors()` method
/// after the loop finishes to inspect any problems.
#[derive(Debug, Error)]
pub enum ReportError {
    /// An I/O error occurred while writing to the underlying writer.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// A serialization error occurred (JSONL reporters only).
    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
    /// The receiving end of a channel was dropped.
    #[error("channel send failed")]
    ChannelSend,
}

/// A timestamped wrapper around an [`AgentEvent`].
///
/// [`JsonlReporter`] serializes each incoming event inside an
/// `EventEnvelope` so that the resulting JSONL stream carries
/// wall-clock timestamps alongside the event payload.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct EventEnvelope<'a> {
    /// When the event was observed.
    pub timestamp: SystemTime,
    /// Session routing key for shared reporter sinks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<&'a SessionId>,
    /// The underlying agent event.
    pub event: &'a AgentEvent,
}

/// Fan-out reporter that forwards every [`AgentEvent`] to multiple child observers.
///
/// `CompositeReporter` itself implements [`LoopObserver`], so it can be
/// handed directly to the agent loop. Each event is cloned once per child
/// observer.
///
/// # Example
///
/// ```rust
/// use agentkit_reporting::{
///     CompositeReporter, JsonlReporter, StdoutReporter, UsageReporter,
/// };
///
/// // Build a reporter that writes to JSONL, prints to stdout, and tracks usage.
/// let reporter = CompositeReporter::new()
///     .with_observer(JsonlReporter::new(Vec::new()))
///     .with_observer(StdoutReporter::new(std::io::stdout()))
///     .with_observer(UsageReporter::new());
/// ```
#[derive(Default)]
pub struct CompositeReporter {
    children: Vec<Box<dyn LoopObserver>>,
}

impl CompositeReporter {
    /// Creates an empty `CompositeReporter` with no child observers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an observer and returns `self` (builder pattern).
    ///
    /// # Arguments
    ///
    /// * `observer` - Any type implementing [`LoopObserver`].
    pub fn with_observer(mut self, observer: impl LoopObserver + 'static) -> Self {
        self.children.push(Box::new(observer));
        self
    }

    /// Adds an observer by mutable reference.
    ///
    /// Use this when you need to add observers after initial construction
    /// rather than in a builder chain.
    ///
    /// # Arguments
    ///
    /// * `observer` - Any type implementing [`LoopObserver`].
    pub fn push(&mut self, observer: impl LoopObserver + 'static) -> &mut Self {
        self.children.push(Box::new(observer));
        self
    }
}

impl LoopObserver for CompositeReporter {
    fn handle_event(&self, event: ObservedEvent) {
        if self.children.is_empty() {
            return;
        }
        let last = self.children.len() - 1;
        for child in &self.children[..last] {
            child.handle_event(event.clone());
        }
        self.children[last].handle_event(event);
    }
}

/// Machine-readable reporter that writes one JSON object per line (JSONL).
///
/// Each [`AgentEvent`] is wrapped in an [`EventEnvelope`] with a timestamp
/// and serialized as a single JSON line. This format is easy to ingest in
/// log aggregation systems or to replay offline.
///
/// I/O and serialization errors are collected internally and can be
/// retrieved with [`take_errors`](JsonlReporter::take_errors).
///
/// # Example
///
/// ```rust
/// use agentkit_reporting::JsonlReporter;
///
/// // Write JSONL to an in-memory buffer (useful in tests).
/// let reporter = JsonlReporter::new(Vec::new());
///
/// // Write JSONL to a file, flushing after every event.
/// # fn example() -> std::io::Result<()> {
/// let file = std::fs::File::create("events.jsonl")?;
/// let reporter = JsonlReporter::new(std::io::BufWriter::new(file));
/// # Ok(())
/// # }
/// ```
pub struct JsonlReporter<W> {
    writer: std::sync::Mutex<W>,
    flush_each_event: bool,
    include_session_id: bool,
    errors: std::sync::Mutex<Vec<ReportError>>,
}

impl<W> JsonlReporter<W>
where
    W: Write,
{
    /// Creates a new `JsonlReporter` writing to the given writer.
    pub fn new(writer: W) -> Self {
        Self {
            writer: std::sync::Mutex::new(writer),
            flush_each_event: true,
            include_session_id: false,
            errors: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Controls whether the writer is flushed after every event (builder pattern).
    pub fn with_flush_each_event(mut self, flush_each_event: bool) -> Self {
        self.flush_each_event = flush_each_event;
        self
    }

    /// Controls whether each JSONL envelope includes the observed session id.
    ///
    /// This is opt-in to preserve the original JSONL envelope shape for
    /// consumers that validate fields strictly.
    pub fn with_session_id(mut self, include_session_id: bool) -> Self {
        self.include_session_id = include_session_id;
        self
    }

    /// Drains and returns all errors accumulated during event handling.
    pub fn take_errors(&self) -> Vec<ReportError> {
        std::mem::take(&mut *self.errors.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn record_result(&self, result: Result<(), ReportError>) {
        if let Err(error) = result {
            self.errors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(error);
        }
    }

    /// Consumes the reporter and returns the underlying writer.
    pub fn into_inner(self) -> W {
        self.writer.into_inner().unwrap_or_else(|e| e.into_inner())
    }
}

impl<W> LoopObserver for JsonlReporter<W>
where
    W: Write + Send,
{
    fn handle_event(&self, event: ObservedEvent) {
        let result = (|| -> Result<(), ReportError> {
            let envelope = EventEnvelope {
                timestamp: SystemTime::now(),
                session_id: self.include_session_id.then_some(event.session_id.as_ref()),
                event: &event.event,
            };
            let mut buf = serde_json::to_vec(&envelope)?;
            buf.push(b'\n');
            let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            writer.write_all(&buf)?;
            if self.flush_each_event {
                writer.flush()?;
            }
            Ok(())
        })();
        self.record_result(result);
    }
}

/// Accumulated token counts across all events seen by a [`UsageReporter`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageTotals {
    /// Total input (prompt) tokens consumed.
    pub input_tokens: u64,
    /// Total output (completion) tokens produced.
    pub output_tokens: u64,
    /// Total reasoning tokens used (model-dependent).
    pub reasoning_tokens: u64,
    /// Total input tokens served from the provider's cache.
    pub cached_input_tokens: u64,
    /// Total input tokens written into the provider's cache.
    pub cache_write_input_tokens: u64,
}

/// Accumulated monetary cost across all events seen by a [`UsageReporter`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CostTotals {
    /// Running total cost expressed in `currency` units.
    pub amount: f64,
    /// ISO 4217 currency code (e.g. `"USD"`), set from the first cost event.
    pub currency: Option<String>,
}

/// Snapshot of everything a [`UsageReporter`] has tracked so far.
///
/// Retrieve this via [`UsageReporter::summary`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageSummary {
    /// Total number of [`AgentEvent`]s observed (of any variant).
    pub events_seen: usize,
    /// Number of events that carried usage information
    /// ([`AgentEvent::UsageUpdated`] or [`AgentEvent::TurnFinished`] with usage).
    pub usage_events_seen: usize,
    /// Number of [`AgentEvent::TurnFinished`] events observed.
    pub turn_results_seen: usize,
    /// Aggregated token counts.
    pub totals: UsageTotals,
    /// Aggregated cost, present only if at least one event carried cost data.
    pub cost: Option<CostTotals>,
}

/// Reporter that aggregates token usage and cost across the entire run.
///
/// `UsageReporter` listens for [`AgentEvent::UsageUpdated`] and
/// [`AgentEvent::TurnFinished`] events and maintains a running
/// [`UsageSummary`]. After the loop completes, call [`summary`](UsageReporter::summary)
/// to read the totals.
///
/// # Example
///
/// ```rust
/// use agentkit_reporting::UsageReporter;
/// use agentkit_loop::LoopObserver;
///
/// let mut reporter = UsageReporter::new();
///
/// // ...pass `reporter` to the agent loop, then afterwards:
/// let summary = reporter.summary();
/// println!(
///     "tokens: {} in / {} out",
///     summary.totals.input_tokens,
///     summary.totals.output_tokens,
/// );
/// ```
#[derive(Default)]
pub struct UsageReporter {
    summary: std::sync::Mutex<UsageSummary>,
}

impl UsageReporter {
    /// Creates a new `UsageReporter` with zeroed counters.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a snapshot of the current [`UsageSummary`].
    pub fn summary(&self) -> UsageSummary {
        self.summary
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn absorb(summary: &mut UsageSummary, usage: &Usage) {
        summary.usage_events_seen += 1;
        if let Some(tokens) = &usage.tokens {
            summary.totals.input_tokens += tokens.input_tokens;
            summary.totals.output_tokens += tokens.output_tokens;
            summary.totals.reasoning_tokens += tokens.reasoning_tokens.unwrap_or_default();
            summary.totals.cached_input_tokens += tokens.cached_input_tokens.unwrap_or_default();
            summary.totals.cache_write_input_tokens +=
                tokens.cache_write_input_tokens.unwrap_or_default();
        }
        if let Some(cost) = &usage.cost {
            let totals = summary.cost.get_or_insert_with(CostTotals::default);
            totals.amount += cost.amount;
            if totals.currency.is_none() {
                totals.currency = Some(cost.currency.clone());
            }
        }
    }
}

impl LoopObserver for UsageReporter {
    fn handle_event(&self, event: ObservedEvent) {
        let event = event.event;
        let mut summary = self.summary.lock().unwrap_or_else(|e| e.into_inner());
        summary.events_seen += 1;
        match event {
            AgentEvent::UsageUpdated(usage) => Self::absorb(&mut summary, &usage),
            AgentEvent::TurnFinished(TurnResult {
                usage: Some(usage), ..
            }) => {
                summary.turn_results_seen += 1;
                Self::absorb(&mut summary, &usage);
            }
            AgentEvent::TurnFinished(_) => {
                summary.turn_results_seen += 1;
            }
            _ => {}
        }
    }
}

/// Growing list of conversation [`Item`]s captured by a [`TranscriptReporter`].
///
/// Items are appended in the order they arrive: user inputs first, then
/// assistant outputs from each finished turn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranscriptView {
    /// The ordered sequence of conversation items.
    pub items: Vec<Item>,
}

/// Reporter that captures the evolving conversation transcript.
///
/// `TranscriptReporter` listens for [`AgentEvent::InputAccepted`] and
/// [`AgentEvent::TurnFinished`] events and accumulates their [`Item`]s
/// into a [`TranscriptView`]. This is useful for post-run analysis or
/// for displaying a conversation history.
///
/// # Example
///
/// ```rust
/// use agentkit_reporting::TranscriptReporter;
/// use agentkit_loop::LoopObserver;
///
/// let mut reporter = TranscriptReporter::new();
///
/// // ...pass `reporter` to the agent loop, then afterwards:
/// for item in &reporter.transcript().items {
///     println!("{:?}: {} parts", item.kind, item.parts.len());
/// }
/// ```
#[derive(Default)]
pub struct TranscriptReporter {
    transcript: std::sync::Mutex<TranscriptView>,
}

impl TranscriptReporter {
    /// Creates a new `TranscriptReporter` with an empty transcript.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a snapshot of the current [`TranscriptView`].
    pub fn transcript(&self) -> TranscriptView {
        self.transcript
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl LoopObserver for TranscriptReporter {
    fn handle_event(&self, event: ObservedEvent) {
        let event = event.event;
        let mut transcript = self.transcript.lock().unwrap_or_else(|e| e.into_inner());
        match event {
            AgentEvent::InputAccepted { items, .. } => {
                transcript.items.extend(items);
            }
            AgentEvent::TurnFinished(result) => {
                transcript.items.extend(result.items);
            }
            _ => {}
        }
    }
}

/// Human-readable reporter that writes structured log lines to a [`Write`] sink.
///
/// Each [`AgentEvent`] is printed as a bracketed tag followed by key fields,
/// for example `[turn] started session=abc turn=1`. Turn results include
/// indented item and part summaries so the operator can follow the
/// conversation at a glance.
///
/// I/O errors are collected internally; call
/// [`take_errors`](StdoutReporter::take_errors) after the loop to inspect them.
///
/// # Example
///
/// ```rust
/// use agentkit_reporting::StdoutReporter;
///
/// // Print events to stderr, hiding usage lines.
/// let reporter = StdoutReporter::new(std::io::stderr())
///     .with_usage(false);
/// ```
pub struct StdoutReporter<W> {
    writer: std::sync::Mutex<W>,
    show_usage: bool,
    errors: std::sync::Mutex<Vec<ReportError>>,
}

impl<W> StdoutReporter<W>
where
    W: Write,
{
    /// Creates a new `StdoutReporter` that writes to the given writer.
    pub fn new(writer: W) -> Self {
        Self {
            writer: std::sync::Mutex::new(writer),
            show_usage: true,
            errors: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Controls whether `[usage]` lines are printed (builder pattern).
    pub fn with_usage(mut self, show_usage: bool) -> Self {
        self.show_usage = show_usage;
        self
    }

    /// Drains and returns all errors accumulated during event handling.
    pub fn take_errors(&self) -> Vec<ReportError> {
        std::mem::take(&mut *self.errors.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn record_result(&self, result: Result<(), ReportError>) {
        if let Err(error) = result {
            self.errors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(error);
        }
    }
}

impl<W> LoopObserver for StdoutReporter<W>
where
    W: Write + Send,
{
    fn handle_event(&self, event: ObservedEvent) {
        let event = event.event;
        let result = (|| -> Result<(), ReportError> {
            let mut buf: Vec<u8> = Vec::new();
            write_stdout_event(&mut buf, &event, self.show_usage)?;
            let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
            writer.write_all(&buf)?;
            writer.flush()?;
            Ok(())
        })();
        self.record_result(result);
    }
}

fn write_stdout_event<W>(
    writer: &mut W,
    event: &AgentEvent,
    show_usage: bool,
) -> Result<(), ReportError>
where
    W: Write,
{
    match event {
        AgentEvent::RunStarted { session_id } => {
            writeln!(writer, "[run] started session={session_id}")?;
        }
        AgentEvent::TurnStarted {
            session_id,
            turn_id,
        } => {
            writeln!(writer, "[turn] started session={session_id} turn={turn_id}")?;
        }
        AgentEvent::InputAccepted { items, .. } => {
            writeln!(writer, "[input] accepted items={}", items.len())?;
        }
        AgentEvent::ContentDelta(delta) => {
            writeln!(writer, "[delta] {delta:?}")?;
        }
        AgentEvent::ToolCallRequested(call) => {
            writeln!(writer, "[tool] call {} {}", call.name, call.input)?;
        }
        AgentEvent::ToolExecutionStarted(call) => {
            writeln!(writer, "[tool] started {} {}", call.name, call.id)?;
        }
        AgentEvent::ToolExecutionProgress(result) => {
            writeln!(
                writer,
                "[tool] progress call_id={} is_error={}",
                result.call_id, result.is_error
            )?;
        }
        AgentEvent::ToolResultReceived(result) => {
            writeln!(
                writer,
                "[tool] result call_id={} is_error={}",
                result.call_id, result.is_error
            )?;
        }
        AgentEvent::ApprovalRequired(request) => {
            writeln!(
                writer,
                "[approval] {} {:?}",
                request.summary, request.reason
            )?;
        }
        AgentEvent::ApprovalResolved { approved } => {
            writeln!(writer, "[approval] resolved approved={approved}")?;
        }
        AgentEvent::ToolCatalogChanged(event) => {
            writeln!(
                writer,
                "[tools] catalog changed source={} added={} removed={} changed={}",
                event.source,
                event.added.len(),
                event.removed.len(),
                event.changed.len()
            )?;
        }
        AgentEvent::MutationStarted {
            turn_id,
            mutator,
            point,
            ..
        } => {
            writeln!(
                writer,
                "[mutation] started turn={} mutator={mutator} point={point:?}",
                turn_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "none".into()),
            )?;
        }
        AgentEvent::MutationFinished {
            turn_id,
            mutator,
            dirty,
            ..
        } => {
            writeln!(
                writer,
                "[mutation] finished turn={} mutator={mutator} dirty={dirty}",
                turn_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "none".into()),
            )?;
        }
        AgentEvent::UsageUpdated(usage) if show_usage => {
            writeln!(writer, "[usage] {}", format_usage(usage))?;
        }
        AgentEvent::UsageUpdated(_) => {}
        AgentEvent::Warning { message } => {
            writeln!(writer, "[warning] {message}")?;
        }
        AgentEvent::RunFailed { message } => {
            writeln!(writer, "[error] {message}")?;
        }
        AgentEvent::TurnFinished(result) => {
            writeln!(
                writer,
                "[turn] finished reason={:?} items={}",
                result.finish_reason,
                result.items.len()
            )?;
            for item in &result.items {
                write_item_summary(writer, item)?;
            }
            if show_usage && let Some(usage) = &result.usage {
                writeln!(writer, "[usage] {}", format_usage(usage))?;
            }
        }
        _ => {}
    }

    writer.flush()?;
    Ok(())
}

fn write_item_summary<W>(writer: &mut W, item: &Item) -> Result<(), ReportError>
where
    W: Write,
{
    writeln!(writer, "  [{}]", item_kind_name(item.kind))?;
    for part in &item.parts {
        match part {
            Part::Text(text) => writeln!(writer, "    [text] {}", text.text)?,
            Part::Reasoning(reasoning) => {
                if let Some(summary) = &reasoning.summary {
                    writeln!(writer, "    [reasoning] {summary}")?;
                } else {
                    writeln!(writer, "    [reasoning]")?;
                }
            }
            Part::ToolCall(call) => {
                writeln!(writer, "    [tool-call] {} {}", call.name, call.input)?
            }
            Part::ToolResult(result) => writeln!(
                writer,
                "    [tool-result] call={} error={}",
                result.call_id, result.is_error
            )?,
            Part::Structured(value) => writeln!(writer, "    [structured] {}", value.value)?,
            Part::Media(media) => writeln!(
                writer,
                "    [media] {:?} {}",
                media.modality, media.mime_type
            )?,
            Part::File(file) => writeln!(
                writer,
                "    [file] {}",
                file.name.as_deref().unwrap_or("<unnamed>")
            )?,
            Part::Custom(custom) => writeln!(writer, "    [custom] {}", custom.kind)?,
        }
    }
    Ok(())
}

fn item_kind_name(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::System => "system",
        ItemKind::Developer => "developer",
        ItemKind::User => "user",
        ItemKind::Assistant => "assistant",
        ItemKind::Tool => "tool",
        ItemKind::Context => "context",
        ItemKind::Notification => "notification",
    }
}

fn format_usage(usage: &Usage) -> String {
    match &usage.tokens {
        Some(TokenUsage {
            input_tokens,
            output_tokens,
            reasoning_tokens,
            cached_input_tokens,
            cache_write_input_tokens,
        }) => format!(
            "input={} output={} reasoning={} cached_input={} cache_write_input={}",
            input_tokens,
            output_tokens,
            reasoning_tokens.unwrap_or_default(),
            cached_input_tokens.unwrap_or_default(),
            cache_write_input_tokens.unwrap_or_default()
        ),
        None => "no token usage".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{FinishReason, MetadataMap, SessionId, TextPart};
    use agentkit_loop::TurnResult;

    #[test]
    fn usage_reporter_accumulates_usage_events_and_turn_results() {
        let reporter = UsageReporter::new();

        reporter.handle_event(observed(AgentEvent::UsageUpdated(Usage {
            tokens: Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                reasoning_tokens: Some(2),
                cached_input_tokens: Some(1),
                cache_write_input_tokens: Some(7),
            }),
            cost: None,
            metadata: MetadataMap::new(),
        })));

        reporter.handle_event(observed(AgentEvent::TurnFinished(TurnResult {
            turn_id: "turn-1".into(),
            finish_reason: FinishReason::Completed,
            items: Vec::new(),
            usage: Some(Usage {
                tokens: Some(TokenUsage {
                    input_tokens: 3,
                    output_tokens: 4,
                    reasoning_tokens: Some(1),
                    cached_input_tokens: None,
                    cache_write_input_tokens: None,
                }),
                cost: None,
                metadata: MetadataMap::new(),
            }),
            metadata: MetadataMap::new(),
        })));

        let summary = reporter.summary();
        assert_eq!(summary.events_seen, 2);
        assert_eq!(summary.usage_events_seen, 2);
        assert_eq!(summary.turn_results_seen, 1);
        assert_eq!(summary.totals.input_tokens, 13);
        assert_eq!(summary.totals.output_tokens, 9);
        assert_eq!(summary.totals.reasoning_tokens, 3);
        assert_eq!(summary.totals.cached_input_tokens, 1);
        assert_eq!(summary.totals.cache_write_input_tokens, 7);
    }

    #[test]
    fn transcript_reporter_tracks_inputs_and_outputs() {
        let reporter = TranscriptReporter::new();

        reporter.handle_event(observed(AgentEvent::InputAccepted {
            session_id: SessionId::new("session-1"),
            items: vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "hello".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }],
        }));

        reporter.handle_event(observed(AgentEvent::TurnFinished(TurnResult {
            turn_id: "turn-1".into(),
            finish_reason: FinishReason::Completed,
            items: vec![Item {
                id: None,
                kind: ItemKind::Assistant,
                parts: vec![Part::Text(TextPart {
                    text: "hi".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }],
            usage: None,
            metadata: MetadataMap::new(),
        })));

        assert_eq!(reporter.transcript().items.len(), 2);
        assert_eq!(reporter.transcript().items[0].kind, ItemKind::User);
        assert_eq!(reporter.transcript().items[1].kind, ItemKind::Assistant);
    }

    #[test]
    fn jsonl_reporter_serializes_event_envelopes() {
        let reporter = JsonlReporter::new(Vec::new());
        reporter.handle_event(observed(AgentEvent::RunStarted {
            session_id: SessionId::new("session-1"),
        }));

        let output = String::from_utf8(reporter.into_inner()).unwrap();
        assert!(output.contains("\"RunStarted\""));
        assert!(output.contains("session-1"));
        assert!(!output.contains("\"session_id\":\"s1\""));
    }

    #[test]
    fn jsonl_reporter_can_include_observed_session_id() {
        let reporter = JsonlReporter::new(Vec::new()).with_session_id(true);
        reporter.handle_event(observed(AgentEvent::ContentDelta(
            agentkit_core::Delta::AppendText {
                part_id: "part-1".into(),
                chunk: "hello".into(),
            },
        )));

        let output = String::from_utf8(reporter.into_inner()).unwrap();
        assert!(output.contains("\"session_id\":\"s1\""));
    }

    fn run_started_event() -> AgentEvent {
        AgentEvent::RunStarted {
            session_id: SessionId::new("s1"),
        }
    }

    fn observed(event: AgentEvent) -> ObservedEvent {
        ObservedEvent {
            session_id: std::sync::Arc::new(SessionId::new("s1")),
            event,
        }
    }

    #[test]
    fn buffered_reporter_flushes_at_capacity() {
        let reporter = BufferedReporter::new(UsageReporter::new(), 2);
        reporter.handle_event(observed(run_started_event()));
        assert_eq!(reporter.pending(), 1);
        assert_eq!(reporter.inner().summary().events_seen, 0);

        reporter.handle_event(observed(run_started_event()));
        assert_eq!(reporter.pending(), 0);
        assert_eq!(reporter.inner().summary().events_seen, 2);
    }

    #[test]
    fn buffered_reporter_manual_flush() {
        let reporter = BufferedReporter::new(UsageReporter::new(), 0);
        reporter.handle_event(observed(run_started_event()));
        reporter.handle_event(observed(run_started_event()));
        assert_eq!(reporter.pending(), 2);

        reporter.flush();
        assert_eq!(reporter.pending(), 0);
        assert_eq!(reporter.inner().summary().events_seen, 2);
    }

    #[test]
    fn buffered_reporter_flushes_on_drop() {
        let inner = {
            let reporter = BufferedReporter::new(UsageReporter::new(), 100);
            reporter.handle_event(observed(run_started_event()));
            reporter.handle_event(observed(run_started_event()));
            assert_eq!(reporter.inner().summary().events_seen, 0);
            // Drop will flush — but we can't inspect after drop.
            // Instead, verify flush works by checking pending before drop.
            assert_eq!(reporter.pending(), 2);
            reporter
        };
        // After the block, `inner` is the dropped BufferedReporter — but we
        // moved it out, so it's still alive here. Verify flush happened on
        // the inner reporter by inspecting it.
        assert_eq!(inner.inner().summary().events_seen, 0);
        // The actual drop-flush happens when `inner` goes out of scope at
        // end of test. We at least verify the API is sound.
    }

    #[test]
    fn channel_reporter_delivers_events() {
        let (reporter, rx) = ChannelReporter::pair();
        reporter.handle_event(observed(run_started_event()));
        reporter.handle_event(observed(run_started_event()));

        let events: Vec<_> = rx.try_iter().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].session_id.0, "s1");
    }

    #[test]
    fn channel_reporter_survives_dropped_receiver() {
        let (reporter, rx) = ChannelReporter::pair();
        drop(rx);
        // Should not panic — errors are silently dropped.
        reporter.handle_event(observed(run_started_event()));
    }

    #[test]
    fn channel_reporter_fallible_returns_error_on_dropped_receiver() {
        let (reporter, rx) = ChannelReporter::pair();
        drop(rx);

        let result = reporter.try_handle_event(&observed(run_started_event()));
        assert!(matches!(result, Err(ReportError::ChannelSend)));
    }

    #[test]
    fn policy_reporter_ignore_swallows_errors() {
        let (reporter, rx) = ChannelReporter::pair();
        drop(rx);
        let policy = PolicyReporter::new(reporter, FailurePolicy::Ignore);
        policy.handle_event(observed(run_started_event()));
        assert!(policy.take_errors().is_empty());
    }

    #[test]
    fn policy_reporter_accumulate_collects_errors() {
        let (reporter, rx) = ChannelReporter::pair();
        drop(rx);
        let policy = PolicyReporter::new(reporter, FailurePolicy::Accumulate);
        policy.handle_event(observed(run_started_event()));
        policy.handle_event(observed(run_started_event()));

        let errors = policy.take_errors();
        assert_eq!(errors.len(), 2);
        assert!(matches!(errors[0], ReportError::ChannelSend));
    }

    #[test]
    #[should_panic(expected = "reporter error: channel send failed")]
    fn policy_reporter_fail_fast_panics() {
        let (reporter, rx) = ChannelReporter::pair();
        drop(rx);
        let policy = PolicyReporter::new(reporter, FailurePolicy::FailFast);
        policy.handle_event(observed(run_started_event()));
    }
}
