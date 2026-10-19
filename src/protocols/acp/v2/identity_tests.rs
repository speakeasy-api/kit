struct Model(bool, bool);
struct Session(bool, bool);
struct Turn(VecDeque<ModelTurnEvent>, bool);
#[async_trait]
impl ModelAdapter for Model {
    type Session = Session;
    async fn start_session(&self, _: SessionConfig) -> Result<Session, LoopError> {
        Ok(Session(self.0, self.1))
    }
}
#[async_trait]
impl ModelSession for Session {
    type Turn = Turn;
    async fn begin_turn(
        &mut self,
        _: TurnRequest,
        _: Option<TurnCancellation>,
    ) -> Result<Turn, LoopError> {
        let mut events = VecDeque::new();
        let mut parts = Vec::new();
        for attempt in 0..=usize::from(self.0) {
            if attempt > 0 {
                events.push_back(ModelTurnEvent::ResponseAttemptSuperseded);
                parts.clear();
            }
            for (index, (kind, text)) in [
                (PartKind::Reasoning, "first thought"),
                (PartKind::Reasoning, "second thought"),
                (PartKind::Text, "answer"),
            ]
            .into_iter()
            .enumerate()
            {
                let part_id = PartId::new(format!("part-{index}"));
                let part = if kind == PartKind::Text {
                    Part::text(text)
                } else {
                    Part::Reasoning(agentkit_core::ReasoningPart::summary(text))
                };
                events.push_back(ModelTurnEvent::Delta(Delta::BeginPart {
                    part_id: part_id.clone(),
                    kind,
                }));
                events.push_back(ModelTurnEvent::Delta(Delta::AppendText {
                    part_id,
                    chunk: text.into(),
                }));
                events.push_back(ModelTurnEvent::Delta(Delta::CommitPart {
                    part: part.clone(),
                }));
                parts.push(part);
            }
        }
        events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![Item::new(ItemKind::Assistant, parts)],
            usage: None,
            metadata: MetadataMap::new(),
            model: None,
            response_id: None,
        }));
        if self.1 {
            events.pop_back();
        }
        Ok(Turn(events, self.1))
    }
}
#[async_trait]
impl ModelTurn for Turn {
    async fn next_event(
        &mut self,
        _: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        match self.0.pop_front() {
            None if self.1 => Err(LoopError::Cancelled),
            event => Ok(event),
        }
    }
}

#[tokio::test]
async fn cold_restore_preserves_live_user_agent_and_separate_thought_ids() {
    for (replacement, cancelled) in [(false, false), (true, false), (false, true), (true, true)] {
        let root = tempfile::tempdir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let session_id = wire::SessionId::new("persisted-identity");
        let loop_id = SessionId::new("persisted-identity");
        let recording = RecordingSink::default();
        {
            let opened = crate::session::open_in(
                root.path(),
                directory.path(),
                "persisted-identity",
                false,
                false,
                vec![],
            )
            .unwrap();
            let sink = ResponseReplacementSink::new(recording.clone());
            let integration = AcpIntegration::default();
            let _handle = integration
                .bind_session(AcpSessionBinding::new(
                    session_id.clone(),
                    loop_id.clone(),
                    sink.clone(),
                ))
                .unwrap();
            let activity = native_activity(session_id.clone(), sink.clone());
            let observer = ResponseReplacementObserver::new(
                integration.clone(),
                sink.clone(),
                session_id.clone(),
                activity,
            );
            let mut driver = Agent::builder()
                .model(identity::Adapter {
                    inner: Model(replacement, cancelled),
                    enabled: true,
                })
                .observer(observer)
                .cancellation(_handle.cancellation_handle())
                .transcript_observer(opened.observer)
                .build()
                .unwrap()
                .start(SessionConfig::new(loop_id).with_response_attempt_supersession())
                .await
                .unwrap();
            let mut input = vec![Item::text(ItemKind::User, "question")];
            let user_id = identity::stamp_user(&mut input);
            driver.submit_input(input).unwrap();
            integration.begin_prompt(&session_id).unwrap();
            sink.update(wire::UpdateSessionNotification::new(
                session_id.clone(),
                wire::SessionUpdate::UserMessage(wire::UserMessage::new(user_id).content(vec![
                    wire::ContentBlock::Text(wire::TextContent::new("question")),
                ])),
            ))
            .unwrap();
            while !matches!(driver.next().await.unwrap(), LoopStep::Finished(_)) {}
        }
        // No driver, binding, adapter or writer survives the reopen.
        let restored = crate::session::open_in(
            root.path(),
            directory.path(),
            "persisted-identity",
            true,
            false,
            vec![],
        )
        .unwrap();
        let replay = transcript_replay(&session_id, &restored.transcript);
        let mut live_ids = Vec::new();
        for notification in recording.updates.lock().unwrap().iter() {
            let (id, clear) = match &notification.update {
                wire::SessionUpdate::UserMessage(m) => (&m.message_id, false),
                wire::SessionUpdate::AgentMessageChunk(m) => (&m.message_id, false),
                wire::SessionUpdate::AgentThoughtChunk(m) => (&m.message_id, false),
                wire::SessionUpdate::AgentMessage(m) => (&m.message_id, true),
                wire::SessionUpdate::AgentThought(m) => (&m.message_id, true),
                _ => continue,
            };
            if clear {
                live_ids.retain(|existing| existing != id);
            } else if !live_ids.contains(id) {
                live_ids.push(id.clone());
            }
        }
        let replay_ids = replay
            .iter()
            .filter_map(|notification| match &notification.update {
                wire::SessionUpdate::UserMessage(m) => Some(m.message_id.clone()),
                wire::SessionUpdate::AgentMessage(m) => Some(m.message_id.clone()),
                wire::SessionUpdate::AgentThought(m) => Some(m.message_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(live_ids.len(), 4);
        assert_eq!(
            replay_ids, live_ids,
            "replacement={replacement}, cancelled={cancelled}"
        );
    }
}

#[test]
fn legacy_replay_ignores_malformed_optional_identity_metadata() {
    let session_id = wire::SessionId::new("legacy");
    let legacy = vec![
        Item::text(ItemKind::User, "question"),
        Item::text(ItemKind::Assistant, "answer"),
    ];
    let mut malformed = legacy.clone();
    malformed[0]
        .metadata
        .insert(identity::KEY.into(), json!(42));
    if let Part::Text(text) = &mut malformed[1].parts[0] {
        text.metadata
            .insert(identity::KEY.into(), json!({"unexpected": true}));
    }
    assert_eq!(
        serde_json::to_value(transcript_replay(&session_id, &legacy)).unwrap(),
        serde_json::to_value(transcript_replay(&session_id, &malformed)).unwrap()
    );
}
