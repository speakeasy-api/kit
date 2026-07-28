use agentkit_core::{ItemKind, Modality, PartKind};
use agentkit_http::HttpError;
use thiserror::Error;

/// Errors produced by the generic chat completions adapter.
#[derive(Debug, Error)]
pub enum CompletionsError {
    /// The underlying HTTP client could not be constructed.
    #[error("failed to build HTTP client: {0}")]
    HttpClient(HttpError),

    /// A transcript part is not supported for the given message role.
    #[error("unsupported item part {part_kind:?} for role {role:?}")]
    UnsupportedPart {
        /// The role of the item that contained the unsupported part.
        role: ItemKind,
        /// The kind of part that is not supported.
        part_kind: PartKind,
    },

    /// A media modality (e.g. video) is not supported by the adapter.
    #[error("unsupported modality {0:?}")]
    UnsupportedModality(Modality),

    /// A data reference type cannot be sent to the provider API.
    #[error("unsupported data reference: {0}")]
    UnsupportedDataRef(String),

    /// The transcript structure is invalid for conversion to chat messages.
    #[error("invalid transcript: {0}")]
    InvalidTranscript(String),

    /// The provider response could not be interpreted.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// A tool name does not match the regex `^[a-zA-Z0-9_-]{{1,64}}$`
    /// required by OpenAI-compatible chat completions APIs.
    #[error("tool name {0:?} does not match ^[a-zA-Z0-9_-]{{1,64}}$")]
    InvalidToolName(String),

    /// A value could not be serialized to JSON for the request body.
    #[error("failed to serialize request data: {0}")]
    Serialize(serde_json::Error),
}
