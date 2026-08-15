use std::time::Duration;

use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, OAuthClientConfig,
};
use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::timeout,
};

use super::CredentialStorage;

pub const FLOW_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_CALLBACK_BYTES: usize = 16 * 1024;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(rename = "type")]
    _kind: Kind,
    #[serde(rename = "clientId")]
    client_id: Option<String>,
    #[serde(rename = "clientMetadataUrl")]
    client_metadata_url: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Oauth,
}

pub struct PendingAuthorization {
    pub url: String,
    listener: TcpListener,
    session: AuthorizationSession,
}

pub async fn begin(
    resource_url: &str,
    config: &Config,
    credential_storage: &CredentialStorage,
    required_scope: Option<&str>,
) -> Result<PendingAuthorization, String> {
    let mut manager = manager(resource_url, config, credential_storage).await?;
    let metadata = manager
        .resolve_metadata()
        .await
        .map_err(|error| format!("OAuth discovery failed: {error}"))?;
    manager.set_metadata(metadata.metadata);
    if config.client_id.is_some() {
        manager
            .initialize_from_store()
            .await
            .map_err(|error| format!("could not restore OAuth client: {error}"))?;
    }

    let defaults = config.scopes.iter().map(String::as_str).collect::<Vec<_>>();
    let scopes = manager.select_scopes(required_scope, &defaults);
    let scope_refs = scopes.iter().map(String::as_str).collect::<Vec<_>>();
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("could not bind OAuth callback: {error}"))?;
    let redirect_uri = format!(
        "http://{}/callback",
        listener
            .local_addr()
            .map_err(|error| format!("could not inspect OAuth callback: {error}"))?
    );

    let session = if let Some(client_id) = config.client_id.as_deref() {
        manager
            .configure_client(
                OAuthClientConfig::new(client_id, &redirect_uri).with_scopes(scopes.clone()),
            )
            .map_err(|error| format!("OAuth client configuration failed: {error}"))?;
        let url = manager
            .get_authorization_url(&scope_refs)
            .await
            .map_err(|error| format!("could not create OAuth URL: {error}"))?;
        AuthorizationSession::for_scope_upgrade(manager, url, &redirect_uri)
    } else {
        let mut request = AuthorizationRequest::new(&redirect_uri)
            .with_scopes(scopes)
            .with_client_name("Kit");
        if let Some(url) = config.client_metadata_url.as_deref() {
            request = request.with_client_metadata_url(url);
        }
        AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, error)| format!("OAuth client registration failed: {error}"))?
    };
    Ok(PendingAuthorization {
        url: session.get_authorization_url().to_string(),
        listener,
        session,
    })
}

pub async fn restore(
    resource_url: &str,
    config: &Config,
    credential_storage: &CredentialStorage,
) -> Result<Option<(String, AuthorizationManager)>, String> {
    if !credential_storage.is_persistent() {
        return Ok(None);
    }
    let mut manager = manager(resource_url, config, credential_storage).await?;
    if !manager
        .initialize_from_store()
        .await
        .map_err(|error| format!("could not restore OAuth credentials: {error}"))?
    {
        return Ok(None);
    }
    match manager.get_access_token().await {
        Ok(token) => Ok(Some((token, manager))),
        Err(AuthError::AuthorizationRequired) => Ok(None),
        Err(error) => Err(format!("could not restore OAuth access token: {error}")),
    }
}

async fn manager(
    resource_url: &str,
    config: &Config,
    credential_storage: &CredentialStorage,
) -> Result<AuthorizationManager, String> {
    let mut manager = AuthorizationManager::new(resource_url)
        .await
        .map_err(|error| format!("OAuth setup failed: {error}"))?;
    credential_storage.configure(&mut manager, &credential_identity(resource_url, config));
    Ok(manager)
}

fn credential_identity(resource_url: &str, config: &Config) -> String {
    format!(
        "{resource_url}\0{}\0{}",
        config.client_id.as_deref().unwrap_or_default(),
        config.client_metadata_url.as_deref().unwrap_or_default()
    )
}

pub async fn finish(
    pending: PendingAuthorization,
) -> Result<(String, AuthorizationManager), String> {
    timeout(
        FLOW_TIMEOUT,
        wait_for_callback(&pending.listener, &pending.session),
    )
    .await
    .map_err(|_| "OAuth callback timed out".to_string())??;
    let token = pending
        .session
        .auth_manager
        .get_access_token()
        .await
        .map_err(|error| format!("OAuth token was unavailable: {error}"))?;
    Ok((token, pending.session.auth_manager))
}

async fn wait_for_callback(
    listener: &TcpListener,
    session: &AuthorizationSession,
) -> Result<(), String> {
    let address = listener
        .local_addr()
        .map_err(|error| format!("could not inspect OAuth callback: {error}"))?;
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|error| format!("OAuth callback failed: {error}"))?;
        let target = match timeout(Duration::from_secs(5), request_target(&mut stream)).await {
            Ok(Ok(target))
                if target.starts_with("/callback?")
                    && ((has_query_param(&target, "code")
                        && has_query_param(&target, "state"))
                        || has_query_param(&target, "error")) =>
            {
                target
            }
            _ => {
                respond(&mut stream, false).await;
                continue;
            }
        };
        let explicit_error = has_query_param(&target, "error");
        let callback_url = format!("http://{address}{target}");
        let result = session.handle_callback_url(&callback_url).await;
        respond(&mut stream, result.is_ok()).await;
        if result.is_err() && !explicit_error {
            continue;
        }
        return result
            .map(|_| ())
            .map_err(|error| format!("OAuth callback was rejected: {error}"));
    }
}

async fn request_target(stream: &mut TcpStream) -> Result<String, String> {
    let mut bytes = Vec::new();
    loop {
        if bytes.len() >= MAX_CALLBACK_BYTES {
            return Err("OAuth callback request was too large".into());
        }
        let mut chunk = [0; 1024];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| format!("could not read OAuth callback: {error}"))?;
        if read == 0 {
            return Err("OAuth callback closed before sending a request".into());
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    parse_request_target(&bytes)
}

fn has_query_param(target: &str, name: &str) -> bool {
    target
        .split_once('?')
        .map(|(_, query)| {
            query
                .split('&')
                .filter_map(|field| field.split_once('='))
                .any(|(key, value)| key == name && !value.is_empty())
        })
        .unwrap_or(false)
}

fn parse_request_target(bytes: &[u8]) -> Result<String, String> {
    let request =
        std::str::from_utf8(bytes).map_err(|_| "OAuth callback was not valid HTTP".to_string())?;
    let mut parts = request
        .lines()
        .next()
        .ok_or_else(|| "OAuth callback was empty".to_string())?
        .split_whitespace();
    if parts.next() != Some("GET") {
        return Err("OAuth callback must use GET".into());
    }
    parts
        .next()
        .map(str::to_string)
        .ok_or_else(|| "OAuth callback omitted its target".into())
}

async fn respond(stream: &mut TcpStream, success: bool) {
    let (status, body) = if success {
        (
            "200 OK",
            "MCP authentication complete. You can return to Kit.",
        )
    } else {
        (
            "400 Bad Request",
            "MCP authentication failed. You can return to Kit and retry.",
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::{has_query_param, parse_request_target};

    #[test]
    fn callback_accepts_only_get_requests_with_targets() {
        assert_eq!(
            parse_request_target(b"GET /callback?code=x&state=y HTTP/1.1\r\n\r\n").unwrap(),
            "/callback?code=x&state=y"
        );
        assert!(parse_request_target(b"POST /callback HTTP/1.1\r\n\r\n").is_err());
        assert!(parse_request_target(b"GET\r\n\r\n").is_err());
        assert!(has_query_param("/callback?code=x&state=y", "state"));
        assert!(!has_query_param("/callback?junk", "state"));
    }
}
