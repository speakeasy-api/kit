//! [`BasicToolExecutor`] over multiple [`ToolSource`]s with the same tool
//! name. Verifies that [`CollisionPolicy::FirstWins`] and
//! [`CollisionPolicy::LastWins`] route invocations to the right source —
//! the snapshot's tool result body shows which source actually answered.

use std::sync::Arc;

use agentkit_core::ToolOutput;
use agentkit_integration_tests::mock_tool::StaticTool;
use agentkit_integration_tests::snapshot::{
    SessionRecording, SnapshotAdapter, assert_recording, snapshot_path,
};
use agentkit_loop::{Agent, LoopInterrupt, LoopStep, SessionConfig};
use agentkit_tools_core::{
    BasicToolExecutor, CollisionPolicy, ToolExecutor, ToolRegistry, ToolSource,
};

#[tokio::test]
async fn first_wins_routes_to_earlier_source() {
    drive_collision(CollisionPolicy::FirstWins, "collision_first_wins.ron").await;
}

#[tokio::test]
async fn last_wins_routes_to_later_source() {
    drive_collision(CollisionPolicy::LastWins, "collision_last_wins.ron").await;
}

async fn drive_collision(policy: CollisionPolicy, snapshot_file: &str) {
    let path = snapshot_path(snapshot_file);
    let recording = SessionRecording::load(&path);
    let adapter = SnapshotAdapter::from_recording(&recording);

    let primary = StaticTool::new("shared", "primary impl", ToolOutput::text("primary"));
    let secondary = StaticTool::new("shared", "secondary impl", ToolOutput::text("secondary"));

    let primary_source: Arc<dyn ToolSource> = Arc::new(ToolRegistry::new().with(primary));
    let secondary_source: Arc<dyn ToolSource> = Arc::new(ToolRegistry::new().with(secondary));

    let executor: Arc<dyn ToolExecutor> = Arc::new(
        BasicToolExecutor::new([primary_source, secondary_source]).with_collision_policy(policy),
    );

    let agent = Agent::builder()
        .model(adapter.clone())
        .tool_executor(executor)
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
            other => panic!("unexpected step: {other:?}"),
        }
    }

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}
