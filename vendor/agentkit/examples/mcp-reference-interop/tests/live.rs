use std::sync::Arc;

use agentkit_mcp::{
    McpConnection, McpServerConfig, McpTransportBinding, StreamableHttpTransportConfig,
};
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use mcp_reference_interop::{
    RecordedRequest, ReferenceImplementation, capture_transport_exchange_with_binary,
    probe_reference_implementation_with_binary,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn rust_sdk_stateful_streamable_http_interop() {
    let result = probe_reference_implementation_with_binary(
        ReferenceImplementation::RustSdkStatefulSse,
        env!("CARGO_BIN_EXE_mcp-reference-server"),
    )
    .await
    .unwrap();

    assert!(result.tool_names.iter().any(|name| name == "echo"));
    assert!(
        result
            .resource_ids
            .iter()
            .any(|id| id == "fixture://greeting"),
        "expected greeting resource, saw {:?}",
        result.resource_ids
    );
    assert!(
        result.prompt_ids.iter().any(|id| id == "greeting-prompt"),
        "expected greeting prompt, saw {:?}",
        result.prompt_ids
    );
    assert_eq!(result.tool_output, "rmcp-stateful:streamable-http-ok");
    assert_eq!(result.resource_text, "rust sdk stateful fixture resource");
    assert_eq!(result.prompt_text, "rust sdk stateful prompt for Ada");
}

#[tokio::test]
async fn rust_sdk_stateless_json_streamable_http_interop() {
    let result = probe_reference_implementation_with_binary(
        ReferenceImplementation::RustSdkStatelessJson,
        env!("CARGO_BIN_EXE_mcp-reference-server"),
    )
    .await
    .unwrap();

    assert!(result.tool_names.iter().any(|name| name == "echo"));
    assert!(
        result
            .resource_ids
            .iter()
            .any(|id| id == "fixture://greeting"),
        "expected greeting resource, saw {:?}",
        result.resource_ids
    );
    assert!(
        result.prompt_ids.iter().any(|id| id == "greeting-prompt"),
        "expected greeting prompt, saw {:?}",
        result.prompt_ids
    );
    assert_eq!(result.tool_output, "rmcp-stateless:streamable-http-ok");
    assert_eq!(result.resource_text, "rust sdk stateless fixture resource");
    assert_eq!(result.prompt_text, "rust sdk stateless prompt for Ada");
}

#[tokio::test]
async fn rust_sdk_stateful_transport_propagates_session_protocol_and_delete() {
    let requests = capture_transport_exchange_with_binary(
        ReferenceImplementation::RustSdkStatefulSse,
        env!("CARGO_BIN_EXE_mcp-reference-server"),
    )
    .await
    .unwrap();

    let initialize = find_jsonrpc_request(&requests, "initialize");
    assert!(!initialize.headers.contains_key("mcp-session-id"));

    let post_requests = requests
        .iter()
        .filter(|request| request.method == "POST")
        .collect::<Vec<_>>();
    assert!(
        post_requests.len() >= 8,
        "expected full MCP exchange, saw {post_requests:#?}"
    );

    let negotiated_protocol = post_requests
        .iter()
        .find(|request| request.jsonrpc_method.as_deref() != Some("initialize"))
        .and_then(|request| request.headers.get("mcp-protocol-version"))
        .cloned()
        .expect("stateful exchange should advertise a negotiated protocol version");

    for request in post_requests {
        if request.jsonrpc_method.as_deref() == Some("initialize") {
            continue;
        }

        assert_eq!(
            request
                .headers
                .get("mcp-protocol-version")
                .map(String::as_str),
            Some(negotiated_protocol.as_str()),
            "missing negotiated protocol header on {:?}",
            request
        );
        assert!(
            request.headers.contains_key("mcp-session-id"),
            "missing session header on {:?}",
            request
        );
    }

    let delete = requests
        .iter()
        .find(|request| request.method == "DELETE" && request.path == "/mcp")
        .expect("stateful close should issue DELETE");
    assert!(delete.headers.contains_key("mcp-session-id"));
}

#[tokio::test]
async fn rust_sdk_stateless_transport_skips_session_and_delete() {
    let requests = capture_transport_exchange_with_binary(
        ReferenceImplementation::RustSdkStatelessJson,
        env!("CARGO_BIN_EXE_mcp-reference-server"),
    )
    .await
    .unwrap();

    assert!(
        requests
            .iter()
            .all(|request| !request.headers.contains_key("mcp-session-id")),
        "stateless transport should not send session headers: {requests:#?}"
    );
    assert!(
        requests
            .iter()
            .all(|request| !(request.method == "DELETE" && request.path == "/mcp")),
        "stateless transport should not send DELETE on close: {requests:#?}"
    );

    let negotiated_protocol = requests
        .iter()
        .filter(|request| request.method == "POST")
        .filter(|request| request.jsonrpc_method.as_deref() != Some("initialize"))
        .find_map(|request| request.headers.get("mcp-protocol-version"))
        .cloned()
        .expect("stateless exchange should advertise a negotiated protocol version");

    for request in requests
        .iter()
        .filter(|request| request.method == "POST")
        .filter(|request| request.jsonrpc_method.as_deref() != Some("initialize"))
    {
        assert_eq!(
            request
                .headers
                .get("mcp-protocol-version")
                .map(String::as_str),
            Some(negotiated_protocol.as_str()),
            "missing negotiated protocol header on {:?}",
            request
        );
    }
}

#[tokio::test]
async fn streamable_http_resumes_interrupted_sse_response() {
    let (base_url, requests, shutdown) = spawn_resume_server().await;
    let url = format!("{base_url}/mcp");

    let connection = McpConnection::connect(&McpServerConfig::new(
        "resume",
        McpTransportBinding::StreamableHttp(StreamableHttpTransportConfig::new(&url)),
    ))
    .await
    .unwrap();

    let tools = connection.list_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");

    let second_get = wait_for_second_get(&requests, std::time::Duration::from_secs(5))
        .await
        .expect("rmcp client should reopen the common SSE GET stream after it closes");
    assert_eq!(
        second_get.headers.get("last-event-id").map(String::as_str),
        Some("evt-1"),
        "reconnect must carry Last-Event-Id from the previous stream so the server can resume",
    );
    assert!(second_get.headers.contains_key("mcp-session-id"));

    connection.close().await.unwrap();
    shutdown.cancel();

    let requests = requests.lock().await.clone();
    let tools_list_post = find_jsonrpc_request(&requests, "tools/list");
    assert!(tools_list_post.headers.contains_key("mcp-session-id"));
    assert_eq!(
        tools_list_post
            .headers
            .get("mcp-protocol-version")
            .map(String::as_str),
        Some("2025-11-25")
    );
}

async fn wait_for_second_get(
    requests: &Arc<Mutex<Vec<RecordedRequest>>>,
    timeout: std::time::Duration,
) -> Option<RecordedRequest> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        {
            let guard = requests.lock().await;
            let mut gets = guard
                .iter()
                .filter(|request| request.method == "GET" && request.path == "/mcp");
            if gets.next().is_some()
                && let Some(second) = gets.next()
            {
                return Some(second.clone());
            }
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

fn find_jsonrpc_request<'a>(requests: &'a [RecordedRequest], method: &str) -> &'a RecordedRequest {
    requests
        .iter()
        .find(|request| request.jsonrpc_method.as_deref() == Some(method))
        .unwrap_or_else(|| panic!("missing JSON-RPC method {method} in {requests:#?}"))
}

#[derive(Clone, Default)]
struct ResumeState {
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    get_count: Arc<Mutex<u32>>,
}

async fn spawn_resume_server() -> (String, Arc<Mutex<Vec<RecordedRequest>>>, CancellationToken) {
    let shutdown = CancellationToken::new();
    let state = ResumeState::default();
    let requests = state.requests.clone();

    let router = Router::new()
        .route(
            "/mcp",
            post(resume_post).get(resume_get).delete(resume_delete),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base_url = format!("http://127.0.0.1:{}", address.port());

    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
                .await;
        }
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    (base_url, requests, shutdown)
}

async fn resume_post(
    State(state): State<ResumeState>,
    headers: HeaderMap,
    body: String,
) -> Response {
    record_request(&state, "POST", "/mcp", &headers, &body).await;

    let request = serde_json::from_str::<Value>(&body).unwrap();
    match request.get("method").and_then(Value::as_str) {
        Some("initialize") => response_with_headers(
            StatusCode::OK,
            &[
                ("content-type", "application/json"),
                ("mcp-session-id", "resume-session"),
            ],
            json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "serverInfo": { "name": "resume-server", "version": "1.0.0" }
                }
            })
            .to_string(),
        ),
        Some("notifications/initialized") => {
            response_with_headers(StatusCode::ACCEPTED, &[], String::new())
        }
        Some("tools/list") => response_with_headers(
            StatusCode::OK,
            &[("content-type", "application/json")],
            json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {
                    "tools": [{
                        "name": "echo",
                        "description": "Echo",
                        "inputSchema": { "type": "object" }
                    }]
                }
            })
            .to_string(),
        ),
        other => panic!("unexpected POST method: {other:?}"),
    }
}

async fn resume_get(State(state): State<ResumeState>, headers: HeaderMap) -> Response {
    record_request(&state, "GET", "/mcp", &headers, "").await;
    let count = {
        let mut counter = state.get_count.lock().await;
        *counter += 1;
        *counter
    };
    let body = if count == 1 {
        // First common GET delivers one SSE frame with an event id, then ends.
        // Per SEP-1699, the rmcp client treats the graceful close as resumable
        // and reconnects with `Last-Event-Id: evt-1` — that is what the test
        // asserts.
        concat!(
            "id: evt-1\n",
            "event: message\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",",
            "\"params\":{\"progressToken\":\"resume-token\",\"progress\":1,\"total\":1}}\n\n"
        )
        .to_string()
    } else {
        String::new()
    };
    response_with_headers(
        StatusCode::OK,
        &[("content-type", "text/event-stream")],
        body,
    )
}

async fn resume_delete(State(state): State<ResumeState>, headers: HeaderMap) -> Response {
    record_request(&state, "DELETE", "/mcp", &headers, "").await;
    response_with_headers(StatusCode::NO_CONTENT, &[], String::new())
}

async fn record_request(
    state: &ResumeState,
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: &str,
) {
    let jsonrpc_method = serde_json::from_str::<Value>(body).ok().and_then(|value| {
        value
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    state.requests.lock().await.push(RecordedRequest {
        method: method.to_string(),
        path: path.to_string(),
        headers: normalize_headers(headers),
        body: body.to_string(),
        jsonrpc_method,
    });
}

fn normalize_headers(headers: &HeaderMap) -> std::collections::BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect()
}

fn response_with_headers(status: StatusCode, headers: &[(&str, &str)], body: String) -> Response {
    let mut response = (status, body).into_response();
    for (name, value) in headers {
        response.headers_mut().insert(
            axum::http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
    }
    response
}
