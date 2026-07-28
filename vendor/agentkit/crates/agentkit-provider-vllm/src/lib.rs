//! vLLM model adapter for the agentkit agent loop.
//!
//! This crate provides [`VllmAdapter`] and [`VllmConfig`] for connecting
//! the agent loop to a [vLLM](https://docs.vllm.ai) server via its
//! OpenAI-compatible chat completions endpoint. It is built on the generic
//! [`agentkit_adapter_completions`] crate.
//!
//! An API key is optional — vLLM servers can run with or without authentication.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use agentkit_loop::{Agent, SessionConfig};
//! use agentkit_provider_vllm::{VllmAdapter, VllmConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = VllmConfig::new("meta-llama/Llama-3.1-8B-Instruct");
//!     let adapter = VllmAdapter::new(config)?;
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

const DEFAULT_ENDPOINT: &str = "http://localhost:8000/v1/chat/completions";

/// Configuration for connecting to a vLLM server.
///
/// An API key is only required if the vLLM server was started with
/// `--api-key`. Build one with [`VllmConfig::new`] for explicit values,
/// or [`VllmConfig::from_env`] to read from environment variables.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_provider_vllm::VllmConfig;
///
/// let config = VllmConfig::new("meta-llama/Llama-3.1-8B-Instruct")
///     .with_base_url("http://gpu-server:8000/v1/chat/completions")
///     .with_temperature(0.0);
/// ```
#[derive(Clone, Debug)]
pub struct VllmConfig {
    /// HuggingFace model identifier served by the vLLM instance,
    /// e.g. `"meta-llama/Llama-3.1-8B-Instruct"`.
    pub model: String,
    /// Chat completions endpoint URL. Defaults to `http://localhost:8000/v1/chat/completions`.
    pub base_url: String,
    /// Optional API key, required only if the vLLM server enforces authentication.
    pub api_key: Option<String>,
    /// Sampling temperature (0.0 = deterministic, higher = more creative).
    pub temperature: Option<f32>,
    /// Maximum number of completion tokens the model may generate.
    pub max_completion_tokens: Option<u32>,
    /// Nucleus sampling parameter.
    pub top_p: Option<f32>,
    /// Whether the model is allowed to emit multiple tool calls in a
    /// single turn. Omitted from the request when `None`.
    pub parallel_tool_calls: Option<bool>,
    /// Request SSE streaming responses. Defaults to `true`.
    pub streaming: bool,
    /// Whether the loaded chat template enforces strict
    /// `user`/`assistant` role alternation. Set to `true` for
    /// Mistral-/Mixtral-/Llama-template models served via vLLM, which
    /// otherwise return `Conversation roles must alternate
    /// user/assistant/user/assistant/...`. See
    /// <https://github.com/vllm-project/vllm/issues/6862>.
    pub strict_alternating_roles: bool,
}

impl VllmConfig {
    /// Creates a new configuration with the given model identifier.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            base_url: DEFAULT_ENDPOINT.into(),
            api_key: None,
            temperature: None,
            max_completion_tokens: None,
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

    /// Sets the API key for authenticated vLLM servers.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Sets the sampling temperature (0.0 for deterministic output).
    pub fn with_temperature(mut self, v: f32) -> Self {
        self.temperature = Some(v);
        self
    }

    /// Sets the maximum number of tokens the model may generate per turn.
    pub fn with_max_completion_tokens(mut self, v: u32) -> Self {
        self.max_completion_tokens = Some(v);
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
    /// adapter merges adjacent user-role messages before sending. See
    /// <https://github.com/vllm-project/vllm/issues/6862>.
    pub fn with_strict_alternating_roles(mut self, flag: bool) -> Self {
        self.strict_alternating_roles = flag;
        self
    }

    /// Builds a configuration from environment variables.
    ///
    /// | Variable | Required | Default |
    /// |---|---|---|
    /// | `VLLM_MODEL` | yes | -- |
    /// | `VLLM_BASE_URL` | no | `http://localhost:8000/v1/chat/completions` |
    /// | `VLLM_API_KEY` | no | -- |
    pub fn from_env() -> Result<Self, VllmError> {
        let model = std::env::var("VLLM_MODEL").map_err(|_| VllmError::MissingEnv("VLLM_MODEL"))?;

        let mut config = Self::new(model);

        if let Ok(url) = std::env::var("VLLM_BASE_URL") {
            config = config.with_base_url(url);
        }
        if let Ok(key) = std::env::var("VLLM_API_KEY") {
            config = config.with_api_key(key);
        }

        Ok(config)
    }
}

/// Request parameters serialized into the vLLM request body.
#[derive(Clone, Debug, Serialize)]
pub struct VllmRequestConfig {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

/// The vLLM provider, implementing [`CompletionsProvider`].
#[derive(Clone, Debug)]
pub struct VllmProvider {
    base_url: String,
    api_key: Option<String>,
    streaming: bool,
    strict_alternating_roles: bool,
    request_config: VllmRequestConfig,
}

impl From<VllmConfig> for VllmProvider {
    fn from(config: VllmConfig) -> Self {
        Self {
            base_url: config.base_url,
            api_key: config.api_key,
            streaming: config.streaming,
            strict_alternating_roles: config.strict_alternating_roles,
            request_config: VllmRequestConfig {
                model: config.model,
                temperature: config.temperature,
                max_completion_tokens: config.max_completion_tokens,
                top_p: config.top_p,
                parallel_tool_calls: config.parallel_tool_calls,
            },
        }
    }
}

impl CompletionsProvider for VllmProvider {
    type Config = VllmRequestConfig;

    fn provider_name(&self) -> &str {
        "vLLM"
    }
    fn endpoint_url(&self) -> &str {
        &self.base_url
    }
    fn config(&self) -> &VllmRequestConfig {
        &self.request_config
    }

    fn preprocess_request(
        &self,
        builder: agentkit_http::HttpRequestBuilder,
    ) -> agentkit_http::HttpRequestBuilder {
        let builder = builder.header(
            "User-Agent",
            concat!("agentkit-provider-vllm/", env!("CARGO_PKG_VERSION")),
        );
        match &self.api_key {
            Some(key) => builder.bearer_auth(key),
            None => builder,
        }
    }

    fn streaming(&self) -> bool {
        self.streaming
    }

    fn requires_alternating_roles(&self) -> bool {
        self.strict_alternating_roles
    }
}

/// Model adapter that connects the agentkit agent loop to a vLLM server.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_loop::Agent;
/// use agentkit_provider_vllm::{VllmAdapter, VllmConfig};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let adapter = VllmAdapter::new(
///     VllmConfig::new("meta-llama/Llama-3.1-8B-Instruct"),
/// )?;
///
/// let agent = Agent::builder()
///     .model(adapter)
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct VllmAdapter(CompletionsAdapter<VllmProvider>);

/// An active session with a vLLM server.
pub type VllmSession = CompletionsSession<VllmProvider>;

/// A completed turn from a vLLM server.
pub type VllmTurn = CompletionsTurn;

impl VllmAdapter {
    /// Creates a new adapter from the given configuration.
    pub fn new(config: VllmConfig) -> Result<Self, VllmError> {
        let provider = VllmProvider::from(config);
        Ok(Self(CompletionsAdapter::new(provider)?))
    }
}

#[async_trait]
impl ModelAdapter for VllmAdapter {
    type Session = VllmSession;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        self.0.start_session(config).await
    }
}

/// Errors produced by the vLLM adapter.
#[derive(Debug, Error)]
pub enum VllmError {
    /// A required environment variable is not set.
    #[error("missing environment variable {0}")]
    MissingEnv(&'static str),

    /// An error from the generic completions adapter.
    #[error(transparent)]
    Completions(#[from] CompletionsError),
}
