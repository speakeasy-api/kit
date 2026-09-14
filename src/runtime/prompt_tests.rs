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
