use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, OnceLock},
};

use aho_corasick::{
    AhoCorasick, AhoCorasickBuilder, Anchored, MatchKind,
    automaton::{Automaton, StateID},
    nfa::contiguous::NFA,
};
use unicode_normalization::UnicodeNormalization;

use crate::domain::{
    lifecycle::ProcessClaim,
    secret::{DataClass, REDACTED, SecretLease, classify_field, classify_header},
};

pub(crate) const MAX_STREAM_HOLDBACK: usize = 64 * 1024;
const MAX_REDACTION_PASSES: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureBoundary {
    Prompt,
    Event,
    Artifact,
    Log,
    Trace,
    CompositionInput,
    TerminalMetadata,
    WorkspaceMetadata,
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
    shared_secrets: &'a [Arc<SecretLease>],
    provenance: Option<SanitizerProvenance>,
    prepared_patterns: Option<&'a SecretPatterns>,
    prepared_text_patterns: Option<&'a SecretPatterns>,
    owned_patterns: Option<Arc<SecretPatterns>>,
    owned_text_patterns: Option<Arc<SecretPatterns>>,
    patterns: OnceLock<Box<SecretPatterns>>,
    text_patterns: OnceLock<Box<SecretPatterns>>,
}

impl<'a> CaptureRedactor<'a> {
    pub const fn new(secrets: &'a [SecretLease]) -> Self {
        Self {
            secrets,
            shared_secrets: &[],
            provenance: None,
            prepared_patterns: None,
            prepared_text_patterns: None,
            owned_patterns: None,
            owned_text_patterns: None,
            patterns: OnceLock::new(),
            text_patterns: OnceLock::new(),
        }
    }

    pub const fn from_shared(secrets: &'a [Arc<SecretLease>]) -> Self {
        Self {
            secrets: &[],
            shared_secrets: secrets,
            provenance: None,
            prepared_patterns: None,
            prepared_text_patterns: None,
            owned_patterns: None,
            owned_text_patterns: None,
            patterns: OnceLock::new(),
            text_patterns: OnceLock::new(),
        }
    }

    pub(crate) fn from_owned_prepared(
        patterns: Arc<SecretPatterns>,
        text_patterns: Arc<SecretPatterns>,
    ) -> CaptureRedactor<'static> {
        CaptureRedactor {
            secrets: &[],
            shared_secrets: &[],
            provenance: None,
            prepared_patterns: None,
            prepared_text_patterns: None,
            owned_patterns: Some(patterns),
            owned_text_patterns: Some(text_patterns),
            patterns: OnceLock::new(),
            text_patterns: OnceLock::new(),
        }
    }

    pub(crate) const fn process_bound(
        secrets: &'a [SecretLease],
        provenance: SanitizerProvenance,
    ) -> Self {
        Self {
            secrets,
            shared_secrets: &[],
            provenance: Some(provenance),
            prepared_patterns: None,
            prepared_text_patterns: None,
            owned_patterns: None,
            owned_text_patterns: None,
            patterns: OnceLock::new(),
            text_patterns: OnceLock::new(),
        }
    }

    pub(crate) const fn combined_process_bound(
        secrets: &'a [SecretLease],
        shared_secrets: &'a [Arc<SecretLease>],
        provenance: SanitizerProvenance,
    ) -> Self {
        Self {
            secrets,
            shared_secrets,
            provenance: Some(provenance),
            prepared_patterns: None,
            prepared_text_patterns: None,
            owned_patterns: None,
            owned_text_patterns: None,
            patterns: OnceLock::new(),
            text_patterns: OnceLock::new(),
        }
    }

    pub fn redact_text(&self, _boundary: CaptureBoundary, value: &str) -> String {
        let patterns = self.text_patterns();
        if patterns.truncated {
            return fail_closed_marker(patterns);
        }
        let mut redacted = value.to_owned();
        for _ in 0..MAX_REDACTION_PASSES {
            let next = redact_patterns(&redacted, patterns);
            if next != redacted {
                redacted = next;
                continue;
            }
            if !patterns.matches(redacted.as_bytes()) {
                return redacted;
            }
            redacted = fail_closed_marker(patterns);
        }
        if patterns.matches(redacted.as_bytes()) {
            String::new()
        } else {
            redacted
        }
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
            patterns: self.patterns().clone(),
            pending: Vec::new(),
            sanitized: Vec::new(),
            provenance: self.provenance,
            finished: false,
            potential_secret: false,
            saturated: false,
            custody: None,
            fixed_patterns: None,
        }
    }

    pub fn scanner(&self) -> SensitiveDataScanner {
        SensitiveDataScanner::new(self.patterns().clone())
    }

    pub fn stream_holdback(&self) -> usize {
        self.patterns().holdback()
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
            StructuredValue::String(value)
                if boundary == CaptureBoundary::Event && trusted_event_literal(value) =>
            {
                StructuredValue::String(value.clone())
            }
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

    pub(crate) fn patterns(&self) -> &SecretPatterns {
        if let Some(patterns) = &self.owned_patterns {
            return patterns;
        }
        if let Some(patterns) = self.prepared_patterns {
            return patterns;
        }
        self.patterns.get_or_init(|| {
            Box::new(SecretPatterns::new(
                self.secrets
                    .iter()
                    .chain(self.shared_secrets.iter().map(Arc::as_ref)),
            ))
        })
    }

    fn text_patterns(&self) -> &SecretPatterns {
        if let Some(patterns) = &self.owned_text_patterns {
            return patterns;
        }
        if let Some(patterns) = self.prepared_text_patterns {
            return patterns;
        }
        self.text_patterns.get_or_init(|| {
            Box::new(SecretPatterns::new_text(
                self.secrets
                    .iter()
                    .chain(self.shared_secrets.iter().map(Arc::as_ref)),
            ))
        })
    }
}

pub(crate) fn trusted_event_identifier_value(value: &str) -> bool {
    const PREFIXES: [&str; 22] = [
        "principal",
        "project",
        "thread",
        "run",
        "attempt",
        "turn",
        "model_call",
        "tool_call",
        "task",
        "agent_link",
        "external_task",
        "daemon_service",
        "workspace",
        "process",
        "terminal",
        "approval",
        "checkpoint",
        "artifact",
        "experiment",
        "mcp_callback",
        "cmd",
        "evt",
    ];
    PREFIXES.iter().any(|prefix| {
        value
            .strip_prefix(prefix)
            .and_then(|value| value.strip_prefix('_'))
            .is_some_and(|payload| {
                payload.len() == 26
                    && payload.bytes().enumerate().all(|(index, byte)| {
                        matches!(byte, b'0'..=b'9' | b'a'..=b'h' | b'j'..=b'k' | b'm'..=b'n' | b'p'..=b't' | b'v'..=b'z')
                            && (index != 0 || byte <= b'7')
                    })
            })
    })
}

pub(crate) fn trusted_event_literal(value: &str) -> bool {
    trusted_event_identifier_value(value) || value.starts_with("surface-pointer:")
}

pub struct SensitiveDataScanner {
    patterns: SecretPatterns,
    state: Option<StateID>,
    decoded_states: Vec<StateID>,
    decoders: [PercentDecoder; 4],
    base64_state: Option<StateID>,
    base64_decoder: Base64Decoder,
    evidence: Vec<u8>,
    work: usize,
    saturated: bool,
    found: bool,
    custody: Option<(crate::domain::secret::SecretCustody, u64, usize)>,
    recent: Vec<u8>,
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
        let base64_state = patterns
            .raw_stream_matcher
            .as_ref()
            .and_then(|matcher| matcher.start_state(Anchored::No).ok());
        Self {
            found: patterns.truncated
                || (patterns.stream_matcher.is_some() && state.is_none())
                || (patterns.raw_stream_matcher.is_some()
                    && (decoded_states.is_empty() || base64_state.is_none())),
            patterns,
            state,
            decoded_states,
            decoders: std::array::from_fn(|_| PercentDecoder::default()),
            base64_state,
            base64_decoder: Base64Decoder::default(),
            evidence: Vec::new(),
            work: 0,
            saturated: false,
            custody: None,
            recent: Vec::new(),
        }
    }

    pub(crate) fn with_custody(
        mut self,
        custody: crate::domain::secret::SecretCustody,
        revision: u64,
        leases: usize,
    ) -> Self {
        self.custody = Some((custody, revision, leases));
        self
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.refresh_custody();
        self.push_current(bytes);
        if self.custody.is_some() {
            self.recent.extend_from_slice(bytes);
            if self.recent.len() > MAX_STREAM_HOLDBACK {
                let drain = self.recent.len() - MAX_STREAM_HOLDBACK;
                self.recent[..drain].fill(0);
                self.recent.drain(..drain);
            }
        }
    }

    fn push_current(&mut self, bytes: &[u8]) {
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
            self.scan_base64_byte(byte);
            if self.found {
                break;
            }
        }
        self.state = Some(state);
        if !self.found {
            self.scan_reconstructable(bytes);
        }
    }

    fn refresh_custody(&mut self) {
        let Some((custody, revision, leases)) = self.custody.clone() else {
            return;
        };
        let current = custody.revision();
        if current == revision {
            return;
        }
        let current_leases = custody.leases().len();
        let recent = self.recent.clone();
        let mut refreshed = custody.redactor().capture().scanner();
        refreshed.push_current(&recent);
        if current_leases <= leases {
            refreshed.found = true;
        }
        refreshed.custody = Some((custody, current, current_leases));
        refreshed.recent = recent;
        *self = refreshed;
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
            base64_state: self.base64_state,
            base64_decoder: self.base64_decoder.clone(),
            evidence: self.evidence.clone(),
            work: self.work,
            saturated: self.saturated,
            found: self.found,
            custody: self.custody.clone(),
            recent: self.recent.clone(),
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
        self.base64_state = self
            .patterns
            .raw_stream_matcher
            .as_ref()
            .and_then(|matcher| matcher.start_state(Anchored::No).ok());
        self.decoders = std::array::from_fn(|_| PercentDecoder::default());
        self.base64_decoder = Base64Decoder::default();
        self.evidence.clear();
        self.work = 0;
        self.saturated = false;
        self.found = self.patterns.truncated
            || (self.patterns.stream_matcher.is_some() && self.state.is_none())
            || (self.patterns.raw_stream_matcher.is_some()
                && (self.decoded_states.is_empty() || self.base64_state.is_none()));
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

    fn scan_base64_byte(&mut self, byte: u8) {
        let Some(decoded) = self.base64_decoder.push(byte) else {
            return;
        };
        let (Some(matcher), Some(state)) = (&self.patterns.raw_stream_matcher, self.base64_state)
        else {
            return;
        };
        let state = matcher.next_state(Anchored::No, state, decoded);
        self.base64_state = Some(state);
        self.found = matcher.is_match(state);
    }

    fn scan_reconstructable(&mut self, bytes: &[u8]) {
        const MAX_EVIDENCE: usize = 1024 * 1024;
        const MAX_WORK: usize = 64 * 1024 * 1024;
        const CHUNK_BYTES: usize = 64 * 1024;
        for bytes in bytes.chunks(CHUNK_BYTES) {
            if self.evidence.len().saturating_add(bytes.len()) > MAX_EVIDENCE {
                self.fail_closed();
                return;
            }
            self.evidence.extend_from_slice(bytes);
            self.work = self.work.saturating_add(self.evidence.len());
            if self.work > MAX_WORK {
                self.fail_closed();
                return;
            }
            if reconstructable_match(&self.patterns.raw, &self.evidence) {
                self.found = true;
                return;
            }
            if !possible_prefix(&self.evidence, &self.patterns.values, &self.patterns.raw)
                && !normalization_prefix_pending(&self.evidence)
            {
                self.evidence.fill(0);
                self.evidence.clear();
                continue;
            }
            let holdback = self.patterns.holdback();
            if self.evidence.len() > holdback {
                let safe = self.evidence.len() - holdback;
                if !possible_prefix(
                    &self.evidence[safe..],
                    &self.patterns.values,
                    &self.patterns.raw,
                ) {
                    self.fail_closed();
                    return;
                }
                self.evidence[..safe].fill(0);
                self.evidence.drain(..safe);
            }
        }
    }

    fn fail_closed(&mut self) {
        self.evidence.fill(0);
        self.evidence.clear();
        self.saturated = true;
        self.found = true;
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

#[derive(Clone, Default)]
struct Base64Decoder {
    previous: u8,
    position: u8,
}

impl Base64Decoder {
    fn push(&mut self, byte: u8) -> Option<u8> {
        if byte.is_ascii_whitespace() || byte.is_ascii_control() {
            return None;
        }
        let Some(value) = base64_value(byte) else {
            self.position = 0;
            self.previous = 0;
            return None;
        };
        self.position += 1;
        let decoded = match self.position {
            1 => None,
            2 => Some(self.previous << 2 | value >> 4),
            3 => Some(self.previous << 4 | value >> 2),
            4 => Some(self.previous << 6 | value),
            _ => unreachable!("base64 decoder position is bounded"),
        };
        self.previous = value;
        if self.position == 4 {
            self.position = 0;
        }
        decoded
    }
}

impl Drop for Base64Decoder {
    fn drop(&mut self) {
        self.previous = 0;
        self.position = 0;
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
    potential_secret: bool,
    saturated: bool,
    custody: Option<(crate::domain::secret::SecretCustody, u64, usize)>,
    fixed_patterns: Option<SecretPatterns>,
}

impl SanitizedCapture {
    pub(crate) fn with_custody(
        mut self,
        custody: crate::domain::secret::SecretCustody,
        revision: u64,
        leases: usize,
    ) -> Self {
        self.custody = Some((custody, revision, leases));
        self
    }

    pub(crate) fn with_custody_and_fixed_patterns(
        mut self,
        custody: crate::domain::secret::SecretCustody,
        revision: u64,
        leases: usize,
        fixed_patterns: SecretPatterns,
    ) -> Self {
        self.custody = Some((custody, revision, leases));
        self.fixed_patterns = Some(fixed_patterns);
        self
    }

    pub const fn boundary(&self) -> CaptureBoundary {
        self.boundary
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<(), SanitizedCaptureError> {
        self.refresh_custody();
        if self.finished {
            return Err(SanitizedCaptureError::AlreadyFinished);
        }
        if self.saturated {
            return Ok(());
        }
        if self.patterns.truncated {
            self.saturate();
            return Ok(());
        }
        if self.patterns.values.is_empty() && self.patterns.raw.is_empty() && self.custody.is_none()
        {
            self.sanitized.extend_from_slice(chunk);
            return Ok(());
        }
        if self.patterns.values.is_empty() && self.patterns.raw.is_empty() {
            self.pending.extend_from_slice(chunk);
            if self.pending.len() > MAX_STREAM_HOLDBACK {
                let ready = self.pending.len() - MAX_STREAM_HOLDBACK;
                self.sanitized.extend_from_slice(&self.pending[..ready]);
                self.pending[..ready].fill(0);
                self.pending.drain(..ready);
            }
            return Ok(());
        }
        for &byte in chunk {
            self.pending.push(byte);
            if self.patterns.matches(&self.pending) {
                self.redact_pending();
                continue;
            }
            let candidate = self.patterns.candidate_start(&self.pending);
            let (candidate, potential_secret) = if candidate < self.pending.len() {
                (candidate, true)
            } else if let Some(start) = speculative_base64_start(&self.pending) {
                (start, false)
            } else if let Some(start) = speculative_utf8_start(&self.pending) {
                (start, false)
            } else {
                (self.pending.len(), false)
            };
            if candidate != 0 {
                self.sanitized.extend_from_slice(&self.pending[..candidate]);
                self.pending[..candidate].fill(0);
                self.pending.drain(..candidate);
            }
            self.potential_secret = potential_secret;
            if self.pending.len()
                > if self.custody.is_some() {
                    MAX_STREAM_HOLDBACK
                } else {
                    self.patterns.holdback()
                }
            {
                self.saturate();
                return Ok(());
            }
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), SanitizedCaptureError> {
        self.refresh_custody();
        if self.finished {
            return Err(SanitizedCaptureError::AlreadyFinished);
        }
        if !self.pending.is_empty() {
            self.sanitized.extend_from_slice(&self.pending);
            self.pending.fill(0);
            self.pending.clear();
            self.potential_secret = false;
        }
        self.settle_complete_output();
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
    pub fn take_ready(&mut self) -> Option<Self> {
        self.refresh_custody();
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
            potential_secret: false,
            saturated: false,
            custody: None,
            fixed_patterns: None,
        })
    }

    fn refresh_custody(&mut self) {
        let Some((custody, revision, leases)) = self.custody.clone() else {
            return;
        };
        let (current, current_shared) = custody.leases_with_revision();
        if current == revision {
            return;
        }
        let current_leases = current_shared.len();
        self.custody = Some((custody.clone(), current, current_leases));
        if current_leases <= leases {
            self.saturate();
        } else {
            let shared = SecretPatterns::from_shared(&current_shared);
            self.patterns = self
                .fixed_patterns
                .as_ref()
                .map_or(shared.clone(), |fixed| fixed.merged(&shared));
            let mut buffered = std::mem::take(&mut self.sanitized);
            buffered.append(&mut self.pending);
            self.potential_secret = false;
            let _ = self.push(&buffered);
            buffered.fill(0);
        }
    }

    fn redact_pending(&mut self) {
        self.pending.fill(0);
        self.pending.clear();
        self.potential_secret = false;
        if !self.sanitized.ends_with(REDACTED.as_bytes()) {
            self.sanitized.extend_from_slice(REDACTED.as_bytes());
        }
        self.settle_stream_marker();
    }

    fn settle_stream_marker(&mut self) {
        for _ in 0..MAX_REDACTION_PASSES {
            if self.patterns.matches(&self.sanitized) {
                self.sanitized.fill(0);
                self.sanitized.clear();
                if self.patterns.matches(REDACTED.as_bytes()) {
                    self.saturated = true;
                    return;
                }
                self.sanitized.extend_from_slice(REDACTED.as_bytes());
                continue;
            }
            let candidate = self.patterns.candidate_start(&self.sanitized);
            if candidate < self.sanitized.len() {
                self.pending = self.sanitized.split_off(candidate);
                self.potential_secret = true;
            }
            return;
        }
        self.sanitized.fill(0);
        self.sanitized.clear();
        self.saturated = true;
    }

    fn settle_complete_output(&mut self) {
        for _ in 0..MAX_REDACTION_PASSES {
            if !self.patterns.matches(&self.sanitized) {
                return;
            }
            self.sanitized.fill(0);
            self.sanitized.clear();
            if self.patterns.matches(REDACTED.as_bytes()) {
                self.saturated = true;
                return;
            }
            self.sanitized.extend_from_slice(REDACTED.as_bytes());
        }
        self.sanitized.fill(0);
        self.sanitized.clear();
        self.saturated = true;
    }

    fn saturate(&mut self) {
        self.sanitized.fill(0);
        self.sanitized.clear();
        self.pending.fill(0);
        self.pending.clear();
        self.potential_secret = false;
        if !self.patterns.matches(REDACTED.as_bytes()) {
            self.sanitized.extend_from_slice(REDACTED.as_bytes());
        }
        self.saturated = true;
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
pub(crate) struct SecretPatterns {
    values: Vec<Vec<u8>>,
    raw: Vec<Vec<u8>>,
    matcher: Option<AhoCorasick>,
    stream_matcher: Option<NFA>,
    raw_stream_matcher: Option<NFA>,
    truncated: bool,
}

impl SecretPatterns {
    fn new<'a>(secrets: impl IntoIterator<Item = &'a SecretLease>) -> Self {
        Self::build(secrets, false)
    }

    fn new_text<'a>(secrets: impl IntoIterator<Item = &'a SecretLease>) -> Self {
        Self::build(secrets, true)
    }

    pub(crate) fn from_shared(secrets: &[Arc<SecretLease>]) -> Self {
        Self::new(secrets.iter().map(Arc::as_ref))
    }

    pub(crate) fn text_from_shared(secrets: &[Arc<SecretLease>]) -> Self {
        Self::new_text(secrets.iter().map(Arc::as_ref))
    }

    fn merged(&self, other: &Self) -> Self {
        if self.truncated || other.truncated {
            let mut patterns = Self::new(std::iter::empty());
            patterns.truncated = true;
            return patterns;
        }
        let leases = self
            .raw
            .iter()
            .chain(&other.raw)
            .map(|value| SecretLease::new(value.clone()))
            .collect::<Vec<_>>();
        Self::new(&leases)
    }

    fn build<'a>(secrets: impl IntoIterator<Item = &'a SecretLease>, text_only: bool) -> Self {
        const MAX_PATTERNS: usize = 4096;
        const MAX_PATTERN_BYTES: usize = 4 * 1024 * 1024;
        const MAX_RAW_BYTES: usize = 256 * 1024;
        const ENCODING_PASSES: usize = 1;
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
                if !text_only || std::str::from_utf8(source).is_ok() {
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
                }
                let mut sources = vec![source.to_vec()];
                if let Ok(text) = std::str::from_utf8(source) {
                    for normalized in [
                        text.nfd().collect::<String>().into_bytes(),
                        text.nfkc().collect::<String>().into_bytes(),
                    ] {
                        if !sources.contains(&normalized) {
                            sources.push(normalized);
                        }
                    }
                }
                let mut frontier = sources;
                for _ in 0..ENCODING_PASSES {
                    let mut next = Vec::new();
                    for item in frontier {
                        if (!text_only || std::str::from_utf8(&item).is_ok()) && !add(item.clone())
                        {
                            break;
                        }
                        for encoded in [
                            percent_encode(&item, false, true),
                            percent_encode(&item, true, true),
                            json_byte_escape(&item, true),
                            json_byte_escape(&item, false),
                            hex_encode(&item, false, false),
                            hex_encode(&item, true, false),
                            hex_encode(&item, false, true),
                            hex_encode(&item, true, true),
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

    fn holdback(&self) -> usize {
        let maximum = self.values.iter().map(Vec::len).max().unwrap_or(0);
        maximum
            .checked_add(256)
            .filter(|_| maximum != 0)
            .unwrap_or(0)
            .min(MAX_STREAM_HOLDBACK)
    }

    fn matches(&self, source: &[u8]) -> bool {
        reconstructable_match(&self.raw, source)
            || self
                .matcher
                .as_ref()
                .is_some_and(|matcher| matcher.is_match(source))
    }

    fn candidate_start(&self, source: &[u8]) -> usize {
        (0..source.len())
            .find(|&start| possible_prefix(&source[start..], &self.values, &self.raw))
            .unwrap_or(source.len())
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

fn fail_closed_marker(patterns: &SecretPatterns) -> String {
    if patterns.matches(REDACTED.as_bytes()) {
        String::new()
    } else {
        REDACTED.to_owned()
    }
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

fn hex_encode(source: &[u8], uppercase: bool, prefixed: bool) -> Vec<u8> {
    let hex = if uppercase {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut output = Vec::with_capacity(source.len().saturating_mul(2).saturating_add(2));
    if prefixed {
        output.extend_from_slice(b"0x");
    }
    for &byte in source {
        output.extend_from_slice(&[hex[(byte >> 4) as usize], hex[(byte & 15) as usize]]);
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

fn reconstructable_match(secrets: &[Vec<u8>], source: &[u8]) -> bool {
    match reconstruction_views(source) {
        Err(()) => true,
        Ok(views) => views.iter().any(|view| {
            secrets
                .iter()
                .any(|secret| contains_ascii_case_insensitive(view, secret))
        }),
    }
}

fn reconstruction_views(source: &[u8]) -> Result<Vec<Vec<u8>>, ()> {
    const MAX_VIEWS: usize = 32;
    let mut views = vec![source.to_vec()];
    let mut cursor = 0;
    while cursor < views.len() && views.len() < MAX_VIEWS {
        let json = decode_json_escapes(&views[cursor])?;
        let candidates = [
            Some(strip_interleaving(&views[cursor])),
            Some(decode_percent_all(&views[cursor])),
            json,
            Some(decode_hex_streams(&views[cursor])),
            Some(decode_base64_streams(&views[cursor])),
            Some(normalize_unicode(&views[cursor])),
        ];
        for candidate in candidates {
            let Some(candidate) = candidate else {
                continue;
            };
            if !candidate.is_empty()
                && candidate != views[cursor]
                && !views.iter().any(|existing| existing == &candidate)
            {
                views.push(candidate);
                if views.len() == MAX_VIEWS {
                    break;
                }
            }
        }
        cursor += 1;
    }
    Ok(views)
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

fn possible_prefix(source: &[u8], patterns: &[Vec<u8>], raw: &[Vec<u8>]) -> bool {
    let stripped = strip_interleaving(source);
    match reconstruction_views(source) {
        Err(()) => true,
        Ok(views) => views.iter().enumerate().any(|(view_index, view)| {
            (0..view.len()).any(|start| {
                let candidate = &view[start..];
                (view_index == 0
                    || candidate.len() >= 2
                    || (source.len() > stripped.len() && *view == stripped)
                    || source.iter().any(|byte| !byte.is_ascii()))
                    && patterns
                        .iter()
                        .chain(raw)
                        .any(|pattern| starts_with_ascii_case_insensitive(pattern, candidate))
            })
        }),
    }
}

fn normalization_prefix_pending(source: &[u8]) -> bool {
    source.len() <= 16
        && source
            .iter()
            .any(|byte| matches!(byte, b'%' | b'\\') || !byte.is_ascii())
}

fn speculative_base64_start(source: &[u8]) -> Option<usize> {
    let start = source
        .iter()
        .rposition(|byte| base64_value(*byte).is_none() && *byte != b'=')
        .map_or(0, |index| index + 1);
    let length = source.len() - start;
    (length != 0 && length <= 7).then_some(start)
}

fn speculative_utf8_start(source: &[u8]) -> Option<usize> {
    std::str::from_utf8(source).err().and_then(|error| {
        (error.error_len().is_none()
            && source[error.valid_up_to()..]
                .iter()
                .any(|byte| !byte.is_ascii()))
        .then_some(error.valid_up_to())
    })
}

fn starts_with_ascii_case_insensitive(value: &[u8], prefix: &[u8]) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn strip_interleaving(source: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(source.len());
    let mut index = 0;
    while index < source.len() {
        if source[index] == 0x1b && source.get(index + 1) == Some(&b'[') {
            index += 2;
            while index < source.len() {
                let byte = source[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        } else {
            let byte = source[index];
            index += 1;
            if !byte.is_ascii_control() && !byte.is_ascii_whitespace() {
                output.push(byte);
            }
        }
    }
    output
}

fn decode_percent_all(source: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(source.len());
    let mut index = 0;
    while index < source.len() {
        if source[index] == b'%'
            && let (Some(high), Some(low)) = (
                source.get(index + 1).copied().and_then(hex_value),
                source.get(index + 2).copied().and_then(hex_value),
            )
        {
            output.push(high << 4 | low);
            index += 3;
        } else {
            output.push(source[index]);
            index += 1;
        }
    }
    output
}

fn decode_json_escapes(source: &[u8]) -> Result<Option<Vec<u8>>, ()> {
    let Ok(text) = std::str::from_utf8(source) else {
        return Ok(Some(Vec::new()));
    };
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\\' {
            output.push(character);
            continue;
        }
        match chars.next() {
            Some('u') => {
                let digits = chars.by_ref().take(4).collect::<String>();
                if digits.len() != 4 {
                    return Ok(None);
                }
                let unit = u16::from_str_radix(&digits, 16).map_err(|_| ())?;
                let scalar = if (0xd800..=0xdbff).contains(&unit) {
                    match chars.next() {
                        None => return Ok(None),
                        Some('\\') => {}
                        Some(_) => return Err(()),
                    }
                    match chars.next() {
                        None => return Ok(None),
                        Some('u') => {}
                        Some(_) => return Err(()),
                    }
                    let low = chars.by_ref().take(4).collect::<String>();
                    if low.len() != 4 {
                        return Ok(None);
                    }
                    let low = u16::from_str_radix(&low, 16).map_err(|_| ())?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return Err(());
                    }
                    0x1_0000 + ((u32::from(unit) - 0xd800) << 10) + (u32::from(low) - 0xdc00)
                } else if (0xdc00..=0xdfff).contains(&unit) {
                    return Err(());
                } else {
                    u32::from(unit)
                };
                output.push(char::from_u32(scalar).ok_or(())?);
            }
            Some('n') => output.push('\n'),
            Some('r') => output.push('\r'),
            Some('t') => output.push('\t'),
            Some('b') => output.push('\u{8}'),
            Some('f') => output.push('\u{c}'),
            Some(value @ ('"' | '\\' | '/')) => output.push(value),
            Some(value) => {
                output.push('\\');
                output.push(value);
            }
            None => return Ok(None),
        }
    }
    Ok(Some(output.into_bytes()))
}

fn decode_hex_streams(source: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut high = None;
    for byte in source.iter().copied() {
        if let Some(value) = hex_value(byte) {
            if let Some(first) = high.take() {
                output.push(first << 4 | value);
            } else {
                high = Some(value);
            }
        } else {
            high = None;
        }
    }
    output
}

fn decode_base64_streams(source: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    for token in source.split(|byte| {
        !byte.is_ascii_alphanumeric() && !matches!(byte, b'+' | b'/' | b'-' | b'_' | b'=')
    }) {
        if token.len() < 4 {
            continue;
        }
        let mut quartet = [0_u8; 4];
        let mut count = 0;
        for byte in token.iter().copied().filter(|byte| *byte != b'=') {
            let Some(value) = base64_value(byte) else {
                count = 0;
                continue;
            };
            quartet[count] = value;
            count += 1;
            if count == 4 {
                output.extend_from_slice(&[
                    quartet[0] << 2 | quartet[1] >> 4,
                    quartet[1] << 4 | quartet[2] >> 2,
                    quartet[2] << 6 | quartet[3],
                ]);
                count = 0;
            }
        }
        if count == 2 {
            output.push(quartet[0] << 2 | quartet[1] >> 4);
        } else if count == 3 {
            output.extend_from_slice(&[
                quartet[0] << 2 | quartet[1] >> 4,
                quartet[1] << 4 | quartet[2] >> 2,
            ]);
        }
    }
    output
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

fn normalize_unicode(source: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(source) else {
        return Vec::new();
    };
    text.nfkc()
        .map(|character| match character {
            'а' => 'a',
            'е' => 'e',
            'і' | 'ӏ' => 'i',
            'о' => 'o',
            'р' => 'p',
            'с' => 'c',
            'х' => 'x',
            'у' => 'y',
            other => other,
        })
        .collect::<String>()
        .into_bytes()
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
    fn capture_preserves_the_byte_after_a_split_secret() {
        let secret = SecretLease::new(b"KIT_TERMINAL_CANARY_714_t6N4".to_vec());
        let redactor = CaptureRedactor::new(std::slice::from_ref(&secret));
        let input = b"KIT_TERMINAL_CANARY_714_t6N4 REQUEST_UNAUTHORIZED";
        let mut capture = redactor.start(CaptureBoundary::Artifact);
        for chunk in input.chunks(3) {
            capture.push(chunk).unwrap();
        }
        capture.finish().unwrap();
        assert_eq!(capture.bytes().unwrap(), b"[REDACTED] REQUEST_UNAUTHORIZED");
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
        let unicode_secret = SecretLease::new("🔐-canary".as_bytes().to_vec());
        let redactor = CaptureRedactor::new(std::slice::from_ref(&unicode_secret));
        let mut scanner = redactor.scanner();
        for byte in br#"\ud83d\udd10-canary"# {
            scanner.push(&[*byte]);
        }
        assert!(scanner.found());

        for invalid in [br#"\ud83dpublic"#.as_slice(), br#"\udc00"#.as_slice()] {
            let mut scanner = redactor.scanner();
            scanner.push(invalid);
            assert!(scanner.found());
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

    #[test]
    fn direct_redaction_rescans_mixed_reconstructable_output() {
        let secret = SecretLease::new(b"credential-canary".to_vec());
        let redactor = CaptureRedactor::new(std::slice::from_ref(&secret));
        let mixed = "credential-canary 63726564656e7469616c2d63616e617279";
        assert_eq!(
            redactor.redact_text(CaptureBoundary::Artifact, mixed),
            "[REDACTED] [REDACTED]"
        );

        let mut capture = redactor.start(CaptureBoundary::Artifact);
        for chunk in mixed.as_bytes().chunks(3) {
            capture.push(chunk).unwrap();
        }
        capture.finish().unwrap();
        assert_eq!(
            capture.bytes().unwrap(),
            b"[REDACTED] [REDACTED]".as_slice()
        );
    }

    #[test]
    fn scanner_finds_prefixed_spaced_hex_base64_ansi_and_confusable_encodings() {
        let secret = SecretLease::new(b"credential-canary".to_vec());
        let redactor = CaptureRedactor::new(std::slice::from_ref(&secret));
        let prefixed = base64(b"xxcredential-canary", false, false);
        let spaced = base64(secret.expose(), true, false)
            .into_iter()
            .flat_map(|byte| [byte, b' '])
            .collect::<Vec<_>>();
        for encoded in [
            b"63726564656e7469616c2d63616e617279".as_slice(),
            prefixed.as_slice(),
            spaced.as_slice(),
            b"c\x1b[31mr\x00e\x1b[0mdential-canary".as_slice(),
            "сredential-canary".as_bytes(),
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
            assert_eq!(
                redactor
                    .sanitize(CaptureBoundary::Artifact, encoded)
                    .bytes()
                    .unwrap()
                    .trim_ascii(),
                REDACTED.as_bytes()
            );
        }

        let normalized = SecretLease::new("café-canary".as_bytes().to_vec());
        let normalized_redactor = CaptureRedactor::new(std::slice::from_ref(&normalized));
        let mut scanner = normalized_redactor.scanner();
        for chunk in "cafe\u{301}-canary".as_bytes().chunks(1) {
            scanner.push(chunk);
        }
        assert!(scanner.found(), "missed canonically equivalent Unicode");
    }

    #[test]
    fn streaming_padding_over_holdback_is_terminal_and_emits_one_marker() {
        let secret = SecretLease::new(b"CANARY".to_vec());
        let redactor = CaptureRedactor::new(std::slice::from_ref(&secret));
        let holdback = redactor.stream_holdback();
        let mut capture = redactor.start(CaptureBoundary::TerminalMetadata);
        capture.push(b"C").unwrap();
        capture.push(&vec![b' '; holdback + 1]).unwrap();
        let failed_closed = capture.take_ready().unwrap();
        let bytes = failed_closed.bytes().unwrap();
        assert_eq!(bytes, REDACTED.as_bytes());
        let mut scanner = redactor.scanner();
        scanner.push(bytes);
        assert!(!scanner.found());

        for chunk in [b"CANARY".as_slice(), b"Password: ", b"again"] {
            capture.push(chunk).unwrap();
            assert!(capture.take_ready().is_none());
        }
        capture.finish().unwrap();
        assert_eq!(capture.bytes().unwrap(), b"");
    }

    #[test]
    fn standalone_fragments_and_common_suffixes_are_byte_identical() {
        let secret = SecretLease::new(b"example-secret".to_vec());
        let redactor = CaptureRedactor::new(std::slice::from_ref(&secret));
        for value in ["thread.archive", "example-", "secret"] {
            assert_eq!(redactor.redact_text(CaptureBoundary::Event, value), value);
            assert_eq!(
                redactor
                    .sanitize(CaptureBoundary::WorkspaceMetadata, value.as_bytes())
                    .bytes()
                    .unwrap(),
                value.as_bytes()
            );
        }
    }

    #[test]
    fn streaming_scanner_finds_every_secret_representation_split_position() {
        let secret = SecretLease::new(b"cross-frame".to_vec());
        let redactor = CaptureRedactor::new(std::slice::from_ref(&secret));
        for representation in [
            b"cross-frame".as_slice(),
            b"%63%72%6F%73%73%2D%66%72%61%6D%65".as_slice(),
            b"63726f73732d6672616d65".as_slice(),
            b"Y3Jvc3MtZnJhbWU=".as_slice(),
        ] {
            for split in 1..representation.len() {
                let mut scanner = redactor.scanner();
                scanner.push(&representation[..split]);
                scanner.push(&representation[split..]);
                assert!(scanner.found(), "missed split {split}");
            }
        }
    }

    #[test]
    fn scanner_saturation_is_terminal_until_reset() {
        let secret = SecretLease::new(b"CANARY".to_vec());
        let redactor = CaptureRedactor::new(std::slice::from_ref(&secret));
        let mut scanner = redactor.scanner();

        scanner.push(b"C");
        for _ in 0..128 {
            scanner.push(&[b' '; 16 * 1024]);
            if scanner.saturated {
                break;
            }
        }
        assert!(scanner.saturated);
        assert!(scanner.found());

        for chunk in [b"CANARY".as_slice(), b"public after saturation", b"again"] {
            scanner.push(chunk);
            assert!(scanner.saturated);
            assert!(scanner.found());
            assert!(scanner.evidence.is_empty());
        }

        scanner.reset();
        assert!(!scanner.saturated);
        scanner.push(b"public after reset");
        assert!(!scanner.found());
    }

    #[test]
    fn scanner_accepts_a_valid_eight_mibibyte_stream() {
        let secret = SecretLease::new(b"CANARY".to_vec());
        let redactor = CaptureRedactor::new(std::slice::from_ref(&secret));
        let mut scanner = redactor.scanner();
        scanner.push(&vec![b'!'; 8 * 1024 * 1024]);
        assert!(!scanner.saturated);
        assert!(!scanner.found());
    }
}
