//! Integration test exercising [`agentkit_mcp::McpHttpClient`].
//!
//! Stands up a minimal HTTP MCP endpoint that rejects any request whose
//! `Authorization` header is not exactly `Bearer token-N`, where `N` is the
//! next sequential request counter on the server. The client side installs
//! a custom [`McpHttpClient`] that mints `token-N` from a shared atomic on
//! every HTTP op, proving dynamic per-request header injection without
//! reconnects or auth-resolver round trips.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use agentkit_mcp::{
    ClientJsonRpcMessage, McpConnection, McpHttpClient, McpServerConfig, McpSseStream,
    McpStreamableHttpError, McpStreamableHttpPostResponse, McpTransportBinding,
    StreamableHttpTransportConfig,
};
use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use rmcp::transport::streamable_http_client::StreamableHttpClient as RmcpStreamableHttpClient;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

#[derive(Clone, Default)]
struct MockState {
    expected: Arc<AtomicU64>,
    accepted_tokens: Arc<Mutex<Vec<String>>>,
    rejections: Arc<AtomicU64>,
}

async fn spawn_mock(state: MockState) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/mcp", post(handle_mcp))
        .with_state(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    addr
}

async fn handle_mcp(State(state): State<MockState>, headers: HeaderMap, body: String) -> Response {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);

    let Some(token) = presented else {
        state.rejections.fetch_add(1, Ordering::SeqCst);
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };

    let expected = state.expected.fetch_add(1, Ordering::SeqCst);
    let want = format!("token-{expected}");
    if token != want {
        state.rejections.fetch_add(1, Ordering::SeqCst);
        return (
            StatusCode::UNAUTHORIZED,
            format!("expected {want}, got {token}"),
        )
            .into_response();
    }
    state.accepted_tokens.lock().await.push(token);

    let message: Value = serde_json::from_str(&body).expect("valid json from rmcp");
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");

    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mock", "version": "0.0.0" }
        }),
        "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
        "tools/list" => json!({
            "tools": [{
                "name": "ping",
                "description": "replies with pong",
                "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
            }]
        }),
        "tools/call" => {
            let tool = message
                .get("params")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            assert_eq!(tool, "ping", "unexpected tool: {tool}");
            json!({
                "content": [{ "type": "text", "text": "pong" }],
                "isError": false
            })
        }
        "resources/list" => json!({ "resources": [] }),
        "prompts/list" => json!({ "prompts": [] }),
        _ => {
            return json_ok(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("unknown method: {method}") }
            }));
        }
    };

    json_ok(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn json_ok(value: Value) -> Response {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        serde_json::to_string(&value).unwrap(),
    )
        .into_response()
}

struct SequentialBearerClient {
    inner: reqwest::Client,
    counter: Arc<AtomicU64>,
}

impl SequentialBearerClient {
    fn next_token(&self) -> String {
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        format!("token-{n}")
    }
}

#[async_trait]
impl McpHttpClient for SequentialBearerClient {
    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<McpStreamableHttpPostResponse, McpStreamableHttpError<reqwest::Error>> {
        let token = self.next_token();
        RmcpStreamableHttpClient::post_message(
            &self.inner,
            uri,
            message,
            session_id,
            Some(token),
            custom_headers,
        )
        .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), McpStreamableHttpError<reqwest::Error>> {
        let token = self.next_token();
        RmcpStreamableHttpClient::delete_session(
            &self.inner,
            uri,
            session_id,
            Some(token),
            custom_headers,
        )
        .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<McpSseStream, McpStreamableHttpError<reqwest::Error>> {
        let token = self.next_token();
        RmcpStreamableHttpClient::get_stream(
            &self.inner,
            uri,
            session_id,
            last_event_id,
            Some(token),
            custom_headers,
        )
        .await
    }
}

#[tokio::test]
async fn dynamic_bearer_rotates_per_http_op() {
    let mock = MockState::default();
    let addr = spawn_mock(mock.clone()).await;

    let counter = Arc::new(AtomicU64::new(0));
    let client = Arc::new(SequentialBearerClient {
        inner: reqwest::Client::new(),
        counter: counter.clone(),
    });

    let connection = McpConnection::connect(&McpServerConfig::new(
        "dynamic-auth",
        McpTransportBinding::StreamableHttp(
            StreamableHttpTransportConfig::new(format!("http://{addr}/mcp"))
                .with_http_client(client),
        ),
    ))
    .await
    .expect("connect succeeds with dynamic bearer");

    // Initial connect produces at least the `initialize` POST + the
    // `notifications/initialized` POST, each with a fresh token.
    let after_connect = counter.load(Ordering::SeqCst);
    assert!(
        after_connect >= 2,
        "expected at least 2 HTTP ops during connect, got {after_connect}"
    );

    let snapshot = connection
        .discover()
        .await
        .expect("discover succeeds with dynamic bearer");
    assert_eq!(snapshot.tools.len(), 1);
    assert_eq!(snapshot.tools[0].name.as_ref(), "ping");

    // Multiple tool calls round-trip without reconnect — each one mints a
    // fresh token-N and is accepted.
    for _ in 0..3 {
        let result = connection
            .call_tool("ping", json!({}))
            .await
            .expect("tool call succeeds");
        let text = result.content[0]
            .raw
            .as_text()
            .map(|t| t.text.clone())
            .expect("text content");
        assert_eq!(text, "pong");
    }

    let accepted = mock.accepted_tokens.lock().await.clone();
    let rejections = mock.rejections.load(Ordering::SeqCst);
    assert_eq!(rejections, 0, "no requests should be rejected");

    // Tokens must be accepted in strict sequential order, starting at 0.
    for (i, token) in accepted.iter().enumerate() {
        assert_eq!(token, &format!("token-{i}"), "out-of-order token at {i}");
    }
    assert!(
        accepted.len() >= 6,
        "expected ≥6 accepted tokens (init, notifications/initialized, tools/list, 3× tools/call), got {}",
        accepted.len()
    );

    // Client counter advanced exactly once per accepted request.
    assert_eq!(
        counter.load(Ordering::SeqCst),
        accepted.len() as u64,
        "client counter should match server-accepted count when no rejections occur"
    );
}

#[tokio::test]
async fn static_bearer_is_rejected_by_sequential_server() {
    // Confirms the test fixture is meaningful: a static bearer fails because
    // the second request arrives with a stale token.
    let mock = MockState::default();
    let addr = spawn_mock(mock.clone()).await;

    let outcome = McpConnection::connect(&McpServerConfig::new(
        "static-bearer",
        McpTransportBinding::StreamableHttp(
            StreamableHttpTransportConfig::new(format!("http://{addr}/mcp"))
                .with_bearer_token("token-0"),
        ),
    ))
    .await;

    // The first HTTP op (initialize) succeeds with token-0; subsequent ops
    // re-use the same token and are rejected. Either connect itself fails or
    // the first follow-up operation fails.
    if let Ok(connection) = outcome {
        let discovery = connection.discover().await;
        assert!(
            discovery.is_err(),
            "expected sequential server to reject reused token-0 on follow-up"
        );
    }
    assert!(
        mock.rejections.load(Ordering::SeqCst) >= 1,
        "expected at least one rejection from the sequential server"
    );
}
