//! User-facing hook messages, separate from model context and persisted transcripts.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, Weak},
};

use agenthooksprotocol::generated::MessageEffect;

type Sink = dyn Fn(&str) -> bool + Send + Sync;
type Subscribers = HashMap<String, Vec<Weak<Sink>>>;

/// Owned by one Hooks instance; subscriptions retain only this routing state.
#[derive(Clone, Default)]
pub(super) struct Routes(Arc<Mutex<Subscribers>>);

impl Routes {
    fn lock(&self) -> MutexGuard<'_, Subscribers> {
        // Writers only insert/remove complete weak references. No callbacks,
        // strong sink destruction, awaits, or external commits occur under the
        // guard. Each entry remains valid even if a collection operation unwinds.
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn subscribe(
        &self,
        session: String,
        sink: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Arc<Subscription> {
        let sink: Arc<Sink> = Arc::new(sink);
        self.lock()
            .entry(session.clone())
            .or_default()
            .push(Arc::downgrade(&sink));
        Arc::new(Subscription {
            routes: self.clone(),
            session,
            sink,
        })
    }

    /// Informational notices use the currently attached session, not an execution
    /// generation. Resolve before callbacks so no callback runs under the lock.
    fn deliver(&self, session: &str, messages: &[MessageEffect]) {
        let targets = self.lock().get(session).cloned().unwrap_or_default();
        let sinks: Vec<_> = targets.iter().filter_map(Weak::upgrade).collect();
        for message in messages {
            if sinks.is_empty() {
                eprintln!("kit hook: {}", message.text);
            } else {
                for sink in &sinks {
                    sink(&message.text);
                }
            }
        }
    }
}

impl super::Hooks {
    pub(crate) fn subscribe_messages(
        &self,
        session: String,
        sink: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Arc<Subscription> {
        self.messages.subscribe(session, sink)
    }

    /// Deliver only settled message effects, including outcomes with deny effects.
    pub(super) fn deliver_messages(&self, session: &str, messages: &[MessageEffect]) {
        self.messages.deliver(session, messages);
    }
}

/// Keep alive with the session observer, including startup and shutdown hooks.
pub(crate) struct Subscription {
    routes: Routes,
    session: String,
    sink: Arc<Sink>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let mut routes = self.routes.lock();
        if let Some(sinks) = routes.get_mut(&self.session) {
            sinks.retain(|sink| !sink.ptr_eq(&Arc::downgrade(&self.sink)));
            if sinks.is_empty() {
                routes.remove(&self.session);
            }
        }
        // The strong sink field is dropped after the guard, allowing reentry.
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn subscriptions_are_session_scoped_and_removed_independently() {
        let routes = Routes::default();
        let (sender, receiver) = std::sync::mpsc::channel();
        let first = routes.subscribe("session".into(), move |text| {
            sender.send(text.to_owned()).is_ok()
        });
        let second = routes.subscribe("session".into(), |_| false);
        assert!(!routes.lock().contains_key("another-session"));
        assert_eq!(routes.lock()["session"].len(), 2);
        routes.deliver("session", &[MessageEffect::new("hello")]);
        assert_eq!(receiver.recv().unwrap(), "hello");
        drop(first);
        assert_eq!(routes.lock()["session"].len(), 1);
        drop(second);
        assert!(!routes.lock().contains_key("session"));
    }

    #[test]
    fn callback_reentry_and_unwind_leave_registry_usable() {
        let routes = Routes::default();
        let nested_routes = routes.clone();
        let subscription = routes.subscribe("session".into(), move |_| {
            let nested = nested_routes.subscribe("nested".into(), |_| true);
            drop(nested);
            panic!("external sink failed");
        });
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                routes.deliver("session", &[MessageEffect::new("hello")]);
            }))
            .is_err()
        );
        drop(subscription);
        assert!(routes.lock().is_empty());
    }

    #[test]
    fn routes_are_isolated_between_hooks_instances() {
        let first = Routes::default();
        let second = Routes::default();
        let (sender, receiver) = std::sync::mpsc::channel();
        let _subscription = first.subscribe("same-session".into(), move |text| {
            sender.send(text.to_owned()).is_ok()
        });
        let _other = second.subscribe("same-session".into(), |_| true);
        second.deliver("same-session", &[MessageEffect::new("other runtime")]);
        assert_eq!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );
        first.deliver("same-session", &[MessageEffect::new("own runtime")]);
        assert_eq!(receiver.recv().unwrap(), "own runtime");
    }
}
