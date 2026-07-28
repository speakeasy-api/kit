use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Serialize};

pub const VM_SCHEMA_VERSION: u16 = 1;
pub const VM_CONTRACT_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct VmFence(u64);

impl VmFence {
    pub fn new(value: u64) -> Result<Self, VmContractError> {
        (value != 0)
            .then_some(Self(value))
            .ok_or(VmContractError::InvalidFence)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VmStorageMode {
    CopyOnWrite,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VmNetworkPolicy {
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SecretHandle(String);

impl SecretHandle {
    pub fn new(value: impl Into<String>) -> Result<Self, VmContractError> {
        let value = value.into();
        if !valid_identifier(&value) {
            return Err(VmContractError::InvalidIdentifier("secret handle"));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VmResourceProfile {
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub pids: u32,
    pub wall_time_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VmRunSpec {
    pub schema_version: u16,
    pub contract_version: u16,
    pub run_id: String,
    pub fence: VmFence,
    pub image_digest: String,
    pub instance_id: String,
    pub rootfs_layer_id: String,
    pub storage_mode: VmStorageMode,
    pub network: VmNetworkPolicy,
    #[serde(default)]
    pub default_grants: BTreeSet<String>,
    #[serde(default)]
    pub secret_handles: BTreeSet<SecretHandle>,
    pub resources: VmResourceProfile,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VmContractDigest([u8; 32]);

impl VmContractDigest {
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

impl fmt::Display for VmContractDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "blake3:{}", self.hex())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VmRunContract {
    spec: VmRunSpec,
    canonical_bytes: Vec<u8>,
    digest: VmContractDigest,
}

impl VmRunContract {
    pub fn new(spec: VmRunSpec) -> Result<Self, VmContractError> {
        validate(&spec)?;
        let canonical_bytes = serde_json::to_vec(&spec)
            .map_err(|error| VmContractError::Serialization(error.to_string()))?;
        let digest = VmContractDigest(*blake3::hash(&canonical_bytes).as_bytes());
        Ok(Self {
            spec,
            canonical_bytes,
            digest,
        })
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, VmContractError> {
        let spec = serde_json::from_slice(bytes)
            .map_err(|error| VmContractError::Serialization(error.to_string()))?;
        let contract = Self::new(spec)?;
        if contract.canonical_bytes != bytes {
            return Err(VmContractError::NonCanonicalEncoding);
        }
        Ok(contract)
    }

    pub const fn spec(&self) -> &VmRunSpec {
        &self.spec
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    pub const fn digest(&self) -> VmContractDigest {
        self.digest
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum VmOutcome {
    Exit(i32),
    Signal(u32),
    Killed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum VmCompletion {
    Exit(i32),
    Signal(u32),
}

impl From<VmCompletion> for VmOutcome {
    fn from(completion: VmCompletion) -> Self {
        match completion {
            VmCompletion::Exit(code) => Self::Exit(code),
            VmCompletion::Signal(signal) => Self::Signal(signal),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VmOutcomeAttestation {
    pub schema_version: u16,
    pub contract_version: u16,
    pub run_id: String,
    pub instance_id: String,
    pub contract_digest: String,
    pub outcome: VmOutcome,
    pub evidence_digest: String,
}

impl VmOutcomeAttestation {
    pub fn new(
        contract: &VmRunContract,
        outcome: VmOutcome,
        evidence_digest: impl Into<String>,
    ) -> Result<Self, VmContractError> {
        let evidence_digest = evidence_digest.into();
        if !valid_sha256(&evidence_digest) {
            return Err(VmContractError::InvalidDigest("outcome evidence"));
        }
        Ok(Self {
            schema_version: VM_SCHEMA_VERSION,
            contract_version: VM_CONTRACT_VERSION,
            run_id: contract.spec.run_id.clone(),
            instance_id: contract.spec.instance_id.clone(),
            contract_digest: contract.digest().to_string(),
            outcome,
            evidence_digest,
        })
    }

    pub fn validates(&self, contract: &VmRunContract) -> bool {
        self.schema_version == VM_SCHEMA_VERSION
            && self.contract_version == VM_CONTRACT_VERSION
            && self.run_id == contract.spec.run_id
            && self.instance_id == contract.spec.instance_id
            && self.contract_digest == contract.digest().to_string()
            && valid_sha256(&self.evidence_digest)
    }

    pub fn validates_outcome(&self, contract: &VmRunContract, outcome: VmOutcome) -> bool {
        self.validates(contract) && self.outcome == outcome
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VmContractError {
    UnsupportedSchemaVersion(u16),
    UnsupportedContractVersion(u16),
    InvalidIdentifier(&'static str),
    InvalidFence,
    InvalidDigest(&'static str),
    DefaultGrantForbidden,
    UnboundedResource(&'static str),
    NonCanonicalEncoding,
    Serialization(String),
}

impl fmt::Display for VmContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion(version) => {
                write!(formatter, "unsupported VM schema version {version}")
            }
            Self::UnsupportedContractVersion(version) => {
                write!(formatter, "unsupported VM contract version {version}")
            }
            Self::InvalidIdentifier(name) => write!(formatter, "invalid VM {name}"),
            Self::InvalidFence => formatter.write_str("VM fence must be non-zero"),
            Self::InvalidDigest(name) => write!(formatter, "invalid {name} digest"),
            Self::DefaultGrantForbidden => {
                formatter.write_str("isolated VM default grants must be empty")
            }
            Self::UnboundedResource(name) => {
                write!(formatter, "VM resource {name} must be finite and non-zero")
            }
            Self::NonCanonicalEncoding => formatter.write_str("VM encoding is not canonical"),
            Self::Serialization(error) => write!(formatter, "VM serialization failed: {error}"),
        }
    }
}

impl std::error::Error for VmContractError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VmTransitionError {
    StaleFence,
    NotRunning,
    NotTerminated,
    NotQuiescent,
    InstanceReused,
    RootfsReused,
    InvalidAttestation,
    OutcomeMismatch,
    OutcomeAlreadyAttested,
}

pub trait IsolatedVm {
    fn start(&mut self, contract: VmRunContract) -> Result<(), VmTransitionError>;
    fn complete(
        &mut self,
        fence: VmFence,
        completion: VmCompletion,
    ) -> Result<(), VmTransitionError>;
    fn kill(&mut self, fence: VmFence) -> Result<(), VmTransitionError>;
    fn attest_quiescence(&mut self, fence: VmFence) -> Result<(), VmTransitionError>;
    fn attest_outcome(
        &mut self,
        fence: VmFence,
        attestation: VmOutcomeAttestation,
    ) -> Result<(), VmTransitionError>;
}

fn validate(spec: &VmRunSpec) -> Result<(), VmContractError> {
    if spec.schema_version != VM_SCHEMA_VERSION {
        return Err(VmContractError::UnsupportedSchemaVersion(
            spec.schema_version,
        ));
    }
    if spec.contract_version != VM_CONTRACT_VERSION {
        return Err(VmContractError::UnsupportedContractVersion(
            spec.contract_version,
        ));
    }
    for (name, value) in [
        ("run id", spec.run_id.as_str()),
        ("instance id", spec.instance_id.as_str()),
        ("rootfs layer id", spec.rootfs_layer_id.as_str()),
    ] {
        if !valid_identifier(value) {
            return Err(VmContractError::InvalidIdentifier(name));
        }
    }
    if spec.fence.get() == 0 {
        return Err(VmContractError::InvalidFence);
    }
    if !valid_sha256(&spec.image_digest) {
        return Err(VmContractError::InvalidDigest("image"));
    }
    if !spec.default_grants.is_empty() {
        return Err(VmContractError::DefaultGrantForbidden);
    }
    if spec
        .secret_handles
        .iter()
        .any(|handle| !valid_identifier(handle.as_str()))
    {
        return Err(VmContractError::InvalidIdentifier("secret handle"));
    }
    for (name, value) in [
        ("cpu", spec.resources.cpu_millis),
        ("memory", spec.resources.memory_bytes),
        ("disk", spec.resources.disk_bytes),
        ("pids", u64::from(spec.resources.pids)),
        ("wall time", spec.resources.wall_time_millis),
    ] {
        if value == 0 {
            return Err(VmContractError::UnboundedResource(name));
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

fn valid_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}
