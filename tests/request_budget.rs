// Process isolation keeps RequestBudget's OnceLock out of other tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn initialization_is_idempotent_but_rejects_a_different_value() {
    const CHILD: &str = "KIT_TEST_REQUEST_BUDGET_INITIALIZATION";
    if std::env::var_os(CHILD).is_some() {
        let budget = kit::request_budget::RequestBudget::try_from(7).unwrap();
        budget.initialize().unwrap();
        budget.initialize().unwrap();
        assert!(
            kit::request_budget::RequestBudget::try_from(8)
                .unwrap()
                .initialize()
                .unwrap_err()
                .contains("different value")
        );
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "initialization_is_idempotent_but_rejects_a_different_value",
        ])
        .env(CHILD, "1")
        .status()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn startup_resolves_effective_stream_budget_and_cli_precedence() {
    // Identical streams finish after the short budget, but before idle/attempt
    // deadlines. Assert outcomes, not elapsed wall time.
    for (configured, cli, succeeds) in [
        (1, None, false),
        (8, None, true),
        (1, Some(8), true),
        (8, Some(1), false),
    ] {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".kit")).unwrap();
        std::fs::write(
            home.path().join(".kit/config.toml"),
            format!("request_budget_seconds = {configured}\n"),
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/stream", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
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
                        break;
                    }
                }
            }
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"id\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"}}]}\n\n").await.unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
            // The short-budget client has already closed its stream.
            let _ = stream.write_all(b"data: {\"id\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"budget-stream-completed\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").await;
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
        if let Some(seconds) = cli {
            command.args(["--request-budget-seconds", &seconds.to_string()]);
        }
        let output = tokio::time::timeout(Duration::from_secs(15), command.output())
            .await
            .expect("local prompt must settle")
            .unwrap();
        server.abort();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.success(),
            succeeds,
            "config={configured}, cli={cli:?}: {stdout}\n{stderr}"
        );
        if succeeds {
            assert!(
                stdout.contains("budget-stream-completed"),
                "{stdout}\n{stderr}"
            );
        } else {
            assert!(
                stderr.contains("budget") || stderr.contains("deadline"),
                "{stdout}\n{stderr}"
            );
        }
    }
}
