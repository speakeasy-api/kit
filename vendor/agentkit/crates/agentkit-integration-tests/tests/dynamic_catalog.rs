//! Mid-session tool catalog mutations propagate to the model.
//!
//! The host owns the writer side of a [`dynamic_catalog`] pair; tools are
//! added/removed between turns. The snapshot's per-turn `tools` list is
//! the LLM's view of the catalog — assertions about whether the model
//! "saw" the mutation reduce to a transcript snapshot diff.

use std::sync::Arc;

use agentkit_core::{Item, ItemKind, ToolOutput};
use agentkit_integration_tests::mock_tool::StaticTool;
use agentkit_integration_tests::snapshot::{
    SessionRecording, SnapshotAdapter, assert_recording, snapshot_path,
};
use agentkit_loop::{Agent, LoopInterrupt, LoopStep, SessionConfig};
use agentkit_tools_core::{ToolName, ToolSource, dynamic_catalog};

#[tokio::test]
async fn dynamic_registry_mutations_flow_to_next_turn() {
    let path = snapshot_path("dynamic_catalog.ron");
    let recording = SessionRecording::load(&path);
    let adapter = SnapshotAdapter::from_recording(&recording);

    let (writer, reader) = dynamic_catalog("dynamic");
    writer.upsert(Arc::new(StaticTool::new(
        "alpha",
        "Returns alpha-body.",
        ToolOutput::text("alpha-body"),
    )));
    // Drain bootstrap events so the first turn doesn't see a synthetic
    // "two tools just appeared" notification.
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

    // Turn 1 — sees [alpha].
    drive_until_finished(&mut driver).await;

    // Mutate before turn 2: add beta.
    writer.upsert(Arc::new(StaticTool::new(
        "beta",
        "Returns beta-body.",
        ToolOutput::text("beta-body"),
    )));

    let pending = await_input_request(&mut driver).await;
    pending
        .submit(&mut driver, vec![Item::text(ItemKind::User, "next")])
        .unwrap();
    // Turn 2 — sees [alpha, beta].
    drive_until_finished(&mut driver).await;

    // Mutate before turn 3: remove alpha.
    assert!(writer.remove(&ToolName::new("alpha")));

    let pending = await_input_request(&mut driver).await;
    pending
        .submit(&mut driver, vec![Item::text(ItemKind::User, "again")])
        .unwrap();
    // Turn 3 — sees [beta].
    drive_until_finished(&mut driver).await;

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}

async fn drive_until_finished<S>(driver: &mut agentkit_loop::LoopDriver<S>)
where
    S: agentkit_loop::ModelSession,
{
    loop {
        match driver.next().await.unwrap() {
            LoopStep::Finished(_) => return,
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {
                panic!("script ran out unexpectedly")
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                panic!("unexpected approval: {}", pending.request.summary)
            }
        }
    }
}

async fn await_input_request<S>(
    driver: &mut agentkit_loop::LoopDriver<S>,
) -> agentkit_loop::InputRequest
where
    S: agentkit_loop::ModelSession,
{
    match driver.next().await.unwrap() {
        LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => req,
        other => panic!("expected AwaitingInput, got {other:?}"),
    }
}
