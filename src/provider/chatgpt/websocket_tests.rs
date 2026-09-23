//! Real loopback API-boundary tests: no credential storage or live inference.
//! Gated on `tui` to reuse its optional tungstenite dependency. Headless tests
//! intentionally omit WebSocket runtime coverage (no additional dependencies).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
use super::*;
use crate::provider::adapter::{KitTurn, ReasoningEffort};
use agentkit_core::{Delta, Item, SessionId, TurnId};
use serde_json::json;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::Instant,
};
use tokio_tungstenite::tungstenite::{self, Message, WebSocket};

const WAIT: Duration = Duration::from_secs(5);
type Socket = WebSocket<TcpStream>;

fn server(run: impl FnOnce(TcpListener) + Send + 'static) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!(
        "http://{}/backend-api/codex/responses",
        listener.local_addr().unwrap()
    );
    listener.set_nonblocking(true).unwrap();
    (endpoint, thread::spawn(move || run(listener)))
}

fn accept(listener: &TcpListener) -> TcpStream {
    let start = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(WAIT)).unwrap();
                stream.set_write_timeout(Some(WAIT)).unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(start.elapsed() < WAIT, "missing expected connection");
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("accept: {error}"),
        }
    }
}

async fn session(endpoint: &str) -> OpenAiSubscriptionSession {
    // Exercise Kit's real transport selection and request policy; override only
    // the destination, dummy authentication, and bounded test timeouts.
    let config = subscription_responses_config(
        "gpt-5.4".into(),
        Authentication::bearer("loopback-only"),
        ResilienceConfig {
            max_retries: 0,
            retry_budget: WAIT,
            attempt_timeout: Some(WAIT),
            stream_idle_timeout: Some(WAIT),
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
        },
        Some(ReasoningEffort::High),
    )
    .with_endpoint(endpoint);
    let adapter = OpenAIResponsesAdapter::new(config).unwrap();
    OpenAiSubscriptionSession {
        inner: adapter
            .start_session(SessionConfig::new("session"))
            .await
            .unwrap(),
        context_window: Some(200_000),
        authentication_binding: "loopback-only".into(),
    }
}

fn request(second: bool) -> TurnRequest {
    let mut transcript = vec![Item::text(ItemKind::User, "first question")];
    if second {
        transcript.push(Item::text(ItemKind::Assistant, "first answer"));
        transcript.push(Item::text(ItemKind::User, "second question"));
    }
    TurnRequest {
        session_id: SessionId::new("session"),
        turn_id: TurnId::new(if second { "second" } else { "first" }),
        transcript,
        available_tools: vec![],
        cache: None,
        metadata: MetadataMap::new(),
    }
}

async fn begin(session: &mut OpenAiSubscriptionSession, second: bool) -> KitTurn {
    let turn = tokio::time::timeout(WAIT, session.begin_turn(request(second), None))
        .await
        .expect("begin turn hung")
        .unwrap();
    KitTurn::OpenAiSubscription(Box::new(turn))
}

fn receive(socket: &mut Socket, second: bool) {
    let message = socket.read().unwrap();
    let wire: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
    assert_eq!(wire["type"], "response.create");
    assert_eq!(wire["model"], "gpt-5.4");
    assert_eq!(wire["store"], false);
    assert_eq!(wire["reasoning"]["effort"], "high");
    for field in ["previous_response_id", "stream", "background"] {
        assert!(wire.get(field).is_none(), "unexpected WS field: {field}");
    }
    assert_transcript(&wire, second);
}

fn assert_transcript(wire: &Value, second: bool) {
    let input = wire["input"].as_array().unwrap();
    assert_eq!(input.len(), if second { 3 } else { 1 });
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"][0]["text"], "first question");
    if second {
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["text"], "first answer");
        assert_eq!(input[2]["role"], "user");
        assert_eq!(input[2]["content"][0]["text"], "second question");
    }
}

fn response(id: &str, text: &str) -> Vec<Value> {
    vec![
        json!({"type":"response.created","response":{"id":id,"model":"gpt-5.4"}}),
        json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg","type":"message"}}),
        json!({"type":"response.content_part.added","item_id":"msg","output_index":0,"content_index":0,"part":{"type":"output_text"}}),
        json!({"type":"response.output_text.delta","item_id":"msg","output_index":0,"content_index":0,"delta":text}),
        json!({"type":"response.output_text.done","item_id":"msg","output_index":0,"content_index":0,"text":text}),
        json!({"type":"response.content_part.done","item_id":"msg","output_index":0,"content_index":0,"part":{"type":"output_text","text":text}}),
        json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg","type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}}),
        json!({"type":"response.completed","response":{"id":id,"model":"gpt-5.4","usage":{"input_tokens":3,"output_tokens":5}}}),
    ]
}

fn success(socket: &mut Socket, id: &str, text: &str) {
    for event in response(id, text) {
        socket
            .send(Message::Text(event.to_string().into()))
            .unwrap();
    }
}

async fn finished(turn: &mut KitTurn, id: &str, text: &str) {
    tokio::time::timeout(WAIT, async {
        let mut finishes = 0;
        let mut output = String::new();
        while let Some(event) = turn.next_event(None).await.unwrap() {
            match event {
                ModelTurnEvent::Delta(Delta::AppendText { chunk, .. }) => output.push_str(&chunk),
                ModelTurnEvent::Finished(result) => {
                    finishes += 1;
                    assert_eq!(result.response_id.as_deref(), Some(id));
                }
                _ => {}
            }
        }
        assert_eq!(finishes, 1);
        assert_eq!(output, text);
    })
    .await
    .expect("turn hung");
}

#[tokio::test]
#[allow(clippy::result_large_err)] // tungstenite's handshake callback error type.
async fn subscription_auto_uses_websocket_and_sends_full_transcript_on_reuse() {
    let (endpoint, peer) = server(|listener| {
        let mut socket = tungstenite::accept_hdr(
            accept(&listener),
            |request: &tungstenite::handshake::server::Request, response| {
                assert_eq!(request.uri().path(), "/backend-api/codex/responses");
                assert_eq!(request.headers()["authorization"], "Bearer loopback-only");
                assert_eq!(request.headers()["originator"], "kit");
                assert_eq!(
                    request.headers()["user-agent"],
                    concat!("kit/", env!("CARGO_PKG_VERSION"))
                );
                Ok(response)
            },
        )
        .unwrap();
        receive(&mut socket, false);
        success(&mut socket, "first", "first answer");
        receive(&mut socket, true);
        success(&mut socket, "second", "second answer");
    });
    let mut session = session(&endpoint).await;
    let mut first = begin(&mut session, false).await;
    finished(&mut first, "first", "first answer").await;
    // Keep the completed wrapper alive while the next turn claims the socket.
    let mut second = begin(&mut session, true).await;
    finished(&mut second, "second", "second answer").await;
    peer.join().unwrap();
}

#[tokio::test]
async fn subscription_cancel_and_drop_release_socket_without_late_contamination() {
    for mode in ["callback", "checkpoint", "drop"] {
        let (release, released) = std::sync::mpsc::channel();
        let (endpoint, peer) = server(move |listener| {
            let mut old = tungstenite::accept(accept(&listener)).unwrap();
            receive(&mut old, false);
            released.recv_timeout(WAIT).unwrap();
            // The kernel may accept writes after client closure. Neither late
            // text nor a terminal response may leak into the fresh turn.
            for event in response("late", "late contamination") {
                if old.send(Message::Text(event.to_string().into())).is_err() {
                    break;
                }
            }
            let mut fresh = tungstenite::accept(accept(&listener)).unwrap();
            receive(&mut fresh, false);
            success(&mut fresh, "fresh", "fresh answer");
        });
        let mut session = session(&endpoint).await;
        let mut old = Some(begin(&mut session, false).await);
        match mode {
            "callback" => old.as_mut().unwrap().on_cancelled(),
            "checkpoint" => {
                let controller = agentkit_core::CancellationController::new();
                let cancellation = controller.handle().checkpoint();
                controller.interrupt();
                let result = tokio::time::timeout(
                    WAIT,
                    old.as_mut().unwrap().next_event(Some(cancellation)),
                )
                .await
                .expect("cancel hung");
                assert!(matches!(result, Err(LoopError::Cancelled)));
            }
            "drop" => drop(old.take()),
            _ => unreachable!(),
        }
        release.send(()).unwrap();
        let mut fresh = begin(&mut session, false).await;
        finished(&mut fresh, "fresh", "fresh answer").await;
        // In callback mode, release must not depend on dropping the wrapper.
        drop(old);
        peer.join().unwrap();
    }
}

// A minimal HTTP peer also handles the rejected upgrade without over-reading.
fn http(listener: &TcpListener, status: &str, body: &str) -> (String, Value) {
    let mut stream = accept(listener);
    let mut raw = Vec::new();
    while !raw.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        raw.push(byte[0]);
        assert!(raw.len() < 64 * 1024);
    }
    let headers = String::from_utf8(raw).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    assert!(length < 64 * 1024);
    let mut request = vec![0; length];
    stream.read_exact(&mut request).unwrap();
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    (
        headers,
        if request.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&request).unwrap()
        },
    )
}

#[tokio::test]
async fn subscription_auto_426_fallback_stays_http_for_future_turns() {
    let (endpoint, peer) = server(|listener| {
        let (headers, _) = http(&listener, "426 Upgrade Required", "");
        assert!(headers.starts_with("GET /backend-api/codex/responses "));
        for second in [false, true] {
            let body = response("fallback", "HTTP answer")
                .into_iter()
                .map(|event| {
                    format!(
                        "event: {}\ndata: {event}\n\n",
                        event["type"].as_str().unwrap()
                    )
                })
                .collect::<String>();
            let (headers, wire) = http(&listener, "200 OK", &body);
            assert!(
                headers.starts_with("POST /backend-api/codex/responses "),
                "fallback must remain sticky"
            );
            assert_eq!(wire["stream"], true);
            assert!(wire.get("previous_response_id").is_none());
            assert_transcript(&wire, second);
        }
    });
    let mut session = session(&endpoint).await;
    for second in [false, true] {
        let mut turn = begin(&mut session, second).await;
        finished(&mut turn, "fallback", "HTTP answer").await;
    }
    peer.join().unwrap();
}
