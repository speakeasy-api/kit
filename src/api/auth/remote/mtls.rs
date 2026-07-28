use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
    time::Duration,
};

use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, UnixTime},
    server::{WebPkiClientVerifier, danger::ClientCertVerifier},
};
use x509_parser::{extensions::GeneralName, parse_x509_certificate};

use crate::api::auth::contract::{
    AuthDecision, AuthDenial, AuthenticatedPrincipal, Authenticator, GrantSnapshot,
};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum SubjectAltName {
    Dns(String),
    Uri(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevocationPolicy {
    LeafOnly,
    PresentedChain,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MtlsSetupError {
    InvalidTrustAnchor,
    NoTrustAnchors,
}

impl fmt::Display for MtlsSetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidTrustAnchor => "invalid mTLS trust anchor",
            Self::NoTrustAnchors => "mTLS requires at least one trust anchor",
        })
    }
}

impl std::error::Error for MtlsSetupError {}

#[derive(Clone, Copy)]
pub struct MtlsObservation<'a> {
    presented_chain_der: &'a [Vec<u8>],
    now: u64,
}

impl<'a> MtlsObservation<'a> {
    pub fn from_tls_transport(presented_chain_der: &'a [Vec<u8>], now: u64) -> Self {
        Self {
            presented_chain_der,
            now,
        }
    }
}

pub struct MtlsAuthenticator {
    verifier: Arc<dyn ClientCertVerifier>,
    revoked_serials: BTreeSet<Vec<u8>>,
    revocation_policy: RevocationPolicy,
    grants_by_san: BTreeMap<SubjectAltName, GrantSnapshot>,
}

impl MtlsAuthenticator {
    pub fn new(
        trusted_roots_der: impl IntoIterator<Item = Vec<u8>>,
        revoked_serials: impl IntoIterator<Item = Vec<u8>>,
        revocation_policy: RevocationPolicy,
        grants_by_san: BTreeMap<SubjectAltName, GrantSnapshot>,
    ) -> Result<Self, MtlsSetupError> {
        let mut roots = RootCertStore::empty();
        for root in trusted_roots_der {
            roots
                .add(CertificateDer::from(root))
                .map_err(|_| MtlsSetupError::InvalidTrustAnchor)?;
        }
        if roots.is_empty() {
            return Err(MtlsSetupError::NoTrustAnchors);
        }
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|_| MtlsSetupError::NoTrustAnchors)?;
        Ok(Self {
            verifier,
            revoked_serials: revoked_serials.into_iter().collect(),
            revocation_policy,
            grants_by_san,
        })
    }
}

impl Authenticator<MtlsObservation<'_>> for MtlsAuthenticator {
    fn authenticate(&self, observation: &MtlsObservation<'_>) -> AuthDecision {
        let (leaf_der, intermediate_der) = observation
            .presented_chain_der
            .split_first()
            .ok_or(AuthDenial::Unauthenticated)?;
        if leaf_der.is_empty() || intermediate_der.iter().any(Vec::is_empty) {
            return Err(AuthDenial::Unauthenticated);
        }

        let leaf = CertificateDer::from(leaf_der.as_slice());
        let intermediates = intermediate_der
            .iter()
            .map(|der| CertificateDer::from(der.as_slice()))
            .collect::<Vec<_>>();
        self.verifier
            .verify_client_cert(
                &leaf,
                &intermediates,
                UnixTime::since_unix_epoch(Duration::from_secs(observation.now)),
            )
            .map_err(|_| AuthDenial::Unauthenticated)?;

        let checked_chain = match self.revocation_policy {
            RevocationPolicy::LeafOnly => &observation.presented_chain_der[..1],
            RevocationPolicy::PresentedChain => observation.presented_chain_der,
        };
        for der in checked_chain {
            let (remainder, certificate) =
                parse_x509_certificate(der).map_err(|_| AuthDenial::Unauthenticated)?;
            if !remainder.is_empty() || self.revoked_serials.contains(certificate.raw_serial()) {
                return Err(AuthDenial::Unauthenticated);
            }
        }

        let (remainder, certificate) =
            parse_x509_certificate(leaf_der).map_err(|_| AuthDenial::Unauthenticated)?;
        if !remainder.is_empty()
            || !certificate
                .key_usage()
                .map_err(|_| AuthDenial::Unauthenticated)?
                .is_some_and(|usage| usage.value.digital_signature())
            || !certificate
                .extended_key_usage()
                .map_err(|_| AuthDenial::Unauthenticated)?
                .is_some_and(|usage| usage.value.client_auth)
        {
            return Err(AuthDenial::Unauthenticated);
        }

        let names = certificate
            .subject_alternative_name()
            .map_err(|_| AuthDenial::Unauthenticated)?
            .ok_or(AuthDenial::Unauthenticated)?;
        let mut mapped = names.value.general_names.iter().filter_map(|name| {
            let configured = match name {
                GeneralName::DNSName(value) => SubjectAltName::Dns((*value).to_owned()),
                GeneralName::URI(value) => SubjectAltName::Uri((*value).to_owned()),
                _ => return None,
            };
            self.grants_by_san.get(&configured)
        });
        let grants = mapped.next().ok_or(AuthDenial::Unauthenticated)?;
        if mapped.any(|candidate| candidate != grants) {
            return Err(AuthDenial::Unauthenticated);
        }
        Ok(AuthenticatedPrincipal::from_grants(grants.clone()))
    }
}
