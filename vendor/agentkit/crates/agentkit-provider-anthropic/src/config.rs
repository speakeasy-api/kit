use serde_json::{Value, json};

use crate::error::AnthropicError;
use crate::server_tool::ServerToolHandle;

/// Default Messages API endpoint.
pub const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
/// Default `anthropic-version` header.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";

/// Extended thinking configuration.
#[derive(Clone, Debug)]
pub enum ThinkingConfig {
    /// Disable extended thinking explicitly.
    Disabled,
    /// Enable extended thinking with a fixed token budget.
    Enabled {
        /// Upper bound on thinking tokens for this turn.
        budget_tokens: u32,
    },
    /// Let the model decide how much to think (supported models only).
    Adaptive,
}

impl ThinkingConfig {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Disabled => json!({ "type": "disabled" }),
            Self::Enabled { budget_tokens } => {
                json!({ "type": "enabled", "budget_tokens": budget_tokens })
            }
            Self::Adaptive => json!({ "type": "adaptive" }),
        }
    }
}

/// Priority/standard routing hint.
#[derive(Clone, Copy, Debug)]
pub enum ServiceTier {
    /// Use priority capacity if available, fall back to standard.
    Auto,
    /// Reject the request rather than fall back to standard capacity.
    StandardOnly,
}

impl ServiceTier {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::StandardOnly => "standard_only",
        }
    }
}

/// Constraint applied to the model's tool-choice behaviour.
#[derive(Clone, Debug)]
pub enum ToolChoice {
    /// Model decides freely whether to call a tool.
    Auto,
    /// Model MUST call exactly one tool.
    Any,
    /// Model MUST NOT call any tool.
    None,
    /// Model MUST call this specific tool.
    Tool {
        /// Name of the tool to force.
        name: String,
    },
}

impl ToolChoice {
    pub(crate) fn to_json(&self, disable_parallel: Option<bool>) -> Value {
        let mut obj = match self {
            Self::Auto => json!({ "type": "auto" }),
            Self::Any => json!({ "type": "any" }),
            Self::None => json!({ "type": "none" }),
            Self::Tool { name } => json!({ "type": "tool", "name": name }),
        };
        if let Some(flag) = disable_parallel
            && let Some(obj) = obj.as_object_mut()
        {
            obj.insert("disable_parallel_tool_use".into(), Value::Bool(flag));
        }
        obj
    }
}

/// Structured output format constraint.
#[derive(Clone, Debug)]
pub enum OutputFormat {
    /// Constrain output to a JSON Schema.
    JsonSchema {
        /// The JSON Schema the model must satisfy.
        schema: Value,
    },
}

impl OutputFormat {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::JsonSchema { schema } => json!({
                "type": "json_schema",
                "schema": schema,
            }),
        }
    }
}

/// Relative reasoning effort hint.
#[derive(Clone, Copy, Debug)]
pub enum OutputEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl OutputEffort {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// MCP server descriptor passed through to Anthropic's `mcp_servers` array.
///
/// Stored as opaque `Value` so that new MCP-server fields land without requiring
/// a provider-crate release.
#[derive(Clone, Debug)]
pub struct AnthropicMcpServer(pub Value);

/// Configuration for connecting to the Anthropic Messages API.
///
/// Build one with either [`AnthropicConfig::new`] (API key) or
/// [`AnthropicConfig::with_auth_token`] (bearer token), or via
/// [`AnthropicConfig::from_env`]. `max_tokens` is a required constructor
/// argument — the Messages API rejects requests without it.
#[derive(Clone)]
pub struct AnthropicConfig {
    /// Anthropic API key (`x-api-key` header).
    pub api_key: Option<String>,
    /// OAuth / bearer token; if set, takes precedence over `api_key`.
    pub auth_token: Option<String>,

    /// Endpoint URL. Defaults to the Anthropic production endpoint.
    pub base_url: String,
    /// Value for the `anthropic-version` header.
    pub anthropic_version: String,
    /// Additional `anthropic-beta` flags to enable.
    pub anthropic_beta: Vec<String>,

    /// Model identifier, e.g. `"claude-opus-4-7"`.
    pub model: String,
    /// Maximum number of tokens the model may generate (required by the API).
    pub max_tokens: u32,

    /// Sampling temperature (0.0–1.0).
    pub temperature: Option<f32>,
    /// Nucleus sampling parameter.
    pub top_p: Option<f32>,
    /// Top-k sampling parameter.
    pub top_k: Option<u32>,
    /// Custom stop sequences.
    pub stop_sequences: Option<Vec<String>>,

    /// Extended thinking configuration.
    pub thinking: Option<ThinkingConfig>,

    /// Priority vs standard capacity routing.
    pub service_tier: Option<ServiceTier>,
    /// Value for `metadata.user_id` in requests.
    pub metadata_user_id: Option<String>,

    /// Forces, restricts, or disables tool-choice behaviour.
    pub tool_choice: Option<ToolChoice>,
    /// If set, overrides the API's default of allowing parallel tool use.
    ///
    /// `Some(true)` disables parallel tool use. Folded into the `tool_choice`
    /// object rather than set as a top-level field.
    pub disable_parallel_tool_use: Option<bool>,
    /// Anthropic-run server tools (web search, code execution, etc.).
    pub server_tools: Vec<ServerToolHandle>,
    /// Pre-existing container identifier for code-execution sessions.
    pub container: Option<String>,

    /// Structured output shape.
    pub output_format: Option<OutputFormat>,
    /// Reasoning effort hint for structured output.
    pub output_effort: Option<OutputEffort>,

    /// MCP servers passed through to Anthropic's request body verbatim.
    pub mcp_servers: Vec<AnthropicMcpServer>,

    /// Whether to request a streaming SSE response from the Messages API.
    ///
    /// Defaults to `true`. Streaming yields incremental `ModelTurnEvent`s as
    /// the model generates, enabling responsive UIs; when disabled the adapter
    /// buffers the full JSON response before emitting any events. Opt out via
    /// [`AnthropicConfig::with_streaming`] for debugging or when an upstream
    /// proxy doesn't forward SSE bodies.
    pub streaming: bool,
}

impl AnthropicConfig {
    /// Creates a new configuration using an API key.
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        max_tokens: u32,
    ) -> Result<Self, AnthropicError> {
        if max_tokens == 0 {
            return Err(AnthropicError::InvalidMaxTokens);
        }
        Ok(Self {
            api_key: Some(api_key.into()),
            auth_token: None,
            base_url: DEFAULT_ENDPOINT.into(),
            anthropic_version: DEFAULT_ANTHROPIC_VERSION.into(),
            anthropic_beta: Vec::new(),
            model: model.into(),
            max_tokens,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            thinking: None,
            service_tier: None,
            metadata_user_id: None,
            tool_choice: None,
            disable_parallel_tool_use: None,
            server_tools: Vec::new(),
            container: None,
            output_format: None,
            output_effort: None,
            mcp_servers: Vec::new(),
            streaming: true,
        })
    }

    /// Creates a new configuration using a bearer auth token.
    pub fn with_auth_token(
        auth_token: impl Into<String>,
        model: impl Into<String>,
        max_tokens: u32,
    ) -> Result<Self, AnthropicError> {
        if max_tokens == 0 {
            return Err(AnthropicError::InvalidMaxTokens);
        }
        Ok(Self {
            api_key: None,
            auth_token: Some(auth_token.into()),
            base_url: DEFAULT_ENDPOINT.into(),
            anthropic_version: DEFAULT_ANTHROPIC_VERSION.into(),
            anthropic_beta: Vec::new(),
            model: model.into(),
            max_tokens,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            thinking: None,
            service_tier: None,
            metadata_user_id: None,
            tool_choice: None,
            disable_parallel_tool_use: None,
            server_tools: Vec::new(),
            container: None,
            output_format: None,
            output_effort: None,
            mcp_servers: Vec::new(),
            streaming: true,
        })
    }

    /// Builds a configuration from environment variables.
    ///
    /// | Variable | Required | Default |
    /// |---|---|---|
    /// | `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` | one | — |
    /// | `ANTHROPIC_MODEL` | yes | — |
    /// | `ANTHROPIC_MAX_TOKENS` | yes | — |
    /// | `ANTHROPIC_BASE_URL` | no | `https://api.anthropic.com/v1/messages` |
    /// | `ANTHROPIC_VERSION` | no | `2023-06-01` |
    /// | `ANTHROPIC_BETA` | no | comma-separated flag list |
    pub fn from_env() -> Result<Self, AnthropicError> {
        let model = std::env::var("ANTHROPIC_MODEL")
            .map_err(|_| AnthropicError::MissingEnv("ANTHROPIC_MODEL"))?;
        let max_tokens: u32 = std::env::var("ANTHROPIC_MAX_TOKENS")
            .map_err(|_| AnthropicError::MissingEnv("ANTHROPIC_MAX_TOKENS"))?
            .parse()
            .map_err(|_| AnthropicError::MissingEnv("ANTHROPIC_MAX_TOKENS"))?;

        let mut config = match (
            std::env::var("ANTHROPIC_AUTH_TOKEN").ok(),
            std::env::var("ANTHROPIC_API_KEY").ok(),
        ) {
            (Some(token), _) => Self::with_auth_token(token, model, max_tokens)?,
            (None, Some(key)) => Self::new(key, model, max_tokens)?,
            (None, None) => return Err(AnthropicError::MissingCredentials),
        };

        if let Ok(url) = std::env::var("ANTHROPIC_BASE_URL") {
            config = config.with_base_url(url);
        }
        if let Ok(ver) = std::env::var("ANTHROPIC_VERSION") {
            config = config.with_anthropic_version(ver);
        }
        if let Ok(betas) = std::env::var("ANTHROPIC_BETA") {
            for flag in betas.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                config = config.with_beta(flag.to_string());
            }
        }

        Ok(config)
    }

    // --- Builder methods ---

    /// Overrides the endpoint URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Overrides the `anthropic-version` header value.
    pub fn with_anthropic_version(mut self, v: impl Into<String>) -> Self {
        self.anthropic_version = v.into();
        self
    }

    /// Adds a single `anthropic-beta` flag.
    pub fn with_beta(mut self, flag: impl Into<String>) -> Self {
        self.anthropic_beta.push(flag.into());
        self
    }

    /// Sets the sampling temperature (0.0 = deterministic).
    pub fn with_temperature(mut self, v: f32) -> Self {
        self.temperature = Some(v);
        self
    }

    /// Sets the nucleus sampling parameter.
    pub fn with_top_p(mut self, v: f32) -> Self {
        self.top_p = Some(v);
        self
    }

    /// Sets the top-k sampling parameter.
    pub fn with_top_k(mut self, v: u32) -> Self {
        self.top_k = Some(v);
        self
    }

    /// Replaces the stop-sequence list.
    pub fn with_stop_sequences(mut self, sequences: impl IntoIterator<Item = String>) -> Self {
        self.stop_sequences = Some(sequences.into_iter().collect());
        self
    }

    /// Sets the extended-thinking configuration.
    pub fn with_thinking(mut self, thinking: ThinkingConfig) -> Self {
        self.thinking = Some(thinking);
        self
    }

    /// Sets the priority/standard routing hint.
    pub fn with_service_tier(mut self, tier: ServiceTier) -> Self {
        self.service_tier = Some(tier);
        self
    }

    /// Sets the `metadata.user_id` value.
    pub fn with_metadata_user_id(mut self, user_id: impl Into<String>) -> Self {
        self.metadata_user_id = Some(user_id.into());
        self
    }

    /// Sets the tool-choice constraint.
    pub fn with_tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    /// Disables parallel tool use (API default is to allow it).
    pub fn disable_parallel_tool_use(mut self, flag: bool) -> Self {
        self.disable_parallel_tool_use = Some(flag);
        self
    }

    /// Appends a server tool to the configuration.
    pub fn with_server_tool(mut self, tool: ServerToolHandle) -> Self {
        self.server_tools.push(tool);
        self
    }

    /// Sets the container identifier for code-execution sessions.
    pub fn with_container(mut self, id: impl Into<String>) -> Self {
        self.container = Some(id.into());
        self
    }

    /// Sets the structured output format.
    pub fn with_output_format(mut self, format: OutputFormat) -> Self {
        self.output_format = Some(format);
        self
    }

    /// Sets the reasoning-effort hint.
    pub fn with_output_effort(mut self, effort: OutputEffort) -> Self {
        self.output_effort = Some(effort);
        self
    }

    /// Appends an MCP server descriptor.
    pub fn with_mcp_server(mut self, server: AnthropicMcpServer) -> Self {
        self.mcp_servers.push(server);
        self
    }

    /// Toggles SSE streaming of model responses.
    ///
    /// Streaming is on by default; pass `false` to fall back to the buffered
    /// non-streaming path (the request body will be sent with `stream: false`
    /// and the full response parsed once the Messages API returns).
    pub fn with_streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }
}
