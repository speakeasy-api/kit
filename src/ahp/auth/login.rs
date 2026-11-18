//! Per-backend sign-in state shared by every Hooks instance in this process:
//! which backends need a login, their pending login links, the loopback
//! listeners that receive their callbacks, and the hints shown to the user.
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock, Mutex, MutexGuard, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use agenthooksprotocol::{
    adapters::registered::{AuthContext, AuthPurpose},
    auth::managed::{ManagedOAuth, OperationBudget, PendingLogin},
};
use tokio::net::TcpListener;

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
        if let Some(problem) = self.problem.as_ref().filter(|_| !self.cleared) {
            return Some(format!(
                "Hook backend `{}` needs you to sign in, but Kit could not start it: {problem}",
                self.backend
            ));
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

/// A stored pending login: the provider that created it, its target, and
/// the `state` its callback carries.
#[derive(Clone)]
pub(super) struct Login {
    pub(super) scope: String,
    pub(super) oauth: Arc<ManagedOAuth>,
    pub(super) context: AuthContext,
    pub(super) state: String,
    pub(super) url: String,
    pub(super) expires_at: SystemTime,
    /// Where the login is recorded for listeners in other processes.
    pub(super) storage: crate::credentials::CredentialStorage,
    pub(super) client: super::network::ClientBuilder,
}

impl Login {
    pub(super) fn new(
        scope: &str,
        oauth: Arc<ManagedOAuth>,
        context: &AuthContext,
        pending: &PendingLogin,
        storage: &crate::credentials::CredentialStorage,
        client: super::network::ClientBuilder,
    ) -> Self {
        Self {
            scope: scope.to_owned(),
            oauth,
            context: context.clone(),
            state: query(pending.authorization_url(), "state").unwrap_or_default(),
            url: pending.authorization_url().to_string(),
            expires_at: pending.expires_at(),
            storage: storage.clone(),
            client,
        }
    }
}

pub(super) fn query(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

struct Pending {
    url: String,
    expires_at: SystemTime,
    port: u16,
    logins: Vec<Login>,
}

type Sink = dyn Fn(&AuthHint) + Send + Sync;

/// Keyed by scope: a backend id within the hooks file that declares it, so
/// two workspaces with the same backend id never share hints or needs.
#[derive(Default)]
struct State {
    needs: HashMap<String, Vec<Need>>,
    preparing: HashSet<String>,
    pending: HashMap<String, Pending>,
    /// Ports this process is listening on.
    listening: HashSet<u16>,
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
        let sink: Arc<Sink> = Arc::new(sink);
        let current: Vec<_> = {
            let mut state = self.lock();
            state.sinks.push((scopes.clone(), Arc::downgrade(&sink)));
            state
                .hints
                .values()
                .filter(|hint| scopes.contains(&hint.scope))
                .cloned()
                .collect()
        };
        // A session attached after a link was shown still sees it.
        for hint in &current {
            sink(hint);
        }
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

    /// A target that now has a usable credential needs no login. When none
    /// of the scope's targets does, its hint is cleared.
    pub(super) fn usable(&self, scope: &str, context: &AuthContext) {
        {
            let mut state = self.lock();
            let Some(needs) = state.needs.get_mut(scope) else {
                return;
            };
            needs.retain(|need| !same_target(&need.context, context));
            if !needs.is_empty() {
                return;
            }
            state.needs.remove(scope);
            if let Some(pending) = state.pending.get_mut(scope) {
                pending
                    .logins
                    .retain(|login| !same_target(&login.context, context));
                if pending.logins.is_empty() {
                    state.pending.remove(scope);
                }
            }
        }
        self.publish(cleared(scope, &context.backend_id, true));
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

    /// Records a stored pending login. A login `prepare` reused keeps the
    /// same link, so repeated turns show the same hint. Returns whether this
    /// process should start listening on `port`; [`Self::show`] then shows
    /// the link.
    pub(super) fn offer(&self, login: Login, pending: &PendingLogin, port: u16) -> bool {
        let url = pending.authorization_url().to_string();
        let mut state = self.lock();
        let entry = state
            .pending
            .entry(login.scope.clone())
            .or_insert_with(|| Pending {
                url: url.clone(),
                expires_at: pending.expires_at(),
                port,
                logins: Vec::new(),
            });
        entry
            .logins
            .retain(|existing| !same_target(&existing.context, &login.context));
        entry.logins.push(login);
        if entry.expires_at < pending.expires_at() || entry.url != url {
            entry.url = url;
            entry.expires_at = entry.expires_at.max(pending.expires_at());
        }
        entry.port = port;
        // A process that lost the port retries on later turns.
        state.listening.insert(port)
    }

    /// Shows the scope's pending link, once a listener (here or in another
    /// Kit process) can receive its callback.
    pub(super) fn show(&self, scope: &str) {
        let hint = {
            let state = self.lock();
            let Some(pending) = state.pending.get(scope) else {
                return;
            };
            let Some(backend) = pending
                .logins
                .first()
                .map(|login| login.context.backend_id.clone())
            else {
                return;
            };
            AuthHint {
                scope: scope.to_owned(),
                backend,
                url: Some(pending.url.clone()),
                expires_at_ms: Some(unix_ms(pending.expires_at)),
                cleared: false,
                signed_in: false,
                problem: None,
            }
        };
        self.publish(hint);
    }

    /// Nothing can receive the scope's callback: its link is withdrawn, so
    /// none is shown that cannot work.
    pub(super) fn withdraw(&self, scope: &str) {
        self.lock().pending.remove(scope);
    }

    /// Shows why a sign-in the scope needs could not be prepared.
    pub(super) fn problem(&self, scope: &str, backend: &str, problem: String) {
        self.publish(AuthHint {
            scope: scope.to_owned(),
            backend: backend.to_owned(),
            url: None,
            expires_at_ms: None,
            cleared: false,
            signed_in: false,
            problem: Some(problem),
        });
    }

    /// Every pending login whose callback arrives on `port`, and the latest
    /// expiry among them. Expired logins are dropped and their hints cleared.
    pub(super) fn logins(&self, port: u16) -> Option<(Vec<Login>, SystemTime)> {
        let now = SystemTime::now();
        let (expired, live) = {
            let mut state = self.lock();
            let expired: Vec<_> = state
                .pending
                .iter()
                .filter(|(_, pending)| pending.port == port && pending.expires_at <= now)
                .map(|(scope, pending)| {
                    let backend = pending
                        .logins
                        .first()
                        .map(|login| login.context.backend_id.clone())
                        .unwrap_or_default();
                    (scope.clone(), backend)
                })
                .collect();
            for (scope, _) in &expired {
                state.pending.remove(scope);
            }
            let live: Vec<_> = state
                .pending
                .values()
                .filter(|pending| pending.port == port)
                .collect();
            let logins = live
                .iter()
                .flat_map(|pending| pending.logins.iter().cloned())
                .collect::<Vec<_>>();
            let expiry = live.iter().map(|pending| pending.expires_at).max();
            (expired, expiry.map(|expiry| (logins, expiry)))
        };
        for (scope, backend) in expired {
            self.publish(cleared(&scope, &backend, false));
        }
        live
    }

    /// `login` completed. The scope's other targets (an upload on another
    /// resource, say) keep their logins, and the hint moves on to the next.
    pub(super) fn completed(&self, login: &Login) {
        let next = {
            let mut state = self.lock();
            if let Some(needs) = state.needs.get_mut(&login.scope) {
                needs.retain(|need| !same_target(&need.context, &login.context));
                if needs.is_empty() {
                    state.needs.remove(&login.scope);
                }
            }
            let remaining = state.pending.get_mut(&login.scope).and_then(|pending| {
                pending.logins.retain(|other| other.state != login.state);
                let next = pending.logins.first()?.clone();
                pending.url = next.url.clone();
                pending.expires_at = next.expires_at;
                Some(next)
            });
            if remaining.is_none() {
                state.pending.remove(&login.scope);
            }
            remaining
        };
        self.publish(match next {
            Some(next) => AuthHint {
                scope: next.scope.clone(),
                backend: next.context.backend_id.clone(),
                url: Some(next.url.clone()),
                expires_at_ms: Some(unix_ms(next.expires_at)),
                cleared: false,
                signed_in: false,
                problem: None,
            },
            None => cleared(&login.scope, &login.context.backend_id, true),
        });
    }

    /// Stops listening on `port` unless a login was offered for it since
    /// the listener last looked: checked under the lock `offer` takes, so a
    /// login is never left without a listener.
    pub(super) fn stop_if_idle(&self, port: u16) -> bool {
        let mut state = self.lock();
        let now = SystemTime::now();
        let waiting = state
            .pending
            .values()
            .any(|pending| pending.port == port && pending.expires_at > now);
        if !waiting {
            state.listening.remove(&port);
        }
        !waiting
    }

    /// Whether `login` is still waiting for its callback.
    pub(super) fn is_pending(&self, login: &Login) -> bool {
        self.lock()
            .pending
            .get(&login.scope)
            .is_some_and(|pending| {
                pending
                    .logins
                    .iter()
                    .any(|other| other.state == login.state)
            })
    }

    /// Claims `port` for a listener of this process; false if one has it.
    pub(super) fn claim(&self, port: u16) -> bool {
        self.lock().listening.insert(port)
    }

    pub(super) fn stopped(&self, port: u16) {
        self.lock().listening.remove(&port);
    }

    /// Forgets everything about a scope, as after logging out.
    pub(super) fn reset(&self, scope: &str, backend: &str) {
        {
            let mut state = self.lock();
            state.needs.remove(scope);
            state.pending.remove(scope);
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

pub(super) struct Preparing(String);

impl Drop for Preparing {
    fn drop(&mut self) {
        registry().lock().preparing.remove(&self.0);
    }
}

pub(super) fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}/callback")
}

/// What a Kit sign-in listener answers at [`PROBE`], so a process that
/// cannot bind a port can tell another Kit from a foreign program.
const PROBE: &str = "/kit-hook-sign-in";
pub(super) const PROBE_REPLY: &str = "kit hook sign-in";

/// Whether a Kit sign-in listener holds `port`.
pub(super) async fn kit_listens(port: u16) -> bool {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let probe = async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .ok()?;
        stream
            .write_all(format!("GET {PROBE} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
            .await
            .ok()?;
        let mut reply = Vec::new();
        stream.read_to_end(&mut reply).await.ok()?;
        Some(String::from_utf8_lossy(&reply).contains(PROBE_REPLY))
    };
    tokio::time::timeout(Duration::from_secs(2), probe)
        .await
        .ok()
        .flatten()
        .unwrap_or(false)
}

/// [`kit_listens`], retried: a listener answers probes even while it
/// exchanges a code, but a starting one may not accept yet.
async fn kit_listens_retrying(port: u16) -> bool {
    for attempt in 0..3 {
        if kit_listens(port).await {
            return true;
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    false
}

/// Who receives callbacks on a port.
pub(super) enum Bound {
    /// This process: serve the listener.
    Here(TcpListener),
    /// Another Kit process, which completes any process's login from the
    /// shared store.
    Peer,
    /// Another program: no sign-in can complete on this port.
    Foreign,
}

/// Binds `port`, or finds out who holds it. A foreign holder is reported
/// with what to change.
pub(super) async fn bind(port: u16) -> Bound {
    match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => Bound::Here(listener),
        Err(_) if kit_listens_retrying(port).await => {
            tracing::debug!(port, "another Kit process receives hook sign-in callbacks");
            Bound::Peer
        }
        Err(error) => {
            tracing::warn!(port, %error, "hook sign-in callback port is in use");
            eprintln!("kit: {}", foreign_port(port));
            Bound::Foreign
        }
    }
}

/// What to tell the user when another program holds `port`.
pub(super) fn foreign_port(port: u16) -> String {
    format!(
        "hook sign-in cannot receive its callback: port {port} is used by another program. Set oauth.redirectPort for the backend in hooks.json."
    )
}

/// Serves this process's pending logins on `listener` until none uses its
/// port.
pub(super) async fn listen_on(listener: TcpListener, port: u16) {
    loop {
        serve(
            &listener,
            port,
            || registry().logins(port),
            |login| registry().completed(login),
        )
        .await;
        if registry().stop_if_idle(port) {
            return;
        }
    }
}

/// While another Kit process receives `login`'s callback, until the login
/// ends or expires: it is completed here as soon as the shared store holds
/// its grant, and this process takes the port over (same link) if the peer
/// stops listening first.
pub(super) async fn watch_peer(login: Login, port: u16) {
    loop {
        if SystemTime::now() >= login.expires_at || !registry().is_pending(&login) {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let deadline = Instant::now() + Duration::from_secs(30);
        let context = AuthContext {
            deadline,
            ..login.context.clone()
        };
        use agenthooksprotocol::adapters::registered::AuthProvider as _;
        if matches!(login.oauth.credential(context).await, Ok(Some(_))) {
            registry().completed(&login);
            return;
        }
        if !registry().claim(port) {
            // A listener of this process serves the port now.
            return;
        }
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return listen_on(listener, port).await,
            Err(_) => registry().stopped(port),
        }
    }
}

/// Completes pending logins from callbacks on `listener`, routed by
/// `state`: this process's own logins in `logins`, else one another Kit
/// process recorded in the shared store. `logins` also reports the latest
/// expiry; serving stops when it reports none or they expire. Each
/// exchange runs on its own task, so probes and other callbacks are
/// answered meanwhile.
pub(super) async fn serve(
    listener: &TcpListener,
    port: u16,
    logins: impl Fn() -> Option<(Vec<Login>, SystemTime)>,
    mut completed: impl FnMut(&Login),
) {
    use tokio::io::AsyncWriteExt as _;
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Login>();
    loop {
        let Some((candidates, expires_at)) = logins().filter(|(logins, _)| !logins.is_empty())
        else {
            return;
        };
        let remaining = expires_at
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return;
        }
        // Wake periodically: a later prepare may add a login or extend one.
        let wait = remaining.min(Duration::from_secs(5));
        let finished = std::pin::pin!(done_rx.recv());
        let accept = std::pin::pin!(tokio::time::timeout(wait, listener.accept()));
        let accepted = match futures_util::future::select(finished, accept).await {
            futures_util::future::Either::Left((Some(login), _)) => {
                completed(&login);
                continue;
            }
            // The loop keeps a sender, so the channel never closes.
            futures_util::future::Either::Left((None, _)) => continue,
            futures_util::future::Either::Right((accepted, _)) => accepted,
        };
        let Ok(Ok((mut stream, _))) = accepted else {
            continue;
        };
        let target = match tokio::time::timeout(Duration::from_secs(5), request_target(&mut stream))
            .await
        {
            Ok(Ok(target)) if target == PROBE => {
                let _ = stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PROBE_REPLY}",
                                PROBE_REPLY.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                continue;
            }
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
        let Ok(callback) = url::Url::parse(&format!("http://127.0.0.1:{port}{target}")) else {
            respond(&mut stream, false).await;
            continue;
        };
        let state = query(&callback, "state").unwrap_or_default();
        let storage = candidates
            .first()
            .map(|login| (login.storage.clone(), login.client));
        // Only the login the callback names; any other is never touched.
        // Logins offered while this callback was awaited count too.
        let candidates = logins().map_or(candidates, |(current, _)| current);
        let matching: Vec<_> = candidates
            .into_iter()
            .filter(|login| login.state == state)
            .collect();
        let done_tx = done_tx.clone();
        tokio::spawn(async move {
            let own = !matching.is_empty();
            let mut done = None;
            for login in matching {
                let deadline = Instant::now() + Duration::from_secs(30);
                let context = AuthContext {
                    deadline,
                    ..login.context.clone()
                };
                if login
                    .oauth
                    .complete(context, callback.clone(), OperationBudget::until(deadline))
                    .await
                    .is_ok()
                {
                    done = Some(login);
                    break;
                }
            }
            let ok = match (&done, storage) {
                (Some(_), _) => true,
                // Another Kit process's login on this shared port.
                (None, Some((storage, client))) if !own => {
                    super::complete_stored(&storage, client, &state, callback).await
                }
                _ => false,
            };
            respond(&mut stream, ok).await;
            if let Some(login) = done {
                let _ = done_tx.send(login);
            }
        });
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
                (Some(url), _) if !hint.cleared => {
                    if !shown.insert(format!("{}\n{url}", hint.backend)) {
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
