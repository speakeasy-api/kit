use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    io::{self, Write},
    sync::Arc,
};

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};
use sha2::{Digest as _, Sha256};

use crate::{
    capabilities::{
        discovery::BindingId,
        kernel::{
            identity::{CapabilityIdentity, Digest, DigestAlgorithm},
            invoke::InvocationStatus,
        },
        schema::number_is_lossless,
    },
    domain::{
        events::TraceId,
        ids::{PrincipalId, ToolCallId},
    },
    runtime::scheduler::limits::Spend,
    store::{artifacts::ArtifactReference, sqlite::idempotency::IdempotencyKey},
};

pub const CANONICAL_RESULT_SCHEMA_VERSION: u16 = 1;
pub const MAX_CANONICAL_RESULT_BYTES: usize = 65_536;
pub const MAX_RESULT_ARTIFACTS: usize = 128;
pub const MAX_RESULT_ERROR_CODE_BYTES: usize = 256;
pub const MAX_RESULT_PROVENANCE_TEXT_BYTES: usize = 256;
pub const MAX_PRESENTATION_BYTES: usize = 16_384;
pub const MAX_PRESENTATION_NAME_BYTES: usize = 64;
const MAX_RESULT_JSON_DEPTH: usize = 64;
const MAX_RESULT_JSON_NODES: usize = 100_000;
const RESULT_DIGEST_DOMAIN: &[u8] = b"KIT-CANONICAL-RESULT\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResultError {
    InvalidJson,
    NonCanonical,
    UnsupportedVersion,
    InvalidStatus,
    InvalidErrorCode,
    InvalidProvenance,
    InvalidArtifact,
    DuplicateArtifact,
    TooManyArtifacts,
    ResultTooLarge,
    InvalidPresentation,
    PresentationTooLarge,
}

impl fmt::Display for ResultError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidJson => "invalid canonical result JSON",
            Self::NonCanonical => "result JSON is not canonical",
            Self::UnsupportedVersion => "unsupported canonical result schema version",
            Self::InvalidStatus => "result status, content, and error code are inconsistent",
            Self::InvalidErrorCode => "result error code is invalid",
            Self::InvalidProvenance => "result provenance is invalid",
            Self::InvalidArtifact => "result artifact reference is invalid",
            Self::DuplicateArtifact => "result artifact reference is duplicated",
            Self::TooManyArtifacts => "result has too many artifact references",
            Self::ResultTooLarge => "canonical result exceeds its byte limit",
            Self::InvalidPresentation => "result presentation is invalid",
            Self::PresentationTooLarge => "result presentation exceeds its byte limit",
        })
    }
}

impl std::error::Error for ResultError {}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CanonicalResultDigest([u8; 32]);

impl CanonicalResultDigest {
    fn new(canonical_bytes: &[u8]) -> Self {
        let mut digest = Sha256::new();
        digest.update(RESULT_DIGEST_DOMAIN);
        digest.update(CANONICAL_RESULT_SCHEMA_VERSION.to_be_bytes());
        digest.update(canonical_bytes);
        Self(digest.finalize().into())
    }

    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }

    pub fn hex(self) -> String {
        let mut value = String::with_capacity(64);
        for byte in self.0 {
            use fmt::Write as _;
            write!(value, "{byte:02x}").expect("writing to a string cannot fail");
        }
        value
    }
}

impl fmt::Display for CanonicalResultDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "sha256:{}", self.hex())
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProvenanceDigest {
    algorithm: DigestAlgorithm,
    bytes: [u8; 32],
}

impl ProvenanceDigest {
    pub const fn algorithm(self) -> DigestAlgorithm {
        self.algorithm
    }

    pub const fn as_bytes(self) -> [u8; 32] {
        self.bytes
    }

    fn parse(value: &str) -> Result<Self, ResultError> {
        let (algorithm, hex) = value
            .split_once(':')
            .ok_or(ResultError::InvalidProvenance)?;
        let algorithm = match algorithm {
            "sha256" => DigestAlgorithm::Sha256,
            "blake3" => DigestAlgorithm::Blake3,
            _ => return Err(ResultError::InvalidProvenance),
        };
        if hex.len() != 64 {
            return Err(ResultError::InvalidProvenance);
        }
        let mut bytes = [0; 32];
        for (output, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
            *output = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
        }
        Ok(Self { algorithm, bytes })
    }
}

impl From<Digest> for ProvenanceDigest {
    fn from(value: Digest) -> Self {
        Self {
            algorithm: value.algorithm(),
            bytes: value.as_bytes(),
        }
    }
}

impl fmt::Display for ProvenanceDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:", self.algorithm.name())?;
        for byte in self.bytes {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResultCapabilityIdentity {
    source: Arc<str>,
    namespace: Arc<str>,
    name: Arc<str>,
    version: Arc<str>,
    implementation_digest: ProvenanceDigest,
}

impl ResultCapabilityIdentity {
    fn from_identity(value: &CapabilityIdentity) -> Result<Self, ResultError> {
        let fields = [
            value.source().as_str(),
            value.namespace().as_str(),
            value.name().as_str(),
            value.version().as_str(),
        ];
        if fields
            .iter()
            .any(|field| !valid_text(field, MAX_RESULT_PROVENANCE_TEXT_BYTES))
        {
            return Err(ResultError::InvalidProvenance);
        }
        Ok(Self {
            source: Arc::from(fields[0]),
            namespace: Arc::from(fields[1]),
            name: Arc::from(fields[2]),
            version: Arc::from(fields[3]),
            implementation_digest: value.implementation_digest().into(),
        })
    }

    fn from_wire(value: &WireCapability) -> Result<Self, ResultError> {
        if [&value.source, &value.namespace, &value.name, &value.version]
            .into_iter()
            .any(|field| !valid_text(field, MAX_RESULT_PROVENANCE_TEXT_BYTES))
        {
            return Err(ResultError::InvalidProvenance);
        }
        Ok(Self {
            source: Arc::from(value.source.as_str()),
            namespace: Arc::from(value.namespace.as_str()),
            name: Arc::from(value.name.as_str()),
            version: Arc::from(value.version.as_str()),
            implementation_digest: ProvenanceDigest::parse(&value.implementation_digest)?,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub const fn implementation_digest(&self) -> ProvenanceDigest {
        self.implementation_digest
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegationProvenance {
    digest: ProvenanceDigest,
    depth: u16,
    maximum_depth: u16,
}

impl DelegationProvenance {
    pub fn new(digest: Digest, depth: u16, maximum_depth: u16) -> Result<Self, ResultError> {
        Self::from_parts(digest.into(), depth, maximum_depth)
    }

    fn from_parts(
        digest: ProvenanceDigest,
        depth: u16,
        maximum_depth: u16,
    ) -> Result<Self, ResultError> {
        if depth == 0 || depth > maximum_depth {
            return Err(ResultError::InvalidProvenance);
        }
        Ok(Self {
            digest,
            depth,
            maximum_depth,
        })
    }

    pub const fn digest(self) -> ProvenanceDigest {
        self.digest
    }

    pub const fn depth(self) -> u16 {
        self.depth
    }

    pub const fn maximum_depth(self) -> u16 {
        self.maximum_depth
    }
}

#[derive(Clone, Debug)]
pub struct CallProvenanceInput {
    pub invocation_id: ToolCallId,
    pub principal_id: PrincipalId,
    pub binding_id: BindingId,
    pub capability: CapabilityIdentity,
    pub schema_digest: Digest,
    pub authorization_snapshot_digest: Digest,
    pub grant_snapshot_digest: Digest,
    pub trace_id: TraceId,
    pub idempotency_key: IdempotencyKey,
    pub remaining_budget: Spend,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallProvenance {
    invocation_id: ToolCallId,
    parent_invocation_id: Option<ToolCallId>,
    principal_id: PrincipalId,
    binding_id: BindingId,
    capability: ResultCapabilityIdentity,
    schema_digest: ProvenanceDigest,
    authorization_snapshot_digest: ProvenanceDigest,
    grant_snapshot_digest: ProvenanceDigest,
    delegation: Option<DelegationProvenance>,
    trace_id: TraceId,
    idempotency_key: IdempotencyKey,
    remaining_budget: Spend,
}

impl CallProvenance {
    pub fn direct(input: CallProvenanceInput) -> Result<Self, ResultError> {
        Self::from_input(input, None, None)
    }

    pub fn nested(
        input: CallProvenanceInput,
        parent_invocation_id: ToolCallId,
        delegation: DelegationProvenance,
    ) -> Result<Self, ResultError> {
        Self::from_input(input, Some(parent_invocation_id), Some(delegation))
    }

    fn from_input(
        input: CallProvenanceInput,
        parent_invocation_id: Option<ToolCallId>,
        delegation: Option<DelegationProvenance>,
    ) -> Result<Self, ResultError> {
        if parent_invocation_id == Some(input.invocation_id) {
            return Err(ResultError::InvalidProvenance);
        }
        if !valid_text(
            input.trace_id.to_string().as_str(),
            MAX_RESULT_PROVENANCE_TEXT_BYTES,
        ) || !valid_text(
            input.idempotency_key.as_str(),
            MAX_RESULT_PROVENANCE_TEXT_BYTES,
        ) {
            return Err(ResultError::InvalidProvenance);
        }
        Ok(Self {
            invocation_id: input.invocation_id,
            parent_invocation_id,
            principal_id: input.principal_id,
            binding_id: input.binding_id,
            capability: ResultCapabilityIdentity::from_identity(&input.capability)?,
            schema_digest: input.schema_digest.into(),
            authorization_snapshot_digest: input.authorization_snapshot_digest.into(),
            grant_snapshot_digest: input.grant_snapshot_digest.into(),
            delegation,
            trace_id: input.trace_id,
            idempotency_key: input.idempotency_key,
            remaining_budget: input.remaining_budget,
        })
    }

    fn from_wire(wire: &DecodedWireResultV1) -> Result<Self, ResultError> {
        let parent_invocation_id = wire
            .parent_invocation_id
            .as_deref()
            .map(ToolCallId::parse)
            .transpose()
            .map_err(|_| ResultError::InvalidProvenance)?;
        let delegation = wire
            .delegation
            .as_ref()
            .map(|value| {
                DelegationProvenance::from_parts(
                    ProvenanceDigest::parse(&value.digest)?,
                    value.depth,
                    value.maximum_depth,
                )
            })
            .transpose()?;
        let invocation_id =
            ToolCallId::parse(&wire.invocation_id).map_err(|_| ResultError::InvalidProvenance)?;
        if parent_invocation_id.is_some() != delegation.is_some()
            || parent_invocation_id == Some(invocation_id)
        {
            return Err(ResultError::InvalidProvenance);
        }
        Ok(Self {
            invocation_id,
            parent_invocation_id,
            principal_id: PrincipalId::parse(&wire.principal_id)
                .map_err(|_| ResultError::InvalidProvenance)?,
            binding_id: BindingId::parse(&wire.binding_id)
                .map_err(|_| ResultError::InvalidProvenance)?,
            capability: ResultCapabilityIdentity::from_wire(&wire.capability)?,
            schema_digest: ProvenanceDigest::parse(&wire.schema_digest)?,
            authorization_snapshot_digest: ProvenanceDigest::parse(
                &wire.authorization_snapshot_digest,
            )?,
            grant_snapshot_digest: ProvenanceDigest::parse(&wire.grant_snapshot_digest)?,
            delegation,
            trace_id: TraceId::parse(&wire.trace_id).map_err(|_| ResultError::InvalidProvenance)?,
            idempotency_key: IdempotencyKey::parse(&wire.idempotency_key)
                .map_err(|_| ResultError::InvalidProvenance)?,
            remaining_budget: wire.remaining_budget.into(),
        })
    }

    pub const fn invocation_id(&self) -> ToolCallId {
        self.invocation_id
    }

    pub const fn parent_invocation_id(&self) -> Option<ToolCallId> {
        self.parent_invocation_id
    }

    pub const fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    pub const fn binding_id(&self) -> BindingId {
        self.binding_id
    }

    pub const fn capability(&self) -> &ResultCapabilityIdentity {
        &self.capability
    }

    pub const fn schema_digest(&self) -> ProvenanceDigest {
        self.schema_digest
    }

    pub const fn authorization_snapshot_digest(&self) -> ProvenanceDigest {
        self.authorization_snapshot_digest
    }

    pub const fn grant_snapshot_digest(&self) -> ProvenanceDigest {
        self.grant_snapshot_digest
    }

    pub const fn delegation(&self) -> Option<DelegationProvenance> {
        self.delegation
    }

    pub const fn trace_id(&self) -> &TraceId {
        &self.trace_id
    }

    pub const fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }

    pub const fn remaining_budget(&self) -> Spend {
        self.remaining_budget
    }
}

#[derive(Debug, PartialEq)]
pub struct CanonicalResult {
    status: InvocationStatus,
    content: Option<Value>,
    error_code: Option<Arc<str>>,
    charged: bool,
    artifacts: Arc<[ArtifactReference]>,
    provenance: CallProvenance,
    canonical_bytes: Arc<[u8]>,
    digest: CanonicalResultDigest,
}

impl CanonicalResult {
    pub fn new<I>(
        status: InvocationStatus,
        content: Option<Value>,
        error_code: Option<&str>,
        charged: bool,
        artifacts: I,
        provenance: CallProvenance,
    ) -> Result<Self, ResultError>
    where
        I: IntoIterator<Item = ArtifactReference>,
    {
        let mut content = content;
        if let Err(error) = validate_status(status, content.as_ref(), error_code, charged) {
            drop_value_iteratively(content.take());
            return Err(error);
        }
        if let Some(value) = content.as_ref()
            && let Err(error) = preflight_value(value)
        {
            drop_value_iteratively(content.take());
            return Err(error);
        }
        let artifacts = collect_artifacts(artifacts)?;
        if let Some(content) = content.as_mut() {
            canonicalize_value(content);
        }
        let error_code = error_code.map(Arc::from);
        Self::build(status, content, error_code, charged, artifacts, provenance)
    }

    pub fn from_canonical_bytes(bytes: impl AsRef<[u8]>) -> Result<Self, ResultError> {
        let bytes = bytes.as_ref();
        if bytes.len() > MAX_CANONICAL_RESULT_BYTES {
            return Err(ResultError::ResultTooLarge);
        }
        preflight_json(bytes)?;
        let value = serde_json::from_slice::<UniqueValue>(bytes)
            .map_err(|_| ResultError::InvalidJson)?
            .0;
        let schema_version = value
            .as_object()
            .and_then(|value| value.get("schema_version"))
            .and_then(Value::as_u64)
            .ok_or(ResultError::InvalidJson)?;
        if schema_version != u64::from(CANONICAL_RESULT_SCHEMA_VERSION) {
            return Err(ResultError::UnsupportedVersion);
        }
        let wire: DecodedWireResultV1 =
            serde_json::from_value(value).map_err(|_| ResultError::InvalidJson)?;
        if wire.schema_version != CANONICAL_RESULT_SCHEMA_VERSION {
            return Err(ResultError::UnsupportedVersion);
        }
        let status = InvocationStatus::from(wire.status);
        let content = if status == InvocationStatus::Succeeded {
            Some(&wire.content)
        } else if wire.content.is_null() {
            None
        } else {
            return Err(ResultError::InvalidStatus);
        };
        validate_status(status, content, wire.error_code.as_deref(), wire.charged)?;
        let artifacts = collect_artifacts(
            wire.artifacts
                .iter()
                .map(|value| {
                    ArtifactReference::parse(value).map_err(|_| ResultError::InvalidArtifact)
                })
                .collect::<Result<Vec<_>, _>>()?,
        )?;
        let provenance = CallProvenance::from_wire(&wire)?;
        let content = if status == InvocationStatus::Succeeded {
            Some(wire.content)
        } else {
            None
        };
        let result = Self::build(
            status,
            content,
            wire.error_code.map(Arc::from),
            wire.charged,
            artifacts,
            provenance,
        )?;
        if result.canonical_bytes.as_ref() != bytes {
            return Err(ResultError::NonCanonical);
        }
        Ok(result)
    }

    fn build(
        status: InvocationStatus,
        content: Option<Value>,
        error_code: Option<Arc<str>>,
        charged: bool,
        artifacts: Vec<ArtifactReference>,
        provenance: CallProvenance,
    ) -> Result<Self, ResultError> {
        let wire = WireResultV1::from_parts(
            status,
            content.as_ref(),
            error_code.as_deref(),
            charged,
            &artifacts,
            &provenance,
        );
        let mut writer = CappedWriter::new(MAX_CANONICAL_RESULT_BYTES);
        if serde_json::to_writer(&mut writer, &wire).is_err() {
            return Err(if writer.exceeded {
                ResultError::ResultTooLarge
            } else {
                ResultError::InvalidJson
            });
        }
        let canonical_bytes = writer.bytes;
        let digest = CanonicalResultDigest::new(&canonical_bytes);
        Ok(Self {
            status,
            content,
            error_code,
            charged,
            artifacts: artifacts.into(),
            provenance,
            canonical_bytes: canonical_bytes.into(),
            digest,
        })
    }

    pub const fn status(&self) -> InvocationStatus {
        self.status
    }

    pub const fn content(&self) -> Option<&Value> {
        self.content.as_ref()
    }

    pub fn error_code(&self) -> Option<&str> {
        self.error_code.as_deref()
    }

    pub const fn charged(&self) -> bool {
        self.charged
    }

    pub fn artifacts(&self) -> &[ArtifactReference] {
        &self.artifacts
    }

    pub const fn provenance(&self) -> &CallProvenance {
        &self.provenance
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }

    pub const fn digest(&self) -> CanonicalResultDigest {
        self.digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Presentation {
    canonical_result_digest: CanonicalResultDigest,
    encoding: Arc<str>,
    spec_version: Arc<str>,
    body: Arc<str>,
}

impl Presentation {
    pub fn new(
        canonical: &CanonicalResult,
        encoding: impl AsRef<str>,
        spec_version: impl AsRef<str>,
        body: impl AsRef<str>,
    ) -> Result<Self, ResultError> {
        let encoding = encoding.as_ref();
        let spec_version = spec_version.as_ref();
        let body = body.as_ref();
        if !matches!(encoding, "json" | "text" | "table" | "artifact" | "toon")
            || !valid_text(spec_version, MAX_PRESENTATION_NAME_BYTES)
            || (encoding == "toon" && spec_version != "3.3")
        {
            return Err(ResultError::InvalidPresentation);
        }
        if body.len() > MAX_PRESENTATION_BYTES {
            return Err(ResultError::PresentationTooLarge);
        }
        let serialized = serde_json::to_vec(&PresentationSize {
            body,
            canonical_result_digest: canonical.digest().to_string(),
            encoding,
            spec_version,
        })
        .map_err(|_| ResultError::InvalidPresentation)?;
        if serialized.len() > MAX_PRESENTATION_BYTES {
            return Err(ResultError::PresentationTooLarge);
        }
        Ok(Self {
            canonical_result_digest: canonical.digest(),
            encoding: encoding.into(),
            spec_version: spec_version.into(),
            body: body.into(),
        })
    }

    pub fn encoding(&self) -> &str {
        &self.encoding
    }

    pub fn spec_version(&self) -> &str {
        &self.spec_version
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub const fn canonical_result_digest(&self) -> CanonicalResultDigest {
        self.canonical_result_digest
    }
}

#[derive(Clone, Debug)]
pub struct PresentedResult {
    canonical: Arc<CanonicalResult>,
    presentation: Presentation,
}

impl PresentedResult {
    pub fn new(
        canonical: Arc<CanonicalResult>,
        presentation: Presentation,
    ) -> Result<Self, ResultError> {
        if presentation.canonical_result_digest != canonical.digest() {
            return Err(ResultError::InvalidPresentation);
        }
        Ok(Self {
            canonical,
            presentation,
        })
    }

    pub fn canonical(&self) -> &Arc<CanonicalResult> {
        &self.canonical
    }

    pub const fn presentation(&self) -> &Presentation {
        &self.presentation
    }

    pub fn with_presentation(&self, presentation: Presentation) -> Result<Self, ResultError> {
        Self::new(Arc::clone(&self.canonical), presentation)
    }
}

fn validate_status(
    status: InvocationStatus,
    content: Option<&Value>,
    error_code: Option<&str>,
    charged: bool,
) -> Result<(), ResultError> {
    if error_code.is_some_and(|value| {
        value.is_empty()
            || value.len() > MAX_RESULT_ERROR_CODE_BYTES
            || !value
                .bytes()
                .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b':' | b'-'))
    }) {
        return Err(ResultError::InvalidErrorCode);
    }
    match status {
        InvocationStatus::Succeeded if content.is_some() && error_code.is_none() && charged => {
            Ok(())
        }
        InvocationStatus::Succeeded => Err(ResultError::InvalidStatus),
        InvocationStatus::Failed if content.is_none() && error_code.is_some() && charged => Ok(()),
        InvocationStatus::ApprovalRequired
        | InvocationStatus::ApprovalDenied
        | InvocationStatus::Cancelled
            if content.is_none() && error_code.is_some() && !charged =>
        {
            Ok(())
        }
        InvocationStatus::OutcomeUnknown if content.is_none() && error_code.is_some() => Ok(()),
        _ => Err(ResultError::InvalidStatus),
    }
}

fn collect_artifacts<I>(artifacts: I) -> Result<Vec<ArtifactReference>, ResultError>
where
    I: IntoIterator<Item = ArtifactReference>,
{
    let mut sorted = BTreeSet::new();
    for (index, artifact) in artifacts.into_iter().enumerate() {
        if index == MAX_RESULT_ARTIFACTS {
            return Err(ResultError::TooManyArtifacts);
        }
        if !sorted.insert(artifact) {
            return Err(ResultError::DuplicateArtifact);
        }
    }
    Ok(sorted.into_iter().collect())
}

fn valid_text(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}

fn hex_nibble(value: u8) -> Result<u8, ResultError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(ResultError::InvalidProvenance),
    }
}

fn preflight_value(root: &Value) -> Result<(), ResultError> {
    let mut stack = vec![(root, 1_usize)];
    let mut nodes = 0_usize;
    let mut weight = 0_usize;
    while let Some((value, depth)) = stack.pop() {
        nodes = nodes.checked_add(1).ok_or(ResultError::InvalidJson)?;
        if nodes > MAX_RESULT_JSON_NODES {
            return Err(ResultError::InvalidJson);
        }
        match value {
            Value::Null => add_weight(&mut weight, 4),
            Value::Bool(value) => add_weight(&mut weight, if *value { 4 } else { 5 }),
            Value::Number(value) => add_weight(&mut weight, value.to_string().len()),
            Value::String(value) => add_weight(&mut weight, json_string_len(value)),
            Value::Array(values) => {
                let depth = depth.checked_add(1).ok_or(ResultError::InvalidJson)?;
                if depth > MAX_RESULT_JSON_DEPTH {
                    return Err(ResultError::InvalidJson);
                }
                ensure_children_fit(nodes, stack.len(), values.len())?;
                add_weight(&mut weight, 2 + values.len().saturating_sub(1));
                stack.extend(values.iter().rev().map(|value| (value, depth)));
            }
            Value::Object(values) => {
                let depth = depth.checked_add(1).ok_or(ResultError::InvalidJson)?;
                if depth > MAX_RESULT_JSON_DEPTH {
                    return Err(ResultError::InvalidJson);
                }
                ensure_children_fit(nodes, stack.len(), values.len())?;
                add_weight(&mut weight, 2 + values.len().saturating_sub(1));
                for (key, value) in values.iter().rev() {
                    add_weight(&mut weight, json_string_len(key).saturating_add(1));
                    stack.push((value, depth));
                }
            }
        }
    }
    if weight > MAX_CANONICAL_RESULT_BYTES {
        return Err(ResultError::ResultTooLarge);
    }
    Ok(())
}

fn ensure_children_fit(nodes: usize, pending: usize, children: usize) -> Result<(), ResultError> {
    if nodes.saturating_add(pending).saturating_add(children) > MAX_RESULT_JSON_NODES {
        return Err(ResultError::InvalidJson);
    }
    Ok(())
}

fn drop_value_iteratively(value: Option<Value>) {
    let mut stack = value.into_iter().collect::<Vec<_>>();
    while let Some(value) = stack.pop() {
        match value {
            Value::Array(mut values) => stack.append(&mut values),
            Value::Object(values) => stack.extend(values.into_iter().map(|(_, value)| value)),
            _ => {}
        }
    }
}

fn add_weight(weight: &mut usize, value: usize) {
    *weight = weight.saturating_add(value);
}

fn json_string_len(value: &str) -> usize {
    value.bytes().fold(2_usize, |length, byte| {
        length.saturating_add(match byte {
            b'"' | b'\\' | b'\x08' | b'\t' | b'\n' | b'\x0c' | b'\r' => 2,
            0..=0x1f => 6,
            _ => 1,
        })
    })
}

fn canonicalize_value(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(canonicalize_value),
        Value::Object(values) => {
            for value in values.values_mut() {
                canonicalize_value(value);
            }
            values.sort_keys();
        }
        _ => {}
    }
}

#[derive(Clone, Copy)]
enum JsonContainer {
    Array,
    Object,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum JsonExpectation {
    Value,
    ArrayValueOrEnd,
    ObjectKeyOrEnd,
    ObjectKey,
    Colon,
    ArrayCommaOrEnd,
    ObjectCommaOrEnd,
    End,
}

fn preflight_json(bytes: &[u8]) -> Result<(), ResultError> {
    std::str::from_utf8(bytes).map_err(|_| ResultError::InvalidJson)?;
    let mut stack = Vec::with_capacity(MAX_RESULT_JSON_DEPTH);
    let mut expectation = JsonExpectation::Value;
    let mut nodes = 0_usize;
    let mut index = 0_usize;
    while index < bytes.len() {
        if matches!(bytes[index], b' ' | b'\n' | b'\r' | b'\t') {
            index += 1;
            continue;
        }
        match expectation {
            JsonExpectation::Value | JsonExpectation::ArrayValueOrEnd => {
                if expectation == JsonExpectation::ArrayValueOrEnd && bytes[index] == b']' {
                    stack.pop();
                    index += 1;
                    expectation = completed_value(&stack);
                    continue;
                }
                add_json_node(&mut nodes)?;
                match bytes[index] {
                    b'{' => {
                        if stack.len() == MAX_RESULT_JSON_DEPTH {
                            return Err(ResultError::InvalidJson);
                        }
                        stack.push(JsonContainer::Object);
                        expectation = JsonExpectation::ObjectKeyOrEnd;
                        index += 1;
                    }
                    b'[' => {
                        if stack.len() == MAX_RESULT_JSON_DEPTH {
                            return Err(ResultError::InvalidJson);
                        }
                        stack.push(JsonContainer::Array);
                        expectation = JsonExpectation::ArrayValueOrEnd;
                        index += 1;
                    }
                    b'"' => {
                        index = scan_json_string(bytes, index)?;
                        expectation = completed_value(&stack);
                    }
                    b't' if bytes[index..].starts_with(b"true") => {
                        index += 4;
                        expectation = completed_value(&stack);
                    }
                    b'f' if bytes[index..].starts_with(b"false") => {
                        index += 5;
                        expectation = completed_value(&stack);
                    }
                    b'n' if bytes[index..].starts_with(b"null") => {
                        index += 4;
                        expectation = completed_value(&stack);
                    }
                    b'-' | b'0'..=b'9' => {
                        index = scan_json_number(bytes, index)?;
                        expectation = completed_value(&stack);
                    }
                    _ => return Err(ResultError::InvalidJson),
                }
            }
            JsonExpectation::ObjectKeyOrEnd => {
                if bytes[index] == b'}' {
                    stack.pop();
                    index += 1;
                    expectation = completed_value(&stack);
                } else if bytes[index] == b'"' {
                    index = scan_json_string(bytes, index)?;
                    expectation = JsonExpectation::Colon;
                } else {
                    return Err(ResultError::InvalidJson);
                }
            }
            JsonExpectation::ObjectKey => {
                if bytes[index] != b'"' {
                    return Err(ResultError::InvalidJson);
                }
                index = scan_json_string(bytes, index)?;
                expectation = JsonExpectation::Colon;
            }
            JsonExpectation::Colon => {
                if bytes[index] != b':' {
                    return Err(ResultError::InvalidJson);
                }
                index += 1;
                expectation = JsonExpectation::Value;
            }
            JsonExpectation::ArrayCommaOrEnd => match bytes[index] {
                b',' => {
                    index += 1;
                    expectation = JsonExpectation::Value;
                }
                b']' => {
                    stack.pop();
                    index += 1;
                    expectation = completed_value(&stack);
                }
                _ => return Err(ResultError::InvalidJson),
            },
            JsonExpectation::ObjectCommaOrEnd => match bytes[index] {
                b',' => {
                    index += 1;
                    expectation = JsonExpectation::ObjectKey;
                }
                b'}' => {
                    stack.pop();
                    index += 1;
                    expectation = completed_value(&stack);
                }
                _ => return Err(ResultError::InvalidJson),
            },
            JsonExpectation::End => return Err(ResultError::InvalidJson),
        }
    }
    if expectation == JsonExpectation::End && stack.is_empty() {
        Ok(())
    } else {
        Err(ResultError::InvalidJson)
    }
}

fn completed_value(stack: &[JsonContainer]) -> JsonExpectation {
    match stack.last() {
        Some(JsonContainer::Array) => JsonExpectation::ArrayCommaOrEnd,
        Some(JsonContainer::Object) => JsonExpectation::ObjectCommaOrEnd,
        None => JsonExpectation::End,
    }
}

fn add_json_node(nodes: &mut usize) -> Result<(), ResultError> {
    *nodes = nodes.checked_add(1).ok_or(ResultError::InvalidJson)?;
    if *nodes > MAX_RESULT_JSON_NODES {
        return Err(ResultError::InvalidJson);
    }
    Ok(())
}

fn scan_json_string(bytes: &[u8], mut index: usize) -> Result<usize, ResultError> {
    index += 1;
    loop {
        let byte = *bytes.get(index).ok_or(ResultError::InvalidJson)?;
        match byte {
            b'"' => return Ok(index + 1),
            b'\\' => {
                let escape = *bytes.get(index + 1).ok_or(ResultError::InvalidJson)?;
                match escape {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => index += 2,
                    b'u' => {
                        let code = scan_hex_quad(bytes, index + 2)?;
                        index += 6;
                        if (0xd800..=0xdbff).contains(&code) {
                            if bytes.get(index..index + 2) != Some(b"\\u") {
                                return Err(ResultError::InvalidJson);
                            }
                            let low = scan_hex_quad(bytes, index + 2)?;
                            if !(0xdc00..=0xdfff).contains(&low) {
                                return Err(ResultError::InvalidJson);
                            }
                            index += 6;
                        } else if (0xdc00..=0xdfff).contains(&code) {
                            return Err(ResultError::InvalidJson);
                        }
                    }
                    _ => return Err(ResultError::InvalidJson),
                }
            }
            0..=0x1f => return Err(ResultError::InvalidJson),
            _ => index += 1,
        }
    }
}

fn scan_hex_quad(bytes: &[u8], start: usize) -> Result<u16, ResultError> {
    let digits = bytes
        .get(start..start + 4)
        .ok_or(ResultError::InvalidJson)?;
    digits.iter().try_fold(0_u16, |value, byte| {
        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return Err(ResultError::InvalidJson),
        };
        Ok((value << 4) | u16::from(nibble))
    })
}

fn scan_json_number(bytes: &[u8], start: usize) -> Result<usize, ResultError> {
    let mut index = start;
    if bytes[index] == b'-' {
        index += 1;
    }
    match bytes.get(index) {
        Some(b'0') => index += 1,
        Some(b'1'..=b'9') => {
            index += 1;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
        }
        _ => return Err(ResultError::InvalidJson),
    }
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let fraction = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == fraction {
            return Err(ResultError::InvalidJson);
        }
    }
    if bytes
        .get(index)
        .is_some_and(|byte| matches!(byte, b'e' | b'E'))
    {
        index += 1;
        if bytes
            .get(index)
            .is_some_and(|byte| matches!(byte, b'+' | b'-'))
        {
            index += 1;
        }
        let exponent = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent {
            return Err(ResultError::InvalidJson);
        }
    }
    if !number_is_lossless(&bytes[start..index]) {
        return Err(ResultError::InvalidJson);
    }
    Ok(index)
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum WireStatusV1 {
    Succeeded,
    Failed,
    ApprovalRequired,
    ApprovalDenied,
    Cancelled,
    OutcomeUnknown,
}

impl From<InvocationStatus> for WireStatusV1 {
    fn from(value: InvocationStatus) -> Self {
        match value {
            InvocationStatus::Succeeded => Self::Succeeded,
            InvocationStatus::Failed => Self::Failed,
            InvocationStatus::ApprovalRequired => Self::ApprovalRequired,
            InvocationStatus::ApprovalDenied => Self::ApprovalDenied,
            InvocationStatus::Cancelled => Self::Cancelled,
            InvocationStatus::OutcomeUnknown => Self::OutcomeUnknown,
        }
    }
}

impl From<WireStatusV1> for InvocationStatus {
    fn from(value: WireStatusV1) -> Self {
        match value {
            WireStatusV1::Succeeded => Self::Succeeded,
            WireStatusV1::Failed => Self::Failed,
            WireStatusV1::ApprovalRequired => Self::ApprovalRequired,
            WireStatusV1::ApprovalDenied => Self::ApprovalDenied,
            WireStatusV1::Cancelled => Self::Cancelled,
            WireStatusV1::OutcomeUnknown => Self::OutcomeUnknown,
        }
    }
}

#[derive(Serialize)]
struct WireResultV1<'a> {
    artifacts: Vec<String>,
    authorization_snapshot_digest: String,
    binding_id: String,
    capability: WireCapability,
    charged: bool,
    content: Option<&'a Value>,
    delegation: Option<WireDelegation>,
    error_code: Option<&'a str>,
    grant_snapshot_digest: String,
    idempotency_key: String,
    invocation_id: String,
    parent_invocation_id: Option<String>,
    principal_id: String,
    remaining_budget: WireSpend,
    schema_digest: String,
    schema_version: u16,
    status: WireStatusV1,
    trace_id: String,
}

impl<'a> WireResultV1<'a> {
    fn from_parts(
        status: InvocationStatus,
        content: Option<&'a Value>,
        error_code: Option<&'a str>,
        charged: bool,
        artifacts: &[ArtifactReference],
        provenance: &CallProvenance,
    ) -> Self {
        Self {
            artifacts: artifacts.iter().map(ToString::to_string).collect(),
            authorization_snapshot_digest: provenance.authorization_snapshot_digest.to_string(),
            binding_id: provenance.binding_id.to_string(),
            capability: WireCapability::from(&provenance.capability),
            charged,
            content,
            delegation: provenance.delegation.map(WireDelegation::from),
            error_code,
            grant_snapshot_digest: provenance.grant_snapshot_digest.to_string(),
            idempotency_key: provenance.idempotency_key.to_string(),
            invocation_id: provenance.invocation_id.to_string(),
            parent_invocation_id: provenance
                .parent_invocation_id
                .map(|value| value.to_string()),
            principal_id: provenance.principal_id.to_string(),
            remaining_budget: provenance.remaining_budget.into(),
            schema_digest: provenance.schema_digest.to_string(),
            schema_version: CANONICAL_RESULT_SCHEMA_VERSION,
            status: status.into(),
            trace_id: provenance.trace_id.to_string(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodedWireResultV1 {
    artifacts: Vec<String>,
    authorization_snapshot_digest: String,
    binding_id: String,
    capability: WireCapability,
    charged: bool,
    content: Value,
    delegation: Option<WireDelegation>,
    error_code: Option<String>,
    grant_snapshot_digest: String,
    idempotency_key: String,
    invocation_id: String,
    parent_invocation_id: Option<String>,
    principal_id: String,
    remaining_budget: WireSpend,
    schema_digest: String,
    schema_version: u16,
    status: WireStatusV1,
    trace_id: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireCapability {
    implementation_digest: String,
    name: String,
    namespace: String,
    source: String,
    version: String,
}

impl From<&ResultCapabilityIdentity> for WireCapability {
    fn from(value: &ResultCapabilityIdentity) -> Self {
        Self {
            implementation_digest: value.implementation_digest.to_string(),
            name: value.name.to_string(),
            namespace: value.namespace.to_string(),
            source: value.source.to_string(),
            version: value.version.to_string(),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireDelegation {
    depth: u16,
    digest: String,
    maximum_depth: u16,
}

impl From<DelegationProvenance> for WireDelegation {
    fn from(value: DelegationProvenance) -> Self {
        Self {
            depth: value.depth,
            digest: value.digest.to_string(),
            maximum_depth: value.maximum_depth,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireSpend {
    cost_microusd: u64,
    processes: u64,
    tokens: u64,
    tools: u64,
    turns: u64,
}

impl From<Spend> for WireSpend {
    fn from(value: Spend) -> Self {
        Self {
            cost_microusd: value.cost_microusd(),
            processes: value.processes(),
            tokens: value.tokens(),
            tools: value.tools(),
            turns: value.turns(),
        }
    }
}

impl From<WireSpend> for Spend {
    fn from(value: WireSpend) -> Self {
        Self::new(
            value.cost_microusd,
            value.tokens,
            value.turns,
            value.tools,
            value.processes,
        )
    }
}

#[derive(Serialize)]
struct PresentationSize<'a> {
    body: &'a str,
    canonical_result_digest: String,
    encoding: &'a str,
    spec_version: &'a str,
}

struct CappedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl CappedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(4096)),
            limit,
            exceeded: false,
        }
    }
}

impl Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("canonical result byte limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueVisitor)
    }
}

struct UniqueVisitor;

impl<'de> Visitor<'de> for UniqueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(Number::from(value))))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(Number::from(value))))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .map(UniqueValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<UniqueValue>()? {
            values.push(value.0);
        }
        Ok(UniqueValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut input: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = BTreeMap::new();
        while let Some((key, value)) = input.next_entry::<String, UniqueValue>()? {
            if values.insert(key, value.0).is_some() {
                return Err(serde::de::Error::custom("duplicate JSON object key"));
            }
        }
        Ok(UniqueValue(Value::Object(
            values.into_iter().collect::<Map<_, _>>(),
        )))
    }
}
