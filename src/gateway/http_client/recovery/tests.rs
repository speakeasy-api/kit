use super::*;
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Sse},
    routing::post,
};
use serde_json::json;
use std::{
    convert::Infallible,
    io::{BufRead, Write},
};
use tokio::sync::{broadcast, mpsc};

#[test]
fn opaque_http_status_parser_is_anchored_and_never_matches_body_text() {
    for prefix in [
        "bounded HTTP: initialize HTTP ",
        "bounded HTTP: POST HTTP ",
        "bounded HTTP: SSE HTTP ",
    ] {
        let error = agent_client_protocol::Error::internal_error()
            .data(format!("{prefix}401 Unauthorized"));
        assert_eq!(diagnostic_status(&error), Some(401));
    }
    for text in [
        "HTTP 401 Unauthorized",
        "initialize: HTTP 401 Unauthorized: secret",
        "bounded HTTP: initialize HTTP 503 Service Unavailable: HTTP 401 Unauthorized",
        "bounded HTTP: POST HTTP 401 Unauthorizedevil",
        "bounded HTTP: POST HTTP 401 Unauthorized: secret",
        "bounded HTTP: POST HTTP 0401 Unauthorized",
        "body: bounded HTTP: initialize HTTP 401 Unauthorized",
    ] {
        let error = agent_client_protocol::Error::internal_error().data(text);
        assert_eq!(diagnostic_status(&error), None);
    }
}

#[test]
fn metadata_validation_requires_both_snapshot_bits_and_explicit_history_policy() {
    let full = json!({"_meta":{"kit/gateway":{"stateSnapshot":true,"configSnapshot":true,"historyAvailable":true}}});
    assert!(snapshot(&full, false).is_some());
    assert!(snapshot(&full, true).is_none());
    let mut no_replay = full.clone();
    no_replay["_meta"]["kit/gateway"]["historyAvailable"] = false.into();
    assert!(snapshot(&no_replay, true).is_some());
    for key in ["stateSnapshot", "configSnapshot", "historyAvailable"] {
        let mut missing = full.clone();
        missing["_meta"]["kit/gateway"]
            .as_object_mut()
            .unwrap()
            .remove(key);
        assert!(snapshot(&missing, false).is_none());
    }
}

#[test]
fn terminal_classification_uses_structured_metadata_only() {
    assert_eq!(
        terminal_reason(
            &json!({"error":{"message":"controller_replaced","data":{"terminal":false,"reason":"unavailable"}}})
        ),
        None
    );
    assert_eq!(
        terminal_reason(&json!({"error":{"data":{"terminal":true,"reason":"root_denied"}}})),
        Some("root_denied")
    );
    assert_eq!(
        terminal_reason(
            &json!({"method":"session/update","params":{"update":{"sessionUpdate":"_gateway_controller","_meta":{"kit/gateway":{"terminal":true,"reason":"controller_replaced"}}}}})
        ),
        Some("controller_replaced")
    );
}

#[test]
fn batch_rejection_is_once_per_request_and_reserves_internal_ids() {
    let mut output = Vec::new();
    reject(&mut output,&json!([{"id":1,"method":"session/prompt"},{"id":"two","method":"session/set_config_option"},{"method":"session/cancel"}]),"outcome_unknown",&Some("durable".into())).unwrap();
    let output: Vec<Value> = std::str::from_utf8(&output)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["error"]["data"]["reason"], "outcome_unknown");
    assert_eq!(output[1]["error"]["data"]["sessionId"], "durable");
    assert!(reserved(&json!("kit.gateway.internal/2/initialize")));
    assert!(!reserved(&json!(9223372036854775807i64)));
}

struct Server {
    remote: super::super::super::Remote,
    _credential: tempfile::NamedTempFile,
    task: tokio::task::JoinHandle<()>,
    requests: mpsc::UnboundedReceiver<Value>,
    events: broadcast::Sender<(String, Option<Value>)>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(initial_status: Option<StatusCode>) -> Self {
        let (incoming, requests) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel::<(String, Option<Value>)>(128);
        let get_events = events.clone();
        let app = Router::new().route("/acp/v2",post(move |Json(message):Json<Value>| {
            let incoming = incoming.clone();
            async move {
                incoming.send(message.clone()).unwrap();
                if message["method"] == "initialize" {
                    if let Some(status) = initial_status.filter(|_| message["id"].is_number()) { return (status,"secret-response-body").into_response(); }
                    return ([("acp-connection-id","connection")],Json(json!({"jsonrpc":"2.0","id":message["id"],"result":{"protocolVersion":2,"info":{"name":"fault-server","version":"1"},"capabilities":{}}}))).into_response();
                }
                StatusCode::ACCEPTED.into_response()
            }
        }).get(move |headers:HeaderMap| {
            let receiver = get_events.subscribe();
            let scope = headers.get("acp-session-id").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
            async move {
                Sse::new(futures_util::stream::unfold((receiver,scope), |(mut receiver,scope)| async move {
                    loop {
                        let (target,value) = receiver.recv().await.ok()?;
                        if target != scope && target != "*" { continue; }
                        let value = value?;
                        return Some((Ok::<_,Infallible>(axum::response::sse::Event::default().data(value.to_string())),(receiver,scope)));
                    }
                }))
            }
        }).delete(|| async { StatusCode::ACCEPTED }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut credential = tempfile::NamedTempFile::new().unwrap();
        writeln!(credential, "test-token").unwrap();
        Self {
            remote: super::super::super::Remote {
                url: format!("http://{address}"),
                credential_file: credential.path().to_owned(),
                session: None,
                no_replay: false,
            },
            _credential: credential,
            task,
            requests,
            events,
        }
    }
    fn update(&self, update: Value) {
        self.events.send(("durable".into(),Some(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"durable","update":update}})))).unwrap();
    }
    fn result(&self, request: &Value, result: Value, scope: &str) {
        self.events
            .send((
                scope.into(),
                Some(json!({"jsonrpc":"2.0","id":request["id"],"result":result})),
            ))
            .unwrap();
    }
}

// Real OS output stream, not a callback into the state machine. Assertions read
// the same serialized FIFO consumed by the local ACP client in production.
fn output_pipe() -> (
    std::os::unix::net::UnixStream,
    mpsc::UnboundedReceiver<Value>,
) {
    let (writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
    let (send, receive) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        for line in io::BufReader::new(reader).lines() {
            let Ok(line) = line else {
                break;
            };
            if send.send(serde_json::from_str(&line).unwrap()).is_err() {
                break;
            }
        }
    });
    (writer, receive)
}
// Output is visible to an independent ACP peer before flush returns. This
// acknowledged OS pipe deterministically lets that peer enqueue its follow-up
// in that window, without callbacks or instrumentation in production code.
struct AcknowledgedOutput {
    writer: std::os::unix::net::UnixStream,
    consumed: std::sync::mpsc::Receiver<()>,
}
impl Write for AcknowledgedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writer.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.consumed
            .recv_timeout(Duration::from_secs(5))
            .map_err(io::Error::other)
    }
}
fn follow_up_during_publication(
    input: mpsc::Sender<(Instant, io::Result<String>)>,
    on_commit: bool,
    next: Value,
) -> (AcknowledgedOutput, mpsc::UnboundedReceiver<Value>) {
    let (writer, reader) = std::os::unix::net::UnixStream::pair().unwrap();
    let (consumed, acknowledgement) = std::sync::mpsc::channel();
    let (observed, receive) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut follow_up = Some((input, next));
        for line in io::BufReader::new(reader).lines() {
            let value: Value = serde_json::from_str(&line.unwrap()).unwrap();
            let ready = if on_commit {
                value["params"]["_meta"][META]["kind"] == "commit"
            } else {
                value["id"] == 1 && value.get("result").is_some()
            };
            if ready && let Some((input, next)) = follow_up.take() {
                input
                    .blocking_send((Instant::now(), Ok(next.to_string())))
                    .unwrap();
            }
            if observed.send(value).is_err() || consumed.send(()).is_err() {
                break;
            }
        }
    });
    (
        AcknowledgedOutput {
            writer,
            consumed: acknowledgement,
        },
        receive,
    )
}

fn frame(id: u64, method: &str, params: Value) -> (Instant, io::Result<String>) {
    (
        Instant::now(),
        Ok(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}).to_string()),
    )
}
async fn until_id(output: &mut mpsc::UnboundedReceiver<Value>, id: Value) -> Value {
    loop {
        let value = output.recv().await.unwrap();
        if value.get("id") == Some(&id) {
            return value;
        }
    }
}
async fn until_kind(output: &mut mpsc::UnboundedReceiver<Value>, kind: &str) -> Value {
    loop {
        let value = output.recv().await.unwrap();
        if value["params"]["_meta"][META]["kind"] == kind {
            return value;
        }
    }
}

#[tokio::test]
async fn real_http_401_is_terminal_and_does_not_leak_body() {
    let mut server = Server::new(Some(StatusCode::UNAUTHORIZED)).await;
    let (send, lines) = mpsc::channel(16);
    send.send(frame(1, "initialize", initialize_params().unwrap()))
        .await
        .unwrap();
    let mut output = Vec::new();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        run(server.remote.clone(), "/remote".into(), lines, &mut output),
    )
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("HTTP 401"));
    assert!(
        !String::from_utf8(output)
            .unwrap()
            .contains("secret-response-body")
    );
    assert_eq!(
        server.requests.recv().await.unwrap()["method"],
        "initialize"
    );
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn lost_create_response_fails_closed_without_creating_twice() {
    let mut server = Server::new(None).await;
    let remote = server.remote.clone();
    let (send, lines) = mpsc::channel(16);
    let (writer, mut output) = output_pipe();
    let scenario = async {
        send.send(frame(1, "initialize", initialize_params().unwrap()))
            .await
            .unwrap();
        until_id(&mut output, json!(1)).await;
        assert_eq!(
            server.requests.recv().await.unwrap()["method"],
            "initialize"
        );
        send.send(frame(
            2,
            "session/new",
            json!({"cwd":"/local","mcpServers":[]}),
        ))
        .await
        .unwrap();
        let new = server.requests.recv().await.unwrap();
        assert_eq!(new["method"], "session/new");
        server.events.send(("*".into(), None)).unwrap();
        let error = until_id(&mut output, json!(2)).await;
        assert_eq!(error["error"]["data"]["reason"], "outcome_unknown");
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(run(remote, "/remote".into(), lines, writer), scenario)
    })
    .await
    .unwrap();
    assert!(result.unwrap_err().to_string().contains("kit gateway list"));
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn lost_prompt_reinitializes_and_replays_same_session_before_commit() {
    let mut server = Server::new(None).await;
    let remote = server.remote.clone();
    let (send, lines) = mpsc::channel(16);
    let (writer, mut output) = follow_up_during_publication(
        send.clone(),
        true,
        json!({"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{"sessionId":"durable","prompt":[]}}),
    );
    let scenario = async {
        send.send(frame(1, "initialize", initialize_params().unwrap()))
            .await
            .unwrap();
        until_id(&mut output, json!(1)).await;
        server.requests.recv().await.unwrap();
        send.send(frame(
            2,
            "session/new",
            json!({"cwd":"/local","mcpServers":[]}),
        ))
        .await
        .unwrap();
        let new = server.requests.recv().await.unwrap();
        server.result(&new,json!({"sessionId":"durable","configOptions":[],"_meta":{"kit/gateway":{"attachment":"owner-1"}}}),"");
        until_id(&mut output, json!(2)).await;
        send.send(frame(
            3,
            "session/prompt",
            json!({"sessionId":"durable","prompt":[]}),
        ))
        .await
        .unwrap();
        assert_eq!(
            server.requests.recv().await.unwrap()["method"],
            "session/prompt"
        );
        server.events.send(("*".into(), None)).unwrap();
        assert_eq!(
            until_id(&mut output, json!(3)).await["error"]["data"]["reason"],
            "outcome_unknown"
        );
        let begin = until_kind(&mut output, "begin").await;
        send.send(frame(
            4,
            "session/prompt",
            json!({"sessionId":"durable","prompt":[]}),
        ))
        .await
        .unwrap();
        assert_eq!(
            until_id(&mut output, json!(4)).await["error"]["data"]["reason"],
            "not_sent"
        );
        let init = server.requests.recv().await.unwrap();
        assert_eq!(init["method"], "initialize");
        let resume = server.requests.recv().await.unwrap();
        assert_eq!(resume["method"], "session/resume");
        assert_eq!(resume["params"]["sessionId"], "durable");
        assert_eq!(resume["params"]["cwd"], "/remote");
        assert_eq!(resume["params"]["replayFrom"], json!({"type":"start"}));
        let fence = &resume["params"]["_meta"]["kit/gateway"];
        assert_eq!(fence["previousAttachment"], "owner-1");
        assert_eq!(fence["recoveryAttempt"], 1);
        assert_eq!(fence["recoveryId"].as_str().unwrap().len(), 64);
        let recovery_id = fence["recoveryId"].clone();
        // The first resume committed remotely, but its response is lost. The
        // next attempt must retain sequence authority and fence old work.
        server.update(json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"candidate one"}}));
        until_kind(&mut output, "replay").await;
        server.events.send(("*".into(), None)).unwrap();
        let second_begin = until_kind(&mut output, "begin").await;
        assert!(
            second_begin["params"]["_meta"][META]["epoch"]
                .as_u64()
                .unwrap()
                > begin["params"]["_meta"][META]["epoch"].as_u64().unwrap()
        );
        let begin = second_begin;
        assert_eq!(
            server.requests.recv().await.unwrap()["method"],
            "initialize"
        );
        let resume = server.requests.recv().await.unwrap();
        assert_eq!(resume["method"], "session/resume");
        assert_eq!(
            resume["params"]["_meta"]["kit/gateway"]["previousAttachment"],
            "owner-1"
        );
        assert_eq!(
            resume["params"]["_meta"]["kit/gateway"]["recoveryId"],
            recovery_id
        );
        assert_eq!(
            resume["params"]["_meta"]["kit/gateway"]["recoveryAttempt"],
            2
        );
        server.update(json!({"sessionUpdate":"config_option_update","configOptions":[]}));
        server.update(json!({"sessionUpdate":"state_update","state":"idle"}));
        server.result(&resume,json!({"_meta":{"kit/gateway":{"attachment":"owner-2","historyAvailable":true,"stateSnapshot":true,"configSnapshot":true}}}),"durable");
        let config = until_kind(&mut output, "replay").await;
        assert_eq!(
            config["params"]["update"]["sessionUpdate"],
            "config_option_update"
        );
        let state = until_kind(&mut output, "replay").await;
        assert_eq!(state["params"]["update"]["sessionUpdate"], "state_update");
        let commit = until_kind(&mut output, "commit").await;
        assert_eq!(
            commit["params"]["_meta"][META]["epoch"],
            begin["params"]["_meta"][META]["epoch"]
        );
        assert_eq!(commit["params"]["_meta"][META]["historyAvailable"], true);
        let follow_up = tokio::select! {
            request = server.requests.recv() => request.unwrap(),
            response = until_id(&mut output,json!(5)) => panic!("post-commit request rejected: {response}"),
        };
        assert_eq!(follow_up["method"], "session/prompt");
        server.result(&follow_up, json!({"stopReason":"end_turn"}), "durable");
        assert!(
            until_id(&mut output, json!(5))
                .await
                .get("result")
                .is_some()
        );
        drop(send);
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(run(remote, "/remote".into(), lines, writer), scenario)
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn initial_initialize_loss_retries_safely_and_preserves_local_id() {
    let mut server = Server::new(Some(StatusCode::SERVICE_UNAVAILABLE)).await;
    let mut remote = server.remote.clone();
    remote.session = Some("durable".into());
    let (send, lines) = mpsc::channel(16);
    let (writer, mut output) = output_pipe();
    let scenario = async {
        send.send(frame(7, "initialize", initialize_params().unwrap()))
            .await
            .unwrap();
        let result = until_id(&mut output, json!(7)).await;
        assert_eq!(result["result"]["protocolVersion"], 2);
        assert_eq!(server.requests.recv().await.unwrap()["id"], 7);
        let retry = server.requests.recv().await.unwrap();
        assert_eq!(retry["method"], "initialize");
        assert!(reserved(&retry["id"]));
        assert_eq!(retry["params"], initialize_params().unwrap());
        drop(send);
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(run(remote, "/remote".into(), lines, writer), scenario)
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn eof_during_backoff_cancels_recovery_without_another_http_attempt() {
    let mut server = Server::new(None).await;
    let remote = server.remote.clone();
    let (send, lines) = mpsc::channel(16);
    let (writer, mut output) = output_pipe();
    let scenario = async {
        send.send(frame(1, "initialize", initialize_params().unwrap()))
            .await
            .unwrap();
        until_id(&mut output, json!(1)).await;
        server.requests.recv().await.unwrap();
        send.send(frame(
            2,
            "session/new",
            json!({"cwd":"/local","mcpServers":[]}),
        ))
        .await
        .unwrap();
        let new = server.requests.recv().await.unwrap();
        server.result(&new,json!({"sessionId":"durable","configOptions":[],"_meta":{"kit/gateway":{"attachment":"owner-1"}}}),"");
        until_id(&mut output, json!(2)).await;
        send.send(frame(
            3,
            "session/prompt",
            json!({"sessionId":"durable","prompt":[]}),
        ))
        .await
        .unwrap();
        server.requests.recv().await.unwrap();
        server.events.send(("*".into(), None)).unwrap();
        until_kind(&mut output, "begin").await;
        drop(send);
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(run(remote, "/remote".into(), lines, writer), scenario)
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(server.requests.try_recv().is_err());
}

#[tokio::test]
async fn recovered_initialize_accepts_follow_up_before_output_flush_returns() {
    let mut server = Server::new(Some(StatusCode::SERVICE_UNAVAILABLE)).await;
    let remote = server.remote.clone();
    let (send, lines) = mpsc::channel(16);
    let (writer, mut output) = follow_up_during_publication(
        send.clone(),
        false,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/local","mcpServers":[]}}),
    );
    let scenario = async {
        send.send(frame(1, "initialize", initialize_params().unwrap()))
            .await
            .unwrap();
        assert_eq!(server.requests.recv().await.unwrap()["id"], 1);
        assert!(reserved(&server.requests.recv().await.unwrap()["id"]));
        assert_eq!(
            until_id(&mut output, json!(1)).await["result"]["protocolVersion"],
            2
        );
        let new = tokio::select! {
            request = server.requests.recv() => request.unwrap(),
            response = until_id(&mut output,json!(2)) => panic!("post-initialize request rejected: {response}"),
        };
        assert_eq!(new["method"], "session/new");
        server.result(&new,json!({"sessionId":"durable","configOptions":[],"_meta":{"kit/gateway":{"attachment":"owner-1"}}}),"");
        assert_eq!(
            until_id(&mut output, json!(2)).await["result"]["sessionId"],
            "durable"
        );
        drop(send);
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(run(remote, "/remote".into(), lines, writer), scenario)
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(server.requests.try_recv().is_err());
}
