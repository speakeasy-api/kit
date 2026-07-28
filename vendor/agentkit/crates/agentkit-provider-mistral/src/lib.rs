//! Mistral model adapter for the agentkit agent loop.
//!
//! This crate provides [`MistralAdapter`] and [`MistralConfig`] for connecting
//! the agent loop to the [Mistral AI](https://mistral.ai) chat completions API.
//! It is built on the generic [`agentkit_adapter_completions`] crate.
//!
//! Note: Mistral uses `max_tokens` instead of the `max_completion_tokens` field
//! used by most other OpenAI-compatible APIs.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use agentkit_loop::{Agent, SessionConfig};
//! use agentkit_provider_mistral::{MistralAdapter, MistralConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = MistralConfig::from_env()?;
//!     let adapter = MistralAdapter::new(config)?;
//!
//!     let agent = Agent::builder()
//!         .model(adapter)
//!         .build()?;
//!
//!     let mut driver = agent
//!         .start(SessionConfig::new("demo"))
//!         .await?;
//!     Ok(())
//! }
//! ```

use agentkit_adapter_completions::{
    CompletionsAdapter, CompletionsError, CompletionsProvider, CompletionsSession, CompletionsTurn,
};
use agentkit_loop::{LoopError, ModelAdapter, SessionConfig};
use async_trait::async_trait;
use serde::Serialize;
use thiserror::Error;

const DEFAULT_ENDPOINT: &str = "https://api.mistral.ai/v1/chat/completions";

/// Configuration for connecting to the Mistral API.
///
/// Build one with [`MistralConfig::new`] for explicit values, or
/// [`MistralConfig::from_env`] to read from environment variables.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_provider_mistral::MistralConfig;
///
/// let config = MistralConfig::new("sk-...", "mistral-large-latest")
///     .with_temperature(0.0)
///     .with_max_tokens(4096);
/// ```
#[derive(Clone, Debug)]
pub struct MistralConfig {
    /// Mistral API key.
    pub api_key: String,
    /// Model identifier, e.g. `"mistral-large-latest"` or `"mistral-small-latest"`.
    pub model: String,
    /// Chat completions endpoint URL. Defaults to the Mistral production URL.
    pub base_url: String,
    /// Sampling temperature (0.0 = deterministic, higher = more creative).
    pub temperature: Option<f32>,
    /// Maximum number of tokens the model may generate. Mistral uses `max_tokens`
    /// rather than `max_completion_tokens`.
    pub max_tokens: Option<u32>,
    /// Nucleus sampling parameter.
    pub top_p: Option<f32>,
    /// Whether the model is allowed to emit multiple tool calls in a
    /// single turn. Omitted from the request when `None` so Mistral's
    /// per-model default applies.
    pub parallel_tool_calls: Option<bool>,
    /// Request SSE streaming responses. Defaults to `true`.
    pub streaming: bool,
    /// Whether to merge consecutive `user` messages before sending.
    /// Defaults to `true` because Mistral's chat templates enforce
    /// strict `user`/`assistant` alternation — the same behavior that
    /// vLLM-served Mistral surfaces as
    /// `Conversation roles must alternate user/assistant/user/assistant/...`.
    /// See <https://github.com/vllm-project/vllm/issues/6862>.
    pub strict_alternating_roles: bool,
}

impl MistralConfig {
    /// Creates a new configuration with the given API key and model identifier.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            base_url: DEFAULT_ENDPOINT.into(),
            temperature: None,
            max_tokens: None,
            top_p: None,
            parallel_tool_calls: None,
            streaming: true,
            strict_alternating_roles: true,
        }
    }

    /// Overrides the default chat completions endpoint URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Sets the sampling temperature (0.0 for deterministic output).
    pub fn with_temperature(mut self, v: f32) -> Self {
        self.temperature = Some(v);
        self
    }

    /// Sets the maximum number of tokens the model may generate per turn.
    pub fn with_max_tokens(mut self, v: u32) -> Self {
        self.max_tokens = Some(v);
        self
    }

    /// Sets the nucleus sampling parameter.
    pub fn with_top_p(mut self, v: f32) -> Self {
        self.top_p = Some(v);
        self
    }

    /// Sets whether the model may emit multiple tool calls in a single turn.
    pub fn with_parallel_tool_calls(mut self, flag: bool) -> Self {
        self.parallel_tool_calls = Some(flag);
        self
    }

    /// Toggles SSE streaming of model responses. Default: true.
    pub fn with_streaming(mut self, flag: bool) -> Self {
        self.streaming = flag;
        self
    }

    /// Toggle strict `user`/`assistant` alternation. Defaults to `true`
    /// for Mistral. Override to `false` only if you've confirmed your
    /// model deployment auto-merges adjacent user messages.
    pub fn with_strict_alternating_roles(mut self, flag: bool) -> Self {
        self.strict_alternating_roles = flag;
        self
    }

    /// Builds a configuration from environment variables.
    ///
    /// | Variable | Required | Default |
    /// |---|---|---|
    /// | `MISTRAL_API_KEY` | yes | -- |
    /// | `MISTRAL_MODEL` | no | `mistral-small-latest` |
    /// | `MISTRAL_BASE_URL` | no | `https://api.mistral.ai/v1/chat/completions` |
    pub fn from_env() -> Result<Self, MistralError> {
        let api_key = std::env::var("MISTRAL_API_KEY")
            .map_err(|_| MistralError::MissingEnv("MISTRAL_API_KEY"))?;
        let model =
            std::env::var("MISTRAL_MODEL").unwrap_or_else(|_| "mistral-small-latest".into());

        let mut config = Self::new(api_key, model);

        if let Ok(url) = std::env::var("MISTRAL_BASE_URL") {
            config = config.with_base_url(url);
        }

        Ok(config)
    }
}

/// Request parameters serialized into the Mistral request body.
///
/// Mistral uses `max_tokens` instead of `max_completion_tokens`.
#[derive(Clone, Debug, Serialize)]
pub struct MistralRequestConfig {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

/// The Mistral provider, implementing [`CompletionsProvider`].
#[derive(Clone, Debug)]
pub struct MistralProvider {
    api_key: String,
    base_url: String,
    streaming: bool,
    strict_alternating_roles: bool,
    request_config: MistralRequestConfig,
}

impl From<MistralConfig> for MistralProvider {
    fn from(config: MistralConfig) -> Self {
        Self {
            api_key: config.api_key,
            base_url: config.base_url,
            streaming: config.streaming,
            strict_alternating_roles: config.strict_alternating_roles,
            request_config: MistralRequestConfig {
                model: config.model,
                temperature: config.temperature,
                max_tokens: config.max_tokens,
                top_p: config.top_p,
                parallel_tool_calls: config.parallel_tool_calls,
            },
        }
    }
}

impl CompletionsProvider for MistralProvider {
    type Config = MistralRequestConfig;

    fn provider_name(&self) -> &str {
        "Mistral"
    }
    fn endpoint_url(&self) -> &str {
        &self.base_url
    }
    fn config(&self) -> &MistralRequestConfig {
        &self.request_config
    }

    fn preprocess_request(
        &self,
        builder: agentkit_http::HttpRequestBuilder,
    ) -> agentkit_http::HttpRequestBuilder {
        builder.bearer_auth(&self.api_key).header(
            "User-Agent",
            concat!("agentkit-provider-mistral/", env!("CARGO_PKG_VERSION")),
        )
    }

    fn streaming(&self) -> bool {
        self.streaming
    }

    fn requires_alternating_roles(&self) -> bool {
        self.strict_alternating_roles
    }
}

/// Model adapter that connects the agentkit agent loop to Mistral.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_loop::Agent;
/// use agentkit_provider_mistral::{MistralAdapter, MistralConfig};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let adapter = MistralAdapter::new(MistralConfig::from_env()?)?;
///
/// let agent = Agent::builder()
///     .model(adapter)
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct MistralAdapter(CompletionsAdapter<MistralProvider>);

/// An active session with the Mistral API.
pub type MistralSession = CompletionsSession<MistralProvider>;

/// A completed turn from the Mistral API.
pub type MistralTurn = CompletionsTurn;

impl MistralAdapter {
    /// Creates a new adapter from the given configuration.
    pub fn new(config: MistralConfig) -> Result<Self, MistralError> {
        let provider = MistralProvider::from(config);
        Ok(Self(CompletionsAdapter::new(provider)?))
    }
}

#[async_trait]
impl ModelAdapter for MistralAdapter {
    type Session = MistralSession;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        self.0.start_session(config).await
    }
}

/// Errors produced by the Mistral adapter.
#[derive(Debug, Error)]
pub enum MistralError {
    /// A required environment variable is not set.
    #[error("missing environment variable {0}")]
    MissingEnv(&'static str),

    /// An error from the generic completions adapter.
    #[error(transparent)]
    Completions(#[from] CompletionsError),
}
