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
        let (served_origin, counter) = (origin.clone(), Arc::clone(&exchanges));
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let acceptor = acceptor.clone();
                let origin = served_origin.clone();
                let counter = Arc::clone(&counter);
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(socket).await else {
                        return;
                    };
                    let (head, request_body) = read_request(&mut stream).await;
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
                    } else if path == "/token" && verified(&request_body) {
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
            task,
        }
    }

    fn exchanges(&self) -> usize {
        self.exchanges.load(Ordering::SeqCst)
    }

    /// A PKCE backend whose preset names this issuer.
    fn backend(&self, id: &str, port: u16) -> Value {
        json!({
            "id": id,
            "transport": {"type": "http", "url": format!("{}/ahp", self.origin)},
            "authentication": {"type": "oauth", "flow": "authorization_code_pkce",
                "issuer": self.origin, "clientId": "kit", "resource": format!("{}/ahp", self.origin)},
            "oauth": {"redirectPort": port, "allowPrivateNetwork": true},
        })
    }
}

async fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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

/// Plays the browser: the user approves the login at `url`, the server
/// issues a code bound to its PKCE challenge, and the browser calls back.
async fn approve(url: &url::Url, port: u16) -> bool {
    assert_eq!(query(url, "code_challenge_method"), "S256");
    let code = format!("code-{}", next_code());
    AUTHORIZATIONS
        .lock()
        .unwrap()
        .insert(code.clone(), query(url, "code_challenge"));
    call_back_with(port, &query(url, "state"), &code).await
}

fn next_code() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

async fn call_back(port: u16, state: &str) -> bool {
    call_back_with(port, state, "never-issued").await
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
async fn sign_in(auth: &HookAuth, id: &str, port: u16) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let browser = tokio::spawn(async move {
        let url: url::Url = rx.recv().await.unwrap();
        assert!(approve(&url, port).await);
    });
    let signed_in = auth
        .login(Some(id), |_, url| tx.send(url.clone()).unwrap())
        .await
        .unwrap();
    assert_eq!(signed_in, [id]);
    browser.await.unwrap();
}

async fn next_hint(hints: &mut mpsc::UnboundedReceiver<AuthHint>) -> AuthHint {
    tokio::time::timeout(Duration::from_secs(10), hints.recv())
        .await
        .expect("a sign-in hint")
        .unwrap()
}

#[tokio::test]
async fn a_backend_with_a_grant_is_never_prompted_again() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let port = free_port().await;
    let auth = hook_auth(&storage, vec![issuer.backend("test.granted", port)]);
    sign_in(&auth, "test.granted", port).await;
    assert_eq!(issuer.exchanges(), 1);

    // A new process sharing the storage finds the grant.
    let again = hook_auth(&storage, vec![issuer.backend("test.granted", port)]);
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
    let port = free_port().await;
    let auth = hook_auth(&storage, vec![issuer.backend("test.hinted", port)]);
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
    // The next turn reuses the stored pending login: the same link, so no
    // new hint, and nothing is exchanged.
    tokio::time::sleep(Duration::from_millis(200)).await;
    auth.turn_started();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(hints.try_recv().is_err());
    let reused = auth
        .prepare(&context(&auth, "test.hinted"), &[])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reused, url);

    assert!(approve(&url, port).await);
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
    let port = free_port().await;
    let auth = hook_auth(&storage, vec![issuer.backend("test.upload", port)]);
    sign_in(&auth, "test.upload", port).await;
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
    let port = free_port().await;
    let backends = || vec![issuer.backend("test.relogin", port)];
    let live = hook_auth(&storage, backends());
    for round in 0..2 {
        sign_in(&live, "test.relogin", port).await;
        // A separate process, sharing the storage, logs out.
        let other = hook_auth(&storage, backends());
        assert!(other.logout("test.relogin").await.unwrap(), "round {round}");
        let status: BTreeMap<_, _> = other.status().await.into_iter().collect();
        assert_eq!(status["test.relogin"], "needs-login", "round {round}");
    }
}

#[tokio::test]
async fn logout_and_status_are_per_backend() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let (first, second) = (free_port().await, free_port().await);
    let auth = hook_auth(
        &storage,
        vec![
            issuer.backend("test.first", first),
            issuer.backend("test.second", second),
            json!({"id": "test.stdio", "transport": {"type": "stdio", "command": "true", "lifecycle": "persistent"}}),
            json!({"id": "test.bearer", "transport": {"type": "http", "url": "https://hooks.example/ahp"},
                "authentication": {"type": "bearer", "tokenEnv": "TEST_HOOK_TOKEN"}}),
        ],
    );
    sign_in(&auth, "test.first", first).await;
    sign_in(&auth, "test.second", second).await;
    assert!(auth.logout("test.first").await.unwrap());
    assert!(!auth.logout("test.first").await.unwrap());
    let status: BTreeMap<_, _> = auth.status().await.into_iter().collect();
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

#[tokio::test]
async fn one_port_serves_two_workspaces_by_state() {
    let issuer = Issuer::start().await;
    let storage = tempfile::tempdir().unwrap();
    let port = free_port().await;
    // The same backend id, declared by two workspaces' hooks files.
    let first = hook_auth_in(
        &storage,
        vec![issuer.backend("test.shared", port)],
        "/a/hooks.json",
    );
    let second = hook_auth_in(
        &storage,
        vec![issuer.backend("test.shared", port)],
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
    assert_ne!(
        first_url, second_url,
        "workspaces never share a pending login"
    );
    // One listener holds the port; it answers Kit's probe.
    assert!(super::login::bind(port).await.is_none());

    // A callback with an unknown state completes nothing.
    assert!(!call_back(port, "forged-state").await);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(first_hints.try_recv().is_err() && second_hints.try_recv().is_err());
    assert_eq!(issuer.exchanges(), 0);

    assert!(approve(&second_url, port).await);
    assert!(next_hint(&mut second_hints).await.signed_in);
    assert!(
        first_hints.try_recv().is_err(),
        "the other workspace is still pending"
    );
    assert!(approve(&first_url, port).await);
    assert!(next_hint(&mut first_hints).await.signed_in);
    assert_eq!(issuer.exchanges(), 2);
    // Logging out of one workspace's backend leaves the other's.
    assert!(first.logout("test.shared").await.unwrap());
    let first_status: BTreeMap<_, _> = first.status().await.into_iter().collect();
    let second_status: BTreeMap<_, _> = second.status().await.into_iter().collect();
    assert_eq!(first_status["test.shared"], "needs-login");
    assert_eq!(second_status["test.shared"], "authorized");
}

#[tokio::test]
async fn a_port_held_by_another_program_is_not_mistaken_for_kit() {
    let foreign = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = foreign.local_addr().unwrap().port();
    let answer = tokio::spawn(async move {
        let (mut stream, _) = foreign.accept().await.unwrap();
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
            .await;
    });
    assert!(super::login::bind(port).await.is_none());
    answer.await.unwrap();
}

#[test]
fn backends_never_share_an_explicit_redirect_port() {
    let storage = tempfile::tempdir().unwrap();
    let backend = |id: &str| {
        json!({"id": id, "transport": {"type": "http", "url": "https://hooks.example/ahp"},
            "oauth": {"redirectPort": 30001}})
    };
    let auth = hook_auth(
        &storage,
        vec![backend("test.port-a"), backend("test.port-b")],
    );
    let ports: Vec<_> = auth
        .0
        .backends
        .values()
        .map(|backend| backend.settings.redirect_port)
        .collect();
    assert_eq!(ports[0], 30001);
    assert_ne!(ports[1], 30001);
    for id in ["a", "b", "com.example.policy"] {
        assert!((30000..32768).contains(&config::default_port(id)));
    }
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
    let port = free_port().await;
    let auth = hook_auth(&storage, vec![issuer.backend("test.rejected", port)]);
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
    assert!(!call_back_with(port, &query(&url, "state"), "stolen-code").await);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        hints.try_recv().is_err(),
        "a rejected exchange is not a sign-in"
    );
    assert_eq!(issuer.exchanges(), 0);
    assert!(
        credential(&auth, context(&auth, "test.rejected"))
            .await
            .is_err()
    );
}
