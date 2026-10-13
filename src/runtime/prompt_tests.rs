//! Deterministic model/tool boundary test for completion during a final turn.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
use super::*;
use agentkit_core::{Delta, PartId, PartKind, ToolCallPart, TurnCancellation};
use agentkit_loop::{
    ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, ModelTurnResult, TurnRequest,
};
use agentkit_tools_core::{ToolAnnotations, ToolRegistry};
use serde_json::json;
use tokio::sync::Notify;

#[derive(Clone)]
struct Model {
    tasks: TaskManagerHandle,
    release: Arc<Notify>,
    turn: usize,
}
struct Turn(VecDeque<ModelTurnEvent>);

#[async_trait]
impl ModelAdapter for Model {
    type Session = Self;
    async fn start_session(&self, _: SessionConfig) -> Result<Self, LoopError> {
        Ok(self.clone())
    }
}
#[async_trait]
impl ModelSession for Model {
    type Turn = Turn;
    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        _: Option<TurnCancellation>,
    ) -> Result<Turn, LoopError> {
        self.turn += 1;
        let (items, reason, call) = match self.turn {
            1 => {
                let call = ToolCallPart {
                    id: "gate-call".into(),
                    name: "gate".into(),
                    input: json!({}),
                    metadata: MetadataMap::new(),
                };
                let mut item = Item::text(ItemKind::Assistant, "");
                item.parts = vec![Part::ToolCall(call.clone())];
                (vec![item], FinishReason::ToolCall, Some(call))
            }
            2 => {
                // This model turn is now in progress. Finish the tool and wait
                // for the manager to publish its queued loop update BEFORE
                // returning the empty final response. No scheduling sleeps.
                self.release.notify_one();
                loop {
                    if matches!(
                        self.tasks.next_event().await,
                        Some(agentkit_task_manager::TaskEvent::Completed(_, _))
                    ) {
                        break;
                    }
                }
                assert!(self.tasks.list_running().await.is_empty());
                (
                    vec![Item::text(ItemKind::Assistant, "")],
                    FinishReason::Completed,
                    None,
                )
            }
            3 => {
                assert!(
                    serde_json::to_string(&request.transcript)
                        .unwrap()
                        .contains("delivered completion")
                );
                (
                    vec![Item::text(ItemKind::Assistant, "resumed final answer")],
                    FinishReason::Completed,
                    None,
                )
            }
            _ => panic!("unexpected extra model turn"),
        };
        let mut events = VecDeque::new();
        if let Some(call) = call {
            events.push_back(ModelTurnEvent::ToolCall(call));
        }
        if self.turn == 3 {
            events.push_back(ModelTurnEvent::Delta(Delta::BeginPart {
                part_id: PartId::new("answer"),
                kind: PartKind::Text,
            }));
            events.push_back(ModelTurnEvent::Delta(Delta::AppendText {
                part_id: PartId::new("answer"),
                chunk: "resumed final answer".into(),
            }));
        }
        events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
            model: None,
            response_id: None,
            finish_reason: reason,
            output_items: items,
            usage: None,
            metadata: MetadataMap::new(),
        }));
        Ok(Turn(events))
    }
}
#[async_trait]
impl ModelTurn for Turn {
    async fn next_event(
        &mut self,
        _: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        Ok(self.0.pop_front())
    }
}
struct Gate {
    spec: ToolSpec,
    release: Arc<Notify>,
}
#[async_trait]
impl Tool for Gate {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    async fn invoke(
        &self,
        request: ToolRequest,
        _: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        self.release.notified().await;
        Ok(ToolResult {
            result: ToolResultPart {
                call_id: request.call_id,
                output: ToolOutput::Text("delivered completion".into()),
                is_error: false,
                metadata: MetadataMap::new(),
            },
            duration: None,
            metadata: MetadataMap::new(),
        })
    }
}
#[tokio::test]
async fn last_completion_queued_during_final_turn_is_not_lost() {
    let manager = AsyncTaskManager::new()
        .routing(|_: &ToolRequest| RoutingDecision::ForegroundThenDetachAfter(Duration::ZERO));
    let tasks = manager.handle();
    let release = Arc::new(Notify::new());
    let tools = ToolRegistry::new().with(Gate {
        release: release.clone(),
        spec: ToolSpec {
            name: ToolName::new("gate"),
            description: "local gated tool".into(),
            input_schema: json!({"type":"object"}),
            output_schema: None,
            annotations: ToolAnnotations::default(),
            metadata: MetadataMap::new(),
        },
    });
    let mut driver = Agent::builder()
        .model(Model {
            tasks: tasks.clone(),
            release,
            turn: 0,
        })
        .add_tool_source(tools)
        .task_manager(manager)
        .input(vec![Item::text(ItemKind::User, "start")])
        .build()
        .unwrap()
        .start(SessionConfig::new("completion-race").without_cache())
        .await
        .unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(3),
        drive_with_tasks(&mut driver, Some(&tasks)),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(output, "resumed final answer");
    assert!(tasks.list_running().await.is_empty());
}

/// Cancels at the real hook transport boundary while model admission is pending.
struct CancelModelAdmission {
    events: Arc<crate::ahp::test_support::Backend>,
    cancellation: CancellationController,
}

impl agenthooksprotocol::adapters::registered::ManagedBackend for CancelModelAdmission {
    fn call(
        &self,
        request: serde_json::Value,
        timeout: Duration,
    ) -> agenthooksprotocol::client::LocalFuture<
        '_,
        Result<serde_json::Value, agenthooksprotocol::client::HookError>,
    > {
        Box::pin(async move {
            let cancels = request["params"]["event"]["type"] == "model.request.before";
            let response = self.events.call(request, timeout).await?;
            if cancels {
                self.cancellation.interrupt();
                // Keep drive_with_tasks suspended until the outer select wins.
                std::future::pending().await
            } else {
                Ok(response)
            }
        })
    }

    fn shutdown(
        &self,
    ) -> agenthooksprotocol::client::LocalFuture<
        '_,
        Result<(), agenthooksprotocol::client::HookError>,
    > {
        self.events.shutdown()
    }
}

#[tokio::test]
async fn persistent_cancellation_emits_cancelled_turn_before_session_end() {
    const ISOLATED: &str = "KIT_AHP_PERSISTENT_CANCEL_TEST";
    if std::env::var(ISOLATED).ok().as_deref() != Some("1") {
        let output = tokio::task::spawn_blocking(|| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "runtime::prompt_tests::persistent_cancellation_emits_cancelled_turn_before_session_end", "--nocapture"])
                .env(ISOLATED, "1")
                .env("OPENROUTER_API_KEY", "test-only-key")
                // This endpoint has no models-catalog URL and must never be called.
                .env("OPENROUTER_BASE_URL", "http://127.0.0.1:1/completions")
                .output().unwrap()
        }).await.unwrap();
        assert!(output.status.success(), "{output:?}");
        return;
    }

    use crate::ahp::test_support::{Backend, hooks_with_backend, intercept, observe};
    let root = tempfile::tempdir().unwrap();
    let cancellation = CancellationController::new();
    let events = Arc::new(Backend::default());
    let hooks = hooks_with_backend(
        vec![
            observe(&["turn.start", "turn.end", "session.end"]),
            intercept(&["model.request.before"]),
        ],
        Arc::new(CancelModelAdmission {
            events: events.clone(),
            cancellation: cancellation.clone(),
        }),
    );
    let mut runtime = Runtime::with_session_provider_and_credentials(
        root.path(),
        "test/model",
        ProviderKind::OpenRouter,
        SessionRequest {
            id: "persistent-cancellation".into(),
            resume: false,
            force: false,
        },
        crate::credentials::CredentialStorage::Memory,
    )
    .unwrap();
    Arc::get_mut(&mut runtime).unwrap().hooks = Some(hooks);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        runtime.run_persistent_interruptible("hello".into(), Some(cancellation.handle())),
    )
    .await
    .unwrap();
    let error = result.unwrap_err();
    assert!(error.contains("cancel"), "{error}");

    let delivered = events.all_events();
    let terminal: Vec<_> = delivered
        .iter()
        .filter(|event| {
            matches!(
                event["type"].as_str(),
                Some("turn.start" | "turn.end" | "session.end")
            )
        })
        .collect();
    assert_eq!(terminal.len(), 3, "{delivered:?}");
    assert_eq!(terminal[0]["type"], "turn.start");
    assert_eq!(terminal[1]["type"], "turn.end");
    assert_eq!(terminal[2]["type"], "session.end");
    assert_eq!(terminal[1]["outcome"], "cancelled");
    assert_eq!(terminal[2]["outcome"], "cancelled");
    assert_eq!(terminal[0]["turn"], terminal[1]["turn"]);
    assert_eq!(terminal[0]["session"], terminal[1]["session"]);
    assert_eq!(terminal[1]["session"], terminal[2]["session"]);
    assert_eq!(events.events("model.request.before").len(), 1);
}
