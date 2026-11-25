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
    listen_error: Option<i64>,
    capabilities: Value,
    listen_requests: usize,
    skills: SkillsFixture,
    resource_subscriptions: std::collections::BTreeSet<String>,
}

#[derive(Default)]
struct SkillsFixture {
    name: Option<&'static str>,
    catalog: Option<Arc<[Value]>>,
    second_page: Option<Arc<[Value]>>,
    cursor_requests: Vec<Option<String>>,
    fail: bool,
    gate: Option<oneshot::Receiver<()>>,
    file_requests: usize,
    delay: Duration,
}

pub(crate) struct Peer {
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

    pub(crate) async fn with_protocol(tools: Vec<Value>, modern: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(PeerState {
            tools,
            fail: false,
            gates: VecDeque::new(),
            modern,
            listen_error: None,
            capabilities: json!({"tools":{"listChanged":true},"resources":{"listChanged":true,"subscribe":true}}),
            listen_requests: 0,
            skills: SkillsFixture::default(),
            resource_subscriptions: Default::default(),
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

    pub(crate) async fn advertise_skills(&self, name: &'static str) {
        let mut state = self.state.lock().await;
        state.capabilities["extensions"] = json!({agentkit_mcp::MCP_SKILLS_EXTENSION_ID: {}});
        state.skills.name = Some(name);
    }

    async fn set(&self, tools: Vec<Value>, method: &str) {
        self.state.lock().await.tools = tools;
        self.notify(method);
    }

    pub(crate) fn resource_updated(&self) {
        self.notifications.send(json!({"jsonrpc":"2.0","method":"notifications/resources/updated","params":{"uri":"test://messages"}}).to_string()).unwrap();
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
            json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":state.lock().await.capabilities.clone(),"ttlMs":0,"cacheScope":"private"})
        }
        "subscriptions/listen" => {
            let error = {
                let mut state = state.lock().await;
                state.listen_requests += 1;
                state.listen_error
            };
            if let Some(code) = error {
                response(
                    &mut stream,
                    "200 OK",
                    Some(json!({"jsonrpc":"2.0","id":id,
                    "error":{"code":code,"message":"listen rejected"}})),
                )
                .await;
                connected.notify_one();
                return;
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n").await.unwrap();
            let ack = json!({"jsonrpc":"2.0","method":"notifications/subscriptions/acknowledged","params":{"_meta":{"io.modelcontextprotocol/subscriptionId":id},"notifications":request["params"]["notifications"]}});
            stream
                .write_all(format!("event: message\ndata: {ack}\n\n").as_bytes())
                .await
                .unwrap();
            connected.notify_one();
            while let Ok(notification) = notifications.recv().await {
                let mut notification: Value = serde_json::from_str(&notification).unwrap();
                if notification["params"].is_null() {
                    notification["params"] = json!({});
                }
                if notification["method"] == "notifications/resources/updated"
                    && !request["params"]["notifications"]["resourceSubscriptions"]
                        .as_array()
                        .is_some_and(|uris| uris.contains(&notification["params"]["uri"]))
                {
                    continue;
                }
                notification["params"]["_meta"] =
                    json!({"io.modelcontextprotocol/subscriptionId":id});
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
            json!({"protocolVersion":"2025-11-25","capabilities":state.lock().await.capabilities.clone(),"serverInfo":{"name":"refresh-peer","version":"1"}})
        }
        "resources/list" => json!({"resources":[{"uri":"test://messages","name":"messages"}]}),
        "resources/templates/list" => json!({"resourceTemplates":[]}),
        "skills/list" => {
            let cursor = request["params"]["cursor"].as_str().map(str::to_owned);
            let (name, catalog, second_page, fail, gate, delay) = {
                let mut state = state.lock().await;
                state.skills.cursor_requests.push(cursor.clone());
                (
                    state.skills.name,
                    state.skills.catalog.clone(),
                    state.skills.second_page.clone(),
                    state.skills.fail,
                    state.skills.gate.take(),
                    state.skills.delay,
                )
            };
            if let Some(gate) = gate {
                let _ = gate.await;
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if fail || name.is_none() {
                response(
                    &mut stream,
                    "200 OK",
                    Some(json!({"jsonrpc":"2.0","id":id,
                    "error":{"code":-32603,"message":"no skills inventory"}})),
                )
                .await;
                return;
            }
            if let Some(catalog) = catalog {
                match (cursor.as_deref(), second_page) {
                    (None, Some(_)) => {
                        json!({"resultType":"complete","skills":catalog.as_ref(),"nextCursor":"second"})
                    }
                    (None, None) => json!({"resultType":"complete","skills":catalog.as_ref()}),
                    (Some("second"), Some(page)) if !page.is_empty() => {
                        json!({"resultType":"complete","skills":page.as_ref()})
                    }
                    _ => {
                        response(&mut stream, "200 OK", Some(json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"continuation failed"}}))).await;
                        return;
                    }
                }
            } else {
                let name = name.unwrap();
                match cursor.as_deref() {
                    None => {
                        json!({"resultType":"complete","skills":[remote_skill(name)],"nextCursor":"second"})
                    }
                    Some("second") => {
                        if let Some(catalog) = second_page {
                            json!({"resultType":"complete","skills":catalog.as_ref()})
                        } else {
                            json!({"resultType":"complete","skills":[remote_skill(&format!("{name}-second"))]})
                        }
                    }
                    Some(other) => panic!("unexpected skill cursor: {other}"),
                }
            }
        }
        "resources/read" => {
            state.lock().await.skills.file_requests += 1;
            json!({"contents":[{"uri":request["params"]["uri"],"mimeType":"text/plain","text":"Hello from the subscribed resource"}]})
        }
        "resources/subscribe" | "resources/unsubscribe" => {
            let uri = request["params"]["uri"].as_str().unwrap().to_owned();
            let mut state = state.lock().await;
            if request["method"] == "resources/subscribe" {
                state.resource_subscriptions.insert(uri);
            } else {
                state.resource_subscriptions.remove(&uri);
            }
            json!({})
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
        "tools/call" => json!({"content":[{"type":"text","text":"ordinary RPC works"}],
            "structuredContent":{"called":request["params"]["name"]},"isError":false}),
        "prompts/list" => json!({"resultType":"complete","prompts":[]}),
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

fn validate_search(runtime: &McpRuntime, response: &Value) {
    let tool = ToolSearch::new(runtime.clone(), std::env::temp_dir());
    jsonschema::validator_for(tool.spec.output_schema.as_ref().unwrap())
        .unwrap()
        .validate(response)
        .unwrap();
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

pub(crate) async fn runtime(path: &Path, peers: &[(&str, &Peer)]) -> McpRuntime {
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
        validate_search(&runtime, &found);
        validate_search(&runtime, &status);
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
        validate_search(&runtime, &found);
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

#[tokio::test]
async fn unadvertised_list_changes_do_not_open_subscription_streams() {
    for capabilities in [
        json!({}),
        json!({"tools":{},"resources":{},"prompts":{}}),
        json!({"tools":{"listChanged":false},"resources":{"listChanged":false},"prompts":{"listChanged":false}}),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let peer = Peer::with_protocol(vec![tool("inspect", "string")], true).await;
        {
            let mut state = peer.state.lock().await;
            state.capabilities = capabilities.clone();
            state.listen_error = Some(-32601);
        }
        let path = directory.path().join("mcp.json");
        config(&path, &[("remote", &peer)]);
        let runtime = connect(Some(path.as_path()), &[], false, CredentialStorage::Memory)
            .await
            .unwrap();
        runtime
            .initialize_servers(&["remote".to_string()])
            .await
            .unwrap();
        let search = ToolSearch::new(runtime.clone(), directory.path().into());
        let found = invoke_discovery(&search, json!({"query":"mcp"}))
            .await
            .unwrap();
        assert_eq!(found["servers"][0]["status"], "authenticated");
        // Initialization and discovery await subscription establishment if attempted.
        // This records requests at the real MCP peer, not internal implementation work.
        assert_eq!(peer.state.lock().await.listen_requests, 0, "{capabilities}");
        assert!(found["servers"][0].get("subscription").is_none());
    }
}

#[tokio::test]
async fn unsupported_listen_preserves_kit_discovery_and_tool_calls() {
    use agentkit_core::{SessionId, TurnId};
    use agentkit_tools_core::{AllowAllPermissions, ToolExecutionOutcome, ToolExecutionScope};
    let directory = tempfile::tempdir().unwrap();
    let peer = Peer::with_protocol(vec![tool("inspect", "string")], true).await;
    peer.state.lock().await.listen_error = Some(-32601);
    let runtime = runtime(&directory.path().join("mcp.json"), &[("remote", &peer)]).await;
    let search = ToolSearch::new(runtime.clone(), directory.path().into());
    let found = invoke_discovery(&search, json!({"query":"inspect"}))
        .await
        .unwrap();
    assert_eq!(
        found["servers"][0]["tools"][0]["name"],
        "mcp_remote_inspect"
    );
    let tool = McpTool::new(runtime.clone());
    let scope = ToolExecutionScope {
        executor: Arc::clone(&tool.executor),
        session_id: SessionId::new("refresh-test"),
        turn_id: TurnId::new("turn"),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: None,
    };
    let context = scope.nested_context(MetadataMap::new());
    let outcome = tool
        .invoke_outcome(
            ToolRequest::new(
                "call",
                "tool",
                json!({"name":"mcp_remote_inspect","args":{}}),
                "refresh-test",
                "turn",
            ),
            &mut context.borrowed(),
        )
        .await;
    let ToolExecutionOutcome::Completed(result) = outcome else {
        panic!("RPC failed: {outcome:?}")
    };
    assert!(!result.result.is_error);
    let ToolOutput::Structured(output) = result.result.output else {
        panic!("expected the server's structured result");
    };
    assert_eq!(output, json!({"called":"inspect"}));
    let _events = runtime.subscribe("refresh-test".into()).unwrap();
    let subscriptions = subscriptions::McpSubscriptionTool::new(runtime);
    assert!(
        invoke_discovery(
            &subscriptions,
            json!({"operation":"subscribe","server":"remote",
        "notifications":{"resourceSubscriptions":["test://messages"]}})
        )
        .await
        .is_err()
    );
}

/// Opt-in live smoke test. Explicit paths keep normal CI isolated from user config.
#[tokio::test]
#[ignore = "requires KIT_LIVE_MCP_CONFIG and optionally KIT_LIVE_CREDENTIAL_DIR"]
async fn live_pinned_sdk_discovery() {
    let path = PathBuf::from(std::env::var("KIT_LIVE_MCP_CONFIG").unwrap());
    let storage = std::env::var_os("KIT_LIVE_CREDENTIAL_DIR")
        .map(|path| CredentialStorage::Filesystem(path.into()))
        .unwrap_or(CredentialStorage::Memory);
    let runtime = connect(Some(path.as_path()), &[], false, storage)
        .await
        .unwrap();
    let artifacts = tempfile::tempdir().unwrap();
    let search = ToolSearch::new(runtime, artifacts.path().into());
    let found = tokio::time::timeout(
        Duration::from_secs(120),
        invoke_discovery(&search, json!({"query":"mcp"})),
    )
    .await
    .unwrap()
    .unwrap();
    let servers = found["servers"].as_array().unwrap();
    assert!(!servers.is_empty());
    for server in servers {
        // Do not print raw diagnostics: remote errors can contain credential URLs.
        println!(
            "{}: status={}, tools={}, unsupported_method={}",
            server["name"],
            server["status"],
            server["available_tool_count"],
            server["error"]
                .as_str()
                .is_some_and(|error| error.to_lowercase().contains("method not found"))
        );
        assert!(
            !server["error"]
                .as_str()
                .is_some_and(|error| error.contains("subscriptions/listen"))
        );
    }
    assert!(
        servers
            .iter()
            .any(|server| server["status"] == "authenticated")
    );
}

#[tokio::test]
async fn retired_session_unsubscribes_replayed_legacy_resources() {
    let directory = tempfile::tempdir().unwrap();
    let peer = Peer::new(vec![]).await;
    let runtime = runtime(&directory.path().join("mcp.json"), &[("remote", &peer)]).await;
    let events = runtime.subscribe("refresh-test".into()).unwrap();
    let tool = subscriptions::McpSubscriptionTool::new(runtime.clone());
    invoke_discovery(
        &tool,
        json!({"operation":"subscribe","server":"remote",
        "notifications":{"resourceSubscriptions":["test://messages"]}}),
    )
    .await
    .unwrap();
    assert!(
        peer.state
            .lock()
            .await
            .resource_subscriptions
            .contains("test://messages")
    );
    let connection = runtime
        .inner
        .manager
        .lock()
        .await
        .connected_server(&McpServerId::new("remote"))
        .unwrap()
        .connection();
    let epoch = connection.connection_epoch();
    // The existing operation gate holds reconciliation while the SDK replaces
    // its transport and replays URIs. Retire the session before releasing it.
    let gate = runtime.operation_gate("remote").await;
    let exclusive = gate.write().await;
    connection
        .resolve_auth(AuthResolution::Cancelled {
            request: connect_auth_request("remote"),
        })
        .await
        .unwrap();
    assert!(connection.connection_epoch() > epoch);
    assert!(connection.subscription_status().await.is_none());
    drop(events);
    drop(exclusive);
    eventually(async || peer.state.lock().await.resource_subscriptions.is_empty()).await;
    // Cleanup must also remove SDK desired state, not just the server's watch.
    connection
        .resolve_auth(AuthResolution::Cancelled {
            request: connect_auth_request("remote"),
        })
        .await
        .unwrap();
    assert!(peer.state.lock().await.resource_subscriptions.is_empty());
}

fn remote_skill(name: &str) -> Value {
    json!({"uri": format!("skill://fixture/{name}/SKILL.md"),
        "frontmatter": {"name": name, "description": "Untrusted server metadata"},
        "resources": "dynamic"})
}

fn skill_names(runtime: &McpRuntime) -> Vec<(String, String)> {
    runtime
        .skill_inventory()
        .into_iter()
        .map(|remote| (remote.server, remote.skill.name.clone()))
        .collect()
}

#[tokio::test]
async fn mcp_skill_inventory_lists_metadata_across_pages_and_connection_replacement() {
    let one = Peer::with_protocol(vec![tool("read", "string")], true).await;
    let two = Peer::with_protocol(vec![tool("read", "string")], true).await;
    let unsupported = Peer::new(vec![tool("read", "string")]).await;
    one.advertise_skills("one").await;
    two.advertise_skills("two").await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    let runtime = runtime(&path, &[("peer", &one), ("unsupported", &unsupported)]).await;
    eventually(async || skill_names(&runtime).len() == 2).await;
    assert_eq!(
        skill_names(&runtime),
        [
            ("peer".into(), "one".into()),
            ("peer".into(), "one-second".into())
        ]
    );
    let expected = vec![None, Some("second".into())];
    assert_eq!(one.state.lock().await.skills.cursor_requests, expected);
    assert_eq!(one.state.lock().await.skills.file_requests, 0);
    assert!(
        unsupported
            .state
            .lock()
            .await
            .skills
            .cursor_requests
            .is_empty()
    );

    config(&path, &[("peer", &two)]);
    runtime.reload_config().await.unwrap();
    assert!(skill_names(&runtime).is_empty());
    runtime.initialize_uninitialized().await.unwrap();
    eventually(async || skill_names(&runtime).len() == 2).await;
    assert_eq!(
        skill_names(&runtime),
        [
            ("peer".into(), "two".into()),
            ("peer".into(), "two-second".into())
        ]
    );
    config(&path, &[]);
    runtime.reload_config().await.unwrap();
    assert!(skill_names(&runtime).is_empty());
    assert_eq!(two.state.lock().await.skills.cursor_requests, expected);
    assert_eq!(two.state.lock().await.skills.file_requests, 0);
}

#[tokio::test]
async fn mcp_skill_inventory_does_not_publish_retired_or_failed_work() {
    let old = Peer::with_protocol(vec![tool("read", "string")], true).await;
    let new = Peer::with_protocol(vec![tool("read", "string")], true).await;
    old.advertise_skills("retired").await;
    new.advertise_skills("unavailable").await;
    new.state.lock().await.skills.fail = true;
    let (release, gate) = oneshot::channel();
    old.state.lock().await.skills.gate = Some(gate);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    let runtime = runtime(&path, &[("peer", &old)]).await;
    eventually(async || old.state.lock().await.skills.cursor_requests.len() == 1).await;
    config(&path, &[("peer", &new)]);
    runtime.reload_config().await.unwrap();
    assert!(skill_names(&runtime).is_empty());
    runtime.initialize_uninitialized().await.unwrap();
    let _ = release.send(());
    eventually(async || new.state.lock().await.skills.cursor_requests.len() == 1).await;
    assert!(skill_names(&runtime).is_empty());
    let (records, tools) = runtime.discovery_catalog().await.unwrap();
    assert!(matches!(records["peer"].status, ServerStatus::Connected));
    assert_eq!(tools["peer"].len(), 1);
}

#[tokio::test]
async fn mcp_skill_inventory_filters_out_old_oauth_transport_epochs() {
    let peer = Peer::with_protocol(vec![tool("read", "string")], true).await;
    peer.advertise_skills("before-auth").await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    let runtime = runtime(&path, &[("peer", &peer)]).await;
    eventually(async || skill_names(&runtime).len() == 2).await;

    let (release, gate) = oneshot::channel();
    {
        let mut state = peer.state.lock().await;
        state.skills.name = Some("after-auth");
        state.skills.gate = Some(gate);
    }
    // The normal auth application path reconnects the underlying transport
    // even though the manager continues to point at the same connection Arc.
    runtime
        .apply_credentials("peer", connect_auth_request("peer"), "oauth-token".into())
        .await
        .unwrap()
        .complete();
    assert!(skill_names(&runtime).is_empty());
    release.send(()).unwrap();
    eventually(async || skill_names(&runtime).len() == 2).await;
    assert_eq!(skill_names(&runtime)[0].1, "after-auth");
    assert_eq!(peer.state.lock().await.skills.file_requests, 0);
}

#[tokio::test]
async fn mcp_skill_inventory_marks_a_pending_first_scan_incomplete() {
    let ready = Peer::with_protocol(Vec::new(), true).await;
    ready.advertise_skills("ready").await;
    let slow = Peer::with_protocol(Vec::new(), true).await;
    slow.advertise_skills("slow").await;
    let (release, gate) = oneshot::channel();
    slow.state.lock().await.skills.gate = Some(gate);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    let runtime = runtime(&path, &[("ready", &ready)]).await;
    eventually(async || skill_names(&runtime).len() == 2).await;
    config(&path, &[("ready", &ready), ("slow", &slow)]);
    runtime.reload_config().await.unwrap();
    runtime.initialize_uninitialized().await.unwrap();
    eventually(async || slow.state.lock().await.skills.cursor_requests.len() == 1).await;
    let snapshot = runtime.skill_inventory_snapshot();
    assert_eq!(snapshot.skills.len(), 2);
    assert!(snapshot.truncated);
    release.send(()).unwrap();
    eventually(async || {
        let snapshot = runtime.skill_inventory_snapshot();
        snapshot.skills.len() == 4 && !snapshot.truncated
    })
    .await;
}

#[tokio::test]
async fn mcp_skill_inventory_retries_without_reconnecting() {
    let peer = Peer::with_protocol(vec![tool("read", "string")], true).await;
    peer.advertise_skills("recovered").await;
    peer.state.lock().await.skills.fail = true;
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&dir.path().join("mcp.json"), &[("peer", &peer)]).await;
    let connection = runtime
        .inner
        .manager
        .lock()
        .await
        .connected_server(&McpServerId::new("peer"))
        .unwrap()
        .connection();
    let epoch = connection.connection_epoch();
    // The fixture records the request and captures its failure flag in the
    // same critical section. Do not mistake the Pending marker for a retry.
    eventually(async || !peer.state.lock().await.skills.cursor_requests.is_empty()).await;
    let (release, gate) = oneshot::channel();
    let previous_attempts = {
        let mut state = peer.state.lock().await;
        state.skills.fail = false;
        state.skills.gate = Some(gate);
        state.skills.cursor_requests.len()
    };
    eventually(async || peer.state.lock().await.skills.cursor_requests.len() > previous_attempts)
        .await;
    let snapshot = runtime.skill_inventory_snapshot();
    assert!(snapshot.skills.is_empty());
    assert!(snapshot.truncated);
    release.send(()).unwrap();
    eventually(async || {
        let snapshot = runtime.skill_inventory_snapshot();
        snapshot.skills.len() == 2 && !snapshot.truncated
    })
    .await;
    assert_eq!(skill_names(&runtime)[0].1, "recovered");
    assert_eq!(connection.connection_epoch(), epoch);
}

#[tokio::test]
async fn mcp_skill_inventory_bounds_entries_and_retained_bytes() {
    use super::skills::{INVENTORY_MAX_BYTES, INVENTORY_MAX_ENTRIES};

    let many = Peer::with_protocol(vec![], true).await;
    many.advertise_skills("many").await;
    many.state.lock().await.skills.catalog = Some(
        (0..INVENTORY_MAX_ENTRIES * 2)
            .map(|i| remote_skill(&format!("skill-{i}")))
            .collect::<Vec<_>>()
            .into(),
    );
    let large = Peer::with_protocol(vec![], true).await;
    large.advertise_skills("large").await;
    let mut escaped = remote_skill("escaped");
    escaped["uri"] = json!(format!("skill://fixture/{}", "\u{0000}".repeat(1200)));
    let mut large_catalog = vec![escaped];
    for i in 0..INVENTORY_MAX_ENTRIES / 2 {
        let mut skill = remote_skill(&format!("large-{i}-{}", "x".repeat(1024)));
        skill["frontmatter"]["description"] = json!("🙂".repeat(500));
        skill["frontmatter"]["metadata"] = json!({"ignored": "x".repeat(2048)});
        skill["resources"] =
            json!([{"uri":"skill://fixture/guide.md", "digest":"sha256:fixture", "size":2048}]);
        large_catalog.push(skill);
    }
    large.state.lock().await.skills.catalog = Some(large_catalog.into());

    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(
        &dir.path().join("mcp.json"),
        &[("many", &many), ("large", &large)],
    )
    .await;
    eventually(async || {
        let inventory = runtime.skill_inventory();
        inventory.iter().any(|entry| entry.server == "many")
            && inventory.iter().any(|entry| entry.server == "large")
    })
    .await;
    let snapshot = runtime.skill_inventory_snapshot();
    assert!(snapshot.truncated);
    let inventory = snapshot.skills;
    assert_eq!(
        inventory
            .iter()
            .filter(|entry| entry.server == "many")
            .count(),
        INVENTORY_MAX_ENTRIES
    );
    let large_inventory: Vec<_> = inventory
        .iter()
        .filter(|entry| entry.server == "large")
        .collect();
    assert!(large_inventory.len() < INVENTORY_MAX_ENTRIES);
    assert!(
        large_inventory
            .iter()
            .all(|entry| entry.skill.name != "escaped")
    );
    let retained_bytes: usize = large_inventory
        .iter()
        .map(|entry| {
            entry.server.len()
                + entry.skill.uri.len()
                + entry.skill.name.len()
                + entry.skill.description.len()
        })
        .sum();
    assert!((INVENTORY_MAX_BYTES / 2..=INVENTORY_MAX_BYTES).contains(&retained_bytes));
    assert!(
        large_inventory
            .iter()
            .all(|entry| entry.skill.description.len() < 2000)
    );
    let snapshot = runtime.skill_inventory();
    assert!(Arc::ptr_eq(&inventory[0].skill, &snapshot[0].skill));
    for entry in &inventory {
        assert!(
            json!({"selector": entry.selector(), "name": entry.skill.name})
                .to_string()
                .len()
                < 8 * 1024
        );
    }
}

#[tokio::test]
async fn mcp_skill_inventory_does_not_request_a_continuation_after_the_entry_cap() {
    use super::skills::INVENTORY_MAX_ENTRIES;

    let peer = Peer::with_protocol(Vec::new(), true).await;
    peer.advertise_skills("full").await;
    {
        let mut state = peer.state.lock().await;
        state.skills.catalog = Some(
            (0..INVENTORY_MAX_ENTRIES)
                .map(|i| remote_skill(&format!("entry-{i}")))
                .collect::<Vec<_>>()
                .into(),
        );
        state.skills.second_page = Some(Vec::<Value>::new().into());
    }
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&dir.path().join("mcp.json"), &[("peer", &peer)]).await;
    eventually(async || {
        let snapshot = runtime.skill_inventory_snapshot();
        snapshot.truncated && snapshot.skills.len() == INVENTORY_MAX_ENTRIES
    })
    .await;
    assert_eq!(runtime.skill_inventory().len(), INVENTORY_MAX_ENTRIES);
    assert_eq!(peer.state.lock().await.skills.cursor_requests, [None]);
}

#[tokio::test]
async fn mcp_skill_inventory_can_fill_rejected_slots_from_a_continuation() {
    use super::skills::INVENTORY_MAX_ENTRIES;
    let peer = Peer::with_protocol(Vec::new(), true).await;
    peer.advertise_skills("partial").await;
    let mut first = (0..INVENTORY_MAX_ENTRIES - 1)
        .map(|i| remote_skill(&format!("entry-{i}")))
        .collect::<Vec<_>>();
    first.push(remote_skill(&"x".repeat(9000)));
    {
        let mut state = peer.state.lock().await;
        state.skills.catalog = Some(first.into());
        state.skills.second_page = Some(vec![remote_skill("last")].into());
    }
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&dir.path().join("mcp.json"), &[("peer", &peer)]).await;
    eventually(async || {
        runtime
            .skill_inventory()
            .iter()
            .any(|entry| entry.skill.name == "last")
    })
    .await;
    let snapshot = runtime.skill_inventory_snapshot();
    assert_eq!(snapshot.skills.len(), INVENTORY_MAX_ENTRIES);
    assert!(snapshot.truncated);
}

#[tokio::test]
async fn mcp_skill_inventory_publishes_partial_deadline_result_and_retries_expansion() {
    use super::skills::INVENTORY_MAX_ENTRIES;
    let peer = Peer::with_protocol(Vec::new(), true).await;
    peer.advertise_skills("partial").await;
    let mut first = (0..INVENTORY_MAX_ENTRIES - 1)
        .map(|i| remote_skill(&format!("entry-{i}")))
        .collect::<Vec<_>>();
    first.push(remote_skill(&"x".repeat(9000)));
    {
        let mut state = peer.state.lock().await;
        state.skills.catalog = Some(first.into());
        state.skills.second_page = Some(vec![remote_skill("last")].into());
        state.skills.delay = Duration::from_millis(1450);
    }
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&dir.path().join("mcp.json"), &[("peer", &peer)]).await;
    eventually(async || {
        let snapshot = runtime.skill_inventory_snapshot();
        snapshot.truncated && snapshot.skills.len() == INVENTORY_MAX_ENTRIES - 1
    })
    .await;
    eventually(async || {
        runtime
            .skill_inventory()
            .iter()
            .any(|entry| entry.skill.name == "last")
    })
    .await;
    assert_eq!(runtime.skill_inventory().len(), INVENTORY_MAX_ENTRIES);
}

#[tokio::test]
async fn mcp_skill_inventory_keeps_validated_rows_after_continuation_error() {
    let peer = Peer::with_protocol(Vec::new(), true).await;
    peer.advertise_skills("prefix").await;
    {
        let mut state = peer.state.lock().await;
        state.skills.catalog = Some(vec![remote_skill("prefix")].into());
        state.skills.second_page = Some(Vec::<Value>::new().into());
    }
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&dir.path().join("mcp.json"), &[("peer", &peer)]).await;
    eventually(async || {
        let snapshot = runtime.skill_inventory_snapshot();
        snapshot.truncated && snapshot.skills.iter().any(|row| row.skill.name == "prefix")
    })
    .await;
}

#[tokio::test]
async fn mcp_skill_inventory_keeps_a_byte_saturated_prefix_after_continuation_error() {
    use super::skills::{INVENTORY_MAX_BYTES, SkillMetadata};
    let peer = Peer::with_protocol(Vec::new(), true).await;
    peer.advertise_skills("full-bytes").await;
    let mut first = Vec::new();
    for i in 0..64 {
        let mut row = remote_skill(&format!("entry-{i}"));
        let charge = "peer".len()
            + row["uri"].as_str().unwrap().len()
            + row["frontmatter"]["description"].as_str().unwrap().len()
            + size_of::<SkillMetadata>()
            + size_of::<Arc<SkillMetadata>>()
            + 2 * size_of::<usize>();
        row["frontmatter"]["name"] = json!("n".repeat(INVENTORY_MAX_BYTES / 64 - charge));
        first.push(row);
    }
    {
        let mut state = peer.state.lock().await;
        state.skills.catalog = Some(first.into());
        state.skills.second_page = Some(Vec::<Value>::new().into());
    }
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&dir.path().join("mcp.json"), &[("peer", &peer)]).await;
    eventually(async || {
        let snapshot = runtime.skill_inventory_snapshot();
        snapshot.truncated && snapshot.skills.len() == 64
    })
    .await;
    assert_eq!(runtime.skill_inventory().len(), 64);
    assert_eq!(
        peer.state.lock().await.skills.cursor_requests,
        [None, Some("second".into())]
    );
}

#[tokio::test]
async fn mcp_skill_inventory_retries_after_a_listing_timeout() {
    let peer = Peer::with_protocol(vec![tool("read", "string")], true).await;
    peer.advertise_skills("slow").await;
    let (release, gate) = oneshot::channel();
    peer.state.lock().await.skills.gate = Some(gate);
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&dir.path().join("mcp.json"), &[("peer", &peer)]).await;
    // Keep the initial request blocked until another attempt completes.
    eventually(async || skill_names(&runtime).len() == 2).await;
    drop(release);
    assert_eq!(skill_names(&runtime)[0].1, "slow");
}

#[tokio::test]
async fn mcp_skill_inventory_allows_multiple_slow_pages_in_one_scan() {
    let peer = Peer::with_protocol(Vec::new(), true).await;
    peer.advertise_skills("slow").await;
    // Both pages fit their individual deadlines, but together exceed one page's budget.
    peer.state.lock().await.skills.delay = Duration::from_millis(1150);
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&dir.path().join("mcp.json"), &[("peer", &peer)]).await;
    eventually(async || skill_names(&runtime).len() == 2).await;
    assert_eq!(
        peer.state.lock().await.skills.cursor_requests,
        [None, Some("second".into())]
    );
}

#[tokio::test]
async fn mcp_skill_inventory_keeps_prefix_when_the_sdk_caps_a_scan() {
    let peer = Peer::with_protocol(Vec::new(), true).await;
    peer.advertise_skills("first").await;
    let mut oversized = remote_skill("oversized");
    let limit = agentkit_mcp::McpSkillScanLimits::default().max_entry_decoded_bytes;
    oversized["frontmatter"]["description"] = json!("x".repeat(limit + 1));
    peer.state.lock().await.skills.second_page = Some(vec![oversized].into());

    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(&dir.path().join("mcp.json"), &[("peer", &peer)]).await;
    eventually(async || {
        let snapshot = runtime.skill_inventory_snapshot();
        snapshot.truncated && snapshot.skills.len() == 1
    })
    .await;
    assert_eq!(skill_names(&runtime), [("peer".into(), "first".into())]);
    let state = peer.state.lock().await;
    assert_eq!(state.skills.cursor_requests, [None, Some("second".into())]);
    assert_eq!(state.skills.file_requests, 0);
}

#[tokio::test]
async fn mcp_skill_inventory_fingerprints_manifest_content_not_wire_order() {
    let base = Peer::with_protocol(vec![], true).await;
    let reordered = Peer::with_protocol(vec![], true).await;
    let changed = Peer::with_protocol(vec![], true).await;
    for (peer, digests, marker) in [
        (&base, [("a", "one"), ("b", "two")], "base"),
        (&reordered, [("b", "two"), ("a", "one")], "reordered"),
        (&changed, [("a", "one"), ("b", "new")], "changed"),
    ] {
        peer.advertise_skills("shared").await;
        let mut skill = remote_skill("shared");
        skill["_meta"] = json!({"fixture/marker": marker});
        skill["resources"] = json!(digests.map(|(file, digest)| json!({
            "uri": format!("skill://fixture/{file}.md"), "digest": digest, "size": 2,
        })));
        peer.state.lock().await.skills.catalog = Some(vec![skill].into());
    }
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(
        &dir.path().join("mcp.json"),
        &[
            ("base", &base),
            ("changed", &changed),
            ("reordered", &reordered),
        ],
    )
    .await;
    eventually(async || runtime.skill_inventory().len() == 3).await;
    let inventory = runtime.skill_inventory();
    assert_eq!(
        inventory[0].skill.fingerprint,
        inventory[2].skill.fingerprint
    );
    assert_ne!(
        inventory[0].skill.fingerprint,
        inventory[1].skill.fingerprint
    );
}

#[cfg(unix)]
#[tokio::test]
async fn mcp_skill_inventory_hides_exited_stdio_peers() {
    let script = r#"
import json, pathlib, sys, time
for line in sys.stdin:
    req = json.loads(line)
    if 'id' not in req:
        continue
    out = {'jsonrpc': '2.0', 'id': req['id']}
    method = req['method']
    if method == 'initialize':
        out['result'] = {'protocolVersion': '2025-11-25', 'capabilities': {'resources': {}, 'extensions': {'io.modelcontextprotocol/skills': {}}}, 'serverInfo': {'name': 'fixture', 'version': '1'}}
    elif method == 'resources/list':
        out['result'] = {'resources': []}
    elif method == 'skills/list':
        out['result'] = {'skills': [{'uri': 'skill://example/SKILL.md', 'frontmatter': {'name': 'example', 'description': 'Example'}, 'resources': 'dynamic'}]}
    else:
        out['error'] = {'code': -32601, 'message': 'Method not found'}
    print(json.dumps(out), flush=True)
    if method == 'skills/list':
        while not pathlib.Path(sys.argv[1]).exists():
            time.sleep(0.01)
        sys.exit(0)
"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    let exit = dir.path().join("exit");
    std::fs::write(&path, json!({"mcpServers":{"peer":{"command":"/usr/bin/env","args":["python3","-u","-c",script,exit]}}}).to_string()).unwrap();
    let runtime = super::connect(Some(&path), &[], false, CredentialStorage::Memory)
        .await
        .unwrap();
    eventually(async || !runtime.skill_inventory().is_empty()).await;
    std::fs::write(exit, []).unwrap();
    eventually(async || runtime.skill_inventory().is_empty()).await;
}
