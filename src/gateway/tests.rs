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
    app: Router,
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
        let app = router(gateway.clone()).unwrap();
        let served_app = app.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, served_app).await.unwrap();
        });
        Self {
            _directory: directory,
            client: reqwest::Client::new(),
            url,
            gateway,
            app,
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
            Err(Failure(status, message, _)) => (status, json!({"error":message})),
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
            committed_owner: None,
            serial: 0,
            pending: HashMap::new(),
            initialized: None,
            session_result: None,
            state: json!({"sessionUpdate":"state_update","state":"idle"}),
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
        committed_owner: None,
        serial: 0,
        pending: HashMap::new(),
        initialized: None,
        session_result: None,
        state: json!({"sessionUpdate":"state_update","state":"idle"}),
        journal: VecDeque::new(),
        journal_bytes: 0,
        replay_complete: true,
    }
}

#[tokio::test]
async fn accepted_child_write_retains_charge_after_dequeue_until_consumed() {
    use agent_client_protocol::{BoundedChannel, ChannelLimits, TransportFrame};
    use futures_util::StreamExt;

    for release_write in [false, true] {
        let (producer, mut incoming) = BoundedChannel::duplex(ChannelLimits {
            max_buffered_frames: 1,
            ..ChannelLimits::default()
        })
        .unwrap();
        let message = json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"resident","prompt":[]}});
        producer
            .tx
            .try_send(TransportFrame::parse_json(&message.to_string()))
            .unwrap();
        let charge = Arc::new(incoming.rx.next().await.unwrap());
        let mut actor = isolated_actor();
        let (writes, mut receiver) = mpsc::channel(1);
        let attached = actor
            .command(
                Request::Attach {
                    startup: None,
                    session: actor.id.clone(),
                    replace: false,
                    replay: true,
                },
                &writes,
            )
            .unwrap();
        actor
            .command(
                Request::Send {
                    session: actor.id.clone(),
                    attachment: attached["attachment"].as_str().unwrap().into(),
                    message: message.clone(),
                    charge: Some(charge),
                },
                &writes,
            )
            .unwrap();
        // HTTP-side ownership is gone. Dequeue alone must not return capacity:
        // the child writer still has to serialize and write this payload.
        let mut writing = Some(receiver.try_recv().unwrap());
        if release_write {
            drop(writing.take());
        }
        let admitted = producer
            .tx
            .try_send(TransportFrame::parse_json(&message.to_string()));
        assert_eq!(admitted.is_ok(), release_write);
        drop(writing);
    }
}

#[test]
fn failed_child_enqueue_does_not_publish_pending_request() {
    let mut actor = isolated_actor();
    let (writes, mut receiver) = mpsc::channel(1);
    let attached = actor
        .command(
            Request::Attach {
                startup: None,
                session: actor.id.clone(),
                replace: false,
                replay: true,
            },
            &writes,
        )
        .unwrap();
    let attachment = attached["attachment"].as_str().unwrap().to_owned();
    writes
        .try_send(ChildWrite {
            message: json!({"occupied":true}),
            _charge: None,
        })
        .unwrap();
    let request = || Request::Send {
        charge: None,
        session: "resident".into(),
        attachment: attachment.clone(),
        message: json!({"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"sessionId":"resident","prompt":[]}}),
    };
    assert!(actor.command(request(), &writes).is_err());
    assert!(actor.pending.is_empty());
    receiver.try_recv().unwrap();
    actor.command(request(), &writes).unwrap();
    assert_eq!(actor.pending.len(), 1);
    assert_eq!(
        receiver.try_recv().unwrap().message["method"],
        "session/prompt"
    );
}

#[test]
fn failed_replay_claim_preserves_owner_and_stale_release_preserves_replacement() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    let claim = |replace| Request::Attach {
        startup: None,
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
            charge: None,
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
                startup: None,
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
                startup: None,
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
    assert_eq!(owner.events.len(), 3);
    assert_eq!(owner.events[2].1["id"], 42);
    assert!(owner.events[2].1.get("error").is_none());
    assert_eq!(
        owner.events[2].1["result"]["_meta"]["kit/gateway"]["historyAvailable"],
        false
    );
    // A later full-replay claimant still cannot evict this healthy controller.
    assert!(
        actor
            .command(
                Request::Attach {
                    startup: None,
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
    // Exercise early responses through the real router. A network client still
    // writing a rejected body can observe TCP reset instead of the already-sent
    // 401/413; that socket race is not this admission assertion. Integration
    // coverage separately sends an unknown-length oversized body over HTTP.
    use axum::body::Body;
    use tower::ServiceExt as _;
    let app = router(harness.gateway.clone()).unwrap();
    for streamed in [false, true] {
        for authorized in [false, true] {
            let body = if streamed {
                let chunks: Vec<_> = oversized
                    .as_bytes()
                    .chunks(65536)
                    .map(|chunk| Ok::<_, io::Error>(chunk.to_vec()))
                    .collect();
                Body::from_stream(futures_util::stream::iter(chunks))
            } else {
                Body::from(oversized.clone())
            };
            let mut request = axum::http::Request::builder()
                .method("POST")
                .uri("/acp/v2")
                .header("Content-Type", "application/json");
            if !streamed {
                request = request.header("Content-Length", oversized.len());
            }
            if authorized {
                request = request.header("Authorization", "Bearer boundary-secret");
            }
            let response = app
                .clone()
                .oneshot(request.body(body).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
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
    use axum::body::{Body, Bytes};
    use std::{future::Future as _, task::Poll};
    use tokio::io::AsyncReadExt as _;
    use tower::ServiceExt as _;
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
        // Initialize the competing connections before reserving all eight MiB
        // of the SDK's global body budget. Both paths share this exact router.
        let mut other_connections = Vec::new();
        for _ in 0..8 {
            let response = harness.client.post(&harness.url).bearer_auth("boundary-secret").json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":2,"info":{"name":"body-holder","version":"1"},"capabilities":{}}})).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            other_connections.push(
                response.headers()["acp-connection-id"]
                    .to_str()
                    .unwrap()
                    .to_owned(),
            );
            let _ = response.bytes().await.unwrap();
        }
        let request = |method: &str, connection: &str, body: Body| {
            axum::http::Request::builder()
                .method(method)
                .uri("/acp/v2")
                .header("Authorization", "Bearer boundary-secret")
                .header("Content-Type", "application/json")
                .header("acp-connection-id", connection)
                .body(body)
                .unwrap()
        };
        let mut bodies = tokio::task::JoinSet::new();
        for other in &other_connections {
            let (ready, polled) = tokio::sync::oneshot::channel();
            let mut ready = Some(ready);
            let body = Body::from_stream(futures_util::stream::poll_fn(move |_| {
                if let Some(ready) = ready.take() {
                    ready.send(()).unwrap();
                }
                Poll::<Option<Result<Bytes, io::Error>>>::Pending
            }));
            bodies.spawn(harness.app.clone().oneshot(request("POST", other, body)));
            // A body is polled only after the SDK holds its 1 MiB reservation.
            tokio::time::timeout(Duration::from_secs(5), polled)
                .await
                .expect("SDK did not poll the reserved body")
                .unwrap();
        }
        let unpollable_body = || {
            Body::from_stream(futures_util::stream::poll_fn(
                |_| -> Poll<Option<Result<Bytes, io::Error>>> {
                    panic!("rejected POST must not poll its body")
                },
            ))
        };
        let response = harness
            .app
            .clone()
            .oneshot(request("POST", &other_connections[0], unpollable_body()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

        let mut deletion = Box::pin(harness.app.clone().oneshot(request(
            "DELETE",
            &connection,
            Body::empty(),
        )));
        // Poll the real service once: DELETE must seal synchronously, without a
        // body/core reservation, then wait for the blocked adapter to drain.
        std::future::poll_fn(|cx| {
            assert!(deletion.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        let response = harness
            .app
            .clone()
            .oneshot(request("POST", &connection, unpollable_body()))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::GONE,
            "sealed admission must precede the exhausted body budget"
        );
        let deletion = if cancel_delete {
            drop(deletion);
            None
        } else {
            Some(deletion)
        };
        release.write_all(b"G").await.unwrap();
        if let Some(deletion) = deletion {
            let deleted = tokio::time::timeout(Duration::from_secs(5), deletion)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(deleted.status(), StatusCode::ACCEPTED);
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
        // Keep every body reservation alive through teardown and actor checks.
        bodies.abort_all();
        while let Some(result) = bodies.join_next().await {
            assert!(result.unwrap_err().is_cancelled());
        }
    }
}

fn resident_claim(actor: &Actor, replay: bool, id: u64) -> Request {
    Request::Attach {
        session: actor.id.clone(),
        replace: true,
        replay,
        startup: Some(
            json!({"jsonrpc":"2.0","id":id,"method":"session/resume","params":{"sessionId":actor.id}}),
        ),
    }
}

fn state_update(state: &str) -> Value {
    json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"resident","update":{"sessionUpdate":"state_update","state":state}}})
}

#[test]
fn resident_snapshot_follows_replay_and_precedes_success() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    actor.session_result = Some(json!({"configOptions":[]}));
    actor.output(state_update("running"));
    actor.output(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"resident","update":{"sessionUpdate":"agent_message","id":"done","content":[]}}}));
    // Assistant output is not evidence that foreground work stopped.
    actor
        .command(resident_claim(&actor, true, 50), &writes)
        .unwrap();
    let events: Vec<_> = actor
        .attachment
        .as_ref()
        .unwrap()
        .events
        .iter()
        .map(|(_, m)| m)
        .collect();
    assert_eq!(events.len(), 5);
    assert_eq!(events[1]["params"]["update"]["id"], "done");
    assert_eq!(
        events[2]["params"]["update"]["sessionUpdate"],
        "config_option_update"
    );
    assert_eq!(events[3]["params"]["update"]["state"], "running");
    assert_eq!(events[4]["id"], 50);
    assert_eq!(
        events[4]["result"]["_meta"]["kit/gateway"],
        json!({"historyAvailable":true,"stateSnapshot":true,"configSnapshot":true,"attachment":actor.attachment.as_ref().unwrap().id})
    );
    // Validate snapshots using the actual ACP v2 SDK, not an invented shape.
    for event in &events[2..4] {
        serde_json::from_value::<agent_client_protocol::schema::v2::UpdateSessionNotification>(
            event["params"].clone(),
        )
        .unwrap();
    }
}

#[test]
fn no_replay_snapshot_retains_running_state_and_current_config_after_eviction() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    actor.session_result = Some(json!({"configOptions":[]}));
    actor.output(state_update("running"));
    let options = json!([{"id":"model","name":"Model","type":"select","currentValue":"new","options":[{"value":"new","name":"New"}]}]);
    actor.output(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"resident","update":{"sessionUpdate":"config_option_update","configOptions":options}}}));
    actor.remember(json!({"payload":"x".repeat(MAX_REPLAY+1)}));
    actor
        .command(resident_claim(&actor, false, 51), &writes)
        .unwrap();
    let owner = actor.attachment.as_ref().unwrap();
    assert_eq!(owner.events.len(), 3);
    assert_eq!(
        owner.events[0].1["params"]["update"]["configOptions"],
        options
    );
    assert_eq!(owner.events[1].1["params"]["update"]["state"], "running");
    assert_eq!(
        owner.events[2].1["result"]["_meta"]["kit/gateway"]["historyAvailable"],
        false
    );
    actor.output(state_update("idle"));
    assert_eq!(
        actor.attachment.as_ref().unwrap().events.back().unwrap().1["params"]["update"]["state"],
        "idle"
    );
}

#[test]
fn failed_resident_snapshot_preserves_owner_and_success_fences_old_generation() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    actor.session_result = Some(json!({"configOptions":[]}));
    actor
        .command(resident_claim(&actor, false, 60), &writes)
        .unwrap();
    let old = actor.attachment.as_ref().unwrap().id.clone();
    // Journal itself is below aggregate limits, but cannot cross HTTP as one frame.
    actor.remember(json!({"payload":"x".repeat(MAX_HTTP_FRAME)}));
    assert!(actor.replay_complete);
    let error = actor
        .command(resident_claim(&actor, true, 61), &writes)
        .unwrap_err();
    assert_eq!(error.2, "replay_unavailable");
    assert_eq!(actor.attachment.as_ref().unwrap().id, old);
    assert_eq!(
        actor.attachment.as_ref().unwrap().events.back().unwrap().1["id"],
        60
    );
    actor.pending.insert(
        9,
        Pending {
            attachment: old.clone(),
            original: json!(99),
            method: "session/prompt".into(),
            replay: false,
        },
    );
    actor
        .command(resident_claim(&actor, false, 62), &writes)
        .unwrap();
    actor.output(json!({"jsonrpc":"2.0","id":9,"result":{"stopReason":"end_turn"}}));
    let current = actor.attachment.as_ref().unwrap().id.clone();
    assert_ne!(old, current);
    assert_eq!(actor.attachment.as_ref().unwrap().events.len(), 3);
    let error = actor
        .command(
            Request::Detach {
                session: actor.id.clone(),
                attachment: old,
            },
            &writes,
        )
        .unwrap_err();
    assert_eq!(error.2, "controller_replaced");
    assert_eq!(actor.attachment.as_ref().unwrap().id, current);
}

#[test]
fn automatic_resume_is_conditioned_on_previous_controller_generation() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    actor.session_result = Some(json!({"configOptions":[]}));
    actor
        .command(resident_claim(&actor, false, 70), &writes)
        .unwrap();
    let old = actor.attachment.as_ref().unwrap().id.clone();
    let mut recovery = resident_claim(&actor, false, 72);
    if let Request::Attach {
        startup: Some(message),
        ..
    } = &mut recovery
    {
        message["params"]["_meta"]["kit/gateway"]["previousAttachment"] = old.clone().into();
    }
    // A separate explicit manual claim has replaced the transport being recovered.
    actor
        .command(resident_claim(&actor, false, 71), &writes)
        .unwrap();
    let current = actor.attachment.as_ref().unwrap().id.clone();
    let error = actor.command(recovery, &writes).unwrap_err();
    assert_eq!(error.2, "controller_replaced");
    assert_eq!(actor.attachment.as_ref().unwrap().id, current);
    assert_eq!(
        actor.attachment.as_ref().unwrap().events.back().unwrap().1["id"],
        71
    );
    let mut recovery = resident_claim(&actor, false, 73);
    if let Request::Attach {
        startup: Some(message),
        ..
    } = &mut recovery
    {
        message["params"]["_meta"]["kit/gateway"]["previousAttachment"] = current.clone().into();
    }
    actor.command(recovery, &writes).unwrap();
    assert_ne!(actor.attachment.as_ref().unwrap().id, current);
    assert_eq!(
        actor.attachment.as_ref().unwrap().events.back().unwrap().1["result"]["_meta"]["kit/gateway"]
            ["attachment"],
        actor.attachment.as_ref().unwrap().id
    );
    let newest = actor.attachment.as_ref().unwrap().id.clone();
    actor
        .command(
            Request::Detach {
                session: actor.id.clone(),
                attachment: newest.clone(),
            },
            &writes,
        )
        .unwrap();
    let mut recovery = resident_claim(&actor, false, 74);
    if let Request::Attach {
        startup: Some(message),
        ..
    } = &mut recovery
    {
        message["params"]["_meta"]["kit/gateway"]["previousAttachment"] = newest.into();
    }
    actor.command(recovery, &writes).unwrap();
}

#[test]
fn recovery_sequence_retries_own_unconfirmed_claim_but_not_manual_replacement() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    actor.session_result = Some(json!({"configOptions":[]}));
    actor
        .command(resident_claim(&actor, false, 80), &writes)
        .unwrap();
    let old = actor.attachment.as_ref().unwrap().id.clone();
    let recovery = |actor: &Actor, id| {
        let mut request = resident_claim(actor, false, id);
        if let Request::Attach {
            startup: Some(message),
            ..
        } = &mut request
        {
            message["params"]["_meta"]["kit/gateway"] = json!({"previousAttachment":old,"recoveryId":"random-per-outage","recoveryAttempt":id});
        }
        request
    };
    actor.command(recovery(&actor, 81), &writes).unwrap();
    let unconfirmed = actor.attachment.as_ref().unwrap().id.clone();
    // The previous result was lost; reuse the same outage ID and old confirmed token.
    actor.command(recovery(&actor, 82), &writes).unwrap();
    assert_ne!(actor.attachment.as_ref().unwrap().id, unconfirmed);
    actor
        .command(resident_claim(&actor, false, 83), &writes)
        .unwrap();
    let manual = actor.attachment.as_ref().unwrap().id.clone();
    assert_eq!(
        actor.command(recovery(&actor, 84), &writes).unwrap_err().2,
        "controller_replaced"
    );
    assert_eq!(actor.attachment.as_ref().unwrap().id, manual);
    // Invalid recovery capabilities cannot mutate ownership either.
    for metadata in [
        json!({"recoveryId":"orphan"}),
        json!({"previousAttachment":manual,"recoveryId":""}),
        json!({"previousAttachment":manual,"recoveryId":"x".repeat(129)}),
    ] {
        let mut request = resident_claim(&actor, false, 85);
        if let Request::Attach {
            startup: Some(message),
            ..
        } = &mut request
        {
            message["params"]["_meta"]["kit/gateway"] = metadata;
        }
        assert_eq!(actor.command(request, &writes).unwrap_err().2, "protocol");
        assert_eq!(actor.attachment.as_ref().unwrap().id, manual);
    }
}

#[test]
fn stale_config_response_refreshes_snapshot_without_cross_generation_response() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    actor.session_result = Some(json!({"configOptions":[]}));
    actor
        .command(resident_claim(&actor, false, 90), &writes)
        .unwrap();
    let old = actor.attachment.as_ref().unwrap().id.clone();
    actor.pending.insert(
        1,
        Pending {
            attachment: old,
            original: json!(900),
            method: "session/set_config_option".into(),
            replay: false,
        },
    );
    actor
        .command(resident_claim(&actor, false, 91), &writes)
        .unwrap();
    let options = json!([{"id":"model","name":"Model","type":"select","currentValue":"updated","options":[{"value":"updated","name":"Updated"}]}]);
    actor.output(json!({"jsonrpc":"2.0","id":1,"result":{"configOptions":options}}));
    assert_eq!(actor.attachment.as_ref().unwrap().events.len(), 3);
    actor
        .command(resident_claim(&actor, false, 92), &writes)
        .unwrap();
    assert_eq!(
        actor.attachment.as_ref().unwrap().events[0].1["params"]["update"]["configOptions"],
        options
    );
}

#[test]
fn aggregate_snapshot_overflow_preserves_prior_controller() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    actor.session_result = Some(json!({"configOptions":[]}));
    actor
        .command(resident_claim(&actor, false, 93), &writes)
        .unwrap();
    let old = actor.attachment.as_ref().unwrap().id.clone();
    // Every frame fits, and the journal alone fits, but trailing snapshots do not.
    let frame = json!({"payload":"x".repeat(MAX_HTTP_FRAME / 2 - 100)});
    for _ in 0..16 {
        actor.remember(frame.clone());
    }
    let remaining = MAX_REPLAY - actor.journal_bytes;
    actor.remember(json!({"payload":"x".repeat(remaining - 14)}));
    assert!(actor.replay_complete);
    assert!(actor.journal_bytes <= MAX_REPLAY);
    assert_eq!(
        actor
            .command(resident_claim(&actor, true, 94), &writes)
            .unwrap_err()
            .2,
        "replay_unavailable"
    );
    assert_eq!(actor.attachment.as_ref().unwrap().id, old);
}

#[test]
fn recovery_attempt_high_water_mark_fences_delayed_and_duplicate_claims() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    actor.session_result = Some(json!({"configOptions":[]}));
    actor
        .command(resident_claim(&actor, false, 100), &writes)
        .unwrap();
    let original = actor.attachment.as_ref().unwrap().id.clone();
    let recovery = |actor: &Actor, previous: &str, attempt: u64| {
        let mut request = resident_claim(actor, false, 100 + attempt);
        if let Request::Attach {
            startup: Some(message),
            ..
        } = &mut request
        {
            message["params"]["_meta"]["kit/gateway"] = json!({
                "previousAttachment":previous,"recoveryId":"ordered-outage","recoveryAttempt":attempt
            });
        }
        request
    };
    // Attempt 1 was delayed before mailbox admission; attempt 2 commits first.
    let late = recovery(&actor, &original, 1);
    actor
        .command(recovery(&actor, &original, 2), &writes)
        .unwrap();
    let second = actor.attachment.as_ref().unwrap().id.clone();
    let queued = actor.attachment.as_ref().unwrap().events.clone();
    assert_eq!(
        actor.command(late, &writes).unwrap_err().2,
        "stale_recovery"
    );
    assert_eq!(actor.attachment.as_ref().unwrap().id, second);
    assert_eq!(actor.attachment.as_ref().unwrap().events, queued);
    // Even the current attachment ID must not bypass duplicate/older fencing.
    for attempt in [1, 2] {
        let error = actor
            .command(recovery(&actor, &second, attempt), &writes)
            .unwrap_err();
        assert_eq!(
            error.data(),
            json!({"reason":"stale_recovery","terminal":true})
        );
        assert_eq!(actor.attachment.as_ref().unwrap().events, queued);
    }
    actor
        .command(recovery(&actor, &original, 3), &writes)
        .unwrap();
    assert_ne!(actor.attachment.as_ref().unwrap().id, second);
    assert_eq!(
        actor.committed_owner.as_ref().unwrap().recovery_attempt,
        Some(3)
    );
    actor
        .command(resident_claim(&actor, false, 104), &writes)
        .unwrap();
    let manual = actor.attachment.as_ref().unwrap().id.clone();
    assert!(
        actor
            .committed_owner
            .as_ref()
            .unwrap()
            .recovery_id
            .is_none()
    );
    assert!(
        actor
            .committed_owner
            .as_ref()
            .unwrap()
            .recovery_attempt
            .is_none()
    );
    assert_eq!(
        actor
            .command(recovery(&actor, &original, 4), &writes)
            .unwrap_err()
            .2,
        "controller_replaced"
    );
    assert_eq!(actor.attachment.as_ref().unwrap().id, manual);
}

#[test]
fn recovery_attempt_metadata_is_paired_positive_and_bounded() {
    let mut actor = isolated_actor();
    let (writes, _receiver) = mpsc::channel(1);
    actor.session_result = Some(json!({"configOptions":[]}));
    actor
        .command(resident_claim(&actor, false, 110), &writes)
        .unwrap();
    let owner = actor.attachment.as_ref().unwrap().id.clone();
    let mut invalid = vec![
        json!({"previousAttachment":owner,"recoveryId":"missing-attempt"}),
        json!({"previousAttachment":owner,"recoveryAttempt":1}),
        json!({"recoveryId":"missing-predecessor","recoveryAttempt":1}),
    ];
    for attempt in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!("1"),
        Value::Null,
        json!(18446744073709551616_f64),
    ] {
        invalid.push(json!({"previousAttachment":owner,"recoveryId":"invalid-attempt","recoveryAttempt":attempt}));
    }
    for metadata in invalid {
        let mut request = resident_claim(&actor, false, 111);
        if let Request::Attach {
            startup: Some(message),
            ..
        } = &mut request
        {
            message["params"]["_meta"]["kit/gateway"] = metadata;
        }
        assert_eq!(actor.command(request, &writes).unwrap_err().2, "protocol");
        assert_eq!(actor.attachment.as_ref().unwrap().id, owner);
        assert!(
            actor
                .committed_owner
                .as_ref()
                .unwrap()
                .recovery_attempt
                .is_none()
        );
    }
}

fn recovery_claim(actor: &Actor, previous: &str, outage: &str, attempt: u64) -> Request {
    let mut request = resident_claim(actor, false, 200 + attempt);
    if let Request::Attach {
        startup: Some(message),
        ..
    } = &mut request
    {
        message["params"]["_meta"]["kit/gateway"] = json!({
            "previousAttachment": previous, "recoveryId": outage, "recoveryAttempt": attempt
        });
    }
    request
}

#[test]
fn detached_owner_fences_replaced_controller_and_preserves_recovery_high_water_mark() {
    for overflow in [false, true] {
        let mut actor = isolated_actor();
        let (writes, _receiver) = mpsc::channel(1);
        actor.session_result = Some(json!({"configOptions":[]}));
        actor
            .command(resident_claim(&actor, false, 200), &writes)
            .unwrap();
        let a = actor.attachment.as_ref().unwrap().id.clone();
        actor
            .command(resident_claim(&actor, false, 201), &writes)
            .unwrap();
        let b = actor.attachment.as_ref().unwrap().id.clone();
        if overflow {
            actor.emit(json!({"payload":"x".repeat(MAX_REPLAY)}));
        } else {
            actor
                .command(
                    Request::Detach {
                        session: actor.id.clone(),
                        attachment: b.clone(),
                    },
                    &writes,
                )
                .unwrap();
        }
        assert!(actor.attachment.is_none());
        assert_eq!(
            actor
                .command(recovery_claim(&actor, &a, "a-outage", 1), &writes)
                .unwrap_err()
                .2,
            "controller_replaced"
        );
        assert!(actor.attachment.is_none());

        // Failed preparation of a manual replacement must not erase B's fence.
        let mut invalid = resident_claim(&actor, false, 202);
        if let Request::Attach {
            startup: Some(message),
            ..
        } = &mut invalid
        {
            message.as_object_mut().unwrap().remove("id");
        }
        assert!(actor.command(invalid, &writes).is_err());
        assert_eq!(actor.committed_owner.as_ref().unwrap().id, b);
        assert_eq!(
            actor
                .command(recovery_claim(&actor, &a, "a-outage", 2), &writes)
                .unwrap_err()
                .2,
            "controller_replaced"
        );
        actor
            .command(recovery_claim(&actor, &b, "b-outage", 2), &writes)
            .unwrap();
        let recovered = actor.attachment.as_ref().unwrap().id.clone();
        actor
            .command(
                Request::Detach {
                    session: actor.id.clone(),
                    attachment: recovered,
                },
                &writes,
            )
            .unwrap();
        for attempt in [1, 2] {
            assert_eq!(
                actor
                    .command(recovery_claim(&actor, &b, "b-outage", attempt), &writes)
                    .unwrap_err()
                    .2,
                "stale_recovery"
            );
            assert!(actor.attachment.is_none());
        }
        // Lost recovery result: original B token is still valid for the same outage.
        actor
            .command(recovery_claim(&actor, &b, "b-outage", 3), &writes)
            .unwrap();
        assert_eq!(
            actor.committed_owner.as_ref().unwrap().recovery_attempt,
            Some(3)
        );
    }
}

#[tokio::test]
async fn cancelled_detach_reply_keeps_committed_generation_fence() {
    let harness = Harness::new().await;
    harness.child().await;
    let a = harness.attach().await;
    harness.handshake(&a).await;
    let b = harness
        .call(json!({"op":"attach","session":"resident","replace":true,"replay":false}))
        .await["attachment"]
        .as_str()
        .unwrap()
        .to_owned();
    let entry = harness.gateway.sessions.lock().await["resident"].clone();
    let (reply, accepted) = oneshot::channel();
    entry
        .sender
        .send(Envelope {
            request: Request::Detach {
                session: "resident".into(),
                attachment: b.clone(),
            },
            reply,
        })
        .await
        .unwrap();
    drop(accepted);
    // FIFO mailbox order makes cancellation deterministic, without sleeps.
    for (previous, expected) in [(&a, StatusCode::CONFLICT), (&b, StatusCode::OK)] {
        let (status, _) = harness.raw(json!({"op":"attach","session":"resident","replace":true,"replay":false,
            "startup":{"jsonrpc":"2.0","id":205,"method":"session/resume","params":{"sessionId":"resident","_meta":{"kit/gateway":{"previousAttachment":previous,"recoveryId":"cancelled-detach","recoveryAttempt":1}}}}
        })).await;
        assert_eq!(status, expected);
    }
}
