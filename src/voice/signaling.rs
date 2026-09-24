//! Direct ChatGPT-subscription WebRTC signaling (Frameless Bidi v3 / AVAS).
//!
//! Wire contract: upstream `codex-api/src/endpoint/realtime_call.rs`,
//! `realtime_websocket/{methods_frameless_bidi,protocol_frameless_bidi,methods_common}.rs`, and
//! `core/src/realtime_conversation.rs::realtime_request_headers`. The subscription
//! `/backend-api` route takes JSON, **not** the public API's multipart form.
//! No API key, Codex credential file, or Codex app-server is used here.
//!
//! HTTP creation is only the media-signaling step. Upstream attaches an authenticated
//! WebSocket at `wss://api.openai.com/v1/live/{call_id}` for control and delegation,
//! retaining the HTTP call's credentials. A native WebRTC data channel has NOT been
//! validated as a replacement. This module prepares that sideband's URL and headers;
//! it does not implement its WebSocket transport or reconnect lifecycle.

use std::{io, time::Duration};

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, LOCATION, USER_AGENT};
use serde::Serialize;
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::{credentials::CredentialStorage, provider::openai_auth as auth};

const ENDPOINT: &str =
    "https://chatgpt.com/backend-api/codex/realtime/calls?intent=quicksilver&architecture=avas";
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SDP_BYTES: usize = 1024 * 1024;

/// SDP answer and call identifier returned by the subscription service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallAnswer {
    pub sdp: String,
    pub call_id: String,
    /// Server-side control connection; never send these credentials to a UI/browser.
    pub sideband: SidebandConnection,
}

/// Connection parameters, not a connected socket. Headers retain the exact OAuth
/// account used for HTTP creation. The WebSocket transport adds handshake headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebandConnection {
    pub url: reqwest::Url,
    pub headers: HeaderMap,
}

/// Establish with the application's resolved Kit OAuth credential storage.
/// Authentication refresh runs off the async executor. No API-key fallback exists.
pub async fn establish_with_storage(
    offer_sdp: &str,
    storage: &CredentialStorage,
) -> io::Result<CallAnswer> {
    if offer_sdp.trim().is_empty() || offer_sdp.len() > MAX_SDP_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid SDP offer size",
        ));
    }
    let storage = storage.clone();
    let credentials = tokio::task::spawn_blocking(move || {
        auth::access_token(&storage, auth::checked_deadline(TIMEOUT)?)
    })
    .await
    .map_err(|_| io::Error::other("subscription authentication worker failed"))?
    .map_err(|error| io::Error::other(error.to_string()))?;
    credentials
        .binding()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let account_id = credentials.account_id().ok_or_else(|| {
        io::Error::other("OpenAI subscription account is missing; run `kit auth login openai`")
    })?;
    let headers = request_headers(credentials.access_token(), account_id)?;
    establish_at(offer_sdp, headers, ENDPOINT).await
}

/// HTTP boundary shared by subscription signaling and offline loopback tests.
async fn establish_at(
    offer_sdp: &str,
    headers: HeaderMap,
    endpoint: &str,
) -> io::Result<CallAnswer> {
    let mut sideband_headers = headers.clone();
    sideband_headers.remove(CONTENT_TYPE);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| io::Error::other("could not create realtime HTTP client"))?;
    let body = serde_json::to_vec(&call_request(offer_sdp))
        .map_err(|_| io::Error::other("could not encode realtime call"))?;
    let mut response = client
        .post(endpoint)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|_| io::Error::other("realtime signaling transport failed"))?;
    if !response.status().is_success() {
        return Err(denial_error(response).await);
    }
    let location = response
        .headers()
        .get(LOCATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| io::Error::other("realtime call response missing or invalid Location"))?;
    let call_id = call_id_from_location(location)?;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| io::Error::other("could not read realtime SDP answer"))?
    {
        if chunk.len() > MAX_SDP_BYTES - body.len() {
            return Err(io::Error::other("realtime SDP answer exceeds size limit"));
        }
        body.extend_from_slice(&chunk);
    }
    let sdp = String::from_utf8(body)
        .map_err(|_| io::Error::other("realtime SDP answer is not UTF-8"))?;
    if sdp.trim().is_empty() {
        return Err(io::Error::other("realtime SDP answer is empty"));
    }
    let sideband = SidebandConnection {
        url: sideband_url(&call_id)?,
        headers: sideband_headers,
    };
    Ok(CallAnswer {
        sdp,
        call_id,
        sideband,
    })
}

/// Read only a bounded prefix; never report request headers or raw bodies.
async fn denial_error(mut response: reqwest::Response) -> io::Error {
    const LIMIT: usize = 16 * 1024;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| diagnostic_text(v, 120))
        .unwrap_or_else(|| "absent".into());
    let mut details = vec![format!("content-type={content_type}")];
    for name in ["x-request-id", "request-id", "cf-ray"] {
        if let Some(v) = response.headers().get(name).and_then(|v| v.to_str().ok()) {
            details.push(format!("{name}={}", diagnostic_text(v, 128)));
        }
    }
    if response
        .headers()
        .get("cf-mitigated")
        .is_some_and(|v| v == "challenge")
    {
        details.push("edge challenge indicated".into());
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(LIMIT - body.len());
                body.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    details.push("body truncated at diagnostic limit".into());
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => {
                details.push("body read failed".into());
                break;
            }
        }
    }
    details.push(format!("body-bytes-read={}", body.len()));
    let text = clean_controls(&String::from_utf8_lossy(&body));
    let text = text.trim_start();
    let lower = text.to_ascii_lowercase();
    if content_type.to_ascii_lowercase().starts_with("text/html")
        || lower.starts_with("<!doctype html")
        || lower.starts_with('<')
    {
        details.push("HTML denial".into());
        if let Some(start) = lower.find("<title>") {
            let start = start + 7;
            if let Some(end) = lower[start..].find("</title>") {
                let title = &text[start..start + end];
                if !title.contains(['<', '>']) {
                    details.push(format!("title={}", diagnostic_text(title, 512)));
                }
            }
        }
    } else if let Ok(value) = serde_json::from_str::<Value>(text) {
        details.push("JSON error".into());
        error_fields(&value, 0, &mut details);
    } else if text.starts_with(['{', '[']) {
        details.push("malformed or truncated JSON error".into());
    } else if !text.is_empty() {
        details.push(format!("message={}", diagnostic_text(text, 1024)));
    }
    io::Error::other(format!(
        "realtime signaling returned HTTP {status}; {}",
        details.join("; ")
    ))
}

fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', '_'], "");
    [
        "authorization",
        "token",
        "apikey",
        "password",
        "secret",
        "credential",
        "cookie",
        "sdp",
    ]
    .iter()
    .any(|marker| key.contains(marker))
}

fn error_fields(value: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > 8 || out.len() >= 16 {
        return;
    }
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if out.len() >= 16 {
                    break;
                }
                if sensitive_key(key) {
                    continue;
                }
                match value {
                    Value::String(text) => out.push(format!(
                        "{}={}",
                        diagnostic_text(key, 80),
                        diagnostic_text(text, 512)
                    )),
                    Value::Number(n) => out.push(format!("{}={n}", diagnostic_text(key, 80))),
                    Value::Object(_) | Value::Array(_) => error_fields(value, depth + 1, out),
                    _ => {}
                }
            }
        }
        Value::Array(values) => {
            for value in values.iter().take(16) {
                error_fields(value, depth + 1, out);
            }
        }
        Value::String(text) => out.push(format!("message={}", diagnostic_text(text, 512))),
        _ => {}
    }
}

fn clean_controls(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .filter(|c| !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .collect()
}

/// Conservative best-effort redaction: omit credential labels and their remainder,
/// and mask opaque words. Arbitrary prose cannot be guaranteed secret-free.
fn diagnostic_text(text: &str, limit: usize) -> String {
    let text = clean_controls(text);
    let mut out = String::new();
    for word in text.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        let label = lower.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if matches!(
            label,
            "bearer"
                | "authorization"
                | "token"
                | "access_token"
                | "refresh_token"
                | "api-key"
                | "api_key"
                | "password"
                | "secret"
                | "sdp"
        ) || [
            "authorization:",
            "authorization=",
            "token=",
            "token:",
            "api_key=",
            "api_key:",
            "api-key:",
            "password:",
            "secret:",
            "sdp:",
            "api-key=",
            "password=",
            "secret=",
            "sdp=",
            "v=0",
            "a=ice-",
            "a=fingerprint:",
            "m=audio",
        ]
        .iter()
        .any(|s| lower.contains(s))
        {
            out.push_str(" [redacted remainder]");
            break;
        }
        let opaque =
            word.len() >= 32 && word.chars().filter(|c| c.is_ascii_alphanumeric()).count() >= 24;
        let word = if opaque || lower.contains("sk-") || lower.contains("eyj") {
            "[redacted]"
        } else {
            word
        };
        if out.len() + word.len() + 1 > limit {
            out.push_str(" [truncated]");
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn request_headers(token: &str, account_id: &str) -> io::Result<HeaderMap> {
    let bearer = Zeroizing::new(format!("Bearer {token}"));
    let mut authorization = HeaderValue::from_str(&bearer)
        .map_err(|_| io::Error::other("invalid subscription authorization header"))?;
    authorization.set_sensitive(true);
    let mut account = HeaderValue::from_str(account_id)
        .map_err(|_| io::Error::other("invalid subscription account header"))?;
    account.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, authorization);
    headers.insert("chatgpt-account-id", account);
    headers.insert("openai-alpha", HeaderValue::from_static("quicksilver=v2"));
    headers.insert("originator", HeaderValue::from_static("kit"));
    // Upstream transport supplies a User-Agent in addition to originator. Identify
    // Kit honestly; do not copy Codex identity or browser challenge headers.
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("kit/", env!("CARGO_PKG_VERSION"))),
    );
    // Upstream provider headers also carry a distinct client version on HTTP and
    // sideband requests. Use Kit's build version, not the upstream Codex version.
    headers.insert(
        "version",
        HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

fn call_request(sdp: &str) -> Value {
    // The gpt-live-1-codex v3 session uses the upstream v1 voice set;
    // cove is its default. Marin belongs to the distinct v2 voice set.
    let output = Map::from_iter([("voice".into(), Value::from("cove"))]);
    let audio = Map::from_iter([("output".into(), Value::Object(output))]);
    let delegation = Map::from_iter([("type".into(), Value::from("client"))]);
    let session = Map::from_iter([
        ("model".into(), Value::from("gpt-live-1-codex")),
        (
            "instructions".into(),
            Value::from(
                "You are Kit's voice interface. Delegate coding work to the agent using handoffs.",
            ),
        ),
        ("audio".into(), Value::Object(audio)),
        ("delegation".into(), Value::Object(delegation)),
    ]);
    Value::Object(Map::from_iter([
        ("sdp".into(), Value::from(sdp)),
        ("session".into(), Value::Object(session)),
    ]))
}

fn sideband_url(call_id: &str) -> io::Result<reqwest::Url> {
    if call_id.is_empty() || matches!(call_id, "." | "..") {
        return Err(io::Error::other("invalid realtime call id"));
    }
    let mut url = reqwest::Url::parse("wss://api.openai.com/v1/live")
        .map_err(|_| io::Error::other("invalid realtime sideband URL"))?;
    url.path_segments_mut()
        .map_err(|_| io::Error::other("invalid realtime sideband path"))?
        .push(call_id);
    Ok(url)
}

fn call_id_from_location(location: &str) -> io::Result<String> {
    location
        .split('?')
        .next()
        .unwrap_or(location)
        .rsplit('/')
        .find(|segment| {
            (segment.starts_with("rtc_") && segment.len() > 4)
                || (segment.len() == 36
                    && segment.char_indices().all(|(index, ch)| match index {
                        8 | 13 | 18 | 23 => ch == '-',
                        _ => ch.is_ascii_hexdigit(),
                    }))
        })
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("realtime call Location does not contain a call id"))
}

/// Client delegation decoded from a Frameless Bidi `delegation.created` item.
/// Upstream maps the item ID to both `handoff_id` and `item_id` internally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffRequested {
    pub handoff_id: String,
    pub item_id: String,
    pub input_transcript: String,
}

/// Match upstream v3: ignore unsupported/malformed events and non-client targets;
/// concatenate only string `input_text` content. Invalid JSON remains an error.
pub fn parse_handoff(payload: &str) -> serde_json::Result<Option<HandoffRequested>> {
    let event: Value = serde_json::from_str(payload)?;
    Ok(parse_delegation(&event))
}

fn parse_delegation(event: &Value) -> Option<HandoffRequested> {
    if event.get("type").and_then(Value::as_str) != Some("delegation.created") {
        return None;
    }
    let item = event.get("item")?.as_object()?;
    if item.get("type").and_then(Value::as_str) != Some("delegation")
        || item.get("target").and_then(Value::as_str) != Some("client")
    {
        return None;
    }
    let item_id = item.get("id")?.as_str()?.to_owned();
    let input_transcript = item
        .get("content")?
        .as_array()?
        .iter()
        .filter(|content| content.get("type").and_then(Value::as_str) == Some("input_text"))
        .filter_map(|content| content.get("text").and_then(Value::as_str))
        .collect::<String>();
    Some(HandoffRequested {
        handoff_id: item_id.clone(),
        item_id,
        input_transcript,
    })
}

/// Semantic stream selectors defined by the Frameless Bidi protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAppendChannel {
    Speakable,
}

/// Build ordered v3 result frames. Send every frame in order over the sideband.
/// Upstream limits text chunks to 500 UTF-8 bytes. A final result uses this same
/// shape: no v1 final-message marker and no `response.create` event is added.
pub fn delegation_context_append(
    delegation_item_id: &str,
    text: &str,
    channel: Option<ContextAppendChannel>,
) -> Vec<Value> {
    context_append_frames(
        "delegation.context.append",
        Some(delegation_item_id),
        text,
        channel,
    )
}

fn context_append_frames(
    event_type: &str,
    delegation_item_id: Option<&str>,
    text: &str,
    channel: Option<ContextAppendChannel>,
) -> Vec<Value> {
    let mut frames = Vec::new();
    let mut rest = text;
    loop {
        let mut end = rest.len().min(500);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let content = Value::Object(Map::from_iter([
            ("type".into(), Value::from("input_text")),
            ("text".into(), Value::from(&rest[..end])),
        ]));
        let mut frame = Map::from_iter([
            ("type".into(), Value::from(event_type)),
            ("content".into(), Value::Array(vec![content])),
        ]);
        if let Some(id) = delegation_item_id {
            frame.insert("delegation_item_id".into(), Value::from(id));
        }
        if let Some(channel) = channel {
            let channel = match channel {
                ContextAppendChannel::Speakable => "speakable",
            };
            frame.insert("channel".into(), Value::from(channel));
        }
        frames.push(Value::Object(frame));
        rest = &rest[end..];
        if rest.is_empty() {
            break;
        }
    }
    frames
}

/// Send before closing the v3 sideband WebSocket, as upstream's writer does.
pub fn session_close() -> Value {
    Value::Object(Map::from_iter([(
        "type".into(),
        Value::from("session.close"),
    )]))
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
    use serde_json::json;

    /// A real HTTP peer, with synthetic credentials only. No OAuth, microphone,
    /// external service, retry, or application-state instrumentation is involved.
    async fn fake_call(status: u16, extra_headers: &str, body: &str) -> io::Result<CallAnswer> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/codex/realtime/calls?intent=quicksilver&architecture=avas",
            listener.local_addr().unwrap()
        );
        let response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
            body.len()
        );
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let header_end = loop {
                let mut byte = [0];
                stream.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
                assert!(request.len() < 8192);
                if request.ends_with(b"\r\n\r\n") {
                    break request.len();
                }
            };
            let headers = String::from_utf8(request.clone()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(|n| n.parse::<usize>().unwrap())
                })
                .unwrap();
            assert!(length < 8192);
            request.resize(header_end + length, 0);
            stream.read_exact(&mut request[header_end..]).await.unwrap();
            assert!(headers.starts_with(
                "POST /codex/realtime/calls?intent=quicksilver&architecture=avas HTTP/1.1\r\n"
            ));
            for header in [
                "authorization: Bearer synthetic-token",
                "chatgpt-account-id: synthetic-account",
                "openai-alpha: quicksilver=v2",
                "originator: kit",
                concat!("user-agent: kit/", env!("CARGO_PKG_VERSION")),
                concat!("version: ", env!("CARGO_PKG_VERSION")),
                "content-type: application/json",
            ] {
                assert!(
                    headers.lines().any(|line| line == header),
                    "missing exact expected request header"
                );
            }
            assert_eq!(
                serde_json::from_slice::<Value>(&request[header_end..]).unwrap(),
                call_request("synthetic-offer")
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let result = establish_at(
            "synthetic-offer",
            request_headers("synthetic-token", "synthetic-account").unwrap(),
            &endpoint,
        )
        .await;
        peer.await.unwrap();
        result
    }

    #[tokio::test]
    async fn loopback_signaling_sends_contract_and_retains_identity() {
        let answer = fake_call(201, "Location: /calls/rtc_local\r\n", "synthetic-answer")
            .await
            .unwrap();
        assert_eq!(answer.sdp, "synthetic-answer");
        assert_eq!(answer.call_id, "rtc_local");
        assert_eq!(
            answer.sideband.headers[USER_AGENT],
            concat!("kit/", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            answer.sideband.headers["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(answer.sideband.headers["originator"], "kit");
        assert!(answer.sideband.headers[AUTHORIZATION].is_sensitive());
        assert!(!answer.sideband.headers.contains_key(CONTENT_TYPE));
    }

    #[tokio::test]
    async fn loopback_denials_report_sanitized_server_reasons() {
        for (status, headers, body, expected) in [
            (
                403,
                "Content-Type: application/json\r\nx-request-id: req-safe-123\r\n",
                r#"{"unexpected":{"code":"new_policy_gate","type":"account_policy","explanation":"Voice disabled for this workspace"}}"#,
                "Voice disabled for this workspace",
            ),
            (
                401,
                "",
                r#"{"error":{"code":"token_expired"}}"#,
                "token_expired",
            ),
            (
                403,
                "",
                "This workspace requires administrator approval",
                "administrator approval",
            ),
            (
                403,
                "",
                r#"{"detail":[{"message":"Region not supported"}]}"#,
                "Region not supported",
            ),
            (
                403,
                "Content-Type: text/html\r\n",
                "<html><title>Access blocked</title><body>private-body</body></html>",
                "title=Access blocked",
            ),
            (429, "", "", "body-bytes-read=0"),
        ] {
            let error = fake_call(status, headers, body)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains(&format!("HTTP {status}")), "{error}");
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("private-body"));
            if headers.contains("x-request-id") {
                assert!(error.contains("req-safe-123"));
            }
        }
        let error = fake_call(
            403,
            "",
            &format!("Workspace disabled {}", "x".repeat(32 * 1024)),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("HTTP 403"));
        assert!(error.contains("truncated"));
        assert!(error.contains("Workspace disabled"));
        assert!(error.len() < 2000);
    }

    #[tokio::test]
    async fn denial_redacts_secrets_and_terminal_controls() {
        let body = r#"{"error":{"message":"Denied \u001b[31m Bearer private-bearer","code":"unfamiliar_gate"},"access_token":"private-token","offer_sdp":"private-sdp","detail":"sk-private-key","other":"eyJhbGciOiJIUzI1NiJ9.payload.signature"}"#;
        let error = fake_call(403, "x-request-id: Bearer private-id\r\n", body)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unfamiliar_gate"));
        for secret in [
            "private-bearer",
            "private-token",
            "private-sdp",
            "sk-private-key",
            "eyJ",
            "private-id",
            "\u{1b}",
        ] {
            assert!(!error.contains(secret), "{error}");
        }
        for secret in [
            "authorization: abc",
            "token=abc",
            "api-key=abc",
            "v=0 m=audio private",
            "a=ice-pwd:private",
        ] {
            assert!(diagnostic_text(secret, 512).contains("redacted"));
        }
        assert!(!diagnostic_text("denied\u{9b}\u{202e}now", 512).contains(['\u{9b}', '\u{202e}']));
        assert!(diagnostic_text(&"é".repeat(2000), 512).len() < 540);
        let malformed = fake_call(403, "", r#"{"access_token":"private-malformed""#)
            .await
            .unwrap_err()
            .to_string();
        assert!(malformed.contains("malformed or truncated JSON"));
        assert!(!malformed.contains("private-malformed"));
        let markup = fake_call(
            403,
            "",
            "<title>Blocked</title><script>private-script</script>",
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(markup.contains("title=Blocked"));
        assert!(!markup.contains("private-script"));
    }

    #[test]
    fn subscription_request_matches_frameless_backend_contract() {
        let headers = request_headers("synthetic-token", "synthetic-account").unwrap();
        assert_eq!(headers["openai-alpha"], "quicksilver=v2");
        assert_eq!(headers[CONTENT_TYPE], "application/json");
        assert!(headers[AUTHORIZATION].is_sensitive());
        assert!(headers["chatgpt-account-id"].is_sensitive());
        assert!(request_headers("bad\ntoken", "account").is_err());
        assert_eq!(
            call_request("offer"),
            json!({
                "sdp": "offer",
                "session": {
                    "model": "gpt-live-1-codex",
                    "instructions": "You are Kit's voice interface. Delegate coding work to the agent using handoffs.",
                    "audio": {"output": {"voice": "cove"}},
                    "delegation": {"type": "client"}
                }
            })
        );
    }

    #[test]
    fn location_and_sideband_match_upstream() {
        assert_eq!(
            call_id_from_location("/calls/rtc_123?x=1").unwrap(),
            "rtc_123"
        );
        let uuid = "12345678-1234-1234-1234-123456789abc";
        assert_eq!(
            call_id_from_location(&format!("/calls/{uuid}/")).unwrap(),
            uuid
        );
        for location in ["", "/calls/rtc_", "/calls/nope?x=rtc_123"] {
            assert!(call_id_from_location(location).is_err());
        }
        assert_eq!(
            sideband_url("rtc_123").unwrap().as_str(),
            "wss://api.openai.com/v1/live/rtc_123"
        );
        assert_eq!(
            sideband_url("../../admin").unwrap().as_str(),
            "wss://api.openai.com/v1/live/..%2F..%2Fadmin"
        );
        assert!(sideband_url("..").is_err());
    }

    #[test]
    fn parses_only_client_delegation_items() {
        let event = json!({"type":"delegation.created", "item":{
            "type":"delegation", "target":"client", "id":"d1", "content":[
                {"type":"input_text","text":"fix "},
                {"type":"output_text","text":"ignored"},
                {"type":"input_text","text":17},
                {"type":"input_text","text":"tests"}
            ]
        }});
        assert_eq!(
            parse_handoff(&event.to_string()).unwrap(),
            Some(HandoffRequested {
                handoff_id: "d1".into(),
                item_id: "d1".into(),
                input_transcript: "fix tests".into()
            })
        );
        let mut other = event.clone();
        other["item"]["target"] = json!("server");
        assert_eq!(parse_handoff(&other.to_string()).unwrap(), None);
        assert_eq!(
            parse_handoff(r#"{"type":"delegation.created"}"#).unwrap(),
            None
        );
        assert_eq!(
            parse_handoff(r#"{"type":"conversation.handoff.requested"}"#).unwrap(),
            None
        );
        assert!(parse_handoff("{").is_err());
    }

    #[test]
    fn result_frames_have_v3_shapes_and_utf8_chunks() {
        assert_eq!(
            delegation_context_append("d1", "done", Some(ContextAppendChannel::Speakable)),
            vec![json!({
                "type":"delegation.context.append", "delegation_item_id":"d1", "channel":"speakable",
                "content":[{"type":"input_text","text":"done"}]
            })]
        );
        let text = format!("{}é🙂", "a".repeat(499));
        let frames = delegation_context_append("d1", &text, Some(ContextAppendChannel::Speakable));
        let chunks: Vec<_> = frames
            .iter()
            .map(|frame| frame["content"][0]["text"].as_str().unwrap())
            .collect();
        assert!(chunks.iter().all(|chunk| chunk.len() <= 500));
        assert_eq!(chunks.concat(), text);
        assert_eq!(delegation_context_append("d1", "", None).len(), 1);
        assert_eq!(session_close(), json!({"type":"session.close"}));
    }
}
