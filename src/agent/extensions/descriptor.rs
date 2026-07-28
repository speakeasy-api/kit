use std::{collections::BTreeMap, fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::ExtensionError;

pub const EXTENSION_DESCRIPTOR_SCHEMA_VERSION: u16 = 1;
pub const HOST_CONTRACT_VERSION: ContractVersion = ContractVersion::new(1, 0, 0);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionPoint {
    ModelAdapter,
    PromptModule,
    CostTable,
}

impl ExtensionPoint {
    pub const ALL: [Self; 3] = [Self::ModelAdapter, Self::PromptModule, Self::CostTable];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ModelAdapter => "model_adapter",
            Self::PromptModule => "prompt_module",
            Self::CostTable => "cost_table",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ContractVersion {
    major: u16,
    minor: u16,
    patch: u16,
}

impl ContractVersion {
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    pub const fn major(self) -> u16 {
        self.major
    }

    pub const fn minor(self) -> u16 {
        self.minor
    }

    pub const fn patch(self) -> u16 {
        self.patch
    }
}

impl fmt::Display for ContractVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for ContractVersion {
    type Err = ExtensionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = || ExtensionError::InvalidContractVersion(value.to_owned());
        let mut parts = value.split('.');
        let major = parts
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        let minor = parts
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        let patch = parts
            .next()
            .ok_or_else(invalid)?
            .parse()
            .map_err(|_| invalid())?;
        if parts.next().is_some() {
            return Err(invalid());
        }
        Ok(Self::new(major, minor, patch))
    }
}

impl Serialize for ContractVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ContractVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExtensionVersion(String);

impl ExtensionVersion {
    pub fn parse(value: impl Into<String>) -> Result<Self, ExtensionError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
        {
            return Err(ExtensionError::InvalidExtensionVersion(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExtensionVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ExtensionVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ExtensionVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExtensionIdentity(String);

impl ExtensionIdentity {
    pub fn parse(value: impl Into<String>) -> Result<Self, ExtensionError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 128
            && !value.contains("..")
            && value.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || (index > 0 && matches!(byte, b'.' | b'-' | b'_'))
            });
        if !valid {
            return Err(ExtensionError::InvalidIdentity(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ExtensionIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ExtensionIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ExtensionIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ContentDigest(String);

impl ContentDigest {
    pub fn parse(value: impl Into<String>) -> Result<Self, ExtensionError> {
        let value = value.into();
        let Some(hex) = value.strip_prefix("sha256:") else {
            return Err(ExtensionError::InvalidDigest(value));
        };
        if hex.len() != 64
            || !hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ExtensionError::InvalidDigest(value));
        }
        Ok(Self(value))
    }

    pub fn sha256(bytes: &[u8]) -> Self {
        Self(format!("sha256:{:x}", Sha256::digest(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ContentDigest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ContentDigest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompatibilityRange {
    pub minimum: ContractVersion,
    pub maximum_exclusive: ContractVersion,
}

impl CompatibilityRange {
    pub const fn new(minimum: ContractVersion, maximum_exclusive: ContractVersion) -> Self {
        Self {
            minimum,
            maximum_exclusive,
        }
    }

    pub const fn contains(self, version: ContractVersion) -> bool {
        !version_less(version, self.minimum) && version_less(version, self.maximum_exclusive)
    }

    pub(crate) fn validate(self) -> Result<(), ExtensionError> {
        if self.minimum >= self.maximum_exclusive {
            Err(ExtensionError::InvalidCompatibilityRange)
        } else {
            Ok(())
        }
    }
}

const fn version_less(left: ContractVersion, right: ContractVersion) -> bool {
    left.major < right.major
        || (left.major == right.major
            && (left.minor < right.minor
                || (left.minor == right.minor && left.patch < right.patch)))
}

impl fmt::Display for CompatibilityRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "[{}, {})", self.minimum, self.maximum_exclusive)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ExtensionReference {
    pub identity: ExtensionIdentity,
    pub version: ExtensionVersion,
}

impl ExtensionReference {
    pub fn new(identity: ExtensionIdentity, version: ExtensionVersion) -> Self {
        Self { identity, version }
    }
}

impl fmt::Display for ExtensionReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.identity, self.version)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExtensionDescriptor {
    schema_version: u16,
    extension_point: ExtensionPoint,
    identity: ExtensionIdentity,
    version: ExtensionVersion,
    compatibility: CompatibilityRange,
    schema_digest: ContentDigest,
    implementation_digest: ContentDigest,
    #[serde(default, flatten)]
    additional_fields: BTreeMap<String, Value>,
}

impl ExtensionDescriptor {
    pub fn new(
        extension_point: ExtensionPoint,
        identity: ExtensionIdentity,
        version: ExtensionVersion,
        compatibility: CompatibilityRange,
        schema_digest: ContentDigest,
        implementation_digest: ContentDigest,
    ) -> Result<Self, ExtensionError> {
        let descriptor = Self {
            schema_version: EXTENSION_DESCRIPTOR_SCHEMA_VERSION,
            extension_point,
            identity,
            version,
            compatibility,
            schema_digest,
            implementation_digest,
            additional_fields: BTreeMap::new(),
        };
        descriptor.validate()?;
        Ok(descriptor)
    }

    pub fn with_additional_field(mut self, name: impl Into<String>, value: Value) -> Self {
        self.additional_fields.insert(name.into(), value);
        self
    }

    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    pub const fn extension_point(&self) -> ExtensionPoint {
        self.extension_point
    }

    pub fn identity(&self) -> &ExtensionIdentity {
        &self.identity
    }

    pub fn version(&self) -> &ExtensionVersion {
        &self.version
    }

    pub const fn compatibility(&self) -> CompatibilityRange {
        self.compatibility
    }

    pub fn schema_digest(&self) -> &ContentDigest {
        &self.schema_digest
    }

    pub fn implementation_digest(&self) -> &ContentDigest {
        &self.implementation_digest
    }

    pub fn additional_fields(&self) -> &BTreeMap<String, Value> {
        &self.additional_fields
    }

    pub fn reference(&self) -> ExtensionReference {
        ExtensionReference::new(self.identity.clone(), self.version.clone())
    }

    pub fn validate(&self) -> Result<(), ExtensionError> {
        if self.schema_version != EXTENSION_DESCRIPTOR_SCHEMA_VERSION {
            return Err(ExtensionError::UnsupportedDescriptorSchemaVersion {
                found: self.schema_version,
            });
        }
        self.compatibility.validate()?;
        if !self.compatibility.contains(HOST_CONTRACT_VERSION) {
            return Err(ExtensionError::IncompatibleContract {
                extension: self.reference(),
                supported: self.compatibility,
                host: HOST_CONTRACT_VERSION,
            });
        }
        Ok(())
    }
}
