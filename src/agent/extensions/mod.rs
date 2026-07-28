mod config;
mod descriptor;
mod registry;

use std::fmt;

pub use config::{
    ConfigSource, EXTENSION_CONFIG_SCHEMA_VERSION, EffectiveExtensionConfig, ExtensionConfigLayer,
    ExtensionConfigStack,
};
pub use descriptor::{
    CompatibilityRange, ContentDigest, ContractVersion, EXTENSION_DESCRIPTOR_SCHEMA_VERSION,
    ExtensionDescriptor, ExtensionIdentity, ExtensionPoint, ExtensionReference, ExtensionVersion,
    HOST_CONTRACT_VERSION,
};
pub(crate) use registry::TrustedExtensionToken;
pub use registry::{
    BUILT_IN_COST_TABLE, BUILT_IN_MODEL_ADAPTER, BUILT_IN_PROMPT_MODULES, ExtensionRegistry,
    OUT_OF_PROCESS_PROTOCOLS, OutOfProcessProtocol, built_in_descriptors, validate_contracts,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionError {
    InvalidIdentity(String),
    InvalidExtensionVersion(String),
    InvalidContractVersion(String),
    InvalidDigest(String),
    InvalidCompatibilityRange,
    UnsupportedDescriptorSchemaVersion {
        found: u16,
    },
    UnsupportedConfigSchemaVersion {
        source: ConfigSource,
        found: u16,
    },
    IncompatibleContract {
        extension: ExtensionReference,
        supported: CompatibilityRange,
        host: ContractVersion,
    },
    DuplicateIdentityVersion(ExtensionReference),
    SchemaDrift {
        extension: ExtensionReference,
        expected: ContentDigest,
        observed: ContentDigest,
    },
    ImplementationDrift {
        extension: ExtensionReference,
        expected: ContentDigest,
        observed: ContentDigest,
    },
    ExtensionPointConflict {
        extension: ExtensionReference,
        expected: ExtensionPoint,
        observed: ExtensionPoint,
    },
    MissingBuiltInSelection(ExtensionPoint),
    UnknownSelection(ExtensionReference),
    OutOfProcessRequired {
        extension: ExtensionReference,
        protocols: Vec<OutOfProcessProtocol>,
    },
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity(value) => {
                write!(formatter, "invalid extension identity {value:?}")
            }
            Self::InvalidExtensionVersion(value) => {
                write!(formatter, "invalid extension version {value:?}")
            }
            Self::InvalidContractVersion(value) => {
                write!(formatter, "invalid contract version {value:?}")
            }
            Self::InvalidDigest(value) => write!(formatter, "invalid content digest {value:?}"),
            Self::InvalidCompatibilityRange => {
                formatter.write_str("extension compatibility range is empty")
            }
            Self::UnsupportedDescriptorSchemaVersion { found } => write!(
                formatter,
                "unsupported extension descriptor schema version {found}"
            ),
            Self::UnsupportedConfigSchemaVersion { source, found } => write!(
                formatter,
                "unsupported {source:?} extension configuration schema version {found}"
            ),
            Self::IncompatibleContract {
                extension,
                supported,
                host,
            } => write!(
                formatter,
                "extension {extension} supports host contracts {supported}, not {host}"
            ),
            Self::DuplicateIdentityVersion(extension) => {
                write!(formatter, "duplicate extension {extension}")
            }
            Self::SchemaDrift {
                extension,
                expected,
                observed,
            } => write!(
                formatter,
                "extension {extension} schema drifted from {expected} to {observed}"
            ),
            Self::ImplementationDrift {
                extension,
                expected,
                observed,
            } => write!(
                formatter,
                "extension {extension} implementation drifted from {expected} to {observed}"
            ),
            Self::ExtensionPointConflict {
                extension,
                expected,
                observed,
            } => write!(
                formatter,
                "extension {extension} is registered for {expected:?}, not {observed:?}"
            ),
            Self::MissingBuiltInSelection(point) => {
                write!(formatter, "missing built-in selection for {point:?}")
            }
            Self::UnknownSelection(extension) => {
                write!(formatter, "unknown extension selection {extension}")
            }
            Self::OutOfProcessRequired {
                extension,
                protocols,
            } => write!(
                formatter,
                "untrusted extension {extension} must use an out-of-process protocol: {protocols:?}"
            ),
        }
    }
}

impl std::error::Error for ExtensionError {}
