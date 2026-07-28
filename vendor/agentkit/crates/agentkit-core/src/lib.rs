//! Core transcript, content, usage, and cancellation primitives for agentkit.
//!
//! This crate provides the foundational data model shared by every crate in
//! the agentkit workspace. It defines:
//!
//! - **Transcript items** ([`Item`], [`ItemKind`]) -- the messages exchanged
//!   between system, user, assistant, and tools.
//! - **Content parts** ([`Part`], [`TextPart`], [`ToolCallPart`], etc.) --
//!   the multimodal pieces that make up each item.
//! - **Streaming deltas** ([`Delta`]) -- incremental updates emitted while a
//!   model turn is in progress.
//! - **Usage tracking** ([`Usage`], [`TokenUsage`], [`CostUsage`]) -- token
//!   counts and cost accounting reported by providers.
//! - **Cancellation** ([`CancellationController`], [`CancellationHandle`],
//!   [`TurnCancellation`]) -- cooperative interruption of running turns.
//! - **Typed identifiers** ([`SessionId`], [`TurnId`], [`ToolCallId`], etc.)
//!   -- lightweight newtypes that prevent accidental ID mix-ups.
//! - **Error types** ([`NormalizeError`], [`ProtocolError`], [`AgentError`])
//!   -- shared error variants used across the workspace.
//!
//! # Example
//!
//! ```rust
//! use agentkit_core::{Item, ItemKind};
//!
//! // Build a minimal transcript to feed into the agent loop.
//! let transcript = vec![
//!     Item::text(ItemKind::System, "You are a coding assistant."),
//!     Item::text(ItemKind::User, "What files are in this repo?"),
//! ];
//!
//! assert_eq!(transcript[0].kind, ItemKind::System);
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_timer::Delay;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

macro_rules! id_newtype {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(
            Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        pub struct $name(pub String);

        impl $name {
            /// Creates a new identifier from any value that can be converted into a [`String`].
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

id_newtype!(
    /// Identifies an agent session.
    ///
    /// A session groups one or more turns that share the same model connection
    /// and transcript history. Pass this to [`agentkit_loop`] when starting a
    /// new session.
    ///
    /// # Example
    ///
    /// ```rust
    /// use agentkit_core::SessionId;
    ///
    /// let id = SessionId::new("coding-agent-1");
    /// assert_eq!(id.to_string(), "coding-agent-1");
    /// ```
    SessionId
);
id_newtype!(
    /// Identifies a single turn within a session.
    ///
    /// Each call to the model within a session gets a unique `TurnId`. This is
    /// used by compaction and reporting to attribute work to specific turns.
    TurnId
);
id_newtype!(
    /// Identifies a transcript [`Item`].
    ///
    /// Providers may assign message IDs to track items across API calls.
    /// This is optional -- locally constructed items typically leave it as `None`.
    MessageId
);
id_newtype!(
    /// Identifies a tool call emitted by the model.
    ///
    /// The model produces a [`ToolCallPart`] with this ID; the corresponding
    /// [`ToolResultPart`] references it via `call_id` so the model can match
    /// results back to requests.
    ToolCallId
);
id_newtype!(
    /// Identifies a tool result.
    ///
    /// Used by providers that assign their own IDs to tool-result payloads.
    ToolResultId
);
id_newtype!(
    /// Identifies a task tracked by a task manager.
    ///
    /// Unlike [`ToolCallId`], which is model/provider-facing, `TaskId` is a
    /// runtime-facing identifier used to inspect, cancel, or correlate work
    /// managed by a task scheduler.
    TaskId
);
id_newtype!(
    /// Provider-assigned identifier for a message.
    ///
    /// Some providers return an opaque ID for each completion response;
    /// this type preserves it for tracing and debugging.
    ProviderMessageId
);
id_newtype!(
    /// Identifies a binary or text artifact stored externally.
    ///
    /// Referenced by [`DataRef::Handle`] when content lives outside the
    /// transcript (e.g. in an artifact store).
    ArtifactId
);
id_newtype!(
    /// Identifies a content part within a streaming [`Delta`] sequence.
    ///
    /// Deltas reference parts by `PartId` so the consumer can reconstruct the
    /// full item as chunks arrive.
    PartId
);
id_newtype!(
    /// Identifies a pending tool-use approval request.
    ///
    /// When a tool requires human approval, the loop emits an approval request
    /// tagged with this ID. The caller responds with a decision keyed to the
    /// same ID.
    ApprovalId
);

/// A map of arbitrary key-value metadata attached to items, parts, and other structures.
///
/// Most agentkit types carry a `metadata` field of this type. Providers,
/// tools, and observers can store domain-specific information here without
/// changing the core schema.
///
/// # Example
///
/// ```rust
/// use agentkit_core::MetadataMap;
/// use serde_json::json;
///
/// let mut meta = MetadataMap::new();
/// meta.insert("source".into(), json!("user-clipboard"));
/// assert_eq!(meta["source"], "user-clipboard");
/// ```
pub type MetadataMap = BTreeMap<String, Value>;

#[derive(Default)]
struct CancellationState {
    generation: AtomicU64,
}

/// Owner-side handle for broadcasting cancellation to running turns.
///
/// Create one `CancellationController` per agent and hand out
/// [`CancellationHandle`]s to the loop and tool executors. Calling
/// [`interrupt`](Self::interrupt) bumps an internal generation counter so that
/// every outstanding [`TurnCancellation`] checkpoint becomes cancelled.
///
/// # Example
///
/// ```rust
/// use agentkit_core::CancellationController;
///
/// let controller = CancellationController::new();
/// let handle = controller.handle();
///
/// // Before an interrupt the generation is 0.
/// assert_eq!(handle.generation(), 0);
///
/// // Signal cancellation (e.g. from a Ctrl-C handler).
/// controller.interrupt();
/// assert_eq!(handle.generation(), 1);
/// ```
#[derive(Clone, Default)]
pub struct CancellationController {
    state: Arc<CancellationState>,
}

/// Read-only view of cancellation state, cheaply cloneable.
///
/// The loop and tool executors receive a `CancellationHandle` and use it to
/// create [`TurnCancellation`] checkpoints or poll for interrupts directly.
///
/// Obtain one from [`CancellationController::handle`].
#[derive(Clone, Default)]
pub struct CancellationHandle {
    state: Arc<CancellationState>,
}

/// A snapshot of the cancellation generation at the start of a turn.
///
/// Created via [`CancellationHandle::checkpoint`] or [`TurnCancellation::new`],
/// this value records the generation counter at creation time. If the counter
/// changes (because [`CancellationController::interrupt`] was called), the
/// checkpoint reports itself as cancelled.
///
/// The agent loop passes a `TurnCancellation` into model and tool calls so
/// they can bail out cooperatively.
///
/// # Example
///
/// ```rust
/// use agentkit_core::{CancellationController, TurnCancellation};
///
/// let controller = CancellationController::new();
/// let checkpoint = TurnCancellation::new(controller.handle());
///
/// assert!(!checkpoint.is_cancelled());
/// controller.interrupt();
/// assert!(checkpoint.is_cancelled());
/// ```
#[derive(Clone, Default)]
pub struct TurnCancellation {
    handle: CancellationHandle,
    generation: u64,
}

impl CancellationController {
    /// Creates a new controller with generation starting at 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a cloneable [`CancellationHandle`] that shares state with this controller.
    pub fn handle(&self) -> CancellationHandle {
        CancellationHandle {
            state: Arc::clone(&self.state),
        }
    }

    /// Broadcasts a cancellation by incrementing the generation counter.
    ///
    /// Returns the new generation value. All [`TurnCancellation`] checkpoints
    /// created before this call will report themselves as cancelled.
    pub fn interrupt(&self) -> u64 {
        self.state.generation.fetch_add(1, Ordering::SeqCst) + 1
    }
}

impl CancellationHandle {
    /// Returns the current generation counter.
    pub fn generation(&self) -> u64 {
        self.state.generation.load(Ordering::SeqCst)
    }

    /// Creates a [`TurnCancellation`] checkpoint capturing the current generation.
    pub fn checkpoint(&self) -> TurnCancellation {
        TurnCancellation {
            handle: self.clone(),
            generation: self.generation(),
        }
    }

    /// Returns `true` if the generation has changed since `generation`.
    ///
    /// # Arguments
    ///
    /// * `generation` - The generation value to compare against, typically
    ///   obtained from a prior call to [`generation`](Self::generation).
    pub fn is_cancelled_since(&self, generation: u64) -> bool {
        self.generation() != generation
    }

    /// Waits asynchronously until the generation changes from `generation`.
    ///
    /// Polls the generation counter every 10 ms. Prefer using
    /// [`TurnCancellation::cancelled`] instead, which captures the generation
    /// automatically.
    ///
    /// # Arguments
    ///
    /// * `generation` - The generation value to wait for a change from.
    pub async fn cancelled_since(&self, generation: u64) {
        while !self.is_cancelled_since(generation) {
            Delay::new(Duration::from_millis(10)).await;
        }
    }
}

impl TurnCancellation {
    /// Creates a checkpoint from the given handle, capturing its current generation.
    ///
    /// # Arguments
    ///
    /// * `handle` - The [`CancellationHandle`] to observe.
    pub fn new(handle: CancellationHandle) -> Self {
        handle.checkpoint()
    }

    /// Returns the generation that was captured when this checkpoint was created.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns `true` if the controller has been interrupted since this checkpoint was created.
    pub fn is_cancelled(&self) -> bool {
        self.handle.is_cancelled_since(self.generation)
    }

    /// Waits asynchronously until this checkpoint becomes cancelled.
    ///
    /// Useful in `tokio::select!` to race a model call against user interruption.
    pub async fn cancelled(&self) {
        self.handle.cancelled_since(self.generation).await;
    }

    /// Returns a reference to the underlying [`CancellationHandle`].
    pub fn handle(&self) -> &CancellationHandle {
        &self.handle
    }
}

/// A single entry in the agent transcript.
///
/// An `Item` represents one message or event in the conversation between
/// the system, user, assistant, and tools. Each item has a [`kind`](ItemKind)
/// that determines its role and a vector of [`Part`]s that carry its content.
///
/// Items are the primary unit of data flowing through the agentkit loop:
/// callers submit user and system items, the loop appends assistant and tool
/// items, and compaction strategies operate on the full `Vec<Item>` transcript.
///
/// # Example
///
/// ```rust
/// use agentkit_core::{Item, ItemKind};
///
/// let user_msg = Item::text(ItemKind::User, "List the workspace crates.");
///
/// assert_eq!(user_msg.kind, ItemKind::User);
/// assert_eq!(user_msg.parts.len(), 1);
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// Optional provider-assigned or user-supplied message identifier.
    pub id: Option<MessageId>,
    /// The role of this item in the transcript.
    pub kind: ItemKind,
    /// The content parts that make up this item.
    pub parts: Vec<Part>,
    /// Arbitrary key-value metadata for this item.
    pub metadata: MetadataMap,
    /// Token / cost usage produced by the model turn that emitted this item.
    /// Populated by the loop on assistant items when the provider reports it.
    pub usage: Option<Usage>,
    /// Why the model turn that produced this item ended. Populated by the
    /// loop on assistant items.
    pub finish_reason: Option<FinishReason>,
    /// When this item was appended to the transcript. Populated by the loop
    /// for every appended item.
    pub created_at: Option<Timestamp>,
}

impl Item {
    /// Builds an item with the given role and parts.
    pub fn new(kind: ItemKind, parts: Vec<Part>) -> Self {
        Self {
            id: None,
            kind,
            parts,
            metadata: MetadataMap::new(),
            usage: None,
            finish_reason: None,
            created_at: None,
        }
    }

    /// Builds a single-text-part item.
    pub fn text(kind: ItemKind, text: impl Into<String>) -> Self {
        Self::new(kind, vec![Part::Text(TextPart::new(text))])
    }

    /// Builds a [`ItemKind::Notification`] item carrying free-form text.
    /// Adapters wrap the content in `<system-reminder>` and deliver it as
    /// a user-role message so the model can react to the notification on
    /// its next turn without violating tool_use/tool_result pairing.
    pub fn notification(text: impl Into<String>) -> Self {
        Self::text(ItemKind::Notification, text)
    }

    /// Sets the item identifier.
    pub fn with_id(mut self, id: impl Into<MessageId>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Replaces the item metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }

    /// Appends one part to the item.
    pub fn push_part(mut self, part: Part) -> Self {
        self.parts.push(part);
        self
    }

    /// Sets the model usage that produced this item.
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }

    /// Sets the finish reason for the model turn that produced this item.
    pub fn with_finish_reason(mut self, reason: FinishReason) -> Self {
        self.finish_reason = Some(reason);
        self
    }

    /// Sets when this item was created.
    pub fn with_created_at(mut self, ts: Timestamp) -> Self {
        self.created_at = Some(ts);
        self
    }
}

/// A wall-clock instant carried on items, expressed as milliseconds since
/// the Unix epoch in UTC. Stamped by the loop when items land in the
/// transcript so consumers can sort, filter, or expire by age without
/// depending on a particular date-time crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(pub u64);

impl Timestamp {
    /// Captures the current wall-clock time. Returns `Timestamp(0)` on the
    /// vanishingly unlikely event the system clock is before the epoch.
    pub fn now() -> Self {
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self(millis)
    }
}

/// The role of an [`Item`] in the transcript.
///
/// Variants are ordered so that
/// `System < Developer < User < Assistant < Tool < Context < Notification`,
/// which is useful for sorting items by priority during compaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ItemKind {
    /// Instructions provided by the application author (highest priority).
    System,
    /// Developer-level instructions that sit between system and user.
    Developer,
    /// A message from the end user.
    User,
    /// A response generated by the model.
    Assistant,
    /// Output from a tool execution.
    Tool,
    /// Ambient context injected by context loaders (e.g. project files, docs).
    Context,
    /// Out-of-band side-channel signal injected mid-conversation:
    /// background-task completions, environment changes, system reminders.
    /// Adapters render it as a user-role message wrapped in
    /// `<system-reminder>` so the model interprets it as a notification
    /// rather than user input. Distinct from [`ItemKind::Context`] in two
    /// ways: (1) temporal placement is preserved (Anthropic adapter does
    /// NOT hoist it to the top-level `system` field), (2) UI hosts can
    /// filter or render notifications differently from user turns.
    ///
    /// Use this when a tool runs in the background and its result
    /// arrives after the original `tool_use` was already paired and
    /// closed — emitting another `tool_result` for the same call_id
    /// would violate the provider schema.
    Notification,
}

/// A content part within an [`Item`].
///
/// Items are composed of one or more parts, each carrying a different kind of
/// content -- plain text, images, files, tool calls, tool results, or
/// provider-specific custom payloads.
///
/// # Example
///
/// ```rust
/// use agentkit_core::{Part, ToolCallPart};
/// use serde_json::json;
///
/// let parts: Vec<Part> = vec![
///     Part::text("Reading the config file..."),
///     Part::ToolCall(ToolCallPart::new(
///         "call-42",
///         "fs_read_file",
///         json!({ "path": "config.toml" }),
///     )),
/// ];
///
/// assert!(matches!(parts[0], Part::Text(_)));
/// assert!(matches!(parts[1], Part::ToolCall(_)));
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Part {
    /// Plain text content.
    Text(TextPart),
    /// Binary or encoded media (images, audio, video).
    Media(MediaPart),
    /// A file attachment.
    File(FilePart),
    /// Structured JSON data, optionally validated against a schema.
    Structured(StructuredPart),
    /// Model reasoning / chain-of-thought output.
    Reasoning(ReasoningPart),
    /// A tool invocation request emitted by the model.
    ToolCall(ToolCallPart),
    /// The result returned by a tool after execution.
    ToolResult(ToolResultPart),
    /// A provider-specific part that does not fit the standard variants.
    Custom(CustomPart),
}

impl Part {
    /// Builds a text part.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextPart::new(text))
    }

    /// Builds a media part.
    pub fn media(modality: Modality, mime_type: impl Into<String>, data: DataRef) -> Self {
        Self::Media(MediaPart::new(modality, mime_type, data))
    }

    /// Builds a file part.
    pub fn file(data: DataRef) -> Self {
        Self::File(FilePart::new(data))
    }

    /// Builds a structured part.
    pub fn structured(value: Value) -> Self {
        Self::Structured(StructuredPart::new(value))
    }

    /// Builds a reasoning-summary part.
    pub fn reasoning(summary: impl Into<String>) -> Self {
        Self::Reasoning(ReasoningPart::summary(summary))
    }
}

/// Discriminant for [`Part`] variants, used in streaming [`Delta`]s.
///
/// When a [`Delta::BeginPart`] arrives the consumer uses the `PartKind` to
/// allocate the right buffer before subsequent append deltas arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartKind {
    /// Corresponds to [`Part::Text`].
    Text,
    /// Corresponds to [`Part::Media`].
    Media,
    /// Corresponds to [`Part::File`].
    File,
    /// Corresponds to [`Part::Structured`].
    Structured,
    /// Corresponds to [`Part::Reasoning`].
    Reasoning,
    /// Corresponds to [`Part::ToolCall`].
    ToolCall,
    /// Corresponds to [`Part::ToolResult`].
    ToolResult,
    /// Corresponds to [`Part::Custom`].
    Custom,
}

/// Plain text content within an [`Item`].
///
/// This is the most common part type: user messages, assistant replies, and
/// system prompts are all represented as `TextPart`s.
///
/// # Example
///
/// ```rust
/// use agentkit_core::TextPart;
///
/// let part = TextPart::new("Hello, world!");
/// assert_eq!(part.text, "Hello, world!");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextPart {
    /// The text content.
    pub text: String,
    /// Arbitrary key-value metadata.
    pub metadata: MetadataMap,
}

impl TextPart {
    /// Builds a text part with empty metadata.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            metadata: MetadataMap::new(),
        }
    }

    /// Replaces the text-part metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Binary or encoded media content (image, audio, video).
///
/// The actual bytes are referenced through a [`DataRef`] which can be inline,
/// a URI, or a handle to an external artifact store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaPart {
    /// The kind of media (audio, image, video, or raw binary).
    pub modality: Modality,
    /// MIME type of the media, e.g. `"image/png"` or `"audio/wav"`.
    pub mime_type: String,
    /// Reference to the media data.
    pub data: DataRef,
    /// Arbitrary key-value metadata.
    pub metadata: MetadataMap,
}

impl MediaPart {
    /// Builds a media part with empty metadata.
    pub fn new(modality: Modality, mime_type: impl Into<String>, data: DataRef) -> Self {
        Self {
            modality,
            mime_type: mime_type.into(),
            data,
            metadata: MetadataMap::new(),
        }
    }

    /// Replaces the media-part metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// The kind of media carried by a [`MediaPart`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Modality {
    /// Audio content (e.g. WAV, MP3).
    Audio,
    /// Image content (e.g. PNG, JPEG).
    Image,
    /// Video content (e.g. MP4).
    Video,
    /// Opaque binary data that does not fit another category.
    Binary,
}

/// A reference to content data that may live inline, at a URI, or in an artifact store.
///
/// Used by [`MediaPart`], [`FilePart`], [`ReasoningPart`], and [`CustomPart`]
/// to point at their underlying data without dictating storage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataRef {
    /// UTF-8 text stored inline (e.g. base64-encoded image data).
    InlineText(String),
    /// Raw bytes stored inline.
    InlineBytes(Vec<u8>),
    /// A URI pointing to externally hosted content.
    Uri(String),
    /// A handle to an artifact managed by an external store.
    Handle(ArtifactId),
}

impl DataRef {
    /// Stores UTF-8 text inline.
    pub fn inline_text(text: impl Into<String>) -> Self {
        Self::InlineText(text.into())
    }

    /// Stores bytes inline.
    pub fn inline_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::InlineBytes(bytes.into())
    }

    /// References externally hosted content by URI.
    pub fn uri(uri: impl Into<String>) -> Self {
        Self::Uri(uri.into())
    }

    /// References content through an artifact handle.
    pub fn handle(id: impl Into<ArtifactId>) -> Self {
        Self::Handle(id.into())
    }
}

/// A file attachment within an [`Item`].
///
/// Files are distinct from [`MediaPart`] in that they carry an optional
/// filename and are not necessarily displayable media.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePart {
    /// Optional human-readable filename (e.g. `"report.csv"`).
    pub name: Option<String>,
    /// Optional MIME type of the file.
    pub mime_type: Option<String>,
    /// Reference to the file data.
    pub data: DataRef,
    /// Arbitrary key-value metadata.
    pub metadata: MetadataMap,
}

impl FilePart {
    /// Builds an unnamed file part with empty metadata.
    pub fn new(data: DataRef) -> Self {
        Self {
            name: None,
            mime_type: None,
            data,
            metadata: MetadataMap::new(),
        }
    }

    /// Builds a named file part with empty metadata.
    pub fn named(name: impl Into<String>, data: DataRef) -> Self {
        Self::new(data).with_name(name)
    }

    /// Sets the file name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the file mime type.
    pub fn with_mime_type(mut self, mime_type: impl Into<String>) -> Self {
        self.mime_type = Some(mime_type.into());
        self
    }

    /// Replaces the file-part metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Structured JSON content, optionally paired with a JSON Schema for validation.
///
/// Providers that support structured output (e.g. function-calling mode) may
/// return a `StructuredPart` instead of free-form text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StructuredPart {
    /// The structured data as a JSON [`Value`].
    pub value: Value,
    /// An optional JSON Schema that `value` conforms to.
    pub schema: Option<Value>,
    /// Arbitrary key-value metadata.
    pub metadata: MetadataMap,
}

impl StructuredPart {
    /// Builds a structured part with empty metadata and no schema.
    pub fn new(value: Value) -> Self {
        Self {
            value,
            schema: None,
            metadata: MetadataMap::new(),
        }
    }

    /// Sets the optional schema.
    pub fn with_schema(mut self, schema: Value) -> Self {
        self.schema = Some(schema);
        self
    }

    /// Replaces the structured-part metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Model reasoning or chain-of-thought output.
///
/// Some providers expose the model's internal reasoning alongside the final
/// answer. The reasoning may be a readable summary, opaque data, or both.
/// The `redacted` flag indicates provider-side filtering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningPart {
    /// A human-readable summary of the model's reasoning.
    pub summary: Option<String>,
    /// Opaque or detailed reasoning data.
    pub data: Option<DataRef>,
    /// `true` if the provider redacted the full reasoning content.
    pub redacted: bool,
    /// Arbitrary key-value metadata.
    pub metadata: MetadataMap,
}

impl ReasoningPart {
    /// Builds a readable reasoning summary.
    pub fn summary(summary: impl Into<String>) -> Self {
        Self {
            summary: Some(summary.into()),
            data: None,
            redacted: false,
            metadata: MetadataMap::new(),
        }
    }

    /// Builds a redacted readable reasoning summary.
    pub fn redacted_summary(summary: impl Into<String>) -> Self {
        Self::summary(summary).with_redacted(true)
    }

    /// Sets the optional reasoning data reference.
    pub fn with_data(mut self, data: DataRef) -> Self {
        self.data = Some(data);
        self
    }

    /// Sets whether the reasoning content was redacted.
    pub fn with_redacted(mut self, redacted: bool) -> Self {
        self.redacted = redacted;
        self
    }

    /// Replaces the reasoning-part metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// A tool invocation request emitted by the model.
///
/// The agent loop receives this part, executes the named tool, and appends a
/// [`ToolResultPart`] back to the transcript for the model to observe.
///
/// # Example
///
/// ```rust
/// use agentkit_core::ToolCallPart;
/// use serde_json::json;
///
/// let call = ToolCallPart::new("call-7", "fs_read_file", json!({ "path": "src/main.rs" }));
///
/// assert_eq!(call.name, "fs_read_file");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallPart {
    /// Unique identifier for this tool call, used to correlate with [`ToolResultPart::call_id`].
    pub id: ToolCallId,
    /// The name of the tool to invoke (e.g. `"fs_read_file"`, `"shell_exec"`).
    pub name: String,
    /// The JSON arguments to pass to the tool.
    pub input: Value,
    /// Arbitrary key-value metadata.
    pub metadata: MetadataMap,
}

impl ToolCallPart {
    /// Builds a tool-call part with empty metadata.
    pub fn new(id: impl Into<ToolCallId>, name: impl Into<String>, input: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            input,
            metadata: MetadataMap::new(),
        }
    }

    /// Replaces the tool-call metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// The result of executing a tool, sent back to the model.
///
/// Each `ToolResultPart` references the [`ToolCallPart`] it answers via
/// `call_id`. The `is_error` flag tells the model whether the tool succeeded.
///
/// # Example
///
/// ```rust
/// use agentkit_core::{ToolOutput, ToolResultPart};
///
/// let result = ToolResultPart::success("call-7", ToolOutput::text("fn main() { ... }"));
///
/// assert!(!result.is_error);
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResultPart {
    /// The [`ToolCallId`] of the tool call this result answers.
    pub call_id: ToolCallId,
    /// The output produced by the tool.
    pub output: ToolOutput,
    /// `true` if the tool execution failed.
    pub is_error: bool,
    /// Arbitrary key-value metadata.
    pub metadata: MetadataMap,
}

impl ToolResultPart {
    /// Builds a successful tool-result part with empty metadata.
    pub fn success(call_id: impl Into<ToolCallId>, output: ToolOutput) -> Self {
        Self {
            call_id: call_id.into(),
            output,
            is_error: false,
            metadata: MetadataMap::new(),
        }
    }

    /// Builds an error tool-result part with empty metadata.
    pub fn error(call_id: impl Into<ToolCallId>, output: ToolOutput) -> Self {
        Self::success(call_id, output).with_is_error(true)
    }

    /// Sets the error flag explicitly.
    pub fn with_is_error(mut self, is_error: bool) -> Self {
        self.is_error = is_error;
        self
    }

    /// Replaces the tool-result metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// The payload returned by a tool execution.
///
/// Tools may return plain text, structured JSON, a composite list of
/// [`Part`]s, or a collection of files.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ToolOutput {
    /// Plain text output.
    Text(String),
    /// Structured JSON output.
    Structured(Value),
    /// A list of content parts (e.g. text + images).
    Parts(Vec<Part>),
    /// A list of file attachments.
    Files(Vec<FilePart>),
}

impl ToolOutput {
    /// Builds plain-text tool output.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    /// Builds structured tool output.
    pub fn structured(value: Value) -> Self {
        Self::Structured(value)
    }

    /// Builds multipart tool output.
    pub fn parts(parts: Vec<Part>) -> Self {
        Self::Parts(parts)
    }

    /// Builds file-based tool output.
    pub fn files(files: Vec<FilePart>) -> Self {
        Self::Files(files)
    }
}

/// A provider-specific content part that does not fit the standard variants.
///
/// Use this for extensions or experimental features that have not been
/// promoted to a first-class [`Part`] variant.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomPart {
    /// A free-form string identifying the custom part type.
    pub kind: String,
    /// Optional data reference.
    pub data: Option<DataRef>,
    /// Optional structured value.
    pub value: Option<Value>,
    /// Arbitrary key-value metadata.
    pub metadata: MetadataMap,
}

impl CustomPart {
    /// Builds a custom part with empty metadata.
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            data: None,
            value: None,
            metadata: MetadataMap::new(),
        }
    }

    /// Sets the custom part data reference.
    pub fn with_data(mut self, data: DataRef) -> Self {
        self.data = Some(data);
        self
    }

    /// Sets the custom part structured value.
    pub fn with_value(mut self, value: Value) -> Self {
        self.value = Some(value);
        self
    }

    /// Replaces the custom-part metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// An incremental update emitted while a model turn is streaming.
///
/// The provider adapter emits a sequence of `Delta` values that the loop and
/// reporters consume to reconstruct the full [`Item`] progressively. A typical
/// sequence looks like:
///
/// 1. [`BeginPart`](Self::BeginPart) -- allocates a new part buffer.
/// 2. One or more [`AppendText`](Self::AppendText) /
///    [`AppendBytes`](Self::AppendBytes) -- fills the buffer.
/// 3. [`CommitPart`](Self::CommitPart) -- finalises the part.
///
/// # Example
///
/// ```rust
/// use agentkit_core::{Delta, PartId, PartKind};
///
/// let deltas = vec![
///     Delta::BeginPart { part_id: PartId::new("p1"), kind: PartKind::Text },
///     Delta::AppendText { part_id: PartId::new("p1"), chunk: "Hello".into() },
///     Delta::AppendText { part_id: PartId::new("p1"), chunk: ", world!".into() },
/// ];
///
/// assert_eq!(deltas.len(), 3);
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Delta {
    /// Signals the start of a new part with the given kind.
    BeginPart {
        /// Identifier for the part being constructed.
        part_id: PartId,
        /// The kind of part being started.
        kind: PartKind,
    },
    /// Appends a text chunk to an in-progress part.
    AppendText {
        /// Identifier of the target part.
        part_id: PartId,
        /// The text chunk to append.
        chunk: String,
    },
    /// Appends raw bytes to an in-progress part.
    AppendBytes {
        /// Identifier of the target part.
        part_id: PartId,
        /// The byte chunk to append.
        chunk: Vec<u8>,
    },
    /// Replaces the structured value of an in-progress part wholesale.
    ReplaceStructured {
        /// Identifier of the target part.
        part_id: PartId,
        /// The new structured value.
        value: Value,
    },
    /// Sets or replaces the metadata on an in-progress part.
    SetMetadata {
        /// Identifier of the target part.
        part_id: PartId,
        /// The new metadata map.
        metadata: MetadataMap,
    },
    /// Finalises a part, providing the fully assembled [`Part`].
    CommitPart {
        /// The completed part.
        part: Part,
    },
}

/// Token and cost usage reported by a model provider for a single turn.
///
/// Reporters and compaction triggers inspect `Usage` to log progress, enforce
/// budgets, and decide when to compact the transcript.
///
/// # Example
///
/// ```rust
/// use agentkit_core::{TokenUsage, Usage};
///
/// let usage = Usage::new(
///     TokenUsage::new(1500, 200)
///         .with_cached_input_tokens(1000)
///         .with_cache_write_input_tokens(1200),
/// );
///
/// let tokens = usage.tokens.as_ref().unwrap();
/// assert_eq!(tokens.input_tokens, 1500);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Token counts for this turn, if the provider reports them.
    pub tokens: Option<TokenUsage>,
    /// Monetary cost for this turn, if the provider reports it.
    pub cost: Option<CostUsage>,
    /// Arbitrary key-value metadata.
    pub metadata: MetadataMap,
}

impl Usage {
    /// Builds a usage record with token counts and no cost.
    pub fn new(tokens: TokenUsage) -> Self {
        Self {
            tokens: Some(tokens),
            cost: None,
            metadata: MetadataMap::new(),
        }
    }

    /// Sets the cost information.
    pub fn with_cost(mut self, cost: CostUsage) -> Self {
        self.cost = Some(cost);
        self
    }

    /// Replaces the usage metadata.
    pub fn with_metadata(mut self, metadata: MetadataMap) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Token counts broken down by direction and special categories.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Number of tokens in the input (prompt) sent to the model.
    pub input_tokens: u64,
    /// Number of tokens in the model's output (completion).
    pub output_tokens: u64,
    /// Tokens consumed by the model's internal reasoning, if reported.
    pub reasoning_tokens: Option<u64>,
    /// Input tokens served from the provider's prompt cache, if reported.
    pub cached_input_tokens: Option<u64>,
    /// Input tokens written into the provider's prompt cache, if reported.
    pub cache_write_input_tokens: Option<u64>,
}

impl TokenUsage {
    /// Builds token usage with required input and output counts.
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            reasoning_tokens: None,
            cached_input_tokens: None,
            cache_write_input_tokens: None,
        }
    }

    /// Sets reasoning token count.
    pub fn with_reasoning_tokens(mut self, reasoning_tokens: u64) -> Self {
        self.reasoning_tokens = Some(reasoning_tokens);
        self
    }

    /// Sets cached input token count.
    pub fn with_cached_input_tokens(mut self, cached_input_tokens: u64) -> Self {
        self.cached_input_tokens = Some(cached_input_tokens);
        self
    }

    /// Sets cache-write input token count.
    pub fn with_cache_write_input_tokens(mut self, cache_write_input_tokens: u64) -> Self {
        self.cache_write_input_tokens = Some(cache_write_input_tokens);
        self
    }
}

/// Monetary cost for a single model turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CostUsage {
    /// The cost amount as a floating-point number.
    pub amount: f64,
    /// The ISO 4217 currency code (e.g. `"USD"`).
    pub currency: String,
    /// An optional provider-specific cost string for display purposes.
    pub provider_amount: Option<String>,
}

impl CostUsage {
    /// Builds cost usage with no provider-specific display string.
    pub fn new(amount: f64, currency: impl Into<String>) -> Self {
        Self {
            amount,
            currency: currency.into(),
            provider_amount: None,
        }
    }

    /// Sets the optional provider-specific display value.
    pub fn with_provider_amount(mut self, provider_amount: impl Into<String>) -> Self {
        self.provider_amount = Some(provider_amount.into());
        self
    }
}

/// The reason a model turn ended.
///
/// The loop inspects the `FinishReason` to decide whether to execute tool
/// calls, request more input, or report an error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishReason {
    /// The model finished generating its response normally.
    Completed,
    /// The model stopped to invoke one or more tools.
    ToolCall,
    /// The response was truncated because the token limit was reached.
    MaxTokens,
    /// The turn was cancelled via [`TurnCancellation`].
    Cancelled,
    /// The provider blocked the response (e.g. content policy violation).
    Blocked,
    /// An error occurred during generation.
    Error,
    /// A provider-specific reason not covered by the standard variants.
    Other(String),
}

/// Error returned when content cannot be normalised into the agentkit data model.
#[derive(Debug, Error)]
pub enum NormalizeError {
    /// The content shape is not supported by the current provider adapter.
    #[error("unsupported content shape: {0}")]
    Unsupported(String),
}

/// Error indicating an invalid state in the provider protocol.
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// The provider or loop reached a state that violates protocol invariants.
    #[error("invalid protocol state: {0}")]
    InvalidState(String),
}

/// Top-level error type that unifies normalisation and protocol errors.
///
/// Provider adapters and the agent loop surface this type so callers can
/// handle both categories uniformly.
#[derive(Debug, Error)]
pub enum AgentError {
    /// A content normalisation error.
    #[error(transparent)]
    Normalize(#[from] NormalizeError),
    /// A protocol-level error.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}
