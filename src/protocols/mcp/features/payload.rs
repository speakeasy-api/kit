use std::{collections::BTreeMap, fmt, sync::Arc};

use serde::{
    Deserializer,
    de::{DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};

use crate::capabilities::kernel::identity::{Digest, DigestAlgorithm};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PayloadLimits {
    max_bytes: usize,
    max_depth: usize,
    max_nodes: usize,
    max_string_bytes: usize,
    max_collection_items: usize,
}

impl PayloadLimits {
    pub(crate) const fn max_bytes(self) -> usize {
        self.max_bytes
    }

    pub const fn new(
        max_bytes: usize,
        max_depth: usize,
        max_nodes: usize,
        max_string_bytes: usize,
        max_collection_items: usize,
    ) -> Result<Self, PayloadError> {
        if max_bytes == 0
            || max_depth == 0
            || max_nodes == 0
            || max_string_bytes == 0
            || max_collection_items == 0
        {
            return Err(PayloadError::InvalidLimits);
        }
        Ok(Self {
            max_bytes,
            max_depth,
            max_nodes,
            max_string_bytes,
            max_collection_items,
        })
    }

    pub const fn with_max_bytes(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            ..Self::default_limits()
        }
    }

    const fn default_limits() -> Self {
        Self {
            max_bytes: 4 * 1024 * 1024,
            max_depth: 64,
            max_nodes: 100_000,
            max_string_bytes: 1024 * 1024,
            max_collection_items: 16_384,
        }
    }
}

impl Default for PayloadLimits {
    fn default() -> Self {
        Self::default_limits()
    }
}

#[derive(Clone, Debug)]
pub struct RawPayload {
    source: Arc<[u8]>,
    canonical: Arc<[u8]>,
    value: Value,
    source_digest: Digest,
    canonical_digest: Digest,
}

impl RawPayload {
    pub fn parse(bytes: impl AsRef<[u8]>, limits: PayloadLimits) -> Result<Self, PayloadError> {
        let bytes = bytes.as_ref();
        if bytes.is_empty() {
            return Err(PayloadError::Malformed);
        }
        if bytes.len() > limits.max_bytes {
            return Err(PayloadError::Bytes);
        }
        preflight_depth(bytes, limits.max_depth)?;
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        let mut state = ParseState { limits, nodes: 0 };
        let value = BoundedValueSeed {
            state: &mut state,
            depth: 0,
        }
        .deserialize(&mut deserializer)
        .map_err(classify_json_error)?;
        deserializer.end().map_err(classify_json_error)?;
        let canonical = serde_json::to_vec(&value).map_err(|_| PayloadError::Malformed)?;
        Ok(Self {
            source_digest: Digest::of(DigestAlgorithm::Sha256, bytes),
            canonical_digest: Digest::of(DigestAlgorithm::Sha256, &canonical),
            source: Arc::from(bytes),
            canonical: Arc::from(canonical),
            value,
        })
    }

    pub fn from_value(value: Value, limits: PayloadLimits) -> Result<Self, PayloadError> {
        let bytes = serde_json::to_vec(&value).map_err(|_| PayloadError::Malformed)?;
        Self::parse(bytes, limits)
    }

    pub fn source_bytes(&self) -> &[u8] {
        &self.source
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical
    }

    pub(crate) fn accounted_bytes(&self) -> Option<usize> {
        self.source.len().checked_add(self.canonical.len())
    }

    pub const fn value(&self) -> &Value {
        &self.value
    }

    pub const fn source_digest(&self) -> Digest {
        self.source_digest
    }

    pub const fn canonical_digest(&self) -> Digest {
        self.canonical_digest
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadError {
    InvalidLimits,
    Bytes,
    Depth,
    Nodes,
    StringBytes,
    CollectionItems,
    DuplicateKey,
    Malformed,
}

impl fmt::Display for PayloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidLimits => "invalid MCP payload limits",
            Self::Bytes => "MCP payload exceeds its byte bound",
            Self::Depth => "MCP payload exceeds its depth bound",
            Self::Nodes => "MCP payload exceeds its node bound",
            Self::StringBytes => "MCP payload string exceeds its byte bound",
            Self::CollectionItems => "MCP payload collection exceeds its item bound",
            Self::DuplicateKey => "MCP payload contains a duplicate JSON object key",
            Self::Malformed => "MCP payload is malformed JSON",
        })
    }
}

impl std::error::Error for PayloadError {}

fn classify_json_error(error: serde_json::Error) -> PayloadError {
    let message = error.to_string();
    for (marker, error) in [
        ("duplicate JSON object key", PayloadError::DuplicateKey),
        ("MCP payload depth bound exceeded", PayloadError::Depth),
        ("MCP payload node bound exceeded", PayloadError::Nodes),
        (
            "MCP payload string bound exceeded",
            PayloadError::StringBytes,
        ),
        (
            "MCP payload collection bound exceeded",
            PayloadError::CollectionItems,
        ),
    ] {
        if message.contains(marker) {
            return error;
        }
    }
    PayloadError::Malformed
}

fn preflight_depth(bytes: &[u8], max_depth: usize) -> Result<(), PayloadError> {
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;
    for &byte in bytes {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth = depth.checked_add(1).ok_or(PayloadError::Depth)?;
                if depth > max_depth {
                    return Err(PayloadError::Depth);
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

struct ParseState {
    limits: PayloadLimits,
    nodes: usize,
}

struct BoundedValueSeed<'a> {
    state: &'a mut ParseState,
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for BoundedValueSeed<'_> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        self.state.nodes = self
            .state
            .nodes
            .checked_add(1)
            .ok_or_else(|| serde::de::Error::custom("MCP payload node bound exceeded"))?;
        if self.depth > self.state.limits.max_depth {
            return Err(serde::de::Error::custom("MCP payload depth bound exceeded"));
        }
        if self.state.nodes > self.state.limits.max_nodes {
            return Err(serde::de::Error::custom("MCP payload node bound exceeded"));
        }
        deserializer.deserialize_any(BoundedValueVisitor {
            state: self.state,
            depth: self.depth,
        })
    }
}

struct BoundedValueVisitor<'a> {
    state: &'a mut ParseState,
    depth: usize,
}

impl<'de> Visitor<'de> for BoundedValueVisitor<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(Value::Number(Number::from(value)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > self.state.limits.max_string_bytes {
            return Err(E::custom("MCP payload string bound exceeded"));
        }
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > self.state.limits.max_string_bytes {
            return Err(E::custom("MCP payload string bound exceeded"));
        }
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(BoundedValueSeed {
            state: self.state,
            depth: self.depth + 1,
        })? {
            if values.len() >= self.state.limits.max_collection_items {
                return Err(serde::de::Error::custom(
                    "MCP payload collection bound exceeded",
                ));
            }
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A>(self, mut input: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = BTreeMap::new();
        while let Some(key) = input.next_key_seed(BoundedStringSeed {
            max_bytes: self.state.limits.max_string_bytes,
        })? {
            if values.len() >= self.state.limits.max_collection_items {
                return Err(serde::de::Error::custom(
                    "MCP payload collection bound exceeded",
                ));
            }
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom("duplicate JSON object key"));
            }
            let value = input.next_value_seed(BoundedValueSeed {
                state: self.state,
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values.into_iter().collect::<Map<_, _>>()))
    }
}

struct BoundedStringSeed {
    max_bytes: usize,
}

impl<'de> DeserializeSeed<'de> for BoundedStringSeed {
    type Value = String;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(BoundedStringVisitor {
            max_bytes: self.max_bytes,
        })
    }
}

struct BoundedStringVisitor {
    max_bytes: usize,
}

impl Visitor<'_> for BoundedStringVisitor {
    type Value = String;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded JSON object key")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > self.max_bytes {
            return Err(E::custom("MCP payload string bound exceeded"));
        }
        Ok(value.to_owned())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > self.max_bytes {
            return Err(E::custom("MCP payload string bound exceeded"));
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_payload_rejects_duplicates_malformed_and_each_bound() {
        let limits = PayloadLimits::new(32, 2, 3, 3, 1).unwrap();
        assert_eq!(
            RawPayload::parse(br#"{"a":1,"a":2}"#, PayloadLimits::default()).unwrap_err(),
            PayloadError::DuplicateKey
        );
        assert_eq!(
            RawPayload::parse(b"{", PayloadLimits::default()).unwrap_err(),
            PayloadError::Malformed
        );
        assert_eq!(
            RawPayload::parse(br#"{"a":[[1]]}"#, limits).unwrap_err(),
            PayloadError::Depth
        );
        assert_eq!(
            RawPayload::parse(br#"{"a":"long"}"#, limits).unwrap_err(),
            PayloadError::StringBytes
        );
        assert_eq!(
            RawPayload::parse(br#"[1,2]"#, limits).unwrap_err(),
            PayloadError::CollectionItems
        );
        assert_eq!(
            RawPayload::parse(
                br#"{"a":1,"b":2}"#,
                PayloadLimits::new(32, 2, 2, 3, 2).unwrap()
            )
            .unwrap_err(),
            PayloadError::Nodes
        );
        assert_eq!(
            RawPayload::parse([b'x'; 33], limits).unwrap_err(),
            PayloadError::Bytes
        );
    }

    #[test]
    fn raw_payload_preserves_authoritative_and_canonical_copies() {
        let payload =
            RawPayload::parse(b"{\n \"b\": 2, \"a\": 1\n}", PayloadLimits::default()).unwrap();
        assert_eq!(payload.source_bytes(), b"{\n \"b\": 2, \"a\": 1\n}");
        assert_eq!(payload.canonical_bytes(), br#"{"a":1,"b":2}"#);
        assert_ne!(payload.source_digest(), payload.canonical_digest());
    }
}
