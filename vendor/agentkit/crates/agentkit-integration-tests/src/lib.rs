//! Test support for agentkit end-to-end integration tests.
//!
//! Two pieces:
//!
//! - [`mock_model`] — a scriptable, introspectable [`ModelAdapter`] used as a
//!   stand-in for a real provider. Tests enqueue a sequence of "turn scripts"
//!   describing what the model should emit (text, tool calls, finish), and
//!   inspect every [`TurnRequest`] the loop hands the model to assert that
//!   the transcript and advertised tool catalog look right.
//!
//! - [`mcp_server`] — an in-memory rmcp server bound to a tokio duplex,
//!   plus a ready-made [`McpHandlerConfig`]/[`McpServerManager`] wiring so
//!   tests can exercise the full agentkit ↔ rmcp ↔ MCP server stack
//!   without spawning a child process.

pub mod http_mcp_server;
pub mod mcp_server;
pub mod mock_model;
pub mod mock_tool;
pub mod snapshot;

use agentkit_core::Item;
use agentkit_loop::{
    Agent, LoopDriver, LoopInterrupt, LoopStep, ModelAdapter, ModelSession, SessionConfig,
};

/// Drives the first `AwaitingInput` interrupt and submits `initial_items`
/// as the opening turn's input. Tests use this when they want to exercise
/// the explicit interrupt path; one-shot callers are usually better off
/// preloading via [`agentkit_loop::AgentBuilder::input`].
pub async fn start_with_initial_input<M>(
    agent: Agent<M>,
    config: SessionConfig,
    initial_items: Vec<Item>,
) -> LoopDriver<M::Session>
where
    M: ModelAdapter,
    M::Session: ModelSession + Send,
{
    let mut driver = agent.start(config).await.expect("agent.start");
    match driver.next().await.expect("first next") {
        LoopStep::Interrupt(LoopInterrupt::AwaitingInput(req)) => {
            req.submit(&mut driver, initial_items)
                .expect("submit initial input");
            driver
        }
        other => panic!("expected AwaitingInput as first step, got {other:?}"),
    }
}
