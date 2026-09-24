use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn input() -> Value {
    json!({"state":{"ticket":"Please help"},"questions":{
        "urgent":{"type":"noul","instructions":"Urgent?","criteria":{"true":"Now"}},
        "route":{"type":"choice","instructions":["Route"],"criteria":{"billing":null,"support":"Help"}},
        "severity":{"type":"score","instructions":{"question":"How severe?"},"criteria":["Low","High"]}
    }})
}
fn response() -> Value {
    json!({"model":"jev-1.13.0","answers":{
        "urgent":{"type":"noul","noul":0.7},
        "route":{"type":"choice","choice":"support","probabilities":{"billing":0.2,"support":0.8},"confidence":0.6},
        "severity":{"type":"score","score":0.8,"legend":{"0":"Low","1":"High"},"probabilities":{"0":0.2,"1":0.8},"confidence":0.6}
    },"usage":{"input_tokens":30,"output_tokens":20}})
}
#[test]
fn validates_three_question_types_and_bounds() {
    let tool = EvalTool::new(CredentialStorage::default());
    let schema = jsonschema::validator_for(&tool.spec().input_schema).unwrap();
    assert!(schema.is_valid(&input()));
    let body = payload(input()).unwrap();
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(validate_response(response(), &body).unwrap(), response());
    for bad in [
        json!(null),
        json!({"state":1,"questions":{}}),
        json!({"state":"x","questions":{"a":{"type":"json","instructions":"x"}}}),
    ] {
        assert!(payload(bad).is_err());
    }
    let mut large = input();
    large["state"] = Value::String("x".repeat(MAX_BYTES));
    assert!(payload(large).is_err());
    for q in [
        json!({"type":"score","instructions":"x","criteria":["one"]}),
        json!({"type":"noul","instructions":"x","criteria":{"maybe":"x"}}),
        json!({"type":"choice","instructions":"x","criteria":{}}),
    ] {
        assert!(payload(json!({"state":"x","questions":{"q":q}})).is_err());
    }
}
#[test]
fn question_name_bounds_count_unicode_characters() {
    for (length, accepted) in [(0, false), (65, true), (128, true), (129, false)] {
        let name = "é".repeat(length);
        let value = json!({
            "state": "x",
            "questions": {name: {"type": "noul", "instructions": "Urgent?"}}
        });
        assert_eq!(payload(value).is_ok(), accepted, "{length} characters");
    }
}

#[test]
fn rejects_malformed_answers() {
    let body = payload(input()).unwrap();
    for (pointer, bad) in [
        ("/model", json!("jev-latest")),
        ("/usage/input_tokens", json!(-1)),
        ("/answers/urgent/noul", json!(1.1)),
        ("/answers/route/choice", json!("unknown")),
        ("/answers/route/confidence", json!(null)),
        ("/answers/route/probabilities/support", json!(0.1)),
        ("/answers/severity/score", json!(2)),
        ("/answers/severity/legend", json!({})),
        ("/answers/urgent/type", json!("choice")),
        ("/answers", json!({})),
    ] {
        let mut value = response();
        *value.pointer_mut(pointer).unwrap() = bad;
        assert!(validate_response(value, &body).is_err(), "{pointer}");
    }
}

// An actual local HTTP peer: tests never read a real key or contact TypeSafe.
async fn peer(status: u16, body: String) -> (String, tokio::task::JoinHandle<Value>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let (offset, length) = loop {
            let mut buffer = [0; 4096];
            let n = socket.read(&mut buffer).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(offset) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..offset]).to_lowercase();
                assert!(headers.starts_with("post /v1/systemone "));
                assert!(headers.contains("authorization: bearer fake-key"));
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                break (offset + 4, length);
            }
        };
        while bytes.len() < offset + length {
            let mut buffer = [0; 4096];
            let n = socket.read(&mut buffer).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&buffer[..n]);
        }
        let request = serde_json::from_slice(&bytes[offset..offset + length]).unwrap();
        let reply = format!(
            "HTTP/1.1 {status} Result\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(reply.as_bytes()).await.unwrap();
        request
    });
    (format!("http://{address}/v1/systemone"), task)
}
#[tokio::test]
async fn sends_all_questions_once_and_preserves_results() {
    let expected = response();
    let (url, peer) = peer(200, expected.to_string()).await;
    let body = payload(input()).unwrap();
    assert_eq!(
        submit(&client().unwrap(), &url, "fake-key", &body)
            .await
            .unwrap(),
        expected
    );
    assert_eq!(peer.await.unwrap(), body);
}
#[tokio::test]
async fn safe_errors_for_rejected_keys_limits_and_malformed_results() {
    for (status, body, message) in [
        (401, "secret state", "could not accept your key"),
        (403, "secret state", "could not accept your key"),
        (429, "secret state", "limit was reached"),
        (500, "secret state", "did not complete"),
        (200, "secret state", "invalid result"),
    ] {
        let (url, peer) = peer(status, body.into()).await;
        let error = submit(
            &client().unwrap(),
            &url,
            "fake-key",
            &payload(input()).unwrap(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains(message));
        assert!(!error.contains("secret state"));
        peer.await.unwrap();
    }
}
#[test]
fn disabled_gate_never_needs_credentials() {
    assert!(!EvalTool::available(false, &CredentialStorage::default()));
}

#[test]
fn credential_gate_uses_only_explicit_sources() {
    const MARKER: &str = "KIT_TEST_EVAL_GATE";
    if let Ok(case) = std::env::var(MARKER) {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().to_path_buf());
        if case == "stored" {
            storage
                .entry("typesafe", "default")
                .save(br#"{"api_key":"fake-stored-key"}"#)
                .unwrap();
        }
        assert!(!EvalTool::available(false, &storage));
        assert_eq!(
            EvalTool::available(true, &storage),
            case == "stored" || case == "environment"
        );
        return;
    }
    for case in ["missing", "stored", "environment", "invalid"] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tools::eval::tests::credential_gate_uses_only_explicit_sources",
            ])
            .env(MARKER, case)
            .env_remove("TYPESAFE_API_KEY");
        if case == "environment" {
            command.env("TYPESAFE_API_KEY", "fake-environment-key");
        }
        if case == "invalid" {
            command.env("TYPESAFE_API_KEY", "invalid key");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn cancellation_while_waiting_does_not_consume_permits() {
    use agentkit_core::{CancellationController, MetadataMap, SessionId, TurnId};
    use agentkit_tools_core::{AllowAllPermissions, OwnedToolContext};
    let tool = EvalTool::new(CredentialStorage::default());
    let held = tool.permits.acquire_many(4).await.unwrap();
    let controller = CancellationController::new();
    let context = OwnedToolContext {
        session_id: SessionId::new("eval-test"),
        turn_id: TurnId::new("turn"),
        metadata: MetadataMap::new(),
        permissions: Arc::new(AllowAllPermissions),
        resources: Arc::new(()),
        cancellation: Some(controller.handle().checkpoint()),
        execution_scope: None,
        approved_request: None,
    };
    let request = ToolRequest::new("call", "eval", input(), "eval-test", "turn");
    let mut borrowed = context.borrowed();
    let mut invocation = Box::pin(tool.invoke(request, &mut borrowed));
    // Poll the real API until it waits for an admission permit, then cancel.
    assert!(futures_util::poll!(invocation.as_mut()).is_pending());
    controller.interrupt();
    assert!(
        invocation
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    drop(held);
    assert!(tool.permits.try_acquire_many(4).is_ok());
}
