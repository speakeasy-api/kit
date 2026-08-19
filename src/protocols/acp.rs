use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use agent_client_protocol::{Client, ConnectionTo, Handled};
use agentkit_acp::{
    AcpClientHandle, AcpClientMessage, AcpIntegration, AcpRuntimeError, AcpSessionBinding,
    AutoDenyResolver, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
    PromptCapabilities, PromptRequest, PromptResponse, SessionAdditionalDirectoriesCapabilities,
    SessionCapabilities, SessionCloseCapabilities, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigSelectGroup, SessionConfigSelectOption,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, StopReason,
};
use agentkit_core::{
    CancellationController, FinishReason, MetadataMap, SessionId as AgentkitSessionId,
};
use agentkit_loop::{LoopDriver, LoopError, LoopInterrupt, LoopStep, ModelSession};
use agentkit_task_manager::{TaskEvent, TaskManagerHandle};
use serde_json::json;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::{
    provider::{ModelGroup, ModelSelection, SelectableAdapter, model_catalog},
    runtime::{AcpDriverContext, Runtime},
};

const MODEL_CONFIG_ID: &str = "model";

fn sdk_error(error: AcpRuntimeError) -> agent_client_protocol::Error {
    agent_client_protocol::util::internal_error(error.to_string())
}

enum Command {
    Prompt {
        request: PromptRequest,
        reply: oneshot::Sender<Result<PromptResponse, AcpRuntimeError>>,
    },
    Cancel,
    SetModel {
        request: SetSessionConfigOptionRequest,
        reply: oneshot::Sender<Result<SetSessionConfigOptionResponse, AcpRuntimeError>>,
    },
    Close {
        reply: oneshot::Sender<()>,
    },
}

struct Server {
    runtime: Arc<Runtime>,
    integration: Arc<AcpIntegration>,
    sessions: Mutex<HashMap<agentkit_acp::SessionId, mpsc::Sender<Command>>>,
    next_session: AtomicU64,
}

impl Server {
    fn new(runtime: Arc<Runtime>, integration: AcpIntegration) -> Self {
        Self {
            runtime,
            integration: Arc::new(integration),
            sessions: Mutex::new(HashMap::new()),
            next_session: AtomicU64::new(1),
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
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let acp_session_id = agentkit_acp::SessionId::new(format!("session-{id}"));
        let agentkit_session_id = AgentkitSessionId::new(acp_session_id.to_string());
        let cancellation = CancellationController::new();
        let (client, messages) = AcpClientHandle::channel();
        tokio::spawn(drain_client_messages(messages, connection));

        let mut metadata = MetadataMap::new();
        metadata.insert("acp.cwd".into(), json!(request.cwd));
        metadata.insert(
            "acp.additional_directories".into(),
            json!(request.additional_directories),
        );
        let binding =
            AcpSessionBinding::new(acp_session_id.clone(), agentkit_session_id.clone(), client)
                .cancellation(cancellation)
                .workspace(request.cwd.clone(), request.additional_directories.clone())
                .metadata(metadata.clone());
        let handle = self.integration.bind_session(binding)?;
        let context = AcpDriverContext {
            acp_session_id: acp_session_id.clone(),
            agentkit_session_id,
            cwd: request.cwd,
            additional_directories: request.additional_directories,
            integration: Arc::clone(&self.integration),
            cancellation: handle.cancellation_handle(),
        };
        let driver = match self.runtime.start_acp_driver(context).await {
            Ok(driver) => driver,
            Err(error) => {
                let _ = self.integration.unbind_session(&acp_session_id);
                return Err(error);
            }
        };
        let (tx, rx) = mpsc::channel(8);
        self.sessions
            .lock()
            .await
            .insert(acp_session_id.clone(), tx);
        let current = driver.adapter.selection().map_err(AcpRuntimeError::Loop)?;
        let catalog = model_catalog(&current).await;
        let options = model_options(&current, &catalog);
        tokio::spawn(session_actor(
            acp_session_id.clone(),
            Arc::clone(&self.integration),
            driver.driver,
            driver.tasks,
            driver.adapter,
            catalog,
            rx,
        ));
        Ok(NewSessionResponse::new(acp_session_id).config_options(Some(options)))
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
            .send(Command::SetModel { request, reply: tx })
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
        let sender = self
            .sessions
            .lock()
            .await
            .remove(&request.session_id)
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(request.session_id.to_string()))?;
        let (tx, rx) = oneshot::channel();
        let closed = async {
            sender
                .send(Command::Close { reply: tx })
                .await
                .map_err(|_| AcpRuntimeError::ClientClosed)?;
            rx.await.map_err(|_| AcpRuntimeError::ClientClosed)
        }
        .await;
        let unbound = self.integration.unbind_session(&request.session_id);
        closed?;
        unbound?;
        Ok(CloseSessionResponse::new())
    }

    async fn sender(
        &self,
        session_id: &agentkit_acp::SessionId,
    ) -> Result<mpsc::Sender<Command>, AcpRuntimeError> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| AcpRuntimeError::SessionNotFound(session_id.to_string()))
    }
}

async fn session_actor<S: ModelSession>(
    session_id: agentkit_acp::SessionId,
    integration: Arc<AcpIntegration>,
    mut driver: LoopDriver<S>,
    tasks: TaskManagerHandle,
    adapter: SelectableAdapter,
    catalog: Vec<ModelGroup>,
    mut commands: mpsc::Receiver<Command>,
) {
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
                Some(Command::SetModel { request, reply }) => {
                    let result = set_model(&adapter, &catalog, request);
                    let _ = reply.send(result);
                }
                Some(Command::Close { reply }) => {
                    clean_up_session(&session_id, &mut driver, &tasks).await;
                    let _ = reply.send(());
                    break;
                }
                None => {
                    clean_up_session(&session_id, &mut driver, &tasks).await;
                    break;
                },
            },
            // Task events remain queued while a prompt is being driven, so a
            // completion cannot be lost in the prompt-to-idle transition.
            event = tasks.next_event() => match event {
                Some(TaskEvent::Completed(snapshot, _))
                    if snapshot.kind == agentkit_task_manager::TaskKind::Background =>
                {
                    if let Err(error) = drive_autonomous(
                        &session_id,
                        &integration,
                        &mut driver,
                    ).await {
                        eprintln!("autonomous ACP continuation failed for {session_id}: {error}");
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
    }
}

fn model_options(current: &ModelSelection, catalog: &[ModelGroup]) -> Vec<SessionConfigOption> {
    let groups = catalog
        .iter()
        .map(|group| {
            let provider = group.provider.as_str();
            let name = match group.provider {
                crate::ProviderKind::OpenAiSubscription => "OpenAI subscription",
                crate::ProviderKind::OpenRouter => "OpenRouter",
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
    vec![
        SessionConfigOption::select(MODEL_CONFIG_ID, "Model", current.id(), groups)
            .category(SessionConfigOptionCategory::Model),
    ]
}

fn set_model(
    adapter: &SelectableAdapter,
    catalog: &[ModelGroup],
    request: SetSessionConfigOptionRequest,
) -> Result<SetSessionConfigOptionResponse, AcpRuntimeError> {
    if request.config_id.to_string() != MODEL_CONFIG_ID {
        return Err(AcpRuntimeError::Unsupported(
            "unknown session configuration option".into(),
        ));
    }
    let value = request
        .value
        .as_value_id()
        .ok_or_else(|| AcpRuntimeError::Unsupported("model selection requires an id value".into()))?
        .to_string();
    let selection = ModelSelection::from_id(&value).map_err(AcpRuntimeError::Loop)?;
    let offered = catalog.iter().any(|group| {
        group.provider == selection.provider && group.models.contains(&selection.model)
    });
    if !offered {
        return Err(AcpRuntimeError::Unsupported(
            "model is not in the advertised catalog".into(),
        ));
    }
    adapter
        .select(selection.clone())
        .map_err(AcpRuntimeError::Loop)?;
    Ok(SetSessionConfigOptionResponse::new(model_options(
        &selection, catalog,
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

async fn drive_prompt<S: ModelSession>(
    session_id: &agentkit_acp::SessionId,
    integration: &AcpIntegration,
    driver: &mut LoopDriver<S>,
    request: PromptRequest,
) -> Result<PromptResponse, AcpRuntimeError> {
    let items = integration.input_port().prompt_to_items(&request)?;
    driver
        .submit_input(items)
        .map_err(|error| AcpRuntimeError::Loop(error.to_string()))?;
    drive_until_pause(session_id, integration, driver, true)
        .await?
        .ok_or_else(|| AcpRuntimeError::Loop("prompt ended without a response".into()))
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
            Err(error) => return Err(AcpRuntimeError::Loop(error.to_string())),
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

async fn serve_transport(
    runtime: Arc<Runtime>,
    transport: impl agent_client_protocol::ConnectTo<agent_client_protocol::Agent> + 'static,
) -> Result<(), AcpRuntimeError> {
    let integration = AcpIntegration::builder()
        .name("kit")
        .approval_resolver(AutoDenyResolver)
        .build()?;
    let state = Arc::new(Server::new(runtime, integration));
    agent_client_protocol::Agent
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
                        responder.respond_with_result(
                            state
                                .new_session(request, connection)
                                .await
                                .map_err(sdk_error),
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
        )
        .connect_to(transport)
        .await
        .map_err(|error| AcpRuntimeError::Sdk(error.to_string()))
}

fn capabilities() -> agentkit_acp::AgentCapabilities {
    agentkit_acp::AgentCapabilities::new()
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
        sync::atomic::{AtomicBool, AtomicUsize},
    };

    use agent_client_protocol::{Channel, schema::ProtocolVersion};
    use agentkit_core::{
        Delta, Item, ItemKind, Part, PartId, PartKind, ToolCallId, ToolCallPart, ToolOutput,
        ToolResultPart, TurnCancellation,
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

    struct ScriptAdapter {
        turns: Arc<AtomicUsize>,
        user_items_seen: Arc<AtomicUsize>,
    }

    struct ScriptSession {
        turns: Arc<AtomicUsize>,
        user_items_seen: Arc<AtomicUsize>,
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
        let acp_session_id = agentkit_acp::SessionId::new("cancel-session");
        let agentkit_session_id = AgentkitSessionId::new("cancel-loop");
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
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(Notify::new());
        let integration = Arc::new(
            AcpIntegration::builder()
                .name("actor-test")
                .approval_resolver(AutoDenyResolver)
                .build()
                .unwrap(),
        );
        let acp_session_id = agentkit_acp::SessionId::new("actor-session");
        let agentkit_session_id = AgentkitSessionId::new("actor-loop");
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
        let actor = tokio::spawn(session_actor(
            acp_session_id.clone(),
            Arc::clone(&integration),
            driver,
            tasks,
            SelectableAdapter::new(crate::ProviderKind::OpenAiSubscription, "gpt-5.4").unwrap(),
            Vec::new(),
            commands_rx,
        ));

        let (reply_tx, reply_rx) = oneshot::channel();
        commands_tx
            .send(Command::Prompt {
                request: PromptRequest::new(
                    acp_session_id,
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
        assert_eq!(notification.session_id.to_string(), "actor-session");
        assert_eq!(turns.load(Ordering::SeqCst), 2);
        assert_eq!(
            user_items_seen.load(Ordering::SeqCst),
            1,
            "autonomous progress must not insert synthetic user content"
        );

        let (close_tx, close_rx) = oneshot::channel();
        commands_tx
            .send(Command::Close { reply: close_tx })
            .await
            .unwrap();
        timeout(Duration::from_secs(1), close_rx)
            .await
            .unwrap()
            .unwrap();
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

        let response = set_model(
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
        assert_eq!(response.config_options.len(), 1);
    }

    #[tokio::test]
    async fn kit_server_does_not_advertise_or_handle_session_fork() {
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
                assert!(
                    initialized
                        .agent_capabilities
                        .session_capabilities
                        .fork
                        .is_none()
                );

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
