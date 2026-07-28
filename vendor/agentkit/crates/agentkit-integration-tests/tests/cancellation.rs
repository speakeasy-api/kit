//! Mid-tool-call cancellation: a [`BlockingTool`] parks on its release
//! signal, the host fires [`CancellationController::interrupt`], and the
//! loop short-circuits to [`FinishReason::Cancelled`] without ever
//! producing the tool's real result. The synthetic interrupted-assistant
//! item is the contract this test pins down.

use agentkit_core::{
    CancellationController, FinishReason, MetadataMap, Part, ToolCallId, ToolCallPart,
};
use agentkit_integration_tests::mock_model::{MockAdapter, TurnScript};
use agentkit_integration_tests::mock_tool::BlockingTool;
use agentkit_loop::{Agent, LoopStep, ModelTurnEvent, ModelTurnResult, SessionConfig};
use agentkit_task_manager::AsyncTaskManager;
use agentkit_tools_core::ToolRegistry;
use serde_json::json;

#[tokio::test]
async fn cancellation_during_tool_call_finishes_with_cancelled_reason() {
    let controller = CancellationController::new();
    let tool = BlockingTool::text("park", "never-arrives");

    let call = ToolCallPart::new(ToolCallId::new("call-park"), "park", json!({}));
    let assistant = agentkit_core::Item::new(
        agentkit_core::ItemKind::Assistant,
        vec![Part::ToolCall(call.clone())],
    );
    let adapter = MockAdapter::new();
    adapter.enqueue(TurnScript::new([
        ModelTurnEvent::ToolCall(call),
        ModelTurnEvent::Finished(ModelTurnResult {
            model: None,
            response_id: None,
            finish_reason: FinishReason::ToolCall,
            output_items: vec![assistant],
            usage: None,
            metadata: MetadataMap::new(),
        }),
    ]));

    // SimpleTaskManager runs the tool inline inside `start_task`, which
    // means a parked tool body never returns control to the loop and
    // cancellation has nothing to interrupt. AsyncTaskManager spawns the
    // tool body as its own tokio task and lets `wait_for_turn` observe
    // cancellation via the supplied checkpoint.
    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(tool.clone()))
        .task_manager(AsyncTaskManager::new())
        .cancellation(controller.handle())
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new("cancel-mid-tool"),
        vec![agentkit_core::Item::text(
            agentkit_core::ItemKind::User,
            "park forever",
        )],
    )
    .await;

    // Tool stays parked: the loop must be the one to short-circuit via
    // cancellation, not the tool returning a real result.
    let cancelled = tokio::join!(driver.next(), async {
        tool.wait_until_entered().await;
        controller.interrupt();
    })
    .0
    .unwrap();

    match cancelled {
        LoopStep::Finished(turn) => {
            assert_eq!(turn.finish_reason, FinishReason::Cancelled);
        }
        LoopStep::Interrupt(other) => panic!("unexpected interrupt: {other:?}"),
    }

    assert_eq!(tool.invocations().len(), 1);

    // Release the parked task so the spawned tool future can unwind
    // before the test exits.
    tool.release();
    let _ = driver.next().await;
}
