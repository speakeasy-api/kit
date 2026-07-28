//! End-to-end test: agent driven by a [`SnapshotAdapter`], with a static
//! [`ToolRegistry`] (one mock recording tool) federated alongside a
//! [`CatalogReader`] backed by a live in-memory rmcp server.
//!
//! The snapshot captures the LLM's view of both sources: the tool catalog
//! per turn (specs from both the static registry and the MCP server), and
//! every tool result that landed in the transcript. A passing snapshot
//! means the loop saw both sources, routed each call to the right
//! executor, and surfaced the result back to the model.

use std::sync::Arc;

use agentkit_integration_tests::mcp_server::spawn_in_memory;
use agentkit_integration_tests::mock_tool::RecordingTool;
use agentkit_integration_tests::snapshot::{
    SessionRecording, SnapshotAdapter, assert_recording, snapshot_path,
};
use agentkit_loop::{Agent, LoopInterrupt, LoopStep, SessionConfig};
use agentkit_mcp::{McpToolAdapter, McpToolNamespace};
use agentkit_tools_core::{ToolRegistry, ToolSource, ToolSpec, dynamic_catalog};

use agentkit_core::ToolOutput;
use serde_json::json;

#[tokio::test]
async fn agent_drives_static_and_mcp_tools_in_one_session() {
    let path = snapshot_path("federation.ron");
    let recording = SessionRecording::load(&path);
    let adapter = SnapshotAdapter::from_recording(&recording);

    // Live in-memory MCP server → CatalogWriter populated with
    // McpToolAdapters, paired with a CatalogReader handed to the agent.
    let server = spawn_in_memory("demo").await;
    let connection = Arc::new(server.connection);
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

    // Static source: an echoing recording tool.
    let echo_local = RecordingTool::new(
        ToolSpec::new(
            "echo_local",
            "Echo the supplied text back as a local in-process result.",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"],
                "additionalProperties": false,
            }),
        ),
        |req| {
            let text = req
                .input
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            Ok(ToolOutput::text(format!("local:{text}")))
        },
    );
    let static_registry = ToolRegistry::new().with(echo_local);

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(static_registry)
        .add_tool_source(reader)
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
                panic!("model script ran out before the final assistant text")
            }
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(pending)) => {
                panic!("unexpected approval interrupt: {}", pending.request.summary)
            }
        }
    }

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}
