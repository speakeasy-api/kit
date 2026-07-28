use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Mutex,
};

use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{AlgorithmParameters, EllipticCurve, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse},
};
use serde::Deserialize;

use crate::api::auth::contract::{
    AuthDecision, AuthDenial, AuthenticatedPrincipal, Authenticator, GrantSnapshot,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OidcAlgorithm {
    Rs256,
    Es256,
    EdDsa,
}

impl OidcAlgorithm {
    const fn jwt(self) -> Algorithm {
        match self {
            Self::Rs256 => Algorithm::RS256,
            Self::Es256 => Algorithm::ES256,
            Self::EdDsa => Algorithm::EdDSA,
        }
    }

    fn from_jwt(algorithm: Algorithm) -> Option<Self> {
        match algorithm {
            Algorithm::RS256 => Some(Self::Rs256),
            Algorithm::ES256 => Some(Self::Es256),
            Algorithm::EdDSA => Some(Self::EdDsa),
            _ => None,
        }
    }

    fn from_jwk(algorithm: KeyAlgorithm) -> Option<Self> {
        match algorithm {
            KeyAlgorithm::RS256 => Some(Self::Rs256),
            KeyAlgorithm::ES256 => Some(Self::Es256),
            KeyAlgorithm::EdDSA => Some(Self::EdDsa),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OidcRevocations {
    token_ids: BTreeSet<String>,
    session_ids: BTreeSet<String>,
}

impl OidcRevocations {
    pub fn new(
        token_ids: impl IntoIterator<Item = String>,
        session_ids: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            token_ids: token_ids.into_iter().collect(),
            session_ids: session_ids.into_iter().collect(),
        }
    }

    fn contains(&self, claims: &Claims) -> bool {
        claims
            .jti
            .as_ref()
            .is_some_and(|id| self.token_ids.contains(id))
            || claims
                .sid
                .as_ref()
                .is_some_and(|id| self.session_ids.contains(id))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OidcSetupError {
    InvalidJwks,
    NoUsableKeys,
}

impl fmt::Display for OidcSetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidJwks => "invalid or ambiguous OIDC JWKS",
            Self::NoUsableKeys => "OIDC JWKS has no allowlisted asymmetric verification keys",
        })
    }
}

impl std::error::Error for OidcSetupError {}

#[derive(Clone, Copy)]
pub struct OidcObservation<'a> {
    authorization: &'a str,
    now: u64,
}

impl<'a> OidcObservation<'a> {
    pub fn from_transport(authorization: &'a str, now: u64) -> Self {
        Self { authorization, now }
    }
}

struct VerificationKey {
    algorithm: OidcAlgorithm,
    key: DecodingKey,
}

pub struct OidcAuthenticator {
    issuer: String,
    audience: String,
    allowed_algorithms: BTreeSet<OidcAlgorithm>,
    keys_by_id: BTreeMap<String, VerificationKey>,
    grants_by_subject: BTreeMap<String, GrantSnapshot>,
    revocations: Mutex<OidcRevocations>,
}

impl OidcAuthenticator {
    pub fn new(
        issuer: impl Into<String>,
        audience: impl Into<String>,
        jwks_json: &str,
        allowed_algorithms: impl IntoIterator<Item = OidcAlgorithm>,
        revocations: OidcRevocations,
        grants_by_subject: BTreeMap<String, GrantSnapshot>,
    ) -> Result<Self, OidcSetupError> {
        let allowed_algorithms = allowed_algorithms.into_iter().collect::<BTreeSet<_>>();
        let jwks =
            serde_json::from_str::<JwkSet>(jwks_json).map_err(|_| OidcSetupError::InvalidJwks)?;
        let mut seen_ids = BTreeSet::new();
        let mut keys_by_id = BTreeMap::new();
        for jwk in &jwks.keys {
            let Some(key_id) = jwk.common.key_id.as_ref() else {
                continue;
            };
            if !seen_ids.insert(key_id.clone()) {
                return Err(OidcSetupError::InvalidJwks);
            }
            let Some(algorithm) = jwk.common.key_algorithm.and_then(OidcAlgorithm::from_jwk) else {
                continue;
            };
            if !allowed_algorithms.contains(&algorithm) {
                continue;
            }
            if matches!(
                jwk.common.public_key_use,
                Some(PublicKeyUse::Encryption | PublicKeyUse::Other(_))
            ) || jwk
                .common
                .key_operations
                .as_ref()
                .is_some_and(|operations| !operations.contains(&KeyOperations::Verify))
                || !key_type_matches(algorithm, &jwk.algorithm)
            {
                return Err(OidcSetupError::InvalidJwks);
            }
            let key = DecodingKey::from_jwk(jwk).map_err(|_| OidcSetupError::InvalidJwks)?;
            keys_by_id.insert(key_id.clone(), VerificationKey { algorithm, key });
        }
        if keys_by_id.is_empty() {
            return Err(OidcSetupError::NoUsableKeys);
        }
        Ok(Self {
            issuer: issuer.into(),
            audience: audience.into(),
            allowed_algorithms,
            keys_by_id,
            grants_by_subject,
            revocations: Mutex::new(revocations),
        })
    }

    pub fn revoke_token(&self, token_id: impl Into<String>) -> Result<(), AuthDenial> {
        self.revocations
            .lock()
            .map_err(|_| AuthDenial::Unauthenticated)?
            .token_ids
            .insert(token_id.into());
        Ok(())
    }

    pub fn revoke_session(&self, session_id: impl Into<String>) -> Result<(), AuthDenial> {
        self.revocations
            .lock()
            .map_err(|_| AuthDenial::Unauthenticated)?
            .session_ids
            .insert(session_id.into());
        Ok(())
    }
}

impl Authenticator<OidcObservation<'_>> for OidcAuthenticator {
    fn authenticate(&self, observation: &OidcObservation<'_>) -> AuthDecision {
        let encoded_token = observation
            .authorization
            .strip_prefix("Bearer ")
            .filter(|token| {
                !token.is_empty() && !token.bytes().any(|byte| byte.is_ascii_whitespace())
            })
            .ok_or(AuthDenial::Unauthenticated)?;
        let header = decode_header(encoded_token).map_err(|_| AuthDenial::Unauthenticated)?;
        let algorithm = OidcAlgorithm::from_jwt(header.alg).ok_or(AuthDenial::Unauthenticated)?;
        if !self.allowed_algorithms.contains(&algorithm) {
            return Err(AuthDenial::Unauthenticated);
        }
        let key = self
            .keys_by_id
            .get(header.kid.as_deref().ok_or(AuthDenial::Unauthenticated)?)
            .filter(|key| key.algorithm == algorithm)
            .ok_or(AuthDenial::Unauthenticated)?;

        let mut validation = Validation::new(algorithm.jwt());
        validation.leeway = 0;
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.set_required_spec_claims(&["iss", "aud", "sub", "exp"]);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        let token = decode::<Claims>(encoded_token, &key.key, &validation)
            .map_err(|_| AuthDenial::Unauthenticated)?;
        let claims = token.claims;
        if claims.iss != self.issuer
            || !claims.aud.contains(&self.audience)
            || observation.now >= claims.exp
            || claims
                .nbf
                .is_some_and(|not_before| observation.now < not_before)
            || self
                .revocations
                .lock()
                .map_err(|_| AuthDenial::Unauthenticated)?
                .contains(&claims)
        {
            return Err(AuthDenial::Unauthenticated);
        }
        self.grants_by_subject
            .get(&claims.sub)
            .cloned()
            .map(AuthenticatedPrincipal::from_grants)
            .ok_or(AuthDenial::Unauthenticated)
    }
}

fn key_type_matches(algorithm: OidcAlgorithm, parameters: &AlgorithmParameters) -> bool {
    match (algorithm, parameters) {
        (OidcAlgorithm::Rs256, AlgorithmParameters::RSA(_)) => true,
        (OidcAlgorithm::Es256, AlgorithmParameters::EllipticCurve(parameters)) => {
            parameters.curve == EllipticCurve::P256
        }
        (OidcAlgorithm::EdDsa, AlgorithmParameters::OctetKeyPair(parameters)) => {
            parameters.curve == EllipticCurve::Ed25519
        }
        _ => false,
    }
}

#[derive(Clone, Deserialize)]
struct Claims {
    iss: String,
    aud: Audience,
    sub: String,
    exp: u64,
    nbf: Option<u64>,
    jti: Option<String>,
    sid: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(BTreeSet<String>),
}

impl Audience {
    fn contains(&self, expected: &str) -> bool {
        match self {
            Self::One(audience) => audience == expected,
            Self::Many(audiences) => audiences.contains(expected),
        }
    }
}
