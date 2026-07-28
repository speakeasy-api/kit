use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    net::IpAddr,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use zeroize::Zeroize;

use crate::domain::{
    crypto::{constant_time_eq, hmac_sha256_domain, sha256},
    secret::{SecretHandle, SecretLease},
};

use super::contract::{
    AuthDecision, AuthDenial, AuthenticatedPrincipal, Authenticator, GrantSnapshot,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopbackSetupError {
    InvalidLifetime,
    InvalidReplayPolicy,
    InvalidAllowlist,
    RandomnessUnavailable,
}

impl fmt::Display for LoopbackSetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLifetime => f.write_str("loopback session expiry must follow issuance"),
            Self::InvalidReplayPolicy => {
                f.write_str("loopback replay window and capacity must be positive")
            }
            Self::InvalidAllowlist => {
                f.write_str("loopback Host and Origin allowlists must contain visible ASCII values")
            }
            Self::RandomnessUnavailable => f.write_str("secure token generation failed"),
        }
    }
}

impl std::error::Error for LoopbackSetupError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoopbackReplayPolicy {
    pub window_seconds: u64,
    pub capacity: usize,
}

impl LoopbackReplayPolicy {
    pub const fn new(window_seconds: u64, capacity: usize) -> Self {
        Self {
            window_seconds,
            capacity,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoopbackRequestTime {
    pub signed: u64,
    pub observed: u64,
}

impl LoopbackRequestTime {
    pub const fn new(signed: u64, observed: u64) -> Self {
        Self { signed, observed }
    }
}

#[derive(Clone, Copy)]
pub struct LoopbackObservation<'a> {
    peer_ip: IpAddr,
    authorization: &'a str,
    host: &'a str,
    origin: &'a str,
    nonce: &'a [u8],
    signature: Option<[u8; 32]>,
    timestamp: u64,
    now: u64,
}

impl<'a> LoopbackObservation<'a> {
    pub fn from_transport(
        peer_ip: IpAddr,
        authorization: &'a str,
        host: &'a str,
        origin: &'a str,
        nonce: &'a [u8],
        signature: &[u8],
        time: LoopbackRequestTime,
    ) -> Self {
        Self {
            peer_ip,
            authorization,
            host,
            origin,
            nonce,
            signature: decode_hex(signature),
            timestamp: time.signed,
            now: time.observed,
        }
    }
}

pub struct LoopbackAuthenticator {
    token_handle: SecretHandle,
    verifier: [u8; 32],
    grants: GrantSnapshot,
    allowed_hosts: BTreeSet<String>,
    allowed_origins: BTreeSet<String>,
    issued_at: u64,
    expires_at: u64,
    revoked: AtomicBool,
    replay_window: u64,
    replay_capacity: usize,
    seen_nonces: Mutex<ReplayCache>,
}

#[derive(Default)]
struct ReplayCache {
    by_digest: BTreeMap<[u8; 32], u64>,
    by_expiry: BTreeSet<(u64, [u8; 32])>,
}

impl LoopbackAuthenticator {
    pub fn issue<H, O>(
        token_handle: SecretHandle,
        grants: GrantSnapshot,
        allowed_hosts: H,
        allowed_origins: O,
        issued_at: u64,
        expires_at: u64,
        replay: LoopbackReplayPolicy,
    ) -> Result<(Self, SecretLease), LoopbackSetupError>
    where
        H: IntoIterator,
        H::Item: Into<String>,
        O: IntoIterator,
        O::Item: Into<String>,
    {
        if expires_at <= issued_at || expires_at == u64::MAX {
            return Err(LoopbackSetupError::InvalidLifetime);
        }
        if replay.window_seconds == 0 || replay.capacity == 0 {
            return Err(LoopbackSetupError::InvalidReplayPolicy);
        }
        let allowed_hosts = allowed_hosts.into_iter().map(Into::into).collect();
        let allowed_origins = allowed_origins.into_iter().map(Into::into).collect();
        if !valid_allowlist(&allowed_hosts) || !valid_allowlist(&allowed_origins) {
            return Err(LoopbackSetupError::InvalidAllowlist);
        }

        let mut random = [0_u8; 32];
        getrandom::fill(&mut random).map_err(|_| LoopbackSetupError::RandomnessUnavailable)?;
        let token = encode_hex(&random);
        random.zeroize();
        let verifier = sha256(&token);

        Ok((
            Self {
                token_handle,
                verifier,
                grants,
                allowed_hosts,
                allowed_origins,
                issued_at,
                expires_at,
                replay_window: replay.window_seconds,
                replay_capacity: replay.capacity,
                revoked: AtomicBool::new(false),
                seen_nonces: Mutex::new(ReplayCache::default()),
            },
            SecretLease::new(token),
        ))
    }

    pub fn token_handle(&self) -> &SecretHandle {
        &self.token_handle
    }

    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }

    pub fn replay_cache_len(&self) -> usize {
        self.seen_nonces
            .lock()
            .map(|cache| cache.by_digest.len())
            .unwrap_or(self.replay_capacity)
    }
}

impl Authenticator<LoopbackObservation<'_>> for LoopbackAuthenticator {
    fn authenticate(&self, observation: &LoopbackObservation<'_>) -> AuthDecision {
        let candidate = observation
            .authorization
            .strip_prefix("Bearer ")
            .unwrap_or_default()
            .as_bytes();
        let candidate_digest = sha256(candidate);
        let token_valid = constant_time_eq(&candidate_digest, &self.verifier)
            && candidate.len() == 64
            && observation.authorization.len() == 71;
        let timestamp = observation.timestamp.to_be_bytes();
        let expected_signature = hmac_sha256_domain(
            &candidate_digest,
            b"KIT-LOOPBACK-REQUEST-V1\0",
            &[&timestamp, observation.nonce],
        );
        let signature_valid = observation
            .signature
            .is_some_and(|signature| constant_time_eq(&signature, &expected_signature));

        if !observation.peer_ip.is_loopback()
            || !self.allowed_hosts.contains(observation.host)
            || !self.allowed_origins.contains(observation.origin)
            || observation.now < self.issued_at
            || observation.now >= self.expires_at
            || self.revoked.load(Ordering::Acquire)
            || !token_valid
            || !signature_valid
            || observation.nonce.is_empty()
            || observation.nonce.len() > 256
            || observation.timestamp > observation.now.saturating_add(self.replay_window)
            || observation
                .timestamp
                .checked_add(self.replay_window)
                .is_none_or(|expires| expires <= observation.now)
        {
            return Err(AuthDenial::Unauthenticated);
        }

        let mut nonce_material = Vec::with_capacity(8 + observation.nonce.len());
        nonce_material.extend_from_slice(&observation.timestamp.to_be_bytes());
        nonce_material.extend_from_slice(observation.nonce);
        let nonce = sha256(&nonce_material);
        nonce_material.zeroize();
        let nonce_expiry = observation
            .timestamp
            .checked_add(self.replay_window)
            .unwrap_or(self.expires_at)
            .min(self.expires_at);
        let mut seen = self
            .seen_nonces
            .lock()
            .map_err(|_| AuthDenial::Unauthenticated)?;
        while let Some((expiry, digest)) = seen.by_expiry.iter().next().copied()
            && expiry <= observation.now
        {
            seen.by_expiry.remove(&(expiry, digest));
            seen.by_digest.remove(&digest);
        }
        if seen.by_digest.contains_key(&nonce) || seen.by_digest.len() >= self.replay_capacity {
            return Err(AuthDenial::Unauthenticated);
        }
        seen.by_digest.insert(nonce, nonce_expiry);
        seen.by_expiry.insert((nonce_expiry, nonce));
        Ok(AuthenticatedPrincipal::from_grants(self.grants.clone()))
    }
}

fn valid_allowlist(values: &BTreeSet<String>) -> bool {
    !values.is_empty()
        && values.iter().all(|value| {
            !value.is_empty()
                && value.len() <= 255
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        })
}

fn encode_hex(bytes: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = Vec::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[usize::from(byte >> 4)]);
        encoded.push(HEX[usize::from(byte & 0x0f)]);
    }
    encoded
}

fn decode_hex(bytes: &[u8]) -> Option<[u8; 32]> {
    if bytes.len() != 64 {
        return None;
    }
    let mut decoded = [0_u8; 32];
    for (output, pair) in decoded.iter_mut().zip(bytes.chunks_exact(2)) {
        *output = hex_nibble(pair[0])?.checked_mul(16)? | hex_nibble(pair[1])?;
    }
    Some(decoded)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
