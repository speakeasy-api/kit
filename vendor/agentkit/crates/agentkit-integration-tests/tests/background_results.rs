//! Matrix coverage for deferred-tool-result handling in `LoopDriver`.
//!
//! Each test exercises one combination of two axes:
//!
//! - **Routing**: pure `Foreground`, pure `Background`,
//!   `ForegroundThenDetachAfter`.
//! - **Completion timing**: in-flight (resolves before yield), post-turn
//!   (resolves after the turn finished, host wakes via `next()`),
//!   and out-of-order completion across multiple tool calls.
//!
//! The invariant the snapshots enforce is the one provider schemas care
//! about: every `tool_use.id` gets exactly one `tool_result.call_id` in
//! the transcript. Late-arriving deferred results that would be a
//! second `tool_result` for the same id MUST land as
//! [`ItemKind::Notification`] instead. The on-disk RON snapshots
//! capture the full transcript shape per turn — single-result invariant,
//! notifications, their order — so each test reduces to driving the
//! loop and a single snapshot comparison. To refresh after intentional
//! changes: `UPDATE_SNAPSHOTS=1 cargo test`.
//!
//! Bootstrapping: the first run of each test (with no `.ron` file
//! present) writes its observed recording to disk and passes. Commit
//! the resulting `.ron` so later runs become strict comparisons.
//!
//! NB: `BlockingTool` invocations need to actually start before we can
//! `release()` them. Across multiple parallel tool calls in one turn,
//! the runtime spawns each call as its own task and the order they
//! reach the body is non-deterministic — but we only assert *what*
//! ends up in the transcript, not *when*. The drive loop coordinates
//! by waiting for `entered`, releasing, then waiting for the
//! corresponding `TaskEvent::Completed` before driving the next turn.

use std::time::Duration;

use agentkit_core::{
    FinishReason, Item, ItemKind, MetadataMap, Part, TextPart, ToolCallId, ToolCallPart,
};
use agentkit_integration_tests::mock_tool::{BlockingTool, NameRoutingPolicy};
use agentkit_integration_tests::snapshot::{
    SessionRecording, SnapshotAdapter, TurnRecord, assert_recording, snapshot_path,
};
use agentkit_loop::{
    Agent, LoopInterrupt, LoopStep, ModelTurnEvent, ModelTurnResult, SessionConfig,
};
use agentkit_task_manager::{
    AsyncTaskManager, RoutingDecision, TaskEvent, TaskManager, TaskManagerHandle,
};
use agentkit_tools_core::ToolRegistry;
use serde_json::json;
use tokio::time::timeout;

// ─── support ──────────────────────────────────────────────────────────────

async fn wait_for_completion(handle: &TaskManagerHandle, tool_name: &str) {
    timeout(Duration::from_secs(2), async {
        loop {
            let event = handle.next_event().await.expect("task event stream ended");
            if let TaskEvent::Completed(snap, _) = event
                && snap.tool_name == tool_name
            {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for completion of {tool_name}"));
}

async fn drive_until_idle(
    driver: &mut agentkit_loop::LoopDriver<impl agentkit_loop::ModelSession>,
) {
    loop {
        let step = driver.next().await.expect("driver step failed");
        match step {
            LoopStep::Finished(_) => return,
            LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => return,
            LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => continue,
            LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(_)) => {
                panic!("unexpected approval interrupt in deferred-result test")
            }
        }
    }
}

fn detach_after(ms: u64) -> RoutingDecision {
    RoutingDecision::ForegroundThenDetachAfter(Duration::from_millis(ms))
}

// ─── seed helpers ─────────────────────────────────────────────────────────

fn user(text: &str) -> Item {
    Item::new(ItemKind::User, vec![Part::Text(TextPart::new(text))])
}

fn tool_call_part(id: &str, name: &str) -> ToolCallPart {
    ToolCallPart::new(ToolCallId::new(id), name, json!({}))
}

/// One scripted turn that emits zero or more tool_calls and finishes
/// with `FinishReason::ToolCall`. The assistant `Item` mirrors the
/// emitted ToolCall parts so the transcript-side observation matches
/// what a real provider would have written.
fn turn_with_tool_calls(calls: &[ToolCallPart]) -> TurnRecord {
    let mut events: Vec<ModelTurnEvent> = calls
        .iter()
        .cloned()
        .map(ModelTurnEvent::ToolCall)
        .collect();
    let assistant_parts: Vec<Part> = calls.iter().cloned().map(Part::ToolCall).collect();
    events.push(ModelTurnEvent::Finished(ModelTurnResult {
        model: None,
        response_id: None,
        finish_reason: FinishReason::ToolCall,
        output_items: vec![Item::new(ItemKind::Assistant, assistant_parts)],
        usage: None,
        metadata: MetadataMap::new(),
    }));
    TurnRecord {
        input: Vec::new(),
        tools: Vec::new(),
        events,
    }
}

/// One scripted turn that emits a single assistant text and finishes.
fn turn_text(text: &str) -> TurnRecord {
    let item = Item::new(ItemKind::Assistant, vec![Part::Text(TextPart::new(text))]);
    TurnRecord {
        input: Vec::new(),
        tools: Vec::new(),
        events: vec![ModelTurnEvent::Finished(ModelTurnResult {
            model: None,
            response_id: None,
            finish_reason: FinishReason::Completed,
            output_items: vec![item],
            usage: None,
            metadata: MetadataMap::new(),
        })],
    }
}

// ─── case 1: pure Foreground (control) ────────────────────────────────────

#[tokio::test]
async fn foreground_emits_single_tool_result_no_notification() {
    let path = snapshot_path("bg_foreground.ron");
    let recording = SessionRecording::load_or_seed(&path, || SessionRecording {
        session_id: "fg".into(),
        initial_items: vec![user("install")],
        turns: vec![
            turn_with_tool_calls(&[tool_call_part("call-fg", "fg-tool")]),
            turn_text("done"),
        ],
        final_transcript: Vec::new(),
    });
    let adapter = SnapshotAdapter::from_recording(&recording);

    let tool = BlockingTool::text("fg-tool", "fg-output");
    tool.release(); // tool returns immediately

    let task_manager = AsyncTaskManager::new().routing(NameRoutingPolicy::new());
    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(tool))
        .task_manager(task_manager)
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    drive_until_idle(&mut driver).await;

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}

// ─── case 2: pure Background (control) ────────────────────────────────────

#[tokio::test]
async fn pure_background_completion_emits_single_tool_result_no_notification() {
    let path = snapshot_path("bg_pure_background.ron");
    let recording = SessionRecording::load_or_seed(&path, || SessionRecording {
        session_id: "pure-bg".into(),
        initial_items: vec![user("kick off")],
        turns: vec![
            turn_with_tool_calls(&[tool_call_part("call-bg", "bg-tool")]),
            turn_text("done"),
        ],
        final_transcript: Vec::new(),
    });
    let adapter = SnapshotAdapter::from_recording(&recording);

    let tool = BlockingTool::text("bg-tool", "bg-output");
    let task_manager = AsyncTaskManager::new()
        .routing(NameRoutingPolicy::new().route("bg-tool", RoutingDecision::Background));
    let handle = task_manager.handle();

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(tool.clone()))
        .task_manager(task_manager)
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    // First next() yields AwaitingInput while the bg task is running.
    match driver.next().await.unwrap() {
        LoopStep::Interrupt(LoopInterrupt::AwaitingInput(_)) => {}
        other => panic!("expected AwaitingInput, got {other:?}"),
    }

    tool.wait_until_entered().await;
    tool.release();
    wait_for_completion(&handle, "bg-tool").await;

    drive_until_idle(&mut driver).await;

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}

// ─── case 3: ForegroundThenDetachAfter, completes before timer ────────────

#[tokio::test]
async fn detach_routing_with_quick_completion_does_not_track_call_id() {
    let path = snapshot_path("bg_detach_quick.ron");
    let recording = SessionRecording::load_or_seed(&path, || SessionRecording {
        session_id: "detach-quick".into(),
        initial_items: vec![user("go")],
        turns: vec![
            turn_with_tool_calls(&[tool_call_part("call-q", "quick-tool")]),
            turn_text("done"),
        ],
        final_transcript: Vec::new(),
    });
    let adapter = SnapshotAdapter::from_recording(&recording);

    let tool = BlockingTool::text("quick-tool", "quick-output");
    tool.release(); // pre-released — invocation returns immediately

    let task_manager = AsyncTaskManager::new()
        .routing(NameRoutingPolicy::new().route("quick-tool", detach_after(500)));

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(tool))
        .task_manager(task_manager)
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    drive_until_idle(&mut driver).await;

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}

// ─── case 4: ForegroundThenDetachAfter detaches, completes post-turn ──────

#[tokio::test]
async fn detach_with_post_turn_completion_emits_single_tool_result_and_notification() {
    let path = snapshot_path("bg_detach_post_turn.ron");
    let recording = SessionRecording::load_or_seed(&path, || SessionRecording {
        session_id: "detach-post".into(),
        initial_items: vec![user("install")],
        turns: vec![
            // Turn 1: emit tool_call, finish with ToolCall reason.
            turn_with_tool_calls(&[tool_call_part("call-d", "detach-tool")]),
            // Turn 2: after detach + synthetic tool_result, finish with text.
            turn_text("kicked off, will let you know"),
            // Turn 3: after Notification arrives, react to it.
            turn_text("install completed"),
        ],
        final_transcript: Vec::new(),
    });
    let adapter = SnapshotAdapter::from_recording(&recording);

    let tool = BlockingTool::text("detach-tool", "detach-output");
    let task_manager = AsyncTaskManager::new()
        .routing(NameRoutingPolicy::new().route("detach-tool", detach_after(50)));
    let handle = task_manager.handle();

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(tool.clone()))
        .task_manager(task_manager)
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    // Drive past detach + assistant text into AwaitingInput.
    drive_until_idle(&mut driver).await;

    // Real completion arrives now.
    tool.wait_until_entered().await;
    tool.release();
    wait_for_completion(&handle, "detach-tool").await;

    // Host explicitly drives the loop to consume the deferred result.
    drive_until_idle(&mut driver).await;

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}

// ─── case 5: two detached tools, complete in call order ───────────────────

#[tokio::test]
async fn two_detached_tools_complete_in_call_order_yields_ordered_notifications() {
    let path = snapshot_path("bg_two_in_order.ron");
    let recording = SessionRecording::load_or_seed(&path, || SessionRecording {
        session_id: "two-in-order".into(),
        initial_items: vec![user("kick off both")],
        turns: vec![
            turn_with_tool_calls(&[
                tool_call_part("call-a", "tool-a"),
                tool_call_part("call-b", "tool-b"),
            ]),
            turn_text("two kicked off"),
            turn_text("a done"),
            turn_text("b done"),
        ],
        final_transcript: Vec::new(),
    });
    let adapter = SnapshotAdapter::from_recording(&recording);

    let tool_a = BlockingTool::text("tool-a", "out-a");
    let tool_b = BlockingTool::text("tool-b", "out-b");
    let task_manager = AsyncTaskManager::new().routing(
        NameRoutingPolicy::new()
            .route("tool-a", detach_after(50))
            .route("tool-b", detach_after(50)),
    );
    let handle = task_manager.handle();

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(
            ToolRegistry::new()
                .with(tool_a.clone())
                .with(tool_b.clone()),
        )
        .task_manager(task_manager)
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    drive_until_idle(&mut driver).await;

    tool_a.wait_until_entered().await;
    tool_b.wait_until_entered().await;
    tool_a.release();
    wait_for_completion(&handle, "tool-a").await;
    drive_until_idle(&mut driver).await;
    tool_b.release();
    wait_for_completion(&handle, "tool-b").await;
    drive_until_idle(&mut driver).await;

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}

// ─── case 6: two detached tools, complete OUT of call order ───────────────

#[tokio::test]
async fn two_detached_tools_complete_out_of_order_yields_completion_ordered_notifications() {
    let path = snapshot_path("bg_two_out_of_order.ron");
    let recording = SessionRecording::load_or_seed(&path, || SessionRecording {
        session_id: "two-out-of-order".into(),
        initial_items: vec![user("kick off both")],
        turns: vec![
            turn_with_tool_calls(&[
                tool_call_part("call-a", "tool-a"),
                tool_call_part("call-b", "tool-b"),
            ]),
            turn_text("two kicked off"),
            turn_text("b done first"),
            turn_text("a done after"),
        ],
        final_transcript: Vec::new(),
    });
    let adapter = SnapshotAdapter::from_recording(&recording);

    let tool_a = BlockingTool::text("tool-a", "out-a");
    let tool_b = BlockingTool::text("tool-b", "out-b");
    let task_manager = AsyncTaskManager::new().routing(
        NameRoutingPolicy::new()
            .route("tool-a", detach_after(50))
            .route("tool-b", detach_after(50)),
    );
    let handle = task_manager.handle();

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(
            ToolRegistry::new()
                .with(tool_a.clone())
                .with(tool_b.clone()),
        )
        .task_manager(task_manager)
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    drive_until_idle(&mut driver).await;

    tool_a.wait_until_entered().await;
    tool_b.wait_until_entered().await;
    // Reverse completion order.
    tool_b.release();
    wait_for_completion(&handle, "tool-b").await;
    drive_until_idle(&mut driver).await;
    tool_a.release();
    wait_for_completion(&handle, "tool-a").await;
    drive_until_idle(&mut driver).await;

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}

// ─── case 7: mixed foreground (live result) + detached (notification) ─────

#[tokio::test]
async fn mixed_foreground_and_detached_tools_only_detached_becomes_notification() {
    let path = snapshot_path("bg_mixed.ron");
    let recording = SessionRecording::load_or_seed(&path, || SessionRecording {
        session_id: "mixed".into(),
        initial_items: vec![user("kick")],
        turns: vec![
            turn_with_tool_calls(&[
                tool_call_part("call-fg", "fast"),
                tool_call_part("call-bg", "slow"),
            ]),
            turn_text("fast finished, slow detached"),
            turn_text("slow done"),
        ],
        final_transcript: Vec::new(),
    });
    let adapter = SnapshotAdapter::from_recording(&recording);

    let fast = BlockingTool::text("fast", "fast-out");
    fast.release();
    let slow = BlockingTool::text("slow", "slow-out");

    let task_manager = AsyncTaskManager::new().routing(
        NameRoutingPolicy::new()
            .route("fast", RoutingDecision::Foreground)
            .route("slow", detach_after(50)),
    );
    let handle = task_manager.handle();

    let agent = Agent::builder()
        .model(adapter.clone())
        .add_tool_source(ToolRegistry::new().with(fast).with(slow.clone()))
        .task_manager(task_manager)
        .build()
        .unwrap();

    let mut driver = agentkit_integration_tests::start_with_initial_input(
        agent,
        SessionConfig::new(recording.session_id.clone()),
        recording.initial_items.clone(),
    )
    .await;

    drive_until_idle(&mut driver).await;
    slow.wait_until_entered().await;
    slow.release();
    wait_for_completion(&handle, "slow").await;
    drive_until_idle(&mut driver).await;

    let observed = adapter.into_recording(&recording, driver.snapshot().transcript.clone());
    assert_recording(&observed, &path);
}
