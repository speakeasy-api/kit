//! A real idle session, real tools and a TCP MCP peer; only the model is scripted.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
use super::*;
use crate::tools::mcp::refresh_tests::{Peer, runtime as mcp_runtime};
use agentkit_core::{Delta, PartId, PartKind, ToolCallPart, TurnCancellation};
use agentkit_loop::{
    Agent, ModelAdapter, ModelTurn, ModelTurnEvent, ModelTurnResult, SessionConfig, TurnRequest,
};
use agentkit_task_manager::{AsyncTaskManager, TaskManager};
use agentkit_tools_core::ToolRegistry;
use async_trait::async_trait;
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicUsize, Ordering},
};

struct ResourceModel {
    step: usize,
    seen: Arc<AtomicUsize>,
}
struct ResourceTurn(VecDeque<ModelTurnEvent>);
#[async_trait]
impl ModelAdapter for ResourceModel {
    type Session = Self;
    async fn start_session(&self, _: SessionConfig) -> Result<Self, LoopError> {
        Ok(Self {
            step: 0,
            seen: self.seen.clone(),
        })
    }
}
#[async_trait]
impl ModelSession for ResourceModel {
    type Turn = ResourceTurn;
    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        _: Option<TurnCancellation>,
    ) -> Result<ResourceTurn, LoopError> {
        let step = self.step;
        self.step += 1;
        let transcript = serde_json::to_string(&request.transcript).unwrap();
        let operation = match step {
            0 => Some(
                json!({"operation":"subscribe","server":"messages","notifications":{"resourceSubscriptions":["test://messages"]}}),
            ),
            2 => {
                assert!(
                    request
                        .transcript
                        .iter()
                        .any(|i| i.kind == ItemKind::Notification)
                );
                Some(
                    json!({"operation":"read_resource","server":"messages","uri":"test://messages"}),
                )
            }
            4 => Some(json!({"operation":"unsubscribe","server":"messages"})),
            _ => None,
        };
        if step == 5 {
            assert!(transcript.contains("\"watching\":false"), "{transcript}");
        }
        if step == 1 {
            assert!(transcript.contains("acknowledged"));
        }
        if step == 3 {
            assert!(transcript.contains("Hello from the subscribed resource"));
            self.seen.fetch_add(1, Ordering::SeqCst);
        }
        let events = if let Some(input) = operation {
            let call = ToolCallPart::new(format!("resource-{step}"), "mcp_subscription", input);
            VecDeque::from([
                ModelTurnEvent::ToolCall(call.clone()),
                ModelTurnEvent::Finished(ModelTurnResult {
                    model: None,
                    response_id: None,
                    finish_reason: FinishReason::ToolCall,
                    output_items: vec![Item::new(ItemKind::Assistant, vec![Part::ToolCall(call)])],
                    usage: None,
                    metadata: MetadataMap::new(),
                }),
            ])
        } else {
            let text = if step == 1 {
                "watching"
            } else if step == 3 {
                "read new resource"
            } else {
                "unsubscribed"
            };
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
        };
        Ok(ResourceTurn(events))
    }
}
#[async_trait]
impl ModelTurn for ResourceTurn {
    async fn next_event(
        &mut self,
        _: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        Ok(self.0.pop_front())
    }
}

#[tokio::test]
async fn resource_subscription_wakes_idle_session_reads_and_unsubscribes() {
    let root = tempfile::tempdir().unwrap();
    let peer = Peer::with_protocol(vec![], true).await;
    let mcp = mcp_runtime(&root.path().join("mcp.json"), &[("messages", &peer)]).await;
    let session_id = agentkit_acp::SessionId::new("resource-wake");
    let loop_id = AgentkitSessionId::new("resource-wake");
    let integration = Arc::new(
        AcpIntegration::builder()
            .name("resource-wake")
            .approval_resolver(AutoDenyResolver)
            .build()
            .unwrap(),
    );
    let (client, mut messages) = AcpClientHandle::channel();
    integration
        .bind_session(AcpSessionBinding::new(
            session_id.clone(),
            loop_id.clone(),
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
    let (transitions, mut states) = mpsc::unbounded_channel();
    let activity = activity::SessionActivity::new(move |transition| {
        transitions.send(transition.active).unwrap();
        Ok(())
    });
    let seen = Arc::new(AtomicUsize::new(0));
    let manager = AsyncTaskManager::new();
    let tasks = manager.handle();
    let tools = ToolRegistry::new().with(crate::tools::McpSubscriptionTool::new(mcp.clone()));
    let driver = Agent::builder()
        .model(ResourceModel {
            step: 0,
            seen: seen.clone(),
        })
        .add_tool_source(tools)
        .task_manager(manager)
        .observer(integration.as_ref().clone())
        .observer(activity.clone())
        .build()
        .unwrap()
        .start(SessionConfig::new(loop_id).without_cache())
        .await
        .unwrap();
    let (commands, received) = mpsc::channel(8);
    let mcp_events = mcp.subscribe(session_id.to_string()).unwrap();
    let runtime = Runtime::new(root.path(), "gpt-5.4").unwrap();
    let skills = runtime.current_skills().await.unwrap();
    let actor = tokio::spawn(session_actor(SessionActor {
        subagents: None,
        voice_monitor: crate::runtime::voice_state::VoiceState::default().monitor(&[]),
        session_id: session_id.clone(),
        runtime,
        integration: integration.clone(),
        binding: SessionBindingGuard::new(integration.clone(), session_id.clone()),
        driver,
        tasks,
        background_jobs: BackgroundJobs::default(),
        structured_completion: false,
        skill_catalog: skill_catalog::SkillCatalogMonitor::new(&skills.skills).unwrap(),
        adapter: SelectableAdapter::new(crate::ProviderKind::OpenAiSubscription, "gpt-5.4")
            .unwrap(),
        catalog: Vec::new(),
        commands: received,
        activity,
        mcp_events,
    }));
    let prompt = async |text: &str| {
        let (reply, response) = oneshot::channel();
        commands
            .send(Command::Prompt {
                request: PromptRequest::new(
                    session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new(text))],
                ),
                reply,
            })
            .await
            .unwrap();
        timeout(Duration::from_secs(10), response)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    };
    prompt("Watch the messages resource").await;
    assert_eq!(seen.load(Ordering::SeqCst), 0);
    while timeout(Duration::from_secs(10), states.recv())
        .await
        .unwrap()
        .unwrap()
    {}
    // No second user prompt: the actual actor is idle when the TCP server emits.
    peer.resource_updated();
    assert!(
        timeout(Duration::from_secs(10), states.recv())
            .await
            .unwrap()
            .unwrap()
    );
    assert!(
        !timeout(Duration::from_secs(10), states.recv())
            .await
            .unwrap()
            .unwrap()
    );
    assert_eq!(seen.load(Ordering::SeqCst), 1);
    prompt("Stop watching").await;
    while timeout(Duration::from_secs(10), states.recv())
        .await
        .unwrap()
        .unwrap()
    {}
    peer.resource_updated();
    let unexpected = timeout(Duration::from_millis(100), states.recv()).await;
    assert!(
        unexpected.is_err(),
        "unsubscribed session transition: {unexpected:?}"
    );
    let (reply, response) = oneshot::channel();
    commands.send(Command::Close { reply }).await.unwrap();
    timeout(Duration::from_secs(10), response)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    actor.await.unwrap();
    drain.abort();
}
