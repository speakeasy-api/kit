#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

use super::*;
use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tokio::time::{Duration, timeout};

const WAIT: Duration = Duration::from_secs(30);
const PROMPT: &str = "a2a-real-session-execution-sentinel";
const ANSWER: &str = "answer-from-local-provider";

fn send(channel: &agent_client_protocol::Channel, method: &str, id: i64, params: Value) {
    channel
        .tx
        .unbounded_send(agent_client_protocol::TransportFrame::Single(
            agent_client_protocol::RawJsonRpcMessage::request(method.into(), params, id.into())
                .unwrap(),
        ))
        .unwrap();
}

async fn receive(channel: &mut agent_client_protocol::Channel) -> Value {
    let frame = timeout(WAIT, channel.rx.next())
        .await
        .expect("ACP frame timed out")
        .expect("ACP transport closed");
    let agent_client_protocol::TransportFrame::Single(message) = frame else {
        panic!("expected a single ACP message");
    };
    serde_json::to_value(message).unwrap()
}

/// The subprocess isolates provider URL and HOME, following runtime's provider
/// boundary tests. Only external HTTP inference is fake: ACP publication,
/// registry admission, HTTP A2A handling, and the session actor all remain real.
#[tokio::test]
async fn stdio_session_a2a_admission_executes_on_real_actor() {
    const CHILD: &str = "KIT_TEST_STDIO_A2A_EXECUTION";
    if std::env::var_os(CHILD).is_none() {
        async fn inference(
            axum::extract::State(requests): axum::extract::State<
                tokio::sync::mpsc::UnboundedSender<Value>,
            >,
            axum::Json(body): axum::Json<Value>,
        ) -> impl axum::response::IntoResponse {
            requests.send(body).unwrap();
            let chunk = json!({"id": "local-a2a-test", "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": ANSWER},
                "finish_reason": "stop"
            }]});
            (
                [("content-type", "text/event-stream")],
                format!("data: {chunk}\n\ndata: [DONE]\n\n"),
            )
        }
        let home = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
        let app = axum::Router::new()
            .fallback(
                axum::routing::post(inference).get(|| async { axum::Json(json!({"data": []})) }),
            )
            .with_state(requests);
        let provider = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "protocols::acp::v2::a2a_tests::stdio_session_a2a_admission_executes_on_real_actor",
                "--nocapture",
            ])
            .env_clear()
            .env(CHILD, "1")
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .env("OPENROUTER_API_KEY", "local-test-key")
            .env("OPENROUTER_BASE_URL", format!("http://{address}/stream"))
            .env("NO_PROXY", "127.0.0.1")
            .kill_on_drop(true);
        let output = timeout(Duration::from_secs(60), command.output())
            .await
            .expect("isolated ACP/A2A test timed out")
            .unwrap();
        provider.abort();
        let _ = provider.await;
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let body = received
            .try_recv()
            .expect("A2A prompt never reached inference");
        assert_eq!(body["model"], "test/model");
        assert!(
            body["messages"].as_array().unwrap().iter().any(|message| {
                message["role"] == "user" && message["content"].to_string().contains(PROMPT)
            }),
            "provider request did not contain the admitted user prompt"
        );
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let runtime = Runtime::new_with_provider_credentials_and_effort(
        root.path(),
        "test/model",
        crate::ProviderKind::OpenRouter,
        crate::credentials::CredentialStorage::Memory,
        None,
    )
    .unwrap();
    let registry = SessionRegistry::new();
    let (mut client, agent) = agent_client_protocol::Channel::duplex();
    let router = agent_client_protocol::Agent
        .protocol_router()
        .with_v2(stdio_component(runtime.clone(), registry.clone()));
    let acp = tokio::spawn(async move { router.connect_to(agent).await });
    send(
        &client,
        "initialize",
        1,
        serde_json::to_value(wire::InitializeRequest::new(
            wire::ProtocolVersion::V2,
            wire::Implementation::new("a2a-test", "0"),
        ))
        .unwrap(),
    );
    let initialized = receive(&mut client).await;
    assert_eq!(initialized["id"], 1);
    assert!(initialized.get("result").is_some(), "{initialized}");
    send(
        &client,
        "session/new",
        2,
        serde_json::to_value(wire::NewSessionRequest::new(root.path().to_path_buf())).unwrap(),
    );
    let created = receive(&mut client).await;
    assert_eq!(created["id"], 2);
    let session: wire::NewSessionResponse =
        serde_json::from_value(created["result"].clone()).unwrap();

    let mut http = crate::protocols::http::start_with_registry(
        runtime,
        "127.0.0.1:0".into(),
        true,
        false,
        None,
        registry,
    )
    .await
    .unwrap();
    let admitted: Value = timeout(
        WAIT,
        reqwest::Client::new()
            .post(format!("http://{}/", http.address()))
            .json(&json!({
                "jsonrpc": "2.0", "id": 3, "method": "SendMessage",
                "params": {"message": {
                    "messageId": "stdio-a2a-test", "role": "ROLE_USER",
                    "parts": [{"text": PROMPT}]
                }}
            }))
            .send(),
    )
    .await
    .expect("A2A admission timed out")
    .unwrap()
    .error_for_status()
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(admitted["id"], 3);
    assert_eq!(
        admitted["result"]["message"]["metadata"]["kind"],
        "admission_acknowledgment"
    );

    // An admission acknowledgment alone cannot prove execution. Require the
    // external provider's answer on this exact ACP session and its idle boundary.
    let mut answer = String::new();
    let mut running = false;
    timeout(WAIT, async {
        loop {
            let frame = receive(&mut client).await;
            assert_eq!(frame["method"], "session/update", "{frame}");
            let update: wire::UpdateSessionNotification =
                serde_json::from_value(frame["params"].clone()).unwrap();
            assert_eq!(update.session_id, session.session_id);
            match update.update {
                wire::SessionUpdate::AgentMessageChunk(chunk) => {
                    if let wire::ContentBlock::Text(text) = chunk.content {
                        answer.push_str(&text.text);
                    }
                }
                wire::SessionUpdate::StateUpdate(wire::StateUpdate::Running(_)) => running = true,
                wire::SessionUpdate::StateUpdate(wire::StateUpdate::Idle(_)) if running => break,
                _ => {}
            }
        }
    })
    .await
    .expect("admitted A2A turn never completed on ACP");
    assert_eq!(answer, ANSWER);
    send(
        &client,
        "session/close",
        4,
        serde_json::to_value(wire::CloseSessionRequest::new(session.session_id)).unwrap(),
    );
    let closed = receive(&mut client).await;
    assert_eq!(closed["id"], 4);
    assert!(closed.get("result").is_some(), "{closed}");
    http.shutdown();
    http.join().await.unwrap();
    acp.abort();
    let _ = acp.await;
}
