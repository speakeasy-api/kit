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
    /// Why Kit could not prepare a sign-in the backend needs, with what to
    /// check: sanitized, never a provider message.
    pub problem: Option<String>,
}

/// The `_meta` key that marks a sign-in hint, on a notice or Kit's own
/// notification. A hint is for a person: a parent Kit never passes a child's
/// hint to its model.
pub(crate) const HOOK_AUTH_META: &str = "kit/hooks/auth";

impl AuthHint {
    /// `_meta` for a notice carrying this hint.
    pub(crate) fn meta(&self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::Map::from_iter([(
            HOOK_AUTH_META.to_owned(),
            serde_json::Value::Object(serde_json::Map::from_iter([
                ("backend".into(), self.backend.clone().into()),
                ("url".into(), self.url.clone().into()),
                ("expiresAt".into(), self.expires_at_ms.into()),
                ("cleared".into(), self.cleared.into()),
            ])),
        )])
    }

    /// Plain text for a client without Kit's status line: once for a new
    /// link and once when sign-in finishes. An expired link says nothing;
    /// the next turn shows its replacement.
    pub(crate) fn notice(&self) -> Option<String> {
        match (&self.url, &self.problem) {
            (Some(url), Some(problem)) if !self.cleared => {
                return Some(format!(
                    "Hook backend `{}` needs you to sign in. Open {url} to approve Kit. Another of its endpoints could not be prepared: {problem}",
                    self.backend
                ));
            }
            (None, Some(problem)) if !self.cleared => {
                return Some(format!(
                    "Hook backend `{}` needs you to sign in, but Kit could not start it: {problem}",
                    self.backend
                ));
            }
            _ => {}
        }
        match (&self.url, self.signed_in) {
            (Some(url), _) if !self.cleared => Some(format!(
                "Hook backend `{}` needs you to sign in. Open {url} to approve Kit.",
                self.backend
            )),
            (_, true) => Some(format!("Hook backend `{}` is signed in.", self.backend)),
            _ => None,
        }
    }
}

/// A target whose last operation needed a login, kept to prepare exactly it.
#[derive(Clone)]
pub(super) struct Need {
    pub(super) context: AuthContext,
    pub(super) required: Vec<String>,
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
    /// The scopes it was prepared with beyond the target's own: a step-up
    /// login's. Completing it meets a need for no more than these.
    pub(super) required: Vec<String>,
    pub(super) url: String,
    pub(super) expires_at: SystemTime,
    /// Stops the login's listener once it is no longer wanted.
    stop: Arc<Notify>,
}

impl Login {
    pub(super) fn new(
        scope: &str,
        oauth: Arc<ManagedOAuth>,
        context: &AuthContext,
        required: &[String],
        pending: PendingLogin,
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
            problem: None,
        }
    }
}

pub(super) fn query(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

type Sink = dyn Fn(&AuthHint) + Send + Sync;

/// Keyed by scope: a backend id within the hooks file that declares it, so
/// two workspaces with the same backend id never share hints or needs.
#[derive(Default)]
struct State {
    needs: HashMap<String, Vec<Need>>,
    preparing: HashSet<String>,
    /// This process's pending logins, one per target; the first is shown.
    pending: HashMap<String, Vec<Login>>,
    hints: HashMap<String, AuthHint>,
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

fn cleared(scope: &str, backend: &str, signed_in: bool) -> AuthHint {
    AuthHint {
        scope: scope.to_owned(),
        backend: backend.to_owned(),
        url: None,
        expires_at_ms: None,
        cleared: true,
        signed_in,
        problem: None,
    }
}

impl Registry {
    fn lock(&self) -> MutexGuard<'_, State> {
        // No callback, await or drop of a sink runs under this guard.
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Delivers `hint` outside the lock: to subscribers for its scope, or
    /// as one stderr line per link when nobody is attached.
    pub(super) fn publish(&self, hint: AuthHint) {
        let (sinks, print) = {
            let mut state = self.lock();
            if state.hints.get(&hint.scope) == Some(&hint)
                || (hint.cleared && !state.hints.contains_key(&hint.scope))
            {
                return;
            }
            if hint.cleared {
                state.hints.remove(&hint.scope);
            } else {
                state.hints.insert(hint.scope.clone(), hint.clone());
            }
            state.sinks.retain(|(_, sink)| sink.strong_count() != 0);
            let sinks: Vec<_> = state
                .sinks
                .iter()
                .filter(|(scopes, _)| scopes.contains(&hint.scope))
                .filter_map(|(_, sink)| sink.upgrade())
                .collect();
            let line = if sinks.is_empty() {
                headless_line(&mut state.printed, &hint)
            } else {
                None
            };
            (sinks, line)
        };
        if let Some(line) = print {
            // The link is shown, never logged.
            tracing::info!(backend = %hint.backend, "hook backend needs sign-in");
            eprintln!("{line}");
        }
        for sink in sinks {
            sink(&hint);
        }
    }

    pub(super) fn subscribe(
        &self,
        scopes: HashSet<String>,
        sink: impl Fn(&AuthHint) + Send + Sync + 'static,
    ) -> HintSubscription {
        // One subscriber's deliveries are serialized: live updates wait
        // until the snapshot below is replayed, so a completion can never
        // arrive before the link it clears. Sinks still run outside the
        // registry lock.
        let inner: Arc<Sink> = Arc::new(sink);
        let gate = Arc::new(Mutex::new(()));
        let sink: Arc<Sink> = Arc::new({
            let (inner, gate) = (Arc::clone(&inner), Arc::clone(&gate));
            move |hint: &AuthHint| {
                let _turn = gate.lock().unwrap_or_else(|poison| poison.into_inner());
                inner(hint);
            }
        });
        let (current, turn) = {
            let mut state = self.lock();
            state.sinks.push((scopes.clone(), Arc::downgrade(&sink)));
            let current: Vec<_> = state
                .hints
                .values()
                .filter(|hint| scopes.contains(&hint.scope))
                .cloned()
                .collect();
            // Taken before the registry lock is released (registry, then
            // gate; never the reverse).
            let turn = gate.lock().unwrap_or_else(|poison| poison.into_inner());
            (current, turn)
        };
        // A session attached after a link was shown still sees it.
        for hint in &current {
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
            Some(existing) => *existing = need,
            None => needs.push(need),
        }
    }

    /// A target that now has a usable credential, signed in here or by
    /// another Kit process, needs no login: its pending login stops, and
    /// the hint moves on to another target's link or clears once none of
    /// the scope's targets needs one. A step-up need (one that requires
    /// more scopes) is not met by a usable credential: it and its link stay
    /// until a login or preparation with those scopes succeeds.
    pub(super) fn usable(&self, scope: &str, context: &AuthContext) {
        self.settle(scope, context, Settle::Usable);
    }

    /// `context`'s target was prepared with `required` scopes beyond its
    /// own. Its need is met when it asks for no more than those; a step-up
    /// need that asks for more (recorded meanwhile, say) stays, as after
    /// [`Self::usable`].
    pub(super) fn prepared(&self, scope: &str, context: &AuthContext, required: &[String]) {
        self.settle(scope, context, Settle::Granted(required));
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
    /// still works. Later turns keep showing it rather than prepare another.
    pub(super) fn has_live_login(&self, scope: &str, context: &AuthContext) -> bool {
        let now = SystemTime::now();
        self.lock().pending.get(scope).is_some_and(|logins| {
            logins
                .iter()
                .any(|login| same_target(&login.context, context) && login.expires_at > now)
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
        let hint = self
            .lock()
            .pending
            .get(scope)
            .and_then(|logins| logins.first())
            .map(Login::hint);
        if let Some(hint) = hint {
            self.publish(hint);
        }
    }

    /// Shows why a sign-in the scope needs could not be prepared.
    /// A link already shown for the scope stays: the problem is added to it.
    pub(super) fn problem(&self, scope: &str, backend: &str, problem: String) {
        let link = self
            .lock()
            .pending
            .get(scope)
            .and_then(|logins| logins.first())
            .map(Login::hint);
        self.publish(AuthHint {
            problem: Some(problem),
            ..link.unwrap_or_else(|| AuthHint {
                cleared: false,
                ..cleared(scope, backend, false)
            })
        });
    }

    /// `login` completed. The scope's other targets (an upload on another
    /// resource, say) keep their logins, and the hint moves on to the next.
    pub(super) fn completed(&self, login: &Login) {
        self.settle(
            &login.scope,
            &login.context,
            Settle::Granted(&login.required),
        );
    }

    /// `login` ended without a grant (its link expired or consent was
    /// denied): it is withdrawn, and the next turn prepares a new one. A
    /// newer login for the same target, which replaced it, stays.
    pub(super) fn ended(&self, login: &Login) {
        self.settle(&login.scope, &login.context, Settle::Ended(&login.state));
    }

    /// Ends the pending login for `context`'s target, or for
    /// [`Settle::Ended`] only the login with that `state` (nothing, if it is
    /// no longer pending). Signed in, the target needs no login either. The
    /// hint shows the scope's next link, or clears once no target needs one.
    /// Every check is made under the lock that removes, so a login that
    /// replaced another meanwhile, or a step-up need, is never withdrawn.
    fn settle(&self, scope: &str, context: &AuthContext, how: Settle<'_>) {
        let signed_in = !matches!(how, Settle::Ended(_));
        let (stopped, hint) = {
            let mut state = self.lock();
            let skip = match how {
                // A login without a state matches no callback, and no other.
                Settle::Ended(only) => {
                    only.is_empty()
                        || !state
                            .pending
                            .get(scope)
                            .is_some_and(|logins| logins.iter().any(|login| login.state == only))
                }
                Settle::Usable => {
                    let known =
                        state.needs.contains_key(scope) || state.pending.contains_key(scope);
                    !known || unmet(&state, scope, context, &[])
                }
                Settle::Granted(granted) => unmet(&state, scope, context, granted),
            };
            if skip {
                return;
            }
            let only = match how {
                Settle::Ended(only) => Some(only),
                _ => None,
            };
            let mut needed = state.needs.contains_key(scope);
            if signed_in && let Some(needs) = state.needs.get_mut(scope) {
                needs.retain(|need| !same_target(&need.context, context));
                needed = !needs.is_empty();
                if !needed {
                    state.needs.remove(scope);
                }
            }
            let mut stopped = Vec::new();
            let next = state.pending.get_mut(scope).and_then(|logins| {
                logins.retain(|login| {
                    let ended = only.map_or_else(
                        || same_target(&login.context, context),
                        |only| login.state == only,
                    );
                    if ended {
                        stopped.push(Arc::clone(&login.stop));
                    }
                    !ended
                });
                logins.first().map(Login::hint)
            });
            if next.is_none() {
                state.pending.remove(scope);
            }
            let hint = match next {
                Some(next) => Some(next),
                // Another target still needs a sign-in: its link comes at
                // the next turn start.
                None if signed_in && needed => None,
                None => Some(cleared(scope, &context.backend_id, signed_in)),
            };
            (stopped, hint)
        };
        for stop in stopped {
            stop.notify_one();
        }
        if let Some(hint) = hint {
            self.publish(hint);
        }
    }

    /// Forgets everything about a scope, as after logging out.
    pub(super) fn reset(&self, scope: &str, backend: &str) {
        let stopped = {
            let mut state = self.lock();
            state.needs.remove(scope);
            state.pending.remove(scope).unwrap_or_default()
        };
        for login in stopped {
            login.stop.notify_one();
        }
        self.publish(cleared(scope, backend, false));
    }
}

/// The stderr line for a session nobody is attached to: once per link.
fn headless_line(printed: &mut HashSet<String>, hint: &AuthHint) -> Option<String> {
    let key = match (&hint.url, &hint.problem) {
        (Some(url), _) => url.clone(),
        (None, Some(problem)) => format!("{}\n{problem}", hint.scope),
        (None, None) => return None,
    };
    if hint.cleared || !printed.insert(key) {
        return None;
    }
    hint.notice().map(|text| format!("kit: {text}"))
}

/// Whether `context`'s target needs scopes beyond `granted`: a step-up
/// need that a credential with only those does not meet.
fn unmet(state: &State, scope: &str, context: &AuthContext, granted: &[String]) -> bool {
    state.needs.get(scope).is_some_and(|needs| {
        needs.iter().any(|need| {
            same_target(&need.context, context)
                && !need.required.iter().all(|scope| granted.contains(scope))
        })
    })
}

/// Why a target's pending login ends.
#[derive(Clone, Copy)]
enum Settle<'a> {
    /// An operation found a usable credential: not enough for a step-up.
    Usable,
    /// Signed in, or prepared, with these scopes beyond the target's own.
    Granted(&'a [String]),
    /// The login with this `state` ended without a grant.
    Ended(&'a str),
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
    /// The login is no longer wanted, as when another process signed in.
    Stopped,
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
        let deadline = Instant::now() + Duration::from_secs(30);
        let context = AuthContext {
            deadline,
            ..login.context.clone()
        };
        let completed = login
            .oauth
            .complete(context, callback, OperationBudget::until(deadline))
            .await
            .is_ok();
        respond(&mut stream, completed).await;
        // A callback with the login's state ends it either way: the SDK
        // claims the login before it exchanges the code, so after a denial
        // or a failed exchange its link no longer works.
        return if completed {
            Outcome::Completed
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
    notify: impl Fn(&AuthHint, String) + Send + Sync + 'static,
) -> impl Fn(&AuthHint) + Send + Sync + 'static {
    let shown: Mutex<HashSet<String>> = Mutex::default();
    move |hint: &AuthHint| {
        let text = {
            let mut shown = shown.lock().unwrap_or_else(|poison| poison.into_inner());
            match (&hint.url, &hint.problem) {
                (Some(url), problem) if !hint.cleared => {
                    let key = match problem {
                        Some(problem) => format!("{}\n{url}\n{problem}", hint.backend),
                        None => format!("{}\n{url}", hint.backend),
                    };
                    if !shown.insert(key) {
                        return;
                    }
                }
                (None, Some(problem)) if !hint.cleared => {
                    if !shown.insert(format!("{}\nproblem\n{problem}", hint.backend)) {
                        return;
                    }
                }
                _ => {
                    let before = shown.len();
                    shown.retain(|key| !key.starts_with(&format!("{}\n", hint.backend)));
                    if before == shown.len() || !hint.signed_in {
                        return;
                    }
                }
            }
            hint.notice()
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
    fn headless_sessions_print_one_line_per_link() {
        let mut printed = HashSet::new();
        let hint = |url: &str| AuthHint {
            scope: "test#test.headless".into(),
            backend: "test.headless".into(),
            url: Some(url.into()),
            expires_at_ms: None,
            cleared: false,
            signed_in: false,
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
}
