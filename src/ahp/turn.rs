use std::sync::Arc;

use agenthooksprotocol::inputs::{TurnEndInput, TurnStartInput};
use agenthooksprotocol::{
    EventType, Integer, Session, TurnEndInputOutcome, TurnEndInputTurn, TurnStartInputTrigger,
    TurnStartInputTurn,
};
use agentkit_core::FinishReason;
use agentkit_loop::{AgentEvent, LoopObserver, ObservedEvent};
use tokio::sync::{mpsc, oneshot};

use super::Hooks;

/// Bridges synchronous native loop boundaries to ordered async observations.
#[derive(Clone)]
pub(crate) struct TurnHook(pub(crate) Arc<Hooks>);

pub(super) enum Delivery {
    Event(Box<ObservedEvent>),
    Flush(oneshot::Sender<()>),
}

impl Hooks {
    fn turn_sender(&self) -> &mpsc::UnboundedSender<Delivery> {
        // OnceLock publishes one complete sender. The sole receiver owns dispatch
        // ordering; no lock or ownership guard is held across backend awaits.
        // The worker owns only the SDK, not Hooks, so closing all senders drains
        // pending events and releases the worker without an ownership cycle.
        self.turn_events.get_or_init(|| {
            let (sender, mut receiver) = mpsc::unbounded_channel();
            let sdk = Arc::clone(&self.sdk);
            tokio::spawn(async move {
                while let Some(delivery) = receiver.recv().await {
                    match delivery {
                        Delivery::Flush(done) => {
                            let _ = done.send(());
                        }
                        Delivery::Event(observed) => {
                            let session = Box::new(Session::new(observed.session_id.to_string()));
                            match observed.event {
                                AgentEvent::TurnStarted { turn_id, .. } => {
                                    let input = TurnStartInput::new(
                                        Vec::new(),
                                        TurnStartInputTrigger::External,
                                        TurnStartInputTurn::new(turn_id.to_string()),
                                    )
                                    .with_session(session);
                                    super::settle(
                                        EventType::TurnStart,
                                        sdk.turn_start(input).await,
                                    );
                                }
                                AgentEvent::TurnFinished(result) => {
                                    let outcome = match result.finish_reason {
                                        FinishReason::Cancelled => TurnEndInputOutcome::Cancelled,
                                        FinishReason::Error | FinishReason::Blocked => {
                                            TurnEndInputOutcome::Failed
                                        }
                                        _ => TurnEndInputOutcome::Completed,
                                    };
                                    let Some(continuation_count) = Integer::new(0.into()) else {
                                        tracing::warn!(
                                            "agent hook turn continuation count is invalid"
                                        );
                                        continue;
                                    };
                                    let input = TurnEndInput::new(
                                        continuation_count,
                                        Vec::new(),
                                        outcome,
                                        TurnEndInputTurn::new(result.turn_id.to_string()),
                                    )
                                    .with_session(session)
                                    .with_sources()
                                    .with_items(super::messages(&result.items));
                                    super::settle(EventType::TurnEnd, sdk.turn_end(input).await);
                                }
                                _ => {}
                            }
                        }
                    }
                }
            });
            sender
        })
    }

    /// Waits for previously queued turn observations, including terminal events.
    /// Call after the relevant driver boundary; this does not stop producers.
    pub(crate) async fn flush_turn_events(&self) {
        let Some(sender) = self.turn_events.get() else {
            return;
        };
        let (done, finished) = oneshot::channel();
        if sender.send(Delivery::Flush(done)).is_err() || finished.await.is_err() {
            tracing::warn!("agent hook turn observation flush failed");
        }
    }
}

impl LoopObserver for TurnHook {
    fn handle_event(&self, observed: ObservedEvent) {
        if matches!(
            observed.event,
            AgentEvent::TurnStarted { .. } | AgentEvent::TurnFinished(_)
        ) && self
            .0
            .turn_sender()
            .send(Delivery::Event(Box::new(observed)))
            .is_err()
        {
            tracing::warn!("agent hook turn observation worker stopped");
        }
    }
}
