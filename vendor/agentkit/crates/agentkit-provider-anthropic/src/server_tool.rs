//! Server-side tools that Anthropic executes inside the model turn.
//!
//! [`ServerTool`] is the forward-compatible extension point. Built-in tools
//! (web search, web fetch, code execution) implement it; use [`RawServerTool`]
//! to pass through any new tool type Anthropic ships before this crate adds
//! first-class support.

use std::sync::Arc;

use serde_json::{Value, json};

/// A server-side tool definition appended to the `tools` array of a Messages
/// API request.
///
/// Implementations must produce the JSON object Anthropic expects and declare
/// any `anthropic-beta` header flags required to activate the tool.
pub trait ServerTool: Send + Sync {
    /// JSON object to append to the top-level `tools` array.
    ///
    /// The object MUST include the versioned `type` string (e.g.
    /// `"web_search_20260209"`) and a user-visible `name`.
    fn to_tool_json(&self) -> Value;

    /// Returns any `anthropic-beta` header flags required for this tool.
    ///
    /// Unioned across all configured server tools and appended to the request's
    /// beta header list.
    fn beta_flags(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Convenience alias for an owned, clonable handle to a [`ServerTool`].
pub type ServerToolHandle = Arc<dyn ServerTool>;

/// Wraps any `ServerTool` as an `Arc` so it can live in [`AnthropicConfig`].
///
/// [`AnthropicConfig`]: crate::AnthropicConfig
pub fn boxed<T: ServerTool + 'static>(tool: T) -> ServerToolHandle {
    Arc::new(tool)
}

/// The default `type` version string for `WebSearchTool`.
pub const DEFAULT_WEB_SEARCH_VERSION: &str = "web_search_20260209";
/// The default `type` version string for `WebFetchTool`.
pub const DEFAULT_WEB_FETCH_VERSION: &str = "web_fetch_20260309";
/// The default `type` version string for `CodeExecutionTool`.
pub const DEFAULT_CODE_EXECUTION_VERSION: &str = "code_execution_20260120";
/// The default `type` version string for `BashCodeExecutionTool`.
pub const DEFAULT_BASH_EXECUTION_VERSION: &str = "bash_code_execution_20260120";
/// The default `type` version string for `TextEditorCodeExecutionTool`.
pub const DEFAULT_TEXT_EDITOR_EXECUTION_VERSION: &str = "text_editor_code_execution_20260120";

/// Anthropic server-side web search.
#[derive(Clone, Debug)]
pub struct WebSearchTool {
    /// Versioned `type` string sent in the request.
    pub version: String,
    /// Optional cap on how many searches the model may issue in one turn.
    pub max_uses: Option<u32>,
    /// Restrict results to these domains (mutually exclusive with `blocked_domains`).
    pub allowed_domains: Vec<String>,
    /// Exclude these domains from results.
    pub blocked_domains: Vec<String>,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self {
            version: DEFAULT_WEB_SEARCH_VERSION.into(),
            max_uses: None,
            allowed_domains: Vec::new(),
            blocked_domains: Vec::new(),
        }
    }
}

impl WebSearchTool {
    /// Builds a web-search tool with the default version.
    pub fn new() -> Self {
        Self::default()
    }

    /// Overrides the versioned `type` string.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Sets the maximum number of searches per turn.
    pub fn with_max_uses(mut self, max: u32) -> Self {
        self.max_uses = Some(max);
        self
    }

    /// Restricts results to the given domain list.
    pub fn with_allowed_domains(mut self, domains: impl IntoIterator<Item = String>) -> Self {
        self.allowed_domains = domains.into_iter().collect();
        self
    }

    /// Excludes the given domain list.
    pub fn with_blocked_domains(mut self, domains: impl IntoIterator<Item = String>) -> Self {
        self.blocked_domains = domains.into_iter().collect();
        self
    }
}

impl ServerTool for WebSearchTool {
    fn to_tool_json(&self) -> Value {
        let mut body = serde_json::Map::new();
        body.insert("type".into(), Value::String(self.version.clone()));
        body.insert("name".into(), Value::String("web_search".into()));
        if let Some(max) = self.max_uses {
            body.insert("max_uses".into(), Value::from(max));
        }
        if !self.allowed_domains.is_empty() {
            body.insert(
                "allowed_domains".into(),
                Value::Array(
                    self.allowed_domains
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        if !self.blocked_domains.is_empty() {
            body.insert(
                "blocked_domains".into(),
                Value::Array(
                    self.blocked_domains
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
        Value::Object(body)
    }
}

/// Anthropic server-side web fetch.
#[derive(Clone, Debug)]
pub struct WebFetchTool {
    /// Versioned `type` string.
    pub version: String,
    /// Optional cap on fetches per turn.
    pub max_uses: Option<u32>,
    /// Optional cap on tokens extracted from a single fetched page.
    pub max_content_tokens: Option<u32>,
    /// Opt in to Anthropic's fetch result caching.
    pub use_cache: Option<bool>,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self {
            version: DEFAULT_WEB_FETCH_VERSION.into(),
            max_uses: None,
            max_content_tokens: None,
            use_cache: None,
        }
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    pub fn with_max_uses(mut self, max: u32) -> Self {
        self.max_uses = Some(max);
        self
    }

    pub fn with_max_content_tokens(mut self, max: u32) -> Self {
        self.max_content_tokens = Some(max);
        self
    }

    pub fn with_use_cache(mut self, enabled: bool) -> Self {
        self.use_cache = Some(enabled);
        self
    }
}

impl ServerTool for WebFetchTool {
    fn to_tool_json(&self) -> Value {
        let mut body = serde_json::Map::new();
        body.insert("type".into(), Value::String(self.version.clone()));
        body.insert("name".into(), Value::String("web_fetch".into()));
        if let Some(max) = self.max_uses {
            body.insert("max_uses".into(), Value::from(max));
        }
        if let Some(max) = self.max_content_tokens {
            body.insert("max_content_tokens".into(), Value::from(max));
        }
        if let Some(flag) = self.use_cache {
            body.insert("use_cache".into(), Value::Bool(flag));
        }
        Value::Object(body)
    }
}

/// Anthropic server-side Python code execution sandbox.
#[derive(Clone, Debug)]
pub struct CodeExecutionTool {
    pub version: String,
}

impl Default for CodeExecutionTool {
    fn default() -> Self {
        Self {
            version: DEFAULT_CODE_EXECUTION_VERSION.into(),
        }
    }
}

impl CodeExecutionTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }
}

impl ServerTool for CodeExecutionTool {
    fn to_tool_json(&self) -> Value {
        json!({
            "type": self.version,
            "name": "code_execution",
        })
    }
}

/// Anthropic server-side bash execution sandbox.
#[derive(Clone, Debug)]
pub struct BashCodeExecutionTool {
    pub version: String,
}

impl Default for BashCodeExecutionTool {
    fn default() -> Self {
        Self {
            version: DEFAULT_BASH_EXECUTION_VERSION.into(),
        }
    }
}

impl BashCodeExecutionTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }
}

impl ServerTool for BashCodeExecutionTool {
    fn to_tool_json(&self) -> Value {
        json!({
            "type": self.version,
            "name": "bash_code_execution",
        })
    }
}

/// Anthropic server-side text editor sandbox.
#[derive(Clone, Debug)]
pub struct TextEditorCodeExecutionTool {
    pub version: String,
}

impl Default for TextEditorCodeExecutionTool {
    fn default() -> Self {
        Self {
            version: DEFAULT_TEXT_EDITOR_EXECUTION_VERSION.into(),
        }
    }
}

impl TextEditorCodeExecutionTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }
}

impl ServerTool for TextEditorCodeExecutionTool {
    fn to_tool_json(&self) -> Value {
        json!({
            "type": self.version,
            "name": "text_editor_code_execution",
        })
    }
}

/// Passthrough wrapper for any server tool Anthropic has shipped but this
/// crate has not yet added first-class support for.
///
/// The embedded `value` is spliced into the `tools` array verbatim; any
/// required beta headers are unioned into the request's beta header list.
#[derive(Clone, Debug)]
pub struct RawServerTool {
    /// JSON object to splice into `tools[]`.
    pub value: Value,
    /// Beta flags this tool needs.
    pub betas: Vec<String>,
}

impl RawServerTool {
    /// Wraps a raw tool definition with no extra beta flags.
    pub fn new(value: Value) -> Self {
        Self {
            value,
            betas: Vec::new(),
        }
    }

    /// Adds a required beta flag.
    pub fn with_beta(mut self, flag: impl Into<String>) -> Self {
        self.betas.push(flag.into());
        self
    }
}

impl ServerTool for RawServerTool {
    fn to_tool_json(&self) -> Value {
        self.value.clone()
    }

    fn beta_flags(&self) -> Vec<String> {
        self.betas.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_search_serializes_filters() {
        let tool = WebSearchTool::new()
            .with_max_uses(3)
            .with_allowed_domains(["docs.rs".to_string()]);
        let json = tool.to_tool_json();
        assert_eq!(json["type"], DEFAULT_WEB_SEARCH_VERSION);
        assert_eq!(json["name"], "web_search");
        assert_eq!(json["max_uses"], 3);
        assert_eq!(json["allowed_domains"][0], "docs.rs");
    }

    #[test]
    fn raw_tool_preserves_body_and_betas() {
        let raw = RawServerTool::new(json!({
            "type": "future_tool_20271231",
            "name": "future_tool",
        }))
        .with_beta("future-tool-2027-12-31");

        assert_eq!(raw.to_tool_json()["name"], "future_tool");
        assert_eq!(raw.beta_flags(), vec!["future-tool-2027-12-31".to_string()]);
    }
}
