use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::{
    api::service::AttemptDriverClaim,
    domain::{
        commands::ExpectedVersion,
        events::{EntityId, EventType, SchemaVersion, TraceId, UtcDateTime},
        ids::{CommandId, EventId, ProjectId, RunId},
        lifecycle::AttemptOwnership,
    },
    store::sqlite::{
        append::{
            AppendCommand, AppendOutcome, ExpectedStreamVersion, NewEvent, SqliteStore, StoreError,
        },
        idempotency::{CanonicalRequestDigest, IdempotencyKey, IdempotencyScope},
    },
};

pub const TOOL_LEARNING_FORMAT: &str = "tool_learning.v1";
pub const TOOL_LEARNING_SCHEMA_VERSION: u16 = 1;
pub const MAX_LEARNING_EVENTS: usize = 10_000;
pub const MAX_LEARNING_CANDIDATES: u16 = 10_000;
pub const MAX_LEARNING_ANALYSIS_INPUT_DIGESTS: u64 = 40_000;
pub const MAX_SEQUENCE_COST_MICROUSD: u64 = 1_000_000_000_000_000;
pub const MAX_SEQUENCE_LATENCY_MS: u64 = 31_536_000_000;

const EVENT_TYPE: &str = "tool_learning.recorded";
const POINTER_PREFIX: &str = "tlp_v1_";

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct LearningPointer(String);

impl LearningPointer {
    pub fn parse(value: impl Into<String>) -> Result<Self, ToolLearningError> {
        let value = value.into();
        parse_pointer(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn domain(&self) -> Result<PointerDomain, ToolLearningError> {
        parse_pointer(&self.0).map(|(domain, _)| domain)
    }
}

#[derive(Clone)]
pub struct ProjectPointerHasher {
    key: [u8; 32],
    project: LearningPointer,
}

impl ProjectPointerHasher {
    pub fn new(project_id: ProjectId, root_key: &[u8; 32]) -> Self {
        let project_key = blake3::keyed_hash(root_key, project_id.to_string().as_bytes());
        let key = blake3::derive_key(
            "kit tool learning project pointer v1",
            project_key.as_bytes(),
        );
        let mut hasher = Self {
            key,
            project: LearningPointer(String::new()),
        };
        hasher.project = hasher.pointer(PointerDomain::Project, project_id.to_string().as_bytes());
        hasher
    }

    pub fn pointer(&self, domain: PointerDomain, value: &[u8]) -> LearningPointer {
        let token = self.mac(domain, value);
        let authenticator = self.mac(domain, &token);
        let mut opaque = [0_u8; 64];
        opaque[..32].copy_from_slice(&token);
        opaque[32..].copy_from_slice(&authenticator);
        LearningPointer(format!(
            "{POINTER_PREFIX}{:02x}_{}",
            domain.tag(),
            hex(&opaque)
        ))
    }

    pub fn validate(
        &self,
        pointer: &LearningPointer,
        expected: PointerDomain,
    ) -> Result<(), ToolLearningError> {
        let (domain, supplied) = parse_pointer(pointer.as_str())?;
        if domain != expected {
            return Err(ToolLearningError::InvalidPointer);
        }
        let expected = self.mac(domain, &supplied[..32]);
        if !constant_time_eq(
            &expected,
            supplied[32..].try_into().expect("pointer tag length"),
        ) {
            return Err(ToolLearningError::InvalidPointer);
        }
        Ok(())
    }

    fn mac(&self, domain: PointerDomain, value: &[u8]) -> [u8; 32] {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.key)
            .expect("SHA-256 HMAC accepts a 32-byte key");
        mac.update(TOOL_LEARNING_FORMAT.as_bytes());
        mac.update(&[domain.tag()]);
        mac.update(&(value.len() as u64).to_be_bytes());
        mac.update(value);
        mac.finalize().into_bytes().into()
    }

    pub(crate) fn encrypt_export_frame(
        &self,
        frame_id: &str,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, ToolLearningError> {
        let key = blake3::derive_key("kit tool learning encrypted export v1", &self.key);
        let mut nonce = [0_u8; 24];
        getrandom::fill(&mut nonce).map_err(|_| ToolLearningError::Encryption)?;
        let ciphertext = XChaCha20Poly1305::new((&key).into())
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: plaintext,
                    aad: frame_id.as_bytes(),
                },
            )
            .map_err(|_| ToolLearningError::Encryption)?;
        let mut frame = nonce.to_vec();
        frame.extend_from_slice(&ciphertext);
        Ok(frame)
    }

    pub(crate) fn project(&self) -> &LearningPointer {
        &self.project
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerDomain {
    Event,
    Project,
    Run,
    Capability,
    Schema,
    Query,
    Handle,
    Binding,
    Call,
    Request,
    Source,
    Experiment,
    Artifact,
    KernelEvent,
    Sequence,
    Field,
}

impl PointerDomain {
    const fn tag(self) -> u8 {
        match self {
            Self::Event => 0,
            Self::Project => 1,
            Self::Run => 2,
            Self::Capability => 3,
            Self::Schema => 4,
            Self::Query => 5,
            Self::Handle => 6,
            Self::Binding => 7,
            Self::Call => 8,
            Self::Request => 9,
            Self::Source => 10,
            Self::Experiment => 11,
            Self::Artifact => 12,
            Self::KernelEvent => 13,
            Self::Sequence => 14,
            Self::Field => 15,
        }
    }

    fn from_tag(tag: u8) -> Option<Self> {
        Some(match tag {
            0 => Self::Event,
            1 => Self::Project,
            2 => Self::Run,
            3 => Self::Capability,
            4 => Self::Schema,
            5 => Self::Query,
            6 => Self::Handle,
            7 => Self::Binding,
            8 => Self::Call,
            9 => Self::Request,
            10 => Self::Source,
            11 => Self::Experiment,
            12 => Self::Artifact,
            13 => Self::KernelEvent,
            14 => Self::Sequence,
            15 => Self::Field,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningOperation {
    Projection,
    Search,
    Inspect,
    Bind,
    Invoke,
}

impl LearningOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Projection => "projection",
            Self::Search => "search",
            Self::Inspect => "inspect",
            Self::Bind => "bind",
            Self::Invoke => "invoke",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningSurface {
    Eager,
    Deferred,
    Generic,
    Discovery,
}

impl LearningSurface {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eager => "eager",
            Self::Deferred => "deferred",
            Self::Generic => "generic",
            Self::Discovery => "discovery",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningCapabilityKind {
    Tool,
    Resource,
    Prompt,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LearningStatus {
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
    OutcomeUnknown,
    Unavailable,
}

impl LearningStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorStage {
    Routing,
    Parsing,
    SchemaValidation,
    Authorization,
    Dispatch,
    Transport,
    ResultValidation,
    Persistence,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Input,
    Policy,
    Budget,
    Auth,
    Transport,
    Url,
    Remote,
    Result,
    Store,
    System,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    UnknownTool,
    MalformedInput,
    InvalidSchema,
    UnsupportedSchema,
    BindingExpired,
    StaleBinding,
    AuthorizationDenied,
    BudgetUnavailable,
    ApprovalRequired,
    AuthRequired,
    AuthDenied,
    CredentialUnavailable,
    EgressDenied,
    UrlElicitationRequired,
    UrlElicitationDeclined,
    Timeout,
    ConnectionRetired,
    QueueFull,
    SessionExpired,
    InvalidLimits,
    InvalidEndpoint,
    InvalidHeader,
    ResponseTooLarge,
    ProtocolVersionRefused,
    MissingPayload,
    ProcessUnavailable,
    RefreshClosed,
    RefreshRetriesExhausted,
    FeatureFailed,
    DiscoveryFailed,
    Io,
    Protocol,
    InvalidResponse,
    SensitiveResponse,
    PersistenceFailed,
    Cancelled,
    OutcomeUnknown,
    Internal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryClass {
    Never,
    Safe,
    AuthorizationResume,
    UrlResume,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentArm {
    Direct,
    Competing,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DownstreamGrade {
    Passed,
    Failed,
    Harmful,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LearningCommon {
    pub format: String,
    pub schema_version: u16,
    pub event_id: LearningPointer,
    pub project: LearningPointer,
    pub run: LearningPointer,
    pub ordinal: u64,
    pub operation: LearningOperation,
    pub surface: LearningSurface,
    pub request: Option<LearningPointer>,
    pub capability: Option<LearningPointer>,
    pub schema: Option<LearningPointer>,
}

#[derive(Clone)]
pub struct PreparedLearningCapture {
    hasher: ProjectPointerHasher,
    run_id: RunId,
    turn: String,
    operation_sequence: u64,
    route: String,
    provider_call_id: String,
    request_digest: [u8; 32],
    surface: LearningSurface,
    capability: LearningPointer,
    schema: LearningPointer,
    binding: Option<LearningPointer>,
    source: LearningPointer,
    kind: LearningCapabilityKind,
    sequence: LearningPointer,
    telemetry: Option<Arc<crate::runtime::telemetry::TelemetryRuntime<'static>>>,
}

impl PreparedLearningCapture {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hasher: ProjectPointerHasher,
        run_id: RunId,
        turn: impl Into<String>,
        operation_sequence: u64,
        route: impl Into<String>,
        provider_call_id: impl Into<String>,
        request: &[u8],
        surface: LearningSurface,
        capability: &[u8],
        schema: &[u8],
        binding: Option<&[u8]>,
        source: &[u8],
        kind: LearningCapabilityKind,
    ) -> Result<Self, ToolLearningError> {
        let turn = turn.into();
        let route = route.into();
        let provider_call_id = provider_call_id.into();
        if turn.is_empty()
            || route.is_empty()
            || provider_call_id.is_empty()
            || turn.len() > 256
            || route.len() > 256
            || provider_call_id.len() > 256
        {
            return Err(ToolLearningError::InvalidRecord);
        }
        let sequence = hasher.pointer(
            PointerDomain::Sequence,
            format!("{run_id}:{turn}").as_bytes(),
        );
        Ok(Self {
            capability: hasher.pointer(PointerDomain::Capability, capability),
            schema: hasher.pointer(PointerDomain::Schema, schema),
            binding: binding.map(|value| hasher.pointer(PointerDomain::Binding, value)),
            source: hasher.pointer(PointerDomain::Source, source),
            hasher,
            run_id,
            turn,
            operation_sequence,
            route,
            provider_call_id,
            request_digest: crate::domain::crypto::sha256(request),
            surface,
            kind,
            sequence,
            telemetry: None,
        })
    }

    pub(crate) fn with_telemetry(
        mut self,
        telemetry: Option<Arc<crate::runtime::telemetry::TelemetryRuntime<'static>>>,
    ) -> Self {
        self.telemetry = telemetry;
        self
    }

    pub(crate) fn required(&self) -> bool {
        self.telemetry
            .as_ref()
            .is_some_and(|telemetry| telemetry.learning_required())
    }

    pub(crate) fn mark_failure(&self, error: &ToolLearningError) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.mark_learning_failure(error.to_string());
        }
    }

    fn call(&self) -> LearningPointer {
        let mut identity = Vec::new();
        let run = self
            .hasher
            .pointer(PointerDomain::Run, self.run_id.to_string().as_bytes());
        let operation_sequence = self.operation_sequence.to_be_bytes();
        for value in [
            run.as_str().as_bytes(),
            self.turn.as_bytes(),
            &operation_sequence,
            self.route.as_bytes(),
            self.provider_call_id.as_bytes(),
            &self.request_digest,
        ] {
            identity.extend_from_slice(&(value.len() as u64).to_be_bytes());
            identity.extend_from_slice(value);
        }
        self.hasher.pointer(PointerDomain::Call, &identity)
    }

    pub fn call_pointer(&self) -> LearningPointer {
        self.call()
    }

    pub(crate) fn hasher(&self) -> &ProjectPointerHasher {
        &self.hasher
    }

    fn common(&self, ordinal: u64, request_digest: &[u8; 32], suffix: &[u8]) -> LearningCommon {
        LearningCommon::new(
            &self.hasher,
            self.run_id,
            ordinal,
            LearningOperation::Invoke,
            self.surface,
            suffix,
            Some(self.hasher.pointer(PointerDomain::Request, request_digest)),
            Some(self.capability.clone()),
            Some(self.schema.clone()),
        )
    }

    pub(crate) fn field(&self, instance_path: &str) -> LearningPointer {
        self.hasher.pointer(
            PointerDomain::Field,
            format!("{}:{instance_path}", self.schema.as_str()).as_bytes(),
        )
    }
}

#[derive(Clone)]
pub(crate) struct LearningFailure {
    pub stage: ErrorStage,
    pub class: ErrorClass,
    pub code: ErrorCode,
    pub field: Option<LearningPointer>,
    pub retry: RetryClass,
    pub dispatched: bool,
    pub known: bool,
}

impl LearningCommon {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        hasher: &ProjectPointerHasher,
        run_id: RunId,
        ordinal: u64,
        operation: LearningOperation,
        surface: LearningSurface,
        operation_identity: &[u8],
        request: Option<LearningPointer>,
        capability: Option<LearningPointer>,
        schema: Option<LearningPointer>,
    ) -> Self {
        let run = hasher.pointer(PointerDomain::Run, run_id.to_string().as_bytes());
        let mut identity = Vec::new();
        identity.extend_from_slice(run.as_str().as_bytes());
        identity.extend_from_slice(&ordinal.to_be_bytes());
        identity.push(operation as u8);
        identity.push(surface as u8);
        identity.extend_from_slice(&(operation_identity.len() as u64).to_be_bytes());
        identity.extend_from_slice(operation_identity);
        if let Some(request) = &request {
            identity.extend_from_slice(request.as_str().as_bytes());
        }
        Self {
            format: TOOL_LEARNING_FORMAT.to_owned(),
            schema_version: TOOL_LEARNING_SCHEMA_VERSION,
            event_id: hasher.pointer(PointerDomain::Event, &identity),
            project: hasher.project().clone(),
            run,
            ordinal,
            operation,
            surface,
            request,
            capability,
            schema,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LearningCandidate {
    pub capability: LearningPointer,
    pub schema: LearningPointer,
    pub surface: LearningSurface,
    pub authorized: bool,
    pub offered: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event_class", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolLearningEvent {
    Opportunity {
        common: LearningCommon,
        offered: u16,
        eager: u16,
        deferred: u16,
        generic_available: bool,
        projection: LearningPointer,
        candidates: Vec<LearningCandidate>,
        detail_artifact: Option<LearningPointer>,
    },
    Search {
        common: LearningCommon,
        query: LearningPointer,
        status: LearningStatus,
        result_count: u16,
        detail_artifact: Option<LearningPointer>,
    },
    Inspection {
        common: LearningCommon,
        handle: LearningPointer,
        status: LearningStatus,
    },
    Call {
        common: LearningCommon,
        call: LearningPointer,
        binding: Option<LearningPointer>,
        source: Option<LearningPointer>,
        kind: Option<LearningCapabilityKind>,
        sequence: Option<LearningPointer>,
        sequence_order: Option<u16>,
        kernel_intent: Option<LearningPointer>,
    },
    Error {
        common: LearningCommon,
        call: LearningPointer,
        stage: ErrorStage,
        class: ErrorClass,
        code: ErrorCode,
        field: Option<LearningPointer>,
        retry: RetryClass,
        dispatched: bool,
        known: bool,
    },
    Outcome {
        common: LearningCommon,
        call: LearningPointer,
        status: LearningStatus,
        dispatched: bool,
        known: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_microusd: Option<u64>,
        kernel_outcome: Option<LearningPointer>,
    },
}

impl ToolLearningEvent {
    pub const fn common(&self) -> &LearningCommon {
        match self {
            Self::Opportunity { common, .. }
            | Self::Search { common, .. }
            | Self::Inspection { common, .. }
            | Self::Call { common, .. }
            | Self::Error { common, .. }
            | Self::Outcome { common, .. } => common,
        }
    }

    pub const fn class_name(&self) -> &'static str {
        match self {
            Self::Opportunity { .. } => "opportunity",
            Self::Search { .. } => "search",
            Self::Inspection { .. } => "inspection",
            Self::Call { .. } => "call",
            Self::Error { .. } => "error",
            Self::Outcome { .. } => "outcome",
        }
    }

    fn common_mut(&mut self) -> &mut LearningCommon {
        match self {
            Self::Opportunity { common, .. }
            | Self::Search { common, .. }
            | Self::Inspection { common, .. }
            | Self::Call { common, .. }
            | Self::Error { common, .. }
            | Self::Outcome { common, .. } => common,
        }
    }

    pub fn validate(&self) -> Result<(), ToolLearningError> {
        let common = self.common();
        if common.format != TOOL_LEARNING_FORMAT
            || common.schema_version != TOOL_LEARNING_SCHEMA_VERSION
            || common.ordinal == 0
        {
            return Err(ToolLearningError::InvalidRecord);
        }
        structural_pointer(&common.event_id, PointerDomain::Event)?;
        structural_pointer(&common.project, PointerDomain::Project)?;
        structural_pointer(&common.run, PointerDomain::Run)?;
        optional_structural(&common.request, PointerDomain::Request)?;
        optional_structural(&common.capability, PointerDomain::Capability)?;
        optional_structural(&common.schema, PointerDomain::Schema)?;
        match self {
            Self::Opportunity {
                offered,
                eager,
                deferred,
                projection,
                candidates,
                detail_artifact,
                ..
            } => {
                if *offered > MAX_LEARNING_CANDIDATES
                    || *eager > *offered
                    || *deferred > *offered
                    || eager.saturating_add(*deferred) > *offered
                    || candidates.len() > usize::from(MAX_LEARNING_CANDIDATES)
                    || candidates
                        .iter()
                        .filter(|candidate| candidate.offered)
                        .count()
                        > usize::from(*offered)
                {
                    return Err(ToolLearningError::BoundExceeded);
                }
                structural_pointer(projection, PointerDomain::Schema)?;
                optional_structural(detail_artifact, PointerDomain::Artifact)?;
                for candidate in candidates {
                    structural_pointer(&candidate.capability, PointerDomain::Capability)?;
                    structural_pointer(&candidate.schema, PointerDomain::Schema)?;
                }
            }
            Self::Search {
                query,
                result_count,
                detail_artifact,
                ..
            } => {
                if *result_count > MAX_LEARNING_CANDIDATES {
                    return Err(ToolLearningError::BoundExceeded);
                }
                structural_pointer(query, PointerDomain::Query)?;
                optional_structural(detail_artifact, PointerDomain::Artifact)?;
            }
            Self::Inspection { handle, .. } => {
                if !matches!(
                    handle.domain()?,
                    PointerDomain::Handle | PointerDomain::Binding
                ) {
                    return Err(ToolLearningError::InvalidPointer);
                }
            }
            Self::Call {
                call,
                binding,
                source,
                sequence,
                sequence_order,
                kernel_intent,
                ..
            } => {
                structural_pointer(call, PointerDomain::Call)?;
                optional_structural(binding, PointerDomain::Binding)?;
                optional_structural(source, PointerDomain::Source)?;
                optional_structural(sequence, PointerDomain::Sequence)?;
                if sequence.is_some() != sequence_order.is_some()
                    || sequence_order.is_some_and(|order| order == 0)
                {
                    return Err(ToolLearningError::InvalidRecord);
                }
                optional_structural(kernel_intent, PointerDomain::KernelEvent)?;
            }
            Self::Error { call, field, .. } => {
                structural_pointer(call, PointerDomain::Call)?;
                optional_structural(field, PointerDomain::Field)?;
            }
            Self::Outcome {
                call,
                cost_microusd,
                kernel_outcome,
                ..
            } => {
                if cost_microusd.is_some_and(|cost| cost > MAX_SEQUENCE_COST_MICROUSD) {
                    return Err(ToolLearningError::BoundExceeded);
                }
                structural_pointer(call, PointerDomain::Call)?;
                optional_structural(kernel_outcome, PointerDomain::KernelEvent)?;
            }
        }
        Ok(())
    }

    pub(crate) fn validate_with(
        &self,
        hasher: &ProjectPointerHasher,
    ) -> Result<(), ToolLearningError> {
        self.validate()?;
        let common = self.common();
        hasher.validate(&common.event_id, PointerDomain::Event)?;
        hasher.validate(&common.project, PointerDomain::Project)?;
        if common.project != *hasher.project() || common.event_id != event_authority(self, hasher)?
        {
            return Err(ToolLearningError::InvalidRecord);
        }
        hasher.validate(&common.run, PointerDomain::Run)?;
        validate_optional(hasher, &common.request, PointerDomain::Request)?;
        validate_optional(hasher, &common.capability, PointerDomain::Capability)?;
        validate_optional(hasher, &common.schema, PointerDomain::Schema)?;
        match self {
            Self::Opportunity {
                projection,
                candidates,
                detail_artifact,
                ..
            } => {
                hasher.validate(projection, PointerDomain::Schema)?;
                validate_optional(hasher, detail_artifact, PointerDomain::Artifact)?;
                for candidate in candidates {
                    hasher.validate(&candidate.capability, PointerDomain::Capability)?;
                    hasher.validate(&candidate.schema, PointerDomain::Schema)?;
                }
            }
            Self::Search {
                query,
                detail_artifact,
                ..
            } => {
                hasher.validate(query, PointerDomain::Query)?;
                validate_optional(hasher, detail_artifact, PointerDomain::Artifact)?;
            }
            Self::Inspection { handle, .. } => {
                hasher.validate(handle, handle.domain()?)?;
            }
            Self::Call {
                call,
                binding,
                source,
                sequence,
                sequence_order: _,
                kernel_intent,
                ..
            } => {
                hasher.validate(call, PointerDomain::Call)?;
                validate_optional(hasher, binding, PointerDomain::Binding)?;
                validate_optional(hasher, source, PointerDomain::Source)?;
                validate_optional(hasher, sequence, PointerDomain::Sequence)?;
                validate_optional(hasher, kernel_intent, PointerDomain::KernelEvent)?;
            }
            Self::Error { call, field, .. } => {
                hasher.validate(call, PointerDomain::Call)?;
                validate_optional(hasher, field, PointerDomain::Field)?;
            }
            Self::Outcome {
                call,
                kernel_outcome,
                ..
            } => {
                hasher.validate(call, PointerDomain::Call)?;
                validate_optional(hasher, kernel_outcome, PointerDomain::KernelEvent)?;
            }
        }
        Ok(())
    }
}

fn seal_event(
    event: &ToolLearningEvent,
    hasher: &ProjectPointerHasher,
) -> Result<ToolLearningEvent, ToolLearningError> {
    let mut event = event.clone();
    let authority = event_authority(&event, hasher)?;
    event.common_mut().event_id = authority;
    Ok(event)
}

fn event_authority(
    event: &ToolLearningEvent,
    hasher: &ProjectPointerHasher,
) -> Result<LearningPointer, ToolLearningError> {
    let mut value = serde_json::to_value(event).map_err(|_| ToolLearningError::InvalidRecord)?;
    value
        .get_mut("common")
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|common| common.remove("event_id"))
        .ok_or(ToolLearningError::InvalidRecord)?;
    let canonical = serde_json::to_vec(&value).map_err(|_| ToolLearningError::InvalidRecord)?;
    Ok(hasher.pointer(PointerDomain::Event, &canonical))
}

pub fn next_ordinal(store: &SqliteStore, run_id: RunId) -> Result<u64, ToolLearningError> {
    let count = store
        .events_for_correlation(EntityId::Run(run_id), EVENT_TYPE, MAX_LEARNING_EVENTS + 1)?
        .len();
    if count > MAX_LEARNING_EVENTS {
        return Err(ToolLearningError::BoundExceeded);
    }
    u64::try_from(count)
        .map_err(|_| ToolLearningError::BoundExceeded)?
        .checked_add(1)
        .ok_or(ToolLearningError::BoundExceeded)
}

pub(crate) fn prepare_events(
    store: &SqliteStore,
    claim: AttemptDriverClaim,
    hasher: &ProjectPointerHasher,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
    events: &[ToolLearningEvent],
) -> Result<(ExpectedStreamVersion, Vec<NewEvent>), ToolLearningError> {
    if events.is_empty() {
        return Err(ToolLearningError::InvalidRecord);
    }
    let events = events
        .iter()
        .map(|event| seal_event(event, hasher))
        .collect::<Result<Vec<_>, _>>()?;
    let run_id = claim.run_id;
    let stream = EntityId::Run(run_id);
    let run_pointer = hasher.pointer(PointerDomain::Run, run_id.to_string().as_bytes());
    let existing = records(store, run_id, hasher)?;
    let first_ordinal = existing.len() as u64 + 1;
    for (index, event) in events.iter().enumerate() {
        event.validate_with(hasher)?;
        if event.common().run != run_pointer
            || event.common().ordinal != first_ordinal + index as u64
        {
            return Err(ToolLearningError::InvalidRecord);
        }
    }
    validate_sequence(existing.iter().chain(events.iter()))?;
    enforce_admission(&existing, &events)?;
    let expected = store.stream_version(stream)?;
    let prepared = events
        .iter()
        .map(|event| {
            let stable = event.common().event_id.as_str();
            Ok(NewEvent {
                id: EventId::from_stable_bytes(stable.as_bytes()),
                stream,
                event_type: EventType::parse(EVENT_TYPE)
                    .expect("tool learning event type is valid"),
                schema_version: SchemaVersion::CURRENT,
                occurred_at: occurred_at.clone(),
                causation_id: CommandId::from_stable_bytes(stable.as_bytes()),
                correlation_id: EntityId::Run(run_id),
                attempt_id: Some(claim.attempt_id),
                trace_id: trace_id.clone(),
                payload: serde_json::to_vec(event).map_err(|_| ToolLearningError::InvalidRecord)?,
                artifacts: b"[]".to_vec(),
            })
        })
        .collect::<Result<Vec<_>, ToolLearningError>>()?;
    Ok((
        ExpectedStreamVersion {
            stream,
            version: ExpectedVersion::new(expected),
        },
        prepared,
    ))
}

pub(crate) fn prepare_invocation_intent(
    store: &SqliteStore,
    claim: AttemptDriverClaim,
    capture: &PreparedLearningCapture,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
    request_digest: &[u8; 32],
    kernel_intent: EventId,
) -> Result<Option<(ExpectedStreamVersion, Vec<NewEvent>)>, ToolLearningError> {
    let call = capture.call();
    if records(store, capture.run_id, &capture.hasher)?
        .iter()
        .any(|event| matches!(event, ToolLearningEvent::Call { call: found, .. } if found == &call))
    {
        return Ok(None);
    }
    let ordinal = next_ordinal(store, capture.run_id)?;
    let event = ToolLearningEvent::Call {
        common: capture.common(ordinal, request_digest, call.as_str().as_bytes()),
        call,
        binding: capture.binding.clone(),
        source: Some(capture.source.clone()),
        kind: Some(capture.kind),
        sequence: Some(capture.sequence.clone()),
        sequence_order: Some(
            u16::try_from(capture.operation_sequence.saturating_add(1))
                .map_err(|_| ToolLearningError::BoundExceeded)?,
        ),
        kernel_intent: Some(capture.hasher.pointer(
            PointerDomain::KernelEvent,
            kernel_intent.to_string().as_bytes(),
        )),
    };
    prepare_events(
        store,
        claim,
        &capture.hasher,
        occurred_at,
        trace_id,
        std::slice::from_ref(&event),
    )
    .map(Some)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_invocation_terminal(
    store: &SqliteStore,
    claim: AttemptDriverClaim,
    capture: &PreparedLearningCapture,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
    request_digest: &[u8; 32],
    failure: Option<LearningFailure>,
    status: LearningStatus,
    dispatched: bool,
    known: bool,
    cost_microusd: Option<u64>,
    kernel_outcome: Option<EventId>,
    include_call: bool,
) -> Result<Option<(ExpectedStreamVersion, Vec<NewEvent>)>, ToolLearningError> {
    let call = capture.call();
    let existing = records(store, capture.run_id, &capture.hasher)?;
    if existing.iter().any(
        |event| matches!(event, ToolLearningEvent::Outcome { call: found, .. } if found == &call),
    ) {
        return Ok(None);
    }
    let mut ordinal = next_ordinal(store, capture.run_id)?;
    let mut events = Vec::with_capacity(3);
    if include_call
        && !existing.iter().any(
            |event| matches!(event, ToolLearningEvent::Call { call: found, .. } if found == &call),
        )
    {
        events.push(ToolLearningEvent::Call {
            common: capture.common(ordinal, request_digest, call.as_str().as_bytes()),
            call: call.clone(),
            binding: capture.binding.clone(),
            source: Some(capture.source.clone()),
            kind: Some(capture.kind),
            sequence: Some(capture.sequence.clone()),
            sequence_order: Some(
                u16::try_from(capture.operation_sequence.saturating_add(1))
                    .map_err(|_| ToolLearningError::BoundExceeded)?,
            ),
            kernel_intent: None,
        });
        ordinal = ordinal
            .checked_add(1)
            .ok_or(ToolLearningError::BoundExceeded)?;
    }
    if let Some(failure) = failure {
        events.push(ToolLearningEvent::Error {
            common: capture.common(ordinal, request_digest, b"invocation-error"),
            call: call.clone(),
            stage: failure.stage,
            class: failure.class,
            code: failure.code,
            field: failure.field.clone(),
            retry: failure.retry,
            dispatched: failure.dispatched,
            known: failure.known,
        });
        ordinal = ordinal
            .checked_add(1)
            .ok_or(ToolLearningError::BoundExceeded)?;
    }
    events.push(ToolLearningEvent::Outcome {
        common: capture.common(ordinal, request_digest, b"invocation-outcome"),
        call,
        status,
        dispatched,
        known,
        cost_microusd,
        kernel_outcome: kernel_outcome.map(|event_id| {
            capture
                .hasher
                .pointer(PointerDomain::KernelEvent, event_id.to_string().as_bytes())
        }),
    });
    prepare_events(
        store,
        claim,
        &capture.hasher,
        occurred_at,
        trace_id,
        &events,
    )
    .map(Some)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_invocation_interruption(
    store: &SqliteStore,
    claim: AttemptDriverClaim,
    capture: &PreparedLearningCapture,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
    request_digest: &[u8; 32],
    failure: LearningFailure,
) -> Result<Option<(ExpectedStreamVersion, Vec<NewEvent>)>, ToolLearningError> {
    let call = capture.call();
    let existing = records(store, capture.run_id, &capture.hasher)?;
    if existing.iter().any(|event| {
        matches!(event, ToolLearningEvent::Error { call: found, code, .. }
            if found == &call && *code == failure.code)
    }) {
        return Ok(None);
    }
    let ordinal = next_ordinal(store, capture.run_id)?;
    let mut events = Vec::with_capacity(2);
    let mut error_ordinal = ordinal;
    if !existing
        .iter()
        .any(|event| matches!(event, ToolLearningEvent::Call { call: found, .. } if found == &call))
    {
        events.push(ToolLearningEvent::Call {
            common: capture.common(ordinal, request_digest, call.as_str().as_bytes()),
            call: call.clone(),
            binding: capture.binding.clone(),
            source: Some(capture.source.clone()),
            kind: Some(capture.kind),
            sequence: Some(capture.sequence.clone()),
            sequence_order: Some(
                u16::try_from(capture.operation_sequence.saturating_add(1))
                    .map_err(|_| ToolLearningError::BoundExceeded)?,
            ),
            kernel_intent: None,
        });
        error_ordinal = ordinal
            .checked_add(1)
            .ok_or(ToolLearningError::BoundExceeded)?;
    }
    events.push(ToolLearningEvent::Error {
        common: capture.common(error_ordinal, request_digest, b"invocation-interruption"),
        call,
        stage: failure.stage,
        class: failure.class,
        code: failure.code,
        field: failure.field,
        retry: failure.retry,
        dispatched: failure.dispatched,
        known: failure.known,
    });
    prepare_events(
        store,
        claim,
        &capture.hasher,
        occurred_at,
        trace_id,
        &events,
    )
    .map(Some)
}

#[allow(clippy::too_many_arguments)]
pub fn append(
    store: &mut SqliteStore,
    owner: AttemptOwnership,
    claim: AttemptDriverClaim,
    hasher: &ProjectPointerHasher,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
    event: &ToolLearningEvent,
) -> Result<AppendOutcome, ToolLearningError> {
    append_many(
        store,
        owner,
        claim,
        hasher,
        occurred_at,
        trace_id,
        std::slice::from_ref(event),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn append_many(
    store: &mut SqliteStore,
    owner: AttemptOwnership,
    claim: AttemptDriverClaim,
    hasher: &ProjectPointerHasher,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
    events: &[ToolLearningEvent],
) -> Result<AppendOutcome, ToolLearningError> {
    append_many_with_hook(
        store,
        owner,
        claim,
        hasher,
        occurred_at,
        trace_id,
        events,
        |_| false,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn append_bind(
    store: &mut SqliteStore,
    owner: AttemptOwnership,
    claim: AttemptDriverClaim,
    hasher: &ProjectPointerHasher,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
    event: &ToolLearningEvent,
    binding_id: &str,
) -> Result<AppendOutcome, ToolLearningError> {
    append_many_inner(
        store,
        owner,
        claim,
        hasher,
        occurred_at,
        trace_id,
        std::slice::from_ref(event),
        |_| false,
        Some(binding_id),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn append_many_with_hook(
    store: &mut SqliteStore,
    owner: AttemptOwnership,
    claim: AttemptDriverClaim,
    hasher: &ProjectPointerHasher,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
    events: &[ToolLearningEvent],
    crash: impl FnMut(crate::store::sqlite::append::CrashPoint) -> bool,
) -> Result<AppendOutcome, ToolLearningError> {
    append_many_inner(
        store,
        owner,
        claim,
        hasher,
        occurred_at,
        trace_id,
        events,
        crash,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn append_many_inner(
    store: &mut SqliteStore,
    owner: AttemptOwnership,
    claim: AttemptDriverClaim,
    hasher: &ProjectPointerHasher,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
    events: &[ToolLearningEvent],
    crash: impl FnMut(crate::store::sqlite::append::CrashPoint) -> bool,
    binding_id: Option<&str>,
) -> Result<AppendOutcome, ToolLearningError> {
    if events.is_empty() || claim.owner() != owner {
        return Err(ToolLearningError::InvalidRecord);
    }
    let events = events
        .iter()
        .map(|event| seal_event(event, hasher))
        .collect::<Result<Vec<_>, _>>()?;
    let run_id = claim.run_id;
    let run_pointer = hasher.pointer(PointerDomain::Run, run_id.to_string().as_bytes());
    let existing = records(store, run_id, hasher)?;
    let first_ordinal = existing.len() as u64 + 1;
    let replay = events.iter().all(|event| {
        existing
            .iter()
            .any(|stored| stored.common().event_id == event.common().event_id && stored == event)
    });
    for (index, event) in events.iter().enumerate() {
        event.validate_with(hasher)?;
        if event.common().run != run_pointer
            || (!replay && event.common().ordinal != first_ordinal + index as u64)
        {
            return Err(ToolLearningError::InvalidRecord);
        }
    }
    if !replay {
        validate_sequence(existing.iter().chain(events.iter()))?;
        enforce_admission(&existing, &events)?;
    }

    let payloads = events
        .iter()
        .map(|event| serde_json::to_vec(event).map_err(|_| ToolLearningError::InvalidRecord))
        .collect::<Result<Vec<_>, _>>()?;
    let mut request = Vec::new();
    for payload in &payloads {
        request.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        request.extend_from_slice(payload);
    }
    let stable = events
        .iter()
        .map(|event| event.common().event_id.as_str())
        .collect::<Vec<_>>()
        .join(":");
    let stream = EntityId::Run(run_id);
    let key = IdempotencyKey::parse(&format!(
        "tool-learning-{}",
        hasher
            .pointer(PointerDomain::Event, stable.as_bytes())
            .as_str()
    ))
    .map_err(|_| ToolLearningError::InvalidRecord)?;
    let scope = IdempotencyScope::new(owner.principal_id, TOOL_LEARNING_FORMAT, stream)
        .map_err(|_| ToolLearningError::InvalidRecord)?;
    let expected_stream_version = store.stream_version(stream)?;
    let command = AppendCommand {
        idempotency_scope: scope,
        idempotency_key: key,
        request_digest: CanonicalRequestDigest::new(crate::domain::crypto::sha256(&request)),
        claim: None,
        driver_claim: Some(claim),
        allow_quiescent_driver_claim: false,
        expected_versions: vec![ExpectedStreamVersion {
            stream,
            version: ExpectedVersion::new(expected_stream_version),
        }],
        events: events
            .iter()
            .zip(payloads)
            .map(|(event, payload)| {
                let stable = event.common().event_id.as_str();
                NewEvent {
                    id: EventId::from_stable_bytes(stable.as_bytes()),
                    stream,
                    event_type: EventType::parse(EVENT_TYPE)
                        .expect("tool learning event type is valid"),
                    schema_version: SchemaVersion::CURRENT,
                    occurred_at: occurred_at.clone(),
                    causation_id: CommandId::from_stable_bytes(stable.as_bytes()),
                    correlation_id: EntityId::Run(run_id),
                    attempt_id: Some(owner.attempt_id),
                    trace_id: trace_id.clone(),
                    payload,
                    artifacts: b"[]".to_vec(),
                }
            })
            .collect(),
        response: stable.as_bytes().to_vec(),
    };
    match binding_id {
        Some(binding_id) => store.append_with_discovery_binding(
            command,
            hasher.project().as_str(),
            run_id,
            binding_id,
        ),
        None => store.append_with_hook(command, crash),
    }
    .map_err(Into::into)
}

pub fn records(
    store: &SqliteStore,
    run_id: RunId,
    hasher: &ProjectPointerHasher,
) -> Result<Vec<ToolLearningEvent>, ToolLearningError> {
    let run_pointer = hasher.pointer(PointerDomain::Run, run_id.to_string().as_bytes());
    let stored = store
        .events_for_correlation(EntityId::Run(run_id), EVENT_TYPE, MAX_LEARNING_EVENTS + 1)?
        .into_iter();
    let records = stored
        .map(|stored| {
            let event: ToolLearningEvent = serde_json::from_slice(&stored.event.payload)
                .map_err(|_| ToolLearningError::InvalidRecord)?;
            event.validate_with(hasher)?;
            if stored.event.correlation_id != EntityId::Run(run_id)
                || stored.event.stream != EntityId::Run(run_id)
                || event.common().run != run_pointer
                || stored.event.id
                    != EventId::from_stable_bytes(event.common().event_id.as_str().as_bytes())
                || stored.event.causation_id
                    != CommandId::from_stable_bytes(event.common().event_id.as_str().as_bytes())
            {
                return Err(ToolLearningError::InvalidRecord);
            }
            Ok(event)
        })
        .collect::<Result<Vec<_>, ToolLearningError>>()?;
    if records.len() > MAX_LEARNING_EVENTS {
        return Err(ToolLearningError::BoundExceeded);
    }
    if records.iter().enumerate().any(|(index, event)| {
        event.common().ordinal != u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1)
    }) {
        return Err(ToolLearningError::InvalidRecord);
    }
    validate_sequence(records.iter())?;
    Ok(records)
}

#[allow(clippy::too_many_arguments)]
pub fn settle_unresolved_continuations(
    store: &mut SqliteStore,
    owner: AttemptOwnership,
    claim: AttemptDriverClaim,
    hasher: &ProjectPointerHasher,
    run_id: RunId,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
    status: LearningStatus,
) -> Result<usize, ToolLearningError> {
    let existing = records(store, run_id, hasher)?;
    let terminal = existing
        .iter()
        .filter_map(|event| match event {
            ToolLearningEvent::Outcome { call, .. } => Some(call),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let committed = if status == LearningStatus::OutcomeUnknown {
        store.events()?
    } else {
        Vec::new()
    };
    let unresolved = existing
        .iter()
        .filter_map(|event| match event {
            ToolLearningEvent::Call {
                common,
                call,
                kernel_intent,
                ..
            } if !terminal.contains(call)
                && (status != LearningStatus::OutcomeUnknown
                    || kernel_intent.as_ref().is_some_and(|intent| {
                        committed.iter().any(|stored| {
                            stored.event.event_type.as_str() == "capability.invocation_dispatched"
                                && committed.iter().any(|candidate| {
                                    candidate.event.stream == stored.event.stream
                                        && candidate.event.event_type.as_str()
                                            == "capability.invocation_intent"
                                        && hasher.pointer(
                                            PointerDomain::KernelEvent,
                                            candidate.event.id.to_string().as_bytes(),
                                        ) == *intent
                                })
                        })
                    })) =>
            {
                Some((common, call))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    if unresolved.is_empty() {
        return Ok(0);
    }
    let mut ordinal = next_ordinal(store, run_id)?;
    let mut additions = Vec::with_capacity(unresolved.len() * 2);
    for (common, call) in unresolved {
        let denied = status == LearningStatus::Failed;
        let unknown = status == LearningStatus::OutcomeUnknown;
        let outcome_ordinal = ordinal
            .checked_add(1)
            .ok_or(ToolLearningError::BoundExceeded)?;
        let make_common = |ordinal, suffix: &[u8]| {
            LearningCommon::new(
                hasher,
                run_id,
                ordinal,
                LearningOperation::Invoke,
                common.surface,
                suffix,
                common.request.clone(),
                common.capability.clone(),
                common.schema.clone(),
            )
        };
        additions.push(ToolLearningEvent::Error {
            common: make_common(
                ordinal,
                if denied {
                    b"continuation-denied"
                } else if unknown {
                    b"continuation-unknown"
                } else {
                    b"continuation-cancelled"
                },
            ),
            call: call.clone(),
            stage: if denied {
                ErrorStage::Authorization
            } else {
                ErrorStage::Dispatch
            },
            class: if denied {
                ErrorClass::Auth
            } else if unknown {
                ErrorClass::Transport
            } else {
                ErrorClass::System
            },
            code: if denied {
                ErrorCode::AuthDenied
            } else if unknown {
                ErrorCode::OutcomeUnknown
            } else {
                ErrorCode::Cancelled
            },
            field: None,
            retry: if unknown {
                RetryClass::Unknown
            } else {
                RetryClass::Never
            },
            dispatched: unknown,
            known: !unknown,
        });
        additions.push(ToolLearningEvent::Outcome {
            common: make_common(
                outcome_ordinal,
                if denied {
                    b"continuation-denied-outcome"
                } else if unknown {
                    b"continuation-unknown-outcome"
                } else {
                    b"continuation-cancelled-outcome"
                },
            ),
            call: call.clone(),
            status,
            dispatched: unknown,
            known: !unknown,
            cost_microusd: None,
            kernel_outcome: None,
        });
        ordinal = ordinal
            .checked_add(2)
            .ok_or(ToolLearningError::BoundExceeded)?;
    }
    let count = additions.len() / 2;
    append_many(
        store,
        owner,
        claim,
        hasher,
        occurred_at,
        trace_id,
        &additions,
    )?;
    Ok(count)
}

fn validate_sequence<'a>(
    records: impl IntoIterator<Item = &'a ToolLearningEvent>,
) -> Result<(), ToolLearningError> {
    let records = records.into_iter().collect::<Vec<_>>();
    let unique = records
        .iter()
        .map(|event| event.common().event_id.as_str())
        .collect::<BTreeSet<_>>();
    if unique.len() != records.len() {
        return Err(ToolLearningError::InvalidRecord);
    }
    let call_records = records
        .iter()
        .filter_map(|event| match event {
            ToolLearningEvent::Call { call, .. } => Some(call),
            _ => None,
        })
        .collect::<Vec<_>>();
    let calls = call_records.iter().copied().collect::<BTreeSet<_>>();
    if calls.len() != call_records.len() {
        return Err(ToolLearningError::InvalidRecord);
    }
    let mut terminal = BTreeSet::new();
    for event in records {
        match event {
            ToolLearningEvent::Error { call, .. } if !calls.contains(call) => {
                return Err(ToolLearningError::InvalidRecord);
            }
            ToolLearningEvent::Outcome { call, .. } => {
                if !calls.contains(call) || !terminal.insert(call) {
                    return Err(ToolLearningError::InvalidRecord);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn enforce_admission(
    existing: &[ToolLearningEvent],
    additions: &[ToolLearningEvent],
) -> Result<(), ToolLearningError> {
    let mut active = BTreeMap::<&LearningPointer, (bool, bool)>::new();
    for event in existing.iter().chain(additions) {
        match event {
            ToolLearningEvent::Call { call, .. } => {
                active.entry(call).or_insert((false, false));
            }
            ToolLearningEvent::Error { call, .. } => active.entry(call).or_default().0 = true,
            ToolLearningEvent::Outcome { call, .. } => active.entry(call).or_default().1 = true,
            _ => {}
        }
    }
    let reserved = active
        .values()
        .map(|(error, outcome)| usize::from(!error) + usize::from(!outcome))
        .sum::<usize>();
    if existing.len() + additions.len() + reserved > MAX_LEARNING_EVENTS {
        return Err(ToolLearningError::BoundExceeded);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisSignal {
    PoorDescription,
    HarmfulDecoy,
    MisunderstoodField,
    ValuableSequence,
}

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisFinding {
    pub signal: AnalysisSignal,
    pub capability: LearningPointer,
    pub schema: LearningPointer,
    pub surface: LearningSurface,
    pub field: Option<LearningPointer>,
    pub sequence: Option<LearningPointer>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrozenFactors {
    pub canonical_actual_config_digest: String,
    pub arm_config: LearningPointer,
    pub receipt: LearningPointer,
    pub declaration_artifact: LearningPointer,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreregisteredExperiment {
    pub experiment: LearningPointer,
    pub run: LearningPointer,
    pub arm: ExperimentArm,
    pub capability: LearningPointer,
    pub schema: LearningPointer,
    pub surface: LearningSurface,
    pub authorized: bool,
    pub offered: bool,
    pub description_only: bool,
    pub frozen_factors: FrozenFactors,
    pub expected_sequence: Vec<PreregisteredSequenceStep>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreregisteredSequenceStep {
    pub capability: LearningPointer,
    pub schema: LearningPointer,
    pub surface: LearningSurface,
    pub ordinal: u16,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HarmReceiptKind {
    Security,
    Rollback,
    Defect,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownstreamGradeRecord {
    pub experiment: LearningPointer,
    pub run: LearningPointer,
    pub grade: DownstreamGrade,
    pub cost_microusd: u64,
    pub latency_ms: u64,
    pub receipt: LearningPointer,
    pub harm_receipt: Option<HarmReceiptKind>,
    pub sequence: Option<SequenceObservation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SequenceObservation {
    pub cost_microusd: u64,
    pub latency_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenLearningAnalysis {
    pub result: CausalResult,
    pub input_digests: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum CausalResult {
    Available {
        findings: BTreeSet<AnalysisFinding>,
        direct_passes: u16,
        competing_passes: u16,
    },
    Unavailable(CausalUnavailable),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CausalUnavailable {
    MissingPreregistration,
    MissingArm,
    MissingDownstreamGrade,
    BoundExceeded,
    MissingLinkage,
    MissingCandidate,
    MissingAuthority,
    MissingLearningRecords,
}

#[derive(Clone, Copy, Debug)]
pub struct ToolLearningAnalyzer {
    max_records: usize,
}

impl ToolLearningAnalyzer {
    pub const fn new(max_records: usize) -> Self {
        Self { max_records }
    }

    pub fn analyze(
        &self,
        events: &[ToolLearningEvent],
        experiments: &[PreregisteredExperiment],
        grades: &[DownstreamGradeRecord],
    ) -> CausalResult {
        if self.max_records == 0
            || events.len() > self.max_records
            || experiments.len() > self.max_records
            || grades.len() > self.max_records
        {
            return CausalResult::Unavailable(CausalUnavailable::BoundExceeded);
        }
        if experiments.is_empty() {
            return CausalResult::Unavailable(CausalUnavailable::MissingPreregistration);
        }
        if experiments.iter().any(|experiment| {
            !matches!(
                experiment.experiment.domain(),
                Ok(PointerDomain::Experiment)
            ) || !matches!(experiment.run.domain(), Ok(PointerDomain::Run))
                || !matches!(
                    experiment.capability.domain(),
                    Ok(PointerDomain::Capability)
                )
                || !matches!(experiment.schema.domain(), Ok(PointerDomain::Schema))
                || !valid_sha256(&experiment.frozen_factors.canonical_actual_config_digest)
                || ![
                    &experiment.frozen_factors.arm_config,
                    &experiment.frozen_factors.receipt,
                    &experiment.frozen_factors.declaration_artifact,
                ]
                .into_iter()
                .all(|pointer| matches!(pointer.domain(), Ok(PointerDomain::Artifact)))
                || experiment.expected_sequence.iter().any(|step| {
                    !matches!(step.capability.domain(), Ok(PointerDomain::Capability))
                        || !matches!(step.schema.domain(), Ok(PointerDomain::Schema))
                })
                || experiment
                    .expected_sequence
                    .iter()
                    .enumerate()
                    .any(|(index, step)| usize::from(step.ordinal) != index + 1)
        }) {
            return CausalResult::Unavailable(CausalUnavailable::MissingAuthority);
        }
        let calls = events
            .iter()
            .filter_map(|event| match event {
                ToolLearningEvent::Call { call, .. } => Some(call),
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        if events.iter().any(|event| match event {
            ToolLearningEvent::Error { call, .. } | ToolLearningEvent::Outcome { call, .. } => {
                !calls.contains(call)
            }
            _ => false,
        }) {
            return CausalResult::Unavailable(CausalUnavailable::MissingLinkage);
        }
        let mut groups = BTreeMap::<
            (&LearningPointer, &LearningPointer, &LearningPointer),
            BTreeMap<ExperimentArm, &PreregisteredExperiment>,
        >::new();
        for experiment in experiments {
            if groups
                .entry((
                    &experiment.experiment,
                    &experiment.capability,
                    &experiment.schema,
                ))
                .or_default()
                .insert(experiment.arm, experiment)
                .is_some()
            {
                return CausalResult::Unavailable(CausalUnavailable::MissingArm);
            }
        }
        if groups.values().any(|arms| {
            !arms.contains_key(&ExperimentArm::Direct)
                || !arms.contains_key(&ExperimentArm::Competing)
        }) {
            return CausalResult::Unavailable(CausalUnavailable::MissingArm);
        }
        let grade = |experiment: &PreregisteredExperiment| {
            grades
                .iter()
                .find(|grade| {
                    grade.experiment == experiment.experiment && grade.run == experiment.run
                })
                .map(|grade| grade.grade)
        };
        if experiments
            .iter()
            .any(|experiment| grade(experiment).is_none())
        {
            return CausalResult::Unavailable(CausalUnavailable::MissingDownstreamGrade);
        }
        if grades.iter().any(|grade| {
            !matches!(grade.experiment.domain(), Ok(PointerDomain::Experiment))
                || !matches!(grade.run.domain(), Ok(PointerDomain::Run))
                || !matches!(grade.receipt.domain(), Ok(PointerDomain::Artifact))
                || grade.cost_microusd > MAX_SEQUENCE_COST_MICROUSD
                || grade.latency_ms == 0
                || grade.latency_ms > MAX_SEQUENCE_LATENCY_MS
                || grade.sequence.is_some_and(|sequence| {
                    sequence.cost_microusd > MAX_SEQUENCE_COST_MICROUSD
                        || sequence.latency_ms == 0
                        || sequence.latency_ms > MAX_SEQUENCE_LATENCY_MS
                })
                || (grade.grade == DownstreamGrade::Harmful && grade.harm_receipt.is_none())
                || (grade.grade != DownstreamGrade::Harmful && grade.harm_receipt.is_some())
        }) || grades
            .iter()
            .map(|grade| (&grade.experiment, &grade.run))
            .collect::<BTreeSet<_>>()
            .len()
            != grades.len()
        {
            return CausalResult::Unavailable(CausalUnavailable::MissingAuthority);
        }
        if experiments.iter().any(|experiment| {
            grades
                .iter()
                .find(|grade| {
                    grade.experiment == experiment.experiment && grade.run == experiment.run
                })
                .is_none_or(|grade| grade.receipt != experiment.frozen_factors.receipt)
        }) {
            return CausalResult::Unavailable(CausalUnavailable::MissingAuthority);
        }
        if experiments
            .iter()
            .any(|experiment| !eligible_candidate(events, experiment))
        {
            return CausalResult::Unavailable(CausalUnavailable::MissingCandidate);
        }
        let selected = |experiment: &PreregisteredExperiment| {
            eligible_events(events, experiment)
                .any(|event| matches!(event, ToolLearningEvent::Call { .. }))
        };
        let mut findings = BTreeSet::new();
        for arms in groups.values() {
            let direct = arms[&ExperimentArm::Direct];
            let competing = arms[&ExperimentArm::Competing];
            if !selected(direct)
                && selected(competing)
                && grade(competing) == Some(DownstreamGrade::Passed)
                && direct.description_only
                && competing.description_only
                && direct.frozen_factors.canonical_actual_config_digest
                    == competing.frozen_factors.canonical_actual_config_digest
                && direct.frozen_factors.declaration_artifact
                    == competing.frozen_factors.declaration_artifact
            {
                findings.insert(finding(AnalysisSignal::PoorDescription, direct, None, None));
            }
            for experiment in arms.values() {
                let downstream = grade(experiment).expect("all grades checked");
                let baseline = if experiment.arm == ExperimentArm::Direct {
                    competing
                } else {
                    direct
                };
                if downstream == DownstreamGrade::Harmful
                    && selected(experiment)
                    && grade(baseline) == Some(DownstreamGrade::Passed)
                {
                    findings.insert(finding(
                        AnalysisSignal::HarmfulDecoy,
                        experiment,
                        None,
                        None,
                    ));
                }
                let mut misunderstood =
                    BTreeMap::<&LearningPointer, BTreeSet<&LearningPointer>>::new();
                for (call, field) in
                    eligible_events(events, experiment).filter_map(|event| match event {
                        ToolLearningEvent::Error {
                            call,
                            field: Some(field),
                            code: ErrorCode::InvalidSchema,
                            known: true,
                            ..
                        } => Some((call, field)),
                        _ => None,
                    })
                {
                    misunderstood.entry(field).or_default().insert(call);
                }
                if downstream == DownstreamGrade::Failed
                    && let Some(field) = misunderstood
                        .into_iter()
                        .find_map(|(field, calls)| (calls.len() >= 3).then_some(field))
                {
                    findings.insert(finding(
                        AnalysisSignal::MisunderstoodField,
                        experiment,
                        Some(field.clone()),
                        None,
                    ));
                }
                let matched_sequence =
                    bind_sequence_calls(events, &experiment.run, &experiment.expected_sequence);
                let experiment_grade = grades
                    .iter()
                    .find(|record| {
                        record.experiment == experiment.experiment && record.run == experiment.run
                    })
                    .expect("all grades checked");
                let baseline_grade = grades
                    .iter()
                    .find(|record| {
                        record.experiment == baseline.experiment && record.run == baseline.run
                    })
                    .expect("all grades checked");
                let exact_successes = experiment
                    .expected_sequence
                    .iter()
                    .map(|step| &step.capability)
                    .collect::<BTreeSet<_>>()
                    .len()
                    >= 2
                    && matched_sequence.as_ref().is_some_and(|(_, calls)| {
                        calls.iter().all(|expected| {
                            events.iter().any(|event| {
                                matches!(
                                    event,
                                    ToolLearningEvent::Outcome {
                                        call,
                                        status: LearningStatus::Succeeded,
                                        known: true,
                                        ..
                                    } if call == expected
                                )
                            })
                        })
                    });
                let baseline_exact = exact_sequence(events, baseline);
                if experiment.arm == ExperimentArm::Competing
                    && downstream == DownstreamGrade::Passed
                    && grade(baseline) == Some(DownstreamGrade::Passed)
                    && matched_sequence.is_some()
                    && exact_successes
                    && baseline_exact
                    && experiment_grade
                        .sequence
                        .zip(baseline_grade.sequence)
                        .is_some_and(|(candidate, baseline)| {
                            candidate.cost_microusd < baseline.cost_microusd
                                || candidate.latency_ms < baseline.latency_ms
                        })
                {
                    findings.insert(finding(
                        AnalysisSignal::ValuableSequence,
                        experiment,
                        None,
                        matched_sequence.map(|(sequence, _)| sequence),
                    ));
                }
            }
        }
        let passes = |arm| {
            u16::try_from(
                experiments
                    .iter()
                    .filter(|experiment| {
                        experiment.arm == arm
                            && grade(experiment) == Some(DownstreamGrade::Passed)
                            && eligible_candidate(events, experiment)
                    })
                    .count(),
            )
            .unwrap_or(u16::MAX)
        };
        CausalResult::Available {
            findings,
            direct_passes: passes(ExperimentArm::Direct),
            competing_passes: passes(ExperimentArm::Competing),
        }
    }

    pub fn analyze_linked(
        &self,
        events: Option<&[ToolLearningEvent]>,
        experiments: Option<&[PreregisteredExperiment]>,
        grades: Option<&[DownstreamGradeRecord]>,
    ) -> CausalResult {
        match (events, experiments, grades) {
            (Some(events), Some(experiments), Some(grades)) => {
                self.analyze(events, experiments, grades)
            }
            _ => CausalResult::Unavailable(CausalUnavailable::MissingLinkage),
        }
    }
}

fn eligible_events<'a>(
    events: &'a [ToolLearningEvent],
    experiment: &'a PreregisteredExperiment,
) -> impl Iterator<Item = &'a ToolLearningEvent> {
    events.iter().filter(move |event| {
        let common = event.common();
        common.run == experiment.run
            && common.capability.as_ref() == Some(&experiment.capability)
            && common.schema.as_ref() == Some(&experiment.schema)
            && common.surface == experiment.surface
    })
}

fn eligible_candidate(events: &[ToolLearningEvent], experiment: &PreregisteredExperiment) -> bool {
    experiment.authorized
        && experiment.offered
        && events.iter().any(|event| {
            event.common().run == experiment.run
                && matches!(event, ToolLearningEvent::Opportunity { candidates, .. } if
                candidates.iter().any(|candidate| {
                    candidate.capability == experiment.capability
                        && candidate.schema == experiment.schema
                        && candidate.surface == experiment.surface
                        && candidate.authorized
                        && candidate.offered
                }))
        })
}

fn exact_sequence(events: &[ToolLearningEvent], experiment: &PreregisteredExperiment) -> bool {
    if experiment
        .expected_sequence
        .iter()
        .map(|step| &step.capability)
        .collect::<BTreeSet<_>>()
        .len()
        < 2
    {
        return false;
    }
    bind_sequence_calls(events, &experiment.run, &experiment.expected_sequence).is_some_and(
        |(_, calls)| {
            calls.iter().all(|expected| {
                events.iter().any(|event| {
                    matches!(event, ToolLearningEvent::Outcome {
                call,
                status: LearningStatus::Succeeded,
                known: true,
                ..
            } if call == expected)
                })
            })
        },
    )
}

pub(crate) fn bind_sequence_calls(
    events: &[ToolLearningEvent],
    run: &LearningPointer,
    expected: &[PreregisteredSequenceStep],
) -> Option<(LearningPointer, Vec<LearningPointer>)> {
    if expected.is_empty() {
        return None;
    }
    let mut sequences = BTreeMap::<&LearningPointer, Vec<_>>::new();
    for event in events {
        if let ToolLearningEvent::Call {
            common,
            call,
            sequence: Some(sequence),
            sequence_order: Some(order),
            ..
        } = event
            && &common.run == run
            && common.operation == LearningOperation::Invoke
        {
            sequences.entry(sequence).or_default().push((
                *order,
                common.capability.as_ref(),
                common.schema.as_ref(),
                common.surface,
                call,
            ));
        }
    }
    let mut matches = sequences.into_iter().filter_map(|(sequence, observed)| {
        let mut observed = observed;
        observed.sort_by_key(|(operation_order, ..)| *operation_order);
        (observed.len() == expected.len()
            && observed.windows(2).all(|pair| pair[0].0 < pair[1].0)
            && observed.iter().zip(expected).all(
                |((_, capability, schema, surface, _), expected)| {
                    *capability == Some(&expected.capability)
                        && *schema == Some(&expected.schema)
                        && *surface == expected.surface
                },
            ))
        .then(|| {
            (
                sequence.clone(),
                observed
                    .into_iter()
                    .map(|(_, _, _, _, call)| call.clone())
                    .collect(),
            )
        })
    });
    let matched = matches.next()?;
    matches.next().is_none().then_some(matched)
}

fn finding(
    signal: AnalysisSignal,
    experiment: &PreregisteredExperiment,
    field: Option<LearningPointer>,
    sequence: Option<LearningPointer>,
) -> AnalysisFinding {
    AnalysisFinding {
        signal,
        capability: experiment.capability.clone(),
        schema: experiment.schema.clone(),
        surface: experiment.surface,
        field,
        sequence,
    }
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[derive(Debug)]
pub enum ToolLearningError {
    InvalidPointer,
    InvalidRecord,
    BoundExceeded,
    Encryption,
    Store(StoreError),
}

impl std::fmt::Display for ToolLearningError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPointer => formatter.write_str("invalid tool-learning pointer"),
            Self::InvalidRecord => formatter.write_str("invalid tool-learning record"),
            Self::BoundExceeded => formatter.write_str("tool-learning bound exceeded"),
            Self::Encryption => formatter.write_str("tool-learning encryption failed"),
            Self::Store(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ToolLearningError {}

impl From<StoreError> for ToolLearningError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

fn structural_pointer(
    pointer: &LearningPointer,
    domain: PointerDomain,
) -> Result<(), ToolLearningError> {
    if pointer.domain()? != domain {
        return Err(ToolLearningError::InvalidPointer);
    }
    Ok(())
}

fn optional_structural(
    pointer: &Option<LearningPointer>,
    domain: PointerDomain,
) -> Result<(), ToolLearningError> {
    if let Some(pointer) = pointer {
        structural_pointer(pointer, domain)?;
    }
    Ok(())
}

fn validate_optional(
    hasher: &ProjectPointerHasher,
    pointer: &Option<LearningPointer>,
    domain: PointerDomain,
) -> Result<(), ToolLearningError> {
    if let Some(pointer) = pointer {
        hasher.validate(pointer, domain)?;
    }
    Ok(())
}

fn parse_pointer(value: &str) -> Result<(PointerDomain, [u8; 64]), ToolLearningError> {
    let mut fields = value
        .strip_prefix(POINTER_PREFIX)
        .ok_or(ToolLearningError::InvalidPointer)?
        .split('_');
    let tag = fields.next().ok_or(ToolLearningError::InvalidPointer)?;
    let mac = fields.next().ok_or(ToolLearningError::InvalidPointer)?;
    if fields.next().is_some() || tag.len() != 2 || mac.len() != 128 {
        return Err(ToolLearningError::InvalidPointer);
    }
    let tag = decode_hex::<1>(tag.as_bytes())?[0];
    Ok((
        PointerDomain::from_tag(tag).ok_or(ToolLearningError::InvalidPointer)?,
        decode_hex(mac.as_bytes())?,
    ))
}

fn decode_hex<const N: usize>(bytes: &[u8]) -> Result<[u8; N], ToolLearningError> {
    if bytes.len() != N * 2 {
        return Err(ToolLearningError::InvalidPointer);
    }
    let mut output = [0_u8; N];
    for (byte, pair) in output.iter_mut().zip(bytes.chunks_exact(2)) {
        let nibble = |value| match value {
            b'0'..=b'9' => Some(value - b'0'),
            b'a'..=b'f' => Some(value - b'a' + 10),
            _ => None,
        };
        *byte = (nibble(pair[0]).ok_or(ToolLearningError::InvalidPointer)? << 4)
            | nibble(pair[1]).ok_or(ToolLearningError::InvalidPointer)?;
    }
    Ok(output)
}

fn constant_time_eq(left: &[u8; 32], right: &[u8; 32]) -> bool {
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
