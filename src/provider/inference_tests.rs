//! Real TLS exchanges against a scripted localhost authorization server.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

use super::*;
use crate::workload_auth::{Config, Grant};
use agentkit_core::{Item, ItemKind, MetadataMap, SessionId, TurnId};
use std::path::PathBuf;
use std::{collections::BTreeMap, sync::Arc};
use tokio::io::AsyncReadExt;
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::mpsc, task::JoinHandle};
use tokio_rustls::{TlsAcceptor, rustls};

const CA: &[u8] = include_bytes!("../../fixtures/workload-auth/ca.pem");
const CERT: &[u8] = include_bytes!("../../fixtures/workload-auth/server.der");
const KEY: &[u8] = include_bytes!("../../fixtures/workload-auth/server-key.der");
const ASSERTION: &str = "private-header.private-payload.private-signature";

struct Reply {
    status: u16,
    body: String,
    content_type: &'static str,
}
impl Reply {
    fn json(body: serde_json::Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.to_string(),
        }
    }
    fn token(value: &str, expiry: u64) -> Self {
        Self::json(serde_json::json!({
            "access_token": value, "token_type": "Bearer", "expires_in": expiry,
            "issued_token_type": "urn:ietf:params:oauth:token-type:access_token",
            "refresh_token": "ignored-refresh-secret"
        }))
    }
    fn status(status: u16) -> Self {
        Self {
            status,
            content_type: "application/json",
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
                    "HTTP/1.1 {} Test\r\nContent-Type: {}\r\nLocation: /redirect-target\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reply.content_type,
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
    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .tls_certs_only([reqwest::Certificate::from_pem(CA).unwrap()])
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }
    fn adapter(
        &self,
        path: PathBuf,
        resource: &str,
        name: ProviderKind,
        streaming: bool,
    ) -> KitAdapter {
        let authentication = crate::provider::inference::test_support::authentication(
            Config {
                token_file: path,
                expected_issuer: format!("{}/tenant", self.origin),
                token_endpoint: format!("{}/token", self.origin),
                resource: resource.into(),
                grant: Grant::TokenExchange,
                ..Default::default()
            },
            self.client(),
        )
        .unwrap();
        let config = OpenRouterConfig::new(authentication, "test-model")
            .with_base_url(format!("{}/chat/completions", self.origin))
            .with_streaming(streaming);
        KitAdapter::Speakeasy(Box::new(SpeakeasyKitAdapter {
            provider: SpeakeasyProvider {
                openrouter: OpenRouterProvider::from(config),
                // Deliberately populate legacy values: federation must ignore them.
                api_key: "legacy-key-secret".into(),
                project: "legacy-project-secret".into(),
                chat_id: Some("legacy-chat-secret".into()),
                federation: true,
                name,
            },
            client: agentkit_http::Http::new(crate::provider::inference::Transport(self.client())),
        }))
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

fn completion(streaming: bool) -> Reply {
    if streaming {
        Reply {
            status: 200,
            content_type: "text/event-stream",
            body: concat!(
                "data: {\"id\":\"test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\n",
                "data: {\"id\":\"test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            ).into(),
        }
    } else {
        Reply::json(serde_json::json!({
            "id": "test", "object": "chat.completion", "created": 1,
            "model": "test-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]
        }))
    }
}

async fn run_turn(adapter: &KitAdapter) -> Result<Vec<ModelTurnEvent>, LoopError> {
    let mut session = adapter
        .start_session(SessionConfig::new("federation-contract"))
        .await?;
    let mut turn = session
        .begin_turn(
            TurnRequest {
                session_id: SessionId::new("federation-contract"),
                turn_id: TurnId::new("turn"),
                transcript: vec![Item::text(ItemKind::User, "hello")],
                available_tools: Vec::new(),
                cache: None,
                metadata: MetadataMap::new(),
            },
            None,
        )
        .await?;
    let mut events = Vec::new();
    while let Some(event) = turn.next_event(None).await? {
        events.push(event);
    }
    Ok(events)
}

fn assert_exchange(request: &Request, resource: &str) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/token");
    let form = request.form();
    assert_eq!(
        form,
        BTreeMap::from([
            (
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:token-exchange".into()
            ),
            ("subject_token".into(), ASSERTION.into()),
            (
                "subject_token_type".into(),
                "urn:ietf:params:oauth:token-type:jwt".into()
            ),
            (
                "requested_token_type".into(),
                "urn:ietf:params:oauth:token-type:access_token".into()
            ),
            ("resource".into(), resource.into()),
        ])
    );
    let headers = request.headers.to_ascii_lowercase();
    assert!(headers.contains("application/x-www-form-urlencoded"));
    assert!(!headers.contains("authorization:"));
}

fn assert_inference(request: &Request, token: &str, streaming: bool) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/chat/completions");
    let headers = request.headers.to_ascii_lowercase();
    assert!(headers.contains(&format!("authorization: bearer {token}\r\n")));
    for forbidden in [
        "gram-key:",
        "gram-project:",
        "gram-chat-id:",
        "legacy-",
        ASSERTION,
    ] {
        assert!(
            !headers.contains(forbidden),
            "unexpected header: {forbidden}"
        );
    }
    let body: Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["messages"][0]["content"], "hello");
    assert_eq!(
        body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        streaming
    );
    assert!(!request.body.contains(ASSERTION));
    assert!(!request.body.contains(token));
}

#[tokio::test]
async fn federation_completion_stream_and_nonstream_use_exchanged_bearer_only() {
    for name in [ProviderKind::Speakeasy, ProviderKind::OpenRouter] {
        for streaming in [false, true] {
            let mut server = Server::start().await;
            let (_dir, path) = source();
            let resource = "https://inference.example/resource";
            let adapter = server.adapter(path, resource, name, streaming);
            server.exchange("inference-secret", 3600);
            server.enqueue(completion(streaming));
            let events = run_turn(&adapter).await.unwrap();
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ModelTurnEvent::Finished(_)))
            );
            let requests = server.drain();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[0].method, "GET");
            assert_exchange(&requests[1], resource);
            assert_inference(&requests[2], "inference-secret", streaming);
            assert_eq!(
                requests[2]
                    .headers
                    .to_ascii_lowercase()
                    .contains("x-gram-source: kit"),
                name == ProviderKind::Speakeasy
            );
        }
    }
}

#[tokio::test]
async fn federation_refusals_are_redacted_and_never_replayed() {
    for streaming in [false, true] {
        for status in [401, 429, 503] {
            let mut server = Server::start().await;
            let (_dir, path) = source();
            let adapter = server.adapter(
                path,
                "https://inference.example/resource",
                ProviderKind::Speakeasy,
                streaming,
            );
            server.exchange("access-secret", 3600);
            server.enqueue(Reply::status(status));
            let error = match run_turn(&adapter).await {
                Ok(_) => panic!("HTTP {status} unexpectedly succeeded"),
                Err(error) => format!("{error:?}"),
            };
            assert!(
                error.contains("workload inference response failed"),
                "{error}"
            );
            for secret in [
                ASSERTION,
                "access-secret",
                "server echoed",
                "legacy-key-secret",
            ] {
                assert!(!error.contains(secret), "credential leaked in refusal");
            }
            let requests = server.drain();
            assert_eq!(
                requests.len(),
                3,
                "HTTP {status} must not replay inference or exchange"
            );
            assert_inference(&requests[2], "access-secret", streaming);
        }
    }
}

#[tokio::test]
async fn federation_providers_keep_independent_tokens_and_cached_grants() {
    let mut server = Server::start().await;
    let (_dir, path) = source();
    let first = server.adapter(
        path.clone(),
        "https://inference.example/first",
        ProviderKind::Speakeasy,
        false,
    );
    let second = server.adapter(
        path,
        "https://inference.example/second",
        ProviderKind::OpenRouter,
        false,
    );
    for (adapter, token, resource) in [
        (&first, "first-secret", "https://inference.example/first"),
        (&second, "second-secret", "https://inference.example/second"),
    ] {
        server.exchange(token, 3600);
        server.enqueue(completion(false));
        run_turn(adapter).await.unwrap();
        let requests = server.drain();
        assert_eq!(requests.len(), 3);
        assert_exchange(&requests[1], resource);
        assert_inference(&requests[2], token, false);
    }
    for (adapter, token) in [(&first, "first-secret"), (&second, "second-secret")] {
        server.enqueue(completion(false));
        run_turn(adapter).await.unwrap();
        let requests = server.drain();
        assert_eq!(
            requests.len(),
            1,
            "provider must reuse only its own cached grant"
        );
        assert_inference(&requests[0], token, false);
    }
}

#[tokio::test]
async fn federation_success_status_error_frames_are_redacted() {
    for (streaming, body) in [
        (
            true,
            "event: error\ndata: access-secret private-header.private-payload.private-signature\n\n",
        ),
        (
            true,
            "data: {\"error\":{\"message\":\"access-secret\"}}\n\n",
        ),
        (
            true,
            "event: error\ndata: {\"message\":\"access-secret\"}\n\n",
        ),
        (false, "{\"error\":{\"message\":\"access-secret\"}}"),
        (false, "access-secret malformed response"),
    ] {
        let mut server = Server::start().await;
        let (_dir, path) = source();
        let adapter = server.adapter(
            path,
            "https://inference.example/resource",
            ProviderKind::OpenRouter,
            streaming,
        );
        server.exchange("access-secret", 3600);
        server.enqueue(Reply {
            status: 200,
            content_type: if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
            body: body.into(),
        });
        let error = match run_turn(&adapter).await {
            Ok(_) => panic!("error frame unexpectedly succeeded"),
            Err(error) => format!("{error:?}"),
        };
        assert!(
            error.contains("workload inference response failed"),
            "{error}"
        );
        assert!(!error.contains("access-secret"));
        assert!(!error.contains(ASSERTION));
        assert_eq!(server.drain().len(), 3);
    }
}

#[tokio::test]
async fn federation_speakeasy_explicit_jwt_bearer_contract_and_no_auth_replay() {
    let mut server = Server::start().await;
    let (_dir, path) = source();
    let authentication = crate::provider::inference::test_support::authentication(
        Config {
            token_file: path,
            token_endpoint: format!("{}/token", server.origin),
            endpoint_mode: crate::workload_auth::EndpointMode::Explicit,
            resource: "urn:gram:inference".into(),
            extensions: BTreeMap::from([(
                "project_id".into(),
                "00000000-0000-0000-0000-000000000001".into(),
            )]),
            ..Default::default()
        },
        server.client(),
    )
    .unwrap();
    let previous = agentkit_http::Authentication::bearer("old-secret")
        .authenticate(None)
        .await
        .unwrap();
    let error = authentication
        .authenticate(Some(&previous))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("not replayed"));
    assert!(!error.contains("old-secret"));
    assert!(server.drain().is_empty());
    let config = OpenRouterConfig::new(authentication, "test-model")
        .with_base_url(format!("{}/chat/completions", server.origin))
        .with_streaming(false);
    let adapter = KitAdapter::Speakeasy(Box::new(SpeakeasyKitAdapter {
        provider: SpeakeasyProvider {
            openrouter: OpenRouterProvider::from(config),
            api_key: String::new(),
            project: String::new(),
            chat_id: None,
            federation: true,
            name: ProviderKind::Speakeasy,
        },
        client: agentkit_http::Http::new(crate::provider::inference::Transport(server.client())),
    }));
    server.enqueue(Reply::json(serde_json::json!({
        "access_token": "speakeasy-secret", "token_type": "Bearer",
        "expires_in": 900, "authorization_expires_in": 900
    })));
    server.enqueue(completion(false));
    run_turn(&adapter).await.unwrap();
    let requests = server.drain();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].form(),
        BTreeMap::from([
            (
                "grant_type".into(),
                "urn:ietf:params:oauth:grant-type:jwt-bearer".into()
            ),
            ("assertion".into(), ASSERTION.into()),
            ("resource".into(), "urn:gram:inference".into()),
            (
                "project_id".into(),
                "00000000-0000-0000-0000-000000000001".into()
            ),
        ])
    );
    assert_inference(&requests[1], "speakeasy-secret", false);
}

fn workload_config(endpoint: &str) -> crate::provider::inference::Config {
    crate::provider::inference::Config {
        endpoint: endpoint.into(),
        workload: Config {
            token_file: PathBuf::from("/missing/projected-assertion"),
            token_endpoint: "https://issuer.example/token".into(),
            endpoint_mode: crate::workload_auth::EndpointMode::Explicit,
            resource: "urn:gram:inference".into(),
            ..Default::default()
        },
    }
}

#[test]
fn federation_configuration_is_validated_redacted_and_provider_scoped() {
    use crate::provider::inference::Instances;
    for endpoint in [
        "http://inference.example/chat/completions",
        "https://secret@inference.example/chat/completions",
        "https://inference.example/chat/completions?token=secret",
        "https://inference.example/chat/completions#secret",
    ] {
        let error = Instances::new(BTreeMap::from([(
            "speakeasy".into(),
            workload_config(endpoint),
        )]))
        .err()
        .unwrap();
        assert!(!error.contains("secret"));
    }
    assert!(
        Instances::new(BTreeMap::from([(
            "anthropic".into(),
            workload_config("https://inference.example")
        )]))
        .is_err()
    );
    let config = workload_config("https://inference.example/chat/completions");
    let debug = format!("{config:?}");
    assert!(!debug.contains("inference.example"));
    assert!(!debug.contains("projected-assertion"));
    let instances = Instances::new(BTreeMap::from([("speakeasy".into(), config)])).unwrap();
    assert!(instances.get(ProviderKind::OpenRouter).is_none());
    assert!(instances.get(ProviderKind::Speakeasy).is_some());
    let adapter = SelectableAdapter::new(ProviderKind::Speakeasy, "test-model")
        .unwrap()
        .with_inference(instances);
    assert!(
        adapter
            .catalog_providers()
            .contains(&ProviderKind::Speakeasy)
    );
    adapter
        .select(ModelSelection::new(
            ProviderKind::Speakeasy,
            "another-model",
        ))
        .unwrap();
    adapter
        .select_reasoning_effort(Some(ReasoningEffort::Low))
        .unwrap();
}

#[tokio::test]
async fn federation_missing_assertion_fails_without_legacy_credentials() {
    let instances = crate::provider::inference::Instances::new(BTreeMap::from([(
        "speakeasy".into(),
        workload_config("https://inference.example/chat/completions"),
    )]))
    .unwrap();
    let adapter = KitAdapter::new_with_inference(
        ProviderKind::Speakeasy,
        "test-model".into(),
        Default::default(),
        None,
        None,
        Default::default(),
        &instances,
    )
    .unwrap();
    let error = match run_turn(&adapter).await {
        Ok(_) => panic!("missing assertion unexpectedly succeeded"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("workload"), "{error}");
    assert!(!error.contains("projected-assertion"));
    assert!(!error.contains("login"));
}
