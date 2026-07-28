use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};

pub const OVERLAY_SCHEMA_VERSION: u16 = 1;
pub const OVERLAY_CONTRACT_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct MutationFence(u64);

impl MutationFence {
    pub fn new(value: u64) -> Result<Self, OverlayContractError> {
        (value != 0)
            .then_some(Self(value))
            .ok_or(OverlayContractError::InvalidFence)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceAccess {
    ReadOnly,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WritableLayerMode {
    CopyOnWrite,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Add,
    Modify,
    Delete,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredChange {
    pub path: String,
    pub kind: ChangeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_digest: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OverlaySpec {
    pub schema_version: u16,
    pub contract_version: u16,
    pub overlay_id: String,
    pub base_revision: u64,
    pub base_digest: String,
    pub source_access: SourceAccess,
    pub writable_layer_id: String,
    pub writable_layer_mode: WritableLayerMode,
    pub mutation_lock_id: String,
    pub fence: MutationFence,
    pub declared_diff: BTreeSet<DeclaredChange>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OverlayDigest([u8; 32]);

impl OverlayDigest {
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }

    pub fn hex(self) -> String {
        let mut value = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            write!(value, "{byte:02x}").expect("writing to a string cannot fail");
        }
        value
    }
}

impl fmt::Display for OverlayDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "blake3:{}", self.hex())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OverlayContract {
    spec: OverlaySpec,
    canonical_bytes: Vec<u8>,
    digest: OverlayDigest,
}

impl OverlayContract {
    pub fn new(spec: OverlaySpec) -> Result<Self, OverlayContractError> {
        validate(&spec)?;
        let canonical_bytes = serde_json::to_vec(&spec)
            .map_err(|error| OverlayContractError::Serialization(error.to_string()))?;
        let digest = OverlayDigest(*blake3::hash(&canonical_bytes).as_bytes());
        Ok(Self {
            spec,
            canonical_bytes,
            digest,
        })
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, OverlayContractError> {
        let spec = serde_json::from_slice(bytes)
            .map_err(|error| OverlayContractError::Serialization(error.to_string()))?;
        let contract = Self::new(spec)?;
        if contract.canonical_bytes != bytes {
            return Err(OverlayContractError::NonCanonicalEncoding);
        }
        Ok(contract)
    }

    pub const fn spec(&self) -> &OverlaySpec {
        &self.spec
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    pub const fn digest(&self) -> OverlayDigest {
        self.digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OverlayContractError {
    UnsupportedSchemaVersion(u16),
    UnsupportedContractVersion(u16),
    InvalidIdentifier(&'static str),
    InvalidFence,
    InvalidDigest(&'static str),
    InvalidPath(String),
    InvalidChangeDigests(String),
    NonCanonicalEncoding,
    Serialization(String),
}

impl fmt::Display for OverlayContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion(version) => {
                write!(formatter, "unsupported overlay schema version {version}")
            }
            Self::UnsupportedContractVersion(version) => {
                write!(formatter, "unsupported overlay contract version {version}")
            }
            Self::InvalidIdentifier(name) => write!(formatter, "invalid overlay {name}"),
            Self::InvalidFence => formatter.write_str("mutation fence must be non-zero"),
            Self::InvalidDigest(name) => write!(formatter, "invalid {name} digest"),
            Self::InvalidPath(path) => write!(formatter, "invalid declared path {path}"),
            Self::InvalidChangeDigests(path) => {
                write!(
                    formatter,
                    "declared digests do not match change kind for {path}"
                )
            }
            Self::NonCanonicalEncoding => formatter.write_str("overlay encoding is not canonical"),
            Self::Serialization(error) => {
                write!(formatter, "overlay serialization failed: {error}")
            }
        }
    }
}

impl std::error::Error for OverlayContractError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OverlayDisposition {
    Promoted,
    Discarded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OverlayTransitionError {
    StaleFence,
    NotActive,
    NotQuiescent,
    WritableLayerReused,
    AlreadyFinalized(OverlayDisposition),
    NotFinalized,
}

pub trait MutationOverlay {
    fn start(&mut self, contract: OverlayContract) -> Result<(), OverlayTransitionError>;
    fn promote(&mut self, fence: MutationFence) -> Result<(), OverlayTransitionError>;
    fn discard(&mut self, fence: MutationFence) -> Result<(), OverlayTransitionError>;
    fn attest_quiescence(&mut self, fence: MutationFence) -> Result<(), OverlayTransitionError>;
}

fn validate(spec: &OverlaySpec) -> Result<(), OverlayContractError> {
    if spec.schema_version != OVERLAY_SCHEMA_VERSION {
        return Err(OverlayContractError::UnsupportedSchemaVersion(
            spec.schema_version,
        ));
    }
    if spec.contract_version != OVERLAY_CONTRACT_VERSION {
        return Err(OverlayContractError::UnsupportedContractVersion(
            spec.contract_version,
        ));
    }
    for (name, value) in [
        ("id", spec.overlay_id.as_str()),
        ("writable layer id", spec.writable_layer_id.as_str()),
        ("mutation lock id", spec.mutation_lock_id.as_str()),
    ] {
        if !valid_identifier(value) {
            return Err(OverlayContractError::InvalidIdentifier(name));
        }
    }
    if spec.fence.get() == 0 {
        return Err(OverlayContractError::InvalidFence);
    }
    if !valid_digest(&spec.base_digest) {
        return Err(OverlayContractError::InvalidDigest("base"));
    }
    let mut paths = BTreeSet::new();
    for change in &spec.declared_diff {
        if !valid_path(&change.path) {
            return Err(OverlayContractError::InvalidPath(change.path.clone()));
        }
        if !paths.insert(&change.path) {
            return Err(OverlayContractError::InvalidPath(change.path.clone()));
        }
        let digests_match = match change.kind {
            ChangeKind::Add => change.base_digest.is_none() && change.result_digest.is_some(),
            ChangeKind::Modify => change.base_digest.is_some() && change.result_digest.is_some(),
            ChangeKind::Delete => change.base_digest.is_some() && change.result_digest.is_none(),
        };
        if !digests_match
            || change
                .base_digest
                .iter()
                .chain(change.result_digest.iter())
                .any(|digest| !valid_digest(digest))
        {
            return Err(OverlayContractError::InvalidChangeDigests(
                change.path.clone(),
            ));
        }
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'/' | b'-')
        })
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("blake3:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.is_ascii()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.contains(':')
        && !path.bytes().any(|byte| byte.is_ascii_control())
        && path.split('/').all(|component| {
            !component.is_empty()
                && !matches!(component, "." | "..")
                && !component.ends_with(['.', ' '])
                && !windows_reserved(component)
        })
}

fn windows_reserved(component: &str) -> bool {
    let stem = component.split('.').next().unwrap_or(component);
    let upper = stem.to_ascii_uppercase();
    matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$")
        || upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
}
