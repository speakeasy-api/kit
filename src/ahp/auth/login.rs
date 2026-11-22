//! Per-backend sign-in state shared by every Hooks instance in this process:
//! which backends need a login, this process's pending logins, and the hints
//! shown to the user. As in Kit's MCP sign-in, each login has its own
//! loopback listener on a port chosen by the system, and the process that
//! prepared it completes it; other processes see the grant in the shared
//! credential store.
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex, MutexGuard, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use agenthooksprotocol::{
    adapters::registered::{AuthContext, AuthPurpose},
    auth::AuthErrorCode,
    auth::managed::{ManagedOAuth, OperationBudget, PendingLogin},
};
use tokio::{net::TcpListener, sync::Notify};

/// What the user is shown about one backend's sign-in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AuthHint {
    /// The backend within the hooks file that declares it.
    pub scope: String,
    pub backend: String,
    /// Where to approve Kit. `None` once cleared.
    pub url: Option<String>,
    /// When the link stops working, in Unix milliseconds.
    pub expires_at_ms: Option<u64>,
    pub cleared: bool,
    /// Cleared because sign-in finished, rather than because the link expired.
    pub signed_in: bool,
    /// Why it cleared.
    pub reason: Option<super::hint::ClearReason>,
    /// What went wrong with the backend's sign-in, as a clause after
    /// "Hook backend `<id>` ", with what to check: sanitized, never a
    /// provider message.
    pub problem: Option<String>,
}

/// The `_meta` key that marks a sign-in hint, on a notice or Kit's own
/// notification. A hint is for a person: a parent Kit never passes a child's
/// hint to its model.
pub(crate) const HOOK_AUTH_META: &str = "kit/hooks/auth";

impl AuthHint {
    /// Plain text for a client without Kit's status line (see
    /// [`super::hint::HookHint::notice_text`]).
    pub(crate) fn notice(&self) -> Option<String> {
        super::hint::HookHint::from(self).notice_text()
    }
}

impl From<&AuthHint> for super::hint::HookHint {
    fn from(hint: &AuthHint) -> Self {
        Self {
            backend: hint.backend.clone(),
            agent: None,
            url: hint.url.clone(),
            expires_at: hint.expires_at_ms,
            cleared: hint.cleared,
            reason: hint.reason,
            problem: hint.problem.clone(),
        }
    }
}

/// A target whose last operation needed a login, kept to prepare exactly it.
#[derive(Clone)]
pub(super) struct Need {
    pub(super) context: AuthContext,
    /// The scopes the SDK reported as required: a 403's step-up scopes, or
    /// none for a plain sign-in.
    pub(super) required: Vec<String>,
}

/// Whether `granted` step-up scopes include every one in `required`.
fn covers(granted: &[String], required: &[String]) -> bool {
    required.iter().all(|scope| granted.contains(scope))
}

fn same_target(a: &AuthContext, b: &AuthContext) -> bool {
    a.purpose == b.purpose && a.destination == b.destination
}

/// A pending login of this process: the provider that holds it, its target,
/// and the `state` its callback carries.
#[derive(Clone)]
pub(super) struct Login {
    pub(super) scope: String,
    pub(super) oauth: Arc<ManagedOAuth>,
    pub(super) context: AuthContext,
    pub(super) pending: PendingLogin,
    pub(super) state: String,
    /// The scopes it was prepared to require: completing it (which the SDK
    /// lets succeed only with a grant that has them) meets a need for no
    /// more than these.
    pub(super) required: Vec<String>,
    pub(super) url: String,
    pub(super) expires_at: SystemTime,
    /// The logout generation it was prepared at: a logout since, in any Kit
    /// process, supersedes it.
    pub(super) generation: Generation,
    /// Stops the login's listener once it is no longer wanted.
    stop: Arc<Notify>,
}

/// A scope's logout generation when a login was prepared, and the storage
/// that holds the current one.
#[derive(Clone)]
pub(super) struct Generation {
    pub(super) storage: crate::credentials::CredentialStorage,
    pub(super) at: String,
}

impl Generation {
    pub(super) async fn read(
        storage: &crate::credentials::CredentialStorage,
        scope: &str,
    ) -> Result<Self, agenthooksprotocol::client::HookError> {
        Ok(Self {
            storage: storage.clone(),
            at: super::store::logout_generation(storage, scope).await?,
        })
    }

    /// Whether no logout happened since. An unreadable marker is an error:
    /// the caller decides whether that refuses (before a grant is stored)
    /// or is tolerated (after).
    pub(super) async fn current(
        &self,
        scope: &str,
    ) -> Result<bool, agenthooksprotocol::client::HookError> {
        super::store::logout_generation(&self.storage, scope)
            .await
            .map(|now| now == self.at)
    }
}

impl Login {
    pub(super) fn new(
        scope: &str,
        oauth: Arc<ManagedOAuth>,
        context: &AuthContext,
        required: &[String],
        pending: PendingLogin,
        generation: Generation,
    ) -> Self {
        Self {
            scope: scope.to_owned(),
            oauth,
            context: context.clone(),
            state: query(pending.authorization_url(), "state").unwrap_or_default(),
            required: required.to_vec(),
            url: pending.authorization_url().to_string(),
            expires_at: pending.expires_at(),
            pending,
            generation,
            stop: Arc::default(),
        }
    }

    fn hint(&self) -> AuthHint {
        AuthHint {
            scope: self.scope.clone(),
            backend: self.context.backend_id.clone(),
            url: Some(self.url.clone()),
            expires_at_ms: Some(unix_ms(self.expires_at)),
            cleared: false,
            signed_in: false,
            reason: None,
            problem: None,
        }
    }
}

pub(super) fn query(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

/// A subscriber: each hint arrives with its sequence number, so one that
/// was overtaken on the way is dropped (see [`ordered`]).
/// `true` marks a persistent hint, `false` a transient diagnostic.
type Sink = dyn Fn(u64, bool, &AuthHint) + Send + Sync;

/// What a subscriber has been shown last for each scope: the sequence
/// number of its latest hint.
type Shown = Mutex<HashMap<String, u64>>;

/// Keyed by scope: a backend id within the hooks file that declares it, so
/// two workspaces with the same backend id never share hints or needs.
#[derive(Default)]
struct State {
    needs: HashMap<String, Vec<Need>>,
    preparing: HashSet<String>,
    /// This process's pending logins, one per target; the first is shown.
    pending: HashMap<String, Vec<Login>>,
    /// The current hint per scope, with its sequence number.
    hints: HashMap<String, (u64, AuthHint)>,
    /// Numbers hints in the order the registry's state produced them.
    sequence: u64,
    /// Links already printed to stderr for a session nobody is attached to.
    printed: HashSet<String>,
    sinks: Vec<(HashSet<String>, Weak<Sink>)>,
}

pub(super) struct Registry(Mutex<State>);

pub(super) fn registry() -> &'static Registry {
    static REGISTRY: LazyLock<Registry> = LazyLock::new(|| Registry(Mutex::default()));
    &REGISTRY
}

pub(super) fn unix_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |elapsed| {
        u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
    })
}

/// Keeps a hint sink registered; dropping it stops delivery.
pub(crate) struct HintSubscription {
    _sink: Arc<Sink>,
}

fn cleared(scope: &str, backend: &str, reason: super::hint::ClearReason) -> AuthHint {
    AuthHint {
        scope: scope.to_owned(),
        backend: backend.to_owned(),
        url: None,
        expires_at_ms: None,
        cleared: true,
        signed_in: reason == super::hint::ClearReason::SignedIn,
        reason: Some(reason),
        problem: None,
    }
}

impl Registry {
    fn lock(&self) -> MutexGuard<'_, State> {
        // No callback, await or drop of a sink runs under this guard.
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Delivers `hint`: to subscribers for its scope, or as one stderr
    /// line per link when nobody is attached. Kit stages each hint with the
    /// state change that produced it instead; tests publish directly.
    #[cfg(test)]
    pub(super) fn publish(&self, hint: AuthHint) {
        let delivery = stage(&mut self.lock(), hint);
        deliver(delivery);
    }

    pub(super) fn subscribe(
        &self,
        scopes: HashSet<String>,
        sink: impl Fn(&AuthHint) + Send + Sync + 'static,
    ) -> HintSubscription {
        let inner: Arc<dyn Fn(&AuthHint) + Send + Sync> = Arc::new(sink);
        let (sink, shown) = ordered(Arc::clone(&inner));
        let (current, mut turn) = {
            let mut state = self.lock();
            state.sinks.push((scopes.clone(), Arc::downgrade(&sink)));
            let current: Vec<_> = state
                .hints
                .values()
                .filter(|(_, hint)| scopes.contains(&hint.scope))
                .cloned()
                .collect();
            // Taken before the registry lock is released (registry, then
            // subscriber; never the reverse): live updates wait until the
            // snapshot is replayed, and any it already includes are dropped.
            let turn = shown.lock().unwrap_or_else(|poison| poison.into_inner());
            (current, turn)
        };
        // A session attached after a link was shown still sees it.
        for (sequence, hint) in &current {
            turn.insert(hint.scope.clone(), *sequence);
            inner(hint);
        }
        drop(turn);
        HintSubscription { _sink: sink }
    }

    pub(super) fn needs_login(&self, scope: &str, context: &AuthContext, required: &[String]) {
        let mut state = self.lock();
        let needs = state.needs.entry(scope.to_owned()).or_default();
        let need = Need {
            context: context.clone(),
            required: required.to_vec(),
        };
        match needs.iter_mut().find(|n| same_target(&n.context, context)) {
            // A pending step-up is kept: a plain observation (or another
            // step-up) adds to its scopes, so only a grant with all of them
            // settles it.
            Some(existing) => {
                let mut required = existing.required.clone();
                for scope in need.required {
                    if !required.contains(&scope) {
                        required.push(scope);
                    }
                }
                required.sort();
                *existing = Need { required, ..need };
            }
            None => needs.push(need),
        }
    }

    /// A target that now has a usable credential with its own scopes,
    /// signed in here or by another Kit process: a plain need is met (see
    /// [`Self::settle`]); a step-up need is not.
    pub(super) fn usable(&self, scope: &str, context: &AuthContext) {
        self.settle(scope, context, Settle::Granted(&[]), None);
    }

    /// `context`'s target was prepared with `required` scopes: a need for
    /// no more is met.
    pub(super) fn prepared(&self, scope: &str, context: &AuthContext, required: &[String]) {
        self.settle(scope, context, Settle::Granted(required), None);
    }

    pub(super) fn needs(&self, scope: &str) -> Vec<Need> {
        self.lock().needs.get(scope).cloned().unwrap_or_default()
    }

    pub(super) fn has_needs(&self, scope: &str) -> bool {
        self.lock().needs.contains_key(scope)
    }

    /// Claims the right to prepare a scope's logins; one task at a time.
    pub(super) fn begin_preparing(&self, scope: &str) -> Option<Preparing> {
        self.lock()
            .preparing
            .insert(scope.to_owned())
            .then(|| Preparing(scope.to_owned()))
    }

    /// Whether this process has a login for `context`'s target whose link
    /// still works and that would meet a need for `required` step-up
    /// scopes. Later turns keep showing it rather than prepare another.
    pub(super) fn has_live_login(
        &self,
        scope: &str,
        context: &AuthContext,
        required: &[String],
    ) -> bool {
        let now = SystemTime::now();
        self.lock().pending.get(scope).is_some_and(|logins| {
            logins.iter().any(|login| {
                same_target(&login.context, context)
                    && login.expires_at > now
                    && covers(&login.required, required)
            })
        })
    }

    /// Records a pending login of this process, replacing the target's
    /// previous one; [`Self::show`] then shows the scope's link.
    pub(super) fn offer(&self, login: Login) {
        let replaced = {
            let mut state = self.lock();
            let logins = state.pending.entry(login.scope.clone()).or_default();
            let replaced: Vec<_> = logins
                .iter()
                .filter(|existing| same_target(&existing.context, &login.context))
                .map(|existing| Arc::clone(&existing.stop))
                .collect();
            logins.retain(|existing| !same_target(&existing.context, &login.context));
            logins.push(login);
            replaced
        };
        for stop in replaced {
            stop.notify_one();
        }
    }

    /// Shows the scope's pending link.
    pub(super) fn show(&self, scope: &str) {
        let delivery = {
            let mut state = self.lock();
            let hint = state
                .pending
                .get(scope)
                .and_then(|logins| logins.first())
                .map(Login::hint);
            hint.and_then(|hint| stage(&mut state, hint))
        };
        deliver(delivery);
    }

    /// Shows why a sign-in the scope needs could not be prepared.
    /// A link already shown for the scope stays: the problem is added to it.
    pub(super) fn problem(&self, scope: &str, backend: &str, problem: String) {
        let delivery = {
            let mut state = self.lock();
            let link = state
                .pending
                .get(scope)
                .and_then(|logins| logins.first())
                .map(Login::hint);
            let hint = AuthHint {
                problem: Some(problem),
                ..link.unwrap_or_else(|| AuthHint {
                    cleared: false,
                    reason: None,
                    ..cleared(scope, backend, super::hint::ClearReason::Withdrawn)
                })
            };
            stage(&mut state, hint)
        };
        deliver(delivery);
    }

    /// Tells the scope's subscribers about a problem that belongs to one
    /// operation, not to a sign-in (a scope denial, say): beside any link
    /// shown, once per subscriber, and never replayed to a later one, since
    /// nothing would clear it.
    pub(super) fn diagnostic(&self, scope: &str, backend: &str, problem: String) {
        let delivery = {
            let mut state = self.lock();
            let link = state
                .pending
                .get(scope)
                .and_then(|logins| logins.first())
                .map(Login::hint);
            let hint = AuthHint {
                problem: Some(problem),
                ..link.unwrap_or_else(|| AuthHint {
                    cleared: false,
                    reason: None,
                    ..cleared(scope, backend, super::hint::ClearReason::Withdrawn)
                })
            };
            stage_transient(&mut state, hint)
        };
        deliver(Some(delivery));
    }

    /// `login` completed: its listener is gone, so it is withdrawn, and it
    /// meets its target's need if its scopes cover it. The scope's other
    /// targets (an upload on another resource, say) keep their logins.
    pub(super) fn completed(&self, login: &Login) {
        self.settle(
            &login.scope,
            &login.context,
            Settle::Granted(&login.required),
            Some(&login.state),
        );
    }

    /// `login` ended without a grant (its link expired or consent was
    /// denied): it is withdrawn, and the next turn prepares a new one. A
    /// newer login for the same target, which replaced it, stays.
    pub(super) fn ended(&self, login: &Login) {
        self.settle(
            &login.scope,
            &login.context,
            Settle::Ended,
            Some(&login.state),
        );
    }

    /// The one place sign-in state changes after an operation, all under
    /// one lock:
    ///
    /// - `finished`, the `state` of a login whose listener is gone, is
    ///   withdrawn (a login without a state matches none).
    /// - [`Settle::Granted`] meets the target's need when its step-up scopes
    ///   cover the need's; then the target's remaining logins stop too. A
    ///   need that asks for more (a step-up, perhaps recorded meanwhile)
    ///   stays, with its logins.
    ///
    /// If anything changed, the hint shows the scope's next link, or clears:
    /// as signed in once no target of the scope needs a login.
    fn settle(&self, scope: &str, context: &AuthContext, how: Settle<'_>, finished: Option<&str>) {
        let (stopped, delivery) = {
            let mut state = self.lock();
            let mut changed = false;
            let mut stopped = Vec::new();
            if let Some(finished) = finished.filter(|state| !state.is_empty())
                && let Some(logins) = state.pending.get_mut(scope)
            {
                let before = logins.len();
                logins.retain(|login| login.state != finished);
                changed |= logins.len() != before;
            }
            if let Settle::Granted(granted) = how {
                let met = state.needs.get(scope).is_none_or(|needs| {
                    needs.iter().all(|need| {
                        !same_target(&need.context, context) || covers(granted, &need.required)
                    })
                });
                if met {
                    if let Some(needs) = state.needs.get_mut(scope) {
                        let before = needs.len();
                        needs.retain(|need| !same_target(&need.context, context));
                        changed |= needs.len() != before;
                        if needs.is_empty() {
                            state.needs.remove(scope);
                        }
                    }
                    if let Some(logins) = state.pending.get_mut(scope) {
                        logins.retain(|login| {
                            let ended = same_target(&login.context, context);
                            if ended {
                                stopped.push(Arc::clone(&login.stop));
                            }
                            !ended
                        });
                        changed |= !stopped.is_empty();
                    }
                }
            }
            if !changed {
                return;
            }
            let next = state
                .pending
                .get(scope)
                .and_then(|logins| logins.first())
                .map(Login::hint);
            if next.is_none() {
                state.pending.remove(scope);
            }
            let reason = match how {
                Settle::Granted(_) if !state.needs.contains_key(scope) => {
                    super::hint::ClearReason::SignedIn
                }
                Settle::Granted(_) => super::hint::ClearReason::Withdrawn,
                Settle::Ended => super::hint::ClearReason::Expired,
            };
            let hint = next.unwrap_or_else(|| cleared(scope, &context.backend_id, reason));
            (stopped, stage(&mut state, hint))
        };
        for stop in stopped {
            stop.notify_one();
        }
        deliver(delivery);
    }

    /// The authorization server would not grant `scopes` to `context`'s
    /// target: its need stops asking for them, so later links ask only for
    /// what can be granted. A fresh 403 asks again.
    pub(super) fn withdraw_step_up(&self, scope: &str, context: &AuthContext, scopes: &[String]) {
        if let Some(needs) = self.lock().needs.get_mut(scope) {
            for need in needs
                .iter_mut()
                .filter(|need| same_target(&need.context, context))
            {
                need.required.retain(|required| !scopes.contains(required));
            }
        }
    }

    /// A logout (elsewhere) moved `scope` to `generation`: this process's
    /// links prepared before it are withdrawn, and only those. Needs stay,
    /// those recorded since and before alike (a logout leaves the backend
    /// needing a sign-in all the more), as do links prepared since; the hint
    /// moves on to the next link, or clears.
    pub(super) fn retire_superseded(&self, scope: &str, backend: &str, generation: &str) {
        let (stopped, delivery) = {
            let mut state = self.lock();
            let Some(logins) = state.pending.get_mut(scope) else {
                return;
            };
            let mut stopped = Vec::new();
            logins.retain(|login| {
                let stale = login.generation.at != generation;
                if stale {
                    stopped.push(Arc::clone(&login.stop));
                }
                !stale
            });
            if stopped.is_empty() {
                return;
            }
            let next = logins.first().map(Login::hint);
            if next.is_none() {
                state.pending.remove(scope);
            }
            let hint = next
                .unwrap_or_else(|| cleared(scope, backend, super::hint::ClearReason::LoggedOut));
            (stopped, stage(&mut state, hint))
        };
        for stop in stopped {
            stop.notify_one();
        }
        deliver(delivery);
    }

    /// Forgets everything about a scope, as after logging out.
    pub(super) fn reset(&self, scope: &str, backend: &str) {
        let (stopped, delivery) = {
            let mut state = self.lock();
            state.needs.remove(scope);
            let stopped = state.pending.remove(scope).unwrap_or_default();
            (
                stopped,
                stage(
                    &mut state,
                    cleared(scope, backend, super::hint::ClearReason::LoggedOut),
                ),
            )
        };
        for login in stopped {
            login.stop.notify_one();
        }
        deliver(delivery);
    }
}

/// One hint's delivery: staged under the registry lock, together with the
/// state change that produced it, and run outside it.
struct Delivery {
    sequence: u64,
    /// A scope's current hint, rather than a transient diagnostic.
    persistent: bool,
    hint: AuthHint,
    sinks: Vec<Arc<Sink>>,
    line: Option<String>,
}

/// Makes `hint` the scope's current hint and numbers it, unless nothing
/// changes. Called under the registry lock by whatever produced the hint,
/// so the current hint and the numbering follow the state's own order.
fn stage(state: &mut State, hint: AuthHint) -> Option<Delivery> {
    let current = state.hints.get(&hint.scope).map(|(_, current)| current);
    if current == Some(&hint) || (hint.cleared && current.is_none()) {
        return None;
    }
    state.sequence += 1;
    let sequence = state.sequence;
    if hint.cleared {
        state.hints.remove(&hint.scope);
    } else {
        state
            .hints
            .insert(hint.scope.clone(), (sequence, hint.clone()));
    }
    state.sinks.retain(|(_, sink)| sink.strong_count() != 0);
    let sinks: Vec<_> = state
        .sinks
        .iter()
        .filter(|(scopes, _)| scopes.contains(&hint.scope))
        .filter_map(|(_, sink)| sink.upgrade())
        .collect();
    let line = if sinks.is_empty() && !stderr_is_logged() {
        headless_line(&mut state.printed, &hint)
    } else {
        None
    };
    Some(Delivery {
        sequence,
        persistent: true,
        hint,
        sinks,
        line,
    })
}

/// Picks `hint`'s sinks without making it the scope's current hint: a later
/// subscriber is not shown it. It takes the number of the latest persistent
/// hint, without advancing it: delivered late, it never hides a persistent
/// hint staged before it, and overtaken by a newer one it is dropped, so
/// its snapshot of the link cannot bring back a link that was cleared.
fn stage_transient(state: &mut State, hint: AuthHint) -> Delivery {
    state.sinks.retain(|(_, sink)| sink.strong_count() != 0);
    let sinks: Vec<_> = state
        .sinks
        .iter()
        .filter(|(scopes, _)| scopes.contains(&hint.scope))
        .filter_map(|(_, sink)| sink.upgrade())
        .collect();
    let line = if sinks.is_empty() && !stderr_is_logged() {
        headless_line(&mut state.printed, &hint)
    } else {
        None
    };
    Delivery {
        sequence: state.sequence,
        persistent: false,
        hint,
        sinks,
        line,
    }
}

/// Runs a staged delivery, outside the registry lock.
fn deliver(delivery: Option<Delivery>) {
    let Some(Delivery {
        sequence,
        persistent,
        hint,
        sinks,
        line,
    }) = delivery
    else {
        return;
    };
    if let Some(line) = line {
        // The link is shown, never logged.
        tracing::info!(backend = %hint.backend, "hook backend needs sign-in");
        eprintln!("{line}");
    }
    for sink in sinks {
        sink(sequence, persistent, &hint);
    }
}

/// A subscriber's sink: its deliveries are serialized, and a hint older
/// than the last one it was shown for the scope (overtaken between the
/// registry lock and delivery) is dropped. A transient diagnostic is shown
/// unless a newer persistent hint was, and never moves the watermark.
fn ordered(inner: Arc<dyn Fn(&AuthHint) + Send + Sync>) -> (Arc<Sink>, Arc<Shown>) {
    let shown: Arc<Shown> = Arc::default();
    let sink: Arc<Sink> = Arc::new({
        let shown = Arc::clone(&shown);
        move |sequence: u64, persistent: bool, hint: &AuthHint| {
            let mut shown = shown.lock().unwrap_or_else(|poison| poison.into_inner());
            let last = shown.get(&hint.scope).copied();
            if persistent {
                if last.is_some_and(|last| last >= sequence) {
                    return;
                }
                shown.insert(hint.scope.clone(), sequence);
            } else if last.is_some_and(|last| last > sequence) {
                return;
            }
            inner(hint);
        }
    });
    (sink, shown)
}

/// Whether this process's stderr is read by a Kit parent (the TUI, or a Kit
/// above a nested one), which keeps it as a diagnostic log. A sign-in link
/// printed there would be logged; that parent shows links through its own
/// channels instead.
fn stderr_is_logged() -> bool {
    crate::events::enabled() || SUBAGENT_PROCESS.load(std::sync::atomic::Ordering::Relaxed)
}

/// This process is a Kit subagent: its stderr goes to its Kit parent.
static SUBAGENT_PROCESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks this process as a Kit subagent (see [`stderr_is_logged`]).
pub(super) fn mark_subagent_process() {
    SUBAGENT_PROCESS.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The stderr line for a session nobody is attached to: once per link.
/// The link and the problem are printed once each, independently: a new
/// problem beside a link already printed is printed without the link.
fn headless_line(printed: &mut HashSet<String>, hint: &AuthHint) -> Option<String> {
    if hint.cleared {
        return None;
    }
    let link = hint
        .url
        .as_ref()
        .is_some_and(|url| printed.insert(url.clone()));
    let problem = hint
        .problem
        .as_ref()
        .is_some_and(|problem| printed.insert(format!("{}\n{problem}", hint.scope)));
    let shown = match (link, problem) {
        (false, false) => return None,
        (true, true) => hint.clone(),
        (true, false) => AuthHint {
            problem: None,
            ..hint.clone()
        },
        (false, true) => AuthHint {
            url: None,
            ..hint.clone()
        },
    };
    shown.notice().map(|text| format!("kit: {text}"))
}

/// What an operation learned about a target's sign-in.
#[derive(Clone, Copy)]
enum Settle<'a> {
    /// It holds a credential with these step-up scopes (none: only the
    /// target's own).
    Granted(&'a [String]),
    /// A login ended without a grant.
    Ended,
}

pub(super) struct Preparing(String);

impl Drop for Preparing {
    fn drop(&mut self) {
        registry().lock().preparing.remove(&self.0);
    }
}

/// A loopback listener for one sign-in, on a port the system chooses, and
/// the redirect that reaches it.
pub(super) async fn bind() -> std::io::Result<(TcpListener, String)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let redirect = format!("http://{}/callback", listener.local_addr()?);
    Ok((listener, redirect))
}

/// How a login's listener finished.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Its callback completed the login: the grant is stored.
    Completed,
    /// The link expired, or the user denied consent.
    Ended,
    /// The issuer granted fewer than the step-up scopes the login required
    /// (named): the SDK saved the grant and ended the login.
    NotGranted(Vec<String>),
    /// The login is no longer wanted, as when another process signed in.
    Stopped,
    /// The backend was logged out (in any Kit process) after the login was
    /// prepared: it stores no grant.
    LoggedOut,
    /// Whether a logout happened during the exchange could not be told: the
    /// sign-in is refused and its grant removed; the user signs in again.
    Unverified,
}

/// Receives `login`'s callback on its own `listener` and completes the
/// login, until it completes, ends or is stopped. Other requests, and
/// callbacks for any other `state`, are refused and change nothing; the
/// first callback with its `state` completes or ends it.
pub(super) async fn serve(listener: &TcpListener, login: &Login) -> Outcome {
    let Ok(address) = listener.local_addr() else {
        return Outcome::Ended;
    };
    loop {
        let remaining = login
            .expires_at
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO);
        use futures_util::future::{Either, select};
        let stopped = std::pin::pin!(login.stop.notified());
        let accept = std::pin::pin!(tokio::time::timeout(remaining, listener.accept()));
        let accepted = match select(stopped, accept).await {
            Either::Left(((), _)) => {
                let budget = OperationBudget::until(Instant::now() + Duration::from_secs(30));
                let _ = login
                    .oauth
                    .cancel_login(login.pending.clone(), budget)
                    .await;
                return Outcome::Stopped;
            }
            Either::Right((Err(_), _)) => return Outcome::Ended,
            Either::Right((Ok(accepted), _)) => accepted,
        };
        let Ok((mut stream, _)) = accepted else {
            // A persistent accept error (too many open files, say) must not
            // spin; the expiry and stop checks above still end the login.
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let target = match tokio::time::timeout(Duration::from_secs(5), request_target(&mut stream))
            .await
        {
            Ok(Ok(target))
                if target.starts_with("/callback?")
                    && has_query_param(&target, "state")
                    && (has_query_param(&target, "code") || has_query_param(&target, "error")) =>
            {
                target
            }
            _ => {
                respond(&mut stream, false).await;
                continue;
            }
        };
        let Ok(callback) = url::Url::parse(&format!("http://{address}{target}")) else {
            respond(&mut stream, false).await;
            continue;
        };
        if query(&callback, "state").as_deref() != Some(login.state.as_str()) {
            respond(&mut stream, false).await;
            continue;
        }
        // A link shown before a logout never signs in. An unreadable marker
        // refuses this callback; the link still works, so the user can retry.
        match login.generation.current(&login.scope).await {
            Ok(true) => {}
            Ok(false) => {
                respond(&mut stream, false).await;
                let budget = OperationBudget::until(Instant::now() + Duration::from_secs(30));
                let _ = login
                    .oauth
                    .cancel_login(login.pending.clone(), budget)
                    .await;
                return Outcome::LoggedOut;
            }
            Err(_) => {
                tracing::warn!(backend = %login.context.backend_id, "hook sign-in refused: the logout marker could not be read");
                respond(&mut stream, false).await;
                continue;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        let context = AuthContext {
            deadline,
            ..login.context.clone()
        };
        let store = super::store::Store::new(login.generation.storage.clone(), &login.scope);
        // What the exchange did not write, so only its own write is undone.
        let before = store
            .token_generations(OperationBudget::until(deadline))
            .await
            .ok();
        let completed = login
            .oauth
            .complete(context, callback, OperationBudget::until(deadline))
            .await;
        // A logout during the exchange: its marker was moved before it
        // removed anything, so whatever the exchange stored is removed again
        // here. Checked whatever `complete` returned: an under-granted step-up
        // stores its grant and still fails access_denied. Removing is
        // idempotent, so a failed exchange that stored nothing costs nothing.
        // Only a verified generation accepts the grant: a logout meanwhile,
        // or a marker that cannot be read, undoes this exchange's write
        // (and only that) and never reports a sign-in.
        let verified = login.generation.current(&login.scope).await;
        if !matches!(verified, Ok(true)) {
            let budget = OperationBudget::until(Instant::now() + Duration::from_secs(30));
            let ours = completed
                .as_ref()
                .ok()
                .map(|credential| credential.attempt_id.clone());
            let discarded = match (&before, ours.as_deref()) {
                (_, Some(ours)) => {
                    store
                        .discard_exchange(&before.clone().unwrap_or_default(), Some(ours), budget)
                        .await
                }
                (Some(before), None) => store.discard_exchange(before, None, budget).await,
                // Nothing to tell this exchange's write from another's.
                (None, None) => Ok(0),
            };
            if let Err(error) = discarded {
                let code = error.auth_code().map_or("failed", AuthErrorCode::as_str);
                tracing::warn!(backend = %login.context.backend_id, code, "a hook sign-in that could not be accepted could not be removed");
            }
            if verified.is_err() {
                tracing::warn!(backend = %login.context.backend_id, "a hook sign-in was refused: the logout marker could not be read");
            }
            if completed
                .as_ref()
                .is_err_and(|error| error.login_ended() == Some(false))
            {
                let _ = login
                    .oauth
                    .cancel_login(login.pending.clone(), budget)
                    .await;
            }
            respond(&mut stream, false).await;
            return if verified.is_err() {
                Outcome::Unverified
            } else {
                Outcome::LoggedOut
            };
        }
        respond(&mut stream, completed.is_ok()).await;
        let error = match completed {
            Ok(_) => return Outcome::Completed,
            Err(error) => error,
        };
        // The SDK says whether the login is still pending: after an invalid
        // callback, say, its link still works and Kit keeps listening.
        if error.login_ended() == Some(false) {
            // An invalid callback (access_denied) changes nothing; a host
            // refusal is shown beside the link, which still works.
            if error.auth_code() != Some(AuthErrorCode::AccessDenied) {
                registry().problem(
                    &login.scope,
                    &login.context.backend_id,
                    super::not_completed(&error),
                );
            }
            continue;
        }
        return if error.auth_code() == Some(AuthErrorCode::AccessDenied)
            && !error.required_scopes().is_empty()
        {
            Outcome::NotGranted(error.required_scopes().to_vec())
        } else {
            Outcome::Ended
        };
    }
}

pub(super) fn event_context(
    backend: &str,
    destination: &str,
    authentication: Option<serde_json::Value>,
) -> AuthContext {
    AuthContext {
        backend_id: backend.to_owned(),
        authentication,
        destination: destination.to_owned(),
        purpose: AuthPurpose::Event,
        deadline: Instant::now() + Duration::from_secs(30),
    }
}

/// Callback requests are a GET line and headers; anything larger is refused.
const MAX_CALLBACK_BYTES: usize = 16 * 1024;

async fn request_target(stream: &mut tokio::net::TcpStream) -> Result<String, ()> {
    use tokio::io::AsyncReadExt as _;
    let mut bytes = Vec::new();
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        if bytes.len() >= MAX_CALLBACK_BYTES {
            return Err(());
        }
        let mut chunk = [0; 1024];
        let read = stream.read(&mut chunk).await.map_err(|_| ())?;
        if read == 0 {
            return Err(());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let request = std::str::from_utf8(&bytes).map_err(|_| ())?;
    let mut parts = request.lines().next().ok_or(())?.split_whitespace();
    if parts.next() != Some("GET") {
        return Err(());
    }
    parts.next().map(str::to_owned).ok_or(())
}

fn has_query_param(target: &str, name: &str) -> bool {
    target.split_once('?').is_some_and(|(_, query)| {
        query
            .split('&')
            .filter_map(|field| field.split_once('='))
            .any(|(key, value)| key == name && !value.is_empty())
    })
}

/// The same page Kit shows after an MCP sign-in.
async fn respond(stream: &mut tokio::net::TcpStream, success: bool) {
    use tokio::io::AsyncWriteExt as _;
    let (status, state, badge, title, heading, message) = if success {
        (
            "200 OK",
            "",
            "Success",
            "Authentication successful",
            "Authentication successful.",
            "You can close this tab and return to Kit.",
        )
    } else {
        (
            "400 Bad Request",
            "failed",
            "Failed",
            "Authentication failed",
            "Let\u{2019}s try that again.",
            "Return to Kit to restart authentication. You can close this tab.",
        )
    };
    let body = include_str!("../../tools/mcp/auth-page.html")
        .replace("{title}", title)
        .replace("{state}", state)
        .replace("{badge}", badge)
        .replace("{heading}", heading)
        .replace("{message}", message)
        .replace("{detail}", "");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Adapts `notify` to a client without Kit's status line: one notice per new
/// link, and one when a backend whose link it showed finishes signing in.
pub(crate) fn notices(
    notify: impl Fn(&super::hint::HookHint, String) + Send + Sync + 'static,
) -> impl Fn(&super::hint::HookHint) + Send + Sync + 'static {
    let shown: Mutex<HashSet<String>> = Mutex::default();
    move |hint: &super::hint::HookHint| {
        let text = {
            let mut shown = shown.lock().unwrap_or_else(|poison| poison.into_inner());
            // One line per link and per problem, for each agent's backend.
            let owner = format!(
                "{}\n{}\n",
                hint.agent.as_deref().unwrap_or(""),
                hint.backend
            );
            match (&hint.url, &hint.problem) {
                (Some(url), problem) if !hint.cleared => {
                    let key = match problem {
                        Some(problem) => format!("{owner}{url}\n{problem}"),
                        None => format!("{owner}{url}"),
                    };
                    if !shown.insert(key) {
                        return;
                    }
                }
                (None, Some(problem)) if !hint.cleared => {
                    if !shown.insert(format!("{owner}problem\n{problem}")) {
                        return;
                    }
                }
                _ => {
                    let before = shown.len();
                    shown.retain(|key| !key.starts_with(&owner));
                    if before == shown.len()
                        || hint.reason != Some(super::hint::ClearReason::SignedIn)
                    {
                        return;
                    }
                }
            }
            hint.notice_text()
        };
        if let Some(text) = text {
            notify(hint, text);
        }
    }
}

#[cfg(test)]
pub(super) mod test_support {
    /// This process's pending logins for `scope`.
    pub(in crate::ahp::auth) fn pending(scope: &str) -> Vec<super::Login> {
        super::registry()
            .lock()
            .pending
            .get(scope)
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;

    #[test]
    fn a_hint_overtaken_on_the_way_is_dropped() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (sink, _) = ordered(Arc::new({
            let seen = Arc::clone(&seen);
            move |hint: &AuthHint| seen.lock().unwrap().push(hint.cleared)
        }));
        let link = AuthHint {
            scope: "test#test.order".into(),
            backend: "test.order".into(),
            url: Some("https://auth.example/a".into()),
            expires_at_ms: None,
            cleared: false,
            signed_in: false,
            reason: None,
            problem: None,
        };
        let done = cleared(
            "test#test.order",
            "test.order",
            super::super::hint::ClearReason::SignedIn,
        );
        // The later state (signed in) arrives first; the older link that
        // was staged before it must not replace it.
        sink(2, true, &done);
        sink(1, true, &link);
        assert_eq!(*seen.lock().unwrap(), [true]);
    }

    #[test]
    fn a_diagnostic_neither_hides_nor_outlives_a_persistent_hint() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (sink, _) = ordered(Arc::new({
            let seen = Arc::clone(&seen);
            move |hint: &AuthHint| {
                seen.lock()
                    .unwrap()
                    .push((hint.url.is_some(), hint.cleared))
            }
        }));
        let scope = "test#test.diagnostic-order";
        let link = AuthHint {
            scope: scope.into(),
            backend: "test.diagnostic-order".into(),
            url: Some("https://auth.example/a".into()),
            expires_at_ms: None,
            cleared: false,
            signed_in: false,
            reason: None,
            problem: Some("requires scopes (x) outside the ones Kit allows for it.".into()),
        };
        let done = cleared(
            scope,
            "test.diagnostic-order",
            super::super::hint::ClearReason::SignedIn,
        );
        // A diagnostic staged after the clear (number 5) is delivered first:
        // the clear still arrives.
        sink(
            5,
            false,
            &AuthHint {
                url: None,
                ..link.clone()
            },
        );
        sink(5, true, &done);
        // One staged before a newer clear, delivered after it, is dropped:
        // its link snapshot does not come back.
        sink(4, false, &link);
        assert_eq!(*seen.lock().unwrap(), [(false, false), (false, true)]);
    }

    #[test]
    fn headless_sessions_print_one_line_per_link() {
        let mut printed = HashSet::new();
        let hint = |url: &str| AuthHint {
            scope: "test#test.headless".into(),
            backend: "test.headless".into(),
            url: Some(url.into()),
            expires_at_ms: None,
            cleared: false,
            signed_in: false,
            reason: None,
            problem: None,
        };
        let first = hint("https://auth.example/a");
        assert_eq!(
            headless_line(&mut printed, &first).as_deref(),
            Some(
                "kit: Hook backend `test.headless` needs you to sign in. Open https://auth.example/a to approve Kit."
            )
        );
        assert_eq!(headless_line(&mut printed, &first), None);
        assert!(headless_line(&mut printed, &hint("https://auth.example/b")).is_some());
        let cleared = AuthHint {
            cleared: true,
            ..hint("https://auth.example/c")
        };
        assert_eq!(headless_line(&mut printed, &cleared), None);
    }

    #[test]
    fn a_headless_problem_beside_a_printed_link_is_printed_once_without_it() {
        let mut printed = HashSet::new();
        let link = AuthHint {
            scope: "test#test.headless-problem".into(),
            backend: "test.headless-problem".into(),
            url: Some("https://auth.example/a".into()),
            expires_at_ms: None,
            cleared: false,
            signed_in: false,
            reason: None,
            problem: None,
        };
        assert!(headless_line(&mut printed, &link).is_some());
        let with_problem = AuthHint {
            problem: Some("could not prepare the upload sign-in.".into()),
            ..link.clone()
        };
        let line = headless_line(&mut printed, &with_problem).unwrap();
        assert!(
            line.contains("could not prepare the upload sign-in"),
            "{line}"
        );
        assert!(!line.contains("https://auth.example/a"), "{line}");
        assert_eq!(headless_line(&mut printed, &with_problem), None);
    }
}
