//! A child agent's hook sign-in notices, relayed to the user of the parent
//! session that owns the agent. The child's hook waits for its user to sign
//! in, so the parent delivers the link the way it delivers its own: to that
//! session's user only, never into child output, inspection or a model.
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, Mutex, Weak},
};

/// One relayed notice: the child agent, the child's notice text and its
/// sign-in `_meta` (kept, so a Kit above the parent drops it from its model
/// too).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChildHint {
    pub(crate) agent: String,
    pub(crate) text: String,
    pub(crate) meta: serde_json::Map<String, serde_json::Value>,
}

type Sink = dyn Fn(&ChildHint) + Send + Sync;

/// Parent session id to the sinks that deliver to its user.
static SINKS: LazyLock<Mutex<HashMap<String, Vec<Weak<Sink>>>>> = LazyLock::new(Mutex::default);

/// Keeps a session's relay registered; dropping it stops delivery.
pub(crate) struct ChildHintSubscription {
    _sink: Arc<Sink>,
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
        .entry(session)
        .or_default()
        .push(Arc::downgrade(&sink));
    ChildHintSubscription { _sink: sink }
}

/// Relays `hint` to the user of `session`, if it is attached; otherwise it
/// is dropped (the child's next turn shows its link again).
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
