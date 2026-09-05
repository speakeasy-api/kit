use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::credentials::CredentialStorage;

const AUTH_URL: &str = "https://openrouter.ai/auth";
const EXCHANGE_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
const NAMESPACE: &str = "openrouter";
const IDENTITY: &str = "default";
const MAX_HTTP_BYTES: usize = 16 * 1024;
const MAX_RECORD_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthCommand {
    Login,
    Status,
    Logout { local_only: bool },
}

#[derive(Clone, Deserialize, Serialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credentials {
    pub(crate) api_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct LoginSecrets {
    verifier: String,
    callback_path: String,
}

#[derive(Serialize)]
struct ExchangeRequest<'a> {
    code: &'a str,
    code_verifier: &'a str,
    code_challenge_method: &'static str,
}

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct ExchangeResponse {
    key: String,
    user_id: Option<String>,
}

enum Callback {
    Ignore(u16, &'static str),
    Error,
    Code(Zeroizing<String>),
}

enum CompletionFailure {
    Exchange(String),
    Save(String),
}

impl CompletionFailure {
    const fn response(&self) -> (u16, &'static str) {
        match self {
            Self::Exchange(_) => (
                502,
                "Authentication failed while exchanging the authorization code.",
            ),
            Self::Save(_) => (
                500,
                "Authentication completed, but the API key could not be saved.",
            ),
        }
    }

    fn into_error(self) -> String {
        match self {
            Self::Exchange(error) | Self::Save(error) => error,
        }
    }
}

fn complete_login(
    exchange: Result<Credentials, String>,
    save_record: impl FnOnce(&Credentials) -> Result<(), String>,
) -> Result<Credentials, CompletionFailure> {
    let record = exchange.map_err(CompletionFailure::Exchange)?;
    save_record(&record).map_err(CompletionFailure::Save)?;
    Ok(record)
}

pub(crate) fn execute(
    command: AuthCommand,
    storage: &CredentialStorage,
    active_source: Option<super::OpenRouterApiKeySource>,
    timeout: Duration,
) -> Result<String, String> {
    match command {
        AuthCommand::Login => login(storage, Instant::now() + timeout),
        AuthCommand::Status => {
            if let Some(source) = active_source {
                Ok(format!(
                    "OpenRouter: authenticated (source: {}).\n",
                    source.label()
                ))
            } else {
                match load(storage)? {
                    Some(_) => {
                        Ok("OpenRouter: authenticated (source: stored credentials).\n".into())
                    }
                    None => Ok("OpenRouter: not authenticated.\n".into()),
                }
            }
        }
        AuthCommand::Logout { local_only } => {
            let removed = storage
                .entry(NAMESPACE, IDENTITY)
                .delete()
                .map_err(|error| format!("could not remove OpenRouter credentials: {error}"))?;
            let qualifier = if local_only {
                " locally"
            } else {
                " locally (OpenRouter API keys must be revoked in the dashboard)"
            };
            let mut output = if removed {
                format!("OpenRouter credentials removed{qualifier}.\n")
            } else {
                "OpenRouter: no stored credentials.\n".into()
            };
            if let Some(source) = active_source {
                output.push_str(&format!(
                    "Warning: {} remains active after logout.\n",
                    source.label()
                ));
            }
            Ok(output)
        }
    }
}

pub(crate) fn load(storage: &CredentialStorage) -> Result<Option<Credentials>, String> {
    let Some(bytes) = storage
        .entry(NAMESPACE, IDENTITY)
        .load()
        .map_err(|error| format!("could not load OpenRouter credentials: {error}"))?
    else {
        return Ok(None);
    };
    if bytes.len() > MAX_RECORD_BYTES {
        return Err("stored OpenRouter credentials exceed 16 KiB".into());
    }
    let record: Credentials = serde_json::from_slice(&bytes)
        .map_err(|_| "stored OpenRouter credentials are invalid".to_string())?;
    validate(&record)?;
    Ok(Some(record))
}

fn save(storage: &CredentialStorage, record: &Credentials) -> Result<(), String> {
    validate(record)?;
    let mut bytes = serde_json::to_vec(record)
        .map_err(|_| "could not serialize OpenRouter credentials".to_string())?;
    let result = if bytes.len() > MAX_RECORD_BYTES {
        Err("OpenRouter credentials exceed 16 KiB".into())
    } else {
        storage
            .entry(NAMESPACE, IDENTITY)
            .save(&bytes)
            .map_err(|error| format!("could not store OpenRouter credentials: {error}"))
    };
    bytes.zeroize();
    result
}

fn validate(record: &Credentials) -> Result<(), String> {
    if record.api_key.is_empty()
        || record.api_key.len() > MAX_RECORD_BYTES
        || !record.api_key.is_ascii()
        || record
            .api_key
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err("OpenRouter returned an invalid API key".into());
    }
    Ok(())
}

fn login(storage: &CredentialStorage, deadline: Instant) -> Result<String, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|_| "could not bind the OpenRouter sign-in callback".to_string())?;
    let port = listener
        .local_addr()
        .map_err(|_| "could not inspect the OpenRouter sign-in callback".to_string())?
        .port();
    let mut secrets = LoginSecrets {
        verifier: random_urlsafe::<64>()?,
        callback_path: format!("/callback/{}", random_urlsafe::<32>()?),
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(secrets.verifier.as_bytes()));
    let callback_url = format!("http://localhost:{port}{}", secrets.callback_path);
    let mut auth_url = Zeroizing::new(authorization_url(&callback_url, &challenge)?.to_string());

    println!(
        "Open this URL to authenticate with OpenRouter:\n{}",
        auth_url.as_str()
    );
    super::openai_auth::open_browser_bounded(auth_url.as_str());
    listener
        .set_nonblocking(true)
        .map_err(|_| "could not configure the OpenRouter sign-in callback".to_string())?;

    let result = 'login: loop {
        if Instant::now() >= deadline {
            break Err("OpenRouter sign-in timed out".into());
        }
        match listener.accept() {
            Ok((mut stream, peer)) => {
                if !peer.ip().is_loopback() {
                    respond(&mut stream, 403, "Forbidden");
                    continue;
                }
                match parse_callback(&mut stream, port, &secrets.callback_path, deadline) {
                    Ok(Callback::Ignore(status, message)) => respond(&mut stream, status, message),
                    Ok(Callback::Error) => {
                        respond(&mut stream, 400, "Authorization failed");
                        break 'login Err("OpenRouter declined the authorization request".into());
                    }
                    Ok(Callback::Code(code)) => match complete_login(
                        exchange_code_at(code.as_str(), &secrets.verifier, deadline, EXCHANGE_URL),
                        |record| save(storage, record),
                    ) {
                        Ok(_) => {
                            respond(
                                &mut stream,
                                200,
                                "Authentication complete. You may close this window.",
                            );
                            break 'login Ok("Authenticated with OpenRouter.\n".into());
                        }
                        Err(failure) => {
                            let (status, message) = failure.response();
                            respond(&mut stream, status, message);
                            break 'login Err(failure.into_error());
                        }
                    },
                    Err(_) => respond(&mut stream, 400, "Sign-in callback was rejected."),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break Err("OpenRouter sign-in callback failed".into()),
        }
    };
    auth_url.zeroize();
    secrets.zeroize();
    result
}

fn authorization_url(callback_url: &str, challenge: &str) -> Result<url::Url, String> {
    let mut url = url::Url::parse(AUTH_URL)
        .map_err(|_| "OpenRouter authorization URL is invalid".to_string())?;
    url.query_pairs_mut()
        .append_pair("callback_url", callback_url)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("key_label", "Kit");
    Ok(url)
}

fn parse_callback(
    stream: &mut TcpStream,
    port: u16,
    expected_path: &str,
    deadline: Instant,
) -> Result<Callback, String> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 2048];
    while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let read_timeout =
            callback_read_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok_or_else(|| "OpenRouter sign-in timed out".to_string())?;
        stream
            .set_read_timeout(Some(read_timeout))
            .map_err(|_| "could not configure callback read timeout".to_string())?;
        let count = stream
            .read(&mut chunk)
            .map_err(|_| "could not read callback request".to_string())?;
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
        if request.len() > MAX_HTTP_BYTES {
            break;
        }
    }
    parse_callback_request(&request, port, expected_path)
}

fn callback_read_timeout(remaining: Duration) -> Option<Duration> {
    (!remaining.is_zero()).then(|| remaining.min(Duration::from_secs(2)))
}

fn parse_callback_request(
    request: &[u8],
    port: u16,
    expected_path: &str,
) -> Result<Callback, String> {
    if request.len() > MAX_HTTP_BYTES || request.windows(2).any(|bytes| bytes == b"\n\n") {
        return Err("callback request framing is invalid".into());
    }
    let text =
        std::str::from_utf8(request).map_err(|_| "callback request is not UTF-8".to_string())?;
    let header_end = text
        .find("\r\n\r\n")
        .ok_or_else(|| "callback headers are incomplete".to_string())?;
    if header_end + 4 != text.len() {
        return Err("callback request has a body".into());
    }
    let mut lines = text[..header_end].split("\r\n");
    let mut request_parts = lines.next().unwrap_or_default().split(' ');
    let (Some("GET"), Some(target), Some("HTTP/1.1"), None) = (
        request_parts.next(),
        request_parts.next(),
        request_parts.next(),
        request_parts.next(),
    ) else {
        return Err("only an exact HTTP/1.1 GET is accepted".into());
    };
    if target.contains('#') || target.len() > 8192 {
        return Err("callback target is invalid".into());
    }
    let mut host = None;
    for line in lines {
        if line.starts_with(' ') || line.starts_with('\t') || !line.is_ascii() {
            return Err("callback header folding is rejected".into());
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| "callback header is malformed".to_string())?;
        if name.eq_ignore_ascii_case("host") && host.replace(value.trim()).is_some() {
            return Err("duplicate Host header".into());
        }
        if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
        {
            return Err("callback body framing is rejected".into());
        }
    }
    let expected_host = format!("localhost:{port}");
    if host != Some(expected_host.as_str()) {
        return Err("callback Host does not match the listener".into());
    }
    let url = url::Url::parse(&format!("http://localhost:{port}{target}"))
        .map_err(|_| "callback URL is malformed".to_string())?;
    if url.path() != expected_path {
        return Ok(Callback::Ignore(404, "Not Found"));
    }
    let mut code = None;
    let mut oauth_error = None;
    let mut description = None;
    for (name, value) in url.query_pairs() {
        let slot = match name.as_ref() {
            "code" => &mut code,
            "error" => &mut oauth_error,
            "error_description" => &mut description,
            _ => continue,
        };
        if slot.replace(Zeroizing::new(value.into_owned())).is_some() {
            return Err("callback contains duplicate parameters".into());
        }
    }
    if code.is_some() && oauth_error.is_some() {
        return Err("callback contains both code and error".into());
    }
    if oauth_error.is_some() {
        return Ok(Callback::Error);
    }
    let code = code.ok_or_else(|| "callback is missing an authorization code".to_string())?;
    if code.is_empty()
        || code.len() > 8192
        || !code.is_ascii()
        || code.chars().any(char::is_control)
    {
        return Err("callback authorization code is invalid".into());
    }
    Ok(Callback::Code(code))
}

fn exchange_code_at(
    code: &str,
    verifier: &str,
    deadline: Instant,
    exchange_url: &str,
) -> Result<Credentials, String> {
    let timeout = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_secs(30));
    if timeout.is_zero() {
        return Err("OpenRouter key exchange timed out".into());
    }
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .timeout(timeout)
        .user_agent(concat!("kit/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| "could not build the OpenRouter key exchange client".to_string())?;
    let response = client
        .post(exchange_url)
        .json(&ExchangeRequest {
            code,
            code_verifier: verifier,
            code_challenge_method: "S256",
        })
        .send()
        .map_err(|_| "could not exchange the OpenRouter authorization code".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "OpenRouter key exchange returned {}",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RECORD_BYTES as u64)
    {
        return Err("OpenRouter key exchange response exceeds 16 KiB".into());
    }
    let mut body = Zeroizing::new(Vec::new());
    response
        .take(MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| "could not read the OpenRouter key exchange response".to_string())?;
    if body.len() > MAX_RECORD_BYTES {
        return Err("OpenRouter key exchange response exceeds 16 KiB".into());
    }
    let response: ExchangeResponse = serde_json::from_slice(&body)
        .map_err(|_| "OpenRouter key exchange response is invalid".to_string())?;
    let record = Credentials {
        api_key: response.key.clone(),
    };
    validate(&record)?;
    Ok(record)
}

fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        502 => "Bad Gateway",
        _ => "Internal Server Error",
    };
    let body = format!("<html><body>{message}</body></html>");
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn random_urlsafe<const N: usize>() -> Result<String, String> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes)
        .map_err(|_| "cryptographic randomness is unavailable".to_string())?;
    let value = URL_SAFE_NO_PAD.encode(bytes);
    bytes.zeroize();
    Ok(value)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn store_openrouter_test_credentials(storage: &CredentialStorage) {
        save(
            storage,
            &Credentials {
                api_key: "test-openrouter-key".into(),
            },
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        thread,
        time::{Duration, Instant},
    };

    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use sha2::{Digest as _, Sha256};

    use crate::credentials::CredentialStorage;

    use super::{
        AuthCommand, Callback, Credentials, ExchangeResponse, MAX_RECORD_BYTES, authorization_url,
        callback_read_timeout, complete_login, exchange_code_at, execute, load, parse_callback,
        parse_callback_request, random_urlsafe, save,
    };

    #[test]
    fn authorization_url_uses_callback_pkce_and_key_label() {
        let callback = "http://localhost:43123/callback/unguessable";
        let verifier = "fixed-verifier";
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let url = authorization_url(callback, &challenge).unwrap();
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();

        assert_eq!(
            url.as_str().split('?').next(),
            Some("https://openrouter.ai/auth")
        );
        assert_eq!(
            query.get("callback_url").map(String::as_str),
            Some(callback)
        );
        assert_eq!(
            query.get("code_challenge").map(String::as_str),
            Some(challenge.as_str())
        );
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(query.get("key_label").map(String::as_str), Some("Kit"));
        assert!(!query.contains_key("state"));
    }

    #[test]
    fn callback_path_token_has_256_bits_of_urlsafe_randomness() {
        let first = random_urlsafe::<32>().unwrap();
        let second = random_urlsafe::<32>().unwrap();
        assert_eq!(first.len(), 43);
        assert_ne!(first, second);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        );
    }

    #[test]
    fn callback_requires_the_unguessable_path_and_exact_local_host() {
        let port = 43123;
        let path = "/callback/unguessable";
        let accepted =
            format!("GET {path}?code=oauth-code HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n");
        match parse_callback_request(accepted.as_bytes(), port, path).unwrap() {
            Callback::Code(code) => assert_eq!(code.as_str(), "oauth-code"),
            _ => panic!("expected authorization code"),
        }

        let wrong_path =
            format!("GET /callback/guessed?code=stolen HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n");
        assert!(matches!(
            parse_callback_request(wrong_path.as_bytes(), port, path).unwrap(),
            Callback::Ignore(404, _)
        ));
        let wrong_host =
            format!("GET {path}?code=stolen HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        assert!(parse_callback_request(wrong_host.as_bytes(), port, path).is_err());
        let duplicate =
            format!("GET {path}?code=one&code=two HTTP/1.1\r\nHost: localhost:{port}\r\n\r\n");
        assert!(parse_callback_request(duplicate.as_bytes(), port, path).is_err());
    }

    #[test]
    fn exchange_posts_the_exact_pkce_json_and_returns_key() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 2048];
            loop {
                let count = stream.read(&mut chunk).unwrap();
                request.extend_from_slice(&chunk[..count]);
                let Some(header_end) = request.windows(4).position(|v| v == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end + 4]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                    })
                    .unwrap();
                if request.len() >= header_end + 4 + length {
                    break;
                }
            }
            let header_end = request.windows(4).position(|v| v == b"\r\n\r\n").unwrap();
            let request_line = String::from_utf8_lossy(&request[..header_end])
                .lines()
                .next()
                .unwrap()
                .to_owned();
            let body: serde_json::Value =
                serde_json::from_slice(&request[header_end + 4..]).unwrap();
            let response_body = r#"{"key":"sk-or-v1-stored","user_id":"user-123"}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .unwrap();
            (request_line, body)
        });

        let record = exchange_code_at(
            "authorization-code",
            "pkce-verifier",
            Instant::now() + Duration::from_secs(5),
            &format!("http://{address}/api/v1/auth/keys"),
        )
        .unwrap();
        assert_eq!(record.api_key, "sk-or-v1-stored");
        let (request_line, body) = server.join().unwrap();
        assert_eq!(request_line, "POST /api/v1/auth/keys HTTP/1.1");
        assert_eq!(
            body,
            serde_json::json!({
                "code": "authorization-code",
                "code_verifier": "pkce-verifier",
                "code_challenge_method": "S256",
            })
        );
    }

    #[test]
    fn exchange_response_allows_harmless_extra_fields_and_optional_user_id() {
        for body in [
            br#"{"key":"secret"}"#.as_slice(),
            br#"{"key":"secret","user_id":null}"#.as_slice(),
            br#"{"key":"secret","user_id":"user"}"#.as_slice(),
        ] {
            let response: ExchangeResponse = serde_json::from_slice(body).unwrap();
            assert_eq!(response.key, "secret");
        }
        let response: ExchangeResponse =
            serde_json::from_slice(br#"{"key":"secret","user_id":null,"extra":true}"#).unwrap();
        assert_eq!(response.key, "secret");
    }

    #[test]
    fn callback_reports_exchange_and_save_failures_separately() {
        let failure = complete_login(Err("exchange failed".into()), |_| {
            panic!("save must not run after an exchange failure")
        })
        .err()
        .unwrap();
        assert_eq!(
            failure.response(),
            (
                502,
                "Authentication failed while exchanging the authorization code."
            )
        );
        assert_eq!(failure.into_error(), "exchange failed");

        let failure = complete_login(
            Ok(Credentials {
                api_key: "secret".into(),
            }),
            |_| Err("save failed".into()),
        )
        .err()
        .unwrap();
        assert_eq!(
            failure.response(),
            (
                500,
                "Authentication completed, but the API key could not be saved."
            )
        );
        assert_eq!(failure.into_error(), "save failed");
    }

    #[test]
    fn callback_read_timeout_is_clamped_to_the_login_deadline() {
        assert_eq!(callback_read_timeout(Duration::ZERO), None);
        assert_eq!(
            callback_read_timeout(Duration::from_millis(25)),
            Some(Duration::from_millis(25))
        );
        assert_eq!(
            callback_read_timeout(Duration::from_secs(10)),
            Some(Duration::from_secs(2))
        );
    }

    #[test]
    fn callback_slow_drip_cannot_extend_the_overall_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let writer = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            for _ in 0..20 {
                if stream.write_all(b"G").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let (mut stream, _) = listener.accept().unwrap();
        let started = Instant::now();
        assert!(
            parse_callback(
                &mut stream,
                address.port(),
                "/callback/token",
                started + Duration::from_millis(60),
            )
            .is_err()
        );
        let elapsed = started.elapsed();
        drop(stream);
        writer.join().unwrap();
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }

    #[test]
    fn auth_status_and_logout_report_the_active_source_without_the_key() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().join("credentials"));
        save(
            &storage,
            &Credentials {
                api_key: "stored-secret".into(),
            },
        )
        .unwrap();
        let status = execute(AuthCommand::Status, &storage, None, Duration::from_secs(1)).unwrap();
        assert!(status.contains("source: stored credentials"));
        assert!(!status.contains("stored-secret"));

        for source in [
            crate::provider::OpenRouterApiKeySource::Flag,
            crate::provider::OpenRouterApiKeySource::Environment,
        ] {
            let status = execute(
                AuthCommand::Status,
                &storage,
                Some(source),
                Duration::from_secs(1),
            )
            .unwrap();
            assert!(status.contains(source.label()));
            assert!(!status.contains("stored-secret"));
            let logout = execute(
                AuthCommand::Logout { local_only: false },
                &storage,
                Some(source),
                Duration::from_secs(1),
            )
            .unwrap();
            assert!(logout.contains("remains active after logout"));
            assert!(logout.contains(source.label()));
            assert!(!logout.contains("stored-secret"));
        }
    }

    #[test]
    fn stored_record_is_strict_bounded_validated_and_not_rewritten() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().join("credentials"));
        let entry = storage.entry("openrouter", "default");
        let original = b"{ \"api_key\": \"stored-key\" }\n";
        entry.save(original).unwrap();
        assert_eq!(load(&storage).unwrap().unwrap().api_key, "stored-key");
        assert_eq!(entry.load().unwrap().unwrap().as_slice(), original);

        entry
            .save(br#"{"api_key":"stored-key","version":1}"#)
            .unwrap();
        assert!(load(&storage).is_err());
        entry.save(&vec![b'x'; MAX_RECORD_BYTES + 1]).unwrap();
        assert!(load(&storage).is_err());
        assert!(
            save(
                &storage,
                &Credentials {
                    api_key: "bad key".into()
                }
            )
            .is_err()
        );
        assert!(
            save(
                &storage,
                &Credentials {
                    api_key: "x".repeat(MAX_RECORD_BYTES),
                },
            )
            .is_err()
        );
    }

    #[test]
    fn credential_debug_is_redacted() {
        let record = Credentials {
            api_key: "very-secret-key".into(),
        };
        let debug = format!("{record:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("very-secret-key"));
    }
}
