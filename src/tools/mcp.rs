mod auth;
mod credentials;

pub use credentials::CredentialStorage;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use agentkit_core::{MetadataMap, ToolOutput, ToolResultPart};
use agentkit_mcp::{
    AuthOperation, AuthRequest, AuthResolution, McpAuthResponder, McpError, McpHandlerConfig,
    McpServerConfig, McpServerId, McpServerManager, McpServerOptions, McpTransportBinding,
    StdioTransportConfig, StreamableHttpTransportConfig,
};
use agentkit_tools_core::{
    CatalogReader, Tool, ToolContext, ToolError, ToolExecutionOutcome, ToolName, ToolRequest,
    ToolResult, ToolSource, ToolSpec,
};
use async_trait::async_trait;
use rmcp::transport::auth::AuthorizationManager;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};

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
    description: Option<String>,
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
    description: Option<String>,
    #[serde(rename = "bearerToken")]
    bearer_token: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    auth: Option<auth::Config>,
}

#[derive(Clone)]
pub struct McpRuntime {
    inner: Arc<Inner>,
}

struct Inner {
    manager: Mutex<McpServerManager>,
    catalog: CatalogReader,
    servers: Arc<RwLock<BTreeMap<String, ServerRecord>>>,
    challenges: Arc<Mutex<BTreeMap<String, AuthRequest>>>,
    pending: Mutex<BTreeMap<String, PendingRecord>>,
    oauth_managers: Mutex<BTreeMap<String, AuthorizationManager>>,
    credential_storage: CredentialStorage,
    auth_setup: Mutex<()>,
    interactive_oauth_enabled: bool,
}

#[derive(Clone)]
struct ServerRecord {
    description: String,
    url: Option<String>,
    oauth: Option<auth::Config>,
    status: ServerStatus,
}

#[derive(Clone)]
enum ServerStatus {
    Connected,
    AuthenticationRequired,
    Pending,
    Error(String),
}

impl ServerStatus {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Connected => "authenticated",
            Self::AuthenticationRequired => "authentication_required",
            Self::Pending => "pending",
            Self::Error(_) => "error",
        }
    }
}

#[derive(Clone)]
struct PendingRecord {
    url: String,
    expires: Instant,
}

struct AuthRecorder {
    challenges: Arc<Mutex<BTreeMap<String, AuthRequest>>>,
    servers: Arc<RwLock<BTreeMap<String, ServerRecord>>>,
}

#[async_trait]
impl McpAuthResponder for AuthRecorder {
    async fn resolve(&self, request: AuthRequest) -> Result<AuthResolution, McpError> {
        let server = request.server_id().unwrap_or("unknown").to_string();
        self.challenges.lock().await.insert(server.clone(), request);
        if let Some(record) = self.servers.write().await.get_mut(&server) {
            record.status = ServerStatus::AuthenticationRequired;
        }
        Err(McpError::AuthResolution(format!(
            "authentication required for MCP server {server}; call auth with that server name"
        )))
    }
}

fn connect_auth_request(server: &str) -> AuthRequest {
    AuthRequest {
        id: format!("stored:{server}"),
        provider: "oauth".into(),
        operation: AuthOperation::McpConnect {
            server_id: server.into(),
            metadata: MetadataMap::new(),
        },
        challenge: MetadataMap::new(),
    }
}

fn challenge_requires_interactive_authorization(challenge: &MetadataMap) -> bool {
    challenge
        .get("required_scope")
        .and_then(Value::as_str)
        .is_some()
        || challenge.get("insufficient_scope").and_then(Value::as_bool) == Some(true)
}

pub async fn connect(
    path: &Path,
    interactive_oauth_enabled: bool,
    credential_storage: CredentialStorage,
) -> Result<McpRuntime, String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| format!("could not read MCP config {}: {error}", path.display()))?;
    let config: Config = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid MCP config {}: {error}", path.display()))?;
    let challenges = Arc::new(Mutex::new(BTreeMap::new()));
    let mut manager = McpServerManager::new();
    let mut records = BTreeMap::new();
    let mut eager = Vec::new();
    let mut persistent_oauth = Vec::new();

    for (id, server) in config.mcp_servers {
        if id.trim().is_empty() {
            return Err("MCP server names must not be empty".into());
        }
        let (binding, record) = match server {
            Server::Stdio(server) => {
                if server.command.trim().is_empty() {
                    return Err(format!("MCP server {id} has an empty command"));
                }
                let mut transport = StdioTransportConfig::new(server.command);
                transport.args = server.args;
                transport.env = server.env.into_iter().collect();
                transport.cwd = server.cwd;
                eager.push(id.clone());
                (
                    McpTransportBinding::Stdio(transport),
                    ServerRecord {
                        description: server.description.unwrap_or_else(|| id.clone()),
                        url: None,
                        oauth: None,
                        status: ServerStatus::Connected,
                    },
                )
            }
            Server::Http(server) => {
                if server.url.trim().is_empty() {
                    return Err(format!("MCP server {id} has an empty URL"));
                }
                if server.auth.is_some() && server.bearer_token.is_some() {
                    return Err(format!(
                        "MCP server {id} cannot use both OAuth and bearerToken"
                    ));
                }
                let mut transport = StreamableHttpTransportConfig::new(server.url.clone());
                if let Some(token) = server.bearer_token {
                    transport = transport.with_bearer_token(token);
                }
                for (name, value) in server.headers {
                    transport = transport
                        .with_header(name.as_str(), value.as_str())
                        .map_err(|error| format!("invalid MCP config for {id}: {error}"))?;
                }
                let status = if let Some(oauth) = &server.auth {
                    if credential_storage.is_persistent() {
                        persistent_oauth.push((id.clone(), server.url.clone(), oauth.clone()));
                    }
                    ServerStatus::AuthenticationRequired
                } else {
                    eager.push(id.clone());
                    ServerStatus::Connected
                };
                (
                    McpTransportBinding::StreamableHttp(transport),
                    ServerRecord {
                        description: server.description.unwrap_or_else(|| id.clone()),
                        url: Some(server.url),
                        oauth: server.auth,
                        status,
                    },
                )
            }
        };
        manager.register_server_with_options(
            McpServerConfig::new(id.clone(), binding),
            McpServerOptions::new().with_timeout(CONNECT_TIMEOUT),
        );
        records.insert(id, record);
    }

    let servers = Arc::new(RwLock::new(records));
    manager.set_handler_config(McpHandlerConfig::new().with_auth_responder(Arc::new(
        AuthRecorder {
            challenges: Arc::clone(&challenges),
            servers: Arc::clone(&servers),
        },
    )));
    for id in eager {
        if let Err(error) = manager.connect_server(&McpServerId::new(&id)).await
            && let Some(record) = servers.write().await.get_mut(&id)
        {
            record.status =
                ServerStatus::Error(format!("could not connect MCP server {id}: {error}"));
        }
    }
    let mut oauth_managers = BTreeMap::new();
    for (id, url, oauth) in persistent_oauth {
        let restored = match auth::restore(&url, &oauth, &credential_storage).await {
            Ok(restored) => restored,
            Err(error) => {
                if let Some(record) = servers.write().await.get_mut(&id) {
                    record.status = ServerStatus::Error(error);
                }
                continue;
            }
        };
        let Some((token, oauth_manager)) = restored else {
            continue;
        };
        let request = connect_auth_request(&id);
        let mut credentials = MetadataMap::new();
        credentials.insert("access_token".into(), Value::String(token));
        if let Err(error) = manager
            .resolve_auth(AuthResolution::provided(request, credentials))
            .await
        {
            if let Some(record) = servers.write().await.get_mut(&id) {
                record.status = ServerStatus::Error(format!(
                    "could not restore MCP credentials for {id}: {error}"
                ));
            }
            continue;
        }
        match manager.connect_server(&McpServerId::new(&id)).await {
            Ok(_) => {
                if let Some(record) = servers.write().await.get_mut(&id) {
                    record.status = ServerStatus::Connected;
                }
            }
            Err(McpError::AuthRequired(request)) => {
                challenges.lock().await.insert(id.clone(), *request);
            }
            Err(error) => {
                if let Some(record) = servers.write().await.get_mut(&id) {
                    record.status = ServerStatus::Error(format!(
                        "could not connect MCP server {id} with stored credentials: {error}"
                    ));
                }
                continue;
            }
        }
        oauth_managers.insert(id, oauth_manager);
    }
    Ok(McpRuntime::new(
        manager,
        servers,
        challenges,
        oauth_managers,
        credential_storage,
        interactive_oauth_enabled,
    ))
}

impl McpRuntime {
    fn new(
        manager: McpServerManager,
        servers: Arc<RwLock<BTreeMap<String, ServerRecord>>>,
        challenges: Arc<Mutex<BTreeMap<String, AuthRequest>>>,
        oauth_managers: BTreeMap<String, AuthorizationManager>,
        credential_storage: CredentialStorage,
        interactive_oauth_enabled: bool,
    ) -> Self {
        let catalog = manager.source();
        Self {
            inner: Arc::new(Inner {
                manager: Mutex::new(manager),
                catalog,
                servers,
                challenges,
                pending: Mutex::new(BTreeMap::new()),
                oauth_managers: Mutex::new(oauth_managers),
                credential_storage,
                auth_setup: Mutex::new(()),
                interactive_oauth_enabled,
            }),
        }
    }

    pub fn catalog(&self) -> CatalogReader {
        self.inner.catalog.clone()
    }

    async fn search(&self, query: &str) -> Result<Value, ToolError> {
        let prepared = PreparedQuery::new(query).ok_or_else(|| {
            ToolError::InvalidInput("query must contain a letter or number".into())
        })?;
        let wildcard = prepared.0.normalized == "mcp";
        let records = self.inner.servers.read().await.clone();
        let manager = self.inner.manager.lock().await;
        let mut groups = Vec::new();

        for (name, record) in records {
            let server_spec = PreparedSpec::new(ToolSpec::new(
                ToolName::new(&name),
                &record.description,
                json!({"type":"object"}),
            ));
            let server_matches = wildcard
                || score_spec(
                    &prepared,
                    &server_spec,
                    &vec![false; prepared.0.tokens.len()],
                )
                .is_some();
            let specs = manager
                .connected_server(&McpServerId::new(&name))
                .map(|handle| handle.tool_registry().specs())
                .unwrap_or_default();
            let mut tools = search_specs(specs.clone(), query)?;
            if server_matches && tools.len() < 20 {
                let mut names = tools
                    .iter()
                    .filter_map(|tool| tool.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect::<BTreeSet<_>>();
                tools.extend(spec_values(
                    specs
                        .into_iter()
                        .filter(|spec| names.insert(spec.name.0.clone()))
                        .collect(),
                    20 - tools.len(),
                ));
            }
            if server_matches || !tools.is_empty() {
                let status = if record.oauth.is_some()
                    && !self.inner.interactive_oauth_enabled
                    && matches!(record.status, ServerStatus::AuthenticationRequired)
                {
                    "authentication_unavailable"
                } else {
                    record.status.as_str()
                };
                let mut group = json!({
                    "name": name,
                    "description": record.description,
                    "status": status,
                    "tools": tools
                });
                if let ServerStatus::Error(error) = record.status {
                    group["error"] = Value::String(error);
                }
                groups.push(group);
            }
        }
        Ok(json!({"servers": groups}))
    }

    async fn authorize(&self, name: &str) -> Result<Value, ToolError> {
        if !self.inner.interactive_oauth_enabled {
            return Err(ToolError::Unavailable(
                "interactive MCP authentication requires the tui, serve, or acp command".into(),
            ));
        }
        let _setup = self.inner.auth_setup.lock().await;
        if let Some(pending) = self.inner.pending.lock().await.get(name).cloned() {
            let remaining = pending
                .expires
                .saturating_duration_since(Instant::now())
                .as_secs();
            return Ok(json!({
                "server": name,
                "status": "pending",
                "url": pending.url,
                "expires_in_seconds": remaining
            }));
        }
        let record = self
            .inner
            .servers
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::InvalidInput(format!("unknown MCP server: {name}")))?;
        let oauth = record.oauth.ok_or_else(|| {
            ToolError::InvalidInput(format!("MCP server {name} is not configured for OAuth"))
        })?;
        let resource_url = record
            .url
            .ok_or_else(|| ToolError::InvalidInput(format!("MCP server {name} is not remote")))?;

        let request = match self.inner.challenges.lock().await.get(name).cloned() {
            Some(request) => request,
            None => {
                let mut manager = self.inner.manager.lock().await;
                if manager.connected_server(&McpServerId::new(name)).is_some() {
                    return Ok(json!({"server":name,"status":"authenticated"}));
                }
                match manager.connect_server(&McpServerId::new(name)).await {
                    Ok(_) => {
                        self.set_status(name, ServerStatus::Connected).await;
                        return Ok(json!({"server":name,"status":"authenticated"}));
                    }
                    Err(McpError::AuthRequired(request)) => *request,
                    Err(error) => {
                        return Err(ToolError::Unavailable(format!(
                            "could not contact MCP server {name}: {error}"
                        )));
                    }
                }
            }
        };
        self.inner
            .challenges
            .lock()
            .await
            .insert(name.to_string(), request.clone());
        let required_scope = request
            .challenge
            .get("required_scope")
            .and_then(Value::as_str)
            .map(str::to_string);
        if !challenge_requires_interactive_authorization(&request.challenge)
            && let Some(manager) = self.inner.oauth_managers.lock().await.remove(name)
            && manager.refresh_token().await.is_ok()
            && let Ok(token) = manager.get_access_token().await
            && self
                .apply_credentials(name, request.clone(), token)
                .await
                .is_ok()
        {
            self.inner
                .oauth_managers
                .lock()
                .await
                .insert(name.to_string(), manager);
            self.inner.challenges.lock().await.remove(name);
            self.set_status(name, ServerStatus::Connected).await;
            return Ok(json!({"server":name,"status":"authenticated"}));
        }
        let pending = auth::begin(
            &resource_url,
            &oauth,
            &self.inner.credential_storage,
            required_scope.as_deref(),
        )
        .await
        .map_err(ToolError::Unavailable)?;
        let url = pending.url.clone();
        self.inner.pending.lock().await.insert(
            name.to_string(),
            PendingRecord {
                url: url.clone(),
                expires: Instant::now() + auth::FLOW_TIMEOUT,
            },
        );
        self.set_status(name, ServerStatus::Pending).await;
        let runtime = self.clone();
        let server = name.to_string();
        tokio::spawn(async move {
            runtime
                .complete_authorization(server, request, pending)
                .await;
        });
        Ok(json!({
            "server": name,
            "status": "pending",
            "url": url,
            "expires_in_seconds": auth::FLOW_TIMEOUT.as_secs()
        }))
    }

    async fn complete_authorization(
        &self,
        server: String,
        request: AuthRequest,
        pending: auth::PendingAuthorization,
    ) {
        let result = async {
            let (token, oauth_manager) = auth::finish(pending).await?;
            self.apply_credentials(&server, request, token).await?;
            Ok::<_, String>(oauth_manager)
        }
        .await;
        self.inner.pending.lock().await.remove(&server);
        if result.is_ok() {
            self.inner.challenges.lock().await.remove(&server);
        }
        self.set_status(
            &server,
            if result.is_ok() {
                ServerStatus::Connected
            } else {
                ServerStatus::AuthenticationRequired
            },
        )
        .await;
        match result {
            Ok(manager) => {
                self.inner
                    .oauth_managers
                    .lock()
                    .await
                    .insert(server, manager);
            }
            Err(error) => eprintln!("MCP authentication for {server} failed: {error}"),
        }
    }

    async fn apply_credentials(
        &self,
        server: &str,
        request: AuthRequest,
        token: String,
    ) -> Result<(), String> {
        let mut credentials = MetadataMap::new();
        credentials.insert("access_token".into(), Value::String(token));
        let mut manager = self.inner.manager.lock().await;
        manager
            .resolve_auth(AuthResolution::provided(request, credentials))
            .await
            .map_err(|error| format!("could not apply MCP credentials: {error}"))?;
        let id = McpServerId::new(server);
        if manager.connected_server(&id).is_none() {
            manager
                .connect_server(&id)
                .await
                .map_err(|error| format!("could not connect authenticated MCP server: {error}"))?;
        }
        Ok(())
    }

    async fn set_status(&self, name: &str, status: ServerStatus) {
        if let Some(record) = self.inner.servers.write().await.get_mut(name) {
            record.status = status;
        }
    }
}

#[derive(Clone)]
pub struct ToolSearch {
    runtime: McpRuntime,
    spec: ToolSpec,
}

impl ToolSearch {
    pub fn new(runtime: McpRuntime) -> Self {
        Self {
            runtime,
            spec: ToolSpec::new(
                ToolName::new("tool_search"),
                "Search configured MCP server names and discovered tool names. Results are grouped by server and include authentication status.",
                json!({"type":"object","properties":{"query":{"type":"string","description":"Capability, product, server, or tool keywords. Use `mcp` to list all configured servers."}},"required":["query"],"additionalProperties":false}),
            )
            .with_output_schema(json!({"type":"object","properties":{"servers":{"type":"array","items":{"type":"object"}}},"required":["servers"],"additionalProperties":false})),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchInput {
    query: String,
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
        let matches = self.runtime.search(&input.query).await?;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(matches),
        )))
    }
}

#[derive(Clone)]
pub struct AuthTool {
    runtime: McpRuntime,
    spec: ToolSpec,
}

impl AuthTool {
    pub fn new(runtime: McpRuntime) -> Self {
        Self {
            runtime,
            spec: ToolSpec::new(
                ToolName::new("auth"),
                "Start OAuth for a configured remote MCP server. Return the URL to the user, then search again after they complete it.",
                json!({"type":"object","properties":{"name":{"type":"string","description":"Exact server name returned by tool_search."}},"required":["name"],"additionalProperties":false}),
            )
            .with_output_schema(json!({"type":"object"})),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthInput {
    name: String,
}

#[async_trait]
impl Tool for AuthTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let input: AuthInput = serde_json::from_value(request.input)
            .map_err(|error| ToolError::InvalidInput(error.to_string()))?;
        let result = self.runtime.authorize(&input.name).await?;
        Ok(ToolResult::new(ToolResultPart::success(
            request.call_id,
            ToolOutput::structured(result),
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
                "Invoke an authenticated MCP tool returned by tool_search.",
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
        if object.keys().any(|key| key != "name" && key != "args") {
            return ToolExecutionOutcome::Failed(ToolError::InvalidInput(
                "unknown field in tool arguments".into(),
            ));
        }
        let Some(name) = object.get("name").and_then(Value::as_str) else {
            return ToolExecutionOutcome::Failed(ToolError::InvalidInput(
                "name must be a string".into(),
            ));
        };
        let Some(args) = object
            .get("args")
            .filter(|value| value.is_object())
            .cloned()
        else {
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
            ToolExecutionOutcome::Completed(value) => Ok(value),
            ToolExecutionOutcome::FailedBeforeInvocation(error)
            | ToolExecutionOutcome::Failed(error) => Err(error),
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

pub fn empty() -> McpRuntime {
    let challenges = Arc::new(Mutex::new(BTreeMap::new()));
    let servers = Arc::new(RwLock::new(BTreeMap::new()));
    let manager = McpServerManager::new().with_handler_config(
        McpHandlerConfig::new().with_auth_responder(Arc::new(AuthRecorder {
            challenges: Arc::clone(&challenges),
            servers: Arc::clone(&servers),
        })),
    );
    McpRuntime::new(
        manager,
        servers,
        challenges,
        BTreeMap::new(),
        CredentialStorage::Memory,
        true,
    )
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
    let limit = match term.chars().count() {
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
    Ok(spec_values(
        matches.into_iter().map(|(_, spec)| spec).collect(),
        20,
    ))
}

fn spec_values(specs: Vec<ToolSpec>, limit: usize) -> Vec<Value> {
    specs
        .into_iter()
        .take(limit)
        .map(|spec| {
            let mut value = json!({
                "name":spec.name.0,
                "description":spec.description,
                "input_schema":spec.input_schema
            });
            if let Some(output) = spec.output_schema {
                value["output_schema"] = output;
            }
            value
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use agentkit_core::MetadataMap;
    use agentkit_tools_core::{ToolName, ToolSpec};
    use serde_json::{Value, json};

    use super::{
        Config, CredentialStorage, challenge_requires_interactive_authorization, search_specs,
    };

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
    fn insufficient_scope_without_required_scope_requires_interactive_authorization() {
        let mut challenge = MetadataMap::new();
        assert!(!challenge_requires_interactive_authorization(&challenge));

        challenge.insert("insufficient_scope".into(), Value::Bool(true));
        assert!(challenge_requires_interactive_authorization(&challenge));
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
    fn config_is_strict_and_accepts_oauth_servers() {
        assert!(
            serde_json::from_str::<Config>(
                r#"{"mcpServers":{"ok":{"command":"server","args":["--stdio"],"env":{"A":"B"}}}}"#
            )
            .is_ok()
        );
        assert!(serde_json::from_str::<Config>(r#"{"mcpServers":{"linear":{"url":"https://mcp.example/mcp","description":"Issue tracking","auth":{"type":"oauth","scopes":[]}}}}"#).is_ok());
        assert!(serde_json::from_str::<Config>(r#"{"mcpServers":{},"extra":true}"#).is_err());
        assert!(
            serde_json::from_str::<Config>(
                r#"{"mcpServers":{"bad":{"command":"x","url":"http://localhost"}}}"#
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn search_groups_lazy_oauth_servers_without_authenticating() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"linear":{"url":"https://unused.invalid/mcp","description":"Issues and project management","auth":{"type":"oauth","scopes":[]}}}}"#,
        )
        .unwrap();
        let runtime = super::connect(&path, true, CredentialStorage::Memory)
            .await
            .unwrap();
        assert_eq!(
            runtime.search("issues").await.unwrap(),
            json!({"servers":[{
                "name":"linear",
                "description":"Issues and project management",
                "status":"authentication_required",
                "tools":[]
            }]})
        );
        assert_eq!(
            runtime.search("calendar").await.unwrap(),
            json!({"servers":[]})
        );
        let one_shot = super::connect(&path, false, CredentialStorage::Memory)
            .await
            .unwrap();
        assert_eq!(
            one_shot.search("linear").await.unwrap()["servers"][0]["status"],
            "authentication_unavailable"
        );
    }

    #[tokio::test]
    async fn remote_auth_failure_is_searchable_without_failing_startup() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(
            &path,
            format!(r#"{{"mcpServers":{{"remote":{{"url":"http://{address}/mcp"}}}}}}"#),
        )
        .unwrap();

        let runtime = super::connect(&path, true, CredentialStorage::Memory)
            .await
            .expect("authentication failure should not prevent startup");
        server.await.unwrap();
        let results = runtime.search("mcp").await.unwrap();
        assert_eq!(results["servers"][0]["name"], "remote");
        assert_eq!(results["servers"][0]["status"], "error");
        assert!(
            results["servers"][0]["error"]
                .as_str()
                .unwrap()
                .contains("auth required")
        );
    }

    #[tokio::test]
    async fn connection_failure_is_searchable_without_failing_startup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"broken":{"command":"kit-command-that-does-not-exist","description":"Broken test server"},"linear":{"url":"https://unused.invalid/mcp","auth":{"type":"oauth"}}}}"#,
        )
        .unwrap();
        let runtime = super::connect(&path, true, CredentialStorage::Memory)
            .await
            .expect("connection failure should not prevent startup");
        let results = runtime.search("mcp").await.unwrap();
        let broken = &results["servers"][0];
        assert_eq!(broken["name"], "broken");
        assert_eq!(broken["status"], "error");
        assert!(
            broken["error"]
                .as_str()
                .unwrap()
                .contains("could not connect MCP server broken")
        );
        assert_eq!(results["servers"][1]["name"], "linear");
        assert_eq!(results["servers"][1]["status"], "authentication_required");
    }
}
