use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{Client, ConnectTo, ConnectionTo, Handled};
use agentkit_acp::{
    AcpClientHandle, AcpClientMessage, AcpIntegration, AcpRuntimeError, AcpSessionBinding,
    AgentAuthCapabilities, AudioContent, AuthMethod, AuthMethodTerminal, AuthenticateRequest,
    AuthenticateResponse, AutoDenyResolver, AvailableCommand, AvailableCommandsUpdate,
    BlobResourceContents, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    ContentBlock, ContentChunk, EmbeddedResource, EmbeddedResourceResource, ForkSessionRequest,
    ForkSessionResponse, ImageContent, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, LogoutCapabilities,
    LogoutRequest, LogoutResponse, NewSessionRequest, NewSessionResponse, Notice, NoticeSeverity,
    PromptCapabilities, PromptRequest, PromptResponse, ResourceLink,
    SessionAdditionalDirectoriesCapabilities, SessionCapabilities, SessionCloseCapabilities,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectGroup,
    SessionConfigSelectOption, SessionForkCapabilities, SessionInfo, SessionListCapabilities,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, StopReason, TextContent, TextResourceContents, ToolCallStatus,
    ToolCallUpdateFields,
};
use agentkit_core::{
    CancellationController, DataRef, FinishReason, Item, ItemKind, MediaPart, MetadataMap,
    Modality, Part, SessionId as AgentkitSessionId, ToolOutput,
};
use agentkit_loop::{
    AgentEvent, LoopDriver, LoopError, LoopInterrupt, LoopObserver, LoopStep, ModelSession,
    ObservedEvent,
};
use agentkit_task_manager::{TaskEvent, TaskManagerHandle};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::{AbortHandle, JoinSet},
    time::timeout,
};

mod skill_catalog;
pub mod v2;

use crate::{
    provider::{
        ModelGroup, ModelSelection, ReasoningEffort, SelectableAdapter, authentication_method_id,
        model_catalog,
    },
    runtime::{AcpDriverContext, AcpForkState, BackgroundJobs, DetachRegistration, Runtime},
};

const MODEL_CONFIG_ID: &str = "model";
const REASONING_EFFORT_CONFIG_ID: &str = "reasoning_effort";
const SESSION_LIST_PAGE_SIZE: usize = 100;
const FORK_PARENT_ID_META: &str = "kit.subagent.parent_id";
const FORK_PARENT_NAME_META: &str = "kit.subagent.parent_name";
const AUTH_REQUIRED_KIND: &str = "authentication_required";

pub(crate) struct AuthenticationRequiredData<'a> {
    pub(crate) method_id: &'a str,
    pub(crate) detail: &'a str,
}

impl<'a> AuthenticationRequiredData<'a> {
    pub(crate) fn new(method_id: &'a str, detail: &'a str) -> Self {
        Self { method_id, detail }
    }

    pub(crate) fn from_value(value: &'a serde_json::Value) -> Option<Self> {
        let data = value.as_object()?;
        (data.get("kind")?.as_str()? == AUTH_REQUIRED_KIND).then_some(Self {
            method_id: data.get("methodId")?.as_str()?,
            detail: data.get("detail")?.as_str()?,
        })
    }

    pub(crate) fn into_value(self) -> serde_json::Value {
        serde_json::json!({
            "kind": AUTH_REQUIRED_KIND,
            "methodId": self.method_id,
            "detail": self.detail,
        })
    }
}

pub(crate) struct TerminalAuthMethodSpec {
    pub(crate) method_id: &'static str,
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
}

const TERMINAL_AUTH_METHODS: &[TerminalAuthMethodSpec] = &[
    TerminalAuthMethodSpec {
        method_id: "openai",
        name: "Sign in with ChatGPT",
        description: "Authenticate Kit with a ChatGPT subscription",
    },
    TerminalAuthMethodSpec {
        method_id: "openrouter",
        name: "Sign in with OpenRouter",
        description: "Authenticate Kit with OpenRouter",
    },
    TerminalAuthMethodSpec {
        method_id: "speakeasy",
        name: "Sign in with Speakeasy",
        description: "Authenticate Kit with Speakeasy",
    },
];

pub(crate) fn terminal_auth_method_specs() -> &'static [TerminalAuthMethodSpec] {
    TERMINAL_AUTH_METHODS
}

fn terminal_auth_methods(
    capabilities: &agentkit_acp::ClientCapabilities,
    persistent_credentials: bool,
) -> Vec<AuthMethod> {
    let supports_terminal_auth = capabilities.auth.terminal
        || capabilities
            .meta
            .as_ref()
            .and_then(|meta| meta.get("terminal-auth"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
    if !supports_terminal_auth || !persistent_credentials {
        return Vec::new();
    }

    terminal_auth_method_specs()
        .iter()
        .map(|method| {
            AuthMethod::Terminal(
                AuthMethodTerminal::new(method.method_id, method.name)
                    .description(method.description)
                    .args(vec![
                        "--terminal-auth-login".into(),
                        method.method_id.into(),
                    ]),
            )
        })
        .collect()
}

fn available_commands_update(session_id: agentkit_acp::SessionId) -> SessionNotification {
    SessionNotification::new(
        session_id,
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
            AvailableCommand::new("compact", "Compact the session context"),
        ])),
    )
}

fn transcript_replay(
    session_id: &agentkit_acp::SessionId,
    transcript: &[Item],
) -> Vec<SessionNotification> {
    let mut replay = Vec::new();
    for item in transcript {
        for part in &item.parts {
            let update = match item.kind {
                ItemKind::User => user_replay_content(part).map(SessionUpdate::UserMessageChunk),
                ItemKind::Assistant => assistant_replay_update(part),
                ItemKind::Tool => tool_replay_update(part),
                ItemKind::Developer if crate::compaction::is_compaction_summary(item) => {
                    assistant_replay_update(part)
                }
                ItemKind::System
                | ItemKind::Developer
                | ItemKind::Context
                | ItemKind::Notification => None,
            };
            if let Some(update) = update {
                replay.push(SessionNotification::new(session_id.clone(), update));
            }
        }
    }
    replay
}

pub(super) fn user_replay_content(part: &Part) -> Option<ContentChunk> {
    let content = match part {
        Part::Text(text) => ContentBlock::Text(TextContent::new(text.text.clone())),
        Part::Media(media) => media_replay_content(media),
        Part::File(file) => {
            data_ref_replay_content(file.name.as_deref(), file.mime_type.as_deref(), &file.data)
        }
        Part::Structured(_)
        | Part::Reasoning(_)
        | Part::ToolCall(_)
        | Part::ToolResult(_)
        | Part::Custom(_) => return None,
    };
    Some(ContentChunk::new(content))
}

fn assistant_replay_update(part: &Part) -> Option<SessionUpdate> {
    match part {
        Part::Text(text) => Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(text.text.clone())),
        ))),
        Part::Reasoning(reasoning) => reasoning.summary.as_ref().map(|summary| {
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(summary.clone()),
            )))
        }),
        Part::ToolCall(call) => Some(SessionUpdate::ToolCall(
            agentkit_acp::ToolCall::new(
                agentkit_acp::ToolCallId::new(call.id.to_string()),
                call.name.clone(),
            )
            .status(ToolCallStatus::Pending)
            .raw_input(call.input.clone()),
        )),
        Part::Media(_)
        | Part::File(_)
        | Part::Structured(_)
        | Part::ToolResult(_)
        | Part::Custom(_) => None,
    }
}

fn tool_replay_update(part: &Part) -> Option<SessionUpdate> {
    let Part::ToolResult(result) = part else {
        return None;
    };
    let status = if result.is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    };
    Some(SessionUpdate::ToolCallUpdate(
        agentkit_acp::ToolCallUpdate::new(
            agentkit_acp::ToolCallId::new(result.call_id.to_string()),
            ToolCallUpdateFields::new()
                .status(status)
                .raw_output(tool_output_raw(&result.output)),
        ),
    ))
}

pub(super) fn tool_output_raw(output: &ToolOutput) -> Option<serde_json::Value> {
    match output {
        ToolOutput::Text(text) => Some(json!({ "text": text })),
        ToolOutput::Structured(value) => Some(value.clone()),
        ToolOutput::Parts(parts) => serde_json::to_value(parts).ok(),
        ToolOutput::Files(files) => serde_json::to_value(files).ok(),
    }
}

fn media_replay_content(media: &MediaPart) -> ContentBlock {
    let payload = data_ref_base64_payload(&media.data);
    match (media.modality, payload) {
        (Modality::Image, Some(payload)) => {
            ContentBlock::Image(ImageContent::new(payload, media.mime_type.clone()))
        }
        (Modality::Audio, Some(payload)) => {
            ContentBlock::Audio(AudioContent::new(payload, media.mime_type.clone()))
        }
        (Modality::Image | Modality::Audio | Modality::Video | Modality::Binary, _) => {
            data_ref_replay_content(None, Some(&media.mime_type), &media.data)
        }
    }
}

fn data_ref_replay_content(
    name: Option<&str>,
    mime_type: Option<&str>,
    data: &DataRef,
) -> ContentBlock {
    match data {
        DataRef::Uri(uri) => {
            let mut link = ResourceLink::new(name.unwrap_or(uri), uri.clone());
            if let Some(mime_type) = mime_type {
                link = link.mime_type(mime_type.to_string());
            }
            ContentBlock::ResourceLink(link)
        }
        DataRef::Handle(handle) => {
            let uri = format!("artifact://{handle}");
            let mut link = ResourceLink::new(name.unwrap_or(&uri), uri.clone());
            if let Some(mime_type) = mime_type {
                link = link.mime_type(mime_type.to_string());
            }
            ContentBlock::ResourceLink(link)
        }
        DataRef::InlineText(text) if mime_type.is_none_or(|mime| mime.starts_with("text/")) => {
            let mut resource = TextResourceContents::new(
                text.clone(),
                format!("agentkit://session-replay/{}", name.unwrap_or("content")),
            );
            if let Some(mime_type) = mime_type {
                resource = resource.mime_type(mime_type.to_string());
            }
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(resource),
            ))
        }
        _ => {
            let mut resource = BlobResourceContents::new(
                data_ref_base64_payload(data).unwrap_or_default(),
                format!("agentkit://session-replay/{}", name.unwrap_or("content")),
            );
            if let Some(mime_type) = mime_type {
                resource = resource.mime_type(mime_type.to_string());
            }
            ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::BlobResourceContents(resource),
            ))
        }
    }
}

fn data_ref_base64_payload(data: &DataRef) -> Option<String> {
    match data {
        DataRef::InlineText(text) => {
            Some(data_url_base64_payload(text).unwrap_or_else(|| BASE64.encode(text.as_bytes())))
        }
        DataRef::InlineBytes(bytes) => Some(BASE64.encode(bytes)),
        DataRef::Uri(uri) => data_url_base64_payload(uri),
        DataRef::Handle(_) => None,
    }
}

fn data_url_base64_payload(text: &str) -> Option<String> {
    let data_url = text.strip_prefix("data:")?;
    let (metadata, payload) = data_url.split_once(',')?;
    if metadata
        .split(';')
        .any(|segment| segment.eq_ignore_ascii_case("base64"))
    {
        Some(payload.to_string())
    } else {
        percent_decode(payload).map(|decoded| BASE64.encode(decoded.as_bytes()))
    }
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = *bytes.get(i + 1)?;
                let lo = *bytes.get(i + 2)?;
                decoded.push((hex_value(hi)? << 4) | hex_value(lo)?);
                i += 3;
            }
            byte => {
                decoded.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Kit-private ACP extension used by the bundled TUI to stop one detached call.
#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "kit/background/cancel", response = CancelBackgroundResponse)]
pub(crate) struct CancelBackgroundRequest {
    pub session_id: agentkit_acp::SessionId,
    pub call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
pub(crate) struct CancelBackgroundResponse {
    pub cancelled: bool,
}

/// Kit-private ACP extension used by the bundled TUI to detach one running compose call.
#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "kit/compose/detach", response = DetachComposeResponse)]
pub(crate) struct DetachComposeRequest {
    pub session_id: agentkit_acp::SessionId,
    pub call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
pub(crate) struct DetachComposeResponse {
    pub detached: bool,
}

/// Kit-private ACP extension used by clients for server-owned workspace search.
#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "kit/files/search", response = FileSearchResponse)]
pub(crate) struct FileSearchRequest {
    pub query: String,
    pub activation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
pub(crate) struct FileSearchResponse {
    pub matches: Vec<crate::file_search::FileMatch>,
}

/// Kit-private ACP notification that keeps the bundled TUI synchronized with
/// turns started autonomously by background task results.
#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcNotification)]
#[notification(method = "kit/turn/state")]
pub(crate) struct TurnStateNotification {
    pub session_id: agentkit_acp::SessionId,
    pub turn_id: u64,
    pub active: bool,
    pub error: Option<String>,
}

fn sdk_error(error: AcpRuntimeError) -> agent_client_protocol::Error {
    let detail = error.to_string();
    match authentication_method_id(&detail) {
        Some(method_id) => agent_client_protocol::Error::auth_required()
            .data(AuthenticationRequiredData::new(method_id, &detail).into_value()),
        None => agent_client_protocol::util::internal_error(detail),
    }
}

#[derive(Debug)]
enum ListSessionsError {
    InvalidCursor,
    Runtime(AcpRuntimeError),
}

fn list_sessions_error(error: ListSessionsError) -> agent_client_protocol::Error {
    match error {
        ListSessionsError::InvalidCursor => {
            agent_client_protocol::Error::invalid_params().data("invalid session list cursor")
        }
        ListSessionsError::Runtime(error) => sdk_error(error),
    }
}

enum Command {
    Prompt {
        request: PromptRequest,
        reply: oneshot::Sender<Result<PromptResponse, AcpRuntimeError>>,
    },
    Cancel,
    SetConfig {
        request: SetSessionConfigOptionRequest,
        reply: oneshot::Sender<Result<SetSessionConfigOptionResponse, AcpRuntimeError>>,
    },
    Fork {
        parent_context: Option<(String, String)>,
        reply: oneshot::Sender<Result<AcpForkState, AcpRuntimeError>>,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
}

struct SessionHandle {
    token: u64,
    commands: mpsc::Sender<Command>,
    background_jobs: BackgroundJobs,
    structured_completion: bool,
    tasks: TaskManagerHandle,
}

#[derive(Clone)]
struct RegisteredSession {
    token: u64,
    session_id: agentkit_acp::SessionId,
    integration: Arc<AcpIntegration>,
    background_jobs: BackgroundJobs,
    tasks: TaskManagerHandle,
    commands: mpsc::WeakSender<Command>,
    actor: AbortHandle,
    completed: watch::Receiver<bool>,
}

type CloseV2Session =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + Sync + 'static>;

#[derive(Clone)]
struct RegisteredV2Session {
    token: u64,
    interrupt: Arc<dyn Fn() + Send + Sync>,
    close: CloseV2Session,
    actor: AbortHandle,
    completed: watch::Receiver<bool>,
}

struct RegistryState {
    accepting: bool,
    permanently_closed: bool,
    generation: u64,
    sessions: HashMap<u64, RegisteredSession>,
    v2_sessions: HashMap<u64, RegisteredV2Session>,
}

struct SessionRegistryInner {
    next_token: AtomicU64,
    lifecycle: tokio::sync::Mutex<()>,
    state: Mutex<RegistryState>,
}

/// Coordinates shutdown across stdio and all connection-scoped HTTP ACP components.
#[derive(Clone)]
pub struct SessionRegistry {
    inner: Arc<SessionRegistryInner>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SessionRegistryInner {
                next_token: AtomicU64::new(1),
                lifecycle: tokio::sync::Mutex::new(()),
                state: Mutex::new(RegistryState {
                    accepting: true,
                    permanently_closed: false,
                    generation: 0,
                    sessions: HashMap::new(),
                    v2_sessions: HashMap::new(),
                }),
            }),
        }
    }

    pub(super) fn next_token(&self) -> u64 {
        self.inner.next_token.fetch_add(1, Ordering::Relaxed)
    }

    fn begin_attachment(&self) -> Result<u64, ()> {
        let state = self
            .inner
            .state
            .lock()
            .expect("ACP session registry poisoned");
        state.accepting.then_some(state.generation).ok_or(())
    }

    fn register(&self, admission: u64, session: RegisteredSession) -> Result<(), ()> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("ACP session registry poisoned");
        if !state.accepting || state.generation != admission {
            return Err(());
        }
        state.sessions.insert(session.token, session);
        Ok(())
    }

    pub(super) fn register_v2(
        &self,
        admission: u64,
        token: u64,
        interrupt: Arc<dyn Fn() + Send + Sync>,
        close: CloseV2Session,
        actor: AbortHandle,
        completed: watch::Receiver<bool>,
    ) -> Result<(), ()> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("ACP session registry poisoned");
        if !state.accepting || state.generation != admission {
            return Err(());
        }
        state.v2_sessions.insert(
            token,
            RegisteredV2Session {
                token,
                interrupt,
                close,
                actor,
                completed,
            },
        );
        Ok(())
    }

    pub(super) fn remove(&self, token: u64) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("ACP session registry poisoned");
        state.sessions.remove(&token);
        state.v2_sessions.remove(&token);
    }

    fn close_gate_and_snapshot(
        &self,
        permanently: bool,
    ) -> (Vec<RegisteredSession>, Vec<RegisteredV2Session>) {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("ACP session registry poisoned");
        state.accepting = false;
        state.permanently_closed |= permanently;
        state.generation = state.generation.wrapping_add(1);
        (
            state.sessions.values().cloned().collect(),
            state.v2_sessions.values().cloned().collect(),
        )
    }

    pub async fn shutdown(&self) {
        self.close_sessions_with_timeout(Duration::from_secs(5), false)
            .await;
    }

    pub(super) async fn reset_authentication(&self) -> bool {
        self.close_sessions_with_timeout(Duration::from_secs(5), true)
            .await
    }

    #[cfg(test)]
    async fn shutdown_with_timeout(&self, limit: Duration) {
        self.close_sessions_with_timeout(limit, false).await;
    }

    async fn close_sessions_with_timeout(&self, limit: Duration, reopen: bool) -> bool {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let (sessions, v2_sessions) = self.close_gate_and_snapshot(!reopen);
        for session in &sessions {
            session.background_jobs.cancel_all();
            let _ = session.integration.interrupt_session(&session.session_id);
        }
        for session in &v2_sessions {
            (session.interrupt)();
        }

        let mut closing = JoinSet::new();
        for mut session in sessions.iter().cloned() {
            let registry = self.clone();
            closing.spawn(async move {
                cancel_background_jobs(&session.tasks, &session.background_jobs).await;
                if let Some(commands) = session.commands.upgrade() {
                    let (reply, acknowledged) = oneshot::channel();
                    if commands.send(Command::Close { reply }).await.is_ok() {
                        let _ = acknowledged.await;
                    }
                }
                if !*session.completed.borrow() {
                    let _ = session.completed.changed().await;
                }
                registry.remove(session.token);
            });
        }

        for mut session in v2_sessions.iter().cloned() {
            let registry = self.clone();
            closing.spawn(async move {
                (session.close)().await;
                if !*session.completed.borrow() {
                    let _ = session.completed.changed().await;
                }
                registry.remove(session.token);
            });
        }

        let timed_out = timeout(limit, async {
            while closing.join_next().await.is_some() {}
        })
        .await
        .is_err();
        let teardown_complete = if timed_out {
            closing.abort_all();
            for session in &sessions {
                if !*session.completed.borrow() {
                    session.actor.abort();
                }
            }
            for session in &v2_sessions {
                if !*session.completed.borrow() {
                    session.actor.abort();
                }
            }
            while closing.join_next().await.is_some() {}
            timeout(limit, async {
                for mut session in sessions.iter().cloned() {
                    if !*session.completed.borrow() {
                        let _ = session.completed.changed().await;
                    }
                }
                for mut session in v2_sessions.iter().cloned() {
                    if !*session.completed.borrow() {
                        let _ = session.completed.changed().await;
                    }
                }
            })
            .await
            .is_ok()
        } else {
            true
        };
        for session in &sessions {
            self.remove(session.token);
        }
        for session in &v2_sessions {
            self.remove(session.token);
        }
        if !teardown_complete {
            self.inner
                .state
                .lock()
                .expect("ACP session registry poisoned")
                .permanently_closed = true;
        }
        let mut reopened = false;
        if reopen && teardown_complete {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("ACP session registry poisoned");
            state.accepting = !state.permanently_closed;
            reopened = state.accepting;
        }
        teardown_complete && (!reopen || reopened)
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

async fn logout_authentication(
    runtime: Arc<Runtime>,
    registry: &SessionRegistry,
) -> Result<(), AcpRuntimeError> {
    let logout = match tokio::task::spawn_blocking(move || runtime.logout_authentication()).await {
        Ok(Err(error)) if !error.credential_state_may_have_changed() => {
            return Err(AcpRuntimeError::Loop(error.to_string()));
        }
        Ok(result) => result.map_err(|error| error.to_string()),
        Err(error) => Err(format!("authentication logout task failed: {error}")),
    };
    let reset = registry.reset_authentication().await;

    match (logout, reset) {
        (Ok(()), true) => Ok(()),
        (Err(error), true) => Err(AcpRuntimeError::Loop(format!(
            "{error}; active ACP sessions were reset because credential state may have changed"
        ))),
        (Ok(()), false) => Err(AcpRuntimeError::Loop(
            "authentication logout could not finish session teardown".into(),
        )),
        (Err(error), false) => Err(AcpRuntimeError::Loop(format!(
            "{error}; authentication logout could not finish session teardown"
        ))),
    }
}

struct AttachedSession {
    session_id: agentkit_acp::SessionId,
    config_options: Vec<SessionConfigOption>,
    canonical_transcript: Vec<Item>,
    activation: oneshot::Sender<()>,
    pending_fork_creation: Option<crate::session::SessionObserver>,
}

struct PreparedLoad {
    response: LoadSessionResponse,
    replay: Vec<SessionNotification>,
    activation: oneshot::Sender<()>,
}

struct PreparedFork {
    response: ForkSessionResponse,
    activation: oneshot::Sender<()>,
    creation: crate::session::SessionObserver,
}

/// Owns an ACP integration binding for an in-flight request or live actor.
/// Dropping either owner must release the durable identity.
struct SessionBindingGuard {
    integration: Arc<AcpIntegration>,
    session_id: agentkit_acp::SessionId,
}

#[derive(Clone)]
struct ResponseInterruptionNoticeObserver {
    inner: AcpIntegration,
    client: AcpClientHandle,
    session_id: agentkit_acp::SessionId,
}

impl ResponseInterruptionNoticeObserver {
    fn new(
        inner: AcpIntegration,
        client: AcpClientHandle,
        session_id: agentkit_acp::SessionId,
    ) -> Self {
        Self {
            inner,
            client,
            session_id,
        }
    }
}

impl LoopObserver for ResponseInterruptionNoticeObserver {
    fn handle_event(&self, event: ObservedEvent) {
        if matches!(&event.event, AgentEvent::ResponseAttemptSuperseded) {
            let notification = SessionNotification::new(
                self.session_id.clone(),
                SessionUpdate::Notice(Notice::new(
                    NoticeSeverity::Warning,
                    "Response interrupted; replacement follows",
                )),
            );
            if let Err(error) = self.client.notify_session(notification) {
                tracing::debug!(%error, "failed to queue ACP v1 interruption notice");
            }
            return;
        }
        self.inner.handle_event(event);
    }
}

impl SessionBindingGuard {
    fn new(integration: Arc<AcpIntegration>, session_id: agentkit_acp::SessionId) -> Self {
        Self {
            integration,
            session_id,
        }
    }
}

impl Drop for SessionBindingGuard {
    fn drop(&mut self) {
        let _ = self.integration.unbind_session(&self.session_id);
    }
}

struct SessionActorGuard {
    server: Weak<Server>,
    registry: SessionRegistry,
    session_id: agentkit_acp::SessionId,
    token: u64,
    completed: watch::Sender<bool>,
}

impl Drop for SessionActorGuard {
    fn drop(&mut self) {
        if let Some(server) = self.server.upgrade() {
            server.remove_session(&self.session_id, self.token);
        }
        self.registry.remove(self.token);
        self.completed.send_replace(true);
    }
}

struct Server {
    runtime: Arc<Runtime>,
    integration: Arc<AcpIntegration>,
    registry: SessionRegistry,
    sessions: Mutex<HashMap<agentkit_acp::SessionId, SessionHandle>>,
    file_search: Arc<Mutex<Option<crate::file_search::WorkspaceFileSearchState>>>,
}

impl Server {
    fn new(runtime: Arc<Runtime>, integration: AcpIntegration, registry: SessionRegistry) -> Self {
        Self {
            runtime,
            integration: Arc::new(integration),
            registry,
            sessions: Mutex::new(HashMap::new()),
            file_search: Arc::new(Mutex::new(None)),
        }
    }

    async fn search_files(&self, request: FileSearchRequest) -> Result<FileSearchResponse, String> {
        crate::file_search::search_workspace(
            Arc::clone(&self.file_search),
            self.runtime.root().to_path_buf(),
            request.query,
            request.activation,
        )
        .await
        .map(|matches| FileSearchResponse { matches })
    }

    fn authenticate(
        &self,
        request: AuthenticateRequest,
    ) -> Result<AuthenticateResponse, agent_client_protocol::Error> {
        let method_id = request.method_id.0.as_ref();
        let detail = if terminal_auth_method_specs()
            .iter()
            .any(|method| method.method_id == method_id)
        {
            "terminal authentication methods must be launched as a separate agent invocation"
        } else {
            "authentication method was not advertised by this agent"
        };
        Err(agent_client_protocol::Error::invalid_params()
            .data(serde_json::json!({ "detail": detail, "methodId": method_id })))
    }

    async fn logout(self: &Arc<Self>) -> Result<LogoutResponse, AcpRuntimeError> {
        logout_authentication(Arc::clone(&self.runtime), &self.registry).await?;
        Ok(LogoutResponse::new())
    }

    fn remove_session(&self, session_id: &agentkit_acp::SessionId, token: u64) {
        let mut sessions = self.sessions.lock().expect("ACP session map poisoned");
        if sessions
            .get(session_id)
            .is_some_and(|session| session.token == token)
        {
            sessions.remove(session_id);
        }
    }

    async fn initialize(&self, request: InitializeRequest) -> InitializeResponse {
        let terminal_authentication = self.runtime.supports_terminal_authentication();
        InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1)
            .agent_capabilities(capabilities(terminal_authentication))
            .auth_methods(terminal_auth_methods(
                &request.client_capabilities,
                terminal_authentication,
            ))
            .agent_info(agentkit_acp::Implementation::new(
                self.integration.name().to_string(),
                self.integration.version().to_string(),
            ))
    }

    async fn new_session(
        self: &Arc<Self>,
        request: NewSessionRequest,
        connection: ConnectionTo<Client>,
    ) -> Result<NewSessionResponse, AcpRuntimeError> {
        // session/new retains the configured-or-generated selection semantics.
        let claim = self.runtime.claim_session()?;
        let attached = self
            .attach_session(
                request.cwd,
                request.additional_directories,
                connection,
                claim,
                None,
            )
            .await?;
        let AttachedSession {
            session_id,
            config_options,
            activation,
            ..
        } = attached;
        let _ = activation.send(());
        Ok(NewSessionResponse::new(session_id).config_options(Some(config_options)))
    }

    async fn list_sessions(
        &self,
        request: ListSessionsRequest,
    ) -> Result<ListSessionsResponse, ListSessionsError> {
        let cwd = self.runtime.root().to_path_buf();
        let offset = request
            .cursor
            .as_deref()
            .map(parse_session_list_cursor)
            .transpose()?
            .unwrap_or(0);
        if request
            .cwd
            .as_ref()
            .is_some_and(|requested| requested != &cwd)
        {
            return Ok(ListSessionsResponse::new(Vec::new()));
        }
        let root = self.runtime.root().to_path_buf();
        let catalog = tokio::task::spawn_blocking(move || crate::session::catalog(&root))
            .await
            .map_err(|error| {
                ListSessionsError::Runtime(AcpRuntimeError::Loop(format!(
                    "session catalog worker failed: {error}"
                )))
            })?
            .map_err(|error| ListSessionsError::Runtime(AcpRuntimeError::Loop(error)))?;
        if offset > catalog.len() {
            return Err(ListSessionsError::InvalidCursor);
        }
        let end = catalog.len().min(offset + SESSION_LIST_PAGE_SIZE);
        let sessions = catalog[offset..end]
            .iter()
            .map(|entry| {
                SessionInfo::new(entry.id.clone(), cwd.clone())
                    .title(entry.title.as_deref().map(str::to_owned))
                    .updated_at(entry.updated_at_rfc3339())
            })
            .collect();
        let mut response = ListSessionsResponse::new(sessions);
        response.next_cursor = (end < catalog.len()).then(|| format!("offset:{end}"));
        Ok(response)
    }

    async fn load_session(
        self: &Arc<Self>,
        request: LoadSessionRequest,
        connection: ConnectionTo<Client>,
    ) -> Result<PreparedLoad, AcpRuntimeError> {
        let claim = self
            .runtime
            .claim_session_load(&request.session_id.to_string())?;
        let attached = self
            .attach_session(
                request.cwd,
                request.additional_directories,
                connection,
                claim,
                None,
            )
            .await?;
        let replay = transcript_replay(&attached.session_id, &attached.canonical_transcript);
        Ok(PreparedLoad {
            response: LoadSessionResponse::new().config_options(Some(attached.config_options)),
            replay,
            activation: attached.activation,
        })
    }

    async fn fork_session(
        self: &Arc<Self>,
        request: ForkSessionRequest,
        connection: ConnectionTo<Client>,
    ) -> Result<PreparedFork, AcpRuntimeError> {
        if !request.mcp_servers.is_empty() {
            return Err(AcpRuntimeError::Loop(
                "Kit does not accept per-session MCP servers".into(),
            ));
        }
        let parent_context = fork_parent_context(request.meta.as_ref());
        let sender = self.sender(&request.session_id).await?;
        let (tx, rx) = oneshot::channel();
        sender
            .send(Command::Fork {
                parent_context,
                reply: tx,
            })
            .await
            .map_err(|_| AcpRuntimeError::SessionNotFound(request.session_id.to_string()))?;
        let forked = rx
            .await
            .map_err(|_| AcpRuntimeError::SessionNotFound(request.session_id.to_string()))??;
        let claim = self.runtime.claim_session_fork()?;
        let attached = self
            .attach_session(
                request.cwd,
                request.additional_directories,
                connection,
                claim,
                Some(forked),
            )
            .await?;
        Ok(PreparedFork {
            response: ForkSessionResponse::new(attached.session_id)
                .config_options(Some(attached.config_options)),
            activation: attached.activation,
            creation: attached
                .pending_fork_creation
                .expect("fork attachment must defer transcript creation"),
        })
    }

    async fn attach_session(
        self: &Arc<Self>,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        connection: ConnectionTo<Client>,
        mut claim: crate::runtime::SessionClaim,
        forked: Option<AcpForkState>,
    ) -> Result<AttachedSession, AcpRuntimeError> {
        let admission = self
            .registry
            .begin_attachment()
            .map_err(|()| AcpRuntimeError::ClientClosed)?;
        // Claim the durable identity before binding ACP so every layer uses
        // the same id and any bind failure releases the selection or reservation.
        let session_id = agentkit_acp::SessionId::new(claim.id());
        let agentkit_session_id = AgentkitSessionId::new(claim.id());
        let cancellation = CancellationController::new();
        let (client, messages) = AcpClientHandle::channel();
        tokio::spawn(drain_client_messages(messages, connection.clone()));
        let (turn_states, turn_state_messages) = mpsc::unbounded_channel();
        tokio::spawn(drain_turn_states(turn_state_messages, connection.clone()));

        let mut metadata = MetadataMap::new();
        metadata.insert("acp.cwd".into(), json!(cwd));
        metadata.insert(
            "acp.additional_directories".into(),
            json!(additional_directories),
        );
        let mcp_events = self.runtime.subscribe_mcp(session_id.to_string());
        let binding =
            AcpSessionBinding::new(session_id.clone(), agentkit_session_id, client.clone())
                .cancellation(cancellation)
                .workspace(cwd.clone(), additional_directories.clone())
                .metadata(metadata);
        let handle = self
            .integration
            .bind_session(binding)
            .map_err(|error| record_acp_runtime_failure(&session_id, "session_bind", error))?;
        let binding = SessionBindingGuard::new(Arc::clone(&self.integration), session_id.clone());
        let observer = ResponseInterruptionNoticeObserver::new(
            self.integration.as_ref().clone(),
            client,
            session_id.clone(),
        );
        let context = AcpDriverContext {
            cwd,
            additional_directories,
            integration: Arc::new(observer),
            cancellation: handle.cancellation_handle(),
            response_attempt_replacement: true,
        };
        let driver = match self
            .runtime
            .start_acp_driver_with_initial(context, &mut claim, forked)
            .await
        {
            Ok(driver) => driver,
            Err(error) => {
                return Err(record_acp_runtime_failure(
                    &session_id,
                    "session_start",
                    error,
                ));
            }
        };
        let current = driver
            .adapter
            .selection()
            .map_err(|error| record_acp_runtime_failure(&session_id, "model_selection", error))?;
        let reasoning_effort = driver
            .adapter
            .reasoning_effort()
            .map_err(|error| record_acp_runtime_failure(&session_id, "reasoning_effort", error))?;
        let catalog = model_catalog(&current).await;
        let config_options = config_options(&current, reasoning_effort, &catalog);
        let background_jobs = driver.background_jobs.clone();
        let tasks = driver.tasks.clone();
        let structured_completion = driver.structured_completion;
        let canonical_transcript = driver.canonical_transcript;
        let skill_catalog = skill_catalog::SkillCatalogMonitor::new(&driver.skills)
            .map_err(|error| record_acp_runtime_failure(&session_id, "skill_catalog", error))?;
        let (tx, rx) = mpsc::channel(8);
        let actor = SessionActor {
            session_id: session_id.clone(),
            runtime: Arc::clone(&self.runtime),
            integration: Arc::clone(&self.integration),
            binding,
            driver: driver.driver,
            tasks: driver.tasks,
            background_jobs: background_jobs.clone(),
            structured_completion,
            skill_catalog,
            adapter: driver.adapter,
            catalog,
            commands: rx,
            turn_states,
            mcp_events,
        };
        let token = self.registry.next_token();
        let (activation, activated) = oneshot::channel();
        let (completed, completion) = watch::channel(false);
        let guard = SessionActorGuard {
            server: Arc::downgrade(self),
            registry: self.registry.clone(),
            session_id: session_id.clone(),
            token,
            completed,
        };
        let actor_task = tokio::spawn(async move {
            let _guard = guard;
            if activated.await.is_ok() {
                session_actor(actor).await;
            }
        });
        let registered = RegisteredSession {
            token,
            session_id: session_id.clone(),
            integration: Arc::clone(&self.integration),
            background_jobs: background_jobs.clone(),
            tasks: tasks.clone(),
            commands: tx.downgrade(),
            actor: actor_task.abort_handle(),
            completed: completion,
        };

        // Hold the request-scoped map lock across registration, commit, and publication.
        // Shutdown either closes the gate before this point or snapshots this actor.
        let mut sessions = self.sessions.lock().expect("ACP session map poisoned");
        if self.registry.register(admission, registered).is_err() {
            drop(sessions);
            drop(activation);
            actor_task.abort();
            let _ = actor_task.await;
            return Err(AcpRuntimeError::ClientClosed);
        }
        let pending_fork_creation = if claim.is_fork() {
            Some(claim.defer_fork_commit())
        } else {
            if let Err(error) = claim.commit() {
                self.registry.remove(token);
                drop(sessions);
                drop(activation);
                actor_task.abort();
                let _ = actor_task.await;
                return Err(record_acp_runtime_failure(
                    &session_id,
                    "session_commit",
                    error,
                ));
            }
            None
        };
        crate::events::emit(&crate::events::RuntimeEvent::SessionStarted {
            session_id: session_id.to_string(),
        });
        sessions.insert(
            session_id.clone(),
            SessionHandle {
                token,
                commands: tx,
                background_jobs,
                structured_completion,
                tasks,
            },
        );
        drop(sessions);
        drop(actor_task);
        Ok(AttachedSession {
            session_id,
            config_options,
            canonical_transcript,
            activation,
            pending_fork_creation,
        })
    }

    async fn prompt(&self, request: PromptRequest) -> Result<PromptResponse, AcpRuntimeError> {
        let sender = self.sender(&request.session_id).await?;
        let (tx, rx) = oneshot::channel();
        sender
            .send(Command::Prompt { request, reply: tx })
            .await
            .map_err(|_| AcpRuntimeError::ClientClosed)?;
        rx.await.map_err(|_| AcpRuntimeError::ClientClosed)?
    }

    async fn set_config(
        &self,
        request: SetSessionConfigOptionRequest,
    ) -> Result<SetSessionConfigOptionResponse, AcpRuntimeError> {
        let sender = self.sender(&request.session_id).await?;
        let (tx, rx) = oneshot::channel();
        sender
            .send(Command::SetConfig { request, reply: tx })
            .await
            .map_err(|_| AcpRuntimeError::ClientClosed)?;
        rx.await.map_err(|_| AcpRuntimeError::ClientClosed)?
    }

    async fn cancel(&self, notification: CancelNotification) -> Result<(), AcpRuntimeError> {
        // Interrupt out of band because the actor may currently be inside
        // `driver.next()`. The queued marker preserves command ordering once
        // that call settles.
        let (sender, background_jobs, tasks, structured_completion) = self
            .sessions
            .lock()
            .expect("ACP session map poisoned")
            .get(&notification.session_id)
            .map(|session| {
                (
                    session.commands.clone(),
                    session.background_jobs.clone(),
                    session.tasks.clone(),
                    session.structured_completion,
                )
            })
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(notification.session_id.to_string()))?;
        if structured_completion {
            cancel_background_jobs(&tasks, &background_jobs).await;
        }
        self.integration
            .interrupt_session(&notification.session_id)?;
        sender
            .send(Command::Cancel)
            .await
            .map_err(|_| AcpRuntimeError::ClientClosed)
    }

    async fn close(
        &self,
        request: CloseSessionRequest,
    ) -> Result<CloseSessionResponse, AcpRuntimeError> {
        // Closing uses the same out-of-band interrupt, then waits for the
        // actor to reach and acknowledge the serialized close boundary.
        let session = self
            .sessions
            .lock()
            .expect("ACP session map poisoned")
            .remove(&request.session_id)
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(request.session_id.to_string()))?;
        cancel_background_jobs(&session.tasks, &session.background_jobs).await;
        self.integration.interrupt_session(&request.session_id)?;
        let (tx, rx) = oneshot::channel();
        session
            .commands
            .send(Command::Close { reply: tx })
            .await
            .map_err(|_| AcpRuntimeError::ClientClosed)?;
        rx.await.map_err(|_| AcpRuntimeError::ClientClosed)?;
        self.registry.remove(session.token);
        Ok(CloseSessionResponse::new())
    }

    async fn sender(
        &self,
        session_id: &agentkit_acp::SessionId,
    ) -> Result<mpsc::Sender<Command>, AcpRuntimeError> {
        self.sessions
            .lock()
            .expect("ACP session map poisoned")
            .get(session_id)
            .map(|session| session.commands.clone())
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))
    }

    async fn detach_compose(
        &self,
        request: DetachComposeRequest,
    ) -> Result<DetachComposeResponse, AcpRuntimeError> {
        let (background_jobs, tasks) = self
            .sessions
            .lock()
            .expect("ACP session map poisoned")
            .get(&request.session_id)
            .map(|session| (session.background_jobs.clone(), session.tasks.clone()))
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(request.session_id.to_string()))?;
        Ok(DetachComposeResponse {
            detached: detach_compose_call(&tasks, &background_jobs, &request.call_id).await,
        })
    }

    async fn cancel_background(
        &self,
        request: CancelBackgroundRequest,
    ) -> Result<CancelBackgroundResponse, AcpRuntimeError> {
        let background_jobs = self
            .sessions
            .lock()
            .expect("ACP session map poisoned")
            .get(&request.session_id)
            .map(|session| session.background_jobs.clone())
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(request.session_id.to_string()))?;
        Ok(CancelBackgroundResponse {
            cancelled: background_jobs.cancel(&request.call_id),
        })
    }
}

pub(super) async fn detach_compose_call(
    tasks: &TaskManagerHandle,
    background_jobs: &BackgroundJobs,
    call_id: &str,
) -> bool {
    let Some(task) = tasks.list_running().await.into_iter().find(|task| {
        task.call_id.0 == call_id
            && task.tool_name == agentkit_tool_compose::COMPOSE_TOOL_NAME
            && task.kind == agentkit_task_manager::TaskKind::Foreground
    }) else {
        return false;
    };
    match background_jobs.detach(call_id) {
        Some(DetachRegistration::AlreadyDetached) => true,
        Some(DetachRegistration::Registered) => {
            if tasks.detach(task.id).await.is_err() {
                background_jobs.restore_foreground(call_id);
                return false;
            }
            true
        }
        None => false,
    }
}

struct SessionActor<S: ModelSession> {
    session_id: agentkit_acp::SessionId,
    runtime: Arc<Runtime>,
    integration: Arc<AcpIntegration>,
    binding: SessionBindingGuard,
    driver: LoopDriver<S>,
    tasks: TaskManagerHandle,
    background_jobs: BackgroundJobs,
    structured_completion: bool,
    skill_catalog: skill_catalog::SkillCatalogMonitor,
    adapter: SelectableAdapter,
    catalog: Vec<ModelGroup>,
    commands: mpsc::Receiver<Command>,
    turn_states: mpsc::UnboundedSender<TurnStateNotification>,
    mcp_events: crate::tools::mcp::McpSubscription,
}

async fn session_actor<S: ModelSession>(actor: SessionActor<S>) {
    let SessionActor {
        session_id,
        runtime,
        integration,
        binding,
        mut driver,
        tasks,
        background_jobs,
        structured_completion,
        mut skill_catalog,
        adapter,
        catalog,
        mut commands,
        turn_states,
        mut mcp_events,
    } = actor;
    let mut binding = Some(binding);
    let mut next_autonomous_turn_id = 0_u64;
    loop {
        tokio::select! {
            // A queued cancel or close wins over a simultaneously-ready task
            // completion, preventing autonomous progress past that boundary.
            biased;
            command = commands.recv() => match command {
                Some(Command::Prompt { request, reply }) => {
                    let result = drive_runtime_prompt(
                        &session_id,
                        &runtime,
                        &integration,
                        &mut skill_catalog,
                        &mut driver,
                        request,
                        &tasks,
                        &background_jobs,
                        structured_completion,
                    ).await;
                    let _ = reply.send(result);
                }
                // The server already interrupted the shared controller; this
                // marker only establishes its serialized actor position.
                Some(Command::Cancel) => {}
                Some(Command::SetConfig { request, reply }) => {
                    let result = set_config(&adapter, &catalog, request);
                    let _ = reply.send(result);
                }
                Some(Command::Fork {
                    parent_context,
                    reply,
                }) => {
                    let result = (|| {
                        let mut transcript = driver.snapshot().transcript;
                        crate::transcript::sanitize_forked_transcript(&mut transcript);
                        Ok(AcpForkState {
                            transcript,
                            selection: adapter.selection().map_err(AcpRuntimeError::Loop)?,
                            reasoning_effort: adapter
                                .reasoning_effort()
                                .map_err(AcpRuntimeError::Loop)?,
                            parent_context,
                        })
                    })();
                    let _ = reply.send(result);
                }
                Some(Command::Close { reply }) => {
                    clean_up_session(&session_id, &mut driver, &tasks, &background_jobs).await;
                    // A close acknowledgement means the actor-owned binding is
                    // already gone, so callers can immediately reuse the id.
                    drop(binding.take());
                    let _ = reply.send(());
                    break;
                }
                None => {
                    clean_up_session(&session_id, &mut driver, &tasks, &background_jobs).await;
                    break;
                },
            },
            event = mcp_events.recv() => {
                if let Some(event) = event {
                    let result = driver
                        .submit_input(vec![Item::notification(event.message)])
                        .map_err(|error| AcpRuntimeError::Loop(error.to_string()));
                    let result = match result {
                        Ok(()) => drive_unsolicited(
                            &session_id,
                            &integration,
                            &mut driver,
                            &turn_states,
                            &mut next_autonomous_turn_id,
                        ).await,
                        Err(error) => Err(error),
                    };
                    if let Err(error) = result {
                        eprintln!("autonomous MCP continuation failed for {session_id}: {error}");
                    }
                }
            }
            // Task events remain queued while a prompt is being driven, so a
            // completion cannot be lost in the prompt-to-idle transition.
            event = tasks.next_event() => match event {
                Some(TaskEvent::Completed(snapshot, _)) => {
                    background_jobs.acknowledge_terminal(&snapshot.call_id);
                    if snapshot.kind == agentkit_task_manager::TaskKind::Background {
                        let result = drive_unsolicited(
                            &session_id,
                            &integration,
                            &mut driver,
                            &turn_states,
                            &mut next_autonomous_turn_id,
                        ).await;
                        if let Err(error) = result {
                            eprintln!("autonomous ACP continuation failed for {session_id}: {error}");
                        }
                    }
                }
                Some(TaskEvent::Cancelled(snapshot) | TaskEvent::Failed(snapshot, _)) => {
                    background_jobs.acknowledge_terminal(&snapshot.call_id);
                }
                Some(_) => {}
                None => break,
            }
        }
    }
}

pub(super) fn config_options(
    current: &ModelSelection,
    reasoning_effort: Option<ReasoningEffort>,
    catalog: &[ModelGroup],
) -> Vec<SessionConfigOption> {
    let groups = catalog
        .iter()
        .map(|group| {
            let provider = group.provider.as_str();
            let name = match group.provider {
                crate::ProviderKind::OpenAiSubscription => "OpenAI subscription",
                crate::ProviderKind::OpenRouter => "OpenRouter",
                crate::ProviderKind::Speakeasy => "Speakeasy",
            };
            let options = group
                .models
                .iter()
                .map(|model| {
                    let selection = ModelSelection {
                        provider: group.provider,
                        model: model.clone(),
                    };
                    SessionConfigSelectOption::new(selection.id(), model.clone())
                })
                .collect();
            SessionConfigSelectGroup::new(provider, name, options)
        })
        .collect::<Vec<_>>();
    let effort_options = [
        ("default", "Default"),
        ("low", "Low"),
        ("medium", "Medium"),
        ("high", "High"),
    ]
    .into_iter()
    .map(|(value, name)| SessionConfigSelectOption::new(value, name))
    .collect();
    vec![
        SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current.id(), groups)
            .category(SessionConfigOptionCategory::Model),
        SessionConfigOption::select(
            REASONING_EFFORT_CONFIG_ID,
            "Reasoning effort",
            reasoning_effort.map_or("default", ReasoningEffort::as_str),
            vec![SessionConfigSelectGroup::new(
                "reasoning-effort",
                "Reasoning effort",
                effort_options,
            )],
        )
        .category(SessionConfigOptionCategory::ThoughtLevel),
    ]
}

pub(super) fn set_config(
    adapter: &SelectableAdapter,
    catalog: &[ModelGroup],
    request: SetSessionConfigOptionRequest,
) -> Result<SetSessionConfigOptionResponse, AcpRuntimeError> {
    let config_id = request.config_id.to_string();
    let value = request
        .value
        .as_value_id()
        .ok_or_else(|| AcpRuntimeError::Unsupported("selection requires an id value".into()))?
        .to_string();
    match config_id.as_str() {
        MODEL_CONFIG_ID => {
            let selection = ModelSelection::from_id(&value).map_err(AcpRuntimeError::Loop)?;
            let offered = catalog.iter().any(|group| {
                group.provider == selection.provider && group.models.contains(&selection.model)
            });
            if !offered {
                return Err(AcpRuntimeError::Unsupported(
                    "model is not in the advertised catalog".into(),
                ));
            }
            adapter.select(selection).map_err(AcpRuntimeError::Loop)?;
        }
        REASONING_EFFORT_CONFIG_ID => {
            let effort = ReasoningEffort::from_id(&value).map_err(AcpRuntimeError::Loop)?;
            adapter
                .select_reasoning_effort(effort)
                .map_err(AcpRuntimeError::Loop)?;
        }
        _ => {
            return Err(AcpRuntimeError::Unsupported(
                "unknown session configuration option".into(),
            ));
        }
    }
    let selection = adapter.selection().map_err(AcpRuntimeError::Loop)?;
    let reasoning_effort = adapter.reasoning_effort().map_err(AcpRuntimeError::Loop)?;
    Ok(SetSessionConfigOptionResponse::new(config_options(
        &selection,
        reasoning_effort,
        catalog,
    )))
}

pub(super) async fn settle_background_jobs(
    tasks: &TaskManagerHandle,
    background_jobs: &BackgroundJobs,
) -> Result<bool, AcpRuntimeError> {
    let initial = background_jobs.activity();
    let mut observed_activity = false;
    loop {
        let activity = background_jobs.activity();
        let running = tasks
            .list_running()
            .await
            .into_iter()
            .any(|task| task.kind == agentkit_task_manager::TaskKind::Background);
        let stable = background_jobs.activity();
        if stable.generation != activity.generation {
            observed_activity = true;
            continue;
        }
        observed_activity |= stable.background_started > initial.background_started
            || stable.active
            || running
            || stable.unacknowledged_terminals;
        if !stable.active && !running && !stable.unacknowledged_terminals {
            return Ok(observed_activity);
        }
        tokio::select! {
            biased;
            _ = background_jobs.activity_after(stable.generation) => {}
            event = tasks.next_event(), if running || stable.unacknowledged_terminals => {
                match event {
                    Some(
                        TaskEvent::Completed(snapshot, _)
                        | TaskEvent::Cancelled(snapshot)
                        | TaskEvent::Failed(snapshot, _),
                    ) => {
                        background_jobs.acknowledge_terminal(&snapshot.call_id);
                    }
                    Some(_) => {}
                    None => {
                        return Err(AcpRuntimeError::Loop(
                            "background task event stream closed before quiescence".into(),
                        ));
                    }
                }
            }
        }
    }
}

pub(super) async fn cancel_background_jobs(
    tasks: &TaskManagerHandle,
    background_jobs: &BackgroundJobs,
) {
    background_jobs.cancel_all();
    let _ = timeout(
        Duration::from_millis(100),
        background_jobs.wait_for_quiescence(),
    )
    .await;
    for task in tasks.list_running().await {
        if task.kind == agentkit_task_manager::TaskKind::Background {
            let _ = tasks.cancel(task.id).await;
        }
    }
}

pub(super) async fn clean_up_session<S: ModelSession>(
    session_id: &agentkit_acp::SessionId,
    driver: &mut LoopDriver<S>,
    tasks: &TaskManagerHandle,
    background_jobs: &BackgroundJobs,
) {
    cancel_background_jobs(tasks, background_jobs).await;
    for task in tasks.list_running().await {
        if let Err(error) = tasks.cancel(task.id).await {
            eprintln!("failed to cancel ACP task for {session_id}: {error}");
        }
    }
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        background_jobs.wait_for_quiescence(),
    )
    .await;
    if let Err(error) = driver.cancel_pending_approvals().await {
        eprintln!("failed to cancel ACP approvals for {session_id}: {error}");
    }
}

fn record_acp_runtime_failure(
    session_id: &agentkit_acp::SessionId,
    code: &str,
    error: impl ToString,
) -> AcpRuntimeError {
    let rendered = error.to_string();
    match crate::fatal::record_runtime_error(
        &session_id.to_string(),
        crate::fatal::Surface::Acp,
        code,
    ) {
        Ok(path) => AcpRuntimeError::Loop(format!("{rendered}; fatal log: {}", path.display())),
        Err(log_error) => {
            eprintln!("could not store fatal error log for {session_id}: {log_error}");
            AcpRuntimeError::Loop(rendered)
        }
    }
}

fn record_acp_loop_failure(
    session_id: &agentkit_acp::SessionId,
    error: &LoopError,
) -> AcpRuntimeError {
    let rendered = crate::fatal::render_loop_error(error);
    match crate::fatal::record_loop_error(
        &session_id.to_string(),
        crate::fatal::Surface::Acp,
        error,
    ) {
        Ok(Some(path)) => {
            AcpRuntimeError::Loop(format!("{rendered}; fatal log: {}", path.display()))
        }
        Ok(None) => AcpRuntimeError::Loop(rendered),
        Err(log_error) => {
            eprintln!("could not store fatal error log for {session_id}: {log_error}");
            AcpRuntimeError::Loop(rendered)
        }
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn drive_prompt<S: ModelSession>(
    session_id: &agentkit_acp::SessionId,
    skills: &[agentkit_tool_skills::Skill],
    integration: &AcpIntegration,
    skill_catalog: &mut skill_catalog::SkillCatalogMonitor,
    driver: &mut LoopDriver<S>,
    request: PromptRequest,
    tasks: &TaskManagerHandle,
    background_jobs: &BackgroundJobs,
    structured_completion: bool,
) -> Result<PromptResponse, AcpRuntimeError> {
    if structured_completion {
        let _ = settle_background_jobs(tasks, background_jobs).await?;
    }
    background_jobs.begin_turn();
    let items = integration.input_port().prompt_to_items(&request)?;
    skill_catalog
        .submit(skills, items, |items| driver.submit_input(items))
        .map_err(|error| match error {
            skill_catalog::SubmitError::Catalog(error) => {
                record_acp_runtime_failure(session_id, "skill_catalog", error)
            }
            skill_catalog::SubmitError::Submit(error) => {
                record_acp_loop_failure(session_id, &error)
            }
        })?;
    drive_submitted_prompt(
        session_id,
        integration,
        driver,
        tasks,
        background_jobs,
        structured_completion,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drive_runtime_prompt<S: ModelSession>(
    session_id: &agentkit_acp::SessionId,
    runtime: &Arc<Runtime>,
    integration: &AcpIntegration,
    skill_catalog: &mut skill_catalog::SkillCatalogMonitor,
    driver: &mut LoopDriver<S>,
    request: PromptRequest,
    tasks: &TaskManagerHandle,
    background_jobs: &BackgroundJobs,
    structured_completion: bool,
) -> Result<PromptResponse, AcpRuntimeError> {
    if structured_completion {
        let _ = settle_background_jobs(tasks, background_jobs).await?;
    }
    let current = runtime
        .current_skills()
        .await
        .map_err(AcpRuntimeError::Loop)?;
    background_jobs.begin_turn();
    let items = integration.input_port().prompt_to_items(&request)?;
    skill_catalog
        .submit(&current.skills, items, |items| driver.submit_input(items))
        .map_err(|error| match error {
            skill_catalog::SubmitError::Catalog(error) => {
                record_acp_runtime_failure(session_id, "skill_catalog", error)
            }
            skill_catalog::SubmitError::Submit(error) => {
                record_acp_loop_failure(session_id, &error)
            }
        })?;
    drop(current);
    drive_submitted_prompt(
        session_id,
        integration,
        driver,
        tasks,
        background_jobs,
        structured_completion,
    )
    .await
}

async fn drive_submitted_prompt<S: ModelSession>(
    session_id: &agentkit_acp::SessionId,
    integration: &AcpIntegration,
    driver: &mut LoopDriver<S>,
    tasks: &TaskManagerHandle,
    background_jobs: &BackgroundJobs,
    structured_completion: bool,
) -> Result<PromptResponse, AcpRuntimeError> {
    let response = match drive_until_pause(
        session_id,
        integration,
        driver,
        true,
        structured_completion.then_some((tasks, background_jobs)),
    )
    .await
    {
        Ok(Some(response)) => response,
        Ok(None) => {
            return Err(AcpRuntimeError::Loop(
                "prompt ended without a response".into(),
            ));
        }
        Err(error) => {
            if structured_completion {
                cancel_background_jobs(tasks, background_jobs).await;
                let _ = settle_background_jobs(tasks, background_jobs).await;
            }
            return Err(error);
        }
    };
    if structured_completion && response.stop_reason == StopReason::Cancelled {
        cancel_background_jobs(tasks, background_jobs).await;
        let _ = settle_background_jobs(tasks, background_jobs).await?;
    }
    Ok(response)
}

async fn drive_unsolicited<S: ModelSession>(
    session_id: &agentkit_acp::SessionId,
    integration: &AcpIntegration,
    driver: &mut LoopDriver<S>,
    turn_states: &mpsc::UnboundedSender<TurnStateNotification>,
    next_turn_id: &mut u64,
) -> Result<(), AcpRuntimeError> {
    *next_turn_id = next_turn_id.wrapping_add(1);
    let turn_id = *next_turn_id;
    let _ = turn_states.send(TurnStateNotification {
        session_id: session_id.clone(),
        turn_id,
        active: true,
        error: None,
    });
    let result = drive_autonomous(session_id, integration, driver).await;
    let error = result.as_ref().err().map(ToString::to_string);
    let _ = turn_states.send(TurnStateNotification {
        session_id: session_id.clone(),
        turn_id,
        active: false,
        error,
    });
    result
}

async fn drive_autonomous<S: ModelSession>(
    session_id: &agentkit_acp::SessionId,
    integration: &AcpIntegration,
    driver: &mut LoopDriver<S>,
) -> Result<(), AcpRuntimeError> {
    let _ = drive_until_pause(session_id, integration, driver, false, None).await?;
    Ok(())
}

async fn drive_until_pause<S: ModelSession>(
    session_id: &agentkit_acp::SessionId,
    integration: &AcpIntegration,
    driver: &mut LoopDriver<S>,
    answer_prompt: bool,
    structured: Option<(&TaskManagerHandle, &BackgroundJobs)>,
) -> Result<Option<PromptResponse>, AcpRuntimeError> {
    loop {
        let step = match driver.next().await {
            Ok(step) => step,
            Err(LoopError::Cancelled) => {
                integration.flush_session_updates(session_id).await?;
                return Ok(answer_prompt.then(|| PromptResponse::new(StopReason::Cancelled)));
            }
            Err(error) => return Err(record_acp_loop_failure(session_id, &error)),
        };
        match step {
            LoopStep::Finished(result) => {
                if result.finish_reason == FinishReason::ToolCall {
                    continue;
                }
                integration.flush_session_updates(session_id).await?;
                if !answer_prompt {
                    return Ok(None);
                }
                let reason = agentkit_acp::finish_reason_to_stop_reason(&result.finish_reason)?;
                if let Some((tasks, background_jobs)) = structured
                    && settle_background_jobs(tasks, background_jobs).await?
                {
                    continue;
                }
                return Ok(Some(PromptResponse::new(reason)));
            }
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                integration.flush_session_updates(session_id).await?;
                if answer_prompt
                    && let Some((tasks, background_jobs)) = structured
                    && settle_background_jobs(tasks, background_jobs).await?
                {
                    continue;
                }
                return Ok(answer_prompt.then(|| PromptResponse::new(StopReason::EndTurn)));
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                let decision = integration
                    .resolve_approval(session_id, pending.request.clone())
                    .await?;
                let decision = decision.to_agentkit_decision().ok_or_else(|| {
                    AcpRuntimeError::Unsupported(
                        "patched approval is not supported by Kit ACP".into(),
                    )
                })?;
                match pending.request.call_id {
                    Some(call_id) => driver.resolve_approval_for(call_id, decision),
                    None => driver.resolve_approval(decision),
                }
                .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?;
            }
        }
    }
}

pub async fn serve(runtime: Arc<Runtime>) -> Result<(), AcpRuntimeError> {
    serve_transport(runtime, agent_client_protocol::Stdio::new()).await
}

pub async fn serve_with_registry(
    runtime: Arc<Runtime>,
    registry: SessionRegistry,
) -> Result<(), AcpRuntimeError> {
    component(runtime, registry)?
        .connect_to(agent_client_protocol::Stdio::new())
        .await
        .map_err(|error| AcpRuntimeError::Sdk(error.to_string()))
}

async fn serve_transport(
    runtime: Arc<Runtime>,
    transport: impl ConnectTo<agent_client_protocol::Agent> + 'static,
) -> Result<(), AcpRuntimeError> {
    let registry = SessionRegistry::new();
    let result = component(runtime, registry.clone())?
        .connect_to(transport)
        .await
        .map_err(|error| AcpRuntimeError::Sdk(error.to_string()));
    registry.shutdown().await;
    result
}

pub(crate) fn http_router(runtime: Arc<Runtime>, registry: SessionRegistry) -> axum::Router {
    agent_client_protocol_http::AcpHttpServer::new(move || {
        let v1 = component(Arc::clone(&runtime), registry.clone())
            .expect("Kit's fixed ACP v1 integration must build");
        let v2 = v2::component(Arc::clone(&runtime), registry.clone())
            .expect("Kit's fixed ACP v2 integration must build");
        agent_client_protocol::Agent
            .protocol_router()
            .with_v1(v1)
            .with_v2(v2)
    })
    .with_options(agent_client_protocol_http::ServerOptions {
        health_endpoint: false,
        ..Default::default()
    })
    .into_router()
}

fn component(
    runtime: Arc<Runtime>,
    registry: SessionRegistry,
) -> Result<impl ConnectTo<agent_client_protocol::Client>, AcpRuntimeError> {
    let integration = AcpIntegration::builder()
        .name("kit")
        .approval_resolver(AutoDenyResolver)
        .build()?;
    let state = Arc::new(Server::new(runtime, integration, registry));
    Ok(agent_client_protocol::Agent
        .builder()
        .name("kit")
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: InitializeRequest, responder, _cx| {
                    responder.respond(state.initialize(request).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: AuthenticateRequest, responder, _cx| {
                    responder.respond_with_result(state.authenticate(request))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |_request: LogoutRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    cx.spawn(async move {
                        responder.respond_with_result(state.logout().await.map_err(sdk_error))
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: NewSessionRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    let connection = cx.clone();
                    cx.spawn(async move {
                        let result = state.new_session(request, connection.clone()).await;
                        let notification = result
                            .as_ref()
                            .ok()
                            .map(|response| available_commands_update(response.session_id.clone()));
                        responder.respond_with_result(result.map_err(sdk_error))?;
                        if let Some(notification) = notification {
                            connection.send_notification(notification)?;
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: LoadSessionRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    let connection = cx.clone();
                    let session_id = request.session_id.clone();
                    cx.spawn(async move {
                        match state.load_session(request, connection.clone()).await {
                            Ok(prepared) => {
                                for notification in prepared.replay {
                                    connection.send_notification(notification)?;
                                }
                                responder.respond(prepared.response)?;
                                // Once success is reported, retain and activate the session even
                                // if the optional post-response command update cannot be sent.
                                let _ = prepared.activation.send(());
                                connection.send_notification(available_commands_update(session_id))
                            }
                            Err(error) => responder.respond_with_result(Err(sdk_error(error))),
                        }
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: ForkSessionRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    let connection = cx.clone();
                    cx.spawn(async move {
                        match state.fork_session(request, connection.clone()).await {
                            Ok(prepared) => {
                                let session_id = prepared.response.session_id.clone();
                                responder.respond(prepared.response)?;
                                prepared.creation.commit_creation();
                                let _ = prepared.activation.send(());
                                connection.send_notification(available_commands_update(session_id))
                            }
                            Err(error) => responder.respond_with_result(Err(sdk_error(error))),
                        }
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: ListSessionsRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    cx.spawn(async move {
                        responder.respond_with_result(
                            state
                                .list_sessions(request)
                                .await
                                .map_err(list_sessions_error),
                        )
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: PromptRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    cx.spawn(async move {
                        responder
                            .respond_with_result(state.prompt(request).await.map_err(sdk_error))
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: SetSessionConfigOptionRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    cx.spawn(async move {
                        responder
                            .respond_with_result(state.set_config(request).await.map_err(sdk_error))
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: DetachComposeRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    cx.spawn(async move {
                        responder.respond_with_result(
                            state.detach_compose(request).await.map_err(sdk_error),
                        )
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: CancelBackgroundRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    cx.spawn(async move {
                        responder.respond_with_result(
                            state.cancel_background(request).await.map_err(sdk_error),
                        )
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: FileSearchRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    cx.spawn(async move {
                        responder.respond_with_result(
                            state.search_files(request).await.map_err(|error| {
                                agent_client_protocol::util::internal_error(error)
                            }),
                        )
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let state = Arc::clone(&state);
                async move |notification: CancelNotification, _cx| {
                    state.cancel(notification).await.map_err(sdk_error)?;
                    Ok(Handled::Yes)
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: CloseSessionRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    cx.spawn(async move {
                        responder.respond_with_result(state.close(request).await.map_err(sdk_error))
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        ))
}

fn fork_parent_context(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<(String, String)> {
    let meta = meta?;
    let id = meta.get(FORK_PARENT_ID_META)?.as_str()?;
    let name = meta.get(FORK_PARENT_NAME_META)?.as_str()?;
    crate::session::validate_id(id).ok()?;
    if name.is_empty()
        || name.len() > 32
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return None;
    }
    Some((id.into(), name.into()))
}

fn parse_session_list_cursor(cursor: &str) -> Result<usize, ListSessionsError> {
    cursor
        .strip_prefix("offset:")
        .and_then(|value| value.parse().ok())
        .ok_or(ListSessionsError::InvalidCursor)
}

fn capabilities(terminal_authentication: bool) -> agentkit_acp::AgentCapabilities {
    let capabilities = agentkit_acp::AgentCapabilities::new()
        .load_session(true)
        .prompt_capabilities(
            PromptCapabilities::new()
                .image(true)
                .audio(true)
                .embedded_context(true),
        )
        .session_capabilities(
            SessionCapabilities::new()
                .list(SessionListCapabilities::new())
                .fork(SessionForkCapabilities::new())
                .additional_directories(SessionAdditionalDirectoriesCapabilities::new())
                .close(SessionCloseCapabilities::new()),
        );
    if terminal_authentication {
        capabilities.auth(AgentAuthCapabilities::new().logout(LogoutCapabilities::new()))
    } else {
        capabilities
    }
}

async fn drain_turn_states(
    mut states: mpsc::UnboundedReceiver<TurnStateNotification>,
    connection: ConnectionTo<Client>,
) {
    while let Some(state) = states.recv().await {
        let _ = connection.send_notification(state);
    }
}

async fn drain_client_messages(
    mut messages: mpsc::UnboundedReceiver<AcpClientMessage>,
    connection: ConnectionTo<Client>,
) {
    while let Some(message) = messages.recv().await {
        match message {
            AcpClientMessage::SessionNotification(notification) => {
                let _ = connection.send_notification(notification);
            }
            AcpClientMessage::Flush { response } => {
                let _ = response.send(());
            }
            AcpClientMessage::PermissionRequest { request, response } => {
                let connection = connection.clone();
                tokio::spawn(async move {
                    let result = connection
                        .send_request(request)
                        .block_task()
                        .await
                        .map_err(|error| AcpRuntimeError::Sdk(error.to_string()));
                    let _ = response.send(result);
                });
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use agent_client_protocol::{Channel, schema::ProtocolVersion};
    use agentkit_core::{
        DataRef, Delta, Item, ItemKind, Modality, Part, PartId, PartKind, ToolCallId, ToolCallPart,
        ToolOutput, ToolResultPart, TurnCancellation,
    };
    use agentkit_loop::{
        Agent, LoopError, ModelAdapter, ModelTurn, ModelTurnEvent, ModelTurnResult, SessionConfig,
        TurnRequest,
    };
    use agentkit_task_manager::{
        AsyncTaskManager, RoutingDecision, TaskLaunchRequest, TaskManager, TaskStartContext,
    };
    use agentkit_tools_core::{
        AllowAllPermissions, BasicToolExecutor, OwnedToolContext, Tool, ToolAnnotations,
        ToolContext, ToolExecutor, ToolName, ToolRegistry, ToolRequest, ToolResult, ToolSource,
        ToolSpec,
    };
    use async_trait::async_trait;
    use tokio::{
        sync::Notify,
        time::{Duration, timeout},
    };

    use super::*;

    #[test]
    fn terminal_auth_methods_require_client_support() {
        assert!(terminal_auth_methods(&agentkit_acp::ClientCapabilities::new(), true).is_empty());

        let capabilities = agentkit_acp::ClientCapabilities::new()
            .auth(agentkit_acp::AuthCapabilities::new().terminal(true));
        assert!(terminal_auth_methods(&capabilities, false).is_empty());
        let methods = terminal_auth_methods(&capabilities, true);
        assert_eq!(methods.len(), 3);
        assert!(matches!(
            &methods[0],
            AuthMethod::Terminal(method)
                if method.id.0.as_ref() == "openai"
                    && method.args == ["--terminal-auth-login", "openai"]
        ));
        assert!(matches!(
            &methods[1],
            AuthMethod::Terminal(method)
                if method.id.0.as_ref() == "openrouter"
                    && method.args == ["--terminal-auth-login", "openrouter"]
        ));
        assert!(matches!(
            &methods[2],
            AuthMethod::Terminal(method)
                if method.id.0.as_ref() == "speakeasy"
                    && method.args == ["--terminal-auth-login", "speakeasy"]
        ));
    }

    #[test]
    fn v1_logout_capability_tracks_terminal_authentication_methods() {
        let client = agentkit_acp::ClientCapabilities::new()
            .auth(agentkit_acp::AuthCapabilities::new().terminal(true));

        for enabled in [false, true] {
            assert_eq!(
                capabilities(enabled).auth.logout.is_some(),
                !terminal_auth_methods(&client, enabled).is_empty()
            );
        }
    }

    #[test]
    fn v1_authentication_errors_and_terminal_login_use_protocol_codes() {
        let error = sdk_error(AcpRuntimeError::Loop(
            "speakeasy_auth_required: run `kit auth login speakeasy`".into(),
        ));
        assert_eq!(error.code, agent_client_protocol::ErrorCode::AuthRequired);

        let root = tempfile::tempdir().unwrap();
        let server = Server::new(
            Runtime::new(root.path(), "gpt-5.4").unwrap(),
            AcpIntegration::builder()
                .name("auth-test")
                .approval_resolver(AutoDenyResolver)
                .build()
                .unwrap(),
            SessionRegistry::new(),
        );
        for method_id in ["openai", "unknown"] {
            let error = server
                .authenticate(AuthenticateRequest::new(method_id))
                .unwrap_err();
            assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
            assert_eq!(error.data.unwrap()["methodId"], method_id);
        }
    }

    #[tokio::test]
    async fn failed_partial_logout_still_resets_active_sessions() {
        let root = tempfile::tempdir().unwrap();
        let credentials = tempfile::tempdir().unwrap();
        let storage =
            crate::credentials::CredentialStorage::Filesystem(credentials.path().to_path_buf());
        storage.make_entry_undeletable_for_test("openrouter", "default");
        storage
            .entry("speakeasy", "default")
            .save(b"removed")
            .unwrap();
        let mut runtime = Runtime::new_with_provider_and_credentials(
            root.path(),
            "gpt-5.4",
            crate::ProviderKind::OpenAiSubscription,
            storage.clone(),
        )
        .unwrap();
        Arc::get_mut(&mut runtime)
            .unwrap()
            .set_ambient_openrouter_api_key_for_test(false);
        let registry = SessionRegistry::new();
        let server = logout_test_server(runtime, registry.clone());
        let (_commands, closed) = register_close_tracking_session(&server, &registry);

        let error = server.logout().await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("could not remove OpenRouter credentials")
        );
        assert!(closed.load(Ordering::SeqCst));
        assert!(
            storage
                .entry("speakeasy", "default")
                .load()
                .unwrap()
                .is_none()
        );
        assert!(registry.begin_attachment().is_ok());
    }

    #[tokio::test]
    async fn unsupported_logout_preserves_active_sessions() {
        let root = tempfile::tempdir().unwrap();
        let credentials = tempfile::tempdir().unwrap();
        let runtime = Runtime::new_with_provider_credentials_effort_and_openrouter_key(
            root.path(),
            "openrouter:test",
            crate::ProviderKind::OpenRouter,
            crate::credentials::CredentialStorage::Filesystem(credentials.path().to_path_buf()),
            None,
            Some(crate::provider::OpenRouterApiKey::new("explicit")),
        )
        .unwrap();
        let registry = SessionRegistry::new();
        let server = logout_test_server(runtime, registry.clone());
        let (_commands, closed) = register_close_tracking_session(&server, &registry);

        let error = server.logout().await.unwrap_err();

        assert!(error.to_string().contains("cannot be logged out"));
        assert!(!closed.load(Ordering::SeqCst));

        registry.shutdown().await;
        assert!(closed.load(Ordering::SeqCst));
    }

    fn logout_test_server(runtime: Arc<Runtime>, registry: SessionRegistry) -> Arc<Server> {
        Arc::new(Server::new(
            runtime,
            AcpIntegration::builder()
                .name("logout-test")
                .approval_resolver(AutoDenyResolver)
                .build()
                .unwrap(),
            registry,
        ))
    }

    fn register_close_tracking_session(
        server: &Arc<Server>,
        registry: &SessionRegistry,
    ) -> (mpsc::Sender<Command>, Arc<AtomicBool>) {
        let token = registry.next_token();
        let (commands, mut received) = mpsc::channel(1);
        let (completed, completion) = watch::channel(false);
        let closed = Arc::new(AtomicBool::new(false));
        let actor_closed = Arc::clone(&closed);
        let actor = tokio::spawn(async move {
            let _completion = CompletionOnDrop(completed);
            if let Some(Command::Close { reply }) = received.recv().await {
                actor_closed.store(true, Ordering::SeqCst);
                let _ = reply.send(());
            }
        });
        registry
            .register(
                registry.begin_attachment().unwrap(),
                RegisteredSession {
                    token,
                    session_id: agentkit_acp::SessionId::new("logout-session"),
                    integration: Arc::clone(&server.integration),
                    background_jobs: BackgroundJobs::default(),
                    tasks: AsyncTaskManager::new().handle(),
                    commands: commands.downgrade(),
                    actor: actor.abort_handle(),
                    completed: completion,
                },
            )
            .unwrap();
        drop(actor);
        (commands, closed)
    }

    #[test]
    fn terminal_auth_methods_support_the_legacy_registry_capability() {
        let mut meta = serde_json::Map::new();
        meta.insert("terminal-auth".into(), serde_json::Value::Bool(true));
        let capabilities = agentkit_acp::ClientCapabilities::new().meta(meta);

        assert_eq!(terminal_auth_methods(&capabilities, true).len(), 3);
    }

    struct CompletionOnDrop(watch::Sender<bool>);

    impl Drop for CompletionOnDrop {
        fn drop(&mut self) {
            self.0.send_replace(true);
        }
    }

    fn shutdown_test_integration() -> Arc<AcpIntegration> {
        Arc::new(
            AcpIntegration::builder()
                .name("shutdown-test")
                .approval_resolver(AutoDenyResolver)
                .build()
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn shutdown_force_cancels_background_before_close_and_rejects_registration() {
        let registry = SessionRegistry::new();
        let integration = shutdown_test_integration();
        let (background_jobs, tasks) = start_non_cooperative_background("shutdown-call").await;
        let session_id = agentkit_acp::SessionId::new("close-me");
        let token = registry.next_token();
        let (commands, mut received) = mpsc::channel(1);
        let weak_commands = commands.downgrade();
        let (completed, completion) = watch::channel(false);
        let closed = Arc::new(AtomicBool::new(false));
        let actor_closed = Arc::clone(&closed);
        let actor = tokio::spawn(async move {
            let _completion = CompletionOnDrop(completed);
            if let Some(Command::Close { reply }) = received.recv().await {
                actor_closed.store(true, Ordering::SeqCst);
                let _ = reply.send(());
            }
        });
        registry
            .register(
                registry.begin_attachment().unwrap(),
                RegisteredSession {
                    token,
                    session_id,
                    integration: Arc::clone(&integration),
                    background_jobs: background_jobs.clone(),
                    tasks: tasks.clone(),
                    commands: weak_commands,
                    actor: actor.abort_handle(),
                    completed: completion,
                },
            )
            .unwrap();
        drop(actor);

        let late_admission = registry.begin_attachment().unwrap();
        registry.shutdown_with_timeout(Duration::from_secs(1)).await;
        assert!(closed.load(Ordering::SeqCst));
        assert!(tasks.list_running().await.is_empty());
        assert!(!background_jobs.activity().active);

        let late_token = registry.next_token();
        let (late_commands, _late_received) = mpsc::channel(1);
        let (late_completed, late_completion) = watch::channel(false);
        let late_actor = tokio::spawn(async move {
            let _completion = CompletionOnDrop(late_completed);
            std::future::pending::<()>().await;
        });
        assert!(
            registry
                .register(
                    late_admission,
                    RegisteredSession {
                        token: late_token,
                        session_id: agentkit_acp::SessionId::new("too-late"),
                        integration,
                        background_jobs: BackgroundJobs::default(),
                        tasks: AsyncTaskManager::new().handle(),
                        commands: late_commands.downgrade(),
                        actor: late_actor.abort_handle(),
                        completed: late_completion,
                    }
                )
                .is_err()
        );
        late_actor.abort();
        late_actor.await.unwrap_err();
    }

    #[tokio::test]
    async fn authentication_reset_closes_shared_v2_sessions_and_reopens_registration() {
        let registry = SessionRegistry::new();
        let token = registry.next_token();
        let closed = Arc::new(AtomicBool::new(false));
        let close_flag = Arc::clone(&closed);
        let (completed, completion) = watch::channel(false);
        let close = Arc::new(move || {
            let close_flag = Arc::clone(&close_flag);
            let completed = completed.clone();
            Box::pin(async move {
                close_flag.store(true, Ordering::SeqCst);
                completed.send_replace(true);
            }) as Pin<Box<dyn Future<Output = ()> + Send>>
        });
        let actor = tokio::spawn(std::future::pending::<()>());
        registry
            .register_v2(
                registry.begin_attachment().unwrap(),
                token,
                Arc::new(|| {}),
                close,
                actor.abort_handle(),
                completion,
            )
            .unwrap();

        let generation = registry
            .inner
            .state
            .lock()
            .expect("ACP session registry poisoned")
            .generation;
        assert!(registry.reset_authentication().await);

        assert!(closed.load(Ordering::SeqCst));
        assert_ne!(
            registry
                .inner
                .state
                .lock()
                .expect("ACP session registry poisoned")
                .generation,
            generation
        );
        assert!(
            registry
                .inner
                .state
                .lock()
                .expect("ACP session registry poisoned")
                .accepting
        );
        actor.abort();
        actor.await.unwrap_err();
    }

    #[tokio::test]
    async fn failed_authentication_reset_cannot_reopen_on_retry() {
        let registry = SessionRegistry::new();
        let token = registry.next_token();
        let (_completed, completion) = watch::channel(false);
        let actor = tokio::spawn(std::future::pending::<()>());
        registry
            .register_v2(
                registry.begin_attachment().unwrap(),
                token,
                Arc::new(|| {}),
                Arc::new(|| Box::pin(std::future::pending())),
                actor.abort_handle(),
                completion,
            )
            .unwrap();

        assert!(
            !registry
                .close_sessions_with_timeout(Duration::from_millis(10), true)
                .await
        );
        assert!(!registry.reset_authentication().await);
        assert!(registry.begin_attachment().is_err());
        actor.abort();
        let _ = actor.await;
    }

    #[tokio::test]
    async fn permanent_shutdown_cannot_be_reopened_by_authentication_reset() {
        let registry = SessionRegistry::new();
        let resetting = registry.clone();
        let reset = tokio::spawn(async move { resetting.reset_authentication().await });

        registry.shutdown().await;
        let _ = reset.await.unwrap();

        assert!(registry.begin_attachment().is_err());
    }

    #[tokio::test]
    async fn shutdown_aborts_actor_at_the_shared_deadline() {
        let registry = SessionRegistry::new();
        let token = registry.next_token();
        let (commands, _received) = mpsc::channel(1);
        let (_completed, completion) = watch::channel(false);
        let actor = tokio::spawn(std::future::pending::<()>());
        registry
            .register(
                registry.begin_attachment().unwrap(),
                RegisteredSession {
                    token,
                    session_id: agentkit_acp::SessionId::new("stuck"),
                    integration: shutdown_test_integration(),
                    background_jobs: BackgroundJobs::default(),
                    tasks: AsyncTaskManager::new().handle(),
                    commands: commands.downgrade(),
                    actor: actor.abort_handle(),
                    completed: completion,
                },
            )
            .unwrap();
        timeout(
            Duration::from_secs(1),
            registry.shutdown_with_timeout(Duration::from_millis(20)),
        )
        .await
        .unwrap();
        let error = timeout(Duration::from_secs(1), actor)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.is_cancelled());
    }

    #[test]
    fn available_commands_advertises_only_compact() {
        let notification = available_commands_update(agentkit_acp::SessionId::new("session"));
        let SessionUpdate::AvailableCommandsUpdate(update) = notification.update else {
            panic!("expected available commands update");
        };
        assert_eq!(
            update
                .available_commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            ["compact"]
        );
    }

    #[test]
    fn replay_restores_data_url_images_as_image_content() {
        let part = Part::media(
            Modality::Image,
            "image/png",
            DataRef::uri("data:image/png;base64,AQID"),
        );
        let chunk = user_replay_content(&part).expect("image replay content");

        assert!(matches!(
            chunk.content,
            ContentBlock::Image(image) if image.data == "AQID"
        ));
    }

    #[test]
    fn transcript_replay_preserves_order_and_skips_unrepresentable_history() {
        let transcript = vec![
            Item::text(ItemKind::System, "system"),
            Item::new(
                ItemKind::User,
                vec![
                    Part::text("user"),
                    Part::media(
                        Modality::Image,
                        "image/png",
                        DataRef::inline_bytes([1, 2, 3]),
                    ),
                    Part::file(DataRef::uri("file:///tmp/input.txt")),
                    Part::structured(json!({ "skip": true })),
                ],
            ),
            Item::new(
                ItemKind::Assistant,
                vec![
                    Part::text("agent"),
                    Part::reasoning("thought"),
                    Part::ToolCall(ToolCallPart::new(
                        "call-1",
                        "read",
                        json!({ "path": "input.txt" }),
                    )),
                    Part::structured(json!({ "skip": true })),
                ],
            ),
            Item::new(
                ItemKind::Tool,
                vec![Part::ToolResult(ToolResultPart::success(
                    "call-1",
                    ToolOutput::text("done"),
                ))],
            ),
            Item::text(ItemKind::Developer, "developer"),
            Item::notification("notification"),
        ];

        let updates = transcript_replay(&agentkit_acp::SessionId::new("session"), &transcript)
            .into_iter()
            .map(|notification| notification.update)
            .collect::<Vec<_>>();

        assert_eq!(updates.len(), 7);
        assert!(matches!(
            &updates[0],
            SessionUpdate::UserMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "user")
        ));
        assert!(matches!(
            &updates[1],
            SessionUpdate::UserMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Image(image) if image.data == "AQID")
        ));
        assert!(matches!(
            &updates[2],
            SessionUpdate::UserMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::ResourceLink(link) if link.uri == "file:///tmp/input.txt")
        ));
        assert!(matches!(
            &updates[3],
            SessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "agent")
        ));
        assert!(matches!(
            &updates[4],
            SessionUpdate::AgentThoughtChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "thought")
        ));
        let SessionUpdate::ToolCall(call) = &updates[5] else {
            panic!("expected pending tool call");
        };
        assert_eq!(call.tool_call_id.to_string(), "call-1");
        assert_eq!(call.status, ToolCallStatus::Pending);
        assert_eq!(call.raw_input, Some(json!({ "path": "input.txt" })));
        let SessionUpdate::ToolCallUpdate(update) = &updates[6] else {
            panic!("expected terminal tool update");
        };
        assert_eq!(update.tool_call_id.to_string(), "call-1");
        assert_eq!(update.fields.status, Some(ToolCallStatus::Completed));
        assert_eq!(update.fields.raw_output, Some(json!({ "text": "done" })));
    }

    #[test]
    fn transcript_replay_includes_the_canonical_compaction_summary() {
        let mut metadata = MetadataMap::new();
        metadata.insert("kit.compaction.summary".into(), true.into());
        let transcript = vec![
            Item::text(ItemKind::Developer, "ordinary developer"),
            Item::text(ItemKind::Developer, "canonical checkpoint").with_metadata(metadata),
        ];

        let replay = transcript_replay(&agentkit_acp::SessionId::new("session"), &transcript);
        assert_eq!(replay.len(), 1);
        assert!(matches!(
            &replay[0].update,
            SessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "canonical checkpoint")
        ));
    }

    #[tokio::test]
    async fn response_interruption_marker_becomes_v1_warning_before_replacement() {
        let integration = AcpIntegration::builder()
            .name("response-interruption-test")
            .approval_resolver(AutoDenyResolver)
            .build()
            .unwrap();
        let session_id = agentkit_acp::SessionId::new("v1-interruption");
        let loop_session_id = AgentkitSessionId::new("v1-interruption-loop");
        let (client, mut messages) = AcpClientHandle::channel();
        integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                loop_session_id.clone(),
                client.clone(),
            ))
            .unwrap();
        let observer = ResponseInterruptionNoticeObserver::new(
            integration.clone(),
            client,
            session_id.clone(),
        );
        let emit = |event| {
            observer.handle_event(ObservedEvent {
                session_id: Arc::new(loop_session_id.clone()),
                event,
            });
        };
        emit(AgentEvent::ResponseAttemptSuperseded);
        emit(AgentEvent::ContentDelta(Delta::BeginPart {
            part_id: PartId::new("replacement"),
            kind: PartKind::Text,
        }));
        emit(AgentEvent::ContentDelta(Delta::AppendText {
            part_id: PartId::new("replacement"),
            chunk: "fresh response".into(),
        }));

        let Some(AcpClientMessage::SessionNotification(notice)) = messages.recv().await else {
            panic!("expected interruption notice");
        };
        let SessionUpdate::Notice(notice) = notice.update else {
            panic!("expected notice before replacement output");
        };
        assert_eq!(notice.severity, NoticeSeverity::Warning);
        assert_eq!(notice.title, "Response interrupted; replacement follows");

        let Some(AcpClientMessage::SessionNotification(replacement)) = messages.recv().await else {
            panic!("expected replacement output");
        };
        assert!(matches!(
            replacement.update,
            SessionUpdate::AgentMessageChunk(chunk)
                if matches!(&chunk.content, ContentBlock::Text(text) if text.text == "fresh response")
        ));
        integration.unbind_session(&session_id).unwrap();
    }

    #[test]
    fn dropping_an_in_flight_binding_guard_releases_the_durable_identity() {
        let integration = Arc::new(
            AcpIntegration::builder()
                .name("binding-guard-test")
                .approval_resolver(AutoDenyResolver)
                .build()
                .unwrap(),
        );
        let session_id = agentkit_acp::SessionId::new("s-binding-guard");
        let agentkit_session_id = AgentkitSessionId::new("s-binding-guard");
        let (client, _messages) = AcpClientHandle::channel();
        integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                agentkit_session_id.clone(),
                client,
            ))
            .unwrap();

        drop(SessionBindingGuard::new(
            Arc::clone(&integration),
            session_id.clone(),
        ));

        let (client, _messages) = AcpClientHandle::channel();
        integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                agentkit_session_id,
                client,
            ))
            .expect("dropped request left the durable identity bound");
        integration.unbind_session(&session_id).unwrap();
    }

    pub(super) struct ScriptAdapter {
        pub(super) turns: Arc<AtomicUsize>,
        pub(super) user_items_seen: Arc<AtomicUsize>,
        pub(super) notification_items_seen: Arc<AtomicUsize>,
    }

    pub(super) struct ScriptSession {
        turns: Arc<AtomicUsize>,
        user_items_seen: Arc<AtomicUsize>,
        notification_items_seen: Arc<AtomicUsize>,
    }

    pub(super) struct ScriptTurn {
        events: VecDeque<ModelTurnEvent>,
    }

    struct CancelAdapter;
    struct CancelSession;

    #[async_trait]
    impl ModelAdapter for CancelAdapter {
        type Session = CancelSession;

        async fn start_session(&self, _config: SessionConfig) -> Result<Self::Session, LoopError> {
            Ok(CancelSession)
        }
    }

    #[async_trait]
    impl ModelSession for CancelSession {
        type Turn = ScriptTurn;

        async fn begin_turn(
            &mut self,
            _request: TurnRequest,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Self::Turn, LoopError> {
            Err(LoopError::Cancelled)
        }
    }

    #[async_trait]
    impl ModelAdapter for ScriptAdapter {
        type Session = ScriptSession;

        async fn start_session(&self, _config: SessionConfig) -> Result<Self::Session, LoopError> {
            Ok(ScriptSession {
                turns: Arc::clone(&self.turns),
                user_items_seen: Arc::clone(&self.user_items_seen),
                notification_items_seen: Arc::clone(&self.notification_items_seen),
            })
        }
    }

    #[async_trait]
    impl ModelSession for ScriptSession {
        type Turn = ScriptTurn;

        async fn begin_turn(
            &mut self,
            request: TurnRequest,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Self::Turn, LoopError> {
            let turn = self.turns.fetch_add(1, Ordering::SeqCst) + 1;
            self.user_items_seen.store(
                request
                    .transcript
                    .iter()
                    .filter(|item| item.kind == ItemKind::User)
                    .count(),
                Ordering::SeqCst,
            );
            self.notification_items_seen.store(
                request
                    .transcript
                    .iter()
                    .filter(|item| item.kind == ItemKind::Notification)
                    .count(),
                Ordering::SeqCst,
            );
            let called = request.transcript.iter().any(|item| {
                item.parts
                    .iter()
                    .any(|part| matches!(part, Part::ToolCall(_)))
            });
            let events = if turn >= 3 {
                let text = "autonomous background completion";
                VecDeque::from([
                    ModelTurnEvent::Delta(Delta::BeginPart {
                        part_id: PartId::new("answer"),
                        kind: PartKind::Text,
                    }),
                    ModelTurnEvent::Delta(Delta::AppendText {
                        part_id: PartId::new("answer"),
                        chunk: text.into(),
                    }),
                    ModelTurnEvent::Finished(ModelTurnResult {
                        model: None,
                        response_id: None,
                        finish_reason: FinishReason::Completed,
                        output_items: vec![Item::text(ItemKind::Assistant, text)],
                        usage: None,
                        metadata: MetadataMap::new(),
                    }),
                ])
            } else if called {
                let text = "compose detached";
                VecDeque::from([
                    ModelTurnEvent::Delta(Delta::BeginPart {
                        part_id: PartId::new("detached"),
                        kind: PartKind::Text,
                    }),
                    ModelTurnEvent::Delta(Delta::AppendText {
                        part_id: PartId::new("detached"),
                        chunk: text.into(),
                    }),
                    ModelTurnEvent::Finished(ModelTurnResult {
                        model: None,
                        response_id: None,
                        finish_reason: FinishReason::Completed,
                        output_items: vec![Item::text(ItemKind::Assistant, text)],
                        usage: None,
                        metadata: MetadataMap::new(),
                    }),
                ])
            } else {
                let call = ToolCallPart {
                    id: ToolCallId::new("background-call"),
                    name: agentkit_tool_compose::COMPOSE_TOOL_NAME.into(),
                    input: json!({}),
                    metadata: MetadataMap::new(),
                };
                VecDeque::from([
                    ModelTurnEvent::ToolCall(call.clone()),
                    ModelTurnEvent::Finished(ModelTurnResult {
                        model: None,
                        response_id: None,
                        finish_reason: FinishReason::ToolCall,
                        output_items: vec![Item {
                            id: None,
                            kind: ItemKind::Assistant,
                            parts: vec![Part::ToolCall(call)],
                            metadata: MetadataMap::new(),
                            usage: None,
                            finish_reason: None,
                            created_at: None,
                        }],
                        usage: None,
                        metadata: MetadataMap::new(),
                    }),
                ])
            };
            Ok(ScriptTurn { events })
        }
    }

    #[async_trait]
    impl ModelTurn for ScriptTurn {
        async fn next_event(
            &mut self,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Option<ModelTurnEvent>, LoopError> {
            Ok(self.events.pop_front())
        }
    }

    pub(super) struct BlockingTool {
        pub(super) spec: ToolSpec,
        pub(super) entered: Arc<AtomicBool>,
        pub(super) release: Arc<Notify>,
    }

    #[async_trait]
    impl Tool for BlockingTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        async fn invoke(
            &self,
            request: agentkit_tools_core::ToolRequest,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, agentkit_tools_core::ToolError> {
            self.entered.store(true, Ordering::SeqCst);
            self.release.notified().await;
            Ok(ToolResult {
                result: ToolResultPart {
                    call_id: request.call_id,
                    output: ToolOutput::Text("background done".into()),
                    is_error: false,
                    metadata: MetadataMap::new(),
                },
                duration: None,
                metadata: MetadataMap::new(),
            })
        }
    }

    struct FinishBackgroundOnDrop {
        jobs: BackgroundJobs,
        call_id: String,
    }

    impl Drop for FinishBackgroundOnDrop {
        fn drop(&mut self) {
            self.jobs.finish_for_test(&self.call_id);
        }
    }

    struct NonCooperativeTool {
        spec: ToolSpec,
        entered: Arc<AtomicBool>,
        jobs: BackgroundJobs,
    }

    #[async_trait]
    impl Tool for NonCooperativeTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        async fn invoke(
            &self,
            request: ToolRequest,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, agentkit_tools_core::ToolError> {
            let _finish = FinishBackgroundOnDrop {
                jobs: self.jobs.clone(),
                call_id: request.call_id.to_string(),
            };
            self.entered.store(true, Ordering::SeqCst);
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    struct FailingBackgroundTool {
        spec: ToolSpec,
        jobs: BackgroundJobs,
    }

    #[async_trait]
    impl Tool for FailingBackgroundTool {
        fn spec(&self) -> &ToolSpec {
            &self.spec
        }

        async fn invoke(
            &self,
            request: ToolRequest,
            _ctx: &mut ToolContext<'_>,
        ) -> Result<ToolResult, agentkit_tools_core::ToolError> {
            let _finish = FinishBackgroundOnDrop {
                jobs: self.jobs.clone(),
                call_id: request.call_id.to_string(),
            };
            Err(agentkit_tools_core::ToolError::Unavailable(
                "fast failure".into(),
            ))
        }
    }

    fn background_task_context(executor: Arc<dyn ToolExecutor>) -> TaskStartContext {
        TaskStartContext {
            executor,
            tool_context: OwnedToolContext {
                session_id: agentkit_core::SessionId::new("background-session"),
                turn_id: agentkit_core::TurnId::new("background-turn"),
                metadata: MetadataMap::new(),
                permissions: Arc::new(AllowAllPermissions),
                resources: Arc::new(()),
                cancellation: None,
                execution_scope: None,
                approved_request: None,
            },
        }
    }

    fn background_task_request(call_id: &str, tool_name: &str) -> TaskLaunchRequest {
        TaskLaunchRequest::plain(
            None,
            ToolRequest {
                call_id: ToolCallId::new(call_id),
                tool_name: ToolName::new(tool_name),
                input: json!({}),
                session_id: agentkit_core::SessionId::new("background-session"),
                turn_id: agentkit_core::TurnId::new("background-turn"),
                metadata: MetadataMap::new(),
            },
        )
    }

    async fn start_non_cooperative_background(
        call_id: &str,
    ) -> (BackgroundJobs, TaskManagerHandle) {
        let jobs = BackgroundJobs::default();
        jobs.register_foreground_for_test(call_id);
        assert_eq!(jobs.detach(call_id), Some(DetachRegistration::Registered));
        let entered = Arc::new(AtomicBool::new(false));
        let tools = ToolRegistry::new().with(NonCooperativeTool {
            spec: ToolSpec {
                name: ToolName::new("non-cooperative"),
                description: "ignores cooperative cancellation".into(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                annotations: ToolAnnotations::default(),
                metadata: MetadataMap::new(),
            },
            entered: Arc::clone(&entered),
            jobs: jobs.clone(),
        });
        let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::new([
            Arc::new(tools) as Arc<dyn ToolSource>
        ]));
        let manager =
            AsyncTaskManager::new().routing(|_request: &ToolRequest| RoutingDecision::Background);
        let tasks = manager.handle();
        manager
            .start_task(
                background_task_request(call_id, "non-cooperative"),
                background_task_context(executor),
            )
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while !entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("non-cooperative task did not start");
        (jobs, tasks)
    }

    #[tokio::test]
    async fn depth_zero_cancel_leaves_detached_background_running() {
        let (jobs, tasks) = start_non_cooperative_background("root-call").await;
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
        let integration = AcpIntegration::builder()
            .name("root-cancel-test")
            .approval_resolver(AutoDenyResolver)
            .build()
            .unwrap();
        let session_id = agentkit_acp::SessionId::new("root-cancel-session");
        let (client, _messages) = AcpClientHandle::channel();
        integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                AgentkitSessionId::new("root-cancel-session"),
                client,
            ))
            .unwrap();
        let server = Server::new(runtime, integration, SessionRegistry::new());
        let (commands, mut received) = mpsc::channel(1);
        server.sessions.lock().unwrap().insert(
            session_id.clone(),
            SessionHandle {
                token: 1,
                commands,
                background_jobs: jobs.clone(),
                structured_completion: false,
                tasks: tasks.clone(),
            },
        );

        server
            .cancel(CancelNotification::new(session_id))
            .await
            .unwrap();
        assert!(matches!(received.recv().await, Some(Command::Cancel)));
        assert!(!jobs.is_cancelled_for_test("root-call"));
        assert!(!tasks.list_running().await.is_empty());

        cancel_background_jobs(&tasks, &jobs).await;
        timeout(
            Duration::from_secs(1),
            settle_background_jobs(&tasks, &jobs),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn failed_background_terminal_settles_after_fast_completion() {
        let jobs = BackgroundJobs::default();
        jobs.register_foreground_for_test("failed-call");
        assert_eq!(
            jobs.detach("failed-call"),
            Some(DetachRegistration::Registered)
        );
        let tools = ToolRegistry::new().with(FailingBackgroundTool {
            spec: ToolSpec {
                name: ToolName::new("fast-failure"),
                description: "fails immediately".into(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
                annotations: ToolAnnotations::default(),
                metadata: MetadataMap::new(),
            },
            jobs: jobs.clone(),
        });
        let executor: Arc<dyn ToolExecutor> = Arc::new(BasicToolExecutor::new([
            Arc::new(tools) as Arc<dyn ToolSource>
        ]));
        let manager =
            AsyncTaskManager::new().routing(|_request: &ToolRequest| RoutingDecision::Background);
        let tasks = manager.handle();
        manager
            .start_task(
                background_task_request("failed-call", "fast-failure"),
                background_task_context(executor),
            )
            .await
            .unwrap();

        assert!(
            timeout(
                Duration::from_secs(1),
                settle_background_jobs(&tasks, &jobs)
            )
            .await
            .expect("failed background terminal did not settle")
            .unwrap()
        );
        assert!(tasks.list_running().await.is_empty());
    }

    #[tokio::test]
    async fn non_cooperative_background_is_force_cancelled_before_settlement() {
        let (jobs, tasks) = start_non_cooperative_background("stuck-call").await;

        timeout(
            Duration::from_secs(1),
            cancel_background_jobs(&tasks, &jobs),
        )
        .await
        .expect("bounded background cancellation hung");
        assert!(tasks.list_running().await.is_empty());
        assert!(
            timeout(
                Duration::from_secs(1),
                settle_background_jobs(&tasks, &jobs)
            )
            .await
            .expect("forced cancellation did not settle")
            .unwrap()
        );
    }

    #[tokio::test]
    async fn cancelled_driver_returns_normal_cancelled_prompt_response() {
        let integration = Arc::new(
            AcpIntegration::builder()
                .name("cancel-test")
                .approval_resolver(AutoDenyResolver)
                .build()
                .unwrap(),
        );
        let acp_session_id = agentkit_acp::SessionId::new("s-cancel-session");
        let agentkit_session_id = AgentkitSessionId::new("s-cancel-session");
        let (client, mut messages) = AcpClientHandle::channel();
        integration
            .bind_session(AcpSessionBinding::new(
                acp_session_id.clone(),
                agentkit_session_id.clone(),
                client,
            ))
            .unwrap();
        let drain = tokio::spawn(async move {
            while let Some(message) = messages.recv().await {
                if let AcpClientMessage::Flush { response } = message {
                    let _ = response.send(());
                }
            }
        });
        let mut driver = Agent::builder()
            .model(CancelAdapter)
            .observer(integration.as_ref().clone())
            .build()
            .unwrap()
            .start(SessionConfig::new(agentkit_session_id).without_cache())
            .await
            .unwrap();

        let task_manager = AsyncTaskManager::new();
        let tasks = task_manager.handle();
        let background_jobs = BackgroundJobs::default();
        let mut skill_catalog = skill_catalog::SkillCatalogMonitor::new(&[]).unwrap();
        let response = drive_prompt(
            &acp_session_id,
            &[],
            &integration,
            &mut skill_catalog,
            &mut driver,
            PromptRequest::new(
                acp_session_id.clone(),
                vec![agentkit_acp::ContentBlock::Text(
                    agentkit_acp::TextContent::new("cancel me"),
                )],
            ),
            &tasks,
            &background_jobs,
            false,
        )
        .await
        .expect("cancellation must be an ACP response, not an RPC error");

        assert_eq!(response.stop_reason, StopReason::Cancelled);
        drain.abort();
    }

    #[tokio::test]
    async fn live_prompt_boundary_refreshes_plugin_skill_catalog() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let plugins = crate::plugins::PluginRuntime::load(
            config.clone(),
            root.path().to_path_buf(),
            root.path().join("cache"),
            root.path().join("data"),
        )
        .await
        .unwrap();
        let runtime = Runtime::with_plugin_runtime(
            Runtime::new(root.path(), "gpt-5.4").unwrap(),
            Some(plugins),
        )
        .unwrap();
        let runtime = Runtime::with_mcp_config(
            runtime,
            None,
            Vec::new(),
            false,
            crate::tools::mcp::CredentialStorage::Memory,
        )
        .await
        .unwrap();
        let baseline = runtime.current_skills().await.unwrap();
        let mut skill_catalog = skill_catalog::SkillCatalogMonitor::new(&baseline.skills).unwrap();
        drop(baseline);

        let package = root.path().join("plugin");
        let skill = package.join("skills/live-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            package.join("plugin.json"),
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"live-plugin"}"#,
        )
        .unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: live-skill\ndescription: Live skill.\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            &config,
            format!(
                "[plugins.live]\nsource = 'path'\npath = '{}'\n",
                package.display()
            ),
        )
        .unwrap();

        let integration = Arc::new(
            AcpIntegration::builder()
                .name("plugin-refresh-test")
                .approval_resolver(AutoDenyResolver)
                .build()
                .unwrap(),
        );
        let acp_session_id = agentkit_acp::SessionId::new("s-plugin-refresh");
        let agentkit_session_id = AgentkitSessionId::new("s-plugin-refresh");
        let (client, mut messages) = AcpClientHandle::channel();
        integration
            .bind_session(AcpSessionBinding::new(
                acp_session_id.clone(),
                agentkit_session_id.clone(),
                client,
            ))
            .unwrap();
        let drain = tokio::spawn(async move {
            while let Some(message) = messages.recv().await {
                if let AcpClientMessage::Flush { response } = message {
                    let _ = response.send(());
                }
            }
        });
        let turns = Arc::new(AtomicUsize::new(2));
        let notification_items_seen = Arc::new(AtomicUsize::new(0));
        let mut driver = Agent::builder()
            .model(ScriptAdapter {
                turns,
                user_items_seen: Arc::new(AtomicUsize::new(0)),
                notification_items_seen: Arc::clone(&notification_items_seen),
            })
            .observer(integration.as_ref().clone())
            .build()
            .unwrap()
            .start(SessionConfig::new(agentkit_session_id).without_cache())
            .await
            .unwrap();
        let task_manager = AsyncTaskManager::new();
        let tasks = task_manager.handle();
        let response = drive_runtime_prompt(
            &acp_session_id,
            &runtime,
            &integration,
            &mut skill_catalog,
            &mut driver,
            PromptRequest::new(
                acp_session_id.clone(),
                vec![agentkit_acp::ContentBlock::Text(
                    agentkit_acp::TextContent::new("use the new skill"),
                )],
            ),
            &tasks,
            &BackgroundJobs::default(),
            false,
        )
        .await
        .unwrap();
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(notification_items_seen.load(Ordering::SeqCst), 1);
        drain.abort();
    }

    #[tokio::test]
    async fn structured_prompt_waits_for_background_completion_and_synthesis() {
        let turns = Arc::new(AtomicUsize::new(0));
        let user_items_seen = Arc::new(AtomicUsize::new(0));
        let notification_items_seen = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Notify::new());
        let integration = Arc::new(
            AcpIntegration::builder()
                .name("structured-test")
                .approval_resolver(AutoDenyResolver)
                .build()
                .unwrap(),
        );
        let acp_session_id = agentkit_acp::SessionId::new("s-structured-session");
        let agentkit_session_id = AgentkitSessionId::new("s-structured-session");
        let (client, mut messages) = AcpClientHandle::channel();
        integration
            .bind_session(AcpSessionBinding::new(
                acp_session_id.clone(),
                agentkit_session_id.clone(),
                client,
            ))
            .unwrap();
        let drain = tokio::spawn(async move {
            while let Some(message) = messages.recv().await {
                if let AcpClientMessage::Flush { response } = message {
                    let _ = response.send(());
                }
            }
        });

        let task_manager = AsyncTaskManager::new()
            .routing(|_request: &agentkit_tools_core::ToolRequest| RoutingDecision::Foreground);
        let tasks = task_manager.handle();
        let tools = ToolRegistry::new().with(BlockingTool {
            spec: ToolSpec {
                name: ToolName::new(agentkit_tool_compose::COMPOSE_TOOL_NAME),
                description: "controlled compose tool".into(),
                input_schema: json!({"type": "object", "additionalProperties": false}),
                output_schema: None,
                annotations: ToolAnnotations::default(),
                metadata: MetadataMap::new(),
            },
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let mut driver = Agent::builder()
            .model(ScriptAdapter {
                turns: Arc::clone(&turns),
                user_items_seen,
                notification_items_seen,
            })
            .add_tool_source(tools)
            .task_manager(task_manager)
            .observer(integration.as_ref().clone())
            .build()
            .unwrap()
            .start(SessionConfig::new(agentkit_session_id).without_cache())
            .await
            .unwrap();
        let background_jobs = BackgroundJobs::default();
        let mut skill_catalog = skill_catalog::SkillCatalogMonitor::new(&[]).unwrap();
        let request = PromptRequest::new(
            acp_session_id.clone(),
            vec![agentkit_acp::ContentBlock::Text(
                agentkit_acp::TextContent::new("start one background call"),
            )],
        );
        let prompt = drive_prompt(
            &acp_session_id,
            &[],
            &integration,
            &mut skill_catalog,
            &mut driver,
            request,
            &tasks,
            &background_jobs,
            true,
        );
        tokio::pin!(prompt);

        timeout(Duration::from_secs(1), async {
            tokio::select! {
                response = &mut prompt => panic!("structured prompt resolved before its task started: {response:?}"),
                _ = async {
                    while !entered.load(Ordering::SeqCst) {
                        tokio::task::yield_now().await;
                    }
                    assert!(detach_compose_call(
                        &tasks,
                        &background_jobs,
                        "background-call",
                    ).await);
                    background_jobs.register_foreground_for_test("background-call");
                    while turns.load(Ordering::SeqCst) < 2 {
                        tokio::task::yield_now().await;
                    }
                } => {}
            }
        })
        .await
        .expect("structured prompt did not reach its provisional response");
        assert!(
            timeout(Duration::from_millis(20), &mut prompt)
                .await
                .is_err()
        );

        // The compose guard can disappear before the task manager publishes its
        // result. Structured completion must wait through that handoff.
        background_jobs.finish_for_test("background-call");
        assert!(
            timeout(Duration::from_millis(20), &mut prompt)
                .await
                .is_err(),
            "structured prompt crossed the terminal publication handoff early"
        );
        release.notify_one();
        let response = timeout(Duration::from_secs(1), &mut prompt)
            .await
            .expect("structured prompt did not synthesize the background result")
            .unwrap();
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(turns.load(Ordering::SeqCst), 3);
        drain.abort();
    }

    #[tokio::test]
    async fn foreground_compose_detaches_out_of_band_and_completes_autonomously() {
        let turns = Arc::new(AtomicUsize::new(0));
        let user_items_seen = Arc::new(AtomicUsize::new(0));
        let notification_items_seen = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Notify::new());
        let integration = Arc::new(
            AcpIntegration::builder()
                .name("actor-test")
                .approval_resolver(AutoDenyResolver)
                .build()
                .unwrap(),
        );
        let acp_session_id = agentkit_acp::SessionId::new("s-actor-session");
        let agentkit_session_id = AgentkitSessionId::new("s-actor-session");
        let (client, mut messages) = AcpClientHandle::channel();
        let cancellation = CancellationController::new();
        integration
            .bind_session(
                AcpSessionBinding::new(acp_session_id.clone(), agentkit_session_id.clone(), client)
                    .cancellation(cancellation.clone()),
            )
            .unwrap();

        let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
        let drain = tokio::spawn(async move {
            while let Some(message) = messages.recv().await {
                match message {
                    AcpClientMessage::SessionNotification(notification) => {
                        let _ = updates_tx.send(notification);
                    }
                    AcpClientMessage::Flush { response } => {
                        let _ = response.send(());
                    }
                    AcpClientMessage::PermissionRequest { .. } => {
                        panic!("auto-deny must not ask the ACP client for permission");
                    }
                }
            }
        });

        let task_manager = AsyncTaskManager::new()
            .routing(|_request: &agentkit_tools_core::ToolRequest| RoutingDecision::Foreground);
        let tasks = task_manager.handle();
        let background_jobs = BackgroundJobs::default();
        let tools = ToolRegistry::new().with(BlockingTool {
            spec: ToolSpec {
                name: ToolName::new(agentkit_tool_compose::COMPOSE_TOOL_NAME),
                description: "controlled compose tool".into(),
                input_schema: json!({"type": "object", "additionalProperties": false}),
                output_schema: None,
                annotations: ToolAnnotations::default(),
                metadata: MetadataMap::new(),
            },
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        });
        let driver = Agent::builder()
            .model(ScriptAdapter {
                turns: Arc::clone(&turns),
                user_items_seen: Arc::clone(&user_items_seen),
                notification_items_seen: Arc::clone(&notification_items_seen),
            })
            .add_tool_source(tools)
            .task_manager(task_manager)
            .observer(integration.as_ref().clone())
            .cancellation(cancellation.handle())
            .build()
            .unwrap()
            .start(SessionConfig::new(agentkit_session_id).without_cache())
            .await
            .unwrap();
        let (commands_tx, commands_rx) = mpsc::channel(8);
        let (turn_states_tx, mut turn_states_rx) = mpsc::unbounded_channel();
        let test_mcp = crate::tools::mcp::empty();
        let mcp_events = test_mcp.subscribe(acp_session_id.to_string());
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
        let skills = runtime.current_skills().await.unwrap();
        let actor = tokio::spawn(session_actor(SessionActor {
            session_id: acp_session_id.clone(),
            runtime,
            integration: Arc::clone(&integration),
            binding: SessionBindingGuard::new(Arc::clone(&integration), acp_session_id.clone()),
            driver,
            tasks: tasks.clone(),
            background_jobs: background_jobs.clone(),
            structured_completion: false,
            skill_catalog: skill_catalog::SkillCatalogMonitor::new(&skills.skills).unwrap(),
            adapter: SelectableAdapter::new(crate::ProviderKind::OpenAiSubscription, "gpt-5.4")
                .unwrap(),
            catalog: Vec::new(),
            commands: commands_rx,
            turn_states: turn_states_tx,
            mcp_events,
        }));

        let (reply_tx, reply_rx) = oneshot::channel();
        commands_tx
            .send(Command::Prompt {
                request: PromptRequest::new(
                    acp_session_id.clone(),
                    vec![agentkit_acp::ContentBlock::Text(
                        agentkit_acp::TextContent::new("start one background call"),
                    )],
                ),
                reply: reply_tx,
            })
            .await
            .unwrap();
        while !entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        assert!(detach_compose_call(&tasks, &background_jobs, "background-call").await);
        background_jobs.register_foreground_for_test("background-call");
        assert!(background_jobs.is_detached_for_test("background-call"));
        timeout(Duration::from_secs(1), reply_rx)
            .await
            .expect("prompt remained blocked after the out-of-band detach")
            .unwrap()
            .unwrap();
        assert_eq!(turns.load(Ordering::SeqCst), 2);

        release.notify_one();
        let completed = timeout(Duration::from_secs(1), async {
            loop {
                let completed = tasks.list_completed().await;
                if !completed.is_empty() {
                    break completed;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached compose did not complete");
        assert_eq!(
            completed[0].kind,
            agentkit_task_manager::TaskKind::Background
        );
        background_jobs.finish_for_test("background-call");
        let notification = timeout(Duration::from_secs(1), async {
            loop {
                let notification = updates_rx.recv().await.expect("update stream closed");
                if let agentkit_acp::SessionUpdate::AgentMessageChunk(chunk) = &notification.update
                    && let agentkit_acp::ContentBlock::Text(text) = &chunk.content
                    && text.text == "autonomous background completion"
                {
                    break notification;
                }
            }
        })
        .await
        .expect("completion did not produce an unsolicited agent update");
        assert_eq!(notification.session_id.to_string(), "s-actor-session");
        let started = turn_states_rx.recv().await.expect("missing turn start");
        let ended = turn_states_rx.recv().await.expect("missing turn end");
        assert!(started.active);
        assert!(!ended.active);
        assert_eq!(started.turn_id, ended.turn_id);
        assert_eq!(started.session_id, acp_session_id);
        assert_eq!(turns.load(Ordering::SeqCst), 3);
        assert_eq!(
            user_items_seen.load(Ordering::SeqCst),
            1,
            "autonomous progress must not insert synthetic user content"
        );

        test_mcp.publish(
            &acp_session_id.to_string(),
            crate::tools::mcp::McpEvent {
                message: "MCP server linear connected".into(),
            },
        );
        let mcp_started = turn_states_rx.recv().await.expect("missing MCP turn start");
        let mcp_ended = turn_states_rx.recv().await.expect("missing MCP turn end");
        assert!(mcp_started.active);
        assert!(!mcp_ended.active);
        assert_eq!(turns.load(Ordering::SeqCst), 4);
        assert_eq!(notification_items_seen.load(Ordering::SeqCst), 2);
        assert_eq!(user_items_seen.load(Ordering::SeqCst), 1);

        let (close_tx, close_rx) = oneshot::channel();
        commands_tx
            .send(Command::Close { reply: close_tx })
            .await
            .unwrap();
        timeout(Duration::from_secs(1), close_rx)
            .await
            .unwrap()
            .unwrap();
        let (client, _messages) = AcpClientHandle::channel();
        integration
            .bind_session(AcpSessionBinding::new(
                acp_session_id.clone(),
                AgentkitSessionId::new(acp_session_id.to_string()),
                client,
            ))
            .expect("actor exit left the durable identity bound");
        integration.unbind_session(&acp_session_id).unwrap();
        timeout(Duration::from_secs(1), actor)
            .await
            .unwrap()
            .unwrap();
        drain.abort();
    }

    #[test]
    fn model_config_switches_the_shared_session_adapter() {
        let adapter =
            SelectableAdapter::new(crate::ProviderKind::OpenAiSubscription, "gpt-5.4").unwrap();
        let catalog = vec![ModelGroup {
            provider: crate::ProviderKind::OpenAiSubscription,
            models: vec!["gpt-5.4".into(), "gpt-5.4-mini".into()],
        }];

        let response = set_config(
            &adapter,
            &catalog,
            SetSessionConfigOptionRequest::new(
                "session",
                MODEL_CONFIG_ID,
                "openai-subscription:gpt-5.4-mini",
            ),
        )
        .unwrap();

        assert_eq!(adapter.selection().unwrap().model, "gpt-5.4-mini");
        assert_eq!(adapter.reasoning_effort().unwrap(), None);
        assert_eq!(response.config_options.len(), 2);
    }

    #[test]
    fn reasoning_effort_config_updates_independently_and_advertises_thought_level() {
        let adapter =
            SelectableAdapter::new(crate::ProviderKind::OpenAiSubscription, "gpt-5.4").unwrap();
        let catalog = vec![ModelGroup {
            provider: crate::ProviderKind::OpenAiSubscription,
            models: vec!["gpt-5.4".into()],
        }];

        let response = set_config(
            &adapter,
            &catalog,
            SetSessionConfigOptionRequest::new("session", REASONING_EFFORT_CONFIG_ID, "high"),
        )
        .unwrap();

        assert_eq!(adapter.selection().unwrap().model, "gpt-5.4");
        assert_eq!(
            adapter.reasoning_effort().unwrap(),
            Some(ReasoningEffort::High)
        );
        assert_eq!(response.config_options.len(), 2);
        let options = serde_json::to_value(response.config_options).unwrap();
        assert_eq!(options[1]["id"], REASONING_EFFORT_CONFIG_ID);
        assert_eq!(options[1]["category"], "thought_level");
        assert_eq!(options[1]["currentValue"], "high");

        let response = set_config(
            &adapter,
            &catalog,
            SetSessionConfigOptionRequest::new("session", REASONING_EFFORT_CONFIG_ID, "default"),
        )
        .unwrap();
        assert_eq!(adapter.reasoning_effort().unwrap(), None);
        let options = serde_json::to_value(response.config_options).unwrap();
        assert_eq!(options[1]["currentValue"], "default");
    }

    #[tokio::test]
    async fn close_then_load_replays_before_response_and_returns_config_options() {
        let root = tempfile::tempdir().unwrap();
        let session_id = crate::session::new_id();
        let opened = crate::session::open(
            root.path(),
            &session_id,
            false,
            false,
            vec![
                Item::text(ItemKind::System, "system"),
                Item::text(ItemKind::User, "remember me"),
                Item::text(ItemKind::Assistant, "remembered"),
            ],
        )
        .unwrap();
        drop(opened);

        let credentials = crate::credentials::CredentialStorage::Memory;
        crate::provider::store_openrouter_test_credentials(&credentials);
        let runtime = Runtime::new_with_provider_credentials_and_effort(
            root.path(),
            "test-model",
            crate::ProviderKind::OpenRouter,
            credentials,
            None,
        )
        .unwrap();
        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn(serve_transport(runtime, agent_transport));
        let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
        let workspace = root.path().to_path_buf();
        let cleanup_session_id = session_id.clone();

        agent_client_protocol::Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    let _ = updates_tx.send(notification.update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, async move |connection| {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let first = connection
                    .send_request(LoadSessionRequest::new(
                        session_id.clone(),
                        workspace.clone(),
                    ))
                    .block_task()
                    .await?;
                let first_options = first.config_options.expect("load config options");
                assert!(!first_options.is_empty());
                assert!(matches!(
                    updates_rx.try_recv(),
                    Ok(SessionUpdate::UserMessageChunk(_))
                ));
                assert!(matches!(
                    updates_rx.try_recv(),
                    Ok(SessionUpdate::AgentMessageChunk(_))
                ));
                assert!(matches!(
                    timeout(Duration::from_secs(1), updates_rx.recv())
                        .await
                        .expect("available commands update timed out"),
                    Some(SessionUpdate::AvailableCommandsUpdate(_))
                ));

                connection
                    .send_request(CloseSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;

                let second = connection
                    .send_request(LoadSessionRequest::new(
                        session_id.clone(),
                        workspace.clone(),
                    ))
                    .block_task()
                    .await?;
                assert_eq!(second.config_options, Some(first_options));
                assert!(matches!(
                    updates_rx.try_recv(),
                    Ok(SessionUpdate::UserMessageChunk(_))
                ));
                assert!(matches!(
                    updates_rx.try_recv(),
                    Ok(SessionUpdate::AgentMessageChunk(_))
                ));
                assert!(matches!(
                    timeout(Duration::from_secs(1), updates_rx.recv())
                        .await
                        .expect("available commands update timed out"),
                    Some(SessionUpdate::AvailableCommandsUpdate(_))
                ));
                connection
                    .send_request(CloseSessionRequest::new(session_id.clone()))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();

        server.abort();
        let _ = server.await;
        if let Some(home) = std::env::var_os("HOME") {
            let directory = PathBuf::from(home).join(".kit/sessions");
            let _ = std::fs::remove_file(directory.join(format!("{cleanup_session_id}.jsonl")));
            let _ = std::fs::remove_file(directory.join(format!("{cleanup_session_id}.lock")));
        }
    }

    #[tokio::test]
    async fn registry_shutdown_closes_real_session_and_rejects_reattach() {
        let root = tempfile::tempdir().unwrap();
        let session_id = crate::session::new_id();
        let opened = crate::session::open(
            root.path(),
            &session_id,
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        drop(opened);

        let credentials = crate::credentials::CredentialStorage::Memory;
        crate::provider::store_openrouter_test_credentials(&credentials);
        let runtime = Runtime::new_with_provider_credentials_and_effort(
            root.path(),
            "test-model",
            crate::ProviderKind::OpenRouter,
            credentials,
            None,
        )
        .unwrap();
        let registry = SessionRegistry::new();
        let (client_transport, agent_transport) = Channel::duplex();
        let server_registry = registry.clone();
        let server_runtime = Arc::clone(&runtime);
        let server = tokio::spawn(async move {
            component(server_runtime, server_registry)
                .unwrap()
                .connect_to(agent_transport)
                .await
        });
        let workspace = root.path().to_path_buf();
        let client_session_id = session_id.clone();
        let (attached, attached_rx) = oneshot::channel();
        let (proceed, proceed_rx) = oneshot::channel();
        let client = tokio::spawn(async move {
            agent_client_protocol::Client
                .builder()
                .connect_with(client_transport, async move |connection| {
                    connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    connection
                        .send_request(LoadSessionRequest::new(
                            client_session_id.clone(),
                            workspace.clone(),
                        ))
                        .block_task()
                        .await?;
                    let _ = attached.send(());
                    let _ = proceed_rx.await;

                    let rejected = connection
                        .send_request(LoadSessionRequest::new(client_session_id, workspace))
                        .block_task()
                        .await;
                    assert!(
                        rejected.is_err(),
                        "closed registry accepted a new attachment"
                    );
                    Ok(())
                })
                .await
        });

        // Starting a real ACP session can exceed two seconds when the full test suite
        // is competing for CPU on CI. Keep a generous bound so genuine hangs still fail.
        timeout(Duration::from_secs(10), attached_rx)
            .await
            .expect("real ACP session did not attach")
            .unwrap();
        assert_eq!(
            registry
                .inner
                .state
                .lock()
                .expect("ACP session registry poisoned")
                .sessions
                .len(),
            1
        );

        registry.shutdown_with_timeout(Duration::from_secs(1)).await;
        {
            let state = registry
                .inner
                .state
                .lock()
                .expect("ACP session registry poisoned");
            assert!(!state.accepting);
            assert!(state.sessions.is_empty());
        }
        proceed.send(()).unwrap();
        timeout(Duration::from_secs(10), client)
            .await
            .expect("ACP client did not observe shutdown")
            .unwrap()
            .unwrap();

        let claim = runtime
            .claim_session_load(&session_id)
            .expect("shutdown left the durable session identity claimed");
        drop(claim);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn load_errors_do_not_poison_the_next_new_session() {
        let root = tempfile::tempdir().unwrap();
        let selected_id = crate::session::new_id();
        let active_id = crate::session::new_id();
        let active = crate::session::open(
            root.path(),
            &active_id,
            false,
            false,
            vec![Item::text(ItemKind::System, "system")],
        )
        .unwrap();
        crate::provider::store_openrouter_test_credentials(
            &crate::credentials::CredentialStorage::Memory,
        );
        let runtime = Runtime::with_session_and_provider(
            root.path(),
            "test-model",
            crate::ProviderKind::OpenRouter,
            crate::runtime::SessionRequest {
                id: selected_id.clone(),
                resume: false,
                force: true,
            },
        )
        .unwrap();
        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn(serve_transport(runtime, agent_transport));
        let workspace = root.path().to_path_buf();
        let requested_selected_id = selected_id.clone();
        let requested_active_id = active_id.clone();

        agent_client_protocol::Client
            .builder()
            .on_receive_notification(
                async move |_notification: SessionNotification, _cx| Ok(()),
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(client_transport, async move |connection| {
                connection
                    .send_request(LoadSessionRequest::new(
                        "missing-session",
                        workspace.clone(),
                    ))
                    .block_task()
                    .await
                    .expect_err("unknown session load must fail");
                connection
                    .send_request(LoadSessionRequest::new(
                        requested_active_id,
                        workspace.clone(),
                    ))
                    .block_task()
                    .await
                    .expect_err("actively locked session load must fail");
                connection
                    .send_request(LoadSessionRequest::new(
                        requested_selected_id.clone(),
                        workspace.clone(),
                    ))
                    .block_task()
                    .await
                    .expect_err("missing matching configured session must fail to load");

                let created = connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;
                assert_eq!(created.session_id.to_string(), requested_selected_id);
                connection
                    .send_request(CloseSessionRequest::new(created.session_id))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();

        drop(active);
        server.abort();
        let _ = server.await;
        if let Some(home) = std::env::var_os("HOME") {
            let directory = PathBuf::from(home).join(".kit/sessions");
            for id in [selected_id, active_id] {
                let _ = std::fs::remove_file(directory.join(format!("{id}.jsonl")));
                let _ = std::fs::remove_file(directory.join(format!("{id}.lock")));
            }
        }
    }

    #[test]
    fn session_list_cursors_parse_offsets_and_reject_malformed_values() {
        assert_eq!(parse_session_list_cursor("offset:100").unwrap(), 100);
        assert!(parse_session_list_cursor("100").is_err());
        assert!(parse_session_list_cursor("offset:nope").is_err());
        assert_eq!(
            list_sessions_error(ListSessionsError::InvalidCursor).code,
            agent_client_protocol::ErrorCode::InvalidParams
        );
    }

    #[tokio::test]
    async fn kit_server_advertises_supported_session_discovery_restoration_and_forking() {
        let root = tempfile::tempdir().unwrap();
        let credentials = crate::credentials::CredentialStorage::Memory;
        crate::provider::store_openrouter_test_credentials(&credentials);
        let runtime = Runtime::new_with_provider_and_credentials(
            root.path(),
            "test-model",
            crate::ProviderKind::OpenRouter,
            credentials,
        )
        .unwrap();
        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn(serve_transport(runtime, agent_transport));

        agent_client_protocol::Client
            .builder()
            .connect_with(client_transport, async move |connection| {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V2))
                    .block_task()
                    .await?;
                assert_eq!(initialized.protocol_version, ProtocolVersion::V1);
                assert!(initialized.agent_capabilities.load_session);
                let sessions = &initialized.agent_capabilities.session_capabilities;
                assert!(sessions.list.is_some());
                assert!(sessions.resume.is_none());
                assert!(sessions.fork.is_some());

                let listed = connection
                    .send_request(ListSessionsRequest::new().cwd(root.path().to_path_buf()))
                    .block_task()
                    .await?;
                assert!(listed.sessions.is_empty());
                let error = connection
                    .send_request(
                        ListSessionsRequest::new()
                            .cwd(root.path().join("other"))
                            .cursor("invalid"),
                    )
                    .block_task()
                    .await
                    .expect_err("malformed list cursor must fail");
                assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);

                connection
                    .send_request(ForkSessionRequest::new(
                        "missing",
                        root.path().to_path_buf(),
                    ))
                    .block_task()
                    .await
                    .expect_err("an unknown fork source must fail");

                let source = connection
                    .send_request(NewSessionRequest::new(root.path().to_path_buf()))
                    .block_task()
                    .await?;
                let configured = connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        source.session_id.clone(),
                        REASONING_EFFORT_CONFIG_ID,
                        "high",
                    ))
                    .block_task()
                    .await?;
                let fork = connection
                    .send_request(ForkSessionRequest::new(
                        source.session_id.clone(),
                        root.path().to_path_buf(),
                    ))
                    .block_task()
                    .await?;
                assert_ne!(fork.session_id, source.session_id);
                assert_eq!(fork.config_options, Some(configured.config_options));
                connection
                    .send_request(CloseSessionRequest::new(fork.session_id))
                    .block_task()
                    .await?;
                connection
                    .send_request(CloseSessionRequest::new(source.session_id))
                    .block_task()
                    .await?;
                Ok(())
            })
            .await
            .unwrap();

        server.abort();
        let _ = server.await;
    }
}
