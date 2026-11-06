#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

use std::{
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Provider {
    listed: Mutex<Vec<&'static str>>,
    completions: Mutex<Vec<String>>,
}

async fn serve(listener: tokio::net::TcpListener, provider: Arc<Provider>) {
    loop {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        let body = loop {
            let n = stream.read(&mut buffer).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buffer[..n]);
            let Some(end) = request.windows(4).position(|s| s == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..end]).to_string();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                break request[end + 4..end + 4 + length].to_vec();
            }
        };
        let response = if request.starts_with(b"GET") {
            let models: Vec<_> = provider
                .listed
                .lock()
                .unwrap()
                .iter()
                .map(|id| serde_json::json!({"id": id}))
                .collect();
            let body = serde_json::json!({"data": models}).to_string();
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        } else {
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            provider
                .completions
                .lock()
                .unwrap()
                .push(request["model"].as_str().unwrap().to_owned());
            let body = "data: {\"id\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\ndata: [DONE]\n\n";
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
        };
        stream.write_all(response.as_bytes()).await.unwrap();
    }
}

async fn prompt(home: &Path, url: &str, args: &[&str]) {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_kit"));
    command
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("OPENROUTER_API_KEY", "local-test-key")
        .env("OPENROUTER_BASE_URL", url)
        .args(["prompt", "--credential-store", "memory", "--root"])
        .arg(home)
        .args(args)
        .arg("hello")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("local prompt must settle")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn transcript(home: &Path) -> std::path::PathBuf {
    let paths: Vec<_> = std::fs::read_dir(home.join(".kit/sessions"))
        .unwrap()
        .flat_map(|workspace| std::fs::read_dir(workspace.unwrap().path()).unwrap())
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    assert_eq!(paths.len(), 1);
    paths.into_iter().next().unwrap()
}

fn latest_usage_model(path: &Path) -> serde_json::Value {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .rev()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|record| !record["item"]["usage"].is_null())
        .expect("transcript should record usage")["item"]["usage"]["metadata"]["kit.model"]
        .clone()
}

#[tokio::test]
async fn resumed_prompt_restores_overrides_and_falls_back_from_the_recorded_model() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".kit")).unwrap();
    std::fs::write(
        home.path().join(".kit/config.toml"),
        "provider = \"openrouter\"\nmodel = \"test/default\"\n",
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let provider = Arc::new(Provider::default());
    *provider.listed.lock().unwrap() = vec!["test/default", "test/first", "test/explicit"];
    let server = tokio::spawn(serve(listener, Arc::clone(&provider)));

    prompt(home.path(), &url, &["--model", "test/first"]).await;
    let path = transcript(home.path());
    let id = path.file_stem().unwrap().to_str().unwrap().to_owned();
    assert_eq!(latest_usage_model(&path), "openrouter:test/first");

    prompt(home.path(), &url, &["--resume", &id]).await;
    prompt(
        home.path(),
        &url,
        &["--resume", &id, "--model", "test/explicit"],
    )
    .await;
    prompt(home.path(), &url, &["--resume", &id]).await;
    assert_eq!(latest_usage_model(&path), "openrouter:test/explicit");

    *provider.listed.lock().unwrap() = vec!["test/default"];
    prompt(home.path(), &url, &["--resume", &id]).await;
    assert_eq!(latest_usage_model(&path), "openrouter:test/default");

    *provider.listed.lock().unwrap() = vec!["test/default", "test/explicit"];
    prompt(home.path(), &url, &["--resume", &id]).await;
    assert_eq!(latest_usage_model(&path), "openrouter:test/explicit");

    server.abort();
    assert_eq!(
        *provider.completions.lock().unwrap(),
        [
            "test/first",
            "test/first",
            "test/explicit",
            "test/explicit",
            "test/default",
            "test/explicit",
        ]
    );
}
