//! Ollama model adapter for the agentkit agent loop.
//!
//! This crate provides [`OllamaAdapter`] and [`OllamaConfig`] for connecting
//! the agent loop to a local [Ollama](https://ollama.ai) instance via its
//! OpenAI-compatible chat completions endpoint. It is built on the generic
//! [`agentkit_adapter_completions`] crate.
//!
//! No API key is required — Ollama runs locally and does not authenticate
//! requests by default.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use agentkit_loop::{Agent, SessionConfig};
//! use agentkit_provider_ollama::{OllamaAdapter, OllamaConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Ollama must be running locally (e.g. `ollama serve`).
//!     let config = OllamaConfig::new("llama3.1:8b");
//!     let adapter = OllamaAdapter::new(config)?;
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

const DEFAULT_ENDPOINT: &str = "http://localhost:11434/v1/chat/completions";

/// Configuration for connecting to a local Ollama instance.
///
/// No API key is needed — Ollama runs without authentication by default.
/// Build one with [`OllamaConfig::new`] for explicit values, or
/// [`OllamaConfig::from_env`] to read from environment variables.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_provider_ollama::OllamaConfig;
///
/// let config = OllamaConfig::new("llama3.1:8b")
///     .with_temperature(0.0)
///     .with_num_predict(4096);
/// ```
#[derive(Clone, Debug)]
pub struct OllamaConfig {
    /// Model name as known to Ollama, e.g. `"llama3.1:8b"` or `"mistral"`.
    pub model: String,
    /// Chat completions endpoint URL. Defaults to `http://localhost:11434/v1/chat/completions`.
    pub base_url: String,
    /// Sampling temperature (0.0 = deterministic, higher = more creative).
    pub temperature: Option<f32>,
    /// Maximum number of tokens to generate (Ollama's equivalent of `max_completion_tokens`).
    pub num_predict: Option<u32>,
    /// Limits the next token selection to the top K most probable tokens.
    pub top_k: Option<u32>,
    /// Nucleus sampling parameter.
    pub top_p: Option<f32>,
    /// Whether the model is allowed to emit multiple tool calls in a
    /// single turn. Omitted from the request when `None`.
    pub parallel_tool_calls: Option<bool>,
    /// Request SSE streaming responses. Defaults to `true`.
    pub streaming: bool,
    /// Whether the loaded chat template enforces strict
    /// `user`/`assistant` role alternation. Set to `true` when running
    /// Mistral-/Mixtral-/Llama-template models locally; the adapter then
    /// merges adjacent user messages before sending.
    pub strict_alternating_roles: bool,
}

impl OllamaConfig {
    /// Creates a new configuration with the given model name.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: DEFAULT_ENDPOINT.into(),
            temperature: None,
            num_predict: None,
            top_k: None,
            top_p: None,
            parallel_tool_calls: None,
            streaming: true,
            strict_alternating_roles: false,
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

    /// Sets the maximum number of tokens to generate.
    pub fn with_num_predict(mut self, v: u32) -> Self {
        self.num_predict = Some(v);
        self
    }

    /// Limits the next token selection to the top K most probable tokens.
    pub fn with_top_k(mut self, v: u32) -> Self {
        self.top_k = Some(v);
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

    /// Enable strict `user`/`assistant` role alternation for chat
    /// templates that require it (notably Mistral, Mixtral, Llama). The
    /// adapter merges adjacent user-role messages before sending. Same
    /// rejection mode as vLLM-served Mistral; see
    /// <https://github.com/vllm-project/vllm/issues/6862>.
    pub fn with_strict_alternating_roles(mut self, flag: bool) -> Self {
        self.strict_alternating_roles = flag;
        self
    }

    /// Builds a configuration from environment variables.
    ///
    /// | Variable | Required | Default |
    /// |---|---|---|
    /// | `OLLAMA_MODEL` | yes | -- |
    /// | `OLLAMA_BASE_URL` | no | `http://localhost:11434/v1/chat/completions` |
    pub fn from_env() -> Result<Self, OllamaError> {
        let model =
            std::env::var("OLLAMA_MODEL").map_err(|_| OllamaError::MissingEnv("OLLAMA_MODEL"))?;

        let mut config = Self::new(model);

        if let Ok(url) = std::env::var("OLLAMA_BASE_URL") {
            config = config.with_base_url(url);
        }

        Ok(config)
    }
}

/// Request parameters serialized into the Ollama request body.
#[derive(Clone, Debug, Serialize)]
pub struct OllamaRequestConfig {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

/// The Ollama provider, implementing [`CompletionsProvider`].
#[derive(Clone, Debug)]
pub struct OllamaProvider {
    base_url: String,
    streaming: bool,
    strict_alternating_roles: bool,
    request_config: OllamaRequestConfig,
}

impl From<OllamaConfig> for OllamaProvider {
    fn from(config: OllamaConfig) -> Self {
        Self {
            base_url: config.base_url,
            streaming: config.streaming,
            strict_alternating_roles: config.strict_alternating_roles,
            request_config: OllamaRequestConfig {
                model: config.model,
                temperature: config.temperature,
                num_predict: config.num_predict,
                top_k: config.top_k,
                top_p: config.top_p,
                parallel_tool_calls: config.parallel_tool_calls,
            },
        }
    }
}

impl CompletionsProvider for OllamaProvider {
    type Config = OllamaRequestConfig;

    fn provider_name(&self) -> &str {
        "Ollama"
    }
    fn endpoint_url(&self) -> &str {
        &self.base_url
    }
    fn config(&self) -> &OllamaRequestConfig {
        &self.request_config
    }

    fn preprocess_request(
        &self,
        builder: agentkit_http::HttpRequestBuilder,
    ) -> agentkit_http::HttpRequestBuilder {
        builder.header(
            "User-Agent",
            concat!("agentkit-provider-ollama/", env!("CARGO_PKG_VERSION")),
        )
    }

    fn streaming(&self) -> bool {
        self.streaming
    }

    fn requires_alternating_roles(&self) -> bool {
        self.strict_alternating_roles
    }
}

/// Model adapter that connects the agentkit agent loop to a local Ollama instance.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_loop::Agent;
/// use agentkit_provider_ollama::{OllamaAdapter, OllamaConfig};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let adapter = OllamaAdapter::new(
///     OllamaConfig::new("llama3.1:8b").with_temperature(0.0),
/// )?;
///
/// let agent = Agent::builder()
///     .model(adapter)
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct OllamaAdapter(CompletionsAdapter<OllamaProvider>);

/// An active session with a local Ollama instance.
pub type OllamaSession = CompletionsSession<OllamaProvider>;

/// A completed turn from a local Ollama instance.
pub type OllamaTurn = CompletionsTurn;

impl OllamaAdapter {
    /// Creates a new adapter from the given configuration.
    pub fn new(config: OllamaConfig) -> Result<Self, OllamaError> {
        let provider = OllamaProvider::from(config);
        Ok(Self(CompletionsAdapter::new(provider)?))
    }
}

#[async_trait]
impl ModelAdapter for OllamaAdapter {
    type Session = OllamaSession;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        self.0.start_session(config).await
    }

    fn provider_name(&self) -> Option<&str> {
        self.0.provider_name()
    }
}

/// Errors produced by the Ollama adapter.
#[derive(Debug, Error)]
pub enum OllamaError {
    /// A required environment variable is not set.
    #[error("missing environment variable {0}")]
    MissingEnv(&'static str),

    /// An error from the generic completions adapter.
    #[error(transparent)]
    Completions(#[from] CompletionsError),
}
