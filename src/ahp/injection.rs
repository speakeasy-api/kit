//! Request-local context injection. Nothing here mutates the durable transcript.
//!
//! Writers: open/lease-drop own registry entries; close retires a queue; enqueue appends atomically;
//! claim reserves a prefix; commit removes that prefix; Delivery::drop releases
//! the reservation on error, unwind, or cancellation. No lock crosses an await,
//! invokes callbacks, or nests with another lock. Poison isolates that queue:
//! callers receive an error, never recover potentially inconsistent state.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agenthooksprotocol::client::ProtocolOutcome;
use agenthooksprotocol::{CanonicalMessageRole, ContentItem, InjectEffectDeliverAt};
use agentkit_core::{Item, ItemKind, Part, TextPart};

const MAX_MESSAGES: usize = 256;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_SESSIONS: usize = 1024;

type Queue = Arc<Mutex<Pending>>;

#[derive(Default)]
pub(super) struct Injections {
    sessions: Arc<Mutex<HashMap<String, Queue>>>,
}

#[derive(Default)]
struct Pending {
    items: Vec<Item>,
    bytes: usize,
    claimed: bool,
    closed: bool,
}

impl Injections {
    /// Own one active generation, including across startup cancellation.
    /// Store the returned lease in HookedSession, not a temporary local.
    pub(super) fn open(&self, session: &str) -> Result<SessionLease, String> {
        let queue = Arc::new(Mutex::new(Pending::default()));
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| "injection registry poisoned")?;
        if sessions.contains_key(session) {
            return Err("injection session is already active".into());
        }
        if sessions.len() >= MAX_SESSIONS {
            return Err("too many injection sessions".into());
        }
        sessions.insert(session.to_owned(), Arc::clone(&queue));
        drop(sessions);
        Ok(SessionLease {
            sessions: Arc::clone(&self.sessions),
            session: session.to_owned(),
            queue,
        })
    }

    /// Retire delivery but retain the active slot until the session owner drops.
    pub(super) fn close(&self, session: &str) -> Result<(), String> {
        let queue = self.queue(session)?;
        retire(&queue)
    }

    /// Capture BEFORE awaiting the hook dispatch. Late outcomes then retain
    /// their original generation and cannot enter a resumed session's queue.
    pub(super) fn producer(&self, session: &str) -> Result<Producer, String> {
        Ok(Producer {
            queue: self.queue(session)?,
        })
    }

    fn queue(&self, session: &str) -> Result<Queue, String> {
        self.sessions
            .lock()
            .map_err(|_| "injection registry poisoned")?
            .get(session)
            .cloned()
            .ok_or_else(|| "injection session is closed".into())
    }

    /// Reserve without removing. Dropping the model-request future cannot lose
    /// the pending context; only successful adapter acceptance consumes it.
    pub(super) fn claim(&self, session: &str) -> Result<Delivery, String> {
        let queue = self.queue(session)?;
        let mut pending = queue.lock().map_err(|_| "injection queue poisoned")?;
        if pending.closed {
            return Err("injection session is closed".into());
        }
        if pending.claimed {
            return Err("injection delivery already in progress".into());
        }
        let items = pending.items.clone();
        let bytes = pending.bytes;
        pending.claimed = true;
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
                let kind = match message.role {
                    CanonicalMessageRole::System => ItemKind::System,
                    CanonicalMessageRole::Developer => ItemKind::Developer,
                    CanonicalMessageRole::User => ItemKind::User,
                    CanonicalMessageRole::Assistant => ItemKind::Assistant,
                    // A synthetic tool result has no native call identity.
                    _ => return Err("unsupported injected message role".into()),
                };
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
        let mut pending = self.queue.lock().map_err(|_| "injection queue poisoned")?;
        if pending.closed {
            return Err("injection session is closed".into());
        }
        if pending.items.len() + items.len() > MAX_MESSAGES || pending.bytes + bytes > MAX_BYTES {
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
        // Close first so outstanding producer tokens reject late outcomes.
        if let Err(error) = retire(&self.queue) {
            tracing::warn!(%error, "injection queue could not retire during session cleanup");
        }
        let removed = match self.sessions.lock() {
            Ok(mut sessions) => {
                if sessions
                    .get(&self.session)
                    .is_some_and(|queue| Arc::ptr_eq(queue, &self.queue))
                {
                    sessions.remove(&self.session)
                } else {
                    None
                }
            }
            Err(_) => {
                tracing::warn!("injection registry poisoned during session cleanup");
                None
            }
        };
        drop(removed);
    }
}

// Retired producer handles retain only an empty queue, not pending payloads.
fn retire(queue: &Queue) -> Result<(), String> {
    let mut pending = queue.lock().map_err(|_| "injection queue poisoned")?;
    pending.closed = true;
    pending.bytes = 0;
    let removed = std::mem::take(&mut pending.items);
    drop(pending);
    drop(removed);
    Ok(())
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
        let mut pending = self.queue.lock().map_err(|_| "injection queue poisoned")?;
        if pending.closed {
            // Session retirement already discarded the queue. This receipt may
            // complete afterwards, but must not address a resumed generation.
            self.committed = true;
            return Ok(());
        }
        let removed: Vec<_> = pending.items.drain(..self.items.len()).collect();
        pending.bytes -= self.bytes;
        pending.claimed = false;
        self.committed = true;
        drop(pending);
        drop(removed);
        Ok(())
    }
}

impl Drop for Delivery {
    fn drop(&mut self) {
        if !self.committed {
            match self.queue.lock() {
                Ok(mut pending) => pending.claimed = false,
                Err(_) => {
                    tracing::warn!("injection queue poisoned; reservation cannot be released")
                }
            }
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
    use agenthooksprotocol::{CanonicalMessage, CanonicalMessages, InjectEffect, TextBodyPart};

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
        let _owner_a = queues.open("a").unwrap();
        queues
            .producer("a")
            .unwrap()
            .enqueue(&outcome("first"))
            .unwrap();
        let delivery = queues.claim("a").unwrap();
        queues
            .producer("a")
            .unwrap()
            .enqueue(&outcome("second"))
            .unwrap();
        assert!(queues.claim("a").is_err());
        drop(delivery);
        let delivery = queues.claim("a").unwrap();
        assert_eq!(texts(&delivery), ["first", "second"]);
        delivery.commit().unwrap();
        assert!(texts(&queues.claim("a").unwrap()).is_empty());
    }

    #[test]
    fn commit_only_consumes_claimed_prefix() {
        let queues = Injections::default();
        let _owner_a = queues.open("a").unwrap();
        queues
            .producer("a")
            .unwrap()
            .enqueue(&outcome("first"))
            .unwrap();
        let delivery = queues.claim("a").unwrap();
        queues
            .producer("a")
            .unwrap()
            .enqueue(&outcome("second"))
            .unwrap();
        delivery.commit().unwrap();
        assert_eq!(texts(&queues.claim("a").unwrap()), ["second"]);
    }

    #[test]
    fn isolated_sessions_and_stale_receipts() {
        let queues = Injections::default();
        let _owner_a = queues.open("a").unwrap();
        let _owner_b = queues.open("b").unwrap();
        queues
            .producer("a")
            .unwrap()
            .enqueue(&outcome("old"))
            .unwrap();
        let old = queues.claim("a").unwrap();
        queues.close("a").unwrap();
        assert!(queues.open("a").is_err());
        assert!(
            queues
                .producer("a")
                .unwrap()
                .enqueue(&outcome("late"))
                .is_err()
        );
        drop(_owner_a);
        let _owner_a = queues.open("a").unwrap();
        queues
            .producer("a")
            .unwrap()
            .enqueue(&outcome("new"))
            .unwrap();
        old.commit().unwrap();
        assert_eq!(texts(&queues.claim("a").unwrap()), ["new"]);
        assert!(texts(&queues.claim("b").unwrap()).is_empty());
    }

    #[test]
    fn stale_producer_cannot_inject_into_resumed_session() {
        let queues = Injections::default();
        let owner = queues.open("a").unwrap();
        let producer = queues.producer("a").unwrap();
        producer.enqueue(&outcome("retired payload")).unwrap();
        assert!(queues.open("a").is_err());
        drop(owner);
        let _resumed = queues.open("a").unwrap();
        assert!(producer.enqueue(&outcome("stale")).is_err());
        assert!(producer.queue.lock().unwrap().items.is_empty());
        assert!(texts(&queues.claim("a").unwrap()).is_empty());
    }

    #[tokio::test]
    async fn cancelled_session_start_releases_ownership() {
        let queues = Arc::new(Injections::default());
        let (ready, received) = tokio::sync::oneshot::channel();
        let task = tokio::spawn({
            let queues = queues.clone();
            async move {
                let _owner = queues.open("a").unwrap();
                ready.send(()).unwrap();
                std::future::pending::<()>().await;
            }
        });
        received.await.unwrap();
        assert!(queues.open("a").is_err());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let _resumed = queues.open("a").unwrap();
    }

    #[test]
    fn invalid_batch_does_not_partially_enqueue() {
        let queues = Injections::default();
        let _owner_a = queues.open("a").unwrap();
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
        queues
            .producer("a")
            .unwrap()
            .enqueue(&outcome("still works"))
            .unwrap();
    }

    #[tokio::test]
    async fn cancellation_releases_reservation_without_losing_context() {
        let queues = Arc::new(Injections::default());
        let _owner_a = queues.open("a").unwrap();
        queues
            .producer("a")
            .unwrap()
            .enqueue(&outcome("retained"))
            .unwrap();
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

    #[test]
    fn poison_is_reported_not_recovered() {
        let queues = Injections::default();
        let _owner_a = queues.open("a").unwrap();
        let _owner_b = queues.open("b").unwrap();
        let queue = queues.queue("a").unwrap();
        let _ = std::panic::catch_unwind(|| {
            let _guard = queue.lock().unwrap();
            panic!("unwind while holding queue");
        });
        assert!(
            queues
                .producer("a")
                .unwrap()
                .enqueue(&outcome("rejected"))
                .is_err()
        );
        assert!(queues.claim("a").is_err());
        queues
            .producer("b")
            .unwrap()
            .enqueue(&outcome("unaffected"))
            .unwrap();
        assert_eq!(texts(&queues.claim("b").unwrap()), ["unaffected"]);
        assert!(queues.close("a").is_err());
        drop(_owner_a);
        let _fresh_owner = queues.open("a").unwrap();
    }
}
