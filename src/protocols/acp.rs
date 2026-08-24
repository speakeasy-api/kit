use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use agent_client_protocol::{Client, ConnectTo, ConnectionTo, Handled};
use agentkit_acp::{
    AcpClientHandle, AcpClientMessage, AcpIntegration, AcpRuntimeError, AcpSessionBinding,
    AudioContent, AutoDenyResolver, AvailableCommand, AvailableCommandsUpdate,
    BlobResourceContents, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    ContentBlock, ContentChunk, EmbeddedResource, EmbeddedResourceResource, ImageContent,
    InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse,
    ResourceLink, SessionAdditionalDirectoriesCapabilities, SessionCapabilities,
    SessionCloseCapabilities, SessionConfigOption, SessionConfigOptionCategory,
    SessionConfigSelectGroup, SessionConfigSelectOption, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason, TextContent,
    TextResourceContents, ToolCallStatus, ToolCallUpdateFields,
};
use agentkit_core::{
    CancellationController, DataRef, FinishReason, Item, ItemKind, MediaPart, MetadataMap,
    Modality, Part, SessionId as AgentkitSessionId, ToolOutput,
};
use agentkit_loop::{LoopDriver, LoopError, LoopInterrupt, LoopStep, ModelSession};
use agentkit_task_manager::{TaskEvent, TaskManagerHandle};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::{AbortHandle, JoinSet},
    time::timeout,
};

use crate::{
    provider::{ModelGroup, ModelSelection, ReasoningEffort, SelectableAdapter, model_catalog},
    runtime::{AcpDriverContext, BackgroundJobs, Runtime},
};

const MODEL_CONFIG_ID: &str = "model";
const REASONING_EFFORT_CONFIG_ID: &str = "reasoning_effort";

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

fn user_replay_content(part: &Part) -> Option<ContentChunk> {
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

fn tool_output_raw(output: &ToolOutput) -> Option<serde_json::Value> {
    match output {
        ToolOutput::Text(text) => Some(json!({ "text": text })),
        ToolOutput::Structured(value) => Some(value.clone()),
        ToolOutput::Parts(parts) => serde_json::to_value(parts).ok(),
        ToolOutput::Files(files) => serde_json::to_value(files).ok(),
    }
}

fn media_replay_content(media: &MediaPart) -> ContentBlock {
    match media.modality {
        Modality::Image
            if matches!(media.data, DataRef::InlineText(_) | DataRef::InlineBytes(_)) =>
        {
            ContentBlock::Image(ImageContent::new(
                data_ref_base64_payload(&media.data),
                media.mime_type.clone(),
            ))
        }
        Modality::Audio
            if matches!(media.data, DataRef::InlineText(_) | DataRef::InlineBytes(_)) =>
        {
            ContentBlock::Audio(AudioContent::new(
                data_ref_base64_payload(&media.data),
                media.mime_type.clone(),
            ))
        }
        Modality::Image | Modality::Audio | Modality::Video | Modality::Binary => {
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
                data_ref_base64_payload(data),
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

fn data_ref_base64_payload(data: &DataRef) -> String {
    match data {
        DataRef::InlineText(text) => {
            data_url_base64_payload(text).unwrap_or_else(|| BASE64.encode(text.as_bytes()))
        }
        DataRef::InlineBytes(bytes) => BASE64.encode(bytes),
        DataRef::Uri(_) | DataRef::Handle(_) => String::new(),
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
    agent_client_protocol::util::internal_error(error.to_string())
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
    Close {
        reply: oneshot::Sender<()>,
    },
}

struct SessionHandle {
    token: u64,
    commands: mpsc::Sender<Command>,
    background_jobs: BackgroundJobs,
}

#[derive(Clone)]
struct RegisteredSession {
    token: u64,
    session_id: agentkit_acp::SessionId,
    integration: Arc<AcpIntegration>,
    commands: mpsc::WeakSender<Command>,
    actor: AbortHandle,
    completed: watch::Receiver<bool>,
}

struct RegistryState {
    accepting: bool,
    sessions: HashMap<u64, RegisteredSession>,
}

struct SessionRegistryInner {
    next_token: AtomicU64,
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
                state: Mutex::new(RegistryState {
                    accepting: true,
                    sessions: HashMap::new(),
                }),
            }),
        }
    }

    fn next_token(&self) -> u64 {
        self.inner.next_token.fetch_add(1, Ordering::Relaxed)
    }

    fn register(&self, session: RegisteredSession) -> Result<(), ()> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("ACP session registry poisoned");
        if !state.accepting {
            return Err(());
        }
        state.sessions.insert(session.token, session);
        Ok(())
    }

    fn remove(&self, token: u64) {
        self.inner
            .state
            .lock()
            .expect("ACP session registry poisoned")
            .sessions
            .remove(&token);
    }

    fn close_gate_and_snapshot(&self) -> Vec<RegisteredSession> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("ACP session registry poisoned");
        state.accepting = false;
        state.sessions.values().cloned().collect()
    }

    pub async fn shutdown(&self) {
        self.shutdown_with_timeout(Duration::from_secs(5)).await;
    }

    async fn shutdown_with_timeout(&self, limit: Duration) {
        let sessions = self.close_gate_and_snapshot();
        for session in &sessions {
            let _ = session.integration.interrupt_session(&session.session_id);
        }

        let mut closing = JoinSet::new();
        for mut session in sessions.iter().cloned() {
            let registry = self.clone();
            closing.spawn(async move {
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

        if timeout(limit, async {
            while closing.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            closing.abort_all();
            for session in &sessions {
                if !*session.completed.borrow() {
                    session.actor.abort();
                }
            }
            while closing.join_next().await.is_some() {}
            for session in sessions {
                self.remove(session.token);
            }
        }
    }
}

impl Default for SessionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

struct AttachedSession {
    session_id: agentkit_acp::SessionId,
    config_options: Vec<SessionConfigOption>,
    canonical_transcript: Vec<Item>,
    activation: oneshot::Sender<()>,
}

struct PreparedLoad {
    response: LoadSessionResponse,
    replay: Vec<SessionNotification>,
    activation: oneshot::Sender<()>,
}

/// Owns an ACP integration binding for an in-flight request or live actor.
/// Dropping either owner must release the durable identity.
struct SessionBindingGuard {
    integration: Arc<AcpIntegration>,
    session_id: agentkit_acp::SessionId,
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
}

impl Server {
    fn new(runtime: Arc<Runtime>, integration: AcpIntegration, registry: SessionRegistry) -> Self {
        Self {
            runtime,
            integration: Arc::new(integration),
            registry,
            sessions: Mutex::new(HashMap::new()),
        }
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
        InitializeResponse::new(request.protocol_version)
            .agent_capabilities(capabilities())
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
            )
            .await?;
        let replay = transcript_replay(&attached.session_id, &attached.canonical_transcript);
        Ok(PreparedLoad {
            response: LoadSessionResponse::new().config_options(Some(attached.config_options)),
            replay,
            activation: attached.activation,
        })
    }

    async fn attach_session(
        self: &Arc<Self>,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        connection: ConnectionTo<Client>,
        mut claim: crate::runtime::SessionClaim,
    ) -> Result<AttachedSession, AcpRuntimeError> {
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
        let binding = AcpSessionBinding::new(session_id.clone(), agentkit_session_id, client)
            .cancellation(cancellation)
            .workspace(cwd.clone(), additional_directories.clone())
            .metadata(metadata);
        let handle = self
            .integration
            .bind_session(binding)
            .map_err(|error| record_acp_runtime_failure(&session_id, "session_bind", error))?;
        let binding = SessionBindingGuard::new(Arc::clone(&self.integration), session_id.clone());
        let context = AcpDriverContext {
            cwd,
            additional_directories,
            integration: Arc::clone(&self.integration),
            cancellation: handle.cancellation_handle(),
        };
        let driver = match self.runtime.start_acp_driver(context, &mut claim).await {
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
        let canonical_transcript = driver.canonical_transcript;
        let (tx, rx) = mpsc::channel(8);
        let actor = SessionActor {
            session_id: session_id.clone(),
            integration: Arc::clone(&self.integration),
            binding,
            driver: driver.driver,
            tasks: driver.tasks,
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
            commands: tx.downgrade(),
            actor: actor_task.abort_handle(),
            completed: completion,
        };
        drop(actor_task);

        // Hold the request-scoped map lock across registration, commit, and publication.
        // Shutdown either closes the gate before this point or snapshots this actor.
        let mut sessions = self.sessions.lock().expect("ACP session map poisoned");
        self.registry
            .register(registered)
            .map_err(|()| AcpRuntimeError::ClientClosed)?;
        if let Err(error) = claim.commit() {
            self.registry.remove(token);
            return Err(record_acp_runtime_failure(
                &session_id,
                "session_commit",
                error,
            ));
        }
        crate::events::emit(&crate::events::RuntimeEvent::SessionStarted {
            session_id: session_id.to_string(),
        });
        sessions.insert(
            session_id.clone(),
            SessionHandle {
                token,
                commands: tx,
                background_jobs,
            },
        );
        drop(sessions);
        Ok(AttachedSession {
            session_id,
            config_options,
            canonical_transcript,
            activation,
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
        self.integration
            .interrupt_session(&notification.session_id)?;
        let sender = self.sender(&notification.session_id).await?;
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
        self.integration.interrupt_session(&request.session_id)?;
        let session = self
            .sessions
            .lock()
            .expect("ACP session map poisoned")
            .remove(&request.session_id)
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(request.session_id.to_string()))?;
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

struct SessionActor<S: ModelSession> {
    session_id: agentkit_acp::SessionId,
    integration: Arc<AcpIntegration>,
    binding: SessionBindingGuard,
    driver: LoopDriver<S>,
    tasks: TaskManagerHandle,
    adapter: SelectableAdapter,
    catalog: Vec<ModelGroup>,
    commands: mpsc::Receiver<Command>,
    turn_states: mpsc::UnboundedSender<TurnStateNotification>,
    mcp_events: crate::tools::mcp::McpSubscription,
}

async fn session_actor<S: ModelSession>(actor: SessionActor<S>) {
    let SessionActor {
        session_id,
        integration,
        binding,
        mut driver,
        tasks,
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
                    let result = drive_prompt(&session_id, &integration, &mut driver, request).await;
                    let _ = reply.send(result);
                }
                // The server already interrupted the shared controller; this
                // marker only establishes its serialized actor position.
                Some(Command::Cancel) => {}
                Some(Command::SetConfig { request, reply }) => {
                    let result = set_config(&adapter, &catalog, request);
                    let _ = reply.send(result);
                }
                Some(Command::Close { reply }) => {
                    clean_up_session(&session_id, &mut driver, &tasks).await;
                    // A close acknowledgement means the actor-owned binding is
                    // already gone, so callers can immediately reuse the id.
                    drop(binding.take());
                    let _ = reply.send(());
                    break;
                }
                None => {
                    clean_up_session(&session_id, &mut driver, &tasks).await;
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
                Some(TaskEvent::Completed(snapshot, _))
                    if snapshot.kind == agentkit_task_manager::TaskKind::Background =>
                {
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
                Some(_) => {}
                None => break,
            }
        }
    }
}

fn config_options(
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

fn set_config(
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

async fn clean_up_session<S: ModelSession>(
    session_id: &agentkit_acp::SessionId,
    driver: &mut LoopDriver<S>,
    tasks: &TaskManagerHandle,
) {
    for task in tasks.list_running().await {
        if let Err(error) = tasks.cancel(task.id).await {
            eprintln!("failed to cancel ACP task for {session_id}: {error}");
        }
    }
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

async fn drive_prompt<S: ModelSession>(
    session_id: &agentkit_acp::SessionId,
    integration: &AcpIntegration,
    driver: &mut LoopDriver<S>,
    request: PromptRequest,
) -> Result<PromptResponse, AcpRuntimeError> {
    let items = integration.input_port().prompt_to_items(&request)?;
    driver
        .submit_input(items)
        .map_err(|error| record_acp_loop_failure(session_id, &error))?;
    drive_until_pause(session_id, integration, driver, true)
        .await?
        .ok_or_else(|| AcpRuntimeError::Loop("prompt ended without a response".into()))
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
    let _ = drive_until_pause(session_id, integration, driver, false).await?;
    Ok(())
}

async fn drive_until_pause<S: ModelSession>(
    session_id: &agentkit_acp::SessionId,
    integration: &AcpIntegration,
    driver: &mut LoopDriver<S>,
    answer_prompt: bool,
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
                return Ok(Some(PromptResponse::new(reason)));
            }
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                integration.flush_session_updates(session_id).await?;
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
        component(Arc::clone(&runtime), registry.clone())
            .expect("Kit's fixed ACP integration must build")
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

fn capabilities() -> agentkit_acp::AgentCapabilities {
    agentkit_acp::AgentCapabilities::new()
        .load_session(true)
        .prompt_capabilities(
            PromptCapabilities::new()
                .image(true)
                .audio(true)
                .embedded_context(true),
        )
        .session_capabilities(
            SessionCapabilities::new()
                .additional_directories(SessionAdditionalDirectoriesCapabilities::new())
                .close(SessionCloseCapabilities::new()),
        )
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
mod tests {
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
    use agentkit_task_manager::{AsyncTaskManager, RoutingDecision, TaskManager};
    use agentkit_tools_core::{
        Tool, ToolAnnotations, ToolContext, ToolName, ToolRegistry, ToolResult, ToolSpec,
    };
    use async_trait::async_trait;
    use tokio::{
        sync::Notify,
        time::{Duration, timeout},
    };

    use super::*;

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
    async fn shutdown_sends_close_and_rejects_new_registration() {
        let registry = SessionRegistry::new();
        let integration = shutdown_test_integration();
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
            .register(RegisteredSession {
                token,
                session_id,
                integration: Arc::clone(&integration),
                commands: weak_commands,
                actor: actor.abort_handle(),
                completed: completion,
            })
            .unwrap();
        drop(actor);

        registry.shutdown_with_timeout(Duration::from_secs(1)).await;
        assert!(closed.load(Ordering::SeqCst));

        let late_token = registry.next_token();
        let (late_commands, _late_received) = mpsc::channel(1);
        let (late_completed, late_completion) = watch::channel(false);
        let late_actor = tokio::spawn(async move {
            let _completion = CompletionOnDrop(late_completed);
            std::future::pending::<()>().await;
        });
        assert!(
            registry
                .register(RegisteredSession {
                    token: late_token,
                    session_id: agentkit_acp::SessionId::new("too-late"),
                    integration,
                    commands: late_commands.downgrade(),
                    actor: late_actor.abort_handle(),
                    completed: late_completion,
                })
                .is_err()
        );
        late_actor.abort();
        late_actor.await.unwrap_err();
    }

    #[tokio::test]
    async fn shutdown_aborts_actor_at_the_shared_deadline() {
        let registry = SessionRegistry::new();
        let token = registry.next_token();
        let (commands, _received) = mpsc::channel(1);
        let (_completed, completion) = watch::channel(false);
        let actor = tokio::spawn(std::future::pending::<()>());
        registry
            .register(RegisteredSession {
                token,
                session_id: agentkit_acp::SessionId::new("stuck"),
                integration: shutdown_test_integration(),
                commands: commands.downgrade(),
                actor: actor.abort_handle(),
                completed: completion,
            })
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

    struct ScriptAdapter {
        turns: Arc<AtomicUsize>,
        user_items_seen: Arc<AtomicUsize>,
        notification_items_seen: Arc<AtomicUsize>,
    }

    struct ScriptSession {
        turns: Arc<AtomicUsize>,
        user_items_seen: Arc<AtomicUsize>,
        notification_items_seen: Arc<AtomicUsize>,
    }

    struct ScriptTurn {
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
            self.turns.fetch_add(1, Ordering::SeqCst);
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
            let completed = request.transcript.iter().any(|item| {
                item.parts
                    .iter()
                    .any(|part| matches!(part, Part::ToolResult(_)))
            });
            let events = if completed {
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
            } else {
                let call = ToolCallPart {
                    id: ToolCallId::new("background-call"),
                    name: "background-test".into(),
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

    struct BlockingTool {
        spec: ToolSpec,
        entered: Arc<AtomicBool>,
        release: Arc<Notify>,
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

        let response = drive_prompt(
            &acp_session_id,
            &integration,
            &mut driver,
            PromptRequest::new(
                acp_session_id.clone(),
                vec![agentkit_acp::ContentBlock::Text(
                    agentkit_acp::TextContent::new("cancel me"),
                )],
            ),
        )
        .await
        .expect("cancellation must be an ACP response, not an RPC error");

        assert_eq!(response.stop_reason, StopReason::Cancelled);
        drain.abort();
    }

    #[tokio::test]
    async fn completed_background_task_advances_actor_and_emits_unsolicited_update() {
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

        let task_manager =
            AsyncTaskManager::new().routing(|request: &agentkit_tools_core::ToolRequest| {
                if request.tool_name.0 == "background-test" {
                    RoutingDecision::Background
                } else {
                    RoutingDecision::Foreground
                }
            });
        let tasks = task_manager.handle();
        let tools = ToolRegistry::new().with(BlockingTool {
            spec: ToolSpec {
                name: ToolName::new("background-test"),
                description: "controlled background tool".into(),
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
        let actor = tokio::spawn(session_actor(SessionActor {
            session_id: acp_session_id.clone(),
            integration: Arc::clone(&integration),
            binding: SessionBindingGuard::new(Arc::clone(&integration), acp_session_id.clone()),
            driver,
            tasks,
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
        timeout(Duration::from_secs(1), reply_rx)
            .await
            .expect("first prompt remained blocked on the background tool")
            .unwrap()
            .unwrap();
        assert_eq!(turns.load(Ordering::SeqCst), 1);
        timeout(Duration::from_secs(1), async {
            while !entered.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background tool never started");

        release.notify_one();
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
        assert_eq!(turns.load(Ordering::SeqCst), 2);
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
        assert_eq!(turns.load(Ordering::SeqCst), 3);
        assert_eq!(notification_items_seen.load(Ordering::SeqCst), 1);
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

        timeout(Duration::from_secs(2), attached_rx)
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
        timeout(Duration::from_secs(2), client)
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

    #[tokio::test]
    async fn kit_server_advertises_only_supported_session_restoration() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
        let (client_transport, agent_transport) = Channel::duplex();
        let server = tokio::spawn(serve_transport(runtime, agent_transport));

        agent_client_protocol::Client
            .builder()
            .connect_with(client_transport, async move |connection| {
                let initialized = connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert!(initialized.agent_capabilities.load_session);
                let sessions = &initialized.agent_capabilities.session_capabilities;
                assert!(sessions.list.is_none());
                assert!(sessions.resume.is_none());
                assert!(sessions.fork.is_none());

                connection
                    .send_request(agentkit_acp::ForkSessionRequest::new(
                        "missing",
                        root.path().to_path_buf(),
                    ))
                    .block_task()
                    .await
                    .expect_err("the Kit ACP server has no session/fork route");
                Ok(())
            })
            .await
            .unwrap();

        server.abort();
        let _ = server.await;
    }
}
