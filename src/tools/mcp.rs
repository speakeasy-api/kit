mod auth;
mod credentials;

pub use crate::credentials::CredentialStorage;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
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
use tokio::sync::{Mutex, RwLock, Semaphore, mpsc, oneshot};

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
    reload: Mutex<ReloadState>,
    reload_flight: Arc<Semaphore>,
    initialization: Mutex<()>,
    auth_setup: Mutex<()>,
    event_routes: Arc<std::sync::Mutex<BTreeMap<String, (u64, mpsc::UnboundedSender<McpEvent>)>>>,
    next_event_route: AtomicU64,
    interactive_oauth_enabled: bool,
}

#[derive(Clone)]
struct ServerRecord {
    description: String,
    url: Option<String>,
    oauth: Option<auth::Config>,
    fingerprint: Vec<u8>,
    status: ServerStatus,
}

struct ReloadState {
    path: Option<PathBuf>,
    raw: Vec<u8>,
    entries: BTreeMap<String, Vec<u8>>,
}

struct PreparedServer {
    config: McpServerConfig,
    record: ServerRecord,
}

#[derive(Clone)]
enum ServerStatus {
    Uninitialized,
    Connected,
    AuthenticationRequired,
    Pending,
    Error(String),
}

impl ServerStatus {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Uninitialized => "available",
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
    fingerprint: Vec<u8>,
    abort: tokio::task::AbortHandle,
}

#[derive(Clone, Debug)]
pub(crate) struct McpEvent {
    pub message: String,
}

pub(crate) struct McpSubscription {
    session_id: String,
    generation: u64,
    routes: Arc<std::sync::Mutex<BTreeMap<String, (u64, mpsc::UnboundedSender<McpEvent>)>>>,
    receiver: mpsc::UnboundedReceiver<McpEvent>,
}

impl McpSubscription {
    pub(crate) async fn recv(&mut self) -> Option<McpEvent> {
        self.receiver.recv().await
    }
}

impl Drop for McpSubscription {
    fn drop(&mut self) {
        if let Ok(mut routes) = self.routes.lock()
            && routes
                .get(&self.session_id)
                .is_some_and(|(generation, _)| *generation == self.generation)
        {
            routes.remove(&self.session_id);
        }
    }
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

fn prepare_config(
    bytes: &[u8],
    path: &Path,
) -> Result<(BTreeMap<String, PreparedServer>, BTreeMap<String, Vec<u8>>), String> {
    let config: Config = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid MCP config {}: {error}", path.display()))?;
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid MCP config {}: {error}", path.display()))?;
    let raw_entries = value
        .get("mcpServers")
        .and_then(Value::as_object)
        .expect("strict Config parsing guarantees an mcpServers object");
    let entries = raw_entries
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                serde_json::to_vec(value).expect("JSON re-encodes"),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut prepared = BTreeMap::new();
    for (id, server) in config.mcp_servers {
        if id.trim().is_empty() {
            return Err("MCP server names must not be empty".into());
        }
        let fingerprint = entries[&id].clone();
        let (binding, record) = prepare_server(&id, server, fingerprint)?;
        prepared.insert(
            id.clone(),
            PreparedServer {
                config: McpServerConfig::new(id, binding),
                record,
            },
        );
    }
    Ok((prepared, entries))
}

fn prepare_server(
    id: &str,
    server: Server,
    fingerprint: Vec<u8>,
) -> Result<(McpTransportBinding, ServerRecord), String> {
    match server {
        Server::Stdio(server) => {
            if server.command.trim().is_empty() {
                return Err(format!("MCP server {id} has an empty command"));
            }
            let mut transport = StdioTransportConfig::new(server.command);
            transport.args = server.args;
            transport.env = server.env.into_iter().collect();
            transport.cwd = server.cwd;
            Ok((
                McpTransportBinding::Stdio(transport),
                ServerRecord {
                    description: server.description.unwrap_or_else(|| id.to_string()),
                    url: None,
                    oauth: None,
                    fingerprint,
                    status: ServerStatus::Uninitialized,
                },
            ))
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
            Ok((
                McpTransportBinding::StreamableHttp(transport),
                ServerRecord {
                    description: server.description.unwrap_or_else(|| id.to_string()),
                    url: Some(server.url),
                    oauth: server.auth,
                    fingerprint,
                    status: ServerStatus::Uninitialized,
                },
            ))
        }
    }
}

pub async fn connect(
    path: &Path,
    interactive_oauth_enabled: bool,
    credential_storage: CredentialStorage,
) -> Result<McpRuntime, String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| format!("could not read MCP config {}: {error}", path.display()))?;
    let (prepared, entries) = prepare_config(&bytes, path)?;
    let challenges = Arc::new(Mutex::new(BTreeMap::new()));
    let mut manager = McpServerManager::new();
    let mut records = BTreeMap::new();

    for (id, server) in prepared {
        let PreparedServer { config, record } = server;
        manager.register_server_with_options(
            config,
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
    Ok(McpRuntime::new(
        manager,
        servers,
        challenges,
        BTreeMap::new(),
        credential_storage,
        ReloadState {
            path: Some(path.to_path_buf()),
            raw: bytes,
            entries,
        },
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
        reload: ReloadState,
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
                reload: Mutex::new(reload),
                reload_flight: Arc::new(Semaphore::new(1)),
                initialization: Mutex::new(()),
                auth_setup: Mutex::new(()),
                event_routes: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
                next_event_route: AtomicU64::new(0),
                interactive_oauth_enabled,
            }),
        }
    }

    pub fn catalog(&self) -> CatalogReader {
        self.inner.catalog.clone()
    }

    pub(crate) fn subscribe(&self, session_id: String) -> McpSubscription {
        let generation = self.inner.next_event_route.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::unbounded_channel();
        self.inner
            .event_routes
            .lock()
            .expect("MCP event routes are poisoned")
            .insert(session_id.clone(), (generation, sender));
        McpSubscription {
            session_id,
            generation,
            routes: Arc::clone(&self.inner.event_routes),
            receiver,
        }
    }

    fn event_generation(&self, session_id: &str) -> Option<u64> {
        self.inner
            .event_routes
            .lock()
            .ok()
            .and_then(|routes| routes.get(session_id).map(|(generation, _)| *generation))
    }

    fn publish_to(&self, session_id: &str, generation: Option<u64>, event: McpEvent) {
        let Some(generation) = generation else {
            return;
        };
        let sender = self.inner.event_routes.lock().ok().and_then(|routes| {
            routes
                .get(session_id)
                .and_then(|(current, sender)| (*current == generation).then(|| sender.clone()))
        });
        if let Some(sender) = sender {
            let _ = sender.send(event);
        }
    }

    #[cfg(test)]
    pub(crate) fn publish(&self, session_id: &str, event: McpEvent) {
        self.publish_to(session_id, self.event_generation(session_id), event);
    }

    async fn reload_config(&self) -> Result<(), String> {
        let permit = Arc::clone(&self.inner.reload_flight)
            .acquire_owned()
            .await
            .map_err(|_| "MCP config reload coordinator closed".to_string())?;
        let runtime = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            runtime.reload_config_inner().await
        })
        .await
        .map_err(|error| format!("MCP config reload task failed: {error}"))?
    }

    async fn reload_config_inner(&self) -> Result<(), String> {
        let mut state = self.inner.reload.lock().await;
        let _initialization = self.inner.initialization.lock().await;
        let Some(path) = state.path.clone() else {
            return Ok(());
        };
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| format!("could not read MCP config {}: {error}", path.display()))?;
        if bytes == state.raw {
            return Ok(());
        }

        // Validate and prepare every transport before changing live state.
        let (mut prepared, entries) = prepare_config(&bytes, &path)?;
        let changed = state
            .entries
            .keys()
            .chain(entries.keys())
            .filter(|name| state.entries.get(*name) != entries.get(*name))
            .cloned()
            .collect::<BTreeSet<_>>();
        if changed.is_empty() {
            state.raw = bytes;
            return Ok(());
        }

        {
            let mut manager = self.inner.manager.lock().await;
            for name in &changed {
                let _ = manager.unregister_server(&McpServerId::new(name)).await;
            }
            for name in &changed {
                if let Some(server) = prepared.get(name) {
                    manager.register_server_with_options(
                        server.config.clone(),
                        McpServerOptions::new().with_timeout(CONNECT_TIMEOUT),
                    );
                }
            }
        }
        {
            let mut servers = self.inner.servers.write().await;
            for name in &changed {
                servers.remove(name);
                if let Some(server) = prepared.remove(name) {
                    servers.insert(name.clone(), server.record);
                }
            }
        }
        {
            let mut challenges = self.inner.challenges.lock().await;
            let mut pending = self.inner.pending.lock().await;
            let mut oauth_managers = self.inner.oauth_managers.lock().await;
            for name in &changed {
                challenges.remove(name);
                if let Some(pending) = pending.remove(name) {
                    pending.abort.abort();
                }
                oauth_managers.remove(name);
            }
        }
        state.raw = bytes;
        state.entries = entries;
        Ok(())
    }

    async fn initialize_server(&self, name: &str) {
        let _initialization = self.inner.initialization.lock().await;
        let Some(record) = self.inner.servers.read().await.get(name).cloned() else {
            return;
        };
        if !matches!(record.status, ServerStatus::Uninitialized) {
            return;
        }

        if let (Some(resource_url), Some(oauth)) = (record.url, record.oauth) {
            if !self.inner.credential_storage.is_persistent() {
                self.set_status(name, ServerStatus::AuthenticationRequired)
                    .await;
                return;
            }
            let restored =
                match auth::restore(&resource_url, &oauth, &self.inner.credential_storage).await {
                    Ok(restored) => restored,
                    Err(error) => {
                        self.set_status(name, ServerStatus::Error(error)).await;
                        return;
                    }
                };
            let Some((token, oauth_manager)) = restored else {
                self.set_status(name, ServerStatus::AuthenticationRequired)
                    .await;
                return;
            };
            let request = connect_auth_request(name);
            let mut credentials = MetadataMap::new();
            credentials.insert("access_token".into(), Value::String(token));
            let result = {
                let mut manager = self.inner.manager.lock().await;
                match manager
                    .resolve_auth(AuthResolution::provided(request, credentials))
                    .await
                {
                    Ok(()) => manager.connect_server(&McpServerId::new(name)).await,
                    Err(error) => {
                        self.set_status(
                            name,
                            ServerStatus::Error(format!(
                                "could not restore MCP credentials for {name}: {error}"
                            )),
                        )
                        .await;
                        return;
                    }
                }
            };
            self.inner
                .oauth_managers
                .lock()
                .await
                .insert(name.to_string(), oauth_manager);
            match result {
                Ok(_) => self.set_status(name, ServerStatus::Connected).await,
                Err(McpError::AuthRequired(request)) => {
                    self.inner
                        .challenges
                        .lock()
                        .await
                        .insert(name.to_string(), *request);
                    self.set_status(name, ServerStatus::AuthenticationRequired)
                        .await;
                }
                Err(error) => {
                    self.set_status(
                        name,
                        ServerStatus::Error(format!(
                            "could not connect MCP server {name} with stored credentials: {error}"
                        )),
                    )
                    .await;
                }
            }
            return;
        }

        let result = self
            .inner
            .manager
            .lock()
            .await
            .connect_server(&McpServerId::new(name))
            .await;
        match result {
            Ok(_) => self.set_status(name, ServerStatus::Connected).await,
            Err(error) => {
                self.set_status(
                    name,
                    ServerStatus::Error(format!("could not connect MCP server {name}: {error}")),
                )
                .await;
            }
        }
    }

    async fn search(&self, query: &str) -> Result<Value, ToolError> {
        let prepared = PreparedQuery::new(query).ok_or_else(|| {
            ToolError::InvalidInput("query must contain a letter or number".into())
        })?;
        self.reload_config().await.map_err(ToolError::Unavailable)?;
        let wildcard = query == "mcp";
        let configured = self.inner.servers.read().await.clone();
        for (name, record) in &configured {
            let server_spec = PreparedSpec::new(ToolSpec::new(
                ToolName::new(name),
                &record.description,
                json!({"type":"object"}),
            ));
            if wildcard
                || score_spec(
                    &prepared,
                    &server_spec,
                    &vec![false; prepared.0.tokens.len()],
                )
                .is_some()
            {
                self.initialize_server(name).await;
            }
        }
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

    async fn authorize(&self, name: &str, session_id: String) -> Result<Value, ToolError> {
        self.reload_config().await.map_err(ToolError::Unavailable)?;
        if !self.inner.interactive_oauth_enabled {
            return Err(ToolError::Unavailable(
                "interactive MCP authentication requires the tui, serve, or acp command".into(),
            ));
        }
        self.initialize_server(name).await;
        let _setup = self.inner.auth_setup.lock().await;
        let _reload = self.inner.reload.lock().await;
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

        let challenge = { self.inner.challenges.lock().await.get(name).cloned() };
        let request = match challenge {
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
            && let Some(mut manager) = self.inner.oauth_managers.lock().await.remove(name)
            && let Ok(token) = auth::refresh(&mut manager, &self.inner.credential_storage).await
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
        self.set_status(name, ServerStatus::Pending).await;
        let runtime = self.clone();
        let server = name.to_string();
        let fingerprint = record.fingerprint.clone();
        let event_generation = self.event_generation(&session_id);
        let (start, started) = oneshot::channel();
        let task = tokio::spawn(async move {
            if started.await.is_ok() {
                runtime
                    .complete_authorization(
                        server,
                        fingerprint,
                        request,
                        pending,
                        session_id,
                        event_generation,
                    )
                    .await;
            }
        });
        self.inner.pending.lock().await.insert(
            name.to_string(),
            PendingRecord {
                url: url.clone(),
                expires: Instant::now() + auth::FLOW_TIMEOUT,
                fingerprint: record.fingerprint,
                abort: task.abort_handle(),
            },
        );
        let _ = start.send(());
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
        fingerprint: Vec<u8>,
        request: AuthRequest,
        pending: auth::PendingAuthorization,
        session_id: String,
        event_generation: Option<u64>,
    ) {
        let finished = auth::finish(pending).await;
        let _reload = self.inner.reload.lock().await;
        let current = self
            .inner
            .servers
            .read()
            .await
            .get(&server)
            .is_some_and(|record| record.fingerprint == fingerprint);
        if !current {
            return;
        }
        let result = async {
            let (token, oauth_manager) = finished?;
            self.apply_credentials(&server, request, token).await?;
            Ok::<_, String>(oauth_manager)
        }
        .await;
        let mut pending = self.inner.pending.lock().await;
        if pending
            .get(&server)
            .is_some_and(|record| record.fingerprint == fingerprint)
        {
            pending.remove(&server);
        }
        drop(pending);
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
                    .insert(server.clone(), manager);
                self.publish_to(
                    &session_id,
                    event_generation,
                    McpEvent {
                        message: format!(
                            "MCP server {server} connected. Its tools are available now; continue the task using tool_search and tool as needed."
                        ),
                    },
                );
            }
            Err(error) => {
                eprintln!("MCP authentication for {server} failed: {error}");
                self.publish_to(
                    &session_id,
                    event_generation,
                    McpEvent {
                        message: format!(
                            "MCP server {server} failed to connect after authentication: {error}"
                        ),
                    },
                );
            }
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
                "Reload the MCP config, then search configured server names and descriptions plus discovered tool names. Matching servers connect lazily; use `mcp` to initialize and list all servers. Before first connection, only the configured name and description are searchable.",
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
                "Reload the MCP config and start OAuth for a configured remote server. Return the URL to the user. Browser completion connects the server and notifies the originating ACP session automatically.",
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
        let result = self
            .runtime
            .authorize(&input.name, request.session_id.to_string())
            .await?;
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
        ReloadState {
            path: None,
            raw: Vec::new(),
            entries: BTreeMap::new(),
        },
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
    async fn search_and_auth_reload_servers_added_after_connect() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(&path, r#"{"mcpServers":{}}"#).unwrap();
        let runtime = super::connect(&path, true, CredentialStorage::Memory)
            .await
            .unwrap();

        std::fs::write(
            &path,
            r#"{"mcpServers":{"linear":{"url":"https://unused.invalid/mcp","description":"Issue tracking","auth":{"type":"oauth"}},"local":{"command":"kit-command-that-does-not-exist","description":"Local helper"}}}"#,
        )
        .unwrap();

        let results = runtime.search("issue").await.unwrap();
        assert_eq!(results["servers"][0]["name"], "linear");
        assert_eq!(results["servers"][0]["status"], "authentication_required");
        let error = runtime
            .authorize("local", "session".into())
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("MCP server local is not configured for OAuth"),
            "added server should be found by auth lookup: {error}"
        );
    }

    #[tokio::test]
    async fn invalid_reload_preserves_the_last_valid_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        let valid = r#"{"mcpServers":{"linear":{"url":"https://unused.invalid/mcp","description":"Issue tracking","auth":{"type":"oauth"}}}}"#;
        std::fs::write(&path, valid).unwrap();
        let runtime = super::connect(&path, true, CredentialStorage::Memory)
            .await
            .unwrap();

        std::fs::write(
            &path,
            r#"{"mcpServers":{"replacement":{"command":"server","unknown":true}}}"#,
        )
        .unwrap();
        let error = runtime.search("linear").await.unwrap_err();
        assert!(error.to_string().contains("invalid MCP config"));

        std::fs::write(&path, valid).unwrap();
        let results = runtime.search("linear").await.unwrap();
        assert_eq!(results["servers"][0]["name"], "linear");
        assert_eq!(results["servers"][0]["status"], "authentication_required");
    }

    #[tokio::test]
    async fn completion_events_are_routed_to_one_session() {
        let runtime = super::empty();
        let mut first = runtime.subscribe("first".into());
        let mut second = runtime.subscribe("second".into());
        runtime.publish_to(
            "first",
            runtime.event_generation("first"),
            super::McpEvent {
                message: "connected".into(),
            },
        );
        assert_eq!(first.recv().await.unwrap().message, "connected");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), second.recv())
                .await
                .is_err()
        );

        let generation = runtime.event_generation("first");
        drop(first);
        let mut replacement = runtime.subscribe("first".into());
        runtime.publish_to(
            "first",
            generation,
            super::McpEvent {
                message: "stale callback".into(),
            },
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), replacement.recv())
                .await
                .is_err(),
            "a stale callback reached a replacement session"
        );
    }

    #[tokio::test]
    async fn remote_auth_failure_is_searchable_without_failing_startup() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let bytes_read = stream.read(&mut request).await.unwrap();
            assert!(bytes_read > 0);
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
            .expect("registration should not contact the server");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut server)
                .await
                .is_err(),
            "MCP server was contacted during startup"
        );
        assert!(
            runtime.search("calendar").await.unwrap()["servers"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let results = runtime.search("remote").await.unwrap();
        server.await.unwrap();
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
        assert!(
            runtime.search("MCP").await.unwrap()["servers"]
                .as_array()
                .unwrap()
                .is_empty()
        );
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
