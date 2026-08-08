use std::{
    collections::BTreeMap,
    fmt,
    str::FromStr,
    sync::atomic::{Ordering, compiler_fence},
    sync::{Arc, OnceLock, RwLock, Weak},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub const REDACTED: &str = "[REDACTED]";
const MAX_REDACTION_PASSES: usize = 8;

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

#[derive(Clone, Default)]
pub struct SecretCustody {
    state: Arc<RwLock<SecretCustodyState>>,
}

type ActiveSecretLeases = BTreeMap<String, BTreeMap<String, Arc<SecretLease>>>;

#[derive(Default)]
struct SecretCustodyState {
    leases: ActiveSecretLeases,
    redactor: Option<Arc<SecretRedactor>>,
    revision: u64,
}

impl SecretCustody {
    pub fn new(leases: impl IntoIterator<Item = Arc<SecretLease>>) -> Self {
        Self::new_named(
            "project",
            leases
                .into_iter()
                .enumerate()
                .map(|(index, lease)| (format!("active-{index}"), lease)),
        )
    }

    pub fn new_named(
        owner: impl Into<String>,
        leases: impl IntoIterator<Item = (String, Arc<SecretLease>)>,
    ) -> Self {
        let custody = Self::default();
        custody.replace_owner(owner, leases);
        custody
    }

    pub fn replace_owner(
        &self,
        owner: impl Into<String>,
        leases: impl IntoIterator<Item = (String, Arc<SecretLease>)>,
    ) {
        let owner = owner.into();
        let leases = leases.into_iter().collect::<BTreeMap<_, _>>();
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.leases.contains_key(&owner) && leases.is_empty() {
            return;
        }
        state.redactor = None;
        state.revision = state
            .revision
            .checked_add(1)
            .expect("custody revision overflow");
        if leases.is_empty() {
            state.leases.remove(&owner);
        } else {
            state.leases.insert(owner, leases);
        }
    }

    pub fn register(
        &self,
        owner: impl Into<String>,
        source: impl Into<String>,
        lease: Arc<SecretLease>,
    ) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.redactor = None;
        state.revision = state
            .revision
            .checked_add(1)
            .expect("custody revision overflow");
        state
            .leases
            .entry(owner.into())
            .or_default()
            .insert(source.into(), lease);
    }

    pub fn remove_owner(&self, owner: &str) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.leases.remove(owner).is_some() {
            state.redactor = None;
            state.revision = state
                .revision
                .checked_add(1)
                .expect("custody revision overflow");
        }
    }

    pub fn remove(&self, owner: &str, source: &str) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut removed = false;
        if let Some(sources) = state.leases.get_mut(owner) {
            removed = sources.remove(source).is_some();
            if sources.is_empty() {
                state.leases.remove(owner);
            }
        }
        if removed {
            state.redactor = None;
            state.revision = state
                .revision
                .checked_add(1)
                .expect("custody revision overflow");
        }
    }

    pub fn revision(&self) -> u64 {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .revision
    }

    pub fn is_empty(&self) -> bool {
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .leases
            .is_empty()
    }

    pub(crate) fn projection_state(&self) -> JsonProjectionState {
        JsonProjectionState {
            custody_revision: self.revision(),
            ..JsonProjectionState::default()
        }
    }

    pub fn redactor(&self) -> Arc<SecretRedactor> {
        self.redactor_with_revision().1
    }

    fn redactor_with_revision(&self) -> (u64, Arc<SecretRedactor>) {
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(redactor) = &state.redactor {
            return (state.revision, Arc::clone(redactor));
        }
        drop(state);
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(redactor) = &state.redactor {
            return (state.revision, Arc::clone(redactor));
        }
        let leases = state
            .leases
            .values()
            .flat_map(|sources| sources.values().cloned())
            .collect();
        let redactor = Arc::new(SecretRedactor::new(
            leases,
            Arc::downgrade(&self.state),
            state.revision,
        ));
        state.redactor = Some(Arc::clone(&redactor));
        (state.revision, redactor)
    }

    pub fn project(
        &self,
        boundary: crate::telemetry::redact::CaptureBoundary,
        bytes: &[u8],
    ) -> crate::telemetry::redact::SanitizedCapture {
        self.redactor().sanitize(boundary, bytes)
    }

    pub fn leases(&self) -> Vec<Arc<SecretLease>> {
        self.leases_with_revision().1
    }

    pub(crate) fn leases_with_revision(&self) -> (u64, Vec<Arc<SecretLease>>) {
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            state.revision,
            state
                .leases
                .values()
                .flat_map(|sources| sources.values().cloned())
                .collect(),
        )
    }

    pub fn project_text_references(
        &self,
        boundary: crate::telemetry::redact::CaptureBoundary,
        value: &str,
    ) -> String {
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut replacements = state
            .leases
            .values()
            .flat_map(|sources| sources.iter())
            .filter_map(|(source, lease)| {
                std::str::from_utf8(lease.expose())
                    .ok()
                    .filter(|secret| !secret.is_empty())
                    .map(|secret| (secret.to_owned(), format!("<secret-ref:{source}>")))
            })
            .collect::<Vec<_>>();
        replacements.sort_by_key(|(secret, _)| std::cmp::Reverse(secret.len()));
        let projected = replacements
            .into_iter()
            .fold(value.to_owned(), |value, (secret, reference)| {
                value.replace(&secret, &reference)
            });
        drop(state);
        self.redactor().redact_text(boundary, &projected)
    }

    pub fn contains(&self, bytes: &[u8]) -> bool {
        let mut scanner = self.redactor().scanner();
        scanner.push(bytes);
        scanner.found()
    }

    pub fn contains_json(&self, value: &serde_json::Value) -> bool {
        let mut scanner = self.redactor().scanner();
        scan_ordered_json(value, &mut scanner);
        scanner.found()
    }

    pub fn project_json(
        &self,
        boundary: crate::telemetry::redact::CaptureBoundary,
        value: &serde_json::Value,
    ) -> serde_json::Value {
        let redactor = self.redactor();
        let mut scanner = redactor.scanner();
        scan_ordered_json(value, &mut scanner);
        let mut values = redactor.scanner();
        scan_json_values(value, &mut values);
        let mut names = redactor.scanner();
        scan_json_names(value, &mut names);
        if (scanner.found() || values.found() || names.found())
            && !json_contains_individual(value, &redactor)
        {
            return redact_aggregate(value, names.found() || !values.found());
        }
        let mut projected = project_json_value(value, boundary, &redactor.capture());
        for _ in 0..MAX_REDACTION_PASSES {
            if !json_reconstructs_secret(&projected, &redactor) {
                return projected;
            }
            projected = redact_aggregate(&projected, true);
        }
        serde_json::Value::Null
    }

    pub(crate) fn project_json_stream(
        &self,
        boundary: crate::telemetry::redact::CaptureBoundary,
        value: &serde_json::Value,
        state: &mut JsonProjectionState,
    ) -> serde_json::Value {
        let (revision, redactor) = self.redactor_with_revision();
        state.custody_revision = revision;
        let mut projected = self.project_json(boundary, value);
        for _ in 0..MAX_REDACTION_PASSES {
            let reconstructed = state.reconstructs_with(&projected, &redactor);
            if !reconstructed.iter().any(|found| *found)
                && !json_reconstructs_secret(&projected, &redactor)
            {
                state.advance(&projected, &redactor);
                return projected;
            }
            projected = redact_aggregate(&projected, reconstructed[2] || !reconstructed[1]);
        }
        projected = serde_json::Value::Null;
        state.advance(&projected, &redactor);
        projected
    }

    pub(crate) fn project_bytes_stream(
        &self,
        boundary: crate::telemetry::redact::CaptureBoundary,
        value: &[u8],
        state: &mut JsonProjectionState,
    ) -> Vec<u8> {
        let (revision, redactor) = self.redactor_with_revision();
        state.custody_revision = revision;
        let projected = redactor
            .sanitize(boundary, value)
            .bytes()
            .expect("sanitize finishes captures")
            .to_vec();
        let mut scanner = redactor.scanner();
        scanner.push(&state.bytes);
        scanner.push(&projected);
        let projected = if scanner.found() {
            REDACTED.as_bytes().to_vec()
        } else {
            projected
        };
        append_suffix(&mut state.bytes, redactor.stream_holdback(), |bytes| {
            bytes.extend_from_slice(&projected)
        });
        state.advance_sequence();
        projected
    }

    pub(crate) fn try_advance_ordered_json_object(
        &self,
        fields: &[(String, serde_json::Value)],
        envelope: &[u8],
        state: &mut JsonProjectionState,
    ) -> bool {
        let (revision, redactor) = self.redactor_with_revision();
        let mut ordered = Vec::new();
        let mut values = Vec::new();
        let mut names = Vec::new();
        for (name, value) in fields {
            ordered.extend_from_slice(name.as_bytes());
            collect_ordered_json(value, &mut ordered);
            collect_json_values(value, &mut values);
            names.extend_from_slice(name.as_bytes());
            collect_json_names(value, &mut names);
        }
        if [
            (&state.ordered, ordered.as_slice()),
            (&state.values, values.as_slice()),
            (&state.names, names.as_slice()),
            (&state.bytes, envelope),
        ]
        .into_iter()
        .any(|(previous, current)| {
            let mut scanner = redactor.scanner();
            scanner.push(previous);
            scanner.push(current);
            scanner.found()
        }) {
            return false;
        }
        let holdback = redactor.stream_holdback();
        append_suffix(&mut state.ordered, holdback, |bytes| {
            bytes.extend_from_slice(&ordered)
        });
        append_suffix(&mut state.values, holdback, |bytes| {
            bytes.extend_from_slice(&values)
        });
        append_suffix(&mut state.names, holdback, |bytes| {
            bytes.extend_from_slice(&names)
        });
        append_suffix(&mut state.bytes, holdback, |bytes| {
            bytes.extend_from_slice(envelope)
        });
        state.custody_revision = revision;
        state.advance_sequence();
        true
    }

    pub(crate) fn project_json_preserving_names(
        &self,
        boundary: crate::telemetry::redact::CaptureBoundary,
        value: &serde_json::Value,
    ) -> serde_json::Value {
        let redactor = self.redactor();
        let mut ordered = redactor.scanner();
        scan_ordered_json(value, &mut ordered);
        let mut values = redactor.scanner();
        scan_json_values(value, &mut values);
        let mut names = redactor.scanner();
        scan_json_names(value, &mut names);
        let redact_root_scalars = root_scalars_reconstruct_secret(value, &redactor);
        let mut projected = if (ordered.found() || values.found() || names.found())
            && !json_contains_individual(value, &redactor)
        {
            redact_aggregate_preserving_root_names(value, redact_root_scalars)
        } else {
            project_json_value_preserving_names(value, boundary, &redactor.capture())
        };
        for _ in 0..MAX_REDACTION_PASSES {
            if !json_reconstructs_secret(&projected, &redactor) {
                return projected;
            }
            projected = redact_aggregate_preserving_root_names(
                &projected,
                root_scalars_reconstruct_secret(&projected, &redactor),
            );
        }
        serde_json::Value::Null
    }

    pub(crate) fn project_json_bytes_preserving_names(
        &self,
        boundary: crate::telemetry::redact::CaptureBoundary,
        bytes: &[u8],
    ) -> Result<Vec<u8>, serde_json::Error> {
        if self.leases().is_empty() {
            return Ok(bytes.to_vec());
        }
        let value = serde_json::from_slice::<serde_json::Value>(bytes)?;
        let projected = self.project_json_preserving_names(boundary, &value);
        let capture = self.redactor().sanitize(boundary, bytes);
        if capture.bytes().expect("sanitize finishes captures") == bytes
            && projected == value
            && !json_reconstructs_secret(&value, &self.redactor())
        {
            Ok(bytes.to_vec())
        } else {
            serde_json::to_vec(&projected)
        }
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct JsonProjectionState {
    ordered: Vec<u8>,
    values: Vec<u8>,
    names: Vec<u8>,
    bytes: Vec<u8>,
    custody_revision: u64,
    sequence: u64,
}

impl JsonProjectionState {
    pub(crate) const VERSION: u16 = 1;
    const CHANNELS: usize = 4;
    pub(crate) const MAX_SERIALIZED_BYTES: usize =
        16 + Self::CHANNELS * (4 + crate::telemetry::redact::MAX_STREAM_HOLDBACK);

    pub(crate) const fn custody_revision(&self) -> u64 {
        self.custody_revision
    }

    pub(crate) fn merge_forward(&mut self, next: Self) -> bool {
        if next.custody_revision != self.custody_revision || next.sequence < self.sequence {
            return false;
        }
        if next.sequence == self.sequence && next != *self {
            return false;
        }
        *self = next;
        true
    }

    pub(crate) fn to_bounded_bytes(&self) -> Option<Vec<u8>> {
        let mut output = Vec::new();
        output.extend_from_slice(&self.custody_revision.to_be_bytes());
        output.extend_from_slice(&self.sequence.to_be_bytes());
        for value in [&self.ordered, &self.values, &self.names, &self.bytes] {
            let length = u32::try_from(value.len()).ok()?;
            output.extend_from_slice(&length.to_be_bytes());
            output.extend_from_slice(value);
            if output.len() > Self::MAX_SERIALIZED_BYTES {
                return None;
            }
        }
        Some(output)
    }

    pub(crate) fn from_bounded_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > Self::MAX_SERIALIZED_BYTES || bytes.len() < 16 {
            return None;
        }
        let custody_revision = u64::from_be_bytes(bytes[..8].try_into().ok()?);
        let sequence = u64::from_be_bytes(bytes[8..16].try_into().ok()?);
        let mut offset = 16;
        let mut take = || {
            let length =
                u32::from_be_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?) as usize;
            offset += 4;
            let value = bytes.get(offset..offset.checked_add(length)?)?.to_vec();
            offset += length;
            Some(value)
        };
        let state = Self {
            ordered: take()?,
            values: take()?,
            names: take()?,
            bytes: take()?,
            custody_revision,
            sequence,
        };
        (offset == bytes.len()).then_some(state)
    }

    fn reconstructs_with(&self, value: &serde_json::Value, redactor: &SecretRedactor) -> [bool; 3] {
        let mut ordered = Vec::new();
        collect_ordered_json(value, &mut ordered);
        let mut values = Vec::new();
        collect_json_values(value, &mut values);
        let mut names = Vec::new();
        collect_json_names(value, &mut names);
        [
            (&self.ordered, ordered),
            (&self.values, values),
            (&self.names, names),
        ]
        .map(|(previous, current)| {
            if previous.is_empty() || current.is_empty() {
                return false;
            }
            let mut scanner = redactor.scanner();
            scanner.push(previous);
            if scanner.found() {
                return true;
            }
            scanner.push(&current);
            scanner.found()
        })
    }

    fn advance(&mut self, value: &serde_json::Value, redactor: &SecretRedactor) {
        let holdback = redactor.stream_holdback();
        append_suffix(&mut self.ordered, holdback, |bytes| {
            collect_ordered_json(value, bytes)
        });
        append_suffix(&mut self.values, holdback, |bytes| {
            collect_json_values(value, bytes)
        });
        append_suffix(&mut self.names, holdback, |bytes| {
            collect_json_names(value, bytes)
        });
        self.advance_sequence();
    }

    fn advance_sequence(&mut self) {
        self.sequence = self
            .sequence
            .checked_add(1)
            .expect("projection state sequence overflow");
    }
}

fn append_suffix(output: &mut Vec<u8>, holdback: usize, append: impl FnOnce(&mut Vec<u8>)) {
    append(output);
    if output.len() > holdback {
        let drain = output.len() - holdback;
        output[..drain].fill(0);
        output.drain(..drain);
    }
}

fn json_reconstructs_secret(value: &serde_json::Value, redactor: &SecretRedactor) -> bool {
    let mut canonical = redactor.scanner();
    match serde_json::to_vec(value) {
        Ok(bytes) => canonical.push(&bytes),
        Err(_) => return true,
    }
    let mut ordered = redactor.scanner();
    scan_ordered_json(value, &mut ordered);
    canonical.found() || ordered.found()
}

fn redact_aggregate(value: &serde_json::Value, redact_names: bool) -> serde_json::Value {
    match value {
        serde_json::Value::String(_) => REDACTED.into(),
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| redact_aggregate(value, redact_names))
                .collect(),
        ),
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .enumerate()
                .map(|(index, (name, value))| {
                    (
                        if redact_names {
                            format!("[REDACTED:{index}]")
                        } else {
                            name.clone()
                        },
                        redact_aggregate(value, redact_names),
                    )
                })
                .collect(),
        ),
        value => value.clone(),
    }
}

fn redact_aggregate_preserving_root_names(
    value: &serde_json::Value,
    redact_root_scalars: bool,
) -> serde_json::Value {
    match value {
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .map(|(name, value)| {
                    let value = match value {
                        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                            redact_aggregate(value, true)
                        }
                        _ if redact_root_scalars => serde_json::Value::String(REDACTED.to_owned()),
                        value => value.clone(),
                    };
                    (name.clone(), value)
                })
                .collect(),
        ),
        value => redact_aggregate(value, true),
    }
}

fn root_scalars_reconstruct_secret(value: &serde_json::Value, redactor: &SecretRedactor) -> bool {
    let mut bytes = Vec::new();
    match value {
        serde_json::Value::Object(values) => {
            for (name, value) in values {
                bytes.extend_from_slice(name.as_bytes());
                if !matches!(
                    value,
                    serde_json::Value::Array(_) | serde_json::Value::Object(_)
                ) {
                    collect_ordered_json(value, &mut bytes);
                }
            }
        }
        value => collect_ordered_json(value, &mut bytes),
    }
    let mut scanner = redactor.scanner();
    scanner.push(&bytes);
    scanner.found()
}

fn json_contains_individual(value: &serde_json::Value, redactor: &SecretRedactor) -> bool {
    match value {
        serde_json::Value::String(value) => {
            let mut scanner = redactor.scanner();
            scanner.push(value.as_bytes());
            scanner.found()
        }
        serde_json::Value::Array(values) => values
            .iter()
            .any(|value| json_contains_individual(value, redactor)),
        serde_json::Value::Object(values) => values.iter().any(|(name, value)| {
            let mut scanner = redactor.scanner();
            scanner.push(name.as_bytes());
            scanner.found() || json_contains_individual(value, redactor)
        }),
        _ => false,
    }
}

fn scan_ordered_json(
    value: &serde_json::Value,
    scanner: &mut crate::telemetry::redact::SensitiveDataScanner,
) {
    let mut bytes = Vec::new();
    collect_ordered_json(value, &mut bytes);
    scanner.push(&bytes);
}

fn collect_ordered_json(value: &serde_json::Value, bytes: &mut Vec<u8>) {
    match value {
        serde_json::Value::String(value) => bytes.extend_from_slice(value.as_bytes()),
        serde_json::Value::Array(values) => {
            for value in values {
                collect_ordered_json(value, bytes);
            }
        }
        serde_json::Value::Object(values) => {
            for (name, value) in values {
                bytes.extend_from_slice(name.as_bytes());
                collect_ordered_json(value, bytes);
            }
        }
        value => bytes.extend_from_slice(value.to_string().as_bytes()),
    }
}

fn scan_json_values(
    value: &serde_json::Value,
    scanner: &mut crate::telemetry::redact::SensitiveDataScanner,
) {
    let mut bytes = Vec::new();
    collect_json_values(value, &mut bytes);
    scanner.push(&bytes);
}

fn collect_json_values(value: &serde_json::Value, bytes: &mut Vec<u8>) {
    match value {
        serde_json::Value::String(value) => bytes.extend_from_slice(value.as_bytes()),
        serde_json::Value::Array(values) => {
            for value in values {
                collect_json_values(value, bytes);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values() {
                collect_json_values(value, bytes);
            }
        }
        value => bytes.extend_from_slice(value.to_string().as_bytes()),
    }
}

fn scan_json_names(
    value: &serde_json::Value,
    scanner: &mut crate::telemetry::redact::SensitiveDataScanner,
) {
    let mut bytes = Vec::new();
    collect_json_names(value, &mut bytes);
    scanner.push(&bytes);
}

fn collect_json_names(value: &serde_json::Value, bytes: &mut Vec<u8>) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                collect_json_names(value, bytes);
            }
        }
        serde_json::Value::Object(values) => {
            for (name, value) in values {
                bytes.extend_from_slice(name.as_bytes());
                collect_json_names(value, bytes);
            }
        }
        _ => {}
    }
}

fn project_json_value(
    value: &serde_json::Value,
    boundary: crate::telemetry::redact::CaptureBoundary,
    redactor: &crate::telemetry::redact::CaptureRedactor<'_>,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(value)
            if boundary == crate::telemetry::redact::CaptureBoundary::Event
                && crate::telemetry::redact::trusted_event_literal(value) =>
        {
            serde_json::Value::String(value.clone())
        }
        serde_json::Value::String(value) => {
            serde_json::Value::String(redactor.redact_text(boundary, value))
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| project_json_value(value, boundary, redactor))
                .collect(),
        ),
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .enumerate()
                .map(|(index, (name, value))| {
                    let projected_name = redactor.redact_text(boundary, name);
                    (
                        if projected_name == name.as_str() {
                            projected_name
                        } else {
                            format!("[REDACTED:{index}]")
                        },
                        project_json_value(value, boundary, redactor),
                    )
                })
                .collect(),
        ),
        value => value.clone(),
    }
}

fn project_json_value_preserving_names(
    value: &serde_json::Value,
    boundary: crate::telemetry::redact::CaptureBoundary,
    redactor: &crate::telemetry::redact::CaptureRedactor<'_>,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(value)
            if boundary == crate::telemetry::redact::CaptureBoundary::Event
                && crate::telemetry::redact::trusted_event_literal(value) =>
        {
            serde_json::Value::String(value.clone())
        }
        serde_json::Value::String(value) => {
            serde_json::Value::String(redactor.redact_text(boundary, value))
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| project_json_value_preserving_names(value, boundary, redactor))
                .collect(),
        ),
        serde_json::Value::Object(values) => serde_json::Value::Object(
            values
                .iter()
                .enumerate()
                .map(|(index, (name, value))| {
                    let projected_name = redactor
                        .redact_text(crate::telemetry::redact::CaptureBoundary::Artifact, name);
                    (
                        if projected_name == name.as_str() {
                            projected_name
                        } else {
                            format!("[REDACTED:{index}]")
                        },
                        project_json_value_preserving_names(value, boundary, redactor),
                    )
                })
                .collect(),
        ),
        value => value.clone(),
    }
}

impl fmt::Debug for SecretCustody {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretCustody")
            .field("active_leases", &self.leases().len())
            .finish()
    }
}

pub struct SecretRedactor {
    leases: Vec<Arc<SecretLease>>,
    patterns: OnceLock<Arc<crate::telemetry::redact::SecretPatterns>>,
    text_patterns: OnceLock<Arc<crate::telemetry::redact::SecretPatterns>>,
    custody: Weak<RwLock<SecretCustodyState>>,
    custody_revision: u64,
}

impl SecretRedactor {
    fn new(
        leases: Vec<Arc<SecretLease>>,
        custody: Weak<RwLock<SecretCustodyState>>,
        custody_revision: u64,
    ) -> Self {
        Self {
            leases,
            patterns: OnceLock::new(),
            text_patterns: OnceLock::new(),
            custody,
            custody_revision,
        }
    }

    pub(crate) fn capture(&self) -> crate::telemetry::redact::CaptureRedactor<'static> {
        self.capture_snapshot().2
    }

    fn capture_snapshot(
        &self,
    ) -> (
        u64,
        usize,
        crate::telemetry::redact::CaptureRedactor<'static>,
    ) {
        let (revision, leases) = self.custody.upgrade().map_or_else(
            || (self.custody_revision, self.leases.clone()),
            |state| {
                let state = state
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (
                    state.revision,
                    state
                        .leases
                        .values()
                        .flat_map(|sources| sources.values().cloned())
                        .collect(),
                )
            },
        );
        let (patterns, text_patterns) = if revision == self.custody_revision {
            (
                Arc::clone(self.patterns.get_or_init(|| {
                    Arc::new(crate::telemetry::redact::SecretPatterns::from_shared(
                        &self.leases,
                    ))
                })),
                Arc::clone(self.text_patterns.get_or_init(|| {
                    Arc::new(crate::telemetry::redact::SecretPatterns::text_from_shared(
                        &self.leases,
                    ))
                })),
            )
        } else {
            (
                Arc::new(crate::telemetry::redact::SecretPatterns::from_shared(
                    &leases,
                )),
                Arc::new(crate::telemetry::redact::SecretPatterns::text_from_shared(
                    &leases,
                )),
            )
        };
        (
            revision,
            leases.len(),
            crate::telemetry::redact::CaptureRedactor::from_owned_prepared(patterns, text_patterns),
        )
    }

    pub fn redact_text(
        &self,
        boundary: crate::telemetry::redact::CaptureBoundary,
        value: &str,
    ) -> String {
        self.capture().redact_text(boundary, value)
    }

    pub fn sanitize(
        &self,
        boundary: crate::telemetry::redact::CaptureBoundary,
        value: &[u8],
    ) -> crate::telemetry::redact::SanitizedCapture {
        let mut capture = self.start(boundary);
        capture.push(value).expect("new capture is writable");
        capture.finish().expect("new capture can be finished");
        capture
    }

    pub fn start(
        &self,
        boundary: crate::telemetry::redact::CaptureBoundary,
    ) -> crate::telemetry::redact::SanitizedCapture {
        let (revision, leases, redactor) = self.capture_snapshot();
        let capture = redactor.start(boundary);
        if let Some(state) = self.custody.upgrade() {
            capture.with_custody(SecretCustody { state }, revision, leases)
        } else {
            capture
        }
    }

    pub fn scanner(&self) -> crate::telemetry::redact::SensitiveDataScanner {
        let (revision, leases, redactor) = self.capture_snapshot();
        let scanner = redactor.scanner();
        if let Some(state) = self.custody.upgrade() {
            scanner.with_custody(SecretCustody { state }, revision, leases)
        } else {
            scanner
        }
    }

    pub fn stream_holdback(&self) -> usize {
        self.capture().stream_holdback()
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

#[cfg(test)]
mod custody_tests {
    use super::*;
    use crate::telemetry::redact::CaptureBoundary;

    #[test]
    fn custody_is_a_union_and_owner_refresh_is_scoped() {
        let custody = SecretCustody::default();
        custody.replace_owner(
            "attempt-a",
            [("source-a".to_owned(), Arc::new(SecretLease::new("alpha")))],
        );
        custody.replace_owner(
            "attempt-b",
            [("source-b".to_owned(), Arc::new(SecretLease::new("bravo")))],
        );
        custody.replace_owner(
            "attempt-a",
            [("source-a".to_owned(), Arc::new(SecretLease::new("after")))],
        );
        assert!(custody.contains(b"after"));
        assert!(custody.contains(b"bravo"));
        assert!(!custody.contains(b"alpha"));
        custody.remove_owner("attempt-a");
        assert!(custody.contains(b"bravo"));
    }

    #[test]
    fn existing_scanners_and_captures_refresh_on_custody_rotation() {
        let custody = SecretCustody::default();
        let mut scanner = custody.redactor().scanner();
        let mut capture = custody.redactor().start(CaptureBoundary::Event);
        scanner.push(b"new-");
        capture.push(b"new-").unwrap();

        custody.register(
            "attempt",
            "rotated",
            Arc::new(SecretLease::new("new-secret")),
        );
        scanner.push(b"secret");
        capture.push(b"secret").unwrap();
        capture.finish().unwrap();

        assert!(scanner.found());
        assert_eq!(capture.bytes().unwrap(), REDACTED.as_bytes());
    }

    #[test]
    fn retained_redactor_refreshes_all_direct_entrypoints_after_rotation() {
        let custody = SecretCustody::new_named(
            "attempt",
            [(
                "original".to_owned(),
                Arc::new(SecretLease::new("old-secret")),
            )],
        );
        let redactor = custody.redactor();
        custody.replace_owner(
            "attempt",
            [(
                "rotated".to_owned(),
                Arc::new(SecretLease::new("new-secret")),
            )],
        );

        assert_eq!(
            redactor.redact_text(CaptureBoundary::Event, "new-secret"),
            REDACTED
        );
        assert_eq!(
            redactor
                .sanitize(CaptureBoundary::Event, b"new-secret")
                .bytes()
                .unwrap(),
            REDACTED.as_bytes()
        );
        let mut scanner = redactor.scanner();
        scanner.push(b"new-secret");
        assert!(scanner.found());

        let marker_custody = SecretCustody::new([
            Arc::new(SecretLease::new("trigger")),
            Arc::new(SecretLease::new(REDACTED)),
        ]);
        let mut state = marker_custody.projection_state();

        assert!(
            marker_custody
                .project_bytes_stream(CaptureBoundary::WorkspaceMetadata, b"trigger", &mut state)
                .is_empty()
        );
        let projected =
            marker_custody.project_json(CaptureBoundary::Event, &serde_json::json!("trigger"));
        assert!(!marker_custody.contains_json(&projected));
    }

    #[test]
    fn serialized_projection_state_prevents_split_partial_reads_after_restart() {
        let custody = SecretCustody::new([Arc::new(SecretLease::new("read-secret"))]);
        let mut state = custody.projection_state();
        assert_eq!(
            custody.project_bytes_stream(CaptureBoundary::WorkspaceMetadata, b"read-", &mut state),
            b"read-"
        );
        let serialized = state.to_bounded_bytes().unwrap();
        let mut restored = JsonProjectionState::from_bounded_bytes(&serialized).unwrap();
        assert_eq!(
            custody.project_bytes_stream(
                CaptureBoundary::WorkspaceMetadata,
                b"secret",
                &mut restored,
            ),
            REDACTED.as_bytes()
        );

        let holdback = crate::telemetry::redact::MAX_STREAM_HOLDBACK;
        let maximum = JsonProjectionState {
            ordered: vec![b'o'; holdback],
            values: vec![b'v'; holdback],
            names: vec![b'n'; holdback],
            bytes: vec![b'b'; holdback],
            custody_revision: custody.revision(),
            sequence: 1,
        };
        let serialized = maximum.to_bounded_bytes().unwrap();
        assert_eq!(serialized.len(), JsonProjectionState::MAX_SERIALIZED_BYTES);
        assert_eq!(
            JsonProjectionState::from_bounded_bytes(&serialized),
            Some(maximum)
        );
    }

    #[test]
    fn ordered_json_projection_catches_split_values_and_names_without_field_exemptions() {
        let custody = SecretCustody::new([Arc::new(SecretLease::new("secret-value"))]);
        let split = serde_json::json!(["secret-", "value"]);
        let projected = custody.project_json(CaptureBoundary::Event, &split);
        assert_eq!(projected, serde_json::json!([REDACTED, REDACTED]));

        let names = serde_json::json!({"secret-": 1, "value": 2});
        let projected = custody.project_json(CaptureBoundary::Event, &names);
        assert!(
            projected
                .as_object()
                .unwrap()
                .keys()
                .all(|name| !name.contains("secret-") && name != "value")
        );

        let direct = serde_json::json!({
            "id": "secret-value",
            "digest": "secret-value",
            "handle": "secret-value",
            "token": "secret-value",
        });
        let projected = custody.project_json(CaptureBoundary::Event, &direct);
        assert!(
            projected
                .as_object()
                .unwrap()
                .values()
                .all(|value| value == REDACTED)
        );
    }

    #[test]
    fn empty_custody_preserves_ordinary_token_fields_and_prompt_uses_references() {
        let empty = SecretCustody::default();
        let input = serde_json::json!({"token": "ordinary", "nested": {"api_key": "public"}});
        assert_eq!(
            empty.project_json(CaptureBoundary::CompositionInput, &input),
            input
        );

        let custody = SecretCustody::new_named(
            "attempt",
            [(
                "env:API_KEY".to_owned(),
                Arc::new(SecretLease::new("raw-secret")),
            )],
        );
        assert_eq!(
            custody.project_text_references(CaptureBoundary::Prompt, "use raw-secret"),
            "use <secret-ref:env:API_KEY>"
        );
    }

    #[test]
    fn raw_json_projection_preserves_clean_nonlexical_key_order() {
        let custody = SecretCustody::new([Arc::new(SecretLease::new("unrelated-canary"))]);
        let canonical = br#"{"z":1,"a":{"y":2,"b":3}}"#;
        assert_eq!(
            custody
                .project_json_bytes_preserving_names(CaptureBoundary::Event, canonical)
                .unwrap(),
            canonical
        );
    }

    #[test]
    fn raw_json_projection_never_preserves_split_root_scalars() {
        let custody = SecretCustody::new([Arc::new(SecretLease::new("root-secret"))]);
        let canonical = br#"{"root-":"secret","public":1}"#;
        let projected = custody
            .project_json_bytes_preserving_names(CaptureBoundary::Event, canonical)
            .unwrap();
        assert_ne!(projected, canonical);
        let mut scanner = custody.redactor().scanner();
        scanner.push(&projected);
        assert!(!scanner.found());
    }

    #[test]
    fn clean_event_projection_keeps_old_bytes_and_authority_digest() {
        let custody = SecretCustody::new([Arc::new(SecretLease::new("unrelated-canary"))]);
        let payload = br#"{"z":1,"a":{"y":2,"b":3}}"#.to_vec();
        let canonical = br#"{"operation":"run.progress","stream":"run_00000000000000000000000000","payload":{"z":1,"a":{"y":2,"b":3}}}"#.to_vec();
        let projected = crate::api::service::project_event_envelopes(
            &custody,
            vec![(canonical.clone(), payload.clone())],
        )
        .unwrap()
        .remove(0);
        let old_digest = crate::capabilities::kernel::identity::Digest::of(
            crate::capabilities::kernel::identity::DigestAlgorithm::Sha256,
            &canonical,
        )
        .to_string();
        assert_eq!(projected.payload, payload);
        assert_eq!(projected.authority_digest, old_digest);
        assert_eq!(projected.digest, old_digest);
        assert_eq!(projected.envelope, canonical);
    }

    #[test]
    fn event_page_state_does_not_join_payloads_separated_by_envelope_metadata() {
        let custody = SecretCustody::new([Arc::new(SecretLease::new("cross-frame"))]);
        for split in 1.."cross-frame".len() {
            let envelopes = [&"cross-frame"[..split], &"cross-frame"[split..]]
                .into_iter()
                .map(|content| {
                    let envelope = serde_json::json!({
                        "operation": "run.progress",
                        "stream": "run_00000000000000000000000000",
                        "payload": {"content": content},
                    });
                    (
                        serde_json::to_vec(&envelope).unwrap(),
                        serde_json::to_vec(&envelope["payload"]).unwrap(),
                    )
                })
                .collect();
            let projected =
                crate::api::service::project_event_envelopes(&custody, envelopes).unwrap();
            let first: serde_json::Value = serde_json::from_slice(&projected[0].payload).unwrap();
            let second: serde_json::Value = serde_json::from_slice(&projected[1].payload).unwrap();
            assert_eq!(first["content"], &"cross-frame"[..split]);
            assert_eq!(second["content"], &"cross-frame"[split..]);
        }

        let ordinary = serde_json::json!({"operation": "thread.archive"});
        let mut state = JsonProjectionState::default();
        assert_eq!(
            custody.project_json_stream(CaptureBoundary::Event, &ordinary, &mut state),
            ordinary
        );
    }

    #[test]
    fn event_projection_scans_splits_under_formerly_trusted_nested_fields() {
        let custody = SecretCustody::new([Arc::new(SecretLease::new("cross-frame"))]);
        let envelope = serde_json::json!({
            "operation": "run.progress",
            "stream": "run_00000000000000000000000000",
            "payload": {"input": {"cross-": "frame"}},
        });
        let projected = crate::api::service::project_event_envelopes(
            &custody,
            vec![(
                serde_json::to_vec(&envelope).unwrap(),
                serde_json::to_vec(&envelope["payload"]).unwrap(),
            )],
        )
        .unwrap()
        .remove(0);

        let mut scanner = custody.redactor().scanner();
        scanner.push(&projected.envelope);
        assert!(!scanner.found());
        assert!(String::from_utf8_lossy(&projected.envelope).contains(REDACTED));
        let envelope: serde_json::Value = serde_json::from_slice(&projected.envelope).unwrap();
        assert_eq!(envelope["operation"], "run.progress");
        assert_eq!(envelope["stream"], "run_00000000000000000000000000");
        assert!(envelope["payload"].is_object());
    }

    #[test]
    fn event_projection_advances_through_metadata_between_pages() {
        let custody = SecretCustody::new([Arc::new(SecretLease::new("cross-frame"))]);
        let envelope = |operation: &str,
                        stream: &str,
                        payload: serde_json::Value,
                        trace: &str,
                        id: &str,
                        attempt: &str,
                        artifacts: serde_json::Value| {
            let canonical = format!(
                r#"{{"operation":"{operation}","stream":"{stream}","payload":{payload},"trace_id":"{trace}","id":"{id}","attempt_id":"{attempt}","artifacts":{artifacts}}}"#
            )
            .into_bytes();
            (canonical, serde_json::to_vec(&payload).unwrap())
        };

        assert!(
            crate::api::service::project_event_envelopes(
                &custody,
                vec![envelope(
                    "cross-",
                    "frame",
                    serde_json::json!({}),
                    "trace",
                    "id",
                    "attempt",
                    serde_json::json!([]),
                )],
            )
            .is_err()
        );

        let projected = crate::api::service::project_event_envelopes(
            &custody,
            vec![envelope(
                "run.progress",
                "stream",
                serde_json::json!({"content": "cross-"}),
                "frame",
                "id",
                "attempt",
                serde_json::json!([]),
            )],
        )
        .unwrap()
        .remove(0);
        let projected_envelope: serde_json::Value =
            serde_json::from_slice(&projected.envelope).unwrap();
        assert_eq!(projected_envelope["payload"]["marker"], REDACTED);
        assert_eq!(
            projected_envelope["payload"]["projection"],
            serde_json::json!({"schema_version": 1, "status": "fail_closed"})
        );
        assert!(projected_envelope["payload"].is_object());
        assert_eq!(
            projected.payload,
            serde_json::to_vec(&projected_envelope["payload"]).unwrap()
        );
        assert_eq!(projected_envelope["trace_id"], "frame");

        assert!(
            crate::api::service::project_event_envelopes(
                &custody,
                vec![envelope(
                    "run.progress",
                    "stream",
                    serde_json::json!({}),
                    "cross-",
                    "frame",
                    "attempt",
                    serde_json::json!([]),
                )],
            )
            .is_err()
        );

        let projected = crate::api::service::project_event_envelopes(
            &custody,
            vec![envelope(
                "run.progress",
                "stream",
                serde_json::json!({}),
                "trace",
                "id",
                "cross-",
                serde_json::json!(["frame"]),
            )],
        )
        .unwrap()
        .remove(0);
        let projected_envelope: serde_json::Value =
            serde_json::from_slice(&projected.envelope).unwrap();
        assert_eq!(projected_envelope["attempt_id"], "cross-");
        assert_eq!(projected_envelope["artifacts"], REDACTED);

        let mut state = custody.projection_state();
        crate::api::service::project_event_envelopes_with_state(
            &custody,
            vec![envelope(
                "run.progress",
                "stream",
                serde_json::json!({}),
                "trace",
                "id",
                "attempt",
                serde_json::json!(["cross-"]),
            )],
            &mut state,
        )
        .unwrap();
        assert!(
            crate::api::service::project_event_envelopes_with_state(
                &custody,
                vec![envelope(
                    "frame",
                    "stream",
                    serde_json::json!({}),
                    "trace",
                    "id",
                    "attempt",
                    serde_json::json!([]),
                )],
                &mut state,
            )
            .is_err()
        );
    }
}
