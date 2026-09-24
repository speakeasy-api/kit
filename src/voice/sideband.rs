//! Native authenticated control channel for an already-created v3 media call.
//!
//! Contract: codex-api `realtime_websocket/methods.rs` and core
//! `client.rs::sideband_websocket_auth_headers` from the pinned source snapshot.
//! CoreCreated uses LegacyWebrtcSideband initialization: FramelessBidi (v3)
//! explicitly skips session.update. The session was configured by call creation.
//! Joining never creates a call, retries, reconnects, or invokes Codex.

use std::time::Duration;

use super::signaling::SidebandConnection;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        handshake::derive_accept_key,
        protocol::{Role, WebSocketConfig},
    },
};

const TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// JSON text control channel; media remains on the native WebRTC transport.
/// Drop on any error. No reconnect or recovery is performed here.
pub struct Sideband {
    stream: WebSocketStream<reqwest::Upgraded>,
}

/// Join with the exact subscription identity used to create the media call.
/// CoreCreated v3 needs no initial session.update.
pub async fn connect(connection: SidebandConnection) -> Result<Sideband, String> {
    let url = https_url(connection.url)?;
    tokio::time::timeout(TIMEOUT, upgrade(url.as_str(), connection.headers))
        .await
        .map_err(|_| "sideband connection timed out")?
}

fn https_url(mut url: reqwest::Url) -> Result<reqwest::Url, String> {
    if url.scheme() != "wss"
        || url.host_str() != Some("api.openai.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.path().starts_with("/v1/live/")
    {
        return Err("invalid subscription sideband endpoint".into());
    }
    let id = url.path().strip_prefix("/v1/live/").unwrap_or("");
    if id.is_empty()
        || id.len() > 256
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("invalid subscription call id".into());
    }
    url.set_scheme("https")
        .map_err(|_| "invalid sideband scheme")?;
    Ok(url)
}

async fn upgrade(url: &str, mut headers: HeaderMap) -> Result<Sideband, String> {
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| "sideband handshake entropy unavailable")?;
    let key = STANDARD.encode(nonce);
    headers.insert("connection", HeaderValue::from_static("Upgrade"));
    headers.insert("upgrade", HeaderValue::from_static("websocket"));
    headers.insert("sec-websocket-version", HeaderValue::from_static("13"));
    headers.insert(
        "sec-websocket-key",
        HeaderValue::from_str(&key).map_err(|_| "invalid sideband handshake key")?,
    );
    let client = reqwest::Client::builder()
        .http1_only()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(TIMEOUT)
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| "could not build sideband HTTP client")?;
    let response = client
        .get(url)
        .headers(headers)
        .send()
        .await
        .map_err(|_| "sideband handshake request failed")?;
    validate_handshake(response.status(), response.headers(), &key)?;
    let socket = response
        .upgrade()
        .await
        .map_err(|_| "sideband HTTP upgrade failed")?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_EVENT_BYTES))
        .max_frame_size(Some(MAX_EVENT_BYTES))
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_EVENT_BYTES + 1024);
    Ok(Sideband {
        stream: WebSocketStream::from_raw_socket(socket, Role::Client, Some(config)).await,
    })
}

fn validate_handshake(
    status: reqwest::StatusCode,
    headers: &HeaderMap,
    key: &str,
) -> Result<(), String> {
    let has_token = |name: &str, token: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|v| v.trim().eq_ignore_ascii_case(token))
    };
    let accepts: Vec<_> = headers.get_all("sec-websocket-accept").iter().collect();
    if status != reqwest::StatusCode::SWITCHING_PROTOCOLS
        || !has_token("connection", "upgrade")
        || !has_token("upgrade", "websocket")
        || accepts.len() != 1
        || accepts[0].as_bytes() != derive_accept_key(key.as_bytes()).as_bytes()
        // Neither extensions nor a subprotocol were offered.
        || headers.contains_key("sec-websocket-extensions")
        || headers.contains_key("sec-websocket-protocol")
    {
        return Err("invalid sideband websocket handshake".into());
    }
    Ok(())
}

impl Sideband {
    /// Receive one bounded JSON text event. Ping/pong is handled internally.
    /// Waiting for an event has no idle deadline; the controller owns cancellation.
    pub async fn recv(&mut self) -> Result<Option<serde_json::Value>, String> {
        while let Some(message) = self.stream.next().await {
            match message.map_err(|_| "sideband receive failed")? {
                Message::Text(text) => {
                    return serde_json::from_str(&text)
                        .map(Some)
                        .map_err(|_| "invalid sideband JSON event".into());
                }
                Message::Close(_) => return Ok(None),
                Message::Ping(_) => {
                    tokio::time::timeout(TIMEOUT, self.stream.flush())
                        .await
                        .map_err(|_| "sideband pong timed out")?
                        .map_err(|_| "sideband pong failed")?;
                }
                Message::Pong(_) => {}
                _ => return Err("unexpected non-text sideband event".into()),
            }
        }
        Ok(None)
    }

    /// Send bounded JSON text. Callers must drop the channel after a send error.
    pub async fn send(&mut self, text: &str) -> Result<(), String> {
        if text.len() > MAX_EVENT_BYTES {
            return Err("sideband event exceeds size limit".into());
        }
        serde_json::from_str::<serde_json::Value>(text)
            .map_err(|_| "invalid outbound sideband JSON")?;
        tokio::time::timeout(
            TIMEOUT,
            self.stream.send(Message::Text(text.to_owned().into())),
        )
        .await
        .map_err(|_| "sideband send timed out")?
        .map_err(|_| "sideband send failed".into())
    }

    pub async fn close(&mut self) -> Result<(), String> {
        self.send(&super::signaling::session_close().to_string())
            .await?;
        self.stream
            .close(None)
            .await
            .map_err(|_| "sideband close failed".into())
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    clippy::disallowed_macros,
    clippy::unwrap_used,
    clippy::expect_used
)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn call_ids_cannot_escape_endpoint() {
        assert_eq!(
            https_url("wss://api.openai.com/v1/live/rtc_test".parse().unwrap())
                .unwrap()
                .as_str(),
            "https://api.openai.com/v1/live/rtc_test"
        );
        for id in ["", "..", "../admin", "x?query", "x#fragment", "%2f", "a/b"] {
            assert!(
                https_url(
                    format!("wss://api.openai.com/v1/live/{id}")
                        .parse()
                        .unwrap()
                )
                .is_err()
            );
        }
    }

    #[test]
    fn handshake_rejects_missing_accept_and_unsolicited_extensions() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let mut h = HeaderMap::new();
        h.insert(
            "connection",
            HeaderValue::from_static("keep-alive, Upgrade"),
        );
        h.insert("upgrade", HeaderValue::from_static("WebSocket"));
        assert!(validate_handshake(reqwest::StatusCode::SWITCHING_PROTOCOLS, &h, key).is_err());
        h.insert(
            "sec-websocket-accept",
            HeaderValue::from_static("s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
        );
        assert!(validate_handshake(reqwest::StatusCode::SWITCHING_PROTOCOLS, &h, key).is_ok());
        assert!(validate_handshake(reqwest::StatusCode::OK, &h, key).is_err());
        h.insert(
            "sec-websocket-extensions",
            HeaderValue::from_static("permessage-deflate"),
        );
        assert!(validate_handshake(reqwest::StatusCode::SWITCHING_PROTOCOLS, &h, key).is_err());
    }

    #[tokio::test]
    async fn startup_failure_and_cancellation_close_connected_session() {
        for cancel in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/v1/live/rtc_test", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut tcp, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    assert!(request.len() < 8192);
                    request.push(tcp.read_u8().await.unwrap());
                }
                let request = String::from_utf8(request).unwrap();
                let key = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("sec-websocket-key")
                            .then(|| value.trim())
                    })
                    .unwrap();
                let accept = derive_accept_key(key.as_bytes());
                tcp.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
                let mut ws = WebSocketStream::from_raw_socket(tcp, Role::Server, None).await;
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(
                        &ws.next().await.unwrap().unwrap().into_text().unwrap()
                    )
                    .unwrap()["type"],
                    "session.close"
                );
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                let channel = upgrade(&url, HeaderMap::new()).await.unwrap();
                if cancel {
                    let (ready, started) = tokio::sync::oneshot::channel();
                    let task = tokio::spawn(async move {
                        let _cleanup = super::super::StartupSideband(Some(channel));
                        ready.send(()).unwrap();
                        std::future::pending::<()>().await;
                    });
                    started.await.unwrap();
                    task.abort();
                    assert!(task.await.unwrap_err().is_cancelled());
                } else {
                    let failed = async move {
                        let _cleanup = super::super::StartupSideband(Some(channel));
                        Err::<(), _>("answer rejected")
                    }
                    .await;
                    assert!(failed.is_err());
                }
                server.await.unwrap();
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn local_upgrade_exchanges_json_without_session_update() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/live/rtc_test", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 8192);
                request.push(tcp.read_u8().await.unwrap());
            }
            let request = String::from_utf8(request).unwrap();
            let key = request
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim())
                })
                .unwrap();
            let accept = derive_accept_key(key.as_bytes());
            tcp.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
            let mut ws = WebSocketStream::from_raw_socket(tcp, Role::Server, None).await;
            // The first frame is the explicit send, not an implicit session.update.
            assert_eq!(
                ws.next().await.unwrap().unwrap().into_text().unwrap(),
                "{\"type\":\"test\"}"
            );
            ws.send(Message::Text("{\"type\":\"session.started\"}".into()))
                .await
                .unwrap();
            ws.close(None).await.unwrap();
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut channel = upgrade(&url, HeaderMap::new()).await.unwrap();
            channel.send("{\"type\":\"test\"}").await.unwrap();
            assert_eq!(
                channel.recv().await.unwrap().unwrap()["type"],
                "session.started"
            );
            assert!(channel.recv().await.unwrap().is_none());
            server.await.unwrap();
        })
        .await
        .unwrap();
    }
}
