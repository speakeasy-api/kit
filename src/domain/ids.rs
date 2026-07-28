use std::fmt;
use std::str::FromStr;

const ENCODED_LEN: usize = 26;
const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdParseError {
    InvalidPrefix { expected: &'static str },
    InvalidLength { expected: usize, actual: usize },
    InvalidCharacter { index: usize },
    Overflow,
}

impl fmt::Display for IdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPrefix { expected } => {
                write!(f, "identifier must start with {expected}_")
            }
            Self::InvalidLength { expected, actual } => {
                write!(
                    f,
                    "identifier payload must be {expected} bytes, got {actual}"
                )
            }
            Self::InvalidCharacter { index } => {
                write!(
                    f,
                    "identifier contains an invalid character at byte {index}"
                )
            }
            Self::Overflow => f.write_str("identifier payload exceeds 128 bits"),
        }
    }
}

impl std::error::Error for IdParseError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdGenerationError;

impl fmt::Display for IdGenerationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("secure identifier generation failed")
    }
}

impl std::error::Error for IdGenerationError {}

fn decode(prefix: &'static str, wire: &str) -> Result<u128, IdParseError> {
    let payload = wire
        .strip_prefix(prefix)
        .and_then(|value| value.strip_prefix('_'))
        .ok_or(IdParseError::InvalidPrefix { expected: prefix })?;

    if payload.len() != ENCODED_LEN {
        return Err(IdParseError::InvalidLength {
            expected: ENCODED_LEN,
            actual: payload.len(),
        });
    }

    let mut value = 0_u128;
    for (index, byte) in payload.bytes().enumerate() {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'h' => byte - b'a' + 10,
            b'j'..=b'k' => byte - b'j' + 18,
            b'm'..=b'n' => byte - b'm' + 20,
            b'p'..=b't' => byte - b'p' + 22,
            b'v'..=b'z' => byte - b'v' + 27,
            _ => return Err(IdParseError::InvalidCharacter { index }),
        };
        if index == 0 && digit > 7 {
            return Err(IdParseError::Overflow);
        }
        value = (value << 5) | u128::from(digit);
    }
    Ok(value)
}

fn encode(mut value: u128, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut encoded = [b'0'; ENCODED_LEN];
    for byte in encoded.iter_mut().rev() {
        *byte = ALPHABET[(value & 31) as usize];
        value >>= 5;
    }
    f.write_str(std::str::from_utf8(&encoded).expect("ID alphabet is ASCII"))
}

macro_rules! typed_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[repr(transparent)]
        pub struct $name(u128);

        impl $name {
            pub const PREFIX: &'static str = $prefix;

            pub fn generate() -> Result<Self, IdGenerationError> {
                let mut bytes = [0_u8; 16];
                getrandom::fill(&mut bytes).map_err(|_| IdGenerationError)?;
                Ok(Self(u128::from_be_bytes(bytes)))
            }

            pub fn parse(wire: &str) -> Result<Self, IdParseError> {
                decode(Self::PREFIX, wire).map(Self)
            }

            #[allow(dead_code)]
            pub(crate) fn from_stable_bytes(bytes: &[u8]) -> Self {
                let digest = blake3::hash(bytes);
                Self(u128::from_be_bytes(
                    digest.as_bytes()[..16]
                        .try_into()
                        .expect("BLAKE3 prefix is 16 bytes"),
                ))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}_", Self::PREFIX)?;
                encode(self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({self})", stringify!($name))
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(wire: &str) -> Result<Self, Self::Err> {
                Self::parse(wire)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = IdParseError;

            fn try_from(wire: &str) -> Result<Self, Self::Error> {
                Self::parse(wire)
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.collect_str(self)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                struct Visitor;

                impl serde::de::Visitor<'_> for Visitor {
                    type Value = $name;

                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        write!(f, "a valid {} identifier", stringify!($name))
                    }

                    fn visit_str<E>(self, wire: &str) -> Result<Self::Value, E>
                    where
                        E: serde::de::Error,
                    {
                        $name::parse(wire).map_err(E::custom)
                    }
                }

                deserializer.deserialize_str(Visitor)
            }
        }
    };
}

typed_id!(PrincipalId, "principal");
typed_id!(ProjectId, "project");
typed_id!(ThreadId, "thread");
typed_id!(RunId, "run");
typed_id!(AttemptId, "attempt");
typed_id!(TurnId, "turn");
typed_id!(ModelCallId, "model_call");
typed_id!(ToolCallId, "tool_call");
typed_id!(TaskId, "task");
typed_id!(AgentLinkId, "agent_link");
typed_id!(ExternalTaskId, "external_task");
typed_id!(DaemonServiceId, "daemon_service");
typed_id!(WorkspaceId, "workspace");
typed_id!(ProcessId, "process");
typed_id!(TerminalId, "terminal");
typed_id!(ApprovalId, "approval");
typed_id!(CheckpointId, "checkpoint");
typed_id!(ArtifactId, "artifact");
typed_id!(ExperimentId, "experiment");
typed_id!(CommandId, "cmd");
typed_id!(EventId, "evt");
