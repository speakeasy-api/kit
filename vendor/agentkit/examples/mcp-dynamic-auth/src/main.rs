//! End-to-end demo of [`agentkit_mcp::McpHttpClient`].
//!
//! Spins up a tiny in-process mock MCP endpoint that only accepts
//! `Authorization: Bearer token-N` where `N` is the next sequential request
//! counter, then drives it through an MCP client whose HTTP layer mints a
//! fresh token on every request from a shared atomic counter — no reconnect,
//! no auth-resolver round trip.

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

#[derive(Clone, Default)]
struct MockState {
    expected: Arc<AtomicU64>,
    observed: Arc<tokio::sync::Mutex<Vec<String>>>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mock = MockState::default();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr: SocketAddr = listener.local_addr()?;
    let server = {
        let router = Router::new()
            .route("/mcp", post(handle_mcp))
            .with_state(mock.clone());
        tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        })
    };

    let counter = Arc::new(AtomicU64::new(0));
    let dynamic_client = Arc::new(SequentialBearerClient {
        inner: reqwest::Client::new(),
        counter: counter.clone(),
    });

    let connection = McpConnection::connect(&McpServerConfig::new(
        "dynamic-auth",
        McpTransportBinding::StreamableHttp(
            StreamableHttpTransportConfig::new(format!("http://{addr}/mcp"))
                .with_http_client(dynamic_client),
        ),
    ))
    .await?;

    println!(
        "[client] connected — counter is at {}",
        counter.load(Ordering::SeqCst)
    );

    let first = connection.discover().await?;
    println!(
        "[first discover] tools={:?} (counter = {})",
        first
            .tools
            .iter()
            .map(|t| t.name.as_ref())
            .collect::<Vec<_>>(),
        counter.load(Ordering::SeqCst)
    );

    let second = connection.discover().await?;
    println!(
        "[second discover] tools={:?} (counter = {})",
        second
            .tools
            .iter()
            .map(|t| t.name.as_ref())
            .collect::<Vec<_>>(),
        counter.load(Ordering::SeqCst)
    );

    connection.close().await?;
    server.abort();

    let observed = mock.observed.lock().await.clone();
    println!("[server] tokens accepted in order: {observed:?}");

    Ok(())
}

/// A minimal [`McpHttpClient`] that prepends `token-N` to every request,
/// where `N` is pulled from a shared atomic and incremented per HTTP op.
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

async fn handle_mcp(State(state): State<MockState>, headers: HeaderMap, body: String) -> Response {
    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);

    let Some(token) = presented else {
        return (StatusCode::UNAUTHORIZED, "missing bearer token").into_response();
    };

    let expected = state.expected.fetch_add(1, Ordering::SeqCst);
    let want = format!("token-{expected}");
    if token != want {
        return (
            StatusCode::UNAUTHORIZED,
            format!("expected {want}, got {token}"),
        )
            .into_response();
    }
    state.observed.lock().await.push(token);

    let message: Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid json").into_response(),
    };
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
