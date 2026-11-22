//! A child agent's hook sign-in hints, relayed to the user of the parent
//! session that owns the agent. The child's hook waits for its user to sign
//! in, so the parent delivers the link the way it delivers its own: to that
//! session's user only, never into child output, inspection or a model.
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex, Weak},
};

/// A child's hint, its `agent` the path from the owning session.
pub(crate) use super::hint::HookHint as ChildHint;

type Sink = dyn Fn(&ChildHint) + Send + Sync;

/// Parent session id to the sinks that deliver to its user.
static SINKS: LazyLock<Mutex<HashMap<String, Vec<Weak<Sink>>>>> = LazyLock::new(Mutex::default);

/// Keeps a session's relay registered; dropping it (when the session's
/// actor ends) stops delivery and removes the session's entry.
pub(crate) struct ChildHintSubscription {
    session: String,
    sink: Arc<Sink>,
}

impl Drop for ChildHintSubscription {
    fn drop(&mut self) {
        let mut sinks = SINKS.lock().unwrap_or_else(|poison| poison.into_inner());
        let ours = Arc::downgrade(&self.sink);
        if let Some(entries) = sinks.get_mut(&self.session) {
            entries.retain(|entry| entry.strong_count() != 0 && !entry.ptr_eq(&ours));
            if entries.is_empty() {
                sinks.remove(&self.session);
            }
        }
    }
}

/// Delivers child hints for `session` to `sink`, from now on.
pub(crate) fn subscribe(
    session: String,
    sink: impl Fn(&ChildHint) + Send + Sync + 'static,
) -> ChildHintSubscription {
    let sink: Arc<Sink> = Arc::new(sink);
    let mut sinks = SINKS.lock().unwrap_or_else(|poison| poison.into_inner());
    sinks.retain(|_, entries| {
        entries.retain(|entry| entry.strong_count() != 0);
        !entries.is_empty()
    });
    sinks
        .entry(session.clone())
        .or_default()
        .push(Arc::downgrade(&sink));
    ChildHintSubscription { session, sink }
}

/// Relays `hint` to the user of `session`, if it is attached; otherwise it
/// is dropped. (A child session's hints are held until it has an owning
/// agent, not until a parent session attaches: a session attached later
/// sees a link only when the child sends it again, or the next one.)
pub(crate) fn relay(session: &str, hint: &ChildHint) {
    let sinks: Vec<_> = SINKS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(session)
        .into_iter()
        .flatten()
        .filter_map(Weak::upgrade)
        .collect();
    // Outside the lock: a sink may block on its connection.
    for sink in sinks {
        sink(hint);
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;

    #[test]
    fn a_session_relay_is_removed_when_its_actor_ends() {
        let subscription = subscribe("test-relay-prune".into(), |_| {});
        assert!(SINKS.lock().unwrap().contains_key("test-relay-prune"));
        drop(subscription);
        assert!(!SINKS.lock().unwrap().contains_key("test-relay-prune"));
    }
}
