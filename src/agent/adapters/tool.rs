use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use agentkit_core::{MetadataMap, ToolOutput, ToolResultPart};
use agentkit_tools_core::{
    ApprovalReason, ApprovalRequest, PermissionCode, PermissionDenial, ToolContext, ToolError,
    ToolExecutionOutcome, ToolExecutor, ToolInterruption, ToolRequest, ToolResult, ToolSpec,
};
use serde_json::Value;

use crate::{
    agent::driver::restart::{
        EffectCorrelation, EffectDispatched, EffectIntent, EffectIntentPayload, EffectJournal,
        EffectJournalAppend, EffectKind, EffectOutcome, EffectStatus, LoopRecord,
        PendingToolApproval, effect_records,
    },
    api::{auth::contract::AuthenticatedPrincipal, service::AttemptDriverClaim},
    capabilities::kernel::{
        grant::{ArgumentConstraints, CapabilityGrantSnapshot, DelegationSnapshot, EffectClass},
        identity::{CapabilityIdentity, Digest},
        invoke::{
            ApprovalState, AuthorizedInvocation, DispatchOutcome, InvocationEnvelope,
            InvocationResult, InvocationStatus, InvokeError, RetrySafety,
        },
    },
    domain::{
        config::RunConfigSnapshot,
        events::{TraceId, UtcDateTime},
        ids::{CommandId, EventId, ProjectId, ToolCallId, WorkspaceId},
        lifecycle::AttemptOwnership,
    },
    executor::cancel::ExecutorCancellationCoordinator,
    runtime::scheduler::{limits::Spend, reserve::BudgetLedger},
    store::sqlite::{append::SqliteStore, idempotency::IdempotencyKey},
};

const MAX_PROVIDER_CALL_ID_BYTES: usize = 256;
const APPROVED_INVOCATION_ID_METADATA: &str = "kit.approved_invocation_id";
const APPROVED_COMMAND_ID_METADATA: &str = "kit.approved_command_id";
const APPROVED_INTENT_EVENT_ID_METADATA: &str = "kit.approved_intent_event_id";
const APPROVED_OUTCOME_EVENT_ID_METADATA: &str = "kit.approved_outcome_event_id";
const APPROVED_IDEMPOTENCY_KEY_METADATA: &str = "kit.approved_idempotency_key";
const MAX_CANONICAL_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_PRESENTATION_BYTES: usize = 16 * 1024;
const MAX_RESULT_CODE_BYTES: usize = 256;
type CostEstimator = Arc<dyn Fn(&Value) -> Result<Spend, String> + Send + Sync>;

#[derive(Clone)]
pub struct ToolBinding {
    spec: ToolSpec,
    capability: CapabilityIdentity,
    discovered_schema_digest: Digest,
    bound_schema_digest: Digest,
    effect: EffectClass,
    argument_constraints: ArgumentConstraints,
    reservation: Spend,
    retry_safety: RetrySafety,
    approval: ApprovalState,
    cost_estimator: Option<CostEstimator>,
}

impl ToolBinding {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        spec: ToolSpec,
        capability: CapabilityIdentity,
        discovered_schema_digest: Digest,
        bound_schema_digest: Digest,
        effect: EffectClass,
        argument_constraints: ArgumentConstraints,
        reservation: Spend,
        retry_safety: RetrySafety,
        approval: ApprovalState,
    ) -> Self {
        Self {
            spec,
            capability,
            discovered_schema_digest,
            bound_schema_digest,
            effect,
            argument_constraints,
            reservation,
            retry_safety,
            approval,
            cost_estimator: None,
        }
    }

    pub(crate) fn with_cost_estimator(
        mut self,
        estimator: impl Fn(&Value) -> Result<Spend, String> + Send + Sync + 'static,
    ) -> Self {
        self.cost_estimator = Some(Arc::new(estimator));
        self
    }

    fn reservation(&self, input: &Value) -> Result<Spend, String> {
        self.cost_estimator
            .as_ref()
            .map_or(Ok(self.reservation), |estimate| estimate(input))
    }
}

#[derive(Clone)]
pub struct ToolKernelContext {
    pub authenticated: AuthenticatedPrincipal,
    pub config: RunConfigSnapshot,
    pub grants: CapabilityGrantSnapshot,
    pub delegation: Option<DelegationSnapshot>,
    pub workspace_id: WorkspaceId,
    pub project_id: ProjectId,
    pub attempt: AttemptOwnership,
    pub claim: AttemptDriverClaim,
    pub current_fence: Arc<AtomicU64>,
    pub cancellation: Arc<AtomicBool>,
    pub cancellation_coordinator: Arc<dyn ExecutorCancellationCoordinator>,
    pub budget: Arc<BudgetLedger>,
}

struct KernelRuntime {
    store: SqliteStore,
    capability: Box<dyn FnMut(&AuthorizedInvocation) -> DispatchOutcome + Send>,
}

struct KernelToolContext {
    session_id: Option<agentkit_core::SessionId>,
    turn_id: Option<agentkit_core::TurnId>,
    cancellation: Option<agentkit_core::TurnCancellation>,
}

#[derive(Clone)]
pub struct ToolExecutorAdapter {
    bindings: BTreeMap<String, ToolBinding>,
    context: ToolKernelContext,
    runtime: Arc<Mutex<KernelRuntime>>,
}

impl ToolExecutorAdapter {
    pub fn new(
        bindings: impl IntoIterator<Item = ToolBinding>,
        context: ToolKernelContext,
        store: SqliteStore,
        capability: impl FnMut(&AuthorizedInvocation) -> DispatchOutcome + Send + 'static,
    ) -> Result<Self, ToolAdapterError> {
        let mut by_name = BTreeMap::new();
        for binding in bindings {
            let name = binding.spec.name.0.clone();
            if by_name.insert(name.clone(), binding).is_some() {
                return Err(ToolAdapterError::DuplicateTool(name));
            }
        }
        if by_name.is_empty() {
            return Err(ToolAdapterError::EmptyCatalog);
        }
        Ok(Self {
            bindings: by_name,
            context,
            runtime: Arc::new(Mutex::new(KernelRuntime {
                store,
                capability: Box::new(capability),
            })),
        })
    }

    fn execute_kernel(
        &self,
        request: ToolRequest,
        ctx: KernelToolContext,
        approved: Option<&ApprovalRequest>,
    ) -> ToolExecutionOutcome {
        let Some(binding) = self.bindings.get(&request.tool_name.0) else {
            return ToolExecutionOutcome::FailedBeforeInvocation(ToolError::NotFound(
                request.tool_name,
            ));
        };
        if request.session_id.0 != self.context.config.run_id().to_string()
            || ctx.session_id.as_ref() != Some(&request.session_id)
            || ctx.turn_id.as_ref() != Some(&request.turn_id)
        {
            return invalid_input("tool request is not bound to the active run and turn");
        }
        if ctx
            .cancellation
            .as_ref()
            .is_some_and(agentkit_core::TurnCancellation::is_cancelled)
        {
            self.context.cancellation.store(true, Ordering::Release);
        }
        let _cancellation_watch = ctx.cancellation.map(|cancellation| {
            CancellationWatch::start(
                cancellation,
                Arc::clone(&self.context.cancellation),
                Arc::clone(&self.context.cancellation_coordinator),
                self.context.attempt,
            )
        });

        let mut runtime = match self.runtime.lock() {
            Ok(runtime) => runtime,
            Err(_) => return internal("tool kernel runtime lock is poisoned"),
        };
        let KernelRuntime { store, capability } = &mut *runtime;
        if store.verify_driver_claim(self.context.claim).is_err() {
            return internal("tool attempt driver claim is stale");
        }
        let base_ids = match persisted_invocation_ids(store, self.context.attempt, &request.call_id)
            .and_then(|ids| {
                ids.map_or_else(|| InvocationIds::mint(&self.context, &request, false), Ok)
            }) {
            Ok(ids) => ids,
            Err(message) => return invalid_input(message),
        };
        let (ids, approval) = match approved {
            None => (base_ids, binding.approval),
            Some(approval) => {
                if binding.approval != ApprovalState::Pending
                    || approval.call_id.as_ref() != Some(&request.call_id)
                    || approval.id.0 != approval_id(base_ids.invocation_id)
                {
                    return invalid_input("approval does not match the durable tool invocation");
                }
                let ids = match InvocationIds::from_approval(&self.context, &request, approval) {
                    Ok(ids) => ids,
                    Err(message) => return invalid_input(message),
                };
                (ids, ApprovalState::Approved)
            }
        };
        let arguments = match serde_json::to_vec(&request.input) {
            Ok(arguments) => arguments,
            Err(error) => return invalid_input(error.to_string()),
        };
        if arguments.len() > crate::capabilities::native::MAX_NATIVE_INPUT_BYTES {
            return invalid_input("tool arguments exceed the trusted input byte limit");
        }
        let validator = match jsonschema::validator_for(&binding.spec.input_schema) {
            Ok(validator) => validator,
            Err(_) => return internal("bound tool schema is invalid"),
        };
        if !validator.is_valid(&request.input) {
            return invalid_input("tool arguments do not match the bound Draft 2020-12 schema");
        }
        let reservation = match binding.reservation(&request.input) {
            Ok(reservation) => reservation,
            Err(error) => return invalid_input(error),
        };

        let correlation = match tool_correlation(store, &self.context, &ids) {
            Ok(correlation) => correlation,
            Err(error) => return internal(error),
        };
        if approved.is_none()
            && approval == ApprovalState::Pending
            && let Some(pending) =
                match persisted_approval(store, self.context.attempt, &correlation) {
                    Ok(pending) => pending,
                    Err(error) => return internal(error),
                }
        {
            return ToolExecutionOutcome::Interrupted(ToolInterruption::ApprovalRequired(
                pending.approval,
            ));
        }
        let binding_snapshot = binding_snapshot(binding, reservation);
        if let Err(error) = append_tool_journal(
            store,
            &self.context,
            &correlation,
            "intent",
            LoopRecord::EffectIntent(EffectIntent {
                kind: EffectKind::Tool,
                correlation: correlation.clone(),
                payload: EffectIntentPayload::Capability {
                    tool_name: request.tool_name.0.clone(),
                    capability: capability_snapshot(&binding.capability),
                    effect: effect_name(binding.effect).to_owned(),
                    input: request.input.clone(),
                    binding: binding_snapshot.clone(),
                },
            }),
        ) {
            return internal(error);
        }
        if matches!(
            approval,
            ApprovalState::NotRequired | ApprovalState::Approved
        ) && let Err(error) = append_tool_journal(
            store,
            &self.context,
            &correlation,
            "dispatch",
            LoopRecord::EffectDispatched(EffectDispatched {
                kind: EffectKind::Tool,
                correlation: correlation.clone(),
            }),
        ) {
            return internal(error);
        }
        let mut bounded_capability =
            |authorized: &AuthorizedInvocation| bound_dispatch(capability(authorized));
        let result = crate::capabilities::native::orchestrate::OrchestratedNativeInvocation::new(
            InvocationEnvelope {
                authenticated: &self.context.authenticated,
                config: &self.context.config,
                grants: &self.context.grants,
                delegation: self.context.delegation.as_ref(),
                capability: &binding.capability,
                discovered_schema_digest: binding.discovered_schema_digest,
                bound_schema_digest: binding.bound_schema_digest,
                effect: binding.effect,
                argument_constraints: &binding.argument_constraints,
                arguments: &arguments,
                workspace_id: self.context.workspace_id,
                project_id: self.context.project_id,
                invocation_id: ids.invocation_id,
                idempotency_key: &ids.idempotency_key,
                reservation,
                retry_safety: binding.retry_safety,
                approval,
                cancellation: &self.context.cancellation,
                attempt: self.context.attempt,
                driver_claim: Some(self.context.claim),
                current_fence: &self.context.current_fence,
                command_id: ids.command_id,
                intent_event_id: ids.intent_event_id,
                outcome_event_id: ids.outcome_event_id,
                occurred_at: &ids.occurred_at,
                trace_id: &ids.trace_id,
            },
            store,
            &self.context.budget,
        )
        .execute(&mut bounded_capability);
        match result {
            Ok(result) => {
                let status = match result.canonical.status {
                    InvocationStatus::Succeeded => EffectStatus::Succeeded,
                    InvocationStatus::Cancelled => EffectStatus::Cancelled,
                    InvocationStatus::OutcomeUnknown => EffectStatus::OutcomeUnknown,
                    InvocationStatus::Failed
                    | InvocationStatus::ApprovalRequired
                    | InvocationStatus::ApprovalDenied => EffectStatus::Failed,
                };
                if let Err(error) = append_tool_journal(
                    store,
                    &self.context,
                    &correlation,
                    "outcome",
                    LoopRecord::EffectOutcome(EffectOutcome {
                        kind: EffectKind::Tool,
                        correlation: correlation.clone(),
                        status,
                        snapshot: None,
                    }),
                ) {
                    return internal(error);
                }
                if result.canonical.status == InvocationStatus::ApprovalRequired {
                    let approval =
                        match persisted_approval(store, self.context.attempt, &correlation) {
                            Ok(Some(pending)) => {
                                return ToolExecutionOutcome::Interrupted(
                                    ToolInterruption::ApprovalRequired(pending.approval),
                                );
                            }
                            Ok(None) => {
                                let approved_ids =
                                    match InvocationIds::mint(&self.context, &request, true) {
                                        Ok(ids) => ids,
                                        Err(error) => return invalid_input(error),
                                    };
                                approval_request(&request, ids.invocation_id, &approved_ids)
                            }
                            Err(error) => return internal(error),
                        };
                    let pending = PendingToolApproval {
                        correlation: correlation.clone(),
                        request: request.clone(),
                        approval: approval.clone(),
                        binding: binding_snapshot,
                    };
                    if let Err(error) = append_tool_journal(
                        store,
                        &self.context,
                        &correlation,
                        "waiting",
                        LoopRecord::ToolApprovalRequested(pending),
                    ) {
                        return internal(error);
                    }
                    ToolExecutionOutcome::Interrupted(ToolInterruption::ApprovalRequired(approval))
                } else {
                    map_result(request, result, ids.invocation_id)
                }
            }
            Err(error) => {
                let status = if matches!(error, InvokeError::InjectedCrash(_)) {
                    EffectStatus::OutcomeUnknown
                } else {
                    EffectStatus::Failed
                };
                if let Err(journal_error) = append_tool_journal(
                    store,
                    &self.context,
                    &correlation,
                    "outcome",
                    LoopRecord::EffectOutcome(EffectOutcome {
                        kind: EffectKind::Tool,
                        correlation: correlation.clone(),
                        status,
                        snapshot: None,
                    }),
                ) {
                    return internal(journal_error);
                }
                map_invoke_error(error)
            }
        }
    }
}

struct CancellationWatch {
    done: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CancellationWatch {
    fn start(
        cancellation: agentkit_core::TurnCancellation,
        signal: Arc<AtomicBool>,
        coordinator: Arc<dyn ExecutorCancellationCoordinator>,
        attempt: AttemptOwnership,
    ) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&done);
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                if cancellation.is_cancelled() {
                    signal.store(true, Ordering::Release);
                    while !stopped.load(Ordering::Acquire) {
                        let _ = coordinator.cancel_attempt(attempt);
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        Self {
            done,
            thread: Some(thread),
        }
    }
}

impl Drop for CancellationWatch {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn persisted_invocation_ids(
    store: &SqliteStore,
    owner: AttemptOwnership,
    call_id: &agentkit_core::ToolCallId,
) -> Result<Option<InvocationIds>, &'static str> {
    let records =
        effect_records(store, owner).map_err(|_| "durable tool correlation is unavailable")?;
    Ok(records.into_iter().find_map(|record| match record {
        LoopRecord::ToolApprovalRequested(pending) | LoopRecord::ToolApprovalRestored(pending)
            if &pending.request.call_id == call_id =>
        {
            Some(InvocationIds {
                invocation_id: ToolCallId::parse(&pending.correlation.operation_id).ok()?,
                command_id: pending.correlation.command_id,
                intent_event_id: pending.correlation.intent_event_id,
                outcome_event_id: pending.correlation.outcome_event_id,
                idempotency_key: IdempotencyKey::parse(&pending.correlation.idempotency_key)
                    .ok()?,
                occurred_at: pending.correlation.occurred_at,
                trace_id: pending.correlation.trace_id,
            })
        }
        _ => None,
    }))
}

impl ToolExecutor for ToolExecutorAdapter {
    fn specs(&self) -> Vec<ToolSpec> {
        self.bindings
            .values()
            .map(|binding| binding.spec.clone())
            .collect()
    }

    fn execute<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        request: ToolRequest,
        ctx: &'life1 mut ToolContext<'life2>,
    ) -> Pin<Box<dyn Future<Output = ToolExecutionOutcome> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        let adapter = self.clone();
        let cancellation = ctx.cancellation.clone();
        let session_id = ctx.capability.session_id.cloned();
        let turn_id = ctx.capability.turn_id.cloned();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                adapter.execute_kernel(
                    request,
                    KernelToolContext {
                        session_id,
                        turn_id,
                        cancellation,
                    },
                    None,
                )
            })
            .await
            .unwrap_or_else(|error| internal(format!("tool worker failed: {error}")))
        })
    }

    fn execute_approved<'life0, 'life1, 'life2, 'life3, 'async_trait>(
        &'life0 self,
        request: ToolRequest,
        approved_request: &'life1 ApprovalRequest,
        ctx: &'life2 mut ToolContext<'life3>,
    ) -> Pin<Box<dyn Future<Output = ToolExecutionOutcome> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        'life3: 'async_trait,
        Self: 'async_trait,
    {
        let adapter = self.clone();
        let cancellation = ctx.cancellation.clone();
        let session_id = ctx.capability.session_id.cloned();
        let turn_id = ctx.capability.turn_id.cloned();
        let approved_request = approved_request.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                adapter.execute_kernel(
                    request,
                    KernelToolContext {
                        session_id,
                        turn_id,
                        cancellation,
                    },
                    Some(&approved_request),
                )
            })
            .await
            .unwrap_or_else(|error| internal(format!("tool worker failed: {error}")))
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolAdapterError {
    EmptyCatalog,
    DuplicateTool(String),
}

impl std::fmt::Display for ToolAdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyCatalog => formatter.write_str("tool catalog must not be empty"),
            Self::DuplicateTool(name) => write!(formatter, "duplicate tool binding: {name}"),
        }
    }
}

impl std::error::Error for ToolAdapterError {}

#[derive(Clone)]
struct InvocationIds {
    invocation_id: ToolCallId,
    command_id: CommandId,
    intent_event_id: EventId,
    outcome_event_id: EventId,
    idempotency_key: IdempotencyKey,
    occurred_at: UtcDateTime,
    trace_id: TraceId,
}

impl InvocationIds {
    fn mint(
        context: &ToolKernelContext,
        request: &ToolRequest,
        approved: bool,
    ) -> Result<Self, &'static str> {
        if request.call_id.0.is_empty() || request.call_id.0.len() > MAX_PROVIDER_CALL_ID_BYTES {
            return Err("provider tool-call id is empty or too large");
        }
        let mut seed = Vec::new();
        for value in [
            context.authenticated.principal_id().to_string(),
            context.project_id.to_string(),
            context.config.run_id().to_string(),
            context.attempt.attempt_id.to_string(),
            context.attempt.fencing_token.get().to_string(),
            request.session_id.to_string(),
            request.turn_id.to_string(),
            request.call_id.to_string(),
        ] {
            seed.extend_from_slice(&(value.len() as u64).to_be_bytes());
            seed.extend_from_slice(value.as_bytes());
        }
        let derive = |label: &[u8]| {
            let mut bytes = seed.clone();
            bytes.extend_from_slice(label);
            bytes.push(u8::from(approved));
            bytes
        };
        let mut pending_seed = seed.clone();
        pending_seed.extend_from_slice(b"invocation");
        let pending_invocation_id = ToolCallId::from_stable_bytes(&pending_seed);
        let invocation_id = if approved {
            approved_invocation_id(pending_invocation_id)
        } else {
            pending_invocation_id
        };
        Ok(Self {
            invocation_id,
            command_id: CommandId::from_stable_bytes(&derive(b"command")),
            intent_event_id: EventId::from_stable_bytes(&derive(b"intent")),
            outcome_event_id: EventId::from_stable_bytes(&derive(b"outcome")),
            idempotency_key: IdempotencyKey::parse(&format!("tool:{invocation_id}"))
                .map_err(|_| "host-minted tool idempotency key is invalid")?,
            occurred_at: UtcDateTime::now().map_err(|_| "tool receipt clock is unavailable")?,
            trace_id: TraceId::parse(&format!("tool-{invocation_id}"))
                .map_err(|_| "host-minted tool trace id is invalid")?,
        })
    }

    fn from_approval(
        context: &ToolKernelContext,
        request: &ToolRequest,
        approval: &ApprovalRequest,
    ) -> Result<Self, &'static str> {
        let mut ids = Self::mint(context, request, true)?;
        ids.invocation_id = approval
            .metadata
            .get(APPROVED_INVOCATION_ID_METADATA)
            .and_then(Value::as_str)
            .ok_or("approved invocation correlation is missing")
            .and_then(|value| {
                ToolCallId::parse(value).map_err(|_| "approved invocation correlation is invalid")
            })?;
        ids.command_id = approval
            .metadata
            .get(APPROVED_COMMAND_ID_METADATA)
            .and_then(Value::as_str)
            .ok_or("approved command correlation is missing")
            .and_then(|value| {
                CommandId::parse(value).map_err(|_| "approved command correlation is invalid")
            })?;
        ids.intent_event_id = approval
            .metadata
            .get(APPROVED_INTENT_EVENT_ID_METADATA)
            .and_then(Value::as_str)
            .ok_or("approved intent correlation is missing")
            .and_then(|value| {
                EventId::parse(value).map_err(|_| "approved intent correlation is invalid")
            })?;
        ids.outcome_event_id = approval
            .metadata
            .get(APPROVED_OUTCOME_EVENT_ID_METADATA)
            .and_then(Value::as_str)
            .ok_or("approved outcome correlation is missing")
            .and_then(|value| {
                EventId::parse(value).map_err(|_| "approved outcome correlation is invalid")
            })?;
        ids.idempotency_key = approval
            .metadata
            .get(APPROVED_IDEMPOTENCY_KEY_METADATA)
            .and_then(Value::as_str)
            .ok_or("approved idempotency correlation is missing")
            .and_then(|value| {
                IdempotencyKey::parse(value)
                    .map_err(|_| "approved idempotency correlation is invalid")
            })?;
        ids.trace_id = TraceId::parse(&format!("tool-{}", ids.invocation_id))
            .map_err(|_| "approved invocation trace is invalid")?;
        Ok(ids)
    }
}

fn tool_correlation(
    store: &SqliteStore,
    context: &ToolKernelContext,
    ids: &InvocationIds,
) -> Result<EffectCorrelation, String> {
    let operation_id = ids.invocation_id.to_string();
    if let Some(correlation) = effect_records(store, context.attempt)
        .map_err(|error| error.to_string())?
        .into_iter()
        .find_map(|record| match record {
            LoopRecord::EffectIntent(intent)
                if intent.kind == EffectKind::Tool
                    && intent.correlation.operation_id == operation_id =>
            {
                Some(intent.correlation)
            }
            LoopRecord::ToolApprovalRequested(pending)
            | LoopRecord::ToolApprovalRestored(pending)
                if pending.correlation.operation_id == operation_id =>
            {
                Some(pending.correlation)
            }
            _ => None,
        })
    {
        return Ok(correlation);
    }
    Ok(EffectCorrelation {
        run_id: context.config.run_id(),
        owner: context.attempt,
        claim: context.claim,
        operation_id,
        idempotency_key: ids.idempotency_key.as_str().to_owned(),
        command_id: ids.command_id,
        intent_event_id: ids.intent_event_id,
        dispatch_event_id: EventId::generate().map_err(|error| error.to_string())?,
        outcome_event_id: ids.outcome_event_id,
        occurred_at: ids.occurred_at.clone(),
        trace_id: ids.trace_id.clone(),
    })
}

fn append_tool_journal(
    store: &mut SqliteStore,
    context: &ToolKernelContext,
    correlation: &EffectCorrelation,
    stage: &str,
    record: LoopRecord,
) -> Result<(), String> {
    let idempotency_key =
        IdempotencyKey::parse(&format!("effect:{}:{stage}", correlation.idempotency_key))
            .map_err(|error| error.to_string())?;
    store
        .append_effect(EffectJournalAppend {
            owner: context.attempt,
            claim: Some(context.claim),
            idempotency_key,
            command_id: correlation.command_id,
            event_id: EventId::generate().map_err(|error| error.to_string())?,
            occurred_at: correlation.occurred_at.clone(),
            trace_id: correlation.trace_id.clone(),
            artifacts: Vec::new(),
            record,
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn persisted_approval(
    store: &SqliteStore,
    owner: AttemptOwnership,
    correlation: &EffectCorrelation,
) -> Result<Option<PendingToolApproval>, String> {
    effect_records(store, owner)
        .map_err(|error| error.to_string())
        .map(|records| {
            records.into_iter().find_map(|record| match record {
                LoopRecord::ToolApprovalRequested(pending)
                | LoopRecord::ToolApprovalRestored(pending)
                    if pending.correlation == *correlation =>
                {
                    Some(pending)
                }
                _ => None,
            })
        })
}

fn capability_snapshot(capability: &CapabilityIdentity) -> Value {
    serde_json::json!({
        "source": capability.source().as_str(),
        "namespace": capability.namespace().as_str(),
        "name": capability.name().as_str(),
        "version": capability.version().as_str(),
        "implementation_digest": capability.implementation_digest().to_string(),
    })
}

fn binding_snapshot(binding: &ToolBinding, reservation: Spend) -> Value {
    serde_json::json!({
        "spec": binding.spec,
        "capability": capability_snapshot(&binding.capability),
        "discovered_schema_digest": binding.discovered_schema_digest.to_string(),
        "bound_schema_digest": binding.bound_schema_digest.to_string(),
        "effect": effect_name(binding.effect),
        "argument_constraints": binding.argument_constraints.predicates().iter()
            .map(|predicate| String::from_utf8_lossy(predicate.as_bytes()).into_owned())
            .collect::<Vec<_>>(),
        "reservation": {
            "cost_microusd": reservation.cost_microusd(),
            "tokens": reservation.tokens(),
            "turns": reservation.turns(),
            "tools": reservation.tools(),
            "processes": reservation.processes(),
        },
        "retry_safety": binding.retry_safety,
        "approval": format!("{:?}", binding.approval).to_ascii_lowercase(),
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

fn bound_dispatch(outcome: DispatchOutcome) -> DispatchOutcome {
    match outcome {
        DispatchOutcome::Succeeded(output) | DispatchOutcome::DurablyCommitted(output)
            if output.body.len() > MAX_CANONICAL_OUTPUT_BYTES
                || output.media_type.len() > MAX_RESULT_CODE_BYTES =>
        {
            DispatchOutcome::Failed {
                code: "tool_output_too_large".to_owned(),
            }
        }
        DispatchOutcome::Succeeded(output) => DispatchOutcome::Succeeded(output),
        DispatchOutcome::DurablyCommitted(output) => DispatchOutcome::DurablyCommitted(output),
        DispatchOutcome::Failed { code } => DispatchOutcome::Failed {
            code: clip_utf8(&code, MAX_RESULT_CODE_BYTES).to_owned(),
        },
    }
}

fn map_result(
    request: ToolRequest,
    result: InvocationResult,
    invocation_id: ToolCallId,
) -> ToolExecutionOutcome {
    match result.canonical.status {
        InvocationStatus::Succeeded => completed(request, result, invocation_id),
        InvocationStatus::ApprovalRequired => {
            internal("approval result was not intercepted by the durable adapter")
        }
        InvocationStatus::ApprovalDenied => ToolExecutionOutcome::FailedBeforeInvocation(
            ToolError::PermissionDenied(PermissionDenial {
                code: PermissionCode::CustomPolicyDenied,
                message: "tool approval denied".to_owned(),
                metadata: MetadataMap::new(),
            }),
        ),
        InvocationStatus::Cancelled => ToolExecutionOutcome::Failed(ToolError::Cancelled),
        InvocationStatus::OutcomeUnknown => {
            ToolExecutionOutcome::Failed(ToolError::Unavailable(format!(
                "tool outcome unknown: {}",
                result.canonical.code.as_deref().unwrap_or("unknown")
            )))
        }
        InvocationStatus::Failed => ToolExecutionOutcome::Failed(ToolError::ExecutionFailed(
            result
                .canonical
                .code
                .unwrap_or_else(|| "tool_failed".to_owned()),
        )),
    }
}

fn completed(
    request: ToolRequest,
    result: InvocationResult,
    invocation_id: ToolCallId,
) -> ToolExecutionOutcome {
    let Some(output) = result.canonical.output else {
        return internal("successful kernel result has no canonical output");
    };
    let clipped = clip_utf8_bytes(&output.body, MAX_PRESENTATION_BYTES);
    let presentation = if output.media_type == "application/json" && !clipped.1 {
        serde_json::from_slice(&output.body)
            .map(ToolOutput::Structured)
            .unwrap_or_else(|_| {
                ToolOutput::Text(String::from_utf8_lossy(&output.body).into_owned())
            })
    } else {
        ToolOutput::Text(String::from_utf8_lossy(clipped.0).into_owned())
    };
    let mut metadata = MetadataMap::new();
    metadata.insert(
        "kit.kernel_status".to_owned(),
        Value::String("succeeded".to_owned()),
    );
    metadata.insert("kit.replayed".to_owned(), Value::Bool(result.replayed));
    metadata.insert(
        "kit.charged".to_owned(),
        Value::Bool(result.canonical.charged),
    );
    metadata.insert(
        "kit.media_type".to_owned(),
        Value::String(output.media_type),
    );
    metadata.insert(
        "kit.presentation_truncated".to_owned(),
        Value::Bool(clipped.1),
    );
    metadata.insert(
        "kit.native_operation_id".to_owned(),
        Value::String(invocation_id.to_string()),
    );
    metadata.insert(
        "kit.native_result_id".to_owned(),
        Value::String(invocation_id.to_string()),
    );
    ToolExecutionOutcome::Completed(
        ToolResult::new(ToolResultPart {
            call_id: request.call_id,
            output: presentation,
            is_error: false,
            metadata: metadata.clone(),
        })
        .with_metadata(metadata),
    )
}

fn approval_request(
    request: &ToolRequest,
    invocation_id: ToolCallId,
    approved: &InvocationIds,
) -> ApprovalRequest {
    let mut approval = ApprovalRequest::new(
        approval_id(invocation_id),
        "kit.capability.invoke",
        ApprovalReason::PolicyRequiresConfirmation,
        format!("Approve tool {}", request.tool_name.0),
    )
    .with_call_id(request.call_id.clone());
    approval.metadata.insert(
        APPROVED_INVOCATION_ID_METADATA.to_owned(),
        Value::String(approved.invocation_id.to_string()),
    );
    approval.metadata.insert(
        APPROVED_COMMAND_ID_METADATA.to_owned(),
        Value::String(approved.command_id.to_string()),
    );
    approval.metadata.insert(
        APPROVED_INTENT_EVENT_ID_METADATA.to_owned(),
        Value::String(approved.intent_event_id.to_string()),
    );
    approval.metadata.insert(
        APPROVED_OUTCOME_EVENT_ID_METADATA.to_owned(),
        Value::String(approved.outcome_event_id.to_string()),
    );
    approval.metadata.insert(
        APPROVED_IDEMPOTENCY_KEY_METADATA.to_owned(),
        Value::String(approved.idempotency_key.as_str().to_owned()),
    );
    approval.metadata.insert(
        "kit.kernel_status".to_owned(),
        Value::String("approval_required".to_owned()),
    );
    approval
        .metadata
        .insert("kit.charged".to_owned(), Value::Bool(false));
    approval
}

fn approval_id(invocation_id: ToolCallId) -> String {
    format!("kit-approval:{invocation_id}")
}

fn approved_invocation_id(invocation_id: ToolCallId) -> ToolCallId {
    ToolCallId::from_stable_bytes(format!("approved:{invocation_id}").as_bytes())
}

fn map_invoke_error(error: InvokeError) -> ToolExecutionOutcome {
    match error {
        InvokeError::AuthorizationDenied(reason) => ToolExecutionOutcome::FailedBeforeInvocation(
            ToolError::PermissionDenied(PermissionDenial {
                code: PermissionCode::CustomPolicyDenied,
                message: format!("capability authorization denied: {reason:?}"),
                metadata: MetadataMap::new(),
            }),
        ),
        InvokeError::SchemaBindingMismatch => invalid_input("tool schema binding mismatch"),
        InvokeError::InvalidArguments => invalid_input("tool arguments are not valid JSON"),
        InvokeError::StaleFence => internal("tool attempt fence is stale"),
        InvokeError::Budget(error) => ToolExecutionOutcome::FailedBeforeInvocation(
            ToolError::Unavailable(format!("tool budget unavailable: {error:?}")),
        ),
        InvokeError::Store(error) => internal(format!("tool outcome persistence failed: {error}")),
        InvokeError::InvalidPersistedOutcome => internal("invalid persisted tool outcome"),
        InvokeError::Serialization(error) => {
            internal(format!("tool outcome serialization failed: {error}"))
        }
        InvokeError::InjectedCrash(point) => {
            internal(format!("tool invocation interrupted at {point:?}"))
        }
    }
}

fn invalid_input(message: impl Into<String>) -> ToolExecutionOutcome {
    ToolExecutionOutcome::FailedBeforeInvocation(ToolError::InvalidInput(message.into()))
}

fn internal(message: impl Into<String>) -> ToolExecutionOutcome {
    ToolExecutionOutcome::Failed(ToolError::Internal(message.into()))
}

fn clip_utf8(value: &str, maximum: usize) -> &str {
    if value.len() <= maximum {
        return value;
    }
    let mut end = maximum;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn clip_utf8_bytes(bytes: &[u8], maximum: usize) -> (&[u8], bool) {
    if bytes.len() <= maximum {
        return (bytes, false);
    }
    let mut end = maximum;
    while end > 0 && std::str::from_utf8(&bytes[..end]).is_err() {
        end -= 1;
    }
    (&bytes[..end], true)
}
