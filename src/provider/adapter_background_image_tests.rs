#![allow(clippy::disallowed_methods, clippy::disallowed_macros)]
//! Regression through the real loop/task manager, not a fabricated notification.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::{collections::VecDeque, time::Duration};

use agentkit_core::{
    DataRef, FinishReason, Item, ItemKind, MetadataMap, Modality, Part, ToolCallPart, ToolOutput,
    ToolResultPart, TurnCancellation,
};
use agentkit_http::Authentication;
use agentkit_loop::{
    Agent, LoopError, LoopInterrupt, LoopStep, ModelAdapter, ModelSession, ModelTurn,
    ModelTurnEvent, ModelTurnResult, SessionConfig, TurnRequest,
};
use agentkit_provider_openai::OpenAIResponsesConfig;
use agentkit_task_manager::{AsyncTaskManager, RoutingDecision, TaskEvent, TaskManager};
use agentkit_tools_core::{
    AllowAllPermissions, Tool, ToolAnnotations, ToolContext, ToolError, ToolName, ToolRegistry,
    ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::{mpsc, watch};

struct Model(mpsc::UnboundedSender<TurnRequest>);
struct Session(mpsc::UnboundedSender<TurnRequest>);
struct Turn(VecDeque<ModelTurnEvent>);

#[async_trait]
impl ModelAdapter for Model {
    type Session = Session;

    async fn start_session(&self, _: SessionConfig) -> Result<Session, LoopError> {
        Ok(Session(self.0.clone()))
    }
}

#[async_trait]
impl ModelSession for Session {
    type Turn = Turn;

    async fn begin_turn(
        &mut self,
        request: TurnRequest,
        _: Option<TurnCancellation>,
    ) -> Result<Turn, LoopError> {
        let answered = request
            .transcript
            .iter()
            .any(|item| item.kind == ItemKind::Tool);
        self.0.send(request).unwrap();
        let mut events = VecDeque::new();
        let (finish_reason, output) = if answered {
            (
                FinishReason::Completed,
                Item::text(ItemKind::Assistant, "done"),
            )
        } else {
            let call = ToolCallPart::new("background-image", "image", json!({}));
            events.push_back(ModelTurnEvent::ToolCall(call.clone()));
            (
                FinishReason::ToolCall,
                Item::new(ItemKind::Assistant, vec![Part::ToolCall(call)]),
            )
        };
        events.push_back(ModelTurnEvent::Finished(ModelTurnResult {
            model: None,
            response_id: None,
            finish_reason,
            output_items: vec![output],
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

struct ImageTool {
    spec: ToolSpec,
    release: watch::Receiver<bool>,
}

#[async_trait]
impl Tool for ImageTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        // The external operation cannot finish until the test has observed the
        // real loop's detach placeholder. No sleeps or scheduler races.
        self.release
            .clone()
            .wait_for(|released| *released)
            .await
            .unwrap();
        Ok(ToolResult {
            result: ToolResultPart::success(
                request.call_id,
                ToolOutput::Parts(vec![
                    Part::text("Selected background screenshot: $.image"),
                    Part::media(
                        Modality::Image,
                        "image/png",
                        DataRef::InlineBytes(vec![1, 2, 3]),
                    ),
                ]),
            )
            .with_metadata(MetadataMap::from_iter([(
                "diagnostic".into(),
                json!("kept"),
            )])),
            duration: None,
            metadata: MetadataMap::new(),
        })
    }
}

#[tokio::test]
async fn real_detached_completion_projects_notification_images_for_responses() {
    // Timeout is a deadlock guard, not an assertion about performance.
    tokio::time::timeout(Duration::from_secs(5), async {
        let (requests, mut received) = mpsc::unbounded_channel();
        let (release, gate) = watch::channel(false);
        let manager = AsyncTaskManager::new().routing(|_: &ToolRequest| {
            RoutingDecision::ForegroundThenDetachAfter(Duration::ZERO)
        });
        let handle = manager.handle();
        let agent = Agent::builder()
            .model(Model(requests))
            .add_tool_source(ToolRegistry::new().with(ImageTool {
                spec: ToolSpec {
                    name: ToolName::new("image"),
                    description: "Return a screenshot after release".into(),
                    input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
                    output_schema: None,
                    annotations: ToolAnnotations::default(),
                    metadata: MetadataMap::new(),
                },
                release: gate,
            }))
            .permissions(AllowAllPermissions)
            .task_manager(manager)
            .build().unwrap();
        let mut driver = agent.start(SessionConfig::new("real-background-image")).await.unwrap();
        driver.submit_input(vec![Item::text(ItemKind::User, "Get the screenshot")]).unwrap();
        assert!(matches!(driver.next().await.unwrap(), LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_))));
        let detached = driver.snapshot().transcript;
        assert!(detached.iter().flat_map(|item| &item.parts).any(|part| {
            matches!(part, Part::ToolResult(result) if matches!(&result.output, ToolOutput::Text(text) if text.contains("running in the background")))
        }));
        release.send(true).unwrap();
        // Task events and the ready-item queue are independent public APIs.
        // Await completion without draining the result the loop must consume.
        loop {
            if matches!(handle.next_event().await.unwrap(), TaskEvent::Completed(_, _)) { break; }
        }
        loop {
            match driver.next().await.unwrap() {
                LoopStep::Finished(_) => break,
                LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {},
                other => panic!("unexpected loop step: {other:?}"),
            }
        }
        let request = std::iter::from_fn(|| received.try_recv().ok())
            .find(|request| request.transcript.iter().any(|item| item.kind == ItemKind::Notification))
            .expect("real loop must send its detached completion notification to the model");
        let original = serde_json::to_value(&request.transcript).unwrap();
        assert!(original.to_string().contains("InlineBytes"));
        assert_eq!(request.transcript.iter().filter(|item| item.kind == ItemKind::Tool).count(), 1);
        for native in [false, true] {
            let projected = super::project_tool_output_images(request.clone(), native).unwrap();
            let attachment = projected.transcript.iter().find(|item| item.metadata.get("kit.projected_tool_images") == Some(&json!(true))).unwrap();
            assert_eq!(attachment.kind, ItemKind::User);
            let wire = OpenAIResponsesConfig::chatgpt_private("gpt-5.4", Authentication::bearer("test"))
                .encode_request(&projected).unwrap();
            let text = wire.to_string();
            assert_eq!(text.matches("data:image/png;base64,AQID").count(), 1);
            assert!(!text.contains("InlineBytes"));
            assert!(text.contains("Selected background screenshot"));
            assert!(text.contains("diagnostic"));
            assert_eq!(wire["input"].as_array().unwrap().iter().filter(|item| item["type"] == "function_call_output").count(), 1);
            assert_eq!(serde_json::to_value(&request.transcript).unwrap(), original);
        }
    }).await.expect("detached completion test deadlocked");
}
