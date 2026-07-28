use std::collections::BTreeMap;

use kit::{
    domain::secret::{
        DataClass, REDACTED, SecretHandle, SecretLease, SecretResolver, classify_field,
        classify_header, with_secret,
    },
    telemetry::redact::{CaptureBoundary, CaptureRedactor, StructuredValue},
};

const CANARY: &str = "kit-canary+/=42";
const CANARY_URL: &str = "%6B%69%74%2D%63%61%6E%61%72%79%2B%2F%3D%34%32";
const CANARY_BASE64: &str = "a2l0LWNhbmFyeSsvPTQy";

struct FixtureResolver;

impl SecretResolver for FixtureResolver {
    type Error = ();

    fn resolve(&self, _handle: &SecretHandle) -> Result<SecretLease, Self::Error> {
        Ok(SecretLease::new(CANARY.as_bytes().to_vec()))
    }
}

#[test]
fn opaque_handles_roundtrip_identifier_without_rendering_it() {
    let handle = SecretHandle::parse("vault:provider/api-key").unwrap();
    assert_eq!(handle.identifier(), "vault:provider/api-key");
    assert_eq!(handle.to_string(), REDACTED);
    assert_eq!(format!("{handle:?}"), REDACTED);

    let wire = serde_json::to_string(&handle).unwrap();
    assert_eq!(serde_json::from_str::<SecretHandle>(&wire).unwrap(), handle);
    assert_eq!(
        with_secret(&FixtureResolver, &handle, |value| value
            == CANARY.as_bytes()),
        Ok(true)
    );
}

#[test]
fn exact_field_header_and_url_classification_redacts_without_substring_false_positives() {
    assert_eq!(classify_field("client_secret"), DataClass::Secret);
    assert_eq!(classify_field("CLIENT_SECRET"), DataClass::Secret);
    assert_eq!(classify_field("secretary"), DataClass::Public);
    assert_eq!(classify_field("redirect_url"), DataClass::Url);
    assert_eq!(classify_header("Authorization"), DataClass::Secret);
    assert_eq!(classify_header("X-Authorization-Mode"), DataClass::Public);

    let leases = [SecretLease::new(CANARY.as_bytes().to_vec())];
    let redactor = CaptureRedactor::new(&leases);
    assert_eq!(
        redactor.redact_header(CaptureBoundary::Log, "authorization", "Bearer anything"),
        REDACTED
    );
    let url = redactor.redact_url(
        CaptureBoundary::Trace,
        "https://user:pass@example.test/cb?api_key=not-the-canary&safe=yes",
    );
    assert_eq!(
        url,
        "https://[REDACTED]@example.test/cb?api_key=[REDACTED]&safe=yes"
    );
}

#[test]
fn persistent_capture_boundaries_remove_raw_and_encoded_canaries() {
    let leases = [SecretLease::new(CANARY.as_bytes().to_vec())];
    let redactor = CaptureRedactor::new(&leases);

    for boundary in [
        CaptureBoundary::Artifact,
        CaptureBoundary::Log,
        CaptureBoundary::TerminalMetadata,
        CaptureBoundary::Trace,
    ] {
        let raw = format!("raw={CANARY} percent={CANARY_URL} base64={CANARY_BASE64}");
        let capture = redactor.sanitize(boundary, raw.as_bytes());
        let sanitized = String::from_utf8_lossy(capture.bytes().unwrap());
        for forbidden in [CANARY, CANARY_URL, CANARY_BASE64] {
            assert_eq!(
                sanitized.matches(forbidden).count(),
                0,
                "{boundary:?} leaked {forbidden}"
            );
        }
    }
}

#[test]
fn streaming_capture_redacts_raw_non_utf8_and_encoded_forms_split_across_chunks() {
    const RAW: &[u8] = &[0xff, 0x00, 0xa9];
    const PERCENT: &[u8] = b"%FF%00%A9";
    const BASE64: &[u8] = b"/wCp";
    let leases = [SecretLease::new(RAW)];
    let redactor = CaptureRedactor::new(&leases);
    let mut capture = redactor.start(CaptureBoundary::Log);
    for chunk in [
        b"raw=".as_slice(),
        &RAW[..2],
        &RAW[2..],
        b" percent=%FF%00",
        b"%A9 base64=/w",
        b"Cp",
    ] {
        capture.push(chunk).unwrap();
    }
    capture.finish().unwrap();
    let output = capture.bytes().unwrap();
    for forbidden in [RAW, PERCENT, BASE64] {
        assert_eq!(
            output
                .windows(forbidden.len())
                .filter(|window| *window == forbidden)
                .count(),
            0
        );
    }
    assert_eq!(
        redactor.redact_text(CaptureBoundary::Log, "safe: café"),
        "safe: café"
    );
    assert_eq!(
        redactor.redact_text(CaptureBoundary::Log, "encoded=/wCp"),
        "encoded=[REDACTED]"
    );
}

#[test]
fn nested_structured_logs_preserve_safe_types_and_replace_sensitive_values() {
    let leases = [SecretLease::new(CANARY.as_bytes().to_vec())];
    let redactor = CaptureRedactor::new(&leases);
    let value = StructuredValue::Object(BTreeMap::from([
        ("attempt".to_owned(), StructuredValue::U64(7)),
        ("cached".to_owned(), StructuredValue::Bool(true)),
        (
            "headers".to_owned(),
            StructuredValue::Object(BTreeMap::from([
                (
                    "Authorization".to_owned(),
                    StructuredValue::String(format!("Bearer {CANARY}")),
                ),
                (
                    "Content-Type".to_owned(),
                    StructuredValue::String("application/json".to_owned()),
                ),
            ])),
        ),
        (
            "nested".to_owned(),
            StructuredValue::Array(vec![StructuredValue::Object(BTreeMap::from([
                (
                    "password".to_owned(),
                    StructuredValue::String(CANARY.to_owned()),
                ),
                (
                    "message".to_owned(),
                    StructuredValue::String(format!("failed with {CANARY_BASE64}")),
                ),
            ]))]),
        ),
    ]));

    let safe = redactor.redact_value(CaptureBoundary::Log, &value);
    let StructuredValue::Object(fields) = &safe else {
        panic!("object changed type");
    };
    assert_eq!(fields["attempt"], StructuredValue::U64(7));
    assert_eq!(fields["cached"], StructuredValue::Bool(true));
    let exported = safe.to_json();
    for forbidden in [CANARY, CANARY_URL, CANARY_BASE64, "Bearer kit-canary"] {
        assert_eq!(
            exported.matches(forbidden).count(),
            0,
            "exported corpus leaked {forbidden}"
        );
    }
    assert!(exported.contains("\"Authorization\":\"[REDACTED]\""));
    assert!(exported.contains("\"password\":\"[REDACTED]\""));
}
