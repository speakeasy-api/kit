use std::time::Duration;

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::credentials::CredentialStorage;

const NAMESPACE: &str = "gemini";
const IDENTITY: &str = "default";
const KEYS_URL: &str = "https://aistudio.google.com/apikey";
const MODELS_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RECORD_BYTES: usize = 16 * 1024;

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GeminiAuthCommand {
    Login,
    Status,
    Logout { local_only: bool },
}

// A new, isolated credential record, not a model-provider configuration.
#[derive(Deserialize, Serialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credentials {
    api_key: String,
}

impl Credentials {
    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CredentialSource {
    Environment,
    Stored,
}

fn read_environment_key() -> Result<Option<Zeroizing<String>>, String> {
    match std::env::var("GEMINI_API_KEY") {
        Ok(value) => Ok(Some(Zeroizing::new(value))),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("Invalid GEMINI_API_KEY. Check the key and try again.".into())
        }
    }
}

/// Resolve the active key without making a network request.
pub(crate) fn credentials(storage: &CredentialStorage) -> Result<Option<Credentials>, String> {
    let environment = read_environment_key()?;
    resolve_with_source(storage, environment.as_deref().map(String::as_str))
        .map(|resolved| resolved.map(|(record, _)| record))
}

fn resolve_with_source(
    storage: &CredentialStorage,
    environment: Option<&str>,
) -> Result<Option<(Credentials, CredentialSource)>, String> {
    if let Some(key) = environment.filter(|key| !key.is_empty()) {
        let record = Credentials {
            api_key: key.into(),
        };
        validate(&record)?;
        Ok(Some((record, CredentialSource::Environment)))
    } else {
        Ok(load(storage)?.map(|record| (record, CredentialSource::Stored)))
    }
}

#[cfg(test)]
fn resolve_key(
    storage: &CredentialStorage,
    environment: Option<&str>,
) -> Result<Option<Credentials>, String> {
    resolve_with_source(storage, environment).map(|resolved| resolved.map(|(record, _)| record))
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GeminiCredentials([REDACTED])")
    }
}

#[doc(hidden)]
pub fn execute_gemini_auth(
    command: GeminiAuthCommand,
    storage: &CredentialStorage,
) -> Result<String, String> {
    match command {
        GeminiAuthCommand::Status => {
            let environment = read_environment_key()?;
            execute_with_env(command, storage, environment.as_deref().map(String::as_str))
        }
        GeminiAuthCommand::Login | GeminiAuthCommand::Logout { .. } => {
            let environment = std::env::var_os("GEMINI_API_KEY")
                .is_some_and(|value| !value.is_empty())
                .then_some("present");
            execute_with_env(command, storage, environment)
        }
    }
}

fn execute_with_env(
    command: GeminiAuthCommand,
    storage: &CredentialStorage,
    environment: Option<&str>,
) -> Result<String, String> {
    let environment_active = environment.is_some_and(|value| !value.is_empty());
    match command {
        GeminiAuthCommand::Login => {
            if !storage.is_persistent() {
                return Err("To save your Gemini key, use --credential-store keychain or --credential-store file.".into());
            }
            eprintln!("Create a Gemini API key at {KEYS_URL}");
            let record = Credentials {
                api_key: rpassword::prompt_password("Gemini API key (hidden): ")
                    .map_err(|_| "could not read Gemini API key from the terminal".to_string())?,
            };
            complete_login(
                storage,
                &record,
                environment_active,
                MODELS_URL,
                LOGIN_TIMEOUT,
            )
        }
        GeminiAuthCommand::Status => match resolve_with_source(storage, environment)? {
            Some((_, CredentialSource::Environment)) => {
                Ok("Gemini: configured via GEMINI_API_KEY.\n".into())
            }
            Some((_, CredentialSource::Stored)) => Ok("Gemini: configured.\n".into()),
            None => Ok("Gemini: not configured.\n".into()),
        },
        GeminiAuthCommand::Logout { local_only } => {
            let entry = storage.entry(NAMESPACE, IDENTITY);
            let removed = entry
                .delete()
                .map_err(|_| "Could not remove your Gemini key. Try again.".to_string())?;
            entry
                .require_disk()
                .map_err(|_| "Could not remove your Gemini key. Try again.".to_string())?;
            let mut output = if removed {
                "Gemini key removed.\n".to_string()
            } else {
                "Gemini: no saved key.\n".to_string()
            };
            if !local_only {
                output.push_str(&format!("Revoke Gemini API keys at {KEYS_URL}.\n"));
            }
            if environment_active {
                output.push_str("Warning: GEMINI_API_KEY remains active after logout.\n");
            }
            Ok(output)
        }
    }
}

// Validate with the non-inference Models API; this does not spend API credits.
// Send the key via the `x-goog-api-key` header (never a query string) and keep
// redirects disabled so validation cannot forward the key to another URL.
fn complete_login(
    storage: &CredentialStorage,
    record: &Credentials,
    environment_active: bool,
    models_url: &str,
    timeout: Duration,
) -> Result<String, String> {
    validate(record)?;
    let unavailable = || "Could not authenticate with Gemini. Try again later.".to_string();
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout.min(Duration::from_secs(5)))
        .timeout(timeout)
        .build()
        .map_err(|_| unavailable())?;
    let response = client
        .get(models_url)
        .header("x-goog-api-key", record.api_key())
        .send()
        .map_err(|_| unavailable())?;
    match response.status() {
        reqwest::StatusCode::OK => {}
        // Gemini returns 400 (API_KEY_INVALID), 401, or 403 for invalid keys.
        reqwest::StatusCode::BAD_REQUEST
        | reqwest::StatusCode::UNAUTHORIZED
        | reqwest::StatusCode::FORBIDDEN => {
            return Err("Gemini rejected this API key. Check the key and try again.".into());
        }
        _ => return Err(unavailable()),
    }
    // Only the authenticated status matters; never read or expose response bodies.
    save(storage, record)?;
    let mut output = "Gemini: authenticated. API key saved.\n".to_string();
    if environment_active {
        output.push_str("Warning: GEMINI_API_KEY overrides your saved key.\n");
    }
    Ok(output)
}

fn validate(record: &Credentials) -> Result<(), String> {
    if record.api_key.is_empty()
        || record
            .api_key
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("Invalid Gemini API key. Copy the key and try again.".into());
    }
    if record.api_key.len() > MAX_RECORD_BYTES {
        return Err("Invalid Gemini API key. Copy the key and try again.".into());
    }
    Ok(())
}

fn load(storage: &CredentialStorage) -> Result<Option<Credentials>, String> {
    let Some(bytes) = storage
        .entry(NAMESPACE, IDENTITY)
        .load()
        .map_err(|_| "Could not read your Gemini key. Log in again.".to_string())?
    else {
        return Ok(None);
    };
    if bytes.len() > MAX_RECORD_BYTES {
        return Err("Could not read your Gemini key. Log in again.".into());
    }
    let record: Credentials = serde_json::from_slice(&bytes)
        .map_err(|_| "Could not read your Gemini key. Log in again.".to_string())?;
    validate(&record)?;
    Ok(Some(record))
}

fn save(storage: &CredentialStorage, record: &Credentials) -> Result<(), String> {
    validate(record)?;
    let bytes = Zeroizing::new(
        serde_json::to_vec(record)
            .map_err(|_| "Could not save your Gemini key. Try again.".to_string())?,
    );
    if bytes.len() > MAX_RECORD_BYTES {
        return Err("Invalid Gemini API key. Copy the key and try again.".into());
    }
    let entry = storage.entry(NAMESPACE, IDENTITY);
    entry
        .save(&bytes)
        .map_err(|_| "Could not save your Gemini key. Try again.".to_string())?;
    entry
        .require_disk()
        .map_err(|_| "Could not save your Gemini key. Try again.".to_string())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;

    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn execute(
        command: GeminiAuthCommand,
        storage: &CredentialStorage,
        environment_active: bool,
    ) -> Result<String, String> {
        let environment = environment_active.then_some("env-test-key");
        execute_with_env(command, storage, environment)
    }

    // A real HTTP boundary: capture the request and return an upstream response.
    fn serve(response: String, delay: Duration) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1beta/models", listener.local_addr().unwrap());
        let task = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                if stream.read(&mut byte).unwrap() == 0 {
                    break;
                }
                request.push(byte[0]);
                assert!(request.len() < 32 * 1024);
            }
            std::thread::sleep(delay);
            let _ = stream.write_all(response.as_bytes());
            String::from_utf8(request).unwrap()
        });
        (url, task)
    }

    #[test]
    fn credentials_are_isolated_from_other_providers() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().into());
        let other = br#"{"api_key":"other-provider-secret"}"#;
        storage.entry("anthropic", IDENTITY).save(other).unwrap();
        assert!(load(&storage).unwrap().is_none());
        save(
            &storage,
            &Credentials {
                api_key: "gemini-secret".into(),
            },
        )
        .unwrap();
        execute(
            GeminiAuthCommand::Logout { local_only: true },
            &storage,
            false,
        )
        .unwrap();
        assert!(load(&storage).unwrap().is_none());
        assert_eq!(
            storage
                .entry("anthropic", IDENTITY)
                .load()
                .unwrap()
                .unwrap()
                .as_slice(),
            other
        );
    }

    #[test]
    fn key_resolution_prefers_nonempty_environment_and_redacts_keys() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().into());
        assert!(resolve_key(&storage, None).unwrap().is_none());
        save(
            &storage,
            &Credentials {
                api_key: "stored-secret".into(),
            },
        )
        .unwrap();
        for environment in [None, Some("")] {
            assert_eq!(
                resolve_key(&storage, environment)
                    .unwrap()
                    .unwrap()
                    .api_key(),
                "stored-secret"
            );
        }
        let key = resolve_key(&storage, Some("environment-secret"))
            .unwrap()
            .unwrap();
        assert_eq!(key.api_key(), "environment-secret");
        assert!(!format!("{key:?}").contains("environment-secret"));
        assert!(resolve_key(&storage, Some("invalid secret")).is_err());
        storage.entry(NAMESPACE, IDENTITY).save(b"invalid").unwrap();
        assert!(resolve_key(&storage, Some("environment-secret")).is_ok());
    }

    #[test]
    fn login_validates_before_saving_and_redacts_failures() {
        for status in [200, 201, 204, 301, 302, 307, 400, 401, 403, 429, 500, 503] {
            let directory = tempfile::tempdir().unwrap();
            let storage = CredentialStorage::Filesystem(directory.path().into());
            save(
                &storage,
                &Credentials {
                    api_key: "original".into(),
                },
            )
            .unwrap();
            let body = "new-secret upstream details";
            let response = format!(
                "HTTP/1.1 {status} Test\r\nLocation: /redirected\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let (url, task) = serve(response, Duration::ZERO);
            let result = complete_login(
                &storage,
                &Credentials {
                    api_key: "new-secret".into(),
                },
                true,
                &url,
                Duration::from_secs(2),
            );
            let request = task.join().unwrap().to_ascii_lowercase();
            assert!(request.starts_with("get /v1beta/models http/1.1\r\n"));
            assert!(request.contains("x-goog-api-key: new-secret\r\n"));
            assert!(!request.contains("authorization:"));
            assert!(!request.contains("key="));
            if status == 200 {
                let output = result.unwrap();
                assert!(output.starts_with("Gemini: authenticated. API key saved."));
                assert!(output.contains("GEMINI_API_KEY overrides your saved key"));
                assert!(!output.contains("new-secret"));
                assert_eq!(load(&storage).unwrap().unwrap().api_key, "new-secret");
            } else {
                let error = result.unwrap_err();
                assert!(!error.contains("new-secret"));
                assert!(!error.contains("upstream"));
                assert!(!error.contains("authenticated"));
                assert_eq!(
                    error.contains("rejected"),
                    matches!(status, 400 | 401 | 403)
                );
                assert_eq!(load(&storage).unwrap().unwrap().api_key, "original");
            }
        }
    }

    #[test]
    fn successful_authentication_does_not_hide_save_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("not-a-directory");
        std::fs::write(&path, b"untouched").unwrap();
        let storage = CredentialStorage::Filesystem(path.clone());
        let (url, task) = serve(
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into(),
            Duration::ZERO,
        );
        let error = complete_login(
            &storage,
            &Credentials {
                api_key: "new-secret".into(),
            },
            false,
            &url,
            Duration::from_secs(2),
        )
        .unwrap_err();
        task.join().unwrap();
        assert_eq!(error, "Could not save your Gemini key. Try again.");
        assert_eq!(std::fs::read(path).unwrap(), b"untouched");
    }

    #[test]
    fn network_failure_and_timeout_preserve_previous_key() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().into());
        save(
            &storage,
            &Credentials {
                api_key: "original".into(),
            },
        )
        .unwrap();
        for (response, delay) in [
            (String::new(), Duration::ZERO),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into(),
                Duration::from_millis(500),
            ),
        ] {
            let (url, task) = serve(response, delay);
            let error = complete_login(
                &storage,
                &Credentials {
                    api_key: "new-secret".into(),
                },
                false,
                &url,
                Duration::from_millis(100),
            )
            .unwrap_err();
            task.join().unwrap();
            assert_eq!(
                error,
                "Could not authenticate with Gemini. Try again later."
            );
            assert_eq!(load(&storage).unwrap().unwrap().api_key, "original");
        }
    }

    #[test]
    fn stored_key_round_trip_status_and_logout() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().into());
        assert!(
            execute(GeminiAuthCommand::Status, &storage, false)
                .unwrap()
                .contains("not configured")
        );
        let record = Credentials {
            api_key: "secret-test-key".into(),
        };
        save(&storage, &record).unwrap();
        assert_eq!(load(&storage).unwrap().unwrap().api_key, record.api_key);
        let bytes = storage.entry(NAMESPACE, IDENTITY).load().unwrap().unwrap();
        assert_eq!(bytes.as_slice(), br#"{"api_key":"secret-test-key"}"#);
        let status = execute(GeminiAuthCommand::Status, &storage, false).unwrap();
        assert_eq!(status, "Gemini: configured.\n");
        assert!(!status.contains(&record.api_key));
        assert!(!format!("{record:?}").contains(&record.api_key));
        let logout = execute(
            GeminiAuthCommand::Logout { local_only: false },
            &storage,
            true,
        )
        .unwrap();
        assert!(logout.contains(KEYS_URL));
        assert!(logout.contains("GEMINI_API_KEY remains active"));
        assert!(load(&storage).unwrap().is_none());
        let logout = execute(
            GeminiAuthCommand::Logout { local_only: true },
            &storage,
            false,
        )
        .unwrap();
        assert!(logout.contains("no saved key"));
        assert!(!logout.contains(KEYS_URL));
    }

    #[test]
    fn malformed_records_are_redacted_and_environment_takes_precedence() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().into());
        for bytes in [
            br#"{"api_key":"secret","extra":true}"#.as_slice(),
            br#"{"api_key":""}"#,
            br#"{"api_key":"secret with spaces"}"#,
            br#"{"api_key":42}"#,
            b"secret-not-json",
            b"{}",
        ] {
            storage.entry(NAMESPACE, IDENTITY).save(bytes).unwrap();
            let error = load(&storage).unwrap_err();
            assert!(!error.contains("secret"));
            let status = execute(GeminiAuthCommand::Status, &storage, true).unwrap();
            assert!(status.contains("configured via GEMINI_API_KEY"));
        }
        storage
            .entry(NAMESPACE, IDENTITY)
            .save(&vec![b'x'; MAX_RECORD_BYTES + 1])
            .unwrap();
        assert!(load(&storage).unwrap_err().contains("Log in again"));
        assert!(
            execute_with_env(
                GeminiAuthCommand::Status,
                &storage,
                Some("invalid key with spaces")
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_key_does_not_overwrite_existing_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().into());
        save(
            &storage,
            &Credentials {
                api_key: "original".into(),
            },
        )
        .unwrap();
        for key in [
            String::new(),
            " ".into(),
            "secret\n".into(),
            "secret\0".into(),
            "x".repeat(MAX_RECORD_BYTES),
        ] {
            assert!(save(&storage, &Credentials { api_key: key }).is_err());
            assert_eq!(load(&storage).unwrap().unwrap().api_key, "original");
        }
        assert!(execute(GeminiAuthCommand::Login, &CredentialStorage::Memory, false).is_err());
    }
}
