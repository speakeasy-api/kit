//! Conformance tests for the pinned MCP protocol revision.
//!
//! Every rmcp connect path must advertise [`PINNED_PROTOCOL_VERSION`], and
//! both transport call sites (stdio child process, Streamable HTTP) must
//! refuse — with the typed [`McpError::UnsupportedProtocolVersion`], before
//! any discovery request — a server whose `initialize` response negotiates
//! any other revision. Both call sites terminate in the same
//! transport-independent contract, [`enforce_pinned_protocol_version`]; the
//! generated-corpus test drives ≥1000 unequal revisions through it while the
//! per-transport tests prove the wiring at each call site, including the
//! reconnect path.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agentkit_core::MetadataMap;
use agentkit_mcp::{
    AuthOperation, AuthRequest, AuthResolution, McpConnection, McpError, McpHandlerConfig,
    McpProtocolVersion, McpResponderRequestContext, McpRoot, McpRootsProvider, McpServerConfig,
    McpServerId, McpTransportBinding, PINNED_PROTOCOL_VERSION, StdioTransportConfig,
    StreamableHttpTransportConfig, enforce_pinned_protocol_version,
};
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

const PIN: &str = "2025-11-25";
const KNOWN_OLDER_REVISIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const DISCOVERY_METHODS: [&str; 3] = ["tools/list", "resources/list", "prompts/list"];

struct EmptyRoots;

#[async_trait::async_trait]
impl McpRootsProvider for EmptyRoots {
    async fn list_roots(
        &self,
        _context: McpResponderRequestContext,
    ) -> Result<Vec<McpRoot>, McpError> {
        Ok(Vec::new())
    }
}

fn refused_revision(error: McpError) -> McpProtocolVersion {
    match error {
        McpError::UnsupportedProtocolVersion {
            expected,
            negotiated: Some(negotiated),
            ..
        } => {
            assert_eq!(expected, PINNED_PROTOCOL_VERSION);
            negotiated
        }
        other => panic!("expected UnsupportedProtocolVersion, got: {other:?}"),
    }
}

fn assert_missing_revision_refused(error: McpError) {
    assert!(
        matches!(
            &error,
            McpError::UnsupportedProtocolVersion {
                expected,
                negotiated: None,
                ..
            } if *expected == PINNED_PROTOCOL_VERSION
        ),
        "missing negotiation must be a typed refusal, got {error:?}"
    );
}

fn revision_value(revision: &str) -> McpProtocolVersion {
    serde_json::from_value(json!(revision)).expect("protocol revision deserializes from string")
}

#[test]
fn pinned_revision_is_exactly_the_rfc_date() {
    assert_eq!(
        serde_json::to_value(&PINNED_PROTOCOL_VERSION).expect("pin serializes"),
        json!(PIN)
    );
}

#[test]
fn upstream_default_revision_still_matches_pin() {
    // Drift alarm: when an rmcp upgrade moves its default negotiated
    // revision, this fails loudly and forces a deliberate re-pin decision
    // instead of a silent revision change.
    assert_eq!(
        rmcp::model::ProtocolVersion::LATEST,
        PINNED_PROTOCOL_VERSION
    );
}

#[test]
fn contract_accepts_exactly_the_pin() {
    let server = McpServerId::new("contract");
    let accepted = enforce_pinned_protocol_version(&server, Some(&PINNED_PROTOCOL_VERSION))
        .expect("the pinned revision is accepted");
    assert_eq!(accepted, PINNED_PROTOCOL_VERSION);
}

#[test]
fn contract_refuses_missing_negotiation() {
    let server = McpServerId::new("contract");
    match enforce_pinned_protocol_version(&server, None) {
        Err(McpError::UnsupportedProtocolVersion {
            expected,
            negotiated: None,
            ..
        }) => assert_eq!(expected, PINNED_PROTOCOL_VERSION),
        other => panic!("missing negotiation must refuse, got: {other:?}"),
    }
}

#[test]
fn contract_refuses_known_older_revisions() {
    let server = McpServerId::new("contract");
    for revision in KNOWN_OLDER_REVISIONS {
        let version = revision_value(revision);
        let error = enforce_pinned_protocol_version(&server, Some(&version))
            .expect_err("older revision must refuse");
        assert_eq!(
            serde_json::to_value(refused_revision(error)).expect("revision serializes"),
            json!(revision)
        );
    }
}

#[test]
fn contract_refuses_generated_unequal_revisions() {
    let server = McpServerId::new("contract-corpus");
    let mut refused = 0usize;
    for year in 1990..2090 {
        for month in 1..=12u32 {
            let candidate = format!("{year:04}-{month:02}-01");
            assert_ne!(candidate, PIN);
            let version = revision_value(&candidate);
            let error = enforce_pinned_protocol_version(&server, Some(&version))
                .expect_err("unequal generated revision must refuse");
            assert_eq!(
                serde_json::to_value(refused_revision(error)).expect("revision serializes"),
                json!(candidate)
            );
            refused += 1;
        }
    }
    assert!(
        refused >= 1000,
        "generated corpus must cover at least 1000 unequal revisions, covered {refused}"
    );
}

#[derive(Clone)]
struct MockState {
    versions: Arc<Vec<String>>,
    initialize_count: Arc<AtomicUsize>,
    methods: Arc<Mutex<Vec<String>>>,
    deletes: Arc<AtomicUsize>,
}

impl MockState {
    fn new(versions: &[&str]) -> Self {
        Self {
            versions: Arc::new(versions.iter().map(|v| v.to_string()).collect()),
            initialize_count: Arc::new(AtomicUsize::new(0)),
            methods: Arc::new(Mutex::new(Vec::new())),
            deletes: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn discovery_requests(&self) -> Vec<String> {
        self.methods
            .lock()
            .await
            .iter()
            .filter(|method| DISCOVERY_METHODS.contains(&method.as_str()))
            .cloned()
            .collect()
    }

    async fn initialized_requests(&self) -> Vec<String> {
        self.methods
            .lock()
            .await
            .iter()
            .filter(|method| method.as_str() == "notifications/initialized")
            .cloned()
            .collect()
    }
}

async fn spawn_mock(state: MockState) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = Router::new()
        .route("/mcp", post(handle_mcp).delete(handle_delete))
        .with_state(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });
    addr
}

async fn handle_mcp(State(state): State<MockState>, body: String) -> Response {
    let message: Value = serde_json::from_str(&body).expect("valid json from rmcp");
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    state.methods.lock().await.push(method.to_string());

    let result = match method {
        "initialize" => {
            let index = state.initialize_count.fetch_add(1, Ordering::SeqCst);
            let version = state.versions[index.min(state.versions.len() - 1)].clone();
            let capabilities = if index == 0 {
                json!({ "tools": { "listChanged": false } })
            } else {
                json!({ "resources": { "listChanged": true } })
            };
            let mut result = json!({
                "protocolVersion": version,
                "capabilities": capabilities,
                "serverInfo": { "name": "pin-mock", "version": "0.0.0" }
            });
            if result["protocolVersion"] == "missing" {
                result.as_object_mut().unwrap().remove("protocolVersion");
            }
            let payload = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            });
            return (
                StatusCode::OK,
                [
                    (
                        axum::http::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    ),
                    (
                        axum::http::HeaderName::from_static("mcp-session-id"),
                        HeaderValue::from_static("pin-test-session"),
                    ),
                ],
                serde_json::to_string(&payload).unwrap(),
            )
                .into_response();
        }
        "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
        "tools/list" => json!({ "tools": [] }),
        _ => json!({}),
    };

    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        serde_json::to_string(&json!({ "jsonrpc": "2.0", "id": id, "result": result })).unwrap(),
    )
        .into_response()
}

async fn handle_delete(State(state): State<MockState>) -> Response {
    state.deletes.fetch_add(1, Ordering::SeqCst);
    StatusCode::OK.into_response()
}

fn http_config(addr: SocketAddr) -> McpServerConfig {
    McpServerConfig::new(
        "pin-http",
        McpTransportBinding::StreamableHttp(StreamableHttpTransportConfig::new(format!(
            "http://{addr}/mcp"
        ))),
    )
}

async fn assert_session_closed(state: &MockState) {
    for _ in 0..40 {
        if state.deletes.load(Ordering::SeqCst) >= 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("refused connection must close the negotiated HTTP session");
}

#[tokio::test]
async fn http_initial_connect_accepts_exact_revision() {
    let state = MockState::new(&[PIN]);
    let addr = spawn_mock(state.clone()).await;

    let connection = McpConnection::connect(&http_config(addr))
        .await
        .expect("exact pinned revision connects");
    assert_eq!(
        connection.negotiated_protocol_version(),
        Some(PINNED_PROTOCOL_VERSION)
    );
    let snapshot = connection.discover().await.expect("discovery succeeds");
    assert!(snapshot.tools.is_empty());
    connection.close().await.expect("close succeeds");
}

#[tokio::test]
async fn http_initial_connect_refuses_known_older_revisions() {
    for revision in KNOWN_OLDER_REVISIONS {
        let state = MockState::new(&[revision]);
        let addr = spawn_mock(state.clone()).await;

        let error = match McpConnection::connect(&http_config(addr)).await {
            Ok(_) => panic!("older revision {revision} must refuse"),
            Err(error) => error,
        };
        assert_eq!(
            serde_json::to_value(refused_revision(error)).expect("revision serializes"),
            json!(revision)
        );
        assert_eq!(
            state.discovery_requests().await,
            Vec::<String>::new(),
            "no discovery request may reach a refused server"
        );
        assert_eq!(state.initialized_requests().await, Vec::<String>::new());
        assert_session_closed(&state).await;
    }
}

#[tokio::test]
async fn http_initial_connect_refuses_missing_revision() {
    let state = MockState::new(&["missing"]);
    let addr = spawn_mock(state.clone()).await;

    let error = match McpConnection::connect(&http_config(addr)).await {
        Ok(_) => panic!("missing revision must refuse"),
        Err(error) => error,
    };
    assert_missing_revision_refused(error);
    assert_eq!(state.discovery_requests().await, Vec::<String>::new());
    assert_eq!(state.initialized_requests().await, Vec::<String>::new());
}

#[tokio::test]
async fn http_reconnect_refuses_revision_downgrade() {
    let state = MockState::new(&[PIN, "2025-06-18"]);
    let addr = spawn_mock(state.clone()).await;

    let connection = McpConnection::connect(&http_config(addr))
        .await
        .expect("initial connect negotiates the pin");
    let generation = connection.handler_config().session_generation();
    let request = AuthRequest {
        id: "pin-reauth".into(),
        provider: "test".into(),
        operation: AuthOperation::McpConnect {
            server_id: "pin-http".into(),
            metadata: MetadataMap::new(),
        },
        challenge: MetadataMap::new(),
    };
    let error = connection
        .resolve_auth(AuthResolution::provided(request, MetadataMap::new()))
        .await
        .expect_err("reconnect against a downgraded server must refuse");
    assert_eq!(
        serde_json::to_value(refused_revision(error)).expect("revision serializes"),
        json!("2025-06-18")
    );
    assert_eq!(state.initialize_count.load(Ordering::SeqCst), 2);
    assert_eq!(connection.handler_config().session_generation(), generation);
    assert_eq!(
        connection.negotiated_protocol_version(),
        Some(PINNED_PROTOCOL_VERSION)
    );
    connection
        .list_tools()
        .await
        .expect("failed replacement leaves the old session usable");
}

#[tokio::test]
async fn http_reconnect_replaces_server_capabilities() {
    let state = MockState::new(&[PIN, PIN]);
    let addr = spawn_mock(state).await;
    let connection = McpConnection::connect_with_handler(
        &http_config(addr),
        McpHandlerConfig::new().with_roots_provider(Arc::new(EmptyRoots)),
    )
    .await
    .expect("initial connect succeeds");
    let initial = connection.capabilities();
    assert_eq!(initial.tools.unwrap().list_changed, Some(false));
    assert!(initial.resources.is_none());

    let request = AuthRequest {
        id: "capability-reauth".into(),
        provider: "test".into(),
        operation: AuthOperation::McpConnect {
            server_id: "pin-http".into(),
            metadata: MetadataMap::new(),
        },
        challenge: MetadataMap::new(),
    };
    connection
        .resolve_auth(AuthResolution::provided(request, MetadataMap::new()))
        .await
        .expect("reinitialize succeeds");

    let replaced = connection.capabilities();
    assert!(replaced.tools.is_none());
    assert_eq!(replaced.resources.unwrap().list_changed, Some(true));
    assert!(connection.handler_config().roots.is_some());
}

#[tokio::test]
async fn authorized_reconnect_candidate_does_not_replace_live_session_before_commit() {
    let state = MockState::new(&[PIN, PIN]);
    let addr = spawn_mock(state.clone()).await;
    let connection = McpConnection::connect_with_handler(
        &http_config(addr),
        McpHandlerConfig::new().with_roots_provider(Arc::new(EmptyRoots)),
    )
    .await
    .expect("initial connect succeeds");
    let generation = connection.handler_config().session_generation();

    let candidate = connection
        .begin_reinitialize_authorized()
        .await
        .expect("candidate initialize succeeds");
    assert!(candidate.capabilities().resources.is_some());
    assert!(connection.capabilities().tools.is_some());
    assert!(connection.capabilities().resources.is_none());
    assert_eq!(connection.handler_config().session_generation(), generation);

    candidate.abort().await.expect("candidate closes");
    assert_eq!(connection.handler_config().session_generation(), generation);
    connection
        .list_tools()
        .await
        .expect("aborted candidate leaves old session usable");
    connection.close().await.expect("old session closes");
    assert_eq!(state.deletes.load(Ordering::SeqCst), 2);
}

const STDIO_FAKE_SERVER: &str = r#"
import json
import sys

version = sys.argv[1]
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    message = json.loads(line)
    if "id" not in message:
        continue
    if message.get("method") == "initialize":
        result = {
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "pin-fake", "version": "0.0.0"},
        }
        if version != "missing":
            result["protocolVersion"] = version
    elif message.get("method") == "tools/list":
        result = {"tools": []}
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}) + "\n")
    sys.stdout.flush()
"#;

fn stdio_config(version: &str) -> McpServerConfig {
    McpServerConfig::new(
        "pin-stdio",
        McpTransportBinding::Stdio(
            StdioTransportConfig::new("python3")
                .with_arg("-c")
                .with_arg(STDIO_FAKE_SERVER)
                .with_arg(version),
        ),
    )
}

#[tokio::test]
async fn stdio_initial_connect_accepts_exact_revision() {
    let connection = McpConnection::connect(&stdio_config(PIN))
        .await
        .expect("exact pinned revision connects over stdio");
    assert_eq!(
        connection.negotiated_protocol_version(),
        Some(PINNED_PROTOCOL_VERSION)
    );
    connection.close().await.expect("close succeeds");
}

#[tokio::test]
async fn stdio_initial_connect_refuses_known_older_revisions() {
    for revision in KNOWN_OLDER_REVISIONS {
        let error = match McpConnection::connect(&stdio_config(revision)).await {
            Ok(_) => panic!("older revision {revision} must refuse over stdio"),
            Err(error) => error,
        };
        assert_eq!(
            serde_json::to_value(refused_revision(error)).expect("revision serializes"),
            json!(revision)
        );
    }
}

#[tokio::test]
async fn stdio_initial_connect_refuses_missing_revision() {
    let error = match McpConnection::connect(&stdio_config("missing")).await {
        Ok(_) => panic!("missing revision must refuse over stdio"),
        Err(error) => error,
    };
    assert_missing_revision_refused(error);
}
