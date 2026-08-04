//! OpenAI model adapter for the agentkit agent loop.
//!
//! This crate provides [`OpenAIAdapter`] and [`OpenAIConfig`] for connecting
//! the agent loop to the [OpenAI](https://platform.openai.com) chat completions
//! API. It is built on the generic [`agentkit_adapter_completions`] crate.
//!
//! # Quick start
//!
//! ```rust,ignore
//! use agentkit_loop::{Agent, SessionConfig};
//! use agentkit_provider_openai::{OpenAIAdapter, OpenAIConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = OpenAIConfig::from_env()?;
//!     let adapter = OpenAIAdapter::new(config)?;
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
use agentkit_core::{MetadataMap, Usage};
use agentkit_loop::{
    LoopError, ModelAdapter, PromptCacheMode, PromptCacheRetention, PromptCacheStrategy,
    SessionConfig, TurnRequest,
};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use std::fmt;
use thiserror::Error;

const DEFAULT_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";

/// Configuration for connecting to the OpenAI API.
///
/// Holds credentials, model selection, and optional request parameters.
/// Build one with [`OpenAIConfig::new`] for explicit values, or
/// [`OpenAIConfig::from_env`] to read from environment variables.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_provider_openai::OpenAIConfig;
///
/// let config = OpenAIConfig::new("sk-...", "gpt-4o")
///     .with_temperature(0.0)
///     .with_max_completion_tokens(4096);
/// ```
#[derive(Clone)]
pub struct OpenAIConfig {
    /// OpenAI API key (starts with `sk-`).
    pub api_key: String,
    /// Model identifier, e.g. `"gpt-4o"` or `"gpt-4o-mini"`.
    pub model: String,
    /// Chat completions endpoint URL. Defaults to the OpenAI production URL.
    pub base_url: String,
    /// Sampling temperature (0.0 = deterministic, higher = more creative).
    pub temperature: Option<f32>,
    /// Maximum number of completion tokens the model may generate.
    pub max_completion_tokens: Option<u32>,
    /// Nucleus sampling parameter.
    pub top_p: Option<f32>,
    /// Penalizes tokens based on how often they appear in the output so far.
    pub frequency_penalty: Option<f32>,
    /// Penalizes tokens based on whether they have appeared at all.
    pub presence_penalty: Option<f32>,
    /// Whether the model is allowed to emit multiple tool calls in a
    /// single turn. Omitted from the request when `None` so OpenAI's
    /// per-model default applies.
    pub parallel_tool_calls: Option<bool>,
    /// Request SSE streaming responses. Defaults to `true`.
    pub streaming: bool,
}

impl fmt::Debug for OpenAIConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAIConfig")
            .field("api_key", &"[REDACTED]")
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("temperature", &self.temperature)
            .field("max_completion_tokens", &self.max_completion_tokens)
            .field("top_p", &self.top_p)
            .field("frequency_penalty", &self.frequency_penalty)
            .field("presence_penalty", &self.presence_penalty)
            .field("parallel_tool_calls", &self.parallel_tool_calls)
            .field("streaming", &self.streaming)
            .finish()
    }
}

impl OpenAIConfig {
    /// Creates a new configuration with the given API key and model identifier.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            base_url: DEFAULT_ENDPOINT.into(),
            temperature: None,
            max_completion_tokens: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            parallel_tool_calls: None,
            streaming: true,
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
    pub fn with_max_completion_tokens(mut self, v: u32) -> Self {
        self.max_completion_tokens = Some(v);
        self
    }

    /// Sets the nucleus sampling parameter.
    pub fn with_top_p(mut self, v: f32) -> Self {
        self.top_p = Some(v);
        self
    }

    /// Sets the frequency penalty (penalizes repeated tokens).
    pub fn with_frequency_penalty(mut self, v: f32) -> Self {
        self.frequency_penalty = Some(v);
        self
    }

    /// Sets the presence penalty (penalizes tokens that have already appeared).
    pub fn with_presence_penalty(mut self, v: f32) -> Self {
        self.presence_penalty = Some(v);
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

    /// Builds a configuration from environment variables.
    ///
    /// Reads the following variables:
    ///
    /// | Variable | Required | Default |
    /// |---|---|---|
    /// | `OPENAI_API_KEY` | yes | -- |
    /// | `OPENAI_MODEL` | no | `gpt-4o` |
    /// | `OPENAI_BASE_URL` | no | `https://api.openai.com/v1/chat/completions` |
    pub fn from_env() -> Result<Self, OpenAIError> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| OpenAIError::MissingEnv("OPENAI_API_KEY"))?;
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-4o".into());

        let mut config = Self::new(api_key, model);

        if let Ok(url) = std::env::var("OPENAI_BASE_URL") {
            config = config.with_base_url(url);
        }

        Ok(config)
    }
}

/// Request parameters serialized into the OpenAI request body.
#[derive(Clone, Debug, Serialize)]
pub struct OpenAIRequestConfig {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

/// The OpenAI provider, implementing [`CompletionsProvider`].
#[derive(Clone)]
pub struct OpenAIProvider {
    api_key: String,
    base_url: String,
    streaming: bool,
    request_config: OpenAIRequestConfig,
}

impl fmt::Debug for OpenAIProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAIProvider")
            .field("api_key", &"[REDACTED]")
            .field("base_url", &self.base_url)
            .field("streaming", &self.streaming)
            .field("request_config", &self.request_config)
            .finish()
    }
}

impl From<OpenAIConfig> for OpenAIProvider {
    fn from(config: OpenAIConfig) -> Self {
        Self {
            api_key: config.api_key,
            base_url: config.base_url,
            streaming: config.streaming,
            request_config: OpenAIRequestConfig {
                model: config.model,
                temperature: config.temperature,
                max_completion_tokens: config.max_completion_tokens,
                top_p: config.top_p,
                frequency_penalty: config.frequency_penalty,
                presence_penalty: config.presence_penalty,
                parallel_tool_calls: config.parallel_tool_calls,
            },
        }
    }
}

impl CompletionsProvider for OpenAIProvider {
    type Config = OpenAIRequestConfig;

    fn provider_name(&self) -> &str {
        "OpenAI"
    }
    fn endpoint_url(&self) -> &str {
        &self.base_url
    }
    fn config(&self) -> &OpenAIRequestConfig {
        &self.request_config
    }

    fn apply_generation_controls(
        &self,
        body: &mut serde_json::Map<String, Value>,
        controls: &agentkit_loop::GenerationControls,
    ) -> Result<(), LoopError> {
        if let Some(maximum) = controls.max_output_tokens {
            if maximum == 0
                || self
                    .request_config
                    .max_completion_tokens
                    .is_some_and(|cap| maximum > cap)
            {
                return Err(LoopError::Unsupported(
                    "OpenAI max output tokens exceed the configured provider cap".into(),
                ));
            }
            body.insert("max_completion_tokens".into(), Value::from(maximum));
        }
        if let Some(temperature) = controls.temperature {
            if !temperature.is_finite() || !(0.0..=2.0).contains(&temperature) {
                return Err(LoopError::Unsupported(
                    "OpenAI temperature is outside the supported range".into(),
                ));
            }
            body.insert("temperature".into(), serde_json::json!(temperature));
        }
        if let Some(stops) = &controls.stop_sequences {
            body.insert(
                "stop".into(),
                Value::Array(stops.iter().cloned().map(Value::String).collect()),
            );
        }
        Ok(())
    }

    fn preprocess_request(
        &self,
        builder: agentkit_http::HttpRequestBuilder,
    ) -> agentkit_http::HttpRequestBuilder {
        builder.bearer_auth(&self.api_key).header(
            "User-Agent",
            concat!("agentkit-provider-openai/", env!("CARGO_PKG_VERSION")),
        )
    }

    fn streaming(&self) -> bool {
        self.streaming
    }

    fn apply_stream_options(
        &self,
        body: &mut serde_json::Map<String, Value>,
    ) -> Result<(), LoopError> {
        body.entry("stream_options")
            .or_insert_with(|| serde_json::json!({ "include_usage": true }));
        Ok(())
    }

    fn apply_prompt_cache(
        &self,
        body: &mut serde_json::Map<String, Value>,
        request: &TurnRequest,
    ) -> Result<(), LoopError> {
        let Some(cache) = &request.cache else {
            return Ok(());
        };
        if matches!(cache.mode, PromptCacheMode::Disabled) {
            return Ok(());
        }

        if let Some(key) = &cache.key {
            body.insert("prompt_cache_key".into(), Value::String(key.clone()));
        }

        if let Some(retention) = cache.retention {
            let value = match retention {
                PromptCacheRetention::Default | PromptCacheRetention::Short => "in_memory",
                PromptCacheRetention::Extended => "24h",
            };
            body.insert("prompt_cache_retention".into(), Value::String(value.into()));
        }

        if matches!(cache.strategy, PromptCacheStrategy::Explicit { .. })
            && matches!(cache.mode, PromptCacheMode::Required)
        {
            return Err(LoopError::Provider(
                "OpenAI chat completions does not support explicit cache breakpoints".into(),
            ));
        }

        Ok(())
    }

    fn postprocess_response(
        &self,
        _usage: &mut Option<Usage>,
        metadata: &mut MetadataMap,
        raw_response: &Value,
    ) {
        if let Some(model) = raw_response.get("model").and_then(Value::as_str) {
            metadata.insert("openai.model".into(), Value::String(model.into()));
        }
        if let Some(fingerprint) = raw_response
            .get("system_fingerprint")
            .and_then(Value::as_str)
        {
            metadata.insert(
                "openai.system_fingerprint".into(),
                Value::String(fingerprint.into()),
            );
        }
    }
}

/// Model adapter that connects the agentkit agent loop to OpenAI.
///
/// # Example
///
/// ```rust,no_run
/// use agentkit_loop::Agent;
/// use agentkit_provider_openai::{OpenAIAdapter, OpenAIConfig};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let adapter = OpenAIAdapter::new(OpenAIConfig::from_env()?)?;
///
/// let agent = Agent::builder()
///     .model(adapter)
///     .build()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct OpenAIAdapter(CompletionsAdapter<OpenAIProvider>);

/// An active session with the OpenAI API.
pub type OpenAISession = CompletionsSession<OpenAIProvider>;

/// A completed turn from the OpenAI API.
pub type OpenAITurn = CompletionsTurn;

impl OpenAIAdapter {
    /// Creates a new adapter from the given configuration.
    pub fn new(config: OpenAIConfig) -> Result<Self, OpenAIError> {
        let provider = OpenAIProvider::from(config);
        Ok(Self(CompletionsAdapter::new(provider)?))
    }

    /// Configured provider output-token ceiling, if one was materialized.
    pub fn max_output_tokens(&self) -> Option<u32> {
        self.0.provider_config().max_completion_tokens
    }
}

#[async_trait]
impl ModelAdapter for OpenAIAdapter {
    type Session = OpenAISession;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        self.0.start_session(config).await
    }

    fn provider_name(&self) -> Option<&str> {
        self.0.provider_name()
    }
}

/// Errors produced by the OpenAI adapter.
#[derive(Debug, Error)]
pub enum OpenAIError {
    /// A required environment variable is not set.
    #[error("missing environment variable {0}")]
    MissingEnv(&'static str),

    /// An error from the generic completions adapter.
    #[error(transparent)]
    Completions(#[from] CompletionsError),
}

#[cfg(test)]
mod tests {
    use agentkit_core::{MetadataMap, SessionId, TurnId};

    use super::*;

    #[test]
    fn credential_debug_is_redacted() {
        const CANARY: &str = "OPENAI_DEBUG_CANARY";
        let config = OpenAIConfig::new(CANARY, "gpt-test");
        let provider = OpenAIProvider::from(config.clone());

        for debug in [format!("{config:?}"), format!("{provider:?}")] {
            assert!(debug.contains("[REDACTED]"));
            assert!(!debug.contains(CANARY));
        }
    }

    fn empty_turn_request(cache: Option<agentkit_loop::PromptCacheRequest>) -> TurnRequest {
        TurnRequest {
            session_id: SessionId::new("session"),
            turn_id: TurnId::new("turn-1"),
            transcript: Vec::new(),
            available_tools: Vec::new(),
            cache,
            structured_output: None,
            generation: Default::default(),
            metadata: MetadataMap::new(),
        }
    }

    #[test]
    fn openai_maps_automatic_cache_request() {
        let provider = OpenAIProvider::from(OpenAIConfig::new("sk-test", "gpt-5.1"));
        let mut body = serde_json::Map::new();

        provider
            .apply_prompt_cache(
                &mut body,
                &empty_turn_request(Some(
                    agentkit_loop::PromptCacheRequest::best_effort(PromptCacheStrategy::Automatic)
                        .with_key("cache-key")
                        .with_retention(PromptCacheRetention::Extended),
                )),
            )
            .unwrap();

        assert_eq!(
            body.get("prompt_cache_key"),
            Some(&Value::String("cache-key".into()))
        );
        assert_eq!(
            body.get("prompt_cache_retention"),
            Some(&Value::String("24h".into()))
        );
    }

    #[test]
    fn per_turn_generation_controls_override_the_exact_request_fields() {
        let provider = OpenAIProvider::from(
            OpenAIConfig::new("sk-test", "gpt-test").with_max_completion_tokens(64),
        );
        let mut body = serde_json::Map::new();
        provider
            .apply_generation_controls(
                &mut body,
                &agentkit_loop::GenerationControls {
                    max_output_tokens: Some(32),
                    temperature: Some(0.25),
                    stop_sequences: Some(vec!["STOP".into()]),
                },
            )
            .unwrap();
        assert_eq!(body["max_completion_tokens"], 32);
        assert_eq!(body["temperature"], 0.25);
        assert_eq!(body["stop"], serde_json::json!(["STOP"]));
    }

    #[test]
    fn openai_rejects_required_explicit_breakpoints() {
        let provider = OpenAIProvider::from(OpenAIConfig::new("sk-test", "gpt-5.1"));
        let mut body = serde_json::Map::new();

        let error = provider
            .apply_prompt_cache(
                &mut body,
                &empty_turn_request(Some(agentkit_loop::PromptCacheRequest::required(
                    PromptCacheStrategy::Explicit {
                        breakpoints: vec![
                            agentkit_loop::PromptCacheBreakpoint::TranscriptItemEnd { index: 0 },
                        ],
                    },
                ))),
            )
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("does not support explicit cache breakpoints")
        );
    }
}
