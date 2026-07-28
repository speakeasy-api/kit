use agentkit_http::HttpError;
use thiserror::Error;

/// Errors produced by the Anthropic adapter.
#[derive(Debug, Error)]
pub enum AnthropicError {
    /// A required environment variable is not set or could not be parsed.
    #[error("missing or invalid environment variable {0}")]
    MissingEnv(&'static str),

    /// Neither an API key nor an auth token was supplied.
    #[error("no Anthropic credentials configured: set api_key or auth_token")]
    MissingCredentials,

    /// `max_tokens` is required by the Messages API and must be > 0.
    #[error("max_tokens must be greater than zero")]
    InvalidMaxTokens,

    /// Failure constructing the underlying HTTP client.
    #[error(transparent)]
    HttpClient(#[from] HttpError),
}
