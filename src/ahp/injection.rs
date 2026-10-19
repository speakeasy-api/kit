//! Request-local context injection. Nothing here mutates the durable transcript.
//! Poison recovery: writers use concrete collections, abort-on-OOM allocation, bounded arithmetic, and no callbacks or panicking destructors.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use agenthooksprotocol::client::ProtocolOutcome;
use agenthooksprotocol::{ContentItem, InjectEffectDeliverAt};
use agentkit_core::{Item, Part, TextPart};

const MAX_MESSAGES: usize = 256;
const MAX_BYTES: usize = 1024 * 1024;

type Queue = Arc<Mutex<Pending>>;

#[derive(Default)]
pub(super) struct Injections {
    sessions: Arc<Mutex<HashMap<String, Queue>>>,
}

#[derive(Default)]
struct Pending {
    items: Vec<Item>,
    bytes: usize,
    in_flight_items: usize,
    closed: bool,
    // Immutable for this active generation, including after delivery retires.
    child: bool,
}

impl Injections {
    /// Own one active generation, including across startup cancellation.
    /// Store the returned lease in HookedSession, not a temporary local.
    pub(super) fn open(&self, session: &str, child: bool) -> Result<SessionLease, String> {
        let queue = Arc::new(Mutex::new(Pending {
            child,
            ..Pending::default()
        }));
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        if sessions.contains_key(session) {
            return Err("injection session is already active".into());
        }
        sessions.insert(session.to_owned(), Arc::clone(&queue));
        drop(sessions);
        Ok(SessionLease {
            sessions: Arc::clone(&self.sessions),
            session: session.to_owned(),
            queue,
        })
    }

    /// Identity follows the same lease as injection delivery. Read without holding
    /// the registry lock while acquiring the queue lock; no callbacks or awaits.
    pub(super) fn is_child(&self, session: &str) -> bool {
        self.queue(session)
            .is_ok_and(|queue| queue.lock().unwrap_or_else(PoisonError::into_inner).child)
    }

    /// Retire delivery but retain the active slot until the session owner drops.
    pub(super) fn close(&self, session: &str) -> Result<(), String> {
        let queue = self.queue(session)?;
        retire(&queue);
        Ok(())
    }

    /// Capture BEFORE awaiting the hook dispatch. Late outcomes then retain
    /// their original generation and cannot enter a resumed session's queue.
    pub(super) fn producer(&self, session: &str) -> Option<Producer> {
        Some(Producer {
            queue: self.queue(session).ok()?,
        })
    }

    fn queue(&self, session: &str) -> Result<Queue, String> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session)
            .cloned()
            .ok_or_else(|| "injection session is closed".into())
    }

    /// Take with rollback. Dropping the model-request future cannot lose
    /// the pending context; only successful adapter acceptance consumes it.
    /// HookedSession::begin_turn(&mut self) is the sole production consumer;
    /// producers may still enqueue while that provider call is awaiting.
    pub(super) fn claim(&self, session: &str) -> Result<Delivery, String> {
        let queue = self.queue(session)?;
        let mut pending = queue.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.closed {
            return Err("injection session is closed".into());
        }
        let items = std::mem::take(&mut pending.items);
        let bytes = pending.bytes;
        pending.in_flight_items = items.len();
        drop(pending);
        Ok(Delivery {
            queue,
            items,
            bytes,
            committed: false,
        })
    }
}

/// Captured dispatch ownership; it cannot target a later session generation.
pub(super) struct Producer {
    queue: Queue,
}

impl Producer {
    /// Accept only settled injections; raw subscription responses are not effects.
    /// A rejected batch leaves the existing queue unchanged.
    pub(super) fn enqueue<C, P>(&self, outcome: &ProtocolOutcome<C, P>) -> Result<(), String> {
        let mut items = Vec::new();
        let mut bytes = 0usize;
        for injection in &outcome.injections {
            if injection.deliver_at != InjectEffectDeliverAt::Now {
                return Err("only inject.context deliverAt=now is supported".into());
            }
            for message in &injection.value.0 {
                let kind = super::message_kind(&message.role)?;
                if message.parts.len() > MAX_MESSAGES {
                    return Err("injected context exceeds parts-per-message limit".into());
                }
                let mut parts = Vec::new();
                for part in &message.parts {
                    let ContentItem::TextBodyPart(text) = part.as_ref() else {
                        return Err("injected context requires complete text parts".into());
                    };
                    bytes = bytes.saturating_add(text.text.len());
                    if bytes > MAX_BYTES {
                        return Err("injected context exceeds byte limit".into());
                    }
                    parts.push(Part::Text(TextPart::new(text.text.clone())));
                }
                items.push(Item::new(kind, parts));
                if items.len() > MAX_MESSAGES {
                    return Err("injected context exceeds message limit".into());
                }
            }
        }
        if items.is_empty() {
            return Ok(());
        }
        let mut pending = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.closed {
            return Err("injection session is closed".into());
        }
        if pending.items.len() + pending.in_flight_items + items.len() > MAX_MESSAGES
            || pending.bytes + bytes > MAX_BYTES
        {
            return Err("pending injected context limit reached".into());
        }
        pending.items.extend(items);
        pending.bytes += bytes;
        Ok(())
    }
}

/// Session lifetime ownership, also covering cancellation during session.start.
pub(super) struct SessionLease {
    sessions: Arc<Mutex<HashMap<String, Queue>>>,
    session: String,
    queue: Queue,
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        retire(&self.queue);
        // Duplicate opens are rejected until this owner removes its entry.
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.session);
    }
}

// Retired producer handles retain only an empty queue, not pending payloads.
fn retire(queue: &Queue) {
    let mut pending = queue.lock().unwrap_or_else(PoisonError::into_inner);
    pending.closed = true;
    pending.bytes = 0;
    pending.items.clear();
}

pub(super) struct Delivery {
    queue: Queue,
    items: Vec<Item>,
    bytes: usize,
    committed: bool,
}

impl Delivery {
    pub(super) fn append_to(&self, transcript: &mut Vec<Item>) {
        transcript.extend(self.items.iter().cloned());
    }

    /// Call synchronously immediately after inner.begin_turn returns Ok.
    /// Later stream cancellation does not replay context already accepted.
    pub(super) fn commit(mut self) -> Result<(), String> {
        let mut pending = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.closed {
            // Session retirement already discarded the queue. This receipt may
            // complete afterwards, but must not address a resumed generation.
            self.committed = true;
            return Ok(());
        }
        pending.bytes -= self.bytes;
        pending.in_flight_items = 0;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        if !self.committed {
            let mut pending = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
            if !pending.closed {
                // The in-flight count keeps this append within MAX_MESSAGES.
                self.items.append(&mut pending.items);
                pending.items = std::mem::take(&mut self.items);
            }
            pending.in_flight_items = 0;
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::used_underscore_binding
)]
mod tests {
    use super::*;
    use agenthooksprotocol::client::Permission;
    use agenthooksprotocol::{
        CanonicalMessage, CanonicalMessageRole, CanonicalMessages, InjectEffect, TextBodyPart,
    };

    fn outcome(text: &str) -> ProtocolOutcome {
        let message = CanonicalMessage::new(
            "context",
            vec![Box::new(ContentItem::TextBodyPart(Box::new(
                TextBodyPart::new("text", text),
            )))],
            CanonicalMessageRole::User,
        );
        ProtocolOutcome {
            messages: vec![],
            injections: vec![InjectEffect::new(
                InjectEffectDeliverAt::Now,
                Box::new(CanonicalMessages(vec![Box::new(message)])),
            )],
            responses: vec![],
            settled_candidate: None,
            decision: Permission::Allow,
            stopped: false,
            continuation_requested: false,
            instructions: vec![],
            approval_invalidated: false,
            authorized: true,
            failures: vec![],
        }
    }

    fn enqueue(queues: &Injections, session: &str, text: &str) {
        queues
            .producer(session)
            .unwrap()
            .enqueue(&outcome(text))
            .unwrap();
    }

    fn texts(delivery: &Delivery) -> Vec<String> {
        let mut transcript = vec![];
        delivery.append_to(&mut transcript);
        transcript
            .iter()
            .flat_map(|item| &item.parts)
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn rollback_order_and_one_shot_commit() {
        let queues = Injections::default();
        let _owner_a = queues.open("a", false).unwrap();
        enqueue(&queues, "a", "first");
        let delivery = queues.claim("a").unwrap();
        enqueue(&queues, "a", "second");
        drop(delivery);
        let delivery = queues.claim("a").unwrap();
        assert_eq!(texts(&delivery), ["first", "second"]);
        delivery.commit().unwrap();
        assert!(texts(&queues.claim("a").unwrap()).is_empty());
    }

    #[test]
    fn commit_preserves_concurrent_enqueue() {
        let queues = Injections::default();
        let _owner_a = queues.open("a", false).unwrap();
        enqueue(&queues, "a", "first");
        let delivery = queues.claim("a").unwrap();
        enqueue(&queues, "a", "second");
        delivery.commit().unwrap();
        assert_eq!(texts(&queues.claim("a").unwrap()), ["second"]);
    }

    #[test]
    fn child_identity_survives_retirement_but_not_lease_release() {
        let queues = Injections::default();
        let child = queues.open("child", true).unwrap();
        let _root = queues.open("root", false).unwrap();
        assert!(queues.is_child("child"));
        assert!(!queues.is_child("root"));
        assert!(queues.open("child", false).is_err());
        assert!(queues.is_child("child"));
        queues.close("child").unwrap();
        assert!(queues.is_child("child"));
        drop(child);
        assert!(!queues.is_child("child"));
        let _resumed = queues.open("child", false).unwrap();
        assert!(!queues.is_child("child"));
    }

    #[test]
    fn isolated_sessions_and_stale_receipts() {
        let queues = Injections::default();
        let _owner_a = queues.open("a", false).unwrap();
        let _owner_b = queues.open("b", false).unwrap();
        enqueue(&queues, "a", "old");
        let old = queues.claim("a").unwrap();
        queues.close("a").unwrap();
        assert!(queues.open("a", false).is_err());
        assert!(
            queues
                .producer("a")
                .unwrap()
                .enqueue(&outcome("late"))
                .is_err()
        );
        drop(_owner_a);
        let _owner_a = queues.open("a", false).unwrap();
        enqueue(&queues, "a", "new");
        old.commit().unwrap();
        assert_eq!(texts(&queues.claim("a").unwrap()), ["new"]);
        assert!(texts(&queues.claim("b").unwrap()).is_empty());
    }

    #[test]
    fn stale_producer_cannot_inject_into_resumed_session() {
        let queues = Injections::default();
        let owner = queues.open("a", false).unwrap();
        let producer = queues.producer("a").unwrap();
        producer.enqueue(&outcome("retired payload")).unwrap();
        assert!(queues.open("a", false).is_err());
        drop(owner);
        let _resumed = queues.open("a", false).unwrap();
        assert!(producer.enqueue(&outcome("stale")).is_err());
        assert!(texts(&queues.claim("a").unwrap()).is_empty());
    }

    #[tokio::test]
    async fn cancelled_session_start_releases_ownership() {
        let queues = Arc::new(Injections::default());
        let (ready, received) = tokio::sync::oneshot::channel();
        let task = tokio::spawn({
            let queues = queues.clone();
            async move {
                let _owner = queues.open("a", true).unwrap();
                ready.send(()).unwrap();
                std::future::pending::<()>().await;
            }
        });
        received.await.unwrap();
        assert!(queues.is_child("a"));
        assert!(queues.open("a", false).is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!queues.is_child("a"));
        let _resumed = queues.open("a", false).unwrap();
    }

    #[test]
    fn invalid_batch_does_not_partially_enqueue() {
        let queues = Injections::default();
        let _owner_a = queues.open("a", false).unwrap();
        let mut too_many_parts = outcome("");
        let part = too_many_parts.injections[0].value.0[0].parts[0].clone();
        too_many_parts.injections[0].value.0[0].parts = vec![part; MAX_MESSAGES + 1];
        assert!(
            queues
                .producer("a")
                .unwrap()
                .enqueue(&too_many_parts)
                .is_err()
        );
        let mut batch = outcome("valid");
        let mut invalid = outcome("invalid");
        invalid.injections[0].deliver_at = InjectEffectDeliverAt::NextTurn;
        batch.injections.extend(invalid.injections);
        assert!(queues.producer("a").unwrap().enqueue(&batch).is_err());
        assert!(texts(&queues.claim("a").unwrap()).is_empty());
        assert!(
            queues
                .producer("a")
                .unwrap()
                .enqueue(&outcome(&"x".repeat(MAX_BYTES + 1)))
                .is_err()
        );
        enqueue(&queues, "a", "still works");
    }

    #[test]
    fn in_flight_context_still_counts_toward_limits() {
        let queues = Injections::default();
        let _owner = queues.open("a", false).unwrap();
        let producer = queues.producer("a").unwrap();
        for _ in 0..MAX_MESSAGES {
            enqueue(&queues, "a", "");
        }
        let delivery = queues.claim("a").unwrap();
        assert!(producer.enqueue(&outcome("overflow")).is_err());
        drop(delivery);
        let delivery = queues.claim("a").unwrap();
        assert_eq!(texts(&delivery).len(), MAX_MESSAGES);
        delivery.commit().unwrap();
        producer.enqueue(&outcome(&"x".repeat(MAX_BYTES))).unwrap();
        let delivery = queues.claim("a").unwrap();
        assert!(producer.enqueue(&outcome("overflow")).is_err());
        delivery.commit().unwrap();
        enqueue(&queues, "a", "room again");
    }

    #[tokio::test]
    async fn cancellation_releases_reservation_without_losing_context() {
        let queues = Arc::new(Injections::default());
        let _owner_a = queues.open("a", false).unwrap();
        enqueue(&queues, "a", "retained");
        let (ready, received) = tokio::sync::oneshot::channel();
        let task = tokio::spawn({
            let queues = queues.clone();
            async move {
                let _delivery = queues.claim("a").unwrap();
                ready.send(()).unwrap();
                std::future::pending::<()>().await;
            }
        });
        received.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(texts(&queues.claim("a").unwrap()), ["retained"]);
    }
}
