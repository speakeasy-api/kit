use std::collections::{BTreeMap, HashSet};

use agentkit_core as upstream;
use agentkit_loop::{LoopInterrupt, TranscriptEvent};
use agentkit_tools_core::{ApprovalReason, ApprovalRequest, ToolInterruption};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const AGENTKIT_BASE_COMMIT: &str = "c3926f1c4f3c945d400c8b6ef039da1f84826fcd";
pub const AGENTKIT_BASE_TREE: &str = "5befb5676ea31703f4485e2d4b5869c39a39cb0f";
pub const AGENTKIT_DIRTY_OVERLAY_SHA256: &str =
    "92178443493858a217a04387442b56ecd2499e86b05b699aa76b685be146abd1";
pub const AGENTKIT_EXCLUDED_PATHS_SHA256: &str =
    "6013053000cc27b0e77ed61964266ff38e246bee41fee9ef829c0d8763ecd3ae";
pub const AGENTKIT_SNAPSHOT_SHA256: &str =
    "7a04d34e1509a0325bba5bd804f4d76afb6662ee7754d4ee903aa59b51867d0a";
pub const RUNLET_SNAPSHOT_SHA256: &str =
    "fef525f0008de628b1aff655d2e5685d2c826c76c8517c50e1ce8a88cfcbb8ef";

pub const ITEM_KIND_VARIANTS: usize = 7;
pub const PART_VARIANTS: usize = 8;
pub const PART_KIND_VARIANTS: usize = 8;
pub const MODALITY_VARIANTS: usize = 4;
pub const DATA_REF_VARIANTS: usize = 4;
pub const TOOL_OUTPUT_VARIANTS: usize = 4;
pub const DELTA_VARIANTS: usize = 6;
pub const FINISH_REASON_VARIANTS: usize = 7;
pub const LOOP_INTERRUPT_VARIANTS: usize = 3;
pub const TOOL_INTERRUPTION_VARIANTS: usize = 1;
pub const APPROVAL_REASON_VARIANTS: usize = 7;

pub type Metadata = BTreeMap<String, Value>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalItem {
    pub id: Option<String>,
    pub kind: CanonicalItemKind,
    pub parts: Vec<CanonicalPart>,
    pub metadata: Metadata,
    pub usage: Option<CanonicalUsage>,
    pub finish_reason: Option<CanonicalFinishReason>,
    pub created_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalItemKind {
    System,
    Developer,
    User,
    Assistant,
    Tool,
    Context,
    Notification,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CanonicalPart {
    Text {
        text: String,
        metadata: Metadata,
    },
    Media {
        modality: CanonicalModality,
        mime_type: String,
        data: CanonicalDataRef,
        metadata: Metadata,
    },
    File {
        name: Option<String>,
        mime_type: Option<String>,
        data: CanonicalDataRef,
        metadata: Metadata,
    },
    Structured {
        value: Value,
        schema: Option<Value>,
        metadata: Metadata,
    },
    Reasoning {
        summary: Option<String>,
        redacted: bool,
        hidden_data: Option<()>,
        provider_metadata: Option<()>,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
        metadata: Metadata,
    },
    ToolResult {
        call_id: String,
        output: CanonicalToolOutput,
        is_error: bool,
        metadata: Metadata,
    },
    Custom {
        kind: String,
        data: Option<CanonicalDataRef>,
        value: Option<Value>,
        metadata: Metadata,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalPartKind {
    Text,
    Media,
    File,
    Structured,
    Reasoning,
    ToolCall,
    ToolResult,
    Custom,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalModality {
    Audio,
    Image,
    Video,
    Binary,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CanonicalDataRef {
    InlineText(String),
    InlineBytes(Vec<u8>),
    Uri(String),
    Artifact(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CanonicalToolOutput {
    Text(String),
    Structured(Value),
    Parts(Vec<CanonicalPart>),
    Files(Vec<CanonicalFile>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CanonicalFile {
    pub name: Option<String>,
    pub mime_type: Option<String>,
    pub data: CanonicalDataRef,
    pub metadata: Metadata,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "provider_reason", rename_all = "snake_case")]
pub enum CanonicalFinishReason {
    Completed,
    ToolCall,
    MaxTokens,
    Cancelled,
    Blocked,
    Error,
    Other(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub uncached_input_tokens: Option<u64>,
    pub tool_calls: Option<u64>,
    pub tool_time_ms: Option<u64>,
    pub compute_time_ms: Option<u64>,
    pub cost_amount: Option<f64>,
    pub cost_currency: Option<String>,
    pub provider_cost_amount: Option<String>,
    pub metadata: Metadata,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CanonicalDelta {
    BeginPart {
        part_id: String,
        kind: CanonicalPartKind,
    },
    AppendText {
        part_id: String,
        chunk: String,
    },
    AppendBytes {
        part_id: String,
        chunk: Vec<u8>,
    },
    ReplaceStructured {
        part_id: String,
        value: Value,
    },
    SetMetadata {
        part_id: String,
        metadata: Metadata,
    },
    CommitPart {
        part: CanonicalPart,
    },
    ReasoningSuppressed {
        part_id: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalTranscriptEvent {
    pub session_id: String,
    pub item: CanonicalItem,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalInterruptKind {
    Approval,
    Input,
    ToolRoundBoundary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalApprovalReason {
    PolicyRequiresConfirmation,
    EscalatedRisk,
    UnknownTarget,
    SensitivePath,
    SensitiveCommand,
    SensitiveServer,
    SensitiveAuthScope,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CanonicalInterrupt {
    pub kind: CanonicalInterruptKind,
    pub blocking: bool,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub approval_id: Option<String>,
    pub task_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub request_kind: Option<String>,
    pub approval_reason: Option<CanonicalApprovalReason>,
    pub message: Option<String>,
    pub transcript_len: Option<usize>,
    pub metadata: Option<Metadata>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalCancellationState {
    Unavailable,
    Active,
    CancellationRequested,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CanonicalCancellation {
    pub state: CanonicalCancellationState,
    pub checkpoint_generation: Option<u64>,
    pub observed_generation: Option<u64>,
}

#[derive(Default)]
pub struct DeltaMapper {
    reasoning_parts: HashSet<String>,
}

impl DeltaMapper {
    pub fn map(&mut self, delta: &upstream::Delta) -> CanonicalDelta {
        match delta {
            upstream::Delta::BeginPart { part_id, kind } => {
                if matches!(kind, upstream::PartKind::Reasoning) {
                    self.reasoning_parts.insert(part_id.0.clone());
                    CanonicalDelta::ReasoningSuppressed {
                        part_id: Some(part_id.0.clone()),
                    }
                } else {
                    CanonicalDelta::BeginPart {
                        part_id: part_id.0.clone(),
                        kind: map_part_kind(*kind),
                    }
                }
            }
            upstream::Delta::AppendText { part_id, chunk } => {
                if self.reasoning_parts.contains(&part_id.0) {
                    CanonicalDelta::ReasoningSuppressed {
                        part_id: Some(part_id.0.clone()),
                    }
                } else {
                    CanonicalDelta::AppendText {
                        part_id: part_id.0.clone(),
                        chunk: chunk.clone(),
                    }
                }
            }
            upstream::Delta::AppendBytes { part_id, chunk } => {
                if self.reasoning_parts.contains(&part_id.0) {
                    CanonicalDelta::ReasoningSuppressed {
                        part_id: Some(part_id.0.clone()),
                    }
                } else {
                    CanonicalDelta::AppendBytes {
                        part_id: part_id.0.clone(),
                        chunk: chunk.clone(),
                    }
                }
            }
            upstream::Delta::ReplaceStructured { part_id, value } => {
                if self.reasoning_parts.contains(&part_id.0) {
                    CanonicalDelta::ReasoningSuppressed {
                        part_id: Some(part_id.0.clone()),
                    }
                } else {
                    CanonicalDelta::ReplaceStructured {
                        part_id: part_id.0.clone(),
                        value: value.clone(),
                    }
                }
            }
            upstream::Delta::SetMetadata { part_id, metadata } => {
                if self.reasoning_parts.contains(&part_id.0) {
                    CanonicalDelta::ReasoningSuppressed {
                        part_id: Some(part_id.0.clone()),
                    }
                } else {
                    CanonicalDelta::SetMetadata {
                        part_id: part_id.0.clone(),
                        metadata: metadata.clone(),
                    }
                }
            }
            upstream::Delta::CommitPart { part } => CanonicalDelta::CommitPart {
                part: from_agentkit_part(part),
            },
        }
    }
}

pub fn from_agentkit_item(item: &upstream::Item) -> CanonicalItem {
    CanonicalItem {
        id: item.id.as_ref().map(|id| id.0.clone()),
        kind: map_item_kind(item.kind),
        parts: item.parts.iter().map(from_agentkit_part).collect(),
        metadata: item.metadata.clone(),
        usage: item.usage.as_ref().map(from_agentkit_usage),
        finish_reason: item.finish_reason.as_ref().map(map_finish_reason),
        created_at_ms: item.created_at.map(|timestamp| timestamp.0),
    }
}

pub fn to_agentkit_item(item: &CanonicalItem) -> upstream::Item {
    upstream::Item {
        id: item.id.clone().map(upstream::MessageId),
        kind: unmap_item_kind(item.kind),
        parts: item.parts.iter().map(to_agentkit_part).collect(),
        metadata: item.metadata.clone(),
        usage: item.usage.as_ref().map(to_agentkit_usage),
        finish_reason: item.finish_reason.as_ref().map(unmap_finish_reason),
        created_at: item.created_at_ms.map(upstream::Timestamp),
    }
}

pub fn from_agentkit_part(part: &upstream::Part) -> CanonicalPart {
    match part {
        upstream::Part::Text(part) => CanonicalPart::Text {
            text: part.text.clone(),
            metadata: part.metadata.clone(),
        },
        upstream::Part::Media(part) => CanonicalPart::Media {
            modality: map_modality(part.modality),
            mime_type: part.mime_type.clone(),
            data: map_data_ref(&part.data),
            metadata: part.metadata.clone(),
        },
        upstream::Part::File(part) => CanonicalPart::File {
            name: part.name.clone(),
            mime_type: part.mime_type.clone(),
            data: map_data_ref(&part.data),
            metadata: part.metadata.clone(),
        },
        upstream::Part::Structured(part) => CanonicalPart::Structured {
            value: part.value.clone(),
            schema: part.schema.clone(),
            metadata: part.metadata.clone(),
        },
        upstream::Part::Reasoning(part) => CanonicalPart::Reasoning {
            summary: part.summary.clone(),
            redacted: part.redacted,
            hidden_data: None,
            provider_metadata: None,
        },
        upstream::Part::ToolCall(part) => CanonicalPart::ToolCall {
            id: part.id.0.clone(),
            name: part.name.clone(),
            input: part.input.clone(),
            metadata: part.metadata.clone(),
        },
        upstream::Part::ToolResult(part) => CanonicalPart::ToolResult {
            call_id: part.call_id.0.clone(),
            output: map_tool_output(&part.output),
            is_error: part.is_error,
            metadata: part.metadata.clone(),
        },
        upstream::Part::Custom(part) => CanonicalPart::Custom {
            kind: part.kind.clone(),
            data: part.data.as_ref().map(map_data_ref),
            value: part.value.clone(),
            metadata: part.metadata.clone(),
        },
    }
}

pub fn from_agentkit_usage(usage: &upstream::Usage) -> CanonicalUsage {
    let tokens = usage.tokens.as_ref();
    let cost = usage.cost.as_ref();
    let uncached_input_tokens = tokens.and_then(|tokens| {
        tokens
            .cached_input_tokens
            .zip(tokens.cache_write_input_tokens)
            .and_then(|(read, write)| read.checked_add(write))
            .and_then(|cached| tokens.input_tokens.checked_sub(cached))
    });
    CanonicalUsage {
        input_tokens: tokens.map(|tokens| tokens.input_tokens),
        output_tokens: tokens.map(|tokens| tokens.output_tokens),
        reasoning_tokens: tokens.and_then(|tokens| tokens.reasoning_tokens),
        cached_input_tokens: tokens.and_then(|tokens| tokens.cached_input_tokens),
        cache_write_input_tokens: tokens.and_then(|tokens| tokens.cache_write_input_tokens),
        uncached_input_tokens,
        tool_calls: None,
        tool_time_ms: None,
        compute_time_ms: None,
        cost_amount: cost.map(|cost| cost.amount),
        cost_currency: cost.map(|cost| cost.currency.clone()),
        provider_cost_amount: cost.and_then(|cost| cost.provider_amount.clone()),
        metadata: usage.metadata.clone(),
    }
}

pub fn from_transcript_event(event: TranscriptEvent<'_>) -> CanonicalTranscriptEvent {
    CanonicalTranscriptEvent {
        session_id: event.session_id.0.clone(),
        item: from_agentkit_item(event.item),
    }
}

pub fn from_loop_interrupt(interrupt: &LoopInterrupt) -> CanonicalInterrupt {
    match interrupt {
        LoopInterrupt::ApprovalRequest(pending) => approval_interrupt(&pending.request),
        LoopInterrupt::AwaitingInput(request) => CanonicalInterrupt {
            kind: CanonicalInterruptKind::Input,
            blocking: false,
            session_id: Some(request.session_id.0.clone()),
            turn_id: None,
            approval_id: None,
            task_id: None,
            tool_call_id: None,
            request_kind: None,
            approval_reason: None,
            message: Some(request.reason.clone()),
            transcript_len: None,
            metadata: None,
        },
        LoopInterrupt::AfterToolResult(round) => CanonicalInterrupt {
            kind: CanonicalInterruptKind::ToolRoundBoundary,
            blocking: false,
            session_id: Some(round.session_id.0.clone()),
            turn_id: Some(round.turn_id.0.clone()),
            approval_id: None,
            task_id: None,
            tool_call_id: None,
            request_kind: None,
            approval_reason: None,
            message: None,
            transcript_len: Some(round.transcript_len),
            metadata: None,
        },
    }
}

pub fn from_tool_interruption(interruption: &ToolInterruption) -> CanonicalInterrupt {
    match interruption {
        ToolInterruption::ApprovalRequired(request) => approval_interrupt(request),
    }
}

pub fn from_turn_cancellation(
    cancellation: Option<&upstream::TurnCancellation>,
) -> CanonicalCancellation {
    let Some(cancellation) = cancellation else {
        return CanonicalCancellation {
            state: CanonicalCancellationState::Unavailable,
            checkpoint_generation: None,
            observed_generation: None,
        };
    };
    CanonicalCancellation {
        state: if cancellation.is_cancelled() {
            CanonicalCancellationState::CancellationRequested
        } else {
            CanonicalCancellationState::Active
        },
        checkpoint_generation: Some(cancellation.generation()),
        observed_generation: Some(cancellation.handle().generation()),
    }
}

fn map_item_kind(kind: upstream::ItemKind) -> CanonicalItemKind {
    match kind {
        upstream::ItemKind::System => CanonicalItemKind::System,
        upstream::ItemKind::Developer => CanonicalItemKind::Developer,
        upstream::ItemKind::User => CanonicalItemKind::User,
        upstream::ItemKind::Assistant => CanonicalItemKind::Assistant,
        upstream::ItemKind::Tool => CanonicalItemKind::Tool,
        upstream::ItemKind::Context => CanonicalItemKind::Context,
        upstream::ItemKind::Notification => CanonicalItemKind::Notification,
    }
}

fn unmap_item_kind(kind: CanonicalItemKind) -> upstream::ItemKind {
    match kind {
        CanonicalItemKind::System => upstream::ItemKind::System,
        CanonicalItemKind::Developer => upstream::ItemKind::Developer,
        CanonicalItemKind::User => upstream::ItemKind::User,
        CanonicalItemKind::Assistant => upstream::ItemKind::Assistant,
        CanonicalItemKind::Tool => upstream::ItemKind::Tool,
        CanonicalItemKind::Context => upstream::ItemKind::Context,
        CanonicalItemKind::Notification => upstream::ItemKind::Notification,
    }
}

fn map_part_kind(kind: upstream::PartKind) -> CanonicalPartKind {
    match kind {
        upstream::PartKind::Text => CanonicalPartKind::Text,
        upstream::PartKind::Media => CanonicalPartKind::Media,
        upstream::PartKind::File => CanonicalPartKind::File,
        upstream::PartKind::Structured => CanonicalPartKind::Structured,
        upstream::PartKind::Reasoning => CanonicalPartKind::Reasoning,
        upstream::PartKind::ToolCall => CanonicalPartKind::ToolCall,
        upstream::PartKind::ToolResult => CanonicalPartKind::ToolResult,
        upstream::PartKind::Custom => CanonicalPartKind::Custom,
    }
}

fn map_modality(modality: upstream::Modality) -> CanonicalModality {
    match modality {
        upstream::Modality::Audio => CanonicalModality::Audio,
        upstream::Modality::Image => CanonicalModality::Image,
        upstream::Modality::Video => CanonicalModality::Video,
        upstream::Modality::Binary => CanonicalModality::Binary,
    }
}

fn unmap_modality(modality: CanonicalModality) -> upstream::Modality {
    match modality {
        CanonicalModality::Audio => upstream::Modality::Audio,
        CanonicalModality::Image => upstream::Modality::Image,
        CanonicalModality::Video => upstream::Modality::Video,
        CanonicalModality::Binary => upstream::Modality::Binary,
    }
}

fn map_data_ref(data: &upstream::DataRef) -> CanonicalDataRef {
    match data {
        upstream::DataRef::InlineText(value) => CanonicalDataRef::InlineText(value.clone()),
        upstream::DataRef::InlineBytes(value) => CanonicalDataRef::InlineBytes(value.clone()),
        upstream::DataRef::Uri(value) => CanonicalDataRef::Uri(value.clone()),
        upstream::DataRef::Handle(value) => CanonicalDataRef::Artifact(value.0.clone()),
    }
}

fn unmap_data_ref(data: &CanonicalDataRef) -> upstream::DataRef {
    match data {
        CanonicalDataRef::InlineText(value) => upstream::DataRef::InlineText(value.clone()),
        CanonicalDataRef::InlineBytes(value) => upstream::DataRef::InlineBytes(value.clone()),
        CanonicalDataRef::Uri(value) => upstream::DataRef::Uri(value.clone()),
        CanonicalDataRef::Artifact(value) => {
            upstream::DataRef::Handle(upstream::ArtifactId(value.clone()))
        }
    }
}

fn map_tool_output(output: &upstream::ToolOutput) -> CanonicalToolOutput {
    match output {
        upstream::ToolOutput::Text(value) => CanonicalToolOutput::Text(value.clone()),
        upstream::ToolOutput::Structured(value) => CanonicalToolOutput::Structured(value.clone()),
        upstream::ToolOutput::Parts(parts) => {
            CanonicalToolOutput::Parts(parts.iter().map(from_agentkit_part).collect())
        }
        upstream::ToolOutput::Files(files) => CanonicalToolOutput::Files(
            files
                .iter()
                .map(|file| CanonicalFile {
                    name: file.name.clone(),
                    mime_type: file.mime_type.clone(),
                    data: map_data_ref(&file.data),
                    metadata: file.metadata.clone(),
                })
                .collect(),
        ),
    }
}

fn unmap_tool_output(output: &CanonicalToolOutput) -> upstream::ToolOutput {
    match output {
        CanonicalToolOutput::Text(value) => upstream::ToolOutput::Text(value.clone()),
        CanonicalToolOutput::Structured(value) => upstream::ToolOutput::Structured(value.clone()),
        CanonicalToolOutput::Parts(parts) => {
            upstream::ToolOutput::Parts(parts.iter().map(to_agentkit_part).collect())
        }
        CanonicalToolOutput::Files(files) => upstream::ToolOutput::Files(
            files
                .iter()
                .map(|file| upstream::FilePart {
                    name: file.name.clone(),
                    mime_type: file.mime_type.clone(),
                    data: unmap_data_ref(&file.data),
                    metadata: file.metadata.clone(),
                })
                .collect(),
        ),
    }
}

fn to_agentkit_part(part: &CanonicalPart) -> upstream::Part {
    match part {
        CanonicalPart::Text { text, metadata } => upstream::Part::Text(upstream::TextPart {
            text: text.clone(),
            metadata: metadata.clone(),
        }),
        CanonicalPart::Media {
            modality,
            mime_type,
            data,
            metadata,
        } => upstream::Part::Media(upstream::MediaPart {
            modality: unmap_modality(*modality),
            mime_type: mime_type.clone(),
            data: unmap_data_ref(data),
            metadata: metadata.clone(),
        }),
        CanonicalPart::File {
            name,
            mime_type,
            data,
            metadata,
        } => upstream::Part::File(upstream::FilePart {
            name: name.clone(),
            mime_type: mime_type.clone(),
            data: unmap_data_ref(data),
            metadata: metadata.clone(),
        }),
        CanonicalPart::Structured {
            value,
            schema,
            metadata,
        } => upstream::Part::Structured(upstream::StructuredPart {
            value: value.clone(),
            schema: schema.clone(),
            metadata: metadata.clone(),
        }),
        CanonicalPart::Reasoning {
            summary, redacted, ..
        } => upstream::Part::Reasoning(upstream::ReasoningPart {
            summary: summary.clone(),
            data: None,
            redacted: *redacted,
            metadata: Metadata::new(),
        }),
        CanonicalPart::ToolCall {
            id,
            name,
            input,
            metadata,
        } => upstream::Part::ToolCall(upstream::ToolCallPart {
            id: upstream::ToolCallId(id.clone()),
            name: name.clone(),
            input: input.clone(),
            metadata: metadata.clone(),
        }),
        CanonicalPart::ToolResult {
            call_id,
            output,
            is_error,
            metadata,
        } => upstream::Part::ToolResult(upstream::ToolResultPart {
            call_id: upstream::ToolCallId(call_id.clone()),
            output: unmap_tool_output(output),
            is_error: *is_error,
            metadata: metadata.clone(),
        }),
        CanonicalPart::Custom {
            kind,
            data,
            value,
            metadata,
        } => upstream::Part::Custom(upstream::CustomPart {
            kind: kind.clone(),
            data: data.as_ref().map(unmap_data_ref),
            value: value.clone(),
            metadata: metadata.clone(),
        }),
    }
}

fn map_finish_reason(reason: &upstream::FinishReason) -> CanonicalFinishReason {
    match reason {
        upstream::FinishReason::Completed => CanonicalFinishReason::Completed,
        upstream::FinishReason::ToolCall => CanonicalFinishReason::ToolCall,
        upstream::FinishReason::MaxTokens => CanonicalFinishReason::MaxTokens,
        upstream::FinishReason::Cancelled => CanonicalFinishReason::Cancelled,
        upstream::FinishReason::Blocked => CanonicalFinishReason::Blocked,
        upstream::FinishReason::Error => CanonicalFinishReason::Error,
        upstream::FinishReason::Other(reason) => CanonicalFinishReason::Other(reason.clone()),
    }
}

fn unmap_finish_reason(reason: &CanonicalFinishReason) -> upstream::FinishReason {
    match reason {
        CanonicalFinishReason::Completed => upstream::FinishReason::Completed,
        CanonicalFinishReason::ToolCall => upstream::FinishReason::ToolCall,
        CanonicalFinishReason::MaxTokens => upstream::FinishReason::MaxTokens,
        CanonicalFinishReason::Cancelled => upstream::FinishReason::Cancelled,
        CanonicalFinishReason::Blocked => upstream::FinishReason::Blocked,
        CanonicalFinishReason::Error => upstream::FinishReason::Error,
        CanonicalFinishReason::Other(reason) => upstream::FinishReason::Other(reason.clone()),
    }
}

fn to_agentkit_usage(usage: &CanonicalUsage) -> upstream::Usage {
    let tokens =
        usage
            .input_tokens
            .zip(usage.output_tokens)
            .map(|(input_tokens, output_tokens)| upstream::TokenUsage {
                input_tokens,
                output_tokens,
                reasoning_tokens: usage.reasoning_tokens,
                cached_input_tokens: usage.cached_input_tokens,
                cache_write_input_tokens: usage.cache_write_input_tokens,
            });
    let cost = usage
        .cost_amount
        .zip(usage.cost_currency.clone())
        .map(|(amount, currency)| upstream::CostUsage {
            amount,
            currency,
            provider_amount: usage.provider_cost_amount.clone(),
        });
    upstream::Usage {
        tokens,
        cost,
        metadata: usage.metadata.clone(),
    }
}

fn map_approval_reason(reason: &ApprovalReason) -> CanonicalApprovalReason {
    match reason {
        ApprovalReason::PolicyRequiresConfirmation => {
            CanonicalApprovalReason::PolicyRequiresConfirmation
        }
        ApprovalReason::EscalatedRisk => CanonicalApprovalReason::EscalatedRisk,
        ApprovalReason::UnknownTarget => CanonicalApprovalReason::UnknownTarget,
        ApprovalReason::SensitivePath => CanonicalApprovalReason::SensitivePath,
        ApprovalReason::SensitiveCommand => CanonicalApprovalReason::SensitiveCommand,
        ApprovalReason::SensitiveServer => CanonicalApprovalReason::SensitiveServer,
        ApprovalReason::SensitiveAuthScope => CanonicalApprovalReason::SensitiveAuthScope,
    }
}

fn approval_interrupt(request: &ApprovalRequest) -> CanonicalInterrupt {
    CanonicalInterrupt {
        kind: CanonicalInterruptKind::Approval,
        blocking: true,
        session_id: None,
        turn_id: None,
        approval_id: Some(request.id.0.clone()),
        task_id: request.task_id.as_ref().map(|id| id.0.clone()),
        tool_call_id: request.call_id.as_ref().map(|id| id.0.clone()),
        request_kind: Some(request.request_kind.clone()),
        approval_reason: Some(map_approval_reason(&request.reason)),
        message: Some(request.summary.clone()),
        transcript_len: None,
        metadata: Some(request.metadata.clone()),
    }
}
