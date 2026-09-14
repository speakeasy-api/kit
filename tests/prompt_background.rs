// Local HTTP fixture exercises the real prompt process without provider access.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn prompt_delivers_background_completion_before_exit() {
    for background in [
        Some(serde_json::json!(true)),
        Some(serde_json::json!(1)),
        None,
    ] {
        let detached = background.is_some();
        let home = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/stream", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for turn in 0..if detached { 3 } else { 2 } {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                let body = loop {
                    let n = stream.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
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
                            break serde_json::from_slice::<serde_json::Value>(
                                &request[end + 4..end + 4 + length],
                            )
                            .unwrap();
                        }
                    }
                };
                let (delta, reason) = if turn == 0 {
                    let mut args = serde_json::json!({"script": "return shell({command: \"sleep 2; echo BACKGROUND_RESULT\", timeout_seconds: 5})"});
                    if let Some(background) = &background {
                        args["background"] = background.clone();
                    }
                    (
                        serde_json::json!({"role":"assistant", "tool_calls":[{"index":0,"id":"background-test","type":"function","function":{"name":body["tools"][0]["function"]["name"],"arguments":args.to_string()}}]}),
                        "tool_calls",
                    )
                } else if detached && turn == 1 {
                    (serde_json::json!({"role":"assistant","content":""}), "stop")
                } else {
                    assert!(
                        body["messages"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|message| message["role"] != "assistant"
                                && message.to_string().contains("BACKGROUND_RESULT")),
                        "completion must reach the resumed model turn"
                    );
                    (
                        serde_json::json!({"role":"assistant","content":"RESUMED_FINAL_ANSWER"}),
                        "stop",
                    )
                };
                let chunk = serde_json::json!({"id":"mock","choices":[{"index":0,"delta":delta,"finish_reason":reason}]});
                let response = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
        });
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_kit"));
        command
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .env("OPENROUTER_API_KEY", "local-test-key")
            .env("OPENROUTER_BASE_URL", url)
            .args([
                "prompt",
                "--provider",
                "openrouter",
                "--model",
                "test/model",
                "--credential-store",
                "memory",
                "--root",
            ])
            .arg(home.path())
            .arg("hello")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(15), command.output())
            .await
            .unwrap()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() || !stdout.contains("RESUMED_FINAL_ANSWER") {
            server.abort();
            panic!("detached={detached}: {stdout}\n{stderr}");
        }
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }
}
