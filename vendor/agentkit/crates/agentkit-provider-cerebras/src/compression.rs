//! Request-body compression (feature = `compression`).
//!
//! Cerebras accepts `application/json` or `application/vnd.msgpack` bodies,
//! optionally gzip-compressed via `Content-Encoding: gzip`. Only the request
//! path is compressed; responses are always plain JSON.

use std::io::Write;

use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::Value;

use crate::config::{CompressionConfig, RequestEncoding};

/// Encoded request body + content headers.
pub struct Encoded {
    /// Body bytes.
    pub body: Vec<u8>,
    /// `Content-Type` value.
    pub content_type: &'static str,
    /// `Content-Encoding` value (if any).
    pub content_encoding: Option<&'static str>,
}

/// Encodes `value` according to the compression configuration. Falls back to
/// plain JSON when the serialized size is below [`CompressionConfig::min_bytes`].
pub fn encode_body(value: &Value, cfg: &CompressionConfig) -> Result<Encoded, String> {
    let json_bytes = serde_json::to_vec(value).map_err(|e| format!("json serialize: {e}"))?;

    if json_bytes.len() < cfg.min_bytes {
        return Ok(Encoded {
            body: json_bytes,
            content_type: "application/json",
            content_encoding: None,
        });
    }

    match cfg.encoding {
        RequestEncoding::Json => Ok(Encoded {
            body: json_bytes,
            content_type: "application/json",
            content_encoding: None,
        }),
        RequestEncoding::Msgpack => {
            let body =
                rmp_serde::to_vec_named(value).map_err(|e| format!("msgpack serialize: {e}"))?;
            Ok(Encoded {
                body,
                content_type: "application/vnd.msgpack",
                content_encoding: None,
            })
        }
        RequestEncoding::JsonGzip => {
            let body = gzip(&json_bytes)?;
            Ok(Encoded {
                body,
                content_type: "application/json",
                content_encoding: Some("gzip"),
            })
        }
        RequestEncoding::MsgpackGzip => {
            let msgpack =
                rmp_serde::to_vec_named(value).map_err(|e| format!("msgpack serialize: {e}"))?;
            let body = gzip(&msgpack)?;
            Ok(Encoded {
                body,
                content_type: "application/vnd.msgpack",
                content_encoding: Some("gzip"),
            })
        }
    }
}

fn gzip(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(bytes)
        .map_err(|e| format!("gzip write: {e}"))?;
    encoder.finish().map_err(|e| format!("gzip finish: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use serde_json::json;
    use std::io::Read;

    fn large_body() -> Value {
        json!({ "data": "x".repeat(8192) })
    }

    #[test]
    fn below_min_bytes_falls_back_to_json() {
        let cfg = CompressionConfig {
            encoding: RequestEncoding::MsgpackGzip,
            min_bytes: 10_000,
        };
        let small = json!({ "hi": "there" });
        let out = encode_body(&small, &cfg).unwrap();
        assert_eq!(out.content_type, "application/json");
        assert!(out.content_encoding.is_none());
    }

    #[test]
    fn json_gzip_round_trips() {
        let cfg = CompressionConfig {
            encoding: RequestEncoding::JsonGzip,
            min_bytes: 0,
        };
        let body = large_body();
        let out = encode_body(&body, &cfg).unwrap();
        assert_eq!(out.content_type, "application/json");
        assert_eq!(out.content_encoding, Some("gzip"));
        let mut decoded = String::new();
        GzDecoder::new(out.body.as_slice())
            .read_to_string(&mut decoded)
            .unwrap();
        let parsed: Value = serde_json::from_str(&decoded).unwrap();
        assert_eq!(parsed, body);
    }

    #[test]
    fn msgpack_selected() {
        let cfg = CompressionConfig {
            encoding: RequestEncoding::Msgpack,
            min_bytes: 0,
        };
        let out = encode_body(&json!({"a": 1}), &cfg).unwrap();
        assert_eq!(out.content_type, "application/vnd.msgpack");
        assert!(out.content_encoding.is_none());
    }
}
