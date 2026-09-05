mod auth;
mod credentials;

pub use crate::credentials::CredentialStorage;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
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
    BasicToolExecutor, CatalogReader, Tool, ToolContext, ToolError, ToolExecutionOutcome,
    ToolExecutionScope, ToolExecutor, ToolName, ToolRequest, ToolResult, ToolSource, ToolSpec,
};
use async_trait::async_trait;
use rmcp::transport::auth::AuthorizationManager;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use tokio::sync::{
    Mutex, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, Semaphore, mpsc, oneshot,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const SEARCH_RESULT_LIMIT: usize = 5;
const SEARCH_MIN_SCORE: u32 = 60;
const SEARCH_RESULT_BYTE_CAP: usize = 32_768;
const SEARCH_SERVER_TEXT_BYTE_CAP: usize = 1_024;
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

fn deserialize_transport_type<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
enum StdioType {
    #[serde(rename = "stdio")]
    Stdio,
}

#[derive(Deserialize)]
enum HttpType {
    #[serde(rename = "streamable-http")]
    StreamableHttp,
    #[serde(rename = "http")]
    Http,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Stdio {
    #[serde(
        rename = "type",
        default,
        deserialize_with = "deserialize_transport_type"
    )]
    _transport_type: Option<StdioType>,
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
    #[serde(
        rename = "type",
        default,
        deserialize_with = "deserialize_transport_type"
    )]
    _transport_type: Option<HttpType>,
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
    plugin_source: Option<crate::plugins::PluginRuntime>,
    reload: Mutex<ReloadState>,
    reload_flight: Arc<Semaphore>,
    reload_epoch: AtomicU64,
    initialization: Mutex<()>,
    auth_setup: Mutex<()>,
    operations: Mutex<BTreeMap<String, Weak<RwLock<()>>>>,
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
    plugin_owned: bool,
    status: ServerStatus,
}

struct ReloadState {
    sources: Vec<SourceState>,
    entries: BTreeMap<String, Vec<u8>>,
    plugins: BTreeMap<String, PreparedServer>,
    plugin_entries: BTreeMap<String, Vec<u8>>,
    plugin_source: Option<crate::plugins::PluginRuntime>,
}

#[derive(Clone)]
pub(crate) struct ConfigSource {
    path: PathBuf,
    required: bool,
    default_stdio_cwd: Option<PathBuf>,
}

impl ConfigSource {
    pub(crate) fn required(path: PathBuf) -> Self {
        Self {
            path,
            required: true,
            default_stdio_cwd: None,
        }
    }

    pub(crate) fn optional_project(path: PathBuf, cwd: PathBuf) -> Self {
        Self {
            path,
            required: false,
            default_stdio_cwd: Some(cwd),
        }
    }
}

pub(crate) trait IntoConfigSources {
    fn into_config_sources(self) -> Vec<ConfigSource>;
}

impl IntoConfigSources for Vec<ConfigSource> {
    fn into_config_sources(self) -> Vec<ConfigSource> {
        self
    }
}

impl<P: AsRef<Path>> IntoConfigSources for Option<P> {
    fn into_config_sources(self) -> Vec<ConfigSource> {
        self.map(|path| ConfigSource::required(path.as_ref().to_path_buf()))
            .into_iter()
            .collect()
    }
}

#[derive(Clone)]
struct SourceState {
    source: ConfigSource,
    raw: Option<Vec<u8>>,
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

fn resolve_stdio_cwd(cwd: Option<&Path>, default: Option<&Path>) -> Option<PathBuf> {
    match cwd {
        Some(cwd) if cwd.is_relative() => default
            .map(|base| base.join(cwd))
            .or_else(|| Some(cwd.to_path_buf())),
        Some(cwd) => Some(cwd.to_path_buf()),
        None => default.map(Path::to_path_buf),
    }
}

fn prepare_config(
    bytes: &[u8],
    path: &Path,
    default_stdio_cwd: Option<&Path>,
) -> Result<PreparedConfiguration, String> {
    let config: Config = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid MCP config {}: {error}", path.display()))?;
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("invalid MCP config {}: {error}", path.display()))?;
    let raw_entries = value
        .get("mcpServers")
        .and_then(Value::as_object)
        .expect("strict Config parsing guarantees an mcpServers object");
    let mut entries = raw_entries
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
        let mut fingerprint = entries[&id].clone();
        let effective_cwd = match &server {
            Server::Stdio(server) => resolve_stdio_cwd(server.cwd.as_deref(), default_stdio_cwd),
            Server::Http(_) => None,
        };
        if let Some(cwd) = &effective_cwd {
            fingerprint.push(0);
            fingerprint.extend_from_slice(cwd.as_os_str().as_encoded_bytes());
        }
        entries.insert(id.clone(), fingerprint.clone());
        let (binding, record) = prepare_server(&id, server, fingerprint, effective_cwd.as_deref())?;
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
    effective_stdio_cwd: Option<&Path>,
) -> Result<(McpTransportBinding, ServerRecord), String> {
    match server {
        Server::Stdio(server) => {
            if server.command.trim().is_empty() {
                return Err(format!("MCP server {id} has an empty command"));
            }
            let mut transport = StdioTransportConfig::new(server.command);
            transport.args = server.args;
            transport.env = server.env.into_iter().collect();
            transport.cwd = effective_stdio_cwd.map(Path::to_path_buf);
            Ok((
                McpTransportBinding::Stdio(transport),
                ServerRecord {
                    description: server.description.unwrap_or_else(|| id.to_string()),
                    url: None,
                    oauth: None,
                    static_authorization: false,
                    fingerprint,
                    plugin_owned: false,
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
                    plugin_owned: false,
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
        match crate::resilient_fs::symlink_metadata(&ancestor) {
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
    let mut resolved = crate::resilient_fs::canonicalize(&ancestor).map_err(|error| {
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
                return Err(format!(
                    "plugin {}: MCP server {:?} uses unsupported SSE transport",
                    plugin.alias, server.name
                ));
            }
            if let Some(owner) = owners.insert(server.name.clone(), plugin.alias.clone()) {
                return Err(format!(
                    "MCP server {:?} is declared by both plugins {owner:?} and {:?}",
                    server.name, plugin.alias
                ));
            }
            let description = format!("{} plugin MCP server", plugin.manifest_name);
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
                            crate::resilient_fs::create_dir_all(&directory).map_err(|error| {
                                format!(
                                    "could not create cwd for plugin {:?} MCP server {:?} at {}: {error}",
                                    plugin.alias, server.name, directory.display()
                                )
                            })?;
                            Some(crate::resilient_fs::canonicalize(&directory).map_err(|error| {
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
                    crate::resilient_fs::global()
                        .require_disk(&plugin.root)
                        .map_err(|error| {
                            format!("plugin MCP files are not available on disk: {error}")
                        })?;
                    crate::resilient_fs::global()
                        .require_disk(&plugin.data_dir)
                        .map_err(|error| {
                            format!("plugin MCP data directory is not available on disk: {error}")
                        })?;
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
                PluginMcpTransport::Sse { .. } => unreachable!("SSE was rejected above"),
            };
            let fingerprint = format!(
                "plugin:v2:{:?}:{:?}:{:?}:{:?}:{binding:?}:{url:?}:{static_authorization}",
                plugin.alias, plugin.manifest_name, plugin.root, plugin.data_dir
            )
            .into_bytes();
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
                        plugin_owned: true,
                        status: ServerStatus::Uninitialized,
                    },
                },
            );
        }
    }
    Ok((prepared, entries))
}

async fn read_source(source: &ConfigSource) -> Result<Option<Vec<u8>>, String> {
    let path = source.path.clone();
    match tokio::task::spawn_blocking(move || crate::config_files::read(&path))
        .await
        .map_err(|error| format!("MCP config read task failed: {error}"))?
    {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if !source.required && error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "could not read MCP config {}: {error}",
            source.path.display()
        )),
    }
}

fn prepare_sources(
    sources: &[SourceState],
    plugin_prepared: &PreparedServers,
    plugin_entries: &ServerFingerprints,
) -> Result<PreparedConfiguration, String> {
    let mut prepared = plugin_prepared.clone();
    let mut entries = plugin_entries.clone();
    for state in sources {
        let Some(bytes) = &state.raw else {
            continue;
        };
        let (source_prepared, source_entries) = prepare_config(
            bytes,
            &state.source.path,
            state.source.default_stdio_cwd.as_deref(),
        )?;
        prepared.extend(source_prepared);
        entries.extend(source_entries);
    }
    Ok((prepared, entries))
}

pub(crate) async fn connect(
    sources: impl IntoConfigSources,
    plugins: &[crate::plugins::ResolvedPluginMcp],
    interactive_oauth_enabled: bool,
    credential_storage: CredentialStorage,
) -> Result<McpRuntime, String> {
    connect_inner(
        sources.into_config_sources(),
        plugins,
        None,
        interactive_oauth_enabled,
        credential_storage,
    )
    .await
}

pub(crate) async fn connect_dynamic(
    sources: impl IntoConfigSources,
    plugins: crate::plugins::PluginRuntime,
    interactive_oauth_enabled: bool,
    credential_storage: CredentialStorage,
) -> Result<McpRuntime, String> {
    let snapshot = plugins.snapshot();
    connect_inner(
        sources.into_config_sources(),
        &snapshot.mcp_plugins,
        Some(plugins),
        interactive_oauth_enabled,
        credential_storage,
    )
    .await
}

async fn connect_inner(
    sources: Vec<ConfigSource>,
    plugins: &[crate::plugins::ResolvedPluginMcp],
    plugin_source: Option<crate::plugins::PluginRuntime>,
    interactive_oauth_enabled: bool,
    credential_storage: CredentialStorage,
) -> Result<McpRuntime, String> {
    let plugins = plugins.to_vec();
    let (plugin_prepared, plugin_entries) =
        tokio::task::spawn_blocking(move || prepare_plugins(&plugins))
            .await
            .map_err(|error| error.to_string())??;
    let mut source_states = Vec::with_capacity(sources.len());
    for source in sources {
        let raw = read_source(&source).await?;
        source_states.push(SourceState { source, raw });
    }
    let (prepared, entries) = prepare_sources(&source_states, &plugin_prepared, &plugin_entries)?;
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
    let runtime = McpRuntime::new(
        manager,
        servers,
        challenges,
        (oauth_sessions, active_replays),
        credential_storage,
        ReloadState {
            sources: source_states,
            entries,
            plugins: plugin_prepared,
            plugin_entries,
            plugin_source,
        },
        interactive_oauth_enabled,
    );
    runtime.spawn_eager_initialization();
    Ok(runtime)
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
        let plugin_source = reload.plugin_source.clone();
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
                plugin_source,
                reload: Mutex::new(reload),
                reload_flight: Arc::new(Semaphore::new(1)),
                reload_epoch: AtomicU64::new(0),
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

    async fn operation_gate(&self, server: &str) -> Arc<RwLock<()>> {
        let mut operations = self.inner.operations.lock().await;
        operations.retain(|_, gate| gate.strong_count() > 0);
        if let Some(gate) = operations.get(server).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(RwLock::new(()));
        operations.insert(server.to_string(), Arc::downgrade(&gate));
        gate
    }

    async fn server_for_tool(&self, tool: &str) -> Option<(String, Vec<u8>, bool, bool)> {
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
                    record.plugin_owned,
                )
            })
    }

    async fn acquire_invocation(&self, tool: &str) -> Result<McpInvocationLease, ToolError> {
        let Some((server, fingerprint, serializes, plugin_owned)) =
            self.server_for_tool(tool).await
        else {
            return Err(ToolError::Unavailable(format!(
                "MCP tool is no longer available: {tool}"
            )));
        };
        let gate = self.operation_gate(&server).await;
        let operation = if serializes {
            McpOperationGuard::Exclusive(gate.write_owned().await)
        } else {
            McpOperationGuard::Shared(gate.read_owned().await)
        };
        let generation = match (&self.inner.plugin_source, plugin_owned) {
            (Some(source), true) => Some(source.generation_lease().await),
            _ => None,
        };
        let current = self.server_for_tool(tool).await;
        if !current.is_some_and(|(current_server, current_fingerprint, _, _)| {
            current_server == server && current_fingerprint == fingerprint
        }) {
            return Err(ToolError::Unavailable(
                "MCP configuration changed while the tool was waiting; retry the call".into(),
            ));
        }
        Ok(McpInvocationLease {
            server,
            fingerprint,
            _operation: operation,
            _generation: generation,
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

    #[cfg(test)]
    pub(crate) async fn config_source_states(&self) -> Vec<(PathBuf, bool, bool)> {
        self.inner
            .reload
            .lock()
            .await
            .sources
            .iter()
            .map(|state| {
                (
                    state.source.path.clone(),
                    state.source.required,
                    state.raw.is_some(),
                )
            })
            .collect()
    }

    pub(crate) async fn refresh(&self) -> Result<(), String> {
        self.reload_config().await
    }

    async fn reload_config(&self) -> Result<(), String> {
        let permit = Arc::clone(&self.inner.reload_flight)
            .acquire_owned()
            .await
            .map_err(|_| "MCP config reload coordinator closed".to_string())?;
        let current = self.inner.reload_epoch.load(Ordering::Acquire);
        let runtime = self.clone();
        tokio::spawn(async move {
            // The spawned task owns completion so caller cancellation cannot
            // strand the reload permit. Every waiter reruns after acquiring the
            // permit: a change can arrive after the preceding reload staged its
            // inputs but before that reload publishes.
            let _permit = permit;
            let result = runtime
                .reload_config_inner()
                .await
                .map_err(crate::plugins::bounded_diagnostic);
            runtime
                .inner
                .reload_epoch
                .store(current.wrapping_add(1), Ordering::Release);
            result
        })
        .await
        .map_err(|error| format!("MCP config reload task failed: {error}"))?
    }

    async fn reload_config_inner(&self) -> Result<(), String> {
        loop {
            // Snapshot the published generation, then perform all config reads,
            // downloads, and package resolution without holding runtime locks.
            let (
                current_sources,
                current_entries,
                current_plugins,
                current_plugin_entries,
                plugin_source,
            ) = {
                let state = self.inner.reload.lock().await;
                (
                    state.sources.clone(),
                    state.entries.clone(),
                    state.plugins.clone(),
                    state.plugin_entries.clone(),
                    state.plugin_source.clone(),
                )
            };
            let staged_plugins = match &plugin_source {
                Some(source) => Some(source.stage().await?),
                None => None,
            };
            let (plugin_prepared, plugin_entries) = match &staged_plugins {
                Some(staged) => {
                    let plugins = staged.resolved.mcp_plugins.clone();
                    tokio::task::spawn_blocking(move || prepare_plugins(&plugins))
                        .await
                        .map_err(|error| error.to_string())??
                }
                None => (current_plugins, current_plugin_entries),
            };
            let mut next_sources = Vec::with_capacity(current_sources.len());
            for current in &current_sources {
                next_sources.push(SourceState {
                    source: current.source.clone(),
                    raw: read_source(&current.source).await?,
                });
            }

            // Configured, project, and explicit files retain their declared
            // precedence over the newly staged plugin baseline.
            let (mut prepared, entries) =
                prepare_sources(&next_sources, &plugin_prepared, &plugin_entries)?;
            validate_server_names(prepared.keys())?;
            let changed = current_entries
                .keys()
                .chain(entries.keys())
                .filter(|name| current_entries.get(*name) != entries.get(*name))
                .cloned()
                .collect::<BTreeSet<_>>();
            let deleted = current_entries
                .keys()
                .filter(|name| !entries.contains_key(*name))
                .cloned()
                .collect::<Vec<_>>();

            let gates = if changed.is_empty() {
                Vec::new()
            } else {
                let mut gates = Vec::with_capacity(changed.len());
                for name in &changed {
                    gates.push(self.operation_gate(name).await);
                }
                gates
            };
            let mut operation_guards = Vec::with_capacity(gates.len());
            let mut blocked = None;
            for gate in &gates {
                match gate.clone().try_write_owned() {
                    Ok(guard) => operation_guards.push(guard),
                    Err(_) => {
                        blocked = Some(gate.clone());
                        break;
                    }
                }
            }
            if let Some(gate) = blocked {
                drop(operation_guards);
                let waited = tokio::time::timeout(CONNECT_TIMEOUT, gate.write_owned())
                    .await
                    .map_err(|_| "timed out waiting for an in-flight MCP operation".to_string())?;
                drop(waited);
                continue;
            }
            let mut initialization_guard = None;
            if !changed.is_empty() {
                initialization_guard = Some(self.inner.initialization.lock().await);
            }
            if !changed.is_empty() {
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

            // The generation writer only covers publication. Staging and MCP
            // manager initialization above may await without hiding the last
            // published skill and MCP generation from readers.
            let generation_writer = match &plugin_source {
                Some(source) => Some(source.generation_writer().await),
                None => None,
            };
            if !changed.is_empty() {
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
            }

            {
                let mut state = self.inner.reload.lock().await;
                state.sources = next_sources;
                state.entries = entries;
                state.plugins = plugin_prepared;
                state.plugin_entries = plugin_entries;
                if let (Some(source), Some(staged)) = (&plugin_source, staged_plugins) {
                    source.publish(staged);
                }
            }
            drop(generation_writer);
            drop(initialization_guard);
            if !deleted.is_empty() {
                let mut operations = self.inner.operations.lock().await;
                for name in deleted {
                    operations.remove(&name);
                }
            }
            drop(operation_guards);
            return Ok(());
        }
    }

    fn spawn_eager_initialization(&self) {
        let runtime = self.clone();
        tokio::spawn(async move {
            runtime.initialize_uninitialized().await;
        });
    }

    async fn initialize_uninitialized(&self) {
        let names = self
            .inner
            .servers
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        self.initialize_servers(&names).await;
    }

    /// Connects every named server that is still `Uninitialized`: stored
    /// OAuth credentials are restored in parallel, then all connections
    /// settle concurrently. Never starts interactive authorization.
    async fn initialize_servers(&self, names: &[String]) {
        let _initialization = self.inner.initialization.lock().await;
        let records = {
            let servers = self.inner.servers.read().await;
            names
                .iter()
                .filter_map(|name| {
                    servers
                        .get(name)
                        .map(|record| (name.clone(), record.clone()))
                })
                .filter(|(_, record)| matches!(record.status, ServerStatus::Uninitialized))
                .collect::<Vec<_>>()
        };
        if records.is_empty() {
            return;
        }
        let restores = futures_util::future::join_all(records.iter().map(|(name, record)| {
            let credential_storage = self.inner.credential_storage.clone();
            async move {
                let restore = match record.url.as_deref() {
                    Some(resource_url)
                        if !record.static_authorization && credential_storage.is_persistent() =>
                    {
                        let oauth = record.oauth.clone().unwrap_or_default();
                        Some(auth::restore(resource_url, &oauth, &credential_storage).await)
                    }
                    _ => None,
                };
                (name.clone(), restore)
            }
        }))
        .await;

        let mut connectable = Vec::new();
        let mut restored = BTreeMap::new();
        let mut failures = Vec::new();
        for (name, restore) in restores {
            match restore {
                None | Some(Ok(None)) => connectable.push(name),
                Some(Ok(Some((token, oauth_manager)))) => {
                    restored.insert(name.clone(), (token, oauth_manager));
                    connectable.push(name);
                }
                Some(Err(error)) => failures.push((name, error)),
            }
        }

        let settled = {
            let mut manager = self.inner.manager.lock().await;
            let mut to_connect = Vec::new();
            for name in connectable {
                if let Some((token, _)) = restored.get(&name) {
                    let mut credentials = MetadataMap::new();
                    credentials.insert("access_token".into(), Value::String(token.clone()));
                    if let Err(error) = manager
                        .resolve_auth(AuthResolution::provided(
                            connect_auth_request(&name),
                            credentials,
                        ))
                        .await
                    {
                        restored.remove(&name);
                        failures.push((
                            name.clone(),
                            format!("could not restore MCP credentials for {name}: {error}"),
                        ));
                        continue;
                    }
                }
                to_connect.push(name);
            }
            manager.connect_servers_settled(to_connect).await
        };

        let restored_names = restored.keys().cloned().collect::<BTreeSet<_>>();
        {
            let mut sessions = self.inner.oauth_sessions.lock().await;
            for (name, (token, oauth_manager)) in restored {
                sessions.insert(name, Arc::new(OAuthSession::new(oauth_manager, token)));
            }
        }

        let fingerprints = records
            .iter()
            .map(|(name, record)| (name.clone(), record.clone()))
            .collect::<BTreeMap<_, _>>();
        let (connected, failed) = settled.into_parts();
        for handle in connected {
            let name = handle.server_id().to_string();
            if let Some(record) = fingerprints.get(&name) {
                self.set_status_checked(&name, &record.fingerprint, ServerStatus::Connected)
                    .await;
            }
        }
        for failure in failed {
            let name = failure.server_id.to_string();
            let Some(record) = fingerprints.get(&name) else {
                continue;
            };
            match failure.error {
                McpError::AuthRequired(_) if record.static_authorization => {
                    self.set_status_checked(
                        &name,
                        &record.fingerprint,
                        ServerStatus::Error(format!(
                            "MCP server {name} rejected its configured static authorization"
                        )),
                    )
                    .await;
                }
                McpError::AuthRequired(request) => {
                    self.inner
                        .challenges
                        .lock()
                        .await
                        .insert(name.clone(), *request);
                    self.set_status_checked(
                        &name,
                        &record.fingerprint,
                        ServerStatus::AuthenticationRequired,
                    )
                    .await;
                }
                error if restored_names.contains(&name) => {
                    self.set_status_checked(
                        &name,
                        &record.fingerprint,
                        ServerStatus::Error(format!(
                            "could not connect MCP server {name} with stored credentials: {error}"
                        )),
                    )
                    .await;
                }
                error => {
                    self.set_status_checked(
                        &name,
                        &record.fingerprint,
                        ServerStatus::Error(format!(
                            "could not connect MCP server {name}: {error}"
                        )),
                    )
                    .await;
                }
            }
        }
        for (name, error) in failures {
            if let Some(record) = fingerprints.get(&name) {
                self.set_status_checked(&name, &record.fingerprint, ServerStatus::Error(error))
                    .await;
            }
        }
    }

    async fn search(&self, query: &str) -> Result<Value, ToolError> {
        let prepared = PreparedQuery::new(query).ok_or_else(|| {
            ToolError::InvalidInput("query must contain a letter or number".into())
        })?;
        self.reload_config().await.map_err(ToolError::Unavailable)?;
        self.initialize_uninitialized().await;
        let records = self.inner.servers.read().await.clone();
        let manager = self.inner.manager.lock().await;
        let available = records
            .keys()
            .filter_map(|name| {
                manager
                    .connected_server(&McpServerId::new(name))
                    .map(|handle| (name.clone(), handle.tool_registry().specs()))
            })
            .collect::<BTreeMap<_, _>>();
        drop(manager);
        Ok(render_search(&prepared, &records, &available))
    }

    async fn authorize(&self, name: &str, session_id: String) -> Result<Value, ToolError> {
        self.reload_config().await.map_err(ToolError::Unavailable)?;
        self.initialize_uninitialized().await;
        let operation = self.operation_gate(name).await;
        let _operation = tokio::time::timeout(CONNECT_TIMEOUT, operation.write())
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
        self.initialize_servers(&[name.to_string()]).await;
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
        let _operation = operation.write().await;
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

    async fn set_status_checked(&self, name: &str, fingerprint: &[u8], status: ServerStatus) {
        if let Some(record) = self.inner.servers.write().await.get_mut(name)
            && record.fingerprint == fingerprint
        {
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
                "Reload the MCP config, finish connecting any servers still initializing, then rank every connected server's tools globally and return at most 5 precise matches with input schemas, grouped by server with match counts. Strongly matching servers that need authentication or failed to connect are listed without tools. The exact query `mcp` instead returns a compact status list, with omission counts if the response cap excludes tail entries.",
                json!({"type":"object","properties":{"query":{"type":"string","description":"Capability, product, server, or tool keywords. The exact query `mcp` lists configured server statuses, subject to the reported response cap."}},"required":["query"],"additionalProperties":false}),
            )
            .with_output_schema(json!({
                "type":"object",
                "properties":{
                    "servers":{"type":"array","items":{
                        "type":"object",
                        "properties":{
                            "name":{"type":"string"},
                            "description":{"type":"string"},
                            "status":{"type":"string"},
                            "error":{"type":"string"},
                            "available_tool_count":{"type":["integer","null"]},
                            "matched_tool_count":{"type":"integer"},
                            "returned_tool_count":{"type":"integer"},
                            "truncated":{"type":"boolean"},
                            "tools":{"type":"array","items":{"type":"object"}}
                        },
                        "required":["name","description","status","available_tool_count"],
                        "additionalProperties":false
                    }},
                    "total_matched":{"type":"integer"},
                    "total_returned":{"type":"integer"},
                    "total_servers":{"type":"integer"},
                    "returned_servers":{"type":"integer"},
                    "truncated":{"type":"boolean"}
                },
                "required":["servers"],
                "additionalProperties":false
            })),
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
    executor: Arc<dyn ToolExecutor>,
    spec: ToolSpec,
}

#[allow(dead_code)]
enum McpOperationGuard {
    Shared(OwnedRwLockReadGuard<()>),
    Exclusive(OwnedRwLockWriteGuard<()>),
}

struct McpInvocationLease {
    server: String,
    fingerprint: Vec<u8>,
    _operation: McpOperationGuard,
    _generation: Option<OwnedRwLockReadGuard<()>>,
}

struct ExecutedMcpCall {
    outcome: ToolExecutionOutcome,
    replay: ReplayCleanup,
    server: Option<(String, Vec<u8>)>,
    _invocation: Option<McpInvocationLease>,
}

impl McpTool {
    pub fn new(runtime: McpRuntime) -> Self {
        let catalog = runtime.catalog();
        let source: Arc<dyn ToolSource> = Arc::new(catalog.clone());
        Self {
            runtime,
            catalog,
            executor: Arc::new(BasicToolExecutor::new([source])),
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
        let invocation = match self.runtime.acquire_invocation(&name).await {
            Ok(invocation) => invocation,
            Err(error) => {
                return ExecutedMcpCall {
                    outcome: ToolExecutionOutcome::FailedBeforeInvocation(error),
                    replay: ReplayCleanup::new(None, self.runtime.clone(), None),
                    server: None,
                    _invocation: None,
                };
            }
        };
        let server = Some((invocation.server.clone(), invocation.fingerprint.clone()));
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
        let scope = ToolExecutionScope {
            executor: Arc::clone(&self.executor),
            ..scope
        };
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
            _invocation: Some(invocation),
        }
    }

    async fn finish_dispatch(&self, call: ExecutedMcpCall) -> ToolExecutionOutcome {
        let ExecutedMcpCall {
            outcome,
            replay,
            server,
            _invocation,
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
            sources: Vec::new(),
            entries: BTreeMap::new(),
            plugins: BTreeMap::new(),
            plugin_entries: BTreeMap::new(),
            plugin_source: None,
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

/// Returns whether the query matches the spec's name: either the whole
/// query appears in the name, or a query term hits a name token.
fn name_matched(query: &PreparedQuery, spec: &PreparedSpec) -> bool {
    spec.name.normalized.contains(&query.0.normalized)
        || query
            .0
            .tokens
            .iter()
            .any(|term| regular_term_score(term, spec) >= 100)
}

/// Scores a spec against the query with the high-precision gates applied:
/// the ranked score must reach `SEARCH_MIN_SCORE`, and the spec must either
/// match every query term or match the query in its name.
fn matched_score(query: &PreparedQuery, spec: &PreparedSpec, fuzzy: &[bool]) -> Option<u32> {
    let score = score_spec(query, spec, fuzzy)?;
    if score < SEARCH_MIN_SCORE {
        return None;
    }
    let full_coverage = query.0.tokens.iter().enumerate().all(|(index, term)| {
        regular_term_score(term, spec) > 0 || fuzzy[index] && fuzzy_term_score(term, spec) > 0
    });
    (full_coverage || name_matched(query, spec)).then_some(score)
}

fn bounded_server_text(value: &str) -> &str {
    if value.len() <= SEARCH_SERVER_TEXT_BYTE_CAP {
        return value;
    }
    let mut end = SEARCH_SERVER_TEXT_BYTE_CAP;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Renders the `tool_search` response from a snapshot of the configured
/// server records and each connected server's tool specs. The exact query
/// `mcp` produces the compact server listing; every other query ranks all
/// connected tools globally and returns at most `SEARCH_RESULT_LIMIT` of
/// them. Both modes drop tail results until the serialized response fits
/// `SEARCH_RESULT_BYTE_CAP`.
fn render_search(
    query: &PreparedQuery,
    records: &BTreeMap<String, ServerRecord>,
    available: &BTreeMap<String, Vec<ToolSpec>>,
) -> Value {
    if query.0.normalized == "mcp" {
        let total_servers = records.len();
        let mut servers = records
            .iter()
            .map(|(name, record)| {
                let mut entry = json!({
                    "name": name,
                    "description": bounded_server_text(&record.description),
                    "status": record.status.as_str(),
                    "available_tool_count": available.get(name).map(Vec::len),
                });
                if let ServerStatus::Error(error) = &record.status {
                    entry["error"] = Value::String(bounded_server_text(error).into());
                }
                entry
            })
            .collect::<Vec<_>>();
        loop {
            let response = json!({
                "returned_servers": servers.len(),
                "servers": servers,
                "total_servers": total_servers,
                "truncated": servers.len() < total_servers,
            });
            if serde_json::to_vec(&response)
                .expect("search results serialize")
                .len()
                <= SEARCH_RESULT_BYTE_CAP
            {
                return response;
            }
            assert!(
                servers.pop().is_some(),
                "empty compact search response exceeds byte cap"
            );
        }
    }

    let catalog = available
        .iter()
        .flat_map(|(name, specs)| {
            specs
                .iter()
                .map(move |spec| (name.clone(), PreparedSpec::new(spec.clone())))
        })
        .collect::<Vec<_>>();
    let fuzzy = query
        .0
        .tokens
        .iter()
        .map(|term| {
            !catalog
                .iter()
                .any(|(_, spec)| regular_term_score(term, spec) > 0)
        })
        .collect::<Vec<_>>();
    let mut candidates = catalog
        .iter()
        .filter_map(|(server, spec)| {
            matched_score(query, spec, &fuzzy).map(|score| (server, spec, score))
        })
        .collect::<Vec<_>>();
    if let Some(top) = candidates.iter().map(|(_, _, score)| *score).max() {
        candidates.retain(|(_, _, score)| score * 3 >= top);
    }
    candidates.sort_by(|(_, left, left_score), (_, right, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.spec.name.0.cmp(&right.spec.name.0))
    });

    let mut matched = BTreeMap::<&str, usize>::new();
    for (server, _, _) in &candidates {
        *matched.entry(server.as_str()).or_default() += 1;
    }
    // A server without returned tools appears only when non-connected and on
    // a strong name match, so a description-only hit cannot surface
    // unrelated auth/error servers. Strong groups are appended after the
    // returned-tool groups, ordered by score then name.
    let mut strong_servers = records
        .iter()
        .filter(|(_, record)| !matches!(record.status, ServerStatus::Connected))
        .filter_map(|(name, record)| {
            let spec = PreparedSpec::new(ToolSpec::new(
                ToolName::new(name.as_str()),
                &record.description,
                json!({"type":"object"}),
            ));
            name_matched(query, &spec).then(|| {
                let score = score_spec(query, &spec, &vec![false; query.0.tokens.len()]);
                (name.clone(), score.unwrap_or(0))
            })
        })
        .collect::<Vec<_>>();
    strong_servers.sort_by(|(left, left_score), (right, right_score)| {
        right_score.cmp(left_score).then_with(|| left.cmp(right))
    });

    let group_for = |name: &str, tools: Vec<Value>| {
        let record = &records[name];
        let matched_count = matched.get(name).copied().unwrap_or(0);
        let mut group = json!({
            "name": name,
            "description": bounded_server_text(&record.description),
            "status": record.status.as_str(),
            "available_tool_count": available.get(name).map(Vec::len),
            "matched_tool_count": matched_count,
            "returned_tool_count": tools.len(),
            "truncated": tools.len() < matched_count,
            "tools": tools,
        });
        if let ServerStatus::Error(error) = &record.status {
            group["error"] = Value::String(bounded_server_text(error).into());
        }
        group
    };

    let mut returned = candidates
        .iter()
        .take(SEARCH_RESULT_LIMIT)
        .collect::<Vec<_>>();
    let total_strong_servers = strong_servers.len();
    let mut returned_strong_servers = total_strong_servers;
    loop {
        // Group the currently returned tools by server, ordered by each
        // server's best globally ranked returned candidate.
        let mut server_order = Vec::<&str>::new();
        let mut returned_by_server = BTreeMap::<&str, Vec<Value>>::new();
        for (server, spec, _) in &returned {
            let tools = match returned_by_server.entry(server.as_str()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    server_order.push(server.as_str());
                    entry.insert(Vec::new())
                }
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            };
            tools.push(json!({
                "name": spec.spec.name.0,
                "description": spec.spec.description,
                "input_schema": spec.spec.input_schema,
            }));
        }
        let groups = server_order
            .iter()
            .map(|name| {
                let tools = returned_by_server.remove(name).unwrap_or_default();
                group_for(name, tools)
            })
            .chain(
                strong_servers[..returned_strong_servers]
                    .iter()
                    .map(|(name, _)| group_for(name, Vec::new())),
            )
            .collect::<Vec<_>>();
        let response = json!({
            "servers": groups,
            "total_matched": candidates.len(),
            "total_returned": returned.len(),
            "truncated": returned.len() < candidates.len()
                || returned_strong_servers < total_strong_servers,
        });
        let serialized = serde_json::to_vec(&response)
            .expect("search results serialize")
            .len();
        if serialized <= SEARCH_RESULT_BYTE_CAP {
            return response;
        }
        if returned_strong_servers > 0 {
            returned_strong_servers -= 1;
        } else {
            assert!(
                returned.pop().is_some(),
                "empty search response exceeds byte cap"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        path::Path,
        sync::{Arc, atomic::Ordering},
        time::Duration,
    };

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
        AuthRecorder, Config, ConfigSource, CredentialStorage, McpTool, OAuthSession,
        OpportunisticRefresh, PreparedQuery, PreparedSpec, ReplayCleanup, ServerRecord,
        ServerStatus, agentkit_auth_not_applied, agentkit_replay_rejected,
        can_opportunistically_refresh, challenge_requires_interactive_authorization, matched_score,
        prepare_config, prepare_plugins, regular_term_score, serializes_tool_calls,
        validate_server_names,
    };
    use crate::plugins::{PluginRuntime, ResolvedPluginMcp, ResolvedPlugins};

    #[cfg(unix)]
    mod config_capacity {
        use crate as kit;
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/capacity.rs"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn config_symlink_reads_memory_only_target() {
        use config_capacity::{Capacity, CapacityDisk};
        use std::sync::atomic::AtomicBool;
        let directory = tempfile::tempdir().unwrap();
        let capacity = Arc::new(Capacity {
            exhausted: AtomicBool::new(true),
            exhaust_on_write: AtomicBool::new(false),
            repaired: directory.path().join("repaired"),
        });
        let filesystem = crate::resilient_fs::Fs::new(Arc::new(CapacityDisk(capacity)));
        let project = directory.path().join("project");
        let elsewhere = directory.path().join("elsewhere");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir_all(elsewhere.join("nested")).unwrap();
        let target = directory.path().join("config.json");
        let link = directory.path().join(".mcp.json");
        std::os::unix::fs::symlink("config.json", &link).unwrap();
        filesystem.write(&target, b"pending").unwrap();
        let parent_link = project.join(".mcp.json");
        std::os::unix::fs::symlink("../config.json", &parent_link).unwrap();
        assert_eq!(
            crate::config_files::read_in(&filesystem, &parent_link).unwrap(),
            b"pending"
        );

        // `..` must follow directory symlinks before selecting the parent.
        std::os::unix::fs::symlink(elsewhere.join("nested"), project.join("directory-link"))
            .unwrap();
        let indirect = project.join("indirect.json");
        std::os::unix::fs::symlink("directory-link/../config.json", &indirect).unwrap();
        filesystem
            .write(elsewhere.join("config.json"), b"resolved parent")
            .unwrap();
        filesystem
            .write(project.join("config.json"), b"wrong lexical parent")
            .unwrap();
        assert_eq!(
            crate::config_files::read_in(&filesystem, &indirect).unwrap(),
            b"resolved parent"
        );

        // Traversal also works through an accepted, memory-only directory.
        filesystem
            .create_dir(directory.path().join("virtual"))
            .unwrap();
        let virtual_link = project.join("virtual.json");
        std::os::unix::fs::symlink("../virtual/../config.json", &virtual_link).unwrap();
        assert_eq!(
            crate::config_files::read_in(&filesystem, &virtual_link).unwrap(),
            b"pending"
        );
        assert_eq!(
            crate::config_files::read_in(&filesystem, &project.join("missing/../config.json"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        assert_eq!(
            crate::config_files::read_in(&filesystem, &project.join("config.json/../config.json"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotADirectory
        );
        assert!(!target.exists());
        assert_eq!(
            crate::config_files::read_in(&filesystem, &link).unwrap(),
            b"pending"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn config_sources_follow_symlinks_and_reload_targets() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("config.json");
        let link = directory.path().join(".mcp.json");
        let chained = directory.path().join("explicit.json");
        std::fs::write(&target, b"first").unwrap();
        symlink("config.json", &link).unwrap();
        symlink(&link, &chained).unwrap();
        for source in [
            ConfigSource::required(chained),
            ConfigSource::optional_project(link.clone(), directory.path().to_path_buf()),
        ] {
            assert_eq!(
                super::read_source(&source).await.unwrap(),
                Some(b"first".to_vec())
            );
            crate::resilient_fs::write(&target, b"second").unwrap();
            assert_eq!(
                super::read_source(&source).await.unwrap(),
                Some(b"second".to_vec())
            );
            std::fs::write(&target, b"first").unwrap();
        }
        // Security-sensitive managed reads still reject final symlinks.
        assert_eq!(
            crate::resilient_fs::read(&link).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        std::fs::remove_file(&target).unwrap();
        assert!(
            super::read_source(&ConfigSource::optional_project(
                link.clone(),
                directory.path().to_path_buf()
            ))
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            super::read_source(&ConfigSource::required(link))
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn config_source_rejects_symlink_cycles() {
        let directory = tempfile::tempdir().unwrap();
        let link = directory.path().join("loop.json");
        std::os::unix::fs::symlink("loop.json", &link).unwrap();
        let error = super::read_source(&ConfigSource::required(link))
            .await
            .unwrap_err();
        assert!(error.contains("too many symbolic links"), "{error}");
    }

    fn spec(name: &str, description: &str) -> ToolSpec {
        ToolSpec::new(ToolName::new(name), description, json!({"type": "object"}))
    }

    fn matched(query: &str, specs: Vec<ToolSpec>) -> Vec<(String, u32)> {
        let query = PreparedQuery::new(query).unwrap();
        let specs = specs.into_iter().map(PreparedSpec::new).collect::<Vec<_>>();
        let fuzzy = query
            .0
            .tokens
            .iter()
            .map(|term| !specs.iter().any(|spec| regular_term_score(term, spec) > 0))
            .collect::<Vec<_>>();
        specs
            .iter()
            .filter_map(|spec| {
                matched_score(&query, spec, &fuzzy).map(|score| (spec.spec.name.0.clone(), score))
            })
            .collect()
    }

    fn connected_oauth_record() -> ServerRecord {
        ServerRecord {
            description: "Remote".into(),
            url: Some("https://example.com/mcp".into()),
            oauth: None,
            static_authorization: false,
            fingerprint: vec![1],
            plugin_owned: false,
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
        let _held = first.write().await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), other.write())
                .await
                .is_ok()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), same.write())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn concurrent_safe_invocations_share_the_reload_gate() {
        let runtime = super::empty();
        let mut record = connected_oauth_record();
        record.url = None;
        runtime
            .inner
            .servers
            .write()
            .await
            .insert("same".into(), record);
        let first = runtime.acquire_invocation("mcp_same_read").await.unwrap();
        let second = tokio::time::timeout(
            Duration::from_millis(25),
            runtime.acquire_invocation("mcp_same_other"),
        )
        .await
        .expect("concurrent-safe MCP invocation was serialized")
        .unwrap();
        drop((first, second));
    }

    #[tokio::test]
    async fn same_name_fingerprint_change_rejects_waiting_invocation() {
        let runtime = super::empty();
        runtime
            .inner
            .servers
            .write()
            .await
            .insert("same".into(), connected_oauth_record());
        let gate = runtime.operation_gate("same").await;
        let held = gate.write().await;
        let waiting = {
            let runtime = runtime.clone();
            tokio::spawn(async move { runtime.acquire_invocation("mcp_same_write").await })
        };
        tokio::time::sleep(Duration::from_millis(25)).await;
        runtime
            .inner
            .servers
            .write()
            .await
            .get_mut("same")
            .unwrap()
            .fingerprint = vec![2];
        drop(held);

        let error = match waiting.await.unwrap() {
            Ok(_) => panic!("waiting invocation accepted a changed fingerprint"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("configuration changed"));
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
        let held = gate.write().await;
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
        // The staged snapshot may publish after the gate clears, but config I/O
        // never runs while runtime or operation locks are held. The next
        // boundary observes an edit that arrived during the wait.
        reloading.await.unwrap().unwrap();
        runtime.reload_config().await.unwrap();
    }

    #[tokio::test]
    async fn plugin_generation_reads_remain_available_while_initialization_is_blocked() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let package = directory.path().join("plugin");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("plugin.json"),
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"blocked-plugin"}"#,
        )
        .unwrap();
        std::fs::write(
            package.join("mcp.json"),
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json","mcpServers":{"live":{"type":"streamable-http","url":"https://example.com/mcp"}}}"#,
        )
        .unwrap();
        std::fs::write(&config, "").unwrap();
        let plugins = PluginRuntime::new(
            config.clone(),
            directory.path().to_path_buf(),
            directory.path().join("cache"),
            directory.path().join("data"),
            ResolvedPlugins::default(),
        );
        let runtime = super::connect_dynamic(
            None::<&Path>,
            plugins.clone(),
            true,
            CredentialStorage::Memory,
        )
        .await
        .unwrap();
        std::fs::write(
            &config,
            format!(
                "[plugins.blocked]\nsource = 'path'\npath = '{}'\n",
                package.display()
            ),
        )
        .unwrap();

        let initialization = runtime.inner.initialization.lock().await;
        let gate = runtime.operation_gate("live").await;
        let reloading = {
            let runtime = runtime.clone();
            tokio::spawn(async move { runtime.reload_config().await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if gate.clone().try_read_owned().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reload did not reach the initialization guard");

        let generation =
            tokio::time::timeout(Duration::from_millis(100), plugins.generation_lease())
                .await
                .expect("plugin generation reads were blocked by MCP initialization");
        drop(generation);
        drop(initialization);
        reloading.await.unwrap().unwrap();
        assert_eq!(plugins.snapshot().mcp_plugins.len(), 1);
    }

    #[tokio::test]
    async fn queued_reload_rereads_changes_that_arrived_during_the_prior_reload() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let package = directory.path().join("plugin");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("plugin.json"),
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"queued-plugin"}"#,
        )
        .unwrap();
        std::fs::write(&config, "").unwrap();
        let plugins = PluginRuntime::new(
            config.clone(),
            directory.path().to_path_buf(),
            directory.path().join("cache"),
            directory.path().join("data"),
            ResolvedPlugins::default(),
        );
        let runtime = super::connect_dynamic(
            None::<&Path>,
            plugins.clone(),
            true,
            CredentialStorage::Memory,
        )
        .await
        .unwrap();
        let write_mcp = |url: &str| {
            std::fs::write(
                package.join("mcp.json"),
                format!(
                    r#"{{"$schema":"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json","mcpServers":{{"live":{{"type":"streamable-http","url":"{url}"}}}}}}"#
                ),
            )
            .unwrap();
        };
        write_mcp("https://example.com/first");
        std::fs::write(
            &config,
            format!(
                "[plugins.queued]\nsource = 'path'\npath = '{}'\n",
                package.display()
            ),
        )
        .unwrap();

        let generation = plugins.generation_lease().await;
        let gate = runtime.operation_gate("live").await;
        let first = {
            let runtime = runtime.clone();
            tokio::spawn(async move { runtime.reload_config().await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if gate.clone().try_read_owned().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first reload did not reach plugin publication");

        write_mcp("https://example.com/second");
        let second = {
            let runtime = runtime.clone();
            tokio::spawn(async move { runtime.reload_config().await })
        };
        drop(generation);
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        assert_eq!(
            runtime.inner.servers.read().await["live"].url.as_deref(),
            Some("https://example.com/second")
        );
    }

    #[tokio::test]
    async fn cancelled_reload_caller_does_not_strand_completion() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(&path, r#"{"mcpServers":{}}"#).unwrap();
        let runtime = super::connect(Some(&path), &[], true, CredentialStorage::Memory)
            .await
            .unwrap();
        let gate = runtime.operation_gate("local").await;
        let held = gate.write().await;
        std::fs::write(&path, r#"{"mcpServers":{"local":{"command":"unused"}}}"#).unwrap();
        let epoch = runtime.inner.reload_epoch.load(Ordering::Acquire);
        let caller = {
            let runtime = runtime.clone();
            tokio::spawn(async move { runtime.reload_config().await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while runtime.inner.reload_flight.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(25)).await;
        caller.abort();
        drop(held);

        tokio::time::timeout(Duration::from_secs(1), async {
            while runtime.inner.reload_epoch.load(Ordering::Acquire) == epoch {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached reload completion did not publish its epoch");
        assert!(runtime.inner.servers.read().await.contains_key("local"));
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
    fn matching_requires_a_name_hit_or_full_term_coverage() {
        // A partial description hit no longer pulls in unrelated tools: the
        // query must match the name or cover every term.
        assert!(
            matched(
                "gmail email capabilities",
                vec![spec("mcp_jira_search", "Search email threads")]
            )
            .is_empty()
        );

        // A name hit passes even when other query terms miss.
        let results = matched(
            "gmail send unrelatedterm",
            vec![spec("mcp_gmail_send_email", "Send an email")],
        );
        assert_eq!(results.len(), 1);
        assert!(results[0].1 >= super::SEARCH_MIN_SCORE);

        // Full term coverage passes without a name hit when the score is
        // strong enough.
        let results = matched(
            "project tracker",
            vec![spec("mcp_pm_create", "Create issues in a project tracker")],
        );
        assert_eq!(results.len(), 1);

        // Tools matching no term are never returned.
        assert!(matched("gmail", vec![spec("mcp_echo", "Echo text")]).is_empty());
    }

    fn search_record(description: &str, status: ServerStatus) -> ServerRecord {
        ServerRecord {
            description: description.into(),
            url: None,
            oauth: None,
            static_authorization: false,
            fingerprint: vec![1],
            plugin_owned: false,
            status,
        }
    }

    fn query(value: &str) -> PreparedQuery {
        PreparedQuery::new(value).unwrap()
    }

    #[test]
    fn unconnected_servers_appear_only_on_a_name_match() {
        let records = BTreeMap::from([(
            "linear".to_string(),
            search_record(
                "Issues and project management",
                ServerStatus::AuthenticationRequired,
            ),
        )]);
        let available = BTreeMap::new();

        // Full description coverage is not enough for a server without
        // returned tools.
        let response =
            super::render_search(&query("issues project management"), &records, &available);
        assert_eq!(response["servers"], json!([]));
        assert_eq!(response["total_matched"], 0);

        let response = super::render_search(&query("linear"), &records, &available);
        assert_eq!(response["servers"][0]["name"], "linear");
        assert_eq!(response["servers"][0]["status"], "authentication_required");
        assert_eq!(response["servers"][0]["available_tool_count"], Value::Null);
        assert_eq!(response["servers"][0]["tools"], json!([]));
    }

    #[test]
    fn search_returns_at_most_five_tools_globally_with_counts() {
        let records = BTreeMap::from([(
            "issues".to_string(),
            search_record("Issue tracker", ServerStatus::Connected),
        )]);
        let specs = (0..8)
            .map(|index| {
                spec(
                    &format!("mcp_issues_tool{index}"),
                    "Create linear issue records",
                )
                .with_output_schema(json!({"type": "object"}))
            })
            .collect::<Vec<_>>();
        let available = BTreeMap::from([("issues".to_string(), specs)]);

        let response = super::render_search(&query("linear issues"), &records, &available);
        assert_eq!(response["total_matched"], 8);
        assert_eq!(response["total_returned"], 5);
        assert_eq!(response["truncated"], true);
        let group = &response["servers"][0];
        assert_eq!(group["available_tool_count"], 8);
        assert_eq!(group["matched_tool_count"], 8);
        assert_eq!(group["returned_tool_count"], 5);
        assert_eq!(group["truncated"], true);
        let tools = group["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 5);
        assert_eq!(tools[0]["name"], "mcp_issues_tool0");
        assert!(tools[0].get("input_schema").is_some());
        assert!(
            tools[0].get("output_schema").is_none(),
            "output schemas are omitted from search results"
        );
    }

    #[test]
    fn server_groups_follow_global_rank_and_require_a_returned_tool() {
        let records = BTreeMap::from([
            (
                "alpha".to_string(),
                search_record("File helpers", ServerStatus::Connected),
            ),
            (
                "omega".to_string(),
                search_record("More file helpers", ServerStatus::Connected),
            ),
            (
                "zeta".to_string(),
                search_record("Best file helpers", ServerStatus::Connected),
            ),
            (
                "files-hub".to_string(),
                search_record("Remote files", ServerStatus::AuthenticationRequired),
            ),
        ]);
        let available = BTreeMap::from([
            (
                "alpha".to_string(),
                (1..=5)
                    .map(|index| spec(&format!("mcp_alpha_files{index}"), "Work with files"))
                    .collect::<Vec<_>>(),
            ),
            (
                "omega".to_string(),
                vec![spec("mcp_omega_files9", "Work with files")],
            ),
            (
                "zeta".to_string(),
                vec![spec("mcp_zeta_files", "Work with files")],
            ),
        ]);

        let response = super::render_search(&query("files"), &records, &available);
        let names = response["servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| group["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        // The exact-name hit on zeta outranks alpha's prefix hits, so group
        // order follows global rank, not alphabetical order; omega matched
        // but owns no returned top-5 tool, so it is omitted entirely; the
        // strong non-connected name match is appended last.
        assert_eq!(names, ["zeta", "alpha", "files-hub"]);
        assert_eq!(response["servers"][0]["tools"][0]["name"], "mcp_zeta_files");
        assert_eq!(response["servers"][1]["matched_tool_count"], 5);
        assert_eq!(response["servers"][1]["returned_tool_count"], 4);
        assert_eq!(response["servers"][1]["truncated"], true);
        assert_eq!(response["servers"][2]["tools"], json!([]));
        // The omitted server still counts toward the global totals.
        assert_eq!(response["total_matched"], 7);
        assert_eq!(response["total_returned"], 5);
        assert_eq!(response["truncated"], true);
    }

    #[test]
    fn oversized_results_drop_the_lowest_ranked_tools() {
        let records = BTreeMap::from([(
            "files".to_string(),
            search_record("File tools", ServerStatus::Connected),
        )]);
        let available = BTreeMap::from([(
            "files".to_string(),
            vec![
                spec("mcp_files_read", "Read files"),
                ToolSpec::new(
                    ToolName::new("mcp_files_write"),
                    "Write files",
                    json!({"type": "object", "description": "x".repeat(2 * super::SEARCH_RESULT_BYTE_CAP)}),
                ),
            ],
        )]);

        let response = super::render_search(&query("files"), &records, &available);
        assert!(
            serde_json::to_vec(&response).unwrap().len() <= super::SEARCH_RESULT_BYTE_CAP,
            "the serialized response respects the byte cap"
        );
        assert_eq!(response["total_matched"], 2);
        assert_eq!(response["total_returned"], 1);
        assert_eq!(response["truncated"], true);
        let group = &response["servers"][0];
        assert_eq!(group["returned_tool_count"], 1);
        assert_eq!(group["truncated"], true);
        assert_eq!(group["tools"][0]["name"], "mcp_files_read");
    }

    #[test]
    fn oversized_status_only_groups_are_dropped_before_tools() {
        let huge_text = format!(
            "{}🦀{}",
            "x".repeat(super::SEARCH_SERVER_TEXT_BYTE_CAP - 1),
            "y".repeat(super::SEARCH_SERVER_TEXT_BYTE_CAP)
        );
        let mut records = BTreeMap::from([(
            "files".to_string(),
            search_record("File tools", ServerStatus::Connected),
        )]);
        for index in 0..40 {
            records.insert(
                format!("files-status-{index:02}"),
                search_record(&huge_text, ServerStatus::Error(huge_text.clone())),
            );
        }
        let available = BTreeMap::from([(
            "files".to_string(),
            vec![spec("mcp_files_read", "Read files")],
        )]);

        let response = super::render_search(&query("files"), &records, &available);
        assert!(serde_json::to_vec(&response).unwrap().len() <= super::SEARCH_RESULT_BYTE_CAP);
        assert_eq!(response["total_matched"], 1);
        assert_eq!(response["total_returned"], 1);
        assert_eq!(response["truncated"], true);
        assert_eq!(response["servers"][0]["name"], "files");
        assert_eq!(response["servers"][0]["tools"][0]["name"], "mcp_files_read");
        let groups = response["servers"].as_array().unwrap();
        assert!(groups.len() > 1 && groups.len() < records.len());
        for group in &groups[1..] {
            let description = group["description"].as_str().unwrap();
            let error = group["error"].as_str().unwrap();
            assert_eq!(description.len(), super::SEARCH_SERVER_TEXT_BYTE_CAP - 1);
            assert_eq!(error.len(), super::SEARCH_SERVER_TEXT_BYTE_CAP - 1);
            assert!(std::str::from_utf8(description.as_bytes()).is_ok());
            assert!(std::str::from_utf8(error.as_bytes()).is_ok());
        }
    }

    #[test]
    fn oversized_compact_listing_reports_tail_omission() {
        let huge_text = format!(
            "{}🦀{}",
            "x".repeat(super::SEARCH_SERVER_TEXT_BYTE_CAP - 1),
            "y".repeat(super::SEARCH_SERVER_TEXT_BYTE_CAP)
        );
        let records = (0..40)
            .map(|index| {
                (
                    format!("server-{index:02}"),
                    search_record(&huge_text, ServerStatus::Error(huge_text.clone())),
                )
            })
            .collect::<BTreeMap<_, _>>();

        let response = super::render_search(&query("McP"), &records, &BTreeMap::new());
        assert!(serde_json::to_vec(&response).unwrap().len() <= super::SEARCH_RESULT_BYTE_CAP);
        assert_eq!(response["total_servers"], records.len());
        let returned = response["returned_servers"].as_u64().unwrap() as usize;
        assert!(returned > 0 && returned < records.len());
        assert_eq!(response["truncated"], true);
        let servers = response["servers"].as_array().unwrap();
        assert_eq!(servers.len(), returned);
        assert_eq!(servers[0]["name"], "server-00");
        assert_eq!(
            servers.last().unwrap()["name"],
            format!("server-{:02}", returned - 1)
        );
        for server in servers {
            let description = server["description"].as_str().unwrap();
            let error = server["error"].as_str().unwrap();
            assert_eq!(description.len(), super::SEARCH_SERVER_TEXT_BYTE_CAP - 1);
            assert_eq!(error.len(), super::SEARCH_SERVER_TEXT_BYTE_CAP - 1);
            assert!(std::str::from_utf8(description.as_bytes()).is_ok());
            assert!(std::str::from_utf8(error.as_bytes()).is_ok());
        }
    }

    #[test]
    fn matching_scores_rank_name_hits_above_description_hits() {
        let results = matched(
            "linear",
            vec![
                spec("mcp_linear_create_issue", "Create an issue"),
                spec("mcp_generic_track", "Linear and Jira integrations"),
            ],
        );
        assert_eq!(results.len(), 2);
        let score = |name: &str| results.iter().find(|(result, _)| result == name).unwrap().1;
        let name_hit = score("mcp_linear_create_issue");
        let description_hit = score("mcp_generic_track");
        assert!(name_hit > description_hit);
        assert!(
            description_hit * 3 < name_hit,
            "the relative gate should drop the description-only hit"
        );
    }

    #[test]
    fn config_is_strict_and_accepts_compatible_transport_types() {
        for config in [
            r#"{"mcpServers":{"legacy":{"command":"server"}}}"#,
            r#"{"mcpServers":{"typed":{"type":"stdio","command":"server"}}}"#,
            r#"{"mcpServers":{"legacy":{"url":"https://mcp.example/mcp"}}}"#,
            r#"{"mcpServers":{"typed":{"type":"streamable-http","url":"https://mcp.example/mcp"}}}"#,
            r#"{"mcpServers":{"typed":{"type":"http","url":"https://mcp.example/mcp"}}}"#,
        ] {
            assert!(serde_json::from_str::<Config>(config).is_ok(), "{config}");
        }
        for config in [
            r#"{"mcpServers":{},"extra":true}"#,
            r#"{"mcpServers":{"bad":{"command":"x","url":"http://localhost"}}}"#,
            r#"{"mcpServers":{"bad":{"type":"http","command":"x"}}}"#,
            r#"{"mcpServers":{"bad":{"type":"stdio","url":"http://localhost"}}}"#,
            r#"{"mcpServers":{"bad":{"type":"sse","url":"http://localhost"}}}"#,
            r#"{"mcpServers":{"bad":{"type":1,"command":"x"}}}"#,
            r#"{"mcpServers":{"bad":{"type":null,"command":"x"}}}"#,
            r#"{"mcpServers":{"bad":{"type":null,"url":"http://localhost"}}}"#,
        ] {
            assert!(serde_json::from_str::<Config>(config).is_err(), "{config}");
        }
    }

    #[test]
    fn project_stdio_resolves_cwd_before_transport_and_fingerprinting() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let path = first.path().join(".mcp.json");

        let missing = br#"{"mcpServers":{"local":{"command":"server"}}}"#;
        let (prepared, _) = prepare_config(missing, &path, Some(first.path())).unwrap();
        let McpTransportBinding::Stdio(transport) = &prepared["local"].config.transport else {
            panic!("expected stdio transport");
        };
        assert_eq!(transport.cwd.as_deref(), Some(first.path()));

        let relative = br#"{"mcpServers":{"local":{"command":"server","cwd":"tools"}}}"#;
        let (prepared, first_entries) =
            prepare_config(relative, &path, Some(first.path())).unwrap();
        let McpTransportBinding::Stdio(transport) = &prepared["local"].config.transport else {
            panic!("expected stdio transport");
        };
        assert_eq!(
            transport.cwd.as_deref(),
            Some(first.path().join("tools").as_path())
        );
        let (_, second_entries) = prepare_config(relative, &path, Some(second.path())).unwrap();
        assert_ne!(first_entries["local"], second_entries["local"]);

        let absolute = first.path().join("absolute");
        let absolute_config = serde_json::to_vec(&json!({
            "mcpServers": {
                "local": {"command": "server", "cwd": absolute}
            }
        }))
        .unwrap();
        let (prepared, first_entries) =
            prepare_config(&absolute_config, &path, Some(first.path())).unwrap();
        let McpTransportBinding::Stdio(transport) = &prepared["local"].config.transport else {
            panic!("expected stdio transport");
        };
        assert_eq!(transport.cwd.as_deref(), Some(absolute.as_path()));
        let (_, second_entries) =
            prepare_config(&absolute_config, &path, Some(second.path())).unwrap();
        assert_eq!(first_entries["local"], second_entries["local"]);
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
    fn plugin_mcp_expands_stdio_paths() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let plugin = plugin(
            "tools",
            directory.path(),
            vec![PluginMcpServer {
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
            }],
        );
        let (prepared, _) = prepare_plugins(&[plugin]).unwrap();
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
    fn plugin_mcp_rejects_sse_without_partial_publication() {
        let directory = tempfile::tempdir().unwrap();
        let plugin = plugin(
            "tools",
            directory.path(),
            vec![PluginMcpServer {
                name: "legacy".into(),
                transport: PluginMcpTransport::Sse {
                    url: "https://example.com/sse".into(),
                    headers: Default::default(),
                },
            }],
        );
        assert!(prepare_plugins(&[plugin]).is_err());
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
    async fn ordered_sources_merge_and_optional_deletion_reveals_lower_layers() {
        let directory = tempfile::tempdir().unwrap();
        let configured = directory.path().join("configured.json");
        let project = directory.path().join(".mcp.json");
        let explicit = directory.path().join("explicit.json");
        std::fs::write(
            &configured,
            r#"{"mcpServers":{"shared":{"command":"missing-configured","description":"configured"},"configured-only":{"command":"missing"}}}"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"{"mcpServers":{"shared":{"type":"stdio","command":"missing-project","description":"project"},"project-only":{"command":"missing"}}}"#,
        )
        .unwrap();
        std::fs::write(
            &explicit,
            r#"{"mcpServers":{"shared":{"command":"missing-explicit","description":"explicit"},"explicit-only":{"command":"missing"}}}"#,
        )
        .unwrap();
        let plugin = plugin(
            "tools",
            directory.path(),
            vec![PluginMcpServer {
                name: "shared".into(),
                transport: PluginMcpTransport::Stdio {
                    command: "missing-plugin".into(),
                    args: Vec::new(),
                    env: Default::default(),
                    cwd: None,
                },
            }],
        );
        let sources = vec![
            ConfigSource::required(configured.clone()),
            ConfigSource::optional_project(project.clone(), directory.path().to_path_buf()),
            ConfigSource::required(explicit.clone()),
        ];
        let runtime = super::connect(sources, &[plugin], true, CredentialStorage::Memory)
            .await
            .unwrap();
        let servers = runtime.inner.servers.read().await;
        assert_eq!(servers["shared"].description, "explicit");
        for name in ["configured-only", "project-only", "explicit-only"] {
            assert!(servers.contains_key(name));
        }
        drop(servers);

        std::fs::write(&explicit, r#"{"mcpServers":{}}"#).unwrap();
        runtime.reload_config().await.unwrap();
        assert_eq!(
            runtime.inner.servers.read().await["shared"].description,
            "project"
        );
        std::fs::remove_file(&project).unwrap();
        runtime.reload_config().await.unwrap();
        assert_eq!(
            runtime.inner.servers.read().await["shared"].description,
            "configured"
        );
        std::fs::write(&configured, r#"{"mcpServers":{}}"#).unwrap();
        runtime.reload_config().await.unwrap();
        assert_eq!(
            runtime.inner.servers.read().await["shared"].description,
            "tools-manifest plugin MCP server"
        );
    }

    #[tokio::test]
    async fn missing_required_source_fails_startup() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("missing.json");
        let error = match super::connect(
            vec![ConfigSource::required(path.clone())],
            &[],
            true,
            CredentialStorage::Memory,
        )
        .await
        {
            Ok(_) => panic!("missing required source was accepted"),
            Err(error) => error,
        };
        assert!(error.contains(&path.display().to_string()));
    }

    #[tokio::test]
    async fn absent_optional_project_source_is_tracked_for_live_creation() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join(".mcp.json");
        let runtime = super::connect(
            vec![ConfigSource::optional_project(
                project.clone(),
                directory.path().to_path_buf(),
            )],
            &[],
            true,
            CredentialStorage::Memory,
        )
        .await
        .unwrap();
        assert!(runtime.inner.servers.read().await.is_empty());

        std::fs::write(
            &project,
            r#"{"mcpServers":{"local":{"command":"missing"}}}"#,
        )
        .unwrap();
        runtime.reload_config().await.unwrap();
        assert!(runtime.inner.servers.read().await.contains_key("local"));
    }

    #[tokio::test]
    async fn invalid_project_reload_preserves_state_until_optional_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let configured = directory.path().join("configured.json");
        let project = directory.path().join(".mcp.json");
        std::fs::write(
            &configured,
            r#"{"mcpServers":{"shared":{"command":"missing","description":"configured"}}}"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"{"mcpServers":{"shared":{"command":"missing","description":"project"}}}"#,
        )
        .unwrap();
        let runtime = super::connect(
            vec![
                ConfigSource::required(configured),
                ConfigSource::optional_project(project.clone(), directory.path().to_path_buf()),
            ],
            &[],
            true,
            CredentialStorage::Memory,
        )
        .await
        .unwrap();
        std::fs::write(
            &project,
            r#"{"mcpServers":{"broken":{"type":"http","command":"x"}}}"#,
        )
        .unwrap();
        assert!(
            runtime
                .reload_config()
                .await
                .unwrap_err()
                .contains("invalid MCP config")
        );
        assert_eq!(
            runtime.inner.servers.read().await["shared"].description,
            "project"
        );

        std::fs::remove_file(&project).unwrap();
        runtime.reload_config().await.unwrap();
        assert_eq!(
            runtime.inner.servers.read().await["shared"].description,
            "configured"
        );
    }

    #[tokio::test]
    async fn shadowed_source_edits_do_not_reconnect_effectively_unchanged_servers() {
        let directory = tempfile::tempdir().unwrap();
        let configured = directory.path().join("configured.json");
        let project = directory.path().join(".mcp.json");
        std::fs::write(
            &configured,
            r#"{"mcpServers":{"shared":{"command":"lower-one"}}}"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"{"mcpServers":{"shared":{"command":"winner"}}}"#,
        )
        .unwrap();
        let runtime = super::connect(
            vec![
                ConfigSource::required(configured.clone()),
                ConfigSource::optional_project(project, directory.path().to_path_buf()),
            ],
            &[],
            true,
            CredentialStorage::Memory,
        )
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        runtime
            .inner
            .servers
            .write()
            .await
            .get_mut("shared")
            .unwrap()
            .status = ServerStatus::Connected;
        let fingerprint = runtime.inner.servers.read().await["shared"]
            .fingerprint
            .clone();

        std::fs::write(
            &configured,
            r#"{"mcpServers":{"shared":{"command":"lower-two"}}}"#,
        )
        .unwrap();
        runtime.reload_config().await.unwrap();
        let record = runtime.inner.servers.read().await["shared"].clone();
        assert_eq!(record.fingerprint, fingerprint);
        assert!(matches!(record.status, ServerStatus::Connected));
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
    async fn removed_server_operation_gate_is_reclaimed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(&path, r#"{"mcpServers":{"removed":{"command":"unused"}}}"#).unwrap();
        let runtime = super::connect(Some(&path), &[], true, CredentialStorage::Memory)
            .await
            .unwrap();
        drop(runtime.operation_gate("removed").await);
        assert!(
            runtime
                .inner
                .operations
                .lock()
                .await
                .contains_key("removed")
        );

        std::fs::write(&path, r#"{"mcpServers":{}}"#).unwrap();
        runtime.reload_config().await.unwrap();
        assert!(
            !runtime
                .inner
                .operations
                .lock()
                .await
                .contains_key("removed")
        );
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
        let runtime = super::connect(None::<&Path>, &[plugin], true, CredentialStorage::Memory)
            .await
            .unwrap();
        assert!(runtime.inner.servers.read().await.contains_key("remote"));
    }

    #[tokio::test]
    async fn live_plugins_add_change_fail_closed_and_remove_through_search() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let package = directory.path().join("plugin");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("plugin.json"),
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"live-plugin"}"#,
        )
        .unwrap();
        std::fs::write(&config, "").unwrap();
        let plugins = PluginRuntime::new(
            config.clone(),
            directory.path().to_path_buf(),
            directory.path().join("cache"),
            directory.path().join("data"),
            ResolvedPlugins::default(),
        );
        let runtime = super::connect_dynamic(
            None::<&Path>,
            plugins.clone(),
            true,
            CredentialStorage::Memory,
        )
        .await
        .unwrap();
        assert!(runtime.inner.servers.read().await.is_empty());

        let write_mcp = |name: &str| {
            std::fs::write(
                package.join("mcp.json"),
                format!(
                    r#"{{"$schema":"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json","mcpServers":{{"{name}":{{"type":"stdio","command":"kit-test-missing-plugin-command"}}}}}}"#
                ),
            )
            .unwrap();
        };
        write_mcp("first");
        std::fs::write(
            &config,
            format!(
                "[plugins.live]\nsource = 'path'\npath = '{}'\n",
                package.display()
            ),
        )
        .unwrap();
        runtime.search("mcp").await.unwrap();
        assert!(matches!(
            runtime.inner.servers.read().await["first"].status,
            ServerStatus::Error(_)
        ));

        write_mcp("second");
        runtime.search("mcp").await.unwrap();
        let servers = runtime.inner.servers.read().await;
        assert!(!servers.contains_key("first"));
        assert!(matches!(servers["second"].status, ServerStatus::Error(_)));
        drop(servers);

        std::fs::write(&config, "[plugins.live\n").unwrap();
        assert!(runtime.search("mcp").await.is_err());
        assert!(runtime.inner.servers.read().await.contains_key("second"));
        assert_eq!(plugins.snapshot().mcp_plugins.len(), 1);

        std::fs::write(&config, "").unwrap();
        runtime.search("mcp").await.unwrap();
        assert!(runtime.inner.servers.read().await.is_empty());
        assert!(plugins.snapshot().mcp_plugins.is_empty());
    }

    #[tokio::test]
    async fn live_plugin_baseline_respects_and_recovers_from_explicit_override() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let explicit = directory.path().join("mcp.json");
        let package = directory.path().join("plugin");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("plugin.json"),
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"override-plugin"}"#,
        )
        .unwrap();
        std::fs::write(&config, "").unwrap();
        std::fs::write(
            &explicit,
            r#"{"mcpServers":{"shared":{"command":"explicit","description":"Explicit"}}}"#,
        )
        .unwrap();
        let plugins = PluginRuntime::new(
            config.clone(),
            directory.path().to_path_buf(),
            directory.path().join("cache"),
            directory.path().join("data"),
            ResolvedPlugins::default(),
        );
        let runtime =
            super::connect_dynamic(Some(&explicit), plugins, true, CredentialStorage::Memory)
                .await
                .unwrap();

        std::fs::write(
            package.join("mcp.json"),
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json","mcpServers":{"shared":{"type":"streamable-http","url":"https://example.com/latest"}}}"#,
        )
        .unwrap();
        std::fs::write(
            &config,
            format!(
                "[plugins.override]\nsource = 'path'\npath = '{}'\n",
                package.display()
            ),
        )
        .unwrap();
        runtime.reload_config().await.unwrap();
        assert_eq!(
            runtime.inner.servers.read().await["shared"].description,
            "Explicit"
        );
        assert!(!runtime.inner.servers.read().await["shared"].plugin_owned);

        let invocation = runtime.acquire_invocation("mcp_shared_read").await.unwrap();
        std::fs::write(
            package.join("mcp.json"),
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json","mcpServers":{"shared":{"type":"streamable-http","url":"https://example.com/newest"}}}"#,
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), runtime.reload_config())
            .await
            .expect("explicit MCP invocation blocked plugin generation publication")
            .unwrap();
        drop(invocation);

        std::fs::write(&explicit, r#"{"mcpServers":{}}"#).unwrap();
        runtime.reload_config().await.unwrap();
        let record = runtime.inner.servers.read().await["shared"].clone();
        assert_eq!(record.description, "override-plugin plugin MCP server");
        assert_eq!(record.url.as_deref(), Some("https://example.com/newest"));
        assert!(record.plugin_owned);
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
        let results = runtime.search("linear").await.unwrap();
        assert_eq!(results["servers"][0]["name"], "linear");
        assert_eq!(results["servers"][0]["status"], "error");
        assert_eq!(results["servers"][0]["available_tool_count"], Value::Null);
        assert_eq!(results["servers"][0]["tools"], json!([]));
        assert_eq!(
            runtime.search("calendar").await.unwrap(),
            json!({"servers":[],"total_matched":0,"total_returned":0,"truncated":false})
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

        let results = runtime.search("linear").await.unwrap();
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
        let (address, server) = bearer_challenge_server(1).await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp.json");
        std::fs::write(
            &path,
            format!(r#"{{"mcpServers":{{"remote":{{"url":"http://{address}/mcp"}}}}}}"#),
        )
        .unwrap();

        let runtime = super::connect(Some(&path), &[], false, CredentialStorage::Memory)
            .await
            .expect("an auth challenge should not fail startup");
        // Eager background initialization contacts the server and settles
        // before any tool_search is issued.
        tokio::time::timeout(std::time::Duration::from_secs(5), server)
            .await
            .expect("MCP server was not contacted by eager startup initialization")
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if matches!(
                    runtime.inner.servers.read().await["remote"].status,
                    ServerStatus::AuthenticationRequired
                ) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("eager initialization did not settle the server status");
        assert!(
            runtime.search("calendar").await.unwrap()["servers"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let results = runtime.search("remote").await.unwrap();
        assert_eq!(results["servers"][0]["name"], "remote");
        assert_eq!(results["servers"][0]["status"], "authentication_required");
        assert_eq!(results["servers"][0]["available_tool_count"], Value::Null);
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
            None,
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
        let results = runtime.search("MCP").await.unwrap();
        assert_eq!(
            results,
            runtime.search("mcp").await.unwrap(),
            "the whole-query server listing is case-insensitive"
        );
        let broken = &results["servers"][0];
        assert_eq!(broken["name"], "broken");
        assert_eq!(broken["status"], "error");
        assert_eq!(broken["available_tool_count"], Value::Null);
        assert!(broken.get("tools").is_none(), "the listing is compact");
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
