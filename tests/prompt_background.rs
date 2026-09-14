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
    let mut cases = vec![
        (Some(serde_json::json!(true)), "success"),
        (Some(serde_json::json!(1)), "success"),
        (None, "success"),
        (Some(serde_json::json!(true)), "failure"),
        (Some(serde_json::json!(true)), "race"),
    ];
    if cfg!(unix) {
        cases.extend([
            (Some(serde_json::json!(true)), "TERM"),
            (Some(serde_json::json!(true)), "INT"),
        ]);
    }
    for (background, scenario) in cases {
        let detached = background.is_some();
        let home = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/stream", listener.local_addr().unwrap());
        let root = home.path().to_path_buf();
        let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut paused_tx = Some(paused_tx);
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
                    match scenario {
                        "failure" => {
                            args["script"] = serde_json::json!(
                                "_ = shell({command: \"sleep 2\"})\nreturn fail(\"BACKGROUND_RESULT\", \"expected local failure\")"
                            )
                        }
                        "race" => {
                            args["script"] = serde_json::json!(
                                "return shell({command: \"while [ ! -f release ]; do sleep 0.02; done; echo BACKGROUND_RESULT; touch completed\", timeout_seconds: 5})"
                            )
                        }
                        "TERM" | "INT" => {
                            args["script"] = serde_json::json!(
                                "return shell({command: \"echo $$ > owned-pid; sleep 4; touch orphan-canary\", timeout_seconds: 6})"
                            )
                        }
                        _ => {}
                    }
                    if let Some(background) = &background {
                        args["background"] = background.clone();
                    }
                    (
                        serde_json::json!({"role":"assistant", "tool_calls":[{"index":0,"id":"background-test","type":"function","function":{"name":body["tools"][0]["function"]["name"],"arguments":args.to_string()}}]}),
                        "tool_calls",
                    )
                } else if detached && turn == 1 {
                    if scenario == "race" {
                        // Release the task only after the empty final model turn
                        // has started; keep its response open until completion.
                        std::fs::write(root.join("release"), "go").unwrap();
                        wait_for_file(&root.join("completed")).await;
                    }
                    let _ = paused_tx.take().unwrap().send(());
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
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = command.spawn().unwrap();
        if scenario == "TERM" || scenario == "INT" {
            tokio::time::timeout(Duration::from_secs(5), paused_rx)
                .await
                .unwrap()
                .unwrap();
            wait_for_file(&home.path().join("owned-pid")).await;
            let pid = child.id().unwrap().to_string();
            assert!(
                tokio::process::Command::new("/bin/kill")
                    .args([format!("-{scenario}"), pid])
                    .status()
                    .await
                    .unwrap()
                    .success()
            );
        }
        let output = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if scenario == "TERM" || scenario == "INT" {
            server.abort();
            // A real descendant canary detects an orphan, not merely CLI exit.
            tokio::time::sleep(Duration::from_millis(4200)).await;
            let orphaned = home.path().join("orphan-canary").exists();
            assert!(!output.status.success(), "signal must not report success");
            assert!(
                !orphaned,
                "{scenario} left the owned shell descendant alive"
            );
            continue;
        }
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

async fn wait_for_file(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[cfg(unix)]
async fn cancel_during_startup(signals: &[&str]) {
    let home = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/chat/completions", listener.local_addr().unwrap());
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let n = stream.read(&mut buffer).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buffer[..n]);
        }
        assert!(request.starts_with(b"GET /models "));
        entered_tx.send(()).unwrap();
        // Keep provider startup pending until AFTER the process exits. There is
        // no model response, credential backend, or external network request.
        let _ = release_rx.await;
        drop(stream);
    });
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_kit"))
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
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(15), entered_rx)
        .await
        .unwrap()
        .unwrap();
    for signal in signals {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        let pid = child.id().unwrap().to_string();
        // A second signal may race the already-completing process.
        let _ = tokio::process::Command::new("/bin/kill")
            .args([*signal, &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
    }
    let output = tokio::time::timeout(Duration::from_secs(3), child.wait_with_output()).await;
    let _ = release_tx.send(());
    server.await.unwrap();
    let output = output
        .expect("signal must exit without waiting for startup metadata")
        .unwrap();
    assert!(!output.status.success());
}

#[cfg(unix)]
#[tokio::test]
async fn startup_signals_do_not_wait_for_metadata() {
    // These each launch a full CLI runtime; keep the scenarios sequential rather
    // than multiplying process startup load in the integration test runner.
    for signals in [&["-TERM"][..], &["-INT"][..], &["-TERM", "-TERM"][..]] {
        cancel_during_startup(signals).await;
    }
}
