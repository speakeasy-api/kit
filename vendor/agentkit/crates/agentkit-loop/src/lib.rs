//! Runtime-agnostic agent loop orchestration for sessions, turns, tools, and interrupts.
//!
//! `agentkit-loop` is the central coordination layer in the agentkit workspace.  It
//! drives a model through a multi-turn agentic loop, executing tool calls,
//! respecting permission checks, surfacing approval interrupts to the host
//! application, and optionally compacting the transcript when it grows too large.
//!
//! # Architecture
//!
//! The main entry point is [`Agent`], constructed via [`AgentBuilder`]. The
//! builder optionally accepts the prior conversation transcript via
//! [`AgentBuilder::transcript`] and the next user turn via
//! [`AgentBuilder::input`] — both default to empty. Calling
//! [`Agent::start`] with a [`SessionConfig`] returns a [`LoopDriver`] that
//! yields [`LoopStep`]s — either a finished turn or an interrupt that
//! requires host resolution before the loop can continue.
//!
//! If no input was preloaded, the first call to [`LoopDriver::next`] yields
//! [`LoopInterrupt::AwaitingInput`] and the host supplies the first user
//! turn via [`InputRequest::submit`]. If input was preloaded, the first
//! `next()` dispatches the model directly — convenient for one-shot calls.
//!
//! ```text
//! Agent::builder()
//!     .model(adapter)              // ModelAdapter implementation
//!     .add_tool_source(registry)   // ToolRegistry (or any ToolSource); call again to federate
//!     .permissions(checker)        // PermissionChecker for gating tool use
//!     .observer(obs)               // LoopObserver for streaming events
//!     .transcript(prior)           // optional: passive prior transcript (system prompt, resumed session)
//!     .input(first_user_turn)      // optional: preload next user turn so first next() drives a turn
//!     .build()?
//!     .start(config).await?  -> LoopDriver
//!         .next().await?     -> LoopStep::Finished | LoopStep::Interrupt(...)
//! ```
//!
//! # Example
//!
//! ```rust,no_run
//! use agentkit_core::{Item, ItemKind};
//! use agentkit_loop::{
//!     Agent, PromptCacheRequest, PromptCacheRetention, SessionConfig,
//! };
//!
//! # async fn example<M: agentkit_loop::ModelAdapter>(adapter: M) -> Result<(), agentkit_loop::LoopError> {
//! // One-shot: preload system prompt and first user message; first next()
//! // drives the model directly.
//! let agent = Agent::builder()
//!     .model(adapter)
//!     .transcript(vec![Item::text(ItemKind::System, "You are a helpful assistant.")])
//!     .input(vec![Item::text(ItemKind::User, "Hello!")])
//!     .build()?;
//!
//! let mut driver = agent
//!     .start(SessionConfig::new("demo").with_cache(
//!         PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
//!     ))
//!     .await?;
//!
//! let _ = driver.next().await?;
//! # Ok(())
//! # }
//! ```

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use agentkit_core::{
    CancellationHandle, Delta, FinishReason, Item, ItemKind, MetadataMap, Part, SessionId, TaskId,
    TextPart, Timestamp, ToolCallId, ToolCallPart, ToolOutput, ToolResultPart, TurnCancellation,
    Usage,
};
use agentkit_task_manager::{
    PendingLoopUpdates, SimpleTaskManager, TOOL_RESULT_NOT_STARTED_METADATA_KEY, TaskApproval,
    TaskLaunchKind, TaskLaunchRequest, TaskManager, TaskResolution, TaskStartContext,
    TaskStartOutcome, TurnTaskUpdate,
};
#[cfg(test)]
use agentkit_task_manager::{
    TOOL_RESULT_FAILURE_KIND_METADATA_KEY, TOOL_RESULT_FAILURE_KIND_PERMISSION_DENIED,
};
#[cfg(test)]
use agentkit_tools_core::ToolContext;
use agentkit_tools_core::{
    AllowAllPermissions, ApprovalDecision, ApprovalRequest, BasicToolExecutor, OwnedToolContext,
    PermissionChecker, ToolCatalogEvent, ToolError, ToolExecutionScope, ToolExecutor, ToolRequest,
    ToolResources, ToolSource, ToolSpec,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

const INTERRUPTED_METADATA_KEY: &str = "agentkit.interrupted";
const INTERRUPT_REASON_METADATA_KEY: &str = "agentkit.interrupt_reason";
const INTERRUPT_STAGE_METADATA_KEY: &str = "agentkit.interrupt_stage";
const USER_CANCELLED_REASON: &str = "user_cancelled";

/// Configuration required to start a new model session.
///
/// Pass this to [`Agent::start`] to initialise the underlying [`ModelSession`]
/// and obtain a [`LoopDriver`].
///
/// # Example
///
/// ```rust
/// use agentkit_loop::{PromptCacheRequest, PromptCacheRetention, SessionConfig};
///
/// let config = SessionConfig::new("my-session").with_cache(
///     PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
/// );
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    /// Unique identifier for the session.
    pub session_id: SessionId,
    /// Arbitrary key-value metadata forwarded to the model adapter.
    pub metadata: MetadataMap,
    /// Default provider-side prompt caching policy for turns in this session.
    pub cache: Option<PromptCacheRequest>,
    /// Default provider-neutral structured-output request for turns in this session.
    pub structured_output: Option<StructuredOutputRequest>,
}

impl SessionConfig {
    /// Builds a session config with empty metadata and no cache policy.
    pub fn new(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            metadata: MetadataMap::new(),
            cache: None,
            structured_output: None,
        }
    }

    /// Replaces the session metadata map.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }

    /// Sets the default prompt cache request for the session.
    pub fn with_cache(mut self, cache: PromptCacheRequest) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Clears any default prompt cache request for the session.
    pub fn without_cache(mut self) -> Self {
        self.cache = None;
        self
    }

    /// Sets the default structured-output request for this session.
    pub fn with_structured_output(mut self, request: StructuredOutputRequest) -> Self {
        self.structured_output = Some(request);
        self
    }

    /// Clears the default structured-output request for this session.
    pub fn without_structured_output(mut self) -> Self {
        self.structured_output = None;
        self
    }
}

/// A provider-neutral JSON structured-output request.
///
/// The digest is derived from the canonical serialized schema and cannot be
/// supplied independently by callers.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "StructuredOutputWire", into = "StructuredOutputWire")]
pub struct StructuredOutputRequest {
    name: String,
    version: u16,
    strict: bool,
    schema: Value,
    schema_digest: String,
    max_output_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StructuredOutputWire {
    name: String,
    version: u16,
    strict: bool,
    schema: Value,
    schema_digest: String,
    max_output_bytes: usize,
}

impl StructuredOutputRequest {
    /// Creates a request and binds its digest to the exact schema value.
    pub fn new(
        name: impl Into<String>,
        version: u16,
        strict: bool,
        schema: Value,
    ) -> Result<Self, StructuredOutputError> {
        let name = name.into();
        if name.is_empty() || version == 0 || !schema.is_object() {
            return Err(StructuredOutputError::InvalidRequest);
        }
        let schema_digest = structured_output_digest(&schema)?;
        Ok(Self {
            name,
            version,
            strict,
            schema,
            schema_digest,
            max_output_bytes: 64 * 1024 * 1024,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn version(&self) -> u16 {
        self.version
    }

    pub const fn strict(&self) -> bool {
        self.strict
    }

    pub fn schema(&self) -> &Value {
        &self.schema
    }

    pub fn schema_digest(&self) -> &str {
        &self.schema_digest
    }

    /// Applies the byte ceiling that adapters must enforce before parsing.
    pub fn with_max_output_bytes(mut self, maximum: usize) -> Result<Self, StructuredOutputError> {
        if maximum == 0 {
            return Err(StructuredOutputError::InvalidRequest);
        }
        self.max_output_bytes = maximum;
        Ok(self)
    }

    pub const fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }
}

impl From<StructuredOutputRequest> for StructuredOutputWire {
    fn from(request: StructuredOutputRequest) -> Self {
        Self {
            name: request.name,
            version: request.version,
            strict: request.strict,
            schema: request.schema,
            schema_digest: request.schema_digest,
            max_output_bytes: request.max_output_bytes,
        }
    }
}

impl TryFrom<StructuredOutputWire> for StructuredOutputRequest {
    type Error = StructuredOutputError;

    fn try_from(wire: StructuredOutputWire) -> Result<Self, Self::Error> {
        let request = Self::new(wire.name, wire.version, wire.strict, wire.schema)?
            .with_max_output_bytes(wire.max_output_bytes)?;
        if request.schema_digest != wire.schema_digest {
            return Err(StructuredOutputError::DigestMismatch);
        }
        Ok(request)
    }
}

fn structured_output_digest(schema: &Value) -> Result<String, StructuredOutputError> {
    let bytes = serde_json::to_vec(schema).map_err(|_| StructuredOutputError::InvalidRequest)?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum StructuredOutputError {
    #[error("invalid structured-output request")]
    InvalidRequest,
    #[error("structured-output schema digest mismatch")]
    DigestMismatch,
}

/// Structured-output behavior sealed into a concrete provider adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructuredOutputCapability {
    version: String,
    strict: bool,
    max_schema_bytes: usize,
}

/// Provider evidence that a correlated structured-output request was honored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredOutputEvidence {
    pub name: String,
    pub version: u16,
    pub strict: bool,
    pub schema_digest: String,
    pub session_id: String,
    pub turn_id: String,
    pub honored: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl StructuredOutputEvidence {
    pub fn honors(&self, request: &StructuredOutputRequest, turn: &TurnRequest) -> bool {
        self.honored
            && self.error.is_none()
            && self.name == request.name()
            && self.version == request.version()
            && self.strict == request.strict()
            && self.schema_digest == request.schema_digest()
            && self.session_id == turn.session_id.to_string()
            && self.turn_id == turn.turn_id.to_string()
    }
}

impl StructuredOutputCapability {
    pub fn new(version: impl Into<String>, strict: bool, max_schema_bytes: usize) -> Option<Self> {
        let version = version.into();
        (!version.is_empty() && max_schema_bytes != 0).then_some(Self {
            version,
            strict,
            max_schema_bytes,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub const fn strict(&self) -> bool {
        self.strict
    }

    pub const fn max_schema_bytes(&self) -> usize {
        self.max_schema_bytes
    }
}

/// Strength of a prompt-cache request.
///
/// `BestEffort` lets adapters ignore unsupported controls while still using
/// any provider-native automatic caching they support. `Required` upgrades
/// unsupported cache requests into provider errors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptCacheMode {
    /// Disable prompt caching for this request.
    Disabled,
    /// Use caching when the provider can honor the request.
    #[default]
    BestEffort,
    /// Fail the turn if the provider cannot honor the request.
    Required,
}

/// High-level provider-neutral cache retention hint.
///
/// Providers map this to their native controls. For example, OpenAI maps
/// `Short` to in-memory retention while OpenRouter Anthropic models map it to
/// the default 5-minute ephemeral cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptCacheRetention {
    /// Use the provider's default cache retention.
    Default,
    /// Prefer the provider's short-lived cache retention mode.
    Short,
    /// Prefer the provider's longest generally available cache retention mode.
    Extended,
}

/// Provider-neutral prompt caching strategy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PromptCacheStrategy {
    /// Let the provider decide the cacheable prefix automatically.
    #[default]
    Automatic,
    /// Apply explicit cache breakpoints to selected prefix boundaries.
    Explicit {
        /// Cache breakpoints in transcript/tool order.
        breakpoints: Vec<PromptCacheBreakpoint>,
    },
}

impl PromptCacheStrategy {
    /// Uses the provider's native automatic cache behavior when available, or
    /// any adapter-provided automatic planning fallback.
    pub fn automatic() -> Self {
        Self::Automatic
    }

    /// Uses explicit cache breakpoints.
    pub fn explicit(breakpoints: impl IntoIterator<Item = PromptCacheBreakpoint>) -> Self {
        Self::Explicit {
            breakpoints: breakpoints.into_iter().collect(),
        }
    }
}

/// Prefix boundary that a provider should cache when using explicit caching.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PromptCacheBreakpoint {
    /// Cache the tool schema prefix through the last available tool.
    ToolsEnd,
    /// Cache through the end of the transcript item at `index`.
    TranscriptItemEnd { index: usize },
    /// Cache through the specific transcript part.
    ///
    /// Not every adapter can target every part precisely; unsupported
    /// fine-grained breakpoints become best-effort no-ops unless the request is
    /// marked [`PromptCacheMode::Required`].
    TranscriptPartEnd {
        item_index: usize,
        part_index: usize,
    },
}

impl PromptCacheBreakpoint {
    /// Cache the tool schema prefix through the last available tool.
    pub fn tools_end() -> Self {
        Self::ToolsEnd
    }

    /// Cache through the end of a transcript item.
    pub fn transcript_item_end(index: usize) -> Self {
        Self::TranscriptItemEnd { index }
    }

    /// Cache through a specific part within a transcript item.
    pub fn transcript_part_end(item_index: usize, part_index: usize) -> Self {
        Self::TranscriptPartEnd {
            item_index,
            part_index,
        }
    }
}

/// Prompt caching request sent alongside a turn.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCacheRequest {
    /// Strength of the caching request.
    pub mode: PromptCacheMode,
    /// Automatic or explicit caching strategy.
    pub strategy: PromptCacheStrategy,
    /// Optional provider-neutral retention hint.
    pub retention: Option<PromptCacheRetention>,
    /// Optional provider cache key or routing key.
    pub key: Option<String>,
}

impl PromptCacheRequest {
    /// Builds a best-effort automatic cache request.
    pub fn automatic() -> Self {
        Self::best_effort(PromptCacheStrategy::automatic())
    }

    /// Builds a required automatic cache request.
    pub fn automatic_required() -> Self {
        Self::required(PromptCacheStrategy::automatic())
    }

    /// Builds a best-effort explicit cache request.
    pub fn explicit(breakpoints: impl IntoIterator<Item = PromptCacheBreakpoint>) -> Self {
        Self::best_effort(PromptCacheStrategy::explicit(breakpoints))
    }

    /// Builds a required explicit cache request.
    pub fn explicit_required(breakpoints: impl IntoIterator<Item = PromptCacheBreakpoint>) -> Self {
        Self::required(PromptCacheStrategy::explicit(breakpoints))
    }

    /// Builds a disabled cache request.
    pub fn disabled() -> Self {
        Self {
            mode: PromptCacheMode::Disabled,
            strategy: PromptCacheStrategy::Automatic,
            retention: None,
            key: None,
        }
    }

    /// Builds a best-effort cache request with the given strategy.
    pub fn best_effort(strategy: PromptCacheStrategy) -> Self {
        Self {
            mode: PromptCacheMode::BestEffort,
            strategy,
            retention: None,
            key: None,
        }
    }

    /// Builds a required cache request with the given strategy.
    pub fn required(strategy: PromptCacheStrategy) -> Self {
        Self {
            mode: PromptCacheMode::Required,
            strategy,
            retention: None,
            key: None,
        }
    }

    /// Overrides the request mode.
    pub fn with_mode(mut self, mode: PromptCacheMode) -> Self {
        self.mode = mode;
        self
    }

    /// Overrides the request strategy.
    pub fn with_strategy(mut self, strategy: PromptCacheStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Applies a provider-neutral retention hint.
    pub fn with_retention(mut self, retention: PromptCacheRetention) -> Self {
        self.retention = Some(retention);
        self
    }

    /// Applies a provider cache key or routing key.
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// Clears any provider-neutral retention hint.
    pub fn without_retention(mut self) -> Self {
        self.retention = None;
        self
    }

    /// Clears any provider cache key or routing key.
    pub fn without_key(mut self) -> Self {
        self.key = None;
        self
    }

    /// Returns true when caching should be active for this request.
    pub fn is_enabled(&self) -> bool {
        !matches!(self.mode, PromptCacheMode::Disabled)
    }
}

/// Payload sent to the model at the start of each turn.
///
/// The [`LoopDriver`] constructs this automatically from its internal state
/// and passes it to [`ModelSession::begin_turn`].  Model adapter authors
/// use the fields to build the provider-specific request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnRequest {
    /// Session this turn belongs to.
    pub session_id: SessionId,
    /// Unique identifier for the current turn.
    pub turn_id: agentkit_core::TurnId,
    /// Full conversation transcript accumulated so far.
    pub transcript: Vec<Item>,
    /// Tool specifications the model may invoke during this turn.
    pub available_tools: Vec<ToolSpec>,
    /// Provider-side prompt caching request for this turn.
    pub cache: Option<PromptCacheRequest>,
    /// Provider-neutral structured-output request for this turn.
    pub structured_output: Option<StructuredOutputRequest>,
    /// Per-turn metadata (e.g. provider hints).
    pub metadata: MetadataMap,
}

/// Final result produced by a single model turn.
///
/// Returned inside [`ModelTurnEvent::Finished`] to signal that the model has
/// completed its generation for this turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelTurnResult {
    /// Why the model stopped generating (e.g. completed, tool call, length).
    pub finish_reason: FinishReason,
    /// Items the model produced during this turn (text, tool calls, etc.).
    pub output_items: Vec<Item>,
    /// Token usage statistics, if available.
    pub usage: Option<Usage>,
    /// Provider-specific metadata about the turn.
    pub metadata: MetadataMap,
    /// Model identifier reported by the provider for this turn, if known.
    ///
    /// Stamped onto inference telemetry spans as `gen_ai.response.model`.
    #[serde(default)]
    pub model: Option<String>,
    /// Provider-assigned response identifier for this turn, if known.
    ///
    /// Stamped onto inference telemetry spans as `gen_ai.response.id`.
    #[serde(default)]
    pub response_id: Option<String>,
}

/// Streaming event emitted by a [`ModelTurn`] during generation.
///
/// The [`LoopDriver`] consumes these events one-by-one via
/// [`ModelTurn::next_event`] and translates them into [`AgentEvent`]s for
/// observers and into transcript mutations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ModelTurnEvent {
    /// Incremental text or content delta from the model.
    Delta(Delta),
    /// The model is requesting a tool call.
    ToolCall(ToolCallPart),
    /// Updated token usage statistics.
    Usage(Usage),
    /// The model has finished generating for this turn.
    Finished(ModelTurnResult),
}

/// Factory for creating model sessions.
///
/// Implement this trait to integrate a model provider (e.g. OpenRouter,
/// Anthropic, a local LLM server) with the agent loop.  [`Agent`] holds a
/// single adapter and calls [`start_session`](ModelAdapter::start_session)
/// once when [`Agent::start`] is invoked.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_loop::{ModelAdapter, ModelSession, SessionConfig, LoopError};
/// use async_trait::async_trait;
///
/// struct MyAdapter;
///
/// #[async_trait]
/// impl ModelAdapter for MyAdapter {
///     type Session = MySession;
///
///     async fn start_session(&self, config: SessionConfig) -> Result<MySession, LoopError> {
///         // Initialize provider-specific session state here.
///         Ok(MySession { /* ... */ })
///     }
/// }
/// # struct MySession;
/// # #[async_trait]
/// # impl ModelSession for MySession {
/// #     type Turn = MyTurn;
/// #     async fn begin_turn(&mut self, _r: agentkit_loop::TurnRequest, _c: Option<agentkit_core::TurnCancellation>) -> Result<MyTurn, LoopError> { todo!() }
/// # }
/// # struct MyTurn;
/// # #[async_trait]
/// # impl agentkit_loop::ModelTurn for MyTurn {
/// #     async fn next_event(&mut self, _c: Option<agentkit_core::TurnCancellation>) -> Result<Option<agentkit_loop::ModelTurnEvent>, LoopError> { todo!() }
/// # }
/// ```
#[async_trait]
pub trait ModelAdapter: Send + Sync {
    /// The session type produced by this adapter.
    type Session: ModelSession;

    /// Create a new model session from the given configuration.
    ///
    /// # Errors
    ///
    /// Returns [`LoopError`] if the provider connection or initialisation fails.
    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError>;

    /// Name of the underlying model provider, when known.
    ///
    /// Stamped onto agent telemetry spans as the `gen_ai.provider.name`
    /// attribute from the OpenTelemetry GenAI semantic conventions. Use a
    /// lowercase identifier (e.g. `openrouter`, `ollama`). The default
    /// returns `None` for adapters without a meaningful provider identity.
    fn provider_name(&self) -> Option<&str> {
        None
    }
}

/// An active model session that can produce sequential turns.
///
/// A session is created once per [`Agent::start`] call and lives for the
/// lifetime of the [`LoopDriver`].  Each call to [`begin_turn`](ModelSession::begin_turn)
/// hands the full transcript to the model and returns a streaming
/// [`ModelTurn`].
#[async_trait]
pub trait ModelSession: Send {
    /// The turn type produced by this session.
    type Turn: ModelTurn;

    /// Start a new turn, sending the transcript and available tools to the model.
    ///
    /// # Arguments
    ///
    /// * `request` -- the turn payload including transcript and tool specs.
    /// * `cancellation` -- optional handle the implementation should poll to
    ///   detect user-initiated cancellation.
    ///
    /// # Errors
    ///
    /// Returns [`LoopError::Cancelled`] when the turn is cancelled, or a
    /// provider-specific error wrapped in [`LoopError`].
    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError>;

    /// Applies deterministic request projection before provider dispatch.
    /// Durable wrappers call this only after authorization and budget admission.
    fn prepare_turn(&mut self, _request: &mut TurnRequest) -> Result<(), LoopError> {
        Ok(())
    }

    /// Returns the structured-output capability owned by this concrete session.
    fn structured_output_capability(&self) -> Option<&StructuredOutputCapability> {
        None
    }

    /// Model identifier this session sends requests to, when known.
    ///
    /// Stamped onto inference telemetry spans as the `gen_ai.request.model`
    /// attribute from the OpenTelemetry GenAI semantic conventions. The
    /// default returns `None` for sessions without a fixed model.
    fn model_name(&self) -> Option<&str> {
        None
    }
}

/// A streaming model turn that yields events one at a time.
///
/// The loop driver calls [`next_event`](ModelTurn::next_event) repeatedly
/// until it returns `Ok(None)` (stream exhausted) or
/// `Ok(Some(ModelTurnEvent::Finished(_)))`.
#[async_trait]
pub trait ModelTurn: Send {
    /// Retrieve the next event from the model's response stream.
    ///
    /// Returns `Ok(None)` when the stream is exhausted.
    ///
    /// # Errors
    ///
    /// Returns [`LoopError::Cancelled`] if `cancellation` fires, or a
    /// provider-specific error wrapped in [`LoopError`].
    async fn next_event(
        &mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError>;
}

/// Observer hook for streaming agent events to the host application.
///
/// Register observers via [`AgentBuilder::observer`] to receive real-time
/// notifications about deltas, tool calls, usage, warnings, and lifecycle
/// events.
///
/// # Example
///
/// ```rust
/// use agentkit_loop::{LoopObserver, ObservedEvent};
///
/// struct StdoutObserver;
///
/// impl LoopObserver for StdoutObserver {
///     fn handle_event(&self, event: ObservedEvent) {
///         println!("{:?}", event.event);
///     }
/// }
/// ```
pub trait LoopObserver: Send + Sync {
    /// Called synchronously for every [`AgentEvent`] emitted by the loop driver.
    /// Observers store mutable state behind interior mutability (`Mutex`,
    /// atomics, channels) so the driver can share an `Arc<dyn LoopObserver>`
    /// across reusable [`Agent`] starts.
    fn handle_event(&self, event: ObservedEvent);
}

/// Session-addressed [`AgentEvent`] envelope delivered to [`LoopObserver`]s.
///
/// Some event variants carry their own session fields, but many high-volume
/// events intentionally stay compact. The envelope gives shared observers a
/// consistent routing key without reshaping every [`AgentEvent`] variant.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservedEvent {
    /// Session this event belongs to.
    pub session_id: Arc<SessionId>,
    /// The operational event emitted by the driver.
    pub event: AgentEvent,
}

/// Receives full [`Item`]s as they are appended to the driver's transcript.
///
/// While [`LoopObserver`] surfaces operational events (deltas, tool calls,
/// lifecycle, telemetry), it can't be reconstructed back into a faithful
/// transcript on its own — content deltas span partial parts and don't
/// carry their parent-Item identity, and historically tool results were
/// pushed into the transcript with no observer event at all. A
/// `TranscriptObserver` is the loss-free counterpart: it fires once per
/// [`Item`] appended, with the full Item shape ready for persistence,
/// replication, or audit.
///
/// Observers are called *synchronously* from inside the driver, in the
/// same order items land in the transcript. Compaction-driven transcript
/// rewrites do **not** fire `on_transcript_event` — those are signaled by
/// [`AgentEvent::CompactionFinished`] instead.
///
/// Register via [`AgentBuilder::transcript_observer`]; multiple observers
/// may be registered and are called in registration order.
///
/// # Example
///
/// ```rust
/// use agentkit_core::Item;
/// use agentkit_loop::{TranscriptEvent, TranscriptObserver};
/// use std::sync::atomic::{AtomicUsize, Ordering};
///
/// struct CountingObserver { items: AtomicUsize }
///
/// impl TranscriptObserver for CountingObserver {
///     fn on_transcript_event(&self, _event: TranscriptEvent<'_>) {
///         self.items.fetch_add(1, Ordering::Relaxed);
///     }
/// }
/// ```
pub trait TranscriptObserver: Send + Sync {
    /// Called synchronously every time an [`Item`] is appended to the
    /// driver's transcript, in transcript order. Observers store mutable
    /// state behind interior mutability so the driver can share an
    /// `Arc<dyn TranscriptObserver>`.
    fn on_transcript_event(&self, event: TranscriptEvent<'_>);
}

/// Session-addressed transcript append event delivered to
/// [`TranscriptObserver`]s.
#[derive(Clone, Debug)]
pub struct TranscriptEvent<'a> {
    /// Session this transcript append belongs to.
    pub session_id: &'a SessionId,
    /// Full item that was appended.
    pub item: &'a Item,
}

/// Where in the loop a [`LoopMutator`] is given a chance to modify the
/// transcript. Mutators run synchronously at these points; mid-stream
/// mutation (e.g. between content deltas) is intentionally not supported
/// because the assistant item is not yet fully constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum MutationPoint {
    /// A tool result has just been appended; the next loop step will be
    /// another inference call.
    AfterToolResult,
    /// A turn has fully ended (assistant final, interrupt, or cancellation)
    /// and any new user input has not yet been dispatched.
    AfterTurnEnded,
}

/// Sink for emitting [`AgentEvent`]s from inside a [`LoopMutator`].
/// The driver supplies a concrete implementation via [`LoopCtx::emitter`].
pub trait EventEmitter: Send + Sync {
    /// Forward `event` to all registered observers.
    fn emit(&self, event: AgentEvent);
}

/// Read-only context handed to a [`LoopMutator`] alongside the cursor.
#[non_exhaustive]
pub struct LoopCtx<'a> {
    /// Session this mutation point belongs to.
    pub session_id: &'a SessionId,
    /// Turn the mutation is associated with, if any.
    pub turn_id: Option<&'a agentkit_core::TurnId>,
    /// Where in the loop the mutator is running.
    pub point: MutationPoint,
    /// Cancellation handle for the active turn, if any.
    pub cancellation: Option<TurnCancellation>,
    /// Sink for emitting events from the mutator (telemetry, progress).
    pub emitter: &'a dyn EventEmitter,
}

/// Mutable handle over the live transcript with dirty tracking.
///
/// Implements [`Deref`](std::ops::Deref)/[`DerefMut`](std::ops::DerefMut) to
/// `Vec<Item>` so mutators read and write through `Vec`'s native API
/// (`push`, `retain`, `iter`, `*cursor = ...`). Any `&mut` access marks the
/// cursor dirty; the loop validates transcript invariants when at least one
/// mutator dirtied the transcript and hard-fails on protocol violations.
pub struct TranscriptCursor<'a> {
    items: &'a mut Vec<Item>,
    pub(crate) dirty: bool,
}

impl<'a> std::ops::Deref for TranscriptCursor<'a> {
    type Target = Vec<Item>;
    fn deref(&self) -> &Vec<Item> {
        self.items
    }
}

impl<'a> std::ops::DerefMut for TranscriptCursor<'a> {
    fn deref_mut(&mut self) -> &mut Vec<Item> {
        self.dirty = true;
        self.items
    }
}

/// Async transcript mutator. Registered via [`AgentBuilder::mutator`] and
/// invoked at each [`MutationPoint`]. Mutators own their derived state
/// (e.g. running token totals via interior mutability) and decide for
/// themselves whether and how to modify the transcript.
///
/// The default implementation is a no-op so trait users override only
/// `mutate`.
#[async_trait]
pub trait LoopMutator: Send + Sync {
    /// Run this mutator. Returning without writing to `cursor` is a no-op.
    /// Errors abort the loop; protocol-violating mutations (orphaned tool
    /// uses or results) are detected by validation and turned into
    /// [`LoopError::Mutator`].
    async fn mutate(
        &self,
        cursor: &mut TranscriptCursor<'_>,
        ctx: LoopCtx<'_>,
    ) -> Result<(), LoopError> {
        let _ = (cursor, ctx);
        Ok(())
    }
}

/// Lifecycle and streaming events emitted by the [`LoopDriver`].
///
/// Observers (see [`LoopObserver`]) receive these events in the order they
/// occur.  They are useful for building UIs, logging, or telemetry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AgentEvent {
    /// The agent run has been initialised.
    RunStarted { session_id: SessionId },
    /// A new model turn is starting.
    TurnStarted {
        session_id: SessionId,
        turn_id: agentkit_core::TurnId,
    },
    /// User input has been accepted into the pending queue.
    InputAccepted {
        session_id: SessionId,
        items: Vec<Item>,
    },
    /// Incremental content delta from the model.
    ContentDelta(Delta),
    /// The model has requested a tool call.
    ToolCallRequested(ToolCallPart),
    /// A tool call is about to execute after policy and approval checks.
    ToolExecutionStarted(ToolCallPart),
    /// A tool call has non-terminal progress to report.
    ///
    /// Used for updates such as background detachment. Unlike
    /// [`AgentEvent::ToolResultReceived`], this does not mean the call has
    /// reached a terminal result.
    ToolExecutionProgress(ToolResultPart),
    /// A tool call's result has landed in the transcript.
    ///
    /// Fires once per terminal [`Part::ToolResult`] that's appended.
    /// Cancellation/denial paths (auth cancelled, approval denied) also emit
    /// this with `is_error = true`.
    ///
    /// Correlate with the matching [`AgentEvent::ToolCallRequested`] via
    /// `call_id`.
    ToolResultReceived(ToolResultPart),
    /// A tool call requires explicit user approval before execution.
    ApprovalRequired(ApprovalRequest),
    /// An approval interrupt has been resolved.
    ApprovalResolved { approved: bool },
    /// The available tool catalog changed and will be reflected on the next model request.
    ToolCatalogChanged(ToolCatalogEvent),
    /// A [`LoopMutator`] is about to run at one of the mutation points.
    /// `mutator` is a stable label the implementation chooses for itself.
    MutationStarted {
        session_id: SessionId,
        turn_id: Option<agentkit_core::TurnId>,
        mutator: String,
        point: MutationPoint,
    },
    /// A [`LoopMutator`] has finished running. `dirty` indicates whether the
    /// transcript was modified; `metadata` carries mutator-specific extras
    /// (e.g. compaction reason, replaced item count).
    MutationFinished {
        session_id: SessionId,
        turn_id: Option<agentkit_core::TurnId>,
        mutator: String,
        dirty: bool,
        metadata: MetadataMap,
    },
    /// Updated token usage statistics.
    UsageUpdated(Usage),
    /// Non-fatal warning (e.g. a tool failure that was recovered from).
    Warning { message: String },
    /// The agent run has failed with an unrecoverable error.
    RunFailed { message: String },
    /// A turn has finished (successfully, via cancellation, etc.).
    TurnFinished(TurnResult),
}

/// Handle for a pending approval interrupt.
///
/// Wraps an [`ApprovalRequest`] and provides ergonomic resolution methods
/// so callers can resolve the interrupt directly instead of searching for
/// the matching method on [`LoopDriver`].
///
/// # Example
///
/// ```rust,no_run
/// # use agentkit_loop::{LoopInterrupt, LoopStep, LoopDriver};
/// # async fn handle<S: agentkit_loop::ModelSession>(driver: &mut LoopDriver<S>) -> Result<(), agentkit_loop::LoopError> {
/// match driver.next().await? {
///     LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
///         println!("Needs approval: {}", pending.request.summary);
///         pending.approve(driver)?;
///     }
///     _ => {}
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingApproval {
    /// The underlying approval request details.
    pub request: ApprovalRequest,
}

impl std::ops::Deref for PendingApproval {
    type Target = ApprovalRequest;
    fn deref(&self) -> &ApprovalRequest {
        &self.request
    }
}

impl PendingApproval {
    /// Approve the pending tool call.
    pub fn approve<S: ModelSession>(self, driver: &mut LoopDriver<S>) -> Result<(), LoopError> {
        let call_id = self
            .request
            .call_id
            .ok_or_else(|| LoopError::InvalidState("pending approval is missing call id".into()))?;
        driver.resolve_approval_for(call_id, ApprovalDecision::Approve)
    }

    /// Deny the pending tool call.
    pub fn deny<S: ModelSession>(self, driver: &mut LoopDriver<S>) -> Result<(), LoopError> {
        let call_id = self
            .request
            .call_id
            .ok_or_else(|| LoopError::InvalidState("pending approval is missing call id".into()))?;
        driver.resolve_approval_for(call_id, ApprovalDecision::Deny { reason: None })
    }

    /// Deny the pending tool call with a reason.
    pub fn deny_with_reason<S: ModelSession>(
        self,
        driver: &mut LoopDriver<S>,
        reason: impl Into<String>,
    ) -> Result<(), LoopError> {
        let call_id = self
            .request
            .call_id
            .ok_or_else(|| LoopError::InvalidState("pending approval is missing call id".into()))?;
        driver.resolve_approval_for(
            call_id,
            ApprovalDecision::Deny {
                reason: Some(reason.into()),
            },
        )
    }

    /// Approve the pending tool call with a patched input.
    ///
    /// The model's original tool input is replaced with `input` before the
    /// tool executes. The transcript still records the call as the model
    /// emitted it; only the executor sees the patched payload. This mirrors
    /// the `PermissionResultAllow(updated_input=...)` pattern from the
    /// Anthropic Agent SDK and is intended for hosts that want to sanitise,
    /// restrict, or augment arguments before tool execution without forcing
    /// the model to re-issue the call.
    pub fn approve_with_patched_input<S: ModelSession>(
        self,
        driver: &mut LoopDriver<S>,
        input: serde_json::Value,
    ) -> Result<(), LoopError> {
        let call_id = self
            .request
            .call_id
            .ok_or_else(|| LoopError::InvalidState("pending approval is missing call id".into()))?;
        driver.resolve_approval_for_with_patched_input(call_id, input)
    }
}

/// Descriptor for a [`LoopInterrupt::AwaitingInput`] interrupt.
///
/// Returned when the driver has no pending input and needs the host to
/// supply items before advancing. This is the entry point for every user
/// turn that wasn't preloaded via [`AgentBuilder::input`]. Transcript items
/// loaded via [`AgentBuilder::transcript`] are passive, so when no input is
/// preloaded the first [`LoopDriver::next`] call surfaces `AwaitingInput`
/// and the host injects the first user message via [`InputRequest::submit`].
///
/// # Example
///
/// ```rust,no_run
/// # use agentkit_loop::{LoopInterrupt, LoopStep, LoopDriver};
/// # use agentkit_core::Item;
/// # async fn handle<S: agentkit_loop::ModelSession>(driver: &mut LoopDriver<S>, items: Vec<Item>) -> Result<(), agentkit_loop::LoopError> {
/// match driver.next().await? {
///     LoopStep::Interrupt(LoopInterrupt::AwaitingInput(pending)) => {
///         pending.submit(driver, items)?;
///     }
///     _ => {}
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRequest {
    /// The session that is waiting for input.
    pub session_id: SessionId,
    /// Human-readable explanation of why input is needed.
    pub reason: String,
}

impl InputRequest {
    /// Submit input items to the driver.
    pub fn submit<S: ModelSession>(
        self,
        driver: &mut LoopDriver<S>,
        items: Vec<Item>,
    ) -> Result<(), LoopError> {
        driver.submit_input(items)
    }
}

/// Outcome of a completed (or cancelled) turn.
///
/// Wrapped by [`LoopStep::Finished`] and also emitted as
/// [`AgentEvent::TurnFinished`] to observers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnResult {
    /// Identifier for the turn that produced this result.
    pub turn_id: agentkit_core::TurnId,
    /// Why the turn ended (completed, tool call, cancelled, etc.).
    pub finish_reason: FinishReason,
    /// Items produced during this turn (assistant text, tool results, etc.).
    pub items: Vec<Item>,
    /// Aggregated token usage, if reported by the model.
    pub usage: Option<Usage>,
    /// Additional metadata about the turn.
    pub metadata: MetadataMap,
}

/// An interrupt that pauses the agent loop until the host resolves it.
///
/// The loop returns an interrupt inside [`LoopStep::Interrupt`] whenever it
/// cannot proceed autonomously.  Each variant carries a handle with
/// resolution methods so callers can resolve the interrupt directly.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_loop::{LoopInterrupt, LoopStep};
/// # use agentkit_loop::LoopDriver;
///
/// # async fn handle<S: agentkit_loop::ModelSession>(driver: &mut LoopDriver<S>) -> Result<(), agentkit_loop::LoopError> {
/// match driver.next().await? {
///     LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
///         println!("Tool {} needs approval: {}", pending.request.request_kind, pending.request.summary);
///         pending.approve(driver)?;
///     }
///     LoopStep::Interrupt(LoopInterrupt::AwaitingInput(pending)) => {
///         println!("Waiting for input: {}", pending.reason);
///         // ... call pending.submit(driver, items)
///     }
///     LoopStep::Interrupt(LoopInterrupt::AfterToolResult(info)) => {
///         // Cooperative yield between tool rounds.  Optionally call
///         // driver.submit_input(...) to interject a user message, then
///         // call driver.next() to resume the turn.
///         let _ = info;
///     }
///     LoopStep::Finished(result) => {
///         println!("Turn finished: {:?}", result.finish_reason);
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoopInterrupt {
    /// A tool call requires explicit approval before it can execute.
    ApprovalRequest(PendingApproval),
    /// The driver has no pending input and needs the host to supply some.
    AwaitingInput(InputRequest),
    /// A tool round finished: all tool calls from the previous assistant
    /// message now have results in the transcript, and the driver is about to
    /// invoke the model again. The host may interject user messages via the
    /// [`ToolRoundInfo::submit`] handle before calling [`LoopDriver::next`]
    /// to resume.
    ///
    /// This is a non-blocking interrupt: callers that do not care about
    /// mid-turn interjection can treat it as a no-op (`_ => continue`) and
    /// the next `next()` call resumes the turn.
    AfterToolResult(ToolRoundInfo),
}

impl LoopInterrupt {
    /// Returns `true` if the interrupt must be explicitly resolved before
    /// the loop can make progress. Approvals are blocking;
    /// [`AwaitingInput`](LoopInterrupt::AwaitingInput) and
    /// [`AfterToolResult`](LoopInterrupt::AfterToolResult) are cooperative
    /// and can be ignored by calling [`LoopDriver::next`] again.
    pub fn is_blocking(&self) -> bool {
        matches!(self, LoopInterrupt::ApprovalRequest(_))
    }
}

/// Metadata describing a completed tool round, surfaced via
/// [`LoopInterrupt::AfterToolResult`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRoundInfo {
    /// The session that produced this tool round.
    pub session_id: SessionId,
    /// The turn that is about to continue into the next model call.
    pub turn_id: agentkit_core::TurnId,
    /// Transcript length at the yield point (for snapshots / UIs).
    pub transcript_len: usize,
}

impl ToolRoundInfo {
    /// Interject user input between tool rounds. Consumes the
    /// [`ToolRoundInfo`] handle so the same yield cannot accept input twice.
    pub fn submit<S: ModelSession>(
        self,
        driver: &mut LoopDriver<S>,
        items: Vec<Item>,
    ) -> Result<(), LoopError> {
        driver.submit_input(items)
    }
}

/// The result of advancing the agent loop by one step.
///
/// Returned by [`LoopDriver::next`].  The host should pattern-match on this
/// to decide whether to continue the loop or resolve an interrupt first.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_loop::LoopStep;
/// # use agentkit_loop::LoopDriver;
///
/// # async fn run<S: agentkit_loop::ModelSession>(driver: &mut LoopDriver<S>) -> Result<(), agentkit_loop::LoopError> {
/// loop {
///     match driver.next().await? {
///         LoopStep::Finished(result) => {
///             println!("Turn complete: {:?}", result.finish_reason);
///             break;
///         }
///         LoopStep::Interrupt(interrupt) => {
///             // Resolve the interrupt, then continue the loop.
///             # break;
///         }
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum LoopStep {
    /// The loop is paused and requires host action.
    Interrupt(LoopInterrupt),
    /// A turn has completed (or been cancelled).
    Finished(TurnResult),
}

/// A read-only snapshot of the loop driver's current state.
///
/// Obtained via [`LoopDriver::snapshot`].  Useful for persisting or
/// inspecting the conversation transcript without holding a mutable
/// reference to the driver.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LoopSnapshot {
    /// Session identifier.
    pub session_id: SessionId,
    /// The full transcript accumulated so far.
    pub transcript: Vec<Item>,
    /// Input items queued but not yet consumed by a turn.
    pub pending_input: Vec<Item>,
}

#[derive(Clone)]
struct PendingApprovalToolCall {
    request: ApprovalRequest,
    decision: Option<ApprovalDecision>,
    surfaced: bool,
    turn_id: agentkit_core::TurnId,
    task_id: TaskId,
    call: ToolCallPart,
    tool_request: ToolRequest,
    cancellation: Option<TurnCancellation>,
}

#[derive(Clone, Default)]
struct ActiveToolRound {
    turn_id: agentkit_core::TurnId,
    pending_calls: VecDeque<(ToolCallPart, ToolRequest)>,
    cancellation: Option<TurnCancellation>,
    background_pending: bool,
    foreground_progressed: bool,
}

/// A configured agent ready to start a session.
///
/// Build one with [`Agent::builder`], supplying at minimum a [`ModelAdapter`].
/// Optionally preload prior conversation state via
/// [`AgentBuilder::transcript`] and the next user turn via
/// [`AgentBuilder::input`]. Then call [`Agent::start`] with a
/// [`SessionConfig`] to obtain a [`LoopDriver`] that drives the agentic loop.
///
/// If no input is preloaded, the first call to [`LoopDriver::next`] yields
/// [`LoopInterrupt::AwaitingInput`] so the host can supply the first user
/// message via [`InputRequest::submit`]. If input was preloaded, the first
/// `next()` dispatches the model directly.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_core::{Item, ItemKind};
/// use agentkit_loop::{
///     Agent, PromptCacheRequest, PromptCacheRetention, SessionConfig,
/// };
/// use agentkit_tools_core::ToolRegistry;
///
/// # async fn example<M: agentkit_loop::ModelAdapter>(adapter: M) -> Result<(), agentkit_loop::LoopError> {
/// let agent = Agent::builder()
///     .model(adapter)
///     .add_tool_source(ToolRegistry::new())
///     .transcript(vec![Item::text(ItemKind::System, "You are a helpful assistant.")])
///     .input(vec![Item::text(ItemKind::User, "Hello!")])
///     .build()?;
///
/// let mut driver = agent
///     .start(SessionConfig::new("s1").with_cache(
///         PromptCacheRequest::automatic().with_retention(PromptCacheRetention::Short),
///     ))
///     .await?;
///
/// // First next() drives the model since input was preloaded.
/// let _ = driver.next().await?;
/// # Ok(())
/// # }
/// ```
pub struct Agent<M>
where
    M: ModelAdapter,
{
    model: M,
    tool_sources: Vec<Arc<dyn ToolSource>>,
    tool_executor: Option<Arc<dyn ToolExecutor>>,
    task_manager: Arc<dyn TaskManager>,
    permissions: Arc<dyn PermissionChecker>,
    resources: Arc<dyn ToolResources>,
    cancellation: Option<CancellationHandle>,
    mutators: Vec<Arc<dyn LoopMutator>>,
    observers: Vec<Arc<dyn LoopObserver>>,
    transcript_observers: Vec<Arc<dyn TranscriptObserver>>,
    transcript: Vec<Item>,
    input: Vec<Item>,
}

impl<M> Agent<M>
where
    M: ModelAdapter,
{
    /// Create a new [`AgentBuilder`] for configuring this agent.
    pub fn builder() -> AgentBuilder<M> {
        AgentBuilder::default()
    }

    /// Start a session, returning a [`LoopDriver`] preloaded with whatever
    /// transcript and input were configured on the builder. See
    /// [`AgentBuilder::transcript`] and [`AgentBuilder::input`] for what each
    /// one does and when to use them.
    ///
    /// This calls [`ModelAdapter::start_session`] and emits an
    /// [`AgentEvent::RunStarted`] event to all registered observers.
    ///
    /// `&self` so a single configured agent can mint multiple sessions over
    /// its lifetime — e.g. an outer agent that uses an inner sub-agent for
    /// transcript compaction.
    ///
    /// # Errors
    ///
    /// Returns [`LoopError`] if the model adapter fails to create a session.
    pub async fn start(&self, config: SessionConfig) -> Result<LoopDriver<M::Session>, LoopError> {
        let session_id = config.session_id.clone();
        let default_cache = config.cache.clone();
        let default_structured_output = config.structured_output.clone();
        let session = self.model.start_session(config).await?;
        let tool_executor = self
            .tool_executor
            .clone()
            .unwrap_or_else(|| Arc::new(BasicToolExecutor::new(self.tool_sources.clone())));
        let driver = LoopDriver {
            session_id: session_id.clone(),
            observed_session_id: Arc::new(session_id.clone()),
            provider_name: self.model.provider_name().map(str::to_owned),
            default_cache,
            default_structured_output,
            next_turn_cache: None,
            session: Some(session),
            tool_executor,
            task_manager: self.task_manager.clone(),
            permissions: self.permissions.clone(),
            resources: self.resources.clone(),
            cancellation: self.cancellation.clone(),
            mutators: self.mutators.clone(),
            observers: self.observers.clone(),
            transcript_observers: self.transcript_observers.clone(),
            transcript: self.transcript.clone(),
            pending_input: self.input.clone(),
            pending_approvals: BTreeMap::new(),
            pending_approval_order: VecDeque::new(),
            active_tool_round: None,
            pending_round_resume: None,
            next_turn_index: 1,
            detached_call_ids: HashSet::new(),
            tool_cancellations: HashMap::new(),
        };
        driver.emit(AgentEvent::RunStarted { session_id });
        Ok(driver)
    }
}

/// Builder for constructing an [`Agent`].
///
/// Obtained via [`Agent::builder`].  The only required field is
/// [`model`](AgentBuilder::model); all others have sensible defaults
/// (no tools, allow-all permissions, no compaction, no observers).
pub struct AgentBuilder<M>
where
    M: ModelAdapter,
{
    model: Option<M>,
    tool_sources: Vec<Arc<dyn ToolSource>>,
    tool_executor: Option<Arc<dyn ToolExecutor>>,
    task_manager: Option<Arc<dyn TaskManager>>,
    permissions: Arc<dyn PermissionChecker>,
    resources: Arc<dyn ToolResources>,
    cancellation: Option<CancellationHandle>,
    mutators: Vec<Arc<dyn LoopMutator>>,
    observers: Vec<Arc<dyn LoopObserver>>,
    transcript_observers: Vec<Arc<dyn TranscriptObserver>>,
    transcript: Vec<Item>,
    input: Vec<Item>,
}

impl<M> Default for AgentBuilder<M>
where
    M: ModelAdapter,
{
    fn default() -> Self {
        Self {
            model: None,
            tool_sources: Vec::new(),
            tool_executor: None,
            task_manager: None,
            permissions: Arc::new(AllowAllPermissions),
            resources: Arc::new(()),
            cancellation: None,
            mutators: Vec::new(),
            observers: Vec::new(),
            transcript_observers: Vec::new(),
            transcript: Vec::new(),
            input: Vec::new(),
        }
    }
}

impl<M> AgentBuilder<M>
where
    M: ModelAdapter,
{
    /// Set the model adapter (required).
    pub fn model(mut self, model: M) -> Self {
        self.model = Some(model);
        self
    }

    /// Adds a tool source to the agent. Call multiple times to compose
    /// federated sources — for example a frozen native [`ToolRegistry`]
    /// alongside an MCP manager's [`agentkit_tools_core::CatalogReader`]
    /// and a skill-watcher reader. Sources are walked in registration
    /// order; the default [`agentkit_tools_core::CollisionPolicy`] is
    /// `FirstWins`.
    ///
    /// Accepts any sized [`ToolSource`]; the agent owns it for the
    /// session. To share a dynamic source between the agent and the
    /// subsystem mutating it, mint a [`agentkit_tools_core::CatalogReader`]
    /// from a [`agentkit_tools_core::dynamic_catalog`] pair — the reader
    /// is sized and owned, hosts never see the underlying `Arc`.
    pub fn add_tool_source<S: ToolSource + 'static>(mut self, source: S) -> Self {
        self.tool_sources.push(Arc::new(source));
        self
    }

    /// Set a custom [`ToolExecutor`]. When provided, the agent uses it
    /// instead of building a [`BasicToolExecutor`] from the configured
    /// sources. Most hosts should use [`add_tool_source`](Self::add_tool_source)
    /// instead; this is for advanced cases (custom routing, instrumentation,
    /// test fakes).
    pub fn tool_executor(mut self, executor: impl ToolExecutor + 'static) -> Self {
        self.tool_executor = Some(Arc::new(executor));
        self
    }

    /// Set the task manager that schedules tool-call execution.
    ///
    /// Defaults to [`SimpleTaskManager`], which preserves the existing
    /// sequential request/response behavior.
    pub fn task_manager(mut self, manager: impl TaskManager + 'static) -> Self {
        self.task_manager = Some(Arc::new(manager));
        self
    }

    /// Set the permission checker that gates tool execution.
    ///
    /// Defaults to allowing all tool calls without prompting.
    pub fn permissions(mut self, permissions: impl PermissionChecker + 'static) -> Self {
        self.permissions = Arc::new(permissions);
        self
    }

    /// Set shared resources available to tool implementations.
    pub fn resources(mut self, resources: impl ToolResources + 'static) -> Self {
        self.resources = Arc::new(resources);
        self
    }

    /// Attach a [`CancellationHandle`] for cooperative cancellation of turns.
    pub fn cancellation(mut self, handle: CancellationHandle) -> Self {
        self.cancellation = Some(handle);
        self
    }

    /// Register a [`LoopMutator`] that runs at every [`MutationPoint`].
    ///
    /// Multiple mutators may be registered; they run in registration order
    /// and the dirty flag propagates across the pipeline. After every pass
    /// in which any mutator dirtied the transcript, the loop validates
    /// protocol invariants (tool_use/tool_result pairing); a violation is a
    /// hard [`LoopError::Mutator`] failure.
    pub fn mutator<L: LoopMutator + 'static>(mut self, mutator: L) -> Self {
        self.mutators.push(Arc::new(mutator));
        self
    }

    /// Register a [`LoopObserver`] that receives [`AgentEvent`]s.
    ///
    /// Multiple observers may be registered; they are called in order.
    pub fn observer<O: LoopObserver + 'static>(mut self, observer: O) -> Self {
        self.observers.push(Arc::new(observer));
        self
    }

    /// Register a [`TranscriptObserver`] that receives an [`Item`] every
    /// time one is appended to the transcript.
    ///
    /// Multiple observers may be registered; they are called in order.
    /// Use this when you need a loss-free view of the transcript (e.g.
    /// for persistence or replication) — [`LoopObserver`] alone is
    /// insufficient because it doesn't expose item boundaries for model
    /// output and historically did not surface tool results at all.
    pub fn transcript_observer<O: TranscriptObserver + 'static>(mut self, observer: O) -> Self {
        self.transcript_observers.push(Arc::new(observer));
        self
    }

    /// Preload the driver's transcript with prior conversation state
    /// (defaults to empty).
    ///
    /// Items pass straight into the driver's transcript without firing
    /// [`TranscriptObserver::on_transcript_event`] — the host is expected to
    /// already know about (and have persisted) anything it preloads. Use
    /// this for resumed sessions or to seed a system prompt.
    pub fn transcript(mut self, transcript: Vec<Item>) -> Self {
        self.transcript = transcript;
        self
    }

    /// Preload the driver's pending-input queue with the next user turn
    /// (defaults to empty).
    ///
    /// When non-empty, the first [`LoopDriver::next`] dispatches the model
    /// directly instead of yielding [`LoopInterrupt::AwaitingInput`]. Use
    /// this for one-shot calls and scripts where the first user turn is
    /// known up front. Items move to the transcript on turn dispatch the
    /// same way submitted input does, firing transcript observers.
    pub fn input(mut self, input: Vec<Item>) -> Self {
        self.input = input;
        self
    }

    /// Consume the builder and produce an [`Agent`].
    ///
    /// # Errors
    ///
    /// Returns [`LoopError::InvalidState`] if no model adapter was provided.
    pub fn build(self) -> Result<Agent<M>, LoopError> {
        let model = self
            .model
            .ok_or_else(|| LoopError::InvalidState("model adapter is required".into()))?;
        Ok(Agent {
            model,
            tool_sources: self.tool_sources,
            tool_executor: self.tool_executor,
            task_manager: self
                .task_manager
                .unwrap_or_else(|| Arc::new(SimpleTaskManager::new())),
            permissions: self.permissions,
            resources: self.resources,
            cancellation: self.cancellation,
            mutators: self.mutators,
            observers: self.observers,
            transcript_observers: self.transcript_observers,
            transcript: self.transcript,
            input: self.input,
        })
    }
}

/// The runtime driver that advances the agent loop step by step.
///
/// Obtained from [`Agent::start`] with the builder's preloaded transcript
/// and pending-input queue baked in.
/// The typical usage pattern is:
///
/// 1. Call [`next`](LoopDriver::next) to advance the loop.
/// 2. Handle the returned [`LoopStep`]:
///    - [`LoopStep::Finished`] -- the turn completed, inspect the result.
///    - [`LoopStep::Interrupt`] -- resolve the interrupt via the bound
///      [`Pending*`](LoopInterrupt) handle, then call `next` again.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_core::{Item, ItemKind};
/// use agentkit_loop::{LoopDriver, LoopStep};
///
/// # async fn drive<S: agentkit_loop::ModelSession>(driver: &mut LoopDriver<S>) -> Result<(), agentkit_loop::LoopError> {
/// let step = driver.next().await?;
/// match step {
///     LoopStep::Finished(result) => println!("Done: {:?}", result.finish_reason),
///     LoopStep::Interrupt(interrupt) => {
///         // Resolve via the pending handle, then call next() again.
///         println!("Interrupted: {interrupt:?}");
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub struct LoopDriver<S>
where
    S: ModelSession,
{
    session_id: SessionId,
    observed_session_id: Arc<SessionId>,
    provider_name: Option<String>,
    default_cache: Option<PromptCacheRequest>,
    default_structured_output: Option<StructuredOutputRequest>,
    next_turn_cache: Option<PromptCacheRequest>,
    session: Option<S>,
    tool_executor: Arc<dyn ToolExecutor>,
    task_manager: Arc<dyn TaskManager>,
    permissions: Arc<dyn PermissionChecker>,
    resources: Arc<dyn ToolResources>,
    cancellation: Option<CancellationHandle>,
    mutators: Vec<Arc<dyn LoopMutator>>,
    observers: Vec<Arc<dyn LoopObserver>>,
    transcript_observers: Vec<Arc<dyn TranscriptObserver>>,
    transcript: Vec<Item>,
    pending_input: Vec<Item>,
    pending_approvals: BTreeMap<ToolCallId, PendingApprovalToolCall>,
    pending_approval_order: VecDeque<ToolCallId>,
    active_tool_round: Option<ActiveToolRound>,
    pending_round_resume: Option<agentkit_core::TurnId>,
    next_turn_index: u64,
    /// Call ids whose original tool_use was already paired with a
    /// synthetic detach tool_result. When the real result eventually
    /// arrives via the task manager, we MUST NOT emit a second
    /// tool_result for the same id — the provider schema requires
    /// exactly one tool_result per tool_use. Instead we route the
    /// resolution into a [`ItemKind::Notification`] item that the model
    /// can react to on the next turn.
    detached_call_ids: HashSet<ToolCallId>,
    tool_cancellations: HashMap<ToolCallId, TurnCancellation>,
}

impl<S> LoopDriver<S>
where
    S: ModelSession,
{
    fn execute_tool_span(
        &self,
        request: &ToolRequest,
        turn_id: &agentkit_core::TurnId,
        launch_kind: &'static str,
    ) -> tracing::Span {
        tracing::info_span!(
            "agent.execute_tool",
            "otel.name" = %format!("execute_tool {}", request.tool_name),
            "gen_ai.operation.name" = "execute_tool",
            "gen_ai.tool.name" = %request.tool_name,
            "gen_ai.tool.call.id" = %request.call_id,
            "gen_ai.conversation.id" = %self.session_id,
            "error.type" = tracing::field::Empty,
            session.id = %self.session_id,
            turn.id = %turn_id,
            launch_kind = launch_kind,
        )
    }

    fn start_task_via_manager(
        &self,
        task_id: Option<TaskId>,
        tool_request: ToolRequest,
        kind: TaskLaunchKind,
        cancellation: Option<TurnCancellation>,
    ) -> impl std::future::Future<Output = Result<TaskStartOutcome, LoopError>> + Send + 'static
    {
        let task_manager = self.task_manager.clone();
        let tool_executor = self.tool_executor.clone();
        let permissions = self.permissions.clone();
        let resources = self.resources.clone();
        let session_id = self.session_id.clone();
        let turn_id = tool_request.turn_id.clone();
        let metadata = tool_request.metadata.clone();

        async move {
            task_manager
                .start_task(
                    TaskLaunchRequest {
                        task_id,
                        request: tool_request.clone(),
                        kind,
                    },
                    TaskStartContext {
                        executor: tool_executor.clone(),
                        tool_context: {
                            let execution_scope = ToolExecutionScope {
                                executor: tool_executor,
                                session_id: session_id.clone(),
                                turn_id: turn_id.clone(),
                                permissions: permissions.clone(),
                                resources: resources.clone(),
                                cancellation: cancellation.clone(),
                            };
                            OwnedToolContext {
                                session_id,
                                turn_id,
                                metadata,
                                permissions,
                                resources,
                                cancellation,
                                execution_scope: Some(execution_scope),
                                approved_request: None,
                            }
                        },
                    },
                )
                .await
                .map_err(|error| LoopError::Tool(ToolError::Internal(error.to_string())))
        }
    }

    fn register_tool_cancellation(
        &mut self,
        call_id: &ToolCallId,
        cancellation: Option<TurnCancellation>,
    ) {
        if let Some(cancellation) = cancellation {
            self.tool_cancellations
                .insert(call_id.clone(), cancellation);
        }
    }

    fn tool_cancellation_for(
        &mut self,
        call_id: &ToolCallId,
        fallback: Option<TurnCancellation>,
    ) -> Option<TurnCancellation> {
        self.tool_cancellations.get(call_id).cloned().or(fallback)
    }

    fn clear_tool_cancellation(&mut self, call_id: &ToolCallId) {
        self.tool_cancellations.remove(call_id);
    }

    fn has_pending_interrupts(&self) -> bool {
        !self.pending_approvals.is_empty()
    }

    fn emit_tool_catalog_events(&mut self, events: Vec<ToolCatalogEvent>) {
        for event in events {
            self.emit(AgentEvent::ToolCatalogChanged(event));
        }
    }

    fn enqueue_pending_approval(
        &mut self,
        turn_id: &agentkit_core::TurnId,
        task: TaskApproval,
        cancellation: Option<TurnCancellation>,
    ) {
        let call_id = task.tool_request.call_id.clone();
        let cancellation = self.tool_cancellation_for(&call_id, cancellation);
        let call = ToolCallPart {
            id: call_id.clone(),
            name: task.tool_request.tool_name.to_string(),
            input: task.tool_request.input.clone(),
            metadata: task.tool_request.metadata.clone(),
        };
        let mut request = task.approval;
        request.call_id = Some(call_id.clone());
        let pending = PendingApprovalToolCall {
            request: request.clone(),
            decision: None,
            surfaced: false,
            turn_id: turn_id.clone(),
            task_id: task.task_id,
            call,
            tool_request: task.tool_request,
            cancellation,
        };
        self.pending_approvals.insert(call_id.clone(), pending);
        if !self.pending_approval_order.iter().any(|id| id == &call_id) {
            self.pending_approval_order.push_back(call_id);
        }
        self.emit(AgentEvent::ApprovalRequired(request));
    }

    fn take_next_unsurfaced_approval_interrupt(&mut self) -> Option<LoopStep> {
        for call_id in self.pending_approval_order.clone() {
            let Some(pending) = self.pending_approvals.get_mut(&call_id) else {
                continue;
            };
            if pending.decision.is_none() && !pending.surfaced {
                pending.surfaced = true;
                return Some(LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(
                    PendingApproval {
                        request: pending.request.clone(),
                    },
                )));
            }
        }
        None
    }

    fn next_unresolved_approval_interrupt(&self) -> Option<LoopStep> {
        self.pending_approval_order.iter().find_map(|call_id| {
            self.pending_approvals.get(call_id).and_then(|pending| {
                pending.decision.is_none().then(|| {
                    LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(PendingApproval {
                        request: pending.request.clone(),
                    }))
                })
            })
        })
    }

    fn take_next_resolved_approval(&mut self) -> Option<PendingApprovalToolCall> {
        let call_id = self.pending_approval_order.iter().find_map(|call_id| {
            self.pending_approvals
                .get(call_id)
                .and_then(|pending| pending.decision.as_ref().map(|_| call_id.clone()))
        })?;
        self.pending_approval_order.retain(|id| id != &call_id);
        self.pending_approvals.remove(&call_id)
    }

    fn queue_resolution_interrupt(
        &mut self,
        turn_id: &agentkit_core::TurnId,
        resolution: TaskResolution,
        cancellation: Option<TurnCancellation>,
    ) -> Option<LoopStep> {
        match resolution {
            TaskResolution::Item(item) => {
                self.append_tool_result_item(item);
                None
            }
            TaskResolution::Approval(task) => {
                self.enqueue_pending_approval(turn_id, task, cancellation);
                self.take_next_unsurfaced_approval_interrupt()
            }
        }
    }

    async fn drain_pending_loop_updates(&mut self) -> Result<(bool, Option<LoopStep>), LoopError> {
        let PendingLoopUpdates { mut resolutions } = self
            .task_manager
            .take_pending_loop_updates()
            .await
            .map_err(|error| LoopError::Tool(ToolError::Internal(error.to_string())))?;
        let mut saw_items = false;
        while let Some(resolution) = resolutions.pop_front() {
            match resolution {
                TaskResolution::Item(item) => {
                    self.append_tool_result_item(item);
                    saw_items = true;
                }
                TaskResolution::Approval(task) => {
                    self.enqueue_pending_approval(&task.tool_request.turn_id.clone(), task, None);
                }
            }
        }
        if let Some(step) = self.finish_cancelled_pending_approval().await? {
            return Ok((saw_items, Some(step)));
        }
        Ok((saw_items, self.take_next_unsurfaced_approval_interrupt()))
    }

    async fn finish_cancelled_pending_approval(&mut self) -> Result<Option<LoopStep>, LoopError> {
        if self.pending_approvals.is_empty() {
            return Ok(None);
        }
        if !self.pending_approvals.values().any(|pending| {
            pending
                .cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled)
        }) {
            return Ok(None);
        }
        self.cancel_pending_approvals().await
    }

    async fn run_mutators(
        &mut self,
        point: MutationPoint,
        turn_id: Option<&agentkit_core::TurnId>,
        cancellation: Option<TurnCancellation>,
    ) -> Result<(), LoopError> {
        if self.mutators.is_empty() {
            return Ok(());
        }
        if cancellation
            .as_ref()
            .is_some_and(TurnCancellation::is_cancelled)
        {
            return Err(LoopError::Cancelled);
        }
        let mutators = self.mutators.clone();
        let session_id = self.session_id.clone();
        let observed_session_id = Arc::clone(&self.observed_session_id);
        let observers = self.observers.clone();
        let emitter = DriverEmitter {
            session_id: &observed_session_id,
            observers: &observers,
        };
        let mut cursor = TranscriptCursor {
            items: &mut self.transcript,
            dirty: false,
        };
        for mutator in &mutators {
            if cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled)
            {
                return Err(LoopError::Cancelled);
            }
            let ctx = LoopCtx {
                session_id: &session_id,
                turn_id,
                point,
                cancellation: cancellation.clone(),
                emitter: &emitter,
            };
            mutator.mutate(&mut cursor, ctx).await?;
        }
        if cursor.dirty {
            validate_transcript_invariants(cursor.items)?;
        }
        Ok(())
    }

    async fn continue_active_tool_round(&mut self) -> Result<Option<LoopStep>, LoopError> {
        let Some(_) = self.active_tool_round.as_ref() else {
            return Ok(None);
        };
        loop {
            let turn_id = self
                .active_tool_round
                .as_ref()
                .map(|active| active.turn_id.clone())
                .ok_or_else(|| LoopError::InvalidState("missing active tool round".into()))?;
            let cancellation = self
                .active_tool_round
                .as_ref()
                .and_then(|active| active.cancellation.clone());

            if cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled)
            {
                self.task_manager
                    .on_turn_interrupted(&turn_id)
                    .await
                    .map_err(|error| LoopError::Tool(ToolError::Internal(error.to_string())))?;
                self.active_tool_round = None;
                return self.finish_cancelled(turn_id, Vec::new()).map(Some);
            }

            let next_call = self
                .active_tool_round
                .as_mut()
                .and_then(|active| active.pending_calls.pop_front());
            if let Some((call, tool_request)) = next_call {
                use tracing::Instrument;
                self.register_tool_cancellation(&call.id, cancellation.clone());
                let dispatch_span = self.execute_tool_span(&tool_request, &turn_id, "plain");
                match self
                    .start_task_via_manager(
                        None,
                        tool_request.clone(),
                        TaskLaunchKind::Plain,
                        cancellation.clone(),
                    )
                    .instrument(dispatch_span.clone())
                    .await?
                {
                    TaskStartOutcome::Ready(resolution) => {
                        let resolution = *resolution;
                        match resolution {
                            TaskResolution::Item(item) => {
                                if !tool_result_not_started(&item) {
                                    self.emit(AgentEvent::ToolExecutionStarted(call.clone()));
                                }
                                if tool_result_is_error(&item) {
                                    dispatch_span.record("error.type", "tool_error");
                                }
                                if let Some(active) = self.active_tool_round.as_mut() {
                                    active.foreground_progressed = true;
                                }
                                self.append_tool_result_item(item);
                            }
                            TaskResolution::Approval(task) => {
                                self.enqueue_pending_approval(&turn_id, task, cancellation.clone());
                            }
                        }
                        continue;
                    }
                    TaskStartOutcome::Pending { kind, .. } => {
                        self.emit(AgentEvent::ToolExecutionStarted(call.clone()));
                        if kind == agentkit_task_manager::TaskKind::Background
                            && let Some(active) = self.active_tool_round.as_mut()
                        {
                            active.background_pending = true;
                        }
                        continue;
                    }
                }
            }

            match self
                .task_manager
                .wait_for_turn(&turn_id, cancellation.clone())
                .await
                .map_err(|error| LoopError::Tool(ToolError::Internal(error.to_string())))?
            {
                Some(TurnTaskUpdate::Resolution(resolution)) => {
                    let resolution = *resolution;
                    match resolution {
                        TaskResolution::Item(item) => {
                            if let Some(active) = self.active_tool_round.as_mut() {
                                active.foreground_progressed = true;
                            }
                            self.append_tool_result_item(item);
                        }
                        TaskResolution::Approval(task) => {
                            self.enqueue_pending_approval(&turn_id, task, cancellation.clone());
                        }
                    }
                }
                Some(TurnTaskUpdate::Detached(snapshot)) => {
                    // The task was promoted to background. Push a synthetic
                    // tool result so the model knows the call is still
                    // running and can continue its turn. Track the
                    // call_id so when the real result arrives later via
                    // the task manager, we route it to a Notification
                    // item instead of emitting a second tool_result for
                    // the same call_id (which would violate the
                    // provider schema — exactly one tool_result per
                    // tool_use).
                    // Order matters: append the synthetic placeholder FIRST as
                    // a real Tool/ToolResult so the tool_use slot is filled
                    // (provider schemas require exactly one tool_result per
                    // tool_use). Only AFTER appending do we record the
                    // call_id in `detached_call_ids` — so the *next* item
                    // for this call_id (the real completion arriving later
                    // via the task manager) is the one converted to a
                    // Notification by `maybe_convert_detached`.
                    let detached_call_id = snapshot.call_id.clone();
                    let detached_result = ToolResultPart {
                        call_id: detached_call_id.clone(),
                        output: ToolOutput::Text(format!(
                            "Tool {} is now running in the background. \
                             The result will be delivered when it completes.",
                            snapshot.tool_name,
                        )),
                        is_error: false,
                        metadata: MetadataMap::new(),
                    };
                    self.emit(AgentEvent::ToolExecutionProgress(detached_result.clone()));
                    self.append_item(Item {
                        id: None,
                        kind: ItemKind::Tool,
                        parts: vec![Part::ToolResult(detached_result)],
                        metadata: MetadataMap::new(),
                        usage: None,
                        finish_reason: None,
                        created_at: None,
                    });
                    self.detached_call_ids.insert(detached_call_id);
                    if let Some(active) = self.active_tool_round.as_mut() {
                        active.background_pending = true;
                        active.foreground_progressed = true;
                    }
                }
                None => {
                    if cancellation
                        .as_ref()
                        .is_some_and(TurnCancellation::is_cancelled)
                    {
                        self.task_manager
                            .on_turn_interrupted(&turn_id)
                            .await
                            .map_err(|error| {
                                LoopError::Tool(ToolError::Internal(error.to_string()))
                            })?;
                        self.active_tool_round = None;
                        return self.finish_cancelled(turn_id, Vec::new()).map(Some);
                    }
                    let active = self.active_tool_round.take().ok_or_else(|| {
                        LoopError::InvalidState("missing active tool round".into())
                    })?;
                    if let Some(step) = self.take_next_unsurfaced_approval_interrupt() {
                        return Ok(Some(step));
                    }
                    if let Some(step) = self.next_unresolved_approval_interrupt() {
                        return Ok(Some(step));
                    }
                    if active.background_pending && !active.foreground_progressed {
                        return Ok(None);
                    }
                    // Yield control back to the host between tool rounds.
                    // All tool calls in this round have results in the
                    // transcript; the transcript is provider-valid.  The
                    // host may submit_input before calling next() to
                    // resume, which will re-enter drive_turn via
                    // pending_round_resume.
                    let info = ToolRoundInfo {
                        session_id: self.session_id.clone(),
                        turn_id: turn_id.clone(),
                        transcript_len: self.transcript.len(),
                    };
                    self.pending_round_resume = Some(turn_id);
                    return Ok(Some(LoopStep::Interrupt(LoopInterrupt::AfterToolResult(
                        info,
                    ))));
                }
            }
        }
    }

    #[tracing::instrument(
        name = "agent.turn",
        skip_all,
        fields(
            otel.name = "invoke_agent",
            gen_ai.operation.name = "invoke_agent",
            gen_ai.conversation.id = %self.session_id,
            gen_ai.provider.name = tracing::field::Empty,
            gen_ai.usage.input_tokens = tracing::field::Empty,
            gen_ai.usage.output_tokens = tracing::field::Empty,
            gen_ai.usage.cost = tracing::field::Empty,
            session.id = %self.session_id,
            turn.id = %turn_id,
            transcript.len = self.transcript.len(),
            saw_tool_call = tracing::field::Empty,
            finish_reason = tracing::field::Empty,
        ),
    )]
    async fn drive_turn(
        &mut self,
        turn_id: agentkit_core::TurnId,
        emit_started: bool,
        mutation_point: MutationPoint,
    ) -> Result<LoopStep, LoopError> {
        if let Some(provider) = &self.provider_name {
            tracing::Span::current().record("gen_ai.provider.name", provider.as_str());
        }
        let cancellation = self
            .cancellation
            .as_ref()
            .map(CancellationHandle::checkpoint);
        match self
            .run_mutators(mutation_point, Some(&turn_id), cancellation.clone())
            .await
        {
            Ok(()) => {}
            Err(LoopError::Cancelled) => {
                return self.finish_cancelled(turn_id, interrupted_assistant_items());
            }
            Err(error) => return Err(error),
        }

        // A mutator may have removed the freshly-submitted input (e.g. a
        // compaction pass that summarised the latest user turn away), leaving
        // the transcript ending in an assistant message or empty — nothing new
        // for the model to respond to. Finish the turn rather than dispatch an
        // assistant-prefill request, which most providers reject.
        if !transcript_has_pending_input(&self.transcript) {
            let turn_result = TurnResult {
                turn_id,
                finish_reason: FinishReason::Completed,
                items: Vec::new(),
                usage: None,
                metadata: MetadataMap::new(),
            };
            self.emit(AgentEvent::TurnFinished(turn_result.clone()));
            return Ok(LoopStep::Finished(turn_result));
        }

        if emit_started {
            self.emit(AgentEvent::TurnStarted {
                session_id: self.session_id.clone(),
                turn_id: turn_id.clone(),
            });
        }
        if cancellation
            .as_ref()
            .is_some_and(TurnCancellation::is_cancelled)
        {
            return self.finish_cancelled(turn_id, interrupted_assistant_items());
        }

        let catalog_events = self.tool_executor.drain_catalog_events();
        self.emit_tool_catalog_events(catalog_events);

        let request = TurnRequest {
            session_id: self.session_id.clone(),
            turn_id: turn_id.clone(),
            transcript: self.transcript.clone(),
            available_tools: self.tool_executor.specs(),
            cache: self
                .next_turn_cache
                .take()
                .or_else(|| self.default_cache.clone()),
            structured_output: self.default_structured_output.clone(),
            metadata: MetadataMap::new(),
        };

        let session = self
            .session
            .as_mut()
            .ok_or_else(|| LoopError::InvalidState("model session is not available".into()))?;

        // Inference span per the OTel GenAI semantic conventions. It wraps the
        // model request and the full event drain rather than just `begin_turn`,
        // so attributes that streaming adapters only learn mid-stream (usage,
        // stop reason, response identity) still land before the span closes.
        // `otel.name` carries the dynamic `chat {model}` span name for
        // OpenTelemetry bridges since tracing span names are static.
        let chat_span = tracing::info_span!(
            "chat",
            "otel.name" = tracing::field::Empty,
            "otel.kind" = "client",
            "gen_ai.operation.name" = "chat",
            "gen_ai.provider.name" = tracing::field::Empty,
            "gen_ai.conversation.id" = %self.session_id,
            "gen_ai.request.model" = tracing::field::Empty,
            "gen_ai.response.model" = tracing::field::Empty,
            "gen_ai.response.id" = tracing::field::Empty,
            "gen_ai.response.finish_reasons" = tracing::field::Empty,
            "gen_ai.usage.input_tokens" = tracing::field::Empty,
            "gen_ai.usage.output_tokens" = tracing::field::Empty,
            "gen_ai.usage.cost" = tracing::field::Empty,
        );
        if let Some(provider) = &self.provider_name {
            chat_span.record("gen_ai.provider.name", provider.as_str());
        }
        match session.model_name() {
            Some(model) => {
                chat_span.record("gen_ai.request.model", model);
                chat_span.record("otel.name", format!("chat {model}").as_str());
            }
            None => {
                chat_span.record("otel.name", "chat");
            }
        }

        use tracing::Instrument;
        let mut turn = match session
            .begin_turn(request, cancellation.clone())
            .instrument(chat_span.clone())
            .await
        {
            Ok(turn) => turn,
            Err(LoopError::Cancelled) => {
                self.task_manager
                    .on_turn_interrupted(&turn_id)
                    .await
                    .map_err(|error| LoopError::Tool(ToolError::Internal(error.to_string())))?;
                return self.finish_cancelled(turn_id, interrupted_assistant_items());
            }
            Err(error) => return Err(error),
        };
        let mut saw_tool_call = false;
        let mut finished_result = None;

        while let Some(event) = match turn
            .next_event(cancellation.clone())
            .instrument(chat_span.clone())
            .await
        {
            Ok(event) => event,
            Err(LoopError::Cancelled) => {
                self.task_manager
                    .on_turn_interrupted(&turn_id)
                    .await
                    .map_err(|error| LoopError::Tool(ToolError::Internal(error.to_string())))?;
                return self.finish_cancelled(turn_id, interrupted_assistant_items());
            }
            Err(error) => return Err(error),
        } {
            if cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled)
            {
                self.task_manager
                    .on_turn_interrupted(&turn_id)
                    .await
                    .map_err(|error| LoopError::Tool(ToolError::Internal(error.to_string())))?;
                return self.finish_cancelled(turn_id, interrupted_assistant_items());
            }
            match event {
                ModelTurnEvent::Delta(delta) => self.emit(AgentEvent::ContentDelta(delta)),
                ModelTurnEvent::Usage(usage) => {
                    if let Some(tokens) = &usage.tokens {
                        chat_span.record("gen_ai.usage.input_tokens", tokens.input_tokens);
                        chat_span.record("gen_ai.usage.output_tokens", tokens.output_tokens);
                    }
                    if let Some(cost) = &usage.cost {
                        chat_span.record("gen_ai.usage.cost", cost.amount);
                    }
                    self.emit(AgentEvent::UsageUpdated(usage));
                }
                ModelTurnEvent::ToolCall(call) => {
                    saw_tool_call = true;
                    self.emit(AgentEvent::ToolCallRequested(call.clone()));
                }
                ModelTurnEvent::Finished(result) => {
                    finished_result = Some(result);
                    break;
                }
            }
        }

        let mut result = finished_result.ok_or_else(|| {
            LoopError::Provider("model turn ended without a Finished event".into())
        })?;
        if let Some(model) = &result.model {
            chat_span.record("gen_ai.response.model", model.as_str());
        }
        if let Some(id) = &result.response_id {
            chat_span.record("gen_ai.response.id", id.as_str());
        }
        if let Some(tokens) = result
            .usage
            .as_ref()
            .and_then(|usage| usage.tokens.as_ref())
        {
            chat_span.record("gen_ai.usage.input_tokens", tokens.input_tokens);
            chat_span.record("gen_ai.usage.output_tokens", tokens.output_tokens);
        }
        if let Some(cost) = result.usage.as_ref().and_then(|usage| usage.cost.as_ref()) {
            chat_span.record("gen_ai.usage.cost", cost.amount);
        }
        chat_span.record(
            "gen_ai.response.finish_reasons",
            tracing::field::debug(&result.finish_reason),
        );
        drop(chat_span);
        tracing::Span::current().record("saw_tool_call", saw_tool_call);
        tracing::Span::current().record(
            "finish_reason",
            tracing::field::debug(&result.finish_reason),
        );
        if let Some(tokens) = result
            .usage
            .as_ref()
            .and_then(|usage| usage.tokens.as_ref())
        {
            tracing::Span::current().record("gen_ai.usage.input_tokens", tokens.input_tokens);
            tracing::Span::current().record("gen_ai.usage.output_tokens", tokens.output_tokens);
        }
        if let Some(cost) = result.usage.as_ref().and_then(|usage| usage.cost.as_ref()) {
            tracing::Span::current().record("gen_ai.usage.cost", cost.amount);
        }
        let now = Timestamp::now();
        let usage = result.usage.clone();
        let finish_reason = result.finish_reason.clone();
        let output_items: Vec<Item> = result
            .output_items
            .drain(..)
            .map(|mut item| {
                if matches!(item.kind, ItemKind::Assistant) {
                    if item.usage.is_none() {
                        item.usage = usage.clone();
                    }
                    if item.finish_reason.is_none() {
                        item.finish_reason = Some(finish_reason.clone());
                    }
                }
                if item.created_at.is_none() {
                    item.created_at = Some(now);
                }
                item
            })
            .collect();
        self.extend_transcript(output_items.clone());

        if saw_tool_call {
            let pending_calls = extract_tool_calls(&output_items)
                .into_iter()
                .map(|call| {
                    let tool_request = ToolRequest {
                        call_id: call.id.clone(),
                        tool_name: agentkit_tools_core::ToolName::new(call.name.clone()),
                        input: call.input.clone(),
                        session_id: self.session_id.clone(),
                        turn_id: turn_id.clone(),
                        metadata: call.metadata.clone(),
                    };
                    (call, tool_request)
                })
                .collect();
            self.active_tool_round = Some(ActiveToolRound {
                turn_id: turn_id.clone(),
                pending_calls,
                cancellation: cancellation.clone(),
                background_pending: false,
                foreground_progressed: false,
            });
            if let Some(step) = self.continue_active_tool_round().await? {
                return Ok(step);
            }
            return Ok(LoopStep::Interrupt(LoopInterrupt::AwaitingInput(
                InputRequest {
                    session_id: self.session_id.clone(),
                    reason: "driver is waiting for input".into(),
                },
            )));
        }

        let turn_result = TurnResult {
            turn_id,
            finish_reason: result.finish_reason,
            items: output_items,
            usage: result.usage,
            metadata: result.metadata,
        };
        self.emit(AgentEvent::TurnFinished(turn_result.clone()));
        Ok(LoopStep::Finished(turn_result))
    }

    async fn resume_after_approval(
        &mut self,
        pending: PendingApprovalToolCall,
    ) -> Result<LoopStep, LoopError> {
        let decision = pending
            .decision
            .clone()
            .ok_or_else(|| LoopError::InvalidState("pending approval has no decision".into()))?;

        match decision {
            ApprovalDecision::Approve => {
                use tracing::Instrument;
                self.emit(AgentEvent::ToolExecutionStarted(pending.call.clone()));
                let dispatch_span =
                    self.execute_tool_span(&pending.tool_request, &pending.turn_id, "approved");
                let cancellation = self
                    .cancellation
                    .as_ref()
                    .map(CancellationHandle::checkpoint);
                self.register_tool_cancellation(&pending.call.id, cancellation.clone());
                match self
                    .start_task_via_manager(
                        Some(pending.task_id.clone()),
                        pending.tool_request.clone(),
                        TaskLaunchKind::Approved(pending.request.clone()),
                        cancellation.clone(),
                    )
                    .instrument(dispatch_span.clone())
                    .await?
                {
                    TaskStartOutcome::Ready(resolution) => {
                        let resolution = *resolution;
                        if let TaskResolution::Item(item) = &resolution
                            && tool_result_is_error(item)
                        {
                            dispatch_span.record("error.type", "tool_error");
                        }
                        if let Some(step) = self.queue_resolution_interrupt(
                            &pending.turn_id,
                            resolution,
                            cancellation,
                        ) {
                            return Ok(step);
                        }
                    }
                    TaskStartOutcome::Pending { .. } => {}
                }
            }
            ApprovalDecision::Deny { reason } => {
                self.append_tool_result_item(Item {
                    id: None,
                    kind: ItemKind::Tool,
                    parts: vec![Part::ToolResult(ToolResultPart {
                        call_id: pending.call.id.clone(),
                        output: ToolOutput::Text(
                            reason.unwrap_or_else(|| "approval denied".into()),
                        ),
                        is_error: true,
                        metadata: pending.call.metadata.clone(),
                    })],
                    metadata: MetadataMap::new(),
                    usage: None,
                    finish_reason: None,
                    created_at: None,
                });
            }
        }

        if let Some(step) = self.continue_active_tool_round().await? {
            Ok(step)
        } else if let Some(step) = self.take_next_unsurfaced_approval_interrupt() {
            Ok(step)
        } else if let Some(step) = self.next_unresolved_approval_interrupt() {
            Ok(step)
        } else {
            self.drive_turn(pending.turn_id, false, MutationPoint::AfterToolResult)
                .await
        }
    }

    fn finish_cancelled(
        &mut self,
        turn_id: agentkit_core::TurnId,
        items: Vec<Item>,
    ) -> Result<LoopStep, LoopError> {
        self.extend_transcript(items.clone());
        let turn_result = TurnResult {
            turn_id,
            finish_reason: FinishReason::Cancelled,
            items,
            usage: None,
            metadata: interrupted_metadata("turn"),
        };
        self.emit(AgentEvent::TurnFinished(turn_result.clone()));
        Ok(LoopStep::Finished(turn_result))
    }

    /// Internal entry point for buffering user input. Reachable only via
    /// [`InputRequest::submit`] (resolves an `AwaitingInput` interrupt,
    /// including the very first one after [`Agent::start`]) and
    /// [`ToolRoundInfo::submit`] (interjects between tool rounds). Prior
    /// transcript items — the passive starting state of a session — are
    /// preloaded via [`AgentBuilder::transcript`]; an opening user turn for
    /// one-shot calls is preloaded via [`AgentBuilder::input`]. New input
    /// after start-up always flows through one of the typed `submit`
    /// handles.
    pub fn submit_input(&mut self, input: Vec<Item>) -> Result<(), LoopError> {
        if self.has_pending_interrupts() {
            return Err(LoopError::InvalidState(
                "cannot submit input while an interrupt is pending".into(),
            ));
        }
        self.emit(AgentEvent::InputAccepted {
            session_id: self.session_id.clone(),
            items: input.clone(),
        });
        self.pending_input.extend(input);
        Ok(())
    }

    /// Override the prompt cache request for the next model turn.
    ///
    /// The override is consumed the next time the driver starts a model turn.
    /// Session-level defaults still apply to later turns.
    pub fn set_next_turn_cache(&mut self, cache: PromptCacheRequest) -> Result<(), LoopError> {
        if self.has_pending_interrupts() {
            return Err(LoopError::InvalidState(
                "cannot update next-turn cache while an interrupt is pending".into(),
            ));
        }
        self.next_turn_cache = Some(cache);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn submit_input_with_cache(
        &mut self,
        input: Vec<Item>,
        cache: PromptCacheRequest,
    ) -> Result<(), LoopError> {
        self.set_next_turn_cache(cache)?;
        self.submit_input(input)
    }

    /// Resolve a pending [`LoopInterrupt::ApprovalRequest`].
    ///
    /// After calling this, invoke [`next`](LoopDriver::next) to continue the
    /// loop.  If the decision is [`ApprovalDecision::Approve`] the tool call
    /// executes; if denied, an error result is fed back to the model.
    ///
    /// # Errors
    ///
    /// Returns [`LoopError::InvalidState`] if no approval is pending.
    pub fn resolve_approval_for(
        &mut self,
        call_id: ToolCallId,
        decision: ApprovalDecision,
    ) -> Result<(), LoopError> {
        let Some(pending) = self.pending_approvals.get_mut(&call_id) else {
            return Err(LoopError::InvalidState(format!(
                "no approval request is pending for call {}",
                call_id.0
            )));
        };
        pending.decision = Some(decision.clone());
        self.emit(AgentEvent::ApprovalResolved {
            approved: matches!(decision, ApprovalDecision::Approve),
        });
        Ok(())
    }

    /// Resolve a pending [`LoopInterrupt::ApprovalRequest`] with a patched
    /// input that replaces the model's original tool arguments.
    ///
    /// Equivalent to calling [`resolve_approval_for`] with
    /// [`ApprovalDecision::Approve`] except the tool sees `input` instead of
    /// what the model emitted. The transcript still records the model's
    /// original call.
    ///
    /// # Errors
    ///
    /// Returns [`LoopError::InvalidState`] if no approval is pending for
    /// `call_id`.
    pub fn resolve_approval_for_with_patched_input(
        &mut self,
        call_id: ToolCallId,
        input: serde_json::Value,
    ) -> Result<(), LoopError> {
        let Some(pending) = self.pending_approvals.get_mut(&call_id) else {
            return Err(LoopError::InvalidState(format!(
                "no approval request is pending for call {}",
                call_id.0
            )));
        };
        pending.tool_request.input = input;
        self.resolve_approval_for(call_id, ApprovalDecision::Approve)
    }

    /// Resolve a pending [`LoopInterrupt::ApprovalRequest`] when exactly one
    /// approval is outstanding.
    pub fn resolve_approval(&mut self, decision: ApprovalDecision) -> Result<(), LoopError> {
        let mut unresolved = self
            .pending_approval_order
            .iter()
            .filter(|call_id| {
                self.pending_approvals
                    .get(*call_id)
                    .is_some_and(|pending| pending.decision.is_none())
            })
            .cloned();
        let Some(call_id) = unresolved.next() else {
            return Err(LoopError::InvalidState(
                "no approval request is pending".into(),
            ));
        };
        if unresolved.next().is_some() {
            return Err(LoopError::InvalidState(
                "multiple approvals are pending; use resolve_approval_for".into(),
            ));
        }
        self.resolve_approval_for(call_id, decision)
    }

    /// Cancel a pending approval interrupt for a specific tool call.
    ///
    /// This clears the blocking approval and appends an error tool result so
    /// the transcript remains provider-valid if the host continues the turn.
    pub fn cancel_pending_approval_for(&mut self, call_id: ToolCallId) -> Result<(), LoopError> {
        let Some(pending) = self.drain_pending_approval_for(&call_id) else {
            return Err(LoopError::InvalidState(format!(
                "no approval request is pending for call {}",
                call_id.0
            )));
        };
        self.reject_drained_approvals(vec![pending]);
        Ok(())
    }

    /// Cancel every pending approval interrupt.
    ///
    /// This is useful when the host cancels the containing turn rather than an
    /// individual approval prompt. Each pending approval is resolved as denied
    /// and receives an error tool result so the transcript remains valid.
    pub async fn cancel_pending_approvals(&mut self) -> Result<Option<LoopStep>, LoopError> {
        if self.pending_approvals.is_empty() {
            return Ok(None);
        }
        let Some(turn_id) = self
            .pending_approval_order
            .iter()
            .find_map(|call_id| self.pending_approvals.get(call_id))
            .map(|pending| pending.turn_id.clone())
        else {
            return Ok(None);
        };
        let pending = self.drain_pending_approval_items();
        self.active_tool_round = None;
        self.task_manager
            .on_turn_interrupted(&turn_id)
            .await
            .map_err(|error| LoopError::Tool(ToolError::Internal(error.to_string())))?;
        self.reject_drained_approvals(pending);
        self.finish_cancelled(turn_id, Vec::new()).map(Some)
    }

    /// Take a read-only snapshot of the driver's current transcript and input queue.
    pub fn snapshot(&self) -> LoopSnapshot {
        LoopSnapshot {
            session_id: self.session_id.clone(),
            transcript: self.transcript.clone(),
            pending_input: self.pending_input.clone(),
        }
    }

    /// Advance the loop by one step.
    ///
    /// This is the main method for driving the agent.  It processes pending
    /// interrupt resolutions, consumes queued input, starts a model turn,
    /// executes tool calls, and returns once the turn finishes or an
    /// interrupt occurs.
    ///
    /// If no input is queued and no interrupt is pending, returns
    /// [`LoopStep::Interrupt(LoopInterrupt::AwaitingInput(..))`](LoopInterrupt::AwaitingInput).
    /// This is the steady state after [`Agent::start`] when no input was
    /// preloaded via [`AgentBuilder::input`]: the prior transcript loaded
    /// via [`AgentBuilder::transcript`] is passive, so the first call
    /// surfaces `AwaitingInput` and waits for the host to supply input via
    /// [`InputRequest::submit`] before any model turn is dispatched. If
    /// input was preloaded, the first call dispatches the model directly.
    ///
    /// # Errors
    ///
    /// Returns [`LoopError::InvalidState`] if called while an unresolved
    /// interrupt is pending, or propagates provider / tool / compaction errors.
    pub async fn next(&mut self) -> Result<LoopStep, LoopError> {
        if let Some(pending) = self.take_next_resolved_approval() {
            return self.resume_after_approval(pending).await;
        }

        if let Some(step) = self.finish_cancelled_pending_approval().await? {
            return Ok(step);
        }

        if let Some(step) = self.take_next_unsurfaced_approval_interrupt() {
            return Ok(step);
        }

        if let Some(step) = self.next_unresolved_approval_interrupt() {
            return Ok(step);
        }

        if let Some(step) = self.continue_active_tool_round().await? {
            return Ok(step);
        }

        let (had_loop_updates, loop_step) = self.drain_pending_loop_updates().await?;
        if let Some(step) = loop_step {
            return Ok(step);
        }

        // Resume after an AfterToolResult yield.  Any input submitted by the
        // host during the yield is folded into the transcript as part of the
        // continuation turn; background task results drained just above are
        // already in the transcript.
        if let Some(turn_id) = self.pending_round_resume.take() {
            let drained: Vec<Item> = std::mem::take(&mut self.pending_input);
            self.extend_transcript(drained);
            return self
                .drive_turn(turn_id, false, MutationPoint::AfterToolResult)
                .await;
        }

        if self.pending_input.is_empty() && !had_loop_updates {
            return Ok(LoopStep::Interrupt(LoopInterrupt::AwaitingInput(
                InputRequest {
                    session_id: self.session_id.clone(),
                    reason: "driver is waiting for input".into(),
                },
            )));
        }

        let turn_id = agentkit_core::TurnId::new(format!("turn-{}", self.next_turn_index));
        self.next_turn_index += 1;
        let drained: Vec<Item> = std::mem::take(&mut self.pending_input);
        self.extend_transcript(drained);
        self.drive_turn(turn_id, true, MutationPoint::AfterTurnEnded)
            .await
    }

    fn emit(&self, event: AgentEvent) {
        fan_out_observed_event(&self.observers, &self.observed_session_id, event);
    }

    /// Append a single [`Item`] to the transcript and notify all
    /// registered [`TranscriptObserver`]s. The single mutation point —
    /// every push to `self.transcript` should funnel through here so
    /// observers see exactly what landed in the transcript.
    fn append_item(&mut self, mut item: Item) {
        if item.created_at.is_none() {
            item.created_at = Some(Timestamp::now());
        }
        for observer in &self.transcript_observers {
            observer.on_transcript_event(TranscriptEvent {
                session_id: &self.session_id,
                item: &item,
            });
        }
        self.transcript.push(item);
    }

    /// Append a tool-result Item: emit one [`AgentEvent::ToolResultReceived`]
    /// per [`Part::ToolResult`] inside the Item, then funnel through
    /// [`Self::append_item`].
    ///
    /// If every `ToolResult` in the item references a `call_id` that was
    /// already paired with a synthetic detach tool_result, the item is
    /// converted to a [`ItemKind::Notification`] before appending.
    /// Without this, we would emit a second `tool_result` for the same
    /// `tool_use_id` — a provider-schema violation that
    /// Anthropic/OpenRouter reject as an "orphaned tool_result".
    /// Observers see [`AgentEvent::ToolExecutionProgress`] for the synthetic
    /// detach placeholder and see [`AgentEvent::ToolResultReceived`] only for
    /// the later terminal result.
    fn append_tool_result_item(&mut self, item: Item) {
        for part in &item.parts {
            if let Part::ToolResult(result) = part {
                self.emit(AgentEvent::ToolResultReceived(result.clone()));
                self.clear_tool_cancellation(&result.call_id);
            }
        }
        let item = self.maybe_convert_detached(item);
        self.append_item(item);
    }

    fn drain_pending_approval_for(
        &mut self,
        call_id: &ToolCallId,
    ) -> Option<PendingApprovalToolCall> {
        let pending = self.pending_approvals.remove(call_id)?;
        self.pending_approval_order.retain(|id| id != call_id);
        self.clear_tool_cancellation(call_id);
        Some(pending)
    }

    fn drain_pending_approval_items(&mut self) -> Vec<PendingApprovalToolCall> {
        let order = std::mem::take(&mut self.pending_approval_order);
        let pending = order
            .iter()
            .filter_map(|call_id| {
                let pending = self.pending_approvals.remove(call_id);
                self.clear_tool_cancellation(call_id);
                pending
            })
            .collect();
        self.pending_approvals.clear();
        pending
    }

    fn reject_drained_approvals(&mut self, pending: Vec<PendingApprovalToolCall>) {
        for pending in pending {
            self.emit(AgentEvent::ApprovalResolved { approved: false });
            self.append_tool_result_item(cancelled_approval_item(pending));
        }
    }

    fn maybe_convert_detached(&mut self, item: Item) -> Item {
        if !matches!(item.kind, ItemKind::Tool) {
            return item;
        }
        let results: Vec<&ToolResultPart> = item
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolResult(r) => Some(r),
                _ => None,
            })
            .collect();
        if results.is_empty()
            || !results
                .iter()
                .all(|r| self.detached_call_ids.contains(&r.call_id))
        {
            return item;
        }
        let mut text = String::new();
        for result in &results {
            self.detached_call_ids.remove(&result.call_id);
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            let label = if result.is_error {
                "failed"
            } else {
                "completed"
            };
            let body = render_tool_output_brief(&result.output);
            text.push_str(&format!(
                "Background tool call {} {}: {body}",
                result.call_id.0, label
            ));
        }
        Item::notification(text)
    }

    /// Append several Items in order through [`Self::append_item`].
    /// Pre-stamps `created_at` once per batch so all items in the batch
    /// share a timestamp and `append_item` skips its own clock read.
    fn extend_transcript(&mut self, items: impl IntoIterator<Item = Item>) {
        let now = Timestamp::now();
        for mut item in items {
            if item.created_at.is_none() {
                item.created_at = Some(now);
            }
            self.append_item(item);
        }
    }
}

fn render_tool_output_brief(output: &ToolOutput) -> String {
    match output {
        ToolOutput::Text(t) => t.clone(),
        ToolOutput::Structured(value) => value.to_string(),
        ToolOutput::Parts(parts) => format!("[{} parts]", parts.len()),
        ToolOutput::Files(files) => format!("[{} files]", files.len()),
    }
}

fn interrupted_metadata(stage: &str) -> MetadataMap {
    let mut metadata = MetadataMap::new();
    metadata.insert(INTERRUPTED_METADATA_KEY.into(), true.into());
    metadata.insert(
        INTERRUPT_REASON_METADATA_KEY.into(),
        USER_CANCELLED_REASON.into(),
    );
    metadata.insert(INTERRUPT_STAGE_METADATA_KEY.into(), stage.into());
    metadata
}

fn interrupted_assistant_items() -> Vec<Item> {
    vec![Item {
        id: None,
        kind: ItemKind::Assistant,
        parts: vec![Part::Text(TextPart {
            text: "Previous assistant response was interrupted by the user before completion."
                .into(),
            metadata: interrupted_metadata("assistant"),
        })],
        metadata: interrupted_metadata("assistant"),
        usage: None,
        finish_reason: None,
        created_at: None,
    }]
}

fn cancelled_approval_item(pending: PendingApprovalToolCall) -> Item {
    Item {
        id: None,
        kind: ItemKind::Tool,
        parts: vec![Part::ToolResult(ToolResultPart {
            call_id: pending.call.id,
            output: ToolOutput::Text("approval cancelled".into()),
            is_error: true,
            metadata: pending.call.metadata,
        })],
        metadata: MetadataMap::new(),
        usage: None,
        finish_reason: None,
        created_at: None,
    }
}

/// Whether the transcript ends in something the model should respond to.
///
/// Only input-bearing trailing roles should drive inference. Passive transcript
/// state (`System`, `Developer`, `Context`), an assistant tail, or an empty
/// transcript has nothing new for the model to respond to.
fn transcript_has_pending_input(transcript: &[Item]) -> bool {
    matches!(
        transcript.last().map(|item| item.kind),
        Some(ItemKind::User | ItemKind::Tool | ItemKind::Notification)
    )
}

fn extract_tool_calls(items: &[Item]) -> Vec<ToolCallPart> {
    let mut calls = Vec::new();
    for item in items {
        for part in &item.parts {
            if let Part::ToolCall(call) = part {
                calls.push(call.clone());
            }
        }
    }
    calls
}

fn tool_result_is_error(item: &Item) -> bool {
    item.parts
        .iter()
        .any(|part| matches!(part, Part::ToolResult(result) if result.is_error))
}

fn tool_result_not_started(item: &Item) -> bool {
    item.parts.iter().any(|part| {
        matches!(
            part,
            Part::ToolResult(result)
                if result
                    .metadata
                    .get(TOOL_RESULT_NOT_STARTED_METADATA_KEY)
                    .and_then(Value::as_bool)
                    == Some(true)
        )
    })
}

/// Errors that can occur while driving the agent loop.
#[derive(Debug, Error)]
pub enum LoopError {
    /// The driver was in an unexpected state for the requested operation.
    #[error("invalid driver state: {0}")]
    InvalidState(String),
    /// The current turn was cancelled via the [`CancellationHandle`].
    #[error("turn cancelled")]
    Cancelled,
    /// An error originating from the model provider.
    #[error("provider error: {0}")]
    Provider(String),
    /// An error originating from tool execution.
    #[error("tool error: {0}")]
    Tool(#[from] ToolError),
    /// An error reported by a [`LoopMutator`] (compaction, redaction, repair).
    #[error("mutator error: {0}")]
    Mutator(String),
    /// The requested operation is not supported.
    #[error("unsupported operation: {0}")]
    Unsupported(String),
}

/// Internal [`EventEmitter`] backed by the driver's observer slice. Lives
/// only for the duration of a [`LoopDriver::run_mutators`] call so the
/// borrow against `self.observers` stays disjoint from the cursor's borrow
/// of `self.transcript`.
struct DriverEmitter<'a> {
    session_id: &'a Arc<SessionId>,
    observers: &'a [Arc<dyn LoopObserver>],
}

impl<'a> EventEmitter for DriverEmitter<'a> {
    fn emit(&self, event: AgentEvent) {
        fan_out_observed_event(self.observers, self.session_id, event);
    }
}

fn fan_out_observed_event(
    observers: &[Arc<dyn LoopObserver>],
    session_id: &Arc<SessionId>,
    event: AgentEvent,
) {
    if observers.is_empty() {
        return;
    }
    let observed = ObservedEvent {
        session_id: Arc::clone(session_id),
        event,
    };
    let last = observers.len() - 1;
    for observer in &observers[..last] {
        observer.handle_event(observed.clone());
    }
    observers[last].handle_event(observed);
}

/// Hard-fails when a mutator's edit leaves the transcript protocol-invalid.
/// The only invariant currently checked is tool_use ↔ tool_result pairing
/// — every [`Part::ToolCall`] must be followed (in transcript order) by a
/// matching [`Part::ToolResult`] with the same `call_id`.
fn validate_transcript_invariants(transcript: &[Item]) -> Result<(), LoopError> {
    let mut pending: HashSet<ToolCallId> = HashSet::new();
    let mut seen_calls: HashSet<ToolCallId> = HashSet::new();
    let mut seen_results: HashSet<ToolCallId> = HashSet::new();
    for item in transcript {
        for part in &item.parts {
            match part {
                Part::ToolCall(call) => {
                    if !seen_calls.insert(call.id.clone()) {
                        return Err(LoopError::Mutator(format!(
                            "transcript invariant violation: duplicate tool_use: {}",
                            call.id.0
                        )));
                    }
                    pending.insert(call.id.clone());
                }
                Part::ToolResult(result) => {
                    if !pending.remove(&result.call_id) {
                        let kind = if seen_results.contains(&result.call_id) {
                            "duplicate"
                        } else {
                            "orphaned"
                        };
                        return Err(LoopError::Mutator(format!(
                            "transcript invariant violation: {kind} tool_result: {}",
                            result.call_id.0
                        )));
                    }
                    seen_results.insert(result.call_id.clone());
                }
                _ => {}
            }
        }
    }
    if !pending.is_empty() {
        let missing: Vec<String> = pending.into_iter().map(|id| id.0).collect();
        return Err(LoopError::Mutator(format!(
            "transcript invariant violation: tool_use(s) without matching tool_result: {}",
            missing.join(", ")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc as StdArc, Mutex as StdMutex};

    use agentkit_core::{
        CancellationController, ItemKind, Part, TextPart, ToolCallId, ToolCallPart, ToolOutput,
        ToolResultPart,
    };
    use agentkit_task_manager::{
        AsyncTaskManager, RoutingDecision, TaskEvent, TaskManager, TaskManagerHandle,
        TaskRoutingPolicy,
    };
    use agentkit_tools_core::{
        FileSystemPermissionRequest, PermissionCode, PermissionDecision, PermissionDenial, Tool,
        ToolAnnotations, ToolCatalogEvent, ToolExecutionOutcome, ToolName, ToolRegistry,
        ToolResult, ToolSpec,
    };
    use serde_json::{Value, json};
    use tokio::sync::Notify;
    use tokio::time::{Duration, timeout};

    use super::*;

    struct FakeAdapter;
    struct SlowAdapter;
    struct RecordingAdapter {
        seen_descriptions: StdArc<StdMutex<Vec<Vec<String>>>>,
        seen_caches: StdArc<StdMutex<Vec<Option<PromptCacheRequest>>>>,
    }
    struct MultiToolAdapter;
    struct DualApprovalAdapter;

    struct FakeSession;
    struct SlowSession;
    struct RecordingSession {
        seen_descriptions: StdArc<StdMutex<Vec<Vec<String>>>>,
        seen_caches: StdArc<StdMutex<Vec<Option<PromptCacheRequest>>>>,
    }
    struct MultiToolSession;
    struct DualApprovalSession;

    struct FakeTurn {
        events: VecDeque<ModelTurnEvent>,
    }

    struct SlowTurn {
        emitted: bool,
    }

    struct RecordingTurn {
        emitted: bool,
    }
    struct MultiToolTurn {
        events: VecDeque<ModelTurnEvent>,
    }
    struct DualApprovalTurn {
        events: VecDeque<ModelTurnEvent>,
    }

    struct DelayedApprovalExecutor {
        entered: StdArc<AtomicBool>,
        release: StdArc<Notify>,
        spec: ToolSpec,
    }

    impl DelayedApprovalExecutor {
        fn new(entered: StdArc<AtomicBool>, release: StdArc<Notify>) -> Self {
            Self {
                entered,
                release,
                spec: ToolSpec {
                    name: ToolName::new("echo"),
                    description: "delayed approval".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "value": { "type": "string" }
                        },
                        "required": ["value"],
                        "additionalProperties": false
                    }),
                    output_schema: None,
                    annotations: ToolAnnotations::default(),
                    metadata: MetadataMap::new(),
                },
            }
        }
    }

    #[async_trait]
    impl ToolExecutor for DelayedApprovalExecutor {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![self.spec.clone()]
        }

        async fn execute(
            &self,
            request: ToolRequest,
            _ctx: &mut ToolContext<'_>,
        ) -> ToolExecutionOutcome {
            self.entered.store(true, Ordering::SeqCst);
            self.release.notified().await;
            ToolExecutionOutcome::Interrupted(
                agentkit_tools_core::ToolInterruption::ApprovalRequired(ApprovalRequest {
                    task_id: None,
                    call_id: None,
                    id: "approval:delayed".into(),
                    request_kind: "delayed.approval".into(),
                    reason: agentkit_tools_core::ApprovalReason::PolicyRequiresConfirmation,
                    summary: "delayed approval".into(),
                    metadata: request.metadata,
                }),
            )
        }
    }

    #[async_trait]
    impl ModelAdapter for FakeAdapter {
        type Session = FakeSession;

        async fn start_session(&self, _config: SessionConfig) -> Result<Self::Session, LoopError> {
            Ok(FakeSession)
        }
    }

    #[async_trait]
    impl ModelAdapter for SlowAdapter {
        type Session = SlowSession;

        async fn start_session(&self, _config: SessionConfig) -> Result<Self::Session, LoopError> {
            Ok(SlowSession)
        }
    }

    #[async_trait]
    impl ModelAdapter for RecordingAdapter {
        type Session = RecordingSession;

        async fn start_session(&self, _config: SessionConfig) -> Result<Self::Session, LoopError> {
            Ok(RecordingSession {
                seen_descriptions: self.seen_descriptions.clone(),
                seen_caches: self.seen_caches.clone(),
            })
        }
    }

    #[async_trait]
    impl ModelAdapter for MultiToolAdapter {
        type Session = MultiToolSession;

        async fn start_session(&self, _config: SessionConfig) -> Result<Self::Session, LoopError> {
            Ok(MultiToolSession)
        }
    }

    #[async_trait]
    impl ModelAdapter for DualApprovalAdapter {
        type Session = DualApprovalSession;

        async fn start_session(&self, _config: SessionConfig) -> Result<Self::Session, LoopError> {
            Ok(DualApprovalSession)
        }
    }

    #[async_trait]
    impl ModelSession for FakeSession {
        type Turn = FakeTurn;

        async fn begin_turn(
            &mut self,
            request: TurnRequest,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Self::Turn, LoopError> {
            let has_tool_result = request.transcript.iter().any(|item| {
                item.kind == ItemKind::Tool
                    && item
                        .parts
                        .iter()
                        .any(|part| matches!(part, Part::ToolResult(_)))
            });
            let tool_name = request
                .available_tools
                .first()
                .map(|tool| tool.name.0.clone())
                .unwrap_or_else(|| "echo".into());

            let events = if has_tool_result {
                let result_text = request
                    .transcript
                    .iter()
                    .rev()
                    .find_map(|item| {
                        item.parts.iter().find_map(|part| match part {
                            Part::ToolResult(ToolResultPart {
                                output: ToolOutput::Text(text),
                                ..
                            }) => Some(text.clone()),
                            _ => None,
                        })
                    })
                    .unwrap_or_else(|| "missing".into());

                VecDeque::from([ModelTurnEvent::Finished(ModelTurnResult {
                    model: None,
                    response_id: None,
                    finish_reason: FinishReason::Completed,
                    output_items: vec![Item {
                        id: None,
                        kind: ItemKind::Assistant,
                        parts: vec![Part::Text(TextPart {
                            text: format!("tool said: {result_text}"),
                            metadata: MetadataMap::new(),
                        })],
                        metadata: MetadataMap::new(),
                        usage: None,
                        finish_reason: None,
                        created_at: None,
                    }],
                    usage: None,
                    metadata: MetadataMap::new(),
                })])
            } else {
                VecDeque::from([
                    ModelTurnEvent::ToolCall(agentkit_core::ToolCallPart {
                        id: ToolCallId::new("call-1"),
                        name: tool_name.clone(),
                        input: json!({ "value": "pong" }),
                        metadata: MetadataMap::new(),
                    }),
                    ModelTurnEvent::Finished(ModelTurnResult {
                        model: None,
                        response_id: None,
                        finish_reason: FinishReason::ToolCall,
                        output_items: vec![Item {
                            id: None,
                            kind: ItemKind::Assistant,
                            parts: vec![Part::ToolCall(agentkit_core::ToolCallPart {
                                id: ToolCallId::new("call-1"),
                                name: tool_name,
                                input: json!({ "value": "pong" }),
                                metadata: MetadataMap::new(),
                            })],
                            metadata: MetadataMap::new(),
                            usage: None,
                            finish_reason: None,
                            created_at: None,
                        }],
                        usage: None,
                        metadata: MetadataMap::new(),
                    }),
                ])
            };

            Ok(FakeTurn { events })
        }
    }

    #[async_trait]
    impl ModelSession for SlowSession {
        type Turn = SlowTurn;

        async fn begin_turn(
            &mut self,
            request: TurnRequest,
            cancellation: Option<TurnCancellation>,
        ) -> Result<Self::Turn, LoopError> {
            let should_block = request
                .transcript
                .iter()
                .rev()
                .find(|item| item.kind == ItemKind::User)
                .is_some_and(|item| {
                    item.parts.iter().any(|part| match part {
                        Part::Text(text) => text.text == "do the long task",
                        _ => false,
                    })
                });

            if should_block && let Some(cancellation) = cancellation {
                cancellation.cancelled().await;
                return Err(LoopError::Cancelled);
            }

            Ok(SlowTurn { emitted: false })
        }
    }

    #[async_trait]
    impl ModelSession for RecordingSession {
        type Turn = RecordingTurn;

        async fn begin_turn(
            &mut self,
            request: TurnRequest,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Self::Turn, LoopError> {
            let descriptions = request
                .available_tools
                .iter()
                .map(|tool| tool.description.clone())
                .collect::<Vec<_>>();
            self.seen_descriptions.lock().unwrap().push(descriptions);
            self.seen_caches.lock().unwrap().push(request.cache.clone());

            Ok(RecordingTurn { emitted: false })
        }
    }

    #[async_trait]
    impl ModelSession for MultiToolSession {
        type Turn = MultiToolTurn;

        async fn begin_turn(
            &mut self,
            request: TurnRequest,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Self::Turn, LoopError> {
            let has_tool_result = request.transcript.iter().any(|item| {
                item.kind == ItemKind::Tool
                    && item
                        .parts
                        .iter()
                        .any(|part| matches!(part, Part::ToolResult(_)))
            });

            let events = if has_tool_result {
                VecDeque::from([ModelTurnEvent::Finished(ModelTurnResult {
                    model: None,
                    response_id: None,
                    finish_reason: FinishReason::Completed,
                    output_items: vec![Item {
                        id: None,
                        kind: ItemKind::Assistant,
                        parts: vec![Part::Text(TextPart {
                            text: "mixed tools finished".into(),
                            metadata: MetadataMap::new(),
                        })],
                        metadata: MetadataMap::new(),
                        usage: None,
                        finish_reason: None,
                        created_at: None,
                    }],
                    usage: None,
                    metadata: MetadataMap::new(),
                })])
            } else {
                let foreground = agentkit_core::ToolCallPart {
                    id: ToolCallId::new("call-foreground"),
                    name: "foreground-wait".into(),
                    input: json!({}),
                    metadata: MetadataMap::new(),
                };
                let background = agentkit_core::ToolCallPart {
                    id: ToolCallId::new("call-background"),
                    name: "background-wait".into(),
                    input: json!({}),
                    metadata: MetadataMap::new(),
                };
                VecDeque::from([
                    ModelTurnEvent::ToolCall(foreground.clone()),
                    ModelTurnEvent::ToolCall(background.clone()),
                    ModelTurnEvent::Finished(ModelTurnResult {
                        model: None,
                        response_id: None,
                        finish_reason: FinishReason::ToolCall,
                        output_items: vec![Item {
                            id: None,
                            kind: ItemKind::Assistant,
                            parts: vec![Part::ToolCall(foreground), Part::ToolCall(background)],
                            metadata: MetadataMap::new(),
                            usage: None,
                            finish_reason: None,
                            created_at: None,
                        }],
                        usage: None,
                        metadata: MetadataMap::new(),
                    }),
                ])
            };

            Ok(MultiToolTurn { events })
        }
    }

    #[async_trait]
    impl ModelSession for DualApprovalSession {
        type Turn = DualApprovalTurn;

        async fn begin_turn(
            &mut self,
            request: TurnRequest,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Self::Turn, LoopError> {
            let tool_results = request
                .transcript
                .iter()
                .flat_map(|item| item.parts.iter())
                .filter(|part| matches!(part, Part::ToolResult(_)))
                .count();

            let events = if tool_results >= 2 {
                VecDeque::from([ModelTurnEvent::Finished(ModelTurnResult {
                    model: None,
                    response_id: None,
                    finish_reason: FinishReason::Completed,
                    output_items: vec![Item {
                        id: None,
                        kind: ItemKind::Assistant,
                        parts: vec![Part::Text(TextPart {
                            text: "both approvals finished".into(),
                            metadata: MetadataMap::new(),
                        })],
                        metadata: MetadataMap::new(),
                        usage: None,
                        finish_reason: None,
                        created_at: None,
                    }],
                    usage: None,
                    metadata: MetadataMap::new(),
                })])
            } else {
                let first = agentkit_core::ToolCallPart {
                    id: ToolCallId::new("call-1"),
                    name: "echo".into(),
                    input: json!({ "value": "first" }),
                    metadata: MetadataMap::new(),
                };
                let second = agentkit_core::ToolCallPart {
                    id: ToolCallId::new("call-2"),
                    name: "echo".into(),
                    input: json!({ "value": "second" }),
                    metadata: MetadataMap::new(),
                };
                VecDeque::from([
                    ModelTurnEvent::ToolCall(first.clone()),
                    ModelTurnEvent::ToolCall(second.clone()),
                    ModelTurnEvent::Finished(ModelTurnResult {
                        model: None,
                        response_id: None,
                        finish_reason: FinishReason::ToolCall,
                        output_items: vec![Item {
                            id: None,
                            kind: ItemKind::Assistant,
                            parts: vec![Part::ToolCall(first), Part::ToolCall(second)],
                            metadata: MetadataMap::new(),
                            usage: None,
                            finish_reason: None,
                            created_at: None,
                        }],
                        usage: None,
                        metadata: MetadataMap::new(),
                    }),
                ])
            };

            Ok(DualApprovalTurn { events })
        }
    }

    #[async_trait]
    impl ModelTurn for FakeTurn {
        async fn next_event(
            &mut self,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Option<ModelTurnEvent>, LoopError> {
            Ok(self.events.pop_front())
        }
    }

    #[async_trait]
    impl ModelTurn for SlowTurn {
        async fn next_event(
            &mut self,
            cancellation: Option<TurnCancellation>,
        ) -> Result<Option<ModelTurnEvent>, LoopError> {
            if let Some(cancellation) = cancellation
                && cancellation.is_cancelled()
            {
                return Err(LoopError::Cancelled);
            }

            if self.emitted {
                Ok(None)
            } else {
                self.emitted = true;
                Ok(Some(ModelTurnEvent::Finished(ModelTurnResult {
                    model: None,
                    response_id: None,
                    finish_reason: FinishReason::Completed,
                    output_items: vec![Item {
                        id: None,
                        kind: ItemKind::Assistant,
                        parts: vec![Part::Text(TextPart {
                            text: "done".into(),
                            metadata: MetadataMap::new(),
                        })],
                        metadata: MetadataMap::new(),
                        usage: None,
                        finish_reason: None,
                        created_at: None,
                    }],
                    usage: None,
                    metadata: MetadataMap::new(),
                })))
            }
        }
    }

    #[async_trait]
    impl ModelTurn for RecordingTurn {
        async fn next_event(
            &mut self,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Option<ModelTurnEvent>, LoopError> {
            if self.emitted {
                Ok(None)
            } else {
                self.emitted = true;
                Ok(Some(ModelTurnEvent::Finished(ModelTurnResult {
                    model: None,
                    response_id: None,
                    finish_reason: FinishReason::Completed,
                    output_items: vec![Item {
                        id: None,
                        kind: ItemKind::Assistant,
                        parts: vec![Part::Text(TextPart {
                            text: "done".into(),
                            metadata: MetadataMap::new(),
                        })],
                        metadata: MetadataMap::new(),
                        usage: None,
                        finish_reason: None,
                        created_at: None,
                    }],
                    usage: None,
                    metadata: MetadataMap::new(),
                })))
            }
        }
    }

    #[async_trait]
    impl ModelTurn for MultiToolTurn {
        async fn next_event(
            &mut self,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Option<ModelTurnEvent>, LoopError> {
            Ok(self.events.pop_front())
        }
    }

    #[async_trait]
    impl ModelTurn for DualApprovalTurn {
        async fn next_event(
            &mut self,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Option<ModelTurnEvent>, LoopError> {
            Ok(self.events.pop_front())
        }
    }

    #[derive(Clone)]
    struct EchoTool {
        spec: ToolSpec,
    }

    #[derive(Clone)]
    struct FailingTool {
        spec: ToolSpec,
    }

    #[derive(Clone)]
    struct RunThenDenyTool {
        spec: ToolSpec,
    }

    impl Default for EchoTool {
        fn default() -> Self {
            Self {
                spec: ToolSpec {
                    name: ToolName::new("echo"),
                    description: "Echo back a value".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "value": { "type": "string" }
                        },
                        "required": ["value"],
                        "additionalProperties": false
                    }),
                    output_schema: None,
                    annotations: ToolAnnotations::default(),
                    metadata: MetadataMap::new(),
                },
            }
        }
    }

    impl Default for FailingTool {
        fn default() -> Self {
            Self {
                spec: ToolSpec {
                    name: ToolName::new("failing"),
                    description: "Always fails after execution starts".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "value": { "type": "string" }
                        },
                        "additionalProperties": true
                    }),
                    output_schema: None,
                    annotations: ToolAnnotations::default(),
                    metadata: MetadataMap::new(),
                },
            }
        }
    }

    impl Default for RunThenDenyTool {
        fn default() -> Self {
            Self {
                spec: ToolSpec {
                    name: ToolName::new("run_then_deny"),
                    description: "Runs, then returns a permission-denied error".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "value": { "type": "string" }
                        },
                        "additionalProperties": true
                    }),
                    output_schema: None,
                    annotations: ToolAnnotations::default(),
                    metadata: MetadataMap::new(),
                },
            }
        }
    }

    #[derive(Clone)]
    struct DynamicSpecTool {
        spec: ToolSpec,
        version: StdArc<AtomicUsize>,
    }

    impl DynamicSpecTool {
        fn new(version: StdArc<AtomicUsize>) -> Self {
            Self {
                spec: ToolSpec {
                    name: ToolName::new("dynamic"),
                    description: "dynamic version 0".into(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }),
                    output_schema: None,
                    annotations: ToolAnnotations::default(),
                    metadata: MetadataMap::new(),
                },
                version,
            }
        }
    }

    #[async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn proposed_requests(
            &self,
            request: &agentkit_tools_core::ToolRequest,
        ) -> Result<
            Vec<Box<dyn agentkit_tools_core::PermissionRequest>>,
            agentkit_tools_core::ToolError,
        > {
            Ok(vec![Box::new(FileSystemPermissionRequest::Read {
                path: "/tmp/echo".into(),
                metadata: request.metadata.clone(),
            })])
        }

        async fn invoke(
            &self,
            request: agentkit_tools_core::ToolRequest,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, agentkit_tools_core::ToolError> {
            let value = request
                .input
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    agentkit_tools_core::ToolError::InvalidInput("missing value".into())
                })?;

            Ok(ToolResult {
                result: ToolResultPart {
                    call_id: request.call_id,
                    output: ToolOutput::Text(value.into()),
                    is_error: false,
                    metadata: MetadataMap::new(),
                },
                duration: None,
                metadata: MetadataMap::new(),
            })
        }
    }

    #[async_trait]
    impl Tool for FailingTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        async fn invoke(
            &self,
            _request: agentkit_tools_core::ToolRequest,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, agentkit_tools_core::ToolError> {
            Err(agentkit_tools_core::ToolError::ExecutionFailed(
                "runtime failed".into(),
            ))
        }
    }

    #[async_trait]
    impl Tool for RunThenDenyTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        async fn invoke(
            &self,
            _request: agentkit_tools_core::ToolRequest,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, agentkit_tools_core::ToolError> {
            Err(agentkit_tools_core::ToolError::PermissionDenied(
                PermissionDenial {
                    code: PermissionCode::CustomPolicyDenied,
                    message: "remote 403".into(),
                    metadata: MetadataMap::new(),
                },
            ))
        }
    }

    #[async_trait]
    impl Tool for DynamicSpecTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        fn current_spec(&self) -> Option<ToolSpec> {
            let mut spec = self.spec.clone();
            spec.description = format!("dynamic version {}", self.version.load(Ordering::SeqCst));
            Some(spec)
        }

        async fn invoke(
            &self,
            request: agentkit_tools_core::ToolRequest,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, agentkit_tools_core::ToolError> {
            Ok(ToolResult {
                result: ToolResultPart {
                    call_id: request.call_id,
                    output: ToolOutput::Text("ok".into()),
                    is_error: false,
                    metadata: MetadataMap::new(),
                },
                duration: None,
                metadata: MetadataMap::new(),
            })
        }
    }

    struct DenyFsReads;

    impl PermissionChecker for DenyFsReads {
        fn evaluate(
            &self,
            request: &dyn agentkit_tools_core::PermissionRequest,
        ) -> PermissionDecision {
            if request.kind() == "filesystem.read" {
                return PermissionDecision::Deny(PermissionDenial {
                    code: PermissionCode::PathNotAllowed,
                    message: "reads denied in test".into(),
                    metadata: MetadataMap::new(),
                });
            }

            PermissionDecision::Allow
        }
    }

    struct ApproveFsReads;

    impl PermissionChecker for ApproveFsReads {
        fn evaluate(
            &self,
            request: &dyn agentkit_tools_core::PermissionRequest,
        ) -> PermissionDecision {
            if request.kind() == "filesystem.read" {
                return PermissionDecision::RequireApproval(ApprovalRequest {
                    task_id: None,
                    call_id: None,
                    id: "approval:fs-read".into(),
                    request_kind: request.kind().into(),
                    reason: agentkit_tools_core::ApprovalReason::SensitivePath,
                    summary: request.summary(),
                    metadata: request.metadata().clone(),
                });
            }

            PermissionDecision::Allow
        }
    }

    struct KeepRecentMutator {
        keep: usize,
    }

    #[async_trait]
    impl LoopMutator for KeepRecentMutator {
        async fn mutate(
            &self,
            cursor: &mut TranscriptCursor<'_>,
            ctx: LoopCtx<'_>,
        ) -> Result<(), LoopError> {
            if cursor.len() < 2 {
                return Ok(());
            }
            let drop = cursor.len().saturating_sub(self.keep);
            ctx.emitter.emit(AgentEvent::MutationStarted {
                session_id: ctx.session_id.clone(),
                turn_id: ctx.turn_id.cloned(),
                mutator: "keep-recent".into(),
                point: ctx.point,
            });
            cursor.drain(..drop);
            ctx.emitter.emit(AgentEvent::MutationFinished {
                session_id: ctx.session_id.clone(),
                turn_id: ctx.turn_id.cloned(),
                mutator: "keep-recent".into(),
                dirty: true,
                metadata: MetadataMap::new(),
            });
            Ok(())
        }
    }

    /// No-op mutator that records the [`MutationPoint`] it is invoked with at
    /// each mutation site, so a test can assert which point the loop reports.
    struct PointRecordingMutator {
        points: StdArc<StdMutex<Vec<MutationPoint>>>,
    }

    #[async_trait]
    impl LoopMutator for PointRecordingMutator {
        async fn mutate(
            &self,
            _cursor: &mut TranscriptCursor<'_>,
            ctx: LoopCtx<'_>,
        ) -> Result<(), LoopError> {
            self.points.lock().unwrap().push(ctx.point);
            Ok(())
        }
    }

    struct RecordingObserver {
        events: StdArc<StdMutex<Vec<AgentEvent>>>,
    }

    impl LoopObserver for RecordingObserver {
        fn handle_event(&self, event: ObservedEvent) {
            let event = event.event;
            self.events.lock().unwrap().push(event);
        }
    }

    struct CatalogExecutor {
        version: AtomicUsize,
        events: StdMutex<Vec<ToolCatalogEvent>>,
    }

    impl CatalogExecutor {
        fn new() -> Self {
            Self {
                version: AtomicUsize::new(0),
                events: StdMutex::new(Vec::new()),
            }
        }

        fn publish_change(&self, version: usize, event: ToolCatalogEvent) {
            self.version.store(version, Ordering::SeqCst);
            self.events.lock().unwrap().push(event);
        }
    }

    #[async_trait]
    impl ToolExecutor for CatalogExecutor {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: ToolName::new("dynamic"),
                description: format!("dynamic version {}", self.version.load(Ordering::SeqCst)),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }),
                output_schema: None,
                annotations: ToolAnnotations::default(),
                metadata: MetadataMap::new(),
            }]
        }

        fn drain_catalog_events(&self) -> Vec<ToolCatalogEvent> {
            std::mem::take(&mut *self.events.lock().unwrap())
        }

        async fn execute(
            &self,
            request: ToolRequest,
            _ctx: &mut ToolContext<'_>,
        ) -> ToolExecutionOutcome {
            ToolExecutionOutcome::Completed(ToolResult {
                result: ToolResultPart {
                    call_id: request.call_id,
                    output: ToolOutput::Text("dynamic-ok".into()),
                    is_error: false,
                    metadata: MetadataMap::new(),
                },
                duration: None,
                metadata: MetadataMap::new(),
            })
        }
    }

    #[derive(Clone)]
    struct BlockingTool {
        spec: ToolSpec,
        entered: StdArc<AtomicBool>,
        release: StdArc<Notify>,
        output: &'static str,
    }

    impl BlockingTool {
        fn new(
            name: &str,
            entered: StdArc<AtomicBool>,
            release: StdArc<Notify>,
            output: &'static str,
        ) -> Self {
            Self {
                spec: ToolSpec {
                    name: ToolName::new(name),
                    description: format!("blocking tool {name}"),
                    input_schema: json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }),
                    output_schema: None,
                    annotations: ToolAnnotations::default(),
                    metadata: MetadataMap::new(),
                },
                entered,
                release,
                output,
            }
        }
    }

    #[async_trait]
    impl Tool for BlockingTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        async fn invoke(
            &self,
            request: agentkit_tools_core::ToolRequest,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, agentkit_tools_core::ToolError> {
            self.entered.store(true, Ordering::SeqCst);
            self.release.notified().await;
            Ok(ToolResult {
                result: ToolResultPart {
                    call_id: request.call_id,
                    output: ToolOutput::Text(self.output.into()),
                    is_error: false,
                    metadata: MetadataMap::new(),
                },
                duration: None,
                metadata: MetadataMap::new(),
            })
        }
    }

    struct NameRoutingPolicy {
        routes: Vec<(String, RoutingDecision)>,
    }

    impl NameRoutingPolicy {
        fn new(routes: impl IntoIterator<Item = (impl Into<String>, RoutingDecision)>) -> Self {
            Self {
                routes: routes
                    .into_iter()
                    .map(|(name, decision)| (name.into(), decision))
                    .collect(),
            }
        }
    }

    impl TaskRoutingPolicy for NameRoutingPolicy {
        fn route(&self, request: &ToolRequest) -> RoutingDecision {
            self.routes
                .iter()
                .find(|(name, _)| name == &request.tool_name.0)
                .map(|(_, decision)| *decision)
                .unwrap_or(RoutingDecision::Foreground)
        }
    }

    async fn wait_for_task_event(handle: &TaskManagerHandle) -> TaskEvent {
        timeout(Duration::from_secs(1), handle.next_event())
            .await
            .expect("timed out waiting for task event")
            .expect("task event stream ended unexpectedly")
    }

    async fn wait_until_entered(flag: &AtomicBool) {
        timeout(Duration::from_secs(1), async {
            while !flag.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task never entered execution");
    }

    async fn wait_until_completed(handle: &TaskManagerHandle) {
        timeout(Duration::from_secs(1), async {
            while handle.list_completed().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("task never completed");
    }

    #[tokio::test]
    async fn loop_continues_after_completed_tool_call() {
        let tools = ToolRegistry::new().with(EchoTool::default());
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(AllowAllPermissions)
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-1"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "ping".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }])
            .unwrap();

        let result = run_until_finished(&mut driver).await;

        match result {
            LoopStep::Finished(turn) => {
                assert_eq!(turn.finish_reason, FinishReason::Completed);
                assert_eq!(turn.items.len(), 1);
                match &turn.items[0].parts[0] {
                    Part::Text(text) => assert_eq!(text.text, "tool said: pong"),
                    other => panic!("unexpected part: {other:?}"),
                }
            }
            other => panic!("unexpected loop step: {other:?}"),
        }
    }

    /// Test helper: drives the loop, transparently resuming non-blocking
    /// cooperative interrupts (AfterToolResult), until a terminal step or a
    /// blocking interrupt is reached.
    async fn run_until_finished<S: ModelSession + Send>(driver: &mut LoopDriver<S>) -> LoopStep {
        loop {
            match driver.next().await.unwrap() {
                LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
                step => return step,
            }
        }
    }

    /// A mutator runs at the top of every `drive_turn`, and the loop labels
    /// the site via [`MutationPoint`]. The first drive of a turn is
    /// `AfterTurnEnded`; the continuation drive that follows a completed tool
    /// round must be `AfterToolResult` (a tool result was just appended and an
    /// inference call is imminent). This pins that the continuation reports the
    /// correct point.
    #[tokio::test]
    async fn post_tool_continuation_reports_after_tool_result_mutation_point() {
        let points = StdArc::new(StdMutex::new(Vec::<MutationPoint>::new()));
        let tools = ToolRegistry::new().with(EchoTool::default());
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(AllowAllPermissions)
            .mutator(PointRecordingMutator {
                points: points.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-mutation-point"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item::text(ItemKind::User, "ping")])
            .unwrap();

        // FakeSession: turn 1 emits a tool call, the continuation turn finishes.
        let _ = run_until_finished(&mut driver).await;

        let recorded = points.lock().unwrap().clone();
        assert_eq!(
            recorded.first(),
            Some(&MutationPoint::AfterTurnEnded),
            "first drive of a fresh turn must report AfterTurnEnded, got {recorded:?}"
        );
        assert!(
            recorded.contains(&MutationPoint::AfterToolResult),
            "post-tool continuation must report AfterToolResult, got {recorded:?}"
        );
    }

    #[test]
    fn pending_input_requires_input_bearing_tail_role() {
        assert!(!transcript_has_pending_input(&[]));
        assert!(!transcript_has_pending_input(&[Item::text(
            ItemKind::System,
            "system"
        )]));
        assert!(!transcript_has_pending_input(&[Item::text(
            ItemKind::Developer,
            "developer"
        )]));
        assert!(!transcript_has_pending_input(&[Item::text(
            ItemKind::Context,
            "context"
        )]));
        assert!(!transcript_has_pending_input(&[Item::text(
            ItemKind::Assistant,
            "assistant"
        )]));

        assert!(transcript_has_pending_input(&[Item::text(
            ItemKind::User,
            "user"
        )]));
        assert!(transcript_has_pending_input(&[Item::notification(
            "background update"
        )]));
        assert!(transcript_has_pending_input(&[Item {
            id: None,
            kind: ItemKind::Tool,
            parts: vec![Part::ToolResult(ToolResultPart {
                call_id: ToolCallId::new("call-test"),
                output: ToolOutput::Text("ok".into()),
                is_error: false,
                metadata: MetadataMap::new(),
            })],
            metadata: MetadataMap::new(),
            usage: None,
            finish_reason: None,
            created_at: None,
        }]));
    }

    /// Drops a trailing `User` item. Stands in for any mutator that removes the
    /// freshly-submitted input during `drive_turn` — e.g. a compaction pass
    /// that summarises the latest user turn away, or a normalisation step that
    /// strips an empty user prompt — leaving the transcript ending in an
    /// assistant message.
    struct DropTrailingUserMutator;

    #[async_trait]
    impl LoopMutator for DropTrailingUserMutator {
        async fn mutate(
            &self,
            cursor: &mut TranscriptCursor<'_>,
            _ctx: LoopCtx<'_>,
        ) -> Result<(), LoopError> {
            if cursor.last().map(|item| item.kind) == Some(ItemKind::User) {
                cursor.pop();
            }
            Ok(())
        }
    }

    /// Mirrors the provider gram hit (Vertex/Bedrock via OpenRouter): a model
    /// that rejects any request whose final message is an assistant message
    /// ("assistant prefill — the conversation must end with a user message").
    /// Records whether it was ever asked to begin such a turn.
    struct RejectAssistantPrefillAdapter {
        saw_assistant_tail: StdArc<AtomicBool>,
    }

    struct RejectAssistantPrefillSession {
        saw_assistant_tail: StdArc<AtomicBool>,
    }

    #[async_trait]
    impl ModelAdapter for RejectAssistantPrefillAdapter {
        type Session = RejectAssistantPrefillSession;

        async fn start_session(&self, _config: SessionConfig) -> Result<Self::Session, LoopError> {
            Ok(RejectAssistantPrefillSession {
                saw_assistant_tail: self.saw_assistant_tail.clone(),
            })
        }
    }

    #[async_trait]
    impl ModelSession for RejectAssistantPrefillSession {
        type Turn = FakeTurn;

        async fn begin_turn(
            &mut self,
            request: TurnRequest,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Self::Turn, LoopError> {
            if request.transcript.last().map(|item| item.kind) == Some(ItemKind::Assistant) {
                self.saw_assistant_tail.store(true, Ordering::SeqCst);
                return Err(LoopError::Provider(
                    "conversation must end with a user message".into(),
                ));
            }
            Ok(FakeTurn {
                events: VecDeque::from([ModelTurnEvent::Finished(ModelTurnResult {
                    model: None,
                    response_id: None,
                    finish_reason: FinishReason::Completed,
                    output_items: vec![Item::text(ItemKind::Assistant, "ok")],
                    usage: None,
                    metadata: MetadataMap::new(),
                })]),
            })
        }
    }

    /// Reproduces the exact failure mode observed in gram: a mutator removes
    /// the just-submitted user input during `drive_turn`, so the transcript
    /// ends in an assistant message with nothing for the model to respond to.
    /// The loop must NOT dispatch a model request in that state — there is no
    /// valid trailing input to drive with — it should finish the turn instead.
    /// The adapter stands in for a provider that rejects assistant prefill, so
    /// any dispatch in this state would surface as a provider error.
    #[tokio::test]
    async fn drive_does_not_dispatch_without_valid_trailing_input() {
        let saw_assistant_tail = StdArc::new(AtomicBool::new(false));
        let agent = Agent::builder()
            .model(RejectAssistantPrefillAdapter {
                saw_assistant_tail: saw_assistant_tail.clone(),
            })
            .mutator(DropTrailingUserMutator)
            // Prior conversation ending in an assistant message — e.g. a cold
            // bootstrap that loaded a completed turn's history.
            .transcript(vec![
                Item::text(ItemKind::User, "kickoff"),
                Item::text(ItemKind::Assistant, "prior reply"),
            ])
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-no-valid-input"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item::text(ItemKind::User, "follow up")])
            .unwrap();

        // The mutator strips the "follow up" user item, leaving [user, assistant].
        let outcome = driver.next().await;

        assert!(
            !saw_assistant_tail.load(Ordering::SeqCst),
            "loop dispatched a model turn whose transcript ends in an assistant \
             message (outcome: {outcome:?}); with no valid trailing input the turn \
             must finish instead of driving"
        );
    }

    #[tokio::test]
    async fn loop_uses_injected_permission_checker() {
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let tools = ToolRegistry::new().with(EchoTool::default());
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(DenyFsReads)
            .observer(RecordingObserver {
                events: events.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-2"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "ping".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }])
            .unwrap();

        let result = run_until_finished(&mut driver).await;

        match result {
            LoopStep::Finished(turn) => match &turn.items[0].parts[0] {
                Part::Text(text) => assert!(text.text.contains("tool permission denied")),
                other => panic!("unexpected part: {other:?}"),
            },
            other => panic!("unexpected loop step: {other:?}"),
        }

        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .all(|event| !matches!(event, AgentEvent::ToolExecutionStarted(_))),
            "denied tools must not be reported as started"
        );
    }

    #[tokio::test]
    async fn failed_tool_execution_still_reports_started() {
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let tools = ToolRegistry::new().with(FailingTool::default());
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(AllowAllPermissions)
            .observer(RecordingObserver {
                events: events.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-failing-start-event"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item::text(ItemKind::User, "ping")])
            .unwrap();

        match run_until_finished(&mut driver).await {
            LoopStep::Finished(turn) => assert_eq!(turn.finish_reason, FinishReason::Completed),
            other => panic!("unexpected loop step: {other:?}"),
        }

        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolExecutionStarted(call) if call.name == "failing"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResultReceived(result) if result.is_error
        )));
    }

    #[tokio::test]
    async fn run_then_deny_tool_execution_still_reports_started() {
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let tools = ToolRegistry::new().with(RunThenDenyTool::default());
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(AllowAllPermissions)
            .observer(RecordingObserver {
                events: events.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-run-then-deny-start-event"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item::text(ItemKind::User, "ping")])
            .unwrap();

        match run_until_finished(&mut driver).await {
            LoopStep::Finished(turn) => assert_eq!(turn.finish_reason, FinishReason::Completed),
            other => panic!("unexpected loop step: {other:?}"),
        }

        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolExecutionStarted(call) if call.name == "run_then_deny"
        )));
        // A mid-execution denial is still a permission denial (failure_kind),
        // but the tool DID start, so it must not carry the not-started marker.
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResultReceived(result)
                if result.is_error
                    && result
                        .metadata
                        .get(TOOL_RESULT_FAILURE_KIND_METADATA_KEY)
                        .and_then(Value::as_str)
                        == Some(TOOL_RESULT_FAILURE_KIND_PERMISSION_DENIED)
                    && result
                        .metadata
                        .get(TOOL_RESULT_NOT_STARTED_METADATA_KEY)
                        .is_none()
        )));
    }

    #[tokio::test]
    async fn async_task_manager_background_round_requires_explicit_continue() {
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let entered = StdArc::new(AtomicBool::new(false));
        let release = StdArc::new(Notify::new());
        let task_manager = AsyncTaskManager::new().routing(NameRoutingPolicy::new([(
            "background-wait",
            RoutingDecision::Background,
        )]));
        let handle = task_manager.handle();
        let tools = ToolRegistry::new().with(BlockingTool::new(
            "background-wait",
            entered.clone(),
            release.clone(),
            "background-done",
        ));
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(AllowAllPermissions)
            .task_manager(task_manager)
            .observer(RecordingObserver {
                events: events.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-background"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "ping".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }])
            .unwrap();

        let first = driver.next().await.unwrap();
        match first {
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {}
            other => panic!("unexpected first loop step: {other:?}"),
        }

        match wait_for_task_event(&handle).await {
            TaskEvent::Started(snapshot) => assert_eq!(snapshot.tool_name, "background-wait"),
            other => panic!("unexpected task event: {other:?}"),
        }
        wait_until_entered(entered.as_ref()).await;
        release.notify_waiters();

        match wait_for_task_event(&handle).await {
            TaskEvent::Completed(_, result) => {
                assert_eq!(result.output, ToolOutput::Text("background-done".into()))
            }
            other => panic!("unexpected completion event: {other:?}"),
        }

        let resumed = driver.next().await.unwrap();
        match resumed {
            LoopStep::Finished(turn) => {
                assert_eq!(turn.finish_reason, FinishReason::Completed);
                match &turn.items[0].parts[0] {
                    Part::Text(text) => assert_eq!(text.text, "tool said: background-done"),
                    other => panic!("unexpected part after resume: {other:?}"),
                }
            }
            other => panic!("unexpected resumed step: {other:?}"),
        }

        let events = events.lock().unwrap();
        let terminal_results: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolResultReceived(result)
                    if result.call_id == ToolCallId::new("call-1") =>
                {
                    Some(result)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            terminal_results.len(),
            1,
            "background completion must emit one terminal result event per call: {events:?}"
        );
    }

    #[tokio::test]
    async fn detached_tool_placeholder_is_progress_not_terminal_result() {
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let entered = StdArc::new(AtomicBool::new(false));
        let release = StdArc::new(Notify::new());
        let task_manager = AsyncTaskManager::new().routing(NameRoutingPolicy::new([(
            "detaching-wait",
            RoutingDecision::ForegroundThenDetachAfter(Duration::from_millis(10)),
        )]));
        let handle = task_manager.handle();
        let tools = ToolRegistry::new().with(BlockingTool::new(
            "detaching-wait",
            entered.clone(),
            release.clone(),
            "detached-done",
        ));
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(AllowAllPermissions)
            .task_manager(task_manager)
            .observer(RecordingObserver {
                events: events.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-detached-progress"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item::text(ItemKind::User, "ping")])
            .unwrap();

        match driver.next().await.unwrap() {
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {}
            other => panic!("unexpected detach step: {other:?}"),
        }

        match wait_for_task_event(&handle).await {
            TaskEvent::Started(snapshot) => assert_eq!(snapshot.tool_name, "detaching-wait"),
            other => panic!("unexpected task event: {other:?}"),
        }
        match wait_for_task_event(&handle).await {
            TaskEvent::Detached(snapshot) => assert_eq!(snapshot.tool_name, "detaching-wait"),
            other => panic!("unexpected detach event: {other:?}"),
        }
        wait_until_entered(entered.as_ref()).await;
        release.notify_waiters();

        match wait_for_task_event(&handle).await {
            TaskEvent::Completed(_, result) => {
                assert_eq!(result.output, ToolOutput::Text("detached-done".into()))
            }
            other => panic!("unexpected completion event: {other:?}"),
        }

        match driver.next().await.unwrap() {
            LoopStep::Finished(turn) => assert_eq!(turn.finish_reason, FinishReason::Completed),
            other => panic!("unexpected resumed step: {other:?}"),
        }

        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolExecutionProgress(result)
                if result.call_id == ToolCallId::new("call-1") && !result.is_error
        )));
        let terminal_results: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolResultReceived(result)
                    if result.call_id == ToolCallId::new("call-1") =>
                {
                    Some(result)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            terminal_results.len(),
            1,
            "detached call must emit one terminal result event: {events:?}"
        );
    }

    #[tokio::test]
    async fn cancelled_background_approval_auto_resolves_when_drained() {
        let controller = CancellationController::new();
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let entered = StdArc::new(AtomicBool::new(false));
        let release = StdArc::new(Notify::new());
        let task_manager = AsyncTaskManager::new().routing(NameRoutingPolicy::new([(
            "echo",
            RoutingDecision::Background,
        )]));
        let handle = task_manager.handle();
        let agent = Agent::builder()
            .model(FakeAdapter)
            .tool_executor(DelayedApprovalExecutor::new(
                entered.clone(),
                release.clone(),
            ))
            .task_manager(task_manager)
            .cancellation(controller.handle())
            .observer(RecordingObserver {
                events: events.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-cancel-delayed-background-approval"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item::text(ItemKind::User, "ping")])
            .unwrap();

        match driver.next().await.unwrap() {
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {}
            other => panic!("unexpected first step: {other:?}"),
        }

        match wait_for_task_event(&handle).await {
            TaskEvent::Started(snapshot) => assert_eq!(snapshot.tool_name, "echo"),
            other => panic!("unexpected task event: {other:?}"),
        }

        wait_until_entered(entered.as_ref()).await;
        controller.interrupt();
        release.notify_waiters();
        wait_until_completed(&handle).await;

        match driver.next().await.unwrap() {
            LoopStep::Finished(turn) => assert_eq!(turn.finish_reason, FinishReason::Cancelled),
            other => panic!("cancelled background approval should finish cancelled, got {other:?}"),
        }

        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ApprovalResolved { approved: false }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolResultReceived(result)
                if result.call_id == ToolCallId::new("call-1") && result.is_error
        )));
    }

    #[tokio::test]
    async fn loop_can_cancel_a_turn_and_continue_after_new_input() {
        let controller = CancellationController::new();
        let agent = Agent::builder()
            .model(SlowAdapter)
            .cancellation(controller.handle())
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-cancel"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "do the long task".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }])
            .unwrap();

        let cancelled = tokio::join!(async { driver.next().await }, async {
            tokio::task::yield_now().await;
            controller.interrupt();
        })
        .0
        .unwrap();

        match cancelled {
            LoopStep::Finished(turn) => {
                assert_eq!(turn.finish_reason, FinishReason::Cancelled);
                assert_eq!(turn.items.len(), 1);
                assert_eq!(turn.items[0].kind, ItemKind::Assistant);
                assert_eq!(
                    turn.items[0].metadata.get(INTERRUPTED_METADATA_KEY),
                    Some(&Value::Bool(true))
                );
            }
            other => panic!("unexpected loop step: {other:?}"),
        }

        driver
            .submit_input(vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "try again".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }])
            .unwrap();

        let result = driver.next().await.unwrap();
        match result {
            LoopStep::Finished(turn) => {
                assert_eq!(turn.finish_reason, FinishReason::Completed);
            }
            other => panic!("unexpected loop step after retry: {other:?}"),
        }
    }

    #[tokio::test]
    async fn loop_interrupt_cancels_foreground_tasks_but_keeps_background_tasks_running() {
        let controller = CancellationController::new();
        let fg_entered = StdArc::new(AtomicBool::new(false));
        let fg_release = StdArc::new(Notify::new());
        let bg_entered = StdArc::new(AtomicBool::new(false));
        let bg_release = StdArc::new(Notify::new());
        let task_manager = AsyncTaskManager::new().routing(NameRoutingPolicy::new([
            ("foreground-wait", RoutingDecision::Foreground),
            ("background-wait", RoutingDecision::Background),
        ]));
        let handle = task_manager.handle();
        let tools = ToolRegistry::new()
            .with(BlockingTool::new(
                "foreground-wait",
                fg_entered.clone(),
                fg_release,
                "foreground-done",
            ))
            .with(BlockingTool::new(
                "background-wait",
                bg_entered.clone(),
                bg_release.clone(),
                "background-done",
            ));
        let agent = Agent::builder()
            .model(MultiToolAdapter)
            .add_tool_source(tools)
            .permissions(AllowAllPermissions)
            .cancellation(controller.handle())
            .task_manager(task_manager)
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-mixed-cancel"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "run both".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }])
            .unwrap();

        let cancelled = tokio::join!(async { driver.next().await }, async {
            let _ = wait_for_task_event(&handle).await;
            let _ = wait_for_task_event(&handle).await;
            wait_until_entered(fg_entered.as_ref()).await;
            wait_until_entered(bg_entered.as_ref()).await;
            controller.interrupt();
        })
        .0
        .unwrap();

        match cancelled {
            LoopStep::Finished(turn) => assert_eq!(turn.finish_reason, FinishReason::Cancelled),
            other => panic!("unexpected loop step after interrupt: {other:?}"),
        }

        match wait_for_task_event(&handle).await {
            TaskEvent::Cancelled(snapshot) => assert_eq!(snapshot.tool_name, "foreground-wait"),
            other => panic!("unexpected post-interrupt event: {other:?}"),
        }

        let running = handle.list_running().await;
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].tool_name, "background-wait");

        bg_release.notify_waiters();
        match wait_for_task_event(&handle).await {
            TaskEvent::Completed(snapshot, result) => {
                assert_eq!(snapshot.tool_name, "background-wait");
                assert_eq!(result.output, ToolOutput::Text("background-done".into()));
            }
            other => panic!("unexpected background completion event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn loop_resumes_after_approved_tool_request() {
        let tools = ToolRegistry::new().with(EchoTool::default());
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(ApproveFsReads)
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-approval"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "ping".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }])
            .unwrap();

        let first = driver.next().await.unwrap();
        match first {
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                assert!(pending.request.task_id.is_some());
                assert_eq!(pending.request.id.0, "approval:fs-read");
                pending.approve(&mut driver).unwrap();
            }
            other => panic!("unexpected loop step: {other:?}"),
        }
        let second = driver.next().await.unwrap();
        match second {
            LoopStep::Finished(turn) => match &turn.items[0].parts[0] {
                Part::Text(text) => assert_eq!(text.text, "tool said: pong"),
                other => panic!("unexpected part: {other:?}"),
            },
            other => panic!("unexpected loop step after approval: {other:?}"),
        }
    }

    #[tokio::test]
    async fn approval_gated_tool_does_not_start_before_approval() {
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let tools = ToolRegistry::new().with(EchoTool::default());
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(ApproveFsReads)
            .observer(RecordingObserver {
                events: events.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-approval-start-event"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item::text(ItemKind::User, "ping")])
            .unwrap();

        let pending = match driver.next().await.unwrap() {
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => pending,
            other => panic!("unexpected loop step: {other:?}"),
        };

        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .all(|event| !matches!(event, AgentEvent::ToolExecutionStarted(_))),
            "tool start must not be reported before approval"
        );

        pending.approve(&mut driver).unwrap();
        match driver.next().await.unwrap() {
            LoopStep::Finished(_) => {}
            other => panic!("unexpected loop step after approval: {other:?}"),
        }

        let started = events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, AgentEvent::ToolExecutionStarted(_)))
            .count();
        assert_eq!(started, 1);
    }

    #[tokio::test]
    async fn cancelling_pending_approval_resolves_it_and_pairs_tool_result() {
        let controller = CancellationController::new();
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let tools = ToolRegistry::new().with(EchoTool::default());
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(ApproveFsReads)
            .cancellation(controller.handle())
            .observer(RecordingObserver {
                events: events.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-cancel-pending-approval"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item::text(ItemKind::User, "ping")])
            .unwrap();

        match driver.next().await.unwrap() {
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(_)) => {}
            other => panic!("unexpected loop step: {other:?}"),
        }

        controller.interrupt();

        match driver.next().await.unwrap() {
            LoopStep::Finished(turn) => {
                assert_eq!(turn.finish_reason, FinishReason::Cancelled);
            }
            other => panic!("unexpected loop step after cancel: {other:?}"),
        }

        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::ApprovalResolved { approved: false })),
            "pending approval cancellation should close approval UI state"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                AgentEvent::ToolResultReceived(result)
                    if result.call_id == ToolCallId::new("call-1") && result.is_error
            )),
            "pending approval cancellation should pair the assistant tool_use"
        );
        drop(events);

        validate_transcript_invariants(&driver.snapshot().transcript).unwrap();
    }

    #[tokio::test]
    async fn resolved_approval_runs_even_if_cancellation_also_fired() {
        let controller = CancellationController::new();
        let tools = ToolRegistry::new().with(EchoTool::default());
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(ApproveFsReads)
            .cancellation(controller.handle())
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-resolved-approval-cancel-race"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item::text(ItemKind::User, "ping")])
            .unwrap();

        let pending = match driver.next().await.unwrap() {
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => pending,
            other => panic!("unexpected loop step: {other:?}"),
        };

        controller.interrupt();
        pending.approve(&mut driver).unwrap();

        match driver.next().await.unwrap() {
            LoopStep::Finished(turn) => {
                assert_eq!(turn.finish_reason, FinishReason::Completed);
                match &turn.items[0].parts[0] {
                    Part::Text(text) => assert_eq!(text.text, "tool said: pong"),
                    other => panic!("unexpected part after approval: {other:?}"),
                }
            }
            other => panic!("unexpected loop step after approved cancel race: {other:?}"),
        }
    }

    #[tokio::test]
    async fn loop_resumes_with_patched_input_on_approval() {
        let tools = ToolRegistry::new().with(EchoTool::default());
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(ApproveFsReads)
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-approval-patched"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "ping".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }])
            .unwrap();

        match driver.next().await.unwrap() {
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                pending
                    .approve_with_patched_input(&mut driver, json!({ "value": "patched" }))
                    .unwrap();
            }
            other => panic!("unexpected loop step: {other:?}"),
        }
        match driver.next().await.unwrap() {
            LoopStep::Finished(turn) => match &turn.items[0].parts[0] {
                Part::Text(text) => assert_eq!(text.text, "tool said: patched"),
                other => panic!("unexpected part: {other:?}"),
            },
            other => panic!("unexpected loop step after approval: {other:?}"),
        }
    }

    #[tokio::test]
    async fn loop_tracks_multiple_pending_approvals_by_call_id() {
        let tools = ToolRegistry::new().with(EchoTool::default());
        let agent = Agent::builder()
            .model(DualApprovalAdapter)
            .add_tool_source(tools)
            .permissions(ApproveFsReads)
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-dual-approval"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "run both approvals".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }])
            .unwrap();

        let pending_first = match driver.next().await.unwrap() {
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                assert_eq!(
                    pending.request.call_id.as_ref().map(|id| id.0.as_str()),
                    Some("call-1")
                );
                pending
            }
            other => panic!("unexpected first loop step: {other:?}"),
        };

        let pending_second = match driver.next().await.unwrap() {
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                assert_eq!(
                    pending.request.call_id.as_ref().map(|id| id.0.as_str()),
                    Some("call-2")
                );
                pending
            }
            other => panic!("unexpected second loop step: {other:?}"),
        };

        pending_second.approve(&mut driver).unwrap();
        match driver.next().await.unwrap() {
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                assert_eq!(
                    pending.request.call_id.as_ref().map(|id| id.0.as_str()),
                    Some("call-1")
                );
            }
            other => panic!("unexpected step after approving second request: {other:?}"),
        }

        pending_first.approve(&mut driver).unwrap();
        match driver.next().await.unwrap() {
            LoopStep::Finished(turn) => {
                assert_eq!(turn.finish_reason, FinishReason::Completed);
                match &turn.items[0].parts[0] {
                    Part::Text(text) => assert_eq!(text.text, "both approvals finished"),
                    other => panic!("unexpected final part: {other:?}"),
                }
            }
            other => panic!("unexpected final loop step: {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancelling_all_pending_approvals_pairs_every_tool_use() {
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let tools = ToolRegistry::new().with(EchoTool::default());
        let agent = Agent::builder()
            .model(DualApprovalAdapter)
            .add_tool_source(tools)
            .permissions(ApproveFsReads)
            .observer(RecordingObserver {
                events: events.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-dual-approval-cancel"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "run both approvals".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }])
            .unwrap();

        for expected_call in ["call-1", "call-2"] {
            match driver.next().await.unwrap() {
                LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                    assert_eq!(
                        pending.request.call_id.as_ref().map(|id| id.0.as_str()),
                        Some(expected_call)
                    );
                }
                other => panic!("unexpected approval step: {other:?}"),
            }
        }

        match driver.cancel_pending_approvals().await.unwrap() {
            Some(LoopStep::Finished(turn)) => {
                assert_eq!(turn.finish_reason, FinishReason::Cancelled);
            }
            other => panic!("unexpected cancellation result: {other:?}"),
        }
        validate_transcript_invariants(&driver.snapshot().transcript).unwrap();

        let events = events.lock().unwrap();
        let cancelled = events
            .iter()
            .filter(|event| matches!(event, AgentEvent::ApprovalResolved { approved: false }))
            .count();
        assert_eq!(cancelled, 2);
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::TurnFinished(turn) if turn.finish_reason == FinishReason::Cancelled
        )));
        for expected_call in ["call-1", "call-2"] {
            assert!(events.iter().any(|event| matches!(
                event,
                AgentEvent::ToolResultReceived(result)
                    if result.call_id == ToolCallId::new(expected_call) && result.is_error
            )));
        }
    }

    #[tokio::test]
    async fn loop_compacts_transcript_before_new_turns() {
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let agent = Agent::builder()
            .model(FakeAdapter)
            .mutator(KeepRecentMutator { keep: 1 })
            .observer(RecordingObserver {
                events: events.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-4"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        for text in ["first", "second"] {
            driver
                .submit_input(vec![Item {
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
                }])
                .unwrap();
            let _ = driver.next().await.unwrap();
        }

        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AgentEvent::MutationFinished { dirty: true, .. }))
        );
    }

    #[test]
    fn transcript_validation_rejects_orphaned_tool_result() {
        let transcript = vec![Item {
            id: None,
            kind: ItemKind::Tool,
            parts: vec![Part::ToolResult(ToolResultPart {
                call_id: "call-1".into(),
                output: ToolOutput::Text("result".into()),
                is_error: false,
                metadata: MetadataMap::new(),
            })],
            metadata: MetadataMap::new(),
            usage: None,
            finish_reason: None,
            created_at: None,
        }];

        let error = validate_transcript_invariants(&transcript).unwrap_err();
        assert!(error.to_string().contains("orphaned tool_result"));
    }

    #[test]
    fn transcript_validation_rejects_duplicate_tool_result() {
        let transcript = vec![
            Item {
                id: None,
                kind: ItemKind::Assistant,
                parts: vec![Part::ToolCall(ToolCallPart {
                    id: "call-1".into(),
                    name: "lookup".into(),
                    input: serde_json::json!({}),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            },
            Item {
                id: None,
                kind: ItemKind::Tool,
                parts: vec![Part::ToolResult(ToolResultPart {
                    call_id: "call-1".into(),
                    output: ToolOutput::Text("result".into()),
                    is_error: false,
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            },
            Item {
                id: None,
                kind: ItemKind::Tool,
                parts: vec![Part::ToolResult(ToolResultPart {
                    call_id: "call-1".into(),
                    output: ToolOutput::Text("again".into()),
                    is_error: false,
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            },
        ];

        let error = validate_transcript_invariants(&transcript).unwrap_err();
        assert!(error.to_string().contains("duplicate tool_result"));
    }

    #[tokio::test]
    async fn loop_refreshes_tool_specs_each_turn() {
        let seen_descriptions = StdArc::new(StdMutex::new(Vec::new()));
        let version = StdArc::new(AtomicUsize::new(1));
        let tools = ToolRegistry::new().with(DynamicSpecTool::new(version.clone()));
        let agent = Agent::builder()
            .model(RecordingAdapter {
                seen_descriptions: seen_descriptions.clone(),
                seen_caches: StdArc::new(StdMutex::new(Vec::new())),
            })
            .add_tool_source(tools)
            .permissions(AllowAllPermissions)
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-dynamic-tools"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        for text in ["first", "second"] {
            driver
                .submit_input(vec![Item {
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
                }])
                .unwrap();

            let _ = driver.next().await.unwrap();
            if text == "first" {
                version.store(2, Ordering::SeqCst);
            }
        }

        let seen_descriptions = seen_descriptions.lock().unwrap();
        assert_eq!(seen_descriptions.len(), 2);
        assert_eq!(seen_descriptions[0], vec!["dynamic version 1".to_string()]);
        assert_eq!(seen_descriptions[1], vec!["dynamic version 2".to_string()]);
    }

    #[tokio::test]
    async fn loop_emits_catalog_change_and_uses_updated_specs_next_turn() {
        let seen_descriptions = StdArc::new(StdMutex::new(Vec::new()));
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let executor = StdArc::new(CatalogExecutor::new());
        let executor_for_agent: Arc<dyn ToolExecutor> = executor.clone();
        let agent = Agent::builder()
            .model(RecordingAdapter {
                seen_descriptions: seen_descriptions.clone(),
                seen_caches: StdArc::new(StdMutex::new(Vec::new())),
            })
            .tool_executor(executor_for_agent)
            .permissions(AllowAllPermissions)
            .observer(RecordingObserver {
                events: events.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-catalog-events"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item::text(ItemKind::User, "first")])
            .unwrap();
        let _ = driver.next().await.unwrap();

        executor.publish_change(
            1,
            ToolCatalogEvent {
                source: "mcp:mock".into(),
                added: vec!["dynamic".into()],
                removed: Vec::new(),
                changed: Vec::new(),
            },
        );

        driver
            .submit_input(vec![Item::text(ItemKind::User, "second")])
            .unwrap();
        let _ = driver.next().await.unwrap();

        let seen_descriptions = seen_descriptions.lock().unwrap();
        assert_eq!(seen_descriptions.len(), 2);
        assert_eq!(seen_descriptions[0], vec!["dynamic version 0".to_string()]);
        assert_eq!(seen_descriptions[1], vec!["dynamic version 1".to_string()]);

        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ToolCatalogChanged(ToolCatalogEvent {
                source,
                added,
                removed,
                changed,
            }) if source == "mcp:mock"
                && added == &vec!["dynamic".to_string()]
                && removed.is_empty()
                && changed.is_empty()
        )));
    }

    #[tokio::test]
    async fn loop_passes_session_default_and_next_turn_cache_requests() {
        let seen_caches = StdArc::new(StdMutex::new(Vec::new()));
        let agent = Agent::builder()
            .model(RecordingAdapter {
                seen_descriptions: StdArc::new(StdMutex::new(Vec::new())),
                seen_caches: seen_caches.clone(),
            })
            .permissions(AllowAllPermissions)
            .build()
            .unwrap();

        let default_cache = PromptCacheRequest::best_effort(PromptCacheStrategy::Automatic)
            .with_retention(PromptCacheRetention::Short);
        let override_cache = PromptCacheRequest::required(PromptCacheStrategy::Explicit {
            breakpoints: vec![PromptCacheBreakpoint::TranscriptItemEnd { index: 0 }],
        });

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("session-cache"),
                metadata: MetadataMap::new(),
                cache: Some(default_cache.clone()),
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "first".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }])
            .unwrap();
        let _ = driver.next().await.unwrap();

        driver
            .submit_input_with_cache(
                vec![Item {
                    id: None,
                    kind: ItemKind::User,
                    parts: vec![Part::Text(TextPart {
                        text: "second".into(),
                        metadata: MetadataMap::new(),
                    })],
                    metadata: MetadataMap::new(),
                    usage: None,
                    finish_reason: None,
                    created_at: None,
                }],
                override_cache.clone(),
            )
            .unwrap();
        let _ = driver.next().await.unwrap();

        let seen = seen_caches.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0], Some(default_cache));
        assert_eq!(seen[1], Some(override_cache));
    }

    #[tokio::test]
    async fn loop_yields_after_tool_result_between_rounds() {
        let tools = ToolRegistry::new().with(EchoTool::default());
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(tools)
            .permissions(AllowAllPermissions)
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("yield-session"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item::text(ItemKind::User, "ping")])
            .unwrap();

        // First next() runs the model turn, resolves the tool call, and
        // yields AfterToolResult before calling the model again.
        let step = driver.next().await.unwrap();
        let info = match step {
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(info)) => info,
            other => panic!("expected AfterToolResult, got {other:?}"),
        };
        assert_eq!(info.session_id, SessionId::new("yield-session"));
        // Transcript at yield: [User, Assistant(tool_call), Tool(result)]
        assert_eq!(info.transcript_len, 3);

        // The yield is cooperative, not blocking.
        let interrupt = LoopInterrupt::AfterToolResult(info.clone());
        assert!(!interrupt.is_blocking());

        // Host interjects a message mid-turn.
        driver
            .submit_input(vec![Item::text(ItemKind::User, "also: report back")])
            .unwrap();

        // Second next() resumes the turn into the next model call, which
        // sees the tool result (and the injected user message) and finishes.
        let step = driver.next().await.unwrap();
        match step {
            LoopStep::Finished(turn) => {
                assert_eq!(turn.finish_reason, FinishReason::Completed);
            }
            other => panic!("expected Finished, got {other:?}"),
        }

        // Transcript must now include the injected user message.
        let snapshot = driver.snapshot();
        let has_injected_message = snapshot.transcript.iter().any(|item| {
            item.kind == ItemKind::User
                && item.parts.iter().any(|part| match part {
                    Part::Text(text) => text.text == "also: report back",
                    _ => false,
                })
        });
        assert!(
            has_injected_message,
            "injected user message should be in transcript, got: {:?}",
            snapshot.transcript
        );
    }

    struct RecordingTranscriptObserver {
        items: StdArc<StdMutex<Vec<Item>>>,
    }

    impl TranscriptObserver for RecordingTranscriptObserver {
        fn on_transcript_event(&self, event: TranscriptEvent<'_>) {
            self.items.lock().unwrap().push(event.item.clone());
        }
    }

    #[tokio::test]
    async fn observers_see_full_tool_round() {
        // A turn with one tool call exercises every interesting path:
        //   user input drained -> model output_items (assistant w/ tool call)
        //   -> tool result Item -> next model output_items (assistant text)
        // The LoopObserver should see exactly one ToolResultReceived; the
        // TranscriptObserver should see all four items in transcript order.
        let events = StdArc::new(StdMutex::new(Vec::<AgentEvent>::new()));
        let items = StdArc::new(StdMutex::new(Vec::<Item>::new()));
        let agent = Agent::builder()
            .model(FakeAdapter)
            .add_tool_source(ToolRegistry::new().with(EchoTool::default()))
            .permissions(AllowAllPermissions)
            .observer(RecordingObserver {
                events: events.clone(),
            })
            .transcript_observer(RecordingTranscriptObserver {
                items: items.clone(),
            })
            .build()
            .unwrap();

        let mut driver = agent
            .start(SessionConfig {
                session_id: SessionId::new("observer-session"),
                metadata: MetadataMap::new(),
                cache: None,
                structured_output: None,
            })
            .await
            .unwrap();

        driver
            .submit_input(vec![Item {
                id: None,
                kind: ItemKind::User,
                parts: vec![Part::Text(TextPart {
                    text: "ping".into(),
                    metadata: MetadataMap::new(),
                })],
                metadata: MetadataMap::new(),
                usage: None,
                finish_reason: None,
                created_at: None,
            }])
            .unwrap();

        let result = run_until_finished(&mut driver).await;
        assert!(matches!(result, LoopStep::Finished(_)), "got {result:?}");

        // LoopObserver: exactly one ToolResultReceived, with the echo
        // tool's output, correlating back to the model's tool call.
        let events = events.lock().unwrap().clone();
        let tool_call_id = events.iter().find_map(|e| match e {
            AgentEvent::ToolCallRequested(c) => Some(c.id.clone()),
            _ => None,
        });
        let tool_results: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::ToolResultReceived(r) => Some(r.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_results.len(), 1, "events: {events:?}");
        assert_eq!(Some(tool_results[0].call_id.clone()), tool_call_id);
        assert!(!tool_results[0].is_error);

        // TranscriptObserver: every transcript mutation surfaces.
        // Expected order: User("ping"), Assistant(tool call), Tool(result),
        // Assistant("tool said: pong").
        let items = items.lock().unwrap().clone();
        assert_eq!(items.len(), 4, "items: {items:?}");
        assert_eq!(items[0].kind, ItemKind::User);
        assert_eq!(items[1].kind, ItemKind::Assistant);
        assert!(
            items[1]
                .parts
                .iter()
                .any(|p| matches!(p, Part::ToolCall(_)))
        );
        assert_eq!(items[2].kind, ItemKind::Tool);
        assert!(
            items[2]
                .parts
                .iter()
                .any(|p| matches!(p, Part::ToolResult(_)))
        );
        assert_eq!(items[3].kind, ItemKind::Assistant);
    }

    #[test]
    fn convenience_cache_builders_construct_expected_defaults() {
        let cache = PromptCacheRequest::automatic()
            .with_retention(PromptCacheRetention::Short)
            .with_key("workspace:demo");
        let session = SessionConfig::new("demo").with_cache(cache.clone());

        assert_eq!(session.session_id, SessionId::new("demo"));
        assert_eq!(session.cache, Some(cache));

        let explicit = PromptCacheRequest::explicit([
            PromptCacheBreakpoint::tools_end(),
            PromptCacheBreakpoint::transcript_item_end(2),
            PromptCacheBreakpoint::transcript_part_end(3, 1),
        ]);

        assert_eq!(explicit.mode, PromptCacheMode::BestEffort);
        assert_eq!(
            explicit.strategy,
            PromptCacheStrategy::Explicit {
                breakpoints: vec![
                    PromptCacheBreakpoint::ToolsEnd,
                    PromptCacheBreakpoint::TranscriptItemEnd { index: 2 },
                    PromptCacheBreakpoint::TranscriptPartEnd {
                        item_index: 3,
                        part_index: 1,
                    },
                ],
            }
        );
    }

    #[test]
    fn structured_output_contract_binds_schema_digest_and_session_default() {
        let request =
            StructuredOutputRequest::new("edit", 1, true, serde_json::json!({"type": "object"}))
                .unwrap();
        assert_eq!(request.name(), "edit");
        assert!(request.schema_digest().starts_with("sha256:"));
        assert_eq!(request.max_output_bytes(), 64 * 1024 * 1024);
        assert_eq!(
            SessionConfig::new("session")
                .with_structured_output(request.clone())
                .structured_output,
            Some(request.clone())
        );

        let mut wire = serde_json::to_value(request).unwrap();
        wire["schema_digest"] = Value::String(format!("sha256:{}", "0".repeat(64)));
        assert!(serde_json::from_value::<StructuredOutputRequest>(wire).is_err());

        let request =
            StructuredOutputRequest::new("edit", 1, true, serde_json::json!({"type": "object"}))
                .unwrap()
                .with_max_output_bytes(1)
                .unwrap();
        assert_eq!(request.max_output_bytes(), 1);
    }
}
