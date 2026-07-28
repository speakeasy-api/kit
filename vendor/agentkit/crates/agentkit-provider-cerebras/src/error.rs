//! Error types for the Cerebras adapter.

use agentkit_core::ItemKind;
use agentkit_http::HttpError;
use agentkit_loop::LoopError;
use thiserror::Error;

use crate::config::PartKindName;

/// Errors produced at adapter construction or external-endpoint call time.
#[derive(Debug, Error)]
pub enum CerebrasError {
    /// Configuration validation failed.
    #[error(transparent)]
    Build(#[from] BuildError),

    /// A required environment variable is missing or unparseable.
    #[error("missing or invalid environment variable {0}")]
    MissingEnv(&'static str),

    /// Cerebras returned a non-success HTTP status.
    #[error("Cerebras request failed with status {status}: {body}")]
    Status {
        /// HTTP status code.
        status: u16,
        /// Body of the failing response (best-effort).
        body: String,
    },

    /// Underlying HTTP transport error.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// Response JSON could not be parsed.
    #[error(transparent)]
    Response(#[from] ResponseError),

    /// Generic provider-side error surfaced through the adapter.
    #[error("{0}")]
    Other(String),
}

impl From<CerebrasError> for LoopError {
    fn from(error: CerebrasError) -> Self {
        match error {
            CerebrasError::Other(msg) => LoopError::Provider(msg),
            other => LoopError::Provider(other.to_string()),
        }
    }
}

/// Errors produced while assembling a Cerebras chat-completions request body.
#[derive(Debug, Error)]
pub enum BuildError {
    /// Transcript contained a content part the Cerebras API cannot accept for
    /// the given role (e.g. a raw `Custom` part on a user message).
    #[error("unsupported content part {part_kind:?} on role {role:?}")]
    UnsupportedPart {
        /// Role of the offending transcript item.
        role: ItemKind,
        /// Kind of the offending part.
        part_kind: PartKindName,
    },

    /// Tool name failed the Cerebras name regex (`^[a-zA-Z0-9_-]{1,64}$`).
    #[error("tool name {0:?} does not match ^[a-zA-Z0-9_-]{{1,64}}$")]
    InvalidToolName(String),

    /// JSON-Schema passed to `OutputFormat::JsonSchema` violated a documented
    /// Cerebras constraint (e.g. `pattern`, `$ref` outside `$defs`, nest depth).
    #[error("response_format schema violates Cerebras constraint: {0}")]
    SchemaViolation(String),

    /// `prediction` was set together with an incompatible parameter.
    #[error("prediction cannot be combined with {0}")]
    PredictionConflicts(&'static str),

    /// A field fell outside the documented valid range.
    #[error("{field} out of range: {message}")]
    OutOfRange {
        /// Name of the offending field.
        field: &'static str,
        /// Human-readable description of the violation.
        message: String,
    },

    /// `from_env()` could not locate a required variable.
    #[error("missing or invalid environment variable {0}")]
    MissingEnv(&'static str),

    /// `top_logprobs` was set without `logprobs: true`.
    #[error("top_logprobs requires logprobs = true")]
    TopLogprobsWithoutLogprobs,

    /// Generic JSON serialization failure.
    #[error(transparent)]
    Serialize(#[from] serde_json::Error),
}

impl From<BuildError> for LoopError {
    fn from(error: BuildError) -> Self {
        LoopError::Provider(error.to_string())
    }
}

/// Errors produced while parsing a Cerebras response (buffered or streaming).
#[derive(Debug, Error)]
pub enum ResponseError {
    /// Malformed or unexpected JSON / missing required field. Reserved for
    /// protocol-level breakage, not for server-reported errors.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Server-reported error surfaced mid-stream via `event: error` or an
    /// unnamed frame whose JSON carries a top-level `error` key.
    #[error("stream error ({status_code:?}): {message}")]
    StreamError {
        /// Error message reported by Cerebras.
        message: String,
        /// Optional HTTP status the server attached to the frame.
        status_code: Option<u16>,
    },
}

impl From<ResponseError> for LoopError {
    fn from(error: ResponseError) -> Self {
        LoopError::Provider(error.to_string())
    }
}
