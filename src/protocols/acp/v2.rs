use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use agent_client_protocol::{Client, ConnectTo, Handled, Responder, V2ConnectionTo};
use agentkit_acp::{
    AcpRuntimeError,
    v2::{
        AcpInjectionBoundary, AcpIntegration, AcpSessionBinding, AcpSessionHandle,
        AcpSessionUpdateSink, wire,
    },
};
use agentkit_core::{
    CancellationController, Delta, FinishReason, Item, ItemKind, Part, PartId, PartKind, SessionId,
    ToolOutput, Usage,
};
use agentkit_loop::{
    AgentEvent, LoopDriver, LoopError, LoopInterrupt, LoopObserver, LoopStep, ModelSession,
    ObservedEvent,
};
use agentkit_task_manager::{TaskEvent, TaskManagerHandle};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    provider::{ProviderKind, SelectableAdapter, authentication_method_id},
    runtime::{AcpDriverContext, BackgroundJobs, Runtime},
};

use super::activity::{ExecutionOrigin, SessionActivity};

use super::{
    AuthenticationRequiredData, CancelBackgroundRequest, CancelBackgroundResponse,
    DetachComposeRequest, DetachComposeResponse, FileSearchRequest, FileSearchResponse,
    SessionRegistry, skill_catalog, terminal_auth_method_specs,
};

const PAGE_SIZE: usize = 100;

static NEXT_ERROR_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_THOUGHT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);

fn available_commands_update(session_id: wire::SessionId) -> wire::UpdateSessionNotification {
    wire::UpdateSessionNotification::new(
        session_id,
        wire::SessionUpdate::AvailableCommandsUpdate(wire::AvailableCommandsUpdate::new(vec![
            wire::AvailableCommand::new("compact", "Compact the session context"),
        ])),
    )
}

fn complete_new_session<E>(
    response: wire::NewSessionResponse,
    activation: oneshot::Sender<()>,
    respond: impl FnOnce(wire::NewSessionResponse) -> Result<(), E>,
    notify: impl FnOnce(wire::UpdateSessionNotification) -> Result<(), E>,
) -> Result<(), E> {
    let session_id = response.session_id.clone();
    respond(response)?;
    let _ = activation.send(());
    notify(available_commands_update(session_id))
}

fn sdk_error(error: AcpRuntimeError) -> agent_client_protocol::Error {
    let detail = error.to_string();
    match authentication_method_id(&detail) {
        Some(method_id) => agent_client_protocol::Error::auth_required()
            .data(AuthenticationRequiredData::new(method_id, &detail).into_value()),
        None => agent_client_protocol::util::internal_error(detail),
    }
}

fn terminal_auth_methods(
    capabilities: &wire::ClientCapabilities,
    provider_available: impl Fn(ProviderKind) -> bool,
) -> Vec<wire::AuthMethod> {
    let supports_terminal_auth = capabilities
        .auth
        .as_ref()
        .and_then(|auth| auth.terminal.as_ref())
        .is_some();
    if !supports_terminal_auth {
        return Vec::new();
    }

    terminal_auth_method_specs()
        .iter()
        .filter(|method| provider_available(method.provider))
        .map(|method| {
            wire::AuthMethod::Terminal(
                wire::AuthMethodTerminal::new(method.method_id, method.name)
                    .description(method.description)
                    .args(vec![
                        "--terminal-auth-login".into(),
                        method.method_id.into(),
                    ]),
            )
        })
        .collect()
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

fn map_loop_error(session_id: &wire::SessionId, error: &LoopError) -> AcpRuntimeError {
    if matches!(error, LoopError::Cancelled) {
        AcpRuntimeError::Cancelled
    } else {
        let session_id = agentkit_acp::SessionId::new(session_id.to_string());
        super::record_acp_loop_failure(&session_id, error)
    }
}

fn loop_error_stop_reason(
    session_id: &wire::SessionId,
    error: &LoopError,
) -> Result<FinishReason, AcpRuntimeError> {
    if matches!(error, LoopError::Cancelled) {
        Ok(FinishReason::Cancelled)
    } else {
        Err(map_loop_error(session_id, error))
    }
}

// Admission excludes competing requests while the actor prepares or drains work.
// It is not observable activity: an admitted autonomous drive may find no work.
// SessionActivity alone owns the ACP lifecycle projected to clients.
fn claim_prompt(busy: &AtomicBool) -> Result<(), AcpRuntimeError> {
    busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map(|_| ())
        .map_err(|_| AcpRuntimeError::Unsupported("session is already running a prompt".into()))
}

#[derive(Clone)]
struct ConnectionSink(V2ConnectionTo<Client>);

#[async_trait]
impl AcpSessionUpdateSink for ConnectionSink {
    fn update(&self, notification: wire::UpdateSessionNotification) -> Result<(), AcpRuntimeError> {
        self.0
            .send_notification(notification)
            .map_err(|error| AcpRuntimeError::Sdk(error.to_string()))
    }

    async fn update_acknowledged(
        &self,
        notification: wire::UpdateSessionNotification,
    ) -> Result<(), AcpRuntimeError> {
        self.update(notification)
    }

    async fn flush(&self) -> Result<(), AcpRuntimeError> {
        Ok(())
    }
}

#[derive(Default)]
struct CurrentReplacementMessages {
    agent: Option<wire::MessageId>,
    thoughts: Vec<wire::MessageId>,
    thought_parts: HashMap<PartId, wire::MessageId>,
    pending_thought: Option<wire::MessageId>,
    replacement: Option<ReplacementGeneration>,
}

struct ReplacementGeneration {
    id: u64,
    agent: Option<wire::MessageId>,
    thought: Option<wire::MessageId>,
}

impl ReplacementGeneration {
    fn new() -> Self {
        static NEXT_REPLACEMENT_GENERATION: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT_REPLACEMENT_GENERATION.fetch_add(1, Ordering::Relaxed),
            agent: None,
            thought: None,
        }
    }

    fn message_id(&mut self, kind: &'static str) -> wire::MessageId {
        let current = match kind {
            "agent" => &mut self.agent,
            "thought" => &mut self.thought,
            _ => unreachable!("replacement message kind is fixed"),
        };
        current
            .get_or_insert_with(|| {
                wire::MessageId::new(format!("kit-response-replacement-{}-{kind}", self.id))
            })
            .clone()
    }
}

#[derive(Clone)]
struct ResponseReplacementSink<S> {
    inner: S,
    current: Arc<Mutex<CurrentReplacementMessages>>,
}

impl<S> ResponseReplacementSink<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            current: Arc::new(Mutex::new(CurrentReplacementMessages::default())),
        }
    }

    // AgentKit's ACP v2 adapter groups every reasoning part between tool boundaries
    // under one message ID. Carry the current PartId across its synchronous observer-to-sink
    // call so clients can render each reasoning part as a separate thought block.
    fn prepare_content_delta(&self, delta: &Delta) {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        current.pending_thought = match delta {
            Delta::BeginPart {
                part_id,
                kind: PartKind::Reasoning,
            } => {
                current
                    .thought_parts
                    .entry(part_id.clone())
                    .or_insert_with(|| {
                        let sequence = NEXT_THOUGHT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed);
                        wire::MessageId::new(format!("kit-thought-{sequence}"))
                    });
                None
            }
            Delta::AppendText { part_id, .. } => current.thought_parts.get(part_id).cloned(),
            _ => None,
        };
    }

    fn clear_pending_thought(&self) {
        self.current
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pending_thought = None;
    }

    fn rewrite_and_track(&self, notification: &mut wire::UpdateSessionNotification) {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match &mut notification.update {
            wire::SessionUpdate::AgentMessageChunk(chunk) => {
                if let Some(replacement) = current.replacement.as_mut() {
                    chunk.message_id = replacement.message_id("agent");
                }
                current.agent = Some(chunk.message_id.clone());
            }
            wire::SessionUpdate::AgentThoughtChunk(chunk) => {
                if let Some(message_id) = current.pending_thought.take() {
                    chunk.message_id = message_id;
                } else if let Some(replacement) = current.replacement.as_mut() {
                    chunk.message_id = replacement.message_id("thought");
                }
                if !current.thoughts.contains(&chunk.message_id) {
                    current.thoughts.push(chunk.message_id.clone());
                }
            }
            _ => {}
        }
    }

    fn reset(&self) {
        *self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = CurrentReplacementMessages::default();
    }
}

impl<S: AcpSessionUpdateSink> ResponseReplacementSink<S> {
    fn clear_current(&self, session_id: &wire::SessionId) -> Vec<Result<(), AcpRuntimeError>> {
        let (agent, thoughts) = {
            let mut current = self
                .current
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let agent = current.agent.take();
            let thoughts = std::mem::take(&mut current.thoughts);
            current.thought_parts.clear();
            current.pending_thought = None;
            current.replacement = Some(ReplacementGeneration::new());
            (agent, thoughts)
        };
        let mut results = Vec::with_capacity(usize::from(agent.is_some()) + thoughts.len());
        if let Some(message_id) = agent {
            results.push(self.inner.update(wire::UpdateSessionNotification::new(
                session_id.clone(),
                wire::SessionUpdate::AgentMessage(
                    wire::AgentMessage::new(message_id).content(Vec::new()),
                ),
            )));
        }
        for message_id in thoughts {
            results.push(self.inner.update(wire::UpdateSessionNotification::new(
                session_id.clone(),
                wire::SessionUpdate::AgentThought(
                    wire::AgentThought::new(message_id).content(Vec::new()),
                ),
            )));
        }
        results
    }
}

/// Native v2 is a projection of the same session lifecycle as v1.
fn native_activity<S: AcpSessionUpdateSink + 'static>(
    session_id: wire::SessionId,
    sink: S,
) -> SessionActivity {
    SessionActivity::new(move |transition| {
        let state = if transition.active {
            wire::StateUpdate::Running(wire::RunningStateUpdate::new())
        } else {
            wire::StateUpdate::Idle(
                wire::IdleStateUpdate::new()
                    .stop_reason(finish_reason_to_stop_reason(&transition.reason)),
            )
        };
        send_state(&sink, &session_id, state)
    })
}

#[async_trait]
impl<S: AcpSessionUpdateSink> AcpSessionUpdateSink for ResponseReplacementSink<S> {
    fn update(
        &self,
        mut notification: wire::UpdateSessionNotification,
    ) -> Result<(), AcpRuntimeError> {
        self.rewrite_and_track(&mut notification);
        self.inner.update(notification)
    }

    async fn update_acknowledged(
        &self,
        mut notification: wire::UpdateSessionNotification,
    ) -> Result<(), AcpRuntimeError> {
        self.rewrite_and_track(&mut notification);
        self.inner.update_acknowledged(notification).await
    }

    async fn flush(&self) -> Result<(), AcpRuntimeError> {
        self.inner.flush().await
    }
}

#[derive(Clone)]
struct ResponseReplacementObserver<S> {
    inner: AcpIntegration,
    sink: ResponseReplacementSink<S>,
    activity: SessionActivity,
    session_id: wire::SessionId,
}

fn usage_update(usage: &Usage) -> Option<wire::UsageUpdate> {
    let tokens = usage.tokens.as_ref()?;
    let used = tokens.input_tokens.checked_add(tokens.output_tokens)?;
    let size = [
        "context_window",
        "context_window_tokens",
        "model.context_window",
        "model.context_length",
        "openrouter.context_length",
    ]
    .iter()
    .find_map(|key| usage.metadata.get(*key).and_then(|value| value.as_u64()))?;
    Some(wire::UsageUpdate::new(used, size))
}

impl<S> ResponseReplacementObserver<S> {
    fn new(
        inner: AcpIntegration,
        sink: ResponseReplacementSink<S>,
        session_id: wire::SessionId,
        activity: SessionActivity,
    ) -> Self {
        Self {
            inner,
            sink,
            activity,
            session_id,
        }
    }
}

impl<S: AcpSessionUpdateSink> ResponseReplacementObserver<S> {
    fn clear_current(&self) {
        for result in self.sink.clear_current(&self.session_id) {
            if let Err(error) = result {
                tracing::debug!(%error, "failed to queue ACP v2 session update");
            }
        }
    }
}

impl<S> LoopObserver for ResponseReplacementObserver<S>
where
    S: AcpSessionUpdateSink + Clone,
{
    fn handle_event(&self, event: ObservedEvent) {
        self.activity.observe(&event.event);
        if let AgentEvent::UsageUpdated(usage) = &event.event {
            let Some(update) = usage_update(usage) else {
                return;
            };
            let notification = wire::UpdateSessionNotification::new(
                self.session_id.clone(),
                wire::SessionUpdate::UsageUpdate(update),
            );
            if let Err(error) = self.sink.update(notification) {
                tracing::debug!(%error, "failed to queue ACP v2 usage update");
            }
            return;
        }
        // A cancelled turn leaves its last streamed attempt visible. Only an explicit
        // supersession event proves that the current attempt is stale and must be cleared.
        if matches!(&event.event, AgentEvent::ResponseAttemptSuperseded) {
            self.clear_current();
            return;
        }
        if matches!(
            &event.event,
            AgentEvent::TurnStarted { .. }
                | AgentEvent::TurnFinished(_)
                | AgentEvent::ToolExecutionStarted(_)
                | AgentEvent::ToolResultReceived(_)
        ) {
            self.sink.reset();
        }
        if let AgentEvent::ContentDelta(delta) = &event.event {
            self.sink.prepare_content_delta(delta);
        }
        self.inner.handle_event(event);
        self.sink.clear_pending_thought();
    }
}

struct PromptCommand {
    request: wire::PromptRequest,
    cancellation_generation: u64,
    reply: oneshot::Sender<Result<oneshot::Sender<()>, AcpRuntimeError>>,
}

enum Command {
    Prompt(PromptCommand),
    SetConfig {
        request: wire::SetSessionConfigOptionRequest,
        reply: oneshot::Sender<Result<wire::SetSessionConfigOptionResponse, AcpRuntimeError>>,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
}

struct SessionHandle {
    token: u64,
    commands: mpsc::Sender<Command>,
    integration: AcpSessionHandle,
    busy: Arc<AtomicBool>,
    background_jobs: BackgroundJobs,
    structured_completion: bool,
    tasks: TaskManagerHandle,
}

struct AttachedSession {
    session_id: wire::SessionId,
    config_options: Vec<wire::SessionConfigOption>,
    canonical_transcript: Vec<Item>,
    activation: oneshot::Sender<()>,
}

struct BindingGuard {
    integration: Arc<AcpIntegration>,
    session_id: wire::SessionId,
}

impl Drop for BindingGuard {
    fn drop(&mut self) {
        let _ = self.integration.unbind_session(&self.session_id);
    }
}

struct ActorGuard {
    server: Weak<Server>,
    registry: SessionRegistry,
    session_id: wire::SessionId,
    token: u64,
    completed: watch::Sender<bool>,
}

impl Drop for ActorGuard {
    fn drop(&mut self) {
        if let Some(server) = self.server.upgrade() {
            server.remove_session(&self.session_id, self.token);
        }
        self.registry.remove(self.token);
        self.completed.send_replace(true);
    }
}

#[derive(Debug)]
enum SessionPublicationError {
    AdmissionClosed,
    Commit(AcpRuntimeError),
}

struct PendingSessionPublication {
    token: u64,
    interrupt: Arc<dyn Fn() + Send + Sync>,
    close: super::CloseV2Session,
    actor: tokio::task::AbortHandle,
    completed: watch::Receiver<bool>,
    session_id: wire::SessionId,
    session: SessionHandle,
}

struct Server {
    runtime: Arc<Runtime>,
    integration: Arc<AcpIntegration>,
    registry: SessionRegistry,
    sessions: Mutex<HashMap<wire::SessionId, SessionHandle>>,
    file_search: Arc<Mutex<Option<crate::file_search::WorkspaceFileSearchState>>>,
}

impl Server {
    fn new(runtime: Arc<Runtime>, registry: SessionRegistry) -> Self {
        Self {
            runtime,
            integration: Arc::new(AcpIntegration::default()),
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

    fn login(
        &self,
        request: wire::LoginAuthRequest,
    ) -> Result<wire::LoginAuthResponse, agent_client_protocol::Error> {
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

    async fn logout(self: &Arc<Self>) -> Result<wire::LogoutAuthResponse, AcpRuntimeError> {
        super::logout_authentication(Arc::clone(&self.runtime), &self.registry).await?;
        Ok(wire::LogoutAuthResponse::new())
    }

    fn publish_session(
        &self,
        admission: &mut super::SessionAdmission,
        publication: PendingSessionPublication,
        commit: impl FnOnce() -> Result<(), AcpRuntimeError>,
    ) -> Result<(), SessionPublicationError> {
        let mut sessions = self.sessions.lock().expect("ACP v2 session map poisoned");
        self.registry
            .register_v2(
                admission,
                publication.token,
                publication.interrupt,
                publication.close,
                publication.actor,
                publication.completed,
            )
            .map_err(|()| SessionPublicationError::AdmissionClosed)?;
        if let Err(error) = commit() {
            self.registry.remove(publication.token);
            return Err(SessionPublicationError::Commit(error));
        }
        sessions.insert(publication.session_id, publication.session);
        Ok(())
    }

    fn remove_session(&self, session_id: &wire::SessionId, token: u64) {
        let mut sessions = self.sessions.lock().expect("ACP v2 session map poisoned");
        if sessions
            .get(session_id)
            .is_some_and(|session| session.token == token)
        {
            sessions.remove(session_id);
        }
    }

    fn initialize(
        &self,
        request: wire::InitializeRequest,
    ) -> Result<wire::InitializeResponse, AcpRuntimeError> {
        if request.protocol_version < wire::ProtocolVersion::V2 {
            return Err(AcpRuntimeError::Unsupported(
                "ACP v2 requires protocol version 2 or newer".into(),
            ));
        }
        Ok(wire::InitializeResponse::new(
            wire::ProtocolVersion::V2,
            wire::Implementation::new("kit", env!("CARGO_PKG_VERSION")),
        )
        .capabilities(agentkit_acp::v2::agent_capabilities())
        .auth_methods(if self.runtime.supports_logout_authentication() {
            terminal_auth_methods(&request.capabilities, |provider| {
                self.runtime.supports_terminal_authentication(provider)
            })
        } else {
            Vec::new()
        }))
    }

    async fn new_session(
        self: &Arc<Self>,
        request: wire::NewSessionRequest,
        connection: V2ConnectionTo<Client>,
    ) -> Result<(wire::NewSessionResponse, oneshot::Sender<()>), AcpRuntimeError> {
        let claim = self.runtime.claim_session()?;
        let attached = self
            .attach_session(
                request.cwd.0,
                request
                    .additional_directories
                    .into_iter()
                    .map(|path| path.0)
                    .collect(),
                connection,
                claim,
            )
            .await?;
        let AttachedSession {
            session_id,
            config_options,
            activation,
            ..
        } = attached;
        Ok((
            wire::NewSessionResponse::new(session_id).config_options(config_options),
            activation,
        ))
    }

    async fn resume_session(
        self: &Arc<Self>,
        request: wire::ResumeSessionRequest,
        connection: V2ConnectionTo<Client>,
    ) -> Result<
        (
            wire::ResumeSessionResponse,
            Vec<wire::UpdateSessionNotification>,
            oneshot::Sender<()>,
        ),
        AcpRuntimeError,
    > {
        let replay = match request.replay_from {
            None => false,
            Some(wire::ReplayFrom::Start(_)) => true,
            Some(_) => {
                return Err(AcpRuntimeError::Unsupported(
                    "unsupported ACP v2 replay cursor".into(),
                ));
            }
        };
        let claim = self
            .runtime
            .claim_session_load(&request.session_id.to_string())?;
        if !claim.is_configured()
            && !crate::session::belongs_to_workspace(
                self.runtime.root(),
                &request.session_id.to_string(),
            )
            .map_err(AcpRuntimeError::Loop)?
        {
            return Err(AcpRuntimeError::SessionNotFound(
                request.session_id.to_string(),
            ));
        }
        let attached = self
            .attach_session(
                request.cwd.0,
                request
                    .additional_directories
                    .into_iter()
                    .map(|path| path.0)
                    .collect(),
                connection,
                claim,
            )
            .await?;
        let updates = if replay {
            transcript_replay(&attached.session_id, &attached.canonical_transcript)
        } else {
            Vec::new()
        };
        Ok((
            wire::ResumeSessionResponse::new().config_options(attached.config_options),
            updates,
            attached.activation,
        ))
    }

    async fn list_sessions(
        &self,
        request: wire::ListSessionsRequest,
    ) -> Result<wire::ListSessionsResponse, ListSessionsError> {
        let cwd = self.runtime.root().to_path_buf();
        let offset = request
            .cursor
            .as_ref()
            .map(|cursor| parse_cursor(cursor.as_ref()))
            .transpose()?
            .unwrap_or(0);
        if request
            .cwd
            .as_ref()
            .is_some_and(|requested| requested.0 != cwd)
        {
            return Ok(wire::ListSessionsResponse::new(Vec::new()));
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
        let end = catalog.len().min(offset + PAGE_SIZE);
        let sessions = catalog[offset..end]
            .iter()
            .map(|entry| catalog_session_info(entry, &cwd))
            .collect();
        let next =
            (end < catalog.len()).then(|| wire::SessionListCursor::new(format!("offset:{end}")));
        Ok(wire::ListSessionsResponse::new(sessions).next_cursor(next))
    }

    async fn attach_session(
        self: &Arc<Self>,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        connection: V2ConnectionTo<Client>,
        mut claim: crate::runtime::SessionClaim,
    ) -> Result<AttachedSession, AcpRuntimeError> {
        let mut admission = self
            .registry
            .begin_attachment()
            .map_err(|()| AcpRuntimeError::ClientClosed)?;
        let session_id = wire::SessionId::new(claim.id());
        let cancellation = CancellationController::new();
        let sink = ResponseReplacementSink::new(ConnectionSink(connection));
        let activity = native_activity(session_id.clone(), sink.clone());
        let binding =
            AcpSessionBinding::new(session_id.clone(), SessionId::new(claim.id()), sink.clone())
                .cancellation(cancellation);
        let handle = self.integration.bind_session(binding)?;
        let binding = BindingGuard {
            integration: Arc::clone(&self.integration),
            session_id: session_id.clone(),
        };
        let observer = ResponseReplacementObserver::new(
            self.integration.as_ref().clone(),
            sink.clone(),
            session_id.clone(),
            activity.clone(),
        );
        let context = AcpDriverContext {
            cwd,
            additional_directories,
            integration: Arc::new(observer),
            cancellation: handle.cancellation_handle(),
            response_attempt_replacement: true,
        };
        let driver = self.runtime.start_acp_driver(context, &mut claim).await?;
        let current = driver.adapter.selection().map_err(AcpRuntimeError::Loop)?;
        let reasoning = driver
            .adapter
            .reasoning_effort()
            .map_err(AcpRuntimeError::Loop)?;
        let catalog = driver.adapter.model_catalog(&current).await;
        let config_options = v2_config_options(&current, reasoning, &catalog);
        let canonical_transcript = driver.canonical_transcript;
        let skill_catalog = skill_catalog::SkillCatalogMonitor::new(&driver.skills)
            .map_err(|error| AcpRuntimeError::Loop(format!("skill catalog error: {error}")))?;
        let background_jobs = driver.background_jobs.clone();
        let tasks = driver.tasks.clone();
        let structured_completion = driver.structured_completion;
        let mcp_events = self.runtime.subscribe_mcp(session_id.to_string());
        let (tx, rx) = mpsc::channel(8);
        let busy = Arc::new(AtomicBool::new(false));
        let actor = SessionActor {
            session_id: session_id.clone(),
            runtime: Arc::clone(&self.runtime),
            integration: Arc::clone(&self.integration),
            handle: handle.clone(),
            busy: Arc::clone(&busy),
            binding,
            activity,
            sink,
            driver: driver.driver,
            tasks: driver.tasks,
            background_jobs: background_jobs.clone(),
            structured_completion,
            skill_catalog,
            adapter: driver.adapter,
            catalog,
            commands: rx,
            mcp_events,
        };
        let token = self.registry.next_token();
        let (activation, activated) = oneshot::channel();
        let (completed, completion) = watch::channel(false);
        let guard = ActorGuard {
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
        let interrupt_handle = handle.clone();
        let interrupt_background_jobs = background_jobs.clone();
        let interrupt = Arc::new(move || {
            interrupt_background_jobs.cancel_all();
            interrupt_handle.interrupt();
        });
        let weak = tx.downgrade();
        let close_background_jobs = background_jobs.clone();
        let close_tasks = tasks.clone();
        let close = Arc::new(move || {
            let close_background_jobs = close_background_jobs.clone();
            let close_tasks = close_tasks.clone();
            let weak = weak.clone();
            Box::pin(async move {
                super::cancel_background_jobs(&close_tasks, &close_background_jobs).await;
                if let Some(commands) = weak.upgrade() {
                    let (reply, acknowledged) = oneshot::channel();
                    if commands.send(Command::Close { reply }).await.is_ok() {
                        let _ = acknowledged.await;
                    }
                }
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        });
        // Registration, durable identity commit, and local publication are one
        // critical section so logout cannot reopen admission around a dead actor.
        let publication = self.publish_session(
            &mut admission,
            PendingSessionPublication {
                token,
                interrupt,
                close,
                actor: actor_task.abort_handle(),
                completed: completion,
                session_id: session_id.clone(),
                session: SessionHandle {
                    token,
                    commands: tx,
                    integration: handle,
                    busy,
                    background_jobs,
                    structured_completion,
                    tasks,
                },
            },
            || claim.commit(),
        );
        if let Err(error) = publication {
            drop(activation);
            actor_task.abort();
            let _ = actor_task.await;
            return Err(match error {
                SessionPublicationError::AdmissionClosed => AcpRuntimeError::ClientClosed,
                SessionPublicationError::Commit(error) => error,
            });
        }
        crate::events::emit(&crate::events::RuntimeEvent::SessionStarted {
            session_id: session_id.to_string(),
        });
        drop(actor_task);
        Ok(AttachedSession {
            session_id,
            config_options,
            canonical_transcript,
            activation,
        })
    }

    async fn prepare_prompt(
        &self,
        request: wire::PromptRequest,
    ) -> Result<oneshot::Sender<()>, AcpRuntimeError> {
        let (sender, busy, handle, cancellation_generation) =
            self.prompt_sender(&request.session_id)?;
        let (reply, response) = oneshot::channel();
        if sender
            .send(Command::Prompt(PromptCommand {
                request,
                cancellation_generation,
                reply,
            }))
            .await
            .is_err()
        {
            handle.stop_injection_turn();
            busy.store(false, Ordering::Release);
            return Err(AcpRuntimeError::ClientClosed);
        }
        match response.await {
            Ok(response) => response,
            Err(_) => {
                handle.stop_injection_turn();
                busy.store(false, Ordering::Release);
                Err(AcpRuntimeError::ClientClosed)
            }
        }
    }

    fn prompt_sender(
        &self,
        session_id: &wire::SessionId,
    ) -> Result<
        (
            mpsc::Sender<Command>,
            Arc<AtomicBool>,
            AcpSessionHandle,
            u64,
        ),
        AcpRuntimeError,
    > {
        let sessions = self.sessions.lock().expect("ACP v2 session map poisoned");
        let session = sessions
            .get(session_id)
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))?;
        claim_prompt(&session.busy)?;
        let handle = session.integration.clone();
        handle.prepare_injection_turn();
        let generation = handle.cancellation_handle().generation();
        Ok((
            session.commands.clone(),
            Arc::clone(&session.busy),
            handle,
            generation,
        ))
    }

    async fn set_config(
        &self,
        request: wire::SetSessionConfigOptionRequest,
    ) -> Result<wire::SetSessionConfigOptionResponse, AcpRuntimeError> {
        let sender = self.sender(&request.session_id)?;
        let (reply, response) = oneshot::channel();
        sender
            .send(Command::SetConfig { request, reply })
            .await
            .map_err(|_| AcpRuntimeError::ClientClosed)?;
        response.await.map_err(|_| AcpRuntimeError::ClientClosed)?
    }

    async fn cancel(
        &self,
        notification: wire::CancelSessionNotification,
    ) -> Result<(), AcpRuntimeError> {
        let session = self
            .sessions
            .lock()
            .expect("ACP v2 session map poisoned")
            .get(&notification.session_id)
            .map(|session| {
                (
                    session.integration.clone(),
                    session.background_jobs.clone(),
                    session.tasks.clone(),
                    session.structured_completion,
                )
            });
        if let Some((handle, background_jobs, tasks, structured_completion)) = session {
            if structured_completion {
                super::cancel_background_jobs(&tasks, &background_jobs).await;
            }
            handle.interrupt();
        }
        Ok(())
    }

    async fn close(
        &self,
        request: wire::CloseSessionRequest,
    ) -> Result<wire::CloseSessionResponse, AcpRuntimeError> {
        let session = self
            .sessions
            .lock()
            .expect("ACP v2 session map poisoned")
            .remove(&request.session_id)
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(request.session_id.to_string()))?;
        super::cancel_background_jobs(&session.tasks, &session.background_jobs).await;
        session.integration.close();
        let (reply, acknowledged) = oneshot::channel();
        session
            .commands
            .send(Command::Close { reply })
            .await
            .map_err(|_| AcpRuntimeError::ClientClosed)?;
        acknowledged
            .await
            .map_err(|_| AcpRuntimeError::ClientClosed)?;
        self.registry.remove(session.token);
        Ok(wire::CloseSessionResponse::new())
    }

    fn sender(
        &self,
        session_id: &wire::SessionId,
    ) -> Result<mpsc::Sender<Command>, AcpRuntimeError> {
        self.sessions
            .lock()
            .expect("ACP v2 session map poisoned")
            .get(session_id)
            .map(|session| session.commands.clone())
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))
    }

    async fn detach_compose(
        &self,
        request: DetachComposeRequest,
    ) -> Result<DetachComposeResponse, AcpRuntimeError> {
        let id = wire::SessionId::new(request.session_id.to_string());
        let (jobs, tasks) = self
            .sessions
            .lock()
            .expect("ACP v2 session map poisoned")
            .get(&id)
            .map(|session| (session.background_jobs.clone(), session.tasks.clone()))
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(id.to_string()))?;
        Ok(DetachComposeResponse {
            detached: super::detach_compose_call(&tasks, &jobs, &request.call_id).await,
        })
    }

    fn cancel_background(
        &self,
        request: CancelBackgroundRequest,
    ) -> Result<CancelBackgroundResponse, AcpRuntimeError> {
        let id = wire::SessionId::new(request.session_id.to_string());
        let jobs = self
            .sessions
            .lock()
            .expect("ACP v2 session map poisoned")
            .get(&id)
            .map(|session| session.background_jobs.clone())
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(id.to_string()))?;
        Ok(CancelBackgroundResponse {
            cancelled: jobs.cancel(&request.call_id),
        })
    }
}

struct SessionActor<S: ModelSession> {
    session_id: wire::SessionId,
    runtime: Arc<Runtime>,
    integration: Arc<AcpIntegration>,
    handle: AcpSessionHandle,
    busy: Arc<AtomicBool>,
    binding: BindingGuard,
    sink: ResponseReplacementSink<ConnectionSink>,
    activity: SessionActivity,
    driver: LoopDriver<S>,
    tasks: TaskManagerHandle,
    background_jobs: BackgroundJobs,
    structured_completion: bool,
    skill_catalog: skill_catalog::SkillCatalogMonitor,
    adapter: SelectableAdapter,
    catalog: Vec<crate::provider::ModelGroup>,
    commands: mpsc::Receiver<Command>,
    mcp_events: crate::tools::mcp::McpSubscription,
}

async fn session_actor<S: ModelSession + Send + 'static>(actor: SessionActor<S>) {
    let SessionActor {
        session_id,
        runtime,
        integration,
        handle,
        busy,
        binding,
        activity,
        sink,
        mut driver,
        tasks,
        background_jobs,
        structured_completion,
        mut skill_catalog,
        adapter,
        catalog,
        mut commands,
        mut mcp_events,
    } = actor;
    let mut binding = Some(binding);
    loop {
        tokio::select! {
            biased;
            command = commands.recv() => match command {
                Some(Command::Prompt(command)) => {
                    let result = prepare_prompt(
                        &session_id,
                        PromptSkillSource::Runtime(&runtime),
                        &integration,
                        &handle,
                        &mut skill_catalog,
                        &mut driver,
                        command,
                        &sink,
                        &tasks,
                        &background_jobs,
                        structured_completion,
                    &activity,)
                    .await;
                    busy.store(false, Ordering::Release);
                    if let Err(error) = result {
                        eprintln!("ACP v2 prompt failed for {session_id}: {error}");
                    }
                }
                Some(Command::SetConfig { request, reply }) => {
                    let result = set_v2_config(&adapter, &catalog, request);
                    let _ = reply.send(result);
                }
                Some(Command::Close { reply }) => {
                    let v1_id = agentkit_acp::SessionId::new(session_id.to_string());
                    super::clean_up_session(&v1_id, &mut driver, &tasks, &background_jobs).await;
                    drop(binding.take());
                    let _ = reply.send(());
                    break;
                }
                None => {
                    let v1_id = agentkit_acp::SessionId::new(session_id.to_string());
                    super::clean_up_session(&v1_id, &mut driver, &tasks, &background_jobs).await;
                    break;
                }
            },
            event = mcp_events.recv() => {
                if let Some(event) = event {
                    let result = match driver.submit_input(vec![Item::notification(event.message)]) {
                        Ok(()) => drive_autonomous(
                            &session_id,
                            &integration,
                            &handle,
                            &busy,
                            &mut driver,
                            &sink,
                        &activity,).await,
                        Err(error) => Err(map_loop_error(&session_id, &error)),
                    };
                    if let Err(error) = result {
                        eprintln!("ACP v2 autonomous turn failed for {session_id}: {error}");
                    }
                }
            }
            event = tasks.next_event() => match event {
                Some(TaskEvent::Completed(snapshot, _)) => {
                    background_jobs.acknowledge_terminal(&snapshot.call_id);
                    if snapshot.kind == agentkit_task_manager::TaskKind::Background
                        && let Err(error) = drive_autonomous(
                            &session_id,
                            &integration,
                            &handle,
                            &busy,
                            &mut driver,
                            &sink,
                        &activity,).await
                    {
                        eprintln!("ACP v2 autonomous turn failed for {session_id}: {error}");
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

enum PromptSkillSource<'a> {
    #[cfg(test)]
    Static(&'a [agentkit_tool_skills::Skill]),
    Runtime(&'a Arc<Runtime>),
}

#[allow(clippy::too_many_arguments)]
async fn prepare_prompt<S: ModelSession + Send + 'static>(
    session_id: &wire::SessionId,
    skill_source: PromptSkillSource<'_>,
    integration: &AcpIntegration,
    handle: &AcpSessionHandle,
    skill_catalog: &mut skill_catalog::SkillCatalogMonitor,
    driver: &mut LoopDriver<S>,
    command: PromptCommand,
    sink: &ResponseReplacementSink<impl AcpSessionUpdateSink>,
    tasks: &TaskManagerHandle,
    background_jobs: &BackgroundJobs,
    structured_completion: bool,
    activity: &SessionActivity,
) -> Result<(), AcpRuntimeError> {
    let PromptCommand {
        request,
        cancellation_generation,
        reply,
    } = command;
    if structured_completion
        && let Err(error) = super::settle_background_jobs(tasks, background_jobs).await
    {
        handle.stop_injection_turn();
        let _ = reply.send(Err(error));
        return Ok(());
    }
    let mut current = None;
    let skills = match skill_source {
        #[cfg(test)]
        PromptSkillSource::Static(skills) => skills,
        PromptSkillSource::Runtime(runtime) => {
            let loaded = match runtime.current_skills().await {
                Ok(current) => current,
                Err(error) => {
                    handle.stop_injection_turn();
                    let _ = reply.send(Err(AcpRuntimeError::Loop(error)));
                    return Ok(());
                }
            };
            &current.insert(loaded).skills
        }
    };
    background_jobs.begin_turn();
    let prepared = integration.prompt_to_items(&request).and_then(|items| {
        skill_catalog
            .submit(skills, items, |items| driver.submit_input(items))
            .map_err(|error| match error {
                skill_catalog::SubmitError::Catalog(error) => {
                    AcpRuntimeError::Loop(format!("skill catalog error: {error}"))
                }
                skill_catalog::SubmitError::Submit(error) => map_loop_error(session_id, &error),
            })?;
        integration.begin_prompt(session_id)
    });
    drop(current);
    let user_message_id = match prepared {
        Ok(message_id) => message_id,
        Err(error) => {
            handle.stop_injection_turn();
            let _ = reply.send(Err(error));
            return Ok(());
        }
    };
    handle.start_injection_turn();
    let (start, started) = oneshot::channel();
    if reply.send(Ok(start)).is_err() || started.await.is_err() {
        handle.stop_injection_turn();
        integration.finish_prompt(session_id);
        return Ok(());
    }
    let result = async {
        sink.update(wire::UpdateSessionNotification::new(
            session_id.clone(),
            wire::SessionUpdate::UserMessage(
                wire::UserMessage::new(user_message_id).content(request.prompt),
            ),
        ))?;
        run_active_turn(
            session_id,
            integration,
            handle,
            driver,
            sink,
            cancellation_generation,
            structured_completion.then_some((tasks, background_jobs)),
            activity,
            ExecutionOrigin::Prompt,
        )
        .await
    }
    .await;
    integration.finish_prompt(session_id);
    handle.stop_injection_turn();
    result
}

#[async_trait]
trait TurnControl<S: ModelSession + Send + 'static>: Sync {
    fn stop_injection_turn(&self);
    fn is_cancelled_since(&self, generation: u64) -> bool;
    async fn handle_injection_boundary(
        &self,
        driver: &mut LoopDriver<S>,
        terminal: bool,
    ) -> Result<AcpInjectionBoundary, AcpRuntimeError>;
}

#[async_trait]
impl<S: ModelSession + Send + 'static> TurnControl<S> for AcpSessionHandle {
    fn stop_injection_turn(&self) {
        AcpSessionHandle::stop_injection_turn(self);
    }

    fn is_cancelled_since(&self, generation: u64) -> bool {
        self.cancellation_handle().is_cancelled_since(generation)
    }

    async fn handle_injection_boundary(
        &self,
        driver: &mut LoopDriver<S>,
        terminal: bool,
    ) -> Result<AcpInjectionBoundary, AcpRuntimeError> {
        AcpSessionHandle::handle_injection_boundary(self, driver, terminal).await
    }
}

async fn drive_prompt<S, C>(
    session_id: &wire::SessionId,
    driver: &mut LoopDriver<S>,
    control: &C,
    cancellation_generation: u64,
    structured: Option<(&TaskManagerHandle, &BackgroundJobs)>,
) -> Result<FinishReason, AcpRuntimeError>
where
    S: ModelSession + Send + 'static,
    C: TurnControl<S>,
{
    let result = drive_prompt_inner(
        session_id,
        driver,
        control,
        cancellation_generation,
        structured,
    )
    .await;
    if matches!(result, Ok(FinishReason::Cancelled)) {
        // A cooperative interrupt is still a live logical turn. Retire it
        // without another `next`, which could execute cancelled model work.
        driver
            .retire_interrupted_turn()
            .await
            .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?;
    }
    result
}

async fn drive_prompt_inner<S, C>(
    session_id: &wire::SessionId,
    driver: &mut LoopDriver<S>,
    control: &C,
    cancellation_generation: u64,
    structured: Option<(&TaskManagerHandle, &BackgroundJobs)>,
) -> Result<FinishReason, AcpRuntimeError>
where
    S: ModelSession + Send + 'static,
    C: TurnControl<S>,
{
    loop {
        let step = match driver.next().await {
            Ok(step) => step,
            Err(error) => {
                control.stop_injection_turn();
                if control.is_cancelled_since(cancellation_generation) {
                    return Ok(FinishReason::Cancelled);
                }
                return loop_error_stop_reason(session_id, &error);
            }
        };
        if control.is_cancelled_since(cancellation_generation) {
            return Ok(FinishReason::Cancelled);
        }
        match step {
            LoopStep::Finished(result) => {
                if result.finish_reason == FinishReason::ToolCall {
                    continue;
                }
                if result.finish_reason == FinishReason::Error {
                    control.stop_injection_turn();
                    if control.is_cancelled_since(cancellation_generation) {
                        return Ok(FinishReason::Cancelled);
                    }
                    return Err(AcpRuntimeError::Loop("model turn failed".into()));
                }
                if let Some((tasks, background_jobs)) = structured
                    && super::settle_background_jobs(tasks, background_jobs).await?
                {
                    continue;
                }
                match control.handle_injection_boundary(driver, true).await {
                    Ok(AcpInjectionBoundary::Delivered | AcpInjectionBoundary::Continue) => {
                        continue;
                    }
                    Ok(AcpInjectionBoundary::Stopped) => {
                        return Ok(FinishReason::Cancelled);
                    }
                    Ok(AcpInjectionBoundary::Finished) => {
                        return Ok(result.finish_reason);
                    }
                    Err(error) => {
                        control.stop_injection_turn();
                        if control.is_cancelled_since(cancellation_generation) {
                            return Ok(FinishReason::Cancelled);
                        }
                        return Err(error);
                    }
                }
            }
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                if let Some((tasks, background_jobs)) = structured
                    && super::settle_background_jobs(tasks, background_jobs).await?
                {
                    continue;
                }
                match control.handle_injection_boundary(driver, true).await {
                    Ok(AcpInjectionBoundary::Delivered | AcpInjectionBoundary::Continue) => {
                        continue;
                    }
                    Ok(AcpInjectionBoundary::Stopped) => {
                        return Ok(FinishReason::Cancelled);
                    }
                    Ok(AcpInjectionBoundary::Finished) => {
                        return Ok(FinishReason::Completed);
                    }
                    Err(error) => {
                        control.stop_injection_turn();
                        if control.is_cancelled_since(cancellation_generation) {
                            return Ok(FinishReason::Cancelled);
                        }
                        return Err(error);
                    }
                }
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {
                match control.handle_injection_boundary(driver, false).await {
                    Ok(AcpInjectionBoundary::Stopped) => {
                        return Ok(FinishReason::Cancelled);
                    }
                    Err(error) => {
                        control.stop_injection_turn();
                        if control.is_cancelled_since(cancellation_generation) {
                            return Ok(FinishReason::Cancelled);
                        }
                        return Err(error);
                    }
                    _ => {}
                }
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(_)) => {
                if let Err(error) = driver.cancel_pending_approvals().await {
                    control.stop_injection_turn();
                    if control.is_cancelled_since(cancellation_generation) {
                        return Ok(FinishReason::Cancelled);
                    }
                    return loop_error_stop_reason(session_id, &error);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_active_turn<S: ModelSession + Send + 'static>(
    session_id: &wire::SessionId,
    integration: &AcpIntegration,
    handle: &AcpSessionHandle,
    driver: &mut LoopDriver<S>,
    sink: &ResponseReplacementSink<impl AcpSessionUpdateSink>,
    cancellation_generation: u64,
    structured: Option<(&TaskManagerHandle, &BackgroundJobs)>,
    activity: &SessionActivity,
    origin: ExecutionOrigin,
) -> Result<(), AcpRuntimeError> {
    activity
        .execute(
            origin,
            async {
                let result = drive_prompt(
                    session_id,
                    driver,
                    handle,
                    cancellation_generation,
                    structured,
                )
                .await;
                let outcome = super::activity::ExecutionOutcome::new(
                    result,
                    handle
                        .cancellation_handle()
                        .is_cancelled_since(cancellation_generation),
                );
                handle.stop_injection_turn();
                let result = super::activity::finalize(
                    outcome,
                    structured,
                    integration.flush_session_updates(session_id),
                    |error| sink.update(error_diagnostic_notification(session_id, error)),
                )
                .await;
                integration.finish_prompt(session_id);
                result
            },
            |reason| Some(reason.clone()),
        )
        .await
        .map(|_| ())
}

async fn drive_autonomous<S: ModelSession + Send + 'static>(
    session_id: &wire::SessionId,
    integration: &AcpIntegration,
    handle: &AcpSessionHandle,
    busy: &AtomicBool,
    driver: &mut LoopDriver<S>,
    sink: &ResponseReplacementSink<impl AcpSessionUpdateSink>,
    activity: &SessionActivity,
) -> Result<(), AcpRuntimeError> {
    if claim_prompt(busy).is_err() {
        return Ok(());
    }
    handle.prepare_injection_turn();
    integration.finish_prompt(session_id);
    let cancellation_generation = handle.cancellation_handle().generation();
    handle.start_injection_turn();
    let result = run_active_turn(
        session_id,
        integration,
        handle,
        driver,
        sink,
        cancellation_generation,
        None,
        activity,
        ExecutionOrigin::Autonomous,
    )
    .await;
    integration.finish_prompt(session_id);
    handle.stop_injection_turn();
    busy.store(false, Ordering::Release);
    result
}

fn error_diagnostic_notification(
    session_id: &wire::SessionId,
    error: &AcpRuntimeError,
) -> wire::UpdateSessionNotification {
    let sequence = NEXT_ERROR_MESSAGE_ID.fetch_add(1, Ordering::Relaxed);
    wire::UpdateSessionNotification::new(
        session_id.clone(),
        wire::SessionUpdate::AgentMessage(
            wire::AgentMessage::new(wire::MessageId::new(format!(
                "{session_id}-error-{sequence}"
            )))
            .content(vec![wire::ContentBlock::Text(wire::TextContent::new(
                error.to_string(),
            ))]),
        ),
    )
}

fn send_state(
    sink: &impl AcpSessionUpdateSink,
    session_id: &wire::SessionId,
    state: wire::StateUpdate,
) -> Result<(), AcpRuntimeError> {
    sink.update(wire::UpdateSessionNotification::new(
        session_id.clone(),
        wire::SessionUpdate::StateUpdate(state),
    ))
}

fn finish_reason_to_stop_reason(reason: &FinishReason) -> wire::StopReason {
    match reason {
        FinishReason::Completed => wire::StopReason::EndTurn,
        FinishReason::ToolCall => wire::StopReason::EndTurn,
        FinishReason::MaxTokens => wire::StopReason::MaxTokens,
        FinishReason::Cancelled => wire::StopReason::Cancelled,
        FinishReason::Blocked => wire::StopReason::Refusal,
        FinishReason::Error => error_stop_reason(),
        FinishReason::Other(reason) => wire::StopReason::Other(reason.clone()),
    }
}

fn error_stop_reason() -> wire::StopReason {
    wire::StopReason::Other("_error".into())
}

fn v2_config_options(
    current: &crate::provider::ModelSelection,
    reasoning: Option<crate::provider::ReasoningEffort>,
    catalog: &[crate::provider::ModelGroup],
) -> Vec<wire::SessionConfigOption> {
    let groups = catalog
        .iter()
        .map(|group| {
            let name = match group.provider {
                crate::ProviderKind::OpenAiSubscription => "OpenAI subscription",
                crate::ProviderKind::OpenRouter => "OpenRouter",
                crate::ProviderKind::Speakeasy => "Speakeasy",
            };
            let options = group
                .models
                .iter()
                .map(|model| {
                    let selection = crate::provider::ModelSelection {
                        provider: group.provider,
                        model: model.clone(),
                    };
                    wire::SessionConfigSelectOption::new(selection.id(), model.clone())
                })
                .collect();
            wire::SessionConfigSelectGroup::new(group.provider.as_str(), name, options)
        })
        .collect::<Vec<_>>();
    let effort_options = [
        ("default", "Default"),
        ("low", "Low"),
        ("medium", "Medium"),
        ("high", "High"),
    ]
    .into_iter()
    .map(|(value, name)| wire::SessionConfigSelectOption::new(value, name))
    .collect();
    vec![
        wire::SessionConfigOption::select(super::MODEL_CONFIG_ID, "Model", current.id(), groups)
            .category(wire::SessionConfigOptionCategory::Model),
        wire::SessionConfigOption::select(
            super::REASONING_EFFORT_CONFIG_ID,
            "Reasoning effort",
            reasoning.map_or("default", crate::provider::ReasoningEffort::as_str),
            vec![wire::SessionConfigSelectGroup::new(
                "reasoning-effort",
                "Reasoning effort",
                effort_options,
            )],
        )
        .category(wire::SessionConfigOptionCategory::ThoughtLevel),
    ]
}

fn set_v2_config(
    adapter: &SelectableAdapter,
    catalog: &[crate::provider::ModelGroup],
    request: wire::SetSessionConfigOptionRequest,
) -> Result<wire::SetSessionConfigOptionResponse, AcpRuntimeError> {
    let config_id = request.config_id.to_string();
    let value = request
        .value
        .as_id()
        .ok_or_else(|| AcpRuntimeError::Unsupported("selection requires an id value".into()))?
        .to_string();
    match config_id.as_str() {
        super::MODEL_CONFIG_ID => {
            let selection =
                crate::provider::ModelSelection::from_id(&value).map_err(AcpRuntimeError::Loop)?;
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
        super::REASONING_EFFORT_CONFIG_ID => {
            let effort =
                crate::provider::ReasoningEffort::from_id(&value).map_err(AcpRuntimeError::Loop)?;
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
    let current = adapter.selection().map_err(AcpRuntimeError::Loop)?;
    let reasoning = adapter.reasoning_effort().map_err(AcpRuntimeError::Loop)?;
    Ok(wire::SetSessionConfigOptionResponse::new(
        v2_config_options(&current, reasoning, catalog),
    ))
}

fn catalog_session_info(entry: &crate::session::CatalogEntry, cwd: &Path) -> wire::SessionInfo {
    let mut info =
        wire::SessionInfo::new(wire::SessionId::new(entry.id.clone()), cwd.to_path_buf())
            .title(entry.title.as_deref().map(str::to_owned))
            .updated_at(entry.updated_at_rfc3339());
    if entry.is_subagent {
        info = info.meta(serde_json::Map::from_iter([(
            "dev.kit.subagent".into(),
            serde_json::Value::Bool(true),
        )]));
    }
    info
}

fn parse_cursor(cursor: &str) -> Result<usize, ListSessionsError> {
    cursor
        .strip_prefix("offset:")
        .and_then(|value| value.parse().ok())
        .ok_or(ListSessionsError::InvalidCursor)
}

fn transcript_replay(
    session_id: &wire::SessionId,
    transcript: &[Item],
) -> Vec<wire::UpdateSessionNotification> {
    let mut replay = Vec::new();
    for (item_index, item) in transcript.iter().enumerate() {
        let message_id =
            |kind: &str| wire::MessageId::new(format!("{session_id}-replay-{item_index}-{kind}"));
        match item.kind {
            ItemKind::User => {
                let content = item
                    .parts
                    .iter()
                    .filter_map(replay_content)
                    .collect::<Vec<_>>();
                if !content.is_empty() {
                    replay.push(wire::SessionUpdate::UserMessage(
                        wire::UserMessage::new(message_id("user")).content(content),
                    ));
                }
            }
            ItemKind::Assistant => {
                let content = item
                    .parts
                    .iter()
                    .filter(|part| matches!(part, Part::Text(_)))
                    .filter_map(replay_content)
                    .collect::<Vec<_>>();
                if !content.is_empty() {
                    replay.push(wire::SessionUpdate::AgentMessage(
                        wire::AgentMessage::new(message_id("agent")).content(content),
                    ));
                }
                let thought = item
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::Reasoning(reasoning) => reasoning.summary.as_ref(),
                        _ => None,
                    })
                    .map(|summary| wire::ContentBlock::Text(wire::TextContent::new(summary)))
                    .collect::<Vec<_>>();
                if !thought.is_empty() {
                    replay.push(wire::SessionUpdate::AgentThought(
                        wire::AgentThought::new(message_id("thought")).content(thought),
                    ));
                }
                for part in &item.parts {
                    if let Part::ToolCall(call) = part {
                        replay.push(wire::SessionUpdate::ToolCallUpdate(
                            wire::ToolCallUpdate::new(wire::ToolCallId::new(call.id.to_string()))
                                .title(call.name.clone())
                                .status(wire::ToolCallStatus::Pending)
                                .raw_input(call.input.clone()),
                        ));
                    }
                }
            }
            ItemKind::Tool => {
                for part in &item.parts {
                    if let Part::ToolResult(result) = part {
                        let status = if result.is_error {
                            wire::ToolCallStatus::Failed
                        } else {
                            wire::ToolCallStatus::Completed
                        };
                        replay.push(wire::SessionUpdate::ToolCallUpdate(
                            wire::ToolCallUpdate::new(wire::ToolCallId::new(
                                result.call_id.to_string(),
                            ))
                            .status(status)
                            .raw_output(super::tool_output_raw(&result.output))
                            .content(replay_tool_output_content(&result.output)),
                        ));
                    }
                }
            }
            ItemKind::Developer if crate::compaction::is_compaction_summary(item) => {
                let content = item
                    .parts
                    .iter()
                    .filter_map(replay_content)
                    .collect::<Vec<_>>();
                if !content.is_empty() {
                    replay.push(wire::SessionUpdate::AgentMessage(
                        wire::AgentMessage::new(message_id("compaction")).content(content),
                    ));
                }
            }
            ItemKind::System | ItemKind::Developer | ItemKind::Context | ItemKind::Notification => {
            }
        }
    }
    replay
        .into_iter()
        .map(|update| wire::UpdateSessionNotification::new(session_id.clone(), update))
        .collect()
}

fn replay_content(part: &Part) -> Option<wire::ContentBlock> {
    let content = super::user_replay_content(part)?.content;
    serde_json::from_value(serde_json::to_value(content).ok()?).ok()
}

fn replay_tool_output_content(output: &ToolOutput) -> Option<Vec<wire::ToolCallContent>> {
    let blocks = match output {
        ToolOutput::Text(text) => vec![wire::ContentBlock::Text(wire::TextContent::new(text))],
        ToolOutput::Structured(value) => vec![wire::ContentBlock::Text(wire::TextContent::new(
            value.to_string(),
        ))],
        ToolOutput::Parts(parts) => parts.iter().filter_map(replay_content).collect(),
        ToolOutput::Files(files) => files
            .iter()
            .map(|file| wire::ContentBlock::Text(wire::TextContent::new(format!("{file:?}"))))
            .collect(),
    };
    (!blocks.is_empty()).then(|| {
        blocks
            .into_iter()
            .map(|block| wire::ToolCallContent::Content(Box::new(wire::Content::new(block))))
            .collect()
    })
}

pub async fn serve(runtime: Arc<Runtime>) -> Result<(), AcpRuntimeError> {
    let registry = SessionRegistry::new();
    let result = serve_with_registry(runtime, registry.clone()).await;
    registry.shutdown().await;
    result
}

pub async fn serve_with_registry(
    runtime: Arc<Runtime>,
    registry: SessionRegistry,
) -> Result<(), AcpRuntimeError> {
    super::connect_stdio(v2_router(runtime, registry)?).await
}

pub(crate) fn http_router(runtime: Arc<Runtime>, registry: SessionRegistry) -> axum::Router {
    agent_client_protocol_http::AcpHttpServer::new(move || {
        v2_router(Arc::clone(&runtime), registry.clone())
            .expect("Kit's fixed ACP v2 integration must build")
    })
    .with_options(agent_client_protocol_http::ServerOptions {
        path: "/acp/v2".into(),
        health_endpoint: false,
        ..Default::default()
    })
    .into_router()
}

fn v2_router(
    runtime: Arc<Runtime>,
    registry: SessionRegistry,
) -> Result<agent_client_protocol::AgentProtocolRouter, AcpRuntimeError> {
    Ok(agent_client_protocol::Agent
        .protocol_router()
        .with_v2(component(runtime, registry)?))
}

pub(crate) fn component(
    runtime: Arc<Runtime>,
    registry: SessionRegistry,
) -> Result<impl ConnectTo<Client>, AcpRuntimeError> {
    let state = Arc::new(Server::new(runtime, registry));
    let agent = agent_client_protocol::Agent
        .v2()
        .name("kit")
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: wire::InitializeRequest, responder, _cx| {
                    responder.respond_with_result(state.initialize(request).map_err(sdk_error))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: wire::LoginAuthRequest, responder, _cx| {
                    responder.respond_with_result(state.login(request))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |_request: wire::LogoutAuthRequest, responder, cx| {
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
                async move |request: wire::NewSessionRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    let connection = cx.clone();
                    cx.spawn(async move {
                        match state.new_session(request, connection.clone()).await {
                            Ok((response, activation)) => complete_new_session(
                                response,
                                activation,
                                |response| responder.respond(response),
                                |notification| connection.send_notification(notification),
                            ),
                            Err(error) => responder.respond_with_error(sdk_error(error)),
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
                async move |request: wire::ListSessionsRequest, responder, cx| {
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
                async move |request: wire::ResumeSessionRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    let connection = cx.clone();
                    let session_id = request.session_id.clone();
                    cx.spawn(async move {
                        match state.resume_session(request, connection.clone()).await {
                            Ok((response, replay, activation)) => {
                                for update in replay {
                                    connection.send_notification(update)?;
                                }
                                responder.respond(response)?;
                                let _ = activation.send(());
                                connection.send_notification(available_commands_update(session_id))
                            }
                            Err(error) => responder.respond_with_error(sdk_error(error)),
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
                async move |request: wire::PromptRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    cx.spawn(async move {
                        match state.prepare_prompt(request).await {
                            Ok(start) => {
                                responder.respond(wire::PromptResponse::new())?;
                                let _ = start.send(());
                                Ok(())
                            }
                            Err(error) => responder.respond_with_error(sdk_error(error)),
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
                async move |request: wire::SetSessionConfigOptionRequest, responder, cx| {
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
                let integration = Arc::clone(&state.integration);
                async move |request: wire::InjectSessionRequest,
                            responder: Responder<wire::InjectSessionResponse>,
                            cx| {
                    let integration = Arc::clone(&integration);
                    cx.spawn(async move {
                        integration.handle_inject_request(request, responder).await
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let integration = Arc::clone(&state.integration);
                async move |request: wire::RevokeInjectSessionRequest, responder, _cx| {
                    responder.respond_with_result(integration.revoke_inject(request).await)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let integration = Arc::clone(&state.integration);
                async move |request: wire::ReplaceInjectSessionRequest, responder, cx| {
                    let integration = Arc::clone(&integration);
                    cx.spawn(async move {
                        responder.respond_with_result(integration.replace_inject(request).await)
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
                async move |request: CancelBackgroundRequest, responder, _cx| {
                    responder
                        .respond_with_result(state.cancel_background(request).map_err(sdk_error))
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
                async move |notification: wire::CancelSessionNotification, _cx| {
                    state.cancel(notification).await.map_err(sdk_error)?;
                    Ok(Handled::Yes)
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let state = Arc::clone(&state);
                async move |request: wire::CloseSessionRequest, responder, cx| {
                    let state = Arc::clone(&state);
                    cx.spawn(async move {
                        responder.respond_with_result(state.close(request).await.map_err(sdk_error))
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        );
    Ok(agent)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::{atomic::AtomicUsize, mpsc as std_mpsc},
    };

    use serde_json::json;

    use agent_client_protocol::schema::MaybeUndefined;
    use agentkit_core::{DataRef, MetadataMap, Modality, TurnCancellation};
    use agentkit_loop::{
        Agent, ModelAdapter, ModelTurn, ModelTurnEvent, ModelTurnResult, SessionConfig,
        TurnRequest, TurnResult,
    };
    use agentkit_task_manager::{AsyncTaskManager, RoutingDecision, TaskManager};
    use agentkit_tools_core::{ToolAnnotations, ToolName, ToolRegistry, ToolSpec};
    use tokio::{
        sync::Notify,
        time::{Duration, timeout},
    };

    use super::*;
    use crate::protocols::acp::tests::{BlockingTool, ScriptAdapter};

    fn terminal_auth_initialize_request() -> wire::InitializeRequest {
        wire::InitializeRequest::new(
            wire::ProtocolVersion::V2,
            wire::Implementation::new("test-client", "0"),
        )
        .capabilities(
            wire::ClientCapabilities::new().auth(
                wire::AuthCapabilities::new().terminal(wire::TerminalAuthCapabilities::new()),
            ),
        )
    }

    fn terminal_auth_method_ids(methods: &[wire::AuthMethod]) -> Vec<&str> {
        methods
            .iter()
            .map(|method| match method {
                wire::AuthMethod::Terminal(method) => method.method_id.0.as_ref(),
                _ => unreachable!("only terminal methods are advertised"),
            })
            .collect()
    }

    #[derive(Clone, Default)]
    struct RecordingSink {
        updates: Arc<Mutex<Vec<wire::UpdateSessionNotification>>>,
        flushes: Arc<AtomicU64>,
        fail_flush: Arc<AtomicBool>,
    }

    #[async_trait]
    impl AcpSessionUpdateSink for RecordingSink {
        fn update(
            &self,
            notification: wire::UpdateSessionNotification,
        ) -> Result<(), AcpRuntimeError> {
            self.updates.lock().unwrap().push(notification);
            Ok(())
        }

        async fn update_acknowledged(
            &self,
            notification: wire::UpdateSessionNotification,
        ) -> Result<(), AcpRuntimeError> {
            self.update(notification)
        }

        async fn flush(&self) -> Result<(), AcpRuntimeError> {
            self.flushes.fetch_add(1, Ordering::Relaxed);
            if self.fail_flush.load(Ordering::Relaxed) {
                Err(AcpRuntimeError::ClientClosed)
            } else {
                Ok(())
            }
        }
    }

    fn assert_running_then_idle(
        updates: &[wire::UpdateSessionNotification],
        stop_reason: wire::StopReason,
    ) {
        assert!(matches!(
            updates.first().map(|update| &update.update),
            Some(wire::SessionUpdate::StateUpdate(
                wire::StateUpdate::Running(_)
            ))
        ));
        assert!(matches!(
            updates.last().map(|update| &update.update),
            Some(wire::SessionUpdate::StateUpdate(wire::StateUpdate::Idle(
                idle
            ))) if idle.stop_reason.as_ref() == Some(&stop_reason)
        ));
        assert_eq!(
            updates
                .iter()
                .filter(|update| matches!(update.update, wire::SessionUpdate::StateUpdate(_)))
                .count(),
            2
        );
    }

    #[test]
    fn observer_reports_usage_with_a_known_context_window() {
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let activity = native_activity(wire::SessionId::new("usage-session"), sink.clone());
        let observer = ResponseReplacementObserver::new(
            AcpIntegration::default(),
            sink,
            wire::SessionId::new("usage-session"),
            activity.clone(),
        );
        let loop_session_id = SessionId::new("usage-loop");
        let emit = |usage| {
            observer.handle_event(ObservedEvent {
                session_id: Arc::new(loop_session_id.clone()),
                event: AgentEvent::UsageUpdated(usage),
            });
        };

        emit(agentkit_core::Usage::new(agentkit_core::TokenUsage::new(
            10, 2,
        )));
        emit(
            agentkit_core::Usage::new(agentkit_core::TokenUsage::new(50_000, 3_000)).with_metadata(
                MetadataMap::from([("context_window".into(), json!(272_000))]),
            ),
        );

        let updates = recording.updates.lock().unwrap();
        assert_eq!(updates.len(), 1);
        let wire::SessionUpdate::UsageUpdate(usage) = &updates[0].update else {
            panic!("expected usage update, got {:?}", updates[0].update);
        };
        assert_eq!(usage.used, 53_000);
        assert_eq!(usage.size, 272_000);
        assert!(usage.cost.is_none());
    }

    #[test]
    fn reasoning_parts_use_separate_thought_message_ids() {
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("thought-session");
        let activity = native_activity(session_id.clone(), sink.clone());
        let loop_session_id = SessionId::new("thought-loop");
        let _handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                loop_session_id.clone(),
                sink.clone(),
            ))
            .unwrap();
        let observer =
            ResponseReplacementObserver::new(integration, sink, session_id, activity.clone());
        let emit = |delta| {
            observer.handle_event(ObservedEvent {
                session_id: Arc::new(loop_session_id.clone()),
                event: AgentEvent::ContentDelta(delta),
            });
        };

        let first = PartId::new("reasoning-1");
        let second = PartId::new("reasoning-2");
        emit(Delta::BeginPart {
            part_id: first.clone(),
            kind: PartKind::Reasoning,
        });
        for chunk in ["first", " continued"] {
            emit(Delta::AppendText {
                part_id: first.clone(),
                chunk: chunk.into(),
            });
        }
        emit(Delta::BeginPart {
            part_id: second.clone(),
            kind: PartKind::Reasoning,
        });
        emit(Delta::AppendText {
            part_id: second,
            chunk: "second".into(),
        });

        let updates = recording.updates.lock().unwrap();
        let ids = updates
            .iter()
            .map(|notification| match &notification.update {
                wire::SessionUpdate::AgentThoughtChunk(chunk) => chunk.message_id.clone(),
                update => panic!("expected thought chunk, got {update:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 3);
        assert_eq!(ids[0], ids[1]);
        assert_ne!(ids[0], ids[2]);
    }

    #[test]
    fn response_replacement_clears_and_remaps_message_ids_in_new_chunk_order() {
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("replacement-session");
        let activity = native_activity(session_id.clone(), sink.clone());
        let loop_session_id = SessionId::new("replacement-loop");
        let _handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                loop_session_id.clone(),
                sink.clone(),
            ))
            .unwrap();
        let observer = ResponseReplacementObserver::new(
            integration,
            sink,
            session_id.clone(),
            activity.clone(),
        );
        let emit = |event| {
            observer.handle_event(ObservedEvent {
                session_id: Arc::new(loop_session_id.clone()),
                event,
            });
        };

        emit(AgentEvent::TurnStarted {
            session_id: loop_session_id.clone(),
            turn_id: agentkit_core::TurnId::new("turn-1"),
        });
        emit(AgentEvent::ContentDelta(agentkit_core::Delta::BeginPart {
            part_id: agentkit_core::PartId::new("message-1"),
            kind: agentkit_core::PartKind::Text,
        }));
        emit(AgentEvent::ContentDelta(agentkit_core::Delta::AppendText {
            part_id: agentkit_core::PartId::new("message-1"),
            chunk: "old answer".into(),
        }));
        emit(AgentEvent::ResponseAttemptSuperseded);
        emit(AgentEvent::ContentDelta(agentkit_core::Delta::BeginPart {
            part_id: agentkit_core::PartId::new("thought-2"),
            kind: agentkit_core::PartKind::Reasoning,
        }));
        emit(AgentEvent::ContentDelta(agentkit_core::Delta::AppendText {
            part_id: agentkit_core::PartId::new("thought-2"),
            chunk: "new ".into(),
        }));
        emit(AgentEvent::ContentDelta(agentkit_core::Delta::AppendText {
            part_id: agentkit_core::PartId::new("thought-2"),
            chunk: "thought".into(),
        }));
        emit(AgentEvent::ContentDelta(agentkit_core::Delta::BeginPart {
            part_id: agentkit_core::PartId::new("message-2"),
            kind: agentkit_core::PartKind::Text,
        }));
        emit(AgentEvent::ContentDelta(agentkit_core::Delta::AppendText {
            part_id: agentkit_core::PartId::new("message-2"),
            chunk: "new ".into(),
        }));
        emit(AgentEvent::ContentDelta(agentkit_core::Delta::AppendText {
            part_id: agentkit_core::PartId::new("message-2"),
            chunk: "answer".into(),
        }));

        let recorded = recording.updates.lock().unwrap();
        assert!(matches!(
            recorded[0].update,
            wire::SessionUpdate::StateUpdate(wire::StateUpdate::Running(_))
        ));
        let updates = &recorded[1..];
        assert_eq!(updates.len(), 6);
        let stale_message_id = match &updates[0].update {
            wire::SessionUpdate::AgentMessageChunk(chunk) => chunk.message_id.clone(),
            update => panic!("expected stale message chunk, got {update:?}"),
        };
        assert!(matches!(
            &updates[1].update,
            wire::SessionUpdate::AgentMessage(message)
                if message.message_id == stale_message_id
                    && message.content.value().is_some_and(Vec::is_empty)
        ));
        let thought_id = match &updates[2].update {
            wire::SessionUpdate::AgentThoughtChunk(chunk) => chunk.message_id.clone(),
            update => panic!("expected replacement thought chunk, got {update:?}"),
        };
        assert!(matches!(
            &updates[3].update,
            wire::SessionUpdate::AgentThoughtChunk(chunk) if chunk.message_id == thought_id
        ));
        let replacement_message_id = match &updates[4].update {
            wire::SessionUpdate::AgentMessageChunk(chunk) => chunk.message_id.clone(),
            update => panic!("expected replacement message chunk, got {update:?}"),
        };
        assert_ne!(replacement_message_id, stale_message_id);
        assert_ne!(replacement_message_id, thought_id);
        assert!(matches!(
            &updates[5].update,
            wire::SessionUpdate::AgentMessageChunk(chunk)
                if chunk.message_id == replacement_message_id
        ));
        drop(recorded);

        emit(AgentEvent::ResponseAttemptSuperseded);
        emit(AgentEvent::ContentDelta(agentkit_core::Delta::BeginPart {
            part_id: agentkit_core::PartId::new("message-3"),
            kind: agentkit_core::PartKind::Text,
        }));
        emit(AgentEvent::ContentDelta(agentkit_core::Delta::AppendText {
            part_id: agentkit_core::PartId::new("message-3"),
            chunk: "third answer".into(),
        }));
        let recorded = recording.updates.lock().unwrap();
        assert!(matches!(
            recorded[0].update,
            wire::SessionUpdate::StateUpdate(wire::StateUpdate::Running(_))
        ));
        let updates = &recorded[1..];
        assert_eq!(updates.len(), 9);
        assert!(matches!(
            &updates[6].update,
            wire::SessionUpdate::AgentMessage(message)
                if message.message_id == replacement_message_id
                    && message.content.value().is_some_and(Vec::is_empty)
        ));
        assert!(matches!(
            &updates[7].update,
            wire::SessionUpdate::AgentThought(message)
                if message.message_id == thought_id
                    && message.content.value().is_some_and(Vec::is_empty)
        ));
        assert!(matches!(
            &updates[8].update,
            wire::SessionUpdate::AgentMessageChunk(chunk)
                if chunk.message_id != replacement_message_id
                    && chunk.message_id != stale_message_id
        ));
        drop(recorded);

        emit(AgentEvent::TurnStarted {
            session_id: loop_session_id.clone(),
            turn_id: agentkit_core::TurnId::new("turn-2"),
        });
        emit(AgentEvent::ResponseAttemptSuperseded);
        assert_eq!(recording.updates.lock().unwrap().len(), 10);
    }

    #[test]
    fn response_replacement_preserves_streamed_messages_when_turn_is_cancelled() {
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("cancelled-replacement-session");
        let activity = native_activity(session_id.clone(), sink.clone());
        let loop_session_id = SessionId::new("cancelled-replacement-loop");
        let _handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                loop_session_id.clone(),
                sink.clone(),
            ))
            .unwrap();
        let observer =
            ResponseReplacementObserver::new(integration, sink, session_id, activity.clone());
        let emit = |event| {
            observer.handle_event(ObservedEvent {
                session_id: Arc::new(loop_session_id.clone()),
                event,
            });
        };
        let emit_part = |part_id: &str, kind, chunk: &str| {
            emit(AgentEvent::ContentDelta(agentkit_core::Delta::BeginPart {
                part_id: agentkit_core::PartId::new(part_id),
                kind,
            }));
            emit(AgentEvent::ContentDelta(agentkit_core::Delta::AppendText {
                part_id: agentkit_core::PartId::new(part_id),
                chunk: chunk.into(),
            }));
        };
        let finish = |turn_id, finish_reason| {
            AgentEvent::TurnFinished(TurnResult {
                turn_id: agentkit_core::TurnId::new(turn_id),
                finish_reason,
                items: Vec::new(),
                usage: None,
                metadata: MetadataMap::new(),
            })
        };

        emit(AgentEvent::TurnStarted {
            session_id: loop_session_id.clone(),
            turn_id: agentkit_core::TurnId::new("turn-1"),
        });
        emit_part("message-1", agentkit_core::PartKind::Text, "answer");
        emit_part("thought-1", agentkit_core::PartKind::Reasoning, "thinking");
        emit(finish("turn-1", FinishReason::Cancelled));

        let recorded = recording.updates.lock().unwrap();
        assert!(matches!(
            recorded[0].update,
            wire::SessionUpdate::StateUpdate(wire::StateUpdate::Running(_))
        ));
        let updates = &recorded[1..];
        assert_eq!(updates.len(), 2);
        assert!(matches!(
            &updates[0].update,
            wire::SessionUpdate::AgentMessageChunk(chunk)
                if chunk.content == wire::ContentBlock::Text(wire::TextContent::new("answer"))
        ));
        assert!(matches!(
            &updates[1].update,
            wire::SessionUpdate::AgentThoughtChunk(chunk)
                if chunk.content == wire::ContentBlock::Text(wire::TextContent::new("thinking"))
        ));
        drop(recorded);

        emit(AgentEvent::TurnStarted {
            session_id: loop_session_id.clone(),
            turn_id: agentkit_core::TurnId::new("turn-2"),
        });
        emit_part("message-2", agentkit_core::PartKind::Text, "completed");
        emit(finish("turn-2", FinishReason::Completed));
        assert_eq!(recording.updates.lock().unwrap().len(), 4);

        emit(AgentEvent::TurnStarted {
            session_id: loop_session_id.clone(),
            turn_id: agentkit_core::TurnId::new("turn-3"),
        });
        emit(AgentEvent::ResponseAttemptSuperseded);
        emit_part(
            "message-3",
            agentkit_core::PartKind::Text,
            "partial replacement",
        );
        emit(finish("turn-3", FinishReason::Cancelled));

        let recorded = recording.updates.lock().unwrap();
        assert!(matches!(
            recorded[0].update,
            wire::SessionUpdate::StateUpdate(wire::StateUpdate::Running(_))
        ));
        let updates = &recorded[1..];
        assert_eq!(updates.len(), 4);
        assert!(matches!(
            &updates[3].update,
            wire::SessionUpdate::AgentMessageChunk(chunk)
                if chunk.content
                    == wire::ContentBlock::Text(wire::TextContent::new("partial replacement"))
        ));
    }

    #[derive(Clone, Copy)]
    enum TestOutcome {
        ToolThenContent,
        Content,
        FinishError,
        ProviderError,
    }

    struct TestAdapter {
        outcome: TestOutcome,
        turns: Arc<AtomicU64>,
        interrupt: Option<AcpSessionHandle>,
    }

    struct TestSession {
        outcome: TestOutcome,
        turns: Arc<AtomicU64>,
        interrupt: Option<AcpSessionHandle>,
    }

    struct TestTurn {
        events: VecDeque<ModelTurnEvent>,
    }

    struct StreamingCancellationAdapter {
        interrupt: AcpSessionHandle,
    }

    struct StreamingCancellationSession {
        interrupt: AcpSessionHandle,
    }

    struct StreamingCancellationTurn {
        interrupt: AcpSessionHandle,
        next: u8,
    }

    #[async_trait]
    impl ModelAdapter for StreamingCancellationAdapter {
        type Session = StreamingCancellationSession;

        async fn start_session(&self, _config: SessionConfig) -> Result<Self::Session, LoopError> {
            Ok(StreamingCancellationSession {
                interrupt: self.interrupt.clone(),
            })
        }
    }

    #[async_trait]
    impl ModelSession for StreamingCancellationSession {
        type Turn = StreamingCancellationTurn;

        async fn begin_turn(
            &mut self,
            _request: TurnRequest,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Self::Turn, LoopError> {
            Ok(StreamingCancellationTurn {
                interrupt: self.interrupt.clone(),
                next: 0,
            })
        }
    }

    #[async_trait]
    impl ModelTurn for StreamingCancellationTurn {
        async fn next_event(
            &mut self,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Option<ModelTurnEvent>, LoopError> {
            let event = match self.next {
                0 => ModelTurnEvent::Delta(agentkit_core::Delta::BeginPart {
                    part_id: agentkit_core::PartId::new("message-1"),
                    kind: agentkit_core::PartKind::Text,
                }),
                1 => ModelTurnEvent::Delta(agentkit_core::Delta::AppendText {
                    part_id: agentkit_core::PartId::new("message-1"),
                    chunk: "partial answer".into(),
                }),
                2 => {
                    self.interrupt.interrupt();
                    return Err(LoopError::Cancelled);
                }
                _ => return Ok(None),
            };
            self.next += 1;
            Ok(Some(event))
        }
    }

    #[async_trait]
    impl ModelAdapter for TestAdapter {
        type Session = TestSession;

        async fn start_session(&self, _config: SessionConfig) -> Result<Self::Session, LoopError> {
            Ok(TestSession {
                outcome: self.outcome,
                turns: Arc::clone(&self.turns),
                interrupt: self.interrupt.clone(),
            })
        }
    }

    #[async_trait]
    impl ModelSession for TestSession {
        type Turn = TestTurn;

        async fn begin_turn(
            &mut self,
            _request: TurnRequest,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Self::Turn, LoopError> {
            self.turns.fetch_add(1, Ordering::Relaxed);
            if let Some(handle) = &self.interrupt {
                handle.interrupt();
            }
            match self.outcome {
                TestOutcome::ToolThenContent => {
                    self.outcome = TestOutcome::Content;
                    let call = agentkit_core::ToolCallPart::new(
                        "boundary-call",
                        "missing-tool",
                        serde_json::json!({}),
                    );
                    Ok(TestTurn {
                        events: VecDeque::from([
                            ModelTurnEvent::ToolCall(call.clone()),
                            ModelTurnEvent::Finished(ModelTurnResult {
                                model: None,
                                response_id: None,
                                finish_reason: FinishReason::ToolCall,
                                output_items: vec![Item::new(
                                    ItemKind::Assistant,
                                    vec![agentkit_core::Part::ToolCall(call)],
                                )],
                                usage: None,
                                metadata: MetadataMap::new(),
                            }),
                        ]),
                    })
                }
                TestOutcome::Content => {
                    let text = "autonomous content";
                    Ok(TestTurn {
                        events: VecDeque::from([
                            ModelTurnEvent::Delta(agentkit_core::Delta::BeginPart {
                                part_id: agentkit_core::PartId::new("message-1"),
                                kind: agentkit_core::PartKind::Text,
                            }),
                            ModelTurnEvent::Delta(agentkit_core::Delta::AppendText {
                                part_id: agentkit_core::PartId::new("message-1"),
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
                        ]),
                    })
                }
                TestOutcome::FinishError => Ok(TestTurn {
                    events: VecDeque::from([ModelTurnEvent::Finished(ModelTurnResult {
                        model: None,
                        response_id: None,
                        finish_reason: FinishReason::Error,
                        output_items: Vec::new(),
                        usage: None,
                        metadata: MetadataMap::new(),
                    })]),
                }),
                TestOutcome::ProviderError => Err(LoopError::Provider("provider failed".into())),
            }
        }
    }

    #[async_trait]
    impl ModelTurn for TestTurn {
        async fn next_event(
            &mut self,
            _cancellation: Option<TurnCancellation>,
        ) -> Result<Option<ModelTurnEvent>, LoopError> {
            Ok(self.events.pop_front())
        }
    }

    struct TestTurnControl {
        pending_steer: AtomicBool,
        boundaries: AtomicU64,
        stops: AtomicU64,
    }

    impl TestTurnControl {
        fn new(pending_steer: bool) -> Self {
            Self {
                pending_steer: AtomicBool::new(pending_steer),
                boundaries: AtomicU64::new(0),
                stops: AtomicU64::new(0),
            }
        }
    }

    #[async_trait]
    impl TurnControl<TestSession> for TestTurnControl {
        fn stop_injection_turn(&self) {
            self.stops.fetch_add(1, Ordering::Relaxed);
        }

        fn is_cancelled_since(&self, _generation: u64) -> bool {
            false
        }

        async fn handle_injection_boundary(
            &self,
            _driver: &mut LoopDriver<TestSession>,
            _terminal: bool,
        ) -> Result<AcpInjectionBoundary, AcpRuntimeError> {
            self.boundaries.fetch_add(1, Ordering::Relaxed);
            if self.pending_steer.swap(false, Ordering::Relaxed) {
                Ok(AcpInjectionBoundary::Delivered)
            } else {
                Ok(AcpInjectionBoundary::Finished)
            }
        }
    }

    async fn test_driver(
        outcome: TestOutcome,
        session_id: &str,
    ) -> (LoopDriver<TestSession>, Arc<AtomicU64>) {
        test_driver_with_interrupt(outcome, session_id, None).await
    }

    async fn test_driver_with_interrupt(
        outcome: TestOutcome,
        session_id: &str,
        interrupt: Option<AcpSessionHandle>,
    ) -> (LoopDriver<TestSession>, Arc<AtomicU64>) {
        let turns = Arc::new(AtomicU64::new(0));
        let driver = Agent::builder()
            .model(TestAdapter {
                outcome,
                turns: Arc::clone(&turns),
                interrupt,
            })
            .build()
            .unwrap()
            .start(SessionConfig::new(SessionId::new(session_id)).without_cache())
            .await
            .unwrap();
        (driver, turns)
    }

    #[tokio::test]
    async fn loop_driver_cancellation_preserves_streamed_message() {
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("cancelled-marker-session");
        let activity = native_activity(session_id.clone(), sink.clone());
        let loop_session_id = SessionId::new("cancelled-marker-loop");
        let cancellation = CancellationController::new();
        let handle = integration
            .bind_session(
                AcpSessionBinding::new(session_id.clone(), loop_session_id.clone(), sink.clone())
                    .cancellation(cancellation),
            )
            .unwrap();
        let observer =
            ResponseReplacementObserver::new(integration, sink, session_id, activity.clone());
        let mut driver = Agent::builder()
            .model(StreamingCancellationAdapter {
                interrupt: handle.clone(),
            })
            .observer(observer)
            .cancellation(handle.cancellation_handle())
            .build()
            .unwrap()
            .start(SessionConfig::new(loop_session_id).without_cache())
            .await
            .unwrap();
        driver
            .submit_input(vec![Item::text(ItemKind::User, "cancel")])
            .unwrap();

        let LoopStep::Finished(result) = driver.next().await.unwrap() else {
            panic!("expected cancelled turn");
        };
        assert_eq!(result.finish_reason, FinishReason::Cancelled);

        let recorded = recording.updates.lock().unwrap();
        assert!(matches!(
            recorded[0].update,
            wire::SessionUpdate::StateUpdate(wire::StateUpdate::Running(_))
        ));
        let updates = &recorded[1..];
        assert_eq!(updates.len(), 1);
        assert!(matches!(
            &updates[0].update,
            wire::SessionUpdate::AgentMessageChunk(chunk)
                if chunk.content == wire::ContentBlock::Text(wire::TextContent::new("partial answer"))
        ));
    }

    #[test]
    fn new_session_response_is_enqueued_before_activation_and_notifications() {
        let (activation, activated) = oneshot::channel();
        let activated = std::cell::RefCell::new(activated);
        let events = std::cell::RefCell::new(Vec::new());
        let response = wire::NewSessionResponse::new(wire::SessionId::new("session"));

        complete_new_session(
            response,
            activation,
            |_| {
                assert!(matches!(
                    activated.borrow_mut().try_recv(),
                    Err(oneshot::error::TryRecvError::Empty)
                ));
                events.borrow_mut().push("response");
                Ok::<(), ()>(())
            },
            |_| {
                assert_eq!(activated.borrow_mut().try_recv(), Ok(()));
                events.borrow_mut().push("notification");
                Ok::<(), ()>(())
            },
        )
        .unwrap();

        assert_eq!(events.into_inner(), ["response", "notification"]);
    }

    #[test]
    fn loop_failures_are_fatal_while_cancellation_stays_cancelled() {
        let session_id = wire::SessionId::new("loop-error-test");
        let error = LoopError::InvalidState("broken".into());

        let Err(AcpRuntimeError::Loop(rendered)) = loop_error_stop_reason(&session_id, &error)
        else {
            panic!("non-cancellation loop errors must fail the turn");
        };
        assert!(rendered.starts_with("invalid driver state: broken"));
        assert!(rendered.contains("fatal log:"));
        assert!(matches!(
            map_loop_error(&session_id, &LoopError::Cancelled),
            AcpRuntimeError::Cancelled
        ));
        assert_eq!(
            loop_error_stop_reason(&session_id, &LoopError::Cancelled).unwrap(),
            FinishReason::Cancelled
        );
    }

    #[tokio::test]
    async fn foreground_provider_error_after_running_terminalizes_once() {
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("foreground-provider-error");
        let activity = native_activity(session_id.clone(), sink.clone());
        let handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                SessionId::new("foreground_provider_error_after_running_terminalizes_once-loop"),
                sink.clone(),
            ))
            .unwrap();
        handle.prepare_injection_turn();
        let cancellation_generation = handle.cancellation_handle().generation();
        let turns = Arc::new(AtomicU64::new(0));
        let observer = ResponseReplacementObserver::new(
            integration.clone(),
            sink.clone(),
            session_id.clone(),
            activity.clone(),
        );
        let mut driver = Agent::builder()
            .model(TestAdapter {
                outcome: TestOutcome::ProviderError,
                turns: turns.clone(),
                interrupt: None,
            })
            .observer(observer)
            .build()
            .unwrap()
            .start(
                SessionConfig::new(SessionId::new(
                    "foreground_provider_error_after_running_terminalizes_once-loop",
                ))
                .without_cache(),
            )
            .await
            .unwrap();
        let (reply, response) = oneshot::channel();
        let command = PromptCommand {
            request: wire::PromptRequest::new(
                session_id.clone(),
                vec![wire::ContentBlock::Text(wire::TextContent::new("fail"))],
            ),
            cancellation_generation,
            reply,
        };
        let acknowledge = async move {
            response.await.unwrap().unwrap().send(()).unwrap();
        };

        let task_manager = AsyncTaskManager::new();
        let tasks = task_manager.handle();
        let background_jobs = BackgroundJobs::default();
        let mut skill_catalog = skill_catalog::SkillCatalogMonitor::new(&[]).unwrap();
        let (result, ()) = tokio::join!(
            prepare_prompt(
                &session_id,
                PromptSkillSource::Static(&[]),
                &integration,
                &handle,
                &mut skill_catalog,
                &mut driver,
                command,
                &sink,
                &tasks,
                &background_jobs,
                false,
                &activity,
            ),
            acknowledge,
        );

        assert!(matches!(result, Err(AcpRuntimeError::Loop(_))));
        assert_eq!(turns.load(Ordering::Relaxed), 1);
        assert_eq!(recording.flushes.load(Ordering::Relaxed), 1);
        let updates = recording.updates.lock().unwrap();
        assert_eq!(updates.len(), 4);
        assert!(matches!(
            updates[1].update,
            wire::SessionUpdate::StateUpdate(wire::StateUpdate::Running(_))
        ));
        assert!(
            serde_json::to_string(&updates[2].update)
                .unwrap()
                .contains("provider failed")
        );
        assert!(matches!(
            updates[2].update,
            wire::SessionUpdate::AgentMessage(_)
        ));
        assert_eq!(
            updates
                .iter()
                .filter(|update| matches!(
                    update.update,
                    wire::SessionUpdate::StateUpdate(wire::StateUpdate::Idle(_))
                ))
                .count(),
            1
        );
        assert!(matches!(
            updates.last().map(|update| &update.update),
            Some(wire::SessionUpdate::StateUpdate(wire::StateUpdate::Idle(idle)))
                if idle.stop_reason == Some(error_stop_reason())
        ));
    }

    #[tokio::test]
    async fn structured_prompt_waits_for_background_synthesis_and_consumes_completion() {
        let turns = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Notify::new());
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
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("v2-structured");
        let activity = native_activity(session_id.clone(), sink.clone());
        let handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                SessionId::new("v2-structured-loop"),
                sink.clone(),
            ))
            .unwrap();
        let observer = ResponseReplacementObserver::new(
            integration.clone(),
            sink.clone(),
            session_id.clone(),
            activity.clone(),
        );
        let mut driver = Agent::builder()
            .model(ScriptAdapter {
                turns: Arc::clone(&turns),
                user_items_seen: Arc::new(AtomicUsize::new(0)),
                notification_items_seen: Arc::new(AtomicUsize::new(0)),
            })
            .observer(observer)
            .add_tool_source(tools)
            .task_manager(task_manager)
            .build()
            .unwrap()
            .start(SessionConfig::new(SessionId::new("v2-structured-loop")).without_cache())
            .await
            .unwrap();
        driver
            .submit_input(vec![Item::text(ItemKind::User, "start background")])
            .unwrap();

        handle.prepare_injection_turn();
        handle.start_injection_turn();
        let generation = handle.cancellation_handle().generation();
        let background_jobs = BackgroundJobs::default();
        let prompt = run_active_turn(
            &session_id,
            &integration,
            &handle,
            &mut driver,
            &sink,
            generation,
            Some((&tasks, &background_jobs)),
            &activity,
            ExecutionOrigin::Prompt,
        );
        tokio::pin!(prompt);

        timeout(Duration::from_secs(1), async {
            tokio::select! {
                result = &mut prompt => panic!("structured v2 prompt resolved early: {result:?}"),
                _ = async {
                    while !entered.load(Ordering::SeqCst) {
                        tokio::task::yield_now().await;
                    }
                    assert!(super::super::detach_compose_call(
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
        .expect("structured v2 prompt did not reach its provisional response");
        assert!(
            timeout(Duration::from_millis(20), &mut prompt)
                .await
                .is_err()
        );

        assert_eq!(
            recording
                .updates
                .lock()
                .unwrap()
                .iter()
                .filter(|update| matches!(update.update, wire::SessionUpdate::StateUpdate(_)))
                .count(),
            1
        );
        background_jobs.finish_for_test("background-call");
        assert!(
            timeout(Duration::from_millis(20), &mut prompt)
                .await
                .is_err(),
            "structured v2 prompt crossed the terminal publication handoff early"
        );
        release.notify_one();
        timeout(Duration::from_secs(1), &mut prompt)
            .await
            .expect("structured v2 prompt did not synthesize")
            .unwrap();
        assert_running_then_idle(
            &recording.updates.lock().unwrap(),
            wire::StopReason::EndTurn,
        );
        assert_eq!(turns.load(Ordering::SeqCst), 3);
        assert!(
            timeout(Duration::from_millis(20), tasks.next_event())
                .await
                .is_err()
        );
        handle.stop_injection_turn();
    }

    struct BoundaryCancellationControl {
        before_boundary: bool,
        boundaries: AtomicU64,
    }

    #[async_trait]
    impl TurnControl<TestSession> for BoundaryCancellationControl {
        fn stop_injection_turn(&self) {}

        fn is_cancelled_since(&self, _generation: u64) -> bool {
            self.before_boundary
        }

        async fn handle_injection_boundary(
            &self,
            _driver: &mut LoopDriver<TestSession>,
            terminal: bool,
        ) -> Result<AcpInjectionBoundary, AcpRuntimeError> {
            assert!(!terminal, "cancellation must occur at AfterToolResult");
            self.boundaries.fetch_add(1, Ordering::Relaxed);
            Ok(AcpInjectionBoundary::Stopped)
        }
    }

    async fn assert_boundary_cancellation_retires_turn(before_boundary: bool) {
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("boundary-cancel");
        let loop_session_id = SessionId::new("boundary-cancel-loop");
        let activity = native_activity(session_id.clone(), sink.clone());
        let _handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                loop_session_id.clone(),
                sink.clone(),
            ))
            .unwrap();
        let turns = Arc::new(AtomicU64::new(0));
        let observer = ResponseReplacementObserver::new(
            integration,
            sink,
            session_id.clone(),
            activity.clone(),
        );
        let mut driver = Agent::builder()
            .model(TestAdapter {
                outcome: TestOutcome::ToolThenContent,
                turns: turns.clone(),
                interrupt: None,
            })
            .observer(observer)
            .build()
            .unwrap()
            .start(SessionConfig::new(loop_session_id).without_cache())
            .await
            .unwrap();
        driver
            .submit_input(vec![Item::text(ItemKind::User, "first")])
            .unwrap();
        let control = BoundaryCancellationControl {
            before_boundary,
            boundaries: AtomicU64::new(0),
        };
        let reason = activity
            .execute(
                ExecutionOrigin::Prompt,
                drive_prompt(&session_id, &mut driver, &control, 0, None),
                |reason| Some(reason.clone()),
            )
            .await
            .unwrap();
        assert_eq!(reason, FinishReason::Cancelled);
        assert_eq!(
            turns.load(Ordering::Relaxed),
            1,
            "must not resume cancelled model work"
        );
        assert_eq!(
            control.boundaries.load(Ordering::Relaxed),
            u64::from(!before_boundary)
        );
        assert!(
            driver
                .snapshot()
                .transcript
                .iter()
                .any(|item| item.kind == ItemKind::Tool)
        );
        assert_running_then_idle(
            &recording.updates.lock().unwrap(),
            wire::StopReason::Cancelled,
        );
        recording.updates.lock().unwrap().clear();

        driver
            .submit_input(vec![Item::text(ItemKind::User, "fresh")])
            .unwrap();
        activity
            .execute(
                ExecutionOrigin::Prompt,
                drive_prompt(
                    &session_id,
                    &mut driver,
                    &TestTurnControl::new(false),
                    0,
                    None,
                ),
                |reason| Some(reason.clone()),
            )
            .await
            .unwrap();
        assert_eq!(turns.load(Ordering::Relaxed), 2);
        assert_running_then_idle(
            &recording.updates.lock().unwrap(),
            wire::StopReason::EndTurn,
        );
    }

    #[tokio::test]
    async fn cancellation_before_tool_boundary_retires_turn() {
        assert_boundary_cancellation_retires_turn(true).await;
    }

    #[tokio::test]
    async fn cancellation_within_tool_boundary_retires_turn() {
        assert_boundary_cancellation_retires_turn(false).await;
    }

    #[tokio::test]
    async fn finish_error_stops_before_delivering_pending_steer() {
        let (mut driver, turns) = test_driver(TestOutcome::FinishError, "finish-error").await;
        driver
            .submit_input(vec![Item::text(ItemKind::User, "fail")])
            .unwrap();
        let control = TestTurnControl::new(true);

        let result = drive_prompt(
            &wire::SessionId::new("finish-error"),
            &mut driver,
            &control,
            0,
            None,
        )
        .await;

        assert!(matches!(
            result,
            Err(AcpRuntimeError::Loop(message)) if message == "model turn failed"
        ));
        assert_eq!(turns.load(Ordering::Relaxed), 1);
        assert_eq!(control.boundaries.load(Ordering::Relaxed), 0);
        assert!(control.pending_steer.load(Ordering::Relaxed));
        assert_eq!(control.stops.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cancellation_race_wins_over_provider_error() {
        let integration = AcpIntegration::default();
        let sink = RecordingSink::default();
        let session_id = wire::SessionId::new("cancel-race");
        let handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                SessionId::new("cancel-race-loop"),
                sink,
            ))
            .unwrap();
        handle.prepare_injection_turn();
        handle.start_injection_turn();
        let generation = handle.cancellation_handle().generation();
        handle.interrupt();
        assert!(handle.cancellation_handle().is_cancelled_since(generation));
        let (mut driver, _) = test_driver(TestOutcome::ProviderError, "cancel-race-loop").await;
        driver
            .submit_input(vec![Item::text(ItemKind::User, "cancel")])
            .unwrap();

        let result = drive_prompt(&session_id, &mut driver, &handle, generation, None).await;

        assert_eq!(result.unwrap(), FinishReason::Cancelled);
    }

    #[tokio::test]
    async fn provider_error_without_cancellation_remains_an_error() {
        let (mut driver, _) = test_driver(TestOutcome::ProviderError, "provider-error").await;
        driver
            .submit_input(vec![Item::text(ItemKind::User, "fail")])
            .unwrap();
        let control = TestTurnControl::new(false);

        let result = drive_prompt(
            &wire::SessionId::new("provider-error"),
            &mut driver,
            &control,
            0,
            None,
        )
        .await;

        assert!(matches!(result, Err(AcpRuntimeError::Loop(_))));
        assert_eq!(control.stops.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn autonomous_no_work_emits_no_state_transition() {
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("autonomous-no-work");
        let activity = native_activity(session_id.clone(), sink.clone());
        let handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                SessionId::new("autonomous_no_work_emits_no_state_transition-loop"),
                sink.clone(),
            ))
            .unwrap();
        let turns = Arc::new(AtomicU64::new(0));
        let observer = ResponseReplacementObserver::new(
            integration.clone(),
            sink.clone(),
            session_id.clone(),
            activity.clone(),
        );
        let mut driver = Agent::builder()
            .model(TestAdapter {
                outcome: TestOutcome::Content,
                turns: turns.clone(),
                interrupt: None,
            })
            .observer(observer)
            .build()
            .unwrap()
            .start(
                SessionConfig::new(SessionId::new(
                    "autonomous_no_work_emits_no_state_transition-loop",
                ))
                .without_cache(),
            )
            .await
            .unwrap();
        let busy = AtomicBool::new(false);

        drive_autonomous(
            &session_id,
            &integration,
            &handle,
            &busy,
            &mut driver,
            &sink,
            &activity,
        )
        .await
        .unwrap();

        assert_eq!(turns.load(Ordering::Relaxed), 0);
        assert!(!busy.load(Ordering::Relaxed));
        assert_eq!(recording.flushes.load(Ordering::Relaxed), 1);
        assert!(recording.updates.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn autonomous_content_emits_running_then_idle() {
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("autonomous-content");
        let activity = native_activity(session_id.clone(), sink.clone());
        let loop_session_id = SessionId::new("autonomous-content-loop");
        let handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                loop_session_id.clone(),
                sink.clone(),
            ))
            .unwrap();
        let turns = Arc::new(AtomicU64::new(0));
        let observer = ResponseReplacementObserver::new(
            integration.clone(),
            sink.clone(),
            session_id.clone(),
            activity.clone(),
        );
        let mut driver = Agent::builder()
            .model(TestAdapter {
                outcome: TestOutcome::Content,
                turns: Arc::clone(&turns),
                interrupt: None,
            })
            .observer(observer)
            .build()
            .unwrap()
            .start(SessionConfig::new(loop_session_id).without_cache())
            .await
            .unwrap();
        driver
            .submit_input(vec![Item::notification("background event")])
            .unwrap();
        let busy = AtomicBool::new(false);

        drive_autonomous(
            &session_id,
            &integration,
            &handle,
            &busy,
            &mut driver,
            &sink,
            &activity,
        )
        .await
        .unwrap();

        assert_eq!(turns.load(Ordering::Relaxed), 1);
        assert!(!busy.load(Ordering::Relaxed));
        assert_eq!(recording.flushes.load(Ordering::Relaxed), 1);
        let update_count = recording.updates.lock().unwrap().len();
        for _ in 0..2 {
            drive_autonomous(
                &session_id,
                &integration,
                &handle,
                &busy,
                &mut driver,
                &sink,
                &activity,
            )
            .await
            .unwrap();
        }
        assert_eq!(turns.load(Ordering::Relaxed), 1);
        assert_eq!(recording.updates.lock().unwrap().len(), update_count);
        let updates = recording.updates.lock().unwrap();
        assert!(updates.iter().any(|update| {
            serde_json::to_string(&update.update)
                .unwrap()
                .contains("autonomous content")
        }));
        assert_running_then_idle(&updates, wire::StopReason::EndTurn);
    }

    #[tokio::test]
    async fn autonomous_provider_error_emits_running_error_idle() {
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("autonomous-provider-error");
        let activity = native_activity(session_id.clone(), sink.clone());
        let handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                SessionId::new("autonomous_provider_error_emits_running_error_idle-loop"),
                sink.clone(),
            ))
            .unwrap();
        let turns = Arc::new(AtomicU64::new(0));
        let observer = ResponseReplacementObserver::new(
            integration.clone(),
            sink.clone(),
            session_id.clone(),
            activity.clone(),
        );
        let mut driver = Agent::builder()
            .model(TestAdapter {
                outcome: TestOutcome::ProviderError,
                turns: turns.clone(),
                interrupt: None,
            })
            .observer(observer)
            .build()
            .unwrap()
            .start(
                SessionConfig::new(SessionId::new(
                    "autonomous_provider_error_emits_running_error_idle-loop",
                ))
                .without_cache(),
            )
            .await
            .unwrap();
        driver
            .submit_input(vec![Item::notification("background event")])
            .unwrap();
        let busy = AtomicBool::new(false);

        let result = drive_autonomous(
            &session_id,
            &integration,
            &handle,
            &busy,
            &mut driver,
            &sink,
            &activity,
        )
        .await;

        assert!(matches!(result, Err(AcpRuntimeError::Loop(_))));
        assert_eq!(turns.load(Ordering::Relaxed), 1);
        assert!(!busy.load(Ordering::Relaxed));
        assert_eq!(recording.flushes.load(Ordering::Relaxed), 1);
        let updates = recording.updates.lock().unwrap();
        assert_eq!(updates.len(), 3);
        assert!(
            serde_json::to_string(&updates[1].update)
                .unwrap()
                .contains("provider failed")
        );
        assert!(matches!(
            updates[1].update,
            wire::SessionUpdate::AgentMessage(_)
        ));
        assert_running_then_idle(&updates, error_stop_reason());
    }

    #[tokio::test]
    async fn autonomous_cancellation_has_no_error_diagnostic_or_continuation() {
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("autonomous-cancel");
        let activity = native_activity(session_id.clone(), sink.clone());
        let handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                SessionId::new(
                    "autonomous_cancellation_has_no_error_diagnostic_or_continuation-loop",
                ),
                sink.clone(),
            ))
            .unwrap();
        let turns = Arc::new(AtomicU64::new(0));
        let observer = ResponseReplacementObserver::new(
            integration.clone(),
            sink.clone(),
            session_id.clone(),
            activity.clone(),
        );
        let mut driver = Agent::builder()
            .model(TestAdapter {
                outcome: TestOutcome::ProviderError,
                turns: turns.clone(),
                interrupt: Some(handle.clone()),
            })
            .observer(observer)
            .build()
            .unwrap()
            .start(
                SessionConfig::new(SessionId::new(
                    "autonomous_cancellation_has_no_error_diagnostic_or_continuation-loop",
                ))
                .without_cache(),
            )
            .await
            .unwrap();
        driver
            .submit_input(vec![Item::notification("background event")])
            .unwrap();
        let busy = AtomicBool::new(false);

        drive_autonomous(
            &session_id,
            &integration,
            &handle,
            &busy,
            &mut driver,
            &sink,
            &activity,
        )
        .await
        .unwrap();

        assert_eq!(turns.load(Ordering::Relaxed), 1);
        assert!(!busy.load(Ordering::Relaxed));
        assert_eq!(recording.flushes.load(Ordering::Relaxed), 1);
        assert_running_then_idle(
            &recording.updates.lock().unwrap(),
            wire::StopReason::Cancelled,
        );
    }

    #[tokio::test]
    async fn flush_failure_after_content_reports_error_before_idle_once() {
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        recording.fail_flush.store(true, Ordering::Relaxed);
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("failed-flush");
        let activity = native_activity(session_id.clone(), sink.clone());
        let handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                SessionId::new("failed-flush-loop"),
                sink.clone(),
            ))
            .unwrap();
        let observer = ResponseReplacementObserver::new(
            integration.clone(),
            sink.clone(),
            session_id.clone(),
            activity.clone(),
        );
        let mut driver = Agent::builder()
            .model(TestAdapter {
                outcome: TestOutcome::Content,
                turns: Arc::new(AtomicU64::new(0)),
                interrupt: None,
            })
            .observer(observer)
            .build()
            .unwrap()
            .start(SessionConfig::new(SessionId::new("failed-flush-loop")).without_cache())
            .await
            .unwrap();
        driver
            .submit_input(vec![Item::notification("work")])
            .unwrap();
        let result = drive_autonomous(
            &session_id,
            &integration,
            &handle,
            &AtomicBool::new(false),
            &mut driver,
            &sink,
            &activity,
        )
        .await;
        assert!(matches!(result, Err(AcpRuntimeError::ClientClosed)));
        activity.settle(None, None).unwrap();
        assert_eq!(recording.flushes.load(Ordering::Relaxed), 1);
        let updates = recording.updates.lock().unwrap();
        assert_running_then_idle(&updates, error_stop_reason());
        assert!(matches!(
            updates[updates.len() - 2].update,
            wire::SessionUpdate::AgentMessage(_)
        ));
        assert!(
            serde_json::to_string(&updates[updates.len() - 2])
                .unwrap()
                .contains(&AcpRuntimeError::ClientClosed.to_string())
        );
    }

    #[tokio::test]
    async fn autonomous_finish_error_emits_running_error_idle() {
        let integration = AcpIntegration::default();
        let recording = RecordingSink::default();
        let sink = ResponseReplacementSink::new(recording.clone());
        let session_id = wire::SessionId::new("autonomous-error");
        let activity = native_activity(session_id.clone(), sink.clone());
        let handle = integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                SessionId::new("autonomous_finish_error_emits_running_error_idle-loop"),
                sink.clone(),
            ))
            .unwrap();
        let turns = Arc::new(AtomicU64::new(0));
        let observer = ResponseReplacementObserver::new(
            integration.clone(),
            sink.clone(),
            session_id.clone(),
            activity.clone(),
        );
        let mut driver = Agent::builder()
            .model(TestAdapter {
                outcome: TestOutcome::FinishError,
                turns: turns.clone(),
                interrupt: None,
            })
            .observer(observer)
            .build()
            .unwrap()
            .start(
                SessionConfig::new(SessionId::new(
                    "autonomous_finish_error_emits_running_error_idle-loop",
                ))
                .without_cache(),
            )
            .await
            .unwrap();
        driver
            .submit_input(vec![Item::notification("background event")])
            .unwrap();
        let busy = AtomicBool::new(false);

        let result = drive_autonomous(
            &session_id,
            &integration,
            &handle,
            &busy,
            &mut driver,
            &sink,
            &activity,
        )
        .await;

        assert!(matches!(result, Err(AcpRuntimeError::Loop(_))));
        assert_eq!(turns.load(Ordering::Relaxed), 1);
        assert!(!busy.load(Ordering::Relaxed));
        assert_eq!(recording.flushes.load(Ordering::Relaxed), 1);
        let updates = recording.updates.lock().unwrap();
        assert_eq!(updates.len(), 3);
        assert!(matches!(
            updates[1].update,
            wire::SessionUpdate::AgentMessage(_)
        ));
        assert!(
            serde_json::to_string(&updates[1].update)
                .unwrap()
                .contains("loop error: model turn failed")
        );
        assert_running_then_idle(&updates, error_stop_reason());
    }

    #[test]
    fn available_commands_advertises_only_compact() {
        let notification = available_commands_update(wire::SessionId::new("session"));
        let wire::SessionUpdate::AvailableCommandsUpdate(update) = notification.update else {
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
    fn concurrent_prompt_admission_is_rejected_until_the_turn_finishes() {
        let busy = AtomicBool::new(false);

        claim_prompt(&busy).unwrap();
        assert!(matches!(
            claim_prompt(&busy),
            Err(AcpRuntimeError::Unsupported(_))
        ));

        busy.store(false, Ordering::Release);
        claim_prompt(&busy).unwrap();
    }

    #[test]
    fn sdk_error_marks_only_missing_credentials_as_authentication_required() {
        let missing_detail = "openrouter_auth_required: set OPENROUTER_API_KEY or run `kit auth login openrouter` before using the OpenRouter provider";
        let missing = sdk_error(AcpRuntimeError::Loop(missing_detail.into()));
        assert_eq!(missing.code, agent_client_protocol::ErrorCode::AuthRequired);
        let data = missing.data.unwrap();
        let required = AuthenticationRequiredData::from_value(&data).unwrap();
        assert_eq!(required.method_id, "openrouter");
        assert_eq!(required.detail, format!("loop error: {missing_detail}"));

        let speakeasy = sdk_error(AcpRuntimeError::Loop(
            "speakeasy_auth_required: run `kit auth login speakeasy`".into(),
        ));
        assert_eq!(
            speakeasy.code,
            agent_client_protocol::ErrorCode::AuthRequired
        );
        assert_eq!(
            AuthenticationRequiredData::from_value(speakeasy.data.as_ref().unwrap())
                .unwrap()
                .method_id,
            "speakeasy"
        );

        let unrelated = sdk_error(AcpRuntimeError::Loop(
            "stored OpenRouter credentials cannot be used with a noncanonical endpoint".into(),
        ));
        assert!(matches!(unrelated.data, Some(serde_json::Value::String(_))));
    }

    #[test]
    fn terminal_auth_login_route_rejects_in_process_login() {
        let root = tempfile::tempdir().unwrap();
        let server = Server::new(
            Runtime::new(root.path(), "gpt-5.4").unwrap(),
            SessionRegistry::new(),
        );

        for (method_id, expected_detail) in [
            (
                "openai",
                "terminal authentication methods must be launched as a separate agent invocation",
            ),
            (
                "unknown",
                "authentication method was not advertised by this agent",
            ),
        ] {
            let error = server
                .login(wire::LoginAuthRequest::new(method_id))
                .unwrap_err();
            let data = error.data.unwrap();

            assert_eq!(error.code, agent_client_protocol::ErrorCode::InvalidParams);
            assert_eq!(data["methodId"], method_id);
            assert_eq!(data["detail"], expected_detail);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn logout_reset_waits_for_registered_session_publication() {
        let root = tempfile::tempdir().unwrap();
        let registry = SessionRegistry::new();
        let server = Arc::new(Server::new(
            Runtime::new(root.path(), "gpt-5.4").unwrap(),
            registry.clone(),
        ));
        let session_id = wire::SessionId::new("publishing-session");
        let integration = server
            .integration
            .bind_session(AcpSessionBinding::new(
                session_id.clone(),
                SessionId::new("publishing-session"),
                RecordingSink::default(),
            ))
            .unwrap();
        let token = registry.next_token();
        let (completed, completion) = watch::channel(false);
        let release = Arc::new(Notify::new());
        let actor_release = Arc::clone(&release);
        let dropping = Arc::new(Notify::new());
        let actor_dropping = Arc::clone(&dropping);
        let guard = ActorGuard {
            server: Arc::downgrade(&server),
            registry: registry.clone(),
            session_id: session_id.clone(),
            token,
            completed,
        };
        let actor_task = tokio::spawn(async move {
            let _guard = guard;
            actor_release.notified().await;
            actor_dropping.notify_one();
        });
        let close_release = Arc::clone(&release);
        let close = Arc::new(move || {
            let close_release = Arc::clone(&close_release);
            Box::pin(async move { close_release.notify_one() })
                as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        });
        let mut admission = registry.begin_attachment().unwrap();
        let actor_abort = actor_task.abort_handle();
        let (commands, _received) = mpsc::channel(1);
        let (registered, registered_rx) = std_mpsc::channel();
        let (continue_publication, continue_publication_rx) = std_mpsc::channel();
        let publisher_server = Arc::clone(&server);
        let published_session_id = session_id.clone();
        let publisher = tokio::task::spawn_blocking(move || {
            publisher_server.publish_session(
                &mut admission,
                PendingSessionPublication {
                    token,
                    interrupt: Arc::new(|| {}),
                    close,
                    actor: actor_abort,
                    completed: completion,
                    session_id: published_session_id,
                    session: SessionHandle {
                        token,
                        commands,
                        integration,
                        busy: Arc::new(AtomicBool::new(false)),
                        background_jobs: BackgroundJobs::default(),
                        structured_completion: false,
                        tasks: AsyncTaskManager::new().handle(),
                    },
                },
                || {
                    registered.send(()).unwrap();
                    continue_publication_rx.recv().unwrap();
                    Ok(())
                },
            )
        });
        tokio::task::spawn_blocking(move || registered_rx.recv().unwrap())
            .await
            .unwrap();

        let reset_registry = registry.clone();
        let mut reset = tokio::spawn(async move { reset_registry.reset_authentication().await });
        dropping.notified().await;
        assert!(
            timeout(Duration::from_millis(20), &mut reset)
                .await
                .is_err()
        );
        continue_publication.send(()).unwrap();

        publisher.await.unwrap().unwrap();
        assert!(reset.await.unwrap());
        actor_task.await.unwrap();
        assert!(!server.sessions.lock().unwrap().contains_key(&session_id));
    }

    #[tokio::test]
    async fn auth_logout_clears_persistent_provider_credentials() {
        let root = tempfile::tempdir().unwrap();
        let credentials = tempfile::tempdir().unwrap();
        let storage =
            crate::credentials::CredentialStorage::Filesystem(credentials.path().to_path_buf());
        crate::provider::store_openrouter_test_credentials(&storage);
        let mut runtime = Runtime::new_with_provider_and_credentials(
            root.path(),
            "openrouter:test",
            crate::ProviderKind::OpenRouter,
            storage.clone(),
        )
        .unwrap();
        Arc::get_mut(&mut runtime)
            .unwrap()
            .set_ambient_openrouter_api_key_for_test(false);
        let server = Arc::new(Server::new(runtime, SessionRegistry::new()));

        server.logout().await.unwrap();

        assert!(
            storage
                .entry("openrouter", "default")
                .load()
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn unmanaged_openrouter_credentials_disable_v2_authentication() {
        for (explicit_key, ambient_key) in [(true, false), (false, true)] {
            for (model, provider) in [
                ("openrouter:test", crate::ProviderKind::OpenRouter),
                ("gpt-5.4", crate::ProviderKind::OpenAiSubscription),
                ("test-model", crate::ProviderKind::Speakeasy),
            ] {
                let root = tempfile::tempdir().unwrap();
                let credentials = tempfile::tempdir().unwrap();
                let mut runtime = Runtime::new_with_provider_credentials_effort_and_openrouter_key(
                    root.path(),
                    model,
                    provider,
                    crate::credentials::CredentialStorage::Filesystem(
                        credentials.path().to_path_buf(),
                    ),
                    None,
                    explicit_key
                        .then(|| crate::provider::OpenRouterApiKey::new("unrelated explicit key")),
                )
                .unwrap();
                Arc::get_mut(&mut runtime)
                    .unwrap()
                    .set_ambient_openrouter_api_key_for_test(ambient_key);
                let server = Arc::new(Server::new(runtime, SessionRegistry::new()));

                let response = server
                    .initialize(terminal_auth_initialize_request())
                    .unwrap();
                let method_ids = terminal_auth_method_ids(&response.auth_methods);
                assert!(method_ids.is_empty());
                assert!(server.logout().await.is_err());
            }
        }
    }

    #[tokio::test]
    async fn v2_router_advertises_and_routes_pending_injection_replacement() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::new_with_provider_and_credentials(
            root.path(),
            "gpt-5.4",
            crate::ProviderKind::OpenAiSubscription,
            crate::credentials::CredentialStorage::Memory,
        )
        .unwrap();
        let (client_transport, agent_transport) = agent_client_protocol::Channel::duplex();
        let router = v2_router(runtime, SessionRegistry::new()).unwrap();
        let server = tokio::spawn(async move { router.connect_to(agent_transport).await });
        let client =
            agent_client_protocol::Client
                .v2()
                .connect_with(client_transport, async move |cx| {
                    let initialized = cx
                        .send_request(wire::InitializeRequest::new(
                            wire::ProtocolVersion::V2,
                            wire::Implementation::new("replacement-test", "0"),
                        ))
                        .block_task()
                        .await?;
                    let pending = initialized
                        .capabilities
                        .session
                        .expect("session capabilities")
                        .inject
                        .expect("injection capabilities")
                        .pending
                        .expect("pending injection capabilities");
                    assert_eq!(pending.replace, Some(true));

                    // A domain error, rather than method-not-found, proves that Kit's
                    // production router forwards replacement requests to AgentKit.
                    let error = cx
                        .send_request(wire::ReplaceInjectSessionRequest::new(
                            "missing-session",
                            "pending-message",
                            vec![wire::ContentBlock::Text(wire::TextContent::new(
                                "replacement",
                            ))],
                        ))
                        .block_task()
                        .await
                        .expect_err("replacement must reject an unknown session");
                    assert_eq!(
                        i32::from(error.code),
                        i32::from(wire::Error::resource_not_found(None).code)
                    );
                    assert_eq!(error.data, Some(json!({ "sessionId": "missing-session" })));
                    Ok(())
                });
        let result = timeout(Duration::from_secs(2), client).await;
        server.abort();
        let _ = server.await;
        result
            .expect("replacement client timed out")
            .expect("replacement client failed");
    }

    #[test]
    fn initialize_negotiates_v2_and_advertises_injection_and_authentication() {
        let root = tempfile::tempdir().unwrap();
        let credentials = tempfile::tempdir().unwrap();
        let mut runtime = Runtime::new_with_provider_and_credentials(
            root.path(),
            "gpt-5.4",
            crate::ProviderKind::OpenAiSubscription,
            crate::credentials::CredentialStorage::Filesystem(credentials.path().to_path_buf()),
        )
        .unwrap();
        Arc::get_mut(&mut runtime)
            .unwrap()
            .set_ambient_openrouter_api_key_for_test(false);
        let server = Server::new(runtime, SessionRegistry::new());
        let response = server
            .initialize(terminal_auth_initialize_request())
            .unwrap();

        assert_eq!(response.protocol_version, wire::ProtocolVersion::V2);
        assert_eq!(response.auth_methods.len(), 3);
        assert!(matches!(
            &response.auth_methods[0],
            wire::AuthMethod::Terminal(method)
                if method.method_id.0.as_ref() == "openai"
                    && method.args == ["--terminal-auth-login", "openai"]
        ));
        assert!(matches!(
            &response.auth_methods[1],
            wire::AuthMethod::Terminal(method)
                if method.method_id.0.as_ref() == "openrouter"
                    && method.args == ["--terminal-auth-login", "openrouter"]
        ));
        assert!(matches!(
            &response.auth_methods[2],
            wire::AuthMethod::Terminal(method)
                if method.method_id.0.as_ref() == "speakeasy"
                    && method.args == ["--terminal-auth-login", "speakeasy"]
        ));
        let unsupported = server
            .initialize(wire::InitializeRequest::new(
                wire::ProtocolVersion::V2,
                wire::Implementation::new("unsupported-client", "0"),
            ))
            .unwrap();
        assert!(unsupported.auth_methods.is_empty());
        let terminal_capabilities = wire::ClientCapabilities::new()
            .auth(wire::AuthCapabilities::new().terminal(wire::TerminalAuthCapabilities::new()));
        assert!(terminal_auth_methods(&terminal_capabilities, |_| false).is_empty());
        let methods = terminal_auth_methods(&terminal_capabilities, |provider| {
            provider != ProviderKind::OpenRouter
        });
        let method_ids = terminal_auth_method_ids(&methods);
        assert_eq!(method_ids, ["openai", "speakeasy"]);

        let metadata_only = wire::ClientCapabilities::new().meta(serde_json::Map::from_iter([(
            "terminal-auth".into(),
            serde_json::Value::Bool(true),
        )]));
        assert!(terminal_auth_methods(&metadata_only, |_| true).is_empty());
        let mut newer = wire::InitializeRequest::new(
            wire::ProtocolVersion::V2,
            wire::Implementation::new("newer-client", "0"),
        );
        newer.protocol_version = serde_json::from_value(json!(99)).unwrap();
        assert_eq!(
            server.initialize(newer).unwrap().protocol_version,
            wire::ProtocolVersion::V2
        );
        let session = response.capabilities.session.expect("session capabilities");
        assert!(session.inject.is_some());
        assert!(session.delete.is_none());
        assert!(session.fork.is_none());
        assert!(
            server
                .initialize(wire::InitializeRequest::new(
                    wire::ProtocolVersion::V1,
                    wire::Implementation::new("test-client", "0"),
                ))
                .is_err()
        );
    }

    #[test]
    fn replay_uses_complete_v2_messages_with_stable_ids() {
        let session_id = wire::SessionId::new("saved");
        let transcript = [
            Item::text(ItemKind::User, "question"),
            Item::text(ItemKind::Assistant, "answer"),
        ];

        let replay = transcript_replay(&session_id, &transcript);
        let replay_again = transcript_replay(&session_id, &transcript);

        assert_eq!(
            serde_json::to_value(&replay).unwrap(),
            serde_json::to_value(&replay_again).unwrap()
        );
        assert_eq!(replay.len(), 2);
        assert!(matches!(
            replay[0].update,
            wire::SessionUpdate::UserMessage(_)
        ));
        assert!(matches!(
            replay[1].update,
            wire::SessionUpdate::AgentMessage(_)
        ));
    }

    #[test]
    fn replay_preserves_data_url_user_images() {
        let replay = transcript_replay(
            &wire::SessionId::new("saved"),
            &[Item::new(
                ItemKind::User,
                vec![Part::media(
                    Modality::Image,
                    "image/png",
                    DataRef::uri("data:image/png;base64,AQID"),
                )],
            )],
        );
        let wire::SessionUpdate::UserMessage(message) = &replay[0].update else {
            panic!("expected user message");
        };
        let MaybeUndefined::Value(content) = &message.content else {
            panic!("expected user content");
        };
        assert!(matches!(
            content.as_slice(),
            [wire::ContentBlock::Image(image)] if image.data == "AQID"
        ));
    }

    #[test]
    fn v2_config_mapping_uses_v2_ids_categories_and_values() {
        let current =
            crate::provider::ModelSelection::new(crate::ProviderKind::OpenRouter, "test-model");
        let catalog = [crate::provider::ModelGroup {
            provider: crate::ProviderKind::OpenRouter,
            models: vec!["test-model".into(), "other-model".into()],
        }];

        let options = v2_config_options(
            &current,
            Some(crate::provider::ReasoningEffort::High),
            &catalog,
        );
        let encoded = serde_json::to_value(&options).unwrap();

        assert_eq!(encoded[0]["configId"], "model");
        assert_eq!(encoded[0]["category"], "model");
        assert_eq!(encoded[0]["currentValue"], "openrouter:test-model");
        assert!(encoded[0].get("id").is_none());
        assert_eq!(encoded[1]["configId"], "reasoning_effort");
        assert_eq!(encoded[1]["category"], "thought_level");
        assert_eq!(encoded[1]["currentValue"], "high");
    }

    #[test]
    fn all_finish_reasons_map_to_faithful_v2_idle_reasons() {
        assert_eq!(
            finish_reason_to_stop_reason(&FinishReason::Completed),
            wire::StopReason::EndTurn
        );
        assert_eq!(
            finish_reason_to_stop_reason(&FinishReason::MaxTokens),
            wire::StopReason::MaxTokens
        );
        assert_eq!(
            finish_reason_to_stop_reason(&FinishReason::Cancelled),
            wire::StopReason::Cancelled
        );
        assert_eq!(
            finish_reason_to_stop_reason(&FinishReason::Blocked),
            wire::StopReason::Refusal
        );
        assert_eq!(
            finish_reason_to_stop_reason(&FinishReason::Error),
            wire::StopReason::Other("_error".into())
        );
        assert_eq!(
            finish_reason_to_stop_reason(&FinishReason::Other("provider-stop".into())),
            wire::StopReason::Other("provider-stop".into())
        );
    }

    #[test]
    fn replay_tool_results_include_visible_content() {
        let content = replay_tool_output_content(&ToolOutput::text("done")).unwrap();
        assert!(matches!(
            content.as_slice(),
            [wire::ToolCallContent::Content(content)]
                if matches!(&content.content, wire::ContentBlock::Text(text) if text.text == "done")
        ));
    }

    #[test]
    fn catalog_entries_enrich_v2_session_info() {
        let info = catalog_session_info(
            &crate::session::CatalogEntry {
                id: "saved".into(),
                title: Some("Saved session".into()),
                preview: Some("Saved session preview".into()),
                is_subagent: true,
                updated_at: 0,
            },
            &PathBuf::from("/workspace"),
        );
        let encoded = serde_json::to_value(info).unwrap();
        assert_eq!(encoded["sessionId"], "saved");
        assert_eq!(encoded["cwd"], "/workspace");
        assert_eq!(encoded["title"], "Saved session");
        assert_eq!(encoded["updatedAt"], "1970-01-01T00:00:00.000Z");
        assert_eq!(encoded["_meta"]["dev.kit.subagent"], true);
    }

    #[test]
    fn cursors_parse_offsets_and_reject_malformed_values() {
        assert_eq!(parse_cursor("offset:100").unwrap(), 100);
        assert!(parse_cursor("100").is_err());
        assert!(parse_cursor("offset:nope").is_err());
        assert_eq!(
            list_sessions_error(ListSessionsError::InvalidCursor).code,
            agent_client_protocol::ErrorCode::InvalidParams
        );
    }

    #[tokio::test]
    async fn malformed_cursor_is_rejected_before_cwd_filtering() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
        let server = Server::new(runtime, SessionRegistry::new());
        let request = wire::ListSessionsRequest::new()
            .cwd(root.path().join("other"))
            .cursor(wire::SessionListCursor::new("invalid"));

        assert!(matches!(
            server.list_sessions(request).await,
            Err(ListSessionsError::InvalidCursor)
        ));
    }
}
