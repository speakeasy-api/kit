//! Real gateway + stdio bridge lifecycle; local fake inference only; no external credentials.
#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Stdio, time::Duration};

use agent_client_protocol::{Channel, ConnectTo, TransportFrame};
use agent_client_protocol_http::HttpClient as AcpHttpClient;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};

const WAIT: Duration = Duration::from_secs(20);
const TOKEN: &str = "gateway-integration-token";

struct Fixture {
    home: tempfile::TempDir,
    root: PathBuf,
    credential: PathBuf,
    client: reqwest::Client,
    provider_url: String,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("project");
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        fs::create_dir(home.path().join(".kit")).unwrap();
        fs::write(
            home.path().join(".kit/config.toml"),
            "provider = \"openrouter\"\nmodel = \"test/model\"\ncredential_store = \"memory\"\n",
        )
        .unwrap();
        let credential = home.path().join("token");
        fs::write(&credential, TOKEN).unwrap();
        fs::set_permissions(&credential, fs::Permissions::from_mode(0o600)).unwrap();
        Self {
            home,
            root,
            credential,
            provider_url: "http://127.0.0.1:1".into(),
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(WAIT)
                .build()
                .unwrap(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kit"));
        command
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", "/usr/bin:/bin")
            .env("OPENROUTER_API_KEY", "unused-local-test-key")
            // Any accidental inference call must fail locally, not reach a paid service.
            .env("OPENROUTER_BASE_URL", &self.provider_url)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    async fn gateway(&self) -> (Child, String) {
        let mut child = self
            .command()
            .args(["gateway", "--listen", "127.0.0.1:0", "--credential-file"])
            .arg(&self.credential)
            .arg("--project")
            .arg(&self.root)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let url = timeout(WAIT, async {
            while let Some(line) = lines.next_line().await.unwrap() {
                if let Some(rest) = line.strip_prefix("Kit gateway listening on ") {
                    return rest.split_whitespace().next().unwrap().to_owned();
                }
            }
            panic!("gateway exited before listening");
        })
        .await
        .expect("gateway startup timed out");
        // Drain diagnostics so an actor can never block on its stderr pipe.
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("gateway: {line}");
            }
        });
        (child, url)
    }

    async fn catalog(&self, url: &str) -> Value {
        let mut client = SdkClient::connect(url).await;
        let response = client.request(2, "session/list", json!({})).await;
        assert!(response.get("error").is_none(), "{response}");
        client.detach().await;
        response["result"].clone()
    }

    fn bridge(&self, url: &str, session: Option<&str>) -> Bridge {
        let mut command = self.command();
        command
            .args(["gateway", "bridge", "--url", url, "--credential-file"])
            .arg(&self.credential)
            .arg("--root")
            .arg(&self.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(session) = session {
            command.args(["--session", session]);
        }
        let mut child = command.spawn().unwrap();
        Bridge {
            updates: Vec::new(),
            stdin: child.stdin.take().unwrap(),
            stdout: BufReader::new(child.stdout.take().unwrap()).lines(),
            child,
        }
    }
}

/// Intentionally use the legacy public SDK transport to prove wire interoperability
/// independently of Kit's bounded stdio bridge.
struct SdkClient {
    channel: Channel,
    transport: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
    updates: Vec<Value>,
}

impl SdkClient {
    async fn connect(url: &str) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {TOKEN}").parse().unwrap(),
        );
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .default_headers(headers)
            .build()
            .unwrap();
        let transport =
            AcpHttpClient::with_endpoint_and_client(format!("{url}/acp/v2"), http).unwrap();
        let (channel, future) = transport.into_channel_and_future();
        let mut client = Self {
            channel,
            transport: tokio::spawn(future),
            updates: Vec::new(),
        };
        let response = client.request(1, "initialize", json!({"protocolVersion":2,"info":{"name":"standard-sdk-test","version":"0"},"capabilities":{}})).await;
        assert_eq!(response["result"]["protocolVersion"], 2, "{response}");
        assert_eq!(
            response["result"]["_meta"]["kit/gateway"]["transport"],
            "bounded-http"
        );
        assert_eq!(
            response["result"]["_meta"]["kit/gateway"]["experimental"],
            true
        );
        client
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.channel
            .tx
            .unbounded_send(TransportFrame::Single(
                serde_json::from_value(
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
                )
                .unwrap(),
            ))
            .unwrap();
        self.response(id).await
    }

    async fn response(&mut self, id: u64) -> Value {
        timeout(WAIT, async {
            while let Some(frame) = self.channel.rx.next().await {
                let TransportFrame::Single(message) = frame else {
                    panic!("unexpected SDK batch");
                };
                let message = serde_json::to_value(message).unwrap();
                if message["method"] == "session/update" {
                    self.updates.push(message["params"]["update"].clone());
                }
                if message["id"] == id {
                    return message;
                }
            }
            panic!("SDK transport closed before response");
        })
        .await
        .expect("SDK response timed out")
    }

    fn send(&self, message: Value) -> bool {
        self.channel
            .tx
            .unbounded_send(TransportFrame::Single(
                serde_json::from_value(message).unwrap(),
            ))
            .is_ok()
    }

    async fn assert_rejected_or_closed(&mut self, id: u64, method: &str, params: Value) {
        if !self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})) {
            return;
        }
        timeout(WAIT, async {
            while let Some(frame) = self.channel.rx.next().await {
                let TransportFrame::Single(message) = frame else {
                    panic!("unexpected SDK batch");
                };
                let message = serde_json::to_value(message).unwrap();
                if message["id"] == id {
                    assert!(
                        message.get("error").is_some(),
                        "stale controller accepted: {message}"
                    );
                    return;
                }
            }
        })
        .await
        .expect("stale controller neither rejected request nor closed");
    }

    async fn detach(self) {
        self.channel.tx.close_channel();
        timeout(WAIT, self.transport)
            .await
            .expect("SDK transport did not close")
            .unwrap()
            .unwrap();
    }
}

struct Bridge {
    updates: Vec<Value>,
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl Bridge {
    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        let mut bytes = serde_json::to_vec(
            &json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}),
        )
        .unwrap();
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await.unwrap();
        timeout(WAIT, async {
            loop {
                let line = self
                    .stdout
                    .next_line()
                    .await
                    .unwrap()
                    .expect("bridge stdout closed");
                let message: Value = serde_json::from_str(&line).unwrap();
                if message["method"] == "session/update" {
                    self.updates.push(message["params"]["update"].clone());
                }
                if message["id"] == id {
                    assert!(message.get("error").is_none(), "{message}");
                    return message["result"].clone();
                }
            }
        })
        .await
        .expect("ACP response timed out")
    }

    async fn handshake(&mut self) -> String {
        let initialized = self.request(1, "initialize", json!({
            "protocolVersion":2, "info":{"name":"gateway-integration","version":"0"}, "capabilities":{}
        })).await;
        assert_eq!(initialized["protocolVersion"], 2);
        let session = self
            .request(2, "session/new", json!({"cwd":"/ignored", "mcpServers":[]}))
            .await;
        session["sessionId"].as_str().unwrap().to_owned()
    }

    async fn detach(self) {
        let Self {
            mut child,
            stdin,
            stdout: _,
            updates: _,
        } = self;
        drop(stdin);
        assert!(
            timeout(WAIT, child.wait())
                .await
                .expect("bridge did not detach")
                .unwrap()
                .success()
        );
    }
}

async fn stop_gateway(child: &mut Child) {
    let status = Command::new("/bin/kill")
        .args(["-INT", &child.id().unwrap().to_string()])
        .status()
        .await
        .unwrap();
    assert!(status.success());
    assert!(
        timeout(WAIT, child.wait())
            .await
            .expect("gateway did not stop")
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn bridge_creates_persists_reconnects_and_restores_session() {
    let fixture = Fixture::new();
    let (mut gateway, url) = fixture.gateway().await;
    let mut bridge = fixture.bridge(&url, None);
    let id = bridge.handshake().await;
    bridge.detach().await;

    let listed = timeout(
        WAIT,
        fixture
            .command()
            .args(["gateway", "list", "--url", &url, "--credential-file"])
            .arg(&fixture.credential)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let catalog: Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert!(
        catalog["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["sessionId"] == id && row["_meta"]["kit.gateway.state"] == "resident")
    );
    let mut reconnected = fixture.bridge(&url, Some(&id));
    assert_eq!(reconnected.handshake().await, id);
    reconnected.detach().await;
    stop_gateway(&mut gateway).await;

    let (mut gateway, url) = fixture.gateway().await;
    let catalog = fixture.catalog(&url).await;
    assert!(
        catalog["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["sessionId"] == id && row["_meta"]["kit.gateway.state"] == "restorable"),
        "{catalog}"
    );
    let mut restored = fixture.bridge(&url, Some(&id));
    assert_eq!(restored.handshake().await, id);
    restored.detach().await;
    stop_gateway(&mut gateway).await;
}

#[tokio::test]
async fn standard_sdk_creates_lists_resumes_and_restores() {
    let fixture = Fixture::new();
    let (mut gateway, url) = fixture.gateway().await;
    let mut client = SdkClient::connect(&url).await;
    let created = client
        .request(
            2,
            "session/new",
            json!({"cwd":fixture.root,"mcpServers":[]}),
        )
        .await;
    assert!(created.get("error").is_none(), "{created}");
    let id = created["result"]["sessionId"].as_str().unwrap().to_owned();
    let catalog = client.request(3, "session/list", json!({})).await;
    let row = catalog["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["sessionId"] == id)
        .unwrap();
    assert_eq!(row["cwd"], json!(fixture.root));
    assert!(row["title"].is_string(), "{row}");
    assert_eq!(row["_meta"]["kit.gateway.state"], "resident");
    client.detach().await;

    let mut client = SdkClient::connect(&url).await;
    let resumed = client
        .request(
            2,
            "session/resume",
            json!({"sessionId":id,"cwd":fixture.root,"mcpServers":[],"replayFrom":{"type":"start"}}),
        )
        .await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    client.detach().await;
    stop_gateway(&mut gateway).await;

    let (mut gateway, url) = fixture.gateway().await;
    let mut client = SdkClient::connect(&url).await;
    let restored = client
        .request(
            2,
            "session/resume",
            json!({"sessionId":id,"cwd":fixture.root,"mcpServers":[],"replayFrom":{"type":"start"}}),
        )
        .await;
    assert!(restored.get("error").is_none(), "{restored}");
    client.detach().await;
    stop_gateway(&mut gateway).await;
}

// Discover only direct children of this fixture's still-running gateway. No
// global process-name matching: parallel tests and unrelated Kit users are safe.
async fn gateway_child_pids(gateway: &mut Child) -> Vec<u32> {
    assert!(gateway.try_wait().unwrap().is_none(), "gateway exited");
    let parent = gateway.id().unwrap();
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid="])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<u32>().ok()?;
            let ppid = fields.next()?.parse::<u32>().ok()?;
            (ppid == parent).then_some(pid)
        })
        .collect()
}

#[tokio::test]
async fn exited_sessions_release_capacity_and_preserve_history_without_restart() {
    const ANSWER: &str = "History survives resident reclamation.";
    let (fixture, mut requests, provider) = controlled_provider().await;
    let (mut gateway, url) = fixture.gateway().await;
    let mut catalog = SdkClient::connect(&url).await;
    let mut saved = None;

    // The production resident limit is 64. Every successful admission after
    // that many exits must reuse capacity in this SAME gateway process.
    for index in 0..65 {
        let mut client = SdkClient::connect(&url).await;
        let created = client
            .request(2, "session/new", json!({"cwd":fixture.root}))
            .await;
        assert!(created.get("error").is_none(), "session {index}: {created}");
        let id = created["result"]["sessionId"].as_str().unwrap().to_owned();
        if index == 0 {
            client.start_prompt(3, &id, "Save this history").await;
            let (_, release) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
            release.send(ANSWER).unwrap();
            client.wait_idle().await;
            saved = Some(id.clone());
        }
        client.detach().await;

        let children = gateway_child_pids(&mut gateway).await;
        assert_eq!(
            children.len(),
            1,
            "expected only the current resident child"
        );
        let status = Command::new("/bin/kill")
            .args(["-KILL", &children[0].to_string()])
            .status()
            .await
            .unwrap();
        assert!(status.success());

        // Wait for actual OS reaping, not a fixed sleep. Do not list here:
        // each next Create must reclaim capacity without relying on List.
        timeout(WAIT, async {
            while !gateway_child_pids(&mut gateway).await.is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("resident child was not reaped");
    }

    let saved = saved.unwrap();
    let listed = catalog.request(3, "session/list", json!({})).await;
    let row = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["sessionId"] == saved)
        .expect("reclamation removed durable catalog history");
    assert_eq!(row["_meta"]["kit.gateway.state"], "restorable", "{row}");
    // Abrupt child exit leaves a stale durable lock, and HTTP resume has no
    // force option. This test is about registry reclamation, not stale-lock
    // recovery: remove only this fixture session's lock AFTER its child has
    // been reaped. Keep the transcript itself untouched for gateway restore.
    let lock_name = format!("{saved}.lock");
    let locks: Vec<_> = fs::read_dir(fixture.home.path().join(".kit/sessions"))
        .unwrap()
        .map(|workspace| workspace.unwrap().path().join(&lock_name))
        .filter(|path| path.exists())
        .collect();
    assert_eq!(locks.len(), 1, "expected the exited session's stale lock");
    fs::remove_file(&locks[0]).unwrap();
    let mut restored = SdkClient::connect(&url).await;
    let response = restored
        .request(
            2,
            "session/resume",
            json!({"sessionId":saved,"cwd":fixture.root,"replayFrom":{"type":"start"}}),
        )
        .await;
    assert!(response.get("error").is_none(), "{response}");
    assert_replayed_answer(&restored.updates, ANSWER);
    assert!(restored.updates.iter().any(|update| {
        update["sessionUpdate"] == "user_message"
            && update["content"].to_string().contains("Save this history")
    }));
    restored.detach().await;
    catalog.detach().await;
    stop_gateway(&mut gateway).await;
    provider.abort();
}

#[tokio::test]
async fn gateway_denies_missing_or_wrong_auth_and_unregistered_roots() {
    let fixture = Fixture::new();
    let (mut gateway, url) = fixture.gateway().await;
    for token in [None, Some("wrong-token")] {
        let request = fixture
            .client
            .post(format!("{url}/acp/v2"))
            .json(&json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":2,"capabilities":{},"info":{"name":"auth-test","version":"0"}}}));
        let request = if let Some(token) = token {
            request.bearer_auth(token)
        } else {
            request
        };
        assert_eq!(
            request.send().await.unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        fixture
            .client
            .post(format!("{url}/v1"))
            .bearer_auth(TOKEN)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );
    let descendant = fixture.root.join("child");
    fs::create_dir(&descendant).unwrap();
    let mut client = SdkClient::connect(&url).await;
    for (index, root) in [descendant, fixture.home.path().to_path_buf()]
        .iter()
        .enumerate()
    {
        let response = client
            .request(
                index as u64 + 2,
                "session/new",
                json!({"cwd":root,"mcpServers":[]}),
            )
            .await;
        assert!(response.get("error").is_some(), "{response}");
    }
    client.detach().await;
    assert_eq!(fixture.catalog(&url).await["sessions"], json!([]));
    stop_gateway(&mut gateway).await;
}

// The fake is only the HTTP provider boundary, following session_reasoning_effort.rs.
// The gateway, bridge, ACP child, inference loop, and transcript writer are real.
async fn delayed_provider(
    answer: &'static str,
) -> (
    Fixture,
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let mut fixture = Fixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    fixture.provider_url = format!("http://{}/stream", listener.local_addr().unwrap());
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let provider = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        let body = loop {
            let n = stream.read(&mut buffer).await.unwrap();
            assert!(n > 0, "provider request closed early");
            request.extend_from_slice(&buffer[..n]);
            if let Some(end) = request.windows(4).position(|s| s == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]);
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break serde_json::from_slice::<Value>(&request[end + 4..end + 4 + length])
                        .unwrap();
                }
            }
        };
        assert!(
            body["messages"].as_array().unwrap().iter().any(|message| {
                message["role"] == "user"
                    && message["content"].to_string().contains("Complete offline")
            }),
            "{body}"
        );
        started_tx.send(()).unwrap();
        release_rx.await.unwrap();
        let chunk = json!({"id":"offline-test","choices":[{"index":0,"delta":{"role":"assistant","content":answer},"finish_reason":"stop"}]});
        let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    (fixture, started_rx, release_tx, provider)
}

#[tokio::test]
async fn sdk_replacement_rejects_stale_control_and_preserves_new_controller() {
    const ANSWER: &str = "New controller completed despite stale cancellation.";
    let (fixture, started_rx, release_tx, provider) = delayed_provider(ANSWER).await;
    let (mut gateway, url) = fixture.gateway().await;
    let mut old = SdkClient::connect(&url).await;
    let created = old
        .request(
            2,
            "session/new",
            json!({"cwd":fixture.root,"mcpServers":[]}),
        )
        .await;
    assert!(created.get("error").is_none(), "{created}");
    let id = created["result"]["sessionId"].as_str().unwrap().to_owned();
    let option = created["result"]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["type"] == "select")
        .expect("host advertised no select config option");
    let config = serde_json::to_value(agentkit_acp::v2::wire::SetSessionConfigOptionRequest::new(
        id.clone(),
        option["configId"].as_str().unwrap(),
        option["currentValue"].as_str().unwrap(),
    ))
    .unwrap();
    let mut new = SdkClient::connect(&url).await;
    let invalid = new
        .request(
            2,
            "session/resume",
            json!({"sessionId":id,"cwd":fixture.root,"replayFrom":{"type":"_unsupported"}}),
        )
        .await;
    assert!(invalid.get("error").is_some(), "{invalid}");
    // Invalid replay must be checked before replacing the original controller.
    let configured = old
        .request(3, "session/set_config_option", config.clone())
        .await;
    assert!(configured.get("error").is_none(), "{configured}");
    let other = old
        .request(5, "session/new", json!({"cwd":fixture.root}))
        .await;
    assert!(other.get("error").is_none(), "{other}");
    let mut other_config = config.clone();
    other_config["sessionId"] = other["result"]["sessionId"].clone();
    let resumed = new
        .request(
            3,
            "session/resume",
            json!({"sessionId":id,"cwd":fixture.root,"replayFrom":{"type":"start"}}),
        )
        .await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    let unaffected = old
        .request(6, "session/set_config_option", other_config)
        .await;
    assert!(
        unaffected.get("error").is_none(),
        "replacement detached unrelated session: {unaffected}"
    );
    new.updates.clear();
    assert!(new.send(json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":id,"prompt":[{"type":"text","text":"Complete offline"}]}})));
    timeout(WAIT, started_rx)
        .await
        .expect("new controller prompt never reached provider")
        .unwrap();
    // A notification has no response: the real prompt result below proves this
    // stale cancel did not reach the child, even if transport teardown raced it.
    old.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}));
    old.assert_rejected_or_closed(
        4,
        "session/prompt",
        json!({"sessionId":id,"prompt":[{"type":"text","text":"Stale controller must not run"}]}),
    )
    .await;
    // Close the SDK transport (including its DELETE), not the resident session.
    // Replacement can already have closed the old stream with a transport error.
    old.channel.tx.close_channel();
    let _closed = timeout(WAIT, old.transport)
        .await
        .expect("old SDK transport did not terminate")
        .unwrap();
    release_tx.send(()).unwrap();
    timeout(WAIT, provider).await.unwrap().unwrap();
    let completed = new.response(4).await;
    assert!(completed.get("error").is_none(), "{completed}");
    // ACP v2 acknowledges acceptance with an empty response. Completion is
    // a state update, not the v1 stopReason response.
    timeout(WAIT, async {
        while !new
            .updates
            .iter()
            .any(|update| update["sessionUpdate"] == "state_update" && update["state"] == "idle")
        {
            let frame = new
                .channel
                .rx
                .next()
                .await
                .expect("transport closed before idle");
            let TransportFrame::Single(message) = frame else {
                panic!("unexpected batch");
            };
            let message = serde_json::to_value(message).unwrap();
            if message["method"] == "session/update" {
                new.updates.push(message["params"]["update"].clone());
            }
        }
    })
    .await
    .expect("new controller prompt never completed");
    assert_replayed_answer(&new.updates, ANSWER);
    let configured = new.request(5, "session/set_config_option", config).await;
    assert!(
        configured.get("error").is_none(),
        "new controller detached by stale DELETE: {configured}"
    );
    new.detach().await;
    stop_gateway(&mut gateway).await;
}

#[tokio::test]
async fn running_prompt_finishes_unattached_and_replays_after_restart() {
    const ANSWER: &str = "Completed while nobody was attached.";
    let (fixture, started_rx, release_tx, provider) = delayed_provider(ANSWER).await;
    let (mut gateway, url) = fixture.gateway().await;
    let mut bridge = fixture.bridge(&url, None);
    let id = bridge.handshake().await;
    let prompt = json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":id,"prompt":[{"type":"text","text":"Complete offline"}]}});
    bridge
        .stdin
        .write_all(format!("{prompt}\n").as_bytes())
        .await
        .unwrap();
    timeout(WAIT, started_rx)
        .await
        .expect("provider was not called")
        .unwrap();
    // SIGKILL cannot send graceful transport teardown or cancel. The lost
    // controller must not own inference lifetime.
    bridge.child.kill().await.unwrap();
    drop(bridge);
    release_tx.send(()).unwrap();
    timeout(WAIT, provider).await.unwrap().unwrap();

    // Observe the real durable write before making ANY new gateway/ACP request.
    // This proves completion did not depend on attachment or a new command.
    timeout(WAIT, async {
        loop {
            let completed = fs::read_dir(fixture.home.path().join(".kit/sessions"))
                .unwrap()
                .flat_map(|workspace| fs::read_dir(workspace.unwrap().path()).unwrap())
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .path()
                        .file_name()
                        .is_some_and(|name| name == format!("{id}.jsonl").as_str())
                })
                .any(|entry| fs::read_to_string(entry.path()).unwrap().contains(ANSWER));
            if completed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("detached prompt never persisted its answer");

    let mut reconnected = fixture.bridge(&url, Some(&id));
    assert_eq!(reconnected.handshake().await, id);
    assert_replayed_answer(&reconnected.updates, ANSWER);
    // Transcript persistence precedes the final live state notification. A
    // resumed stream can truthfully replay running, then deliver idle later.
    timeout(WAIT, async {
        while !reconnected
            .updates
            .iter()
            .rev()
            .find(|update| update["sessionUpdate"] == "state_update")
            .is_some_and(|update| update["state"] == "idle")
        {
            let line = reconnected
                .stdout
                .next_line()
                .await
                .unwrap()
                .expect("bridge closed before final idle");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["method"] == "session/update" {
                reconnected
                    .updates
                    .push(message["params"]["update"].clone());
            }
        }
    })
    .await
    .expect("resumed stream never reached idle");
    let state = reconnected
        .updates
        .iter()
        .rev()
        .find(|update| update["sessionUpdate"] == "state_update");
    assert_eq!(
        state.map(|update| &update["state"]),
        Some(&json!("idle")),
        "{:?}",
        reconnected.updates
    );
    reconnected.detach().await;
    let mut sdk = SdkClient::connect(&url).await;
    let resumed = sdk.request(2, "session/resume", json!({"sessionId":id,"cwd":fixture.root,"mcpServers":[],"replayFrom":{"type":"start"}})).await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    assert_replayed_answer(&sdk.updates, ANSWER);
    sdk.detach().await;
    stop_gateway(&mut gateway).await;

    let (mut gateway, url) = fixture.gateway().await;
    let mut restored = fixture.bridge(&url, Some(&id));
    assert_eq!(restored.handshake().await, id);
    assert_replayed_answer(&restored.updates, ANSWER);
    assert!(
        restored.updates.iter().any(|update| {
            update["sessionUpdate"] == "user_message"
                && update["content"].to_string().contains("Complete offline")
        }),
        "{:?}",
        restored.updates
    );
    restored.detach().await;
    let mut sdk = SdkClient::connect(&url).await;
    let resumed = sdk.request(2, "session/resume", json!({"sessionId":id,"cwd":fixture.root,"mcpServers":[],"replayFrom":{"type":"start"}})).await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    assert_replayed_answer(&sdk.updates, ANSWER);
    sdk.detach().await;
    stop_gateway(&mut gateway).await;
}

fn assert_replayed_answer(updates: &[Value], answer: &str) {
    // Live replay carries chunks; durable transcript replay carries whole messages.
    let mut text = String::new();
    for update in updates {
        match update["sessionUpdate"].as_str() {
            Some("agent_message") => {
                for block in update["content"].as_array().unwrap() {
                    if let Some(content) = block["text"].as_str() {
                        text.push_str(content);
                    }
                }
            }
            Some("agent_message_chunk") => {
                if let Some(chunk) = update["content"]["text"].as_str() {
                    text.push_str(chunk);
                }
            }
            _ => {}
        }
    }
    assert_eq!(text, answer, "{updates:?}");
}

#[tokio::test]
async fn shutdown_reaps_resident_with_live_http_controller() {
    let fixture = Fixture::new();
    let (mut gateway, url) = fixture.gateway().await;
    let mut client = SdkClient::connect(&url).await;
    let created = client
        .request(2, "session/new", json!({"cwd":fixture.root}))
        .await;
    assert!(created.get("error").is_none(), "{created}");
    let id = created["result"]["sessionId"].as_str().unwrap().to_owned();
    // Keep the SDK/SSE connection alive while stopping the supervisor. It must
    // not make graceful shutdown wait indefinitely or retain the transcript lock.
    stop_gateway(&mut gateway).await;
    client.channel.tx.close_channel();
    let _closed = timeout(WAIT, client.transport).await.unwrap().unwrap();
    let (mut gateway, url) = fixture.gateway().await;
    let mut client = SdkClient::connect(&url).await;
    let restored = client
        .request(
            2,
            "session/resume",
            json!({"sessionId":id,"replayFrom":{"type":"start"}}),
        )
        .await;
    assert!(restored.get("error").is_none(), "{restored}");
    client.detach().await;
    stop_gateway(&mut gateway).await;
}

// A genuine provider boundary: each HTTP inference request is observable and
// independently released, including requests abandoned by session/cancel.
type ProviderRequest = (Value, tokio::sync::oneshot::Sender<&'static str>);
async fn controlled_provider() -> (
    Fixture,
    tokio::sync::mpsc::Receiver<ProviderRequest>,
    tokio::task::JoinHandle<()>,
) {
    async fn inference(
        axum::extract::State(requests): axum::extract::State<
            tokio::sync::mpsc::Sender<ProviderRequest>,
        >,
        axum::Json(body): axum::Json<Value>,
    ) -> impl axum::response::IntoResponse {
        let (release, answer) = tokio::sync::oneshot::channel();
        requests.send((body, release)).await.unwrap();
        let answer = answer.await.unwrap_or("abandoned");
        // Keep individual ACP frames below the frame ceiling even when the
        // complete answer is large enough to saturate an HTTP mailbox.
        let mut remaining = answer;
        let mut events = String::new();
        while !remaining.is_empty() {
            let mut end = remaining.len().min(64 * 1024);
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            let (content, rest) = remaining.split_at(end);
            let finish = if rest.is_empty() { Some("stop") } else { None };
            let chunk = json!({"id":"controlled-test","choices":[{"index":0,"delta":{"role":"assistant","content":content},"finish_reason":finish}]});
            events.push_str(&format!("data: {chunk}\n\n"));
            remaining = rest;
        }
        events.push_str("data: [DONE]\n\n");
        ([("content-type", "text/event-stream")], events)
    }
    let mut fixture = Fixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    fixture.provider_url = format!("http://{}/stream", listener.local_addr().unwrap());
    let (requests, received) = tokio::sync::mpsc::channel(8);
    let router = axum::Router::new()
        .fallback(axum::routing::post(inference))
        // Cumulative-output tests send genuine growing conversation history.
        // This provider fixture must not impose Axum's unrelated 2 MiB body cap.
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(requests);
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (fixture, received, task)
}

impl SdkClient {
    async fn wait_idle(&mut self) {
        timeout(WAIT, async {
            loop {
                // A resumed controller can still receive the previous turn's
                // final idle event. Only accept idle after this turn ran.
                let mut running = false;
                for update in &self.updates {
                    if update["sessionUpdate"] == "state_update" {
                        if update["state"] == "running" {
                            running = true;
                        } else if running && update["state"] == "idle" {
                            return;
                        }
                    }
                }
                let TransportFrame::Single(message) = self
                    .channel
                    .rx
                    .next()
                    .await
                    .expect("transport closed before idle")
                else {
                    panic!("unexpected batch");
                };
                let message = serde_json::to_value(message).unwrap();
                if message["method"] == "session/update" {
                    self.updates.push(message["params"]["update"].clone());
                }
            }
        })
        .await
        .expect("session did not become idle");
    }

    async fn start_prompt(&mut self, request: u64, session: &str, text: &str) {
        self.updates.clear();
        let response = self
            .request(
                request,
                "session/prompt",
                json!({"sessionId":session,"prompt":[{"type":"text","text":text}]}),
            )
            .await;
        assert!(response.get("error").is_none(), "{response}");
    }
}

#[tokio::test]
async fn bridge_healthy_connection_exceeds_former_lifetime_output_limit() {
    // Every provider answer and live ACP content frame is modest. Receiving the
    // previous turn's idle event is the pacing boundary, not elapsed time or an
    // assumption about socket buffering. The same bridge/HTTP connection and
    // resident session remain alive for the entire sequence.
    static ANSWER: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| "x".repeat(64 * 1024));
    const TURNS: usize = 129;
    assert!(TURNS * ANSWER.len() > 8 * 1024 * 1024);

    let (fixture, mut requests, provider) = controlled_provider().await;
    let (mut gateway, url) = fixture.gateway().await;
    let mut bridge = fixture.bridge(&url, None);
    let id = bridge.handshake().await;
    let mut received_bytes = 0;
    for turn in 0..TURNS {
        bridge.updates.clear();
        bridge
            .request(
                3 + turn as u64,
                "session/prompt",
                json!({"sessionId":id,"prompt":[{"type":"text","text":"Next answer"}]}),
            )
            .await;
        let (_, release) = timeout(WAIT, requests.recv())
            .await
            .expect("prompt did not reach the provider")
            .unwrap();
        release.send(ANSWER.as_str()).unwrap();
        timeout(WAIT, async {
            loop {
                let line = bridge
                    .stdout
                    .next_line()
                    .await
                    .unwrap()
                    .expect("healthy bridge closed before idle");
                let message: Value = serde_json::from_str(&line).unwrap();
                if message["method"] == "session/update" {
                    let update = &message["params"]["update"];
                    let idle =
                        update["sessionUpdate"] == "state_update" && update["state"] == "idle";
                    bridge.updates.push(update.clone());
                    if idle {
                        break;
                    }
                }
            }
        })
        .await
        .expect("healthy connection did not complete the next turn");
        assert_replayed_answer(&bridge.updates, ANSWER.as_str());
        received_bytes += ANSWER.len();
    }
    assert!(received_bytes > 8 * 1024 * 1024);
    // Request/response still works after exceeding the former lifetime cap;
    // there is deliberately no reconnect, resume, or prompt retry here.
    let listed = bridge
        .request(3 + TURNS as u64, "session/list", json!({}))
        .await;
    assert!(
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| { session["sessionId"].as_str() == Some(id.as_str()) })
    );
    bridge.detach().await;
    stop_gateway(&mut gateway).await;
    provider.abort();
}

#[tokio::test]
async fn sdk_steering_delivers_then_accepts_subsequent_prompt() {
    let (fixture, mut requests, provider) = controlled_provider().await;
    let (mut gateway, url) = fixture.gateway().await;
    let mut client = SdkClient::connect(&url).await;
    let created = client
        .request(
            2,
            "session/new",
            json!({"cwd":fixture.root,"mcpServers":[]}),
        )
        .await;
    let id = created["result"]["sessionId"].as_str().unwrap();
    client.start_prompt(3, id, "First instruction").await;
    let (_, release) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
    let steer = client.request(4, "session/inject", json!({"sessionId":id,"mode":"steer","content":[{"type":"text","text":"Use the steered instruction"}]})).await;
    assert!(steer.get("error").is_none(), "{steer}");
    release.send("Initial answer").unwrap();
    let (body, release) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
    assert!(
        body["messages"]
            .to_string()
            .contains("Use the steered instruction"),
        "{body}"
    );
    release.send("Steered answer").unwrap();
    client.wait_idle().await;
    assert_replayed_answer(&client.updates, "Initial answerSteered answer");
    client.start_prompt(5, id, "Next independent prompt").await;
    let (body, release) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
    assert!(
        body["messages"]
            .to_string()
            .contains("Next independent prompt")
    );
    release.send("Subsequent answer").unwrap();
    client.wait_idle().await;
    assert_replayed_answer(&client.updates, "Subsequent answer");
    client.detach().await;
    stop_gateway(&mut gateway).await;
    provider.abort();
}

#[tokio::test]
async fn sdk_cancel_settles_then_accepts_subsequent_prompt() {
    let (fixture, mut requests, provider) = controlled_provider().await;
    let (mut gateway, url) = fixture.gateway().await;
    let mut client = SdkClient::connect(&url).await;
    let created = client
        .request(
            2,
            "session/new",
            json!({"cwd":fixture.root,"mcpServers":[]}),
        )
        .await;
    let id = created["result"]["sessionId"].as_str().unwrap();
    client
        .start_prompt(3, id, "Cancel this blocked inference")
        .await;
    let (_, abandoned) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
    assert!(
        client.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
    );
    // Cancellation must settle without waiting for the provider to complete.
    client.wait_idle().await;
    client.start_prompt(4, id, "Healthy after cancel").await;
    let (body, release) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
    assert!(
        body["messages"]
            .to_string()
            .contains("Healthy after cancel")
    );
    release.send("Recovered after cancellation").unwrap();
    client.wait_idle().await;
    assert_replayed_answer(&client.updates, "Recovered after cancellation");
    drop(abandoned);
    client.detach().await;
    stop_gateway(&mut gateway).await;
    provider.abort();
}

#[tokio::test]
async fn sdk_transport_loss_preserves_running_prompt_and_replays() {
    let (fixture, mut requests, provider) = controlled_provider().await;
    let (mut gateway, url) = fixture.gateway().await;
    let mut client = SdkClient::connect(&url).await;
    let created = client
        .request(
            2,
            "session/new",
            json!({"cwd":fixture.root,"mcpServers":[]}),
        )
        .await;
    let id = created["result"]["sessionId"].as_str().unwrap();
    client
        .start_prompt(3, id, "Survive lost HTTP transport")
        .await;
    let (_, release) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
    // Abort the actual HTTP client driver, without DELETE or session/cancel.
    client.transport.abort();
    assert!(client.transport.await.unwrap_err().is_cancelled());
    drop(client.channel);
    release.send("Completed after HTTP transport loss").unwrap();
    // Observe durable completion before a new HTTP request can refresh a lease.
    timeout(WAIT, async {
        loop {
            let complete = fs::read_dir(fixture.home.path().join(".kit/sessions"))
                .unwrap()
                .flat_map(|workspace| fs::read_dir(workspace.unwrap().path()).unwrap())
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name() == format!("{id}.jsonl").as_str())
                .any(|entry| {
                    fs::read_to_string(entry.path())
                        .unwrap()
                        .contains("Completed after HTTP transport loss")
                });
            if complete {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("lost HTTP transport cancelled inference");
    let mut resumed = SdkClient::connect(&url).await;
    let response = resumed
        .request(
            2,
            "session/resume",
            json!({"sessionId":id,"cwd":fixture.root,"replayFrom":{"type":"start"}}),
        )
        .await;
    assert!(response.get("error").is_none(), "{response}");
    assert_replayed_answer(&resumed.updates, "Completed after HTTP transport loss");
    resumed
        .start_prompt(3, id, "Healthy after transport loss")
        .await;
    let (_, release) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
    release.send("New controller answer").unwrap();
    resumed.wait_idle().await;
    assert_replayed_answer(&resumed.updates, "New controller answer");
    resumed.detach().await;
    stop_gateway(&mut gateway).await;
    provider.abort();
}

#[tokio::test]
async fn sdk_revoke_injection_requires_current_controller() {
    let (fixture, mut requests, provider) = controlled_provider().await;
    let (mut gateway, url) = fixture.gateway().await;
    let mut old = SdkClient::connect(&url).await;
    let created = old
        .request(2, "session/new", json!({"cwd":fixture.root}))
        .await;
    let id = created["result"]["sessionId"].as_str().unwrap();
    old.start_prompt(3, id, "Run while a steer is pending")
        .await;
    let (_, release) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
    let injected = old.request(4, "session/inject", json!({"sessionId":id,"mode":"steer","content":[{"type":"text","text":"Revoked instruction must not reach inference"}]})).await;
    assert!(injected.get("error").is_none(), "{injected}");
    let message_id = injected["result"]["messageId"].as_str().unwrap();
    let mut current = SdkClient::connect(&url).await;
    let resumed = current
        .request(
            2,
            "session/resume",
            json!({"sessionId":id,"replayFrom":{"type":"start"}}),
        )
        .await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    let revoke = json!({"sessionId":id,"messageId":message_id});
    old.assert_rejected_or_closed(5, "session/revoke_inject", revoke.clone())
        .await;
    // The pinned capability also advertises pending.replace; exercise its
    // companion route rather than advertising an unsupported operation.
    let replaced = current.request(6, "session/replace_inject", json!({"sessionId":id,"messageId":message_id,"content":[{"type":"text","text":"Replacement must also be revoked"}]})).await;
    assert!(replaced.get("error").is_none(), "{replaced}");
    let revoked = current.request(3, "session/revoke_inject", revoke).await;
    assert_eq!(revoked["result"], json!({}), "{revoked}");
    release.send("Answer without revoked steer").unwrap();
    current.wait_idle().await;
    assert!(!current.updates.iter().any(
        |update| update["sessionUpdate"] == "user_message" && update["messageId"] == message_id
    ));
    current.start_prompt(4, id, "Continue after revoke").await;
    let (body, release) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
    assert!(
        !body["messages"]
            .to_string()
            .contains("Revoked instruction must not reach inference"),
        "{body}"
    );
    release.send("Healthy after revoke").unwrap();
    current.wait_idle().await;
    assert_replayed_answer(&current.updates, "Healthy after revoke");
    old.channel.tx.close_channel();
    let _ = timeout(WAIT, old.transport).await.unwrap();
    current.detach().await;
    stop_gateway(&mut gateway).await;
    provider.abort();
}

#[tokio::test]
async fn sdk_cold_and_resident_replay_preference_matrix() {
    const ANSWER: &str = "History for the replay preference matrix";
    let (fixture, mut requests, provider) = controlled_provider().await;
    let (mut gateway, url) = fixture.gateway().await;
    let mut seed = SdkClient::connect(&url).await;
    let created = seed
        .request(2, "session/new", json!({"cwd":fixture.root}))
        .await;
    let id = created["result"]["sessionId"].as_str().unwrap();
    seed.start_prompt(3, id, "Seed replay history").await;
    let (_, release) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
    release.send(ANSWER).unwrap();
    seed.wait_idle().await;
    seed.detach().await;
    stop_gateway(&mut gateway).await;
    provider.abort();

    // Each cold preference is followed by every resident preference, also
    // proving a no-replay cold restore still seeds the later full replay cache.
    for cold in [None, Some(Value::Null), Some(json!({"type":"start"}))] {
        let (mut gateway, url) = fixture.gateway().await;
        for (index, replay) in [cold, None, Some(Value::Null), Some(json!({"type":"start"}))]
            .into_iter()
            .enumerate()
        {
            let mut client = SdkClient::connect(&url).await;
            let mut params = json!({"sessionId":id,"cwd":fixture.root});
            if let Some(replay) = replay {
                params["replayFrom"] = replay;
            }
            let expected = params["replayFrom"] == json!({"type":"start"});
            let response = client.request(2, "session/resume", params.clone()).await;
            assert!(
                response.get("error").is_none(),
                "case {index}: {params}: {response}"
            );
            if expected {
                assert_replayed_answer(&client.updates, ANSWER);
            } else {
                assert!(
                    !client.updates.iter().any(|update| matches!(
                        update["sessionUpdate"].as_str(),
                        Some(
                            "user_message"
                                | "user_message_chunk"
                                | "agent_message"
                                | "agent_message_chunk"
                                | "agent_thought"
                                | "agent_thought_chunk"
                        )
                    )),
                    "case {index}: {params}: {:?}",
                    client.updates
                );
            }
            client.detach().await;
        }
        stop_gateway(&mut gateway).await;
    }
}

// Deterministic generic ACP HTTP fixture: no dependency on a control plane.
// Session streams use the SAME path plus Acp-Session-Id, not a sessions URL.
async fn next_event(response: &mut reqwest::Response, buffer: &mut String) -> Value {
    timeout(WAIT, async {
        loop {
            if let Some(end) = buffer.find("\n\n") {
                let event: String = buffer.drain(..end + 2).collect();
                if let Some(data) = event.lines().find_map(|line| line.strip_prefix("data: ")) {
                    return serde_json::from_str(data).unwrap();
                }
            } else {
                let bytes = response.chunk().await.unwrap().expect("SSE closed");
                buffer.push_str(std::str::from_utf8(&bytes).unwrap());
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn pinned_sdk_http_initialize_capabilities_and_session_stream_contract() {
    let fixture = Fixture::new();
    let (mut gateway, url) = fixture.gateway().await;
    let endpoint = format!("{url}/acp/v2");
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"info":{"name":"generic-http-fixture","version":"1"},"capabilities":{}}});
    let response = fixture
        .client
        .post(&endpoint)
        .bearer_auth(TOKEN)
        .json(&initialize)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "application/json");
    let connection = response.headers()["acp-connection-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(!connection.is_empty());
    let initialized: Value = response.json().await.unwrap();
    assert_eq!(
        initialized,
        json!({"jsonrpc":"2.0","id":1,"result":{
            "protocolVersion":2,"info":{"name":"kit-gateway","version":env!("CARGO_PKG_VERSION")},
            "capabilities":{"session":{"prompt":{"image":{},"audio":{},"embeddedContext":{}},"inject":{"modes":["steer"],"steerInStream":["finish"],"pending":{"replace":true}},"list":{}}},
            "_meta":{"kit/gateway":{"experimental":true,"transport":"bounded-http","maxFrameBytes":1048576,"coreBufferedBytesPerDirection":16777216,"httpEgressBytesPerConnection":4194304,"liveReplayLimitBytes":8388608,"liveReplayLimitEvents":4094}}
        }})
    );
    let mut stream = fixture
        .client
        .get(&endpoint)
        .bearer_auth(TOKEN)
        .header("Acp-Connection-Id", &connection)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), reqwest::StatusCode::OK);
    assert_eq!(stream.headers()["content-type"], "text/event-stream");
    assert_eq!(stream.headers()["acp-connection-id"], connection);
    let response = fixture
        .client
        .post(&endpoint)
        .bearer_auth(TOKEN)
        .header("Acp-Connection-Id", &connection)
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":fixture.root}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let created = next_event(&mut stream, &mut String::new()).await;
    assert_eq!(created["id"], 2);
    let id = created["result"]["sessionId"].as_str().unwrap();
    let mut session = fixture
        .client
        .get(&endpoint)
        .bearer_auth(TOKEN)
        .header("Acp-Connection-Id", &connection)
        .header("Acp-Session-Id", id)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(session.status(), reqwest::StatusCode::OK);
    assert_eq!(session.headers()["acp-session-id"], id);
    assert_eq!(session.headers()["acp-connection-id"], connection);
    assert_eq!(session.headers()["content-type"], "text/event-stream");
    // All HTTP entry points reject missing/wrong bearer before processing the
    // valid connection/session IDs, including steering's mandatory companion.
    let revoke = json!({"jsonrpc":"2.0","id":3,"method":"session/revoke_inject","params":{"sessionId":id,"messageId":"not-pending"}});
    for token in [None, Some("wrong-token")] {
        for method in [
            reqwest::Method::GET,
            reqwest::Method::POST,
            reqwest::Method::DELETE,
        ] {
            let request = fixture
                .client
                .request(method.clone(), &endpoint)
                .header("Acp-Connection-Id", &connection)
                .header("Acp-Session-Id", id);
            let request = if method == reqwest::Method::POST {
                request.json(&revoke)
            } else {
                request
            };
            let request = if let Some(token) = token {
                request.bearer_auth(token)
            } else {
                request
            };
            assert_eq!(
                request.send().await.unwrap().status(),
                reqwest::StatusCode::UNAUTHORIZED
            );
        }
    }
    let response = fixture
        .client
        .post(&endpoint)
        .bearer_auth(TOKEN)
        .header("Acp-Connection-Id", &connection)
        .header("Acp-Session-Id", id)
        .json(&revoke)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    let mut buffer = String::new();
    let rejected = loop {
        let event = next_event(&mut session, &mut buffer).await;
        if event["id"] == 3 {
            break event;
        }
        assert_eq!(event["method"], "session/update");
        assert_eq!(event["params"]["sessionId"], id);
    };
    assert_eq!(rejected["error"]["code"], -32002, "{rejected}");
    assert_eq!(rejected["error"]["data"]["reason"], "unknown_message_id");
    let response = fixture
        .client
        .delete(&endpoint)
        .bearer_auth(TOKEN)
        .header("Acp-Connection-Id", &connection)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::ACCEPTED);
    drop(session);
    drop(stream);
    stop_gateway(&mut gateway).await;
}

fn limit_test_initialize() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"info":{"name":"http-limit-test","version":"0"},"capabilities":{}}})
}

async fn initialize_limit_test_connection(fixture: &Fixture, endpoint: &str) -> String {
    let response = fixture
        .client
        .post(endpoint)
        .bearer_auth(TOKEN)
        .json(&limit_test_initialize())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let connection = response.headers()["acp-connection-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let initialized: Value = response.json().await.unwrap();
    assert_eq!(initialized["result"]["protocolVersion"], 2, "{initialized}");
    connection
}

#[tokio::test]
async fn bundled_catalog_releases_connection_slots_on_every_exit() {
    let fixture = Fixture::new();
    let (mut gateway, url) = fixture.gateway().await;
    // Exceed the simultaneous-connection limit using sequential real bounded
    // clients. Successful CLI exit must await DELETE, not leak logical slots.
    for _ in 0..65 {
        let output = timeout(
            WAIT,
            fixture
                .command()
                .args(["gateway", "list", "--url", &url, "--credential-file"])
                .arg(&fixture.credential)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let catalog: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(catalog["sessions"], json!([]));
    }
    stop_gateway(&mut gateway).await;
}

#[tokio::test]
async fn gateway_http_connection_limit_releases_slot_after_graceful_delete() {
    let fixture = Fixture::new();
    let (mut gateway, url) = fixture.gateway().await;
    let endpoint = format!("{url}/acp/v2");
    // Initialize only: no session actor or inference provider is involved. Consume
    // each response before the next POST so request admission is not the limit.
    let mut connections = Vec::new();
    for _ in 0..64 {
        let connection = initialize_limit_test_connection(&fixture, &endpoint).await;
        assert!(!connections.contains(&connection));
        connections.push(connection);
    }
    let rejected = fixture
        .client
        .post(&endpoint)
        .bearer_auth(TOKEN)
        .json(&limit_test_initialize())
        .send()
        .await
        .unwrap();
    // The bounded SDK reports connection admission exhaustion as explicit 429;
    // the gateway's separate concurrent-request admission uses 503.
    assert_eq!(rejected.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    let deleted = fixture
        .client
        .delete(&endpoint)
        .bearer_auth(TOKEN)
        .header("Acp-Connection-Id", &connections[0])
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), reqwest::StatusCode::ACCEPTED);
    let replacement = initialize_limit_test_connection(&fixture, &endpoint).await;
    assert!(!connections.contains(&replacement));
    // Replacing exactly the released slot fills the current limit again.
    let rejected = fixture
        .client
        .post(&endpoint)
        .bearer_auth(TOKEN)
        .json(&limit_test_initialize())
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
    stop_gateway(&mut gateway).await;
}

#[tokio::test]
async fn gateway_http_rejects_streamed_body_over_one_mib_without_content_length() {
    let fixture = Fixture::new();
    let (mut gateway, url) = fixture.gateway().await;
    let endpoint = format!("{url}/acp/v2");
    // Valid JSON plus legal trailing whitespace isolates byte admission from
    // parsing. An unknown-length reqwest stream uses HTTP/1.1 chunked framing.
    let mut body = serde_json::to_vec(&limit_test_initialize()).unwrap();
    body.resize(1024 * 1024 + 1, b' ');
    let chunks: Vec<_> = body
        .chunks(16 * 1024)
        .map(|chunk| Ok::<_, std::io::Error>(chunk.to_vec()))
        .collect();
    let request = fixture
        .client
        .post(&endpoint)
        .bearer_auth(TOKEN)
        .version(reqwest::Version::HTTP_11)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(reqwest::Body::wrap_stream(futures_util::stream::iter(
            chunks,
        )))
        .build()
        .unwrap();
    assert!(
        !request
            .headers()
            .contains_key(reqwest::header::CONTENT_LENGTH)
    );
    assert_eq!(request.body().unwrap().as_bytes(), None);
    let rejected = fixture.client.execute(request).await.unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    // A rejected unknown-length initialization must not poison other connections.
    let mut unrelated = SdkClient::connect(&url).await;
    let listed = unrelated.request(2, "session/list", json!({})).await;
    assert!(listed.get("error").is_none(), "{listed}");
    assert!(listed.get("result").is_some(), "{listed}");
    unrelated.detach().await;
    stop_gateway(&mut gateway).await;
}

#[tokio::test]
async fn unread_session_mailbox_terminates_http_but_preserves_resident_prompt() {
    static ANSWER: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        format!("{}MAILBOX_SATURATION_COMPLETE", "x".repeat(80 * 64 * 1024))
    });
    let (fixture, mut requests, provider) = controlled_provider().await;
    let (mut gateway, url) = fixture.gateway().await;
    let endpoint = format!("{url}/acp/v2");
    let connection = initialize_limit_test_connection(&fixture, &endpoint).await;
    let mut stream = fixture
        .client
        .get(&endpoint)
        .bearer_auth(TOKEN)
        .header("Acp-Connection-Id", &connection)
        .header("Accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), reqwest::StatusCode::OK);
    let mut buffer = String::new();
    let created = fixture.client.post(&endpoint).bearer_auth(TOKEN)
        .header("Acp-Connection-Id", &connection)
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":fixture.root,"mcpServers":[]}}))
        .send().await.unwrap();
    assert_eq!(created.status(), reqwest::StatusCode::ACCEPTED);
    let created = next_event(&mut stream, &mut buffer).await;
    assert_eq!(created["id"], 2);
    let id = created["result"]["sessionId"].as_str().unwrap();

    // Never open Acp-Session-Id SSE. The real SDK creates the session mailbox
    // when routing its first notification; only a session GET can drain it.
    // Connection SSE remains readable for EOF, with no socket-buffer or
    // scheduler-speed assumption about how slowly a connected reader consumes.
    // The session/prompt reply is session-routed too; the provider request below
    // proves the resident child accepted the turn without draining that reply.
    let accepted = fixture.client.post(&endpoint).bearer_auth(TOKEN)
        .header("Acp-Connection-Id", &connection)
        .json(&json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":id,"prompt":[{"type":"text","text":"Finish despite unread session output"}]}}))
        .send().await.unwrap();
    assert_eq!(accepted.status(), reqwest::StatusCode::ACCEPTED);
    let (_, release) = timeout(WAIT, requests.recv()).await.unwrap().unwrap();
    // More than 4 MiB / 64 frames, but each provider/ACP chunk is only 64 KiB.
    release.send(ANSWER.as_str()).unwrap();
    timeout(WAIT, async {
        while stream.chunk().await.unwrap().is_some() {}
    })
    .await
    .expect("saturated HTTP connection did not reach EOF");
    let rejected = fixture
        .client
        .post(&endpoint)
        .bearer_auth(TOKEN)
        .header("Acp-Connection-Id", &connection)
        .json(&json!({"jsonrpc":"2.0","id":4,"method":"session/list","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::NOT_FOUND);

    // The accepted resident turn must finish durably without a new controller
    // refreshing its lease, even though its original HTTP connection is gone.
    timeout(WAIT, async {
        loop {
            let complete = fs::read_dir(fixture.home.path().join(".kit/sessions"))
                .unwrap()
                .flat_map(|workspace| fs::read_dir(workspace.unwrap().path()).unwrap())
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name() == format!("{id}.jsonl").as_str())
                .any(|entry| {
                    fs::read_to_string(entry.path())
                        .unwrap()
                        .contains(ANSWER.as_str())
                });
            if complete {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("HTTP egress saturation cancelled the accepted resident prompt");

    // Durable content can precede the child's final idle notification. This
    // regression proves transport isolation, not a current-state/no-replay
    // snapshot: do not race a second prompt against uncertain resident state.
    let mut fresh = SdkClient::connect(&url).await;
    let listed = fresh.request(2, "session/list", json!({})).await;
    assert!(
        listed["result"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["sessionId"] == id)
    );
    fresh.detach().await;
    stop_gateway(&mut gateway).await;
    provider.abort();
}
