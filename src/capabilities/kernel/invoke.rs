use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};

use crate::{
    agent::accounting::AccountingError,
    api::{auth::contract::AuthenticatedPrincipal, service::AttemptDriverClaim},
    capabilities::result::Presentation,
    domain::{
        commands::ExpectedVersion,
        config::RunConfigSnapshot,
        events::{ArtifactRef, EntityId, EventType, SchemaVersion, TraceId, UtcDateTime},
        ids::{CommandId, EventId, ProjectId, ToolCallId, WorkspaceId},
        lifecycle::AttemptOwnership,
    },
    runtime::scheduler::{
        limits::Spend,
        reserve::{BudgetError, BudgetLedger, ReservationId, ReservationSnapshot},
    },
    store::sqlite::{
        append::{
            AppendCommand, AppendOutcome, ExpectedStreamVersion, NewEvent, SqliteStore, StoreError,
        },
        idempotency::{
            CanonicalRequestDigest, IdempotencyKey, IdempotencyScope, IdempotencyStatus,
        },
    },
    telemetry::tool_learning::{
        ErrorClass as LearningErrorClass, ErrorCode as LearningErrorCode,
        ErrorStage as LearningErrorStage, LearningFailure, LearningStatus, PreparedLearningCapture,
        RetryClass as LearningRetryClass,
    },
};

use super::{
    grant::{
        self, ArgumentConstraints, CapabilityGrantSnapshot, DelegationSnapshot, EffectClass,
        GrantReasonCode, GrantRequest,
    },
    grant_ext::RequestExtension,
    identity::{CapabilityIdentity, Digest, DigestAlgorithm, put_bytes, put_digest},
};

const INTENT_COMMAND: &str = "capability.invoke.intent";
const DISPATCH_COMMAND: &str = "capability.invoke.dispatch";
const OUTCOME_COMMAND: &str = "capability.invoke.outcome";
const INTENT_EVENT: &str = "capability.invocation_intent";
const DISPATCH_EVENT: &str = "capability.invocation_dispatched";
const OUTCOME_EVENT: &str = "capability.invocation_outcome";
pub const MAX_INVOCATION_ARGUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_INVOCATION_ARTIFACT_DIGESTS: usize = 128;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrySafety {
    Idempotent,
    NonIdempotent,
}

impl RetrySafety {
    pub(crate) const fn tag(self) -> u8 {
        match self {
            Self::Idempotent => 0,
            Self::NonIdempotent => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    NotRequired,
    Pending,
    Approved,
    Denied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InvocationCrashPoint {
    BeforeIntent,
    BetweenIntentAndDispatch,
    AfterDispatch,
    BeforeOutcome,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CanonicalOutput {
    pub media_type: String,
    pub body: Vec<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_digests: Vec<ArtifactRef>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DispatchOutcome {
    Succeeded(CanonicalOutput),
    DurablyCommitted(CanonicalOutput),
    Failed {
        code: String,
    },
    DurablyFailed {
        code: String,
        output: CanonicalOutput,
    },
    OutcomeUnknown {
        code: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationStatus {
    Succeeded,
    Failed,
    ApprovalRequired,
    ApprovalDenied,
    Cancelled,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CanonicalInvocationResult {
    pub status: InvocationStatus,
    pub output: Option<CanonicalOutput>,
    pub code: Option<String>,
    pub charged: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvocationResult {
    pub canonical: CanonicalInvocationResult,
    pub presentation: Option<Presentation>,
    pub replayed: bool,
    pub reservation: ReservationSnapshot,
}

#[derive(Clone)]
pub struct InvocationEnvelope<'a> {
    pub authenticated: &'a AuthenticatedPrincipal,
    pub config: &'a RunConfigSnapshot,
    pub grants: &'a CapabilityGrantSnapshot,
    pub delegation: Option<&'a DelegationSnapshot>,
    pub extension: RequestExtension,
    pub capability: &'a CapabilityIdentity,
    pub discovered_schema_digest: Digest,
    pub bound_schema_digest: Digest,
    pub effect: EffectClass,
    pub argument_constraints: &'a ArgumentConstraints,
    pub arguments: &'a [u8],
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub invocation_id: ToolCallId,
    pub idempotency_key: &'a IdempotencyKey,
    pub reservation: Spend,
    pub retry_safety: RetrySafety,
    pub approval: ApprovalState,
    pub cancellation: &'a Arc<AtomicBool>,
    pub attempt: AttemptOwnership,
    pub driver_claim: Option<AttemptDriverClaim>,
    pub current_fence: &'a Arc<AtomicU64>,
    pub command_id: CommandId,
    pub intent_event_id: EventId,
    pub outcome_event_id: EventId,
    pub occurred_at: &'a UtcDateTime,
    pub trace_id: &'a TraceId,
    pub learning: Option<&'a PreparedLearningCapture>,
}

impl InvocationEnvelope<'_> {
    pub(crate) fn grant_request(&self) -> GrantRequest<'_> {
        self.grant_request_for(self.authenticated)
    }

    pub(crate) fn grant_request_for<'a>(
        &'a self,
        authenticated: &'a AuthenticatedPrincipal,
    ) -> GrantRequest<'a> {
        GrantRequest {
            authenticated,
            capability: self.capability,
            schema_digest: self.bound_schema_digest,
            effect: self.effect,
            argument_constraints: self.argument_constraints,
            workspace_id: self.workspace_id,
            project_id: self.project_id,
            config: self.config,
            grants: self.grants,
            delegation: self.delegation,
            extension: self.extension.clone(),
        }
    }

    pub(crate) fn canonical_request_digest(
        &self,
        decision_digest: Digest,
    ) -> CanonicalRequestDigest {
        request_digest(self, decision_digest)
    }

    pub(crate) fn preflight_authority(
        &self,
        store: &mut SqliteStore,
        allow_quiescent_driver_claim: bool,
    ) -> Result<(), InvokeError> {
        ensure_current_fence(self)?;
        if self.attempt.principal_id != self.authenticated.principal_id() {
            return Err(InvokeError::StaleFence);
        }
        let claim = self.driver_claim.ok_or(InvokeError::MissingDriverClaim)?;
        if claim.owner() != self.attempt || claim.run_id != self.config.run_id() {
            return Err(InvokeError::StaleFence);
        }
        if allow_quiescent_driver_claim {
            store.verify_quiescent_driver_claim(claim)?;
        } else {
            store.verify_driver_claim(claim)?;
        }
        Ok(())
    }

    fn preflight(
        &self,
        store: &mut SqliteStore,
        allow_quiescent_driver_claim: bool,
    ) -> Result<(), InvokeError> {
        self.preflight_authority(store, allow_quiescent_driver_claim)?;
        if self.discovered_schema_digest != self.bound_schema_digest {
            return Err(InvokeError::SchemaBindingMismatch);
        }
        if self.arguments.len() > MAX_INVOCATION_ARGUMENT_BYTES {
            return Err(InvokeError::InvalidArguments);
        }
        serde_json::from_slice::<serde_json::Value>(self.arguments)
            .map_err(|_| InvokeError::InvalidArguments)?;
        Ok(())
    }
}

impl<'a> InvocationEnvelope<'a> {
    pub(crate) fn bind_transport_arguments<'b>(
        &'b self,
        arguments: &'b [u8],
    ) -> InvocationEnvelope<'b>
    where
        'a: 'b,
    {
        InvocationEnvelope {
            authenticated: self.authenticated,
            config: self.config,
            grants: self.grants,
            delegation: self.delegation,
            extension: self.extension.clone(),
            capability: self.capability,
            discovered_schema_digest: self.discovered_schema_digest,
            bound_schema_digest: self.bound_schema_digest,
            effect: self.effect,
            argument_constraints: self.argument_constraints,
            arguments,
            workspace_id: self.workspace_id,
            project_id: self.project_id,
            invocation_id: self.invocation_id,
            idempotency_key: self.idempotency_key,
            reservation: self.reservation,
            retry_safety: self.retry_safety,
            approval: self.approval,
            cancellation: self.cancellation,
            attempt: self.attempt,
            driver_claim: self.driver_claim,
            current_fence: self.current_fence,
            command_id: self.command_id,
            intent_event_id: self.intent_event_id,
            outcome_event_id: self.outcome_event_id,
            occurred_at: self.occurred_at,
            trace_id: self.trace_id,
            learning: self.learning,
        }
    }

    pub(crate) fn bind_external<'b>(
        self,
        capability: &'b CapabilityIdentity,
        schema_digest: Digest,
        effect: EffectClass,
        retry_safety: RetrySafety,
        arguments: &'b [u8],
    ) -> InvocationEnvelope<'b>
    where
        'a: 'b,
    {
        InvocationEnvelope {
            authenticated: self.authenticated,
            config: self.config,
            grants: self.grants,
            delegation: self.delegation,
            extension: self.extension,
            capability,
            discovered_schema_digest: schema_digest,
            bound_schema_digest: schema_digest,
            effect,
            argument_constraints: self.argument_constraints,
            arguments,
            workspace_id: self.workspace_id,
            project_id: self.project_id,
            invocation_id: self.invocation_id,
            idempotency_key: self.idempotency_key,
            reservation: self.reservation,
            retry_safety,
            approval: self.approval,
            cancellation: self.cancellation,
            attempt: self.attempt,
            driver_claim: self.driver_claim,
            current_fence: self.current_fence,
            command_id: self.command_id,
            intent_event_id: self.intent_event_id,
            outcome_event_id: self.outcome_event_id,
            occurred_at: self.occurred_at,
            trace_id: self.trace_id,
            learning: self.learning,
        }
    }
}

pub struct AuthorizedInvocation {
    capability: CapabilityIdentity,
    schema_digest: Digest,
    effect: EffectClass,
    arguments: Vec<u8>,
    invocation_id: ToolCallId,
    idempotency_key: String,
    attempt: AttemptOwnership,
    extension: RequestExtension,
    request_digest: CanonicalRequestDigest,
    reservation_id: ReservationId,
}

impl AuthorizedInvocation {
    pub fn capability(&self) -> &CapabilityIdentity {
        &self.capability
    }

    pub const fn schema_digest(&self) -> Digest {
        self.schema_digest
    }

    pub const fn effect(&self) -> EffectClass {
        self.effect
    }

    pub fn arguments(&self) -> &[u8] {
        &self.arguments
    }

    pub const fn invocation_id(&self) -> ToolCallId {
        self.invocation_id
    }

    pub fn idempotency_key(&self) -> &str {
        &self.idempotency_key
    }

    pub const fn attempt(&self) -> AttemptOwnership {
        self.attempt
    }

    pub const fn extension(&self) -> &RequestExtension {
        &self.extension
    }
}

pub(crate) enum PrepareOutcome {
    Authorized(Box<AuthorizedInvocation>),
    Completed(Box<InvocationResult>),
}

pub(crate) struct InvocationRuntime<'a> {
    store: &'a mut SqliteStore,
    budget: &'a BudgetLedger,
    backend: &'a mut dyn FnMut(&AuthorizedInvocation) -> DispatchOutcome,
    crash_at: Option<InvocationCrashPoint>,
}

pub(crate) struct InvocationPhaseRuntime<'a> {
    store: &'a mut SqliteStore,
    budget: &'a BudgetLedger,
    crash_at: Option<InvocationCrashPoint>,
}

impl<'a> InvocationPhaseRuntime<'a> {
    pub(crate) const fn new(
        store: &'a mut SqliteStore,
        budget: &'a BudgetLedger,
        crash_at: Option<InvocationCrashPoint>,
    ) -> Self {
        Self {
            store,
            budget,
            crash_at,
        }
    }
}

impl<'a> InvocationRuntime<'a> {
    pub(crate) fn new(
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

    pub(crate) fn with_crash_at(mut self, crash_at: InvocationCrashPoint) -> Self {
        self.crash_at = Some(crash_at);
        self
    }
}

struct Dispatcher<'a> {
    backend: &'a mut dyn FnMut(&AuthorizedInvocation) -> DispatchOutcome,
}

impl Dispatcher<'_> {
    fn dispatch(&mut self, invocation: &AuthorizedInvocation) -> DispatchOutcome {
        (self.backend)(invocation)
    }
}

#[derive(Debug)]
pub enum InvokeError {
    AuthorizationDenied(GrantReasonCode),
    SchemaBindingMismatch,
    InvalidArguments,
    MissingDriverClaim,
    StaleFence,
    Budget(BudgetError),
    Store(StoreError),
    InvalidPersistedOutcome,
    ArtifactDigestLimit,
    Serialization(serde_json::Error),
    InjectedCrash(InvocationCrashPoint),
    NativeCapabilityBinding,
    UnsupportedValidation,
    BrokerAuth,
    Accounting(AccountingError),
    ToolReservationRequired,
}

impl fmt::Display for InvokeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthorizationDenied(reason) => {
                write!(f, "capability invocation denied: {reason:?}")
            }
            Self::SchemaBindingMismatch => {
                f.write_str("discovered and bound capability schema digests differ")
            }
            Self::InvalidArguments => f.write_str("capability arguments are not valid JSON"),
            Self::MissingDriverClaim => {
                f.write_str("capability invocation requires an attempt driver claim")
            }
            Self::StaleFence => f.write_str("capability invocation attempt fence is stale"),
            Self::Budget(error) => write!(f, "capability invocation budget error: {error:?}"),
            Self::Store(error) => error.fmt(f),
            Self::InvalidPersistedOutcome => f.write_str("persisted capability outcome is invalid"),
            Self::ArtifactDigestLimit => {
                f.write_str("capability outcome artifact digest limit exceeded")
            }
            Self::Serialization(error) => {
                write!(f, "capability event serialization failed: {error}")
            }
            Self::InjectedCrash(point) => {
                write!(f, "injected capability invocation crash at {point:?}")
            }
            Self::NativeCapabilityBinding => {
                f.write_str("native capability is not bound to an exact descriptor")
            }
            Self::UnsupportedValidation => {
                f.write_str("normalized schema validation is unsupported")
            }
            Self::BrokerAuth => f.write_str("native invocation entered broker auth handling"),
            Self::Accounting(error) => error.fmt(f),
            Self::ToolReservationRequired => {
                f.write_str("capability invocation requires at least one reserved tool")
            }
        }
    }
}

impl std::error::Error for InvokeError {}

impl From<BudgetError> for InvokeError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<StoreError> for InvokeError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<serde_json::Error> for InvokeError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

pub(crate) fn invoke(
    envelope: InvocationEnvelope<'_>,
    runtime: InvocationRuntime<'_>,
) -> Result<InvocationResult, InvokeError> {
    let authorized = match prepare(
        &envelope,
        &mut InvocationPhaseRuntime::new(runtime.store, runtime.budget, runtime.crash_at),
    )? {
        PrepareOutcome::Authorized(authorized) => *authorized,
        PrepareOutcome::Completed(result) => return Ok(*result),
    };
    let dispatched = Dispatcher {
        backend: runtime.backend,
    }
    .dispatch(&authorized);
    complete(
        &envelope,
        &mut InvocationPhaseRuntime::new(runtime.store, runtime.budget, runtime.crash_at),
        authorized,
        dispatched,
    )
}

pub(crate) fn prepare(
    envelope: &InvocationEnvelope<'_>,
    runtime: &mut InvocationPhaseRuntime<'_>,
) -> Result<PrepareOutcome, InvokeError> {
    prepare_with_learning(envelope, runtime, false)
}

pub(crate) fn prepare_resuming_dispatch(
    envelope: &InvocationEnvelope<'_>,
    runtime: &mut InvocationPhaseRuntime<'_>,
) -> Result<PrepareOutcome, InvokeError> {
    prepare_with_learning(envelope, runtime, true)
}

fn prepare_with_learning(
    envelope: &InvocationEnvelope<'_>,
    runtime: &mut InvocationPhaseRuntime<'_>,
    resume_dispatched: bool,
) -> Result<PrepareOutcome, InvokeError> {
    let result = prepare_inner(envelope, runtime, resume_dispatched);
    if let Err(error) = &result
        && let Some(failure) = pre_kernel_learning_failure(error)
    {
        let _ = capture_rejected(envelope, runtime.store, failure);
    }
    result
}

fn pre_kernel_learning_failure(error: &InvokeError) -> Option<LearningFailure> {
    let (stage, class, code, retry) = match error {
        InvokeError::AuthorizationDenied(_) => (
            LearningErrorStage::Authorization,
            LearningErrorClass::Policy,
            LearningErrorCode::AuthorizationDenied,
            LearningRetryClass::Never,
        ),
        InvokeError::SchemaBindingMismatch
        | InvokeError::InvalidArguments
        | InvokeError::UnsupportedValidation => (
            LearningErrorStage::SchemaValidation,
            LearningErrorClass::Input,
            LearningErrorCode::InvalidSchema,
            LearningRetryClass::Never,
        ),
        InvokeError::Budget(_) | InvokeError::ToolReservationRequired => (
            LearningErrorStage::Dispatch,
            LearningErrorClass::Budget,
            LearningErrorCode::BudgetUnavailable,
            LearningRetryClass::Safe,
        ),
        _ => return None,
    };
    Some(LearningFailure {
        stage,
        class,
        code,
        field: None,
        retry,
        dispatched: false,
        known: true,
    })
}

pub(crate) fn replay(
    envelope: &InvocationEnvelope<'_>,
    runtime: &mut InvocationPhaseRuntime<'_>,
) -> Result<Option<InvocationResult>, InvokeError> {
    envelope.preflight(runtime.store, true)?;
    let decision = grant::decide(envelope.grant_request());
    let reason = decision.reason();
    let decision_digest = decision.snapshot_digest();
    decision
        .into_authorized_inputs()
        .ok_or(InvokeError::AuthorizationDenied(reason))?;
    let request_digest = request_digest(envelope, decision_digest);
    let Some(result) = persisted_outcome(envelope, runtime.store, request_digest)? else {
        return Ok(None);
    };
    reconcile_persisted_learning(envelope, runtime.store, request_digest, &result)?;
    let reservation_id = reservation_id(request_digest);
    runtime
        .budget
        .reserve(reservation_id, envelope.reservation)?;
    settle(runtime.budget, reservation_id, result, true).map(Some)
}

pub(crate) fn capture_rejected(
    envelope: &InvocationEnvelope<'_>,
    store: &mut SqliteStore,
    failure: LearningFailure,
) -> Result<(), InvokeError> {
    let Some(capture) = envelope.learning else {
        return Ok(());
    };
    let decision_digest = grant::decide(envelope.grant_request()).snapshot_digest();
    let request_digest = request_digest(envelope, decision_digest);
    let claim = envelope
        .driver_claim
        .ok_or(InvokeError::MissingDriverClaim)?;
    let prepared = if matches!(
        failure.retry,
        LearningRetryClass::AuthorizationResume | LearningRetryClass::UrlResume
    ) {
        crate::telemetry::tool_learning::prepare_invocation_interruption(
            store,
            claim,
            capture,
            envelope.occurred_at.clone(),
            envelope.trace_id.clone(),
            request_digest.as_bytes(),
            failure.clone(),
        )
    } else {
        crate::telemetry::tool_learning::prepare_invocation_terminal(
            store,
            claim,
            capture,
            envelope.occurred_at.clone(),
            envelope.trace_id.clone(),
            request_digest.as_bytes(),
            Some(failure.clone()),
            if failure.known {
                LearningStatus::Failed
            } else {
                LearningStatus::OutcomeUnknown
            },
            failure.dispatched,
            failure.known,
            None,
            None,
            true,
        )
    };
    persist_learning(envelope, store, capture, "rejected", prepared, false)
}

pub(crate) fn capture_external_failure(
    envelope: &InvocationEnvelope<'_>,
    store: &mut SqliteStore,
    code: &str,
) -> Result<(), InvokeError> {
    let dispatched = store.invocation_was_dispatched(envelope.invocation_id)?;
    let failure = external_failure(code, envelope.retry_safety, dispatched);
    capture_rejected(envelope, store, failure)
}

fn external_failure(code: &str, retry_safety: RetrySafety, dispatched: bool) -> LearningFailure {
    let result = if dispatched {
        unknown(code, true)
    } else {
        terminal(InvocationStatus::Failed, None, Some(code), false)
    };
    let mut failure = learning_failure(&result, retry_safety, dispatched)
        .expect("failed external invocation has learning failure metadata");
    failure.dispatched = dispatched;
    failure.known = !dispatched;
    failure
}

fn prepare_inner(
    envelope: &InvocationEnvelope<'_>,
    runtime: &mut InvocationPhaseRuntime<'_>,
    resume_dispatched: bool,
) -> Result<PrepareOutcome, InvokeError> {
    envelope.preflight(runtime.store, false)?;

    let decision = grant::decide(envelope.grant_request());
    let decision_digest = decision.snapshot_digest();
    let reason = decision.reason();
    let authorized_inputs = decision
        .into_authorized_inputs()
        .ok_or(InvokeError::AuthorizationDenied(reason))?;

    let request_digest = request_digest(envelope, decision_digest);
    let reservation_id = reservation_id(request_digest);
    runtime
        .budget
        .reserve(reservation_id, envelope.reservation)?;

    if runtime.crash_at == Some(InvocationCrashPoint::BeforeIntent) {
        runtime.budget.release(reservation_id)?;
        return Err(InvokeError::InjectedCrash(
            InvocationCrashPoint::BeforeIntent,
        ));
    }

    append_intent(envelope, runtime.store, request_digest, reservation_id)?;
    if let Some(result) = persisted_outcome(envelope, runtime.store, request_digest)? {
        reconcile_persisted_learning(envelope, runtime.store, request_digest, &result)?;
        return settle(runtime.budget, reservation_id, result, true)
            .map(Box::new)
            .map(PrepareOutcome::Completed);
    }

    if runtime.crash_at == Some(InvocationCrashPoint::BetweenIntentAndDispatch) {
        persist_injected_crash(
            envelope,
            runtime,
            request_digest,
            reservation_id,
            InvocationCrashPoint::BetweenIntentAndDispatch,
            unknown("interrupted_before_dispatch", false),
            false,
        )?;
    }

    if envelope.cancellation.load(Ordering::Acquire) {
        let result = terminal(InvocationStatus::Cancelled, None, Some("cancelled"), false);
        let result = append_outcome(envelope, runtime.store, request_digest, &result, false)?;
        return settle(runtime.budget, reservation_id, result, false)
            .map(Box::new)
            .map(PrepareOutcome::Completed);
    }

    let interruption = match envelope.approval {
        ApprovalState::Pending => Some(terminal(
            InvocationStatus::ApprovalRequired,
            None,
            Some("approval_required"),
            false,
        )),
        ApprovalState::Denied => Some(terminal(
            InvocationStatus::ApprovalDenied,
            None,
            Some("approval_denied"),
            false,
        )),
        ApprovalState::NotRequired | ApprovalState::Approved => None,
    };
    if let Some(result) = interruption {
        let result = append_outcome(envelope, runtime.store, request_digest, &result, false)?;
        return settle(runtime.budget, reservation_id, result, false)
            .map(Box::new)
            .map(PrepareOutcome::Completed);
    }

    if ensure_current_fence(envelope).is_err()
        || ensure_current_claim(envelope, runtime.store).is_err()
    {
        let result = unknown("stale_fence_before_dispatch", false);
        let result = append_outcome(envelope, runtime.store, request_digest, &result, false)?;
        return settle(runtime.budget, reservation_id, result, false)
            .map(Box::new)
            .map(PrepareOutcome::Completed);
    }

    let dispatch_replayed = append_dispatch(envelope, runtime.store, request_digest)?;
    if dispatch_replayed && !resume_dispatched {
        let result = unknown("recovery_requires_reconciliation", true);
        let result = append_outcome(envelope, runtime.store, request_digest, &result, true)?;
        return settle(runtime.budget, reservation_id, result, false)
            .map(Box::new)
            .map(PrepareOutcome::Completed);
    }

    let authorized = AuthorizedInvocation {
        capability: envelope.capability.clone(),
        schema_digest: envelope.bound_schema_digest,
        effect: envelope.effect,
        arguments: envelope.arguments.to_vec(),
        invocation_id: envelope.invocation_id,
        idempotency_key: envelope.idempotency_key.as_str().to_owned(),
        attempt: envelope.attempt,
        extension: authorized_inputs.into_extension(),
        request_digest,
        reservation_id,
    };

    Ok(PrepareOutcome::Authorized(Box::new(authorized)))
}

pub(crate) fn complete(
    envelope: &InvocationEnvelope<'_>,
    runtime: &mut InvocationPhaseRuntime<'_>,
    authorized: AuthorizedInvocation,
    dispatched: DispatchOutcome,
) -> Result<InvocationResult, InvokeError> {
    let request_digest = authorized.request_digest;
    let reservation_id = authorized.reservation_id;

    if runtime.crash_at == Some(InvocationCrashPoint::AfterDispatch) {
        persist_injected_crash(
            envelope,
            runtime,
            request_digest,
            reservation_id,
            InvocationCrashPoint::AfterDispatch,
            unknown("interrupted_after_dispatch", true),
            true,
        )?;
    }

    if ensure_current_fence(envelope).is_err()
        || ensure_current_claim(envelope, runtime.store).is_err()
    {
        let result = unknown("stale_fence_after_dispatch", true);
        let result = append_outcome(envelope, runtime.store, request_digest, &result, true)?;
        return settle(runtime.budget, reservation_id, result, false);
    }
    if envelope.cancellation.load(Ordering::Acquire)
        && !matches!(
            &dispatched,
            DispatchOutcome::DurablyCommitted(_) | DispatchOutcome::DurablyFailed { .. }
        )
    {
        let result = unknown("cancelled_after_dispatch", true);
        let result = append_outcome(envelope, runtime.store, request_digest, &result, true)?;
        return settle(runtime.budget, reservation_id, result, false);
    }

    if runtime.crash_at == Some(InvocationCrashPoint::BeforeOutcome) {
        persist_injected_crash(
            envelope,
            runtime,
            request_digest,
            reservation_id,
            InvocationCrashPoint::BeforeOutcome,
            unknown("interrupted_before_outcome", true),
            true,
        )?;
    }

    let mut result = match dispatched {
        DispatchOutcome::Succeeded(output) => {
            terminal(InvocationStatus::Succeeded, Some(output), None, true)
        }
        DispatchOutcome::DurablyCommitted(output) => {
            terminal(InvocationStatus::Succeeded, Some(output), None, true)
        }
        DispatchOutcome::Failed { code } => {
            terminal(InvocationStatus::Failed, None, Some(&code), true)
        }
        DispatchOutcome::DurablyFailed { code, output } => {
            terminal(InvocationStatus::Failed, Some(output), Some(&code), true)
        }
        DispatchOutcome::OutcomeUnknown { code } => unknown(&code, true),
    };
    if output_artifacts(&result).is_err() {
        result = terminal(
            InvocationStatus::Failed,
            None,
            Some("artifact_digest_limit"),
            true,
        );
    }
    let result = append_outcome(envelope, runtime.store, request_digest, &result, true)?;
    settle(runtime.budget, reservation_id, result, false)
}

fn ensure_current_fence(envelope: &InvocationEnvelope<'_>) -> Result<(), InvokeError> {
    if envelope.current_fence.load(Ordering::Acquire) == envelope.attempt.fencing_token.get() {
        Ok(())
    } else {
        Err(InvokeError::StaleFence)
    }
}

fn ensure_current_claim(
    envelope: &InvocationEnvelope<'_>,
    store: &mut SqliteStore,
) -> Result<(), InvokeError> {
    store
        .verify_driver_claim(
            envelope
                .driver_claim
                .ok_or(InvokeError::MissingDriverClaim)?,
        )
        .map_err(InvokeError::Store)
}

fn terminal(
    status: InvocationStatus,
    output: Option<CanonicalOutput>,
    code: Option<&str>,
    charged: bool,
) -> CanonicalInvocationResult {
    CanonicalInvocationResult {
        status,
        output,
        code: code.map(str::to_owned),
        charged,
    }
}

fn unknown(code: &str, charged: bool) -> CanonicalInvocationResult {
    terminal(InvocationStatus::OutcomeUnknown, None, Some(code), charged)
}

fn settle(
    budget: &BudgetLedger,
    reservation_id: ReservationId,
    canonical: CanonicalInvocationResult,
    replayed: bool,
) -> Result<InvocationResult, InvokeError> {
    let reservation = if canonical.charged {
        budget.commit(reservation_id)?
    } else {
        budget.release(reservation_id)?
    };
    Ok(InvocationResult {
        canonical,
        presentation: None,
        replayed,
        reservation,
    })
}

fn persist_injected_crash(
    envelope: &InvocationEnvelope<'_>,
    runtime: &mut InvocationPhaseRuntime<'_>,
    request_digest: CanonicalRequestDigest,
    reservation_id: ReservationId,
    point: InvocationCrashPoint,
    result: CanonicalInvocationResult,
    dispatched: bool,
) -> Result<InvocationResult, InvokeError> {
    let result = append_outcome(envelope, runtime.store, request_digest, &result, dispatched)?;
    let _ = settle(runtime.budget, reservation_id, result, false)?;
    Err(InvokeError::InjectedCrash(point))
}

fn request_digest(
    envelope: &InvocationEnvelope<'_>,
    decision_digest: Digest,
) -> CanonicalRequestDigest {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"KCAPINVOKE\0");
    put_bytes(
        &mut bytes,
        envelope.authenticated.principal_id().to_string().as_bytes(),
    );
    put_bytes(&mut bytes, envelope.project_id.to_string().as_bytes());
    put_bytes(&mut bytes, envelope.workspace_id.to_string().as_bytes());
    put_bytes(&mut bytes, envelope.invocation_id.to_string().as_bytes());
    envelope.capability.write_canonical(&mut bytes);
    put_digest(&mut bytes, envelope.bound_schema_digest);
    bytes.push(envelope.effect.tag());
    bytes.extend_from_slice(
        &(envelope.argument_constraints.predicates().len() as u64).to_be_bytes(),
    );
    for predicate in envelope.argument_constraints.predicates() {
        put_bytes(&mut bytes, predicate.as_bytes());
    }
    put_bytes(&mut bytes, envelope.arguments);
    bytes.extend_from_slice(&envelope.config.digest());
    put_digest(&mut bytes, envelope.grants.digest());
    put_digest(&mut bytes, decision_digest);
    match envelope.delegation {
        Some(delegation) => {
            bytes.push(1);
            put_digest(&mut bytes, delegation.digest());
        }
        None => bytes.push(0),
    }
    bytes.push(envelope.retry_safety.tag());
    bytes.push(match envelope.approval {
        ApprovalState::NotRequired => 0,
        ApprovalState::Pending => 1,
        ApprovalState::Approved => 2,
        ApprovalState::Denied => 3,
    });
    put_spend(&mut bytes, envelope.reservation);
    CanonicalRequestDigest::new(Digest::of(DigestAlgorithm::Sha256, &bytes).as_bytes())
}

fn reservation_id(digest: CanonicalRequestDigest) -> ReservationId {
    ReservationId::new(u128::from_be_bytes(
        digest.as_bytes()[..16].try_into().unwrap(),
    ))
}

fn append_intent(
    envelope: &InvocationEnvelope<'_>,
    store: &mut SqliteStore,
    request_digest: CanonicalRequestDigest,
    reservation_id: ReservationId,
) -> Result<bool, InvokeError> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "invocation_id": envelope.invocation_id.to_string(),
        "principal_id": envelope.authenticated.principal_id().to_string(),
        "project_id": envelope.project_id.to_string(),
        "workspace_id": envelope.workspace_id.to_string(),
        "capability": {
            "source": envelope.capability.source().as_str(),
            "namespace": envelope.capability.namespace().as_str(),
            "name": envelope.capability.name().as_str(),
            "version": envelope.capability.version().as_str(),
            "implementation_digest": envelope.capability.implementation_digest().to_string(),
        },
        "schema_digest": envelope.bound_schema_digest.to_string(),
        "effect": effect_name(envelope.effect),
        "arguments_digest": Digest::of(DigestAlgorithm::Sha256, envelope.arguments).to_string(),
        "grant_snapshot_digest": envelope.grants.digest().to_string(),
        "config_snapshot_digest": hex(&envelope.config.digest()),
        "idempotency_key": envelope.idempotency_key.as_str(),
        "retry_safety": envelope.retry_safety,
        "attempt_id": envelope.attempt.attempt_id.to_string(),
        "attempt_fence": envelope.attempt.fencing_token.get(),
        "reservation_id": reservation_id.get().to_string(),
        "reservation": spend_value(envelope.reservation),
    }))?;
    let stream = EntityId::ToolCall(envelope.invocation_id);
    let expected_versions = vec![ExpectedStreamVersion {
        stream,
        version: ExpectedVersion::new(0),
    }];
    let events = vec![event(
        envelope,
        envelope.intent_event_id,
        INTENT_EVENT,
        payload,
        b"[]".to_vec(),
    )];
    let outcome = store.append(AppendCommand {
        idempotency_scope: scope(envelope, INTENT_COMMAND)?,
        idempotency_key: envelope.idempotency_key.clone(),
        request_digest,
        claim: None,
        driver_claim: envelope.driver_claim,
        allow_quiescent_driver_claim: false,
        expected_versions,
        events,
        response: b"intent-v1".to_vec(),
    })?;
    if let Some(capture) = envelope.learning {
        let prepared = crate::telemetry::tool_learning::prepare_invocation_intent(
            store,
            envelope
                .driver_claim
                .ok_or(InvokeError::MissingDriverClaim)?,
            capture,
            envelope.occurred_at.clone(),
            envelope.trace_id.clone(),
            request_digest.as_bytes(),
            envelope.intent_event_id,
        );
        persist_learning(envelope, store, capture, "intent", prepared, true)?;
        if capture.required() {
            store.reserve_learning_reconciliation(
                capture.hasher().project().as_str(),
                &envelope.invocation_id.to_string(),
            )?;
        }
    }
    Ok(matches!(outcome, AppendOutcome::Replayed(_)))
}

fn persisted_outcome(
    envelope: &InvocationEnvelope<'_>,
    store: &mut SqliteStore,
    request_digest: CanonicalRequestDigest,
) -> Result<Option<CanonicalInvocationResult>, InvokeError> {
    match store.idempotency_status(&scope(envelope, OUTCOME_COMMAND)?, envelope.idempotency_key)? {
        IdempotencyStatus::Missing => Ok(None),
        IdempotencyStatus::Pending {
            request_digest: found,
        } if found == request_digest => Err(InvokeError::InvalidPersistedOutcome),
        IdempotencyStatus::Pending { .. } => Err(InvokeError::InvalidPersistedOutcome),
        IdempotencyStatus::Terminal {
            request_digest: found,
            result,
        } if found == request_digest => serde_json::from_slice(&result.response)
            .map(Some)
            .map_err(|_| InvokeError::InvalidPersistedOutcome),
        IdempotencyStatus::Terminal { .. } => Err(InvokeError::InvalidPersistedOutcome),
    }
}

fn append_dispatch(
    envelope: &InvocationEnvelope<'_>,
    store: &mut SqliteStore,
    request_digest: CanonicalRequestDigest,
) -> Result<bool, InvokeError> {
    let payload = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "invocation_id": envelope.invocation_id.to_string(),
        "idempotency_key": envelope.idempotency_key.as_str(),
        "attempt_id": envelope.attempt.attempt_id.to_string(),
        "attempt_fence": envelope.attempt.fencing_token.get(),
    }))?;
    let event_id = EventId::from_stable_bytes(
        format!("capability-dispatch:{}", envelope.invocation_id).as_bytes(),
    );
    let outcome = store.append(AppendCommand {
        idempotency_scope: scope(envelope, DISPATCH_COMMAND)?,
        idempotency_key: envelope.idempotency_key.clone(),
        request_digest,
        claim: None,
        driver_claim: envelope.driver_claim,
        allow_quiescent_driver_claim: false,
        expected_versions: vec![ExpectedStreamVersion {
            stream: EntityId::ToolCall(envelope.invocation_id),
            version: ExpectedVersion::new(1),
        }],
        events: vec![event(
            envelope,
            event_id,
            DISPATCH_EVENT,
            payload,
            b"[]".to_vec(),
        )],
        response: b"dispatch-v1".to_vec(),
    })?;
    Ok(matches!(outcome, AppendOutcome::Replayed(_)))
}

fn append_outcome(
    envelope: &InvocationEnvelope<'_>,
    store: &mut SqliteStore,
    request_digest: CanonicalRequestDigest,
    result: &CanonicalInvocationResult,
    dispatched: bool,
) -> Result<CanonicalInvocationResult, InvokeError> {
    let artifacts = output_artifacts(result)?;
    let response = serde_json::to_vec(result)?;
    let payload = serde_json::to_vec(&serde_json::json!({
        "schema_version": 1,
        "invocation_id": envelope.invocation_id.to_string(),
        "idempotency_key": envelope.idempotency_key.as_str(),
        "attempt_id": envelope.attempt.attempt_id.to_string(),
        "attempt_fence": envelope.attempt.fencing_token.get(),
        "result": result,
    }))?;
    let stream = EntityId::ToolCall(envelope.invocation_id);
    let mut expected_versions = vec![ExpectedStreamVersion {
        stream,
        version: ExpectedVersion::new(if dispatched { 2 } else { 1 }),
    }];
    let mut events = vec![event(
        envelope,
        envelope.outcome_event_id,
        OUTCOME_EVENT,
        payload,
        artifacts,
    )];
    if let Some(capture) = envelope.learning {
        let (status, known) = learning_status(result);
        let failure = learning_failure(result, envelope.retry_safety, dispatched);
        let interruption = failure.as_ref().is_some_and(|failure| {
            matches!(
                failure.retry,
                LearningRetryClass::AuthorizationResume | LearningRetryClass::UrlResume
            )
        });
        let prepared = if interruption {
            crate::telemetry::tool_learning::prepare_invocation_interruption(
                store,
                envelope
                    .driver_claim
                    .ok_or(InvokeError::MissingDriverClaim)?,
                capture,
                envelope.occurred_at.clone(),
                envelope.trace_id.clone(),
                request_digest.as_bytes(),
                failure.expect("learning interruption has a failure"),
            )
        } else {
            crate::telemetry::tool_learning::prepare_invocation_terminal(
                store,
                envelope
                    .driver_claim
                    .ok_or(InvokeError::MissingDriverClaim)?,
                capture,
                envelope.occurred_at.clone(),
                envelope.trace_id.clone(),
                request_digest.as_bytes(),
                failure,
                status,
                dispatched,
                known,
                dispatched.then_some(envelope.reservation.cost_microusd()),
                Some(envelope.outcome_event_id),
                true,
            )
        };
        match prepared {
            Ok(Some((version, mut learning_events))) => {
                expected_versions.push(version);
                events.append(&mut learning_events);
            }
            Ok(None) => {}
            Err(error) => {
                capture.mark_failure(&error);
            }
        }
    }
    let command = AppendCommand {
        idempotency_scope: scope(envelope, OUTCOME_COMMAND)?,
        idempotency_key: envelope.idempotency_key.clone(),
        request_digest,
        claim: None,
        driver_claim: envelope.driver_claim,
        allow_quiescent_driver_claim: false,
        expected_versions,
        events,
        response,
    };
    let outcome = if let Some(capture) = envelope.learning {
        let mut provider_only = command.clone();
        provider_only.expected_versions.truncate(1);
        provider_only.events.truncate(1);
        match store.append_with_learning_reconciliation(
            command,
            capture.hasher().project().as_str(),
            &envelope.invocation_id.to_string(),
            capture.required(),
        ) {
            Ok(outcome) => Ok(outcome),
            Err(error) if !capture.required() => {
                capture.mark_failure(&crate::telemetry::tool_learning::ToolLearningError::Store(
                    error,
                ));
                store.append(provider_only)
            }
            Err(error) => Err(error),
        }
    } else {
        store.append(command)
    }?;
    let (bytes, replayed) = match outcome {
        AppendOutcome::Committed(response) => (response.response, false),
        AppendOutcome::Replayed(response) => (response.response, true),
    };
    let persisted: CanonicalInvocationResult =
        serde_json::from_slice(&bytes).map_err(|_| InvokeError::InvalidPersistedOutcome)?;
    if let Some(capture) = envelope.learning {
        if !replayed {
            match store.has_learning_marker(&envelope.invocation_id.to_string()) {
                Ok(true) => {}
                Ok(false) => capture.mark_failure(
                    &crate::telemetry::tool_learning::ToolLearningError::BoundExceeded,
                ),
                Err(error) => capture.mark_failure(
                    &crate::telemetry::tool_learning::ToolLearningError::Store(error),
                ),
            }
        }
        match store.reconcile_learning_markers(capture.hasher().project().as_str(), 256) {
            Ok(_) => match store.has_learning_marker(&envelope.invocation_id.to_string()) {
                Ok(false) => {}
                Ok(true) => capture.mark_failure(
                    &crate::telemetry::tool_learning::ToolLearningError::BoundExceeded,
                ),
                Err(error) => capture.mark_failure(
                    &crate::telemetry::tool_learning::ToolLearningError::Store(error),
                ),
            },
            Err(error) => capture.mark_failure(
                &crate::telemetry::tool_learning::ToolLearningError::Store(error),
            ),
        }
    }
    Ok(persisted)
}

fn reconcile_persisted_learning(
    envelope: &InvocationEnvelope<'_>,
    store: &mut SqliteStore,
    request_digest: CanonicalRequestDigest,
    result: &CanonicalInvocationResult,
) -> Result<(), InvokeError> {
    let Some(capture) = envelope.learning else {
        return Ok(());
    };
    let dispatched = match store.events() {
        Ok(events) => events.iter().any(|stored| {
            stored.event.stream == EntityId::ToolCall(envelope.invocation_id)
                && stored.event.event_type.as_str() == DISPATCH_EVENT
        }),
        Err(error) => {
            capture.mark_failure(&crate::telemetry::tool_learning::ToolLearningError::Store(
                error,
            ));
            return Ok(());
        }
    };
    let (status, known) = learning_status(result);
    let failure = learning_failure(result, envelope.retry_safety, dispatched);
    let prepared = crate::telemetry::tool_learning::prepare_invocation_terminal(
        store,
        envelope
            .driver_claim
            .ok_or(InvokeError::MissingDriverClaim)?,
        capture,
        envelope.occurred_at.clone(),
        envelope.trace_id.clone(),
        request_digest.as_bytes(),
        failure,
        status,
        dispatched,
        known,
        dispatched.then_some(envelope.reservation.cost_microusd()),
        Some(envelope.outcome_event_id),
        false,
    );
    persist_learning(envelope, store, capture, "outcome", prepared, false)
}

fn persist_learning(
    envelope: &InvocationEnvelope<'_>,
    store: &mut SqliteStore,
    capture: &PreparedLearningCapture,
    phase: &str,
    prepared: Result<
        Option<(ExpectedStreamVersion, Vec<NewEvent>)>,
        crate::telemetry::tool_learning::ToolLearningError,
    >,
    pre_effect: bool,
) -> Result<(), InvokeError> {
    let result = prepared.and_then(|prepared| {
        let Some((version, events)) = prepared else {
            return Ok(());
        };
        let mut bytes = Vec::new();
        for event in &events {
            bytes.extend_from_slice(&event.payload);
        }
        let key = IdempotencyKey::parse(&format!("learning-{phase}-{}", envelope.invocation_id))
            .map_err(|_| crate::telemetry::tool_learning::ToolLearningError::InvalidRecord)?;
        store
            .append(AppendCommand {
                idempotency_scope: IdempotencyScope::new(
                    envelope.authenticated.principal_id(),
                    "capability.invoke.learning",
                    EntityId::ToolCall(envelope.invocation_id),
                )
                .map_err(|_| crate::telemetry::tool_learning::ToolLearningError::InvalidRecord)?,
                idempotency_key: key,
                request_digest: CanonicalRequestDigest::new(crate::domain::crypto::sha256(&bytes)),
                claim: None,
                driver_claim: envelope.driver_claim,
                allow_quiescent_driver_claim: false,
                expected_versions: vec![version],
                events,
                response: format!("learning-{phase}-v1").into_bytes(),
            })
            .map(|_| ())
            .map_err(crate::telemetry::tool_learning::ToolLearningError::Store)
    });
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            capture.mark_failure(&error);
            if pre_effect && capture.required() {
                Err(learning_store_error(error))
            } else {
                Ok(())
            }
        }
    }
}

fn scope(
    envelope: &InvocationEnvelope<'_>,
    command: &str,
) -> Result<IdempotencyScope, InvokeError> {
    IdempotencyScope::new(
        envelope.authenticated.principal_id(),
        command,
        EntityId::ToolCall(envelope.invocation_id),
    )
    .map_err(|_| InvokeError::InvalidPersistedOutcome)
}

fn event(
    envelope: &InvocationEnvelope<'_>,
    id: EventId,
    event_type: &str,
    payload: Vec<u8>,
    artifacts: Vec<u8>,
) -> NewEvent {
    NewEvent {
        id,
        stream: EntityId::ToolCall(envelope.invocation_id),
        event_type: EventType::parse(event_type).expect("invocation event type is valid"),
        schema_version: SchemaVersion::CURRENT,
        occurred_at: envelope.occurred_at.clone(),
        causation_id: envelope.command_id,
        correlation_id: EntityId::Run(envelope.config.run_id()),
        attempt_id: Some(envelope.attempt.attempt_id),
        trace_id: envelope.trace_id.clone(),
        payload,
        artifacts,
    }
}

fn output_artifacts(result: &CanonicalInvocationResult) -> Result<Vec<u8>, InvokeError> {
    let artifacts = result
        .output
        .as_ref()
        .map(|output| output.artifact_digests.as_slice())
        .unwrap_or_default();
    if artifacts.len() > MAX_INVOCATION_ARTIFACT_DIGESTS {
        return Err(InvokeError::ArtifactDigestLimit);
    }
    serde_json::to_vec(&artifacts).map_err(InvokeError::Serialization)
}

fn learning_store_error(error: crate::telemetry::tool_learning::ToolLearningError) -> InvokeError {
    match error {
        crate::telemetry::tool_learning::ToolLearningError::Store(error) => {
            InvokeError::Store(error)
        }
        _ => InvokeError::InvalidPersistedOutcome,
    }
}

fn learning_status(result: &CanonicalInvocationResult) -> (LearningStatus, bool) {
    let ambiguous = result.code.as_deref().is_some_and(learning_code_ambiguous);
    match result.status {
        InvocationStatus::Succeeded => (LearningStatus::Succeeded, true),
        InvocationStatus::Cancelled => (LearningStatus::Cancelled, true),
        InvocationStatus::ApprovalRequired => (LearningStatus::Interrupted, true),
        InvocationStatus::OutcomeUnknown => (LearningStatus::OutcomeUnknown, false),
        InvocationStatus::Failed if ambiguous => (LearningStatus::OutcomeUnknown, false),
        InvocationStatus::Failed | InvocationStatus::ApprovalDenied => {
            (LearningStatus::Failed, true)
        }
    }
}

fn learning_failure(
    result: &CanonicalInvocationResult,
    retry_safety: RetrySafety,
    dispatched: bool,
) -> Option<LearningFailure> {
    if result.status == InvocationStatus::Succeeded {
        return None;
    }
    let code = result.code.as_deref().unwrap_or_default();
    let ambiguous =
        learning_code_ambiguous(code) || (dispatched && retry_safety == RetrySafety::NonIdempotent);
    let (stage, class, mapped, retry) = if code.contains("egress_invalid") {
        (
            LearningErrorStage::Transport,
            LearningErrorClass::Input,
            LearningErrorCode::InvalidEndpoint,
            LearningRetryClass::Never,
        )
    } else if code.contains("egress_denied") {
        (
            LearningErrorStage::Authorization,
            LearningErrorClass::Policy,
            LearningErrorCode::EgressDenied,
            LearningRetryClass::Never,
        )
    } else if code.contains("operation_queue_full") {
        (
            LearningErrorStage::Dispatch,
            LearningErrorClass::System,
            LearningErrorCode::QueueFull,
            LearningRetryClass::Safe,
        )
    } else if code.contains("sensitive_payload") {
        (
            LearningErrorStage::ResultValidation,
            LearningErrorClass::Result,
            LearningErrorCode::SensitiveResponse,
            LearningRetryClass::Never,
        )
    } else if code.contains("invalid_response") || code.contains("missing_payload") {
        (
            LearningErrorStage::ResultValidation,
            LearningErrorClass::Result,
            if code.contains("missing_payload") {
                LearningErrorCode::MissingPayload
            } else {
                LearningErrorCode::InvalidResponse
            },
            LearningRetryClass::Never,
        )
    } else if code.contains("response_too_large") {
        (
            LearningErrorStage::ResultValidation,
            LearningErrorClass::Result,
            LearningErrorCode::ResponseTooLarge,
            LearningRetryClass::Never,
        )
    } else if code.contains("invalid_limits") {
        (
            LearningErrorStage::Parsing,
            LearningErrorClass::Input,
            LearningErrorCode::InvalidLimits,
            LearningRetryClass::Never,
        )
    } else if code.contains("invalid_endpoint") {
        (
            LearningErrorStage::Routing,
            LearningErrorClass::Input,
            LearningErrorCode::InvalidEndpoint,
            LearningRetryClass::Never,
        )
    } else if code.contains("invalid_header") {
        (
            LearningErrorStage::Transport,
            LearningErrorClass::Input,
            LearningErrorCode::InvalidHeader,
            LearningRetryClass::Never,
        )
    } else if code.contains("protocol_version_refused") {
        (
            LearningErrorStage::Transport,
            LearningErrorClass::Transport,
            LearningErrorCode::ProtocolVersionRefused,
            LearningRetryClass::Never,
        )
    } else if code.contains("protocol_failed") {
        (
            LearningErrorStage::Transport,
            LearningErrorClass::Transport,
            LearningErrorCode::Protocol,
            LearningRetryClass::Never,
        )
    } else if code.contains("process_unavailable") {
        (
            LearningErrorStage::Dispatch,
            LearningErrorClass::System,
            LearningErrorCode::ProcessUnavailable,
            LearningRetryClass::Safe,
        )
    } else if code.contains("refresh_retries_exhausted") {
        (
            LearningErrorStage::Transport,
            LearningErrorClass::Transport,
            LearningErrorCode::RefreshRetriesExhausted,
            LearningRetryClass::Never,
        )
    } else if code.contains("refresh_closed") {
        (
            LearningErrorStage::Transport,
            LearningErrorClass::Transport,
            LearningErrorCode::RefreshClosed,
            LearningRetryClass::Unknown,
        )
    } else if code.contains("session_expired") {
        (
            LearningErrorStage::Transport,
            LearningErrorClass::Transport,
            LearningErrorCode::SessionExpired,
            LearningRetryClass::Never,
        )
    } else if code.contains("feature_failed") {
        (
            LearningErrorStage::ResultValidation,
            LearningErrorClass::Remote,
            LearningErrorCode::FeatureFailed,
            LearningRetryClass::Never,
        )
    } else if code.contains("discovery_failed") {
        (
            LearningErrorStage::Routing,
            LearningErrorClass::Remote,
            LearningErrorCode::DiscoveryFailed,
            LearningRetryClass::Safe,
        )
    } else if code.contains("stale_binding") {
        (
            LearningErrorStage::Routing,
            LearningErrorClass::Policy,
            LearningErrorCode::StaleBinding,
            LearningRetryClass::Never,
        )
    } else if code.contains("binding_expired") {
        (
            LearningErrorStage::Authorization,
            LearningErrorClass::Policy,
            LearningErrorCode::BindingExpired,
            LearningRetryClass::Never,
        )
    } else if code.contains("authorization_failed") {
        (
            LearningErrorStage::Authorization,
            LearningErrorClass::Policy,
            LearningErrorCode::AuthorizationDenied,
            LearningRetryClass::Never,
        )
    } else if code.contains("credential") {
        (
            LearningErrorStage::Authorization,
            LearningErrorClass::Auth,
            LearningErrorCode::CredentialUnavailable,
            LearningRetryClass::Never,
        )
    } else if code.contains("auth_required")
        || code.contains("auth_interrupted")
        || code.contains("approval_required")
    {
        (
            LearningErrorStage::Authorization,
            LearningErrorClass::Auth,
            LearningErrorCode::AuthRequired,
            LearningRetryClass::AuthorizationResume,
        )
    } else if code.contains("auth_denied") || result.status == InvocationStatus::ApprovalDenied {
        (
            LearningErrorStage::Authorization,
            LearningErrorClass::Auth,
            LearningErrorCode::AuthDenied,
            LearningRetryClass::Never,
        )
    } else if code.contains("url_elicitation_required") {
        (
            LearningErrorStage::Transport,
            LearningErrorClass::Url,
            LearningErrorCode::UrlElicitationRequired,
            LearningRetryClass::UrlResume,
        )
    } else if code.contains("url_elicitation_declined") {
        (
            LearningErrorStage::Transport,
            LearningErrorClass::Url,
            LearningErrorCode::UrlElicitationDeclined,
            LearningRetryClass::Never,
        )
    } else if code.contains("timeout") {
        (
            LearningErrorStage::Transport,
            LearningErrorClass::Transport,
            LearningErrorCode::Timeout,
            LearningRetryClass::Unknown,
        )
    } else if code.contains("connection_retired") {
        (
            LearningErrorStage::Transport,
            LearningErrorClass::Transport,
            LearningErrorCode::ConnectionRetired,
            LearningRetryClass::Unknown,
        )
    } else if code.contains("transport_io") || code.contains("cleanup") {
        (
            LearningErrorStage::Transport,
            LearningErrorClass::Transport,
            LearningErrorCode::Io,
            LearningRetryClass::Unknown,
        )
    } else if ambiguous || result.status == InvocationStatus::OutcomeUnknown {
        (
            LearningErrorStage::Dispatch,
            LearningErrorClass::Transport,
            LearningErrorCode::OutcomeUnknown,
            LearningRetryClass::Unknown,
        )
    } else if result.status == InvocationStatus::Cancelled {
        (
            LearningErrorStage::Dispatch,
            LearningErrorClass::System,
            LearningErrorCode::Cancelled,
            LearningRetryClass::Never,
        )
    } else {
        (
            LearningErrorStage::Dispatch,
            LearningErrorClass::Remote,
            LearningErrorCode::Internal,
            LearningRetryClass::Never,
        )
    };
    Some(LearningFailure {
        stage,
        class,
        code: mapped,
        field: None,
        retry,
        dispatched,
        known: !ambiguous && result.status != InvocationStatus::OutcomeUnknown,
    })
}

fn learning_code_ambiguous(code: &str) -> bool {
    [
        "timeout",
        "retired",
        "io",
        "cleanup",
        "outcome_unknown",
        "session_expired",
        "refresh_closed",
        "retries_exhausted",
        "unavailable_after_dispatch",
        "interrupted_after_dispatch",
    ]
    .iter()
    .any(|needle| code.contains(needle))
}

fn put_spend(bytes: &mut Vec<u8>, spend: Spend) {
    for value in [
        spend.cost_microusd(),
        spend.tokens(),
        spend.turns(),
        spend.tools(),
        spend.processes(),
    ] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
}

fn spend_value(spend: Spend) -> serde_json::Value {
    serde_json::json!({
        "cost_microusd": spend.cost_microusd(),
        "tokens": spend.tokens(),
        "turns": spend.turns(),
        "tools": spend.tools(),
        "processes": spend.processes(),
    })
}

fn effect_name(effect: EffectClass) -> &'static str {
    match effect {
        EffectClass::ModelCall => "model_call",
        EffectClass::WorkspaceRead => "workspace_read",
        EffectClass::WorkspaceWrite => "workspace_write",
        EffectClass::ProcessSpawn => "process_spawn",
        EffectClass::NetworkEgress => "network_egress",
    }
}

fn hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_digest_limit_is_checked_before_event_serialization() {
        let artifact = ArtifactRef::parse(&format!("blake3:{}", "ab".repeat(32))).unwrap();
        let result = CanonicalInvocationResult {
            status: InvocationStatus::Succeeded,
            output: Some(CanonicalOutput {
                media_type: "application/json".to_owned(),
                body: b"{}".to_vec(),
                artifact_digests: vec![artifact; MAX_INVOCATION_ARTIFACT_DIGESTS + 1],
            }),
            code: None,
            charged: true,
        };
        assert!(matches!(
            output_artifacts(&result),
            Err(InvokeError::ArtifactDigestLimit)
        ));
    }

    #[test]
    fn every_mcp_completion_class_has_a_fixed_learning_mapping() {
        use crate::telemetry::tool_learning::{ErrorCode, ErrorStage};

        #[rustfmt::skip]
        let cases = [
            ("mcp.invalid_limits", ErrorStage::Parsing, ErrorCode::InvalidLimits),
            ("mcp.invalid_endpoint", ErrorStage::Routing, ErrorCode::InvalidEndpoint),
            ("mcp.invalid_header", ErrorStage::Transport, ErrorCode::InvalidHeader),
            ("mcp.response_too_large", ErrorStage::ResultValidation, ErrorCode::ResponseTooLarge),
            ("mcp.protocol_version_refused", ErrorStage::Transport, ErrorCode::ProtocolVersionRefused),
            ("mcp.missing_payload", ErrorStage::ResultValidation, ErrorCode::MissingPayload),
            ("mcp.process_unavailable", ErrorStage::Dispatch, ErrorCode::ProcessUnavailable),
            ("mcp.transport_timeout", ErrorStage::Transport, ErrorCode::Timeout),
            ("mcp.transport_auth_interrupted", ErrorStage::Authorization, ErrorCode::AuthRequired),
            ("mcp.sensitive_payload", ErrorStage::ResultValidation, ErrorCode::SensitiveResponse),
            ("mcp.session_expired", ErrorStage::Transport, ErrorCode::SessionExpired),
            ("mcp.url_elicitation_required", ErrorStage::Transport, ErrorCode::UrlElicitationRequired),
            ("mcp.url_elicitation_outcome_unknown", ErrorStage::Dispatch, ErrorCode::OutcomeUnknown),
            ("mcp.url_elicitation_declined", ErrorStage::Transport, ErrorCode::UrlElicitationDeclined),
            ("mcp.credential_failed", ErrorStage::Authorization, ErrorCode::CredentialUnavailable),
            ("mcp.egress_denied", ErrorStage::Authorization, ErrorCode::EgressDenied),
            ("mcp.egress_invalid", ErrorStage::Transport, ErrorCode::InvalidEndpoint),
            ("mcp.invalid_response", ErrorStage::ResultValidation, ErrorCode::InvalidResponse),
            ("mcp.authorization_failed", ErrorStage::Authorization, ErrorCode::AuthorizationDenied),
            ("mcp.connection_retired", ErrorStage::Transport, ErrorCode::ConnectionRetired),
            ("mcp.binding_expired", ErrorStage::Authorization, ErrorCode::BindingExpired),
            ("mcp.stale_binding", ErrorStage::Routing, ErrorCode::StaleBinding),
            ("mcp.operation_queue_full", ErrorStage::Dispatch, ErrorCode::QueueFull),
            ("mcp.cancelled", ErrorStage::Dispatch, ErrorCode::Cancelled),
            ("mcp.refresh_closed", ErrorStage::Transport, ErrorCode::RefreshClosed),
            ("mcp.refresh_retries_exhausted", ErrorStage::Transport, ErrorCode::RefreshRetriesExhausted),
            ("mcp.transport_io", ErrorStage::Transport, ErrorCode::Io),
            ("mcp.feature_failed", ErrorStage::ResultValidation, ErrorCode::FeatureFailed),
            ("mcp.discovery_failed", ErrorStage::Routing, ErrorCode::DiscoveryFailed),
            ("mcp.protocol_failed", ErrorStage::Transport, ErrorCode::Protocol),
        ];
        for (completion, stage, code) in cases {
            let result = CanonicalInvocationResult {
                status: if completion == "mcp.cancelled" {
                    InvocationStatus::Cancelled
                } else if completion == "mcp.url_elicitation_outcome_unknown" {
                    InvocationStatus::OutcomeUnknown
                } else {
                    InvocationStatus::Failed
                },
                output: None,
                code: Some(completion.to_owned()),
                charged: false,
            };
            let mapped = learning_failure(&result, RetrySafety::Idempotent, true).unwrap();
            assert_eq!((mapped.stage, mapped.code), (stage, code), "{completion}");
        }
    }

    #[test]
    fn external_errors_preserve_committed_dispatch_evidence() {
        let before = external_failure("mcp.authorization_failed", RetrySafety::Idempotent, false);
        assert!(!before.dispatched);
        assert!(before.known);

        let after = external_failure("mcp.transport_io", RetrySafety::Idempotent, true);
        assert!(after.dispatched);
        assert!(!after.known);
        assert_eq!(after.code, LearningErrorCode::Io);
    }
}
