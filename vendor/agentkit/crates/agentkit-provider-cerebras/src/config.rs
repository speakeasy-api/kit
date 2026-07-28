//! `CerebrasConfig` and its leaf enums (tool-choice, output format, reasoning,
//! service tier, prediction, compression).
//!
//! All validation lives on the builder so invalid configurations fail fast
//! before a request body is ever assembled.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::error::BuildError;

/// Default Cerebras Inference API base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.cerebras.ai/v1";

/// Default for `X-Cerebras-Version-Patch`. Unset until the caller opts in.
pub const DEFAULT_VERSION_PATCH: Option<u32> = None;

/// Short name for `agentkit_core::PartKind`, kept local so the `BuildError`
/// enum does not leak the full core type through its signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PartKindName {
    /// Text part.
    Text,
    /// Media (image, audio) part.
    Media,
    /// File attachment.
    File,
    /// Structured JSON value part.
    Structured,
    /// Reasoning / thinking trace.
    Reasoning,
    /// Tool invocation.
    ToolCall,
    /// Tool result.
    ToolResult,
    /// Provider-specific opaque part.
    Custom,
}

impl From<agentkit_core::PartKind> for PartKindName {
    fn from(value: agentkit_core::PartKind) -> Self {
        match value {
            agentkit_core::PartKind::Text => PartKindName::Text,
            agentkit_core::PartKind::Media => PartKindName::Media,
            agentkit_core::PartKind::File => PartKindName::File,
            agentkit_core::PartKind::Structured => PartKindName::Structured,
            agentkit_core::PartKind::Reasoning => PartKindName::Reasoning,
            agentkit_core::PartKind::ToolCall => PartKindName::ToolCall,
            agentkit_core::PartKind::ToolResult => PartKindName::ToolResult,
            agentkit_core::PartKind::Custom => PartKindName::Custom,
        }
    }
}

/// Tool-choice constraint mirroring the Cerebras API.
#[derive(Clone, Debug)]
pub enum ToolChoice {
    /// Disables tool calling even when tools are attached.
    None,
    /// Model decides whether to call a tool (API default when tools present).
    Auto,
    /// Model MUST emit at least one tool call.
    Required,
    /// Model MUST call this specific tool.
    Function {
        /// Name of the tool to force.
        name: String,
    },
}

impl ToolChoice {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::None => Value::String("none".into()),
            Self::Auto => Value::String("auto".into()),
            Self::Required => Value::String("required".into()),
            Self::Function { name } => json!({
                "type": "function",
                "function": { "name": name },
            }),
        }
    }
}

/// Structured-output shape.
#[derive(Clone, Debug)]
pub enum OutputFormat {
    /// Free-form text (API default).
    Text,
    /// Arbitrary JSON object — no schema attached.
    JsonObject,
    /// JSON that validates against the given schema.
    JsonSchema {
        /// The schema document.
        schema: Value,
        /// When `true`, the model will strictly honour every schema rule;
        /// additionalProperties are forbidden and the root must be an object.
        strict: bool,
        /// Optional schema name forwarded to the API as
        /// `json_schema.name`.
        name: Option<String>,
    },
}

impl OutputFormat {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Text => json!({ "type": "text" }),
            Self::JsonObject => json!({ "type": "json_object" }),
            Self::JsonSchema {
                schema,
                strict,
                name,
            } => {
                let mut inner = Map::new();
                if let Some(name) = name {
                    inner.insert("name".into(), Value::String(name.clone()));
                }
                inner.insert("strict".into(), Value::Bool(*strict));
                inner.insert("schema".into(), schema.clone());
                json!({
                    "type": "json_schema",
                    "json_schema": Value::Object(inner),
                })
            }
        }
    }
}

/// Reasoning-effort preset. Applies to `gpt-oss` models; `None` only to
/// `glm-4.7`.
#[derive(Clone, Copy, Debug)]
pub enum ReasoningEffort {
    /// Minimal reasoning.
    Low,
    /// Balanced reasoning.
    Medium,
    /// Maximum reasoning.
    High,
    /// No reasoning (glm-4.7 only).
    None,
}

impl ReasoningEffort {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::None => "none",
        }
    }
}

/// Reasoning-output format the model should emit.
#[derive(Clone, Copy, Debug)]
pub enum ReasoningFormat {
    /// Provider parses reasoning into a separate field.
    Parsed,
    /// Raw reasoning inside the content stream.
    Raw,
    /// Reasoning produced but not returned.
    Hidden,
    /// Opt-out of reasoning output.
    None,
}

impl ReasoningFormat {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Parsed => "parsed",
            Self::Raw => "raw",
            Self::Hidden => "hidden",
            Self::None => "none",
        }
    }
}

/// Reasoning configuration bundle. Individual fields pass through to the
/// Cerebras API verbatim so new models that accept the same knobs Just Work.
#[derive(Clone, Debug, Default)]
pub struct ReasoningConfig {
    /// `reasoning_effort`.
    pub effort: Option<ReasoningEffort>,
    /// `reasoning_format`.
    pub format: Option<ReasoningFormat>,
    /// `clear_thinking` (glm-4.7).
    pub clear_thinking: Option<bool>,
    /// `disable_reasoning` (deprecated 2026-07-21).
    pub disable_reasoning: Option<bool>,
}

impl ReasoningConfig {
    /// Empty configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets `reasoning_effort`.
    pub fn with_effort(mut self, effort: ReasoningEffort) -> Self {
        self.effort = Some(effort);
        self
    }

    /// Sets `reasoning_format`.
    pub fn with_format(mut self, format: ReasoningFormat) -> Self {
        self.format = Some(format);
        self
    }

    /// Sets `clear_thinking`.
    pub fn with_clear_thinking(mut self, flag: bool) -> Self {
        self.clear_thinking = Some(flag);
        self
    }

    /// Sets `disable_reasoning`.
    pub fn with_disable_reasoning(mut self, flag: bool) -> Self {
        self.disable_reasoning = Some(flag);
        self
    }

    /// Applies the configuration onto a request-body `Map`. Fields are passed
    /// through verbatim; model-specific whitelisting is the server's job.
    pub(crate) fn apply(&self, body: &mut Map<String, Value>) {
        if let Some(effort) = self.effort {
            body.insert(
                "reasoning_effort".into(),
                Value::String(effort.as_str().into()),
            );
        }
        if let Some(format) = self.format {
            body.insert(
                "reasoning_format".into(),
                Value::String(format.as_str().into()),
            );
        }
        if let Some(flag) = self.clear_thinking {
            body.insert("clear_thinking".into(), Value::Bool(flag));
        }
        if let Some(flag) = self.disable_reasoning {
            body.insert("disable_reasoning".into(), Value::Bool(flag));
        }
    }
}

/// Priority / capacity tier. Private preview — hidden behind
/// `feature = "service-tiers"`.
#[cfg(feature = "service-tiers")]
#[derive(Clone, Copy, Debug)]
pub enum ServiceTier {
    /// Priority capacity with strict queuing.
    Priority,
    /// Default capacity.
    Default,
    /// Auto: try priority, fall back to default.
    Auto,
    /// Flex tier (lowest cost, highest latency).
    Flex,
}

#[cfg(feature = "service-tiers")]
impl ServiceTier {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::Default => "default",
            Self::Auto => "auto",
            Self::Flex => "flex",
        }
    }
}

/// `queue_threshold` header value. Only meaningful on `flex`/`auto` tiers.
#[cfg(feature = "service-tiers")]
#[derive(Clone, Copy, Debug)]
pub struct QueueThreshold(pub u32);

/// Predicted-outputs configuration.
///
/// When provided the model uses the supplied content as a speculation hint.
/// Conflicts with `tools`, `logprobs`, and `n > 1`.
#[cfg(feature = "predicted-outputs")]
#[derive(Clone, Debug)]
pub enum Prediction {
    /// Literal content the model should try to predict.
    Content(String),
}

#[cfg(feature = "predicted-outputs")]
impl Prediction {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Content(text) => json!({ "type": "content", "content": text }),
        }
    }
}

/// Request-body encoding. Compression only affects the request path; responses
/// are always plain JSON.
#[cfg(feature = "compression")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestEncoding {
    /// `application/json` (no compression).
    Json,
    /// `application/vnd.msgpack`.
    Msgpack,
    /// `application/json` + `Content-Encoding: gzip`.
    JsonGzip,
    /// `application/vnd.msgpack` + `Content-Encoding: gzip`.
    MsgpackGzip,
}

/// Compression configuration paired with a minimum-size threshold below which
/// compression is skipped (msgpack+gzip overhead isn't worth paying for tiny
/// payloads).
#[cfg(feature = "compression")]
#[derive(Clone, Debug)]
pub struct CompressionConfig {
    /// Target encoding.
    pub encoding: RequestEncoding,
    /// Skip compression when the serialized JSON is smaller than this many
    /// bytes.
    pub min_bytes: usize,
}

#[cfg(feature = "compression")]
impl CompressionConfig {
    /// Convenience constructor — default threshold of 4 KiB.
    pub fn new(encoding: RequestEncoding) -> Self {
        Self {
            encoding,
            min_bytes: 4096,
        }
    }
}

/// Full configuration for a Cerebras adapter.
///
/// Build one with [`CerebrasConfig::new`] or [`CerebrasConfig::from_env`],
/// then refine via the `with_*` methods.
#[derive(Clone, Debug)]
pub struct CerebrasConfig {
    // --- auth & transport ---
    /// `Authorization: Bearer <api_key>`.
    pub api_key: String,
    /// Endpoint base URL.
    pub base_url: String,
    /// `X-Cerebras-Version-Patch` header. `None` opts into the current API
    /// default.
    pub version_patch: Option<u32>,
    /// Extra request headers appended after the crate-managed set.
    pub extra_headers: Vec<(String, String)>,
    /// JSON object deep-merged into the request body after all typed fields
    /// have been applied. Forward-compat escape hatch.
    pub extra_body: Option<Value>,

    // --- model ---
    /// Model identifier (e.g. `"gpt-oss-120b"`).
    pub model: String,
    /// Upper bound on output tokens.
    pub max_completion_tokens: Option<u32>,
    /// Minimum output tokens. `-1` is a documented sentinel meaning
    /// "full-sequence length".
    pub min_tokens: Option<i32>,

    // --- sampling ---
    /// Sampling temperature (0.0..=2.0).
    pub temperature: Option<f32>,
    /// Nucleus sampling parameter.
    pub top_p: Option<f32>,
    /// Repetition penalty (frequency).
    pub frequency_penalty: Option<f32>,
    /// Repetition penalty (presence).
    pub presence_penalty: Option<f32>,
    /// Stop sequences. Max 4 per API docs.
    pub stop: Option<Vec<String>>,
    /// RNG seed.
    pub seed: Option<i64>,
    /// Per-token logit biases.
    pub logit_bias: Option<BTreeMap<String, i32>>,
    /// Request logprobs with the response.
    pub logprobs: Option<bool>,
    /// Top-N logprobs per token (requires `logprobs = true`).
    pub top_logprobs: Option<u32>,
    /// Opaque end-user identifier forwarded as `user`.
    pub user: Option<String>,

    // --- tools ---
    /// Tool-choice constraint. `None` (field, not variant) = API default.
    pub tool_choice: Option<ToolChoice>,
    /// Toggle parallel tool calls (default: allowed).
    pub parallel_tool_calls: Option<bool>,
    /// Whether synthesised function tools should have `strict: true`.
    pub tool_strict: bool,

    // --- output ---
    /// Structured output shape.
    pub output_format: Option<OutputFormat>,

    // --- reasoning ---
    /// Reasoning effort / format / clear-thinking / disable flag.
    pub reasoning: Option<ReasoningConfig>,

    // --- stream ---
    /// Request SSE streaming. Defaults to `true`.
    pub streaming: bool,

    // --- preview-gated ---
    /// Predicted-outputs configuration.
    #[cfg(feature = "predicted-outputs")]
    pub prediction: Option<Prediction>,

    /// Requested service tier.
    #[cfg(feature = "service-tiers")]
    pub service_tier: Option<ServiceTier>,

    /// `queue_threshold` header value in milliseconds.
    #[cfg(feature = "service-tiers")]
    pub queue_threshold_ms: Option<u32>,

    /// Compression configuration.
    #[cfg(feature = "compression")]
    pub compression: Option<CompressionConfig>,
}

impl CerebrasConfig {
    /// Creates a new configuration with the given API key and model.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self, BuildError> {
        let api_key = api_key.into();
        if api_key.is_empty() {
            return Err(BuildError::MissingEnv("CEREBRAS_API_KEY"));
        }
        Ok(Self {
            api_key,
            base_url: DEFAULT_BASE_URL.into(),
            version_patch: DEFAULT_VERSION_PATCH,
            extra_headers: Vec::new(),
            extra_body: None,
            model: model.into(),
            max_completion_tokens: None,
            min_tokens: None,
            temperature: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            stop: None,
            seed: None,
            logit_bias: None,
            logprobs: None,
            top_logprobs: None,
            user: None,
            tool_choice: None,
            parallel_tool_calls: None,
            tool_strict: false,
            output_format: None,
            reasoning: None,
            streaming: true,
            #[cfg(feature = "predicted-outputs")]
            prediction: None,
            #[cfg(feature = "service-tiers")]
            service_tier: None,
            #[cfg(feature = "service-tiers")]
            queue_threshold_ms: None,
            #[cfg(feature = "compression")]
            compression: None,
        })
    }

    /// Builds a configuration from environment variables.
    ///
    /// | Variable | Required | Default |
    /// |---|---|---|
    /// | `CEREBRAS_API_KEY` | yes | — |
    /// | `CEREBRAS_MODEL` | yes | — |
    /// | `CEREBRAS_BASE_URL` | no | `https://api.cerebras.ai/v1` |
    /// | `CEREBRAS_VERSION_PATCH` | no | — |
    /// | `CEREBRAS_MAX_COMPLETION_TOKENS` | no | — |
    pub fn from_env() -> Result<Self, BuildError> {
        let api_key = std::env::var("CEREBRAS_API_KEY")
            .map_err(|_| BuildError::MissingEnv("CEREBRAS_API_KEY"))?;
        let model = std::env::var("CEREBRAS_MODEL")
            .map_err(|_| BuildError::MissingEnv("CEREBRAS_MODEL"))?;
        let mut config = Self::new(api_key, model)?;
        if let Ok(url) = std::env::var("CEREBRAS_BASE_URL") {
            config.base_url = url;
        }
        if let Ok(patch) = std::env::var("CEREBRAS_VERSION_PATCH") {
            config.version_patch = Some(
                patch
                    .parse()
                    .map_err(|_| BuildError::MissingEnv("CEREBRAS_VERSION_PATCH"))?,
            );
        }
        if let Ok(max) = std::env::var("CEREBRAS_MAX_COMPLETION_TOKENS") {
            config.max_completion_tokens = Some(
                max.parse()
                    .map_err(|_| BuildError::MissingEnv("CEREBRAS_MAX_COMPLETION_TOKENS"))?,
            );
        }
        config.validate()?;
        Ok(config)
    }

    /// Runs every validation rule. Called automatically from `from_env`; call
    /// it yourself after mutating fields directly.
    pub fn validate(&self) -> Result<(), BuildError> {
        if let Some(t) = self.temperature
            && !(0.0..=2.0).contains(&t)
        {
            return Err(BuildError::OutOfRange {
                field: "temperature",
                message: format!("{t} not in 0.0..=2.0"),
            });
        }
        if let Some(fp) = self.frequency_penalty
            && !(-2.0..=2.0).contains(&fp)
        {
            return Err(BuildError::OutOfRange {
                field: "frequency_penalty",
                message: format!("{fp} not in -2.0..=2.0"),
            });
        }
        if let Some(pp) = self.presence_penalty
            && !(-2.0..=2.0).contains(&pp)
        {
            return Err(BuildError::OutOfRange {
                field: "presence_penalty",
                message: format!("{pp} not in -2.0..=2.0"),
            });
        }
        if let Some(stops) = &self.stop
            && stops.len() > 4
        {
            return Err(BuildError::OutOfRange {
                field: "stop",
                message: format!("{} > 4 sequences", stops.len()),
            });
        }
        if let Some(min) = self.min_tokens
            && min < -1
        {
            return Err(BuildError::OutOfRange {
                field: "min_tokens",
                message: format!("{min} < -1 (sentinel)"),
            });
        }
        if let Some(tl) = self.top_logprobs {
            if tl > 20 {
                return Err(BuildError::OutOfRange {
                    field: "top_logprobs",
                    message: format!("{tl} > 20"),
                });
            }
            if !matches!(self.logprobs, Some(true)) {
                return Err(BuildError::TopLogprobsWithoutLogprobs);
            }
        }
        #[cfg(feature = "service-tiers")]
        if let Some(ms) = self.queue_threshold_ms
            && !(50..=20_000).contains(&ms)
        {
            return Err(BuildError::OutOfRange {
                field: "queue_threshold_ms",
                message: format!("{ms} not in 50..=20000"),
            });
        }
        Ok(())
    }

    // --- Builder methods ---

    /// Overrides the endpoint base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = url.into();
        self
    }

    /// Sets `X-Cerebras-Version-Patch`.
    pub fn with_version_patch(mut self, v: u32) -> Self {
        self.version_patch = Some(v);
        self
    }

    /// Appends an extra header. Clones the existing list, adds, returns.
    pub fn with_extra_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((key.into(), value.into()));
        self
    }

    /// Replaces the extra-body passthrough.
    pub fn with_extra_body(mut self, body: Value) -> Self {
        self.extra_body = Some(body);
        self
    }

    /// Sets `max_completion_tokens`.
    pub fn with_max_completion_tokens(mut self, v: u32) -> Self {
        self.max_completion_tokens = Some(v);
        self
    }

    /// Sets `min_tokens`. `-1` is the documented sentinel.
    pub fn with_min_tokens(mut self, v: i32) -> Self {
        self.min_tokens = Some(v);
        self
    }

    /// Sets sampling temperature.
    pub fn with_temperature(mut self, v: f32) -> Self {
        self.temperature = Some(v);
        self
    }

    /// Sets top_p.
    pub fn with_top_p(mut self, v: f32) -> Self {
        self.top_p = Some(v);
        self
    }

    /// Sets frequency_penalty.
    pub fn with_frequency_penalty(mut self, v: f32) -> Self {
        self.frequency_penalty = Some(v);
        self
    }

    /// Sets presence_penalty.
    pub fn with_presence_penalty(mut self, v: f32) -> Self {
        self.presence_penalty = Some(v);
        self
    }

    /// Replaces stop sequences.
    pub fn with_stop(mut self, stops: impl IntoIterator<Item = String>) -> Self {
        self.stop = Some(stops.into_iter().collect());
        self
    }

    /// Sets the RNG seed.
    pub fn with_seed(mut self, v: i64) -> Self {
        self.seed = Some(v);
        self
    }

    /// Sets logit_bias map.
    pub fn with_logit_bias(mut self, bias: BTreeMap<String, i32>) -> Self {
        self.logit_bias = Some(bias);
        self
    }

    /// Sets logprobs.
    pub fn with_logprobs(mut self, flag: bool) -> Self {
        self.logprobs = Some(flag);
        self
    }

    /// Sets top_logprobs (requires logprobs=true).
    pub fn with_top_logprobs(mut self, v: u32) -> Self {
        self.top_logprobs = Some(v);
        self
    }

    /// Sets the `user` end-user identifier.
    pub fn with_user(mut self, id: impl Into<String>) -> Self {
        self.user = Some(id.into());
        self
    }

    /// Sets the tool-choice constraint.
    pub fn with_tool_choice(mut self, choice: ToolChoice) -> Self {
        self.tool_choice = Some(choice);
        self
    }

    /// Sets parallel_tool_calls.
    pub fn with_parallel_tool_calls(mut self, flag: bool) -> Self {
        self.parallel_tool_calls = Some(flag);
        self
    }

    /// Sets tool_strict.
    pub fn with_tool_strict(mut self, flag: bool) -> Self {
        self.tool_strict = flag;
        self
    }

    /// Sets the structured output format.
    pub fn with_output_format(mut self, format: OutputFormat) -> Self {
        self.output_format = Some(format);
        self
    }

    /// Sets the reasoning configuration.
    pub fn with_reasoning(mut self, cfg: ReasoningConfig) -> Self {
        self.reasoning = Some(cfg);
        self
    }

    /// Toggles SSE streaming. Default: true.
    pub fn with_streaming(mut self, flag: bool) -> Self {
        self.streaming = flag;
        self
    }

    /// Sets the predicted-outputs configuration.
    #[cfg(feature = "predicted-outputs")]
    pub fn with_prediction(mut self, prediction: Prediction) -> Self {
        self.prediction = Some(prediction);
        self
    }

    /// Sets the service-tier.
    #[cfg(feature = "service-tiers")]
    pub fn with_service_tier(mut self, tier: ServiceTier) -> Self {
        self.service_tier = Some(tier);
        self
    }

    /// Sets the `queue_threshold` header value.
    #[cfg(feature = "service-tiers")]
    pub fn with_queue_threshold_ms(mut self, ms: u32) -> Self {
        self.queue_threshold_ms = Some(ms);
        self
    }

    /// Sets the compression configuration.
    #[cfg(feature = "compression")]
    pub fn with_compression(mut self, cfg: CompressionConfig) -> Self {
        self.compression = Some(cfg);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_api_key() {
        let err = CerebrasConfig::new("", "gpt-oss-120b").unwrap_err();
        assert!(matches!(err, BuildError::MissingEnv(_)));
    }

    #[test]
    fn validate_catches_bad_temperature() {
        let mut cfg = CerebrasConfig::new("k", "m").unwrap();
        cfg.temperature = Some(3.0);
        let err = cfg.validate().unwrap_err();
        assert!(matches!(
            err,
            BuildError::OutOfRange {
                field: "temperature",
                ..
            }
        ));
    }

    #[test]
    fn validate_catches_top_logprobs_without_logprobs() {
        let mut cfg = CerebrasConfig::new("k", "m").unwrap();
        cfg.top_logprobs = Some(5);
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, BuildError::TopLogprobsWithoutLogprobs));
    }

    #[test]
    fn validate_catches_too_many_stops() {
        let mut cfg = CerebrasConfig::new("k", "m").unwrap();
        cfg.stop = Some(vec![
            "a".into(),
            "b".into(),
            "c".into(),
            "d".into(),
            "e".into(),
        ]);
        let err = cfg.validate().unwrap_err();
        assert!(matches!(err, BuildError::OutOfRange { field: "stop", .. }));
    }

    #[test]
    fn validate_accepts_min_tokens_sentinel() {
        let mut cfg = CerebrasConfig::new("k", "m").unwrap();
        cfg.min_tokens = Some(-1);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn tool_choice_function_serializes_correctly() {
        let json = ToolChoice::Function {
            name: "search".into(),
        }
        .to_json();
        assert_eq!(json["type"], "function");
        assert_eq!(json["function"]["name"], "search");
    }

    #[test]
    fn output_format_json_schema_serializes_correctly() {
        let schema = json!({ "type": "object" });
        let json = OutputFormat::JsonSchema {
            schema: schema.clone(),
            strict: true,
            name: Some("person".into()),
        }
        .to_json();
        assert_eq!(json["type"], "json_schema");
        assert_eq!(json["json_schema"]["strict"], true);
        assert_eq!(json["json_schema"]["name"], "person");
        assert_eq!(json["json_schema"]["schema"], schema);
    }
}
