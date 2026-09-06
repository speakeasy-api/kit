use super::*;
use futures_util::StreamExt;
use std::sync::OnceLock;

/// Fail at the actual transport acknowledgement boundary, not at a driver hook.
/// The immutable fault is installed before the actor starts. Only the external
/// send-attempt counter changes here; normal ConnectionSink tracking is untouched.
/// Retain only the cancellation controller, never the session that owns this sink.
#[derive(Clone)]
struct PartialDeliverySink {
    inner: ConnectionSink,
    fault: Arc<OnceLock<wire::MessageId>>,
    cancellation: CancellationController,
    failures: Arc<AtomicU64>,
    fail_after_observation: Arc<Notify>,
}

#[async_trait]
impl AcpSessionUpdateSink for PartialDeliverySink {
    fn update(&self, notification: wire::UpdateSessionNotification) -> Result<(), AcpRuntimeError> {
        self.inner.update(notification)
    }

    async fn update_acknowledged(
        &self,
        notification: wire::UpdateSessionNotification,
    ) -> Result<(), AcpRuntimeError> {
        if let Some(failed_id) = self.fault.get()
            && matches!(&notification.update, wire::SessionUpdate::UserMessage(message)
                if &message.message_id == failed_id)
        {
            self.failures.fetch_add(1, Ordering::Relaxed);
            // B has already entered driver pending_input, but neither the real
            // transport nor ConnectionSink's delivered bookkeeping accepts it.
            self.fail_after_observation.notified().await;
            self.cancellation.interrupt();
            return Err(AcpRuntimeError::Sdk(
                "injected B notification failure".into(),
            ));
        }
        self.inner.update_acknowledged(notification).await
    }

    async fn flush(&self) -> Result<(), AcpRuntimeError> {
        self.inner.flush().await
    }
}

async fn inject_content(
    client: &mut InjectionWire,
    session_id: &wire::SessionId,
    request_id: i64,
    content: Vec<wire::ContentBlock>,
) -> wire::MessageId {
    send_wire(
        &client.client,
        "session/inject",
        request_id,
        serde_json::to_value(wire::InjectSessionRequest::new(
            session_id.clone(),
            wire::SessionInjectMode::Steer,
            content,
        ))
        .unwrap(),
    );
    let response = receive_wire(&mut client.client).await;
    assert!(response.get("result").is_some(), "{response}");
    let receipt = client.receipts.recv().await.unwrap();
    let id = receipt.message_id().clone();
    receipt.activate_after_response().await.unwrap();
    id
}

fn content_occurrences(transcript: &[Item], content: &[wire::ContentBlock], expected: usize) {
    for item in agentkit_acp::v2::content_blocks_to_items(content).unwrap() {
        assert_eq!(
            transcript
                .iter()
                .filter(|actual| actual.kind == item.kind && actual.parts == item.parts)
                .count(),
            expected,
            "wrong durable multiplicity for {:?}",
            item.parts,
        );
    }
}

async fn partial_delivery_retires_actor(multi_item: bool, identical: bool) {
    let first_text = if identical {
        "identical steering"
    } else {
        "/compact acknowledged steering"
    };
    let second_text = if identical {
        first_text
    } else {
        "unacknowledged steering B"
    };
    let content = |text: &str, resource: &str| {
        let mut content = vec![wire::ContentBlock::Text(wire::TextContent::new(text))];
        if multi_item {
            content.push(wire::ContentBlock::ResourceLink(wire::ResourceLink::new(
                resource,
                format!("file:///{resource}"),
            )));
        }
        content
    };
    let first_content = content(first_text, "acknowledged-context-A");
    let second_content = content(second_text, "unacknowledged-context-B");
    assert_eq!(
        agentkit_acp::v2::content_blocks_to_items(&first_content)
            .unwrap()
            .len(),
        if multi_item { 2 } else { 1 },
        "the multi-item case must exercise a prefix of items, not notifications",
    );
    let root = tempfile::tempdir().unwrap();
    let durable_id = crate::session::new_id();
    let session_id = wire::SessionId::new(durable_id.clone());
    let integration = Arc::new(AcpIntegration::default());
    let turns = Arc::new(AtomicU64::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let summaries = Arc::new(Mutex::new(Vec::new()));

    // Attempt 1 is a new attachment and disk reload, not an automatic retry on
    // the failed driver. Reuse the integration and session ID to test unbinding.
    for reconnect in [false, true] {
        let runtime = Runtime::new_with_provider_and_credentials(
            root.path(),
            "gpt-5.4",
            ProviderKind::OpenAiSubscription,
            crate::credentials::CredentialStorage::Memory,
        )
        .unwrap();
        let loop_id = SessionId::new(durable_id.clone());
        let opened = crate::session::open(
            root.path(),
            &durable_id,
            reconnect,
            false,
            if reconnect {
                vec![]
            } else {
                vec![
                    Item::text(ItemKind::System, "system"),
                    Item::text(ItemKind::User, "older content ".repeat(20_000)),
                    Item::text(ItemKind::Assistant, "recent response")
                        .with_usage(Usage::new(agentkit_core::TokenUsage::new(100, 0))),
                ]
            },
        )
        .unwrap();
        if reconnect {
            content_occurrences(&opened.transcript, &first_content, 1);
            content_occurrences(&opened.transcript, &second_content, usize::from(identical));
        }
        let work = Arc::new(Mutex::new(InjectionWork::default()));
        let (mut client, connection) =
            InjectionWire::connect(integration.clone(), work.clone()).await;
        let fault = Arc::new(OnceLock::new());
        let cancellation = CancellationController::new();
        let failures = Arc::new(AtomicU64::new(0));
        let fail_after_observation = Arc::new(Notify::new());
        let sink = ResponseReplacementSink::new(PartialDeliverySink {
            inner: ConnectionSink(connection, work.clone()),
            fault: fault.clone(),
            cancellation: cancellation.clone(),
            failures: failures.clone(),
            fail_after_observation: fail_after_observation.clone(),
        });
        let activity = native_activity(session_id.clone(), sink.clone());
        let handle = integration
            .bind_session(
                AcpSessionBinding::new(session_id.clone(), loop_id.clone(), sink.clone())
                    .cancellation(cancellation.clone()),
            )
            .unwrap();
        handle.prepare_injection_turn();
        handle.start_injection_turn();
        let generation = handle.cancellation_handle().generation();
        let first_id = if reconnect {
            None
        } else {
            Some(inject_content(&mut client, &session_id, 2, first_content.clone()).await)
        };
        let second_id = inject_content(&mut client, &session_id, 3, second_content.clone()).await;
        assert_eq!(
            work.lock().unwrap().pending.len(),
            if reconnect { 1 } else { 2 }
        );
        if !reconnect {
            assert!(fault.set(second_id.clone()).is_ok());
        }

        let selection = SelectableAdapter::new_with_credentials(
            ProviderKind::OpenAiSubscription,
            "gpt-5.4",
            crate::credentials::CredentialStorage::Memory,
        )
        .unwrap();
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
        let manager = AsyncTaskManager::new();
        let tasks = manager.handle();
        let settlement = crate::runtime::InputSettlement::default();
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
            .cancellation(handle.cancellation_handle())
            .build()
            .unwrap()
            .start(SessionConfig::new(loop_id).without_cache())
            .await
            .unwrap();
        let busy = Arc::new(AtomicBool::new(false));
        let (commands, receiver) = mpsc::channel(8);
        let mcp = crate::tools::mcp::empty();
        let actor = tokio::spawn(session_actor(SessionActor {
            initial_generation: None,
            admission_released: Arc::new(Notify::new()),
            session_id: session_id.clone(),
            runtime,
            integration: integration.clone(),
            handle: handle.clone(),
            busy: busy.clone(),
            binding: BindingGuard {
                integration: integration.clone(),
                session_id: session_id.clone(),
            },
            sink,
            activity,
            driver: settlement.wrap(driver),
            tasks,
            background_jobs: BackgroundJobs::default(),
            structured_completion: false,
            skill_catalog: skill_catalog::SkillCatalogMonitor::new(&[]).unwrap(),
            adapter: selection,
            catalog: vec![],
            commands: receiver,
            mcp_events: mcp.subscribe(durable_id.clone()),
        }));
        claim_prompt(&busy).unwrap();
        queue_prompt(&commands, &session_id, &handle, "explicit prompt")
            .await
            .send(())
            .unwrap();

        if !reconnect {
            // Observe A through the real Channel while B's transport send is
            // suspended. Both acceptance receipts were activated before run.
            loop {
                let update = client.update().await;
                if matches!(&update.update, wire::SessionUpdate::UserMessage(message)
                    if Some(&message.message_id) == first_id.as_ref())
                {
                    break;
                }
            }
            let calls_before_cancel = turns.load(Ordering::Relaxed);
            let requests_before_cancel = requests.lock().unwrap().len();
            let summaries_before_cancel = summaries.lock().unwrap().len();
            assert_eq!(summaries_before_cancel, 0);
            fail_after_observation.notify_one();

            // An Idle(Cancelled) notification is not proof that this attachment
            // can be reused. The command receiver and BindingGuard must retire.
            timeout(Duration::from_secs(5), actor)
                .await
                .expect("partial transport failure must retire, not idle or retry")
                .unwrap();
            assert!(commands.is_closed());
            let (reply, _) = oneshot::channel();
            assert!(commands.send(Command::Snapshot { reply }).await.is_err());
            assert!(matches!(
                integration.flush_session_updates(&session_id).await,
                Err(AcpRuntimeError::SessionNotFound(_))
            ));
            assert!(handle.cancellation_handle().is_cancelled_since(generation));
            assert_eq!(failures.load(Ordering::Relaxed), 1, "B must not auto-retry");
            assert_eq!(turns.load(Ordering::Relaxed), calls_before_cancel);
            assert_eq!(requests.lock().unwrap().len(), requests_before_cancel);
            assert!(
                summaries.lock().unwrap().len() == summaries_before_cancel,
                "/compact must not execute"
            );
            let pending = work.lock().unwrap().pending.clone();
            assert!(!pending.contains(first_id.as_ref().unwrap()));
            assert_eq!(pending.len(), 1);
            assert!(
                pending.contains(&second_id),
                "failed B must not be marked delivered"
            );
            let persisted = crate::session::load(root.path(), &durable_id).unwrap();
            content_occurrences(&persisted, &first_content, 1);
            content_occurrences(&persisted, &second_content, usize::from(identical));
            let disk = std::fs::read_to_string(crate::session::transcript_path_for_test(
                root.path(),
                &durable_id,
            ))
            .unwrap();
            assert_eq!(disk.matches(first_text).count(), 1);
            if !identical {
                assert!(
                    !disk.contains(second_text),
                    "B must never be appended, even transiently"
                );
            }
            if multi_item {
                assert!(!disk.contains("unacknowledged-context-B"));
            }
            // Drain everything already emitted after joining the actor, without
            // waiting for Idle (retirement is required even if Idle cannot send).
            let mut delivered = vec![first_id.clone().unwrap()];
            while let Some(frame) = client.client.rx.next().now_or_never().flatten() {
                let agent_client_protocol::TransportFrame::Single(message) = frame else {
                    panic!("unexpected batch frame");
                };
                let value = serde_json::to_value(message).unwrap();
                if value["method"] == "session/update" {
                    let update: wire::UpdateSessionNotification =
                        serde_json::from_value(value["params"].clone()).unwrap();
                    if let wire::SessionUpdate::UserMessage(message) = update.update
                        && (Some(&message.message_id) == first_id.as_ref()
                            || message.message_id == second_id)
                    {
                        delivered.push(message.message_id);
                    }
                }
            }
            assert_eq!(delivered, vec![first_id.unwrap()]);
        } else {
            let mut delivered = Vec::new();
            loop {
                let update = client.update().await;
                match update.update {
                    wire::SessionUpdate::UserMessage(message)
                        if message.message_id == second_id =>
                    {
                        delivered.push(message.message_id);
                    }
                    wire::SessionUpdate::StateUpdate(wire::StateUpdate::Idle(idle)) => {
                        assert_eq!(idle.stop_reason, Some(wire::StopReason::EndTurn));
                        break;
                    }
                    _ => {}
                }
            }
            assert_eq!(delivered, vec![second_id]);
            assert!(work.lock().unwrap().pending.is_empty());
            assert_eq!(failures.load(Ordering::Relaxed), 0);
            let snapshot = snapshot_actor(&commands).await;
            content_occurrences(
                &snapshot.canonical_transcript,
                &first_content,
                1 + usize::from(identical),
            );
            content_occurrences(
                &snapshot.canonical_transcript,
                &second_content,
                1 + usize::from(identical),
            );
            assert_eq!(
                crate::session::load(root.path(), &durable_id).unwrap(),
                snapshot.canonical_transcript,
            );
            let (reply, response) = oneshot::channel();
            commands.send(Command::Close { reply }).await.unwrap();
            response.await.unwrap();
            actor.await.unwrap();
        }
    }
}

#[tokio::test]
async fn partial_transport_error_and_cancel_retire_actor() {
    timeout(
        Duration::from_secs(20),
        partial_delivery_retires_actor(false, false),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn partial_transport_error_preserves_all_items_of_acknowledged_steer() {
    timeout(
        Duration::from_secs(20),
        partial_delivery_retires_actor(true, false),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn partial_transport_error_distinguishes_identical_steers_by_position() {
    timeout(
        Duration::from_secs(20),
        partial_delivery_retires_actor(false, true),
    )
    .await
    .unwrap();
}
