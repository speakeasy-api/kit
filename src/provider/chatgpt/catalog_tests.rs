//! Catalog discovery through the real credential store and HTTP boundary.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const WAIT: Duration = Duration::from_secs(5);
const CATALOG: &str =
    r#"{"models":[{"slug":"gpt-5.4","context_window":272000,"visibility":"list"}]}"#;

pub(super) async fn adapter() -> (OpenAiSubscriptionAdapter, TcpListener, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let storage = crate::credentials::CredentialStorage::Filesystem(directory.path().into());
    let record = auth::test_support::token_record("loopback-only", "account", "generation");
    storage
        .entry("openai-subscription", "subscription")
        .save(&serde_json::to_vec(&record).unwrap())
        .unwrap();
    let mut adapter = OpenAiSubscriptionAdapter::new(
        SubscriptionConfig::new("gpt-5.4".into())
            .unwrap()
            .with_credential_storage(storage),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    adapter.catalog_client.endpoint = format!("http://{}/models", listener.local_addr().unwrap());
    (adapter, listener, directory)
}

pub(super) async fn request(listener: &TcpListener) -> TcpStream {
    tokio::time::timeout(WAIT, async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(stream.read_u8().await.unwrap());
            assert!(header.len() < 8192);
        }
        let header = String::from_utf8(header).unwrap();
        assert!(
            header.starts_with("GET /models?client_version=0.159.3 HTTP/1.1\r\n"),
            "{header}"
        );
        stream
    })
    .await
    .expect("catalog request did not arrive")
}

pub(super) async fn respond(mut stream: TcpStream, status: &str, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}

pub(super) async fn discovered(session: &OpenAiSubscriptionSession) {
    tokio::time::timeout(WAIT, async {
        while !session.context_window.worker.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("discovery did not finish");
    assert_eq!(session.context_window.value.get(), Some(&272_000));
}

#[tokio::test]
async fn context_discovery_does_not_block_start_and_retries_failed_http() {
    let (adapter, listener, _directory) = adapter().await;
    // The peer cannot reply until startup returns: the timeout is only a
    // deadlock watchdog, not a wall-clock performance assertion.
    let session = tokio::time::timeout(WAIT, adapter.start_session(SessionConfig::new("session")))
        .await
        .expect("session start waited for catalog HTTP")
        .unwrap();
    let first = request(&listener).await;
    assert!(session.context_window.value.get().is_none());
    respond(first, "503 Service Unavailable", "").await;
    let retry = request(&listener).await;
    assert!(session.context_window.value.get().is_none());
    respond(retry, "200 OK", CATALOG).await;
    discovered(&session).await;
    // The picker and background discovery share the credential-bound result.
    let catalog = adapter.model_catalog().await.unwrap();
    assert_eq!(catalog.context_windows.get("gpt-5.4"), Some(&272_000));
}

#[tokio::test]
async fn context_discovery_cancellation_releases_cache_initialization() {
    let (adapter, listener, _directory) = adapter().await;
    let session = adapter
        .start_session(SessionConfig::new("first"))
        .await
        .unwrap();
    let mut pending = request(&listener).await;
    let worker = session.context_window.worker.abort_handle();
    drop(session);
    tokio::time::timeout(WAIT, async {
        while !worker.is_finished() {
            tokio::task::yield_now().await;
        }
        // The real pending HTTP request is cancelled, not detached.
        let mut byte = [0];
        assert_eq!(pending.read(&mut byte).await.unwrap(), 0);
    })
    .await
    .unwrap();
    let next = adapter
        .start_session(SessionConfig::new("next"))
        .await
        .unwrap();
    let retry = request(&listener).await;
    respond(retry, "200 OK", CATALOG).await;
    discovered(&next).await;
}

#[tokio::test]
async fn context_discovery_stops_when_credentials_change() {
    let (adapter, listener, _directory) = adapter().await;
    let session = adapter
        .start_session(SessionConfig::new("session"))
        .await
        .unwrap();
    let first = request(&listener).await;
    let record = auth::test_support::token_record("new-token", "new-account", "new-generation");
    adapter
        .config
        .credential_storage
        .entry("openai-subscription", "subscription")
        .save(&serde_json::to_vec(&record).unwrap())
        .unwrap();
    respond(first, "503 Service Unavailable", "").await;
    tokio::time::timeout(WAIT, async {
        while !session.context_window.worker.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(session.context_window.value.get().is_none());
    let next = adapter
        .start_session(SessionConfig::new("next"))
        .await
        .unwrap();
    respond(request(&listener).await, "200 OK", CATALOG).await;
    discovered(&next).await;
}

#[tokio::test]
async fn foreground_catalog_lookup_does_not_wait_out_background_discovery() {
    let (adapter, listener, _directory) = adapter().await;
    let session = adapter
        .start_session(SessionConfig::new("session"))
        .await
        .unwrap();
    // Background discovery now owns cache initialization and its request stalls.
    let _stalled = request(&listener).await;
    let started = std::time::Instant::now();
    let lookup = tokio::time::timeout(MODEL_CATALOG_BACKGROUND_TIMEOUT, adapter.model_catalog())
        .await
        .expect("foreground lookup waited for background discovery");
    assert!(lookup.is_err());
    assert!(started.elapsed() < MODEL_CATALOG_AUTH_TIMEOUT * 2);
    drop(session);
}
