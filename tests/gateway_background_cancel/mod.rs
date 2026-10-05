use super::*;

#[tokio::test]
async fn sdk_cancels_detached_background_call_after_controller_replacement() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    // Fake only the external model. The gateway, child ACP process, compose,
    // detached job registry and shell cancellation are all real.
    async fn inference(
        axum::extract::State(turn): axum::extract::State<Arc<AtomicUsize>>,
        axum::Json(body): axum::Json<Value>,
    ) -> impl axum::response::IntoResponse {
        let (delta, reason) = if turn.fetch_add(1, Ordering::SeqCst) == 0 {
            let args = json!({"background":true,"script":"return shell({command: \"echo $$ > background.pid; exec sleep 120\", timeout_seconds: 150})"});
            (
                json!({"role":"assistant","tool_calls":[{"index":0,"id":"gateway-background-call","type":"function","function":{"name":body["tools"][0]["function"]["name"],"arguments":args.to_string()}}]}),
                "tool_calls",
            )
        } else {
            (
                json!({"role":"assistant","content":"Waiting for detached work."}),
                "stop",
            )
        };
        let chunk = json!({"id":"local-background-test","choices":[{"index":0,"delta":delta,"finish_reason":reason}]});
        (
            [("content-type", "text/event-stream")],
            format!("data: {chunk}\n\ndata: [DONE]\n\n"),
        )
    }

    let mut fixture = Fixture::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    fixture.provider_url = format!("http://{}/stream", listener.local_addr().unwrap());
    let provider = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new()
                .fallback(axum::routing::post(inference))
                .with_state(Arc::new(AtomicUsize::new(0))),
        )
        .await
        .unwrap();
    });
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
    // The native operation acknowledges cancellation even for unknown calls.
    // Pin its snake_case wire format without changing those semantics.
    let missing = client
        .request(
            3,
            "kit/background/cancel",
            json!({"session_id":id,"call_id":"missing"}),
        )
        .await;
    assert_eq!(missing["result"]["cancelled"], true, "{missing}");
    let malformed = client
        .request(
            4,
            "kit/background/cancel",
            json!({"sessionId":id,"call_id":"missing"}),
        )
        .await;
    assert!(malformed.get("error").is_some(), "{malformed}");
    client.start_prompt(5, id, "Start a background task.").await;
    client.wait_idle().await;
    let pid = timeout(WAIT, async {
        loop {
            if let Ok(pid) = fs::read_to_string(fixture.root.join("background.pid"))
                && let Ok(pid) = pid.trim().parse::<u32>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background shell did not start");
    assert!(
        client.send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
    );

    // A reply on the same transport fences the earlier cancel notification
    // before replacing its controller.
    let barrier = client
        .request(
            6,
            "kit/background/cancel",
            json!({"session_id":id,"call_id":"barrier"}),
        )
        .await;
    assert_eq!(barrier["result"]["cancelled"], true, "{barrier}");

    let mut replacement = SdkClient::connect(&url).await;
    let resumed = replacement
        .request(
            2,
            "session/resume",
            json!({"sessionId":id,"cwd":fixture.root,"mcpServers":[]}),
        )
        .await;
    assert!(resumed.get("error").is_none(), "{resumed}");
    client
        .assert_rejected_or_closed(
            7,
            "kit/background/cancel",
            json!({"session_id":id,"call_id":"gateway-background-call"}),
        )
        .await;
    let wrong = replacement
        .request(
            3,
            "kit/background/cancel",
            json!({"session_id":"another-session","call_id":"gateway-background-call"}),
        )
        .await;
    assert!(wrong.get("error").is_some(), "{wrong}");
    // Session cancellation and controller replacement must leave detached work alive.
    assert!(
        Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap()
            .success()
    );
    let stopped = replacement
        .request(
            4,
            "kit/background/cancel",
            json!({"session_id":id,"call_id":"gateway-background-call"}),
        )
        .await;
    assert_eq!(stopped["result"]["cancelled"], true, "{stopped}");
    let repeated = replacement
        .request(
            5,
            "kit/background/cancel",
            json!({"session_id":id,"call_id":"gateway-background-call"}),
        )
        .await;
    assert_eq!(repeated["result"]["cancelled"], true, "{repeated}");
    timeout(WAIT, async {
        while Command::new("/bin/kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap()
            .success()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("background cancellation did not reap the shell");
    replacement.detach().await;
    // A replaced transport may already have been closed by the server.
    client.transport.abort();
    stop_gateway(&mut gateway).await;
    provider.abort();
}
