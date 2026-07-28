//! Mid-turn user interjection via [`LoopInterrupt::AfterToolResult`].
//!
//! Between tool rounds the loop yields a cooperative
//! [`AfterToolResult`] interrupt. A host that wants to inject a user
//! message *before* the next model turn calls
//! [`ToolRoundInfo::submit`]. The snapshot pins that the injected
//! `User` item lands in the transcript ahead of the next model turn —
//! i.e. that the model's turn-2 input includes the interjected user
//! message.

use agentkit_core::{
    FinishReason, Item, ItemKind, MetadataMap, Part, TextPart, ToolCallId, ToolCallPart, ToolOutput,
};
use agentkit_integration_tests::mock_tool::StaticTool;
use agentkit_integration_tests::snapshot::{
    SessionRecording, SnapshotAdapter, TurnRecord, assert_recording, snapshot_path,
};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, ModelTurnEvent, ModelTurnResult, SessionConfig,
};
use agentkit_tools_core::ToolRegistry;
use serde_json::json;

#[tokio::test]
async fn after_tool_result_submit_interjects_user_message_into_next_turn() {
    let path = snapshot_path("after_tool_result_submit.ron");
    let recording = SessionRecording::load_or_seed(&path, || SessionRecording {
        session_id: "after-tool-submit".into(),
        initial_items: vec![Item::text(ItemKind::User, "kick off")],
        turns: vec![
            TurnRecord {
                input: Vec::new(),
                tools: Vec::new(),
                events: vec![
                    ModelTurnEvent::ToolCall(ToolCallPart::new(
                        ToolCallId::new("call-1"),
                        "noop",
                        json!({}),
                    )),
                    ModelTurnEvent::Finished(ModelTurnResult {
                        model: None,
                        response_id: None,
                        finish_reason: FinishReason::ToolCall,
                        output_items: vec![Item::new(
                            ItemKind::Assistant,
                            vec![Part::ToolCall(ToolCallPart::new(
                                ToolCallId::new("call-1"),
                                "noop",
                                json!({}),
                            ))],
                        )],
                        usage: None,
                        metadata: MetadataMap::new(),
                    }),
                ],
            },
            // Turn 2 input must include the interjected user item.
            TurnRecord {
                input: Vec::new(),
                tools: Vec::new(),
                events: vec![ModelTurnEvent::Finished(ModelTurnResult {
                    model: None,
                    response_id: None,
                    finish_reason: FinishReason::Completed,
                    output_items: vec![Item::new(
                        ItemKind::Assistant,
                        vec![Part::Text(TextPart::new("acknowledged interjection"))],
                    )],
                    usage: None,
                    metadata: MetadataMap::new(),
                })],
            },
        ],
        final_transcript: Vec::new(),
    });
    let adapter = SnapshotAdapter::from_recording(&recording);

    let tool = StaticTool::new("noop", "Returns ok.", ToolOutput::text("ok"));

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(tool))
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    let mut interjected = false;
    loop {
        match driver.next().await.unwrap() {
            LoopStep::Finished(_) => break,
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(round)) => {
                if !interjected {
                    round
                        .submit(
                            &mut driver,
                            vec![Item::text(ItemKind::User, "actually wait, also do this")],
                        )
                        .unwrap();
                    interjected = true;
                }
            }
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                panic!("unexpected AwaitingInput before Finished")
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(_)) => {
                panic!("unexpected approval interrupt")
            }
        }
    }

    assert!(interjected, "AfterToolResult interrupt must fire");

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}
