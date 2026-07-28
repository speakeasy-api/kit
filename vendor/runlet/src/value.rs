use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Deterministic runtime value representation used throughout Runlet.
pub enum CanonicalValue {
    /// Absence of a value.
    Null,
    /// Boolean value.
    Boolean(bool),
    /// Signed 64-bit integer.
    Integer(i64),
    /// Finite binary64 number.
    Number(f64),
    /// Unicode string.
    String(String),
    /// Opaque byte sequence.
    Bytes(Vec<u8>),
    /// Ordered list of values.
    List(Vec<CanonicalValue>),
    /// String-keyed object kept in lexical key order.
    Object(BTreeMap<String, CanonicalValue>),
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
/// Failure while constructing or serializing a canonical value.
pub enum ValueError {
    /// A `NaN` or infinite number was supplied.
    #[error("RL5103 NON_FINITE_NUMBER: non-finite numbers are not portable")]
    NonFiniteNumber,
    /// JSON output was requested for a value containing raw bytes.
    #[error("RL5207 NOT_JSON_REPRESENTABLE: bytes have no implicit JSON encoding")]
    NotJsonRepresentable,
    /// Serialization exceeded the defensive nesting limit.
    #[error("canonical presentation exceeded the configured depth")]
    DepthLimit,
}

impl CanonicalValue {
    /// Creates a finite number, normalizing negative zero.
    pub fn number(value: f64) -> Result<Self, ValueError> {
        if value.is_finite() {
            Ok(Self::Number(if value == 0.0 { 0.0 } else { value }))
        } else {
            Err(ValueError::NonFiniteNumber)
        }
    }

    /// Runlet Canonical Value Encoding, version 1.
    pub fn rcve(&self) -> Result<Vec<u8>, ValueError> {
        let mut out = vec![];
        self.encode(&mut out)?;
        Ok(out)
    }

    /// Returns the SHA-256 digest of the version-1 canonical encoding.
    pub fn digest(&self) -> Result<[u8; 32], ValueError> {
        let bytes = self.rcve()?;
        Ok(Sha256::digest(bytes).into())
    }

    /// Returns the canonical digest as lowercase hexadecimal.
    pub fn digest_hex(&self) -> Result<String, ValueError> {
        Ok(hex::encode(self.digest()?))
    }

    /// Serializes the value as deterministic, compact presentation JSON.
    ///
    /// Raw byte values return [`ValueError::NotJsonRepresentable`].
    pub fn presentation_json(&self) -> Result<String, ValueError> {
        let mut out = String::new();
        self.json(&mut out, 0)?;
        Ok(out)
    }

    fn encode(&self, out: &mut Vec<u8>) -> Result<(), ValueError> {
        match self {
            Self::Null => out.push(0x00),
            Self::Boolean(false) => out.push(0x01),
            Self::Boolean(true) => out.push(0x02),
            Self::Integer(v) => {
                out.push(0x10);
                let zig = ((*v as u64) << 1) ^ ((*v >> 63) as u64);
                leb(zig, out);
            }
            Self::Number(v) => {
                if !v.is_finite() {
                    return Err(ValueError::NonFiniteNumber);
                }
                out.push(0x11);
                out.extend_from_slice(&(if *v == 0.0 { 0.0 } else { *v }).to_bits().to_be_bytes());
            }
            Self::String(s) => {
                out.push(0x20);
                leb(s.len() as u64, out);
                out.extend_from_slice(s.as_bytes());
            }
            Self::Bytes(b) => {
                out.push(0x21);
                leb(b.len() as u64, out);
                out.extend_from_slice(b);
            }
            Self::List(xs) => {
                out.push(0x30);
                leb(xs.len() as u64, out);
                for x in xs {
                    x.encode(out)?;
                }
            }
            Self::Object(map) => {
                out.push(0x31);
                leb(map.len() as u64, out);
                for (k, v) in map {
                    Self::String(k.clone()).encode(out)?;
                    v.encode(out)?;
                }
            }
        }
        Ok(())
    }

    fn json(&self, out: &mut String, depth: usize) -> Result<(), ValueError> {
        if depth > 256 {
            return Err(ValueError::DepthLimit);
        }
        match self {
            Self::Null => out.push_str("null"),
            Self::Boolean(v) => out.push_str(if *v { "true" } else { "false" }),
            Self::Integer(v) => out.push_str(&v.to_string()),
            Self::Number(v) => out.push_str(&js_number(*v)?),
            Self::String(s) => json_string(s, out),
            Self::Bytes(_) => return Err(ValueError::NotJsonRepresentable),
            Self::List(xs) => {
                out.push('[');
                for (i, x) in xs.iter().enumerate() {
                    if i > 0 {
                        out.push(',')
                    }
                    x.json(out, depth + 1)?;
                }
                out.push(']');
            }
            Self::Object(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',')
                    }
                    json_string(k, out);
                    out.push(':');
                    v.json(out, depth + 1)?;
                }
                out.push('}');
            }
        }
        Ok(())
    }
}

fn leb(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let mut b = (n & 0x7f) as u8;
        n >>= 7;
        if n != 0 {
            b |= 0x80
        }
        out.push(b);
        if n == 0 {
            break;
        }
    }
}

fn json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn js_number(v: f64) -> Result<String, ValueError> {
    if !v.is_finite() {
        return Err(ValueError::NonFiniteNumber);
    }
    if v == 0.0 {
        return Ok("0".into());
    }
    let mut raw = ryu::Buffer::new().format_finite(v).to_ascii_lowercase();
    let abs = v.abs();
    if let Some(ep) = raw.find('e') {
        let exponent: i32 = raw[ep + 1..].parse().unwrap();
        let mant = &raw[..ep];
        if (1e-6..1e21).contains(&abs) {
            let neg = mant.starts_with('-');
            let digits = mant.trim_start_matches('-').replace('.', "");
            let decimal = 1 + exponent;
            let mut fixed = String::new();
            if neg {
                fixed.push('-')
            }
            if decimal <= 0 {
                fixed.push_str("0.");
                fixed.extend(std::iter::repeat_n('0', (-decimal) as usize));
                fixed.push_str(&digits);
            } else if decimal as usize >= digits.len() {
                fixed.push_str(&digits);
                fixed.extend(std::iter::repeat_n('0', decimal as usize - digits.len()));
            } else {
                let d = decimal as usize;
                fixed.push_str(&digits[..d]);
                fixed.push('.');
                fixed.push_str(&digits[d..]);
            }
            return Ok(fixed);
        }
        let (m, e) = raw.split_at(ep);
        let exp = &e[1..];
        raw = format!(
            "{}e{}{}",
            m,
            if exp.starts_with('-') || exp.starts_with('+') {
                ""
            } else {
                "+"
            },
            exp
        );
    } else if abs < 1e-6 {
        // ryu already chooses scientific notation for values below ECMAScript's fixed threshold in practice.
    }
    Ok(raw)
}

impl From<bool> for CanonicalValue {
    fn from(v: bool) -> Self {
        Self::Boolean(v)
    }
}
impl From<i64> for CanonicalValue {
    fn from(v: i64) -> Self {
        Self::Integer(v)
    }
}
impl From<String> for CanonicalValue {
    fn from(v: String) -> Self {
        Self::String(v)
    }
}
impl From<&str> for CanonicalValue {
    fn from(v: &str) -> Self {
        Self::String(v.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rcve_examples() {
        assert_eq!(CanonicalValue::Null.rcve().unwrap(), [0]);
        assert_eq!(CanonicalValue::Integer(-1).rcve().unwrap(), [0x10, 1]);
        assert_eq!(
            CanonicalValue::String("é".into()).rcve().unwrap(),
            [0x20, 2, 0xc3, 0xa9]
        );
    }
    #[test]
    fn canonical_json() {
        let mut o = BTreeMap::new();
        o.insert("z".into(), 1.into());
        o.insert("a".into(), CanonicalValue::String("\n".into()));
        assert_eq!(
            CanonicalValue::Object(o).presentation_json().unwrap(),
            "{\"a\":\"\\n\",\"z\":1}"
        );
    }
    #[test]
    fn number_thresholds() {
        assert_eq!(js_number(-0.0).unwrap(), "0");
        assert_eq!(js_number(1e21).unwrap(), "1e+21");
    }
}
