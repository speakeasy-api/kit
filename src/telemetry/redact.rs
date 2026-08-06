use std::{collections::BTreeMap, fmt};

use aho_corasick::{
    AhoCorasick, AhoCorasickBuilder, Anchored, MatchKind,
    automaton::{Automaton, StateID},
    nfa::contiguous::NFA,
};

use crate::domain::{
    lifecycle::ProcessClaim,
    secret::{DataClass, REDACTED, SecretLease, classify_field, classify_header},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureBoundary {
    Event,
    Artifact,
    Log,
    Trace,
    TerminalMetadata,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StructuredValue {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    String(String),
    Array(Vec<StructuredValue>),
    Object(BTreeMap<String, StructuredValue>),
}

impl StructuredValue {
    pub fn to_json(&self) -> String {
        let mut output = String::new();
        self.write_json(&mut output);
        output
    }

    fn write_json(&self, output: &mut String) {
        match self {
            Self::Null => output.push_str("null"),
            Self::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Self::I64(value) => output.push_str(&value.to_string()),
            Self::U64(value) => output.push_str(&value.to_string()),
            Self::F64(value) if value.is_finite() => output.push_str(&value.to_string()),
            Self::F64(_) => output.push_str("null"),
            Self::String(value) => write_json_string(output, value),
            Self::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        output.push(',');
                    }
                    value.write_json(output);
                }
                output.push(']');
            }
            Self::Object(fields) => {
                output.push('{');
                for (index, (name, value)) in fields.iter().enumerate() {
                    if index != 0 {
                        output.push(',');
                    }
                    write_json_string(output, name);
                    output.push(':');
                    value.write_json(output);
                }
                output.push('}');
            }
        }
    }
}

pub struct CaptureRedactor<'a> {
    secrets: &'a [SecretLease],
    provenance: Option<SanitizerProvenance>,
}

impl<'a> CaptureRedactor<'a> {
    pub const fn new(secrets: &'a [SecretLease]) -> Self {
        Self {
            secrets,
            provenance: None,
        }
    }

    pub(crate) const fn process_bound(
        secrets: &'a [SecretLease],
        provenance: SanitizerProvenance,
    ) -> Self {
        Self {
            secrets,
            provenance: Some(provenance),
        }
    }

    pub fn redact_text(&self, _boundary: CaptureBoundary, value: &str) -> String {
        let patterns = SecretPatterns::new(self.secrets);
        if patterns.truncated {
            return REDACTED.to_owned();
        }
        redact_patterns(value, &patterns)
    }

    pub fn sanitize(&self, boundary: CaptureBoundary, value: &[u8]) -> SanitizedCapture {
        let mut capture = self.start(boundary);
        capture.push(value).expect("new capture is writable");
        capture.finish().expect("new capture can be finished");
        capture
    }

    pub fn start(&self, boundary: CaptureBoundary) -> SanitizedCapture {
        SanitizedCapture {
            boundary,
            patterns: SecretPatterns::new(self.secrets),
            pending: Vec::new(),
            sanitized: Vec::new(),
            provenance: self.provenance,
            finished: false,
        }
    }

    pub fn scanner(&self) -> SensitiveDataScanner {
        SensitiveDataScanner::new(SecretPatterns::new(self.secrets))
    }

    pub fn redact_field(&self, boundary: CaptureBoundary, name: &str, value: &str) -> String {
        match classify_field(name) {
            DataClass::Secret => REDACTED.to_owned(),
            DataClass::Url => self.redact_url(boundary, value),
            DataClass::Public => self.redact_text(boundary, value),
        }
    }

    pub fn redact_header(&self, boundary: CaptureBoundary, name: &str, value: &str) -> String {
        if classify_header(name) == DataClass::Secret {
            REDACTED.to_owned()
        } else {
            self.redact_text(boundary, value)
        }
    }

    pub fn redact_url(&self, boundary: CaptureBoundary, value: &str) -> String {
        let mut output = redact_url_fields(value);
        output = self.redact_text(boundary, &output);
        output
    }

    pub fn redact_value(
        &self,
        boundary: CaptureBoundary,
        value: &StructuredValue,
    ) -> StructuredValue {
        self.redact_nested(boundary, value, false)
    }

    fn redact_nested(
        &self,
        boundary: CaptureBoundary,
        value: &StructuredValue,
        headers: bool,
    ) -> StructuredValue {
        match value {
            StructuredValue::String(value) => {
                StructuredValue::String(self.redact_text(boundary, value))
            }
            StructuredValue::Array(values) => StructuredValue::Array(
                values
                    .iter()
                    .map(|value| self.redact_nested(boundary, value, headers))
                    .collect(),
            ),
            StructuredValue::Object(fields) => StructuredValue::Object(
                fields
                    .iter()
                    .map(|(name, value)| {
                        let class = if headers {
                            classify_header(name)
                        } else {
                            classify_field(name)
                        };
                        let value = match class {
                            DataClass::Secret => StructuredValue::String(REDACTED.to_owned()),
                            DataClass::Url => match value {
                                StructuredValue::String(value) => {
                                    StructuredValue::String(self.redact_url(boundary, value))
                                }
                                _ => StructuredValue::String(REDACTED.to_owned()),
                            },
                            DataClass::Public => self.redact_nested(
                                boundary,
                                value,
                                name.eq_ignore_ascii_case("headers"),
                            ),
                        };
                        (self.redact_text(boundary, name), value)
                    })
                    .collect(),
            ),
            safe => safe.clone(),
        }
    }
}

pub struct SensitiveDataScanner {
    patterns: SecretPatterns,
    state: Option<StateID>,
    decoded_states: Vec<StateID>,
    decoders: [PercentDecoder; 4],
    found: bool,
}

impl SensitiveDataScanner {
    fn new(patterns: SecretPatterns) -> Self {
        let state = patterns
            .stream_matcher
            .as_ref()
            .and_then(|matcher| matcher.start_state(Anchored::No).ok());
        let decoded_states = patterns
            .raw_stream_matcher
            .as_ref()
            .and_then(|matcher| matcher.start_state(Anchored::No).ok())
            .map(|state| vec![state; 4])
            .unwrap_or_default();
        Self {
            found: patterns.truncated
                || (patterns.stream_matcher.is_some() && state.is_none())
                || (patterns.raw_stream_matcher.is_some() && decoded_states.is_empty()),
            patterns,
            state,
            decoded_states,
            decoders: std::array::from_fn(|_| PercentDecoder::default()),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.found {
            return;
        }
        let Some(mut state) = self.state else {
            return;
        };
        for &byte in bytes {
            let matched = {
                let Some(matcher) = &self.patterns.stream_matcher else {
                    return;
                };
                state = matcher.next_state(Anchored::No, state, byte);
                matcher.is_match(state)
            };
            if matched {
                self.found = true;
                break;
            }
            self.scan_decoded_byte(0, byte);
            if self.found {
                break;
            }
        }
        self.state = Some(state);
    }

    pub const fn found(&self) -> bool {
        self.found
    }

    pub(crate) fn fork(&self) -> Self {
        Self {
            patterns: self.patterns.clone(),
            state: self.state,
            decoded_states: self.decoded_states.clone(),
            decoders: self.decoders.clone(),
            found: self.found,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.state = self
            .patterns
            .stream_matcher
            .as_ref()
            .and_then(|matcher| matcher.start_state(Anchored::No).ok());
        self.decoded_states = self
            .patterns
            .raw_stream_matcher
            .as_ref()
            .and_then(|matcher| matcher.start_state(Anchored::No).ok())
            .map(|state| vec![state; 4])
            .unwrap_or_default();
        self.decoders = std::array::from_fn(|_| PercentDecoder::default());
        self.found = self.patterns.truncated
            || (self.patterns.stream_matcher.is_some() && self.state.is_none())
            || (self.patterns.raw_stream_matcher.is_some() && self.decoded_states.is_empty());
    }

    fn scan_decoded_byte(&mut self, layer: usize, byte: u8) {
        if layer == self.decoders.len() || self.decoded_states.is_empty() {
            return;
        }
        let mut output = [0_u8; 3];
        let count = self.decoders[layer].push(byte, &mut output);
        for &decoded in &output[..count] {
            let (state, matched) = {
                let matcher = self
                    .patterns
                    .raw_stream_matcher
                    .as_ref()
                    .expect("decoded states require a matcher");
                let state = matcher.next_state(Anchored::No, self.decoded_states[layer], decoded);
                (state, matcher.is_match(state))
            };
            self.decoded_states[layer] = state;
            if matched {
                self.found = true;
                return;
            }
            self.scan_decoded_byte(layer + 1, decoded);
            if self.found {
                return;
            }
        }
    }
}

#[derive(Clone, Default)]
struct PercentDecoder {
    pending: [u8; 2],
    len: u8,
}

impl PercentDecoder {
    fn push(&mut self, byte: u8, output: &mut [u8; 3]) -> usize {
        match self.len {
            0 if byte == b'%' => {
                self.len = 1;
                0
            }
            0 => {
                output[0] = byte;
                1
            }
            1 if hex_value(byte).is_some() => {
                self.pending[0] = byte;
                self.len = 2;
                0
            }
            1 => {
                self.len = 0;
                output[0] = b'%';
                output[1] = byte;
                2
            }
            2 => {
                self.len = 0;
                if let (Some(high), Some(low)) = (hex_value(self.pending[0]), hex_value(byte)) {
                    output[0] = high << 4 | low;
                    1
                } else {
                    output[0] = b'%';
                    output[1] = self.pending[0];
                    output[2] = byte;
                    3
                }
            }
            _ => unreachable!("percent decoder state is bounded"),
        }
    }
}

impl Drop for PercentDecoder {
    fn drop(&mut self) {
        self.pending.fill(0);
    }
}

impl fmt::Debug for CaptureRedactor<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CaptureRedactor([REDACTED])")
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct SanitizerProvenance {
    nonce: [u8; 32],
    claim: ProcessClaim,
    binding: [u8; 32],
}

impl SanitizerProvenance {
    pub(crate) fn issue(
        claim: ProcessClaim,
        profile_digest: &str,
        invocation_intent: &str,
    ) -> Result<Self, getrandom::Error> {
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce)?;
        let mut binding = blake3::Hasher::new();
        binding.update(profile_digest.as_bytes());
        binding.update(&[0]);
        binding.update(invocation_intent.as_bytes());
        Ok(Self {
            nonce,
            claim,
            binding: *binding.finalize().as_bytes(),
        })
    }

    pub(crate) const fn claim(self) -> ProcessClaim {
        self.claim
    }
}

impl fmt::Debug for SanitizerProvenance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SanitizerProvenance(REDACTED)")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapturePersistencePolicy(Option<SanitizerProvenance>);

impl CapturePersistencePolicy {
    pub const fn no_secrets() -> Self {
        Self(None)
    }

    pub(crate) const fn process_bound(provenance: SanitizerProvenance) -> Self {
        Self(Some(provenance))
    }

    pub(crate) fn is_for(self, claim: ProcessClaim) -> bool {
        self.0.is_none_or(|provenance| provenance.claim() == claim)
    }

    pub(crate) fn accepts(self, claim: ProcessClaim, capture: &SanitizedCapture) -> bool {
        match self.0 {
            None => capture.provenance.is_none(),
            Some(expected) => expected.claim() == claim && capture.provenance == Some(expected),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SanitizedCaptureError {
    AlreadyFinished,
    NotFinished,
}

impl fmt::Display for SanitizedCaptureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AlreadyFinished => "sanitized capture is already finished",
            Self::NotFinished => "sanitized capture is not finished",
        })
    }
}

impl std::error::Error for SanitizedCaptureError {}

/// Stateful capture that cannot expose bytes until all chunks have crossed the
/// redaction boundary. Only a possible secret-pattern prefix is retained
/// between chunks, so split raw, percent, and base64 encodings are redacted.
pub struct SanitizedCapture {
    boundary: CaptureBoundary,
    patterns: SecretPatterns,
    pending: Vec<u8>,
    sanitized: Vec<u8>,
    provenance: Option<SanitizerProvenance>,
    finished: bool,
}

impl SanitizedCapture {
    pub const fn boundary(&self) -> CaptureBoundary {
        self.boundary
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), SanitizedCaptureError> {
        if self.finished {
            return Err(SanitizedCaptureError::AlreadyFinished);
        }
        self.pending.extend_from_slice(chunk);
        self.flush(false);
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), SanitizedCaptureError> {
        if self.finished {
            return Err(SanitizedCaptureError::AlreadyFinished);
        }
        self.flush(true);
        self.finished = true;
        Ok(())
    }

    pub fn bytes(&self) -> Result<&[u8], SanitizedCaptureError> {
        self.finished
            .then_some(self.sanitized.as_slice())
            .ok_or(SanitizedCaptureError::NotFinished)
    }

    /// Takes bytes that are known not to be a prefix of a secret pattern while preserving the
    /// streaming matcher for later input. The returned capture is finished and retains the same
    /// process provenance, so it can cross a persistence boundary immediately.
    pub(crate) fn take_ready(&mut self) -> Option<Self> {
        if self.sanitized.is_empty() {
            return None;
        }
        Some(Self {
            boundary: self.boundary,
            patterns: SecretPatterns {
                values: Vec::new(),
                raw: Vec::new(),
                matcher: None,
                stream_matcher: None,
                raw_stream_matcher: None,
                truncated: false,
            },
            pending: Vec::new(),
            sanitized: std::mem::take(&mut self.sanitized),
            provenance: self.provenance,
            finished: true,
        })
    }

    fn flush(&mut self, finishing: bool) {
        let mut consumed = 0;
        while consumed < self.pending.len() {
            let pending = &self.pending[consumed..];
            if let Some(length) = self
                .patterns
                .values
                .iter()
                .filter(|pattern| pending.starts_with(pattern))
                .map(Vec::len)
                .max()
            {
                self.sanitized.extend_from_slice(REDACTED.as_bytes());
                consumed += length;
                continue;
            }
            if !finishing
                && self
                    .patterns
                    .values
                    .iter()
                    .any(|pattern| pattern.starts_with(pending))
            {
                break;
            }
            self.sanitized.push(self.pending[consumed]);
            consumed += 1;
        }
        if consumed != 0 {
            self.pending[..consumed].fill(0);
            self.pending.drain(..consumed);
        }
    }
}

impl fmt::Debug for SanitizedCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SanitizedCapture")
            .field("boundary", &self.boundary)
            .field("finished", &self.finished)
            .field("bytes", &self.sanitized.len())
            .field("provenance", &self.provenance.as_ref().map(|_| "REDACTED"))
            .finish()
    }
}

impl Drop for SanitizedCapture {
    fn drop(&mut self) {
        self.pending.fill(0);
    }
}

#[derive(Clone)]
struct SecretPatterns {
    values: Vec<Vec<u8>>,
    raw: Vec<Vec<u8>>,
    matcher: Option<AhoCorasick>,
    stream_matcher: Option<NFA>,
    raw_stream_matcher: Option<NFA>,
    truncated: bool,
}

impl SecretPatterns {
    fn new(secrets: &[SecretLease]) -> Self {
        const MAX_PATTERNS: usize = 4096;
        const MAX_PATTERN_BYTES: usize = 4 * 1024 * 1024;
        const MAX_RAW_BYTES: usize = 256 * 1024;
        const ENCODING_PASSES: usize = 3;
        let mut values = Vec::new();
        let mut raw = Vec::new();
        let mut total_bytes = 0_usize;
        let mut raw_bytes = 0_usize;
        let mut truncated = false;
        {
            let mut add = |value: Vec<u8>| {
                if value.is_empty() || values.iter().any(|existing| existing == &value) {
                    return true;
                }
                let Some(next_bytes) = total_bytes.checked_add(value.len()) else {
                    truncated = true;
                    return false;
                };
                if values.len() == MAX_PATTERNS || next_bytes > MAX_PATTERN_BYTES {
                    truncated = true;
                    return false;
                }
                total_bytes = next_bytes;
                values.push(value);
                true
            };
            for secret in secrets {
                let source = secret.expose();
                if source.is_empty() {
                    continue;
                }
                let Some(next_raw_bytes) = raw_bytes.checked_add(source.len()) else {
                    truncated = true;
                    break;
                };
                if raw.len() == MAX_PATTERNS || next_raw_bytes > MAX_RAW_BYTES {
                    truncated = true;
                    break;
                }
                raw_bytes = next_raw_bytes;
                raw.push(source.to_vec());
                let mut frontier = vec![source.to_vec()];
                for _ in 0..ENCODING_PASSES {
                    let mut next = Vec::new();
                    for item in frontier {
                        if !add(item.clone()) {
                            break;
                        }
                        for encoded in [
                            percent_encode(&item, false, true),
                            percent_encode(&item, true, true),
                            json_byte_escape(&item, true),
                            json_byte_escape(&item, false),
                            base64(&item, false, true),
                            base64(&item, false, false),
                            base64(&item, true, true),
                            base64(&item, true, false),
                        ] {
                            if add(encoded.clone()) {
                                next.push(encoded);
                            }
                        }
                    }
                    frontier = next;
                    if frontier.is_empty() {
                        break;
                    }
                }
            }
        }
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        let matcher = (!truncated && !values.is_empty())
            .then(|| build_matcher(&values))
            .transpose()
            .unwrap_or_else(|_| {
                truncated = true;
                None
            });
        let stream_matcher = (!truncated && !values.is_empty())
            .then(|| build_stream_matcher(&values))
            .transpose()
            .unwrap_or_else(|_| {
                truncated = true;
                None
            });
        let raw_stream_matcher = (!truncated && !raw.is_empty())
            .then(|| build_stream_matcher(&raw))
            .transpose()
            .unwrap_or_else(|_| {
                truncated = true;
                None
            });
        Self {
            values,
            raw,
            matcher,
            stream_matcher,
            raw_stream_matcher,
            truncated,
        }
    }
}

fn build_matcher(patterns: &[Vec<u8>]) -> Result<AhoCorasick, aho_corasick::BuildError> {
    AhoCorasickBuilder::new()
        .ascii_case_insensitive(true)
        .match_kind(MatchKind::LeftmostLongest)
        .build(patterns)
}

fn build_stream_matcher(patterns: &[Vec<u8>]) -> Result<NFA, aho_corasick::BuildError> {
    NFA::builder()
        .ascii_case_insensitive(true)
        .match_kind(MatchKind::Standard)
        .build(patterns)
}

impl Drop for SecretPatterns {
    fn drop(&mut self) {
        for value in &mut self.values {
            value.fill(0);
            std::hint::black_box(value);
        }
        for value in &mut self.raw {
            value.fill(0);
            std::hint::black_box(value);
        }
    }
}

fn redact_patterns(value: &str, patterns: &SecretPatterns) -> String {
    if patterns.truncated {
        return REDACTED.to_owned();
    }
    let Some(matcher) = &patterns.matcher else {
        return value.to_owned();
    };
    let input = value.as_bytes();
    let mut output = Vec::with_capacity(input.len());
    let mut offset = 0_usize;
    for matched in matcher.find_iter(input) {
        output.extend_from_slice(&input[offset..matched.start()]);
        output.extend_from_slice(REDACTED.as_bytes());
        offset = matched.end();
    }
    output.extend_from_slice(&input[offset..]);
    String::from_utf8(output).expect("redacting UTF-8 preserves UTF-8")
}

fn redact_url_fields(value: &str) -> String {
    let authority_start = value.find("://").map_or(0, |index| index + 3);
    let authority_end = value[authority_start..]
        .find(['/', '?', '#'])
        .map_or(value.len(), |index| authority_start + index);
    let mut output = String::with_capacity(value.len());
    output.push_str(&value[..authority_start]);
    if let Some(at) = value[authority_start..authority_end].rfind('@') {
        output.push_str(REDACTED);
        output.push('@');
        output.push_str(&value[authority_start + at + 1..authority_end]);
    } else {
        output.push_str(&value[authority_start..authority_end]);
    }

    let Some(question) = value[authority_end..].find('?').map(|i| authority_end + i) else {
        output.push_str(&value[authority_end..]);
        return output;
    };
    output.push_str(&value[authority_end..=question]);
    let query_end = value[question + 1..]
        .find('#')
        .map_or(value.len(), |index| question + 1 + index);
    for (index, field) in value[question + 1..query_end].split('&').enumerate() {
        if index != 0 {
            output.push('&');
        }
        let (name, separator, field_value) = field.find('=').map_or((field, "", ""), |equals| {
            (&field[..equals], "=", &field[equals + 1..])
        });
        output.push_str(name);
        output.push_str(separator);
        if classify_field(&percent_decode(name)) == DataClass::Secret {
            output.push_str(REDACTED);
        } else {
            output.push_str(field_value);
        }
    }
    output.push_str(&value[query_end..]);
    output
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            output.push(high << 4 | low);
            index += 3;
        } else {
            output.push(if bytes[index] == b'+' {
                b' '
            } else {
                bytes[index]
            });
            index += 1;
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn percent_encode(source: &[u8], all: bool, uppercase: bool) -> Vec<u8> {
    let hex = if uppercase {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut output = Vec::with_capacity(source.len() * 3);
    for &byte in source {
        if !all && (byte.is_ascii_alphanumeric() || b"-._~".contains(&byte)) {
            output.push(byte);
        } else {
            output.extend_from_slice(&[b'%', hex[(byte >> 4) as usize], hex[(byte & 15) as usize]]);
        }
    }
    output
}

fn json_byte_escape(source: &[u8], uppercase: bool) -> Vec<u8> {
    let hex = if uppercase {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut output = Vec::with_capacity(source.len().saturating_mul(6));
    for &byte in source {
        output.extend_from_slice(&[
            b'\\',
            b'u',
            b'0',
            b'0',
            hex[(byte >> 4) as usize],
            hex[(byte & 15) as usize],
        ]);
    }
    output
}

fn base64(source: &[u8], url_safe: bool, padded: bool) -> Vec<u8> {
    let alphabet = if url_safe {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let mut output = Vec::with_capacity(source.len().div_ceil(3) * 4);
    for chunk in source.chunks(3) {
        let bits = u32::from(chunk[0]) << 16
            | u32::from(*chunk.get(1).unwrap_or(&0)) << 8
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(alphabet[((bits >> 18) & 63) as usize]);
        output.push(alphabet[((bits >> 12) & 63) as usize]);
        if chunk.len() > 1 {
            output.push(alphabet[((bits >> 6) & 63) as usize]);
        } else if padded {
            output.push(b'=');
        }
        if chunk.len() > 2 {
            output.push(alphabet[(bits & 63) as usize]);
        } else if padded {
            output.push(b'=');
        }
    }
    output
}

fn write_json_string(output: &mut String, value: &str) {
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                use fmt::Write;
                write!(output, "\\u{:04x}", character as u32).unwrap();
            }
            character => output.push(character),
        }
    }
    output.push('"');
}

#[cfg(test)]
mod scanner_tests {
    use super::*;

    #[test]
    fn scanner_finds_split_binary_url_and_base64_canaries() {
        let secret = SecretLease::new([0, 1, 2, 0xfe, 0xff]);
        let redactor = CaptureRedactor::new(std::slice::from_ref(&secret));
        for encoded in [
            secret.expose().to_vec(),
            percent_encode(secret.expose(), true, true),
            base64(secret.expose(), false, true),
        ] {
            let mut scanner = redactor.scanner();
            for byte in encoded {
                scanner.push(&[byte]);
            }
            assert!(scanner.found());
        }
    }

    #[test]
    fn scanner_finds_mixed_case_nested_percent_and_json_encodings() {
        let secret = SecretLease::new(b"credential-canary".to_vec());
        let redactor = CaptureRedactor::new(std::slice::from_ref(&secret));
        for encoded in [
            b"%2563%2572%2565%2564%2565%256E%2574%2569%2561%256c%252D%2563%2561%256E%2561%2572%2579".as_slice(),
            b"\\u0063\\u0072\\u0065\\u0064\\u0065\\u006e\\u0074\\u0069\\u0061\\u006c\\u002d\\u0063\\u0061\\u006e\\u0061\\u0072\\u0079".as_slice(),
        ] {
            let mut scanner = redactor.scanner();
            for chunk in encoded.chunks(1) {
                scanner.push(chunk);
            }
            assert!(scanner.found(), "missed {}", String::from_utf8_lossy(encoded));
        }
    }

    #[test]
    fn scanner_finds_partial_mixed_and_four_level_percent_encodings() {
        let secret = SecretLease::new(b"credential-canary".to_vec());
        let redactor = CaptureRedactor::new(std::slice::from_ref(&secret));
        for encoded in [
            b"cred%65ntial%2dcanary".as_slice(),
            b"%25252563redential-canary".as_slice(),
        ] {
            let mut scanner = redactor.scanner();
            for chunk in encoded.chunks(1) {
                scanner.push(chunk);
            }
            assert!(
                scanner.found(),
                "missed {}",
                String::from_utf8_lossy(encoded)
            );
        }
    }
}
