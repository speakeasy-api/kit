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
        // A pin the operator wrote but got wrong is never treated as absent.
        if self.settings.invalid_issuer {
            return Err(denied());
        }
        let metadata = context.metadata;
        let issuer = https(&metadata.issuer)?;
        let resource = https(&metadata.resource)?;
        if resource.origin() != context.target.destination().origin() {
            return Err(denied());
        }
        match self.pinned_issuer(context.target) {
            Some(pinned) if pinned != metadata.issuer => return Err(denied()),
            Some(_) => {}
            // A secret is never disclosed to an issuer the backend merely
            // advertised: confidential clients need a configured one.
            None if context.client_secret_ref.is_some() || self.confidential(context.target) => {
                return Err(denied());
            }
            None if !self.settings.trust_advertised_issuer => return Err(denied()),
            None => {}
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

    /// The SDK adds the operation's required scopes to every acquisition
    /// itself, so the selection is always the target's requested scopes.
    fn select(
        &self,
        target: &AuthTarget,
        resource: &ResourceMetadata,
        _required: &[String],
    ) -> Result<TrustDecision, HookError> {
        if self.settings.invalid_issuer {
            return Err(denied());
        }
        let destination = target.destination();
        if https(&resource.resource)?.origin() != destination.origin() {
            return Err(denied());
        }
        // Without a configured issuer, the advertised one is trusted only on
        // explicit opt-in, and never for a confidential client.
        if self.pinned_issuer(target).is_none()
            && (self.confidential(target) || !self.settings.trust_advertised_issuer)
        {
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
        Ok(TrustDecision::new(issuer, self.requested(target)))
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

    /// The scopes every sign-in asks for, none of them mandatory: a
    /// preset's `scopes` when it has them (the SDK requires a preset's to be
    /// exactly the request), otherwise the backend's `oauth.scopes`.
    pub(super) fn requested(&self, target: &AuthTarget) -> Vec<String> {
        let scopes = match target.preset_scopes() {
            Some(preset) => preset.to_vec(),
            None => self.settings.scopes.clone().unwrap_or_default(),
        };
        normalized(scopes)
    }

    /// The most Kit accepts for the target: its request and the backend's
    /// `oauth.scopes`. A step-up a backend asks for must lie within these.
    pub(super) fn ceiling(&self, target: &AuthTarget) -> Vec<String> {
        let mut scopes = self.requested(target);
        scopes.extend(self.settings.scopes.iter().flatten().cloned());
        normalized(scopes)
    }
}

fn normalized(mut scopes: Vec<String>) -> Vec<String> {
    scopes.sort();
    scopes.dedup();
    scopes
}
