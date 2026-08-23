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
use agentkit_plugins::PluginMcpTransport;
use agentkit_tools_core::{
    CatalogReader, Tool, ToolContext, ToolError, ToolExecutionOutcome, ToolExecutionScope,
    ToolName, ToolRequest, ToolResult, ToolSource, ToolSpec,
};
use async_trait::async_trait;
use rmcp::transport::auth::AuthorizationManager;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock, Semaphore, mpsc, oneshot};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_TOOL_TIMEOUT_SECONDS: u64 = 3_600;
// These messages are part of the pinned agentkit-mcp 0.10.6 adapter contract.
// Revalidate them before upgrading that dependency.
const AGENTKIT_REPLAY_REJECTED: &str = "MCP auth challenge unresolved after retry";
const AGENTKIT_RESPONDER_FAILED: &str = "auth responder failed:";
const AGENTKIT_APPLY_AUTH_FAILED: &str = "applying auth resolution failed:";

type EventRoutes = Arc<std::sync::Mutex<BTreeMap<String, (u64, mpsc::UnboundedSender<McpEvent>)>>>;
type OAuthSessions = Arc<Mutex<BTreeMap<String, Arc<OAuthSession>>>>;
type ActiveReplays = Arc<Mutex<BTreeMap<String, u64>>>;
type PreparedServers = BTreeMap<String, PreparedServer>;
type ServerFingerprints = BTreeMap<String, Vec<u8>>;
type PreparedConfiguration = (PreparedServers, ServerFingerprints);

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
    oauth_sessions: OAuthSessions,
    active_replays: ActiveReplays,
    credential_storage: CredentialStorage,
    reload: Mutex<ReloadState>,
    reload_flight: Arc<Semaphore>,
    initialization: Mutex<()>,
    auth_setup: Mutex<()>,
    operations: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    event_routes: EventRoutes,
    next_event_route: AtomicU64,
    interactive_oauth_enabled: bool,
}

#[derive(Clone)]
struct ServerRecord {
    description: String,
    url: Option<String>,
    oauth: Option<auth::Config>,
    static_authorization: bool,
    fingerprint: Vec<u8>,
    status: ServerStatus,
}

struct ReloadState {
    path: Option<PathBuf>,
    raw: Vec<u8>,
    entries: BTreeMap<String, Vec<u8>>,
    plugins: BTreeMap<String, PreparedServer>,
    plugin_entries: BTreeMap<String, Vec<u8>>,
}

#[derive(Clone)]
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

struct OAuthSession {
    manager: Mutex<AuthorizationManager>,
    tokens: std::sync::Mutex<OAuthSessionTokens>,
}

struct OAuthSessionTokens {
    applied: String,
    pending: Option<PendingRefresh>,
    next_generation: u64,
}

enum PendingRefresh {
    Refreshing { generation: u64, cancelled: bool },
    Ready { generation: u64, token: String },
}

enum OpportunisticRefresh {
    Apply { token: String, generation: u64 },
    Coalesced,
}

impl OAuthSession {
    fn new(manager: AuthorizationManager, access_token: String) -> Self {
        Self {
            manager: Mutex::new(manager),
            tokens: std::sync::Mutex::new(OAuthSessionTokens {
                applied: access_token,
                pending: None,
                next_generation: 0,
            }),
        }
    }

    async fn refresh(&self, credential_storage: &CredentialStorage) -> Result<String, String> {
        let rejected_token = self
            .tokens
            .lock()
            .expect("OAuth session tokens are poisoned")
            .applied
            .clone();
        let mut manager = self.manager.lock().await;
        let token = auth::refresh(&mut manager, credential_storage, &rejected_token)
            .await
            .map_err(|error| error.to_string())?;
        let mut tokens = self
            .tokens
            .lock()
            .expect("OAuth session tokens are poisoned");
        tokens.applied = token.clone();
        tokens.pending = None;
        Ok(token)
    }

    async fn opportunistic_refresh(
        self: &Arc<Self>,
        credential_storage: &CredentialStorage,
    ) -> Result<OpportunisticRefresh, String> {
        let (observed_token, generation) = {
            let mut tokens = self
                .tokens
                .lock()
                .expect("OAuth session tokens are poisoned");
            if tokens.pending.is_some() {
                return Ok(OpportunisticRefresh::Coalesced);
            }
            tokens.next_generation = tokens.next_generation.wrapping_add(1);
            let generation = tokens.next_generation;
            let observed_token = tokens.applied.clone();
            tokens.pending = Some(PendingRefresh::Refreshing {
                generation,
                cancelled: false,
            });
            (observed_token, generation)
        };
        let session = Arc::clone(self);
        let credential_storage = credential_storage.clone();
        tokio::spawn(async move {
            let result = {
                let mut manager = session.manager.lock().await;
                auth::refresh(&mut manager, &credential_storage, &observed_token)
                    .await
                    .map_err(|error| error.to_string())
            };
            let mut tokens = session
                .tokens
                .lock()
                .expect("OAuth session tokens are poisoned");
            let cancelled = match tokens.pending {
                Some(PendingRefresh::Refreshing {
                    generation: current,
                    cancelled,
                }) if current == generation => cancelled,
                _ => return Err("MCP credential refresh was superseded".into()),
            };
            if cancelled {
                tokens.pending = None;
                return Err("MCP credential refresh was cancelled".into());
            }
            match result {
                Ok(token) => {
                    tokens.pending = Some(PendingRefresh::Ready {
                        generation,
                        token: token.clone(),
                    });
                    Ok(OpportunisticRefresh::Apply { token, generation })
                }
                Err(error) => {
                    tokens.pending = None;
                    Err(error)
                }
            }
        })
        .await
        .map_err(|error| format!("MCP credential refresh task failed: {error}"))?
    }

    fn finish_replay(&self, commit: bool) -> Option<u64> {
        let mut tokens = self
            .tokens
            .lock()
            .expect("OAuth session tokens are poisoned");
        match tokens.pending.take()? {
            PendingRefresh::Ready { generation, token } => {
                if commit {
                    tokens.applied = token;
                }
                Some(generation)
            }
            PendingRefresh::Refreshing { generation, .. } => {
                tokens.pending = Some(PendingRefresh::Refreshing {
                    generation,
                    cancelled: true,
                });
                Some(generation)
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct McpEvent {
    pub message: String,
}

pub(crate) struct McpSubscription {
    session_id: String,
    generation: u64,
    routes: EventRoutes,
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
    oauth_sessions: OAuthSessions,
    active_replays: ActiveReplays,
    credential_storage: CredentialStorage,
}

#[async_trait]
impl McpAuthResponder for AuthRecorder {
    async fn resolve(&self, request: AuthRequest) -> Result<AuthResolution, McpError> {
        let server = request.server_id().unwrap_or("unknown").to_string();
        let record = self.servers.read().await.get(&server).cloned();
        let refreshable = record
            .as_ref()
            .is_some_and(|record| can_opportunistically_refresh(&request, record));

        let session = if refreshable {
            self.oauth_sessions.lock().await.get(&server).cloned()
        } else {
            None
        };
        if let Some(session) = session {
            let refresh = session
                .opportunistic_refresh(&self.credential_storage)
                .await;
            if matches!(refresh, Ok(OpportunisticRefresh::Coalesced)) {
                return Err(McpError::AuthResolution(format!(
                    "MCP credentials for {server} were refreshed by another request; retry the tool call"
                )));
            }
            if let Ok(OpportunisticRefresh::Apply { token, generation }) = refresh {
                let current_session = self
                    .oauth_sessions
                    .lock()
                    .await
                    .get(&server)
                    .is_some_and(|current| Arc::ptr_eq(current, &session));
                let current_server = match record.as_ref() {
                    Some(record) => self
                        .servers
                        .read()
                        .await
                        .get(&server)
                        .is_some_and(|current| current.fingerprint == record.fingerprint),
                    None => false,
                };
                if current_session && current_server {
                    self.challenges
                        .lock()
                        .await
                        .insert(server.clone(), request.clone());
                    let still_current =
                        self.servers
                            .read()
                            .await
                            .get(&server)
                            .is_some_and(|current| {
                                record
                                    .as_ref()
                                    .is_some_and(|record| current.fingerprint == record.fingerprint)
                            });
                    if still_current {
                        self.active_replays
                            .lock()
                            .await
                            .insert(server.clone(), generation);
                        let mut credentials = MetadataMap::new();
                        credentials.insert("access_token".into(), Value::String(token));
                        return Ok(AuthResolution::provided(request, credentials));
                    }
                    let mut challenges = self.challenges.lock().await;
                    if challenges.get(&server) == Some(&request) {
                        challenges.remove(&server);
                    }
                }
            }
        }

        let current_server = match record.as_ref() {
            Some(record) => self
                .servers
                .read()
                .await
                .get(&server)
                .is_some_and(|current| current.fingerprint == record.fingerprint),
            None => false,
        };
        if current_server {
            self.challenges
                .lock()
                .await
                .insert(server.clone(), request.clone());
            let updated = if let (Some(original), Some(current)) =
                (record.as_ref(), self.servers.write().await.get_mut(&server))
                && current.fingerprint == original.fingerprint
            {
                current.status = ServerStatus::AuthenticationRequired;
                true
            } else {
                false
            };
            if !updated {
                let mut challenges = self.challenges.lock().await;
                if challenges.get(&server) == Some(&request) {
                    challenges.remove(&server);
                }
            }
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

fn agentkit_replay_rejected(error: &str) -> bool {
    error.contains(AGENTKIT_REPLAY_REJECTED)
}

fn agentkit_auth_not_applied(error: &str) -> bool {
    error.contains(AGENTKIT_RESPONDER_FAILED) || error.contains(AGENTKIT_APPLY_AUTH_FAILED)
}

fn serializes_tool_calls(record: &ServerRecord) -> bool {
    record.url.is_some() && !record.static_authorization
}

fn can_opportunistically_refresh(request: &AuthRequest, record: &ServerRecord) -> bool {
    record.url.is_some()
        && !record.static_authorization
        && matches!(record.status, ServerStatus::Connected)
        && matches!(&request.operation, AuthOperation::McpToolCall { .. })
        && request.challenge.get("flow_kind").and_then(Value::as_str) == Some("http_bearer")
        && !challenge_requires_interactive_authorization(&request.challenge)
}

fn prepare_config(bytes: &[u8], path: &Path) -> Result<PreparedConfiguration, String> {
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

fn validate_server_names<'a>(names: impl Iterator<Item = &'a String>) -> Result<(), String> {
    let names = names.collect::<Vec<_>>();
    for (index, name) in names.iter().enumerate() {
        for other in &names[index + 1..] {
            if name.starts_with(&format!("{other}_")) || other.starts_with(&format!("{name}_")) {
                return Err(format!(
                    "MCP server names {name:?} and {other:?} produce ambiguous tool names"
                ));
            }
        }
    }
    Ok(())
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
                    static_authorization: false,
                    fingerprint,
                    status: ServerStatus::Uninitialized,
                },
            ))
        }
        Server::Http(server) => {
            if server.url.trim().is_empty() {
                return Err(format!("MCP server {id} has an empty URL"));
            }
            let authorization_header = server
                .headers
                .keys()
                .any(|name| name.eq_ignore_ascii_case("authorization"));
            if server.auth.is_some() && (server.bearer_token.is_some() || authorization_header) {
                return Err(format!(
                    "MCP server {id} cannot use both OAuth and static authorization"
                ));
            }
            let mut transport = StreamableHttpTransportConfig::new(server.url.clone());
            let static_authorization = server.bearer_token.is_some() || authorization_header;
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
                    static_authorization,
                    fingerprint,
                    status: ServerStatus::Uninitialized,
                },
            ))
        }
    }
}

fn plugin_text(path: &Path, field: &str, alias: &str) -> Result<String, String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        format!(
            "plugin {alias:?} has a non-UTF-8 {field} path: {}",
            path.display()
        )
    })
}

fn expand_plugin_value(value: &str, root: &str, data: &str) -> String {
    const ROOT: &str = "${PLUGIN_ROOT}";
    const DATA: &str = "${PLUGIN_DATA}";
    let mut output = String::with_capacity(value.len());
    let mut remaining = value;
    while let Some(index) = [remaining.find(ROOT), remaining.find(DATA)]
        .into_iter()
        .flatten()
        .min()
    {
        output.push_str(&remaining[..index]);
        remaining = &remaining[index..];
        if remaining.starts_with(ROOT) {
            output.push_str(root);
            remaining = &remaining[ROOT.len()..];
        } else {
            output.push_str(data);
            remaining = &remaining[DATA.len()..];
        }
    }
    output.push_str(remaining);
    output
}

fn contained_plugin_path(path: PathBuf, root: &Path, context: &str) -> Result<PathBuf, String> {
    let mut ancestor = path.clone();
    let mut suffix = Vec::new();
    loop {
        match std::fs::symlink_metadata(&ancestor) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let component = ancestor
                    .file_name()
                    .ok_or_else(|| format!("plugin MCP {context} path has no existing ancestor"))?;
                suffix.push(component.to_os_string());
                ancestor.pop();
            }
            Err(error) => {
                return Err(format!(
                    "could not inspect plugin MCP {context} path {}: {error}",
                    ancestor.display()
                ));
            }
        }
    }
    let mut resolved = ancestor.canonicalize().map_err(|error| {
        format!(
            "could not resolve plugin MCP {context} path {}: {error}",
            ancestor.display()
        )
    })?;
    if !resolved.starts_with(root) {
        return Err(format!(
            "plugin MCP {context} path resolves outside {}",
            root.display()
        ));
    }
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn prepare_plugins(
    plugins: &[crate::plugins::ResolvedPluginMcp],
) -> Result<PreparedConfiguration, String> {
    let mut prepared = BTreeMap::new();
    let mut entries = BTreeMap::new();
    let mut owners = BTreeMap::<String, String>::new();
    for plugin in plugins {
        let root = plugin_text(&plugin.root, "root", &plugin.alias)?;
        let data = plugin_text(&plugin.data_dir, "data", &plugin.alias)?;
        for server in &plugin.servers {
            if matches!(server.transport, PluginMcpTransport::Sse { .. }) {
                eprintln!(
                    "plugin {}: MCP server {:?} uses unsupported SSE transport; skipping",
                    plugin.alias, server.name
                );
                continue;
            }
            if let Some(owner) = owners.insert(server.name.clone(), plugin.alias.clone()) {
                return Err(format!(
                    "MCP server {:?} is declared by both plugins {owner:?} and {:?}",
                    server.name, plugin.alias
                ));
            }
            let description = format!("{} plugin MCP server", plugin.manifest_name);
            let fingerprint =
                format!("plugin:{}:{:?}", plugin.alias, server.transport).into_bytes();
            let (binding, url, static_authorization) = match &server.transport {
                PluginMcpTransport::Stdio {
                    command,
                    args,
                    env,
                    cwd,
                } => {
                    let command = if let Some(command) = command.strip_prefix("./") {
                        let command = contained_plugin_path(
                            plugin.root.join(command),
                            &plugin.root,
                            "command",
                        )?;
                        plugin_text(&command, "command", &plugin.alias)?
                    } else {
                        command.clone()
                    };
                    let mut transport = StdioTransportConfig::new(command);
                    transport.args = args
                        .iter()
                        .map(|value| expand_plugin_value(value, &root, &data))
                        .collect();
                    transport.env = env
                        .iter()
                        .map(|(name, value)| {
                            (name.clone(), expand_plugin_value(value, &root, &data))
                        })
                        .chain([
                            ("PLUGIN_ROOT".to_owned(), root.clone()),
                            ("PLUGIN_DATA".to_owned(), data.clone()),
                        ])
                        .collect();
                    transport.cwd = match cwd.as_deref() {
                        Some("${PLUGIN_ROOT}") => Some(plugin.root.clone()),
                        Some("${PLUGIN_DATA}") => Some(plugin.data_dir.clone()),
                        Some(cwd) if cwd.starts_with("${PLUGIN_ROOT}/") => {
                            Some(contained_plugin_path(
                                plugin.root.join(&cwd["${PLUGIN_ROOT}/".len()..]),
                                &plugin.root,
                                "cwd",
                            )?)
                        }
                        Some(cwd) if cwd.starts_with("${PLUGIN_DATA}/") => {
                            let directory = contained_plugin_path(
                                plugin.data_dir.join(&cwd["${PLUGIN_DATA}/".len()..]),
                                &plugin.data_dir,
                                "data cwd",
                            )?;
                            std::fs::create_dir_all(&directory).map_err(|error| {
                                format!(
                                    "could not create cwd for plugin {:?} MCP server {:?} at {}: {error}",
                                    plugin.alias, server.name, directory.display()
                                )
                            })?;
                            Some(directory.canonicalize().map_err(|error| {
                                format!(
                                    "could not resolve cwd for plugin {:?} MCP server {:?}: {error}",
                                    plugin.alias, server.name
                                )
                            })?)
                        }
                        Some(cwd) if cwd.starts_with("./") => Some(contained_plugin_path(
                            plugin.root.join(&cwd[2..]),
                            &plugin.root,
                            "cwd",
                        )?),
                        Some(cwd) => {
                            return Err(format!(
                                "plugin {:?} MCP server {:?} has unsupported cwd {cwd:?}",
                                plugin.alias, server.name
                            ));
                        }
                        None => None,
                    };
                    (McpTransportBinding::Stdio(transport), None, false)
                }
                PluginMcpTransport::StreamableHttp { url, headers } => {
                    let mut transport = StreamableHttpTransportConfig::new(url.clone());
                    for (name, value) in headers {
                        transport = transport
                            .with_header(name.as_str(), value.as_str())
                            .map_err(|error| {
                                format!("invalid plugin MCP config for {}: {error}", server.name)
                            })?;
                    }
                    (
                        McpTransportBinding::StreamableHttp(transport),
                        Some(url.clone()),
                        headers
                            .keys()
                            .any(|name| name.eq_ignore_ascii_case("authorization")),
                    )
                }
                PluginMcpTransport::Sse { .. } => unreachable!("SSE was skipped above"),
            };
            entries.insert(server.name.clone(), fingerprint.clone());
            prepared.insert(
                server.name.clone(),
                PreparedServer {
                    config: McpServerConfig::new(server.name.clone(), binding),
                    record: ServerRecord {
                        description,
                        url,
                        oauth: None,
                        static_authorization,
                        fingerprint,
                        status: ServerStatus::Uninitialized,
                    },
                },
            );
        }
    }
    Ok((prepared, entries))
}

pub async fn connect(
    path: Option<&Path>,
    plugins: &[crate::plugins::ResolvedPluginMcp],
    interactive_oauth_enabled: bool,
    credential_storage: CredentialStorage,
) -> Result<McpRuntime, String> {
    let (plugin_prepared, plugin_entries) = prepare_plugins(plugins)?;
    let (bytes, explicit_prepared, explicit_entries) = match path {
        Some(path) => {
            let bytes = tokio::fs::read(path).await.map_err(|error| {
                format!("could not read MCP config {}: {error}", path.display())
            })?;
            let (prepared, entries) = prepare_config(&bytes, path)?;
            (bytes, prepared, entries)
        }
        None => (Vec::new(), BTreeMap::new(), BTreeMap::new()),
    };
    let mut prepared = plugin_prepared.clone();
    prepared.extend(explicit_prepared);
    let mut entries = plugin_entries.clone();
    entries.extend(explicit_entries);
    validate_server_names(prepared.keys())?;
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
    let oauth_sessions = Arc::new(Mutex::new(BTreeMap::new()));
    let active_replays = Arc::new(Mutex::new(BTreeMap::new()));
    manager.set_handler_config(McpHandlerConfig::new().with_auth_responder(Arc::new(
        AuthRecorder {
            challenges: Arc::clone(&challenges),
            servers: Arc::clone(&servers),
            oauth_sessions: Arc::clone(&oauth_sessions),
            active_replays: Arc::clone(&active_replays),
            credential_storage: credential_storage.clone(),
        },
    )));
    Ok(McpRuntime::new(
        manager,
        servers,
        challenges,
        (oauth_sessions, active_replays),
        credential_storage,
        ReloadState {
            path: path.map(Path::to_path_buf),
            raw: bytes,
            entries,
            plugins: plugin_prepared,
            plugin_entries,
        },
        interactive_oauth_enabled,
    ))
}

impl McpRuntime {
    fn new(
        manager: McpServerManager,
        servers: Arc<RwLock<BTreeMap<String, ServerRecord>>>,
        challenges: Arc<Mutex<BTreeMap<String, AuthRequest>>>,
        oauth: (OAuthSessions, ActiveReplays),
        credential_storage: CredentialStorage,
        reload: ReloadState,
        interactive_oauth_enabled: bool,
    ) -> Self {
        let catalog = manager.source();
        let (oauth_sessions, active_replays) = oauth;
        Self {
            inner: Arc::new(Inner {
                manager: Mutex::new(manager),
                catalog,
                servers,
                challenges,
                pending: Mutex::new(BTreeMap::new()),
                oauth_sessions,
                active_replays,
                credential_storage,
                reload: Mutex::new(reload),
                reload_flight: Arc::new(Semaphore::new(1)),
                initialization: Mutex::new(()),
                auth_setup: Mutex::new(()),
                operations: Mutex::new(BTreeMap::new()),
                event_routes: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
                next_event_route: AtomicU64::new(0),
                interactive_oauth_enabled,
            }),
        }
    }

    pub fn catalog(&self) -> CatalogReader {
        self.inner.catalog.clone()
    }

    async fn operation_gate(&self, server: &str) -> Arc<Mutex<()>> {
        self.inner
            .operations
            .lock()
            .await
            .entry(server.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn server_for_tool(&self, tool: &str) -> Option<(String, Vec<u8>, bool)> {
        self.inner
            .servers
            .read()
            .await
            .iter()
            .filter(|(server, _)| tool.starts_with(&format!("mcp_{server}_")))
            .max_by_key(|(server, _)| server.len())
            .map(|(server, record)| {
                (
                    server.clone(),
                    record.fingerprint.clone(),
                    serializes_tool_calls(record),
                )
            })
    }

    async fn finish_tool_call(
        &self,
        server: &str,
        fingerprint: &[u8],
        generation: u64,
        authenticated: bool,
        force_interactive: bool,
    ) {
        let current_replay = {
            let mut active_replays = self.inner.active_replays.lock().await;
            if active_replays.get(server) == Some(&generation) {
                active_replays.remove(server);
                true
            } else {
                false
            }
        };
        if !current_replay {
            return;
        }
        let challenged = self.inner.challenges.lock().await.contains_key(server);
        if !challenged {
            return;
        }
        let mut servers = self.inner.servers.write().await;
        let Some(record) = servers
            .get_mut(server)
            .filter(|record| record.fingerprint == fingerprint)
        else {
            return;
        };
        record.status = if authenticated {
            ServerStatus::Connected
        } else {
            ServerStatus::AuthenticationRequired
        };
        drop(servers);
        let mut challenges = self.inner.challenges.lock().await;
        if authenticated {
            challenges.remove(server);
        } else if force_interactive && let Some(challenge) = challenges.get_mut(server) {
            challenge
                .challenge
                .insert("insufficient_scope".into(), Value::Bool(true));
        }
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
        let Some(path) = state.path.clone() else {
            return Ok(());
        };
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| format!("could not read MCP config {}: {error}", path.display()))?;
        if bytes == state.raw {
            return Ok(());
        }

        // Validate explicit transports before changing live state, then layer
        // them over the immutable plugin baseline.
        let (explicit_prepared, explicit_entries) = prepare_config(&bytes, &path)?;
        let mut prepared = state.plugins.clone();
        prepared.extend(explicit_prepared);
        let mut entries = state.plugin_entries.clone();
        entries.extend(explicit_entries);
        validate_server_names(prepared.keys())?;
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
        drop(state);
        let gates = {
            let mut operations = self.inner.operations.lock().await;
            changed
                .iter()
                .map(|name| {
                    operations
                        .entry(name.clone())
                        .or_insert_with(|| Arc::new(Mutex::new(())))
                        .clone()
                })
                .collect::<Vec<_>>()
        };
        let mut operation_guards = Vec::with_capacity(gates.len());
        for gate in &gates {
            operation_guards.push(
                tokio::time::timeout(CONNECT_TIMEOUT, gate.lock())
                    .await
                    .map_err(|_| "timed out waiting for an in-flight MCP operation".to_string())?,
            );
        }
        let mut state = self.inner.reload.lock().await;
        let _initialization = self.inner.initialization.lock().await;
        let current_bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| format!("could not reread MCP config {}: {error}", path.display()))?;
        if current_bytes != bytes {
            return Err("MCP config changed while reload was waiting; retry the operation".into());
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
            let mut oauth_sessions = self.inner.oauth_sessions.lock().await;
            let mut active_replays = self.inner.active_replays.lock().await;
            for name in &changed {
                challenges.remove(name);
                if let Some(pending) = pending.remove(name) {
                    pending.abort.abort();
                }
                oauth_sessions.remove(name);
                active_replays.remove(name);
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

        if let Some(resource_url) = record.url.as_deref()
            && !record.static_authorization
            && self.inner.credential_storage.is_persistent()
        {
            let oauth = record.oauth.clone().unwrap_or_default();
            let restored =
                match auth::restore(resource_url, &oauth, &self.inner.credential_storage).await {
                    Ok(restored) => restored,
                    Err(error) => {
                        self.set_status(name, ServerStatus::Error(error)).await;
                        return;
                    }
                };
            if let Some((token, oauth_manager)) = restored {
                let request = connect_auth_request(name);
                let mut credentials = MetadataMap::new();
                credentials.insert("access_token".into(), Value::String(token.clone()));
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
                self.inner.oauth_sessions.lock().await.insert(
                    name.to_string(),
                    Arc::new(OAuthSession::new(oauth_manager, token)),
                );
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
            Err(McpError::AuthRequired(_)) if record.static_authorization => {
                self.set_status(
                    name,
                    ServerStatus::Error(format!(
                        "MCP server {name} rejected its configured static authorization"
                    )),
                )
                .await;
            }
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
                let status = record.status.as_str();
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
        let operation = self.operation_gate(name).await;
        let _operation = tokio::time::timeout(CONNECT_TIMEOUT, operation.lock())
            .await
            .map_err(|_| {
                ToolError::Unavailable(format!(
                    "timed out waiting for an in-flight MCP operation on {name}"
                ))
            })?;
        if !self.inner.interactive_oauth_enabled {
            return Err(ToolError::Unavailable(
                "interactive MCP authentication requires the tui, serve, or acp command".into(),
            ));
        }
        let _reload = self.inner.reload.lock().await;
        self.initialize_server(name).await;
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
        let resource_url = record
            .url
            .clone()
            .ok_or_else(|| ToolError::InvalidInput(format!("MCP server {name} is not remote")))?;
        if record.static_authorization {
            return Err(ToolError::InvalidInput(format!(
                "MCP server {name} uses static authorization; update or remove that credential before starting OAuth"
            )));
        }
        let oauth = record.oauth.clone().unwrap_or_default();

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
        let www_authenticate = request
            .challenge
            .get("www_authenticate")
            .and_then(Value::as_str)
            .map(str::to_string);
        let oauth_session = if challenge_requires_interactive_authorization(&request.challenge) {
            None
        } else {
            self.inner.oauth_sessions.lock().await.get(name).cloned()
        };
        if let Some(session) = oauth_session
            && let Ok(token) = session.refresh(&self.inner.credential_storage).await
            && self
                .apply_credentials(name, request.clone(), token)
                .await
                .is_ok()
        {
            self.inner.challenges.lock().await.remove(name);
            self.set_status(name, ServerStatus::Connected).await;
            return Ok(json!({"server":name,"status":"authenticated"}));
        }
        let pending = auth::begin(
            &resource_url,
            &oauth,
            &self.inner.credential_storage,
            required_scope.as_deref(),
            www_authenticate.as_deref(),
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
        let operation = self.operation_gate(&server).await;
        let _operation = operation.lock().await;
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
            self.apply_credentials(&server, request, token.clone())
                .await?;
            Ok::<_, String>((oauth_manager, token))
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
            Ok((manager, token)) => {
                self.inner
                    .oauth_sessions
                    .lock()
                    .await
                    .insert(server.clone(), Arc::new(OAuthSession::new(manager, token)));
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

struct ReplayCleanup {
    session: Option<Arc<OAuthSession>>,
    target: Option<(McpRuntime, String, Vec<u8>)>,
}

impl ReplayCleanup {
    fn new(
        session: Option<Arc<OAuthSession>>,
        runtime: McpRuntime,
        target: Option<(String, Vec<u8>)>,
    ) -> Self {
        Self {
            session,
            target: target.map(|(server, fingerprint)| (runtime, server, fingerprint)),
        }
    }

    fn finish(mut self, commit: bool) -> Option<u64> {
        self.target = None;
        self.session
            .take()
            .and_then(|session| session.finish_replay(commit))
    }
}

impl Drop for ReplayCleanup {
    fn drop(&mut self) {
        let generation = self
            .session
            .take()
            .and_then(|session| session.finish_replay(false));
        if let Some(generation) = generation
            && let Some((runtime, server, fingerprint)) = self.target.take()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move {
                runtime
                    .finish_tool_call(&server, &fingerprint, generation, true, false)
                    .await;
            });
        }
    }
}

#[derive(Clone)]
pub struct McpTool {
    runtime: McpRuntime,
    catalog: CatalogReader,
    spec: ToolSpec,
}

struct ExecutedMcpCall {
    outcome: ToolExecutionOutcome,
    replay: ReplayCleanup,
    server: Option<(String, Vec<u8>)>,
    _operation: Option<OwnedMutexGuard<()>>,
}

impl McpTool {
    pub fn new(runtime: McpRuntime) -> Self {
        Self {
            catalog: runtime.catalog(),
            runtime,
            spec: ToolSpec::new(
                ToolName::new("tool"),
                "Invoke an authenticated MCP tool returned by tool_search.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "args": {"type": "object"},
                        "timeout_seconds": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_TOOL_TIMEOUT_SECONDS,
                            "description": "Overrides the default 60-second deadline for this call. Omit unless the tool is expected to return after the default timeout."
                        }
                    },
                    "required": ["name", "args"],
                    "additionalProperties": false
                }),
            ),
        }
    }

    fn timeout(object: &serde_json::Map<String, Value>) -> Result<Duration, ToolError> {
        let Some(value) = object.get("timeout_seconds") else {
            return Ok(DEFAULT_TOOL_TIMEOUT);
        };
        let Some(seconds) = value
            .as_u64()
            .filter(|seconds| (1..=MAX_TOOL_TIMEOUT_SECONDS).contains(seconds))
        else {
            return Err(ToolError::InvalidInput(format!(
                "timeout_seconds must be an integer from 1 to {MAX_TOOL_TIMEOUT_SECONDS}"
            )));
        };
        Ok(Duration::from_secs(seconds))
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
        if object
            .keys()
            .any(|key| key != "name" && key != "args" && key != "timeout_seconds")
        {
            return ToolExecutionOutcome::Failed(ToolError::InvalidInput(
                "unknown field in tool arguments".into(),
            ));
        }
        let Some(name) = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
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
        let timeout = match Self::timeout(object) {
            Ok(timeout) => timeout,
            Err(error) => return ToolExecutionOutcome::Failed(error),
        };
        let tool_name = ToolName::new(name.clone());
        if self.catalog.get(&tool_name).is_none() {
            return ToolExecutionOutcome::Failed(ToolError::InvalidInput(format!(
                "unknown MCP tool: {}",
                tool_name.0
            )));
        }
        let Some(scope) = context.execution_scope.clone() else {
            return ToolExecutionOutcome::Failed(ToolError::Unavailable(
                "tool requires an execution scope".into(),
            ));
        };
        let timeout_name = name.clone();
        let call = tokio::time::timeout(
            timeout,
            self.dispatch_call(request, scope, name, tool_name, args),
        );
        let result = if let Some(cancellation) = context.cancellation.clone() {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    return ToolExecutionOutcome::Failed(ToolError::Cancelled);
                }
                result = call => result,
            }
        } else {
            call.await
        };
        match result {
            Ok(call) => self.finish_dispatch(call).await,
            Err(_) => ToolExecutionOutcome::Failed(ToolError::ExecutionFailed(format!(
                "MCP tool {timeout_name} timed out after {} seconds; inspect remote state before retrying side effects",
                timeout.as_secs()
            ))),
        }
    }

    async fn dispatch_call(
        &self,
        request: ToolRequest,
        scope: ToolExecutionScope,
        name: String,
        tool_name: ToolName,
        args: Value,
    ) -> ExecutedMcpCall {
        let initial_server = self.runtime.server_for_tool(&name).await;
        let server_name = initial_server.as_ref().map(|(server, _, _)| server.clone());
        let operation = match initial_server.as_ref() {
            Some((server, _, true)) => Some(self.runtime.operation_gate(server).await),
            _ => None,
        };
        let operation = match operation {
            Some(operation) => Some(operation.lock_owned().await),
            None => None,
        };
        let server = self
            .runtime
            .server_for_tool(&name)
            .await
            .filter(|(server, _, _)| {
                server_name
                    .as_ref()
                    .is_some_and(|expected| server == expected)
            })
            .map(|(server, fingerprint, _)| (server, fingerprint));
        let replay = ReplayCleanup::new(
            match server.as_ref() {
                Some((server, _)) => self
                    .runtime
                    .inner
                    .oauth_sessions
                    .lock()
                    .await
                    .get(server)
                    .cloned(),
                None => None,
            },
            self.runtime.clone(),
            server.clone(),
        );
        let outcome = scope
            .execute_child(
                ToolRequest::new(
                    request.call_id,
                    tool_name,
                    args,
                    request.session_id,
                    request.turn_id,
                )
                .with_metadata(request.metadata),
            )
            .await;
        ExecutedMcpCall {
            outcome,
            replay,
            server,
            _operation: operation,
        }
    }

    async fn finish_dispatch(&self, call: ExecutedMcpCall) -> ToolExecutionOutcome {
        let ExecutedMcpCall {
            outcome,
            replay,
            server,
            _operation,
        } = call;
        let succeeded = matches!(outcome, ToolExecutionOutcome::Completed(_));
        let error = match &outcome {
            ToolExecutionOutcome::FailedBeforeInvocation(error)
            | ToolExecutionOutcome::Failed(error) => Some(error.to_string()),
            _ => None,
        };
        let replay_rejected = error.as_deref().is_some_and(agentkit_replay_rejected);
        let auth_not_applied = error.as_deref().is_some_and(agentkit_auth_not_applied);
        let reached_server = succeeded || error.is_some() && !auth_not_applied;
        let interrupted = matches!(outcome, ToolExecutionOutcome::Interrupted(_));
        let replay_generation = replay.finish(reached_server);
        if let Some(generation) = replay_generation
            && let Some((server, fingerprint)) = server
        {
            self.runtime
                .finish_tool_call(
                    &server,
                    &fingerprint,
                    generation,
                    interrupted || reached_server && !replay_rejected,
                    replay_rejected,
                )
                .await;
        }
        if replay_rejected {
            ToolExecutionOutcome::Failed(ToolError::ExecutionFailed(
                "MCP credentials were rejected after refresh; call auth with that server name"
                    .into(),
            ))
        } else {
            outcome
        }
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
    let oauth_sessions = Arc::new(Mutex::new(BTreeMap::new()));
    let active_replays = Arc::new(Mutex::new(BTreeMap::new()));
    let credential_storage = CredentialStorage::Memory;
    let manager = McpServerManager::new().with_handler_config(
        McpHandlerConfig::new().with_auth_responder(Arc::new(AuthRecorder {
            challenges: Arc::clone(&challenges),
            servers: Arc::clone(&servers),
            oauth_sessions: Arc::clone(&oauth_sessions),
            active_replays: Arc::clone(&active_replays),
            credential_storage: credential_storage.clone(),
        })),
    );
    McpRuntime::new(
        manager,
        servers,
        challenges,
        (oauth_sessions, active_replays),
        credential_storage,
        ReloadState {
            path: None,
            raw: Vec::new(),
            entries: BTreeMap::new(),
            plugins: BTreeMap::new(),
            plugin_entries: BTreeMap::new(),
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
    use std::{collections::BTreeMap, sync::Arc, time::Duration};

    use agentkit_core::MetadataMap;
    use agentkit_mcp::{
        AuthOperation, AuthRequest, AuthResolution, McpAuthResponder, McpTransportBinding,
    };
    use agentkit_plugins::{PluginMcpServer, PluginMcpTransport};
    use agentkit_tools_core::{ToolName, ToolSpec};
    use rmcp::transport::auth::{
        AuthorizationManager, AuthorizationMetadata, CredentialStore, InMemoryCredentialStore,
        OAuthTokenResponse, StoredCredentials,
    };
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::{Mutex, RwLock, oneshot},
    };

    use super::{
        AuthRecorder, Config, CredentialStorage, McpTool, OAuthSession, OpportunisticRefresh,
        ReplayCleanup, ServerRecord, ServerStatus, agentkit_auth_not_applied,
        agentkit_replay_rejected, can_opportunistically_refresh,
        challenge_requires_interactive_authorization, prepare_plugins, search_specs,
        serializes_tool_calls, validate_server_names,
    };
    use crate::plugins::ResolvedPluginMcp;

    fn spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec::new(ToolName::new(name), description, json!({"type": "object"}))
    }

    fn names(results: &[serde_json::Value]) -> Vec<&str> {
        results
            .iter()
            .map(|result| result["name"].as_str().unwrap())
            .collect()
    }

    fn connected_oauth_record() -> ServerRecord {
        ServerRecord {
            description: "Remote".into(),
            url: Some("https://example.com/mcp".into()),
            oauth: None,
            static_authorization: false,
            fingerprint: vec![1],
            status: ServerStatus::Connected,
        }
    }

    fn tool_auth_request() -> AuthRequest {
        AuthRequest {
            id: "mcp:remote:tools/call".into(),
            provider: "mcp.remote".into(),
            operation: AuthOperation::McpToolCall {
                server_id: "remote".into(),
                tool_name: "write".into(),
                input: json!({"value": 1}),
                metadata: MetadataMap::new(),
            },
            challenge: MetadataMap::from_iter([(
                "flow_kind".into(),
                Value::String("http_bearer".into()),
            )]),
        }
    }

    async fn refreshable_oauth_session() -> (
        Arc<OAuthSession>,
        tokio::task::JoinHandle<()>,
        oneshot::Receiver<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (request_seen_tx, request_seen_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let _ = request_seen_tx.send(());
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            let body = r#"{"access_token":"new-access","token_type":"Bearer","refresh_token":"new-refresh","expires_in":3600}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let store = InMemoryCredentialStore::new();
        let token: OAuthTokenResponse = serde_json::from_value(json!({
            "access_token": "old-access",
            "token_type": "Bearer",
            "refresh_token": "old-refresh",
            "expires_in": 3600
        }))
        .unwrap();
        store
            .save(StoredCredentials::new(
                "client".into(),
                Some(token),
                Vec::new(),
                None,
            ))
            .await
            .unwrap();
        let mut manager = AuthorizationManager::new(&format!("http://{address}/mcp"))
            .await
            .unwrap();
        let metadata: AuthorizationMetadata = serde_json::from_value(json!({
            "authorization_endpoint": format!("http://{address}/authorize"),
            "token_endpoint": format!("http://{address}/token")
        }))
        .unwrap();
        manager.set_metadata(metadata);
        manager.set_credential_store(store);
        (
            Arc::new(OAuthSession::new(manager, "old-access".into())),
            server,
            request_seen_rx,
        )
    }

    #[test]
    fn mcp_tool_timeout_schema_is_optional_and_has_no_schema_default() {
        let tool = McpTool::new(super::empty());
        let timeout = &tool.spec.input_schema["properties"]["timeout_seconds"];

        assert_eq!(timeout["minimum"], 1);
        assert_eq!(timeout["maximum"], 3_600);
        assert!(timeout.get("default").is_none());
        assert_eq!(
            timeout["description"],
            "Overrides the default 60-second deadline for this call. Omit unless the tool is expected to return after the default timeout."
        );
        assert_eq!(tool.spec.input_schema["required"], json!(["name", "args"]));
    }

    #[test]
    fn mcp_tool_timeout_defaults_to_sixty_seconds_and_validates_overrides() {
        let input = json!({"name": "remote", "args": {}});
        assert_eq!(
            McpTool::timeout(input.as_object().unwrap()).unwrap(),
            Duration::from_secs(60)
        );

        let input = json!({"name": "remote", "args": {}, "timeout_seconds": 300});
        assert_eq!(
            McpTool::timeout(input.as_object().unwrap()).unwrap(),
            Duration::from_secs(300)
        );

        for timeout in [json!(0), json!(3_601), json!(1.5), json!("60")] {
            let input = json!({"timeout_seconds": timeout});
            assert!(McpTool::timeout(input.as_object().unwrap()).is_err());
        }
    }

    #[test]
    fn insufficient_scope_without_required_scope_requires_interactive_authorization() {
        let mut challenge = MetadataMap::new();
        assert!(!challenge_requires_interactive_authorization(&challenge));

        challenge.insert("insufficient_scope".into(), Value::Bool(true));
        assert!(challenge_requires_interactive_authorization(&challenge));
    }

    #[test]
    fn opportunistic_refresh_requires_a_plain_connected_oauth_tool_challenge() {
        let record = connected_oauth_record();
        let mut request = tool_auth_request();
        assert!(can_opportunistically_refresh(&request, &record));

        request
            .challenge
            .insert("insufficient_scope".into(), Value::Bool(true));
        assert!(!can_opportunistically_refresh(&request, &record));

        let mut static_record = record;
        static_record.static_authorization = true;
        assert!(!can_opportunistically_refresh(
            &tool_auth_request(),
            &static_record
        ));
    }

    #[test]
    fn only_remote_non_static_servers_serialize_tool_calls() {
        let mut record = connected_oauth_record();
        assert!(serializes_tool_calls(&record));
        record.static_authorization = true;
        assert!(!serializes_tool_calls(&record));
        record.static_authorization = false;
        record.url = None;
        assert!(!serializes_tool_calls(&record));
    }

    #[test]
    fn pinned_agentkit_auth_errors_are_classified() {
        assert!(agentkit_replay_rejected(
            "MCP auth challenge unresolved after retry: request"
        ));
        assert!(agentkit_auth_not_applied("auth responder failed: nope"));
        assert!(agentkit_auth_not_applied(
            "applying auth resolution failed: nope"
        ));
        assert!(!agentkit_auth_not_applied("tool application error"));
    }

    #[tokio::test]
    async fn concurrent_tool_challenges_trigger_one_refresh_and_one_replay() {
        let challenges = Arc::new(Mutex::new(BTreeMap::new()));
        let servers = Arc::new(RwLock::new(BTreeMap::from([(
            "remote".into(),
            connected_oauth_record(),
        )])));
        let (session, refresh_server, _request_seen) = refreshable_oauth_session().await;
        let oauth_sessions = Arc::new(Mutex::new(BTreeMap::from([(
            "remote".into(),
            Arc::clone(&session),
        )])));
        let recorder = AuthRecorder {
            challenges: Arc::clone(&challenges),
            servers: Arc::clone(&servers),
            oauth_sessions,
            active_replays: Arc::new(Mutex::new(BTreeMap::new())),
            credential_storage: CredentialStorage::Memory,
        };

        let (first, second) = tokio::join!(
            recorder.resolve(tool_auth_request()),
            recorder.resolve(tool_auth_request())
        );
        refresh_server.await.unwrap();
        let mut provided = 0;
        let mut coalesced = 0;
        for result in [first, second] {
            match result {
                Ok(AuthResolution::Provided { credentials, .. }) => {
                    assert_eq!(credentials["access_token"], "new-access");
                    provided += 1;
                }
                Err(error) if error.to_string().contains("refreshed by another request") => {
                    coalesced += 1;
                }
                other => panic!("unexpected opportunistic refresh result: {other:?}"),
            }
        }
        assert_eq!((provided, coalesced), (1, 1));
        let delayed = recorder.resolve(tool_auth_request()).await.unwrap_err();
        assert!(delayed.to_string().contains("refreshed by another request"));
        assert!(matches!(
            servers.read().await["remote"].status,
            ServerStatus::Connected
        ));
        assert!(challenges.lock().await.contains_key("remote"));
        assert!(session.finish_replay(true).is_some());
    }

    #[tokio::test]
    async fn cancelled_replay_reuses_the_refreshed_token() {
        let runtime = super::empty();
        runtime
            .inner
            .servers
            .write()
            .await
            .insert("remote".into(), connected_oauth_record());
        runtime
            .inner
            .challenges
            .lock()
            .await
            .insert("remote".into(), tool_auth_request());
        let (session, refresh_server, _request_seen) = refreshable_oauth_session().await;
        let refresh = session
            .opportunistic_refresh(&CredentialStorage::Memory)
            .await
            .unwrap();
        refresh_server.await.unwrap();
        let OpportunisticRefresh::Apply { token, generation } = refresh else {
            panic!("opportunistic refresh was unexpectedly coalesced");
        };
        assert_eq!(token, "new-access");
        runtime
            .inner
            .active_replays
            .lock()
            .await
            .insert("remote".into(), generation);

        drop(ReplayCleanup::new(
            Some(Arc::clone(&session)),
            runtime.clone(),
            Some(("remote".into(), vec![1])),
        ));
        tokio::task::yield_now().await;

        assert!(matches!(
            runtime.inner.servers.read().await["remote"].status,
            ServerStatus::Connected
        ));
        assert!(!runtime.inner.challenges.lock().await.contains_key("remote"));
        assert_eq!(
            session.refresh(&CredentialStorage::Memory).await.unwrap(),
            "new-access"
        );
    }

    #[tokio::test]
    async fn refresh_continues_after_the_calling_future_is_cancelled() {
        let (session, refresh_server, request_seen) = refreshable_oauth_session().await;
        let refreshing = {
            let session = Arc::clone(&session);
            tokio::spawn(async move {
                session
                    .opportunistic_refresh(&CredentialStorage::Memory)
                    .await
            })
        };
        request_seen.await.unwrap();
        refreshing.abort();
        match refreshing.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("refresh caller was not cancelled"),
        }
        assert!(session.finish_replay(false).is_some());
        refresh_server.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if session
                    .tokens
                    .lock()
                    .expect("OAuth session tokens are poisoned")
                    .pending
                    .is_none()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            session.refresh(&CredentialStorage::Memory).await.unwrap(),
            "new-access"
        );
    }

    #[tokio::test]
    async fn cancelled_replay_cleanup_does_not_clear_a_newer_challenge() {
        let runtime = super::empty();
        let mut record = connected_oauth_record();
        record.status = ServerStatus::AuthenticationRequired;
        runtime
            .inner
            .servers
            .write()
            .await
            .insert("remote".into(), record);
        let mut challenge = tool_auth_request();
        challenge
            .challenge
            .insert("insufficient_scope".into(), Value::Bool(true));
        runtime
            .inner
            .challenges
            .lock()
            .await
            .insert("remote".into(), challenge);
        let (session, refresh_server, _request_seen) = refreshable_oauth_session().await;
        let refresh = session
            .opportunistic_refresh(&CredentialStorage::Memory)
            .await
            .unwrap();
        refresh_server.await.unwrap();
        let OpportunisticRefresh::Apply { generation, .. } = refresh else {
            panic!("opportunistic refresh was unexpectedly coalesced");
        };
        runtime
            .inner
            .active_replays
            .lock()
            .await
            .insert("remote".into(), generation.wrapping_add(1));

        drop(ReplayCleanup::new(
            Some(session),
            runtime.clone(),
            Some(("remote".into(), vec![1])),
        ));
        tokio::task::yield_now().await;

        assert!(matches!(
            runtime.inner.servers.read().await["remote"].status,
            ServerStatus::AuthenticationRequired
        ));
        assert_eq!(
            runtime.inner.challenges.lock().await["remote"]
                .challenge
                .get("insufficient_scope"),
            Some(&Value::Bool(true))
        );
    }

    #[tokio::test]
    async fn operation_gates_only_serialize_the_same_server() {
        let runtime = super::empty();
        let first = runtime.operation_gate("first").await;
        let same = runtime.operation_gate("first").await;
        let other = runtime.operation_gate("other").await;
        let _held = first.lock().await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), other.lock())
                .await
                .is_ok()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), same.lock())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejected_replay_preserves_interactive_auth_recovery() {
        let runtime = super::empty();
        runtime
            .inner
            .servers
            .write()
            .await
            .insert("remote".into(), connected_oauth_record());
        runtime
            .inner
            .challenges
            .lock()
            .await
            .insert("remote".into(), tool_auth_request());
        runtime
            .inner
            .active_replays
            .lock()
            .await
            .insert("remote".into(), 1);

        runtime
            .finish_tool_call("remote", &[1], 1, false, true)
            .await;

        assert!(matches!(
            runtime.inner.servers.read().await["remote"].status,
            ServerStatus::AuthenticationRequired
        ));
        assert_eq!(
            runtime.inner.challenges.lock().await["remote"]
                .challenge
                .get("insufficient_scope"),
            Some(&Value::Bool(true))
        );
    }

    #[tokio::test]
    async fn reload_does_not_hold_runtime_locks_while_waiting_for_a_server() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(&path, r#"{"mcpServers":{}}"#).unwrap();
        let runtime = super::connect(Some(&path), &[], true, CredentialStorage::Memory)
            .await
            .unwrap();
        let gate = runtime.operation_gate("local").await;
        let held = gate.lock().await;
        std::fs::write(&path, r#"{"mcpServers":{"local":{"command":"unused"}}}"#).unwrap();
        let reloading = {
            let runtime = runtime.clone();
            tokio::spawn(async move { runtime.reload_config().await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;

        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                runtime.inner.reload.lock()
            )
            .await
            .is_ok()
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                runtime.inner.initialization.lock()
            )
            .await
            .is_ok()
        );
        std::fs::write(
            &path,
            r#"{"mcpServers":{"local":{"command":"new-command"}}}"#,
        )
        .unwrap();
        drop(held);
        let error = reloading.await.unwrap().unwrap_err();
        assert!(error.contains("changed while reload was waiting"));
        runtime.reload_config().await.unwrap();
    }

    #[test]
    fn overlapping_server_names_are_rejected() {
        let names = BTreeMap::from([("foo".to_string(), ()), ("foo_bar".to_string(), ())]);
        assert!(
            validate_server_names(names.keys())
                .unwrap_err()
                .contains("ambiguous tool names")
        );
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

    async fn bearer_challenge_server(
        requests: usize,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                stream
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
            }
        });
        (address, server)
    }

    fn plugin(
        alias: &str,
        root: &std::path::Path,
        servers: Vec<PluginMcpServer>,
    ) -> ResolvedPluginMcp {
        let root = root.canonicalize().unwrap();
        let data_dir = root.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        ResolvedPluginMcp {
            alias: alias.into(),
            manifest_name: format!("{alias}-manifest"),
            root,
            data_dir,
            servers,
        }
    }

    #[test]
    fn plugin_mcp_expands_stdio_paths_and_skips_sse() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let plugin = plugin(
            "tools",
            directory.path(),
            vec![
                PluginMcpServer {
                    name: "local".into(),
                    transport: PluginMcpTransport::Stdio {
                        command: "./bin/server".into(),
                        args: vec!["${PLUGIN_DATA}/db".into()],
                        env: std::collections::BTreeMap::from([(
                            "ROOT_COPY".into(),
                            "${PLUGIN_ROOT}".into(),
                        )]),
                        cwd: Some("${PLUGIN_DATA}/work".into()),
                    },
                },
                PluginMcpServer {
                    name: "legacy".into(),
                    transport: PluginMcpTransport::Sse {
                        url: "https://example.com/sse".into(),
                        headers: Default::default(),
                    },
                },
            ],
        );
        let (prepared, _) = prepare_plugins(&[plugin]).unwrap();
        assert!(!prepared.contains_key("legacy"));
        let McpTransportBinding::Stdio(transport) = &prepared["local"].config.transport else {
            panic!("expected stdio transport");
        };
        assert_eq!(transport.command, root.join("bin/server").to_str().unwrap());
        assert_eq!(transport.args, [root.join("data/db").to_str().unwrap()]);
        assert_eq!(
            transport.cwd.as_deref(),
            Some(root.join("data/work").as_path())
        );
        assert!(
            transport
                .env
                .contains(&("PLUGIN_ROOT".into(), root.to_str().unwrap().into()))
        );
        assert!(
            transport
                .env
                .contains(&("ROOT_COPY".into(), root.to_str().unwrap().into()))
        );
    }

    #[test]
    fn plugin_mcp_preserves_transport_defaults_and_http_literals() {
        assert_eq!(
            super::expand_plugin_value(
                "${PLUGIN_ROOT}:${PLUGIN_DATA}",
                "/tmp/${PLUGIN_DATA}/root",
                "/tmp/${PLUGIN_ROOT}/data",
            ),
            "/tmp/${PLUGIN_DATA}/root:/tmp/${PLUGIN_ROOT}/data"
        );

        let directory = tempfile::tempdir().unwrap();
        let plugin = plugin(
            "tools",
            directory.path(),
            vec![
                PluginMcpServer {
                    name: "local".into(),
                    transport: PluginMcpTransport::Stdio {
                        command: "server".into(),
                        args: Vec::new(),
                        env: Default::default(),
                        cwd: None,
                    },
                },
                PluginMcpServer {
                    name: "remote".into(),
                    transport: PluginMcpTransport::StreamableHttp {
                        url: "https://example.com/mcp".into(),
                        headers: std::collections::BTreeMap::from([(
                            "x-path".into(),
                            "${PLUGIN_ROOT}".into(),
                        )]),
                    },
                },
            ],
        );
        let (prepared, _) = prepare_plugins(&[plugin]).unwrap();
        let McpTransportBinding::Stdio(stdio) = &prepared["local"].config.transport else {
            panic!("expected stdio transport");
        };
        assert_eq!(stdio.cwd, None);
        let McpTransportBinding::StreamableHttp(http) = &prepared["remote"].config.transport else {
            panic!("expected HTTP transport");
        };
        assert_eq!(http.url, "https://example.com/mcp");
        assert_eq!(http.headers[0].1.to_str().unwrap(), "${PLUGIN_ROOT}");
    }

    #[test]
    fn duplicate_plugin_mcp_names_are_rejected() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let server = || PluginMcpServer {
            name: "shared".into(),
            transport: PluginMcpTransport::StreamableHttp {
                url: "https://example.com/mcp".into(),
                headers: Default::default(),
            },
        };
        let error = match prepare_plugins(&[
            plugin("first", first.path(), vec![server()]),
            plugin("second", second.path(), vec![server()]),
        ]) {
            Ok(_) => panic!("duplicate plugin MCP names were accepted"),
            Err(error) => error,
        };
        assert!(
            error.contains("both plugins \"first\" and \"second\""),
            "{error}"
        );
    }

    #[tokio::test]
    async fn explicit_reload_removal_restores_plugin_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"shared":{"command":"explicit","description":"Explicit"}}}"#,
        )
        .unwrap();
        let plugin = plugin(
            "tools",
            directory.path(),
            vec![PluginMcpServer {
                name: "shared".into(),
                transport: PluginMcpTransport::StreamableHttp {
                    url: "https://example.com/mcp".into(),
                    headers: Default::default(),
                },
            }],
        );
        let runtime = super::connect(Some(&path), &[plugin], true, CredentialStorage::Memory)
            .await
            .unwrap();
        assert_eq!(
            runtime.inner.servers.read().await["shared"].description,
            "Explicit"
        );

        std::fs::write(&path, r#"{"mcpServers":{}}"#).unwrap();
        runtime.reload_config().await.unwrap();
        let record = runtime.inner.servers.read().await["shared"].clone();
        assert_eq!(record.description, "tools-manifest plugin MCP server");
        assert_eq!(record.url.as_deref(), Some("https://example.com/mcp"));
    }

    #[tokio::test]
    async fn plugin_only_mcp_configuration_is_registered() {
        let directory = tempfile::tempdir().unwrap();
        let plugin = plugin(
            "tools",
            directory.path(),
            vec![PluginMcpServer {
                name: "remote".into(),
                transport: PluginMcpTransport::StreamableHttp {
                    url: "https://example.com/mcp".into(),
                    headers: Default::default(),
                },
            }],
        );
        let runtime = super::connect(None, &[plugin], true, CredentialStorage::Memory)
            .await
            .unwrap();
        assert!(runtime.inner.servers.read().await.contains_key("remote"));
    }

    #[tokio::test]
    async fn explicit_oauth_overrides_do_not_skip_the_initial_connection() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"linear":{"url":"https://unused.invalid/mcp","description":"Issues and project management","auth":{"type":"oauth","scopes":[]}}}}"#,
        )
        .unwrap();
        let runtime = super::connect(Some(&path), &[], true, CredentialStorage::Memory)
            .await
            .unwrap();
        let results = runtime.search("issues").await.unwrap();
        assert_eq!(results["servers"][0]["name"], "linear");
        assert_eq!(results["servers"][0]["status"], "error");
        assert_eq!(
            runtime.search("calendar").await.unwrap(),
            json!({"servers":[]})
        );
    }

    #[tokio::test]
    async fn search_and_auth_reload_servers_added_after_connect() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(&path, r#"{"mcpServers":{}}"#).unwrap();
        let runtime = super::connect(Some(&path), &[], true, CredentialStorage::Memory)
            .await
            .unwrap();

        std::fs::write(
            &path,
            r#"{"mcpServers":{"linear":{"url":"https://unused.invalid/mcp","description":"Issue tracking","auth":{"type":"oauth"}},"local":{"command":"kit-command-that-does-not-exist","description":"Local helper"}}}"#,
        )
        .unwrap();

        let results = runtime.search("issue").await.unwrap();
        assert_eq!(results["servers"][0]["name"], "linear");
        assert_eq!(results["servers"][0]["status"], "error");
        let error = runtime
            .authorize("local", "session".into())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("MCP server local is not remote"),
            "added server should be found by auth lookup: {error}"
        );
    }

    #[tokio::test]
    async fn invalid_reload_preserves_the_last_valid_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        let valid = r#"{"mcpServers":{"linear":{"url":"https://unused.invalid/mcp","description":"Issue tracking","auth":{"type":"oauth"}}}}"#;
        std::fs::write(&path, valid).unwrap();
        let runtime = super::connect(Some(&path), &[], true, CredentialStorage::Memory)
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
        assert_eq!(results["servers"][0]["status"], "error");
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
        let (address, mut server) = bearer_challenge_server(1).await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(
            &path,
            format!(r#"{{"mcpServers":{{"remote":{{"url":"http://{address}/mcp"}}}}}}"#),
        )
        .unwrap();

        let runtime = super::connect(Some(&path), &[], false, CredentialStorage::Memory)
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
        assert_eq!(results["servers"][0]["status"], "authentication_required");
        let challenge = runtime.inner.challenges.lock().await["remote"].clone();
        assert_eq!(
            challenge
                .challenge
                .get("www_authenticate")
                .and_then(Value::as_str),
            Some("Bearer")
        );
    }

    #[tokio::test]
    async fn static_authorization_is_not_replaced_by_reactive_oauth() {
        let (address, server) = bearer_challenge_server(3).await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"mcpServers":{{"token":{{"url":"http://{address}/token","bearerToken":"static"}},"header":{{"url":"http://{address}/header","headers":{{"aUtHoRiZaTiOn":"Basic static"}}}}}}}}"#
            ),
        )
        .unwrap();
        let plugin = plugin(
            "tools",
            directory.path(),
            vec![PluginMcpServer {
                name: "plugin-header".into(),
                transport: PluginMcpTransport::StreamableHttp {
                    url: format!("http://{address}/plugin"),
                    headers: std::collections::BTreeMap::from([(
                        "Authorization".into(),
                        "Bearer static".into(),
                    )]),
                },
            }],
        );
        let runtime = super::connect(Some(&path), &[plugin], true, CredentialStorage::Memory)
            .await
            .unwrap();

        let results = runtime.search("mcp").await.unwrap();
        server.await.unwrap();
        for name in ["header", "plugin-header", "token"] {
            let result = results["servers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|result| result["name"] == name)
                .unwrap();
            assert_eq!(result["status"], "error");
            assert!(
                result["error"]
                    .as_str()
                    .unwrap()
                    .contains("rejected its configured static authorization")
            );
            let error = runtime.authorize(name, "session".into()).await.unwrap_err();
            assert!(error.to_string().contains("uses static authorization"));
        }
    }

    #[test]
    fn explicit_oauth_cannot_coexist_with_an_authorization_header() {
        let path = std::path::Path::new("mcp.json");
        let error = match super::prepare_config(
            br#"{"mcpServers":{"remote":{"url":"https://example.com/mcp","headers":{"AUTHORIZATION":"Bearer static"},"auth":{"type":"oauth"}}}}"#,
            path,
        ) {
            Ok(_) => panic!("OAuth and static authorization were accepted together"),
            Err(error) => error,
        };
        assert!(
            error.contains("cannot use both OAuth and static authorization"),
            "{error}"
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
        let runtime = super::connect(Some(&path), &[], true, CredentialStorage::Memory)
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
        assert_eq!(results["servers"][1]["status"], "error");
    }
}
