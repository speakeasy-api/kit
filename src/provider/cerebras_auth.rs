use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use super::{CerebrasApiKey, CerebrasApiKeySource, CerebrasAuthCommand};
use crate::credentials::CredentialStorage;

// A new namespace: existing providers' credential records are unchanged.
const NAMESPACE: &str = "cerebras";
const IDENTITY: &str = "default";
const MAX_RECORD_BYTES: usize = 16 * 1024;

#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
struct Credentials {
    api_key: String,
}

fn validate(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > 8192 || !key.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("Cerebras API key must be nonempty printable ASCII without whitespace (maximum 8192 bytes)".into());
    }
    Ok(())
}

pub(super) fn load(storage: &CredentialStorage) -> Result<Option<CerebrasApiKey>, String> {
    let Some(bytes) = storage
        .entry(NAMESPACE, IDENTITY)
        .load()
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    if bytes.len() > MAX_RECORD_BYTES {
        return Err("stored Cerebras credential is too large".into());
    }
    let mut record: Credentials = serde_json::from_slice(&bytes)
        .map_err(|_| "stored Cerebras credential is invalid".to_string())?;
    validate(&record.api_key)?;
    Ok(Some(CerebrasApiKey::new(std::mem::take(
        &mut record.api_key,
    ))))
}

fn save(storage: &CredentialStorage, key: &CerebrasApiKey) -> Result<(), String> {
    validate(key.as_str())?;
    #[derive(Serialize)]
    struct Record<'a> {
        api_key: &'a str,
    }
    let bytes = Zeroizing::new(
        serde_json::to_vec(&Record {
            api_key: key.as_str(),
        })
        .map_err(|_| "could not encode Cerebras credential".to_string())?,
    );
    storage
        .entry(NAMESPACE, IDENTITY)
        .save(&bytes)
        .map_err(|error| error.to_string())
}

pub(super) fn execute(
    command: CerebrasAuthCommand,
    storage: &CredentialStorage,
    active_key: Option<(&CerebrasApiKey, CerebrasApiKeySource)>,
) -> Result<String, String> {
    match command {
        CerebrasAuthCommand::Login => {
            let (key, _) = active_key.ok_or_else(||
                "Create a key at https://cloud.cerebras.ai, then supply CEREBRAS_API_KEY or --cerebras-api-key to save it.".to_string())?;
            save(storage, key)?;
            Ok(format!(
                "Cerebras: API key saved (storage: {}; not remotely validated).\n",
                storage.cli_name()
            ))
        }
        CerebrasAuthCommand::Status => {
            if let Some((_, source)) = active_key {
                Ok(format!(
                    "Cerebras: API key configured (source: {}; not remotely validated).\n",
                    source.label()
                ))
            } else if load(storage)?.is_some() {
                Ok(format!(
                    "Cerebras: API key configured (storage: {}; not remotely validated).\n",
                    storage.cli_name()
                ))
            } else {
                Ok("Cerebras: no API key configured.\n".into())
            }
        }
        CerebrasAuthCommand::Logout { local_only: _ } => {
            storage
                .entry(NAMESPACE, IDENTITY)
                .delete()
                .map_err(|error| error.to_string())?;
            let mut message = "Cerebras: local API key removed. Revoke the key at https://cloud.cerebras.ai if needed.\n".to_string();
            if let Some((_, source)) = active_key {
                message.push_str(&format!(
                    "The key supplied by {} remains active.\n",
                    source.label()
                ));
            }
            Ok(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_debug_is_redacted_and_empty_parse_is_rejected() {
        let key: CerebrasApiKey = "test-secret".parse().unwrap();
        assert_eq!(format!("{key:?}"), "CerebrasApiKey([REDACTED])");
        assert_eq!(key.clone().as_str(), "test-secret");
        assert!("".parse::<CerebrasApiKey>().is_err());
        fn zeroized<T: Zeroize + ZeroizeOnDrop>() {}
        zeroized::<CerebrasApiKey>();
    }

    #[test]
    fn stored_key_roundtrip_status_and_logout() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().to_owned());
        assert!(load(&storage).unwrap().is_none());
        assert!(execute(CerebrasAuthCommand::Login, &storage, None).is_err());
        let key = CerebrasApiKey::new("test-secret");
        let active = Some((&key, CerebrasApiKeySource::Environment));
        let message = execute(CerebrasAuthCommand::Login, &storage, active).unwrap();
        assert!(!message.contains(key.as_str()));
        assert_eq!(load(&storage).unwrap().unwrap().as_str(), key.as_str());
        let bytes = storage.entry(NAMESPACE, IDENTITY).load().unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            serde_json::json!({"api_key": "test-secret"})
        );
        assert!(
            execute(CerebrasAuthCommand::Status, &storage, active)
                .unwrap()
                .contains("CEREBRAS_API_KEY")
        );
        assert!(
            execute(CerebrasAuthCommand::Status, &storage, None)
                .unwrap()
                .contains("storage: file")
        );
        assert!(
            execute(
                CerebrasAuthCommand::Logout { local_only: false },
                &storage,
                active
            )
            .unwrap()
            .contains("remains active")
        );
        assert!(load(&storage).unwrap().is_none());
        execute(
            CerebrasAuthCommand::Logout { local_only: true },
            &storage,
            None,
        )
        .unwrap();
    }

    #[test]
    fn invalid_records_do_not_disclose_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(directory.path().to_owned());
        let entry = storage.entry(NAMESPACE, IDENTITY);
        for bytes in [
            br#"{"api_key":"test-secret","unknown":true}"#.as_slice(),
            br#"{"api_key":"test-secret\n"}"#,
            br#"{"api_key":42}"#,
            br#"{"api_key":""}"#,
            b"test-secret",
        ] {
            entry.save(bytes).unwrap();
            let error = load(&storage).unwrap_err();
            assert!(!error.contains("test-secret"));
        }
        entry.save(&vec![b'x'; MAX_RECORD_BYTES + 1]).unwrap();
        assert!(load(&storage).unwrap_err().contains("too large"));
        assert!(save(&storage, &CerebrasApiKey::new("a b")).is_err());
    }
}
