#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn cli_resume_restores_effort_instead_of_changed_process_default() {
    for initial_effort in ["high", "default"] {
        let home = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/stream", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
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
                            bodies.push(
                                serde_json::from_slice::<serde_json::Value>(
                                    &request[end + 4..end + 4 + length],
                                )
                                .unwrap(),
                            );
                            break;
                        }
                    }
                }
                let body = "data: {\"id\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
            bodies
        });
        let mut session_id = None;
        for effort in [initial_effort, "low"] {
            let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_kit"));
            command
                .env_clear()
                .env("HOME", home.path())
                .env("PATH", "/usr/bin:/bin")
                .env("OPENROUTER_API_KEY", "local-test-key")
                .env("OPENROUTER_BASE_URL", &url)
                .args([
                    "prompt",
                    "--provider",
                    "openrouter",
                    "--model",
                    "test/model",
                    "--credential-store",
                    "memory",
                    "--reasoning-effort",
                    effort,
                    "--root",
                ])
                .arg(home.path())
                .arg("hello")
                .stdin(Stdio::null())
                .kill_on_drop(true);
            if let Some(id) = &session_id {
                command.arg("--resume").arg(id);
            }
            let output = tokio::time::timeout(Duration::from_secs(30), command.output())
                .await
                .expect("local prompt must settle")
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if session_id.is_none() {
                let workspaces = std::fs::read_dir(home.path().join(".kit/sessions")).unwrap();
                let transcripts: Vec<_> = workspaces
                    .flat_map(|workspace| std::fs::read_dir(workspace.unwrap().path()).unwrap())
                    .filter_map(Result::ok)
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
                    .collect();
                assert_eq!(transcripts.len(), 1);
                session_id = Some(
                    transcripts[0]
                        .path()
                        .file_stem()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_owned(),
                );
            }
        }
        let bodies = tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .unwrap()
            .unwrap();
        for body in bodies {
            let expected = if initial_effort == "default" {
                serde_json::Value::Null
            } else {
                serde_json::json!(initial_effort)
            };
            assert_eq!(body["reasoning"]["effort"], expected);
        }
    }
}
