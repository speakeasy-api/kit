use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use agenthooksprotocol::inputs::TurnEndInput;
use agenthooksprotocol::{EventType, Integer, Session, TurnEndInputOutcome, TurnEndInputTurn};
use agentkit_core::FinishReason;
use agentkit_loop::{AgentEvent, LoopObserver, ObservedEvent};

use super::Hooks;

pub(super) type TurnQueues = Arc<Mutex<HashMap<String, SessionTurns>>>;

/// Queues native turn completions until the session's next async hook point.
/// Starts run in the prompt mutator, where their effects can govern admission.
#[derive(Clone)]
pub(crate) struct TurnHook(pub(crate) Arc<Hooks>);

#[derive(Default)]
pub(super) struct SessionTurns {
    pending: VecDeque<AgentEvent>,
    started: HashSet<String>,
    delivery: Arc<tokio::sync::Mutex<()>>,
}

impl Hooks {
    /// Claims the one start delivery for a native turn. The serialized driver is
    /// the only admission writer; completion draining removes the claim.
    pub(super) fn claim_turn_start(&self, session: &str, turn: &str) -> bool {
        lock(&self.turn_events)
            .entry(session.to_owned())
            .or_default()
            .started
            .insert(turn.to_owned())
    }

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
        let owner = self.session(session);
        let session = session.to_owned();
        // A cancelled caller must not abandon an observation the SDK has begun delivering.
        let delivered = tokio::spawn(async move {
            let _guard = guard;
            while let Some((event, needs_start)) = next(&queues, &session) {
                // Idle background approvals can be cancelled before any mutator
                // runs. Pair their completion without admitting historical input.
                if needs_start && let AgentEvent::TurnFinished(result) = &event {
                    let input = agenthooksprotocol::inputs::TurnStartInput::new(
                        Vec::new(),
                        agenthooksprotocol::TurnStartInputTrigger::External,
                        agenthooksprotocol::TurnStartInputTurn::new(result.turn_id.to_string()),
                    )
                    .with_session(owner.clone());
                    super::settle(
                        EventType::TurnStart,
                        super::dispatch(sdk.turn_start(input.with_sources()), None).await,
                    );
                }
                deliver(&sdk, owner.clone(), event).await;
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
    // Writers insert complete queues, enqueue native completion values, or pop/remove
    // them. No callbacks or awaits run under this guard; poisoning cannot leave a
    // partially delivered value in the queue. Start claims are complete string
    // insertions/removals with no external work under the lock; delivery is
    // serialized separately and cancellation cannot abandon a claimed delivery.
    queues.lock().unwrap_or_else(PoisonError::into_inner)
}

fn next(queues: &TurnQueues, session: &str) -> Option<(AgentEvent, bool)> {
    let mut queues = lock(queues);
    let turns = queues.get_mut(session)?;
    let event = turns.pending.pop_front()?;
    let needs_start = matches!(&event, AgentEvent::TurnFinished(result)
        if !turns.started.remove(&result.turn_id.to_string()));
    Some((event, needs_start))
}

async fn deliver(sdk: &agenthooksprotocol::Hooks, owner: Box<Session>, event: AgentEvent) {
    if let AgentEvent::TurnFinished(result) = event {
        let outcome = match result.finish_reason {
            FinishReason::Cancelled => TurnEndInputOutcome::Cancelled,
            FinishReason::Error | FinishReason::Blocked => TurnEndInputOutcome::Failed,
            _ => TurnEndInputOutcome::Completed,
        };
        // Kit grants no AHP continue effect; native tool iterations are not
        // hook-requested turn continuations.
        let Some(continuations) = Integer::from_i64(0) else {
            return;
        };
        let mut input = TurnEndInput::new(
            continuations,
            Vec::new(),
            outcome,
            TurnEndInputTurn::new(result.turn_id.to_string()),
        )
        .with_session(owner);
        input.last_assistant_item = result
            .items
            .iter()
            .enumerate()
            .rev()
            .find(|(_, item)| item.kind == agentkit_core::ItemKind::Assistant)
            .map(|(index, item)| {
                let mut reference = agenthooksprotocol::TurnEndInputLastAssistantItem::new(
                    super::item_id(item, index),
                );
                if item.id.is_none() {
                    reference.synthesized = agenthooksprotocol::Presence::Present(true);
                }
                reference
            });
        if let Some(reasoning) = result
            .usage
            .as_ref()
            .and_then(|usage| usage.tokens.as_ref())
            .and_then(|tokens| tokens.reasoning_tokens)
        {
            let mut extensions = agenthooksprotocol::Extensions::new();
            extensions.additional_properties.insert(
                "dev.kit.usage".into(),
                super::object([("reasoningTokens", reasoning.into())]),
            );
            input.extensions = Some(Box::new(extensions));
        }
        input.usage = result.usage.as_ref().map(super::usage::turn).map(Box::new);
        let input = input
            .with_sources()
            .with_items(super::messages(&result.items));
        super::settle(
            EventType::TurnEnd,
            super::dispatch(sdk.turn_end(input), None).await,
        );
    }
}

impl LoopObserver for TurnHook {
    fn handle_event(&self, observed: ObservedEvent) {
        if matches!(observed.event, AgentEvent::TurnFinished(_)) {
            lock(&self.0.turn_events)
                .entry(observed.session_id.to_string())
                .or_default()
                .pending
                .push_back(observed.event);
        }
    }
}
