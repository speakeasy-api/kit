mod partial_delivery;

use super::*;

/// The provider boundary records actual requests, not driver implementation work.
struct ContextAdapter {
    turns: Arc<AtomicU64>,
    requests: Arc<Mutex<Vec<Vec<Item>>>>,
}

struct ContextSession {
    inner: TestSession,
    requests: Arc<Mutex<Vec<Vec<Item>>>>,
}

#[async_trait]
impl ModelAdapter for ContextAdapter {
    type Session = ContextSession;

    async fn start_session(&self, config: SessionConfig) -> Result<Self::Session, LoopError> {
        Ok(ContextSession {
            inner: TestAdapter {
                outcome: TestOutcome::Content,
                turns: self.turns.clone(),
                interrupt: None,
            }
            .start_session(config)
            .await?,
            requests: self.requests.clone(),
        })
    }
}

#[async_trait]
impl ModelSession for ContextSession {
    type Turn = TestTurn;

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        self.requests
            .lock()
            .unwrap()
            .push(request.transcript.clone());
        self.inner.begin_turn(request, cancellation).await
    }
}

struct InjectionWire {
    client: agent_client_protocol::Channel,
    server: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
    receipts: mpsc::UnboundedReceiver<agentkit_acp::v2::AcpInjectAcceptance>,
}

impl Drop for InjectionWire {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl InjectionWire {
    async fn connect(
        integration: Arc<AcpIntegration>,
        work: Arc<Mutex<InjectionWork>>,
    ) -> (Self, V2ConnectionTo<Client>) {
        let (mut client, agent) = agent_client_protocol::Channel::duplex();
        let (connections, mut connection) = mpsc::unbounded_channel();
        let (accepted, receipts) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            agent_client_protocol::Agent
                .v2()
                .on_receive_request(
                    async move |request: wire::InitializeRequest, responder, cx| {
                        connections.send(cx.clone()).unwrap();
                        responder.respond(
                            wire::InitializeResponse::new(
                                request.protocol_version,
                                wire::Implementation::new("review-injection-test", "0"),
                            )
                            .capabilities(agentkit_acp::v2::agent_capabilities()),
                        )
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: wire::InjectSessionRequest, responder, cx| {
                        let integration = integration.clone();
                        let accepted = accepted.clone();
                        let work = work.clone();
                        cx.spawn(async move {
                            let Some(reserved) = integration
                                .reserve_inject_request(request, responder)
                                .await?
                            else {
                                return Ok(());
                            };
                            let id = reserved.response().message_id;
                            work.lock().unwrap().pending.insert(id.clone());
                            let mut pending = TrackedInjection {
                                work,
                                id,
                                retained: false,
                            };
                            let receipt = reserved.respond_tracked()?.expect("tracked acceptance");
                            pending.retained = true;
                            accepted.send(receipt).ok();
                            Ok(())
                        })?;
                        Ok(())
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_to(agent)
                .await
        });
        send_wire(
            &client,
            "initialize",
            1,
            serde_json::to_value(wire::InitializeRequest::new(
                wire::ProtocolVersion::V2,
                wire::Implementation::new("review-injection-client", "0"),
            ))
            .unwrap(),
        );
        assert!(receive_wire(&mut client).await.get("result").is_some());
        let connection = connection.recv().await.unwrap();
        (
            Self {
                client,
                server,
                receipts,
            },
            connection,
        )
    }

    async fn inject(
        &mut self,
        session_id: &wire::SessionId,
        request_id: i64,
        text: &str,
    ) -> agentkit_acp::v2::AcpInjectAcceptance {
        send_wire(
            &self.client,
            "session/inject",
            request_id,
            serde_json::to_value(wire::InjectSessionRequest::new(
                session_id.clone(),
                wire::SessionInjectMode::Steer,
                vec![wire::ContentBlock::Text(wire::TextContent::new(text))],
            ))
            .unwrap(),
        );
        let response = receive_wire(&mut self.client).await;
        assert!(
            response.get("result").is_some(),
            "injection failed: {response}"
        );
        self.receipts.recv().await.unwrap()
    }

    async fn update(&mut self) -> wire::UpdateSessionNotification {
        let message = receive_wire(&mut self.client).await;
        assert_eq!(message["method"], "session/update", "{message}");
        serde_json::from_value(message["params"].clone()).unwrap()
    }
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Prompt,
    Autonomous,
    InitialBranch,
    SetConfig,
}

fn text_count(transcript: &[Item], expected: &str) -> usize {
    transcript
        .iter()
        .filter(|item| {
            item.kind == ItemKind::User
                && item
                    .parts
                    .iter()
                    .any(|part| matches!(part, Part::Text(text) if text.text == expected))
        })
        .count()
}

async fn snapshot_actor(commands: &mpsc::Sender<Command>) -> SessionSnapshot {
    let (reply, response) = oneshot::channel();
    commands.send(Command::Snapshot { reply }).await.unwrap();
    timeout(Duration::from_secs(5), response)
        .await
        .unwrap()
        .expect("cancelled delivery must retain the same actor")
        .unwrap()
}

async fn queue_prompt(
    commands: &mpsc::Sender<Command>,
    session_id: &wire::SessionId,
    handle: &AcpSessionHandle,
    text: &str,
) -> oneshot::Sender<()> {
    let (reply, response) = oneshot::channel();
    commands
        .send(Command::Prompt(PromptCommand {
            request: wire::PromptRequest::new(
                session_id.clone(),
                vec![wire::ContentBlock::Text(wire::TextContent::new(text))],
            ),
            cancellation_generation: handle.cancellation_handle().generation(),
            reply,
        }))
        .await
        .unwrap();
    timeout(Duration::from_secs(5), response)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

async fn cancelled_delivery_keeps_actor(entry: Entry) {
    const FIRST: &str = "/compact cancelled steering";
    const SECOND: &str = "second unactivated steering";
    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new_with_provider_and_credentials(
        root.path(),
        "gpt-5.4",
        ProviderKind::OpenAiSubscription,
        crate::credentials::CredentialStorage::Memory,
    )
    .unwrap();
    let durable_id = crate::session::new_id();
    let session_id = wire::SessionId::new(durable_id.clone());
    let loop_id = SessionId::new(durable_id.clone());
    let opened = crate::session::open(
        root.path(),
        &durable_id,
        false,
        false,
        vec![
            Item::text(ItemKind::System, "system"),
            Item::text(ItemKind::User, "older content ".repeat(20_000)),
            Item::text(ItemKind::Assistant, "recent response")
                .with_usage(Usage::new(agentkit_core::TokenUsage::new(100, 0))),
        ],
    )
    .unwrap();
    let integration = Arc::new(AcpIntegration::default());
    let work = Arc::new(Mutex::new(InjectionWork::default()));
    let (mut wire_client, connection) =
        InjectionWire::connect(integration.clone(), work.clone()).await;
    let sink = ResponseReplacementSink::new(ConnectionSink(connection, work.clone()));
    let activity = native_activity(session_id.clone(), sink.clone());
    let handle = integration
        .bind_session(AcpSessionBinding::new(
            session_id.clone(),
            loop_id.clone(),
            sink.clone(),
        ))
        .unwrap();
    handle.prepare_injection_turn();
    handle.start_injection_turn();
    let generation = handle.cancellation_handle().generation();
    let first = wire_client.inject(&session_id, 2, FIRST).await;
    let first_id = first.message_id().clone();
    first.activate_after_response().await.unwrap();
    let second = wire_client.inject(&session_id, 3, SECOND).await;
    let second_id = second.message_id().clone();
    assert_eq!(work.lock().unwrap().pending.len(), 2);

    let selection = SelectableAdapter::new_with_credentials(
        ProviderKind::OpenAiSubscription,
        "gpt-5.4",
        crate::credentials::CredentialStorage::Memory,
    )
    .unwrap();
    let summaries = Arc::new(Mutex::new(Vec::new()));
    let compactor = crate::compaction::automatic(
        SwitchSummaryAdapter {
            selection: selection.clone(),
            seen: summaries.clone(),
            outcome: TestOutcome::Content,
            interrupt: None,
        },
        Default::default(),
        Some(opened.observer.clone()),
        loop_id.clone(),
    )
    .unwrap();
    let turns = Arc::new(AtomicU64::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let manager = AsyncTaskManager::new();
    let tasks = manager.handle();
    let settlement = crate::runtime::InputSettlement::default();
    let initial_branch = matches!(entry, Entry::InitialBranch);
    let driver = Agent::builder()
        .mutator(settlement.clone())
        .mutator(compactor)
        .model(ContextAdapter {
            turns: turns.clone(),
            requests: requests.clone(),
        })
        .task_manager(manager)
        .observer(ResponseReplacementObserver::new(
            (*integration).clone(),
            sink.clone(),
            session_id.clone(),
            activity.clone(),
        ))
        .transcript_observer(settlement.observer(opened.observer))
        .transcript(opened.transcript)
        .input(if initial_branch {
            vec![Item::text(ItemKind::User, "initial branch prompt")]
        } else {
            vec![]
        })
        .cancellation(handle.cancellation_handle())
        .build()
        .unwrap()
        .start(SessionConfig::new(loop_id).without_cache())
        .await
        .unwrap();
    let busy = Arc::new(AtomicBool::new(initial_branch));
    let (commands, receiver) = mpsc::channel(8);
    let mcp = crate::tools::mcp::empty();
    let events = mcp.subscribe(durable_id.clone());
    if matches!(entry, Entry::Autonomous) {
        mcp.publish(
            &durable_id,
            crate::tools::mcp::McpEvent {
                message: "autonomous trigger".into(),
            },
        );
    }
    let actor = tokio::spawn(session_actor(SessionActor {
        initial_generation: initial_branch.then_some(generation),
        admission_released: Arc::new(Notify::new()),
        session_id: session_id.clone(),
        runtime,
        integration: integration.clone(),
        handle: handle.clone(),
        busy: busy.clone(),
        binding: BindingGuard {
            integration,
            session_id: session_id.clone(),
        },
        sink,
        activity,
        driver: settlement.wrap(driver),
        tasks,
        background_jobs: BackgroundJobs::default(),
        structured_completion: false,
        skill_catalog: skill_catalog::SkillCatalogMonitor::new(&[]).unwrap(),
        adapter: selection.clone(),
        catalog: vec![crate::provider::ModelGroup {
            provider: ProviderKind::OpenAiSubscription,
            models: vec!["gpt-5.4-mini".into()],
            context_windows: [("gpt-5.4-mini".into(), 150)].into_iter().collect(),
        }],
        commands: receiver,
        mcp_events: events,
    }));
    let mut config_response = None;
    match entry {
        Entry::Prompt => {
            claim_prompt(&busy).unwrap();
            queue_prompt(&commands, &session_id, &handle, "original prompt")
                .await
                .send(())
                .unwrap();
        }
        Entry::SetConfig => {
            let mut request = wire::SetSessionConfigOptionRequest::new(
                session_id.clone(),
                super::super::super::MODEL_CONFIG_ID,
                "openai-subscription:gpt-5.4-mini",
            );
            let (reply, response) = oneshot::channel();
            commands
                .send(Command::SetConfig {
                    request: request.clone(),
                    reply,
                    cancellation_generation: generation,
                })
                .await
                .unwrap();
            let warning = response.await.unwrap().unwrap_err();
            let warning: model_switch::Warning =
                serde_json::from_value(warning.data.unwrap()[model_switch::META].clone()).unwrap();
            assert!(summaries.lock().unwrap().is_empty());
            request.meta = Some(serde_json::Map::from_iter([(
                model_switch::META.into(),
                serde_json::to_value(model_switch::Confirmation {
                    token: warning.token,
                    action: model_switch::Decision::Compact,
                })
                .unwrap(),
            )]));
            let (reply, response) = oneshot::channel();
            commands
                .send(Command::SetConfig {
                    request,
                    reply,
                    cancellation_generation: generation,
                })
                .await
                .unwrap();
            config_response = Some(response);
        }
        Entry::Autonomous | Entry::InitialBranch => {}
    }

    let mut updates = Vec::new();
    loop {
        let update = wire_client.update().await;
        let delivered = matches!(&update.update, wire::SessionUpdate::UserMessage(message)
            if message.message_id == first_id);
        updates.push(update);
        if delivered {
            break;
        }
    }
    assert!(!work.lock().unwrap().pending.contains(&first_id));
    assert!(work.lock().unwrap().pending.contains(&second_id));
    let calls_before_cancel = turns.load(Ordering::Relaxed);
    let summaries_before_cancel = summaries.lock().unwrap().len();
    assert_eq!(
        summaries_before_cancel,
        usize::from(matches!(entry, Entry::SetConfig))
    );
    assert_eq!(
        text_count(
            &crate::session::load(root.path(), &durable_id).unwrap(),
            FIRST
        ),
        0,
        "the delivered notification precedes persistence while the second receipt holds the boundary"
    );
    handle.interrupt();
    loop {
        let update = wire_client.update().await;
        let idle = matches!(
            &update.update,
            wire::SessionUpdate::StateUpdate(wire::StateUpdate::Idle(_))
        );
        updates.push(update);
        if idle {
            break;
        }
    }
    let states = updates
        .iter()
        .filter(|update| matches!(update.update, wire::SessionUpdate::StateUpdate(_)))
        .cloned()
        .collect::<Vec<_>>();
    assert_running_then_idle(&states, wire::StopReason::Cancelled);
    assert!(!updates.iter().any(
        |update| matches!(&update.update, wire::SessionUpdate::UserMessage(message)
        if message.message_id == second_id)
    ));
    if let Some(response) = config_response {
        assert!(response.await.unwrap().is_err());
        assert_eq!(selection.selection().unwrap().model, "gpt-5.4");
    }
    assert_eq!(
        text_count(
            &crate::session::load(root.path(), &durable_id).unwrap(),
            FIRST
        ),
        1,
        "cancellation must persist already acknowledged steering"
    );
    let settled = snapshot_actor(&commands).await;
    assert_eq!(text_count(&settled.canonical_transcript, FIRST), 1);
    assert_eq!(text_count(&settled.canonical_transcript, SECOND), 0);
    let persisted = crate::session::load(root.path(), &durable_id).unwrap();
    assert_eq!(persisted, settled.canonical_transcript);
    assert_eq!(turns.load(Ordering::Relaxed), calls_before_cancel);
    assert_eq!(summaries.lock().unwrap().len(), summaries_before_cancel);
    assert!(work.lock().unwrap().pending.contains(&second_id));

    // A real queued Prompt response gate proves the same actor remains usable
    // without accidentally activating the second receipt or executing old work.
    claim_prompt(&busy).unwrap();
    handle.prepare_injection_turn();
    let start = queue_prompt(&commands, &session_id, &handle, "fresh generation prompt").await;
    assert_eq!(turns.load(Ordering::Relaxed), calls_before_cancel);
    assert!(work.lock().unwrap().pending.contains(&second_id));
    second.activate_after_response().await.unwrap();
    start.send(()).unwrap();
    let mut fresh_updates = Vec::new();
    loop {
        let update = wire_client.update().await;
        let idle = matches!(
            &update.update,
            wire::SessionUpdate::StateUpdate(wire::StateUpdate::Idle(_))
        );
        fresh_updates.push(update);
        if idle {
            break;
        }
    }
    let states = fresh_updates
        .iter()
        .filter(|update| matches!(update.update, wire::SessionUpdate::StateUpdate(_)))
        .cloned()
        .collect::<Vec<_>>();
    assert_running_then_idle(&states, wire::StopReason::EndTurn);
    assert_eq!(
        fresh_updates
            .iter()
            .filter(|update| matches!(&update.update,
        wire::SessionUpdate::UserMessage(message) if message.message_id == second_id))
            .count(),
        1
    );
    let final_snapshot = snapshot_actor(&commands).await;
    assert_eq!(text_count(&final_snapshot.canonical_transcript, FIRST), 1);
    assert_eq!(text_count(&final_snapshot.canonical_transcript, SECOND), 1);
    assert_eq!(summaries.lock().unwrap().len(), summaries_before_cancel);
    assert!(work.lock().unwrap().pending.is_empty());
    assert!(requests.lock().unwrap().iter().any(|transcript| text_count(
        transcript,
        "fresh generation prompt"
    ) == 1
        && text_count(transcript, FIRST) == 1));
    assert_eq!(
        crate::session::load(root.path(), &durable_id).unwrap(),
        final_snapshot.canonical_transcript
    );
    let (reply, response) = oneshot::channel();
    commands.send(Command::Close { reply }).await.unwrap();
    response.await.unwrap();
    actor.await.unwrap();
}

#[tokio::test]
async fn delivered_steering_cancellation_retains_prompt_actor() {
    timeout(
        Duration::from_secs(20),
        cancelled_delivery_keeps_actor(Entry::Prompt),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn delivered_steering_cancellation_retains_autonomous_actor() {
    timeout(
        Duration::from_secs(20),
        cancelled_delivery_keeps_actor(Entry::Autonomous),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn delivered_steering_cancellation_retains_initial_branch_actor() {
    timeout(
        Duration::from_secs(20),
        cancelled_delivery_keeps_actor(Entry::InitialBranch),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn delivered_steering_cancellation_retains_compaction_actor() {
    timeout(
        Duration::from_secs(20),
        cancelled_delivery_keeps_actor(Entry::SetConfig),
    )
    .await
    .unwrap();
}
