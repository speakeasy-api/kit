//! Real async-manager execution documents the upstream spawn boundary without
//! wrapping or patching it. Set KIT_TEST_REQUIRE_TASK_MANAGER_ANCESTRY=1 to turn
//! the known limitation assertion into the desired (currently failing) contract.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use agentkit_core::{
    FinishReason, Item, ItemKind, MetadataMap, Part, SessionId, ToolCallPart, ToolOutput,
    ToolResultPart, TurnCancellation,
};
use agentkit_loop::{
    Agent, LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, ModelTurnResult,
    SessionConfig, TurnRequest,
};
use agentkit_task_manager::AsyncTaskManager;
use agentkit_tools_core::{
    Tool, ToolContext, ToolError, ToolName, ToolRegistry, ToolRequest, ToolResult, ToolSpec,
};
use async_trait::async_trait;
use serde_json::json;
use tracing::Instrument;
use tracing_subscriber::prelude::*;

use super::{ErrorSpanLayer, operation, snapshot};

struct FixtureAdapter;
struct FixtureSession(bool);
struct FixtureTurn(VecDeque<ModelTurnEvent>);

#[async_trait]
impl ModelAdapter for FixtureAdapter {
    type Session = FixtureSession;

    async fn start_session(&self, _: SessionConfig) -> Result<Self::Session, LoopError> {
        Ok(FixtureSession(false))
    }
}

#[async_trait]
impl ModelSession for FixtureSession {
    type Turn = FixtureTurn;

    async fn begin_turn(
        &mut self,
        _: TurnRequest,
        _: Option<TurnCancellation>,
    ) -> Result<Self::Turn, LoopError> {
        if self.0 {
            return Err(LoopError::Provider("fixture final failure".into()));
        }
        self.0 = true;
        let call = ToolCallPart::new("probe-call", "probe", json!({}));
        Ok(FixtureTurn(VecDeque::from([
            ModelTurnEvent::ToolCall(call.clone()),
            ModelTurnEvent::Finished(ModelTurnResult {
                finish_reason: FinishReason::ToolCall,
                output_items: vec![Item::new(ItemKind::Assistant, vec![Part::ToolCall(call)])],
                usage: None,
                metadata: MetadataMap::new(),
                model: None,
                response_id: None,
            }),
        ])))
    }
}

#[async_trait]
impl ModelTurn for FixtureTurn {
    async fn next_event(
        &mut self,
        _: Option<TurnCancellation>,
    ) -> Result<Option<ModelTurnEvent>, LoopError> {
        Ok(self.0.pop_front())
    }
}

struct Probe {
    spec: ToolSpec,
    executed: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for Probe {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    async fn invoke(
        &self,
        request: ToolRequest,
        _: &mut ToolContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        // Same allowlisted shape as the control span, but a distinct numeric
        // marker so the loop's own inference spans cannot satisfy the assertion.
        let span = tracing::info_span!(target: "agentkit_loop", "chat",
            gen_ai.operation.name = "chat", gen_ai.usage.output_tokens = 4242_u64);
        assert!(
            !span.is_disabled(),
            "global subscriber must reach the spawned task"
        );
        async {
            self.executed.store(true, Ordering::SeqCst);
            Ok(ToolResult::new(ToolResultPart::success(
                request.call_id,
                ToolOutput::text("probe completed"),
            )))
        }
        .instrument(span)
        .await
    }
}

#[tokio::test]
async fn async_manager_does_not_retain_invocation_ancestry() {
    const TEST: &str = "telemetry::error_spans::task_manager_tests::async_manager_does_not_retain_invocation_ancestry";
    const CHILD: &str = "KIT_TASK_MANAGER_ANCESTRY_TEST_CHILD";
    if std::env::var(CHILD).as_deref() != Ok(TEST) {
        // A global subscriber in a fresh process tests span propagation, not
        // the separate failure to propagate a thread-local default subscriber.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, TEST)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
            "isolated child must run the exact ancestry test"
        );
        return;
    }
    tracing_subscriber::registry().with(ErrorSpanLayer).init();
    let executed = Arc::new(AtomicBool::new(false));
    let tools = ToolRegistry::new().with(Probe {
        spec: ToolSpec::new(ToolName::new("probe"), "probe", json!({"type": "object"})),
        executed: executed.clone(),
    });
    let agent = Agent::builder()
        .model(FixtureAdapter)
        .add_tool_source(tools)
        // Default routing is foreground: detachment is not needed to lose ancestry.
        .task_manager(AsyncTaskManager::new())
        .input(vec![Item::text(ItemKind::User, "probe")])
        .build()
        .unwrap();
    let root = operation("prompt");
    let error = async {
        tracing::info_span!(target: "agentkit_loop", "chat",
            gen_ai.operation.name = "chat", gen_ai.usage.output_tokens = 4241_u64)
        .in_scope(|| {});
        let mut driver = agent
            .start(SessionConfig::new(SessionId::new("ancestry-test")).without_cache())
            .await
            .unwrap();
        for _ in 0..8 {
            if let Err(error) = driver.next().await {
                return error;
            }
        }
        panic!("fixture did not reach the final provider failure");
    }
    .instrument(root.clone())
    .await;
    assert!(matches!(error, LoopError::Provider(message) if message == "fixture final failure"));
    assert!(executed.load(Ordering::SeqCst));
    let snapshot = snapshot(&root).expect("operation retains history at the actual error boundary");
    assert!(snapshot.valid());
    assert!(!snapshot.truncated);
    assert!(
        snapshot
            .fragments
            .iter()
            .any(|fragment| fragment.name == "agent.execute_tool")
    );
    let has_marker = |marker| {
        snapshot.fragments.iter().any(|fragment| {
            fragment.name == "chat"
                && fragment.fields.get("gen_ai.usage.output_tokens") == Some(&json!(marker))
        })
    };
    assert!(
        has_marker(4241),
        "allowlisted control span must be captured"
    );
    if std::env::var("KIT_TEST_REQUIRE_TASK_MANAGER_ANCESTRY").as_deref() == Ok("1") {
        assert!(
            has_marker(4242),
            "actual tool invocation lost its dispatch/operation ancestry"
        );
    } else {
        assert!(
            !has_marker(4242),
            "upstream propagation changed; revisit the documented scope"
        );
    }
}
