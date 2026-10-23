//! Clientless RFC 7523 and RFC 8693 workload authentication. Assertions never enter credential storage.
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};

use serde::Deserialize;
use tokio::{io::AsyncReadExt, sync::Mutex};
use zeroize::Zeroizing;

const MAX_BYTES: usize = 8192;
const TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) fn session(
        config: super::Config,
        client: reqwest::Client,
    ) -> Result<super::Session, String> {
        config.validate()?;
        Ok(super::Session {
            config,
            client,
            cached: tokio::sync::Mutex::new(None),
        })
    }
    impl super::Session {
        pub async fn token(&self) -> Result<String, String> {
            self.acquisition()
                .await
                .map(|value| value.token().to_owned())
        }
    }
}

const ACCESS_TOKEN: &str = "urn:ietf:params:oauth:token-type:access_token";

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Grant {
    #[default]
    JwtBearer,
    TokenExchange,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EndpointMode {
    #[default]
    Discovery,
    Explicit,
}

/// Trusted deployment configuration, not input supplied by an MCP server or model.
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub token_file: PathBuf,
    #[serde(default)]
    pub expected_issuer: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub resource: String,
    #[serde(default)]
    pub grant: Grant,
    #[serde(default)]
    pub endpoint_mode: EndpointMode,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub extensions: BTreeMap<String, String>,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if !self.token_file.is_absolute() {
            return Err("workload tokenFile must be an absolute path".into());
        }
        let endpoint = https_url(&self.token_endpoint)?;
        if self.endpoint_mode == EndpointMode::Discovery {
            let issuer = https_url(&self.expected_issuer)?;
            https_url(&self.resource)?;
            if issuer.query().is_some() || issuer.origin() != endpoint.origin() {
                return Err("workload token endpoint must share the expected issuer origin".into());
            }
        } else {
            if !self.expected_issuer.is_empty() || endpoint.query().is_some() {
                return Err("explicit workload endpoints cannot specify an issuer or query".into());
            }
            if !self.resource.is_empty() {
                if self.resource.chars().any(char::is_whitespace) {
                    return Err("invalid workload resource".into());
                }
                let resource = url::Url::parse(&self.resource)
                    .map_err(|_| "workload resource must be an absolute URI")?;
                if resource.fragment().is_some()
                    || !resource.username().is_empty()
                    || resource.password().is_some()
                {
                    return Err("invalid workload resource".into());
                }
            }
        }
        for value in self.audience.iter().chain(self.scope.iter()) {
            if value.is_empty() || value.len() > 2048 || value.chars().any(char::is_control) {
                return Err("invalid workload audience or scope".into());
            }
        }
        if self.scope.as_ref().is_some_and(|scope| {
            scope.split(' ').any(|part| {
                part.is_empty()
                    || !part.bytes().all(|c| {
                        c == 0x21 || (0x23..=0x5b).contains(&c) || (0x5d..=0x7e).contains(&c)
                    })
            })
        }) {
            return Err("invalid workload scope".into());
        }
        // Extensions are non-secret deployment parameters, never alternative credentials.
        const RESERVED: &[&str] = &[
            "grant_type",
            "assertion",
            "resource",
            "audience",
            "scope",
            "subject_token",
            "subject_token_type",
            "requested_token_type",
            "actor_token",
            "actor_token_type",
            "client_id",
            "client_secret",
            "client_assertion",
            "client_assertion_type",
            "refresh_token",
            "access_token",
            "token",
            "token_type",
            "username",
            "password",
            "authorization",
            "code",
            "redirect_uri",
            "code_verifier",
        ];
        if self.extensions.len() > 16
            || self.extensions.iter().any(|(key, value)| {
                key.is_empty()
                    || key.len() > 64
                    || !key
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
                    || RESERVED.contains(&key.as_str())
                    || value.is_empty()
                    || value.len() > 1024
                    || value.chars().any(char::is_control)
            })
        {
            return Err("invalid workload extension parameters".into());
        }
        Ok(())
    }
}

fn https_url(value: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(value).map_err(|_| "invalid workload HTTPS URL")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("workload URLs must be HTTPS without credentials or fragments".into());
    }
    Ok(url)
}

#[derive(Clone)]
pub struct Acquisition(Arc<Zeroizing<String>>);

impl Acquisition {
    pub fn token(&self) -> &str {
        self.0.as_str()
    }
}

struct Cached {
    value: Acquisition,
    acquired: SystemTime,
    refresh_at: SystemTime,
}

/// One resource and one credential source per cache. No refresh tokens or fallback.
pub struct Session {
    config: Config,
    client: reqwest::Client,
    cached: Mutex<Option<Cached>>,
}

impl Session {
    pub fn new(config: Config) -> Result<Self, String> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| "could not create workload HTTP client")?;
        Ok(Self {
            config,
            client,
            cached: Mutex::new(None),
        })
    }

    pub async fn invalidate_acquisition(&self, rejected: &Acquisition) {
        let mut cached = self.cached.lock().await;
        if cached
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(&current.value.0, &rejected.0))
        {
            *cached = None;
        }
    }

    pub async fn acquisition(&self) -> Result<Acquisition, String> {
        // Serialize acquisition, not merely publication, to avoid consuming a
        // single-use assertion concurrently. Waiting and all I/O are bounded.
        tokio::time::timeout(TIMEOUT, self.acquire())
            .await
            .map_err(|_| "workload authentication timed out".to_string())?
    }

    async fn acquire(&self) -> Result<Acquisition, String> {
        let mut cached = self.cached.lock().await;
        let now = SystemTime::now();
        if let Some(token) = cached.as_ref()
            && now >= token.acquired
            && now < token.refresh_at
        {
            return Ok(token.value.clone());
        }
        *cached = None;
        if self.config.endpoint_mode == EndpointMode::Discovery {
            self.verify_issuer().await?;
        }
        // Reopen on every exchange: projected files rotate through symlinks.
        let limit = if self.config.endpoint_mode == EndpointMode::Explicit {
            16384
        } else {
            MAX_BYTES
        };
        let assertion = read_assertion_bounded(&self.config.token_file, limit).await?;
        let mut form = Vec::new();
        match self.config.grant {
            Grant::JwtBearer => {
                form.push(("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"));
                form.push(("assertion", assertion.as_str()));
            }
            Grant::TokenExchange => {
                form.push((
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:token-exchange",
                ));
                form.push(("subject_token", assertion.as_str()));
                form.push(("subject_token_type", "urn:ietf:params:oauth:token-type:jwt"));
                form.push(("requested_token_type", ACCESS_TOKEN));
            }
        }
        if !self.config.resource.is_empty() {
            form.push(("resource", self.config.resource.as_str()));
        }
        if let Some(value) = &self.config.audience {
            form.push(("audience", value));
        }
        if let Some(value) = &self.config.scope {
            form.push(("scope", value));
        }
        form.extend(
            self.config
                .extensions
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        );
        let started = SystemTime::now();
        // Never automatically retry a POST: the server may already have consumed
        // this assertion's replay identifier, even if the response was lost.
        let response = self
            .client
            .post(&self.config.token_endpoint)
            .form(&form)
            .send()
            .await
            .map_err(|_| "workload token exchange failed")?;
        if !response.status().is_success() {
            return Err("workload token exchange refused".into());
        }
        let bytes = bounded_body(response).await?;
        let token: Token =
            serde_json::from_slice(&bytes).map_err(|_| "invalid workload token response")?;
        if !token.token_type.eq_ignore_ascii_case("bearer")
            || token.expires_in == Some(0)
            || (self.config.grant == Grant::JwtBearer && token.expires_in.is_none())
            || (self.config.grant == Grant::TokenExchange
                && token.issued_token_type.as_deref() != Some(ACCESS_TOKEN))
            || token.access_token.is_empty()
            || token.access_token.len() > MAX_BYTES
            || !token.access_token.bytes().all(|c| c.is_ascii_graphic())
        {
            return Err("invalid workload token response".into());
        }
        // Cap long issuer lifetimes; subtract a proportional margin for short ones.
        let Some(expires_in) = token.expires_in else {
            // RFC 8693 section 2.2.1 recommends, but does not require, expires_in.
            // Without a lifetime, return this acquisition without caching it.
            return Ok(Acquisition(Arc::new(Zeroizing::new(token.access_token))));
        };
        let lifetime = expires_in.min(86400);
        let usable = Duration::from_secs(lifetime)
            .saturating_sub(Duration::from_millis((lifetime * 100).min(30000)));
        let refresh_at = started
            .checked_add(usable)
            .ok_or("invalid workload token expiry")?;
        let completed = SystemTime::now();
        if completed < started || completed >= refresh_at {
            return Err("workload authentication expired during exchange".into());
        }
        let value = Acquisition(Arc::new(Zeroizing::new(token.access_token)));
        let result = value.clone();
        *cached = Some(Cached {
            value,
            acquired: started,
            refresh_at,
        });
        Ok(result)
    }

    async fn verify_issuer(&self) -> Result<(), String> {
        let issuer = https_url(&self.config.expected_issuer)?;
        let path = issuer.path().trim_end_matches('/');
        let metadata_url = format!(
            "{}/.well-known/oauth-authorization-server{}",
            issuer.origin().ascii_serialization(),
            path
        );
        // Discovery is safe to retry once; no assertion has been read or sent.
        let mut response = self
            .client
            .get(&metadata_url)
            .send()
            .await
            .map_err(|_| "workload issuer discovery failed")?;
        if response.status() == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            tokio::time::sleep(Duration::from_millis(100)).await;
            response = self
                .client
                .get(&metadata_url)
                .send()
                .await
                .map_err(|_| "workload issuer discovery failed")?;
        }
        if !response.status().is_success() {
            return Err("workload issuer discovery refused".into());
        }
        let bytes = bounded_body(response).await?;
        let metadata: Metadata =
            serde_json::from_slice(&bytes).map_err(|_| "invalid workload issuer metadata")?;
        if metadata.issuer != self.config.expected_issuer
            || metadata.token_endpoint != self.config.token_endpoint
        {
            return Err(
                "workload issuer or token endpoint does not match trusted configuration".into(),
            );
        }
        Ok(())
    }
}

#[derive(Deserialize)]
struct Metadata {
    issuer: String,
    token_endpoint: String,
}
#[derive(Deserialize)]
struct Token {
    access_token: String,
    token_type: String,
    #[serde(default, deserialize_with = "optional_expiry")]
    expires_in: Option<u64>,
    issued_token_type: Option<String>,
}

fn optional_expiry<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    u64::deserialize(deserializer).map(Some)
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "could not read workload response")?
    {
        if bytes.len().saturating_add(chunk.len()) > 65536 {
            return Err("workload response exceeds size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
async fn read_assertion(path: &std::path::Path) -> Result<Zeroizing<String>, String> {
    read_assertion_bounded(path, MAX_BYTES).await
}

async fn read_assertion_bounded(
    path: &std::path::Path,
    limit: usize,
) -> Result<Zeroizing<String>, String> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options
        .open(path)
        .await
        .map_err(|_| "could not open workload token file")?;
    if !file
        .metadata()
        .await
        .map_err(|_| "could not inspect workload token file")?
        .is_file()
    {
        return Err("workload token must be a regular file".into());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "could not read workload token file")?;
    if bytes.len() > limit {
        return Err("workload assertion exceeds size limit".into());
    }
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| "invalid workload assertion")?
        .trim_end_matches(['\r', '\n']);
    // Syntax only. Claims are never decoded or treated as authorization.
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|p| {
            p.is_empty()
                || !p
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
        })
    {
        return Err("invalid workload assertion".into());
    }
    Ok(Zeroizing::new(value.to_string()))
}

#[cfg(test)]
mod https_tests;
