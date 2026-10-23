//! Real TLS exchanges against a scripted localhost authorization server.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

use super::*;
use std::{collections::BTreeMap, sync::Arc};
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::mpsc, task::JoinHandle};
use tokio_rustls::{TlsAcceptor, rustls};

const CA: &[u8] = include_bytes!("../../fixtures/workload-auth/ca.pem");
const CERT: &[u8] = include_bytes!("../../fixtures/workload-auth/server.der");
const KEY: &[u8] = include_bytes!("../../fixtures/workload-auth/server-key.der");
const ASSERTION: &str = "private-header.private-payload.private-signature";

struct Reply {
    status: u16,
    body: String,
}
impl Reply {
    fn json(body: serde_json::Value) -> Self {
        Self {
            status: 200,
            body: body.to_string(),
        }
    }
    fn token(value: &str, expiry: u64) -> Self {
        Self::json(serde_json::json!({
            "access_token": value, "token_type": "Bearer", "expires_in": expiry,
            "refresh_token": "ignored-refresh-secret"
        }))
    }
    fn status(status: u16) -> Self {
        Self {
            status,
            body: format!("server echoed {ASSERTION} access-secret"),
        }
    }
}

#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    headers: String,
    body: String,
}
impl Request {
    fn form(&self) -> BTreeMap<String, String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .into_owned()
            .collect()
    }
}

struct Server {
    origin: String,
    replies: mpsc::UnboundedSender<Reply>,
    requests: mpsc::UnboundedReceiver<Request>,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
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
        let origin = format!("https://{}", listener.local_addr().unwrap());
        let (replies, mut responses) = mpsc::unbounded_channel::<Reply>();
        let (records, requests) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                let mut stream = acceptor.accept(socket).await.unwrap();
                let mut data = Vec::new();
                let (header_end, length) = loop {
                    let mut chunk = [0; 4096];
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert!(n > 0, "request ended before headers");
                    data.extend_from_slice(&chunk[..n]);
                    assert!(data.len() < 32768);
                    if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&data[..end]).unwrap();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        break (end + 4, length);
                    }
                };
                while data.len() < header_end + length {
                    let mut chunk = [0; 4096];
                    let n = stream.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&chunk[..n]);
                }
                let headers = String::from_utf8(data[..header_end].to_vec()).unwrap();
                let mut line = headers.lines().next().unwrap().split_whitespace();
                let request = Request {
                    method: line.next().unwrap().into(),
                    path: line.next().unwrap().into(),
                    body: String::from_utf8(data[header_end..header_end + length].to_vec())
                        .unwrap(),
                    headers,
                };
                records.send(request).unwrap();
                // An unexpected retry receives a refusal, so a broken client fails
                // promptly rather than hanging while waiting for a scripted reply.
                let reply = responses.try_recv().unwrap_or_else(|_| Reply::status(500));
                let response = format!(
                    "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nLocation: /redirect-target\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                let _ = stream.shutdown().await;
            }
        });
        Self {
            origin,
            replies,
            requests,
            task,
        }
    }
    fn enqueue(&self, reply: Reply) {
        self.replies.send(reply).unwrap();
    }
    fn discovery(&self) {
        self.enqueue(Reply::json(serde_json::json!({
            "issuer": format!("{}/tenant", self.origin),
            "token_endpoint": format!("{}/token", self.origin)
        })));
    }
    fn exchange(&self, token: &str, expiry: u64) {
        self.discovery();
        self.enqueue(Reply::token(token, expiry));
    }
    fn session(&self, path: PathBuf, resource: &str) -> Session {
        let config = Config {
            token_file: path,
            expected_issuer: format!("{}/tenant", self.origin),
            token_endpoint: format!("{}/token", self.origin),
            resource: resource.into(),
            ..Default::default()
        };
        test_support::session(
            config,
            reqwest::Client::builder()
                .tls_certs_only([reqwest::Certificate::from_pem(CA).unwrap()])
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        )
        .unwrap()
    }
    fn drain(&mut self) -> Vec<Request> {
        let mut requests = Vec::new();
        while let Ok(request) = self.requests.try_recv() {
            requests.push(request);
        }
        requests
    }
}

fn source() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assertion");
    std::fs::write(&path, format!("{ASSERTION}\n")).unwrap();
    (dir, path)
}

#[tokio::test]
async fn https_fixture_chain_and_hostname_are_verified() {
    let server = Server::start().await;
    let (_dir, path) = source();
    let session = server.session(path, "https://api.example/mcp");
    server.enqueue(Reply::status(200));
    // Keep transport diagnostics in test code; production errors stay redacted.
    let response = session.client.get(&server.origin).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn https_grant_single_flight_and_resource_isolation() {
    let mut server = Server::start().await;
    let (_dir, path) = source();
    let session = Arc::new(server.session(path.clone(), "https://api.example/one"));
    server.exchange("first-access", 3600);
    let mut jobs = tokio::task::JoinSet::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(16));
    for _ in 0..16 {
        let session = session.clone();
        let barrier = barrier.clone();
        jobs.spawn(async move {
            barrier.wait().await;
            session.token().await
        });
    }
    while let Some(result) = jobs.join_next().await {
        assert_eq!(result.unwrap().unwrap(), "first-access");
    }
    assert_eq!(session.token().await.unwrap(), "first-access");
    let requests = server.drain();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(
        requests[0].path,
        "/.well-known/oauth-authorization-server/tenant"
    );
    assert!(requests[0].body.is_empty());
    assert!(!requests[0].headers.contains(ASSERTION));
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].path, "/token");
    assert!(
        !requests[1]
            .headers
            .to_ascii_lowercase()
            .contains("authorization:")
    );
    assert_eq!(
        requests[1].form(),
        BTreeMap::from([
            (
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:jwt-bearer".into()
            ),
            ("assertion".into(), ASSERTION.into()),
            ("resource".into(), "https://api.example/one".into()),
        ])
    );
    let other = server.session(path, "https://api.example/two");
    server.exchange("second-access", 3600);
    assert_eq!(other.token().await.unwrap(), "second-access");
    assert_eq!(
        server.drain()[1].form()["resource"],
        "https://api.example/two"
    );
    assert_eq!(session.token().await.unwrap(), "first-access");
    assert!(server.drain().is_empty());
}

#[tokio::test]
async fn https_expiry_and_invalidation_reopen_rotated_source() {
    let mut server = Server::start().await;
    let (_dir, path) = source();
    let session = server.session(path.clone(), "https://api.example/mcp");
    server.exchange("short-access", 1);
    assert_eq!(session.token().await.unwrap(), "short-access");
    std::fs::write(&path, "rotated.payload.signature").unwrap();
    assert_eq!(session.token().await.unwrap(), "short-access");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    server.exchange("renewed-access", 3600);
    assert_eq!(session.token().await.unwrap(), "renewed-access");
    std::fs::write(&path, "invalidated.payload.signature").unwrap();
    session
        .invalidate_acquisition(&session.acquisition().await.unwrap())
        .await;
    server.exchange("invalidated-access", 3600);
    assert_eq!(session.token().await.unwrap(), "invalidated-access");
    let requests = server.drain();
    let assertions: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "POST")
        .map(|r| r.form()["assertion"].clone())
        .collect();
    assert_eq!(
        assertions,
        [
            ASSERTION,
            "rotated.payload.signature",
            "invalidated.payload.signature"
        ]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn https_projected_symlink_rotation_is_reopened() {
    let mut server = Server::start().await;
    let (dir, first) = source();
    let second = dir.path().join("second");
    std::fs::write(&second, "second.payload.signature").unwrap();
    let link = dir.path().join("projected");
    std::os::unix::fs::symlink(&first, &link).unwrap();
    let session = server.session(link.clone(), "https://api.example/mcp");
    server.exchange("first-access", 1);
    assert_eq!(session.token().await.unwrap(), "first-access");
    let replacement = dir.path().join("replacement");
    std::os::unix::fs::symlink(&second, &replacement).unwrap();
    std::fs::rename(&replacement, &link).unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    server.exchange("second-access", 3600);
    assert_eq!(session.token().await.unwrap(), "second-access");
    std::os::unix::fs::symlink(&first, &replacement).unwrap();
    std::fs::rename(&replacement, &link).unwrap();
    session
        .invalidate_acquisition(&session.acquisition().await.unwrap())
        .await;
    server.exchange("third-access", 3600);
    assert_eq!(session.token().await.unwrap(), "third-access");
    let assertions: Vec<_> = server
        .drain()
        .iter()
        .filter(|r| r.method == "POST")
        .map(|r| r.form()["assertion"].clone())
        .collect();
    assert_eq!(
        assertions,
        [ASSERTION, "second.payload.signature", ASSERTION]
    );
}

#[tokio::test]
async fn https_discovery_redirect_and_mismatch_never_send_assertion() {
    for failure in ["redirect", "issuer", "endpoint"] {
        let mut server = Server::start().await;
        let (_dir, path) = source();
        let session = server.session(path, "https://api.example/mcp");
        if failure == "redirect" {
            server.enqueue(Reply::status(302));
        } else {
            server.enqueue(Reply::json(serde_json::json!({
                "issuer": if failure == "issuer" { "https://wrong.example".into() } else { format!("{}/tenant", server.origin) },
                "token_endpoint": if failure == "endpoint" { "https://wrong.example/token".into() } else { format!("{}/token", server.origin) }
            })));
        }
        let error = session.token().await.unwrap_err();
        assert!(!error.contains(ASSERTION));
        let requests = server.drain();
        assert_eq!(requests.len(), 1, "{failure}");
        assert_eq!(requests[0].method, "GET");
        assert!(requests[0].body.is_empty());
        assert!(!requests[0].headers.contains(ASSERTION));
    }
}

#[tokio::test]
async fn https_post_failures_are_not_retried_or_leaked() {
    for status in [302, 307, 400, 401, 503] {
        let mut server = Server::start().await;
        let (_dir, path) = source();
        let session = server.session(path, "https://api.example/mcp");
        server.discovery();
        server.enqueue(Reply::status(status));
        assert_eq!(
            session.token().await.unwrap_err(),
            "workload token exchange refused"
        );
        let requests = server.drain();
        assert_eq!(requests.len(), 2, "status {status}");
        assert_eq!(requests[1].method, "POST");
        // A later explicit acquisition can succeed: failures are not cached.
        server.exchange("recovered-access", 3600);
        assert_eq!(session.token().await.unwrap(), "recovered-access");
    }
}

#[tokio::test]
async fn https_delayed_rejection_preserves_newer_acquisition() {
    let mut server = Server::start().await;
    let (_dir, path) = source();
    let session = server.session(path, "https://api.example/mcp");
    server.exchange("same-access-value", 3600);
    let first = session.acquisition().await.unwrap();
    let delayed = first.clone();
    session.invalidate_acquisition(&first).await;
    // An issuer may return the same token bytes for a distinct exchange.
    server.exchange("same-access-value", 3600);
    let replacement = session.acquisition().await.unwrap();
    session.invalidate_acquisition(&delayed).await;
    let retained = session.acquisition().await.unwrap();
    assert!(Arc::ptr_eq(&replacement.0, &retained.0));
    assert_eq!(server.drain().len(), 4);
}

#[tokio::test]
async fn https_discovery_503_retries_once_only() {
    for recover in [false, true] {
        let mut server = Server::start().await;
        let (_dir, path) = source();
        let session = server.session(path, "https://api.example/mcp");
        server.enqueue(Reply::status(503));
        if recover {
            server.exchange("access", 3600);
        } else {
            server.enqueue(Reply::status(503));
        }
        let result = session.token().await;
        if recover {
            assert_eq!(result.unwrap(), "access");
        } else {
            assert_eq!(result.unwrap_err(), "workload issuer discovery refused");
        }
        let requests = server.drain();
        assert_eq!(requests.iter().filter(|r| r.method == "GET").count(), 2);
        assert_eq!(
            requests.iter().filter(|r| r.method == "POST").count(),
            usize::from(recover)
        );
    }
}

#[tokio::test]
async fn https_invalid_responses_are_rejected_without_caching_or_leaks() {
    let valid = serde_json::json!({"access_token":"access-secret", "token_type":"Bearer", "expires_in":3600});
    let mut bodies = vec![format!("not-json {ASSERTION}"), "{}".into()];
    for (field, value) in [
        ("token_type", serde_json::json!("Basic")),
        ("expires_in", serde_json::Value::Null),
        ("expires_in", serde_json::json!(0)),
        ("expires_in", serde_json::json!(-1)),
        ("expires_in", serde_json::json!("3600")),
        ("access_token", serde_json::json!("")),
        ("access_token", serde_json::json!("access-secret\n")),
        ("access_token", serde_json::json!("a".repeat(8193))),
    ] {
        let mut body = valid.clone();
        body[field] = value;
        bodies.push(body.to_string());
    }
    for body in bodies {
        let mut server = Server::start().await;
        let (_dir, path) = source();
        let session = server.session(path, "https://api.example/mcp");
        server.discovery();
        server.enqueue(Reply { status: 200, body });
        assert_eq!(
            session.token().await.unwrap_err(),
            "invalid workload token response"
        );
        assert_eq!(server.drain().len(), 2);
        server.exchange("valid-access", 3600);
        assert_eq!(session.token().await.unwrap(), "valid-access");
        assert_eq!(server.drain().len(), 2);
    }
}

fn exchange_reply(expiry: Option<u64>) -> Reply {
    let mut value = serde_json::json!({
        "access_token":"exchange-access", "token_type":"bEaReR", "issued_token_type": ACCESS_TOKEN
    });
    if let Some(expiry) = expiry {
        value["expires_in"] = expiry.into();
    }
    Reply::json(value)
}

#[tokio::test]
async fn https_explicit_grants_exact_fields_and_rotation() {
    for grant in [Grant::JwtBearer, Grant::TokenExchange] {
        let mut server = Server::start().await;
        let (_dir, path) = source();
        let mut session = server.session(path.clone(), "https://api.example/mcp");
        session.config.endpoint_mode = EndpointMode::Explicit;
        session.config.expected_issuer.clear();
        session.config.grant = grant;
        session.config.resource = "urn:example:inference".into();
        session.config.scope = Some("inference".into());
        session.config.audience = Some("backend".into());
        session
            .config
            .extensions
            .insert("federation_policy_id".into(), "example".into());
        session.config.validate().unwrap();
        server.enqueue(exchange_reply(Some(3600)));
        let first = session.acquisition().await.unwrap();
        assert_eq!(first.token(), "exchange-access");
        assert_eq!(session.token().await.unwrap(), first.token());
        let requests = server.drain();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/token");
        let mut expected = BTreeMap::from([
            ("resource".into(), "urn:example:inference".into()),
            ("scope".into(), "inference".into()),
            ("audience".into(), "backend".into()),
            ("federation_policy_id".into(), "example".into()),
        ]);
        let source_field = if grant == Grant::JwtBearer {
            expected.insert(
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:jwt-bearer".into(),
            );
            "assertion"
        } else {
            expected.insert(
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:token-exchange".into(),
            );
            expected.insert(
                "subject_token_type".into(),
                "urn:ietf:params:oauth:token-type:jwt".into(),
            );
            expected.insert("requested_token_type".into(), ACCESS_TOKEN.into());
            "subject_token"
        };
        expected.insert(source_field.into(), ASSERTION.into());
        assert_eq!(requests[0].form(), expected);
        session.invalidate_acquisition(&first).await;
        let large = format!("{}.bbb.ccc", "a".repeat(12000));
        std::fs::write(&path, &large).unwrap();
        server.enqueue(exchange_reply(Some(3600)));
        let renewed = session.acquisition().await.unwrap();
        assert_eq!(server.drain()[0].form()[source_field], large);
        session.invalidate_acquisition(&first).await;
        assert!(Arc::ptr_eq(
            &renewed.0,
            &session.acquisition().await.unwrap().0
        ));
        assert!(server.drain().is_empty());
    }
}

#[tokio::test]
async fn https_exchange_validates_response_and_does_not_cache_unknown_expiry() {
    let mut server = Server::start().await;
    let (_dir, path) = source();
    let mut session = server.session(path, "https://api.example/mcp");
    session.config.endpoint_mode = EndpointMode::Explicit;
    session.config.expected_issuer.clear();
    session.config.grant = Grant::TokenExchange;
    for (field, value) in [
        ("issued_token_type", serde_json::Value::Null),
        (
            "issued_token_type",
            serde_json::json!("urn:ietf:params:oauth:token-type:jwt"),
        ),
        ("token_type", serde_json::json!("N_A")),
        ("expires_in", serde_json::json!(0)),
        ("expires_in", serde_json::json!(-1)),
        ("expires_in", serde_json::json!(0.5)),
    ] {
        let mut response = serde_json::json!({"access_token":"secret-access", "token_type":"Bearer", "issued_token_type":ACCESS_TOKEN, "expires_in":3600});
        response[field] = value;
        server.enqueue(Reply::json(response));
        assert_eq!(
            session.token().await.unwrap_err(),
            "invalid workload token response"
        );
        assert_eq!(server.drain().len(), 1);
    }
    for _ in 0..2 {
        server.enqueue(exchange_reply(None));
        assert_eq!(session.token().await.unwrap(), "exchange-access");
        assert!(session.cached.lock().await.is_none());
        assert_eq!(server.drain().len(), 1);
    }
}

#[tokio::test]
async fn https_exchange_discovery_single_flight_and_expiry() {
    let mut server = Server::start().await;
    let (_dir, path) = source();
    let mut session = server.session(path.clone(), "https://api.example/mcp");
    session.config.grant = Grant::TokenExchange;
    let session = Arc::new(session);
    server.discovery();
    server.enqueue(exchange_reply(Some(1)));
    let mut jobs = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let session = session.clone();
        jobs.spawn(async move { session.acquisition().await });
    }
    while let Some(result) = jobs.join_next().await {
        assert_eq!(result.unwrap().unwrap().token(), "exchange-access");
    }
    let requests = server.drain();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[1].form()["subject_token"], ASSERTION);
    // Force the existing cache past expiry without timing-dependent sleeps.
    session.cached.lock().await.as_mut().unwrap().refresh_at = SystemTime::UNIX_EPOCH;
    std::fs::write(path, "rotated.jwt.signature").unwrap();
    server.discovery();
    server.enqueue(exchange_reply(Some(3600)));
    assert_eq!(session.token().await.unwrap(), "exchange-access");
    assert_eq!(
        server.drain()[1].form()["subject_token"],
        "rotated.jwt.signature"
    );
}

#[tokio::test]
async fn https_explicit_rejection_never_retries_or_retains_stale_token() {
    let mut server = Server::start().await;
    let (_dir, path) = source();
    let mut session = server.session(path, "https://api.example/mcp");
    session.config.endpoint_mode = EndpointMode::Explicit;
    session.config.expected_issuer.clear();
    session.config.grant = Grant::TokenExchange;
    for status in [302, 401, 503] {
        server.enqueue(exchange_reply(Some(3600)));
        let old = session.acquisition().await.unwrap();
        session.invalidate_acquisition(&old).await;
        server.drain();
        server.enqueue(Reply::status(status));
        assert_eq!(
            session.token().await.unwrap_err(),
            "workload token exchange refused"
        );
        assert!(session.cached.lock().await.is_none());
        assert_eq!(server.drain().len(), 1);
    }
}
