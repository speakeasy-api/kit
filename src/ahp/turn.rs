use std::sync::{Arc, PoisonError};

use agenthooksprotocol::inputs::{TurnEndInput, TurnStartInput};
use agenthooksprotocol::{
    EventType, Integer, Session, TurnEndInputOutcome, TurnEndInputTurn, TurnStartInputTrigger,
    TurnStartInputTurn,
};
use agentkit_core::FinishReason;
use agentkit_loop::{AgentEvent, LoopObserver, ObservedEvent};

use super::Hooks;

/// Queues native turn boundaries until the session's next async hook point.
#[derive(Clone)]
pub(crate) struct TurnHook(pub(crate) Arc<Hooks>);

impl Hooks {
    /// Delivers the turn observations queued for `session`, in order.
    pub(crate) async fn drain_turn_events(&self, session: &str) {
        while let Some(event) = self.next_turn_event(session) {
            let owner = Box::new(Session::new(session));
            match event {
                AgentEvent::TurnStarted { turn_id, .. } => {
                    let input = TurnStartInput::new(
                        Vec::new(),
                        TurnStartInputTrigger::External,
                        TurnStartInputTurn::new(turn_id.to_string()),
                    )
                    .with_session(owner);
                    super::settle(EventType::TurnStart, self.sdk.turn_start(input).await);
                }
                AgentEvent::TurnFinished(result) => {
                    let outcome = match result.finish_reason {
                        FinishReason::Cancelled => TurnEndInputOutcome::Cancelled,
                        FinishReason::Error | FinishReason::Blocked => TurnEndInputOutcome::Failed,
                        _ => TurnEndInputOutcome::Completed,
                    };
                    let Some(continuations) = Integer::from_i64(0) else {
                        continue;
                    };
                    let input = TurnEndInput::new(
                        continuations,
                        Vec::new(),
                        outcome,
                        TurnEndInputTurn::new(result.turn_id.to_string()),
                    )
                    .with_session(owner)
                    .with_sources()
                    .with_items(super::messages(&result.items));
                    super::settle(EventType::TurnEnd, self.sdk.turn_end(input).await);
                }
                _ => {}
            }
        }
    }

    fn next_turn_event(&self, session: &str) -> Option<AgentEvent> {
        let mut queues = self
            .turn_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let queue = queues.get_mut(session)?;
        let event = queue.pop_front();
        if queue.is_empty() {
            queues.remove(session);
        }
        event
    }
}

impl LoopObserver for TurnHook {
    fn handle_event(&self, observed: ObservedEvent) {
        if matches!(
            observed.event,
            AgentEvent::TurnStarted { .. } | AgentEvent::TurnFinished(_)
        ) {
            self.0
                .turn_events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(observed.session_id.to_string())
                .or_default()
                .push_back(observed.event);
        }
    }
}
