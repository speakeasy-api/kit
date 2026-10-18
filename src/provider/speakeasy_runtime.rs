use serde::Deserialize;
use zeroize::{Zeroize, ZeroizeOnDrop};

use super::speakeasy_auth;
use crate::credentials::CredentialStorage;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn runtime_record_is_versioned_origin_bound_and_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let storage = CredentialStorage::Filesystem(dir.path().to_path_buf());
        let entry = storage.entry("speakeasy", "assistant-runtime");
        let write =
            |value: serde_json::Value| entry.save(&serde_json::to_vec(&value).unwrap()).unwrap();
        let env = |name: &str| match name {
            "SPEAKEASY_ORIGIN" => Some("http://aicp:8080/".into()),
            "SPEAKEASY_ALLOW_INSECURE_HTTP" => Some("1".into()),
            _ => None,
        };
        write(json!({"version":1,"bearer_token":"sai_test","origin":"http://aicp:8080"}));
        let config = Configuration::load_with_env(&storage, env)
            .unwrap()
            .unwrap();
        assert!(config.auth.is_runtime());
        assert_eq!(config.origin, "http://aicp:8080");
        assert!(Configuration::load_with_env(&storage, |_| None).is_err());
        for value in [
            json!({"version":2,"bearer_token":"sai_test","origin":"http://aicp:8080"}),
            json!({"version":1,"bearer_token":"bad\nheader","origin":"http://aicp:8080"}),
            json!({"version":1,"bearer_token":"sai_test","origin":"http://other:8080"}),
            json!({"version":1,"bearer_token":"sai_test","origin":"http://aicp:8080","api_key":"mixed"}),
            json!({"bearer_token":"sai_test","origin":"http://aicp:8080"}),
        ] {
            write(value);
            assert!(Configuration::load_with_env(&storage, env).is_err());
        }
        // A malformed runtime record is not treated as an absent credential.
        entry.save(b"not json").unwrap();
        assert!(Configuration::load_with_env(&storage, env).is_err());
    }

    #[test]
    fn origins_require_explicit_http_opt_in_and_no_ambient_url_authority() {
        assert_eq!(
            trusted_origin("https://EXAMPLE.com:443/", false).unwrap(),
            "https://example.com"
        );
        assert!(trusted_origin("http://aicp:8080", false).is_err());
        assert_eq!(
            trusted_origin("http://aicp:8080", true).unwrap(),
            "http://aicp:8080"
        );
        for value in [
            "https://user:pass@example.com",
            "https://example.com/path",
            "https://example.com?x",
            "https://example.com#x",
            "file:///tmp/test",
        ] {
            assert!(trusted_origin(value, true).is_err());
        }
    }

    #[test]
    fn runtime_chat_requires_explicit_non_nil_uuid() {
        let mut metadata = agentkit_core::MetadataMap::new();
        assert!(canonical_chat_id(&metadata).is_err());
        for value in [
            json!(null),
            json!(42),
            json!("guessed-session"),
            json!(uuid::Uuid::nil().to_string()),
        ] {
            metadata.insert(CHAT_ID_KEY.into(), value);
            assert!(canonical_chat_id(&metadata).is_err());
        }
        let id = uuid::Uuid::new_v4().to_string();
        metadata.insert(CHAT_ID_KEY.into(), json!(id));
        assert_eq!(canonical_chat_id(&metadata).unwrap(), id);
    }
}

pub(super) const CHAT_ID_KEY: &str = "ai.speakeasy/gram-chat-id";

#[derive(Clone, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
struct RuntimeCredential {
    version: u32,
    bearer_token: String,
    origin: String,
}

#[derive(Clone)]
pub(super) enum Authentication {
    Runtime(zeroize::Zeroizing<String>),
    Legacy(speakeasy_auth::Credentials),
}

#[derive(Clone)]
pub(super) struct Configuration {
    pub origin: String,
    pub auth: Authentication,
}

impl Configuration {
    pub fn load(storage: &CredentialStorage) -> Result<Option<Self>, String> {
        Self::load_with_env(storage, |name| std::env::var(name).ok())
    }

    fn load_with_env(
        storage: &CredentialStorage,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, String> {
        let runtime = storage
            .entry("speakeasy", "assistant-runtime")
            .load()
            .map_err(|_| "could not read Speakeasy runtime credentials".to_string())?;
        let insecure = env("SPEAKEASY_ALLOW_INSECURE_HTTP").as_deref() == Some("1");
        if let Some(bytes) = runtime {
            let bytes = zeroize::Zeroizing::new(bytes);
            if bytes.len() > 16 * 1024 {
                return Err("Speakeasy runtime credential exceeds 16 KiB".into());
            }
            let record: RuntimeCredential = serde_json::from_slice(&bytes)
                .map_err(|_| "invalid Speakeasy runtime credential record".to_string())?;
            if record.version != 1
                || record.bearer_token.is_empty()
                || record.bearer_token.len() > 8192
                || !record.bearer_token.bytes().all(|b| b.is_ascii_graphic())
            {
                return Err("invalid Speakeasy runtime credential version or bearer".into());
            }
            let configured = env("SPEAKEASY_ORIGIN")
                .ok_or("SPEAKEASY_ORIGIN is required for runtime credentials")?;
            let origin = trusted_origin(&configured, insecure)?;
            if origin != trusted_origin(&record.origin, insecure)? {
                return Err(
                    "Speakeasy runtime credential origin does not match SPEAKEASY_ORIGIN".into(),
                );
            }
            return Ok(Some(Self {
                origin,
                auth: Authentication::Runtime(zeroize::Zeroizing::new(record.bearer_token.clone())),
            }));
        }
        let Some(legacy) = speakeasy_auth::load(storage)? else {
            return Ok(None);
        };
        let origin = trusted_origin(
            &env("SPEAKEASY_ORIGIN").unwrap_or_else(|| "https://ai.speakeasy.com".into()),
            insecure,
        )?;
        Ok(Some(Self {
            origin,
            auth: Authentication::Legacy(legacy),
        }))
    }
}

fn trusted_origin(value: &str, insecure: bool) -> Result<String, String> {
    let invalid = || {
        "invalid Speakeasy origin: use an HTTPS origin, or explicitly allow HTTP for development"
            .to_string()
    };
    let url = reqwest::Url::parse(value).map_err(|_| invalid())?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && insecure))
    {
        return Err(invalid());
    }
    Ok(url.origin().ascii_serialization())
}

impl Authentication {
    pub fn is_runtime(&self) -> bool {
        matches!(self, Self::Runtime(_))
    }

    pub fn http(
        &self,
        builder: agentkit_http::HttpRequestBuilder,
    ) -> agentkit_http::HttpRequestBuilder {
        match self {
            Self::Runtime(token) => {
                builder.header("Authorization", format!("Bearer {}", token.as_str()))
            }
            Self::Legacy(record) => builder
                .header("Gram-Key", &record.api_key)
                .header("Gram-Project", &record.project),
        }
    }

    pub fn reqwest(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Self::Runtime(token) => builder.bearer_auth(token.as_str()),
            Self::Legacy(record) => builder
                .header("Gram-Key", &record.api_key)
                .header("Gram-Project", &record.project),
        }
    }
}

pub(super) fn canonical_chat_id(metadata: &agentkit_core::MetadataMap) -> Result<String, String> {
    let id = metadata.get(CHAT_ID_KEY).and_then(serde_json::Value::as_str)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .filter(|id| !id.is_nil())
        .ok_or("Speakeasy runtime inference requires canonical ai.speakeasy/gram-chat-id session metadata")?;
    Ok(id.to_string())
}
