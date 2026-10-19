//! Non-interactive credentials never enter the persistent interactive OAuth store.
use std::path::PathBuf;

use rmcp::transport::auth::{AuthorizationManager, ClientCredentialsConfig, JwtSigningAlgorithm};
use serde::Deserialize;
use tracing::instrument::WithSubscriber;
use zeroize::Zeroizing;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub client_id: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    pub resource: String,
    pub expected_issuer: String,
    pub method: Method,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Method {
    ClientSecret {
        client_secret_file: PathBuf,
    },
    PrivateKeyJwt {
        private_key_file: PathBuf,
        #[serde(default)]
        signing_algorithm: Algorithm,
        token_endpoint_audience: Option<String>,
    },
}

#[derive(Clone, Copy, Default, Deserialize)]
pub enum Algorithm {
    #[default]
    RS256,
    RS384,
    RS512,
    ES256,
    ES384,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        let path = match &self.method {
            Method::ClientSecret { client_secret_file } => client_secret_file,
            Method::PrivateKeyJwt {
                private_key_file, ..
            } => private_key_file,
        };
        if self.client_id.trim().is_empty() || !path.is_absolute() {
            return Err(
                "machine MCP auth requires a client_id and an absolute credential file path".into(),
            );
        }
        for value in [&self.resource, &self.expected_issuer] {
            let url = url::Url::parse(value).map_err(|_| "invalid machine OAuth URL")?;
            if url.scheme() != "https"
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err(
                    "machine OAuth URLs must be HTTPS without credentials or fragments".into(),
                );
            }
        }
        Ok(())
    }

    async fn credentials(&self) -> Result<ClientCredentialsConfig, String> {
        let scopes = self.scopes.clone();
        let resource = Some(self.resource.clone());
        let client_id = self.client_id.clone();
        match &self.method {
            Method::ClientSecret { client_secret_file } => {
                let bytes = read_secret(client_secret_file).await?;
                let secret = std::str::from_utf8(&bytes)
                    .map_err(|_| "machine OAuth secret must be UTF-8")?;
                if secret.trim_end_matches(['\r', '\n']).is_empty() {
                    return Err("machine OAuth secret must not be empty".into());
                }
                Ok(ClientCredentialsConfig::ClientSecret {
                    client_id,
                    client_secret: secret.trim_end_matches(['\r', '\n']).to_owned(),
                    scopes,
                    resource,
                })
            }
            Method::PrivateKeyJwt {
                private_key_file,
                signing_algorithm,
                token_endpoint_audience,
            } => {
                let key = read_secret(private_key_file).await?;
                let signing_algorithm = match signing_algorithm {
                    Algorithm::RS256 => JwtSigningAlgorithm::RS256,
                    Algorithm::RS384 => JwtSigningAlgorithm::RS384,
                    Algorithm::RS512 => JwtSigningAlgorithm::RS512,
                    Algorithm::ES256 => JwtSigningAlgorithm::ES256,
                    Algorithm::ES384 => JwtSigningAlgorithm::ES384,
                };
                Ok(ClientCredentialsConfig::PrivateKeyJwt {
                    client_id,
                    signing_key: key.to_vec(),
                    signing_algorithm,
                    token_endpoint_audience: token_endpoint_audience.clone(),
                    scopes,
                    resource,
                })
            }
        }
    }
}

async fn read_secret(path: &std::path::Path) -> Result<Zeroizing<Vec<u8>>, String> {
    use tokio::io::AsyncReadExt;
    // Provisioning may rotate symlinks; check both the path and opened handle.
    // Nonblocking open also rejects a FIFO substituted between those checks.
    if !tokio::fs::metadata(path)
        .await
        .map_err(|_| "could not inspect machine OAuth credential file")?
        .is_file()
    {
        return Err("machine OAuth credential must be a regular file".into());
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options
        .open(path)
        .await
        .map_err(|_| "could not open machine OAuth credential file")?;
    if !file
        .metadata()
        .await
        .map_err(|_| "could not inspect machine OAuth credential file")?
        .is_file()
    {
        return Err("machine OAuth credential must be a regular file".into());
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(65537)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "could not read machine OAuth credential file")?;
    if bytes.is_empty() || bytes.len() > 65536 {
        return Err("machine OAuth credential must contain 1–65536 bytes".into());
    }
    Ok(bytes)
}

pub async fn authenticate(
    url: &str,
    config: &Config,
) -> Result<(String, AuthorizationManager), String> {
    config.validate()?;
    validate_transport_url(url)?;
    // rmcp 3.5.1's JWT exchange retains a non-Send form serializer across
    // its HTTP await. Poll the upstream flow on one blocking worker, while the
    // caller remains Send. Dropping the caller cancels the worker's async flow;
    // no detached acquisition can publish into the owning OAuth session.
    let url = url.to_owned();
    let config = config.clone();
    let runtime = tokio::runtime::Handle::current();
    let (cancel_on_drop, cancelled) = tokio::sync::oneshot::channel::<()>();
    let worker = tokio::task::spawn_blocking(move || {
        runtime.block_on(async move {
            let acquisition = std::pin::pin!(tokio::time::timeout(
                std::time::Duration::from_secs(60),
                async {
                    let manager = AuthorizationManager::new(url.as_str())
                        .await
                        .map_err(|_| "machine OAuth setup failed")?;
                    authenticate_manager(manager, &config).await
                }
            ));
            match futures_util::future::select(cancelled, acquisition).await {
                futures_util::future::Either::Left(_) => {
                    Err("machine OAuth acquisition cancelled".to_owned())
                }
                futures_util::future::Either::Right((result, _)) => {
                    result.map_err(|_| "machine OAuth acquisition timed out".to_owned())?
                }
            }
        })
    });
    let result = worker
        .await
        .map_err(|_| "machine OAuth worker failed".to_owned());
    drop(cancel_on_drop);
    result?
}

async fn authenticate_manager(
    mut manager: AuthorizationManager,
    config: &Config,
) -> Result<(String, AuthorizationManager), String> {
    let resolution = manager
        .resolve_metadata()
        .await
        .map_err(|_| "machine OAuth discovery failed")?;
    if resolution.metadata.issuer.as_deref() != Some(config.expected_issuer.as_str()) {
        return Err("machine OAuth issuer does not match expected_issuer".into());
    }
    let issuer = url::Url::parse(&config.expected_issuer).map_err(|_| "invalid expected issuer")?;
    let endpoint = url::Url::parse(&resolution.metadata.token_endpoint)
        .map_err(|_| "invalid machine OAuth token endpoint")?;
    if endpoint.scheme() != "https" || endpoint.origin() != issuer.origin() {
        return Err(
            "machine OAuth token endpoint must share the expected HTTPS issuer origin".into(),
        );
    }
    manager.set_metadata(resolution.metadata);
    // Use the same rmcp sequence as OAuthState::authenticate_client_credentials,
    // but pin the discovered issuer before reading or sending any credential.
    let credentials = config.credentials().await?;
    manager
        .validate_client_credentials_metadata(&credentials)
        .map_err(|_| "machine OAuth method is not supported by the issuer")?;
    manager
        .configure_client_credentials(&credentials)
        .map_err(|_| "machine OAuth client configuration failed")?;
    // rmcp debug-logs the token response, including arbitrary vendor fields.
    // Silence this future only; a vendor response must not become a log channel
    // for secrets or assertions reflected by an issuer. Errors remain redacted.
    manager
        .exchange_client_credentials(&credentials)
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await
        .map_err(|_| "machine OAuth token exchange failed")?;
    // Do not call get_access_token: it may choose refresh_token for short-lived
    // tokens. Machine sessions always reacquire with client_credentials.
    let (_, token) = manager
        .get_credentials()
        .await
        .map_err(|_| "machine OAuth token unavailable")?;
    let token = token.ok_or("machine OAuth token unavailable")?;
    let token: Token = serde_json::from_value(
        serde_json::to_value(token).map_err(|_| "machine OAuth token unavailable")?,
    )
    .map_err(|_| "machine OAuth token unavailable")?;
    Ok((token.access_token, manager))
}

#[derive(Deserialize)]
pub struct Token {
    pub access_token: String,
    pub expires_in: Option<u64>,
}

pub async fn expiry(manager: &AuthorizationManager, acquired_at: u64) -> Result<u64, String> {
    let (_, token) = manager
        .get_credentials()
        .await
        .map_err(|_| "machine OAuth token unavailable")?;
    let token: Token = serde_json::from_value(
        serde_json::to_value(token).map_err(|_| "machine OAuth token unavailable")?,
    )
    .map_err(|_| "machine OAuth token unavailable")?;
    Ok(token
        .expires_in
        .map(|seconds| acquired_at.saturating_add(seconds.saturating_sub(30)))
        .unwrap_or(u64::MAX))
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(u64::MAX)
}

pub fn validate_transport_url(value: &str) -> Result<(), String> {
    let url = url::Url::parse(value).map_err(|_| "invalid machine MCP URL")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("machine MCP URL must be HTTPS without credentials or fragments".into());
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests;
