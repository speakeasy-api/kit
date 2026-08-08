use std::{
    collections::{BTreeSet, VecDeque},
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use agentkit_core::{Delta, Part, PartId, PartKind, TurnCancellation, Usage};
use agentkit_loop::{
    LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent, ModelTurnResult,
    SessionConfig, TurnRequest,
};
use serde::{Deserialize, Serialize};

use crate::{
    agent::{
        agentkit_bridge::mapping::from_agentkit_item,
        driver::restart::{
            BoundarySnapshot, CommittedModelOutcome, EFFECT_CORRELATION_METADATA,
            EffectCorrelation, EffectDispatched, EffectIntent, EffectIntentPayload, EffectJournal,
            EffectJournalAppend, EffectKind, EffectOutcome, EffectStatus, LoopRecord, SafeBoundary,
        },
    },
    api::{auth::contract::AuthenticatedPrincipal, service::AttemptDriverClaim},
    capabilities::kernel::{
        grant::{
            self, ArgumentConstraints, CapabilityGrantSnapshot, DelegationSnapshot, EffectClass,
            GrantRequest,
        },
        identity::{CapabilityIdentity, Digest},
    },
    domain::{
        commands::ExpectedVersion,
        config::RunConfigSnapshot,
        crypto::sha256,
        events::{EntityId, EventType, SchemaVersion, TraceId, UtcDateTime},
        ids::{CommandId, EventId, ModelCallId, WorkspaceId},
        lifecycle::AttemptOwnership,
    },
    runtime::scheduler::{
        AdmissionKind, DurableScheduler, ReservationRequest,
        budget::RunBudget,
        limits::Spend,
        reserve::{ReservationId, ReservationStatus},
    },
    store::sqlite::{
        append::{AppendCommand, AppendOutcome, ExpectedStreamVersion, NewEvent, SqliteStore},
        idempotency::{
            CanonicalRequestDigest, IdempotencyKey, IdempotencyScope, IdempotencyStatus,
        },
    },
};

const INTENT_COMMAND: &str = "model_call.intent";
const DISPATCH_COMMAND: &str = "model_call.dispatch";
const OUTCOME_COMMAND: &str = "model_call.outcome";
const INTENT_EVENT: &str = "model_call.intent";
const DISPATCH_EVENT: &str = "model_call.dispatched";
const OUTCOME_EVENT: &str = "model_call.outcome";
const PROVIDER_IDEMPOTENCY_KEY: &str = "kit.model_call.idempotency_key";
const PROVIDER_REQUEST_DIGEST: &str = "kit.model_call.request_digest";

pub type ModelOutcomeValidator =
    Arc<dyn Fn(&ModelTurnResult) -> Result<(), LoopError> + Send + Sync + 'static>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelCrashPoint {
    BeforeIntent,
    BetweenIntentAndDispatch,
    AfterDispatch,
    BeforeOutcome,
    AfterOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderIdempotency {
    Unproven,
    Enforced,
}

#[derive(Clone)]
pub struct ModelSecurity {
    pub authenticated: AuthenticatedPrincipal,
    pub config: RunConfigSnapshot,
    pub grants: CapabilityGrantSnapshot,
    pub delegation: Option<DelegationSnapshot>,
    pub capability: CapabilityIdentity,
    pub schema_digest: Digest,
    pub argument_constraints: ArgumentConstraints,
    pub workspace_id: WorkspaceId,
    pub attempt: AttemptOwnership,
    pub claim: AttemptDriverClaim,
}

#[derive(Clone, Copy, Debug)]
pub struct ModelPolicy {
    pub reservation: Spend,
    pub provider_idempotency: ProviderIdempotency,
    pub max_buffered_bytes: usize,
    pub max_delta_bytes: usize,
    pub detached: bool,
}

impl Default for ModelPolicy {
    fn default() -> Self {
        Self {
            reservation: Spend::new(0, 1, 1, 0, 0),
            provider_idempotency: ProviderIdempotency::Unproven,
            max_buffered_bytes: 8 * 1024 * 1024,
            max_delta_bytes: 16 * 1024,
            detached: false,
        }
    }
}

struct ModelKernel {
    store: Mutex<SqliteStore>,
    scheduler: DurableScheduler,
    security: ModelSecurity,
    policy: ModelPolicy,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
    crash_at: Option<ModelCrashPoint>,
    outcome_validator: Option<ModelOutcomeValidator>,
}

pub struct DurableModelAdapter<M> {
    inner: M,
    kernel: Arc<ModelKernel>,
}

impl<M> DurableModelAdapter<M> {
    pub fn new(
        inner: M,
        store: SqliteStore,
        scheduler: DurableScheduler,
        security: ModelSecurity,
        policy: ModelPolicy,
        occurred_at: UtcDateTime,
        trace_id: TraceId,
    ) -> Self {
        Self {
            inner,
            kernel: Arc::new(ModelKernel {
                store: Mutex::new(store),
                scheduler,
                security,
                policy,
                occurred_at,
                trace_id,
                crash_at: None,
                outcome_validator: None,
            }),
        }
    }

    pub fn with_crash_at(mut self, point: ModelCrashPoint) -> Self {
        Arc::get_mut(&mut self.kernel)
            .expect("crash point must be configured before the adapter is shared")
            .crash_at = Some(point);
        self
    }

    pub fn with_outcome_validator(mut self, validator: ModelOutcomeValidator) -> Self {
        Arc::get_mut(&mut self.kernel)
            .expect("outcome validator must be configured before the adapter is shared")
            .outcome_validator = Some(validator);
        self
    }
}

impl<M> ModelAdapter for DurableModelAdapter<M>
where
    M: ModelAdapter,
{
    type Session = DurableModelSession<M::Session>;

    fn start_session<'life0, 'async_trait>(
        &'life0 self,
        config: SessionConfig,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Session, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.kernel.authorize_dispatch()?;
            let provider = self.inner.provider_name().map(str::to_owned);
            let config_bytes = serde_json::to_vec(&config).map_err(model_error)?;
            let session = self.inner.start_session(config).await?;
            let model = session.model_name().map(str::to_owned);
            let model_snapshot_digest =
                model_snapshot_digest(provider.as_deref(), model.as_deref(), &config_bytes);
            Ok(DurableModelSession {
                inner: session,
                kernel: Arc::clone(&self.kernel),
                provider,
                model,
                model_snapshot_digest,
            })
        })
    }

    fn provider_name(&self) -> Option<&str> {
        self.inner.provider_name()
    }
}

pub struct DurableModelSession<S> {
    inner: S,
    kernel: Arc<ModelKernel>,
    provider: Option<String>,
    model: Option<String>,
    model_snapshot_digest: [u8; 32],
}

impl<S> ModelSession for DurableModelSession<S>
where
    S: ModelSession,
{
    type Turn = DurableModelTurn<S::Turn>;

    fn begin_turn<'life0, 'async_trait>(
        &'life0 mut self,
        mut request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Turn, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            self.kernel.authorize_dispatch()?;
            self.kernel.validate_policy()?;
            self.inner.prepare_turn(&mut request)?;

            let prompt = serde_json::to_vec(&request).map_err(model_error)?;
            let prompt_digest = sha256(&prompt);
            let operation_digest = operation_digest(
                &self.kernel.security,
                &request,
                prompt_digest,
                self.model_snapshot_digest,
            );
            let request_digest = CanonicalRequestDigest::new(operation_digest);
            let identity_digest = operation_identity_digest(
                &self.kernel.security,
                &request,
                self.model_snapshot_digest,
            );
            let key = IdempotencyKey::parse(&format!("model-{}", hex(&identity_digest)))
                .map_err(model_error)?;
            let original_reservation = reservation_id(identity_digest, 0);
            let reservation = self.kernel.request_reservation(&request)?;

            let existing_intent = self.kernel.intent(&key, request_digest)?;
            let (intent, intent_replayed) = match existing_intent {
                Some(intent) => (intent, true),
                None => {
                    self.kernel
                        .reserve(original_reservation, &key, false, reservation)?;
                    if self.kernel.crash_at == Some(ModelCrashPoint::BeforeIntent) {
                        self.kernel.release(original_reservation)?;
                        return Err(injected(ModelCrashPoint::BeforeIntent));
                    }
                    (
                        self.kernel.append_intent(
                            &key,
                            request_digest,
                            original_reservation,
                            prompt_digest,
                            self.model_snapshot_digest,
                            self.provider.as_deref(),
                            self.model.as_deref(),
                            reservation,
                            &request,
                        )?,
                        false,
                    )
                }
            };
            let prompt_request = request.clone();

            if let Some(outcome) = self.kernel.outcome(&key, request_digest)? {
                self.kernel.append_journal_outcome(
                    &key,
                    &intent,
                    &outcome,
                    self.kernel.boundary(&request, outcome.result.as_ref()),
                )?;
                self.kernel.settle_outcome(&outcome)?;
                return outcome.into_turn(Arc::clone(&self.kernel));
            }

            if self.kernel.crash_at == Some(ModelCrashPoint::BetweenIntentAndDispatch) {
                return Err(injected(ModelCrashPoint::BetweenIntentAndDispatch));
            }

            if cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled)
            {
                let outcome = PersistedOutcome::cancelled(intent.reservation_id);
                self.kernel
                    .append_outcome(&key, request_digest, &intent, false, &outcome, None)?;
                self.kernel.release(intent.reservation_id)?;
                return Err(LoopError::Cancelled);
            }

            let existing_dispatch = self.kernel.dispatch(&key, request_digest)?;
            if self.kernel.policy.provider_idempotency == ProviderIdempotency::Unproven
                && let Some(dispatch) = &existing_dispatch
            {
                self.kernel.commit_unknown(
                    &key,
                    request_digest,
                    &intent,
                    dispatch.reservation_id,
                    "recovery_requires_reconciliation",
                )?;
                return Err(outcome_unknown());
            }

            let active_reservation = match existing_dispatch.as_ref() {
                Some(dispatch) => dispatch.reservation_id,
                None => self.kernel.active_reservation(
                    intent.reservation_id,
                    identity_digest,
                    &key,
                    intent_replayed,
                    reservation,
                )?,
            };

            let dispatch = match existing_dispatch {
                Some(dispatch) => dispatch,
                None => {
                    self.kernel
                        .scheduler
                        .mark_dispatched(active_reservation)
                        .map_err(model_error)?;
                    let (dispatch, replayed) = self.kernel.append_dispatch(
                        &key,
                        request_digest,
                        &intent,
                        active_reservation,
                    )?;
                    if replayed
                        && self.kernel.policy.provider_idempotency == ProviderIdempotency::Unproven
                    {
                        self.kernel.commit_unknown(
                            &key,
                            request_digest,
                            &intent,
                            dispatch.reservation_id,
                            "concurrent_dispatch",
                        )?;
                        return Err(outcome_unknown());
                    }
                    dispatch
                }
            };

            request.metadata.insert(
                PROVIDER_IDEMPOTENCY_KEY.to_owned(),
                serde_json::Value::String(key.as_str().to_owned()),
            );
            request.metadata.insert(
                PROVIDER_REQUEST_DIGEST.to_owned(),
                serde_json::Value::String(hex(request_digest.as_bytes())),
            );
            request.metadata.insert(
                EFFECT_CORRELATION_METADATA.to_owned(),
                serde_json::to_value(&intent.correlation).map_err(model_error)?,
            );
            self.kernel.verify_claim()?;
            let provider = self.inner.begin_turn(request, cancellation.clone());
            let turn = match if let Some(cancellation) = cancellation.as_ref() {
                tokio::select! {
                    result = provider => result,
                    _ = cancellation.cancelled() => {
                        self.kernel.commit_unknown(
                            &key,
                            request_digest,
                            &intent,
                            dispatch.reservation_id,
                            "cancelled_during_provider_begin",
                        )?;
                        return Err(outcome_unknown());
                    }
                }
            } else {
                provider.await
            } {
                Ok(turn) => turn,
                Err(
                    error @ (LoopError::ProviderNotDispatched { .. } | LoopError::SessionStale(_)),
                ) => {
                    self.kernel.commit_not_dispatched(
                        &key,
                        request_digest,
                        &intent,
                        dispatch.reservation_id,
                    )?;
                    return Err(error);
                }
                Err(error) => {
                    self.kernel.commit_unknown(
                        &key,
                        request_digest,
                        &intent,
                        dispatch.reservation_id,
                        "provider_begin_error",
                    )?;
                    return Err(error);
                }
            };

            if self.kernel.crash_at == Some(ModelCrashPoint::AfterDispatch) {
                self.kernel.commit_unknown(
                    &key,
                    request_digest,
                    &intent,
                    dispatch.reservation_id,
                    "interrupted_after_dispatch",
                )?;
                return Err(injected(ModelCrashPoint::AfterDispatch));
            }

            Ok(DurableModelTurn {
                inner: Some(turn),
                buffered: VecDeque::new(),
                kernel: Arc::clone(&self.kernel),
                key,
                request_digest,
                intent,
                dispatch,
                request: prompt_request,
                cancellation,
                terminal: false,
            })
        })
    }

    fn model_name(&self) -> Option<&str> {
        self.model.as_deref()
    }

    fn prepare_turn(&mut self, request: &mut TurnRequest) -> Result<(), LoopError> {
        self.inner.prepare_turn(request)
    }

    fn structured_output_capability(&self) -> Option<&agentkit_loop::StructuredOutputCapability> {
        self.inner.structured_output_capability()
    }
}

pub struct DurableModelTurn<T> {
    inner: Option<T>,
    buffered: VecDeque<ModelTurnEvent>,
    kernel: Arc<ModelKernel>,
    key: IdempotencyKey,
    request_digest: CanonicalRequestDigest,
    intent: IntentReceipt,
    dispatch: DispatchReceipt,
    request: TurnRequest,
    cancellation: Option<TurnCancellation>,
    terminal: bool,
}

impl<T> Drop for DurableModelTurn<T> {
    fn drop(&mut self) {
        if !self.terminal {
            let _ = self.kernel.commit_unknown(
                &self.key,
                self.request_digest,
                &self.intent,
                self.dispatch.reservation_id,
                "model_turn_dropped_after_dispatch",
            );
            self.terminal = true;
        }
    }
}

impl<T> ModelTurn for DurableModelTurn<T>
where
    T: ModelTurn,
{
    fn next_event<'life0, 'async_trait>(
        &'life0 mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<ModelTurnEvent>, LoopError>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if !self.terminal {
                let cancellation = cancellation.or_else(|| self.cancellation.clone());
                if let Err(error) = self.buffer_provider(cancellation).await {
                    if !self.terminal {
                        self.kernel.commit_unknown(
                            &self.key,
                            self.request_digest,
                            &self.intent,
                            self.dispatch.reservation_id,
                            "stream_buffer_error",
                        )?;
                        self.buffered.clear();
                        self.terminal = true;
                    }
                    return Err(error);
                }
            }
            Ok(self.buffered.pop_front())
        })
    }
}

impl<T> DurableModelTurn<T>
where
    T: ModelTurn,
{
    async fn buffer_provider(
        &mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Result<(), LoopError> {
        let mut bytes = 0usize;
        let mut hidden = BTreeSet::new();
        let mut latest_usage = None;
        let mut finished = None;
        let turn = self.inner.as_mut().expect("live model turn is present");

        loop {
            if cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled)
            {
                self.kernel.commit_unknown(
                    &self.key,
                    self.request_digest,
                    &self.intent,
                    self.dispatch.reservation_id,
                    "cancelled_after_dispatch",
                )?;
                self.terminal = true;
                return Err(outcome_unknown());
            }
            let next = turn.next_event(cancellation.clone());
            let event = match if let Some(cancellation) = cancellation.as_ref() {
                tokio::select! {
                    result = next => result,
                    _ = cancellation.cancelled() => {
                        self.kernel.commit_unknown(
                            &self.key,
                            self.request_digest,
                            &self.intent,
                            self.dispatch.reservation_id,
                            "cancelled_during_provider_stream",
                        )?;
                        self.terminal = true;
                        return Err(outcome_unknown());
                    }
                }
            } else {
                next.await
            } {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(error) => {
                    self.kernel.commit_unknown(
                        &self.key,
                        self.request_digest,
                        &self.intent,
                        self.dispatch.reservation_id,
                        "provider_stream_error",
                    )?;
                    self.terminal = true;
                    return Err(error);
                }
            };
            match event {
                ModelTurnEvent::Delta(delta) => {
                    for delta in
                        sanitize_delta(delta, &mut hidden, self.kernel.policy.max_delta_bytes)
                    {
                        push_bounded(
                            &mut self.buffered,
                            ModelTurnEvent::Delta(delta),
                            &mut bytes,
                            self.kernel.policy.max_buffered_bytes,
                        )?;
                    }
                }
                ModelTurnEvent::Usage(usage) => {
                    latest_usage = Some(usage.clone());
                    push_bounded(
                        &mut self.buffered,
                        ModelTurnEvent::Usage(usage),
                        &mut bytes,
                        self.kernel.policy.max_buffered_bytes,
                    )?;
                }
                ModelTurnEvent::ToolCall(call) => {
                    if self.kernel.outcome_validator.is_some() {
                        self.kernel.commit_unknown(
                            &self.key,
                            self.request_digest,
                            &self.intent,
                            self.dispatch.reservation_id,
                            "provider_tool_call_rejected",
                        )?;
                        self.buffered.clear();
                        self.terminal = true;
                        return Err(LoopError::Unsupported(
                            "detached model output rejected".to_owned(),
                        ));
                    }
                    push_bounded(
                        &mut self.buffered,
                        ModelTurnEvent::ToolCall(call),
                        &mut bytes,
                        self.kernel.policy.max_buffered_bytes,
                    )?;
                }
                ModelTurnEvent::Finished(mut result) => {
                    if result.usage.is_none() {
                        result.usage = latest_usage.clone();
                    }
                    if self
                        .kernel
                        .outcome_validator
                        .as_ref()
                        .is_some_and(|validator| validator(&result).is_err())
                    {
                        self.kernel.commit_rejected(
                            &self.key,
                            self.request_digest,
                            &self.intent,
                            self.dispatch.reservation_id,
                            result.usage.as_ref(),
                            "provider_output_rejected",
                        )?;
                        self.buffered.clear();
                        self.terminal = true;
                        return Err(LoopError::Unsupported(
                            "detached model output rejected".to_owned(),
                        ));
                    }
                    discard_reasoning(&mut result);
                    push_bounded(
                        &mut self.buffered,
                        ModelTurnEvent::Finished(result.clone()),
                        &mut bytes,
                        self.kernel.policy.max_buffered_bytes,
                    )?;
                    finished = Some(result);
                    break;
                }
            }
        }

        let Some(result) = finished else {
            self.kernel.commit_unknown(
                &self.key,
                self.request_digest,
                &self.intent,
                self.dispatch.reservation_id,
                "stream_ended_without_outcome",
            )?;
            self.buffered.clear();
            self.terminal = true;
            return Err(outcome_unknown());
        };

        if self.kernel.verify_claim().is_err() {
            self.kernel.commit_unknown(
                &self.key,
                self.request_digest,
                &self.intent,
                self.dispatch.reservation_id,
                "stale_fence_after_dispatch",
            )?;
            self.buffered.clear();
            self.terminal = true;
            return Err(outcome_unknown());
        }
        if self.kernel.crash_at == Some(ModelCrashPoint::BeforeOutcome) {
            self.kernel.commit_unknown(
                &self.key,
                self.request_digest,
                &self.intent,
                self.dispatch.reservation_id,
                "interrupted_before_outcome",
            )?;
            self.buffered.clear();
            self.terminal = true;
            return Err(injected(ModelCrashPoint::BeforeOutcome));
        }

        let reserved = self
            .kernel
            .scheduler
            .snapshot(self.dispatch.reservation_id)
            .map_err(model_error)?
            .spend();
        let outcome = PersistedOutcome::succeeded(self.dispatch.reservation_id, result, reserved);
        let boundary = self.kernel.boundary(&self.request, outcome.result.as_ref());
        let (committed, replayed) = self.kernel.append_outcome(
            &self.key,
            self.request_digest,
            &self.intent,
            true,
            &outcome,
            boundary,
        )?;
        if self.kernel.crash_at == Some(ModelCrashPoint::AfterOutcome) {
            self.buffered.clear();
            self.inner = None;
            self.terminal = true;
            return Err(injected(ModelCrashPoint::AfterOutcome));
        }
        self.kernel.settle_outcome(&committed)?;
        if committed.status != ModelOutcomeStatus::Succeeded {
            self.buffered.clear();
            self.terminal = true;
            return Err(outcome_unknown());
        }
        if replayed {
            self.buffered =
                VecDeque::from([ModelTurnEvent::Finished(committed.result.ok_or_else(
                    || LoopError::Provider("persisted model result is missing".to_owned()),
                )?)]);
        }
        self.inner = None;
        self.terminal = true;
        Ok(())
    }
}

impl ModelKernel {
    fn validate_policy(&self) -> Result<(), LoopError> {
        if self.policy.reservation.turns() == 0
            || self.policy.reservation.tools() != 0
            || self.policy.reservation.processes() != 0
            || self.policy.max_buffered_bytes == 0
            || self.policy.max_delta_bytes == 0
        {
            Err(LoopError::Provider(
                "invalid durable model policy".to_owned(),
            ))
        } else {
            Ok(())
        }
    }

    fn authorize(&self) -> Result<(), LoopError> {
        let security = &self.security;
        if security.authenticated.principal_id() != security.attempt.principal_id
            || security.config.principal_id() != security.attempt.principal_id
            || security.claim.owner() != security.attempt
        {
            return Err(LoopError::Provider(
                "model call attempt owner or fence is stale".to_owned(),
            ));
        }
        self.verify_claim()?;
        let decision = grant::decide(GrantRequest {
            authenticated: &security.authenticated,
            capability: &security.capability,
            schema_digest: security.schema_digest,
            effect: EffectClass::ModelCall,
            argument_constraints: &security.argument_constraints,
            workspace_id: security.workspace_id,
            project_id: security.config.project_id(),
            config: &security.config,
            grants: &security.grants,
            delegation: security.delegation.as_ref(),
            extension: crate::capabilities::kernel::grant_ext::RequestExtension::default(),
        });
        if decision.is_allowed() {
            Ok(())
        } else {
            Err(LoopError::Provider(format!(
                "model capability denied: {:?}",
                decision.reason()
            )))
        }
    }

    fn authorize_dispatch(&self) -> Result<(), LoopError> {
        self.authorize()
    }

    fn boundary(
        &self,
        request: &TurnRequest,
        result: Option<&ModelTurnResult>,
    ) -> Option<BoundarySnapshot> {
        (!self.policy.detached)
            .then(|| model_boundary(request, result))
            .flatten()
    }

    fn verify_claim(&self) -> Result<(), LoopError> {
        self.store
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .verify_driver_claim(self.security.claim)
            .map_err(model_error)
    }

    fn reserve(
        &self,
        id: ReservationId,
        key: &IdempotencyKey,
        retry: bool,
        spend: Spend,
    ) -> Result<ReservationStatus, LoopError> {
        let request = ReservationRequest {
            id,
            run_id: self.security.config.run_id(),
            principal_id: self.security.attempt.principal_id,
            attempt: Some(self.security.attempt),
            idempotency_key: if retry {
                format!("{}-retry", key.as_str())
            } else {
                key.as_str().to_owned()
            },
            kind: AdmissionKind::Model,
            spend,
        };
        self.scheduler
            .reserve(&request)
            .map(|snapshot| snapshot.status())
            .map_err(model_error)
    }

    fn active_reservation(
        &self,
        original: ReservationId,
        operation_digest: [u8; 32],
        key: &IdempotencyKey,
        replayed: bool,
        spend: Spend,
    ) -> Result<ReservationId, LoopError> {
        let status = if replayed {
            self.scheduler
                .snapshot(original)
                .map_err(model_error)?
                .status()
        } else {
            ReservationStatus::Reserved
        };
        match status {
            ReservationStatus::Reserved => Ok(original),
            ReservationStatus::Released => {
                let retry = reservation_id(operation_digest, 1);
                if self.reserve(retry, key, true, spend)? == ReservationStatus::Reserved {
                    Ok(retry)
                } else {
                    Err(outcome_unknown())
                }
            }
            ReservationStatus::Debited
            | ReservationStatus::Reconciled
            | ReservationStatus::ActualOverage => Err(outcome_unknown()),
        }
    }

    fn release(&self, id: ReservationId) -> Result<(), LoopError> {
        self.scheduler.release(id).map(|_| ()).map_err(model_error)
    }

    fn settle(&self, id: ReservationId, charged: bool) -> Result<(), LoopError> {
        if charged {
            self.scheduler.debit(id).map(|_| ()).map_err(model_error)
        } else {
            self.release(id)
        }
    }

    fn settle_outcome(&self, outcome: &PersistedOutcome) -> Result<(), LoopError> {
        self.settle(outcome.reservation_id, outcome.charged)?;
        if !outcome.charged
            || (!self.policy.detached && self.policy.reservation.cost_microusd() != 0)
        {
            return Ok(());
        }
        let actual = outcome.settlement.as_ref().map_or_else(
            || {
                self.scheduler
                    .snapshot(outcome.reservation_id)
                    .map(|snapshot| snapshot.spend())
                    .map_err(model_error)
            },
            |settlement| Ok(settlement.spend()),
        )?;
        self.scheduler
            .reconcile(outcome.reservation_id, actual)
            .map(|_| ())
            .map_err(model_error)
    }

    fn request_reservation(&self, request: &TurnRequest) -> Result<Spend, LoopError> {
        let base = if self.policy.detached || self.policy.reservation.cost_microusd() != 0 {
            self.policy.reservation
        } else {
            let totals = self
                .scheduler
                .totals(self.security.config.run_id())
                .map_err(model_error)?;
            let remaining = RunBudget::from_effective_config(self.security.config.effective())
                .remaining(totals.committed, totals.reserved);
            Spend::new(
                remaining.cost_microusd(),
                self.policy.reservation.tokens(),
                self.policy.reservation.turns(),
                self.policy.reservation.tools(),
                self.policy.reservation.processes(),
            )
        };
        request_reservation(base, request)
    }

    fn intent(
        &self,
        key: &IdempotencyKey,
        digest: CanonicalRequestDigest,
    ) -> Result<Option<IntentReceipt>, LoopError> {
        self.read_record(INTENT_COMMAND, key, digest)
    }

    fn dispatch(
        &self,
        key: &IdempotencyKey,
        digest: CanonicalRequestDigest,
    ) -> Result<Option<DispatchReceipt>, LoopError> {
        self.read_record(DISPATCH_COMMAND, key, digest)
    }

    fn outcome(
        &self,
        key: &IdempotencyKey,
        digest: CanonicalRequestDigest,
    ) -> Result<Option<PersistedOutcome>, LoopError> {
        self.read_record(OUTCOME_COMMAND, key, digest)
    }

    fn read_record<T: for<'de> Deserialize<'de>>(
        &self,
        command: &str,
        key: &IdempotencyKey,
        digest: CanonicalRequestDigest,
    ) -> Result<Option<T>, LoopError> {
        let scope = self.scope(command)?;
        let status = self
            .store
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .idempotency_status(&scope, key)
            .map_err(model_error)?;
        match status {
            IdempotencyStatus::Missing => Ok(None),
            IdempotencyStatus::Terminal {
                request_digest,
                result,
            } if request_digest == digest => serde_json::from_slice(&result.response)
                .map(Some)
                .map_err(model_error),
            IdempotencyStatus::Pending { .. } | IdempotencyStatus::Terminal { .. } => Err(
                LoopError::Provider("invalid durable model record".to_owned()),
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn append_intent(
        &self,
        key: &IdempotencyKey,
        digest: CanonicalRequestDigest,
        reservation_id: ReservationId,
        prompt_digest: [u8; 32],
        model_snapshot_digest: [u8; 32],
        provider: Option<&str>,
        model: Option<&str>,
        reservation: Spend,
        request: &TurnRequest,
    ) -> Result<IntentReceipt, LoopError> {
        let receipt = IntentReceipt {
            model_call_id: ModelCallId::generate().map_err(model_error)?,
            reservation_id,
            correlation: EffectCorrelation {
                run_id: self.security.config.run_id(),
                owner: self.security.attempt,
                claim: self.security.claim,
                operation_id: String::new(),
                idempotency_key: key.as_str().to_owned(),
                command_id: CommandId::generate().map_err(model_error)?,
                intent_event_id: EventId::generate().map_err(model_error)?,
                dispatch_event_id: EventId::generate().map_err(model_error)?,
                outcome_event_id: EventId::generate().map_err(model_error)?,
                occurred_at: self.occurred_at.clone(),
                trace_id: self.trace_id.clone(),
            },
        };
        let mut receipt = receipt;
        receipt.correlation.operation_id = receipt.model_call_id.to_string();
        if !self.policy.detached {
            self.append_journal(
                &format!("effect:{}:intent", key.as_str()),
                receipt.correlation.command_id,
                LoopRecord::EffectIntent(EffectIntent {
                    kind: EffectKind::Model,
                    correlation: receipt.correlation.clone(),
                    payload: EffectIntentPayload::Model {
                        provider: provider.map(str::to_owned),
                        model: model.map(str::to_owned),
                        prompt_digest: format!("sha256:{}", hex(&prompt_digest)),
                        config_digest: format!("sha256:{}", self.security.config.digest_hex()),
                        model_digest: format!("sha256:{}", hex(&model_snapshot_digest)),
                        model_intent: request.metadata.get("kit.grammar_edit.intent").cloned(),
                    },
                }),
            )?;
        }
        let payload = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "model_call_id": receipt.model_call_id,
            "run_id": self.security.config.run_id(),
            "project_id": self.security.config.project_id(),
            "workspace_id": self.security.workspace_id,
            "attempt_id": self.security.attempt.attempt_id,
            "attempt_fence": self.security.attempt.fencing_token.get(),
            "prompt_snapshot_digest": format!("sha256:{}", hex(&prompt_digest)),
            "config_snapshot_digest": format!("sha256:{}", self.security.config.digest_hex()),
            "model_snapshot_digest": format!("sha256:{}", hex(&model_snapshot_digest)),
            "provider": provider,
            "model": model,
            "structured_output": request.structured_output,
            "model_intent": request.metadata.get("kit.grammar_edit.intent"),
            "reservation_id": format!("{:032x}", reservation_id.get()),
            "reservation": spend_json(reservation),
            "provider_idempotency": match self.policy.provider_idempotency {
                ProviderIdempotency::Unproven => "unproven",
                ProviderIdempotency::Enforced => "enforced",
            },
            "detached": self.policy.detached,
        }))
        .map_err(model_error)?;
        self.append_record(
            INTENT_COMMAND,
            INTENT_EVENT,
            key,
            digest,
            &receipt,
            payload,
            receipt.correlation.command_id,
            receipt.correlation.intent_event_id,
            0,
        )
        .map(|(receipt, _)| receipt)
    }

    fn append_dispatch(
        &self,
        key: &IdempotencyKey,
        digest: CanonicalRequestDigest,
        intent: &IntentReceipt,
        reservation_id: ReservationId,
    ) -> Result<(DispatchReceipt, bool), LoopError> {
        if !self.policy.detached {
            self.append_journal(
                &format!("effect:{}:dispatch", key.as_str()),
                intent.correlation.command_id,
                LoopRecord::EffectDispatched(EffectDispatched {
                    kind: EffectKind::Model,
                    correlation: intent.correlation.clone(),
                }),
            )?;
        }
        let receipt = DispatchReceipt { reservation_id };
        let payload = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "model_call_id": intent.model_call_id,
            "attempt_id": self.security.attempt.attempt_id,
            "attempt_fence": self.security.attempt.fencing_token.get(),
            "reservation_id": format!("{:032x}", reservation_id.get()),
        }))
        .map_err(model_error)?;
        self.append_record(
            DISPATCH_COMMAND,
            DISPATCH_EVENT,
            key,
            digest,
            &receipt,
            payload,
            intent.correlation.command_id,
            intent.correlation.dispatch_event_id,
            1,
        )
    }

    fn append_outcome(
        &self,
        key: &IdempotencyKey,
        digest: CanonicalRequestDigest,
        intent: &IntentReceipt,
        dispatched: bool,
        outcome: &PersistedOutcome,
        snapshot: Option<BoundarySnapshot>,
    ) -> Result<(PersistedOutcome, bool), LoopError> {
        let artifacts = outcome
            .result
            .as_ref()
            .map(artifact_refs)
            .unwrap_or_default();
        let payload = serde_json::to_vec(&serde_json::json!({
            "schema_version": 1,
            "model_call_id": intent.model_call_id,
            "reservation_id": format!("{:032x}", outcome.reservation_id.get()),
            "attempt_id": self.security.attempt.attempt_id,
            "attempt_fence": self.security.attempt.fencing_token.get(),
            "status": outcome.status,
            "charged": outcome.charged,
            "finish_reason": outcome.result.as_ref().map(|result| &result.finish_reason),
            "provider_request_id": outcome.result.as_ref().and_then(|result| result.response_id.as_deref()),
            "usage": outcome.usage,
            "settlement": outcome.settlement,
            "policy_violation": outcome.policy_violation,
            "artifacts": artifacts,
            "result_digest": outcome.result.as_ref().map(|result| {
                serde_json::to_vec(result).map(|bytes| format!("sha256:{}", hex(&sha256(&bytes))))
            }).transpose().map_err(model_error)?,
            "error": outcome.error,
            "model_outcome": outcome.result.as_ref().and_then(|result| result.metadata.get("kit.grammar_edit.outcome")),
        }))
        .map_err(model_error)?;
        let appended = self.append_record(
            OUTCOME_COMMAND,
            OUTCOME_EVENT,
            key,
            digest,
            outcome,
            payload,
            intent.correlation.command_id,
            intent.correlation.outcome_event_id,
            if dispatched { 2 } else { 1 },
        )?;
        self.append_journal_outcome(key, intent, &appended.0, snapshot)?;
        Ok(appended)
    }

    #[allow(clippy::too_many_arguments)]
    fn append_record<T>(
        &self,
        command: &str,
        event_type: &str,
        key: &IdempotencyKey,
        digest: CanonicalRequestDigest,
        response: &T,
        payload: Vec<u8>,
        command_id: CommandId,
        event_id: EventId,
        expected_version: u64,
    ) -> Result<(T, bool), LoopError>
    where
        T: Clone + for<'de> Deserialize<'de> + Serialize,
    {
        let response = serde_json::to_vec(response).map_err(model_error)?;
        let payload_value =
            serde_json::from_slice::<serde_json::Value>(&payload).map_err(model_error)?;
        let model_call_id = match payload_value
            .get("model_call_id")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .and_then(|value| ModelCallId::parse(&value).ok())
        {
            Some(id) => id,
            None => return Err(LoopError::Provider("invalid model call payload".to_owned())),
        };
        let command = AppendCommand {
            idempotency_scope: self.scope(command)?,
            idempotency_key: key.clone(),
            request_digest: digest,
            claim: None,
            driver_claim: Some(self.security.claim),
            allow_quiescent_driver_claim: false,
            expected_versions: vec![ExpectedStreamVersion {
                stream: EntityId::ModelCall(model_call_id),
                version: ExpectedVersion::new(expected_version),
            }],
            events: vec![NewEvent {
                id: event_id,
                stream: EntityId::ModelCall(model_call_id),
                event_type: EventType::parse(event_type).map_err(model_error)?,
                schema_version: SchemaVersion::CURRENT,
                occurred_at: self.occurred_at.clone(),
                causation_id: command_id,
                correlation_id: EntityId::Run(self.security.config.run_id()),
                attempt_id: Some(self.security.attempt.attempt_id),
                trace_id: self.trace_id.clone(),
                payload,
                artifacts: serde_json::to_vec(
                    payload_value
                        .get("artifacts")
                        .unwrap_or(&serde_json::Value::Array(Vec::new())),
                )
                .map_err(model_error)?,
            }],
            response,
        };
        let appended = self
            .store
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .append(command)
            .map_err(model_error)?;
        let (response, replayed) = match appended {
            AppendOutcome::Committed(response) => (response.response, false),
            AppendOutcome::Replayed(response) => (response.response, true),
        };
        serde_json::from_slice(&response)
            .map(|value| (value, replayed))
            .map_err(model_error)
    }

    fn scope(&self, command: &str) -> Result<IdempotencyScope, LoopError> {
        IdempotencyScope::new(
            self.security.attempt.principal_id,
            command,
            EntityId::Run(self.security.config.run_id()),
        )
        .map_err(model_error)
    }

    fn commit_unknown(
        &self,
        key: &IdempotencyKey,
        digest: CanonicalRequestDigest,
        intent: &IntentReceipt,
        reservation_id: ReservationId,
        code: &str,
    ) -> Result<(), LoopError> {
        let reserved = self
            .scheduler
            .snapshot(reservation_id)
            .map_err(model_error)?
            .spend();
        let outcome = PersistedOutcome::unknown(reservation_id, code, reserved);
        let (committed, _) = self.append_outcome(key, digest, intent, true, &outcome, None)?;
        self.settle(committed.reservation_id, committed.charged)
    }

    fn commit_not_dispatched(
        &self,
        key: &IdempotencyKey,
        digest: CanonicalRequestDigest,
        intent: &IntentReceipt,
        reservation_id: ReservationId,
    ) -> Result<(), LoopError> {
        let outcome = PersistedOutcome::not_dispatched(reservation_id);
        let (committed, _) = self.append_outcome(key, digest, intent, true, &outcome, None)?;
        self.settle(committed.reservation_id, false)
    }

    fn commit_rejected(
        &self,
        key: &IdempotencyKey,
        digest: CanonicalRequestDigest,
        intent: &IntentReceipt,
        reservation_id: ReservationId,
        usage: Option<&Usage>,
        code: &str,
    ) -> Result<(), LoopError> {
        let reserved = self
            .scheduler
            .snapshot(reservation_id)
            .map_err(model_error)?
            .spend();
        let outcome = PersistedOutcome::rejected(reservation_id, code, reserved, usage);
        let (committed, _) = self.append_outcome(key, digest, intent, true, &outcome, None)?;
        self.settle_outcome(&committed)
    }

    fn append_journal_outcome(
        &self,
        key: &IdempotencyKey,
        intent: &IntentReceipt,
        outcome: &PersistedOutcome,
        snapshot: Option<BoundarySnapshot>,
    ) -> Result<(), LoopError> {
        if self.policy.detached {
            return Ok(());
        }
        let status = match outcome.status {
            ModelOutcomeStatus::Succeeded => EffectStatus::Succeeded,
            ModelOutcomeStatus::Failed => EffectStatus::Failed,
            ModelOutcomeStatus::Cancelled => EffectStatus::Cancelled,
            ModelOutcomeStatus::OutcomeUnknown => EffectStatus::OutcomeUnknown,
        };
        self.append_journal(
            &format!("effect:{}:outcome", key.as_str()),
            intent.correlation.command_id,
            LoopRecord::EffectOutcome(EffectOutcome {
                kind: EffectKind::Model,
                correlation: intent.correlation.clone(),
                status,
                snapshot,
            }),
        )
    }

    fn append_journal(
        &self,
        idempotency_key: &str,
        command_id: CommandId,
        record: LoopRecord,
    ) -> Result<(), LoopError> {
        let idempotency_key = IdempotencyKey::parse(idempotency_key).map_err(model_error)?;
        self.store
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .append_effect(EffectJournalAppend {
                owner: self.security.attempt,
                claim: Some(self.security.claim),
                idempotency_key,
                command_id,
                event_id: EventId::generate().map_err(model_error)?,
                occurred_at: self.occurred_at.clone(),
                trace_id: self.trace_id.clone(),
                artifacts: Vec::new(),
                record,
            })
            .map(|_| ())
            .map_err(model_error)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct IntentReceipt {
    model_call_id: ModelCallId,
    #[serde(with = "reservation_serde")]
    reservation_id: ReservationId,
    correlation: EffectCorrelation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DispatchReceipt {
    #[serde(with = "reservation_serde")]
    reservation_id: ReservationId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ModelOutcomeStatus {
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedOutcome {
    status: ModelOutcomeStatus,
    result: Option<ModelTurnResult>,
    usage: Option<Usage>,
    artifacts: Vec<String>,
    error: Option<String>,
    charged: bool,
    #[serde(default)]
    settlement: Option<ModelSettlement>,
    #[serde(default)]
    policy_violation: Option<String>,
    #[serde(with = "reservation_serde")]
    reservation_id: ReservationId,
}

impl PersistedOutcome {
    fn succeeded(reservation_id: ReservationId, result: ModelTurnResult, reserved: Spend) -> Self {
        let settlement = actual_spend(reserved, result.usage.as_ref());
        Self {
            status: ModelOutcomeStatus::Succeeded,
            usage: result.usage.clone(),
            artifacts: artifact_refs(&result),
            result: Some(result),
            error: None,
            charged: true,
            policy_violation: settlement.policy_violation.clone(),
            settlement: Some(settlement),
            reservation_id,
        }
    }

    fn cancelled(reservation_id: ReservationId) -> Self {
        Self {
            status: ModelOutcomeStatus::Cancelled,
            result: None,
            usage: None,
            artifacts: Vec::new(),
            error: Some("cancelled_before_dispatch".to_owned()),
            charged: false,
            settlement: None,
            policy_violation: None,
            reservation_id,
        }
    }

    fn not_dispatched(reservation_id: ReservationId) -> Self {
        Self {
            status: ModelOutcomeStatus::Failed,
            result: None,
            usage: None,
            artifacts: Vec::new(),
            error: Some("provider_not_dispatched".to_owned()),
            charged: false,
            settlement: None,
            policy_violation: None,
            reservation_id,
        }
    }

    fn unknown(reservation_id: ReservationId, code: &str, reserved: Spend) -> Self {
        Self {
            status: ModelOutcomeStatus::OutcomeUnknown,
            result: None,
            usage: None,
            artifacts: Vec::new(),
            error: Some(code.to_owned()),
            charged: true,
            settlement: Some(ModelSettlement::from_spend(reserved)),
            policy_violation: None,
            reservation_id,
        }
    }

    fn rejected(
        reservation_id: ReservationId,
        code: &str,
        reserved: Spend,
        usage: Option<&Usage>,
    ) -> Self {
        let settlement = actual_spend(reserved, usage);
        Self {
            status: ModelOutcomeStatus::OutcomeUnknown,
            result: None,
            usage: None,
            artifacts: Vec::new(),
            error: Some(code.to_owned()),
            charged: true,
            policy_violation: Some(settlement.policy_violation.clone().map_or_else(
                || code.to_owned(),
                |violation| format!("{code},{violation}"),
            )),
            settlement: Some(settlement),
            reservation_id,
        }
    }

    fn into_turn<T>(self, kernel: Arc<ModelKernel>) -> Result<DurableModelTurn<T>, LoopError> {
        match self.status {
            ModelOutcomeStatus::Succeeded => {
                let result = self.result.ok_or_else(|| {
                    LoopError::Provider("persisted model result is missing".into())
                })?;
                let model_call_id = ModelCallId::generate().map_err(model_error)?;
                let correlation = EffectCorrelation {
                    run_id: kernel.security.config.run_id(),
                    owner: kernel.security.attempt,
                    claim: kernel.security.claim,
                    operation_id: model_call_id.to_string(),
                    idempotency_key: "persisted".to_owned(),
                    command_id: CommandId::generate().map_err(model_error)?,
                    intent_event_id: EventId::generate().map_err(model_error)?,
                    dispatch_event_id: EventId::generate().map_err(model_error)?,
                    outcome_event_id: EventId::generate().map_err(model_error)?,
                    occurred_at: kernel.occurred_at.clone(),
                    trace_id: kernel.trace_id.clone(),
                };
                Ok(DurableModelTurn {
                    inner: None,
                    buffered: VecDeque::from([ModelTurnEvent::Finished(result)]),
                    kernel,
                    key: IdempotencyKey::parse("persisted").expect("static key is valid"),
                    request_digest: CanonicalRequestDigest::new([0; 32]),
                    intent: IntentReceipt {
                        model_call_id,
                        reservation_id: self.reservation_id,
                        correlation,
                    },
                    dispatch: DispatchReceipt {
                        reservation_id: self.reservation_id,
                    },
                    request: TurnRequest {
                        session_id: agentkit_core::SessionId::new("persisted"),
                        turn_id: agentkit_core::TurnId::new("persisted"),
                        transcript: Vec::new(),
                        available_tools: Vec::new(),
                        cache: None,
                        structured_output: None,
                        generation: Default::default(),
                        metadata: agentkit_core::MetadataMap::new(),
                    },
                    cancellation: None,
                    terminal: true,
                })
            }
            ModelOutcomeStatus::Failed => {
                Err(LoopError::Provider(self.error.unwrap_or_else(|| {
                    "model call failed before dispatch".to_owned()
                })))
            }
            ModelOutcomeStatus::Cancelled => Err(LoopError::Cancelled),
            ModelOutcomeStatus::OutcomeUnknown => Err(outcome_unknown()),
        }
    }
}

fn operation_digest(
    security: &ModelSecurity,
    request: &TurnRequest,
    prompt_digest: [u8; 32],
    model_snapshot_digest: [u8; 32],
) -> [u8; 32] {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"KITMODEL\0");
    bytes.extend_from_slice(security.attempt.attempt_id.to_string().as_bytes());
    bytes.extend_from_slice(&security.attempt.fencing_token.get().to_be_bytes());
    bytes.extend_from_slice(security.config.run_id().to_string().as_bytes());
    bytes.extend_from_slice(request.session_id.to_string().as_bytes());
    bytes.extend_from_slice(request.turn_id.to_string().as_bytes());
    bytes.extend_from_slice(&prompt_digest);
    bytes.extend_from_slice(&security.config.digest());
    bytes.extend_from_slice(&model_snapshot_digest);
    sha256(&bytes)
}

fn operation_identity_digest(
    security: &ModelSecurity,
    request: &TurnRequest,
    model_snapshot_digest: [u8; 32],
) -> [u8; 32] {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"KITMODEL-IDENTITY\0");
    bytes.extend_from_slice(security.attempt.attempt_id.to_string().as_bytes());
    bytes.extend_from_slice(&security.attempt.fencing_token.get().to_be_bytes());
    bytes.extend_from_slice(security.config.run_id().to_string().as_bytes());
    bytes.extend_from_slice(request.session_id.to_string().as_bytes());
    bytes.extend_from_slice(request.turn_id.to_string().as_bytes());
    if let Some(correlation) = request
        .metadata
        .get(crate::agent::driver::restart::EFFECT_CORRELATION_METADATA)
    {
        bytes.extend_from_slice(
            &serde_json::to_vec(correlation).expect("effect correlation value serializes"),
        );
    }
    if let Some(result) = request.transcript.iter().rev().find_map(|item| {
        item.parts
            .iter()
            .rev()
            .find(|part| matches!(part, agentkit_core::Part::ToolResult(_)))
    }) {
        bytes.extend_from_slice(
            &serde_json::to_vec(result).expect("tool result correlation serializes"),
        );
    }
    bytes.extend_from_slice(&model_snapshot_digest);
    sha256(&bytes)
}

fn request_reservation(base: Spend, request: &TurnRequest) -> Result<Spend, LoopError> {
    let projected_bytes = serde_json::to_vec(request).map_err(model_error)?.len();
    let tokens = u64::try_from(projected_bytes.div_ceil(3))
        .ok()
        .and_then(|input| {
            input.checked_add(u64::from(
                request.generation.max_output_tokens.unwrap_or_default(),
            ))
        })
        .ok_or_else(|| LoopError::Provider("model prompt token estimate overflowed".to_owned()))?;
    Ok(Spend::new(
        base.cost_microusd(),
        base.tokens().max(tokens),
        base.turns(),
        base.tools(),
        base.processes(),
    ))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ModelSettlement {
    cost_microusd: u64,
    tokens: u64,
    turns: u64,
    tools: u64,
    processes: u64,
    #[serde(skip)]
    policy_violation: Option<String>,
}

impl ModelSettlement {
    fn from_spend(spend: Spend) -> Self {
        Self {
            cost_microusd: spend.cost_microusd(),
            tokens: spend.tokens(),
            turns: spend.turns(),
            tools: spend.tools(),
            processes: spend.processes(),
            policy_violation: None,
        }
    }

    fn spend(&self) -> Spend {
        Spend::new(
            self.cost_microusd,
            self.tokens,
            self.turns,
            self.tools,
            self.processes,
        )
    }
}

fn actual_spend(reserved: Spend, usage: Option<&Usage>) -> ModelSettlement {
    let tokens = usage
        .and_then(|usage| usage.tokens.as_ref())
        .and_then(|tokens| {
            tokens
                .input_tokens
                .checked_add(tokens.cached_input_tokens.unwrap_or_default())?
                .checked_add(tokens.cache_write_input_tokens.unwrap_or_default())?
                .checked_add(tokens.output_tokens)?
                .checked_add(tokens.reasoning_tokens.unwrap_or_default())
        });
    let cost = usage
        .and_then(|usage| usage.cost.as_ref())
        .filter(|cost| cost.currency.eq_ignore_ascii_case("USD"))
        .and_then(|cost| {
            let micros = cost.amount * 1_000_000.0;
            (micros.is_finite() && micros >= 0.0 && micros <= u64::MAX as f64)
                .then(|| micros.ceil() as u64)
        });
    let mut violations = Vec::new();
    if usage.and_then(|usage| usage.cost.as_ref()).is_none() {
        violations.push("provider_cost_missing");
    } else if cost.is_none() {
        violations.push("provider_cost_invalid");
    } else if cost.is_some_and(|cost| cost > reserved.cost_microusd()) {
        violations.push("provider_cost_overage");
    }
    if tokens.is_none() {
        violations.push("provider_tokens_missing_or_invalid");
    } else if tokens.is_some_and(|tokens| tokens > reserved.tokens()) {
        violations.push("provider_token_overage");
    }
    let spend = Spend::new(
        cost.unwrap_or(reserved.cost_microusd()),
        tokens.unwrap_or(reserved.tokens()),
        1,
        0,
        0,
    );
    ModelSettlement {
        policy_violation: (!violations.is_empty()).then(|| violations.join(",")),
        ..ModelSettlement::from_spend(spend)
    }
}

fn model_snapshot_digest(provider: Option<&str>, model: Option<&str>, config: &[u8]) -> [u8; 32] {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"KITMODEL-SNAPSHOT\0");
    bytes.extend_from_slice(provider.unwrap_or_default().as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(model.unwrap_or_default().as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(config);
    sha256(&bytes)
}

fn model_boundary(
    request: &TurnRequest,
    result: Option<&ModelTurnResult>,
) -> Option<BoundarySnapshot> {
    let result = result?;
    let mut transcript = request
        .transcript
        .iter()
        .map(from_agentkit_item)
        .collect::<Vec<_>>();
    transcript.extend(result.output_items.iter().map(from_agentkit_item));
    Some(BoundarySnapshot {
        boundary: SafeBoundary::AfterModelOutcome,
        transcript,
        resume_index: Some(0),
        model_outcome: Some(CommittedModelOutcome::from_agentkit(result)),
    })
}

fn reservation_id(digest: [u8; 32], retry: u8) -> ReservationId {
    let mut bytes = Vec::with_capacity(33);
    bytes.extend_from_slice(&digest);
    bytes.push(retry);
    ReservationId::new(u128::from_be_bytes(
        sha256(&bytes)[..16].try_into().unwrap(),
    ))
}

fn discard_reasoning(result: &mut ModelTurnResult) {
    for item in &mut result.output_items {
        item.parts.retain(|part| {
            !matches!(part, Part::Reasoning(_))
                || matches!(part, Part::Reasoning(reasoning) if crate::agent::providers::openai_subscription::durable_reasoning(reasoning))
        });
    }
}

fn sanitize_delta(delta: Delta, hidden: &mut BTreeSet<PartId>, max: usize) -> Vec<Delta> {
    match delta {
        Delta::BeginPart {
            part_id,
            kind: PartKind::Reasoning,
        } => {
            hidden.insert(part_id);
            Vec::new()
        }
        Delta::BeginPart { part_id, kind } => vec![Delta::BeginPart { part_id, kind }],
        Delta::AppendText { part_id, chunk } if !hidden.contains(&part_id) => {
            split_text(chunk, max)
                .into_iter()
                .map(|chunk| Delta::AppendText {
                    part_id: part_id.clone(),
                    chunk,
                })
                .collect()
        }
        Delta::AppendBytes { part_id, chunk } if !hidden.contains(&part_id) => chunk
            .chunks(max)
            .map(|chunk| Delta::AppendBytes {
                part_id: part_id.clone(),
                chunk: chunk.to_vec(),
            })
            .collect(),
        Delta::ReplaceStructured { part_id, value } if !hidden.contains(&part_id) => {
            vec![Delta::ReplaceStructured { part_id, value }]
        }
        Delta::SetMetadata { part_id, metadata } if !hidden.contains(&part_id) => {
            vec![Delta::SetMetadata { part_id, metadata }]
        }
        Delta::CommitPart {
            part: Part::Reasoning(_),
        } => Vec::new(),
        Delta::CommitPart { part } => vec![Delta::CommitPart { part }],
        _ => Vec::new(),
    }
}

fn split_text(text: String, max: usize) -> Vec<String> {
    if text.len() <= max {
        return vec![text];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + max).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = text[start..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(offset, _)| start + offset);
        }
        chunks.push(text[start..end].to_owned());
        start = end;
    }
    chunks
}

fn push_bounded(
    events: &mut VecDeque<ModelTurnEvent>,
    event: ModelTurnEvent,
    used: &mut usize,
    maximum: usize,
) -> Result<(), LoopError> {
    let size = serde_json::to_vec(&event).map_err(model_error)?.len();
    *used = used
        .checked_add(size)
        .ok_or_else(|| LoopError::Provider("model stream buffer overflow".to_owned()))?;
    if *used > maximum {
        return Err(LoopError::Provider(
            "model stream exceeded durable buffer limit".to_owned(),
        ));
    }
    events.push_back(event);
    Ok(())
}

fn artifact_refs(result: &ModelTurnResult) -> Vec<String> {
    let mut artifacts = BTreeSet::new();
    for item in &result.output_items {
        for part in &item.parts {
            let reference = match part {
                Part::Media(media) => Some(&media.data),
                Part::File(file) => Some(&file.data),
                Part::Custom(custom) => custom.data.as_ref(),
                _ => None,
            };
            if let Some(agentkit_core::DataRef::Handle(handle)) = reference
                && handle.0.starts_with("blake3:")
            {
                artifacts.insert(handle.0.clone());
            }
        }
    }
    artifacts.into_iter().collect()
}

fn spend_json(spend: Spend) -> serde_json::Value {
    serde_json::json!({
        "cost_microusd": spend.cost_microusd(),
        "tokens": spend.tokens(),
        "turns": spend.turns(),
        "tools": spend.tools(),
        "processes": spend.processes(),
    })
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use fmt::Write as _;
        write!(output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}

fn model_error(error: impl fmt::Display) -> LoopError {
    LoopError::Provider(format!("durable model adapter: {error}"))
}

fn injected(point: ModelCrashPoint) -> LoopError {
    LoopError::Provider(format!("injected durable model crash at {point:?}"))
}

fn outcome_unknown() -> LoopError {
    LoopError::Provider("model call outcome_unknown".to_owned())
}

mod reservation_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::runtime::scheduler::reserve::ReservationId;

    pub fn serialize<S>(id: &ReservationId, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u128(id.get())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<ReservationId, D::Error>
    where
        D: Deserializer<'de>,
    {
        u128::deserialize(deserializer).map(ReservationId::new)
    }
}

#[cfg(test)]
mod tests {
    use agentkit_core::{CostUsage, Item, ItemKind, MetadataMap, TokenUsage};
    use agentkit_loop::{StructuredOutputRequest, TurnRequest};

    use super::*;

    #[test]
    fn reservation_estimate_includes_projected_schema_and_instructions() {
        let mut request = TurnRequest {
            session_id: agentkit_core::SessionId::new("session"),
            turn_id: agentkit_core::TurnId::new("turn"),
            transcript: vec![Item::text(
                ItemKind::Developer,
                "structured edit instructions",
            )],
            available_tools: Vec::new(),
            cache: None,
            structured_output: None,
            generation: Default::default(),
            metadata: MetadataMap::new(),
        };
        let ordinary = request_reservation(Spend::new(0, 1, 1, 0, 0), &request).unwrap();
        request.structured_output = Some(
            StructuredOutputRequest::new(
                "edit",
                1,
                true,
                serde_json::json!({
                    "type": "object",
                    "properties": {"content": {"type": "string", "const": "x".repeat(4096)}}
                }),
            )
            .unwrap(),
        );
        let constrained = request_reservation(Spend::new(0, 1, 1, 0, 0), &request).unwrap();
        assert!(constrained.tokens() > ordinary.tokens());
        assert_eq!(
            constrained.tokens(),
            u64::try_from(serde_json::to_vec(&request).unwrap().len().div_ceil(3)).unwrap()
        );
    }

    #[test]
    fn detached_cost_settlement_is_exact_or_conservatively_full() {
        let reserved = Spend::new(10, 100, 1, 0, 0);
        let exact_usage =
            Usage::new(TokenUsage::new(12, 3)).with_cost(CostUsage::new(0.000_006, "USD"));
        let exact = actual_spend(reserved, Some(&exact_usage));
        assert_eq!(exact.spend(), Spend::new(6, 15, 1, 0, 0));
        assert!(exact.policy_violation.is_none());

        let missing = actual_spend(reserved, None);
        assert_eq!(missing.spend(), reserved);
        assert!(missing.policy_violation.is_some());
        for usage in [
            Usage::new(TokenUsage::new(12, 3)),
            Usage::new(TokenUsage::new(12, 3)).with_cost(CostUsage::new(0.000_006, "EUR")),
        ] {
            let settlement = actual_spend(reserved, Some(&usage));
            assert_eq!(settlement.spend(), Spend::new(10, 15, 1, 0, 0));
            assert!(settlement.policy_violation.is_some());
        }

        let overage = actual_spend(
            reserved,
            Some(&Usage::new(TokenUsage::new(120, 30)).with_cost(CostUsage::new(0.000_011, "USD"))),
        );
        assert_eq!(overage.spend(), Spend::new(11, 150, 1, 0, 0));
        assert_eq!(
            overage.policy_violation.as_deref(),
            Some("provider_cost_overage,provider_token_overage")
        );

        let cached = Usage::new(
            TokenUsage::new(12, 3)
                .with_cached_input_tokens(4)
                .with_cache_write_input_tokens(5)
                .with_reasoning_tokens(2),
        )
        .with_cost(CostUsage::new(0.000_006, "USD"));
        assert_eq!(actual_spend(reserved, Some(&cached)).spend().tokens(), 26);
    }
}
