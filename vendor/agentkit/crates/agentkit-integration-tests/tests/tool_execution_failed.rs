//! A tool whose [`Tool::invoke`] returns `Err(ToolError::ExecutionFailed)`
//! lands in the transcript as a synthetic `ToolResult` with `is_error:
//! true`. The model gets a follow-up turn to react to the failure. The
//! snapshot pins the synthetic-error shape and the assistant's response.

use agentkit_core::{
    FinishReason, Item, ItemKind, MetadataMap, Part, TextPart, ToolCallId, ToolCallPart,
};
use agentkit_integration_tests::mock_tool::RecordingTool;
use agentkit_integration_tests::snapshot::{
    SessionRecording, SnapshotAdapter, TurnRecord, assert_recording, snapshot_path,
};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, ModelTurnEvent, ModelTurnResult, SessionConfig,
};
use agentkit_tools_core::{ToolError, ToolName, ToolRegistry, ToolSpec};
use serde_json::json;

#[tokio::test]
async fn tool_execution_failure_lands_as_synthetic_error_result() {
    let path = snapshot_path("tool_execution_failed.ron");
    let recording = SessionRecording::load_or_seed(&path, || SessionRecording {
        session_id: "tool-fail".into(),
        initial_items: vec![Item::text(ItemKind::User, "do the failing thing")],
        turns: vec![
            // Turn 1: model emits the tool call.
            TurnRecord {
                input: Vec::new(),
                tools: Vec::new(),
                events: vec![
                    ModelTurnEvent::ToolCall(ToolCallPart::new(
                        ToolCallId::new("call-fail"),
                        "boom",
                        json!({}),
                    )),
                    ModelTurnEvent::Finished(ModelTurnResult {
                        model: None,
                        response_id: None,
                        finish_reason: FinishReason::ToolCall,
                        output_items: vec![Item::new(
                            ItemKind::Assistant,
                            vec![Part::ToolCall(agentkit_core::ToolCallPart::new(
                                agentkit_core::ToolCallId::new("call-fail"),
                                "boom",
                                json!({}),
                            ))],
                        )],
                        usage: None,
                        metadata: MetadataMap::new(),
                    }),
                ],
            },
            // Turn 2: model reacts to the synthetic error.
            TurnRecord {
                input: Vec::new(),
                tools: Vec::new(),
                events: vec![ModelTurnEvent::Finished(ModelTurnResult {
                    model: None,
                    response_id: None,
                    finish_reason: FinishReason::Completed,
                    output_items: vec![Item::new(
                        ItemKind::Assistant,
                        vec![Part::Text(TextPart::new("tool blew up, sorry"))],
                    )],
                    usage: None,
                    metadata: MetadataMap::new(),
                })],
            },
        ],
        final_transcript: Vec::new(),
    });
    let adapter = SnapshotAdapter::from_recording(&recording);

    let failing = RecordingTool::new(
        ToolSpec::new(
            ToolName::new("boom"),
            "Always fails with ExecutionFailed.",
            json!({ "type": "object", "additionalProperties": true }),
        ),
        |_req| Err(ToolError::ExecutionFailed("kaboom".into())),
    );

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(failing.clone()))
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    loop {
        match driver.next().await.unwrap() {
            LoopStep::Finished(_) => break,
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                panic!("unexpected AwaitingInput before Finished")
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(_)) => {
                panic!("unexpected approval interrupt")
            }
        }
    }

    assert_eq!(failing.call_count(), 1);

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}
