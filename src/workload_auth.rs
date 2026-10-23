//! Clientless RFC 7523 workload authentication. Assertions never enter credential storage.
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

use serde::Deserialize;
use tokio::{io::AsyncReadExt, sync::Mutex};
use zeroize::Zeroizing;

static GENERATION: AtomicU64 = AtomicU64::new(0);
const MAX_BYTES: usize = 8192;
const TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(test)]
mod tests;

/// Embedders restoring a process-memory snapshot must call this while requests are
/// quiesced, before resuming work. Transcript restores create new sessions instead.
/// This invalidates every workload cache, including acquisitions started earlier.
pub fn invalidate_after_restore() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// Trusted deployment configuration, not input supplied by an MCP server or model.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub token_file: PathBuf,
    pub expected_issuer: String,
    pub token_endpoint: String,
    pub resource: String,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if !self.token_file.is_absolute() {
            return Err("workload tokenFile must be an absolute path".into());
        }
        let issuer = https_url(&self.expected_issuer)?;
        let endpoint = https_url(&self.token_endpoint)?;
        https_url(&self.resource)?;
        if issuer.query().is_some() || issuer.origin() != endpoint.origin() {
            return Err("workload token endpoint must share the expected issuer origin".into());
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

struct Cached {
    value: Zeroizing<String>,
    acquired: SystemTime,
    refresh_at: SystemTime,
    generation: u64,
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

    pub async fn invalidate(&self) {
        *self.cached.lock().await = None;
    }

    pub async fn token(&self) -> Result<String, String> {
        // Serialize acquisition, not merely publication, to avoid consuming a
        // single-use assertion concurrently. Waiting and all I/O are bounded.
        tokio::time::timeout(TIMEOUT, self.acquire())
            .await
            .map_err(|_| "workload authentication timed out".to_string())?
    }

    async fn acquire(&self) -> Result<String, String> {
        let mut cached = self.cached.lock().await;
        let generation = GENERATION.load(Ordering::Acquire);
        let now = SystemTime::now();
        if let Some(token) = cached.as_ref()
            && token.generation == generation
            && now >= token.acquired
            && now < token.refresh_at
        {
            return Ok(token.value.to_string());
        }
        *cached = None;
        self.verify_issuer().await?;
        // Reopen on every exchange: projected files rotate through symlinks.
        let assertion = read_assertion(&self.config.token_file).await?;
        let started = SystemTime::now();
        // Never automatically retry a POST: the server may already have consumed
        // this assertion's replay identifier, even if the response was lost.
        let response = self
            .client
            .post(&self.config.token_endpoint)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
                ("resource", self.config.resource.as_str()),
            ])
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
            || token.expires_in == 0
            || token.access_token.is_empty()
            || token.access_token.len() > MAX_BYTES
            || !token.access_token.bytes().all(|c| c.is_ascii_graphic())
        {
            return Err("invalid workload token response".into());
        }
        // Cap long issuer lifetimes; subtract a proportional margin for short ones.
        let lifetime = token.expires_in.min(86400);
        let usable = Duration::from_secs(lifetime)
            .saturating_sub(Duration::from_millis((lifetime * 100).min(30000)));
        let refresh_at = started
            .checked_add(usable)
            .ok_or("invalid workload token expiry")?;
        if GENERATION.load(Ordering::Acquire) != generation || SystemTime::now() >= refresh_at {
            return Err("workload authentication invalidated or expired during exchange".into());
        }
        let value = Zeroizing::new(token.access_token);
        let result = value.to_string();
        *cached = Some(Cached {
            value,
            acquired: started,
            refresh_at,
            generation,
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
    expires_in: u64,
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

async fn read_assertion(path: &std::path::Path) -> Result<Zeroizing<String>, String> {
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
    file.take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "could not read workload token file")?;
    if bytes.len() > MAX_BYTES {
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
