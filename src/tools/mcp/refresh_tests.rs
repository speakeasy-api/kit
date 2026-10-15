//! Protocol-boundary coverage: the only fake is a TCP MCP peer.
use super::*;
use serde_json::json;
use std::collections::VecDeque;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, broadcast},
    task::JoinSet,
};

// The mutex owns a complete response snapshot and the queue of response gates.
// Only test controls write tools/fail; handlers only pop a gate and clone a
// snapshot. No guard survives socket IO or a gate wait. Cancellation can discard
// a response but cannot leave a partially changed catalog or a claimed gate.
struct PeerState {
    tools: Vec<Value>,
    fail: bool,
    gates: VecDeque<(oneshot::Receiver<()>, oneshot::Sender<()>)>,
    modern: bool,
}

struct Peer {
    url: String,
    state: Arc<Mutex<PeerState>>,
    notifications: broadcast::Sender<String>,
    connected: Arc<Notify>,
    requests: mpsc::UnboundedReceiver<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Peer {
    async fn new(tools: Vec<Value>) -> Self {
        Self::with_protocol(tools, false).await
    }

    async fn with_protocol(tools: Vec<Value>, modern: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(PeerState {
            tools,
            fail: false,
            gates: VecDeque::new(),
            modern,
        }));
        let (notifications, _) = broadcast::channel(32);
        let connected = Arc::new(Notify::new());
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let task = {
            let state = state.clone();
            let notifications = notifications.clone();
            let connected = connected.clone();
            tokio::spawn(async move {
                let mut connections = JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.unwrap();
                            connections.spawn(serve(stream, state.clone(), notifications.subscribe(), connected.clone(), requests_tx.clone()));
                        }
                        _ = connections.join_next(), if !connections.is_empty() => {}
                    }
                }
            })
        };
        Self {
            url,
            state,
            notifications,
            connected,
            requests,
            task,
        }
    }

    async fn set(&self, tools: Vec<Value>, method: &str) {
        self.state.lock().await.tools = tools;
        self.notify(method);
    }

    fn notify(&self, method: &str) {
        self.notifications
            .send(json!({"jsonrpc":"2.0","method":method}).to_string())
            .unwrap();
    }

    async fn block_next(&self) -> (oneshot::Sender<()>, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let (closed_tx, closed_rx) = oneshot::channel();
        self.state.lock().await.gates.push_back((rx, closed_tx));
        (tx, closed_rx)
    }

    async fn requested(&mut self) {
        tokio::time::timeout(Duration::from_secs(15), self.requests.recv())
            .await
            .unwrap()
            .unwrap();
    }
}

async fn response(stream: &mut TcpStream, status: &str, body: Option<Value>) {
    let body = body.map(|v| v.to_string()).unwrap_or_default();
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nMcp-Session-Id: refresh-test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    // Runtime cancellation is allowed to close a response connection.
    let _ = stream.write_all(format!("{header}{body}").as_bytes()).await;
}

async fn serve(
    mut stream: TcpStream,
    state: Arc<Mutex<PeerState>>,
    mut notifications: broadcast::Receiver<String>,
    connected: Arc<Notify>,
    requests: mpsc::UnboundedSender<()>,
) {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(bytes.len() < 65_536, "oversized HTTP headers");
    };
    let header = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    if header.starts_with("GET ") {
        if state.lock().await.modern {
            response(&mut stream, "405 Method Not Allowed", None).await;
            return;
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n").await.unwrap();
        connected.notify_one();
        loop {
            tokio::select! {
                notification = notifications.recv() => {
                    let Ok(notification) = notification else { return; };
                    if stream.write_all(format!("event: message\ndata: {notification}\n\n").as_bytes()).await.is_err() { return; }
                }
                read = stream.read_u8() => { if read.is_err() { return; } }
            }
        }
    }
    if header.starts_with("DELETE ") {
        response(&mut stream, "200 OK", None).await;
        return;
    }
    let length = header
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).await.unwrap();
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    let request: Value = serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
    let Some(id) = request.get("id") else {
        response(&mut stream, "202 Accepted", None).await;
        return;
    };
    let result = match request["method"].as_str().unwrap() {
        "server/discover" => {
            if !state.lock().await.modern {
                response(&mut stream, "200 OK", Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"legacy peer"}}))).await;
                return;
            }
            json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":true}},"ttlMs":0,"cacheScope":"private"})
        }
        "subscriptions/listen" => {
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n").await.unwrap();
            let ack = json!({"jsonrpc":"2.0","method":"notifications/subscriptions/acknowledged","params":{"_meta":{"io.modelcontextprotocol/subscriptionId":id},"notifications":{"toolsListChanged":true}}});
            stream
                .write_all(format!("event: message\ndata: {ack}\n\n").as_bytes())
                .await
                .unwrap();
            connected.notify_one();
            while let Ok(notification) = notifications.recv().await {
                let mut notification: Value = serde_json::from_str(&notification).unwrap();
                notification["params"] =
                    json!({"_meta":{"io.modelcontextprotocol/subscriptionId":id}});
                if stream
                    .write_all(format!("event: message\ndata: {notification}\n\n").as_bytes())
                    .await
                    .is_err()
                {
                    return;
                }
            }
            return;
        }
        "initialize" => {
            json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"refresh-peer","version":"1"}})
        }
        "tools/list" => {
            let (tools, fail, gate) = {
                let mut state = state.lock().await;
                (state.tools.clone(), state.fail, state.gates.pop_front())
            };
            let _ = requests.send(());
            if let Some((gate, closed)) = gate {
                tokio::select! {
                    _ = gate => {},
                    _ = stream.read_u8() => {
                        let _ = closed.send(());
                        return;
                    }
                }
            }
            if fail {
                response(&mut stream, "200 OK", Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"catalog temporarily unavailable"}}))).await;
                return;
            }
            json!({"resultType":"complete","tools":tools})
        }
        "ping" => json!({}),
        other => panic!("unexpected MCP request: {other}"),
    };
    response(
        &mut stream,
        "200 OK",
        Some(json!({"jsonrpc":"2.0","id":id,"result":result})),
    )
    .await;
}

fn tool(name: &str, kind: &str) -> Value {
    json!({"name":name,"description":format!("{name} fixture"),"inputSchema":{"type":"object","properties":{"value":{"type":kind}}}})
}

fn config(path: &Path, peers: &[(&str, &Peer)]) {
    let servers = peers
        .iter()
        .map(|(name, peer)| ((*name).to_owned(), json!({"url":peer.url})))
        .collect::<Map<_, _>>();
    std::fs::write(path, json!({"mcpServers":servers}).to_string()).unwrap();
}

async fn runtime(path: &Path, peers: &[(&str, &Peer)]) -> McpRuntime {
    config(path, peers);
    let runtime = connect(Some(path), &[], false, CredentialStorage::Memory)
        .await
        .unwrap();
    runtime
        .initialize_servers(
            &peers
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
    for (_, peer) in peers {
        tokio::time::timeout(Duration::from_secs(15), peer.connected.notified())
            .await
            .unwrap();
    }
    runtime
}

async fn eventually(mut condition: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if condition().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("catalog did not reach the expected state");
}

fn schema(runtime: &McpRuntime, name: &str) -> Option<Value> {
    runtime
        .catalog()
        .get(&ToolName::new(name))
        .map(|entry| entry.spec().input_schema.clone())
}

#[tokio::test]
async fn modern_and_legacy_notifications_publish_add_remove_and_schema_changes() {
    for modern in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let peer = Peer::with_protocol(vec![tool("alpha", "string")], modern).await;
        let runtime = runtime(&directory.path().join("mcp.json"), &[("remote", &peer)]).await;
        assert!(schema(&runtime, "mcp_remote_alpha").is_some());
        peer.set(
            vec![tool("alpha", "integer"), tool("beta", "string")],
            "notifications/tools/list_changed",
        )
        .await;
        // Observe publication directly: a search must not be needed to drive refresh.
        eventually(async || {
            schema(&runtime, "mcp_remote_beta").is_some()
                && schema(&runtime, "mcp_remote_alpha").unwrap()["properties"]["value"]["type"]
                    == "integer"
        })
        .await;
        peer.set(
            vec![tool("beta", "boolean")],
            "notifications/tools/list_changed",
        )
        .await;
        eventually(async || {
            schema(&runtime, "mcp_remote_alpha").is_none()
                && schema(&runtime, "mcp_remote_beta").unwrap()["properties"]["value"]["type"]
                    == "boolean"
        })
        .await;
        let found = runtime.search("beta").await.unwrap();
        assert_eq!(found["servers"][0]["tools"][0]["name"], "mcp_remote_beta");
        let status = runtime.search("mcp").await.unwrap();
        if modern {
            let subscription = &status["servers"][0]["subscription"];
            assert_eq!(subscription["state"], "active");
            assert_eq!(subscription["requested"]["toolsListChanged"], true);
            assert_eq!(subscription["acknowledged"]["toolsListChanged"], true);
        } else {
            assert!(status["servers"][0].get("subscription").is_none());
        }
    }
}

#[tokio::test]
async fn failed_refresh_retains_last_good_catalog_and_retries_without_new_notification() {
    let directory = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![tool("alpha", "string")]).await;
    let runtime = runtime(&directory.path().join("mcp.json"), &[("remote", &peer)]).await;
    peer.state.lock().await.fail = true;
    peer.set(
        vec![tool("beta", "string")],
        "notifications/tools/list_changed",
    )
    .await;
    eventually(async || {
        let found = runtime.search("mcp").await.unwrap();
        found["servers"][0]["catalog_stale"] == true
            && found["servers"][0]["catalog_error"].is_string()
    })
    .await;
    assert!(schema(&runtime, "mcp_remote_alpha").is_some());
    assert!(schema(&runtime, "mcp_remote_beta").is_none());
    peer.state.lock().await.fail = false;
    eventually(async || {
        schema(&runtime, "mcp_remote_beta").is_some()
            && schema(&runtime, "mcp_remote_alpha").is_none()
    })
    .await;
    eventually(async || {
        let found = runtime.search("mcp").await.unwrap();
        found["servers"][0]["catalog_stale"] != true
            && found["servers"][0]["catalog_error"].is_null()
    })
    .await;
}

#[tokio::test]
async fn notification_during_fetch_is_not_lost_and_slow_peer_does_not_block_other_work() {
    let directory = tempfile::tempdir().unwrap();
    let mut slow = Peer::new(vec![tool("alpha", "string")]).await;
    let fast = Peer::new(vec![tool("other", "string")]).await;
    let runtime = runtime(
        &directory.path().join("mcp.json"),
        &[("slow", &slow), ("fast", &fast)],
    )
    .await;
    slow.requested().await; // initial tools/list
    let (release, _closed) = slow.block_next().await;
    slow.set(
        vec![tool("intermediate", "string")],
        "notifications/tools/list_changed",
    )
    .await;
    slow.requested().await;
    let slow_connection = runtime
        .inner
        .manager
        .lock()
        .await
        .connected_server(&McpServerId::new("slow"))
        .unwrap()
        .connection();
    let before = slow_connection.catalog_revision();
    slow.set(
        vec![tool("final", "boolean")],
        "notifications/tools/list_changed",
    )
    .await;
    // Wait for the public connection invalidation revision, not merely the
    // fake peer's write. This proves the second notification was processed
    // while the first tools/list response is still withheld.
    eventually(async || slow_connection.catalog_revision() > before).await;
    fast.set(
        vec![tool("fresh", "string")],
        "notifications/tools/list_changed",
    )
    .await;
    // The blocked socket remains blocked throughout both assertions. Timeouts
    // are deadlock guards, not performance or exact-work-count assertions.
    eventually(async || schema(&runtime, "mcp_fast_fresh").is_some()).await;
    let found = tokio::time::timeout(Duration::from_secs(15), runtime.search("alpha"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found["servers"][0]["tools"][0]["name"], "mcp_slow_alpha");
    release.send(()).unwrap();
    eventually(async || {
        schema(&runtime, "mcp_slow_final").is_some()
            && schema(&runtime, "mcp_slow_intermediate").is_none()
            && schema(&runtime, "mcp_slow_alpha").is_none()
    })
    .await;
}

#[tokio::test]
async fn config_replacement_fences_an_inflight_old_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp.json");
    let mut old = Peer::new(vec![tool("original", "string")]).await;
    let replacement = Peer::new(vec![tool("replacement", "boolean")]).await;
    let runtime = runtime(&path, &[("remote", &old)]).await;
    old.requested().await;
    let (_release, closed) = old.block_next().await;
    old.set(
        vec![tool("obsolete", "string")],
        "notifications/tools/list_changed",
    )
    .await;
    old.requested().await;
    config(&path, &[("remote", &replacement)]);
    tokio::time::timeout(Duration::from_secs(15), runtime.reload_config())
        .await
        .unwrap()
        .unwrap();
    runtime
        .initialize_servers(&["remote".to_owned()])
        .await
        .unwrap();
    eventually(async || schema(&runtime, "mcp_remote_replacement").is_some()).await;
    tokio::time::timeout(Duration::from_secs(15), closed)
        .await
        .expect("replacement must cancel the stale HTTP request")
        .unwrap();
    // After cancellation settles, verify replacement notifications still publish.
    tokio::time::timeout(Duration::from_secs(15), replacement.connected.notified())
        .await
        .unwrap();
    replacement
        .set(
            vec![tool("replacement", "integer")],
            "notifications/tools/list_changed",
        )
        .await;
    eventually(async || {
        schema(&runtime, "mcp_remote_replacement").unwrap()["properties"]["value"]["type"]
            == "integer"
    })
    .await;
    assert!(schema(&runtime, "mcp_remote_obsolete").is_none());
    assert!(schema(&runtime, "mcp_remote_original").is_none());
}

#[tokio::test]
async fn dropping_runtime_during_refresh_releases_its_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let mut peer = Peer::new(vec![tool("alpha", "string")]).await;
    let runtime = runtime(&directory.path().join("mcp.json"), &[("remote", &peer)]).await;
    peer.requested().await;
    let (_release, closed) = peer.block_next().await;
    peer.set(
        vec![tool("beta", "string")],
        "notifications/tools/list_changed",
    )
    .await;
    peer.requested().await;
    let weak = Arc::downgrade(&runtime.inner);
    drop(runtime);
    eventually(async || weak.upgrade().is_none()).await;
    tokio::time::timeout(Duration::from_secs(15), closed)
        .await
        .expect("dropping the runtime must cancel its in-flight HTTP refresh")
        .unwrap();
}

async fn invoke_discovery(tool: &dyn Tool, input: Value) -> Result<Value, ToolError> {
    use agentkit_core::{SessionId, TurnId};
    use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext};
    let context = OwnedToolContext {
        session_id: SessionId::new("refresh-test"),
        turn_id: TurnId::new("turn"),
        metadata: MetadataMap::new(),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
        execution_scope: None,
        approved_request: None,
    };
    let result = tool
        .invoke(
            ToolRequest::new(
                "discovery",
                tool.spec().name.clone(),
                input,
                "refresh-test",
                "turn",
            ),
            &mut context.borrowed(),
        )
        .await?;
    let ToolOutput::Structured(value) = result.result.output else {
        panic!("expected structured discovery output");
    };
    Ok(value)
}

#[tokio::test]
async fn schema_reference_is_invalidated_by_a_live_catalog_change() {
    let directory = tempfile::tempdir().unwrap();
    let mut original = tool("inspect", "string");
    original["inputSchema"]["description"] = json!("large schema description ".repeat(1800));
    let peer = Peer::new(vec![original.clone()]).await;
    let runtime = runtime(&directory.path().join("mcp.json"), &[("remote", &peer)]).await;
    let search = ToolSearch::new(runtime.clone(), directory.path().into());
    let walker = ToolSchema::new(runtime.clone(), directory.path().into());
    let found = invoke_discovery(&search, json!({"query":"inspect"}))
        .await
        .unwrap();
    let reference = found["servers"][0]["tools"][0]["schema_ref"].clone();
    assert!(reference.is_string());
    invoke_discovery(&walker, json!({"schema_ref":reference}))
        .await
        .unwrap();
    original["inputSchema"]["properties"]["value"]["type"] = json!("integer");
    peer.set(vec![original], "notifications/tools/list_changed")
        .await;
    eventually(async || {
        schema(&runtime, "mcp_remote_inspect").unwrap()["properties"]["value"]["type"] == "integer"
    })
    .await;
    assert!(
        invoke_discovery(&walker, json!({"schema_ref":reference}))
            .await
            .is_err()
    );
    let found = invoke_discovery(&search, json!({"query":"inspect"}))
        .await
        .unwrap();
    let replacement = found["servers"][0]["tools"][0]["schema_ref"].clone();
    assert!(replacement.is_string());
    assert_ne!(replacement, reference);
    invoke_discovery(&walker, json!({"schema_ref":replacement}))
        .await
        .unwrap();
}

#[tokio::test]
async fn authentication_reconnect_fences_the_old_connection_epoch() {
    let directory = tempfile::tempdir().unwrap();
    let mut peer = Peer::new(vec![tool("original", "string")]).await;
    let runtime = runtime(&directory.path().join("mcp.json"), &[("remote", &peer)]).await;
    peer.requested().await;
    let connection = runtime
        .inner
        .manager
        .lock()
        .await
        .connected_server(&McpServerId::new("remote"))
        .unwrap()
        .connection();
    let epoch = connection.connection_epoch();
    let (_release, closed) = peer.block_next().await;
    peer.set(
        vec![tool("obsolete", "string")],
        "notifications/tools/list_changed",
    )
    .await;
    peer.requested().await;
    peer.state.lock().await.tools = vec![tool("authenticated", "boolean")];
    let mut credentials = MetadataMap::new();
    credentials.insert("access_token".into(), json!("replacement-token"));
    tokio::time::timeout(
        Duration::from_secs(15),
        connection.resolve_auth(AuthResolution::provided(
            connect_auth_request("remote"),
            credentials,
        )),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(connection.connection_epoch() > epoch);
    let current = runtime
        .inner
        .manager
        .lock()
        .await
        .connected_server(&McpServerId::new("remote"))
        .unwrap()
        .connection();
    assert!(Arc::ptr_eq(&current, &connection));
    tokio::time::timeout(Duration::from_secs(15), closed)
        .await
        .expect("reconnect must cancel the old-epoch HTTP request")
        .unwrap();
    eventually(async || schema(&runtime, "mcp_remote_authenticated").is_some()).await;
    assert!(schema(&runtime, "mcp_remote_obsolete").is_none());
    assert!(schema(&runtime, "mcp_remote_original").is_none());
}
