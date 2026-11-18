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
    pub backend: String,
    /// Where to approve Kit. `None` once cleared.
    pub url: Option<String>,
    /// When the link stops working, in Unix milliseconds.
    pub expires_at_ms: Option<u64>,
    pub cleared: bool,
    /// Cleared because sign-in finished, rather than because the link expired.
    pub signed_in: bool,
}

impl AuthHint {
    /// Plain text for a client without Kit's status line: once for a new
    /// link and once when sign-in finishes. An expired link says nothing;
    /// the next turn shows its replacement.
    pub(crate) fn notice(&self) -> Option<String> {
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

/// A stored pending login: the provider that created it and its target.
pub(super) type Login = (Arc<ManagedOAuth>, AuthContext);

struct Pending {
    url: String,
    expires_at: SystemTime,
    /// Every stored login whose callback this backend's redirect receives.
    logins: Vec<Login>,
    listening: bool,
}

type Sink = dyn Fn(&AuthHint) + Send + Sync;

#[derive(Default)]
struct State {
    needs: HashMap<String, Vec<Need>>,
    /// Each HTTP backend's own endpoint, for failures that name only it.
    endpoints: HashMap<String, AuthContext>,
    preparing: HashSet<String>,
    pending: HashMap<String, Pending>,
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

impl Registry {
    fn lock(&self) -> MutexGuard<'_, State> {
        // No callback, await or drop of a sink runs under this guard.
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Delivers `hint` outside the lock: to subscribers for its backend, or
    /// as one stderr line per link when nobody is attached.
    pub(super) fn publish(&self, hint: AuthHint) {
        let (sinks, print) = {
            let mut state = self.lock();
            if state.hints.get(&hint.backend) == Some(&hint) {
                return;
            }
            if hint.cleared {
                state.hints.remove(&hint.backend);
            } else {
                state.hints.insert(hint.backend.clone(), hint.clone());
            }
            state.sinks.retain(|(_, sink)| sink.strong_count() != 0);
            let sinks: Vec<_> = state
                .sinks
                .iter()
                .filter(|(backends, _)| backends.contains(&hint.backend))
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
        backends: HashSet<String>,
        sink: impl Fn(&AuthHint) + Send + Sync + 'static,
    ) -> HintSubscription {
        let sink: Arc<Sink> = Arc::new(sink);
        let current: Vec<_> = {
            let mut state = self.lock();
            state.sinks.push((backends.clone(), Arc::downgrade(&sink)));
            state
                .hints
                .values()
                .filter(|hint| backends.contains(&hint.backend))
                .cloned()
                .collect()
        };
        // A session attached after a link was shown still sees it.
        for hint in &current {
            sink(hint);
        }
        HintSubscription { _sink: sink }
    }

    pub(super) fn register_endpoint(&self, context: AuthContext) {
        self.lock()
            .endpoints
            .insert(context.backend_id.clone(), context);
    }

    /// A delivery diagnostic names only the backend: its own endpoint needs
    /// a login unless a more precise target was already recorded.
    pub(super) fn delivery_failed(&self, backend: &str, required: &[String]) {
        let context = {
            let state = self.lock();
            if state.needs.contains_key(backend) {
                return;
            }
            state.endpoints.get(backend).cloned()
        };
        if let Some(context) = context {
            self.needs_login(&context, required);
        }
    }

    pub(super) fn needs_login(&self, context: &AuthContext, required: &[String]) {
        let mut state = self.lock();
        let needs = state.needs.entry(context.backend_id.clone()).or_default();
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
    /// of the backend's targets does, its hint is cleared.
    pub(super) fn usable(&self, context: &AuthContext) {
        let cleared = {
            let mut state = self.lock();
            let Some(needs) = state.needs.get_mut(&context.backend_id) else {
                return;
            };
            needs.retain(|need| !same_target(&need.context, context));
            if !needs.is_empty() {
                return;
            }
            state.needs.remove(&context.backend_id);
            if let Some(pending) = state.pending.get_mut(&context.backend_id) {
                pending
                    .logins
                    .retain(|(_, login)| !same_target(login, context));
                if pending.logins.is_empty() {
                    state.pending.remove(&context.backend_id);
                }
            }
            state.hints.contains_key(&context.backend_id)
        };
        if cleared {
            self.publish(AuthHint {
                backend: context.backend_id.clone(),
                url: None,
                expires_at_ms: None,
                cleared: true,
                signed_in: true,
            });
        }
    }

    pub(super) fn needs(&self, backend: &str) -> Vec<Need> {
        self.lock().needs.get(backend).cloned().unwrap_or_default()
    }

    pub(super) fn has_needs(&self, backend: &str) -> bool {
        self.lock().needs.contains_key(backend)
    }

    /// Claims the right to prepare `backend`'s logins; one task at a time.
    pub(super) fn begin_preparing(&self, backend: &str) -> Option<Preparing> {
        self.lock()
            .preparing
            .insert(backend.to_owned())
            .then(|| Preparing(backend.to_owned()))
    }

    /// Records a stored pending login and shows its link. A login `prepare`
    /// reused keeps the same link, so repeated turns show the same hint.
    /// Returns whether this process should start the callback listener.
    pub(super) fn offer(
        &self,
        oauth: Arc<ManagedOAuth>,
        context: &AuthContext,
        login: &PendingLogin,
    ) -> bool {
        let backend = context.backend_id.clone();
        let url = login.authorization_url().to_string();
        let (listen, hint) = {
            let mut state = self.lock();
            let pending = state
                .pending
                .entry(backend.clone())
                .or_insert_with(|| Pending {
                    url: url.clone(),
                    expires_at: login.expires_at(),
                    logins: Vec::new(),
                    listening: false,
                });
            pending
                .logins
                .retain(|(_, existing)| !same_target(existing, context));
            pending.logins.push((oauth, context.clone()));
            if pending.expires_at < login.expires_at() || pending.url != url {
                pending.url = url.clone();
                pending.expires_at = pending.expires_at.max(login.expires_at());
            }
            let listen = !pending.listening;
            pending.listening = true;
            let hint = AuthHint {
                backend: backend.clone(),
                url: Some(pending.url.clone()),
                expires_at_ms: Some(unix_ms(pending.expires_at)),
                cleared: false,
                signed_in: false,
            };
            (listen, hint)
        };
        self.publish(hint);
        listen
    }

    fn logins(&self, backend: &str) -> Option<(Vec<Login>, SystemTime)> {
        self.lock()
            .pending
            .get(backend)
            .map(|pending| (pending.logins.clone(), pending.expires_at))
    }

    /// The listener stopped: sign-in finished, the link expired, or the port
    /// belongs to another process, which will receive the callback instead.
    fn stopped(&self, backend: &str, signed_in: bool) {
        let clear = {
            let mut state = self.lock();
            let expired = state
                .pending
                .get(backend)
                .is_none_or(|pending| pending.expires_at <= SystemTime::now());
            if signed_in {
                state.needs.remove(backend);
            }
            if signed_in || expired {
                state.pending.remove(backend);
                true
            } else {
                if let Some(pending) = state.pending.get_mut(backend) {
                    pending.listening = false;
                }
                false
            }
        };
        if clear {
            self.publish(AuthHint {
                backend: backend.to_owned(),
                url: None,
                expires_at_ms: None,
                cleared: true,
                signed_in,
            });
        }
    }

    /// Forgets everything about `backend`, as after logging out.
    pub(super) fn reset(&self, backend: &str) {
        {
            let mut state = self.lock();
            state.needs.remove(backend);
            state.pending.remove(backend);
        }
        self.publish(AuthHint {
            backend: backend.to_owned(),
            url: None,
            expires_at_ms: None,
            cleared: true,
            signed_in: false,
        });
    }
}

/// The stderr line for a session nobody is attached to: once per link.
fn headless_line(printed: &mut HashSet<String>, hint: &AuthHint) -> Option<String> {
    let url = hint.url.as_ref().filter(|_| !hint.cleared)?;
    if !printed.insert(url.clone()) {
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

/// Receives `backend`'s sign-in callbacks on its fixed loopback port until
/// one completes a pending login or every link expires.
pub(super) async fn listen(backend: String, port: u16) {
    let listener = match TcpListener::bind(("127.0.0.1", port)).await {
        Ok(listener) => listener,
        Err(error) => {
            // Usually another Kit process waiting for the same backend; any
            // process sharing the credentials can complete the login.
            tracing::debug!(%backend, %error, "hook sign-in callback port unavailable");
            registry().stopped(&backend, false);
            return;
        }
    };
    let signed_in = serve(&listener, port, || registry().logins(&backend)).await;
    registry().stopped(&backend, signed_in);
}

/// Completes a pending login from callbacks on `listener`. `logins` reports
/// the current candidates and their common expiry; `None` stops serving.
pub(super) async fn serve(
    listener: &TcpListener,
    port: u16,
    logins: impl Fn() -> Option<(Vec<Login>, SystemTime)>,
) -> bool {
    loop {
        let Some((candidates, expires_at)) = logins() else {
            return false;
        };
        let remaining = expires_at
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            return false;
        }
        // Wake periodically: a later prepare may extend the expiry.
        let wait = remaining.min(Duration::from_secs(30));
        let Ok(accepted) = tokio::time::timeout(wait, listener.accept()).await else {
            continue;
        };
        let Ok((mut stream, _)) = accepted else {
            continue;
        };
        let target =
            match tokio::time::timeout(Duration::from_secs(5), request_target(&mut stream)).await {
                Ok(Ok(target))
                    if target.starts_with("/callback?")
                        && ((has_query_param(&target, "code")
                            && has_query_param(&target, "state"))
                            || has_query_param(&target, "error")) =>
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
        let mut completed = false;
        for (oauth, context) in candidates {
            let deadline = Instant::now() + Duration::from_secs(30);
            let context = AuthContext {
                deadline,
                ..context
            };
            // A callback for another login fails validation without
            // consuming that login, so each candidate may be tried.
            if oauth
                .complete(context, callback.clone(), OperationBudget::until(deadline))
                .await
                .is_ok()
            {
                completed = true;
                break;
            }
        }
        respond(&mut stream, completed).await;
        if completed {
            return true;
        }
    }
}

/// The context `prepare` and status use for a backend's own endpoint.
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
    notify: impl Fn(String) + Send + Sync + 'static,
) -> impl Fn(&AuthHint) + Send + Sync + 'static {
    let shown: Mutex<HashSet<String>> = Mutex::default();
    move |hint: &AuthHint| {
        let text = {
            let mut shown = shown.lock().unwrap_or_else(|poison| poison.into_inner());
            match &hint.url {
                Some(url) if !hint.cleared => {
                    if !shown.insert(format!("{}\n{url}", hint.backend)) {
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
            notify(text);
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
            backend: "test.headless".into(),
            url: Some(url.into()),
            expires_at_ms: None,
            cleared: false,
            signed_in: false,
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
