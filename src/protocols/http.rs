use std::{convert::Infallible, io, path::Path, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, Response, StatusCode, header},
};
use subtle::ConstantTimeEq as _;
use tower::ServiceExt as _;

use crate::runtime::Runtime;

#[derive(Clone)]
struct BearerToken(Vec<u8>);

impl BearerToken {
    fn load(path: &Path) -> io::Result<Self> {
        let mut token = std::fs::read(path).map_err(|error| {
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

    fn authorizes(&self, headers: &HeaderMap) -> bool {
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

pub async fn start(
    runtime: Arc<Runtime>,
    address: String,
    serve_a2a: bool,
    serve_remote_acp: bool,
    credential_file: Option<&Path>,
) -> Result<std::net::SocketAddr, Box<dyn std::error::Error>> {
    if !serve_a2a && !serve_remote_acp {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HTTP server requires A2A, remote ACP, or both",
        )
        .into());
    }
    let credential = credential_file.map(BearerToken::load).transpose()?;
    // Bind once so an ephemeral port cannot be stolen between selection and serving.
    let listener = tokio::net::TcpListener::bind(&address).await?;
    let bound = listener.local_addr()?;
    let a2a = serve_a2a
        .then(|| crate::protocols::a2a::dispatcher(runtime.clone(), bound, credential.is_some()))
        .transpose()?
        .map(Arc::new);
    let acp = serve_remote_acp.then(|| crate::protocols::acp::http_router(runtime));
    serve_bound(listener, a2a, acp, credential);
    Ok(bound)
}

fn serve_bound(
    listener: tokio::net::TcpListener,
    a2a: Option<Arc<a2a_protocol_server::JsonRpcDispatcher>>,
    acp: Option<Router>,
    credential: Option<BearerToken>,
) {
    tokio::spawn(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(connection) => connection,
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let _ = stream.set_nodelay(true);
            let a2a = a2a.clone();
            let acp = acp.clone();
            let credential = credential.clone();
            tokio::spawn(async move {
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
    });
}

async fn dispatch(
    request: Request<hyper::body::Incoming>,
    a2a: Option<Arc<a2a_protocol_server::JsonRpcDispatcher>>,
    acp: Option<Router>,
    credential: Option<BearerToken>,
) -> Result<Response<Body>, Infallible> {
    if credential
        .as_ref()
        .is_some_and(|credential| !credential.authorizes(request.headers()))
    {
        return Ok(Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, "Bearer")
            .body(Body::from("unauthorized"))
            .expect("fixed unauthorized response"));
    }

    if request.uri().path() == "/acp"
        && let Some(router) = acp
    {
        let response = router
            .oneshot(request.map(Body::new))
            .await
            .expect("Axum router is infallible");
        return Ok(response);
    }

    if let Some(dispatcher) = a2a {
        return Ok(dispatcher.dispatch(request).await.map(Body::new));
    }

    Ok(Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("not found"))
        .expect("fixed not-found response"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{BearerToken, start};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

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
        let bound = start(runtime, "127.0.0.1:0".into(), true, true, Some(&credential))
            .await
            .unwrap();
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
                "params": { "protocolVersion": 1 }
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert!(response.headers().contains_key("acp-connection-id"));

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
    }
}
