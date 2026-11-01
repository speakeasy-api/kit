use std::{convert::Infallible, io, path::Path, sync::Arc, time::Duration};

use futures_util::future::{Either, select};
use tokio::{task::JoinSet, time::timeout};
use tokio_util::sync::CancellationToken;

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, HeaderValue, Request, Response, StatusCode, header},
};
use subtle::ConstantTimeEq as _;
use tower::ServiceExt as _;

use crate::runtime::Runtime;

#[derive(Clone)]
pub(crate) struct BearerToken(Vec<u8>);

impl BearerToken {
    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        let mut token = crate::resilient_fs::read(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "could not read server credential {}: {error}",
                    path.display()
                ),
            )
        })?;
        if token.ends_with(b"\n") {
            token.pop();
            if token.ends_with(b"\r") {
                token.pop();
            }
        }
        if token.is_empty() || token.iter().any(u8::is_ascii_whitespace) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "server credential {} must contain one non-empty bearer token",
                    path.display()
                ),
            ));
        }
        Ok(Self(token))
    }

    pub(crate) fn authorizes(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
        else {
            return false;
        };
        let Some((scheme, token)) = value.split_once(' ') else {
            return false;
        };
        scheme.eq_ignore_ascii_case("bearer")
            && !token.is_empty()
            && bool::from(self.0.as_slice().ct_eq(token.as_bytes()))
    }
}

pub struct HttpServer {
    address: std::net::SocketAddr,
    stop_accepting: CancellationToken,
    accepts_stopped: CancellationToken,
    shutdown_connections: CancellationToken,
    task: Option<tokio::task::JoinHandle<io::Result<()>>>,
}

impl HttpServer {
    pub fn address(&self) -> std::net::SocketAddr {
        self.address
    }

    pub async fn stop_accepting(&self) {
        self.stop_accepting.cancel();
        self.accepts_stopped.cancelled().await;
    }

    pub fn shutdown_connections(&self) {
        self.shutdown_connections.cancel();
    }

    pub fn shutdown(&self) {
        self.stop_accepting.cancel();
        self.shutdown_connections();
    }

    pub async fn join(&mut self) -> io::Result<()> {
        let result = self
            .task
            .as_mut()
            .ok_or_else(|| io::Error::other("HTTP server has already been joined"))?
            .await;
        self.task.take();
        result.map_err(|error| io::Error::other(format!("HTTP server task failed: {error}")))?
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        self.stop_accepting.cancel();
        self.shutdown_connections.cancel();
    }
}

/// Opt-in remote ACP HTTP features. Defaults preserve same-origin access only.
#[derive(Clone, Debug, Default)]
pub struct HttpOptions {
    pub health_endpoint: bool,
    pub allowed_origins: Vec<String>,
}

impl HttpOptions {
    fn cors(&self) -> io::Result<agent_client_protocol_http::CorsOptions> {
        for origin in &self.allowed_origins {
            let valid = url::Url::parse(origin).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.origin().ascii_serialization() == *origin
            });
            if !valid {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ACP CORS origins must be exact http(s) origins without paths, credentials, or wildcards",
                ));
            }
        }
        if self.allowed_origins.is_empty() {
            Ok(agent_client_protocol_http::CorsOptions::disabled())
        } else {
            agent_client_protocol_http::CorsOptions::allow_origins(&self.allowed_origins)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
        }
    }
}

#[derive(Clone)]
struct AcpRouter {
    router: Router,
    cors: agent_client_protocol_http::CorsOptions,
}

impl AcpRouter {
    fn apply_error_cors<B>(&self, request: &Request<B>, response: &mut Response<Body>) {
        if !matches!(request.uri().path(), "/acp" | "/acp/v2") {
            return;
        }
        if let agent_client_protocol_http::CorsOptions::AllowOrigins(origins) = &self.cors
            && let Some(origin) = request.headers().get(header::ORIGIN)
            && origins.contains(origin)
        {
            response
                .headers_mut()
                .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
            response
                .headers_mut()
                .append(header::VARY, HeaderValue::from_static("Origin"));
        }
    }
}

pub async fn start(
    runtime: Arc<Runtime>,
    address: String,
    serve_a2a: bool,
    serve_remote_acp: bool,
    credential_file: Option<&Path>,
) -> Result<HttpServer, Box<dyn std::error::Error>> {
    start_with_registry(
        runtime,
        address,
        serve_a2a,
        serve_remote_acp,
        credential_file,
        crate::protocols::acp::SessionRegistry::new(),
    )
    .await
}

pub async fn start_with_registry(
    runtime: Arc<Runtime>,
    address: String,
    serve_a2a: bool,
    serve_remote_acp: bool,
    credential_file: Option<&Path>,
    sessions: crate::protocols::acp::SessionRegistry,
) -> Result<HttpServer, Box<dyn std::error::Error>> {
    start_with_options(
        runtime,
        address,
        serve_a2a,
        serve_remote_acp,
        credential_file,
        sessions,
        HttpOptions::default(),
    )
    .await
}

pub async fn start_with_options(
    runtime: Arc<Runtime>,
    address: String,
    serve_a2a: bool,
    serve_remote_acp: bool,
    credential_file: Option<&Path>,
    sessions: crate::protocols::acp::SessionRegistry,
    options: HttpOptions,
) -> Result<HttpServer, Box<dyn std::error::Error>> {
    let cors = options.cors()?;
    if !serve_remote_acp && (options.health_endpoint || !options.allowed_origins.is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ACP HTTP options require remote ACP",
        )
        .into());
    }
    if !serve_a2a && !serve_remote_acp {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HTTP server requires A2A, remote ACP, or both",
        )
        .into());
    }
    let credential_file = credential_file.map(Path::to_path_buf);
    let credential = tokio::task::spawn_blocking(move || {
        credential_file
            .as_deref()
            .map(BearerToken::load)
            .transpose()
    })
    .await??;
    // Bind once so an ephemeral port cannot be stolen between selection and serving.
    let listener = tokio::net::TcpListener::bind(&address).await?;
    let bound = listener.local_addr()?;
    let a2a = serve_a2a
        .then(|| crate::protocols::a2a::dispatcher(runtime.clone(), bound, credential.is_some()))
        .transpose()?
        .map(Arc::new);
    let acp = serve_remote_acp.then(|| AcpRouter {
        router: crate::protocols::acp::http_router(
            runtime.clone(),
            sessions.clone(),
            cors.clone(),
            options.health_endpoint,
        )
        .merge(crate::protocols::acp::v2::http_router(
            runtime,
            sessions,
            cors.clone(),
        )),
        cors,
    });
    let stop_accepting = crate::resilient_fs::shutdown_token().child_token();
    let accepts_stopped = CancellationToken::new();
    let shutdown_connections = crate::resilient_fs::shutdown_token().child_token();
    let task = tokio::spawn(serve_bound(
        listener,
        a2a,
        acp,
        credential,
        stop_accepting.clone(),
        accepts_stopped.clone(),
        shutdown_connections.clone(),
    ));
    Ok(HttpServer {
        address: bound,
        stop_accepting,
        accepts_stopped,
        shutdown_connections,
        task: Some(task),
    })
}

async fn serve_bound(
    listener: tokio::net::TcpListener,
    a2a: Option<Arc<a2a_protocol_server::JsonRpcDispatcher>>,
    acp: Option<AcpRouter>,
    credential: Option<BearerToken>,
    stop_accepting: CancellationToken,
    accepts_stopped: CancellationToken,
    shutdown_connections: CancellationToken,
) -> io::Result<()> {
    struct SignalOnDrop(CancellationToken);

    impl Drop for SignalOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }

    let accepts_stopped = SignalOnDrop(accepts_stopped);
    let mut connections = JoinSet::new();
    loop {
        // Left-first polling preserves stop > guarded join > accept. Leave
        // both owning scopes before handling the event to release JoinSet borrows.
        let event = {
            let stop = std::pin::pin!(stop_accepting.cancelled());
            let activity = std::pin::pin!(async {
                let completed = std::pin::pin!(async {
                    if !connections.is_empty()
                        && let Some(completed) = connections.join_next().await
                    {
                        return completed;
                    }
                    std::future::pending().await
                });
                let accepted = std::pin::pin!(listener.accept());
                match select(completed, accepted).await {
                    Either::Left((completed, _pending)) => Either::Left(completed),
                    Either::Right((accepted, _pending)) => Either::Right(accepted),
                }
            });
            match select(stop, activity).await {
                Either::Left(((), _pending)) => None,
                Either::Right((event, _pending)) => Some(event),
            }
        };
        match event {
            None => break,
            Some(Either::Left(completed)) => report_connection(completed),
            Some(Either::Right(accepted)) => match accepted {
                Ok((stream, _)) => {
                    let _ = stream.set_nodelay(true);
                    let a2a = a2a.clone();
                    let acp = acp.clone();
                    let credential = credential.clone();
                    connections.spawn(async move {
                        let service = hyper::service::service_fn(move |request| {
                            dispatch(request, a2a.clone(), acp.clone(), credential.clone())
                        });
                        let io = hyper_util::rt::TokioIo::new(stream);
                        let _ = hyper_util::server::conn::auto::Builder::new(
                            hyper_util::rt::TokioExecutor::new(),
                        )
                        .serve_connection_with_upgrades(io, service)
                        .await;
                    });
                }
                Err(error) => {
                    eprintln!("HTTP accept failed: {error}");
                    let stopped = {
                        let stop = std::pin::pin!(stop_accepting.cancelled());
                        let backoff =
                            std::pin::pin!(tokio::time::sleep(Duration::from_millis(100)));
                        match select(stop, backoff).await {
                            Either::Left(((), _pending)) => true,
                            Either::Right(((), _pending)) => false,
                        }
                    };
                    if stopped {
                        break;
                    }
                }
            },
        }
    }
    drop(listener);
    drop(accepts_stopped);

    loop {
        let completed = {
            let shutdown = std::pin::pin!(shutdown_connections.cancelled());
            let completed = std::pin::pin!(async {
                if !connections.is_empty()
                    && let Some(completed) = connections.join_next().await
                {
                    return completed;
                }
                std::future::pending().await
            });
            match select(shutdown, completed).await {
                Either::Left(((), _pending)) => None,
                Either::Right((completed, _pending)) => Some(completed),
            }
        };
        match completed {
            None => break,
            Some(completed) => report_connection(completed),
        }
    }

    if timeout(Duration::from_secs(1), drain_connections(&mut connections))
        .await
        .is_err()
    {
        connections.abort_all();
        while let Some(result) = connections.join_next().await {
            report_connection(result);
        }
    }
    Ok(())
}

async fn drain_connections(connections: &mut JoinSet<()>) {
    while let Some(result) = connections.join_next().await {
        report_connection(result);
    }
}

fn report_connection(result: Result<(), tokio::task::JoinError>) {
    if let Err(error) = result
        && !error.is_cancelled()
    {
        eprintln!("HTTP connection task failed: {error}");
    }
}

async fn dispatch(
    request: Request<hyper::body::Incoming>,
    a2a: Option<Arc<a2a_protocol_server::JsonRpcDispatcher>>,
    acp: Option<AcpRouter>,
    credential: Option<BearerToken>,
) -> Result<Response<Body>, Infallible> {
    // Browser preflight carries no bearer token and cannot invoke an ACP method.
    // Delegate origin/method policy to the SDK, adding the host's auth header.
    if request.method() == axum::http::Method::OPTIONS
        && matches!(request.uri().path(), "/acp" | "/acp/v2")
        && request.headers().contains_key(header::ORIGIN)
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
        && let Some(router) = acp.clone()
    {
        let mut response = router.router.oneshot(request.map(Body::new)).await?;
        if let Some(headers) = response
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .cloned()
        {
            let mut value = headers.as_bytes().to_vec();
            value.extend_from_slice(b",authorization,last-event-id");
            if let Ok(value) = HeaderValue::from_bytes(&value) {
                response
                    .headers_mut()
                    .insert(header::ACCESS_CONTROL_ALLOW_HEADERS, value);
            }
        }
        return Ok(response);
    }
    if credential
        .as_ref()
        .is_some_and(|credential| !credential.authorizes(request.headers()))
    {
        let mut response = Response::new(Body::from("unauthorized"));
        *response.status_mut() = StatusCode::UNAUTHORIZED;
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        if let Some(router) = &acp {
            router.apply_error_cors(&request, &mut response);
        }
        return Ok(response);
    }

    // The SDK does not retain transport event IDs. Never silently accept an
    // event cursor and give clients a fresh stream that can omit updates.
    if matches!(request.uri().path(), "/acp" | "/acp/v2")
        && request.headers().contains_key("last-event-id")
    {
        let mut response = Response::new(Body::from(
            "Last-Event-ID resumption is unsupported; use ACP session/resume",
        ));
        *response.status_mut() = StatusCode::BAD_REQUEST;
        if let Some(router) = &acp {
            router.apply_error_cors(&request, &mut response);
        }
        return Ok(response);
    }

    if matches!(request.uri().path(), "/acp" | "/acp/v2" | "/health")
        && let Some(router) = acp
    {
        let response = router.router.oneshot(request.map(Body::new)).await?;
        return Ok(response);
    }

    if let Some(dispatcher) = a2a {
        return Ok(dispatcher.dispatch(request).await.map(Body::new));
    }

    let mut response = Response::new(Body::from("not found"));
    *response.status_mut() = StatusCode::NOT_FOUND;
    Ok(response)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use std::time::Duration;

    use super::{BearerToken, start};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[test]
    fn http_origins_require_an_explicit_http_origin() {
        for origin in [
            "*",
            "null",
            "https://example.com/path",
            "https://user@example.com",
            "https://example.com/",
            "file:///tmp",
        ] {
            assert!(
                super::HttpOptions {
                    allowed_origins: vec![origin.into()],
                    ..Default::default()
                }
                .cors()
                .is_err()
            );
        }
        assert_eq!(
            super::HttpOptions::default().cors().unwrap(),
            agent_client_protocol_http::CorsOptions::Disabled
        );
        assert!(
            super::HttpOptions {
                allowed_origins: vec!["https://example.com".into()],
                ..Default::default()
            }
            .cors()
            .is_ok()
        );
    }

    #[tokio::test]
    async fn http_health_and_cors_are_opt_in_and_preserve_authentication() {
        let directory = tempfile::tempdir().unwrap();
        let credential = directory.path().join("token");
        std::fs::write(&credential, "secret-token").unwrap();
        let runtime = crate::Runtime::new(directory.path(), "gpt-5.4").unwrap();
        let client = reqwest::Client::new();
        for enabled in [false, true] {
            let mut server = super::start_with_options(
                runtime.clone(),
                "127.0.0.1:0".into(),
                false,
                true,
                Some(&credential),
                crate::protocols::acp::SessionRegistry::new(),
                super::HttpOptions {
                    health_endpoint: enabled,
                    allowed_origins: if enabled {
                        vec!["https://example.com".into()]
                    } else {
                        vec![]
                    },
                },
            )
            .await
            .unwrap();
            let base = format!("http://{}", server.address());
            assert_eq!(
                client
                    .get(format!("{base}/health"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                reqwest::StatusCode::UNAUTHORIZED
            );
            let health = client
                .get(format!("{base}/health"))
                .bearer_auth("secret-token")
                .send()
                .await
                .unwrap();
            assert_eq!(
                health.status(),
                if enabled {
                    reqwest::StatusCode::OK
                } else {
                    reqwest::StatusCode::NOT_FOUND
                }
            );
            for path in ["/acp", "/acp/v2"] {
                for origin in ["https://example.com", "https://denied.example"] {
                    let response = client
                        .request(reqwest::Method::OPTIONS, format!("{base}{path}"))
                        .header("origin", origin)
                        .header("access-control-request-method", "POST")
                        .header(
                            "access-control-request-headers",
                            "authorization,content-type,last-event-id",
                        )
                        .send()
                        .await
                        .unwrap();
                    let allowed = response.headers().get("access-control-allow-origin");
                    if enabled && origin == "https://example.com" {
                        assert_eq!(allowed.unwrap(), origin);
                        assert!(
                            response.headers()["access-control-allow-headers"]
                                .to_str()
                                .unwrap()
                                .contains("authorization")
                        );
                        assert!(
                            response.headers()["access-control-allow-headers"]
                                .to_str()
                                .unwrap()
                                .contains("last-event-id")
                        );
                    } else {
                        assert!(allowed.is_none());
                    }
                }
                // Preflight never authorizes work; browser clients can read errors.
                for origin in [
                    None,
                    Some("https://example.com"),
                    Some("https://denied.example"),
                ] {
                    for authenticated in [false, true] {
                        let mut request = client
                            .get(format!("{base}{path}"))
                            .header("last-event-id", "unsupported-event");
                        if let Some(origin) = origin {
                            request = request.header("origin", origin);
                        }
                        if authenticated {
                            request = request.bearer_auth("secret-token");
                        }
                        let response = request.send().await.unwrap();
                        assert_eq!(
                            response.status(),
                            if authenticated {
                                reqwest::StatusCode::BAD_REQUEST
                            } else {
                                reqwest::StatusCode::UNAUTHORIZED
                            }
                        );
                        if enabled && origin == Some("https://example.com") {
                            assert_eq!(
                                response.headers()["access-control-allow-origin"],
                                "https://example.com"
                            );
                            assert_eq!(response.headers()["vary"], "Origin");
                        } else {
                            assert!(
                                response
                                    .headers()
                                    .get("access-control-allow-origin")
                                    .is_none()
                            );
                            assert!(response.headers().get("vary").is_none());
                        }
                    }
                }
            }
            let response = client
                .get(format!("{base}/.well-known/agent-card.json"))
                .header("origin", "https://example.com")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
            assert!(
                response
                    .headers()
                    .get("access-control-allow-origin")
                    .is_none()
            );

            server.shutdown();
            server.join().await.unwrap();
        }
    }

    #[test]
    fn credential_file_accepts_one_token_and_one_trailing_newline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        std::fs::write(&path, b"secret-token\r\n").unwrap();
        let token = BearerToken::load(&path).unwrap();
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer secret-token".parse().unwrap(),
        );
        assert!(token.authorizes(&headers));
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer wrong".parse().unwrap(),
        );
        assert!(!token.authorizes(&headers));
    }

    #[test]
    fn credential_file_rejects_empty_or_multiline_tokens() {
        let directory = tempfile::tempdir().unwrap();
        for (name, contents) in [
            ("empty", b"".as_slice()),
            ("lines", b"one\ntwo"),
            ("double-newline", b"token\n\n"),
            ("space", b"not a token"),
        ] {
            let path = directory.path().join(name);
            std::fs::write(&path, contents).unwrap();
            assert!(BearerToken::load(&path).is_err());
        }
    }

    #[tokio::test]
    async fn port_zero_is_retained_and_bearer_protects_all_protocols() {
        let directory = tempfile::tempdir().unwrap();
        let credential = directory.path().join("token");
        std::fs::write(&credential, "secret-token").unwrap();
        let runtime = crate::Runtime::new(directory.path(), "gpt-5.4").unwrap();
        let mut server = start(runtime, "127.0.0.1:0".into(), true, true, Some(&credential))
            .await
            .unwrap();
        let bound = server.address();
        assert_ne!(bound.port(), 0);
        assert!(tokio::net::TcpListener::bind(bound).await.is_err());

        let client = reqwest::Client::new();
        let unauthorized_acp = client
            .post(format!("http://{bound}/acp"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized_acp.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(
            unauthorized_acp.headers()[reqwest::header::WWW_AUTHENTICATE],
            "Bearer"
        );
        assert_eq!(
            client
                .post(format!("http://{bound}/acp/v2"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );

        let card = format!("http://{bound}/.well-known/agent-card.json");
        assert_eq!(
            client.get(&card).send().await.unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        let response = client
            .get(&card)
            .bearer_auth("secret-token")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let card: serde_json::Value = response.json().await.unwrap();
        assert!(card["securitySchemes"]["bearer"].is_object());

        let response = client
            .post(format!("http://{bound}/acp"))
            .bearer_auth("secret-token")
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": 2,
                    "info": { "name": "kit-test", "version": "0" }
                }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let routed: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            routed["result"]["protocolVersion"], 2,
            "dual-protocol endpoint did not route to ACP v2: {routed}"
        );

        let response = client
            .post(format!("http://{bound}/acp"))
            .bearer_auth("secret-token")
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "initialize",
                "params": { "protocolVersion": 1 }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert!(response.headers().contains_key("acp-connection-id"));
        let initialized: serde_json::Value = response.json().await.unwrap();
        assert_eq!(initialized["result"]["protocolVersion"], 1);

        let response = client
            .post(format!("http://{bound}/acp/v2"))
            .bearer_auth("secret-token")
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "initialize",
                "params": {
                    "protocolVersion": 99,
                    "info": { "name": "kit-test", "version": "0" }
                }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let initialized: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            initialized["result"]["protocolVersion"], 2,
            "unexpected ACP v2 initialize response: {initialized}"
        );

        let mut websocket = tokio::net::TcpStream::connect(bound).await.unwrap();
        websocket
            .write_all(
                format!(
                    "GET /acp HTTP/1.1\r\nHost: {bound}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nAuthorization: Bearer secret-token\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut handshake = vec![0; 1024];
        let read = tokio::time::timeout(Duration::from_secs(2), websocket.read(&mut handshake))
            .await
            .unwrap()
            .unwrap();
        let handshake = String::from_utf8_lossy(&handshake[..read]);
        assert!(
            handshake.starts_with("HTTP/1.1 101"),
            "unexpected WebSocket handshake: {handshake}"
        );
        drop(websocket);
        server.shutdown();
        server.join().await.unwrap();
    }

    #[tokio::test]
    async fn a2a_empty_text_returns_failed_task() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = crate::Runtime::new(directory.path(), "gpt-5.4").unwrap();
        let mut server = start(runtime, "127.0.0.1:0".into(), true, false, None)
            .await
            .unwrap();
        // The advertised agent URL is the root, not an invented /rpc route.
        let response = reqwest::Client::new()
            .post(format!("http://{}/", server.address()))
            .header("A2A-Version", "1.0")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": "empty-text",
                "method": "SendMessage",
                "params": {
                    "message": {
                        "messageId": "empty-text-message",
                        "role": "ROLE_USER",
                        "parts": [{ "text": " \n\t" }]
                    },
                    "configuration": {
                        "acceptedOutputModes": ["text/plain"],
                        "returnImmediately": false
                    }
                }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = response.json().await.unwrap();
        server.shutdown();
        server.join().await.unwrap();

        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(body["id"], "empty-text", "unexpected response: {body}");
        assert!(body.get("error").is_none(), "unexpected RPC error: {body}");
        let task = &body["result"]["task"];
        assert!(task["id"].as_str().is_some_and(|id| !id.is_empty()));
        assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
        assert_eq!(task["artifacts"][0]["artifactId"], "error");
        assert_eq!(
            task["artifacts"][0]["parts"][0]["text"],
            "A2A request must contain a text part"
        );
    }

    #[tokio::test]
    async fn a2a_rejects_explicit_unsupported_media_type() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = crate::Runtime::new(directory.path(), "gpt-5.4").unwrap();
        let mut server = start(runtime, "127.0.0.1:0".into(), true, false, None)
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .post(format!("http://{}/", server.address()))
            .header("A2A-Version", "1.0")
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": "unsupported-media",
                "method": "SendMessage",
                "params": {
                    "message": {
                        "messageId": "unsupported-media-message",
                        "role": "ROLE_USER",
                        // Whitespace also keeps this request model-free if validation regresses.
                        "parts": [{ "text": " ", "mediaType": "image/png" }]
                    }
                }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = response.json().await.unwrap();
        server.shutdown();
        server.join().await.unwrap();

        assert_eq!(body["jsonrpc"], "2.0");
        assert_eq!(
            body["id"], "unsupported-media",
            "unexpected response: {body}"
        );
        assert_eq!(body["error"]["code"], -32005, "unexpected response: {body}");
        assert!(body.get("result").is_none());
    }

    #[tokio::test]
    async fn ready_stop_preempts_queued_accept_and_empty_drain_waits_for_shutdown() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let stop = tokio_util::sync::CancellationToken::new();
        let accepts_stopped = tokio_util::sync::CancellationToken::new();
        let shutdown = tokio_util::sync::CancellationToken::new();
        stop.cancel();
        let server = tokio::spawn(super::serve_bound(
            listener,
            None,
            None,
            None,
            stop,
            accepts_stopped.clone(),
            shutdown.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(2), accepts_stopped.cancelled())
            .await
            .unwrap();
        assert!(!server.is_finished(), "empty drain must wait for shutdown");
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let mut response = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut response))
            .await
            .unwrap();
        match read {
            Ok(_) => {}
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset),
        }
        assert!(response.is_empty(), "stop must prevent an HTTP response");
    }

    #[tokio::test]
    async fn connection_task_panic_is_isolated() {
        let mut connections = tokio::task::JoinSet::new();
        connections.spawn(async { panic!("connection test panic") });
        super::report_connection(connections.join_next().await.unwrap());

        connections.spawn(async {});
        connections.join_next().await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn shutdown_releases_listener_for_rebind() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = crate::Runtime::new(directory.path(), "gpt-5.4").unwrap();
        let mut server = start(runtime, "127.0.0.1:0".into(), true, false, None)
            .await
            .unwrap();
        let bound = server.address();

        server.shutdown();
        server.join().await.unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let rebound = loop {
            match tokio::net::TcpListener::bind(bound).await {
                Ok(listener) => break listener,
                Err(error)
                    if error.kind() == std::io::ErrorKind::AddrInUse
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("could not rebind {bound} after shutdown: {error}"),
            }
        };
        drop(rebound);
    }
}
