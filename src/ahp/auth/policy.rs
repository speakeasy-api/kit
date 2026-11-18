//! Which authorization servers and endpoints Kit trusts for a hook backend.
use agenthooksprotocol::{
    auth::{
        AuthErrorCode, AuthTarget,
        managed::{EndpointTrust, OAuthPolicy, ResourceMetadata, TrustDecision},
    },
    client::HookError,
};
use url::Url;

use super::config::Settings;

fn denied() -> HookError {
    HookError::authentication(AuthErrorCode::UntrustedDestination)
}

/// An absolute HTTPS URL without credentials or a fragment.
fn https(value: &str) -> Result<Url, HookError> {
    let url = Url::parse(value).map_err(|_| denied())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(denied());
    }
    Ok(url)
}

/// A backend's authorization server is the one its `authentication` preset
/// or Kit configuration names; otherwise the one its protected resource
/// metadata names. The resource is always the backend's own origin.
pub(super) struct Policy {
    pub(super) settings: Settings,
}

impl OAuthPolicy for Policy {
    fn approve_endpoints(&self, context: EndpointTrust<'_>) -> Result<(), HookError> {
        let metadata = context.metadata;
        let issuer = https(&metadata.issuer)?;
        let resource = https(&metadata.resource)?;
        if resource.origin() != context.target.destination().origin() {
            return Err(denied());
        }
        match self.pinned_issuer(context.target) {
            Some(pinned) if pinned != metadata.issuer => return Err(denied()),
            // A secret is never disclosed to an issuer the backend merely
            // advertised: confidential clients need a configured one.
            None if context.client_secret_ref.is_some() || self.confidential(context.target) => {
                return Err(denied());
            }
            _ => {}
        }
        let token = https(&metadata.token_endpoint)?;
        for endpoint in metadata
            .authorization_endpoint
            .iter()
            .chain(&metadata.registration_endpoint)
        {
            https(endpoint)?;
        }
        // A client secret is only ever sent to its issuer's own origin.
        if context.client_secret_ref.is_some() && token.origin() != issuer.origin() {
            return Err(denied());
        }
        Ok(())
    }

    fn select(
        &self,
        target: &AuthTarget,
        resource: &ResourceMetadata,
        challenge_scopes: &[String],
    ) -> Result<TrustDecision, HookError> {
        let destination = target.destination();
        if https(&resource.resource)?.origin() != destination.origin() {
            return Err(denied());
        }
        if self.confidential(target) && self.pinned_issuer(target).is_none() {
            return Err(denied());
        }
        let issuer = match self.pinned_issuer(target) {
            Some(pinned) => resource
                .authorization_servers
                .iter()
                .find(|server| **server == pinned)
                .cloned(),
            None => resource
                .authorization_servers
                .iter()
                .find(|server| https(server).is_ok())
                .cloned(),
        }
        .ok_or_else(denied)?;
        let mut scopes = self.requested(target);
        for scope in challenge_scopes {
            if !scopes.contains(scope) {
                scopes.push(scope.clone());
            }
        }
        Ok(TrustDecision::new(issuer, scopes))
    }
}

impl Policy {
    fn pinned_issuer(&self, target: &AuthTarget) -> Option<String> {
        self.settings
            .issuer
            .clone()
            .or_else(|| target.issuer().map(str::to_owned))
    }

    /// Client credentials: the client authenticates with a secret.
    fn confidential(&self, target: &AuthTarget) -> bool {
        let flow = match target.binding() {
            agenthooksprotocol::auth::AuthBinding::OAuth { flow, .. } => Some(*flow),
            _ => self.settings.flow,
        };
        flow == Some(agenthooksprotocol::auth::OAuthFlow::ClientCredentials)
    }

    /// The scopes the backend's settings name, else its preset's.
    pub(super) fn requested(&self, target: &AuthTarget) -> Vec<String> {
        self.settings
            .scopes
            .clone()
            .unwrap_or_else(|| target.scopes().to_vec())
    }
}
