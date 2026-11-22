//! Best-effort, bounded ACP child multiplexing. Disk inspection is independent.
//! `State` has one lock: subscribe, announce, capabilities and forward write it.
//! They publish complete owned JSON values and never await under the guard.
//! Broadcast wakeups occur only after a complete state commit; a panic poisons
//! and isolates streaming instead of recovering uncertain
//! parent-before-child ordering. IDs and ownership survive prompt generations.
use agent_client_protocol::schema::v1::{self as acp, SessionUpdate};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
};
use tokio::sync::broadcast;

const QUEUE: usize = 2048;
const FRAME_BYTES: usize = 64 * 1024;
const NODES: usize = 1024;
const HISTORY: usize = 512;
const HISTORY_BYTES: usize = 4 * 1024 * 1024;
const SUBSCRIBERS: usize = 16;
type Event = (String, Value);

#[derive(Default)]
pub(crate) struct Stream {
    state: Mutex<State>,
}
#[derive(Default)]
struct State {
    parent: Option<String>,
    // In insertion order, so reconnect snapshots always announce ancestors first.
    nodes: Vec<Event>,
    positions: HashMap<String, usize>,
    subscribers: Vec<broadcast::Sender<Event>>,
    history: VecDeque<(Event, usize)>,
    history_bytes: usize,
    steering: HashMap<String, Steering>,
}
impl Stream {
    /// Records `parent` as the session that owns these subagents, without
    /// subscribing to their updates: their hook sign-in links are relayed to
    /// it whatever its client's capabilities.
    pub(crate) fn own(&self, parent: String) {
        if let Ok(mut state) = self.state.lock()
            && state.parent.is_none()
        {
            state.parent = Some(parent);
        }
    }

    pub(crate) fn subscribe(
        &self,
        parent: String,
        replay_history: bool,
    ) -> broadcast::Receiver<Event> {
        let (sender, receiver) = broadcast::channel(QUEUE);
        let Ok(mut state) = self.state.lock() else {
            return receiver;
        };
        if state
            .parent
            .as_ref()
            .is_some_and(|current| current != &parent)
        {
            return receiver;
        }
        state
            .subscribers
            .retain(|subscriber| subscriber.receiver_count() != 0);
        if state.subscribers.len() == SUBSCRIBERS {
            return receiver;
        }
        state.parent = Some(parent.clone());
        // A private queue avoids replaying append-only chunks to existing live
        // subscribers. Ancestors precede all retained content; the bounded tail
        // preserves its original cross-session order. Snapshot + tail fits QUEUE.
        // This is a partial tail: ACP allows chunks before identity, including
        // when the original message upsert has fallen outside the replay budget.
        for (owner, update) in &state.nodes {
            // Ownership is needed even for no-history resumes. Cached grants
            // are not fresh authorizations on a new subscription.
            let mut update = update.clone();
            revoke_grants(&mut update);
            let _ = sender.send((
                if owner.is_empty() {
                    parent.clone()
                } else {
                    owner.clone()
                },
                update.clone(),
            ));
        }
        if replay_history {
            for ((owner, update), _) in &state.history {
                let _ = sender.send((
                    if owner.is_empty() {
                        parent.clone()
                    } else {
                        owner.clone()
                    },
                    update.clone(),
                ));
            }
        }
        state.subscribers.push(sender);
        receiver
    }

    pub(crate) fn announce(
        &self,
        id: &str,
        name: &str,
        task: &str,
        status: crate::events::SubagentStatus,
        generation: u64,
        runtime: &crate::events::SubagentRuntime,
    ) {
        let activity = match status {
            crate::events::SubagentStatus::Starting | crate::events::SubagentStatus::Working => {
                acp::StateUpdate::Running(acp::RunningStateUpdate::new())
            }
            crate::events::SubagentStatus::Idle => {
                acp::StateUpdate::Idle(acp::IdleStateUpdate::new())
            }
            crate::events::SubagentStatus::Removed => {
                acp::StateUpdate::Unknown(acp::UnknownStateUpdate::new())
            }
        };
        let update = SessionUpdate::SubagentUpdate(
            acp::SubagentUpdate::new(id.to_owned())
                .title(name.to_owned())
                .description(task.chars().take(4096).collect::<String>())
                .state(activity),
        );
        let Ok(mut update) = serde_json::to_value(update) else {
            return;
        };
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state
            .steering
            .get(id)
            .is_some_and(|prior| prior.generation > generation)
        {
            return;
        }
        if !state.positions.contains_key(id) && state.nodes.len() == NODES {
            return;
        }
        let supported = state
            .steering
            .get(id)
            .is_some_and(|prior| prior.generation == generation && prior.supported);
        let steering = Steering {
            generation,
            supported,
            active: matches!(status, crate::events::SubagentStatus::Working),
        };
        update["_meta"] = steering.meta();
        if let Ok(runtime) = serde_json::to_value(runtime) {
            update["_meta"][crate::events::SUBAGENT_RUNTIME_META] = runtime;
        }
        state.steering.insert(id.to_owned(), steering);
        Self::publish(&mut state, "", update);
    }

    pub(crate) fn capabilities(&self, id: &str, generation: u64, supported: bool) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(steering) = state.steering.get_mut(id) else {
            return;
        };
        if steering.generation != generation {
            return;
        }
        steering.supported = supported;
        let mut update = match serde_json::to_value(SessionUpdate::SubagentUpdate(
            acp::SubagentUpdate::new(id.to_owned()),
        )) {
            Ok(update) => update,
            Err(_) => return,
        };
        update["_meta"] = steering.meta();
        Self::publish(&mut state, "", update);
    }

    pub(crate) fn parent(&self, owner: &str) -> Option<String> {
        let state = self.state.lock().ok()?;
        let position = *state.positions.get(owner)?;
        let parent = &state.nodes[position].0;
        if parent.is_empty() {
            state.parent.clone()
        } else {
            Some(parent.clone())
        }
    }

    pub(crate) fn directed(&self, owner: &str, update: Value) {
        self.forward(owner, update.clone());
        let parent = {
            let Ok(state) = self.state.lock() else { return };
            let Some(&position) = state.positions.get(owner) else {
                return;
            };
            state.nodes[position].0.clone()
        };
        self.forward(&parent, update);
    }

    pub(crate) fn forward(&self, owner: &str, update: Value) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        Self::publish(&mut state, owner, update);
    }

    fn publish(state: &mut State, owner: &str, update: Value) {
        // Bound frame admission without allocating a serialized duplicate.
        let mut limit = Limit(FRAME_BYTES);
        if serde_json::to_writer(&mut limit, &update).is_err() {
            return;
        }
        let size = FRAME_BYTES - limit.0 + owner.len();
        if !owner.is_empty() && !state.positions.contains_key(owner) {
            return;
        }
        if update["sessionUpdate"] == "subagent_update" {
            let Some(id) = update["sessionId"].as_str() else {
                return;
            };
            if let Some(&position) = state.positions.get(id) {
                if state.nodes[position].0 != owner {
                    return;
                }
                if let (Some(current), Some(patch)) =
                    (state.nodes[position].1.as_object_mut(), update.as_object())
                {
                    let mut merged = current.clone();
                    patch_update(&mut merged, patch);
                    if serde_json::to_writer(Limit(FRAME_BYTES), &merged).is_err() {
                        return;
                    }
                    *current = merged;
                }
            } else {
                if state.nodes.len() == NODES {
                    return;
                }
                let position = state.nodes.len();
                state.positions.insert(id.to_owned(), position);
                state.nodes.push((owner.to_owned(), update.clone()));
            }
        }
        if update["sessionUpdate"] == "state_update"
            && let Some(&position) = state.positions.get(owner)
            && let Some(object) = update.as_object()
        {
            let mut activity = object.clone();
            activity.remove("sessionUpdate");
            let mut snapshot = state.nodes[position].1.clone();
            snapshot["state"] = Value::Object(activity);
            if let Some(steering) = state.steering.get_mut(owner) {
                if update["state"] != "running" {
                    steering.active = false;
                }
                if let Some(snapshot) = snapshot.as_object_mut() {
                    patch_update(
                        snapshot,
                        &serde_json::Map::from_iter([("_meta".into(), steering.meta())]),
                    );
                }
            }
            if serde_json::to_writer(Limit(FRAME_BYTES), &snapshot).is_ok() {
                // V1 has no child-addressed state_update. Publish a minimal
                // parent-addressed association patch as well as retaining it,
                // before the raw v2 update. This is a known ownership edge.
                let association_owner = state.nodes[position].0.clone();
                let mut patch = match serde_json::to_value(SessionUpdate::SubagentUpdate(
                    acp::SubagentUpdate::new(owner.to_owned()),
                )) {
                    Ok(patch) => patch,
                    Err(_) => return,
                };
                patch["state"] = snapshot["state"].clone();
                if let Some(steering) = state.steering.get(owner) {
                    patch["_meta"] = steering.meta();
                }
                state.nodes[position].1 = snapshot;
                Self::publish(state, &association_owner, patch);
            }
        }
        // State snapshots are replayed from `nodes`; replaying an older
        // state_update after that snapshot would resurrect completed work.
        if !matches!(
            update["sessionUpdate"].as_str(),
            Some("subagent_update" | "state_update")
        ) {
            while state.history.len() >= HISTORY || state.history_bytes + size > HISTORY_BYTES {
                let Some((_, removed)) = state.history.pop_front() else {
                    break;
                };
                state.history_bytes -= removed;
            }
            state
                .history
                .push_back(((owner.to_owned(), update.clone()), size));
            state.history_bytes += size;
        }
        let Some(parent) = &state.parent else { return };
        let event = (
            if owner.is_empty() {
                parent.clone()
            } else {
                owner.to_owned()
            },
            update,
        );
        state
            .subscribers
            .retain(|subscriber| subscriber.send(event.clone()).is_ok());
    }
}
/// `_meta` patches keys, not the whole object. Null values remove individual
/// keys; a null `_meta` clears the object. Other update fields replace wholesale.
fn patch_update(
    current: &mut serde_json::Map<String, Value>,
    patch: &serde_json::Map<String, Value>,
) {
    for (key, value) in patch {
        if key != "_meta" || value.is_null() {
            current.insert(key.clone(), value.clone());
        } else if let Some(keys) = value.as_object() {
            let mut meta = current
                .get("_meta")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            for (key, value) in keys {
                if value.is_null() {
                    meta.remove(key);
                } else {
                    meta.insert(key.clone(), value.clone());
                }
            }
            current.insert("_meta".into(), Value::Object(meta));
        }
    }
}

/// Replayed metadata and descendants observed through another harness do not
/// authorize this server's direct-child mutation endpoints. Live local grants
/// come only from the active generation's capability report.
fn revoke_grants(update: &mut Value) {
    if update.get("capabilities").is_some() {
        update["capabilities"] = Value::Null;
    }
    if let Some(grant) = update
        .get_mut("_meta")
        .and_then(Value::as_object_mut)
        .and_then(|meta| meta.get_mut("kit/subagent/steer"))
    {
        if let Some(grant) = grant.as_object_mut() {
            grant.insert("canSteer".into(), Value::Bool(false));
        } else {
            *grant = Value::Null;
        }
    }
}

struct Steering {
    generation: u64,
    supported: bool,
    active: bool,
}
impl Steering {
    fn meta(&self) -> Value {
        Value::Object(serde_json::Map::from_iter([(
            "kit/subagent/steer".into(),
            Value::Object(serde_json::Map::from_iter([
                ("generation".into(), Value::from(self.generation)),
                (
                    "canSteer".into(),
                    Value::Bool(self.active && self.supported),
                ),
            ])),
        )]))
    }
}

struct Limit(usize);
impl std::io::Write for Limit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.0 {
            return Err(std::io::Error::other("stream frame exceeds budget"));
        }
        self.0 -= bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Connection-local remote IDs are never exposed directly: independent harnesses
/// can reuse them. Resume staging, registration and notifications are the writers.
/// Lock order is Routes -> Stream; there is no reverse edge or await. Poison
/// isolates this connection's stream. A retained route carries no execution gate.
#[derive(Default)]
pub(crate) struct Routes(Mutex<RoutesState>);
#[derive(Default)]
struct RoutesState {
    live: HashMap<String, Route>,
    resume: Option<Resume>,
    /// Hook sign-in hints from child sessions not yet routed to an owner, or
    /// arriving while a registration replays: the latest per (session,
    /// originating agent and backend), bounded.
    held: Vec<(String, String, Hint)>,
    /// Sessions whose held hints are being replayed: hints that arrive
    /// meanwhile are held too, so none overtakes an older one.
    replaying: std::collections::HashSet<String>,
    /// Sessions that had a hint dropped at a bound before they had an
    /// owner: once they do, it is told more backends need a sign-in.
    overflowed: std::collections::HashSet<String>,
    /// Sessions removed here: a late hint from one is ignored, though its
    /// remote close may not have settled.
    ended: std::collections::HashSet<String>,
    /// The last hint passed on per (session, agent and backend): an identical
    /// one right after it (the same hint in the other form, say) is dropped.
    last: HashMap<(String, String), Hint>,
}

/// The most (session, agent and backend) pairs whose last hint is kept.
const LAST_HINTS: usize = 256;

type Hint = crate::ahp::auth::hint::HookHint;

/// A replay in progress: dropping it ends the replay.
struct Replay<'a> {
    routes: &'a Routes,
    remote: &'a str,
}

impl Drop for Replay<'_> {
    fn drop(&mut self) {
        let mut state = self
            .routes
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        state.replaying.remove(self.remote);
    }
}

/// A held hint's key within its session: the agent that originated it
/// (relayed from further down the tree) and its backend.
fn hint_key(hint: &Hint) -> String {
    format!("{}\n{}", hint.agent.as_deref().unwrap_or(""), hint.backend)
}

/// Whether a hint's link has expired: it is not replayed.
fn expired(hint: &Hint) -> bool {
    !hint.cleared
        && hint
            .expires_at
            .is_some_and(|at| at <= crate::events::now_millis())
}

/// The most sessions remembered as overflowed or ended.
const REMEMBERED_SESSIONS: usize = 256;

/// The most hook sign-in hints held for one unrouted child session (a few
/// backends); each session has its own, so none evicts another's.
const HELD_HINTS: usize = 4;
/// The most hook sign-in hints held for one child connection in all. When
/// full a new hint is dropped: session ids come from the child, so one that
/// invents them can fill this only for itself (`Routes` is per child), and
/// a later turn shows a dropped link again.
const HELD_TOTAL: usize = 64;
/// Resume has no local transcript owner yet. Keep associations independently of
/// the bounded content tail so eviction cannot orphan future descendant traffic.
struct Resume {
    root: String,
    associations: Vec<Event>,
    positions: HashMap<String, usize>,
    history: VecDeque<(Event, usize)>,
    bytes: usize,
}
impl Resume {
    fn record(&mut self, remote: &str, update: Value) {
        if remote != self.root && !self.positions.contains_key(remote) {
            return;
        }
        // The root's new prompt lifecycle is authoritative at attachment time.
        // A replayed idle must not revoke that generation's fresh capability grant.
        if remote == self.root && update["sessionUpdate"] == "state_update" {
            return;
        }
        let mut limit = Limit(FRAME_BYTES);
        if serde_json::to_writer(&mut limit, &(remote, &update)).is_err() {
            return;
        }
        let size = FRAME_BYTES - limit.0;
        if update["sessionUpdate"] == "subagent_update" {
            let Some(id) = update["sessionId"].as_str() else {
                return;
            };
            if let Some(&position) = self.positions.get(id) {
                if self.associations[position].0 != remote {
                    return;
                }
                if let (Some(current), Some(patch)) = (
                    self.associations[position].1.as_object(),
                    update.as_object(),
                ) {
                    let mut merged = current.clone();
                    patch_update(&mut merged, patch);
                    if serde_json::to_writer(Limit(FRAME_BYTES), &merged).is_ok() {
                        self.associations[position].1 = Value::Object(merged);
                    }
                }
            } else if id != self.root && self.associations.len() < NODES - 1 {
                self.positions
                    .insert(id.to_owned(), self.associations.len());
                self.associations.push((remote.to_owned(), update));
            }
            return;
        }
        if update["sessionUpdate"] == "state_update" {
            if let Some(&position) = self.positions.get(remote)
                && let Some(object) = update.as_object()
            {
                let mut activity = object.clone();
                activity.remove("sessionUpdate");
                let mut snapshot = self.associations[position].1.clone();
                snapshot["state"] = Value::Object(activity);
                if serde_json::to_writer(Limit(FRAME_BYTES), &snapshot).is_ok() {
                    self.associations[position].1 = snapshot;
                }
            }
            // Current state belongs in the association snapshot, never the FIFO
            // tail: eviction must not lose idle, nor replay an older transition
            // after a newer parent-provided state snapshot.
            return;
        }
        while self.history.len() >= HISTORY || self.bytes + size > HISTORY_BYTES {
            let Some((_, removed)) = self.history.pop_front() else {
                break;
            };
            self.bytes -= removed;
        }
        self.history.push_back(((remote.to_owned(), update), size));
        self.bytes += size;
    }
}
#[derive(Clone)]
struct Route {
    owner: String,
    generation: u64,
    transcript: super::transcript::Transcript,
    segment: Option<String>,
    sequence: u64,
    announced_message: Option<String>,
}
impl Routes {
    pub(crate) fn begin_resume(&self, remote: String) {
        let Ok(mut state) = self.0.lock() else { return };
        if state.resume.is_none() && !state.live.contains_key(&remote) {
            state.resume = Some(Resume {
                root: remote,
                associations: Vec::new(),
                positions: HashMap::new(),
                history: VecDeque::new(),
                bytes: 0,
            });
        }
    }

    /// Registers `remote`'s owner, then hands `deliver` the hook sign-in
    /// hints held for it, in order, outside the lock. Hints that arrive
    /// while they are delivered queue behind them; the replay ends (and new
    /// hints go straight to the owner) in the same critical section that
    /// finds nothing more held, so no caller can leave it unfinished.
    pub(crate) fn register(
        &self,
        remote: &str,
        owner: &str,
        generation: u64,
        transcript: super::transcript::Transcript,
        mut deliver: impl FnMut(&Hint),
    ) {
        let (mut held, overflowed) = self.register_locked(remote, owner, generation, transcript);
        if held.is_empty() && !overflowed {
            return;
        }
        // Ends the replay however this returns, a panic in `deliver` too, so
        // the session's later hints are never held for good.
        let _replay = Replay {
            routes: self,
            remote,
        };
        if overflowed {
            // What could not be held: the user is told where to look.
            deliver(&crate::ahp::auth::hint::more_backends());
        }
        while !held.is_empty() {
            for update in &held {
                deliver(update);
            }
            let Ok(mut state) = self.0.lock() else {
                return;
            };
            held = Self::take_held(&mut state, remote);
            if held.is_empty() {
                state.replaying.remove(remote);
            }
        }
    }

    fn register_locked(
        &self,
        remote: &str,
        owner: &str,
        generation: u64,
        transcript: super::transcript::Transcript,
    ) -> (Vec<Hint>, bool) {
        let Ok(mut state) = self.0.lock() else {
            return (Vec::new(), false);
        };
        state.ended.remove(remote);
        let routes = &mut state.live;
        if routes.len() < NODES || routes.contains_key(remote) {
            routes.insert(
                remote.to_owned(),
                Route {
                    owner: owner.to_owned(),
                    generation,
                    transcript,
                    segment: None,
                    sequence: 0,
                    announced_message: None,
                },
            );
        }
        if state
            .resume
            .as_ref()
            .is_some_and(|resume| resume.root == remote)
            && let Some(resume) = state.resume.take()
        {
            for (remote, update) in resume.associations {
                Self::forward_registered(&mut state.live, &remote, update);
            }
            for ((remote, update), _) in resume.history {
                Self::forward_registered(&mut state.live, &remote, update);
            }
        }
        if !state.live.contains_key(remote) {
            return (Vec::new(), false);
        }
        let held = Self::take_held(&mut state, remote);
        // Only while there is something to replay do new hints queue behind.
        if !held.is_empty() {
            state.replaying.insert(remote.to_owned());
        }
        let overflowed = state.overflowed.remove(remote);
        (held, overflowed)
    }

    fn take_held(state: &mut RoutesState, remote: &str) -> Vec<Hint> {
        let mut held = Vec::new();
        state.held.retain(|(session, _, update)| {
            let mine = session == remote;
            if mine && !expired(update) {
                held.push(update.clone());
            }
            !mine
        });
        // Replayed hints count as passed on, for the duplicate check.
        for hint in &held {
            let key = (remote.to_owned(), hint_key(hint));
            if state.last.len() < LAST_HINTS || state.last.contains_key(&key) {
                state.last.insert(key, hint.clone());
            }
        }
        held
    }

    /// Where a child session's hook sign-in hint goes: its owner and
    /// transcript, for as long as the session lives (not only while a prompt
    /// runs). A session with no owner yet keeps its latest hint per backend,
    /// replayed by [`Self::register`]; a clear replaces the link it clears.
    pub(crate) fn hook_hint(
        &self,
        remote: &str,
        update: Hint,
    ) -> Option<(String, super::transcript::Transcript, Hint)> {
        let Ok(mut state) = self.0.lock() else {
            return None;
        };
        // A removed session's late hint never brings its link back.
        if state.ended.contains(remote) {
            return None;
        }
        if !state.replaying.contains(remote)
            && let Some(route) = state.live.get(remote)
        {
            let route = (route.owner.clone(), route.transcript.clone());
            let key = (remote.to_owned(), hint_key(&update));
            if state.last.get(&key) == Some(&update) {
                return None;
            }
            if state.last.len() < LAST_HINTS || state.last.contains_key(&key) {
                state.last.insert(key, update.clone());
            }
            return Some((route.0, route.1, update));
        }
        let key = hint_key(&update);
        let state = &mut *state;
        let held = &mut state.held;
        if let Some(index) = held
            .iter()
            .position(|(session, held, _)| session == remote && *held == key)
        {
            // The latest goes last, after anything held before it.
            held.remove(index);
            held.push((remote.to_owned(), key, update));
        } else if held.len() < HELD_TOTAL
            && held
                .iter()
                .filter(|(session, _, _)| session == remote)
                .count()
                < HELD_HINTS
        {
            held.push((remote.to_owned(), key, update));
        } else if state.overflowed.len() < REMEMBERED_SESSIONS {
            state.overflowed.insert(remote.to_owned());
        }
        None
    }

    /// A child session ended: nothing is held for it any longer.
    pub(crate) fn end_session(&self, remote: &str) {
        if let Ok(mut state) = self.0.lock() {
            // Its hints no longer have an owner to go to.
            state.live.remove(remote);
            state.last.retain(|(session, _), _| session != remote);
            state.held.retain(|(session, _, _)| session != remote);
            state.replaying.remove(remote);
            state.overflowed.remove(remote);
            if state.ended.len() < REMEMBERED_SESSIONS {
                state.ended.insert(remote.to_owned());
            }
        }
    }
    pub(crate) fn forward(&self, remote: &str, update: Value) {
        let Ok(mut state) = self.0.lock() else { return };
        if !state.live.contains_key(remote)
            && let Some(resume) = &mut state.resume
        {
            resume.record(remote, update);
            return;
        }
        Self::forward_registered(&mut state.live, remote, update);
    }
    fn forward_registered(routes: &mut HashMap<String, Route>, remote: &str, mut update: Value) {
        let Some(route) = routes.get(remote).cloned() else {
            return;
        };
        if update["sessionUpdate"] == "subagent_update" {
            let Some(child) = update["sessionId"].as_str().map(str::to_owned) else {
                return;
            };
            if !routes.contains_key(&child) {
                if routes.len() == NODES {
                    return;
                }
                routes.insert(
                    child.clone(),
                    Route {
                        owner: format!("{}:{}:{}", route.owner.len(), route.owner, child),
                        generation: route.generation,
                        transcript: route.transcript.clone(),
                        segment: None,
                        sequence: 0,
                        announced_message: None,
                    },
                );
            }
            update["sessionId"] = Value::String(routes[&child].owner.clone());
            revoke_grants(&mut update);
        }
        for key in ["senderSessionId", "recipientSessionId"] {
            if let Some(id) = update[key].as_str().map(str::to_owned) {
                // Do not leak unannounced identities from an unrelated session.
                update[key] = routes
                    .get(&id)
                    .map(|route| Value::String(route.owner.clone()))
                    .unwrap_or(Value::Null);
            }
        }
        // Assistant output is addressed to the delegating session, not the human.
        // Keep native content and IDs, but express this as an inter-session view.
        match update["sessionUpdate"].as_str() {
            Some(
                "agent_message" | "agent_message_chunk" | "user_message" | "user_message_chunk",
            ) => {
                let incoming = update["sessionUpdate"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("user_"));
                let chunk = update["sessionUpdate"]
                    .as_str()
                    .is_some_and(|kind| kind.ends_with("_chunk"));
                let Some(current) = routes.get_mut(remote) else {
                    return;
                };
                let kind = update["sessionUpdate"].as_str().unwrap_or_default();
                if current.segment.as_deref() != Some(kind) {
                    let Some(sequence) = current.sequence.checked_add(1) else {
                        return;
                    };
                    current.sequence = sequence;
                    current.segment = Some(kind.to_owned());
                }
                let native = update["messageId"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("{}-{}", route.generation, current.sequence));
                // Message IDs are transcript-local upstream. Namespace both views
                // so sibling replies cannot overwrite each other on the parent.
                let id = format!("{}:{}:{}", route.owner.len(), route.owner, native);
                let announce = !chunk || current.announced_message.as_deref() != Some(&id);
                current.announced_message = Some(id.clone());
                let mut message = acp::SessionMessage::new(id.clone())
                    .sender_message_id(acp::MessageId::from(id.clone()))
                    .recipient_message_id(acp::MessageId::from(id.clone()));
                let own = acp::SessionId::from(route.owner.clone());
                let parent = route.transcript.stream_parent(&route.owner);
                if incoming {
                    message = message.recipient_session_id(own);
                    if let Some(parent) = parent {
                        message = message.sender_session_id(acp::SessionId::from(parent));
                    }
                } else {
                    message = message.sender_session_id(own);
                    if let Some(parent) = parent {
                        message = message.recipient_session_id(acp::SessionId::from(parent));
                    }
                }
                if !chunk {
                    match update.get("content") {
                        None => {}
                        Some(Value::Null) => {
                            message.content = agent_client_protocol::schema::MaybeUndefined::Null;
                        }
                        Some(content) => {
                            if let Ok(content) =
                                serde_json::from_value::<Vec<acp::ContentBlock>>(content.clone())
                            {
                                message = message.content(content);
                            }
                        }
                    }
                }
                if announce
                    && let Ok(message) =
                        serde_json::to_value(SessionUpdate::SessionMessage(message))
                {
                    route.transcript.directed(&route.owner, message);
                }
                if chunk
                    && let Ok(content) =
                        serde_json::from_value::<acp::ContentBlock>(update["content"].clone())
                    && let Ok(chunk) = serde_json::to_value(SessionUpdate::SessionMessageChunk(
                        acp::SessionMessageChunk::new(id, content),
                    ))
                {
                    route.transcript.directed(&route.owner, chunk);
                }
            }
            _ => {
                if let Some(current) = routes.get_mut(remote) {
                    current.segment = None;
                }
                route.transcript.forward(&route.owner, update);
            }
        }
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
    use crate::events::SubagentStatus;
    use serde_json::json;

    #[test]
    fn announcement_runtime_identity_survives_capability_patches_on_replay() {
        let stream = Stream::default();
        let runtime = crate::events::SubagentRuntime {
            harness: "acp.kit".into(),
            vendor: crate::events::HarnessVendor::Kit,
            model: Some("gpt".into()),
            created_at_unix_ms: 10,
            generation_started_at_unix_ms: 20,
            generation_finished_at_unix_ms: Some(30),
        };
        stream.announce("child", "worker", "task", SubagentStatus::Idle, 1, &runtime);
        stream.capabilities("child", 1, true);
        let mut replay = stream.subscribe("parent".into(), false);
        let snapshot = replay.try_recv().unwrap().1;
        assert_eq!(
            snapshot["_meta"][crate::events::SUBAGENT_RUNTIME_META],
            json!({"harness":"acp.kit","vendor":"kit","model":"gpt","createdAtUnixMs":10,"generationStartedAtUnixMs":20,"generationFinishedAtUnixMs":30})
        );
    }

    #[test]
    fn announcement_precedes_messages_and_survives_generations() {
        let stream = Stream::default();
        let mut rx = stream.subscribe("parent".into(), true);
        stream.forward("child", json!({"sessionUpdate":"agent_thought_chunk"}));
        assert!(rx.try_recv().is_err());
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Starting,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        stream.directed(
            "child",
            json!({"sessionUpdate":"session_message","messageId":"m"}),
        );
        let announcement = rx.try_recv().unwrap();
        assert_eq!(announcement.0, "parent");
        assert_eq!(announcement.1["sessionId"], "child");
        assert_eq!(rx.try_recv().unwrap().0, "child");
        assert_eq!(rx.try_recv().unwrap().0, "parent");
        stream.announce(
            "child",
            "worker",
            "again",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        assert_eq!(rx.try_recv().unwrap().1["sessionId"], "child");
        let mut replay = stream.subscribe("parent".into(), true);
        assert_eq!(replay.try_recv().unwrap().1["description"], "again");
        assert_eq!(replay.try_recv().unwrap().0, "child");
        assert_eq!(replay.try_recv().unwrap().0, "parent");
        assert!(replay.try_recv().is_err());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ownership_is_immutable_and_replay_is_parent_first() {
        let stream = Stream::default();
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        stream.forward(
            "child",
            json!({"sessionUpdate":"subagent_update","sessionId":"nested","title":"nested"}),
        );
        stream.forward(
            "",
            json!({"sessionUpdate":"subagent_update","sessionId":"nested","title":"stolen"}),
        );
        let mut rx = stream.subscribe("parent".into(), true);
        assert_eq!(rx.try_recv().unwrap().1["sessionId"], "child");
        let nested = rx.try_recv().unwrap();
        assert_eq!(nested.0, "child");
        assert_eq!(nested.1["title"], "nested");
        assert_eq!(stream.parent("nested").as_deref(), Some("child"));
    }

    #[test]
    fn oversized_updates_are_rejected_without_poisoning_stream() {
        let stream = Stream::default();
        let mut rx = stream.subscribe("parent".into(), true);
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        rx.try_recv().unwrap();
        stream.forward("child", json!({"content":"x".repeat(FRAME_BYTES)}));
        assert!(rx.try_recv().is_err());
        stream.forward("child", json!({"sessionUpdate":"plan","entries":[]}));
        assert_eq!(rx.try_recv().unwrap().1["sessionUpdate"], "plan");
    }

    #[test]
    fn full_snapshot_fits_before_receiver_is_returned() {
        let stream = Stream::default();
        for index in 0..NODES {
            stream.announce(
                &format!("child-{index}"),
                "worker",
                "task",
                SubagentStatus::Idle,
                1,
                &crate::events::SubagentRuntime::default(),
            );
        }
        let mut rx = stream.subscribe("parent".into(), true);
        for index in 0..NODES {
            assert_eq!(
                rx.try_recv().unwrap().1["sessionId"],
                format!("child-{index}")
            );
        }
        assert!(rx.try_recv().is_err());
    }

    fn hook_hint(backend: &str, url: Option<&str>, cleared: bool) -> Hint {
        Hint {
            backend: backend.to_owned(),
            agent: None,
            url: url.map(str::to_owned),
            expires_at: None,
            cleared,
            reason: cleared.then_some(crate::ahp::auth::hint::ClearReason::SignedIn),
            problem: None,
        }
    }

    #[test]
    fn a_hook_hint_reaches_its_owner_whenever_the_session_lives() {
        let transcripts = super::super::transcript::Transcripts::default();
        transcripts.start("owner", 1);
        let routes = Routes::default();
        // Before the first prompt: held, the latest per backend, a clear
        // replacing the link it clears.
        assert!(
            routes
                .hook_hint("child", hook_hint("a", Some("https://a/1"), false))
                .is_none()
        );
        assert!(
            routes
                .hook_hint("child", hook_hint("a", None, true))
                .is_none()
        );
        assert!(
            routes
                .hook_hint("child", hook_hint("b", Some("https://b/1"), false))
                .is_none()
        );
        // Another session's are its own.
        assert!(
            routes
                .hook_hint("other", hook_hint("a", Some("https://a/2"), false))
                .is_none()
        );
        let mut held = Vec::new();
        routes.register(
            "child",
            "owner",
            1,
            transcripts.get("owner", 1).unwrap(),
            |update| {
                held.push(update.clone());
            },
        );
        assert_eq!(
            held,
            [
                hook_hint("a", None, true),
                hook_hint("b", Some("https://b/1"), false)
            ]
        );
        // Registered once, it stays routed between prompts: a link or clear
        // published while idle reaches the owner.
        let (owner, _, update) = routes
            .hook_hint("child", hook_hint("a", Some("https://a/3"), false))
            .unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(update, hook_hint("a", Some("https://a/3"), false));
        assert!(
            routes
                .hook_hint("child", hook_hint("a", None, true))
                .is_some()
        );
        // An ended session keeps nothing.
        routes.end_session("other");
        let mut replayed = 0;
        routes.register(
            "other",
            "owner",
            1,
            transcripts.get("owner", 1).unwrap(),
            |_| {
                replayed += 1;
            },
        );
        assert_eq!(replayed, 0);
    }

    #[test]
    fn a_hint_during_replay_never_overtakes_an_older_one() {
        let transcripts = super::super::transcript::Transcripts::default();
        transcripts.start("owner", 1);
        let routes = Routes::default();
        routes.hook_hint("child", hook_hint("a", Some("https://a/1"), false));
        let mut delivered = Vec::new();
        routes.register(
            "child",
            "owner",
            1,
            transcripts.get("owner", 1).unwrap(),
            |update| {
                delivered.push(update.clone());
                // A clear arrives while the replay is delivered: it queues behind.
                if delivered.len() == 1 {
                    assert!(
                        routes
                            .hook_hint("child", hook_hint("a", None, true))
                            .is_none()
                    );
                }
            },
        );
        assert_eq!(
            delivered,
            [
                hook_hint("a", Some("https://a/1"), false),
                hook_hint("a", None, true)
            ]
        );
        // The replay is over: hints go straight through.
        assert!(
            routes
                .hook_hint("child", hook_hint("a", Some("https://a/2"), false))
                .is_some()
        );
    }

    #[test]
    fn expired_held_links_are_not_replayed_and_agents_do_not_collide() {
        let transcripts = super::super::transcript::Transcripts::default();
        transcripts.start("owner", 1);
        let routes = Routes::default();
        let mut expired = hook_hint("a", Some("https://a/old"), false);
        expired.expires_at = Some(1);
        routes.hook_hint("child", expired);
        // A grandchild's link for the same backend, relayed by the child.
        let mut relayed = hook_hint("b", Some("https://b/grandchild"), false);
        relayed.agent = Some("s-grandchild".into());
        routes.hook_hint("child", hook_hint("b", Some("https://b/own"), false));
        routes.hook_hint("child", relayed.clone());
        let mut held = Vec::new();
        routes.register(
            "child",
            "owner",
            1,
            transcripts.get("owner", 1).unwrap(),
            |update| {
                held.push(update.clone());
            },
        );
        assert_eq!(
            held,
            [hook_hint("b", Some("https://b/own"), false), relayed]
        );
    }

    #[test]
    fn held_hook_hints_are_bounded() {
        let routes = Routes::default();
        for backend in 0..HELD_HINTS + 4 {
            routes.hook_hint(
                "child",
                hook_hint(&backend.to_string(), Some("https://x"), false),
            );
        }
        assert_eq!(routes.0.lock().unwrap().held.len(), HELD_HINTS);
        // Each session has its own bound.
        routes.hook_hint("other", hook_hint("0", Some("https://x"), false));
        assert_eq!(routes.0.lock().unwrap().held.len(), HELD_HINTS + 1);
        // A child inventing session ids fills no more than the whole bound,
        // and scans no longer list.
        for session in 0..HELD_TOTAL * 4 {
            routes.hook_hint(
                &format!("made-up-{session}"),
                hook_hint("0", Some("https://x"), false),
            );
        }
        assert_eq!(routes.0.lock().unwrap().held.len(), HELD_TOTAL);
    }

    #[test]
    fn a_panic_mid_replay_does_not_hold_later_hints() {
        let transcripts = super::super::transcript::Transcripts::default();
        transcripts.start("owner", 1);
        let routes = Routes::default();
        routes.hook_hint("child", hook_hint("a", Some("https://a/1"), false));
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            routes.register(
                "child",
                "owner",
                1,
                transcripts.get("owner", 1).unwrap(),
                |_| panic!("delivery failed"),
            );
        }));
        assert!(panicked.is_err());
        assert!(
            routes
                .hook_hint("child", hook_hint("a", None, true))
                .is_some(),
            "a later hint is held after the replay panicked"
        );
        // So too when the panic is in delivering the overflow notice.
        for backend in 0..HELD_HINTS + 1 {
            routes.hook_hint(
                "over",
                hook_hint(&backend.to_string(), Some("https://x"), false),
            );
        }
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            routes.register(
                "over",
                "owner",
                1,
                transcripts.get("owner", 1).unwrap(),
                |hint| {
                    if hint.backend == crate::ahp::auth::hint::MORE_BACKENDS {
                        panic!("delivery failed");
                    }
                },
            );
        }));
        assert!(panicked.is_err());
        assert!(
            routes
                .hook_hint("over", hook_hint("0", None, true))
                .is_some(),
            "a later hint is held after the overflow notice panicked"
        );
    }

    #[test]
    fn the_same_hint_in_the_other_form_is_passed_on_once() {
        let transcripts = super::super::transcript::Transcripts::default();
        transcripts.start("owner", 1);
        let routes = Routes::default();
        routes.register(
            "child",
            "owner",
            1,
            transcripts.get("owner", 1).unwrap(),
            |_| {},
        );
        let link = hook_hint("a", Some("https://a/1"), false);
        assert!(routes.hook_hint("child", link.clone()).is_some());
        assert!(routes.hook_hint("child", link.clone()).is_none());
        // A change goes through, and then the earlier one again.
        assert!(
            routes
                .hook_hint("child", hook_hint("a", None, true))
                .is_some()
        );
        assert!(routes.hook_hint("child", link).is_some());
    }

    #[test]
    fn a_closed_session_routes_no_late_hint_however_many_closed_before() {
        let transcripts = super::super::transcript::Transcripts::default();
        transcripts.start("owner", 1);
        let routes = Routes::default();
        routes.register(
            "first",
            "owner",
            1,
            transcripts.get("owner", 1).unwrap(),
            |_| {},
        );
        routes.end_session("first");
        // Many more sessions close on the same connection.
        for session in 0..REMEMBERED_SESSIONS + 8 {
            let session = format!("other-{session}");
            routes.register(
                &session,
                "owner",
                1,
                transcripts.get("owner", 1).unwrap(),
                |_| {},
            );
            routes.end_session(&session);
        }
        assert!(
            routes
                .hook_hint("first", hook_hint("a", Some("https://late"), false))
                .is_none()
        );
        assert!(!routes.0.lock().unwrap().live.contains_key("first"));
    }

    #[test]
    fn nested_routes_and_directed_message_ids_are_stable_and_distinct() {
        let transcripts = super::super::transcript::Transcripts::default();
        let mut rx = transcripts.stream.subscribe("parent".into(), true);
        let routes = Routes::default();
        for child in ["a", "b"] {
            transcripts.stream.announce(
                child,
                child,
                "task",
                SubagentStatus::Working,
                1,
                &crate::events::SubagentRuntime::default(),
            );
            transcripts.start(child, 1);
            routes.register(child, child, 1, transcripts.get(child, 1).unwrap(), |_| {});
        }
        rx.try_recv().unwrap();
        rx.try_recv().unwrap();
        let chunk = json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"hello"}});
        routes.forward("a", chunk.clone());
        let first = rx.try_recv().unwrap();
        assert_eq!(first.1["sessionUpdate"], "session_message");
        assert_eq!(first.1["senderSessionId"], "a");
        assert_eq!(first.1["recipientSessionId"], "parent");
        for _ in 0..3 {
            rx.try_recv().unwrap();
        }
        routes.forward("a", chunk.clone());
        assert_eq!(rx.try_recv().unwrap().1["messageId"], first.1["messageId"]);
        rx.try_recv().unwrap();
        assert!(rx.try_recv().is_err());
        routes.forward("b", chunk);
        assert_ne!(rx.try_recv().unwrap().1["messageId"], first.1["messageId"]);
        for _ in 0..3 {
            rx.try_recv().unwrap();
        }
        routes.forward(
            "a",
            json!({"sessionUpdate":"subagent_update", "sessionId":"remote-nested"}),
        );
        let child = rx.try_recv().unwrap();
        assert_eq!(child.0, "a");
        routes.forward(
            "remote-nested",
            json!({"sessionUpdate":"plan","entries":[]}),
        );
        assert_eq!(
            rx.try_recv().unwrap().0,
            child.1["sessionId"].as_str().unwrap()
        );
    }

    #[test]
    fn steering_requires_current_generation_support_and_live_work() {
        let stream = Stream::default();
        let mut rx = stream.subscribe("parent".into(), true);
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        assert_eq!(
            rx.try_recv().unwrap().1["_meta"]["kit/subagent/steer"],
            json!({"generation":1,"canSteer":false})
        );
        stream.capabilities("child", 1, true);
        assert_eq!(
            rx.try_recv().unwrap().1["_meta"]["kit/subagent/steer"]["canSteer"],
            true
        );
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Idle,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        assert_eq!(
            rx.try_recv().unwrap().1["_meta"]["kit/subagent/steer"]["canSteer"],
            false
        );
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            2,
            &crate::events::SubagentRuntime::default(),
        );
        assert_eq!(
            rx.try_recv().unwrap().1["_meta"]["kit/subagent/steer"],
            json!({"generation":2,"canSteer":false})
        );
        stream.capabilities("child", 1, true);
        stream.announce(
            "child",
            "worker",
            "old",
            SubagentStatus::Idle,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        assert!(rx.try_recv().is_err());
        stream.capabilities("child", 2, true);
        assert_eq!(
            rx.try_recv().unwrap().1["_meta"]["kit/subagent/steer"]["canSteer"],
            true
        );
        let mut replay = stream.subscribe("parent".into(), true);
        assert_eq!(
            replay.try_recv().unwrap().1["_meta"]["kit/subagent/steer"],
            json!({"generation":2,"canSteer":false})
        );
    }

    #[test]
    fn reconnect_replays_bounded_ordered_history_without_duplicating_live_chunks() {
        let stream = Stream::default();
        let mut live = stream.subscribe("parent".into(), true);
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        live.try_recv().unwrap();
        for sequence in 0..HISTORY + 3 {
            stream.forward("child", json!({"sessionUpdate":"session_message_chunk", "messageId":"m", "content":{"type":"text","text":sequence.to_string()}}));
            live.try_recv().unwrap();
        }
        let mut replay = stream.subscribe("parent".into(), true);
        assert_eq!(
            replay.try_recv().unwrap().1["sessionUpdate"],
            "subagent_update"
        );
        for sequence in 3..HISTORY + 3 {
            assert_eq!(
                replay.try_recv().unwrap().1["content"]["text"],
                sequence.to_string()
            );
        }
        assert!(replay.try_recv().is_err());
        assert!(live.try_recv().is_err());
        stream.forward("child", json!({"sessionUpdate":"plan","entries":[]}));
        assert_eq!(replay.try_recv().unwrap(), live.try_recv().unwrap());
    }

    #[test]
    fn history_byte_budget_evicts_old_frames_without_regressing_current_state() {
        let stream = Stream::default();
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        let text = "x".repeat(FRAME_BYTES / 2);
        for sequence in 0..HISTORY {
            stream.forward("child", json!({"sessionUpdate":"session_message_chunk","messageId":sequence.to_string(),"content":{"type":"text","text":text}}));
        }
        stream.forward(
            "child",
            json!({"sessionUpdate":"state_update","state":"running"}),
        );
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Idle,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        let mut replay = stream.subscribe("parent".into(), true);
        let announcement = replay.try_recv().unwrap();
        assert_eq!(
            announcement.1["_meta"]["kit/subagent/steer"]["canSteer"],
            false
        );
        let mut retained_bytes = 0;
        let mut count = 0;
        while let Ok((_, event)) = replay.try_recv() {
            assert_eq!(event["sessionUpdate"], "session_message_chunk");
            retained_bytes += serde_json::to_vec(&event).unwrap().len();
            count += 1;
        }
        assert!(count > 0 && count < HISTORY);
        assert!(retained_bytes <= HISTORY_BYTES);
    }

    #[test]
    fn nested_current_state_is_replayed_in_ownership_snapshot() {
        let stream = Stream::default();
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        stream.forward(
            "child",
            json!({"sessionUpdate":"subagent_update","sessionId":"nested"}),
        );
        stream.forward(
            "nested",
            json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}),
        );
        let mut replay = stream.subscribe("parent".into(), true);
        replay.try_recv().unwrap();
        assert_eq!(
            replay.try_recv().unwrap().1["state"],
            json!({"state":"idle","stopReason":"end_turn"})
        );
        assert!(replay.try_recv().is_err());
    }

    #[test]
    fn nested_kit_steering_grants_do_not_authorize_the_direct_child_rpc() {
        let transcripts = super::super::transcript::Transcripts::default();
        let mut rx = transcripts.stream.subscribe("parent".into(), true);
        transcripts.stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        transcripts.start("child", 1);
        let routes = Routes::default();
        routes.register(
            "remote",
            "child",
            1,
            transcripts.get("child", 1).unwrap(),
            |_| {},
        );
        rx.try_recv().unwrap();
        routes.forward("remote", json!({"sessionUpdate":"subagent_update", "sessionId":"nested", "_meta":{"kit/subagent/steer":{"generation":1,"canSteer":true}}}));
        assert_eq!(
            rx.try_recv().unwrap().1["_meta"]["kit/subagent/steer"]["canSteer"],
            false
        );
    }

    #[test]
    fn no_history_resume_announces_ownership_without_old_content_or_grants() {
        let stream = Stream::default();
        let mut live = stream.subscribe("parent".into(), false);
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        stream.capabilities("child", 1, true);
        stream.forward("child", json!({"sessionUpdate":"session_message_chunk", "messageId":"m", "content":{"type":"text","text":"old"}}));
        for _ in 0..3 {
            live.try_recv().unwrap();
        }
        let mut resumed = stream.subscribe("parent".into(), false);
        let association = resumed.try_recv().unwrap();
        assert_eq!(association.1["sessionId"], "child");
        assert_eq!(
            association.1["_meta"]["kit/subagent/steer"],
            json!({"generation":1,"canSteer":false})
        );
        assert!(resumed.try_recv().is_err());
        assert!(live.try_recv().is_err());
        // A new capability report, not a cached replay, can grant steering.
        stream.capabilities("child", 1, true);
        assert_eq!(
            resumed.try_recv().unwrap().1["_meta"]["kit/subagent/steer"]["canSteer"],
            true
        );
        stream.forward("child", json!({"sessionUpdate":"plan","entries":[]}));
        assert_eq!(resumed.try_recv().unwrap().1["sessionUpdate"], "plan");
    }

    #[test]
    fn replay_revokes_cached_standard_and_kit_mutation_grants() {
        let stream = Stream::default();
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        stream.forward("child", json!({"sessionUpdate":"subagent_update", "sessionId":"nested", "capabilities":{"cancel":{}}, "_meta":{"kit/subagent/steer":{"generation":3,"canSteer":true}}}));
        for replay_history in [false, true] {
            let mut rx = stream.subscribe("parent".into(), replay_history);
            rx.try_recv().unwrap();
            let nested = rx.try_recv().unwrap();
            assert_eq!(nested.1["capabilities"], Value::Null);
            assert_eq!(
                nested.1["_meta"]["kit/subagent/steer"],
                json!({"generation":3,"canSteer":false})
            );
        }
    }

    #[test]
    fn metadata_key_patches_match_live_and_reconnect_snapshots() {
        let stream = Stream::default();
        let mut live = stream.subscribe("parent".into(), false);
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        live.try_recv().unwrap();
        let mut observed = serde_json::Map::new();
        for patch in [
            json!({"sessionUpdate":"subagent_update","sessionId":"child","_meta":{"one":1,"two":{"a":1}}}),
            json!({"sessionUpdate":"subagent_update","sessionId":"child","_meta":{"one":null,"three":3}}),
            json!({"sessionUpdate":"subagent_update","sessionId":"child","description":"omitted meta"}),
            json!({"sessionUpdate":"subagent_update","sessionId":"child","_meta":{"two":{"b":2}}}),
        ] {
            stream.forward("", patch);
            let event = live.try_recv().unwrap();
            patch_update(&mut observed, event.1.as_object().unwrap());
        }
        assert_eq!(observed["_meta"], json!({"two":{"b":2},"three":3}));
        let mut replay = stream.subscribe("parent".into(), false);
        let snapshot = replay.try_recv().unwrap();
        assert_eq!(snapshot.1["_meta"]["two"], observed["_meta"]["two"]);
        assert_eq!(snapshot.1["_meta"]["three"], 3);
        assert!(snapshot.1["_meta"].get("one").is_none());
        stream.forward(
            "child",
            json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}),
        );
        let mut after_state = stream.subscribe("parent".into(), false);
        assert_eq!(after_state.try_recv().unwrap().1["_meta"]["three"], 3);
        stream.forward(
            "",
            json!({"sessionUpdate":"subagent_update","sessionId":"child","_meta":null}),
        );
        let mut cleared = stream.subscribe("parent".into(), false);
        assert_eq!(cleared.try_recv().unwrap().1["_meta"], Value::Null);
    }

    #[test]
    fn resume_tail_eviction_preserves_associations_and_metadata() {
        let transcripts = super::super::transcript::Transcripts::default();
        transcripts.stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        transcripts.start("child", 1);
        let mut rx = transcripts.stream.subscribe("parent".into(), false);
        rx.try_recv().unwrap();
        let routes = Routes::default();
        routes.begin_resume("remote".into());
        routes.forward("remote", json!({"sessionUpdate":"subagent_update","sessionId":"nested","_meta":{"one":1,"two":2}}));
        routes.forward("remote", json!({"sessionUpdate":"subagent_update","sessionId":"nested","_meta":{"one":null,"three":3}}));
        for sequence in 0..HISTORY + 3 {
            routes.forward("nested", json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":sequence.to_string()}}));
        }
        assert!(rx.try_recv().is_err());
        routes.forward(
            "remote",
            json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}),
        );
        routes.register(
            "remote",
            "child",
            1,
            transcripts.get("child", 1).unwrap(),
            |_| {},
        );
        let association = rx.try_recv().unwrap();
        assert_eq!(association.1["_meta"], json!({"two":2,"three":3}));
        let nested = association.1["sessionId"].as_str().unwrap();
        for sequence in 3..HISTORY + 3 {
            let event = rx.try_recv().unwrap();
            assert_eq!(event.0, nested);
            assert_eq!(event.1["content"]["text"], sequence.to_string());
        }
        routes.forward("nested", json!({"sessionUpdate":"plan","entries":[]}));
        assert_eq!(rx.try_recv().unwrap().0, nested);
        transcripts.stream.capabilities("child", 1, true);
        assert_eq!(
            rx.try_recv().unwrap().1["_meta"]["kit/subagent/steer"]["canSteer"],
            true
        );
    }

    #[test]
    fn resume_descendant_state_survives_tail_eviction_and_respects_newer_associations() {
        for newer_association in [false, true] {
            let transcripts = super::super::transcript::Transcripts::default();
            transcripts.stream.announce(
                "child",
                "worker",
                "task",
                SubagentStatus::Working,
                1,
                &crate::events::SubagentRuntime::default(),
            );
            transcripts.start("child", 1);
            let mut rx = transcripts.stream.subscribe("parent".into(), false);
            rx.try_recv().unwrap();
            let routes = Routes::default();
            routes.begin_resume("remote".into());
            routes.forward("remote", json!({"sessionUpdate":"subagent_update","sessionId":"nested","state":{"state":"running"}}));
            routes.forward(
                "nested",
                json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}),
            );
            if newer_association {
                routes.forward("remote", json!({"sessionUpdate":"subagent_update","sessionId":"nested","state":{"state":"running"}}));
            }
            let content_count = if newer_association { 2 } else { HISTORY + 1 };
            for sequence in 0..content_count {
                routes.forward("nested", json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":sequence.to_string()}}));
            }
            routes.register(
                "remote",
                "child",
                1,
                transcripts.get("child", 1).unwrap(),
                |_| {},
            );
            let association = rx.try_recv().unwrap();
            assert_eq!(
                association.1["state"]["state"],
                if newer_association { "running" } else { "idle" }
            );
            for sequence in content_count.saturating_sub(HISTORY)..content_count {
                let event = rx.try_recv().unwrap();
                assert_eq!(event.1["sessionUpdate"], "agent_thought_chunk");
                assert_eq!(event.1["content"]["text"], sequence.to_string());
            }
            assert!(rx.try_recv().is_err());
            // The retained stream snapshot must agree, not just the live event.
            let mut replay = transcripts.stream.subscribe("parent".into(), false);
            replay.try_recv().unwrap();
            assert_eq!(
                replay.try_recv().unwrap().1["state"],
                association.1["state"]
            );
        }
    }

    #[test]
    fn directed_message_views_preserve_omission_null_and_replacement_content() {
        for kind in ["agent_message", "user_message"] {
            let transcripts = super::super::transcript::Transcripts::default();
            transcripts.stream.announce(
                "child",
                "worker",
                "task",
                SubagentStatus::Working,
                1,
                &crate::events::SubagentRuntime::default(),
            );
            transcripts.start("child", 1);
            let mut rx = transcripts.stream.subscribe("parent".into(), false);
            rx.try_recv().unwrap();
            let routes = Routes::default();
            routes.register(
                "remote",
                "child",
                1,
                transcripts.get("child", 1).unwrap(),
                |_| {},
            );
            let mut views: HashMap<String, super::super::messages::Messages> = HashMap::new();
            let updates = [
                (
                    json!({"sessionUpdate":kind,"messageId":"m","content":[{"type":"text","text":"old"}]}),
                    "old",
                ),
                (json!({"sessionUpdate":kind,"messageId":"m"}), "old"),
                (
                    json!({"sessionUpdate":kind,"messageId":"m","content":null}),
                    "",
                ),
                (
                    json!({"sessionUpdate":format!("{kind}_chunk"),"messageId":"m","content":{"type":"text","text":"new"}}),
                    "new",
                ),
                (
                    json!({"sessionUpdate":kind,"messageId":"m","content":[{"type":"text","text":"replacement"}]}),
                    "replacement",
                ),
            ];
            for (native, expected) in updates {
                let content = native.get("content").cloned();
                let chunk = native["sessionUpdate"]
                    .as_str()
                    .unwrap()
                    .ends_with("_chunk");
                routes.forward("remote", native);
                while let Ok((owner, mut event)) = rx.try_recv() {
                    let decoded: acp::SessionUpdate =
                        serde_json::from_value(event.clone()).unwrap();
                    match decoded {
                        acp::SessionUpdate::SessionMessage(_) => {
                            if !chunk {
                                assert_eq!(event.get("content"), content.as_ref());
                            }
                            event["sessionUpdate"] = Value::String("agent_message".into());
                        }
                        acp::SessionUpdate::SessionMessageChunk(_) => {
                            event["sessionUpdate"] = Value::String("agent_message_chunk".into());
                        }
                        _ => panic!("expected a directed message"),
                    }
                    // Exercise the actual native text reducer for each view;
                    // only the envelope discriminator differs from its API.
                    let update = super::super::messages::parse(&event).unwrap().unwrap();
                    views.entry(owner).or_default().record(update);
                }
                assert_eq!(views["parent"].clone().finish(), expected);
                assert_eq!(views["child"].clone().finish(), expected);
            }
        }
    }

    #[test]
    fn live_descendant_state_has_parent_addressed_v1_association_patch() {
        let stream = Stream::default();
        let mut rx = stream.subscribe("root".into(), false);
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        stream.forward("child", json!({"sessionUpdate":"subagent_update","sessionId":"grandchild","state":{"state":"running"}}));
        stream.forward(
            "grandchild",
            json!({"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}),
        );
        let mut associations = Vec::new();
        let mut raw_states = Vec::new();
        while let Ok((owner, update)) = rx.try_recv() {
            if update["sessionUpdate"] == "state_update" {
                assert_eq!(
                    associations.len(),
                    3,
                    "ownership and state patch precede raw state"
                );
                raw_states.push((owner, update));
                continue;
            }
            // Decode with the actual v1 SDK API. The server's private projection
            // passes subagent_update through unchanged and drops raw state_update.
            let notification: acp::SessionNotification = serde_json::from_value(json!({
                "sessionId":owner, "update":update,
            }))
            .unwrap();
            if let acp::SessionUpdate::SubagentUpdate(association) = notification.update {
                associations.push((notification.session_id.to_string(), association));
            }
        }
        assert_eq!(associations.len(), 3);
        assert_eq!(associations[0].0, "root");
        assert_eq!(associations[0].1.session_id.to_string(), "child");
        assert_eq!(associations[1].0, "child");
        assert_eq!(associations[1].1.session_id.to_string(), "grandchild");
        assert_eq!(associations[2].0, "child");
        assert_eq!(associations[2].1.session_id.to_string(), "grandchild");
        assert!(matches!(
            associations[2].1.state,
            agent_client_protocol::schema::MaybeUndefined::Value(acp::StateUpdate::Idle(_))
        ));
        assert_eq!(raw_states.len(), 1);
        assert_eq!(raw_states[0].0, "grandchild");
        let mut replay = stream.subscribe("root".into(), false);
        replay.try_recv().unwrap();
        assert_eq!(
            replay.try_recv().unwrap().1["state"],
            json!({"state":"idle","stopReason":"end_turn"})
        );
    }

    #[test]
    fn poisoned_ordering_is_isolated() {
        let stream = Stream::default();
        let mut rx = stream.subscribe("parent".into(), true);
        let _ = std::panic::catch_unwind(|| {
            let _guard = stream.state.lock().unwrap();
            panic!("abandoned transition");
        });
        stream.announce(
            "child",
            "worker",
            "task",
            SubagentStatus::Working,
            1,
            &crate::events::SubagentRuntime::default(),
        );
        assert!(rx.try_recv().is_err());
    }
}
