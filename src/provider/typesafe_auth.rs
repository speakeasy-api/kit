use std::time::Duration;

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::credentials::CredentialStorage;

const NAMESPACE: &str = "typesafe";
const IDENTITY: &str = "default";
const KEYS_URL: &str = "https://console.typesafe.ai/keys";
const MODELS_URL: &str = "https://api.typesafe.ai/v1/models";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RECORD_BYTES: usize = 16 * 1024;

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TypeSafeAuthCommand {
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

/// Resolve the active key without making a network request.
pub(crate) fn resolve_api_key(storage: &CredentialStorage) -> Result<Option<Credentials>, String> {
    let environment = match std::env::var("TYPESAFE_API_KEY") {
        Ok(value) => Some(Zeroizing::new(value)),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("Invalid TYPESAFE_API_KEY. Check the key and try again.".into());
        }
    };
    resolve_key(storage, environment.as_deref().map(String::as_str))
}

fn resolve_key(
    storage: &CredentialStorage,
    environment: Option<&str>,
) -> Result<Option<Credentials>, String> {
    if let Some(key) = environment.filter(|key| !key.is_empty()) {
        let record = Credentials {
            api_key: key.into(),
        };
        validate(&record)?;
        Ok(Some(record))
    } else {
        load(storage)
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TypeSafeCredentials([REDACTED])")
    }
}

#[doc(hidden)]
pub fn execute_typesafe_auth(
    command: TypeSafeAuthCommand,
    storage: &CredentialStorage,
) -> Result<String, String> {
    let environment_active =
        std::env::var_os("TYPESAFE_API_KEY").is_some_and(|value| !value.is_empty());
    execute(command, storage, environment_active)
}

fn execute(
    command: TypeSafeAuthCommand,
    storage: &CredentialStorage,
    environment_active: bool,
) -> Result<String, String> {
    match command {
        TypeSafeAuthCommand::Login => {
            if !storage.is_persistent() {
                return Err("To save your TypeSafe key, use --credential-store keychain or --credential-store file.".into());
            }
            eprintln!("Create a TypeSafe API key at {KEYS_URL}");
            let record = Credentials {
                api_key: rpassword::prompt_password("TypeSafe API key (hidden): ")
                    .map_err(|_| "could not read TypeSafe API key from the terminal".to_string())?,
            };
            complete_login(
                storage,
                &record,
                environment_active,
                MODELS_URL,
                LOGIN_TIMEOUT,
            )
        }
        TypeSafeAuthCommand::Status => {
            if environment_active {
                Ok("TypeSafe: configured via TYPESAFE_API_KEY.\n".into())
            } else if load(storage)?.is_some() {
                Ok("TypeSafe: configured.\n".into())
            } else {
                Ok("TypeSafe: not configured.\n".into())
            }
        }
        TypeSafeAuthCommand::Logout { local_only } => {
            let removed = storage
                .entry(NAMESPACE, IDENTITY)
                .delete()
                .map_err(|_| "Could not remove your TypeSafe key. Try again.".to_string())?;
            let mut output = if removed {
                "TypeSafe key removed.\n".to_string()
            } else {
                "TypeSafe: no saved key.\n".to_string()
            };
            if !local_only {
                output.push_str(&format!("Revoke TypeSafe API keys at {KEYS_URL}.\n"));
            }
            if environment_active {
                output.push_str("Warning: TYPESAFE_API_KEY remains active after logout.\n");
            }
            Ok(output)
        }
    }
}

// https://docs.typesafe.ai/models documents this authenticated, non-inference GET.
// Keep redirects disabled so validation cannot forward the key to another URL.
fn complete_login(
    storage: &CredentialStorage,
    record: &Credentials,
    environment_active: bool,
    models_url: &str,
    timeout: Duration,
) -> Result<String, String> {
    validate(record)?;
    let unavailable = || "Could not authenticate with TypeSafe. Try again later.".to_string();
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout.min(Duration::from_secs(5)))
        .timeout(timeout)
        .build()
        .map_err(|_| unavailable())?;
    let response = client
        .get(models_url)
        .bearer_auth(&record.api_key)
        .send()
        .map_err(|_| unavailable())?;
    match response.status() {
        reqwest::StatusCode::OK => {}
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
            return Err("TypeSafe rejected this API key. Check the key and try again.".into());
        }
        _ => return Err(unavailable()),
    }
    // Only the authenticated status matters; never read or expose response bodies.
    save(storage, record)?;
    let mut output = "TypeSafe: authenticated. API key saved.\n".to_string();
    if environment_active {
        output.push_str("Warning: TYPESAFE_API_KEY overrides your saved key.\n");
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
        return Err("Invalid TypeSafe API key. Copy the key and try again.".into());
    }
    if record.api_key.len() > MAX_RECORD_BYTES {
        return Err("Invalid TypeSafe API key. Copy the key and try again.".into());
    }
    Ok(())
}

fn load(storage: &CredentialStorage) -> Result<Option<Credentials>, String> {
    let Some(bytes) = storage
        .entry(NAMESPACE, IDENTITY)
        .load()
        .map_err(|_| "Could not read your TypeSafe key. Log in again.".to_string())?
    else {
        return Ok(None);
    };
    if bytes.len() > MAX_RECORD_BYTES {
        return Err("Could not read your TypeSafe key. Log in again.".into());
    }
    let record: Credentials = serde_json::from_slice(&bytes)
        .map_err(|_| "Could not read your TypeSafe key. Log in again.".to_string())?;
    validate(&record)?;
    Ok(Some(record))
}

fn save(storage: &CredentialStorage, record: &Credentials) -> Result<(), String> {
    validate(record)?;
    let bytes = Zeroizing::new(
        serde_json::to_vec(record)
            .map_err(|_| "Could not save your TypeSafe key. Try again.".to_string())?,
    );
    if bytes.len() > MAX_RECORD_BYTES {
        return Err("Invalid TypeSafe API key. Copy the key and try again.".into());
    }
    storage
        .entry(NAMESPACE, IDENTITY)
        .save(&bytes)
        .map_err(|_| "Could not save your TypeSafe key. Try again.".to_string())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_macros)]
mod tests {
    use super::*;

    use std::io::{Read, Write};
    use std::net::TcpListener;

    // A real HTTP boundary: capture the request and return an upstream response.
    fn serve(response: String, delay: Duration) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1/models", listener.local_addr().unwrap());
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
        for status in [200, 201, 204, 301, 302, 307, 401, 403, 429, 500, 503] {
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
            assert!(request.starts_with("get /v1/models http/1.1\r\n"));
            assert!(request.contains("authorization: bearer new-secret\r\n"));
            if status == 200 {
                let output = result.unwrap();
                assert!(output.starts_with("TypeSafe: authenticated. API key saved."));
                assert!(output.contains("TYPESAFE_API_KEY overrides your saved key"));
                assert!(!output.contains("new-secret"));
                assert_eq!(load(&storage).unwrap().unwrap().api_key, "new-secret");
            } else {
                let error = result.unwrap_err();
                assert!(!error.contains("new-secret"));
                assert!(!error.contains("upstream"));
                assert!(!error.contains("authenticated"));
                assert_eq!(error.contains("rejected"), status == 401 || status == 403);
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
        assert_eq!(error, "Could not save your TypeSafe key. Try again.");
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
                "Could not authenticate with TypeSafe. Try again later."
            );
            assert_eq!(load(&storage).unwrap().unwrap().api_key, "original");
        }
    }

    #[test]
    fn stored_key_round_trip_status_and_logout() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().into());
        assert!(
            execute(TypeSafeAuthCommand::Status, &storage, false)
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
        let status = execute(TypeSafeAuthCommand::Status, &storage, false).unwrap();
        assert_eq!(status, "TypeSafe: configured.\n");
        assert!(!status.contains(&record.api_key));
        assert!(!format!("{record:?}").contains(&record.api_key));
        let logout = execute(
            TypeSafeAuthCommand::Logout { local_only: false },
            &storage,
            true,
        )
        .unwrap();
        assert!(logout.contains(KEYS_URL));
        assert!(logout.contains("TYPESAFE_API_KEY remains active"));
        assert!(load(&storage).unwrap().is_none());
        let logout = execute(
            TypeSafeAuthCommand::Logout { local_only: true },
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
            let status = execute(TypeSafeAuthCommand::Status, &storage, true).unwrap();
            assert!(status.contains("configured via TYPESAFE_API_KEY"));
        }
        storage
            .entry(NAMESPACE, IDENTITY)
            .save(&vec![b'x'; MAX_RECORD_BYTES + 1])
            .unwrap();
        assert!(load(&storage).unwrap_err().contains("Log in again"));
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
        assert!(
            execute(
                TypeSafeAuthCommand::Login,
                &CredentialStorage::Memory,
                false
            )
            .is_err()
        );
    }
}
