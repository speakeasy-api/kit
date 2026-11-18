//! A backend's OAuth settings: its registration `authentication` preset, and
//! the `oauth` object beside it in hooks.json, which AHP harness adapters share.
//! AHP receivers ignore unknown registration members, so the SDK passes it by.
//!
//! ```json
//! "oauth": {
//!   "issuer": "https://auth.example.com",
//!   "flow": "authorization_code_pkce",
//!   "scopes": ["hooks"],
//!   "clientId": "kit",
//!   "clientSecretRef": "example-secret",
//!   "clientMetadataDocument": { "url": "https://…/client.json", "redirectUris": ["http://127.0.0.1:30123/callback"] },
//!   "registration": true,
//!   "redirectPort": 30123,
//!   "loginLifetimeMinutes": 10,
//!   "allowPrivateNetwork": false,
//!   "trustAdvertisedIssuer": false
//! }
//! ```
use std::time::Duration;

use agenthooksprotocol::auth::OAuthFlow;
use serde_json::Value;

const MAX_LIFETIME: Duration = Duration::from_secs(600);

/// Permission to register a client (RFC 7591) at the backend's `issuer`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Registration {
    pub client_name: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub initial_access_token_ref: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Settings {
    /// The one authorization server trusted for this backend.
    pub issuer: Option<String>,
    pub flow: Option<OAuthFlow>,
    pub scopes: Option<Vec<String>>,
    /// A client registered in advance. It always wins over CIMD and DCR.
    pub client_id: Option<String>,
    /// For client credentials: a key in `~/.kit/ahp-secrets.json`.
    pub client_secret_ref: Option<String>,
    /// A published Client ID Metadata Document and its `redirect_uris`.
    pub client_metadata_document: Option<(String, Vec<String>)>,
    pub registration: Option<Registration>,
    /// The fixed loopback port of this backend's sign-in callback.
    pub redirect_port: u16,
    pub login_lifetime: Duration,
    /// Allow the authorization server on loopback or private addresses.
    pub allow_private_network: bool,
    /// Trust the issuer the backend advertises when none is configured.
    pub trust_advertised_issuer: bool,
    /// `issuer` was given but is not an https URL: the backend's OAuth is
    /// refused until it is fixed, never treated as unpinned.
    pub invalid_issuer: bool,
    /// An `oauth` object was given: the user expects OAuth here.
    pub configured: bool,
}

/// A stable port per backend, so every Kit process derives the same
/// redirect. 30000 to 32767 lies below the Linux (32768+) and macOS (49152+)
/// ephemeral ranges and apart from other AHP adapters' (pi: 20000 to 29999),
/// so neither an outgoing connection nor another harness takes it.
pub(super) fn default_port(backend: &str) -> u16 {
    let hash = blake3::hash(backend.as_bytes());
    let bytes = hash.as_bytes();
    30000 + u16::from_le_bytes([bytes[0], bytes[1]]) % 2768
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|item| item.as_str().filter(|s| !s.is_empty()).map(str::to_owned))
        .collect()
}

fn text(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

impl Settings {
    /// Reads one hooks.json backend entry. Problems name settings that were
    /// ignored.
    pub(super) fn parse(backend: &Value) -> (Self, Vec<String>) {
        let id = backend["id"].as_str().unwrap_or_default();
        let mut problems = Vec::new();
        let mut settings = Self {
            issuer: None,
            flow: None,
            scopes: None,
            client_id: None,
            client_secret_ref: None,
            client_metadata_document: None,
            registration: None,
            redirect_port: default_port(id),
            login_lifetime: MAX_LIFETIME,
            allow_private_network: false,
            trust_advertised_issuer: false,
            invalid_issuer: false,
            configured: backend["oauth"].is_object(),
        };
        let raw = &backend["oauth"];
        if !raw.is_null() && !raw.is_object() {
            problems.push(format!("{id}: oauth is not an object; ignored"));
        }
        let mut ignore = |name: &str, why: &str| {
            problems.push(format!("{id}: oauth.{name} {why}; ignored"));
        };
        let given = |name: &str| raw.get(name).filter(|value| !value.is_null());
        if let Some(issuer) = given("issuer") {
            match issuer.as_str().filter(|s| s.starts_with("https://")) {
                Some(issuer) => settings.issuer = Some(issuer.to_owned()),
                None => settings.invalid_issuer = true,
            }
        }
        if let Some(flow) = given("flow") {
            match flow.as_str() {
                Some("authorization_code_pkce") => {
                    settings.flow = Some(OAuthFlow::AuthorizationCodePkce);
                }
                Some("client_credentials") => settings.flow = Some(OAuthFlow::ClientCredentials),
                _ => ignore(
                    "flow",
                    "is not \"authorization_code_pkce\" or \"client_credentials\"",
                ),
            }
        }
        for (name, slot) in [
            ("clientId", &mut settings.client_id),
            ("clientSecretRef", &mut settings.client_secret_ref),
        ] {
            if let Some(value) = given(name) {
                match text(value) {
                    Some(value) => *slot = Some(value),
                    None => ignore(name, "is not a string"),
                }
            }
        }
        if let Some(scopes) = given("scopes") {
            match strings(scopes) {
                Some(scopes) => settings.scopes = Some(scopes),
                None => ignore("scopes", "is not a list of strings"),
            }
        }
        if let Some(document) = given("clientMetadataDocument") {
            match (text(&document["url"]), strings(&document["redirectUris"])) {
                (Some(url), Some(redirects)) => {
                    settings.client_metadata_document = Some((url, redirects));
                }
                _ => ignore("clientMetadataDocument", "needs url and redirectUris"),
            }
        }
        match given("registration") {
            None | Some(Value::Bool(false)) => {}
            Some(Value::Bool(true)) => settings.registration = Some(Registration::default()),
            Some(Value::Object(registration)) => {
                let field = |name: &str| registration.get(name).unwrap_or(&Value::Null);
                settings.registration = Some(Registration {
                    client_name: text(field("clientName")),
                    scopes: strings(field("scopes")),
                    initial_access_token_ref: text(field("initialAccessTokenRef")),
                });
            }
            Some(_) => ignore("registration", "is not true or an object"),
        }
        // Permission to register is for one issuer, named here.
        if settings.registration.is_some() && settings.issuer.is_none() {
            settings.registration = None;
            ignore("registration", "needs oauth.issuer");
        }
        if let Some(port) = given("redirectPort") {
            match port
                .as_u64()
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port >= 1024)
            {
                Some(port) => settings.redirect_port = port,
                None => ignore("redirectPort", "is not a port from 1024 to 65535"),
            }
        }
        if let Some(minutes) = given("loginLifetimeMinutes") {
            match minutes.as_f64().filter(|m| *m > 0.0 && *m <= 10.0) {
                Some(minutes) => {
                    settings.login_lifetime = Duration::from_secs_f64(minutes * 60.0);
                }
                None => ignore("loginLifetimeMinutes", "is not more than 0 and at most 10"),
            }
        }
        for (name, slot) in [
            ("allowPrivateNetwork", &mut settings.allow_private_network),
            (
                "trustAdvertisedIssuer",
                &mut settings.trust_advertised_issuer,
            ),
        ] {
            if let Some(value) = given(name) {
                match value.as_bool() {
                    Some(value) => *slot = value,
                    None => ignore(name, "is not a boolean"),
                }
            }
        }
        if settings.invalid_issuer {
            problems.push(format!(
                "{id}: oauth.issuer is not an https URL; OAuth for this backend is refused until it is fixed"
            ));
        }
        (settings, problems)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_shared_oauth_object() {
        let (settings, problems) = Settings::parse(&json!({"id": "com.example", "oauth": {
            "issuer": "https://auth.example", "flow": "client_credentials", "scopes": ["hooks"],
            "clientId": "kit", "clientSecretRef": "secret",
            "clientMetadataDocument": {"url": "https://kit.example/client.json",
                "redirectUris": ["http://127.0.0.1:50000/callback"]},
            "registration": {"clientName": "Kit", "initialAccessTokenRef": "iat"},
            "redirectPort": 50000, "loginLifetimeMinutes": 5, "allowPrivateNetwork": true
        }}));
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(settings.issuer.as_deref(), Some("https://auth.example"));
        assert_eq!(settings.flow, Some(OAuthFlow::ClientCredentials));
        assert_eq!(settings.redirect_port, 50000);
        assert_eq!(settings.login_lifetime, Duration::from_secs(300));
        assert!(settings.allow_private_network && settings.configured);
        assert_eq!(
            settings
                .registration
                .unwrap()
                .initial_access_token_ref
                .as_deref(),
            Some("iat")
        );
    }

    #[test]
    fn invalid_settings_are_named_and_ignored() {
        let (settings, problems) = Settings::parse(&json!({"id": "b", "oauth": {
            "issuer": "http://auth.example", "registration": true, "redirectPort": 80,
            "loginLifetimeMinutes": 30
        }}));
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(settings.invalid_issuer && settings.issuer.is_none());
        assert!(
            problems
                .iter()
                .any(|problem| problem.contains("refused until it is fixed"))
        );
        assert!(settings.registration.is_none());
        assert_eq!(settings.redirect_port, default_port("b"));
        assert_eq!(settings.login_lifetime, MAX_LIFETIME);
        let (unconfigured, problems) = Settings::parse(&json!({"id": "b"}));
        assert!(problems.is_empty() && !unconfigured.configured);
    }
}
