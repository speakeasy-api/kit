#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
use super::*;
use serde_json::json;
use tempfile::TempDir;

struct Harness {
    _directory: TempDir,
    client: reqwest::Client,
    url: String,
    gateway: Arc<Gateway>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Harness {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let token = directory.path().join("token");
        std::fs::write(&token, "boundary-secret").unwrap();
        let gateway = Arc::new(Gateway {
            stopping: AtomicBool::new(false),
            token: BearerToken::load(&token).unwrap(),
            roots: vec![directory.path().canonicalize().unwrap()],
            sessions: Mutex::new(HashMap::new()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/acp/v2", listener.local_addr().unwrap());
        let app = router(gateway.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            _directory: directory,
            client: reqwest::Client::new(),
            url,
            gateway,
            server,
        }
    }
    async fn raw(&self, value: Value) -> (StatusCode, Value) {
        match handle(
            State(self.gateway.clone()),
            Json(serde_json::from_value(value).unwrap()),
        )
        .await
        {
            Ok(Json(value)) => (StatusCode::OK, value),
            Err(Failure(status, message)) => (status, json!({"error":message})),
        }
    }

    async fn call(&self, value: Value) -> Value {
        let (status, value) = self.raw(value).await;
        assert_eq!(status, StatusCode::OK, "{value}");
        value
    }
    async fn child(&self) {
        // A fake is used at the real ACP subprocess boundary, not a callback in
        // the supervisor. An explicit command releases delayed work deterministically.
        let source = r#"
import sys,json
pending=None
def emit(value):
 print(json.dumps(value),flush=True)
def result(req,value):
 emit({'jsonrpc':'2.0','id':req['id'],'result':value})
for line in sys.stdin:
 req=json.loads(line); method=req['method']
 if method=='initialize': result(req,{'protocolVersion':2,'info':{'name':'fake','version':'1'},'capabilities':{}})
 elif method=='session/new': result(req,{'sessionId':'resident','configOptions':[]})
 elif method=='session/prompt':
  pending=req
  emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'resident','update':{'sessionUpdate':'state_update','state':'running'}}})
  emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'resident','update':{'sessionUpdate':'agent_message','content':[],'id':'started'}}})
 elif method=='session/inject' or method=='session/cancel':
  emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'resident','update':{'sessionUpdate':'state_update','state':'idle'}}})
  emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'resident','update':{'sessionUpdate':'agent_message','content':[],'id':method}}})
  if pending: result(pending,{'stopReason':'cancelled' if method=='session/cancel' else 'end_turn'}); pending=None
 elif 'id' in req: result(req,{})
"#;
        self.child_source("resident", source, &[]).await;
    }
    async fn child_source(&self, id: &str, source: &str, args: &[String]) {
        let child = tokio::process::Command::new("python3")
            .args(["-u", "-c", source])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let actor = Actor {
            id: id.to_owned(),
            root: self.gateway.roots[0].clone(),
            restore: false,
            attachment: None,
            serial: 0,
            pending: HashMap::new(),
            initialized: None,
            session_result: None,
            journal: VecDeque::new(),
            journal_bytes: 0,
            replay_complete: true,
        };
        let (sender, receiver) = mpsc::channel(32);
        let (completion, stopped) = watch::channel(());
        tokio::spawn(async move {
            actor.run(child, receiver).await;
            drop(completion);
        });
        self.gateway.sessions.lock().await.insert(
            id.to_owned(),
            Entry {
                root: self.gateway.roots[0].clone(),
                sender,
                stopped,
            },
        );
    }
    async fn attach(&self) -> String {
        self.call(json!({"op":"attach","session":"resident","replay":true}))
            .await["attachment"]
            .as_str()
            .unwrap()
            .to_owned()
    }
    async fn send(&self, attachment: &str, message: Value) {
        self.call(
            json!({"op":"send","session":"resident","attachment":attachment,"message":message}),
        )
        .await;
    }
    async fn events(&self, attachment: &str, predicate: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let result = self.call(json!({"op":"poll","session":"resident","attachment":attachment,"cursor":0})).await;
                let events:Vec<_> = result["events"].as_array().unwrap().iter().map(|e|e["message"].clone()).collect();
                if predicate(&events) { break events; }
                tokio::task::yield_now().await;
            }
        }).await.unwrap()
    }
    async fn handshake(&self, attachment: &str) {
        self.send(
            attachment,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        )
        .await;
        self.events(attachment, |events| events.iter().any(|e| e["id"] == 1))
            .await;
        self.send(
            attachment,
            json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/ignored"}}),
        )
        .await;
        self.events(attachment, |events| events.iter().any(|e| e["id"] == 2))
            .await;
    }
}
#[tokio::test]
async fn completed_actor_reclamation_keeps_live_replacement() {
    let harness = Harness::new().await;
    let source = "import sys; sys.stdin.read()";
    harness.child_source("resident", source, &[]).await;
    let mut old = harness.gateway.sessions.lock().await["resident"].clone();
    harness.child_source("resident", source, &[]).await;
    let mut replacement = harness.gateway.sessions.lock().await["resident"].clone();
    let (reply, _) = oneshot::channel();
    old.sender
        .send(Envelope {
            request: Request::Shutdown,
            reply,
        })
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), old.stopped.changed())
            .await
            .unwrap()
            .is_err()
    );
    // The old generation completed after replacement. Listing/repeated sweeps
    // must select the current entry, not remove a captured session ID.
    for _ in 0..2 {
        let rows = harness.call(json!({"op":"list"})).await;
        assert_eq!(rows["sessions"][0]["state"], "resident");
        assert!(
            harness.gateway.sessions.lock().await["resident"]
                .sender
                .same_channel(&replacement.sender)
        );
    }
    let (reply, _) = oneshot::channel();
    replacement
        .sender
        .send(Envelope {
            request: Request::Shutdown,
            reply,
        })
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), replacement.stopped.changed())
            .await
            .unwrap()
            .is_err()
    );
    for _ in 0..2 {
        let rows = harness.call(json!({"op":"list"})).await;
        assert_eq!(rows["sessions"], json!([]));
        assert!(harness.gateway.sessions.lock().await.is_empty());
    }
}

#[tokio::test]
async fn closed_command_receiver_does_not_reclaim_unfinished_actor() {
    let harness = Harness::new().await;
    let (sender, receiver) = mpsc::channel(1);
    let (completion, stopped) = watch::channel(());
    drop(receiver);
    harness.gateway.sessions.lock().await.insert(
        "stopping".into(),
        Entry {
            root: harness.gateway.roots[0].clone(),
            sender,
            stopped,
        },
    );
    let rows = harness.call(json!({"op":"list"})).await;
    assert_eq!(rows["sessions"][0]["state"], "exited");
    assert_eq!(harness.gateway.sessions.lock().await.len(), 1);
    drop(completion);
    let rows = harness.call(json!({"op":"list"})).await;
    assert_eq!(rows["sessions"], json!([]));
    assert!(harness.gateway.sessions.lock().await.is_empty());
}

#[tokio::test]
async fn auth_precedes_parsing_and_registration_precedes_spawn() {
    let harness = Harness::new().await;
    for token in [None, Some("wrong")] {
        let mut request = harness.client.post(&harness.url).body("not json");
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let unregistered = tempfile::tempdir().unwrap();
    let (status, _) = harness
        .raw(json!({"op":"create","root":unregistered.path()}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(harness.gateway.sessions.lock().await.is_empty());
    let (status, _) = harness
        .raw(json!({"op":"create","root":harness.gateway.roots[0],"session":"../../elsewhere"}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(harness.gateway.sessions.lock().await.is_empty());
}
#[tokio::test]
async fn disconnect_replay_single_controller_and_stale_response_isolation() {
    let harness = Harness::new().await;
    harness.child().await;
    let first = harness.attach().await;
    harness.handshake(&first).await;
    assert_eq!(
        harness
            .raw(json!({"op":"attach","session":"resident","replay":true}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    harness.send(&first,json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"resident","prompt":[]}})).await;
    harness
        .events(&first, |events| {
            events
                .iter()
                .any(|e| e["params"]["update"]["id"] == "started")
        })
        .await;
    harness
        .call(json!({"op":"detach","session":"resident","attachment":first}))
        .await;
    let listed = harness.call(json!({"op":"list"})).await;
    assert_eq!(listed["sessions"][0]["state"], "resident");
    let second = harness.attach().await;
    harness.handshake(&second).await;
    let replay = harness.events(&second, |_| true).await;
    assert!(
        replay
            .iter()
            .any(|event| event["params"]["update"]["state"] == "running")
    );
    // A second startup on the same attachment must not duplicate replay or
    // create another child session; a close must not be falsely acknowledged.
    for method in ["session/new", "session/close"] {
        let (status, _) = harness
            .raw(json!({"op":"send","session":"resident","attachment":second,
            "message":{"jsonrpc":"2.0","id":90,"method":method,"params":{"sessionId":"resident"}}}))
            .await;
        assert_eq!(
            status,
            if method == "session/new" {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            }
        );
    }
    assert_eq!(harness.raw(json!({"op":"send","session":"resident","attachment":first,"message":{"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"resident"}}})).await.0,StatusCode::CONFLICT);
    harness
        .send(
            &second,
            json!({"jsonrpc":"2.0","method":"session/inject","params":{}}),
        )
        .await;
    let events = harness
        .events(&second, |events| {
            events
                .iter()
                .any(|e| e["params"]["update"]["id"] == "session/inject")
        })
        .await;
    assert!(
        events
            .iter()
            .any(|e| e["params"]["update"]["id"] == "started")
    );
    assert!(
        !events.iter().any(|e| e["id"] == 3),
        "old response must not collide with new attachment request IDs"
    );
    assert!(
        events
            .iter()
            .any(|event| event["params"]["update"]["state"] == "idle")
    );
    // The same unacknowledged cursor can be fetched again without losing events.
    assert_eq!(events, harness.events(&second, |_| true).await);
    harness.send(&second,json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"resident","prompt":[]}})).await;
    harness
        .send(
            &second,
            json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":"resident"}}),
        )
        .await;
    let events = harness
        .events(&second, |events| events.iter().any(|e| e["id"] == 4))
        .await;
    assert!(
        events
            .iter()
            .any(|e| e["id"] == 4 && e["result"]["stopReason"] == "cancelled")
    );
    // Malformed object shapes fail without poisoning the resident actor.
    let (status,_)=harness.raw(json!({"op":"send","session":"resident","attachment":second,"message":{"jsonrpc":"2.0","id":8,"method":"initialize","params":"wrong"}})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    harness
        .send(
            &second,
            json!({"jsonrpc":"2.0","id":9,"method":"initialize","params":{}}),
        )
        .await;
    harness
        .events(&second, |events| events.iter().any(|e| e["id"] == 9))
        .await;
}
#[test]
fn private_bind_requires_explicit_opt_in() {
    assert!(permitted_address("127.0.0.1:0".parse().unwrap(), false));
    assert!(!permitted_address("10.1.2.3:80".parse().unwrap(), false));
    assert!(permitted_address("10.1.2.3:80".parse().unwrap(), true));
    for address in ["0.0.0.0:80", "8.8.8.8:80", "[::]:80"] {
        assert!(!permitted_address(address.parse().unwrap(), true));
    }
}

fn isolated_actor() -> Actor {
    Actor {
        id: "resident".into(),
        root: PathBuf::from("/approved"),
        restore: false,
        attachment: None,
        serial: 0,
        pending: HashMap::new(),
        initialized: None,
        session_result: None,
        journal: VecDeque::new(),
        journal_bytes: 0,
        replay_complete: true,
    }
}

#[test]
fn failed_child_enqueue_does_not_publish_pending_request() {
    let mut actor = isolated_actor();
    let (writes, mut receiver) = mpsc::channel(1);
    let attached = actor
        .command(
            Request::Attach {
                session: actor.id.clone(),
                replace: false,
                replay: true,
            },
            &writes,
        )
        .unwrap();
    let attachment = attached["attachment"].as_str().unwrap().to_owned();
    writes.try_send(json!({"occupied":true})).unwrap();
    let request = || Request::Send {
        session: "resident".into(),
        attachment: attachment.clone(),
        message: json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"resident","prompt":[]}}),
    };
    assert!(actor.command(request(), &writes).is_err());
    assert!(actor.pending.is_empty());
    receiver.try_recv().unwrap();
    actor.command(request(), &writes).unwrap();
    assert_eq!(actor.pending.len(), 1);
    assert_eq!(receiver.try_recv().unwrap()["method"], "session/prompt");
}

#[test]
fn failed_replay_claim_preserves_owner_and_stale_release_preserves_replacement() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    let claim = |replace| Request::Attach {
        session: "resident".into(),
        replace,
        replay: true,
    };
    let first = actor.command(claim(false), &writes).unwrap()["attachment"]
        .as_str()
        .unwrap()
        .to_owned();
    let second = actor.command(claim(true), &writes).unwrap()["attachment"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(first, second);
    assert!(
        actor
            .command(
                Request::Detach {
                    session: actor.id.clone(),
                    attachment: first
                },
                &writes
            )
            .is_err()
    );
    assert_eq!(actor.attachment.as_ref().unwrap().id, second);
    actor.remember(json!({"payload":"x".repeat(MAX_REPLAY + 1)}));
    assert!(!actor.replay_complete);
    assert!(actor.journal_bytes <= MAX_REPLAY);
    assert!(actor.command(claim(true), &writes).is_err());
    assert_eq!(actor.attachment.as_ref().unwrap().id, second);
}

#[tokio::test]
async fn dropped_acceptance_future_does_not_cancel_resident_request() {
    let harness = Harness::new().await;
    harness.child().await;
    let attachment = harness.attach().await;
    harness.handshake(&attachment).await;
    let entry = harness.gateway.sessions.lock().await["resident"].clone();
    let (reply, accepted) = oneshot::channel();
    entry.sender.send(Envelope {
        request: Request::Send {
            session: "resident".into(), attachment: attachment.clone(),
            message: json!({"jsonrpc":"2.0","id":51,"method":"session/prompt","params":{"sessionId":"resident","prompt":[]}}),
        }, reply,
    }).await.unwrap();
    drop(accepted);
    harness
        .events(&attachment, |events| {
            events
                .iter()
                .any(|event| event["params"]["update"]["state"] == "running")
        })
        .await;
    harness
        .send(
            &attachment,
            json!({"jsonrpc":"2.0","method":"session/inject","params":{"sessionId":"resident"}}),
        )
        .await;
    let events = harness
        .events(&attachment, |events| {
            events.iter().any(|event| event["id"] == 51)
        })
        .await;
    assert!(
        events
            .iter()
            .any(|event| event["id"] == 51 && event["result"]["stopReason"] == "end_turn")
    );
}

#[test]
fn transcript_replay_overflow_returns_error_not_truncated_success() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    let attachment = actor
        .command(
            Request::Attach {
                session: actor.id.clone(),
                replace: false,
                replay: true,
            },
            &writes,
        )
        .unwrap()["attachment"]
        .as_str()
        .unwrap()
        .to_owned();
    actor.pending.insert(
        1,
        Pending {
            attachment: attachment.clone(),
            original: json!(42),
            method: "session/resume".into(),
            replay: true,
        },
    );
    actor.remember(json!({"payload":"x".repeat(MAX_REPLAY + 1)}));
    actor.output(json!({"jsonrpc":"2.0","id":1,"result":{"configOptions":[]}}));
    let events = actor
        .command(
            Request::Poll {
                session: actor.id.clone(),
                attachment,
                cursor: 0,
            },
            &writes,
        )
        .unwrap();
    let response = &events["events"][0]["message"];
    assert_eq!(response["id"], 42);
    assert!(response.get("error").is_some());
    assert!(response.get("result").is_none());
    assert!(!actor.attachment.as_ref().unwrap().live);
}

#[test]
fn no_replay_restore_and_claim_survive_incomplete_internal_cache() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    actor.remember(json!({"payload":"x".repeat(MAX_REPLAY + 1)}));
    assert!(!actor.replay_complete);
    let attachment = actor
        .command(
            Request::Attach {
                session: actor.id.clone(),
                replace: true,
                replay: false,
            },
            &writes,
        )
        .unwrap()["attachment"]
        .as_str()
        .unwrap()
        .to_owned();
    actor.pending.insert(
        1,
        Pending {
            attachment: attachment.clone(),
            original: json!(42),
            method: "session/resume".into(),
            replay: false,
        },
    );
    actor.output(json!({"jsonrpc":"2.0","id":1,"result":{"sessionId":"resident"}}));
    let owner = actor.attachment.as_ref().unwrap();
    assert!(owner.live);
    assert_eq!(owner.events.len(), 1);
    assert_eq!(owner.events[0].1["id"], 42);
    assert!(owner.events[0].1.get("error").is_none());
    // A later full-replay claimant still cannot evict this healthy controller.
    assert!(
        actor
            .command(
                Request::Attach {
                    session: actor.id.clone(),
                    replace: true,
                    replay: true,
                },
                &writes
            )
            .is_err()
    );
    assert_eq!(actor.attachment.as_ref().unwrap().id, attachment);
}

#[tokio::test]
async fn raw_sdk_body_limit_covers_declared_and_streamed_lengths() {
    let harness = Harness::new().await;
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"info":{"name":"limit-test","version":"1"},"capabilities":{}}}).to_string();
    let at_limit = format!("{initialize}{}", " ".repeat(1024 * 1024 - initialize.len()));
    let oversized = format!("{at_limit} ");
    for streamed in [false, true] {
        for authorized in [false, true] {
            let body = if streamed {
                let chunks: Vec<_> = oversized
                    .as_bytes()
                    .chunks(65536)
                    .map(|chunk| Ok::<_, io::Error>(chunk.to_vec()))
                    .collect();
                reqwest::Body::wrap_stream(futures_util::stream::iter(chunks))
            } else {
                reqwest::Body::from(oversized.clone())
            };
            let request = harness
                .client
                .post(&harness.url)
                .header("Content-Type", "application/json")
                .body(body);
            let request = if authorized {
                request.bearer_auth("boundary-secret")
            } else {
                request
            };
            assert_eq!(
                request.send().await.unwrap().status(),
                if authorized {
                    StatusCode::PAYLOAD_TOO_LARGE
                } else {
                    StatusCode::UNAUTHORIZED
                },
                "streamed={streamed}, authorized={authorized}"
            );
        }
    }
    // Exactly the public limit still reaches the real SDK and initializes.
    let response = harness
        .client
        .post(&harness.url)
        .bearer_auth("boundary-secret")
        .header("Content-Type", "application/json")
        .body(at_limit)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let connection = response.headers()["acp-connection-id"]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        response.json::<Value>().await.unwrap()["result"]["protocolVersion"],
        2
    );
    assert_eq!(
        harness
            .client
            .delete(&harness.url)
            .bearer_auth("boundary-secret")
            .header("acp-connection-id", &connection)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::ACCEPTED
    );
    for missing in [&connection, "not-a-connection"] {
        assert_eq!(
            harness
                .client
                .delete(&harness.url)
                .bearer_auth("boundary-secret")
                .header("acp-connection-id", missing)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
}

#[tokio::test]
async fn http_delete_drains_accepted_prompt_behind_blocked_initialization() {
    use tokio::io::AsyncReadExt as _;
    async fn event(response: &mut reqwest::Response, buffer: &mut String) -> Value {
        loop {
            if let Some(end) = buffer.find("\n\n") {
                let frame: String = buffer.drain(..end + 2).collect();
                if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) {
                    return serde_json::from_str(data).unwrap();
                }
            } else {
                let chunk = response
                    .chunk()
                    .await
                    .unwrap()
                    .expect("SSE ended before setup");
                buffer.push_str(std::str::from_utf8(&chunk).unwrap());
            }
        }
    }
    // Exercise both ordinary DELETE and cancellation of its HTTP requester.
    // Only the external ACP child boundary is fake; real HTTP/SDK mailboxes,
    // adapter serialization, actor ownership, and teardown all participate.
    for cancel_delete in [false, true] {
        let harness = Harness::new().await;
        harness.child().await;
        let blocked = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        harness
            .child_source(
                "blocked",
                r#"
import sys,json,socket
for line in sys.stdin:
 req=json.loads(line)
 if req['method']=='initialize':
  with socket.create_connection(('127.0.0.1',int(sys.argv[1]))) as gate:
   gate.sendall(b'R')
   assert gate.recv(1)==b'G'
  result={'protocolVersion':2,'info':{'name':'blocked','version':'1'},'capabilities':{}}
 else: result={'sessionId':'blocked','configOptions':[]}
 if 'id' in req: print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}),flush=True)
"#,
                &[blocked.local_addr().unwrap().port().to_string()],
            )
            .await;
        let initialized = harness.client.post(&harness.url).bearer_auth("boundary-secret").json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"info":{"name":"drain-test","version":"1"},"capabilities":{}}})).send().await.unwrap();
        assert_eq!(initialized.status(), StatusCode::OK);
        let connection = initialized.headers()["acp-connection-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let mut stream = harness
            .client
            .get(&harness.url)
            .bearer_auth("boundary-secret")
            .header("acp-connection-id", &connection)
            .header("acp-session-id", "resident")
            .header("accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        let post = |message: Value| {
            harness
                .client
                .post(&harness.url)
                .bearer_auth("boundary-secret")
                .header("acp-connection-id", &connection)
                .json(&message)
        };
        assert_eq!(post(json!({"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"resident","cwd":harness.gateway.roots[0]}})).send().await.unwrap().status(), StatusCode::ACCEPTED);
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut buffer = String::new();
            loop {
                let message = event(&mut stream, &mut buffer).await;
                if message["id"] == 2 {
                    assert!(message.get("error").is_none(), "{message}");
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(post(json!({"jsonrpc":"2.0","id":3,"method":"session/resume","params":{"sessionId":"blocked","cwd":harness.gateway.roots[0]}})).send().await.unwrap().status(), StatusCode::ACCEPTED);
        let (mut release, _) = tokio::time::timeout(Duration::from_secs(5), blocked.accept())
            .await
            .unwrap()
            .unwrap();
        let mut ready = [0];
        release.read_exact(&mut ready).await.unwrap();
        assert_eq!(ready, [b'R']);
        // A receives HTTP 202 while B still prevents adapter dispatch.
        assert_eq!(post(json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"sessionId":"resident","prompt":[]}})).send().await.unwrap().status(), StatusCode::ACCEPTED);
        let delete = harness
            .client
            .delete(&harness.url)
            .bearer_auth("boundary-secret")
            .header("acp-connection-id", &connection);
        let deletion = tokio::spawn(async move { delete.send().await });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let response =
                    post(json!({"jsonrpc":"2.0","method":"$/cancel_request","params":{"id":999}}))
                        .send()
                        .await
                        .unwrap();
                if response.status() == StatusCode::CONFLICT {
                    break;
                }
                assert_eq!(response.status(), StatusCode::ACCEPTED);
            }
        })
        .await
        .expect("DELETE did not close admission while initialization was blocked");
        if cancel_delete {
            deletion.abort();
        }
        release.write_all(b"G").await.unwrap();
        let deleted = tokio::time::timeout(Duration::from_secs(5), deletion)
            .await
            .unwrap();
        if cancel_delete {
            assert!(deleted.unwrap_err().is_cancelled());
        } else {
            assert_eq!(deleted.unwrap().unwrap().status(), StatusCode::ACCEPTED);
        }
        // Cancellation of the HTTP request cannot abandon server teardown.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let response = harness
                    .client
                    .get(&harness.url)
                    .bearer_auth("boundary-secret")
                    .header("acp-connection-id", &connection)
                    .header("accept", "text/event-stream")
                    .send()
                    .await
                    .unwrap();
                if response.status() == StatusCode::NOT_FOUND {
                    break;
                }
            }
        })
        .await
        .expect("SDK connection was not removed after drain");
        let attachment = harness
            .call(json!({"op":"attach","session":"resident","replace":true,"replay":true}))
            .await["attachment"]
            .as_str()
            .unwrap()
            .to_owned();
        harness.handshake(&attachment).await;
        harness
            .events(&attachment, |events| {
                events
                    .iter()
                    .any(|message| message["params"]["update"]["id"] == "started")
            })
            .await;
        // The accepted prompt remains under resident ownership after DELETE.
        harness.send(&attachment, json!({"jsonrpc":"2.0","method":"session/inject","params":{"sessionId":"resident"}})).await;
        harness
            .events(&attachment, |events| {
                events
                    .iter()
                    .any(|message| message["params"]["update"]["state"] == "idle")
            })
            .await;
    }
}
