//! Server-pushed `notifications/progress` flows through the rmcp client
//! into [`McpConnection::subscribe_events`] subscribers as
//! [`McpServerEvent::Progress`]. Asserted while a federated agent is
//! actively driving the connection — i.e. the broadcast pipe stays
//! healthy through the agentkit ↔ rmcp data path that tools share.

use std::sync::Arc;
use std::time::Duration;

use agentkit_core::{
    FinishReason, Item, ItemKind, MetadataMap, Part, TextPart, ToolCallId, ToolCallPart,
};
use agentkit_integration_tests::mcp_server::spawn_in_memory;
use agentkit_integration_tests::snapshot::{
    SessionRecording, SnapshotAdapter, TurnRecord, assert_recording, snapshot_path,
};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, ModelTurnEvent, ModelTurnResult, SessionConfig,
};
use agentkit_mcp::{
    McpProgressNotificationParam, McpServerEvent, McpToolAdapter, McpToolNamespace,
};
use agentkit_tools_core::{ToolSource, dynamic_catalog};
use rmcp::model::{NumberOrString, ProgressToken};
use serde_json::json;
use tokio::time::timeout;

#[tokio::test]
async fn server_progress_notification_reaches_subscriber_during_agent_run() {
    let path = snapshot_path("mcp_progress.ron");
    let recording = SessionRecording::load_or_seed(&path, || {
        let multiply = ToolCallPart::new(
            ToolCallId::new("call-mul"),
            "multiply",
            json!({ "a": 6, "b": 7 }),
        );
        SessionRecording {
            session_id: "mcp-progress".into(),
            initial_items: vec![Item::text(ItemKind::User, "multiply 6 and 7")],
            turns: vec![
                TurnRecord {
                    input: Vec::new(),
                    tools: Vec::new(),
                    events: vec![
                        ModelTurnEvent::ToolCall(multiply.clone()),
                        ModelTurnEvent::Finished(ModelTurnResult {
                            model: None,
                            response_id: None,
                            finish_reason: FinishReason::ToolCall,
                            output_items: vec![Item::new(
                                ItemKind::Assistant,
                                vec![Part::ToolCall(multiply)],
                            )],
                            usage: None,
                            metadata: MetadataMap::new(),
                        }),
                    ],
                },
                TurnRecord {
                    input: Vec::new(),
                    tools: Vec::new(),
                    events: vec![ModelTurnEvent::Finished(ModelTurnResult {
                        model: None,
                        response_id: None,
                        finish_reason: FinishReason::Completed,
                        output_items: vec![Item::new(
                            ItemKind::Assistant,
                            vec![Part::Text(TextPart::new("the answer is 42"))],
                        )],
                        usage: None,
                        metadata: MetadataMap::new(),
                    })],
                },
            ],
            final_transcript: Vec::new(),
        }
    });
    let adapter = SnapshotAdapter::from_recording(&recording);

    let server = spawn_in_memory("demo").await;
    let connection = Arc::new(server.connection);
    let mut events = connection.subscribe_events();

    let snapshot = connection.discover().await.expect("discover succeeds");
    let (writer, reader) = dynamic_catalog("mcp:demo");
    for tool in snapshot.tools.iter().cloned() {
        let mcp_adapter = McpToolAdapter::with_namespace(
            connection.server_id(),
            connection.clone(),
            tool,
            &McpToolNamespace::Default,
        );
        writer.upsert(Arc::new(mcp_adapter));
    }
    let _ = reader.drain_catalog_events();

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(reader)
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    // Fire the progress notification from the server side BEFORE driving
    // the loop, so a buggy "events bound only after first call" wiring
    // would still surface as a missing event below.
    server
        .peer
        .notify_progress(McpProgressNotificationParam {
            progress_token: ProgressToken(NumberOrString::String("agent-tok".into())),
            progress: 0.42,
            total: Some(1.0),
            message: Some("midway".into()),
        })
        .await
        .expect("server notify_progress succeeds");

    loop {
        match driver.next().await.unwrap() {
            LoopStep::Finished(_) => break,
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                panic!("model script ran out before reaching Finished")
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                panic!("unexpected approval interrupt: {}", pending.request.summary)
            }
        }
    }

    let event = timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("progress event arrives in time")
        .expect("event channel still open");
    match event {
        McpServerEvent::Progress(progress) => {
            assert_eq!(progress.progress, 0.42);
            assert_eq!(progress.message.as_deref(), Some("midway"));
            assert_eq!(progress.total, Some(1.0));
        }
        other => panic!("expected McpServerEvent::Progress, got {other:?}"),
    }

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}
