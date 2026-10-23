//! Per-request workload authentication, including catalog reads and SSE reconnects.
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use agentkit_mcp::{ClientJsonRpcMessage, McpHttpClient, McpSseStream};
use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};

use crate::workload_auth::{Acquisition, Config, Session};

type Error = StreamableHttpError<reqwest::Error>;

pub(super) struct Client {
    destination: String,
    session: Session,
    http: reqwest::Client,
}

impl Client {
    pub(super) fn new(destination: &str, config: Config) -> Result<Self, String> {
        config.validate()?;
        let url = url::Url::parse(destination).map_err(|_| "invalid workload MCP URL")?;
        let resource =
            url::Url::parse(&config.resource).map_err(|_| "invalid workload MCP resource")?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url != resource
        {
            return Err(
                "workload MCP URL must be HTTPS and match the configured resource exactly".into(),
            );
        }
        let http = PostStatus::client(reqwest::Client::builder())
            .map_err(|_| "could not build workload MCP HTTP client")?;
        Ok(Self {
            destination: destination.to_owned(),
            session: Session::new(config)?,
            http,
        })
    }

    async fn post_with_client(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Error> {
        let acquisition = self.authorization(&uri, &custom_headers).await?;
        let status = PostStatus::default();
        let result = status
            .observe(self.http.post_message(
                uri,
                message,
                session_id,
                Some(acquisition.token().to_owned()),
                custom_headers,
            ))
            .await;
        self.finish(&acquisition, status.rejected(), result).await
    }

    async fn authorization(
        &self,
        uri: &str,
        headers: &HashMap<HeaderName, HeaderValue>,
    ) -> Result<Acquisition, Error> {
        if uri != self.destination
            || headers
                .keys()
                .any(|name| name == reqwest::header::AUTHORIZATION || name == reqwest::header::HOST)
        {
            return Err(Self::error(
                "workload MCP transport destination or authorization override rejected",
            ));
        }
        self.session.acquisition().await.map_err(Self::error)
    }

    fn error(message: impl Into<String>) -> Error {
        StreamableHttpError::Io(std::io::Error::other(message.into()))
    }

    async fn finish<T>(
        &self,
        acquisition: &Acquisition,
        raw_rejection: bool,
        result: Result<T, Error>,
    ) -> Result<T, Error> {
        let rejected = match &result {
            Err(
                StreamableHttpError::AuthRequired(_) | StreamableHttpError::InsufficientScope(_),
            ) => true,
            Err(StreamableHttpError::Client(error)) => matches!(
                error.status(),
                Some(reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN)
            ),
            _ => false,
        };
        if raw_rejection || rejected {
            // Do not replay a possibly side-effecting POST or follow challenge URLs.
            self.session.invalidate_acquisition(acquisition).await;
        }
        result
    }
}

// Pinned reqwest 0.13.5 / tower 0.5.3 call the classifier synchronously in
// ResponseFuture::poll, inside the scoped POST future. Tokio restores this scope
// for each poll, including worker migration; concurrent POSTs never share a flag.
// Revalidate this seam when upgrading reqwest or tower. rmcp retains its parser.
tokio::task_local! {
    static POST_REJECTION: Arc<AtomicBool>;
}

#[derive(Default)]
struct PostStatus(Arc<AtomicBool>);

impl PostStatus {
    fn client(builder: reqwest::ClientBuilder) -> Result<reqwest::Client, Error> {
        let policy = reqwest::retry::never().classify_fn(move |response| {
            if matches!(
                response.status(),
                Some(reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN)
            ) {
                let _ = POST_REJECTION.try_with(|rejected| rejected.store(true, Ordering::Release));
            }
            // Always return success: this observes status, never retries a POST.
            // Do not set max_retries_per_request(0): tower then skips classification.
            response.success()
        });
        builder
            .redirect(reqwest::redirect::Policy::none())
            .retry(policy)
            .build()
            .map_err(StreamableHttpError::Client)
    }

    async fn observe<F: std::future::Future>(&self, future: F) -> F::Output {
        POST_REJECTION.scope(Arc::clone(&self.0), future).await
    }

    fn rejected(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[async_trait]
impl McpHttpClient for Client {
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Error> {
        self.post_with_client(uri, message, session_id, custom_headers)
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Error> {
        let acquisition = self.authorization(&uri, &custom_headers).await?;
        self.finish(
            &acquisition,
            false,
            self.http
                .delete_session(
                    uri,
                    session_id,
                    Some(acquisition.token().to_owned()),
                    custom_headers,
                )
                .await,
        )
        .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<McpSseStream, Error> {
        let acquisition = self.authorization(&uri, &custom_headers).await?;
        self.finish(
            &acquisition,
            false,
            self.http
                .get_stream(
                    uri,
                    session_id,
                    last_event_id,
                    Some(acquisition.token().to_owned()),
                    custom_headers,
                )
                .await,
        )
        .await
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;

    async fn tls_script(
        replies: impl FnOnce(&str) -> Vec<(u16, String, String)>,
    ) -> (String, tokio::task::JoinHandle<(Vec<String>, usize)>) {
        tls_connections(replies, false).await
    }

    async fn tls_connections(
        replies: impl FnOnce(&str) -> Vec<(u16, String, String)>,
        keep_alive: bool,
    ) -> (String, tokio::task::JoinHandle<(Vec<String>, usize)>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio_rustls::{TlsAcceptor, rustls};
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(
                    include_bytes!("../../../fixtures/workload-auth/server.der").to_vec(),
                )],
                rustls::pki_types::PrivatePkcs8KeyDer::from(
                    include_bytes!("../../../fixtures/workload-auth/server-key.der").to_vec(),
                )
                .into(),
            )
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("https://{}/mcp", listener.local_addr().unwrap());
        let origin = format!("https://{}", listener.local_addr().unwrap());
        let replies = replies(&origin);
        let task = tokio::spawn(async move {
            let acceptor = TlsAcceptor::from(Arc::new(config));
            let mut requests = Vec::new();
            let mut connection = None;
            let mut accepted = 0;
            for (status, content_type, body) in replies {
                let connection_header = if keep_alive { "keep-alive" } else { "close" };
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: {connection_header}\r\n\r\n{body}",
                    body.len()
                );
                if connection.is_none() {
                    let (socket, _) = listener.accept().await.unwrap();
                    connection = Some(acceptor.accept(socket).await.unwrap());
                    accepted += 1;
                }
                let stream = connection.as_mut().unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    bytes.extend_from_slice(&chunk[..read]);
                    assert!(bytes.len() < 32768);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if bytes.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                stream.write_all(response.as_bytes()).await.unwrap();
                if !keep_alive {
                    let _ = stream.shutdown().await;
                    connection = None;
                }
                requests.push(String::from_utf8(bytes).unwrap());
            }
            (requests, accepted)
        });
        (uri, task)
    }

    async fn tls_reply(
        status: u16,
        content_type: &str,
        body: &str,
    ) -> (String, tokio::task::JoinHandle<(Vec<String>, usize)>) {
        tls_script(|_| vec![(status, content_type.to_owned(), body.to_owned())]).await
    }

    fn tls_client() -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .tls_certs_only([reqwest::Certificate::from_pem(include_bytes!(
                "../../../fixtures/workload-auth/ca.pem"
            ))
            .unwrap()])
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
    }

    #[tokio::test]
    async fn sequential_posts_reuse_one_tls_connection() {
        let reply = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let (uri, mcp) = tls_connections(
            |_| {
                vec![
                    (200, "application/json".into(), reply.into()),
                    (200, "application/json".into(), reply.into()),
                ]
            },
            true,
        )
        .await;
        let (issuer, auth) = tls_script(|origin| {
            vec![
                (
                    200,
                    "application/json".into(),
                    serde_json::json!({"issuer":origin,"token_endpoint":format!("{origin}/token")})
                        .to_string(),
                ),
                (
                    200,
                    "application/json".into(),
                    serde_json::json!({"access_token":"T1","token_type":"Bearer","expires_in":600})
                        .to_string(),
                ),
            ]
        })
        .await;
        let directory = tempfile::tempdir().unwrap();
        let token_file = directory.path().join("assertion.jwt");
        std::fs::write(&token_file, "header.payload.signature").unwrap();
        let origin = issuer.strip_suffix("/mcp").unwrap();
        let session = crate::workload_auth::test_support::session(
            Config {
                token_file,
                expected_issuer: origin.into(),
                token_endpoint: format!("{origin}/token"),
                resource: uri.clone(),
            },
            tls_client().build().unwrap(),
        )
        .unwrap();
        let client = Client {
            destination: uri.clone(),
            session,
            http: PostStatus::client(tls_client()).unwrap(),
        };
        for _ in 0..2 {
            let message =
                serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
                    .unwrap();
            let result = client
                .post_message(uri.clone().into(), message, None, None, HashMap::new())
                .await
                .unwrap();
            assert!(matches!(result, StreamableHttpPostResponse::Json(_, _)));
        }
        let (requests, accepted) = mcp.await.unwrap();
        assert_eq!(
            accepted, 1,
            "both POSTs must use the same accepted TLS connection"
        );
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.starts_with("POST /mcp ")
                    && request.to_lowercase().contains("authorization: bearer t1"))
        );
        auth.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_post_status_scopes_are_isolated() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"rejected"}}"#;
        let (rejected_uri, rejected_server) = tls_reply(401, "application/json", body).await;
        let (successful_uri, successful_server) = tls_reply(200, "application/json", body).await;
        let http = PostStatus::client(tls_client()).unwrap();
        let rejected = PostStatus::default();
        let successful = PostStatus::default();
        let message = || {
            serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
                .unwrap()
        };
        // Poll both scoped futures in one task: a thread-local/unscoped flag would
        // let the rejection contaminate its concurrently polled neighbour.
        let (first, second) = tokio::join!(
            rejected.observe(http.post_message(
                rejected_uri.into(),
                message(),
                None,
                Some("T1".into()),
                HashMap::new()
            )),
            successful.observe(http.post_message(
                successful_uri.into(),
                message(),
                None,
                Some("T2".into()),
                HashMap::new()
            )),
        );
        assert!(matches!(first, Ok(StreamableHttpPostResponse::Json(_, _))));
        assert!(matches!(second, Ok(StreamableHttpPostResponse::Json(_, _))));
        assert!(rejected.rejected());
        assert!(!successful.rejected());
        assert!(POST_REJECTION.try_with(|_| ()).is_err());
        rejected_server.await.unwrap();
        successful_server.await.unwrap();

        // Calls without a POST scope (including GET/DELETE) have no rejection
        // slot to update, even when the pooled client's classifier runs.
        let (uri, server) = tls_reply(403, "application/json", body).await;
        let _ = http
            .post_message(
                uri.into(),
                message(),
                None,
                Some("T3".into()),
                HashMap::new(),
            )
            .await;
        assert!(!successful.rejected());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn bare_post_rejection_reacquires_on_next_operation() {
        for code in [401, 403] {
            for json in [false, true] {
                let (uri, server) = tls_script(|origin| {
                    let discovery = serde_json::json!({"issuer": origin, "token_endpoint": format!("{origin}/token")}).to_string();
                    let token = |value| serde_json::json!({"access_token": value, "token_type": "Bearer", "expires_in": 600}).to_string();
                    vec![
                        (200, "application/json".into(), discovery.clone()),
                        (200, "application/json".into(), token("T1")),
                        (code, if json { "application/json" } else { "text/plain" }.into(), if json { r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"rejected"}}"# } else { "rejected" }.into()),
                        (200, "application/json".into(), discovery),
                        (200, "application/json".into(), token("T2")),
                        (200, "application/json".into(), r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.into()),
                    ]
                }).await;
                let directory = tempfile::tempdir().unwrap();
                let path = directory.path().join("assertion.jwt");
                std::fs::write(&path, "header.payload.signature").unwrap();
                let origin = uri.strip_suffix("/mcp").unwrap();
                let session = crate::workload_auth::test_support::session(
                    Config {
                        token_file: path,
                        expected_issuer: origin.into(),
                        token_endpoint: format!("{origin}/token"),
                        resource: uri.clone(),
                    },
                    tls_client().build().unwrap(),
                )
                .unwrap();
                let client = Client {
                    destination: uri.clone(),
                    session,
                    http: PostStatus::client(tls_client()).unwrap(),
                };
                let message = || {
                    serde_json::from_value(
                        serde_json::json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
                    )
                    .unwrap()
                };
                let first = client
                    .post_message(uri.clone().into(), message(), None, None, HashMap::new())
                    .await;
                assert_eq!(first.is_ok(), json);
                let second = client
                    .post_message(uri.into(), message(), None, None, HashMap::new())
                    .await;
                assert!(matches!(second, Ok(StreamableHttpPostResponse::Json(_, _))));
                let (requests, _) = server.await.unwrap();
                assert_eq!(requests.len(), 6);
                assert!(requests[1].starts_with("POST /token "));
                assert!(
                    requests[2]
                        .to_lowercase()
                        .contains("authorization: bearer t1")
                );
                assert!(requests[4].starts_with("POST /token "));
                assert!(
                    requests[5]
                        .to_lowercase()
                        .contains("authorization: bearer t2")
                );
            }
        }
    }

    #[tokio::test]
    async fn bare_post_rejections_preserve_raw_status_without_replaying() {
        for code in [401, 403] {
            for json in [false, true] {
                let body = if json {
                    r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"rejected"}}"#
                } else {
                    "rejected"
                };
                let (uri, server) = tls_reply(
                    code,
                    if json {
                        "application/json"
                    } else {
                        "text/plain"
                    },
                    body,
                )
                .await;
                let status = PostStatus::default();
                let http = PostStatus::client(
                    reqwest::Client::builder()
                        .tls_certs_only([reqwest::Certificate::from_pem(include_bytes!(
                            "../../../fixtures/workload-auth/ca.pem"
                        ))
                        .unwrap()])
                        .no_proxy()
                        .timeout(std::time::Duration::from_secs(5)),
                )
                .unwrap();
                let message = serde_json::from_value(
                    serde_json::json!({"jsonrpc":"2.0","id":1,"method":"ping"}),
                )
                .unwrap();
                let result = status
                    .observe(http.post_message(
                        uri.into(),
                        message,
                        None,
                        Some("T1".into()),
                        HashMap::new(),
                    ))
                    .await;
                assert!(status.rejected(), "bare HTTP {code} was lost");
                if json {
                    assert!(matches!(result, Ok(StreamableHttpPostResponse::Json(_, _))));
                } else {
                    assert!(matches!(
                        result,
                        Err(StreamableHttpError::UnexpectedServerResponse(_))
                    ));
                }
                assert!(
                    server.await.unwrap().0[0]
                        .to_lowercase()
                        .contains("authorization: bearer t1")
                );
            }
        }
    }

    #[tokio::test]
    async fn post_status_observation_preserves_json_and_sse_success() {
        use futures_util::StreamExt;
        for sse in [false, true] {
            let body = if sse {
                "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n"
            } else {
                r#"{"jsonrpc":"2.0","id":1,"result":{}}"#
            };
            let (uri, server) = tls_reply(
                200,
                if sse {
                    "text/event-stream"
                } else {
                    "application/json"
                },
                body,
            )
            .await;
            let status = PostStatus::default();
            let http = PostStatus::client(
                reqwest::Client::builder()
                    .tls_certs_only([reqwest::Certificate::from_pem(include_bytes!(
                        "../../../fixtures/workload-auth/ca.pem"
                    ))
                    .unwrap()])
                    .no_proxy()
                    .timeout(std::time::Duration::from_secs(5)),
            )
            .unwrap();
            let message =
                serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
                    .unwrap();
            let result = status
                .observe(http.post_message(
                    uri.into(),
                    message,
                    None,
                    Some("T1".into()),
                    HashMap::new(),
                ))
                .await
                .unwrap();
            assert!(!status.rejected());
            if sse {
                let StreamableHttpPostResponse::Sse(mut events, _) = result else {
                    panic!("expected SSE")
                };
                assert!(events.next().await.unwrap().is_ok());
            } else {
                assert!(matches!(result, StreamableHttpPostResponse::Json(_, _)));
            }
            server.await.unwrap();
        }
    }

    fn config() -> Config {
        serde_json::from_value(serde_json::json!({
            "tokenFile": "/nonexistent/kit-workload-test-token",
            "expectedIssuer": "https://issuer.example",
            "tokenEndpoint": "https://issuer.example/token",
            "resource": "https://mcp.example/mcp"
        }))
        .unwrap()
    }

    #[test]
    fn workload_destination_is_bound_to_resource() {
        for destination in [
            "http://mcp.example/mcp",
            "https://other.example/mcp",
            "https://mcp.example/other",
            "https://user@mcp.example/mcp",
            "https://mcp.example/mcp#fragment",
        ] {
            assert!(Client::new(destination, config()).is_err());
        }
        assert!(Client::new("https://mcp.example/mcp", config()).is_ok());
    }

    #[tokio::test]
    async fn workload_transport_rejects_overrides_before_acquisition() {
        let client = Client::new("https://mcp.example/mcp", config()).unwrap();
        let error = client
            .authorization("https://other.example/mcp", &HashMap::new())
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("override rejected"));
        for name in [reqwest::header::AUTHORIZATION, reqwest::header::HOST] {
            let headers = HashMap::from([(name, HeaderValue::from_static("override"))]);
            assert!(
                client
                    .authorization("https://mcp.example/mcp", &headers)
                    .await
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("override rejected")
            );
        }
        assert!(
            client
                .authorization("https://mcp.example/mcp", &HashMap::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn workload_prepared_transport_requires_credentials_before_http() {
        let value = serde_json::json!({
            "url": "https://mcp.example/mcp",
            "auth": {
                "type": "workload_identity",
                "tokenFile": "/nonexistent/kit-workload-test-token",
                "expectedIssuer": "https://issuer.example",
                "tokenEndpoint": "https://issuer.example/token",
                "resource": "https://mcp.example/mcp"
            }
        });
        let server = serde_json::from_value(value.clone()).unwrap();
        let (binding, _) =
            super::super::prepare_server("workload", server, Vec::new(), None).unwrap();
        let super::super::McpTransportBinding::StreamableHttp(transport) = binding else {
            panic!("expected workload authentication configuration")
        };
        let client = transport.http_client.unwrap();
        assert!(
            client
                .get_stream(
                    Arc::from("https://mcp.example/mcp"),
                    None,
                    None,
                    None,
                    HashMap::new()
                )
                .await
                .is_err()
        );
        for field in ["bearerToken", "headers"] {
            let mut conflict = value.clone();
            conflict[field] = if field == "headers" {
                serde_json::json!({"Authorization": "Bearer static"})
            } else {
                serde_json::json!("static")
            };
            let server = serde_json::from_value(conflict).unwrap();
            assert!(super::super::prepare_server("workload", server, Vec::new(), None).is_err());
        }
    }

    #[tokio::test]
    async fn workload_configuration_never_uses_stored_oauth() {
        let mut value = serde_json::json!({
            "type": "workload_identity",
            "tokenFile": "/nonexistent/kit-workload-test-token",
            "expectedIssuer": "https://issuer.example",
            "tokenEndpoint": "https://issuer.example/token",
            "resource": "https://mcp.example/mcp"
        });
        let config: super::super::auth::Config = serde_json::from_value(value.clone()).unwrap();
        assert!(config.workload().is_some());
        assert!(config.machine().is_none());
        let storage = super::super::CredentialStorage::Memory;
        assert!(
            super::super::auth::restore("https://mcp.example/mcp", &config, &storage)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            super::super::auth::begin("https://mcp.example/mcp", &config, &storage, None, None)
                .await
                .is_err()
        );
        value["clientId"] = serde_json::json!("not-supported");
        assert!(serde_json::from_value::<super::super::auth::Config>(value).is_err());
    }
}
