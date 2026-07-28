use std::{
    fmt,
    str::FromStr,
    sync::atomic::{Ordering, compiler_fence},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub const REDACTED: &str = "[REDACTED]";

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SecretHandle(String);

impl SecretHandle {
    pub fn parse(identifier: &str) -> Result<Self, SecretHandleError> {
        if identifier.is_empty()
            || identifier.len() > 255
            || identifier.bytes().any(|byte| !byte.is_ascii_graphic())
        {
            Err(SecretHandleError)
        } else {
            Ok(Self(identifier.to_owned()))
        }
    }

    pub fn identifier(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl fmt::Display for SecretHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl FromStr for SecretHandle {
    type Err = SecretHandleError;

    fn from_str(identifier: &str) -> Result<Self, Self::Err> {
        Self::parse(identifier)
    }
}

impl Serialize for SecretHandle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.identifier())
    }
}

impl<'de> Deserialize<'de> for SecretHandle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = SecretHandle;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an opaque secret identifier")
            }

            fn visit_str<E>(self, identifier: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                SecretHandle::parse(identifier).map_err(E::custom)
            }
        }

        deserializer.deserialize_str(Visitor)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SecretHandleError;

impl fmt::Display for SecretHandleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("secret identifier must contain 1 to 255 visible ASCII bytes")
    }
}

impl std::error::Error for SecretHandleError {}

pub trait SecretResolver {
    type Error;

    fn resolve(&self, handle: &SecretHandle) -> Result<SecretLease, Self::Error>;
}

pub struct SecretLease {
    value: Vec<u8>,
}

impl SecretLease {
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self {
            value: value.into(),
        }
    }

    pub fn expose(&self) -> &[u8] {
        &self.value
    }
}

impl fmt::Debug for SecretLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl fmt::Display for SecretLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl Drop for SecretLease {
    fn drop(&mut self) {
        self.value.fill(0);
        compiler_fence(Ordering::SeqCst);
        std::hint::black_box(&mut self.value);
    }
}

pub fn with_secret<R, T>(
    resolver: &R,
    handle: &SecretHandle,
    use_secret: impl FnOnce(&[u8]) -> T,
) -> Result<T, R::Error>
where
    R: SecretResolver,
{
    let lease = resolver.resolve(handle)?;
    Ok(use_secret(lease.expose()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataClass {
    Public,
    Secret,
    Url,
}

pub fn classify_field(name: &str) -> DataClass {
    if matches_ascii_case(
        name,
        &[
            "access_token",
            "api_key",
            "apikey",
            "bearer_token",
            "client_secret",
            "credential",
            "credentials",
            "password",
            "passwd",
            "passphrase",
            "private_key",
            "refresh_token",
            "secret",
            "session_token",
            "token",
        ],
    ) {
        DataClass::Secret
    } else if matches_ascii_case(
        name,
        &[
            "callback_url",
            "endpoint",
            "redirect_url",
            "uri",
            "url",
            "webhook_url",
        ],
    ) {
        DataClass::Url
    } else {
        DataClass::Public
    }
}

pub fn classify_header(name: &str) -> DataClass {
    if matches_ascii_case(
        name,
        &[
            "authorization",
            "cookie",
            "proxy-authorization",
            "set-cookie",
            "x-api-key",
            "x-auth-token",
        ],
    ) {
        DataClass::Secret
    } else {
        DataClass::Public
    }
}

fn matches_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}
