//! User-facing hook messages, separate from model context and persisted transcripts.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
};

use agenthooksprotocol::generated::MessageEffect;

type Sink = dyn Fn(&str) -> bool + Send + Sync;

#[derive(Default)]
struct Routes(Mutex<HashMap<String, Vec<Weak<Sink>>>>);

impl Routes {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Vec<Weak<Sink>>>> {
        // Writers only insert/remove complete weak references. No callbacks,
        // strong sink destruction, awaits, or external commits occur under the
        // guard. Each entry remains valid even if a collection operation unwinds.
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn targets(&self, session: &str) -> Vec<Weak<Sink>> {
        self.lock().get(session).cloned().unwrap_or_default()
    }
}

fn routes() -> &'static Routes {
    static ROUTES: OnceLock<Routes> = OnceLock::new();
    ROUTES.get_or_init(Routes::default)
}

/// Keep this registration alive with the session observer, including startup
/// and shutdown hooks. Cloned observers share a registration via `Arc`.
pub(crate) struct Subscription {
    session: String,
    sink: Arc<Sink>,
}

impl Subscription {
    pub(crate) fn new(
        session: String,
        sink: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Arc<Self> {
        let sink: Arc<Sink> = Arc::new(sink);
        routes()
            .lock()
            .entry(session.clone())
            .or_default()
            .push(Arc::downgrade(&sink));
        Arc::new(Self { session, sink })
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut routes = routes().lock();
        if let Some(sinks) = routes.get_mut(&self.session) {
            sinks.retain(|sink| !sink.ptr_eq(&Arc::downgrade(&self.sink)));
            if sinks.is_empty() {
                routes.remove(&self.session);
            }
        }
        // The strong sink field is dropped after the guard, allowing reentry.
    }
}

/// The presentation route captured before hook dispatch. Weak targets must not
/// extend a retired observer's lifetime or resolve a later session attachment.
#[derive(Clone)]
pub(super) struct Destination {
    targets: Vec<Weak<Sink>>,
}

/// Capture before awaiting the hook backend, using the originating session ID.
/// An empty snapshot belongs to CLI; an expired ACP snapshot remains ACP.
pub(super) fn capture(session: &str) -> Destination {
    Destination {
        targets: routes().targets(session),
    }
}

impl Destination {
    /// Deliver only settled effects, once, including outcomes with deny effects.
    /// Retired ACP destinations are discarded, never redirected to a new
    /// observer or to process stderr. Failed ACP sends are likewise not retried.
    pub(super) fn deliver(&self, messages: &[MessageEffect]) {
        if self.targets.is_empty() {
            for message in messages {
                eprintln!("kit hook: {}", message.text);
            }
            return;
        }
        for message in messages {
            for target in &self.targets {
                if let Some(sink) = target.upgrade() {
                    sink(&message.text);
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_are_session_scoped_and_removed_independently() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let first = Subscription::new("message-routing-test".into(), move |text| {
            sender.send(text.to_owned()).is_ok()
        });
        let second = Subscription::new("message-routing-test".into(), |_| false);
        assert!(routes().targets("another-session").is_empty());
        assert_eq!(routes().targets("message-routing-test").len(), 2);
        capture("message-routing-test").deliver(&[MessageEffect::new("hello")]);
        assert_eq!(receiver.recv().unwrap(), "hello");
        drop(first);
        assert_eq!(routes().targets("message-routing-test").len(), 1);
        drop(second);
        assert!(routes().targets("message-routing-test").is_empty());
    }
    #[test]
    fn callback_reentry_and_unwind_leave_registry_usable() {
        let subscription = Subscription::new("message-unwind-test".into(), |_| {
            let nested = Subscription::new("message-reentry-test".into(), |_| true);
            drop(nested);
            panic!("external sink failed");
        });
        let destination = capture("message-unwind-test");
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                destination.deliver(&[MessageEffect::new("hello")]);
            }))
            .is_err()
        );
        drop(subscription);
        assert!(routes().targets("message-unwind-test").is_empty());
        assert!(routes().targets("message-reentry-test").is_empty());
    }
    #[test]
    fn captured_destination_never_reaches_reattached_session() {
        let session = "message-reattach-test";
        let (old_sender, old_receiver) = std::sync::mpsc::channel();
        let old = Subscription::new(session.into(), move |text| {
            old_sender.send(text.to_owned()).is_ok()
        });
        let stale = capture(session);
        drop(old);
        // Capturing a destination does not retain the old callback or sender.
        assert_eq!(
            old_receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        );
        assert!(!stale.targets.is_empty());
        assert!(
            stale
                .targets
                .iter()
                .all(|target| target.upgrade().is_none())
        );

        let (sender, receiver) = std::sync::mpsc::channel();
        let _current = Subscription::new(session.into(), move |text| {
            sender.send(text.to_owned()).is_ok()
        });
        stale.deliver(&[MessageEffect::new("old result")]);
        assert_eq!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );
        capture(session).deliver(&[MessageEffect::new("current result")]);
        assert_eq!(receiver.recv().unwrap(), "current result");
    }
}
