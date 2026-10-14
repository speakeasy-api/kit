use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use agenthooksprotocol::inputs::{TurnEndInput, TurnStartInput};
use agenthooksprotocol::{
    EventType, Integer, Session, TurnEndInputOutcome, TurnEndInputTurn, TurnStartInputTrigger,
    TurnStartInputTurn,
};
use agentkit_core::FinishReason;
use agentkit_loop::{AgentEvent, LoopObserver, ObservedEvent};

use super::Hooks;

pub(super) type TurnQueues = Arc<Mutex<HashMap<String, SessionTurns>>>;

/// Queues native turn boundaries until the session's next async hook point.
#[derive(Clone)]
pub(crate) struct TurnHook(pub(crate) Arc<Hooks>);

#[derive(Default)]
pub(super) struct SessionTurns {
    pending: VecDeque<AgentEvent>,
    delivery: Arc<tokio::sync::Mutex<()>>,
}

impl Hooks {
    /// Delivers the turn observations queued for `session`, in order.
    pub(crate) async fn drain_turn_events(&self, session: &str) {
        let delivery = Arc::clone(
            &lock(&self.turn_events)
                .entry(session.to_owned())
                .or_default()
                .delivery,
        );
        let guard = delivery.lock_owned().await;
        if lock(&self.turn_events)
            .get(session)
            .is_none_or(|turns| turns.pending.is_empty())
        {
            return;
        }
        let sdk = Arc::clone(&self.sdk);
        let queues = Arc::clone(&self.turn_events);
        let session = session.to_owned();
        // A cancelled caller must not abandon an observation the SDK has begun delivering.
        let delivered = tokio::spawn(async move {
            let _guard = guard;
            while let Some(event) = next(&queues, &session) {
                deliver(&sdk, &session, event).await;
            }
        });
        if delivered.await.is_err() {
            tracing::warn!("agent hook turn observation delivery failed");
        }
    }

    pub(super) fn forget_turn_events(&self, session: &str) {
        lock(&self.turn_events).remove(session);
    }
}

fn lock(queues: &TurnQueues) -> MutexGuard<'_, HashMap<String, SessionTurns>> {
    queues.lock().unwrap_or_else(PoisonError::into_inner)
}

fn next(queues: &TurnQueues, session: &str) -> Option<AgentEvent> {
    lock(queues).get_mut(session)?.pending.pop_front()
}

async fn deliver(sdk: &agenthooksprotocol::Hooks, session: &str, event: AgentEvent) {
    let owner = Box::new(Session::new(session));
    match event {
        AgentEvent::TurnStarted { turn_id, .. } => {
            let input = TurnStartInput::new(
                Vec::new(),
                TurnStartInputTrigger::External,
                TurnStartInputTurn::new(turn_id.to_string()),
            )
            .with_session(owner);
            super::settle(EventType::TurnStart, sdk.turn_start(input).await);
        }
        AgentEvent::TurnFinished(result) => {
            let outcome = match result.finish_reason {
                FinishReason::Cancelled => TurnEndInputOutcome::Cancelled,
                FinishReason::Error | FinishReason::Blocked => TurnEndInputOutcome::Failed,
                _ => TurnEndInputOutcome::Completed,
            };
            let Some(continuations) = Integer::from_i64(0) else {
                return;
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
            super::settle(EventType::TurnEnd, sdk.turn_end(input).await);
        }
        _ => {}
    }
}

impl LoopObserver for TurnHook {
    fn handle_event(&self, observed: ObservedEvent) {
        if matches!(
            observed.event,
            AgentEvent::TurnStarted { .. } | AgentEvent::TurnFinished(_)
        ) {
            lock(&self.0.turn_events)
                .entry(observed.session_id.to_string())
                .or_default()
                .pending
                .push_back(observed.event);
        }
    }
}
