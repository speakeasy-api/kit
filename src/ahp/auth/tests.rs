//! Sign-in against a local HTTPS authorization server, through the real SDK
//! managed OAuth, Kit's credential storage and its loopback callback.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

use std::{
    collections::HashMap,
    sync::atomic::{AtomicUsize, Ordering},
};

use agenthooksprotocol::adapters::registered::AuthPurpose;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_rustls::{TlsAcceptor, rustls};

use super::*;

const CA: &[u8] = include_bytes!("../../../fixtures/workload-auth/ca.pem");
const CERT: &[u8] = include_bytes!("../../../fixtures/workload-auth/server.der");
const KEY: &[u8] = include_bytes!("../../../fixtures/workload-auth/server-key.der");

/// An authorization server issuing a fresh token per code exchange. It is
/// also the protected resource's origin.
struct Issuer {
    origin: String,
    exchanges: Arc<AtomicUsize>,
    /// Holds each token response this long.
    delay_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Token requests received, answered or not.
    received: Arc<AtomicUsize>,
    /// Every request received.
    hits: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Issuer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut (impl AsyncReadExt + Unpin)) -> (String, String) {
    let mut data = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).await.unwrap();
        assert!(n > 0, "request ended early");
        data.extend_from_slice(&chunk[..n]);
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8(data[..end].to_vec()).unwrap();
            let length = head
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while data.len() < end + 4 + length {
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0);
                data.extend_from_slice(&chunk[..n]);
            }
            let body = String::from_utf8(data[end + 4..end + 4 + length].to_vec()).unwrap();
            return (head, body);
        }
    }
}

impl Issuer {
    async fn start() -> Self {
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(CERT.to_vec())],
                rustls::pki_types::PrivatePkcs8KeyDer::from(KEY.to_vec()).into(),
            )
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let exchanges = Arc::new(AtomicUsize::new(0));
        let delay_ms = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (served_origin, counter) = (origin.clone(), Arc::clone(&exchanges));
        let served_delay = Arc::clone(&delay_ms);
        let received = Arc::new(AtomicUsize::new(0));
        let served_received = Arc::clone(&received);
        let hits = Arc::new(AtomicUsize::new(0));
        let served_hits = Arc::clone(&hits);
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                let origin = served_origin.clone();
                let counter = Arc::clone(&counter);
                let delay = Duration::from_millis(served_delay.load(Ordering::SeqCst));
                let received = Arc::clone(&served_received);
                let hit = Arc::clone(&served_hits);
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(socket).await else {
                        return;
                    };
                    let (head, request_body) = read_request(&mut stream).await;
                    hit.fetch_add(1, Ordering::SeqCst);
                    let path = head
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned();
                    let body = if path.starts_with("/.well-known/oauth-authorization-server") {
                        json!({
                            "issuer": origin,
                            "authorization_endpoint": format!("{origin}/authorize"),
                            "token_endpoint": format!("{origin}/token"),
                            "response_types_supported": ["code"],
                            "grant_types_supported": ["authorization_code", "refresh_token"],
                            "code_challenge_methods_supported": ["S256"],
                            "token_endpoint_auth_methods_supported": ["none"],
                        })
                    } else if path == "/token" && {
                        received.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(delay).await;
                        verified(&request_body)
                    } {
                        let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                        json!({"access_token": format!("token-{n}"), "token_type": "Bearer",
                            "expires_in": 3600, "refresh_token": format!("refresh-{n}")})
                    } else if path == "/token" {
                        json!({"error": "invalid_grant"})
                    } else {
                        json!({"error": "not_found"})
                    };
                    let status = if body.get("error").is_none() {
                        "200 OK"
                    } else if path == "/token" {
                        "400 Bad Request"
                    } else {
                        "404 Not Found"
                    };
                    let body = body.to_string();
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self {
            origin,
            exchanges,
            delay_ms,
            received,
            hits,
            task,
        }
    }

    fn exchanges(&self) -> usize {
        self.exchanges.load(Ordering::SeqCst)
    }

    /// A PKCE backend whose preset names this issuer.
    fn backend(&self, id: &str) -> Value {
        json!({
            "id": id,
            "transport": {"type": "http", "url": format!("{}/ahp", self.origin)},
            "authentication": {"type": "oauth", "flow": "authorization_code_pkce",
                "issuer": self.origin, "clientId": "kit", "resource": format!("{}/ahp", self.origin)},
            "oauth": {"allowPrivateNetwork": true},
        })
    }
}

/// Hook auth for `backends`, trusting the test authorization server.
fn hook_auth(storage: &tempfile::TempDir, backends: Vec<Value>) -> HookAuth {
    hook_auth_in(storage, backends, "/project/.agents/hooks.json")
}

/// [`hook_auth`] for backends declared by the hooks file `source`.
fn hook_auth_in(storage: &tempfile::TempDir, backends: Vec<Value>, source: &str) -> HookAuth {
    let sources = backends
        .iter()
        .map(|backend| {
            (
                backend["id"].as_str().unwrap().to_owned(),
                source.to_owned(),
            )
        })
        .collect();
    super::test_support::hook_auth(
        CredentialStorage::Filesystem(storage.path().to_path_buf()),
        &json!({"protocolVersion": "draft", "hooks": backends}),
        &sources,
        trusting_client,
    )
}

/// Trusts only the test CA.
fn trusting_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder().tls_certs_only([reqwest::Certificate::from_pem(CA).unwrap()])
}

/// Codes the fake authorization server issued, with their PKCE challenge.
static AUTHORIZATIONS: std::sync::LazyLock<Mutex<HashMap<String, String>>> =
    std::sync::LazyLock::new(Mutex::default);

/// Whether a token request redeems an issued code with its PKCE verifier,
/// or presents a refresh token. A code is redeemed once.
fn verified(body: &str) -> bool {
    use base64::Engine as _;
    use sha2::Digest as _;
    let form: BTreeMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    match form.get("grant_type").map(String::as_str) {
        Some("refresh_token") => form.contains_key("refresh_token"),
        Some("authorization_code") => {
            let (Some(code), Some(verifier)) = (form.get("code"), form.get("code_verifier")) else {
                return false;
            };
            let Some(challenge) = AUTHORIZATIONS.lock().unwrap().remove(code) else {
                return false;
            };
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(sha2::Sha256::digest(verifier.as_bytes()))
                == challenge
        }
        _ => false,
    }
}

fn query(url: &url::Url, name: &str) -> String {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .unwrap()
        .1
        .into_owned()
}

/// The loopback port of the login at `url`: its redirect's.
fn port(url: &url::Url) -> u16 {
    let redirect = url::Url::parse(&query(url, "redirect_uri")).unwrap();
    assert_eq!(redirect.host_str(), Some("127.0.0.1"));
    redirect.port().unwrap()
}

/// Issues a code bound to the PKCE challenge of the login at `url`.
fn issue_code(url: &url::Url) -> String {
    assert_eq!(query(url, "code_challenge_method"), "S256");
    let code = format!("code-{}", next_code());
    AUTHORIZATIONS
        .lock()
        .unwrap()
        .insert(code.clone(), query(url, "code_challenge"));
    code
}

/// Plays the browser: the user approves the login at `url`, the server
/// issues a code bound to its PKCE challenge, and the browser calls back
/// to the login's redirect.
async fn approve(url: &url::Url) -> bool {
    let code = issue_code(url);
    call_back_with(port(url), &query(url, "state"), &code).await
}

fn next_code() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

async fn call_back_with(port: u16, state: &str, code: &str) -> bool {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(
            format!("GET /callback?code={code}&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    response.starts_with(b"HTTP/1.1 200")
}

fn context(auth: &HookAuth, id: &str) -> AuthContext {
    auth.0.backends[id].context().unwrap()
}

async fn credential(auth: &HookAuth, context: AuthContext) -> Result<Option<String>, HookError> {
    Provider(auth.clone())
        .credential(context)
        .await
        .map(|credential| credential.map(|credential| credential.token))
}

/// Signs `id` in through `kit auth login hooks <id>`.
async fn sign_in(auth: &HookAuth, id: &str) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let browser = tokio::spawn(async move {
        let url: url::Url = rx.recv().await.unwrap();
        assert!(approve(&url).await);
    });
    let signed_in = auth
        .login(Some(id), |_, url| {
            // Kit listens before it shows the link, so it can be opened at once.
            assert!(
                std::net::TcpStream::connect(("127.0.0.1", port(url))).is_ok(),
                "the link was shown before Kit listened for its callback"
            );
            tx.send(url.clone()).unwrap();
        })
        .await
        .unwrap();
    assert_eq!(signed_in, [id]);
    browser.await.unwrap();
}

async fn next_hint(hints: &mut mpsc::UnboundedReceiver<AuthHint>) -> AuthHint {
    tokio::time::timeout(Duration::from_secs(30), hints.recv())
        .await
        .expect("a sign-in hint")
        .unwrap()
}

#[tokio::test]
async fn a_backend_with_a_grant_is_never_prompted_again() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let auth = hook_auth(&storage, vec![issuer.backend("test.granted")]);
    sign_in(&auth, "test.granted").await;
    assert_eq!(issuer.exchanges(), 1);

    // A new process sharing the storage finds the grant.
    let again = hook_auth(&storage, vec![issuer.backend("test.granted")]);
    let signed_in = again
        .login(None, |backend, _| panic!("{backend} was prompted again"))
        .await
        .unwrap();
    assert!(signed_in.is_empty());
    assert_eq!(
        credential(&again, context(&again, "test.granted"))
            .await
            .unwrap()
            .as_deref(),
        Some("token-1")
    );
    // Turn starts leave it alone: no hint, no new login.
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = again.subscribe(move |hint| tx.send(hint.clone()).unwrap());
    again.turn_started();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(hints.try_recv().is_err());
    assert_eq!(issuer.exchanges(), 1);
}

#[tokio::test]
async fn turns_repeat_one_link_until_sign_in_clears_it() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let auth = hook_auth(&storage, vec![issuer.backend("test.hinted")]);
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = auth.subscribe(move |hint| tx.send(hint.clone()).unwrap());

    // Delivery never signs in; it fails and records the need.
    let error = credential(&auth, context(&auth, "test.hinted"))
        .await
        .unwrap_err();
    assert_eq!(error.auth_code(), Some(AuthErrorCode::InteractionRequired));

    auth.turn_started();
    let first = next_hint(&mut hints).await;
    assert_eq!(first.backend, "test.hinted");
    assert!(!first.cleared);
    let url = url::Url::parse(first.url.as_deref().unwrap()).unwrap();
    // The next turn keeps this process's pending login: the same link, so
    // no new hint, and nothing is exchanged.
    let backend = auth.0.backends["test.hinted"].clone();
    let states = || -> Vec<String> {
        login::test_support::pending(&backend.scope)
            .into_iter()
            .map(|login| login.state)
            .collect()
    };
    assert_eq!(states(), [query(&url, "state")]);
    auth.prepare_backend(&backend).await;
    assert_eq!(states(), [query(&url, "state")]);
    assert!(hints.try_recv().is_err());

    assert!(approve(&url).await);
    let cleared = next_hint(&mut hints).await;
    assert!(cleared.cleared && cleared.signed_in, "{cleared:?}");
    assert_eq!(
        cleared.notice().as_deref(),
        Some("Hook backend `test.hinted` is signed in.")
    );
    assert_eq!(
        credential(&auth, context(&auth, "test.hinted"))
            .await
            .unwrap()
            .as_deref(),
        Some("token-1")
    );
}

#[tokio::test]
async fn event_and_upload_endpoints_share_one_consent() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let auth = hook_auth(&storage, vec![issuer.backend("test.upload")]);
    sign_in(&auth, "test.upload").await;
    let mut upload = context(&auth, "test.upload");
    upload.purpose = AuthPurpose::Upload;
    upload.destination = format!("{}/ahp/uploads", issuer.origin);
    assert_eq!(
        credential(&auth, upload).await.unwrap().as_deref(),
        Some("token-1")
    );
    assert_eq!(issuer.exchanges(), 1);
}

#[tokio::test]
async fn logout_from_another_process_always_finds_the_grant() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let backends = || vec![issuer.backend("test.relogin")];
    let live = hook_auth(&storage, backends());
    for round in 0..2 {
        sign_in(&live, "test.relogin").await;
        // A separate process, sharing the storage, logs out.
        let other = hook_auth(&storage, backends());
        assert!(other.logout("test.relogin").await.unwrap(), "round {round}");
        let status: BTreeMap<_, _> = other.status(None).await.unwrap().into_iter().collect();
        assert_eq!(status["test.relogin"], "needs-login", "round {round}");
    }
}

#[tokio::test]
async fn logout_and_status_are_per_backend() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let auth = hook_auth(
        &storage,
        vec![
            issuer.backend("test.first"),
            issuer.backend("test.second"),
            json!({"id": "test.stdio", "transport": {"type": "stdio", "command": "true", "lifecycle": "persistent"}}),
            json!({"id": "test.bearer", "transport": {"type": "http", "url": "https://hooks.example/ahp"},
                "authentication": {"type": "bearer", "tokenEnv": "TEST_HOOK_TOKEN"}}),
        ],
    );
    sign_in(&auth, "test.first").await;
    sign_in(&auth, "test.second").await;
    assert!(auth.logout("test.first").await.unwrap());
    assert!(!auth.logout("test.first").await.unwrap());
    // Logging out also stops a pending link's listener.
    let url = auth
        .prepare(&context(&auth, "test.first"), &[])
        .await
        .unwrap()
        .unwrap();
    assert!(TcpStream::connect(("127.0.0.1", port(&url))).await.is_ok());
    auth.logout("test.first").await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while TcpStream::connect(("127.0.0.1", port(&url))).await.is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("logout stops the link's listener");
    let status: BTreeMap<_, _> = auth.status(None).await.unwrap().into_iter().collect();
    assert_eq!(status["test.first"], "needs-login");
    assert_eq!(status["test.second"], "authorized");
    assert_eq!(status["test.stdio"], "no authentication (stdio)");
    assert_eq!(status["test.bearer"], "bearer token");
    assert!(auth.logout("test.missing").await.is_err());
}

/// A protected resource that rejects every request as unauthorized, and
/// counts the requests it received.
async fn rejecting_resource() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://127.0.0.1:{}/ahp",
        listener.local_addr().unwrap().port()
    );
    let received = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&received);
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let _ = read_request(&mut stream).await;
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = stream
                    .write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Bearer\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    (url, received, task)
}

#[tokio::test]
async fn a_fail_closed_backend_without_a_grant_denies() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let (url, received, task) = rejecting_resource().await;
    let backend = json!({
        "id": "test.closed",
        "transport": {"type": "http", "url": url},
        "authentication": {"type": "oauth", "flow": "authorization_code_pkce",
            "issuer": issuer.origin, "clientId": "kit", "resource": url},
        "oauth": {"allowPrivateNetwork": true},
        "subscriptions": [crate::ahp::test_support::intercept(&["context.compact.before"])],
    });
    let auth = hook_auth(&storage, vec![backend.clone()]);
    let registration =
        serde_json::from_value(json!({"protocolVersion": "draft", "hooks": [backend]})).unwrap();
    let mut options = crate::ahp::options().unwrap();
    options.backend.allow_loopback_http = true;
    options.backend.auth_provider = Some(Arc::new(auth.providers()));
    let hooks = crate::ahp::Hooks::new(registration, options).unwrap();
    let items = [agentkit_core::Item::text(
        agentkit_core::ItemKind::User,
        "old",
    )];
    let compaction = hooks
        .compact_before("closed", &items, true, "summarize", None)
        .await;
    assert!(
        matches!(compaction, Some(crate::ahp::Compaction::Denied(_))),
        "a backend that cannot authenticate must deny"
    );
    // The event was never sent without its credential.
    assert_eq!(received.load(Ordering::SeqCst), 0);
    task.abort();
}

#[test]
fn notices_show_each_link_once_and_completion_once() {
    let shown = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&shown);
    let sink = notices(move |_, text| recorded.lock().unwrap().push(text));
    let pending = AuthHint {
        scope: "test#test.notice".into(),
        backend: "test.notice".into(),
        url: Some("https://auth.example/authorize?state=1".into()),
        expires_at_ms: Some(1),
        cleared: false,
        signed_in: false,
        problem: None,
    };
    sink(&pending);
    sink(&pending);
    let cleared = AuthHint {
        url: None,
        cleared: true,
        signed_in: true,
        ..pending.clone()
    };
    sink(&cleared);
    sink(&cleared);
    assert_eq!(
        *shown.lock().unwrap(),
        [
            "Hook backend `test.notice` needs you to sign in. Open https://auth.example/authorize?state=1 to approve Kit.",
            "Hook backend `test.notice` is signed in.",
        ]
    );
}

#[test]
fn an_advertised_issuer_is_trusted_only_on_opt_in() {
    use agenthooksprotocol::auth::managed::{OAuthPolicy as _, ResourceMetadata};
    let target = login::event_context("test.trust", "https://hooks.example/ahp", None)
        .target()
        .unwrap();
    let resource = ResourceMetadata::new(
        "https://hooks.example/ahp".into(),
        vec!["https://auth.example".into()],
    );
    let policy = |oauth: Value| policy::Policy {
        settings: config::Settings::parse(&json!({"id": "test.trust", "oauth": oauth})).0,
    };
    assert!(policy(json!({})).select(&target, &resource, &[]).is_err());
    let trusted = policy(json!({"trustAdvertisedIssuer": true}))
        .select(&target, &resource, &[])
        .unwrap();
    assert_eq!(trusted.issuer, "https://auth.example");
    // A configured issuer the backend does not advertise is refused.
    assert!(
        policy(json!({"issuer": "https://other.example"}))
            .select(&target, &resource, &[])
            .is_err()
    );
    // A pin that is not an https URL refuses OAuth rather than unpinning it.
    assert!(
        policy(json!({"issuer": "http://auth.example", "trustAdvertisedIssuer": true}))
            .select(&target, &resource, &[])
            .is_err()
    );
    // Client credentials never trust an advertised issuer.
    assert!(
        policy(json!({"trustAdvertisedIssuer": true, "flow": "client_credentials"}))
            .select(&target, &resource, &[])
            .is_err()
    );
}

#[tokio::test]
async fn a_callback_the_server_rejects_completes_nothing() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let auth = hook_auth(&storage, vec![issuer.backend("test.rejected")]);
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = auth.subscribe(move |hint| tx.send(hint.clone()).unwrap());
    let _ = credential(&auth, context(&auth, "test.rejected")).await;
    auth.turn_started();
    let url = url::Url::parse(&next_hint(&mut hints).await.url.unwrap()).unwrap();
    // The right state, but a code issued for another PKCE challenge.
    AUTHORIZATIONS
        .lock()
        .unwrap()
        .insert("stolen-code".into(), "another-challenge".into());
    assert!(!call_back_with(port(&url), &query(&url, "state"), "stolen-code").await);
    // The SDK ended the login, so its dead link is withdrawn, not signed in.
    let cleared = next_hint(&mut hints).await;
    assert!(
        cleared.cleared && !cleared.signed_in,
        "a rejected exchange is not a sign-in: {cleared:?}"
    );
    assert_eq!(issuer.exchanges(), 0);
    assert!(
        credential(&auth, context(&auth, "test.rejected"))
            .await
            .is_err()
    );
    // The next turn shows a fresh link.
    auth.turn_started();
    let fresh = next_hint(&mut hints).await;
    let fresh = url::Url::parse(fresh.url.as_deref().unwrap()).unwrap();
    assert_ne!(query(&fresh, "state"), query(&url, "state"));
    assert!(approve(&fresh).await);
}

#[tokio::test]
async fn finishing_one_target_keeps_the_others_link() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let auth = hook_auth(&storage, vec![issuer.backend("test.two-targets")]);
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = auth.subscribe(move |hint| tx.send(hint.clone()).unwrap());
    // The event endpoint, and an upload endpoint on another resource.
    let event = context(&auth, "test.two-targets");
    let mut upload = event.clone();
    upload.purpose = AuthPurpose::Upload;
    upload.destination = format!("{}/uploads", issuer.origin);
    upload.authentication = Some(json!({"type": "oauth", "flow": "authorization_code_pkce",
        "issuer": issuer.origin, "clientId": "kit", "resource": format!("{}/uploads", issuer.origin)}));
    for target in [&event, &upload] {
        assert!(credential(&auth, target.clone()).await.is_err());
    }
    auth.turn_started();
    // Wait until both targets' logins are pending, each with its own
    // listener.
    let scope = auth.0.backends["test.two-targets"].scope.clone();
    let logins = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let logins = login::test_support::pending(&scope);
            if logins.len() >= 2 {
                return logins;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("pending logins");
    let (first, second) = (&logins[0], &logins[1]);
    assert_ne!(first.url, second.url);
    assert_ne!(
        port(&url::Url::parse(&first.url).unwrap()),
        port(&url::Url::parse(&second.url).unwrap())
    );
    assert!(approve(&url::Url::parse(&first.url).unwrap()).await);
    // The other target's link is shown next, not cleared.
    let next = loop {
        let hint = next_hint(&mut hints).await;
        if hint.url.as_deref() != Some(first.url.as_str()) {
            break hint;
        }
    };
    assert!(!next.cleared, "{next:?}");
    assert_eq!(next.url.as_deref(), Some(second.url.as_str()));
    assert!(approve(&url::Url::parse(&second.url).unwrap()).await);
    assert!(next_hint(&mut hints).await.signed_in);
    assert!(credential(&auth, event).await.unwrap().is_some());
    assert!(credential(&auth, upload).await.unwrap().is_some());
}

#[tokio::test]
async fn logout_all_workspaces_finds_grants_of_a_moved_project() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let old = hook_auth_in(
        &storage,
        vec![issuer.backend("test.moved")],
        "/old/hooks.json",
    );
    sign_in(&old, "test.moved").await;
    // The project moved: its new hooks file has no state, and plain logout
    // finds nothing.
    let moved = hook_auth_in(
        &storage,
        vec![issuer.backend("test.moved")],
        "/new/hooks.json",
    );
    assert!(!moved.logout("test.moved").await.unwrap());
    let backend = CredentialStorage::Filesystem(storage.path().to_path_buf());
    assert_eq!(
        super::logout_everywhere(&backend, "test.moved")
            .await
            .unwrap(),
        1
    );
    // Nothing is left anywhere.
    assert_eq!(
        super::logout_everywhere(&backend, "test.moved")
            .await
            .unwrap(),
        0
    );
    let status: BTreeMap<_, _> = old.status(None).await.unwrap().into_iter().collect();
    assert_eq!(status["test.moved"], "needs-login");
}

#[tokio::test]
async fn an_exchange_whose_request_was_sent_commits_after_the_caller_leaves() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    // No listener: the test completes the login itself.
    let auth = hook_auth(&storage, vec![issuer.backend("test.written")]);
    let target = context(&auth, "test.written");
    let manager = auth.manager(&target).unwrap();
    let redirect = "http://127.0.0.1:9/callback";
    let Preparation::InteractionRequired(pending) = manager
        .prepare_with(
            with_deadline(&target),
            PrepareOptions::new().redirect_uri(redirect),
            budget(),
        )
        .await
        .unwrap()
    else {
        panic!("a login is needed");
    };
    let url = pending.authorization_url();
    AUTHORIZATIONS
        .lock()
        .unwrap()
        .insert("written-code".into(), query(url, "code_challenge"));
    let callback = url::Url::parse(&format!(
        "{redirect}?code=written-code&state={}",
        query(url, "state")
    ))
    .unwrap();
    issuer.delay_ms.store(1000, Ordering::SeqCst);
    // The caller gives up once the server has the request but before it
    // answers. The request was marked written once connected, so the SDK
    // still commits the response.
    let exchange = manager.complete(with_deadline(&target), callback, budget());
    let request_arrived = async {
        while issuer.received.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::select! {
        _ = exchange => panic!("the response was held back"),
        () = request_arrived => {}
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(issuer.exchanges(), 1);
    assert!(credential(&auth, target).await.unwrap().is_some());
}

/// A hook server behind OAuth whose subscriptions come from
/// hooks/configuration. It answers only once Kit holds a token for it.
struct ProtectedServer {
    oauth: Arc<ManagedOAuth>,
    endpoint: AuthContext,
    events: Mutex<Vec<Value>>,
}

impl agenthooksprotocol::adapters::registered::ManagedBackend for ProtectedServer {
    fn call(&self, request: Value, _: Duration) -> LocalFuture<'_, Result<Value, HookError>> {
        Box::pin(async move {
            // The resource refuses a request without a usable token.
            if !matches!(
                self.oauth.credential(with_deadline(&self.endpoint)).await,
                Ok(Some(_))
            ) {
                return Err(HookError::authentication(
                    AuthErrorCode::InteractionRequired,
                ));
            }
            if request["method"] == "hooks/configuration" {
                return Ok(json!({"jsonrpc": "2.0", "id": request["id"], "result": {
                    "protocolVersion": "draft",
                    "configuration": {"subscriptions": [
                        crate::ahp::test_support::observe(&["context.compact.before"])
                    ]},
                }}));
            }
            self.events
                .lock()
                .unwrap()
                .push(request["params"]["event"].clone());
            Ok(Value::Null)
        })
    }

    fn shutdown(&self) -> LocalFuture<'_, Result<(), HookError>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn a_backend_configured_after_sign_in_is_delivered_to_in_the_same_session() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let mut backend = issuer.backend("test.configured");
    backend.as_object_mut().unwrap().remove("subscriptions");
    let auth = hook_auth(&storage, vec![backend.clone()]);
    let endpoint = context(&auth, "test.configured");
    let server = Arc::new(ProtectedServer {
        oauth: auth.manager(&endpoint).unwrap(),
        endpoint: endpoint.clone(),
        events: Mutex::default(),
    });
    let registration =
        serde_json::from_value(json!({"protocolVersion": "draft", "hooks": [backend]})).unwrap();
    let options = crate::ahp::options()
        .unwrap()
        .with_backend("test.configured", Arc::clone(&server) as _);
    let hooks = crate::ahp::Hooks::new(registration, auth.install(options)).unwrap();
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = auth.subscribe(move |hint| tx.send(hint.clone()).unwrap());

    // Configuration needs a sign-in: the backend is skipped, and the
    // configuration diagnostic records the need.
    hooks.sdk.initialize().await.unwrap();
    let scope = auth.0.backends["test.configured"].scope.clone();
    assert!(login::registry().has_needs(&scope));

    // The next turn shows the link; signing in completes it.
    auth.turn_started();
    let url = url::Url::parse(&next_hint(&mut hints).await.url.unwrap()).unwrap();
    assert!(approve(&url).await);
    assert!(next_hint(&mut hints).await.signed_in);

    // The same Hooks configure the backend now and deliver to it.
    hooks.sdk.initialize().await.unwrap();
    let items = [agentkit_core::Item::text(
        agentkit_core::ItemKind::User,
        "old",
    )];
    hooks
        .compact_before("configured", &items, true, "summarize", None)
        .await;
    tokio::time::timeout(Duration::from_secs(30), async {
        while server.events.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the event reaches the backend configured after sign-in");
    assert_eq!(
        server.events.lock().unwrap()[0]["type"],
        "context.compact.before"
    );
}

/// A session whose backend `id` supplies its subscriptions through
/// hooks/configuration, which needs a sign-in: initialized, so the
/// configuration was skipped.
async fn skipped_configuration(
    issuer: &Issuer,
    storage: &tempfile::TempDir,
    id: &str,
) -> (HookAuth, Arc<crate::ahp::Hooks>, Arc<ProtectedServer>) {
    let mut backend = issuer.backend(id);
    backend.as_object_mut().unwrap().remove("subscriptions");
    let auth = hook_auth(storage, vec![backend.clone()]);
    let endpoint = context(&auth, id);
    let server = Arc::new(ProtectedServer {
        oauth: auth.manager(&endpoint).unwrap(),
        endpoint,
        events: Mutex::default(),
    });
    let registration =
        serde_json::from_value(json!({"protocolVersion": "draft", "hooks": [backend]})).unwrap();
    let options = crate::ahp::options()
        .unwrap()
        .with_backend(id, Arc::clone(&server) as _);
    let hooks = Arc::new(crate::ahp::Hooks::new(registration, auth.install(options)).unwrap());
    hooks.sdk.initialize().await.unwrap();
    assert!(auth.was_skipped(&auth.0.backends[id]));
    (auth, hooks, server)
}

/// After a turn start, the session configures the backend it skipped and
/// delivers an event to it.
async fn delivered_after_turn(
    auth: &HookAuth,
    hooks: &crate::ahp::Hooks,
    server: &ProtectedServer,
    id: &str,
) {
    auth.turn_started();
    let endpoint = context(auth, id);
    tokio::time::timeout(Duration::from_secs(30), async {
        while Provider(auth.clone()).preparations(&endpoint) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the turn start prepares the signed-in backend");
    hooks.sdk.initialize().await.unwrap();
    let items = [agentkit_core::Item::text(
        agentkit_core::ItemKind::User,
        "old",
    )];
    hooks
        .compact_before("configured", &items, true, "summarize", None)
        .await;
    tokio::time::timeout(Duration::from_secs(30), async {
        while server.events.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the event reaches the backend configured after sign-in");
    assert!(!auth.was_skipped(&auth.0.backends[id]));
}

#[tokio::test]
async fn a_sign_in_elsewhere_configures_a_skipped_backend_at_the_next_turn() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let id = "test.configured-elsewhere";
    let (auth, hooks, server) = skipped_configuration(&issuer, &storage, id).await;
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = auth.subscribe(move |hint| tx.send(hint.clone()).unwrap());
    // The session shows its link; the user signs in from another process
    // (`kit auth login hooks` in another terminal, say) instead.
    auth.turn_started();
    assert!(next_hint(&mut hints).await.url.is_some());
    let other = hook_auth(&storage, vec![issuer.backend(id)]);
    let (url, complete) = elsewhere(&other, id).await;
    complete(issue_code(&url)).await;
    delivered_after_turn(&auth, &hooks, &server, id).await;
}

#[tokio::test]
async fn a_sign_in_by_another_session_configures_a_skipped_backend() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let id = "test.configured-by-session";
    let (auth, hooks, server) = skipped_configuration(&issuer, &storage, id).await;
    // Another session of this process signs in: the need it shares with
    // this one is cleared, but this session's configuration is still skipped.
    let other = hook_auth(&storage, vec![issuer.backend(id)]);
    sign_in(&other, id).await;
    assert!(
        credential(&other, context(&other, id))
            .await
            .unwrap()
            .is_some()
    );
    assert!(!login::registry().has_needs(&auth.0.backends[id].scope));
    delivered_after_turn(&auth, &hooks, &server, id).await;
}

#[tokio::test]
async fn a_skipped_configuration_is_prepared_once_per_credential() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let id = "test.rejects-token";
    let auth = hook_auth(&storage, vec![issuer.backend(id)]);
    let other = hook_auth(&storage, vec![issuer.backend(id)]);
    let backend = auth.0.backends[id].clone();
    let prepared = || Provider(auth.clone()).preparations(&context(&auth, id));
    let (url, complete) = elsewhere(&other, id).await;
    complete(issue_code(&url)).await;

    auth.configuration_needs_login(id);
    auth.prepare_backend(&backend).await;
    assert_eq!(prepared(), 1);
    // The backend rejects that token and its configuration is skipped
    // again: Kit does not prepare it again for the same credential.
    auth.configuration_needs_login(id);
    auth.prepare_backend(&backend).await;
    assert_eq!(prepared(), 1);
    assert!(auth.was_skipped(&backend));
    // A new sign-in brings a new credential, which is tried once.
    assert!(other.logout(id).await.unwrap());
    let (url, complete) = elsewhere(&other, id).await;
    complete(issue_code(&url)).await;
    auth.prepare_backend(&backend).await;
    assert_eq!(prepared(), 2);
}

#[tokio::test]
async fn reconfiguring_without_a_credential_never_offers_a_link() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let id = "test.quiet";
    let auth = hook_auth(&storage, vec![issuer.backend(id)]);
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = auth.subscribe(move |hint| tx.send(hint.clone()).unwrap());
    let scope = auth.0.backends[id].scope.clone();
    // The credential is gone by the time the configuration is prepared.
    auth.prepare_quietly(&context(&auth, id)).await.unwrap();
    assert!(login::test_support::pending(&scope).is_empty());
    assert!(hints.try_recv().is_err());
    assert_eq!(Provider(auth.clone()).preparations(&context(&auth, id)), 0);
}

/// What happens, besides a delivery, once a credential with only the base
/// scopes exists while a step-up link is shown.
#[derive(Clone, Copy)]
enum BaseCredential {
    /// The next turn finds it.
    NextTurn,
    /// The backend's configuration was skipped: the next turn prepares it
    /// again, with the base scopes.
    SkippedConfiguration,
    /// A preparation for the base scopes that started before the 403
    /// finishes after it.
    BasePreparation,
}

/// A step-up link stays live through `then`, and still completes.
async fn a_step_up_link_stays(id: &str, then: BaseCredential) {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let mut backend = issuer.backend(id);
    backend["authentication"]["scopes"] = json!(["hooks:invoke"]);
    let auth = hook_auth(&storage, vec![backend.clone()]);
    let other = hook_auth(&storage, vec![backend]);
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = auth.subscribe(move |hint| tx.send(hint.clone()).unwrap());
    let target = context(&auth, id);
    let scope = auth.0.backends[id].scope.clone();
    // A 403 asked for more scopes: the step-up link is shown.
    login::registry().needs_login(&scope, &target, &["hooks:invoke".into()]);
    auth.turn_started();
    let shown = next_hint(&mut hints).await;
    let url = url::Url::parse(shown.url.as_deref().unwrap()).unwrap();
    let states = || -> Vec<String> {
        login::test_support::pending(&scope)
            .into_iter()
            .map(|login| login.state)
            .collect()
    };
    assert_eq!(states(), [query(&url, "state")]);
    // A credential with the base scopes exists (signed in elsewhere), and
    // a delivery finds it.
    let (base, complete) = elsewhere(&other, id).await;
    complete(issue_code(&base)).await;
    assert!(credential(&auth, target.clone()).await.unwrap().is_some());
    match then {
        BaseCredential::NextTurn => auth.prepare_backend(&auth.0.backends[id].clone()).await,
        BaseCredential::SkippedConfiguration => {
            auth.configuration_needs_login(id);
            auth.prepare_backend(&auth.0.backends[id].clone()).await;
            // The configuration is still retried with the base credential.
            assert_eq!(Provider(auth.clone()).preparations(&target), 1);
        }
        BaseCredential::BasePreparation => {
            assert_eq!(auth.prepare(&target, &[]).await.unwrap(), None);
        }
    }
    assert_eq!(states(), [query(&url, "state")]);
    assert!(login::registry().has_needs(&scope));
    assert!(hints.try_recv().is_err());
    // The earlier link still completes the step-up.
    assert!(approve(&url).await);
    let cleared = next_hint(&mut hints).await;
    assert!(cleared.cleared && cleared.signed_in, "{cleared:?}");
    assert!(!login::registry().has_needs(&scope));
}

#[tokio::test]
async fn a_step_up_link_stays_while_the_base_credential_works() {
    a_step_up_link_stays("test.step-up", BaseCredential::NextTurn).await;
}

#[tokio::test]
async fn reconfiguring_with_the_base_credential_keeps_the_step_up_link() {
    a_step_up_link_stays("test.step-up-skipped", BaseCredential::SkippedConfiguration).await;
}

#[tokio::test]
async fn a_late_base_preparation_keeps_the_step_up_link() {
    a_step_up_link_stays("test.step-up-race", BaseCredential::BasePreparation).await;
}

#[tokio::test]
async fn scope_settings_add_to_the_presets_scopes() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let mut backend = issuer.backend("test.scopes");
    backend["authentication"]["scopes"] = json!(["hooks:invoke"]);
    backend["oauth"]["scopes"] = json!(["hooks:upload"]);
    let auth = hook_auth(&storage, vec![backend]);
    let target = context(&auth, "test.scopes");
    let url = auth.prepare(&target, &[]).await.unwrap().unwrap();
    let mut requested: Vec<_> = query(&url, "scope").split(' ').map(str::to_owned).collect();
    requested.sort();
    assert_eq!(requested, ["hooks:invoke", "hooks:upload"]);
    // And the grant satisfies the preset's requirement.
    assert!(approve(&url).await);
    assert!(credential(&auth, target).await.unwrap().is_some());
}

#[tokio::test]
async fn a_configuration_need_is_this_workspaces_only() {
    let storage = tempfile::tempdir().unwrap();
    let backend = json!({"id": "test.same-id", "transport": {"type": "http", "url": "https://hooks.example/ahp"},
        "oauth": {"issuer": "https://auth.example"}});
    let first = hook_auth_in(&storage, vec![backend.clone()], "/first/hooks.json");
    let second = hook_auth_in(&storage, vec![backend], "/second/hooks.json");
    first.configuration_needs_login("test.same-id");
    assert!(login::registry().has_needs(&first.0.backends["test.same-id"].scope));
    assert!(!login::registry().has_needs(&second.0.backends["test.same-id"].scope));
}

#[test]
fn a_backend_with_an_authorization_header_never_needs_a_login() {
    let storage = tempfile::tempdir().unwrap();
    let auth = hook_auth(
        &storage,
        vec![
            json!({"id": "test.header-auth", "transport": {"type": "http",
            "url": "https://hooks.example/ahp", "headers": {"Authorization": "Bearer fixed"}},
            "oauth": {"issuer": "https://auth.example"}}),
        ],
    );
    auth.configuration_needs_login("test.header-auth");
    assert!(!login::registry().has_needs(&auth.0.backends["test.header-auth"].scope));
}

#[tokio::test]
async fn status_for_one_backend_checks_only_that_backend() {
    let (first, second) = (Issuer::start().await, Issuer::start().await);
    let storage = tempfile::tempdir().unwrap();
    let auth = hook_auth(
        &storage,
        vec![
            first.backend("test.status-one"),
            second.backend("test.status-other"),
        ],
    );
    let lines = auth.status(Some("test.status-one")).await.unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].0, "test.status-one");
    assert_eq!(
        second.hits.load(Ordering::SeqCst),
        0,
        "another backend's issuer was contacted"
    );
    // The named backend itself was checked.
    let checked = first.hits.load(Ordering::SeqCst);
    assert!(checked > 0, "the named backend was not checked");
    // A typo is refused before any credential check.
    assert!(auth.status(Some("test.typo")).await.is_err());
    assert_eq!(first.hits.load(Ordering::SeqCst), checked);
    assert_eq!(second.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_sign_in_that_cannot_be_prepared_says_why() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let mut backend = issuer.backend("test.unprepared");
    // The issuer is on loopback, which this backend does not allow.
    backend["oauth"]["allowPrivateNetwork"] = json!(false);
    let auth = hook_auth(&storage, vec![backend]);
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = auth.subscribe(move |hint| tx.send(hint.clone()).unwrap());
    auth.configuration_needs_login("test.unprepared");
    auth.turn_started();
    let hint = next_hint(&mut hints).await;
    assert_eq!(hint.url, None);
    let notice = hint.notice().unwrap();
    assert!(
        notice.contains("untrusted_destination") && notice.contains("oauth.allowPrivateNetwork"),
        "{notice}"
    );
}

#[tokio::test]
async fn event_and_upload_targets_with_different_scopes_each_get_their_own_ceiling() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let mut backend = issuer.backend("test.scoped-targets");
    backend["authentication"]["scopes"] = json!(["hooks:invoke"]);
    let auth = hook_auth(&storage, vec![backend]);
    let event = context(&auth, "test.scoped-targets");
    let mut upload = event.clone();
    upload.purpose = AuthPurpose::Upload;
    upload.destination = format!("{}/ahp/uploads", issuer.origin);
    let mut binding = event.authentication.clone().unwrap();
    binding["scopes"] = json!(["hooks:upload"]);
    upload.authentication = Some(binding);
    // The event target builds its manager first.
    assert!(credential(&auth, event).await.is_err());
    // The upload target's own scope is requested, not refused against the
    // event target's ceiling.
    let url = auth.prepare(&upload, &[]).await.unwrap().unwrap();
    assert_eq!(query(&url, "scope"), "hooks:upload");
}

#[tokio::test]
async fn a_problem_with_one_target_keeps_the_others_link() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let auth = hook_auth(&storage, vec![issuer.backend("test.link-and-problem")]);
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = auth.subscribe(move |hint| tx.send(hint.clone()).unwrap());
    let event = context(&auth, "test.link-and-problem");
    let url = auth.prepare(&event, &[]).await.unwrap().unwrap();
    assert_eq!(
        next_hint(&mut hints).await.url.as_deref(),
        Some(url.as_str())
    );
    // Another target of the backend fails to prepare.
    let scope = auth.0.backends["test.link-and-problem"].scope.clone();
    login::registry().problem(&scope, "test.link-and-problem", "access_denied.".into());
    let hint = next_hint(&mut hints).await;
    assert_eq!(
        hint.url.as_deref(),
        Some(url.as_str()),
        "the live link was dropped"
    );
    let notice = hint.notice().unwrap();
    assert!(
        notice.contains(url.as_str()) && notice.contains("access_denied"),
        "{notice}"
    );
}

#[test]
fn a_replayed_link_reaches_a_new_subscriber_before_its_completion() {
    let scope = "test#test.replay-order".to_owned();
    let link = AuthHint {
        scope: scope.clone(),
        backend: "test.replay-order".into(),
        url: Some("https://auth.example/replay".into()),
        expires_at_ms: None,
        cleared: false,
        signed_in: false,
        problem: None,
    };
    login::registry().publish(link.clone());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let completer = Arc::new(Mutex::new(None));
    let _subscription = login::registry().subscribe(HashSet::from([scope.clone()]), {
        let (seen, completer, scope) = (Arc::clone(&seen), Arc::clone(&completer), scope.clone());
        move |hint: &AuthHint| {
            if hint.url.is_some() {
                // While the link is replayed, the login completes elsewhere.
                let scope = scope.clone();
                *completer.lock().unwrap() = Some(std::thread::spawn(move || {
                    login::registry().publish(AuthHint {
                        scope,
                        backend: "test.replay-order".into(),
                        url: None,
                        expires_at_ms: None,
                        cleared: true,
                        signed_in: true,
                        problem: None,
                    });
                }));
                std::thread::sleep(Duration::from_millis(200));
            }
            seen.lock().unwrap().push(hint.cleared);
        }
    });
    let completer = completer.lock().unwrap().take().unwrap();
    completer.join().unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        [false, true],
        "the completion overtook the replayed link"
    );
}

#[tokio::test]
async fn a_scope_outside_the_allowed_set_is_explained_not_signed_in() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let mut backend = issuer.backend("test.ceiling");
    backend["authentication"]["scopes"] = json!(["hooks:invoke"]);
    let auth = hook_auth(&storage, vec![backend]);
    let target = context(&auth, "test.ceiling");
    sign_in(&auth, "test.ceiling").await;
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = auth.subscribe(move |hint| tx.send(hint.clone()).unwrap());
    // The backend asks for a scope Kit does not allow it.
    let challenge = agenthooksprotocol::adapters::registered::AuthChallenge {
        status: 403,
        headers: BTreeMap::from([(
            "WWW-Authenticate".to_owned(),
            r#"Bearer error="insufficient_scope", scope="hooks:admin""#.to_owned(),
        )]),
        attempt_id: None,
    };
    let error = Provider(auth.clone())
        .challenge(target, challenge)
        .await
        .unwrap_err();
    assert_eq!(error.auth_code(), Some(AuthErrorCode::AccessDenied));
    let hint = next_hint(&mut hints).await;
    let notice = hint.notice().unwrap();
    assert!(
        notice.contains("hooks:admin") && notice.contains("oauth.scopes"),
        "{notice}"
    );
    assert!(!login::registry().has_needs(&auth.0.backends["test.ceiling"].scope));
}

#[tokio::test]
async fn two_workspaces_with_one_backend_id_sign_in_separately() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    // The same backend id, declared by two workspaces' hooks files.
    let first = hook_auth_in(
        &storage,
        vec![issuer.backend("test.shared")],
        "/a/hooks.json",
    );
    let second = hook_auth_in(
        &storage,
        vec![issuer.backend("test.shared")],
        "/b/hooks.json",
    );
    let (first_tx, mut first_hints) = mpsc::unbounded_channel();
    let _first = first.subscribe(move |hint| first_tx.send(hint.clone()).unwrap());
    let (second_tx, mut second_hints) = mpsc::unbounded_channel();
    let _second = second.subscribe(move |hint| second_tx.send(hint.clone()).unwrap());
    for auth in [&first, &second] {
        let error = credential(auth, context(auth, "test.shared"))
            .await
            .unwrap_err();
        assert_eq!(error.auth_code(), Some(AuthErrorCode::InteractionRequired));
        auth.turn_started();
    }
    let first_url = url::Url::parse(&next_hint(&mut first_hints).await.url.unwrap()).unwrap();
    let second_url = url::Url::parse(&next_hint(&mut second_hints).await.url.unwrap()).unwrap();
    assert_ne!(port(&first_url), port(&second_url));

    // A callback with an unknown state completes nothing.
    assert!(!call_back_with(port(&first_url), "forged-state", "never-issued").await);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(first_hints.try_recv().is_err() && second_hints.try_recv().is_err());
    assert_eq!(issuer.exchanges(), 0);

    assert!(approve(&second_url).await);
    assert!(next_hint(&mut second_hints).await.signed_in);
    assert!(
        first_hints.try_recv().is_err(),
        "the other workspace is still pending"
    );
    assert!(approve(&first_url).await);
    assert!(next_hint(&mut first_hints).await.signed_in);
    assert_eq!(issuer.exchanges(), 2);
    // Logging out of one workspace's backend leaves the other's.
    assert!(first.logout("test.shared").await.unwrap());
    let first_status: BTreeMap<_, _> = first.status(None).await.unwrap().into_iter().collect();
    let second_status: BTreeMap<_, _> = second.status(None).await.unwrap().into_iter().collect();
    assert_eq!(first_status["test.shared"], "needs-login");
    assert_eq!(second_status["test.shared"], "authorized");
}

/// A sign-in to `id` by another Kit process sharing the credential store:
/// its own provider prepares the login, and completing it with a code
/// touches nothing of this process's sign-in state.
async fn elsewhere(
    auth: &HookAuth,
    id: &str,
) -> (
    url::Url,
    impl FnOnce(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>,
) {
    let target = context(auth, id);
    let manager = auth.manager(&target).unwrap();
    let redirect = "http://127.0.0.1:9/callback";
    let Preparation::InteractionRequired(pending) = manager
        .prepare_with(
            with_deadline(&target),
            PrepareOptions::new().redirect_uri(redirect),
            budget(),
        )
        .await
        .unwrap()
    else {
        panic!("a login is needed");
    };
    let url = pending.authorization_url().clone();
    let state = query(&url, "state");
    let complete = move |code: String| -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> {
        Box::pin(async move {
            let callback =
                url::Url::parse(&format!("{redirect}?code={code}&state={state}")).unwrap();
            manager
                .complete(with_deadline(&target), callback, budget())
                .await
                .unwrap();
        })
    };
    (url, complete)
}

/// Starts `kit auth login hooks <id>` for `auth`, as another Kit process
/// would, and returns its link once it is shown.
fn cli_login(
    auth: &HookAuth,
    id: &'static str,
) -> (
    tokio::task::JoinHandle<Result<Vec<String>, String>>,
    mpsc::UnboundedReceiver<url::Url>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let auth = auth.clone();
    let task = tokio::spawn(async move {
        auth.login(Some(id), move |_, url| tx.send(url.clone()).unwrap())
            .await
    });
    (task, rx)
}

#[tokio::test]
async fn processes_sign_in_to_one_backend_concurrently_and_share_the_grant() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    // Two Kit processes for one workspace, sharing the credential store.
    let session = hook_auth(&storage, vec![issuer.backend("test.concurrent")]);
    let cli = hook_auth(&storage, vec![issuer.backend("test.concurrent")]);
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = session.subscribe(move |hint| tx.send(hint.clone()).unwrap());
    let target = context(&session, "test.concurrent");
    assert!(credential(&session, target.clone()).await.is_err());
    session.turn_started();
    let session_url = url::Url::parse(&next_hint(&mut hints).await.url.unwrap()).unwrap();
    // The other process signs in at the same time, with its own login.
    let (other_url, complete) = elsewhere(&cli, "test.concurrent").await;
    assert_ne!(query(&session_url, "state"), query(&other_url, "state"));

    // Only the process that prepared a login completes it: the other
    // process's callback, delivered to this one, is refused.
    let code = issue_code(&other_url);
    assert!(!call_back_with(port(&session_url), &query(&other_url, "state"), &code).await);
    assert_eq!(issuer.exchanges(), 0);
    complete(code).await;
    assert_eq!(issuer.exchanges(), 1);

    // The grant is shared: the session's next delivery uses it, and its
    // own hint clears without the user opening its link.
    assert_eq!(
        credential(&session, target).await.unwrap().as_deref(),
        Some("token-1")
    );
    let cleared = next_hint(&mut hints).await;
    assert!(cleared.cleared && cleared.signed_in, "{cleared:?}");
    // Its listener is gone, so its link no longer completes anything.
    tokio::time::timeout(Duration::from_secs(30), async {
        while TcpStream::connect(("127.0.0.1", port(&session_url)))
            .await
            .is_ok()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the session stops listening once signed in elsewhere");
    assert_eq!(issuer.exchanges(), 1);
}

#[tokio::test]
async fn a_turn_start_sees_a_grant_from_another_process() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let session = hook_auth(&storage, vec![issuer.backend("test.elsewhere")]);
    let other = hook_auth(&storage, vec![issuer.backend("test.elsewhere")]);
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = session.subscribe(move |hint| tx.send(hint.clone()).unwrap());
    assert!(
        credential(&session, context(&session, "test.elsewhere"))
            .await
            .is_err()
    );
    session.turn_started();
    assert!(next_hint(&mut hints).await.url.is_some());
    let (url, complete) = elsewhere(&other, "test.elsewhere").await;
    complete(issue_code(&url)).await;
    // The next turn start finds the grant instead of showing the link again.
    session.turn_started();
    let cleared = next_hint(&mut hints).await;
    assert!(cleared.cleared && cleared.signed_in, "{cleared:?}");
}

#[tokio::test]
async fn the_newest_completed_sign_in_wins() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let first = hook_auth(&storage, vec![issuer.backend("test.newest")]);
    let second = hook_auth(&storage, vec![issuer.backend("test.newest")]);
    let (first_login, mut first_links) = cli_login(&first, "test.newest");
    let (second_login, mut second_links) = cli_login(&second, "test.newest");
    let first_url = first_links.recv().await.unwrap();
    let second_url = second_links.recv().await.unwrap();
    assert!(approve(&first_url).await);
    first_login.await.unwrap().unwrap();
    assert!(approve(&second_url).await);
    second_login.await.unwrap().unwrap();
    for auth in [&first, &second] {
        assert_eq!(
            credential(auth, context(auth, "test.newest"))
                .await
                .unwrap()
                .as_deref(),
            Some("token-2")
        );
    }
}

#[tokio::test]
async fn an_expired_link_is_withdrawn() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let mut backend = issuer.backend("test.expires");
    // A link that lasts under two seconds.
    backend["oauth"]["loginLifetimeMinutes"] = json!(0.03);
    let auth = hook_auth(&storage, vec![backend]);
    let (tx, mut hints) = mpsc::unbounded_channel();
    let _subscription = auth.subscribe(move |hint| tx.send(hint.clone()).unwrap());
    let _ = credential(&auth, context(&auth, "test.expires")).await;
    auth.turn_started();
    let shown = next_hint(&mut hints).await;
    let url = url::Url::parse(shown.url.as_deref().unwrap()).unwrap();
    // At expiry the link is withdrawn, not left clickable.
    let cleared = next_hint(&mut hints).await;
    assert!(cleared.cleared && !cleared.signed_in, "{cleared:?}");
    assert!(
        TcpStream::connect(("127.0.0.1", port(&url))).await.is_err(),
        "the listener outlived its link"
    );
    // The next turn prepares a new link.
    auth.turn_started();
    let again = next_hint(&mut hints).await;
    assert!(again.url.is_some() && again.url != shown.url, "{again:?}");
}

#[tokio::test]
async fn denying_consent_ends_the_login() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let auth = hook_auth(&storage, vec![issuer.backend("test.denied")]);
    let (tx, mut links) = mpsc::unbounded_channel();
    let browser = tokio::spawn(async move {
        let url: url::Url = links.recv().await.unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port(&url))).await.unwrap();
        stream
            .write_all(
                format!(
                    "GET /callback?error=access_denied&state={} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
                    query(&url, "state")
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
    });
    let error = auth
        .login(Some("test.denied"), |_, url| tx.send(url.clone()).unwrap())
        .await
        .unwrap_err();
    assert!(error.contains("not approved"), "{error}");
    browser.await.unwrap();
    assert_eq!(issuer.exchanges(), 0);
}

#[tokio::test]
async fn an_ended_login_never_withdraws_its_replacement() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let auth = hook_auth(&storage, vec![issuer.backend("test.replaced")]);
    let target = context(&auth, "test.replaced");
    let scope = auth.0.backends["test.replaced"].scope.clone();
    let _ = credential(&auth, target.clone()).await;
    let first = auth.prepare(&target, &[]).await.unwrap().unwrap();
    let old = login::test_support::pending(&scope).remove(0);
    // A newer login for the same target replaces the first.
    let second = auth.prepare(&target, &[]).await.unwrap().unwrap();
    assert_ne!(query(&first, "state"), query(&second, "state"));
    // The first login's listener ending late withdraws only itself.
    login::registry().ended(&old);
    let states: Vec<_> = login::test_support::pending(&scope)
        .into_iter()
        .map(|login| login.state)
        .collect();
    assert_eq!(states, [query(&second, "state")]);
    assert!(approve(&second).await);
}

#[tokio::test]
async fn signing_in_to_every_backend_tries_them_all_and_reports_each_failure() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    // Sorted first, and its client secret reference resolves to nothing.
    let broken = json!({"id": "test.a-broken", "transport": {"type": "http", "url": format!("{}/ahp", issuer.origin)},
        "oauth": {"issuer": issuer.origin, "flow": "client_credentials", "clientId": "kit",
            "clientSecretRef": "test.missing", "allowPrivateNetwork": true}});
    let auth = hook_auth(&storage, vec![broken, issuer.backend("test.b-working")]);
    let (tx, mut links) = mpsc::unbounded_channel();
    let browser = tokio::spawn(async move {
        let url: url::Url = links.recv().await.unwrap();
        assert!(approve(&url).await);
    });
    let error = auth
        .login(None, |_, url| tx.send(url.clone()).unwrap())
        .await
        .unwrap_err();
    browser.await.unwrap();
    assert!(
        error.contains("test.a-broken: failing (credential_unavailable)"),
        "{error}"
    );
    assert!(
        error.contains("Signed in to hook backends: test.b-working"),
        "{error}"
    );
    assert!(
        credential(&auth, context(&auth, "test.b-working"))
            .await
            .unwrap()
            .is_some()
    );
}

#[test]
fn only_pkce_targets_listen_for_a_callback() {
    let storage = tempfile::tempdir().unwrap();
    let auth = hook_auth(
        &storage,
        vec![
            json!({"id": "test.pkce", "transport": {"type": "http", "url": "https://hooks.example/ahp"},
                "oauth": {"issuer": "https://auth.example"}}),
            json!({"id": "test.machine", "transport": {"type": "http", "url": "https://hooks.example/ahp"},
                "oauth": {"issuer": "https://auth.example", "flow": "client_credentials"}}),
            json!({"id": "test.preset-machine", "transport": {"type": "http", "url": "https://hooks.example/ahp"},
                "authentication": {"type": "oauth", "flow": "client_credentials",
                    "issuer": "https://auth.example", "clientId": "kit", "resource": "https://hooks.example/ahp"}}),
        ],
    );
    assert!(auth.pkce(&context(&auth, "test.pkce")).unwrap());
    assert!(!auth.pkce(&context(&auth, "test.machine")).unwrap());
    assert!(!auth.pkce(&context(&auth, "test.preset-machine")).unwrap());
}
