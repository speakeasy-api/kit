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
    prepare_inner(envelope, runtime, false)
}

pub(crate) fn prepare_resuming_dispatch(
    envelope: &InvocationEnvelope<'_>,
    runtime: &mut InvocationPhaseRuntime<'_>,
) -> Result<PrepareOutcome, InvokeError> {
    prepare_inner(envelope, runtime, true)
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
    let reservation_id = reservation_id(request_digest);
    runtime
        .budget
        .reserve(reservation_id, envelope.reservation)?;
    settle(runtime.budget, reservation_id, result, true).map(Some)
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
    let outcome = store.append(AppendCommand {
        idempotency_scope: scope(envelope, INTENT_COMMAND)?,
        idempotency_key: envelope.idempotency_key.clone(),
        request_digest,
        claim: None,
        driver_claim: envelope.driver_claim,
        allow_quiescent_driver_claim: false,
        expected_versions: vec![ExpectedStreamVersion {
            stream: EntityId::ToolCall(envelope.invocation_id),
            version: ExpectedVersion::new(0),
        }],
        events: vec![event(
            envelope,
            envelope.intent_event_id,
            INTENT_EVENT,
            payload,
            b"[]".to_vec(),
        )],
        response: b"intent-v1".to_vec(),
    })?;
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
    let outcome = store.append(AppendCommand {
        idempotency_scope: scope(envelope, OUTCOME_COMMAND)?,
        idempotency_key: envelope.idempotency_key.clone(),
        request_digest,
        claim: None,
        driver_claim: envelope.driver_claim,
        allow_quiescent_driver_claim: false,
        expected_versions: vec![ExpectedStreamVersion {
            stream: EntityId::ToolCall(envelope.invocation_id),
            version: ExpectedVersion::new(if dispatched { 2 } else { 1 }),
        }],
        events: vec![event(
            envelope,
            envelope.outcome_event_id,
            OUTCOME_EVENT,
            payload,
            artifacts,
        )],
        response,
    })?;
    let bytes = match outcome {
        AppendOutcome::Committed(response) | AppendOutcome::Replayed(response) => response.response,
    };
    serde_json::from_slice(&bytes).map_err(|_| InvokeError::InvalidPersistedOutcome)
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
}
