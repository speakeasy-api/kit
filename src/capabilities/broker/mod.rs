use std::{fmt, sync::atomic::Ordering};

use serde::{Deserialize, Serialize};

use crate::{
    agent::accounting::{AccountingError, SpeculationOutcome, ToolMeasurement, UsageEnvelope},
    capabilities::{
        kernel::{
            grant,
            identity::{Digest, DigestAlgorithm},
            invoke::{
                AuthorizedInvocation, DispatchOutcome, InvocationCrashPoint, InvocationEnvelope,
                InvocationResult, InvocationRuntime, InvokeError, MAX_INVOCATION_ARGUMENT_BYTES,
            },
        },
        native::NativeCatalog,
        schema::{NormalizedSchema, SchemaValidation},
    },
    domain::{
        commands::ExpectedVersion,
        events::{EntityId, EventType, SchemaVersion},
        ids::{ApprovalId, EventId},
        secret::SecretHandle,
    },
    runtime::scheduler::reserve::BudgetLedger,
    store::sqlite::{
        append::{
            AppendCommand, AppendOutcome, ExpectedStreamVersion, NewEvent, SqliteStore, StoreError,
        },
        idempotency::{
            CanonicalRequestDigest, IdempotencyKey, IdempotencyScope, IdempotencyStatus,
        },
    },
};

pub mod transport_auth;

const AUTH_RECORD_VERSION: u16 = 1;
const AUTH_CHALLENGE_COMMAND: &str = "capability.broker_auth.challenge";
const AUTH_RESOLUTION_COMMAND: &str = "capability.broker_auth.resolve";
const AUTH_CHALLENGE_EVENT: &str = "capability.broker_auth_challenged";
const AUTH_RESOLUTION_EVENT: &str = "capability.broker_auth_resolved";
const MAX_AUTH_SCOPE_BYTES: usize = 512;
const MAX_AUTH_RECORD_BYTES: usize = 8192;

struct AuthChannel {
    challenge_command: &'static str,
    resolution_command: &'static str,
    challenge_event: &'static str,
    resolution_event: &'static str,
}

const REQUIREMENT_CHANNEL: AuthChannel = AuthChannel {
    challenge_command: AUTH_CHALLENGE_COMMAND,
    resolution_command: AUTH_RESOLUTION_COMMAND,
    challenge_event: AUTH_CHALLENGE_EVENT,
    resolution_event: AUTH_RESOLUTION_EVENT,
};

pub struct BrokerInvocation<'a> {
    envelope: InvocationEnvelope<'a>,
    validation_schema: &'a NormalizedSchema,
    auth: Option<BrokerAuthRequirement>,
}

impl<'a> BrokerInvocation<'a> {
    pub fn native(envelope: InvocationEnvelope<'a>) -> Result<Self, BrokerError> {
        let descriptor = NativeCatalog::by_identity(envelope.capability)
            .ok_or(BrokerError::NativeCapabilityBinding)?;
        Ok(Self::validated(envelope, descriptor.normalized_schema()))
    }

    pub fn generic(envelope: InvocationEnvelope<'a>, schema: &'a NormalizedSchema) -> Self {
        Self::validated(envelope, schema)
    }

    pub fn external(envelope: InvocationEnvelope<'a>, schema: &'a NormalizedSchema) -> Self {
        Self::validated(envelope, schema)
    }

    pub fn nested(
        mut envelope: InvocationEnvelope<'a>,
        schema: &'a NormalizedSchema,
        delegation: &'a grant::DelegationSnapshot,
    ) -> Self {
        envelope.delegation = Some(delegation);
        Self::validated(envelope, schema)
    }

    fn validated(envelope: InvocationEnvelope<'a>, schema: &'a NormalizedSchema) -> Self {
        Self {
            envelope,
            validation_schema: schema,
            auth: None,
        }
    }

    pub fn with_auth_requirement(mut self, auth: BrokerAuthRequirement) -> Self {
        self.auth = Some(auth);
        self
    }

    pub(crate) fn arguments(&self) -> &[u8] {
        self.envelope.arguments
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerAuthRequirement {
    scope: String,
    credential_id: Option<SecretHandle>,
}

impl BrokerAuthRequirement {
    pub fn new(scope: impl Into<String>) -> Result<Self, BrokerError> {
        let scope = scope.into();
        if scope.is_empty()
            || scope.len() > MAX_AUTH_SCOPE_BYTES
            || scope
                .bytes()
                .any(|byte| !(byte.is_ascii_graphic() || byte == b' '))
        {
            return Err(BrokerError::InvalidAuthRequirement);
        }
        Ok(Self {
            scope,
            credential_id: None,
        })
    }

    pub fn with_credential_id(mut self, credential_id: SecretHandle) -> Self {
        self.credential_id = Some(credential_id);
        self
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn credential_id(&self) -> Option<&SecretHandle> {
        self.credential_id.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthChallenge {
    pub challenge_id: ApprovalId,
    pub principal_id: String,
    pub project_id: String,
    pub invocation_id: String,
    pub decision_digest: String,
    pub request_digest: String,
    pub capability_source: String,
    pub capability_namespace: String,
    pub capability_name: String,
    pub capability_version: String,
    pub capability_implementation_digest: String,
    pub schema_digest: String,
    pub scope: String,
    pub credential_id: Option<SecretHandle>,
    pub trace_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthResolution {
    Granted,
    Denied,
}

pub struct BrokerRuntime<'a> {
    store: &'a mut SqliteStore,
    budget: &'a BudgetLedger,
    backend: &'a mut dyn FnMut(&AuthorizedInvocation) -> DispatchOutcome,
    crash_at: Option<InvocationCrashPoint>,
}

impl<'a> BrokerRuntime<'a> {
    pub fn new(
        store: &'a mut SqliteStore,
        budget: &'a BudgetLedger,
        backend: &'a mut dyn FnMut(&AuthorizedInvocation) -> DispatchOutcome,
    ) -> Self {
        Self {
            store,
            budget,
            backend,
            crash_at: None,
        }
    }

    #[cfg(debug_assertions)]
    pub fn with_crash_at(mut self, crash_at: InvocationCrashPoint) -> Self {
        self.crash_at = Some(crash_at);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerResult {
    pub invocation: InvocationResult,
    pub accounting: UsageEnvelope,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum BrokerOutcome {
    Completed(BrokerResult),
    AuthRequired(AuthChallenge),
}

#[derive(Debug)]
pub enum BrokerError {
    NativeCapabilityBinding,
    SchemaBindingMismatch,
    UnsupportedValidation,
    InvalidArguments,
    InvalidAuthRequirement,
    InvalidTransportOperation,
    AuthCredentialMismatch,
    AuthNotRequired,
    AuthResolutionCancelled,
    TransportAuthCancelled,
    AuthPrincipalMismatch,
    AuthScopeMismatch,
    AuthDenied,
    RepeatedAuthChallenge,
    ReplayNotAuthorized,
    ReplayPermitConsumed,
    TransportAlreadyCompleted,
    TransportOutcomeUnknown,
    InvalidAuthState,
    AuthStore(StoreError),
    Invoke(InvokeError),
    Accounting(AccountingError),
    ToolReservationRequired,
}

impl fmt::Display for BrokerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NativeCapabilityBinding => {
                formatter.write_str("native capability is not bound to an exact descriptor")
            }
            Self::SchemaBindingMismatch => {
                formatter.write_str("validation schema does not match the bound schema digest")
            }
            Self::UnsupportedValidation => {
                formatter.write_str("normalized schema validation is unsupported")
            }
            Self::InvalidArguments => {
                formatter.write_str("arguments do not satisfy the normalized schema")
            }
            Self::InvalidAuthRequirement => formatter.write_str("invalid broker auth requirement"),
            Self::InvalidTransportOperation => {
                formatter.write_str("transport operation must contain 1 to 128 visible ASCII bytes")
            }
            Self::AuthCredentialMismatch => {
                formatter.write_str("broker auth credential does not match the invocation")
            }
            Self::AuthNotRequired => formatter.write_str("broker auth is not required"),
            Self::AuthResolutionCancelled => {
                formatter.write_str("broker auth cannot be resolved for a cancelled invocation")
            }
            Self::TransportAuthCancelled => formatter
                .write_str("broker transport auth cannot proceed for a cancelled invocation"),
            Self::AuthPrincipalMismatch => {
                formatter.write_str("broker auth principal or project does not match")
            }
            Self::AuthScopeMismatch => {
                formatter.write_str("broker transport auth scope does not match")
            }
            Self::AuthDenied => formatter.write_str("broker auth was denied"),
            Self::RepeatedAuthChallenge => {
                formatter.write_str("broker transport auth was challenged again after resolution")
            }
            Self::ReplayNotAuthorized => {
                formatter.write_str("broker transport replay requires a granted auth resolution")
            }
            Self::ReplayPermitConsumed => {
                formatter.write_str("broker transport replay permit was already consumed")
            }
            Self::TransportAlreadyCompleted => {
                formatter.write_str("broker transport operation already completed")
            }
            Self::TransportOutcomeUnknown => {
                formatter.write_str("broker transport operation requires outcome reconciliation")
            }
            Self::InvalidAuthState => formatter.write_str("invalid persisted broker auth state"),
            Self::AuthStore(error) => error.fmt(formatter),
            Self::Invoke(error) => error.fmt(formatter),
            Self::Accounting(error) => error.fmt(formatter),
            Self::ToolReservationRequired => {
                formatter.write_str("broker invocation requires at least one reserved tool")
            }
        }
    }
}

impl std::error::Error for BrokerError {}

pub fn invoke(
    request: BrokerInvocation<'_>,
    runtime: BrokerRuntime<'_>,
) -> Result<BrokerOutcome, BrokerError> {
    request
        .envelope
        .preflight_authority(runtime.store, false)
        .map_err(BrokerError::Invoke)?;
    preflight(&request)?;
    if request.envelope.reservation.tools() == 0 {
        return Err(BrokerError::ToolReservationRequired);
    }
    if !request.envelope.cancellation.load(Ordering::Acquire)
        && matches!(
            request.envelope.approval,
            crate::capabilities::kernel::invoke::ApprovalState::NotRequired
                | crate::capabilities::kernel::invoke::ApprovalState::Approved
        )
        && let Some(requirement) = &request.auth
    {
        ensure_auth_credential(&request.envelope, requirement)?;
        let decision = grant::decide(request.envelope.grant_request());
        if !decision.is_allowed() {
            return Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(
                decision.reason(),
            )));
        }
        let binding = AuthBinding::new(&request, requirement, decision.snapshot_digest())?;
        match auth_state(
            runtime.store,
            &request.envelope,
            &binding,
            &REQUIREMENT_CHANNEL,
        )? {
            AuthState::Missing => {
                append_challenge(
                    runtime.store,
                    &request.envelope,
                    &binding,
                    &REQUIREMENT_CHANNEL,
                )?;
                return Ok(BrokerOutcome::AuthRequired(binding.challenge()));
            }
            AuthState::Waiting => {
                return Ok(BrokerOutcome::AuthRequired(binding.challenge()));
            }
            AuthState::Denied => return Err(BrokerError::AuthDenied),
            AuthState::Granted => {}
        }
    }

    let kernel_runtime = InvocationRuntime::new(runtime.store, runtime.budget, runtime.backend);
    let kernel_runtime = match runtime.crash_at {
        Some(point) => kernel_runtime.with_crash_at(point),
        None => kernel_runtime,
    };
    let invocation = crate::capabilities::kernel::invoke::invoke(request.envelope, kernel_runtime)
        .map_err(BrokerError::Invoke)?;
    let accounting = UsageEnvelope::from_tool_outcome(
        &invocation.canonical,
        &ToolMeasurement::one_call(),
        SpeculationOutcome::None,
        None,
        Some(invocation.reservation),
    )
    .map_err(BrokerError::Accounting)?;
    Ok(BrokerOutcome::Completed(BrokerResult {
        invocation,
        accounting,
    }))
}

pub fn resolve_auth(
    request: &BrokerInvocation<'_>,
    actor: &crate::api::auth::contract::AuthenticatedPrincipal,
    resolution: AuthResolution,
    store: &mut SqliteStore,
) -> Result<(), BrokerError> {
    request
        .envelope
        .preflight_authority(store, true)
        .map_err(BrokerError::Invoke)?;
    preflight(request)?;
    if request.envelope.cancellation.load(Ordering::Acquire) {
        return Err(BrokerError::AuthResolutionCancelled);
    }
    let requirement = request.auth.as_ref().ok_or(BrokerError::AuthNotRequired)?;
    ensure_auth_credential(&request.envelope, requirement)?;
    if actor.principal_id() != request.envelope.authenticated.principal_id()
        || actor.grant_snapshot().project_id() != request.envelope.project_id
    {
        return Err(BrokerError::AuthPrincipalMismatch);
    }
    let decision = grant::decide(request.envelope.grant_request_for(actor));
    if !decision.is_allowed() {
        return Err(BrokerError::Invoke(InvokeError::AuthorizationDenied(
            decision.reason(),
        )));
    }
    let binding = AuthBinding::new(request, requirement, decision.snapshot_digest())?;
    match auth_state(store, &request.envelope, &binding, &REQUIREMENT_CHANNEL)? {
        AuthState::Missing => return Err(BrokerError::InvalidAuthState),
        AuthState::Waiting | AuthState::Granted | AuthState::Denied => {}
    }
    append_resolution(
        store,
        &request.envelope,
        &binding,
        resolution,
        &REQUIREMENT_CHANNEL,
    )
}

fn preflight(request: &BrokerInvocation<'_>) -> Result<(), BrokerError> {
    validate(request.validation_schema, &request.envelope)
}

fn ensure_auth_credential(
    envelope: &InvocationEnvelope<'_>,
    requirement: &BrokerAuthRequirement,
) -> Result<(), BrokerError> {
    let Some(required) = requirement.credential_id() else {
        return Ok(());
    };
    let matches_request = envelope.extension.credential() == Some(required)
        || envelope
            .extension
            .egress()
            .is_some_and(|egress| egress.credential() == required);
    if matches_request {
        Ok(())
    } else {
        Err(BrokerError::AuthCredentialMismatch)
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthRecord {
    schema_version: u16,
    kind: String,
    challenge_id: String,
    principal_id: String,
    project_id: String,
    invocation_id: String,
    decision_digest: String,
    request_digest: String,
    capability_source: String,
    capability_namespace: String,
    capability_name: String,
    capability_version: String,
    capability_implementation_digest: String,
    schema_digest: String,
    scope: String,
    credential_id: Option<String>,
    trace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transport_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transport_operation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transport_binding: Option<transport_auth::TransportBinding>,
    resolution: Option<String>,
}

struct AuthBinding {
    record: AuthRecord,
    stream: ApprovalId,
    digest: CanonicalRequestDigest,
}

impl AuthBinding {
    fn new(
        request: &BrokerInvocation<'_>,
        requirement: &BrokerAuthRequirement,
        decision_digest: Digest,
    ) -> Result<Self, BrokerError> {
        Self::build(request, requirement, decision_digest, "stream", None, None)
    }

    fn build(
        request: &BrokerInvocation<'_>,
        requirement: &BrokerAuthRequirement,
        decision_digest: Digest,
        stream_kind: &str,
        transport: Option<(&str, &str)>,
        transport_binding: Option<&transport_auth::TransportBinding>,
    ) -> Result<Self, BrokerError> {
        let envelope = &request.envelope;
        let identity = auth_identity(envelope, stream_kind);
        let stream = ApprovalId::from_stable_bytes(identity.as_bytes());
        let request_digest = envelope.canonical_request_digest(decision_digest);
        let record = AuthRecord {
            schema_version: AUTH_RECORD_VERSION,
            kind: "challenge".to_owned(),
            challenge_id: stream.to_string(),
            principal_id: envelope.authenticated.principal_id().to_string(),
            project_id: envelope.project_id.to_string(),
            invocation_id: envelope.invocation_id.to_string(),
            decision_digest: decision_digest.to_string(),
            request_digest: digest_hex(request_digest),
            capability_source: envelope.capability.source().as_str().to_owned(),
            capability_namespace: envelope.capability.namespace().as_str().to_owned(),
            capability_name: envelope.capability.name().as_str().to_owned(),
            capability_version: envelope.capability.version().as_str().to_owned(),
            capability_implementation_digest: envelope
                .capability
                .implementation_digest()
                .to_string(),
            schema_digest: envelope.bound_schema_digest.to_string(),
            scope: requirement.scope.clone(),
            credential_id: requirement
                .credential_id
                .as_ref()
                .map(|credential| credential.identifier().to_owned()),
            trace_id: envelope.trace_id.as_str().to_owned(),
            transport_kind: transport.map(|(kind, _)| kind.to_owned()),
            transport_operation: transport.map(|(_, operation)| operation.to_owned()),
            transport_binding: transport_binding.cloned(),
            resolution: None,
        };
        let bytes = record_bytes(&record)?;
        Ok(Self {
            record,
            stream,
            digest: canonical_digest(&bytes),
        })
    }

    fn challenge(&self) -> AuthChallenge {
        AuthChallenge {
            challenge_id: self.stream,
            principal_id: self.record.principal_id.clone(),
            project_id: self.record.project_id.clone(),
            invocation_id: self.record.invocation_id.clone(),
            decision_digest: self.record.decision_digest.clone(),
            request_digest: self.record.request_digest.clone(),
            capability_source: self.record.capability_source.clone(),
            capability_namespace: self.record.capability_namespace.clone(),
            capability_name: self.record.capability_name.clone(),
            capability_version: self.record.capability_version.clone(),
            capability_implementation_digest: self.record.capability_implementation_digest.clone(),
            schema_digest: self.record.schema_digest.clone(),
            scope: self.record.scope.clone(),
            credential_id: self
                .record
                .credential_id
                .as_deref()
                .map(SecretHandle::parse)
                .transpose()
                .expect("persisted auth credential was validated"),
            trace_id: self.record.trace_id.clone(),
        }
    }

    fn resolution(
        &self,
        resolution: AuthResolution,
    ) -> Result<(AuthRecord, Vec<u8>, CanonicalRequestDigest), BrokerError> {
        let mut record = self.record.clone();
        record.kind = "resolution".to_owned();
        record.resolution = Some(
            match resolution {
                AuthResolution::Granted => "granted",
                AuthResolution::Denied => "denied",
            }
            .to_owned(),
        );
        let bytes = record_bytes(&record)?;
        let digest = canonical_digest(&bytes);
        Ok((record, bytes, digest))
    }
}

enum AuthState {
    Missing,
    Waiting,
    Granted,
    Denied,
}

fn auth_state(
    store: &mut SqliteStore,
    envelope: &InvocationEnvelope<'_>,
    binding: &AuthBinding,
    channel: &AuthChannel,
) -> Result<AuthState, BrokerError> {
    let key = auth_key(envelope)?;
    let challenge = store
        .idempotency_status(
            &auth_scope(envelope, binding.stream, channel.challenge_command)?,
            &key,
        )
        .map_err(BrokerError::AuthStore)?;
    let resolution = store
        .idempotency_status(
            &auth_scope(envelope, binding.stream, channel.resolution_command)?,
            &key,
        )
        .map_err(BrokerError::AuthStore)?;
    match challenge {
        IdempotencyStatus::Missing => {
            if matches!(resolution, IdempotencyStatus::Missing) {
                Ok(AuthState::Missing)
            } else {
                Err(BrokerError::InvalidAuthState)
            }
        }
        IdempotencyStatus::Pending { .. } => Err(BrokerError::InvalidAuthState),
        IdempotencyStatus::Terminal {
            request_digest,
            result,
        } => {
            if request_digest != binding.digest
                || result.commit_positions.len() != 1
                || checked_record(&result.response)? != binding.record
            {
                return Err(BrokerError::InvalidAuthState);
            }
            match resolution {
                IdempotencyStatus::Missing => Ok(AuthState::Waiting),
                IdempotencyStatus::Pending { .. } => Err(BrokerError::InvalidAuthState),
                IdempotencyStatus::Terminal {
                    request_digest,
                    result,
                } => {
                    if result.commit_positions.len() != 1 {
                        return Err(BrokerError::InvalidAuthState);
                    }
                    let record = checked_record(&result.response)?;
                    let resolution = match record.resolution.as_deref() {
                        Some("granted") => AuthResolution::Granted,
                        Some("denied") => AuthResolution::Denied,
                        _ => return Err(BrokerError::InvalidAuthState),
                    };
                    let (expected, _, expected_digest) = binding.resolution(resolution)?;
                    if request_digest != expected_digest || record != expected {
                        return Err(BrokerError::InvalidAuthState);
                    }
                    Ok(match resolution {
                        AuthResolution::Granted => AuthState::Granted,
                        AuthResolution::Denied => AuthState::Denied,
                    })
                }
            }
        }
    }
}

fn append_challenge(
    store: &mut SqliteStore,
    envelope: &InvocationEnvelope<'_>,
    binding: &AuthBinding,
    channel: &AuthChannel,
) -> Result<(), BrokerError> {
    let bytes = record_bytes(&binding.record)?;
    append_auth(
        store,
        envelope,
        binding.stream,
        channel.challenge_command,
        channel.challenge_event,
        0,
        binding.digest,
        bytes,
        false,
    )
    .map(|_| ())
}

fn append_resolution(
    store: &mut SqliteStore,
    envelope: &InvocationEnvelope<'_>,
    binding: &AuthBinding,
    resolution: AuthResolution,
    channel: &AuthChannel,
) -> Result<(), BrokerError> {
    let (_, bytes, digest) = binding.resolution(resolution)?;
    append_auth(
        store,
        envelope,
        binding.stream,
        channel.resolution_command,
        channel.resolution_event,
        1,
        digest,
        bytes,
        true,
    )
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
fn append_auth(
    store: &mut SqliteStore,
    envelope: &InvocationEnvelope<'_>,
    stream: ApprovalId,
    command: &str,
    event_type: &str,
    version: u64,
    digest: CanonicalRequestDigest,
    payload: Vec<u8>,
    allow_quiescent_driver_claim: bool,
) -> Result<AppendOutcome, BrokerError> {
    let event_id = EventId::from_stable_bytes(
        format!("{}:{stream}", auth_identity(envelope, event_type)).as_bytes(),
    );
    store
        .append(AppendCommand {
            idempotency_scope: auth_scope(envelope, stream, command)?,
            idempotency_key: auth_key(envelope)?,
            request_digest: digest,
            claim: None,
            driver_claim: envelope.driver_claim,
            allow_quiescent_driver_claim,
            expected_versions: vec![ExpectedStreamVersion {
                stream: EntityId::Approval(stream),
                version: ExpectedVersion::new(version),
            }],
            events: vec![NewEvent {
                id: event_id,
                stream: EntityId::Approval(stream),
                event_type: EventType::parse(event_type).expect("broker auth event type is valid"),
                schema_version: SchemaVersion::CURRENT,
                occurred_at: envelope.occurred_at.clone(),
                causation_id: envelope.command_id,
                correlation_id: EntityId::Run(envelope.config.run_id()),
                attempt_id: Some(envelope.attempt.attempt_id),
                trace_id: envelope.trace_id.clone(),
                payload: payload.clone(),
                artifacts: b"[]".to_vec(),
            }],
            response: payload,
        })
        .map_err(BrokerError::AuthStore)
}

fn auth_identity(envelope: &InvocationEnvelope<'_>, kind: &str) -> String {
    format!(
        "broker-auth:{kind}:{}:{}:{}:{}:{}",
        envelope.authenticated.principal_id(),
        envelope.project_id,
        envelope.config.run_id(),
        envelope.attempt.attempt_id,
        envelope.invocation_id
    )
}

fn auth_scope(
    envelope: &InvocationEnvelope<'_>,
    stream: ApprovalId,
    command: &str,
) -> Result<IdempotencyScope, BrokerError> {
    IdempotencyScope::new(
        envelope.authenticated.principal_id(),
        command,
        EntityId::Approval(stream),
    )
    .map_err(|_| BrokerError::InvalidAuthState)
}

fn auth_key(envelope: &InvocationEnvelope<'_>) -> Result<IdempotencyKey, BrokerError> {
    IdempotencyKey::parse(&format!("auth-{}", envelope.invocation_id))
        .map_err(|_| BrokerError::InvalidAuthState)
}

fn record_bytes(record: &AuthRecord) -> Result<Vec<u8>, BrokerError> {
    let bytes = serde_json::to_vec(record).map_err(|_| BrokerError::InvalidAuthState)?;
    if bytes.len() > MAX_AUTH_RECORD_BYTES {
        return Err(BrokerError::InvalidAuthRequirement);
    }
    Ok(bytes)
}

fn checked_record(bytes: &[u8]) -> Result<AuthRecord, BrokerError> {
    if bytes.len() > MAX_AUTH_RECORD_BYTES {
        return Err(BrokerError::InvalidAuthState);
    }
    let record: AuthRecord =
        serde_json::from_slice(bytes).map_err(|_| BrokerError::InvalidAuthState)?;
    if record.schema_version != AUTH_RECORD_VERSION {
        return Err(BrokerError::InvalidAuthState);
    }
    Ok(record)
}

fn canonical_digest(bytes: &[u8]) -> CanonicalRequestDigest {
    CanonicalRequestDigest::new(Digest::of(DigestAlgorithm::Sha256, bytes).as_bytes())
}

fn digest_hex(digest: CanonicalRequestDigest) -> String {
    use fmt::Write as _;

    let mut output = String::from("sha256:");
    for byte in digest.as_bytes() {
        write!(output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

fn validate(
    schema: &NormalizedSchema,
    envelope: &InvocationEnvelope<'_>,
) -> Result<(), BrokerError> {
    if envelope.discovered_schema_digest != envelope.bound_schema_digest {
        return Err(BrokerError::Invoke(InvokeError::SchemaBindingMismatch));
    }
    if envelope.arguments.len() > MAX_INVOCATION_ARGUMENT_BYTES {
        return Err(BrokerError::Invoke(InvokeError::InvalidArguments));
    }
    if schema.source().normalized_digest() != envelope.bound_schema_digest {
        return Err(BrokerError::SchemaBindingMismatch);
    }
    let arguments = serde_json::from_slice(envelope.arguments)
        .map_err(|_| BrokerError::Invoke(InvokeError::InvalidArguments))?;
    let validation = schema.validate(&arguments);
    if validation == SchemaValidation::Unsupported {
        return Err(BrokerError::UnsupportedValidation);
    }
    match validation {
        SchemaValidation::Valid => Ok(()),
        SchemaValidation::Invalid => Err(BrokerError::InvalidArguments),
        SchemaValidation::Unsupported => unreachable!("handled above"),
    }
}
