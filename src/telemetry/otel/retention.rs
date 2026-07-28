use std::fmt;

use super::ExportBatch;

#[derive(Clone, Eq, PartialEq)]
pub struct EncryptionKeyHandle(String);

impl EncryptionKeyHandle {
    pub fn new(identifier: impl Into<String>) -> Result<Self, RetentionError> {
        let identifier = identifier.into();
        if identifier.trim().is_empty() {
            return Err(RetentionError::InvalidKeyHandle);
        }
        Ok(Self(identifier))
    }

    pub fn identifier(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for EncryptionKeyHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("EncryptionKeyHandle([REDACTED])")
    }
}

impl fmt::Display for EncryptionKeyHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

pub trait AeadProvider: Sync {
    fn seal(
        &self,
        key: &EncryptionKeyHandle,
        plaintext: &[u8],
        associated_data: &[u8],
    ) -> Result<Vec<u8>, String>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    pub retain_until_unix_nanos: u64,
    pub sensitive: bool,
    pub encryption_required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetainedTelemetry {
    pub schema_version: u16,
    pub retain_until_unix_nanos: u64,
    pub encrypted: bool,
    pub bytes: Vec<u8>,
}

pub trait RetentionSink {
    fn persist(&mut self, record: RetainedTelemetry) -> Result<(), String>;
}

pub struct RetentionHook<'a> {
    policy: RetentionPolicy,
    aead: Option<&'a dyn AeadProvider>,
    key: Option<&'a EncryptionKeyHandle>,
}

impl<'a> RetentionHook<'a> {
    pub const fn new(
        policy: RetentionPolicy,
        aead: Option<&'a dyn AeadProvider>,
        key: Option<&'a EncryptionKeyHandle>,
    ) -> Self {
        Self { policy, aead, key }
    }

    pub fn retain(
        &self,
        batch: &ExportBatch,
        sink: &mut dyn RetentionSink,
    ) -> Result<(), RetentionError> {
        batch.validate().map_err(RetentionError::InvalidBatch)?;
        let mut plaintext = batch
            .to_canonical_json()
            .map_err(|error| RetentionError::Serialization(error.to_string()))?;
        let encryption_required = self.policy.sensitive || self.policy.encryption_required;
        let (bytes, encrypted) = if encryption_required {
            let (Some(aead), Some(key)) = (self.aead, self.key) else {
                plaintext.fill(0);
                return Err(RetentionError::EncryptionUnavailable);
            };
            let associated_data = format!(
                "kit.telemetry.v{};retain_until={}",
                batch.schema_version, self.policy.retain_until_unix_nanos
            );
            let result = aead
                .seal(key, &plaintext, associated_data.as_bytes())
                .map_err(RetentionError::Encryption);
            plaintext.fill(0);
            (result?, true)
        } else {
            (plaintext, false)
        };
        sink.persist(RetainedTelemetry {
            schema_version: batch.schema_version,
            retain_until_unix_nanos: self.policy.retain_until_unix_nanos,
            encrypted,
            bytes,
        })
        .map_err(RetentionError::Persistence)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RetentionError {
    InvalidKeyHandle,
    InvalidBatch(String),
    Serialization(String),
    EncryptionUnavailable,
    Encryption(String),
    Persistence(String),
}

impl fmt::Display for RetentionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKeyHandle => formatter.write_str("encryption key handle is empty"),
            Self::InvalidBatch(error) => write!(formatter, "invalid telemetry batch: {error}"),
            Self::Serialization(error) => {
                write!(formatter, "telemetry serialization failed: {error}")
            }
            Self::EncryptionUnavailable => {
                formatter.write_str("required telemetry encryption is unavailable")
            }
            Self::Encryption(error) => write!(formatter, "telemetry encryption failed: {error}"),
            Self::Persistence(error) => write!(formatter, "telemetry retention failed: {error}"),
        }
    }
}

impl std::error::Error for RetentionError {}
