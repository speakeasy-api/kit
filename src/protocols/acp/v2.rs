use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
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
    CancellationController, FinishReason, Item, ItemKind, Part, SessionId, ToolOutput,
};
use agentkit_loop::{LoopDriver, LoopError, LoopInterrupt, LoopStep, ModelSession};
use agentkit_task_manager::{TaskEvent, TaskManagerHandle};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    provider::{SelectableAdapter, model_catalog},
    runtime::{AcpDriverContext, BackgroundJobs, Runtime},
};

use super::{
    CancelBackgroundRequest, CancelBackgroundResponse, DetachComposeRequest, DetachComposeResponse,
    SessionRegistry,
};

const PAGE_SIZE: usize = 100;

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
    agent_client_protocol::util::internal_error(error.to_string())
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
) -> Result<wire::StopReason, AcpRuntimeError> {
    if matches!(error, LoopError::Cancelled) {
        Ok(wire::StopReason::Cancelled)
    } else {
        Err(map_loop_error(session_id, error))
    }
}

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

struct Server {
    runtime: Arc<Runtime>,
    integration: Arc<AcpIntegration>,
    registry: SessionRegistry,
    sessions: Mutex<HashMap<wire::SessionId, SessionHandle>>,
}

impl Server {
    fn new(runtime: Arc<Runtime>, registry: SessionRegistry) -> Self {
        Self {
            runtime,
            integration: Arc::new(AcpIntegration::default()),
            registry,
            sessions: Mutex::new(HashMap::new()),
        }
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
        .capabilities(agentkit_acp::v2::agent_capabilities()))
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

    fn list_sessions(
        &self,
        request: wire::ListSessionsRequest,
    ) -> Result<wire::ListSessionsResponse, AcpRuntimeError> {
        let cwd = self.runtime.root().to_path_buf();
        if request
            .cwd
            .as_ref()
            .is_some_and(|requested| requested.0 != cwd)
        {
            return Ok(wire::ListSessionsResponse::new(Vec::new()));
        }
        let offset = request
            .cursor
            .as_ref()
            .map(|cursor| parse_cursor(cursor.as_ref()))
            .transpose()?
            .unwrap_or(0);
        let ids = crate::session::list_ids(self.runtime.root()).map_err(AcpRuntimeError::Loop)?;
        if offset > ids.len() {
            return Err(AcpRuntimeError::Unsupported(
                "invalid session list cursor".into(),
            ));
        }
        let end = ids.len().min(offset + PAGE_SIZE);
        let sessions = ids[offset..end]
            .iter()
            .map(|id| wire::SessionInfo::new(wire::SessionId::new(id.as_str()), cwd.clone()))
            .collect();
        let next = (end < ids.len()).then(|| wire::SessionListCursor::new(format!("offset:{end}")));
        Ok(wire::ListSessionsResponse::new(sessions).next_cursor(next))
    }

    async fn attach_session(
        self: &Arc<Self>,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        connection: V2ConnectionTo<Client>,
        mut claim: crate::runtime::SessionClaim,
    ) -> Result<AttachedSession, AcpRuntimeError> {
        let session_id = wire::SessionId::new(claim.id());
        let cancellation = CancellationController::new();
        let sink = ConnectionSink(connection);
        let binding =
            AcpSessionBinding::new(session_id.clone(), SessionId::new(claim.id()), sink.clone())
                .cancellation(cancellation);
        let handle = self.integration.bind_session(binding)?;
        let binding = BindingGuard {
            integration: Arc::clone(&self.integration),
            session_id: session_id.clone(),
        };
        let context = AcpDriverContext {
            cwd,
            additional_directories,
            integration: Arc::clone(&self.integration),
            cancellation: handle.cancellation_handle(),
        };
        let driver = self.runtime.start_acp_driver(context, &mut claim).await?;
        let current = driver.adapter.selection().map_err(AcpRuntimeError::Loop)?;
        let reasoning = driver
            .adapter
            .reasoning_effort()
            .map_err(AcpRuntimeError::Loop)?;
        let catalog = model_catalog(&current).await;
        let config_options = v2_config_options(&current, reasoning, &catalog);
        let canonical_transcript = driver.canonical_transcript;
        let background_jobs = driver.background_jobs.clone();
        let tasks = driver.tasks.clone();
        let mcp_events = self.runtime.subscribe_mcp(session_id.to_string());
        let (tx, rx) = mpsc::channel(8);
        let busy = Arc::new(AtomicBool::new(false));
        let actor = SessionActor {
            session_id: session_id.clone(),
            integration: Arc::clone(&self.integration),
            handle: handle.clone(),
            busy: Arc::clone(&busy),
            binding,
            sink,
            driver: driver.driver,
            tasks: driver.tasks,
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
        let interrupt = Arc::new(move || interrupt_handle.interrupt());
        let weak = tx.downgrade();
        let close = Arc::new(move || {
            let weak = weak.clone();
            Box::pin(async move {
                if let Some(commands) = weak.upgrade() {
                    let (reply, acknowledged) = oneshot::channel();
                    if commands.send(Command::Close { reply }).await.is_ok() {
                        let _ = acknowledged.await;
                    }
                }
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        });
        self.registry
            .register_v2(
                token,
                interrupt,
                close,
                actor_task.abort_handle(),
                completion,
            )
            .map_err(|()| AcpRuntimeError::ClientClosed)?;
        drop(actor_task);
        if let Err(error) = claim.commit() {
            self.registry.remove(token);
            return Err(error);
        }
        crate::events::emit(&crate::events::RuntimeEvent::SessionStarted {
            session_id: session_id.to_string(),
        });
        self.sessions
            .lock()
            .expect("ACP v2 session map poisoned")
            .insert(
                session_id.clone(),
                SessionHandle {
                    token,
                    commands: tx,
                    integration: handle,
                    busy,
                    background_jobs,
                    tasks,
                },
            );
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
        let handle = self
            .sessions
            .lock()
            .expect("ACP v2 session map poisoned")
            .get(&notification.session_id)
            .map(|session| session.integration.clone());
        if let Some(handle) = handle {
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
    integration: Arc<AcpIntegration>,
    handle: AcpSessionHandle,
    busy: Arc<AtomicBool>,
    binding: BindingGuard,
    sink: ConnectionSink,
    driver: LoopDriver<S>,
    tasks: TaskManagerHandle,
    adapter: SelectableAdapter,
    catalog: Vec<crate::provider::ModelGroup>,
    commands: mpsc::Receiver<Command>,
    mcp_events: crate::tools::mcp::McpSubscription,
}

async fn session_actor<S: ModelSession + Send + 'static>(actor: SessionActor<S>) {
    let SessionActor {
        session_id,
        integration,
        handle,
        busy,
        binding,
        sink,
        mut driver,
        tasks,
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
                        &integration,
                        &handle,
                        &mut driver,
                        command,
                        &sink,
                    )
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
                    super::clean_up_session(&v1_id, &mut driver, &tasks).await;
                    drop(binding.take());
                    let _ = reply.send(());
                    break;
                }
                None => {
                    let v1_id = agentkit_acp::SessionId::new(session_id.to_string());
                    super::clean_up_session(&v1_id, &mut driver, &tasks).await;
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
                        ).await,
                        Err(error) => Err(map_loop_error(&session_id, &error)),
                    };
                    if let Err(error) = result {
                        eprintln!("ACP v2 autonomous turn failed for {session_id}: {error}");
                    }
                }
            }
            event = tasks.next_event() => match event {
                Some(TaskEvent::Completed(snapshot, _))
                    if snapshot.kind == agentkit_task_manager::TaskKind::Background =>
                {
                    if let Err(error) = drive_autonomous(
                        &session_id,
                        &integration,
                        &handle,
                        &busy,
                        &mut driver,
                        &sink,
                    ).await {
                        eprintln!("ACP v2 autonomous turn failed for {session_id}: {error}");
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
    }
}

async fn prepare_prompt<S: ModelSession + Send + 'static>(
    session_id: &wire::SessionId,
    integration: &AcpIntegration,
    handle: &AcpSessionHandle,
    driver: &mut LoopDriver<S>,
    command: PromptCommand,
    sink: &ConnectionSink,
) -> Result<(), AcpRuntimeError> {
    let PromptCommand {
        request,
        cancellation_generation,
        reply,
    } = command;
    let prepared = integration.prompt_to_items(&request).and_then(|items| {
        driver
            .submit_input(items)
            .map_err(|error| map_loop_error(session_id, &error))?;
        integration.begin_prompt(session_id)
    });
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
        send_state(
            sink,
            session_id,
            wire::StateUpdate::Running(wire::RunningStateUpdate::new()),
        )?;
        let stop_reason = drive_prompt(session_id, driver, handle, cancellation_generation).await?;
        let _ = integration.flush_session_updates(session_id).await;
        integration.finish_prompt(session_id);
        send_state(
            sink,
            session_id,
            wire::StateUpdate::Idle(wire::IdleStateUpdate::new().stop_reason(stop_reason)),
        )
    }
    .await;
    integration.finish_prompt(session_id);
    handle.stop_injection_turn();
    result
}

async fn drive_prompt<S: ModelSession + Send + 'static>(
    session_id: &wire::SessionId,
    driver: &mut LoopDriver<S>,
    handle: &AcpSessionHandle,
    cancellation_generation: u64,
) -> Result<wire::StopReason, AcpRuntimeError> {
    let cancellation = handle.cancellation_handle();
    loop {
        let step = match driver.next().await {
            Ok(step) => step,
            Err(error) => {
                handle.stop_injection_turn();
                return loop_error_stop_reason(session_id, &error);
            }
        };
        if cancellation.is_cancelled_since(cancellation_generation) {
            return Ok(wire::StopReason::Cancelled);
        }
        match step {
            LoopStep::Finished(result) => {
                if result.finish_reason == FinishReason::ToolCall {
                    continue;
                }
                match handle.handle_injection_boundary(driver, true).await {
                    Ok(AcpInjectionBoundary::Delivered | AcpInjectionBoundary::Continue) => {
                        continue;
                    }
                    Ok(AcpInjectionBoundary::Stopped) => {
                        return Ok(wire::StopReason::Cancelled);
                    }
                    Ok(AcpInjectionBoundary::Finished) => {
                        return Ok(finish_reason_to_stop_reason(&result.finish_reason));
                    }
                    Err(_) => return Ok(error_stop_reason()),
                }
            }
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                match handle.handle_injection_boundary(driver, true).await {
                    Ok(AcpInjectionBoundary::Delivered | AcpInjectionBoundary::Continue) => {
                        continue;
                    }
                    Ok(AcpInjectionBoundary::Stopped) => {
                        return Ok(wire::StopReason::Cancelled);
                    }
                    Ok(AcpInjectionBoundary::Finished) => return Ok(wire::StopReason::EndTurn),
                    Err(_) => return Ok(error_stop_reason()),
                }
            }
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {
                match handle.handle_injection_boundary(driver, false).await {
                    Ok(AcpInjectionBoundary::Stopped) => {
                        return Ok(wire::StopReason::Cancelled);
                    }
                    Err(_) => return Ok(error_stop_reason()),
                    _ => {}
                }
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(_)) => {
                if let Err(error) = driver.cancel_pending_approvals().await {
                    handle.stop_injection_turn();
                    return loop_error_stop_reason(session_id, &error);
                }
            }
        }
    }
}

async fn drive_autonomous<S: ModelSession + Send + 'static>(
    session_id: &wire::SessionId,
    integration: &AcpIntegration,
    handle: &AcpSessionHandle,
    busy: &AtomicBool,
    driver: &mut LoopDriver<S>,
    sink: &ConnectionSink,
) -> Result<(), AcpRuntimeError> {
    if claim_prompt(busy).is_err() {
        return Ok(());
    }
    handle.prepare_injection_turn();
    integration.finish_prompt(session_id);
    let cancellation_generation = handle.cancellation_handle().generation();
    handle.start_injection_turn();
    let result = async {
        send_state(
            sink,
            session_id,
            wire::StateUpdate::Running(wire::RunningStateUpdate::new()),
        )?;
        let stop_reason = drive_prompt(session_id, driver, handle, cancellation_generation).await?;
        let _ = integration.flush_session_updates(session_id).await;
        send_state(
            sink,
            session_id,
            wire::StateUpdate::Idle(wire::IdleStateUpdate::new().stop_reason(stop_reason)),
        )
    }
    .await;
    integration.finish_prompt(session_id);
    handle.stop_injection_turn();
    busy.store(false, Ordering::Release);
    result
}

fn send_state(
    sink: &ConnectionSink,
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

fn parse_cursor(cursor: &str) -> Result<usize, AcpRuntimeError> {
    cursor
        .strip_prefix("offset:")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| AcpRuntimeError::Unsupported("invalid session list cursor".into()))
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
    serve_transport(runtime, agent_client_protocol::Stdio::new()).await
}

pub async fn serve_with_registry(
    runtime: Arc<Runtime>,
    registry: SessionRegistry,
) -> Result<(), AcpRuntimeError> {
    v2_router(runtime, registry)?
        .connect_to(agent_client_protocol::Stdio::new())
        .await
        .map_err(|error| AcpRuntimeError::Sdk(error.to_string()))
}

async fn serve_transport(
    runtime: Arc<Runtime>,
    transport: impl ConnectTo<agent_client_protocol::Agent> + 'static,
) -> Result<(), AcpRuntimeError> {
    let registry = SessionRegistry::new();
    let result = v2_router(runtime, registry.clone())?
        .connect_to(transport)
        .await
        .map_err(|error| AcpRuntimeError::Sdk(error.to_string()));
    registry.shutdown().await;
    result
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
                async move |request: wire::ListSessionsRequest, responder, _cx| {
                    responder.respond_with_result(state.list_sessions(request).map_err(sdk_error))
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
    use serde_json::json;

    use super::*;

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
            wire::StopReason::Cancelled
        );
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
    fn initialize_negotiates_v2_and_advertises_injection() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
        let server = Server::new(runtime, SessionRegistry::new());
        let response = server
            .initialize(wire::InitializeRequest::new(
                wire::ProtocolVersion::V2,
                wire::Implementation::new("test-client", "0"),
            ))
            .unwrap();

        assert_eq!(response.protocol_version, wire::ProtocolVersion::V2);
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
    fn cursors_are_stable_and_reject_malformed_values() {
        assert_eq!(parse_cursor("offset:100").unwrap(), 100);
        assert!(parse_cursor("100").is_err());
        assert!(parse_cursor("offset:nope").is_err());
    }
}
