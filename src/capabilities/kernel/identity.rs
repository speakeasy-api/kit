use std::{fmt, str::FromStr, sync::Arc};

use crate::{domain::crypto::sha256, store::artifacts::ArtifactId};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DigestAlgorithm {
    Sha256,
    Blake3,
}

impl DigestAlgorithm {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Blake3 => "blake3",
        }
    }

    pub(crate) const fn tag(self) -> u8 {
        match self {
            Self::Sha256 => 0,
            Self::Blake3 => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Digest {
    algorithm: DigestAlgorithm,
    bytes: [u8; 32],
}

impl Digest {
    pub fn of(algorithm: DigestAlgorithm, input: &[u8]) -> Self {
        let bytes = match algorithm {
            DigestAlgorithm::Sha256 => sha256(input),
            DigestAlgorithm::Blake3 => ArtifactId::digest(input).as_bytes(),
        };
        Self { algorithm, bytes }
    }

    pub const fn algorithm(self) -> DigestAlgorithm {
        self.algorithm
    }

    pub const fn as_bytes(self) -> [u8; 32] {
        self.bytes
    }

    pub fn hex(self) -> String {
        let mut value = String::with_capacity(64);
        for byte in self.bytes {
            use fmt::Write as _;
            write!(value, "{byte:02x}").expect("writing to a string cannot fail");
        }
        value
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algorithm.name(), self.hex())
    }
}

impl FromStr for Digest {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (algorithm, hex) = value.split_once(':').ok_or("invalid digest")?;
        if hex.len() != 64 {
            return Err("invalid digest");
        }
        let mut bytes = [0_u8; 32];
        for (output, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
            let nibble = |byte| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                _ => None,
            };
            *output = (nibble(pair[0]).ok_or("invalid digest")? << 4)
                | nibble(pair[1]).ok_or("invalid digest")?;
        }
        Ok(Self {
            algorithm: match algorithm {
                "sha256" => DigestAlgorithm::Sha256,
                "blake3" => DigestAlgorithm::Blake3,
                _ => return Err("invalid digest"),
            },
            bytes,
        })
    }
}

impl serde::Serialize for Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <String as serde::Deserialize>::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

macro_rules! identity_text {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(Arc<str>);

        impl $name {
            pub fn new(value: impl Into<Arc<str>>) -> Result<Self, IdentityError> {
                let value = value.into();
                if value.is_empty() {
                    return Err(IdentityError::Empty($label));
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

identity_text!(CapabilitySource, "source");
identity_text!(CapabilityNamespace, "namespace");
identity_text!(CapabilityName, "name");
identity_text!(CapabilityVersion, "version");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityError {
    Empty(&'static str),
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty(field) => write!(f, "capability {field} must not be empty"),
        }
    }
}

impl std::error::Error for IdentityError {}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CapabilityIdentity {
    source: CapabilitySource,
    namespace: CapabilityNamespace,
    name: CapabilityName,
    version: CapabilityVersion,
    implementation_digest: Digest,
}

impl CapabilityIdentity {
    pub const fn new(
        source: CapabilitySource,
        namespace: CapabilityNamespace,
        name: CapabilityName,
        version: CapabilityVersion,
        implementation_digest: Digest,
    ) -> Self {
        Self {
            source,
            namespace,
            name,
            version,
            implementation_digest,
        }
    }

    pub const fn source(&self) -> &CapabilitySource {
        &self.source
    }

    pub const fn namespace(&self) -> &CapabilityNamespace {
        &self.namespace
    }

    pub const fn name(&self) -> &CapabilityName {
        &self.name
    }

    pub const fn version(&self) -> &CapabilityVersion {
        &self.version
    }

    pub const fn implementation_digest(&self) -> Digest {
        self.implementation_digest
    }

    pub(crate) fn write_canonical(&self, output: &mut Vec<u8>) {
        put_bytes(output, self.source.as_str().as_bytes());
        put_bytes(output, self.namespace.as_str().as_bytes());
        put_bytes(output, self.name.as_str().as_bytes());
        put_bytes(output, self.version.as_str().as_bytes());
        put_digest(output, self.implementation_digest);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSchema {
    source: Arc<[u8]>,
    dialect: Arc<str>,
    documentation: Arc<[u8]>,
    normalized: Arc<[u8]>,
    source_digest: Digest,
    normalized_digest: Digest,
}

impl SourceSchema {
    pub fn new(
        source: impl Into<Arc<[u8]>>,
        dialect: impl Into<Arc<str>>,
        documentation: impl Into<Arc<[u8]>>,
        normalized: impl Into<Arc<[u8]>>,
        algorithm: DigestAlgorithm,
    ) -> Result<Self, SchemaError> {
        let source = source.into();
        let dialect = dialect.into();
        let documentation = documentation.into();
        let normalized = normalized.into();
        if source.is_empty() {
            return Err(SchemaError::EmptySource);
        }
        if dialect.is_empty() {
            return Err(SchemaError::EmptyDialect);
        }
        if normalized.is_empty() {
            return Err(SchemaError::EmptyNormalizedView);
        }
        Ok(Self {
            source_digest: Digest::of(algorithm, &source),
            normalized_digest: Digest::of(algorithm, &normalized),
            source,
            dialect,
            documentation,
            normalized,
        })
    }

    pub fn source_bytes(&self) -> &[u8] {
        &self.source
    }

    pub fn dialect(&self) -> &str {
        &self.dialect
    }

    pub fn documentation(&self) -> &[u8] {
        &self.documentation
    }

    pub fn normalized_bytes(&self) -> &[u8] {
        &self.normalized
    }

    pub const fn source_digest(&self) -> Digest {
        self.source_digest
    }

    pub const fn normalized_digest(&self) -> Digest {
        self.normalized_digest
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaError {
    EmptySource,
    EmptyDialect,
    EmptyNormalizedView,
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptySource => "source JSON Schema must not be empty",
            Self::EmptyDialect => "source JSON Schema dialect must not be empty",
            Self::EmptyNormalizedView => "normalized JSON Schema view must not be empty",
        })
    }
}

impl std::error::Error for SchemaError {}

pub(crate) fn put_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    output.extend_from_slice(bytes);
}

pub(crate) fn put_digest(output: &mut Vec<u8>, digest: Digest) {
    output.push(digest.algorithm().tag());
    output.extend_from_slice(&digest.as_bytes());
}
