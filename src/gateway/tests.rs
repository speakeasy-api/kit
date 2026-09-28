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
        let child = tokio::process::Command::new("python3")
            .args(["-u", "-c", source])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let actor = Actor {
            id: "resident".into(),
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
            "resident".into(),
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
