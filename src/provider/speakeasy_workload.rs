//! Explicit, deployment-owned workload credentials for native inference.
use agentkit_http::{HttpClient, HttpError, HttpRequest, HttpResponse};
use async_trait::async_trait;
use serde::Deserialize;
use std::io::Read;

const CONFIG_ENV: &str = "KIT_SPEAKEASY_WORKLOAD_AUTH";

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Config {
    pub project: String,
    pub workload: crate::workload_auth::Config,
}

impl Config {
    pub(super) fn from_env() -> Result<Option<Self>, String> {
        let Some(path) = std::env::var_os(CONFIG_ENV) else {
            return Ok(None);
        };
        let path = std::path::PathBuf::from(path);
        if !path.is_absolute() {
            return Err(format!("{CONFIG_ENV}: configuration path must be absolute"));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options
            .open(path)
            .map_err(|_| format!("{CONFIG_ENV}: cannot open workload configuration"))?;
        let metadata = file
            .metadata()
            .map_err(|_| format!("{CONFIG_ENV}: cannot inspect workload configuration"))?;
        if !metadata.is_file() || metadata.len() > 65536 {
            return Err(format!(
                "{CONFIG_ENV}: configuration must be a regular file of at most 64 KiB"
            ));
        }
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .map_err(|_| format!("{CONFIG_ENV}: cannot read workload configuration"))?;
        if bytes.len() > 65536 {
            return Err(format!("{CONFIG_ENV}: configuration exceeds 64 KiB"));
        }
        Self::parse(&bytes).map(Some)
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let config: Self = serde_json::from_slice(bytes)
            .map_err(|_| format!("{CONFIG_ENV}: invalid workload configuration"))?;
        if config.project.trim().is_empty()
            || agentkit_http::HeaderValue::from_str(&config.project).is_err()
        {
            return Err(format!(
                "{CONFIG_ENV}: project must be a nonempty HTTP header value"
            ));
        }
        config.workload.validate()?;
        if !inference_origin(&config.workload.resource) {
            return Err(format!(
                "{CONFIG_ENV}: resource must belong to https://ai.speakeasy.com"
            ));
        }
        Ok(config)
    }
}

pub(super) struct Client {
    client: reqwest::Client,
    session: crate::workload_auth::Session,
}

impl Client {
    pub(super) fn new(client: reqwest::Client, config: Config) -> Result<Self, String> {
        Ok(Self {
            client,
            session: crate::workload_auth::Session::new(config.workload)?,
        })
    }
}

fn inference_origin(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("ai.speakeasy.com")
        && url.port_or_known_default() == Some(443)
        && url.username().is_empty()
        && url.password().is_none()
}

#[async_trait]
impl HttpClient for Client {
    async fn execute(&self, mut request: HttpRequest) -> Result<HttpResponse, HttpError> {
        // Check before reading or exchanging the assertion. Never follow redirects.
        if !inference_origin(&request.url) {
            return Err(HttpError::Other(
                "workload inference requires https://ai.speakeasy.com".into(),
            ));
        }
        let token = zeroize::Zeroizing::new(self.session.token().await.map_err(HttpError::Other)?);
        let mut authorization =
            agentkit_http::HeaderValue::from_str(&format!("Bearer {}", token.as_str()))
                .map_err(|_| HttpError::Other("invalid workload access token".into()))?;
        authorization.set_sensitive(true);
        request.headers.remove("Gram-Key");
        request
            .headers
            .insert(agentkit_http::header::AUTHORIZATION, authorization);
        let response = HttpClient::execute(&self.client, request).await?;
        if response.status() == agentkit_http::StatusCode::UNAUTHORIZED {
            self.session.invalidate().await;
        }
        Ok(response)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;

    #[test]
    fn workload_inference_is_pinned_to_https_origin() {
        assert!(inference_origin(
            "https://ai.speakeasy.com/chat/completions"
        ));
        assert!(inference_origin(
            "https://ai.speakeasy.com:443/chat/completions"
        ));
        for url in [
            "http://ai.speakeasy.com/chat/completions",
            "https://ai.speakeasy.com:444/chat/completions",
            "https://ai.speakeasy.com.evil.example/chat/completions",
            "https://evil.example/chat/completions",
            "https://user@ai.speakeasy.com/chat/completions",
        ] {
            assert!(!inference_origin(url), "{url}");
        }
    }

    #[test]
    fn workload_resource_must_be_inference_origin() {
        let mut value = serde_json::json!({
            "project": "test",
            "workload": {
                "tokenFile": "/run/assertion.jwt",
                "expectedIssuer": "https://issuer.example",
                "tokenEndpoint": "https://issuer.example/token",
                "resource": "https://ai.speakeasy.com"
            }
        });
        assert!(Config::parse(&serde_json::to_vec(&value).unwrap()).is_ok());
        value["workload"]["resource"] = serde_json::json!("https://mcp.example.com");
        assert!(Config::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn workload_configuration_is_explicit_and_strict() {
        assert!(Config::parse(b"{}").is_err());
        assert!(Config::parse(br#"{"project":"test","workload":{},"apiKey":"secret"}"#).is_err());
    }
}
