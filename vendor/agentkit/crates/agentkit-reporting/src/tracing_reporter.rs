//! Tracing-based reporter.
//!
//! [`TracingReporter`] converts [`AgentEvent`]s into [`tracing`] events,
//! bridging the observer system with the Rust ecosystem's structured logging /
//! distributed tracing infrastructure.
//!
//! This module is only available when the `tracing` feature is enabled.

use agentkit_loop::{AgentEvent, LoopObserver, ObservedEvent};

/// Reporter that emits [`tracing`] events for every [`AgentEvent`].
///
/// Event levels are chosen to match typical subscriber filter defaults:
///
/// | Agent event | Tracing level |
/// |---|---|
/// | `RunStarted`, `TurnStarted`, `TurnFinished` | `INFO` |
/// | `ToolCallRequested`, `ToolExecutionStarted/Progress`, `ApprovalRequired/Resolved`, `AuthRequired/Resolved` | `INFO` |
/// | `InputAccepted`, `UsageUpdated`, `CompactionStarted/Finished` | `DEBUG` |
/// | `ContentDelta` | `TRACE` |
/// | `Warning` | `WARN` |
/// | `RunFailed` | `ERROR` |
///
/// All events are emitted under the target `"agentkit_reporting"` (matching
/// the crate name) so reporter output is filterable independently of
/// agentkit's own internal `tracing::instrument` spans, which use their own
/// crate paths (`agentkit_loop`, `agentkit_provider_*`, etc.). With
/// `RUST_LOG`, this lets you tune reporter verbosity separately from loop
/// internals:
///
/// ```text
/// RUST_LOG=agentkit_reporting=info,agentkit_loop=debug
/// ```
///
/// To route reporter output into your application's own log namespace,
/// implement [`LoopObserver`] directly and call `tracing::*!` macros with
/// your own `target:` literal. The `tracing` macros require compile-time
/// constant targets, so a runtime-configurable target on this reporter is
/// not exposed.
///
/// # Example
///
/// ```rust
/// use agentkit_reporting::TracingReporter;
///
/// let reporter = TracingReporter::new();
/// ```
#[derive(Default)]
pub struct TracingReporter {
    _private: (),
}

impl TracingReporter {
    /// Creates a new `TracingReporter` whose events are emitted under the
    /// `agentkit_reporting` tracing target.
    pub fn new() -> Self {
        Self::default()
    }
}

impl LoopObserver for TracingReporter {
    fn handle_event(&self, event: ObservedEvent) {
        let event = event.event;
        match &event {
            AgentEvent::RunStarted { session_id } => {
                tracing::info!(target: "agentkit_reporting", session_id = %session_id, "agent run started");
            }
            AgentEvent::TurnStarted {
                session_id,
                turn_id,
            } => {
                tracing::info!(target: "agentkit_reporting", session_id = %session_id, turn_id = %turn_id, "turn started");
            }
            AgentEvent::InputAccepted { session_id, items } => {
                tracing::debug!(target: "agentkit_reporting", session_id = %session_id, item_count = items.len(), "input accepted");
            }
            AgentEvent::ContentDelta(delta) => {
                tracing::trace!(target: "agentkit_reporting", ?delta, "content delta");
            }
            AgentEvent::ToolCallRequested(call) => {
                tracing::info!(target: "agentkit_reporting", tool = %call.name, "tool call requested");
            }
            AgentEvent::ToolExecutionStarted(call) => {
                tracing::info!(target: "agentkit_reporting", tool = %call.name, call_id = %call.id, "tool execution started");
            }
            AgentEvent::ToolExecutionProgress(result) => {
                tracing::info!(
                    target: "agentkit_reporting",
                    call_id = %result.call_id,
                    is_error = result.is_error,
                    "tool execution progress"
                );
            }
            AgentEvent::ToolResultReceived(result) => {
                tracing::info!(
                    target: "agentkit_reporting",
                    call_id = %result.call_id,
                    is_error = result.is_error,
                    "tool result received"
                );
            }
            AgentEvent::ApprovalRequired(request) => {
                tracing::info!(target: "agentkit_reporting", summary = %request.summary, reason = ?request.reason, "approval required");
            }
            AgentEvent::ApprovalResolved { approved } => {
                tracing::info!(target: "agentkit_reporting", approved, "approval resolved");
            }
            AgentEvent::ToolCatalogChanged(event) => {
                tracing::info!(
                    target: "agentkit_reporting",
                    source = %event.source,
                    added = event.added.len(),
                    removed = event.removed.len(),
                    changed = event.changed.len(),
                    "tool catalog changed"
                );
            }
            AgentEvent::MutationStarted {
                session_id,
                turn_id,
                mutator,
                point,
            } => {
                let turn = turn_id.as_ref().map(|t| t.to_string()).unwrap_or_default();
                tracing::debug!(
                    target: "agentkit_reporting",
                    session_id = %session_id,
                    turn_id = %turn,
                    mutator = %mutator,
                    point = ?point,
                    "mutation started"
                );
            }
            AgentEvent::MutationFinished {
                session_id,
                turn_id,
                mutator,
                dirty,
                ..
            } => {
                let turn = turn_id.as_ref().map(|t| t.to_string()).unwrap_or_default();
                tracing::debug!(
                    target: "agentkit_reporting",
                    session_id = %session_id,
                    turn_id = %turn,
                    mutator = %mutator,
                    dirty,
                    "mutation finished"
                );
            }
            AgentEvent::UsageUpdated(usage) => {
                if let Some(tokens) = &usage.tokens {
                    tracing::debug!(
                        target: "agentkit_reporting",
                        input_tokens = tokens.input_tokens,
                        output_tokens = tokens.output_tokens,
                        "usage updated"
                    );
                }
            }
            AgentEvent::Warning { message } => {
                tracing::warn!(target: "agentkit_reporting", message, "agent warning");
            }
            AgentEvent::RunFailed { message } => {
                tracing::error!(target: "agentkit_reporting", message, "agent run failed");
            }
            AgentEvent::TurnFinished(result) => {
                tracing::info!(
                    target: "agentkit_reporting",
                    finish_reason = ?result.finish_reason,
                    item_count = result.items.len(),
                    "turn finished"
                );
            }
            _ => {}
        }
    }
}
