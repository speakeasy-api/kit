use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq as _;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

#[cfg(not(windows))]
use std::process::{Command, Stdio};

use crate::credentials::CredentialStorage;

const DASHBOARD_URL: &str = "https://app.getgram.ai/";
const VERIFY_URL: &str = "https://app.getgram.ai/rpc/keys.verify";
const NAMESPACE: &str = "speakeasy";
const IDENTITY: &str = "default";
const MAX_HTTP_BYTES: usize = 16 * 1024;
const MAX_RECORD_BYTES: usize = 16 * 1024;
const MAX_VERIFY_BYTES: usize = 64 * 1024;

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
    pub(crate) project: String,
    pub(crate) email: Option<String>,
    pub(crate) organization_id: Option<String>,
}

#[derive(Zeroize, ZeroizeOnDrop)]
struct CallbackCredentials {
    api_key: String,
    project: Option<String>,
    email: Option<String>,
    organization_id: Option<String>,
}

#[derive(Deserialize)]
struct VerifyResponse {
    organization: VerifyOrganization,
    projects: Vec<VerifyProject>,
    scopes: Vec<String>,
}

#[derive(Deserialize)]
struct VerifyOrganization {
    id: String,
}

#[derive(Deserialize)]
struct VerifyProject {
    slug: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("api_key", &"[REDACTED]")
            .field("project", &self.project)
            .field("email", &self.email)
            .field("organization_id", &self.organization_id)
            .finish()
    }
}

pub(crate) fn execute(
    command: AuthCommand,
    storage: &CredentialStorage,
    timeout: Duration,
) -> Result<String, String> {
    match command {
        AuthCommand::Login => login(storage, Instant::now() + timeout),
        AuthCommand::Status => match load(storage)? {
            Some(record) => Ok(format!(
                "Speakeasy: authenticated{} for project {}.\n",
                record
                    .email
                    .as_deref()
                    .map(|email| format!(" as {email}"))
                    .unwrap_or_default(),
                record.project
            )),
            None => Ok("Speakeasy: not authenticated.\n".into()),
        },
        AuthCommand::Logout { local_only } => {
            let removed = storage
                .entry(NAMESPACE, IDENTITY)
                .delete()
                .map_err(|error| format!("could not remove Speakeasy credentials: {error}"))?;
            let qualifier = if local_only {
                " locally"
            } else {
                " locally (Speakeasy API keys must be revoked in the dashboard)"
            };
            Ok(if removed {
                format!("Speakeasy credentials removed{qualifier}.\n")
            } else {
                "Speakeasy: no stored credentials.\n".into()
            })
        }
    }
}

pub(crate) fn load(storage: &CredentialStorage) -> Result<Option<Credentials>, String> {
    let Some(bytes) = storage
        .entry(NAMESPACE, IDENTITY)
        .load()
        .map_err(|error| format!("could not load Speakeasy credentials: {error}"))?
    else {
        return Ok(None);
    };
    if bytes.len() > MAX_RECORD_BYTES {
        return Err("stored Speakeasy credentials exceed 16 KiB".into());
    }
    let record: Credentials = serde_json::from_slice(&bytes)
        .map_err(|_| "stored Speakeasy credentials are invalid".to_string())?;
    validate(&record)?;
    Ok(Some(record))
}

fn save(storage: &CredentialStorage, record: &Credentials) -> Result<(), String> {
    validate(record)?;
    let mut bytes = serde_json::to_vec(record)
        .map_err(|_| "could not serialize Speakeasy credentials".to_string())?;
    let result = if bytes.len() > MAX_RECORD_BYTES {
        Err("Speakeasy credentials exceed 16 KiB".into())
    } else {
        storage
            .entry(NAMESPACE, IDENTITY)
            .save(&bytes)
            .map_err(|error| format!("could not store Speakeasy credentials: {error}"))
    };
    bytes.zeroize();
    result
}

fn validate(record: &Credentials) -> Result<(), String> {
    if !valid_api_key(&record.api_key) {
        return Err("Speakeasy returned an invalid Gram API key".into());
    }
    if !valid_project(&record.project) {
        return Err("Speakeasy returned an invalid project slug".into());
    }
    for value in [record.email.as_deref(), record.organization_id.as_deref()]
        .into_iter()
        .flatten()
    {
        if value.is_empty()
            || value.len() > 512
            || !value.is_ascii()
            || value.contains(['\r', '\n'])
        {
            return Err("Speakeasy returned invalid account metadata".into());
        }
    }
    Ok(())
}

fn valid_api_key(value: &str) -> bool {
    ["gram_live_", "gram_test_", "gram_local_"]
        .iter()
        .any(|prefix| {
            value.strip_prefix(prefix).is_some_and(|secret| {
                secret.len() == 64
                    && secret
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            })
        })
}

fn valid_project(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn verify_credentials(
    callback: CallbackCredentials,
    deadline: Instant,
) -> Result<Credentials, String> {
    verify_credentials_at(callback, deadline, VERIFY_URL)
}

fn verify_credentials_at(
    callback: CallbackCredentials,
    deadline: Instant,
    verify_url: &str,
) -> Result<Credentials, String> {
    let timeout = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_secs(30));
    if timeout.is_zero() {
        return Err("Speakeasy credential verification timed out".into());
    }
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout.min(Duration::from_secs(10)))
        .timeout(timeout)
        .user_agent(concat!("kit/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| "could not build the Speakeasy verification client".to_string())?;
    let response = client
        .get(verify_url)
        .header("Gram-Key", &callback.api_key)
        .send()
        .map_err(|_| "could not verify the Speakeasy API key".to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "Speakeasy API key verification returned {}",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_VERIFY_BYTES as u64)
    {
        return Err("Speakeasy verification response is too large".into());
    }
    let mut body = Zeroizing::new(Vec::new());
    response
        .take(MAX_VERIFY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| "could not read the Speakeasy verification response".to_string())?;
    if body.len() > MAX_VERIFY_BYTES {
        return Err("Speakeasy verification response is too large".into());
    }
    let verified: VerifyResponse = serde_json::from_slice(&body)
        .map_err(|_| "Speakeasy verification response is invalid".to_string())?;
    if !verified
        .scopes
        .iter()
        .any(|scope| matches!(scope.as_str(), "producer" | "chat"))
    {
        return Err("Speakeasy API key does not allow chat completions".into());
    }
    if let Some(expected) = callback.organization_id.as_deref()
        && expected != verified.organization.id
    {
        return Err("Speakeasy API key belongs to a different organization".into());
    }
    let project = match callback.project.as_ref() {
        Some(project) if verified.projects.iter().any(|entry| entry.slug == *project) => {
            project.clone()
        }
        Some(_) => return Err("Speakeasy callback selected an inaccessible project".into()),
        None => verified
            .projects
            .first()
            .map(|entry| entry.slug.clone())
            .ok_or_else(|| "Speakeasy API key has no accessible projects".to_string())?,
    };
    let record = Credentials {
        api_key: callback.api_key.clone(),
        project,
        email: callback.email.clone(),
        organization_id: Some(verified.organization.id),
    };
    validate(&record)?;
    Ok(record)
}

fn login(storage: &CredentialStorage, deadline: Instant) -> Result<String, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|_| "could not bind the Speakeasy sign-in callback".to_string())?;
    listener
        .set_nonblocking(true)
        .map_err(|_| "could not configure the Speakeasy sign-in callback".to_string())?;
    let port = listener
        .local_addr()
        .map_err(|_| "could not read the Speakeasy callback address".to_string())?
        .port();
    let state = random_state()?;
    let callback_url = format!("http://127.0.0.1:{port}/callback?state={state}");
    let url = sign_in_url(&callback_url)?;

    println!("Open this URL to authenticate with Speakeasy:\n{url}");
    open_browser_bounded(url.as_str());

    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => match parse_callback(&mut stream, port, &state, deadline) {
                Ok(Some(callback)) => {
                    let record = match verify_credentials(callback, deadline) {
                        Ok(record) => record,
                        Err(error) => {
                            write_response(
                                &mut stream,
                                401,
                                "Sign-in failed because the API key could not be verified.",
                            );
                            return Err(error);
                        }
                    };
                    if let Err(error) = save(storage, &record) {
                        write_response(
                            &mut stream,
                            500,
                            "Sign-in failed because the credential could not be stored.",
                        );
                        return Err(error);
                    }
                    write_response(
                        &mut stream,
                        200,
                        "Sign-in complete. You can close this window.",
                    );
                    return Ok(format!(
                        "Authenticated with Speakeasy{} for project {}.\n",
                        record
                            .email
                            .as_deref()
                            .map(|email| format!(" as {email}"))
                            .unwrap_or_default(),
                        record.project
                    ));
                }
                Ok(None) => write_response(&mut stream, 404, "Not found."),
                Err(_) => {
                    write_response(&mut stream, 400, "Sign-in callback was rejected.");
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return Err("Speakeasy sign-in callback failed".into()),
        }
    }
    Err("Speakeasy sign-in timed out".into())
}

fn sign_in_url(callback_url: &str) -> Result<url::Url, String> {
    let mut url = url::Url::parse(DASHBOARD_URL)
        .map_err(|_| "Speakeasy dashboard URL is invalid".to_string())?;
    url.query_pairs_mut()
        .append_pair("from_cli", "true")
        .append_pair("cli_callback_url", callback_url)
        .append_pair("key_scope", "producer")
        .append_pair("callback_method", "post");
    Ok(url)
}

fn parse_callback(
    stream: &mut TcpStream,
    port: u16,
    expected_state: &str,
    deadline: Instant,
) -> Result<Option<CallbackCredentials>, String> {
    let mut request = Zeroizing::new(Vec::new());
    let mut chunk = [0_u8; 2048];
    let header_end = loop {
        if let Some(position) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break position + 4;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Speakeasy sign-in callback timed out".into());
        }
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_secs(2))))
            .ok();
        let count = stream
            .read(&mut chunk)
            .map_err(|_| "could not read the Speakeasy callback".to_string())?;
        if count == 0 {
            return Err("Speakeasy callback headers are incomplete".into());
        }
        request.extend_from_slice(&chunk[..count]);
        if request.len() > MAX_HTTP_BYTES {
            return Err("Speakeasy callback request is too large".into());
        }
    };
    let headers = std::str::from_utf8(&request[..header_end - 4])
        .map_err(|_| "Speakeasy callback headers are not UTF-8".to_string())?;
    let mut lines = headers.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let (Some("POST"), Some(target), Some("HTTP/1.1"), None) = (
        request_line.next(),
        request_line.next(),
        request_line.next(),
        request_line.next(),
    ) else {
        return Err("Speakeasy callback must be an HTTP/1.1 POST".into());
    };
    if target.contains('#') || target.len() > 8192 {
        return Err("Speakeasy callback target is invalid".into());
    }
    let target = target.to_owned();
    let mut host = None;
    let mut content_length = None;
    let mut content_type = None;
    for line in lines {
        if line.starts_with([' ', '\t']) || !line.is_ascii() {
            return Err("Speakeasy callback headers are invalid".into());
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| "Speakeasy callback header is malformed".to_string())?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("host") && host.replace(value).is_some() {
            return Err("Speakeasy callback contains duplicate Host headers".into());
        }
        if name.eq_ignore_ascii_case("content-length") {
            let length = value
                .parse::<usize>()
                .map_err(|_| "Speakeasy callback Content-Length is invalid".to_string())?;
            if content_length.replace(length).is_some() {
                return Err("Speakeasy callback contains duplicate Content-Length headers".into());
            }
        }
        if name.eq_ignore_ascii_case("content-type")
            && content_type.replace(value.to_ascii_lowercase()).is_some()
        {
            return Err("Speakeasy callback contains duplicate Content-Type headers".into());
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("Speakeasy callback transfer encoding is unsupported".into());
        }
    }
    if host != Some(format!("127.0.0.1:{port}").as_str()) {
        return Err("Speakeasy callback Host is invalid".into());
    }
    if content_type.as_deref() != Some("application/x-www-form-urlencoded") {
        return Err("Speakeasy callback Content-Type is invalid".into());
    }
    let content_length = content_length
        .filter(|length| header_end.saturating_add(*length) <= MAX_HTTP_BYTES)
        .ok_or_else(|| "Speakeasy callback Content-Length is invalid".to_string())?;
    while request.len() < header_end + content_length {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Speakeasy sign-in callback timed out".into());
        }
        stream
            .set_read_timeout(Some(remaining.min(Duration::from_secs(2))))
            .ok();
        let count = stream
            .read(&mut chunk)
            .map_err(|_| "could not read the Speakeasy callback body".to_string())?;
        if count == 0 {
            return Err("Speakeasy callback body is incomplete".into());
        }
        request.extend_from_slice(&chunk[..count]);
        if request.len() > header_end + content_length {
            return Err("Speakeasy callback body length is invalid".into());
        }
    }
    if request.len() != header_end + content_length {
        return Err("Speakeasy callback body length is invalid".into());
    }
    let url = url::Url::parse(&format!("http://127.0.0.1:{port}{target}"))
        .map_err(|_| "Speakeasy callback target is invalid".to_string())?;
    if url.path() != "/callback" {
        return Ok(None);
    }
    let states = url
        .query_pairs()
        .filter(|(key, _)| key == "state")
        .map(|(_, value)| value.into_owned())
        .collect::<Vec<_>>();
    let [state] = states.as_slice() else {
        return Err("Speakeasy callback must contain exactly one state".into());
    };
    if state.len() != expected_state.len()
        || !bool::from(state.as_bytes().ct_eq(expected_state.as_bytes()))
    {
        return Err("Speakeasy callback state did not match".into());
    }
    let fields = Zeroizing::new(
        url::form_urlencoded::parse(&request[header_end..])
            .into_owned()
            .collect::<Vec<_>>(),
    );
    let value = |name: &str| -> Result<Option<String>, String> {
        let values = fields
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .collect::<Vec<_>>();
        match values.as_slice() {
            [] => Ok(None),
            [value] => Ok(Some(value.clone())),
            _ => Err(format!(
                "Speakeasy callback contains duplicate {name} fields"
            )),
        }
    };
    let api_key =
        value("api_key")?.ok_or_else(|| "Speakeasy callback omitted the API key".to_string())?;
    if !valid_api_key(&api_key) {
        return Err("Speakeasy returned an invalid Gram API key".into());
    }
    let project = value("project")?;
    if project
        .as_deref()
        .is_some_and(|project| !valid_project(project))
    {
        return Err("Speakeasy returned an invalid project slug".into());
    }
    Ok(Some(CallbackCredentials {
        api_key,
        project,
        email: value("email")?,
        organization_id: value("organization_id")?,
    }))
}

fn write_response(stream: &mut TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let body = format!("<!doctype html><meta charset=utf-8><title>Kit</title><p>{message}</p>");
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

fn random_state() -> Result<String, String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| "cryptographic randomness is unavailable".to_string())?;
    let state = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    bytes.zeroize();
    Ok(state)
}

#[cfg(not(windows))]
fn open_browser_bounded(url: &str) {
    let mut command = Command::new(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    });
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(mut child) = command.spawn() {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(windows)]
fn open_browser_bounded(url: &str) {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt as _};
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    let operation = OsStr::new("open")
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let url = OsStr::new(url)
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both arguments are NUL-terminated UTF-16 strings and remain live for the call.
    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_record() -> Credentials {
        Credentials {
            api_key: format!("gram_live_{}", "ab".repeat(32)),
            project: "kit-test".into(),
            email: Some("person@example.com".into()),
            organization_id: Some("org_123".into()),
        }
    }

    #[test]
    fn credentials_round_trip_without_exposing_the_key() {
        let storage = CredentialStorage::Memory;
        save(&storage, &test_record()).unwrap();
        let loaded = load(&storage).unwrap().unwrap();
        assert_eq!(loaded.project, "kit-test");
        assert!(!format!("{loaded:?}").contains("gram_live_"));
        storage.entry(NAMESPACE, IDENTITY).delete().unwrap();
    }

    #[test]
    fn sign_in_url_uses_the_dashboard_cli_contract() {
        let url = sign_in_url("http://127.0.0.1:1234/callback?state=abc").unwrap();
        let pairs = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(pairs.get("from_cli").map(|v| v.as_ref()), Some("true"));
        assert_eq!(pairs.get("key_scope").map(|v| v.as_ref()), Some("producer"));
        assert_eq!(
            pairs.get("callback_method").map(|v| v.as_ref()),
            Some("post")
        );
        assert_eq!(
            pairs.get("cli_callback_url").map(|v| v.as_ref()),
            Some("http://127.0.0.1:1234/callback?state=abc")
        );
    }

    #[test]
    fn callback_verification_selects_a_project_and_stores_credentials() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let key = format!("gram_live_{}", "ab".repeat(32));
        let callback_key = key.clone();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let body = format!("api_key={callback_key}&email=person%40example.com");
            write!(
                stream,
                "POST /callback?state=expected HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let (mut stream, _) = listener.accept().unwrap();
        let callback = parse_callback(
            &mut stream,
            port,
            "expected",
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
        assert_eq!(callback.project, None);
        client.join().unwrap();

        let verify_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let verify_address = verify_listener.local_addr().unwrap();
        let verify_server = std::thread::spawn(move || {
            let (mut stream, _) = verify_listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 2048];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&chunk[..count]);
            }
            let body = r#"{"organization":{"id":"org_123"},"projects":[{"slug":"default-project"}],"scopes":["producer"]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            String::from_utf8(request).unwrap()
        });
        let record = verify_credentials_at(
            callback,
            Instant::now() + Duration::from_secs(5),
            &format!("http://{verify_address}/rpc/keys.verify"),
        )
        .unwrap();
        let request = verify_server.join().unwrap().to_ascii_lowercase();
        assert!(request.contains(&format!("gram-key: {key}")));
        assert_eq!(record.project, "default-project");

        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().to_path_buf());
        save(&storage, &record).unwrap();
        let loaded = load(&storage).unwrap().unwrap();
        assert_eq!(loaded.project, "default-project");
        assert_eq!(loaded.email.as_deref(), Some("person@example.com"));
    }
}
