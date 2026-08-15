use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};

use agentkit_core::{ToolOutput, ToolResultPart};
use agentkit_mcp::{
    McpServerConfig, McpServerManager, McpServerOptions, McpTransportBinding, StdioTransportConfig,
    StreamableHttpTransportConfig,
};
use agentkit_tools_core::{
    CatalogReader, Tool, ToolContext, ToolError, ToolExecutionOutcome, ToolName, ToolRequest,
    ToolResult, ToolSource, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(rename = "mcpServers")]
    mcp_servers: BTreeMap<String, Server>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Server {
    Stdio(Stdio),
    Http(Http),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stdio {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Http {
    url: String,
    #[serde(rename = "bearerToken")]
    bearer_token: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

pub async fn connect(path: &Path) -> Result<(McpServerManager, CatalogReader), String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("could not read MCP config {}: {e}", path.display()))?;
    let config: Config = serde_json::from_slice(&bytes)
        .map_err(|e| format!("invalid MCP config {}: {e}", path.display()))?;
    let mut manager = McpServerManager::new();
    for (id, server) in config.mcp_servers {
        if id.trim().is_empty() {
            return Err("MCP server names must not be empty".into());
        }
        let binding = match server {
            Server::Stdio(server) => {
                if server.command.trim().is_empty() {
                    return Err(format!("MCP server {id} has an empty command"));
                }
                let mut transport = StdioTransportConfig::new(server.command);
                transport.args = server.args;
                transport.env = server.env.into_iter().collect();
                transport.cwd = server.cwd;
                McpTransportBinding::Stdio(transport)
            }
            Server::Http(server) => {
                if server.url.trim().is_empty() {
                    return Err(format!("MCP server {id} has an empty URL"));
                }
                let mut transport = StreamableHttpTransportConfig::new(server.url);
                if let Some(token) = server.bearer_token {
                    transport = transport.with_bearer_token(token);
                }
                for (name, value) in server.headers {
                    transport = transport
                        .with_header(name.as_str(), value.as_str())
                        .map_err(|e| format!("invalid MCP config for {id}: {e}"))?;
                }
                McpTransportBinding::StreamableHttp(transport)
            }
        };
        manager.register_server_with_options(
            McpServerConfig::new(id, binding),
            McpServerOptions::new().with_timeout(CONNECT_TIMEOUT),
        );
    }
    manager
        .connect_all()
        .await
        .map_err(|e| format!("could not connect MCP servers: {e}"))?;
    let catalog = manager.source();
    Ok((manager, catalog))
}

#[derive(Clone)]
pub struct ToolSearch {
    catalog: CatalogReader,
    spec: ToolSpec,
}

impl ToolSearch {
    pub fn new(catalog: CatalogReader) -> Self {
        Self {
            catalog,
            spec: ToolSpec::new(
                ToolName::new("tool_search"),
                "Search connected MCP tools with ranked keywords. Terms match independently; close spelling matches are used only when a term has no regular match. Returns up to 20 schemas.",
                json!({"type":"object","properties":{"query":{"type":"string","description":"Space-separated capability or product keywords, for example `project management linear jira`. Use `mcp` to list all connected MCP tools."}},"required":["query"],"additionalProperties":false}),
            )
            .with_output_schema(json!({"type":"array","items":{"type":"object"}})),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    query: String,
}

#[derive(Clone)]
struct PreparedText {
    normalized: String,
    tokens: Vec<String>,
}

impl PreparedText {
    fn new(value: &str) -> Self {
        let lowercase = value.to_lowercase();
        let tokens = lowercase
            .split(|character: char| !character.is_alphanumeric())
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        Self {
            normalized: tokens.join(" "),
            tokens,
        }
    }
}

struct PreparedQuery(PreparedText);

impl PreparedQuery {
    fn new(value: &str) -> Option<Self> {
        let mut text = PreparedText::new(value);
        let mut seen = BTreeSet::new();
        text.tokens.retain(|token| seen.insert(token.clone()));
        (!text.tokens.is_empty()).then_some(Self(text))
    }
}

struct PreparedSpec {
    spec: ToolSpec,
    name: PreparedText,
    description: PreparedText,
}

impl PreparedSpec {
    fn new(spec: ToolSpec) -> Self {
        let name = PreparedText::new(&spec.name.0);
        let description = PreparedText::new(&spec.description);
        Self {
            spec,
            name,
            description,
        }
    }
}

fn regular_term_score(term: &str, spec: &PreparedSpec) -> u32 {
    if spec.name.tokens.iter().any(|token| token == term) {
        200
    } else if term.chars().count() >= 2
        && spec.name.tokens.iter().any(|token| token.starts_with(term))
    {
        140
    } else if term.chars().count() >= 3 && spec.name.tokens.iter().any(|token| token.contains(term))
    {
        100
    } else if spec.description.tokens.iter().any(|token| token == term) {
        30
    } else if term.chars().count() >= 2
        && spec
            .description
            .tokens
            .iter()
            .any(|token| token.starts_with(term))
    {
        20
    } else if term.chars().count() >= 3
        && spec
            .description
            .tokens
            .iter()
            .any(|token| token.contains(term))
    {
        10
    } else {
        0
    }
}

fn fuzzy_term_score(term: &str, spec: &PreparedSpec) -> u32 {
    let length = term.chars().count();
    let limit = match length {
        5..=7 => 1,
        8.. => 2,
        _ => return 0,
    };
    if spec
        .name
        .tokens
        .iter()
        .any(|token| levenshtein(term, token) <= limit)
    {
        50
    } else if spec
        .description
        .tokens
        .iter()
        .any(|token| levenshtein(term, token) <= limit)
    {
        8
    } else {
        0
    }
}

fn levenshtein(left: &str, right: &str) -> usize {
    let right = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    for (left_index, left_character) in left.chars().enumerate() {
        let mut current = Vec::with_capacity(right.len() + 1);
        current.push(left_index + 1);
        for (right_index, right_character) in right.iter().enumerate() {
            let substitution =
                previous[right_index] + usize::from(left_character != *right_character);
            current.push(
                (current[right_index] + 1)
                    .min(previous[right_index + 1] + 1)
                    .min(substitution),
            );
        }
        previous = current;
    }
    previous[right.len()]
}

fn score_spec(query: &PreparedQuery, spec: &PreparedSpec, fuzzy: &[bool]) -> Option<u32> {
    let mut score = if query.0.normalized == spec.name.normalized {
        1_000
    } else if spec.name.normalized.contains(&query.0.normalized) {
        300
    } else if spec.description.normalized.contains(&query.0.normalized) {
        60
    } else {
        0
    };
    let mut matched = 0_u32;
    for (index, term) in query.0.tokens.iter().enumerate() {
        let regular = regular_term_score(term, spec);
        let term_score = if regular == 0 && fuzzy[index] {
            fuzzy_term_score(term, spec)
        } else {
            regular
        };
        if term_score > 0 {
            matched += 1;
            score += term_score;
        }
    }
    if matched == 0 {
        return None;
    }
    score += 10 * matched.saturating_sub(1);
    if matched as usize == query.0.tokens.len() {
        score += 20;
    }
    Some(score)
}

fn search_specs(specs: Vec<ToolSpec>, query: &str) -> Result<Vec<Value>, ToolError> {
    let query = PreparedQuery::new(query)
        .ok_or_else(|| ToolError::InvalidInput("query must contain a letter or number".into()))?;
    let specs = specs.into_iter().map(PreparedSpec::new).collect::<Vec<_>>();
    let fuzzy = query
        .0
        .tokens
        .iter()
        .map(|term| !specs.iter().any(|spec| regular_term_score(term, spec) > 0))
        .collect::<Vec<_>>();
    let mut matches = specs
        .into_iter()
        .filter_map(|spec| score_spec(&query, &spec, &fuzzy).map(|score| (score, spec.spec)))
        .collect::<Vec<_>>();
    matches.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.name.0.cmp(&right.name.0))
    });
    Ok(matches.into_iter().take(20).map(|(_, spec)| {
        let mut value = json!({"name":spec.name.0,"description":spec.description,"input_schema":spec.input_schema});
        if let Some(output) = spec.output_schema { value["output_schema"] = output; }
        value
    }).collect())
}

#[async_trait]
impl Tool for ToolSearch {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: SearchInput = serde_json::from_value(request.input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let matches = search_specs(self.catalog.specs(), &input.query)?;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(json!(matches)),
        )))
    }
}

#[derive(Clone)]
pub struct McpTool {
    catalog: CatalogReader,
    spec: ToolSpec,
}
impl McpTool {
    pub fn new(catalog: CatalogReader) -> Self {
        Self {
            catalog,
            spec: ToolSpec::new(
                ToolName::new("tool"),
                "Invoke a connected MCP tool returned by tool_search.",
                json!({"type":"object","properties":{"name":{"type":"string"},"args":{"type":"object"}},"required":["name","args"],"additionalProperties":false}),
            ),
        }
    }
    async fn dispatch(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        let Some(object) = request.input.as_object() else {
            return ToolExecutionOutcome::Failed(ToolError::InvalidInput(
                "arguments must be an object".into(),
            ));
        };
        if object.keys().any(|k| k != "name" && k != "args") {
            return ToolExecutionOutcome::Failed(ToolError::InvalidInput(
                "unknown field in tool arguments".into(),
            ));
        }
        let Some(name) = object.get("name").and_then(Value::as_str) else {
            return ToolExecutionOutcome::Failed(ToolError::InvalidInput(
                "name must be a string".into(),
            ));
        };
        let Some(args) = object.get("args").filter(|v| v.is_object()).cloned() else {
            return ToolExecutionOutcome::Failed(ToolError::InvalidInput(
                "args must be an object".into(),
            ));
        };
        let name = ToolName::new(name);
        if self.catalog.get(&name).is_none() {
            return ToolExecutionOutcome::Failed(ToolError::InvalidInput(format!(
                "unknown MCP tool: {}",
                name.0
            )));
        }
        let Some(scope) = context.execution_scope.clone() else {
            return ToolExecutionOutcome::Failed(ToolError::Unavailable(
                "tool requires an execution scope".into(),
            ));
        };
        scope
            .execute_child(
                ToolRequest::new(
                    request.call_id,
                    name,
                    args,
                    request.session_id,
                    request.turn_id,
                )
                .with_metadata(request.metadata),
            )
            .await
    }
}
#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn invoke(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        match self.dispatch(request, context).await {
            ToolExecutionOutcome::Completed(v) => Ok(v),
            ToolExecutionOutcome::FailedBeforeInvocation(e) | ToolExecutionOutcome::Failed(e) => {
                Err(e)
            }
            ToolExecutionOutcome::Interrupted(_) => {
                Err(ToolError::Unavailable("MCP tool requires approval".into()))
            }
        }
    }
    async fn invoke_outcome(
        &self,
        request: ToolRequest,
        context: &mut ToolContext<'_>,
    ) -> ToolExecutionOutcome {
        self.dispatch(request, context).await
    }
}

pub fn empty() -> (McpServerManager, CatalogReader) {
    let manager = McpServerManager::new();
    let catalog = manager.source();
    (manager, catalog)
}

#[cfg(test)]
mod tests {
    use agentkit_tools_core::{ToolName, ToolSpec};
    use serde_json::json;

    use super::{Config, search_specs};

    fn spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec::new(ToolName::new(name), description, json!({"type": "object"}))
    }

    fn names(results: &[serde_json::Value]) -> Vec<&str> {
        results
            .iter()
            .map(|result| result["name"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn search_treats_llm_queries_as_ranked_keyword_bags() {
        let results = search_specs(
            vec![
                spec(
                    "mcp_linear_create_issue",
                    "Create work in a project management workspace",
                ),
                spec("mcp_jira_search", "Search tickets"),
                spec(
                    "mcp_generic",
                    "Project management integrations for Linear and Jira",
                ),
                spec("mcp_echo", "Echo text"),
            ],
            "PROJECT management linear jira",
        )
        .unwrap();
        let names = names(&results);
        assert_eq!(names[0], "mcp_linear_create_issue");
        assert!(names[..2].contains(&"mcp_jira_search"));
        assert_eq!(names.last(), Some(&"mcp_generic"));
        assert!(!names.contains(&"mcp_echo"));
    }

    #[test]
    fn exact_name_terms_outrank_description_matches() {
        let results = search_specs(
            vec![
                spec("mcp_linear", "Issue tracker"),
                spec("mcp_other", "A Linear integration"),
            ],
            "linear",
        )
        .unwrap();
        assert_eq!(names(&results), ["mcp_linear", "mcp_other"]);
    }

    #[test]
    fn levenshtein_is_only_a_fallback_for_unmatched_terms() {
        let fuzzy =
            search_specs(vec![spec("mcp_tasks", "Project management")], "managment").unwrap();
        assert_eq!(names(&fuzzy), ["mcp_tasks"]);
        let regular = search_specs(
            vec![
                spec("mcp_exact", "Project management"),
                spec("mcp_near", "Project managemint"),
            ],
            "management",
        )
        .unwrap();
        assert_eq!(names(&regular), ["mcp_exact"]);
    }

    #[test]
    fn search_is_stable_limited_and_rejects_punctuation_only_queries() {
        let specs = (0..25)
            .rev()
            .map(|index| spec(&format!("mcp_tool_{index:02}"), "shared capability"))
            .collect();
        let results = search_specs(specs, "shared!!! shared").unwrap();
        assert_eq!(results.len(), 20);
        assert_eq!(names(&results)[..2], ["mcp_tool_00", "mcp_tool_01"]);
        assert!(search_specs(Vec::new(), " --- ").is_err());
    }

    #[test]
    fn config_is_strict() {
        assert!(
            serde_json::from_str::<Config>(
                r#"{"mcpServers":{"ok":{"command":"server","args":["--stdio"],"env":{"A":"B"}}}}"#
            )
            .is_ok()
        );
        assert!(serde_json::from_str::<Config>(r#"{"mcpServers":{},"extra":true}"#).is_err());
        assert!(
            serde_json::from_str::<Config>(
                r#"{"mcpServers":{"bad":{"command":"x","url":"http://localhost"}}}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<Config>(
                r#"{"mcpServers":{"bad":{"url":"http://localhost","unknown":1}}}"#
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn connection_failure_is_reported_at_startup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"broken":{"command":"kit-command-that-does-not-exist"}}}"#,
        )
        .unwrap();
        let error = super::connect(&path)
            .await
            .err()
            .expect("connection should fail");
        assert!(error.contains("could not connect MCP servers"), "{error}");
    }
}
