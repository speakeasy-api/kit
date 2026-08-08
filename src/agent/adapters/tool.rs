use std::{
    collections::{BTreeMap, BTreeSet},
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
    ApprovalReason, ApprovalRequest, PermissionCode, PermissionDenial, ToolCatalogEvent,
    ToolContext, ToolError, ToolExecutionOutcome, ToolExecutor, ToolInterruption, ToolRequest,
    ToolResult, ToolSpec,
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
    capabilities::schema::NormalizedSchema,
    capabilities::{
        catalog::CatalogSnapshot,
        discovery::{CapabilityBinding, CapabilityInspection, DiscoveryHandle, DiscoverySession},
        registration::{
            BindingRegistry, BoundRegistrationCall, DirectInvokeCall, PortableInvokeCall,
            ProviderCapabilityContract, RegistrationCall, RegistrationMode, RegistrationPlan,
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
    store::artifacts::ArtifactStore,
    store::sqlite::{append::SqliteStore, idempotency::IdempotencyKey},
    telemetry::tool_learning::{
        self, ErrorClass, ErrorCode, ErrorStage, LearningCandidate, LearningCapabilityKind,
        LearningCommon, LearningOperation, LearningStatus, LearningSurface, PointerDomain,
        PreparedLearningCapture, ProjectPointerHasher, RetryClass, ToolLearningEvent,
    },
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
const MCP_AUTH_REQUEST_KIND: &str = "kit.mcp.auth";
const MCP_AUTH_SCOPE_METADATA: &str = "kit.mcp.auth_scope";
const MCP_AUTH_CHALLENGE_KIND_METADATA: &str = "kit.mcp.auth_challenge_kind";
const MCP_AUTH_CHALLENGE_GENERATION_METADATA: &str = "kit.mcp.auth_challenge_generation";
const MCP_AUTH_CHALLENGE_ID_METADATA: &str = "kit.mcp.auth_challenge_id";
const LEARNING_SURFACE_METADATA: &str = "kit.learning_surface";
const LEARNING_OPERATION_SEQUENCE_METADATA: &str = "kit.operation_sequence";
const LEARNING_ROUTE_METADATA: &str = "kit.learning_route";
type CostEstimator = Arc<dyn Fn(&Value) -> Result<Spend, String> + Send + Sync>;

#[derive(Clone)]
pub struct ToolBinding {
    spec: ToolSpec,
    capability: CapabilityIdentity,
    schema: NormalizedSchema,
    discovered_schema_digest: Digest,
    bound_schema_digest: Digest,
    effect: EffectClass,
    argument_constraints: ArgumentConstraints,
    reservation: Spend,
    retry_safety: RetrySafety,
    approval: ApprovalState,
    cost_estimator: Option<CostEstimator>,
    external: Option<Arc<CapabilityBinding>>,
    extension: crate::capabilities::kernel::grant_ext::RequestExtension,
}

impl ToolBinding {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        spec: ToolSpec,
        capability: CapabilityIdentity,
        schema: NormalizedSchema,
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
            schema,
            discovered_schema_digest,
            bound_schema_digest,
            effect,
            argument_constraints,
            reservation,
            retry_safety,
            approval,
            cost_estimator: None,
            external: None,
            extension: Default::default(),
        }
    }

    pub(crate) fn mcp(
        spec: ToolSpec,
        binding: Arc<CapabilityBinding>,
        constraints: ArgumentConstraints,
        extension: crate::capabilities::kernel::grant_ext::RequestExtension,
    ) -> Self {
        let entry = binding.pinned_entry();
        Self {
            spec,
            capability: entry.identity().clone(),
            schema: entry.schemas().input().schema().clone(),
            discovered_schema_digest: binding.input_schema_digest(),
            bound_schema_digest: binding.input_schema_digest(),
            effect: entry.side_effects().effect(),
            argument_constraints: constraints,
            reservation: Spend::new(0, 0, 0, 1, 0),
            retry_safety: entry.side_effects().retry_safety(),
            approval: ApprovalState::NotRequired,
            cost_estimator: None,
            external: Some(binding),
            extension,
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
    pub custody: crate::domain::secret::SecretCustody,
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
struct McpToolRuntime {
    runtime: Arc<crate::protocols::mcp::transport::McpCapabilityRuntime>,
    artifacts: Arc<ArtifactStore>,
    policy: crate::protocols::mcp::transport::McpResultPolicy,
}

enum ToolInvocation {
    Completed(
        InvocationResult,
        Option<crate::capabilities::result::Presentation>,
    ),
    AuthRequired(crate::capabilities::broker::AuthChallenge),
    Failed(InvokeError),
    TransportFailed(crate::protocols::mcp::transport::TransportError),
}

#[derive(Clone)]
pub struct ToolExecutorAdapter {
    bindings: Arc<Mutex<BTreeMap<String, ToolBinding>>>,
    context: ToolKernelContext,
    runtime: Arc<Mutex<KernelRuntime>>,
    mcp: Option<McpToolRuntime>,
    discovery: Option<Arc<Mutex<DiscoveryToolRuntime>>>,
    catalog_events: Arc<Mutex<Vec<ToolCatalogEvent>>>,
}

#[derive(Clone)]
pub(crate) struct DiscoveryAuthority {
    pub constraints: ArgumentConstraints,
    pub extension: crate::capabilities::kernel::grant_ext::RequestExtension,
}

pub(crate) struct ToolDiscoveryConfig {
    pub catalog: CatalogSnapshot,
    pub authorities: Vec<DiscoveryAuthority>,
    pub provider: ProviderCapabilityContract,
    pub telemetry: Option<Arc<crate::runtime::telemetry::TelemetryRuntime<'static>>>,
    pub pointer_key: [u8; 32],
}

struct DiscoveryToolRuntime {
    catalog: CatalogSnapshot,
    authorities: Vec<DiscoveryAuthority>,
    provider: ProviderCapabilityContract,
    bound: BTreeMap<crate::capabilities::discovery::BindingId, Arc<CapabilityBinding>>,
    hasher: ProjectPointerHasher,
    opportunity: u64,
    telemetry: Option<Arc<crate::runtime::telemetry::TelemetryRuntime<'static>>>,
}

struct EarlyLearningCall {
    common: LearningCommon,
    call: crate::telemetry::tool_learning::LearningPointer,
    binding: Option<crate::telemetry::tool_learning::LearningPointer>,
    source: Option<crate::telemetry::tool_learning::LearningPointer>,
    kind: Option<LearningCapabilityKind>,
    sequence: Option<crate::telemetry::tool_learning::LearningPointer>,
    sequence_order: Option<u16>,
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
        Ok(Self {
            bindings: Arc::new(Mutex::new(by_name)),
            context,
            runtime: Arc::new(Mutex::new(KernelRuntime {
                store,
                capability: Box::new(capability),
            })),
            mcp: None,
            discovery: None,
            catalog_events: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub(crate) fn with_mcp_runtime(
        mut self,
        runtime: Arc<crate::protocols::mcp::transport::McpCapabilityRuntime>,
        artifacts: Arc<ArtifactStore>,
        policy: crate::protocols::mcp::transport::McpResultPolicy,
    ) -> Self {
        self.mcp = Some(McpToolRuntime {
            runtime,
            artifacts,
            policy,
        });
        self
    }

    pub(crate) fn with_discovery(mut self, config: ToolDiscoveryConfig) -> Result<Self, String> {
        let hasher = ProjectPointerHasher::new(self.context.project_id, &config.pointer_key);
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| "tool kernel runtime lock is poisoned".to_owned())?;
        let mut recovered =
            tool_learning::records(&runtime.store, self.context.config.run_id(), &hasher)
                .map_err(|error| error.to_string())?;
        let committed = runtime.store.events().map_err(|error| error.to_string())?;
        let terminal_calls = recovered
            .iter()
            .filter_map(|event| match event {
                ToolLearningEvent::Outcome { call, .. } => Some(call.clone()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        let resumable_calls = recovered
            .iter()
            .filter_map(|event| match event {
                ToolLearningEvent::Error {
                    call,
                    retry: RetryClass::AuthorizationResume | RetryClass::UrlResume,
                    ..
                } => Some(call.clone()),
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        let incomplete = recovered
            .iter()
            .filter_map(|event| match event {
                ToolLearningEvent::Call {
                    common,
                    call,
                    kernel_intent: Some(kernel_intent),
                    ..
                } if !terminal_calls.contains(call) && !resumable_calls.contains(call) => {
                    Some((common.clone(), call.clone(), kernel_intent.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for (common, call, kernel_intent) in incomplete {
            let Some(intent) = committed.iter().find(|stored| {
                stored.event.event_type.as_str() == "capability.invocation_intent"
                    && hasher.pointer(
                        PointerDomain::KernelEvent,
                        stored.event.id.to_string().as_bytes(),
                    ) == kernel_intent
            }) else {
                continue;
            };
            let dispatched = committed.iter().any(|stored| {
                stored.event.stream == intent.event.stream
                    && stored.event.event_type.as_str() == "capability.invocation_dispatched"
            });
            if committed.iter().any(|stored| {
                stored.event.stream == intent.event.stream
                    && stored.event.event_type.as_str() == "capability.invocation_outcome"
            }) {
                continue;
            }
            if !dispatched {
                continue;
            }
            let ordinal = tool_learning::next_ordinal(&runtime.store, self.context.config.run_id())
                .map_err(|error| error.to_string())?;
            let recovered_common = |ordinal, suffix: &[u8]| {
                LearningCommon::new(
                    &hasher,
                    self.context.config.run_id(),
                    ordinal,
                    LearningOperation::Invoke,
                    common.surface,
                    suffix,
                    common.request.clone(),
                    common.capability.clone(),
                    common.schema.clone(),
                )
            };
            let events = [
                ToolLearningEvent::Error {
                    common: recovered_common(ordinal, b"recovered-incomplete-error"),
                    call: call.clone(),
                    stage: ErrorStage::Dispatch,
                    class: ErrorClass::System,
                    code: ErrorCode::OutcomeUnknown,
                    field: None,
                    retry: RetryClass::Unknown,
                    dispatched: true,
                    known: false,
                },
                ToolLearningEvent::Outcome {
                    common: recovered_common(
                        ordinal.saturating_add(1),
                        b"recovered-incomplete-outcome",
                    ),
                    call,
                    status: LearningStatus::OutcomeUnknown,
                    dispatched: true,
                    known: false,
                    cost_microusd: None,
                    kernel_outcome: None,
                },
            ];
            tool_learning::append_many(
                &mut runtime.store,
                self.context.attempt,
                self.context.claim,
                &hasher,
                UtcDateTime::now().map_err(|error| error.to_string())?,
                TraceId::parse("tool-learning-recovery")
                    .expect("tool-learning recovery trace ID is valid"),
                &events,
            )
            .map_err(|error| error.to_string())?;
        }
        recovered = tool_learning::records(&runtime.store, self.context.config.run_id(), &hasher)
            .map_err(|error| error.to_string())?;
        if let Some(telemetry) = &config.telemetry {
            let _ = telemetry.export_learning_outbox(&mut runtime.store, &hasher);
        }
        let opportunity = recovered
            .iter()
            .filter(|event| matches!(event, ToolLearningEvent::Opportunity { .. }))
            .count() as u64;
        let stored_bindings = runtime
            .store
            .discovery_bindings(hasher.project().as_str(), self.context.config.run_id())
            .map_err(|error| error.to_string())?;
        let mut discovery = DiscoveryToolRuntime {
            catalog: config.catalog,
            authorities: config.authorities,
            provider: config.provider,
            bound: BTreeMap::new(),
            hasher,
            opportunity,
            telemetry: config.telemetry,
        };
        for stored in stored_bindings {
            let id = crate::capabilities::discovery::BindingId::parse(&stored)
                .map_err(|error| error.to_string())?;
            let binding = discovery.authorities.iter().find_map(|authority| {
                discovery.catalog.entries().iter().find_map(|entry| {
                    discovery
                        .session(&self.context, authority)
                        .bind_identity(entry.identity())
                        .filter(|binding| binding.id() == id)
                })
            });
            let Some(binding) = binding else {
                runtime
                    .store
                    .remove_discovery_binding(
                        discovery.hasher.project().as_str(),
                        self.context.config.run_id(),
                        &stored,
                    )
                    .map_err(|error| error.to_string())?;
                continue;
            };
            discovery.bound.insert(id, Arc::new(binding));
        }
        drop(runtime);
        let mut tools = self
            .bindings
            .lock()
            .map_err(|_| "tool binding lock is poisoned".to_owned())?;
        for binding in discovery.bound.values() {
            let wire_name = crate::capabilities::registration::direct_wire_name(binding.id());
            tools.insert(
                wire_name.clone(),
                discovery_tool_binding(&discovery, &self.context, wire_name, Arc::clone(binding))?,
            );
        }
        drop(tools);
        self.discovery = Some(Arc::new(Mutex::new(discovery)));
        Ok(self)
    }

    fn execute_kernel(
        &self,
        mut request: ToolRequest,
        ctx: KernelToolContext,
        approved: Option<&ApprovalRequest>,
    ) -> ToolExecutionOutcome {
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
        if self.context.custody.contains_json(&request.input) {
            return invalid_input(
                "active secret values are forbidden in tool input; use an opaque secret reference",
            );
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
        if let Some(discovery) = &self.discovery {
            let mut discovery = match discovery.lock() {
                Ok(discovery) => discovery,
                Err(_) => return internal("tool discovery runtime lock is poisoned"),
            };
            match discovery.prepare(
                &self.context,
                store,
                request,
                &self.bindings,
                &self.catalog_events,
            ) {
                Ok(DiscoveryPrepared::Completed(outcome)) => return outcome,
                Ok(DiscoveryPrepared::Invoke(prepared)) => request = prepared,
                Err(outcome) => return outcome,
            }
        }
        let binding = match self.bindings.lock() {
            Ok(bindings) => bindings.get(&request.tool_name.0).cloned(),
            Err(_) => return internal("tool binding lock is poisoned"),
        };
        let Some(binding) = binding else {
            return ToolExecutionOutcome::FailedBeforeInvocation(ToolError::NotFound(
                request.tool_name,
            ));
        };
        let base_ids = match persisted_invocation_ids(store, self.context.attempt, &request.call_id)
            .and_then(|ids| {
                ids.map_or_else(|| InvocationIds::mint(&self.context, &request, false), Ok)
            }) {
            Ok(ids) => ids,
            Err(message) => {
                return self.fail_before_kernel(
                    store,
                    &request,
                    &binding,
                    ErrorStage::Parsing,
                    ErrorCode::MalformedInput,
                    message,
                );
            }
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
            Err(error) => {
                return self.fail_before_kernel(
                    store,
                    &request,
                    &binding,
                    ErrorStage::Parsing,
                    ErrorCode::MalformedInput,
                    &error.to_string(),
                );
            }
        };
        let reservation = match binding.reservation(&request.input) {
            Ok(reservation) => reservation,
            Err(error) => {
                return self.fail_before_kernel(
                    store,
                    &request,
                    &binding,
                    ErrorStage::SchemaValidation,
                    ErrorCode::InvalidSchema,
                    &error,
                );
            }
        };
        let learning = match prepared_learning_capture(
            self.discovery.as_ref(),
            &self.context,
            &request,
            &binding,
        ) {
            Ok(learning) => learning,
            Err(error) => {
                let required = self.discovery.as_ref().is_some_and(|discovery| {
                    discovery
                        .lock()
                        .ok()
                        .and_then(|discovery| discovery.telemetry.clone())
                        .is_some_and(|telemetry| {
                            telemetry.mark_learning_failure(error.clone());
                            telemetry.learning_required()
                        })
                });
                if required {
                    return internal(error);
                }
                None
            }
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
        let binding_snapshot = binding_snapshot(&binding, reservation);
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
        let envelope = InvocationEnvelope {
            authenticated: &self.context.authenticated,
            config: &self.context.config,
            grants: &self.context.grants,
            delegation: self.context.delegation.as_ref(),
            extension: binding.extension.clone(),
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
            learning: learning.as_ref(),
        };
        let result = if let Some(external) = &binding.external {
            let Some(mcp) = &self.mcp else {
                return internal("MCP binding has no executor-owned runtime");
            };
            let call = match BoundRegistrationCall::direct(Arc::clone(external), &arguments) {
                Ok(call) => call,
                Err(error) => return invalid_input(error.to_string()),
            };
            let resolved = match resolved_mcp_auth(store, self.context.attempt, &request.call_id) {
                Ok(resolution) => resolution,
                Err(error) => return internal(error),
            };
            let invoke = |store: &mut SqliteStore| {
                tokio::runtime::Handle::current().block_on(mcp.runtime.invoke_registered(
                    &call,
                    envelope.clone(),
                    store,
                    &self.context.budget,
                    &mcp.artifacts,
                    &mcp.policy,
                ))
            };
            let mut outcome = invoke(store);
            if let Ok(crate::capabilities::broker::BrokerOutcome::AuthRequired(current)) = &outcome
                && let Some(resolved) = resolved
            {
                let resolution = if resolved.granted {
                    crate::capabilities::broker::AuthResolution::Granted
                } else {
                    crate::capabilities::broker::AuthResolution::Denied
                };
                outcome = match mcp.runtime.resolve_registered_auth(
                    &call,
                    envelope.clone(),
                    &self.context.authenticated,
                    resolution,
                    current,
                    resolved.challenge_id,
                    resolved.challenge_kind,
                    resolved.challenge_generation,
                    store,
                ) {
                    Ok(true) => invoke(store),
                    Ok(false) => outcome,
                    Err(error) => Err(error),
                };
            }
            match outcome {
                Ok(crate::capabilities::broker::BrokerOutcome::Completed(result)) => {
                    ToolInvocation::Completed(result.invocation, result.presentation)
                }
                Ok(crate::capabilities::broker::BrokerOutcome::AuthRequired(challenge)) => {
                    ToolInvocation::AuthRequired(challenge)
                }
                Err(error) => ToolInvocation::TransportFailed(error),
            }
        } else {
            let mut bounded_capability =
                |authorized: &AuthorizedInvocation| bound_dispatch(capability(authorized));
            crate::capabilities::native::orchestrate::OrchestratedCapabilityInvocation::new(
                envelope,
                &binding.schema,
                store,
                &self.context.budget,
            )
            .execute(&mut bounded_capability)
            .map(|result| ToolInvocation::Completed(result, None))
            .unwrap_or_else(ToolInvocation::Failed)
        };
        if kernel_dispatched(store, ids.invocation_id)
            && let Err(error) = append_tool_journal(
                store,
                &self.context,
                &correlation,
                "dispatch",
                LoopRecord::EffectDispatched(EffectDispatched {
                    kind: EffectKind::Tool,
                    correlation: correlation.clone(),
                }),
            )
        {
            return internal(error);
        }
        match result {
            ToolInvocation::AuthRequired(challenge) => {
                if let Err(error) = append_tool_journal(
                    store,
                    &self.context,
                    &correlation,
                    "auth-required",
                    LoopRecord::EffectOutcome(EffectOutcome {
                        kind: EffectKind::Tool,
                        correlation: correlation.clone(),
                        status: EffectStatus::AuthRequired,
                        snapshot: None,
                    }),
                ) {
                    return internal(error);
                }
                ToolExecutionOutcome::Interrupted(ToolInterruption::ApprovalRequired(
                    mcp_auth_request(&request, ids.invocation_id, &challenge),
                ))
            }
            ToolInvocation::Completed(result, presentation) => {
                let status = match result.canonical.status {
                    InvocationStatus::Succeeded => EffectStatus::Succeeded,
                    InvocationStatus::Cancelled => EffectStatus::Cancelled,
                    InvocationStatus::OutcomeUnknown => EffectStatus::OutcomeUnknown,
                    InvocationStatus::Failed
                    | InvocationStatus::ApprovalRequired
                    | InvocationStatus::ApprovalDenied => EffectStatus::Failed,
                };
                let outcome_record = LoopRecord::EffectOutcome(EffectOutcome {
                    kind: EffectKind::Tool,
                    correlation: correlation.clone(),
                    status,
                    snapshot: None,
                });
                if let Err(error) = append_tool_journal(
                    store,
                    &self.context,
                    &correlation,
                    "outcome",
                    outcome_record,
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
                    map_result(request, result, presentation, ids.invocation_id)
                }
            }
            ToolInvocation::Failed(error) => {
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
            ToolInvocation::TransportFailed(error) => {
                if let Err(journal_error) = append_tool_journal(
                    store,
                    &self.context,
                    &correlation,
                    "outcome",
                    LoopRecord::EffectOutcome(EffectOutcome {
                        kind: EffectKind::Tool,
                        correlation: correlation.clone(),
                        status: transport_effect_status(&error, binding.retry_safety),
                        snapshot: None,
                    }),
                ) {
                    return internal(journal_error);
                }
                map_transport_error(error)
            }
        }
    }

    fn fail_before_kernel(
        &self,
        store: &mut SqliteStore,
        request: &ToolRequest,
        binding: &ToolBinding,
        stage: ErrorStage,
        code: ErrorCode,
        message: &str,
    ) -> ToolExecutionOutcome {
        let Some(discovery) = &self.discovery else {
            return invalid_input(message);
        };
        let discovery = match discovery.lock() {
            Ok(discovery) => discovery,
            Err(_) => return internal("tool discovery runtime lock is poisoned"),
        };
        let surface = learning_surface(request).unwrap_or(LearningSurface::Eager);
        let call = discovery.call_common_bound(&self.context, store, request, surface, binding);
        discovery.fail_call(
            &self.context,
            store,
            &call,
            surface,
            stage,
            ErrorClass::Input,
            code,
            None,
        )
    }
}

fn kernel_dispatched(store: &SqliteStore, invocation_id: ToolCallId) -> bool {
    store.events().is_ok_and(|events| {
        events.iter().any(|event| {
            event.event.stream == crate::domain::events::EntityId::ToolCall(invocation_id)
                && event.event.event_type.as_str() == "capability.invocation_dispatched"
        })
    })
}

enum DiscoveryPrepared {
    Completed(ToolExecutionOutcome),
    Invoke(ToolRequest),
}

#[allow(clippy::result_large_err)]
impl DiscoveryToolRuntime {
    fn capability_pointer(
        &self,
        identity: &CapabilityIdentity,
    ) -> crate::telemetry::tool_learning::LearningPointer {
        self.hasher.pointer(
            PointerDomain::Capability,
            &serde_json::to_vec(&capability_snapshot(identity))
                .expect("capability pointer input is serializable"),
        )
    }

    fn session<'a>(
        &'a self,
        context: &'a ToolKernelContext,
        authority: &'a DiscoveryAuthority,
    ) -> DiscoverySession<'a> {
        DiscoverySession::new(
            &self.catalog,
            &context.authenticated,
            &context.config,
            &context.grants,
            context.delegation.as_ref(),
            context.workspace_id,
            context.project_id,
            &authority.constraints,
            authority.extension.clone(),
        )
    }

    fn binding_valid(&self, context: &ToolKernelContext, binding: &CapabilityBinding) -> bool {
        self.authorities
            .iter()
            .any(|authority| binding.validate(&self.session(context, authority)).is_ok())
    }

    fn plan(
        &self,
        context: &ToolKernelContext,
    ) -> Result<(BindingRegistry, RegistrationPlan), String> {
        let registry = BindingRegistry::new(self.bound.values().cloned())
            .map_err(|error| error.to_string())?;
        let plan = registry
            .plan_authorized(&self.provider, |binding| {
                self.binding_valid(context, binding)
            })
            .map_err(|error| error.to_string())?;
        Ok((registry, plan))
    }

    fn specs(
        &self,
        context: &ToolKernelContext,
    ) -> Result<(Vec<ToolSpec>, RegistrationMode), String> {
        if self.authorities.is_empty() {
            return Ok((Vec::new(), RegistrationMode::PortableGeneric));
        }
        let (registry, plan) = self.plan(context)?;
        let mode = plan.mode();
        let mut specs = plan.eager_tools().to_vec();
        if mode == RegistrationMode::Deferred {
            specs.extend(
                plan.deferred_tools_authorized(&registry, |binding| {
                    self.binding_valid(context, binding)
                })
                .map_err(|error| error.to_string())?
                .iter()
                .map(|definition| definition.spec().clone()),
            );
        }
        Ok((specs, mode))
    }

    fn search(
        &self,
        context: &ToolKernelContext,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::capabilities::discovery::SearchResult>, String> {
        let mut results = Vec::new();
        let mut seen = BTreeSet::new();
        for authority in &self.authorities {
            for result in self
                .session(context, authority)
                .search(query, crate::capabilities::discovery::MAX_SEARCH_RESULTS)
                .map_err(|error| error.to_string())?
            {
                if seen.insert(result.handle()) {
                    results.push(result);
                }
                if results.len() == limit {
                    return Ok(results);
                }
            }
        }
        Ok(results)
    }

    fn inspect(
        &self,
        context: &ToolKernelContext,
        handle: DiscoveryHandle,
    ) -> Option<CapabilityInspection> {
        self.authorities
            .iter()
            .find_map(|authority| self.session(context, authority).inspect(handle))
    }

    fn bind(
        &self,
        context: &ToolKernelContext,
        inspection: &CapabilityInspection,
    ) -> Option<CapabilityBinding> {
        self.authorities
            .iter()
            .find_map(|authority| self.session(context, authority).bind(inspection).ok())
    }

    #[allow(clippy::too_many_arguments)]
    fn common(
        &self,
        context: &ToolKernelContext,
        store: &SqliteStore,
        operation: LearningOperation,
        surface: LearningSurface,
        stable_key: &[u8],
        capability: Option<crate::telemetry::tool_learning::LearningPointer>,
        schema: Option<crate::telemetry::tool_learning::LearningPointer>,
    ) -> LearningCommon {
        self.common_at(
            context,
            tool_learning::next_ordinal(store, context.config.run_id()).unwrap_or(u64::MAX),
            operation,
            surface,
            stable_key,
            capability,
            schema,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn common_at(
        &self,
        context: &ToolKernelContext,
        ordinal: u64,
        operation: LearningOperation,
        surface: LearningSurface,
        stable_key: &[u8],
        capability: Option<crate::telemetry::tool_learning::LearningPointer>,
        schema: Option<crate::telemetry::tool_learning::LearningPointer>,
    ) -> LearningCommon {
        LearningCommon::new(
            &self.hasher,
            context.config.run_id(),
            ordinal,
            operation,
            surface,
            stable_key,
            None,
            capability,
            schema,
        )
    }

    fn persist(
        &self,
        context: &ToolKernelContext,
        store: &mut SqliteStore,
        event: ToolLearningEvent,
    ) -> Result<(), String> {
        self.persist_many(context, store, std::slice::from_ref(&event))?;
        Ok(())
    }

    fn persist_many(
        &self,
        context: &ToolKernelContext,
        store: &mut SqliteStore,
        events: &[ToolLearningEvent],
    ) -> Result<(), String> {
        self.persist_many_inner(context, store, events, false)
    }

    fn persist_many_strict(
        &self,
        context: &ToolKernelContext,
        store: &mut SqliteStore,
        events: &[ToolLearningEvent],
    ) -> Result<(), String> {
        self.persist_many_inner(context, store, events, true)
    }

    fn persist_many_inner(
        &self,
        context: &ToolKernelContext,
        store: &mut SqliteStore,
        events: &[ToolLearningEvent],
        strict: bool,
    ) -> Result<(), String> {
        let appended = tool_learning::append_many(
            store,
            context.attempt,
            context.claim,
            &self.hasher,
            UtcDateTime::now().map_err(|error| error.to_string())?,
            TraceId::parse("tool-learning").expect("tool-learning trace ID is valid"),
            events,
        );
        let appended = match appended {
            Ok(appended) => appended,
            Err(error) => {
                if let Some(telemetry) = &self.telemetry {
                    telemetry.mark_learning_failure(error.to_string());
                    if strict || telemetry.learning_required() {
                        return Err(error.to_string());
                    }
                } else if strict {
                    return Err(error.to_string());
                }
                return Ok(());
            }
        };
        if matches!(
            appended,
            crate::store::sqlite::append::AppendOutcome::Committed(_)
        ) && let Some(telemetry) = &self.telemetry
        {
            let _ = telemetry.export_learning_outbox(store, &self.hasher);
        }
        Ok(())
    }

    fn persist_bind(
        &self,
        context: &ToolKernelContext,
        store: &mut SqliteStore,
        event: &ToolLearningEvent,
        binding_id: &str,
    ) -> Result<(), String> {
        let appended = tool_learning::append_bind(
            store,
            context.attempt,
            context.claim,
            &self.hasher,
            UtcDateTime::now().map_err(|error| error.to_string())?,
            TraceId::parse("tool-learning").expect("tool-learning trace ID is valid"),
            event,
            binding_id,
        );
        let appended = match appended {
            Ok(appended) => appended,
            Err(error) => {
                if let Some(telemetry) = &self.telemetry {
                    telemetry.mark_learning_failure(error.to_string());
                }
                return Err(error.to_string());
            }
        };
        if matches!(
            appended,
            crate::store::sqlite::append::AppendOutcome::Committed(_)
        ) && let Some(telemetry) = &self.telemetry
        {
            let _ = telemetry.export_learning_outbox(store, &self.hasher);
        }
        Ok(())
    }

    fn prepare(
        &mut self,
        context: &ToolKernelContext,
        store: &mut SqliteStore,
        request: ToolRequest,
        bindings: &Arc<Mutex<BTreeMap<String, ToolBinding>>>,
        catalog_events: &Arc<Mutex<Vec<ToolCatalogEvent>>>,
    ) -> Result<DiscoveryPrepared, ToolExecutionOutcome> {
        match request.tool_name.0.as_str() {
            "tools_search" => self.execute_search(context, store, request),
            "tools_inspect" => self.execute_inspect(context, store, request),
            "tools_bind" => self.execute_bind(context, store, request, bindings, catalog_events),
            "tools_invoke" => self.prepare_registered(context, store, request, true),
            _ => {
                let external = bindings
                    .lock()
                    .map_err(|_| internal("tool binding lock is poisoned"))?
                    .get(&request.tool_name.0)
                    .map(|binding| binding.external.is_some());
                match external {
                    Some(true) => self.prepare_registered(context, store, request, false),
                    Some(false) => Ok(DiscoveryPrepared::Invoke(request)),
                    None => {
                        let call = self.call_common(
                            context,
                            store,
                            &request,
                            LearningSurface::Deferred,
                            None,
                        );
                        Err(self.fail_call(
                            context,
                            store,
                            &call,
                            LearningSurface::Deferred,
                            ErrorStage::Routing,
                            ErrorClass::Input,
                            ErrorCode::UnknownTool,
                            None,
                        ))
                    }
                }
            }
        }
    }

    fn execute_search(
        &self,
        context: &ToolKernelContext,
        store: &mut SqliteStore,
        request: ToolRequest,
    ) -> Result<DiscoveryPrepared, ToolExecutionOutcome> {
        let parsed = search_input(&request.input);
        let query = parsed.map(|(query, _)| query);
        let query_pointer = self
            .hasher
            .pointer(PointerDomain::Query, query.unwrap_or_default().as_bytes());
        let stable = format!("search:{}", request.call_id.0);
        if parsed.is_none() {
            let call = self.call_common(context, store, &request, LearningSurface::Discovery, None);
            let ordinal = tool_learning::next_ordinal(store, context.config.run_id())
                .map_err(|_| internal("tool-learning operation admission failed"))?;
            let events = self.failure_events(
                context,
                ordinal,
                &call,
                LearningSurface::Discovery,
                LearningOperation::Search,
                ErrorStage::SchemaValidation,
                ErrorClass::Input,
                ErrorCode::InvalidSchema,
                None,
            );
            self.persist_many_strict(context, store, &events)
                .map_err(internal)?;
            return Err(invalid_input("capability search input is invalid"));
        }
        let (query, limit) = parsed.expect("validated search input");
        let results = self.search(context, query, limit);
        let (status, count) = match &results {
            Ok(results) => (LearningStatus::Succeeded, results.len()),
            Err(_) => (LearningStatus::Failed, 0),
        };
        let event = ToolLearningEvent::Search {
            common: self.common(
                context,
                store,
                LearningOperation::Search,
                LearningSurface::Discovery,
                stable.as_bytes(),
                None,
                None,
            ),
            query: query_pointer,
            status,
            result_count: u16::try_from(count).unwrap_or(u16::MAX),
            detail_artifact: None,
        };
        self.persist(context, store, event).map_err(internal)?;
        let results = results.map_err(invalid_input)?;
        let value = serde_json::json!({
            "results": results.into_iter().map(|result| serde_json::json!({
                "handle": result.handle().to_string(),
                "name": result.identity().name().as_str(),
                "namespace": result.identity().namespace().as_str(),
                "summary": result.summary(),
                "version": result.identity().version().as_str(),
            })).collect::<Vec<_>>()
        });
        Ok(DiscoveryPrepared::Completed(discovery_completed(
            request, value,
        )))
    }

    fn execute_inspect(
        &self,
        context: &ToolKernelContext,
        store: &mut SqliteStore,
        request: ToolRequest,
    ) -> Result<DiscoveryPrepared, ToolExecutionOutcome> {
        let handle_text = request
            .input
            .get("handle")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let handle_pointer = self
            .hasher
            .pointer(PointerDomain::Handle, handle_text.as_bytes());
        let inspection = DiscoveryHandle::parse(handle_text)
            .ok()
            .and_then(|handle| self.inspect(context, handle));
        let event = ToolLearningEvent::Inspection {
            common: self.common(
                context,
                store,
                LearningOperation::Inspect,
                LearningSurface::Discovery,
                format!("inspect:{}", request.call_id.0).as_bytes(),
                inspection
                    .as_ref()
                    .map(|inspection| self.capability_pointer(inspection.definition().identity())),
                inspection.as_ref().map(|inspection| {
                    self.hasher.pointer(
                        PointerDomain::Schema,
                        inspection
                            .definition()
                            .schemas()
                            .input()
                            .schema()
                            .source()
                            .normalized_digest()
                            .to_string()
                            .as_bytes(),
                    )
                }),
            ),
            handle: handle_pointer,
            status: if inspection.is_some() {
                LearningStatus::Succeeded
            } else {
                LearningStatus::Unavailable
            },
        };
        self.persist(context, store, event).map_err(internal)?;
        let inspection =
            inspection.ok_or_else(|| invalid_input("capability inspection is unavailable"))?;
        let entry = inspection.definition();
        let value = serde_json::json!({
            "handle": inspection.handle().to_string(),
            "identity": {
                "name": entry.identity().name().as_str(),
                "namespace": entry.identity().namespace().as_str(),
                "version": entry.identity().version().as_str(),
            },
            "input_schema": crate::protocols::mcp::features::model_schema_projection(
                entry.schemas().input().schema().value().clone()
            ),
            "summary": entry.search().summary(),
        });
        Ok(DiscoveryPrepared::Completed(discovery_completed(
            request, value,
        )))
    }

    fn execute_bind(
        &mut self,
        context: &ToolKernelContext,
        store: &mut SqliteStore,
        request: ToolRequest,
        bindings: &Arc<Mutex<BTreeMap<String, ToolBinding>>>,
        catalog_events: &Arc<Mutex<Vec<ToolCatalogEvent>>>,
    ) -> Result<DiscoveryPrepared, ToolExecutionOutcome> {
        let handle_text = request
            .input
            .get("handle")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let inspection = DiscoveryHandle::parse(handle_text)
            .ok()
            .and_then(|handle| self.inspect(context, handle));
        let binding = inspection
            .as_ref()
            .and_then(|inspection| self.bind(context, inspection))
            .map(Arc::new);
        let event = ToolLearningEvent::Inspection {
            common: self.common(
                context,
                store,
                LearningOperation::Bind,
                LearningSurface::Discovery,
                format!("bind:{}", request.call_id.0).as_bytes(),
                inspection
                    .as_ref()
                    .map(|inspection| self.capability_pointer(inspection.definition().identity())),
                inspection.as_ref().map(|inspection| {
                    self.hasher.pointer(
                        PointerDomain::Schema,
                        inspection
                            .definition()
                            .schemas()
                            .input()
                            .schema()
                            .source()
                            .normalized_digest()
                            .to_string()
                            .as_bytes(),
                    )
                }),
            ),
            handle: binding.as_ref().map_or_else(
                || {
                    self.hasher
                        .pointer(PointerDomain::Handle, handle_text.as_bytes())
                },
                |binding| {
                    self.hasher
                        .pointer(PointerDomain::Binding, binding.id().to_string().as_bytes())
                },
            ),
            status: if binding.is_some() {
                LearningStatus::Succeeded
            } else {
                LearningStatus::Unavailable
            },
        };
        let binding = match binding {
            Some(binding) => binding,
            None => {
                let call =
                    self.call_common(context, store, &request, LearningSurface::Discovery, None);
                let ordinal = tool_learning::next_ordinal(store, context.config.run_id())
                    .map_err(|_| internal("tool-learning operation admission failed"))?;
                let mut failure = self.failure_events(
                    context,
                    ordinal + 1,
                    &call,
                    LearningSurface::Discovery,
                    LearningOperation::Bind,
                    ErrorStage::Authorization,
                    ErrorClass::Policy,
                    ErrorCode::BindingExpired,
                    None,
                );
                let mut events = Vec::with_capacity(4);
                events.push(event);
                events.append(&mut failure);
                self.persist_many_strict(context, store, &events)
                    .map_err(internal)?;
                return Err(invalid_input("capability binding is unavailable"));
            }
        };
        let wire_name = crate::capabilities::registration::direct_wire_name(binding.id());
        self.persist_bind(context, store, &event, &binding.id().to_string())
            .map_err(internal)?;
        self.bound.insert(binding.id(), Arc::clone(&binding));
        bindings
            .lock()
            .map_err(|_| internal("tool binding lock is poisoned"))?
            .insert(
                wire_name.clone(),
                discovery_tool_binding(self, context, wire_name.clone(), Arc::clone(&binding))
                    .map_err(internal)?,
            );
        let (_, plan) = self.plan(context).map_err(internal)?;
        if plan.mode() == RegistrationMode::Deferred {
            let mut event = ToolCatalogEvent::new("kit.discovery");
            event.added.push(wire_name.clone());
            catalog_events
                .lock()
                .map_err(|_| internal("tool catalog event lock is poisoned"))?
                .push(event);
        }
        Ok(DiscoveryPrepared::Completed(discovery_completed(
            request,
            serde_json::json!({
                "binding_id": binding.id().to_string(),
                "route": if plan.mode() == RegistrationMode::Deferred { wire_name } else { "tools_invoke".to_owned() },
            }),
        )))
    }

    fn prepare_registered(
        &self,
        context: &ToolKernelContext,
        store: &mut SqliteStore,
        mut request: ToolRequest,
        generic: bool,
    ) -> Result<DiscoveryPrepared, ToolExecutionOutcome> {
        let surface = if generic {
            LearningSurface::Generic
        } else {
            LearningSurface::Deferred
        };
        request.metadata.insert(
            LEARNING_SURFACE_METADATA.to_owned(),
            Value::String(surface.as_str().to_owned()),
        );
        request.metadata.insert(
            LEARNING_ROUTE_METADATA.to_owned(),
            Value::String(request.tool_name.0.clone()),
        );
        let resolved = if generic {
            request
                .input
                .get("binding_id")
                .and_then(Value::as_str)
                .and_then(|id| {
                    self.bound
                        .values()
                        .find(|binding| binding.id().to_string() == id)
                })
        } else {
            self.bound.values().find(|binding| {
                crate::capabilities::registration::direct_wire_name(binding.id())
                    == request.tool_name.0
            })
        };
        let early_call = self.call_common(context, store, &request, surface, resolved);
        let bytes = serde_json::to_vec(&request.input).map_err(|_| {
            self.fail_call(
                context,
                store,
                &early_call,
                surface,
                ErrorStage::Parsing,
                ErrorClass::Input,
                ErrorCode::MalformedInput,
                None,
            )
        })?;
        let (registry, plan) = self.plan(context).map_err(|_| {
            self.fail_call(
                context,
                store,
                &early_call,
                surface,
                ErrorStage::Authorization,
                ErrorClass::Policy,
                ErrorCode::BindingExpired,
                None,
            )
        })?;
        let call = if generic {
            RegistrationCall::Portable(PortableInvokeCall::new(bytes))
        } else {
            RegistrationCall::Direct(DirectInvokeCall::new(request.tool_name.0.clone(), bytes))
        };
        let bound = plan
            .invoke_authorized(&registry, call, |binding| {
                self.binding_valid(context, binding)
            })
            .map_err(|error| {
                let (stage, code) = match error {
                    crate::capabilities::registration::InvocationError::SchemaInvalid(path) => {
                        return self.fail_call(
                            context,
                            store,
                            &early_call,
                            surface,
                            ErrorStage::SchemaValidation,
                            ErrorClass::Input,
                            ErrorCode::InvalidSchema,
                            Some(&path),
                        );
                    }
                    crate::capabilities::registration::InvocationError::SchemaUnsupported => {
                        (ErrorStage::SchemaValidation, ErrorCode::UnsupportedSchema)
                    }
                    crate::capabilities::registration::InvocationError::BindingExpired => {
                        (ErrorStage::Authorization, ErrorCode::BindingExpired)
                    }
                    crate::capabilities::registration::InvocationError::UnknownBinding => {
                        (ErrorStage::Routing, ErrorCode::StaleBinding)
                    }
                    crate::capabilities::registration::InvocationError::UnknownWireName => {
                        (ErrorStage::Routing, ErrorCode::UnknownTool)
                    }
                    _ => (ErrorStage::Parsing, ErrorCode::MalformedInput),
                };
                self.fail_call(
                    context,
                    store,
                    &early_call,
                    surface,
                    stage,
                    ErrorClass::Input,
                    code,
                    None,
                )
            })?;
        let binding = self
            .bound
            .get(&bound.binding().id())
            .expect("registration plan references a live binding");
        let wire_name = crate::capabilities::registration::direct_wire_name(binding.id());
        Ok(DiscoveryPrepared::Invoke(ToolRequest {
            tool_name: agentkit_tools_core::ToolName::new(wire_name),
            input: bound.context().input().clone(),
            ..request
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn fail_call(
        &self,
        context: &ToolKernelContext,
        store: &mut SqliteStore,
        call: &EarlyLearningCall,
        surface: LearningSurface,
        stage: ErrorStage,
        class: ErrorClass,
        code: ErrorCode,
        instance_path: Option<&str>,
    ) -> ToolExecutionOutcome {
        let ordinal = match tool_learning::next_ordinal(store, context.config.run_id()) {
            Ok(ordinal) => ordinal,
            Err(error) => {
                if let Some(telemetry) = &self.telemetry {
                    telemetry.mark_learning_failure(error.to_string());
                    if telemetry.learning_required() {
                        return internal("tool-learning operation admission failed");
                    }
                }
                return invalid_input("registered capability call is invalid");
            }
        };
        let events = self.failure_events(
            context,
            ordinal,
            call,
            surface,
            LearningOperation::Invoke,
            stage,
            class,
            code,
            instance_path,
        );
        if self.persist_many_strict(context, store, &events).is_err() {
            return internal("tool-learning failure persistence failed");
        }
        invalid_input("registered capability call is invalid")
    }

    #[allow(clippy::too_many_arguments)]
    fn failure_events(
        &self,
        context: &ToolKernelContext,
        ordinal: u64,
        call: &EarlyLearningCall,
        surface: LearningSurface,
        operation: LearningOperation,
        stage: ErrorStage,
        class: ErrorClass,
        code: ErrorCode,
        instance_path: Option<&str>,
    ) -> Vec<ToolLearningEvent> {
        let call_event = ToolLearningEvent::Call {
            common: self.common_at(
                context,
                ordinal,
                operation,
                surface,
                format!("call:{}", call.call.as_str()).as_bytes(),
                call.common.capability.clone(),
                call.common.schema.clone(),
            ),
            call: call.call.clone(),
            binding: call.binding.clone(),
            source: call.source.clone(),
            kind: call.kind,
            sequence: call.sequence.clone(),
            sequence_order: call.sequence_order,
            kernel_intent: None,
        };
        let error = ToolLearningEvent::Error {
            common: self.common_at(
                context,
                ordinal.saturating_add(1),
                operation,
                surface,
                format!("error:{}:{stage:?}:{code:?}", call.call.as_str()).as_bytes(),
                call.common.capability.clone(),
                call.common.schema.clone(),
            ),
            call: call.call.clone(),
            stage,
            class,
            code,
            field: instance_path
                .zip(call.common.schema.as_ref())
                .map(|(path, schema)| {
                    self.hasher.pointer(
                        PointerDomain::Field,
                        format!("{}:{path}", schema.as_str()).as_bytes(),
                    )
                }),
            retry: RetryClass::Never,
            dispatched: false,
            known: true,
        };
        let outcome = ToolLearningEvent::Outcome {
            common: self.common_at(
                context,
                ordinal.saturating_add(2),
                operation,
                surface,
                format!("outcome:{}", call.call.as_str()).as_bytes(),
                call.common.capability.clone(),
                call.common.schema.clone(),
            ),
            call: call.call.clone(),
            status: LearningStatus::Failed,
            dispatched: false,
            known: true,
            cost_microusd: None,
            kernel_outcome: None,
        };
        vec![call_event, error, outcome]
    }

    fn call_common(
        &self,
        context: &ToolKernelContext,
        store: &SqliteStore,
        request: &ToolRequest,
        surface: LearningSurface,
        binding: Option<&Arc<CapabilityBinding>>,
    ) -> EarlyLearningCall {
        let input = serde_json::to_vec(&request.input).unwrap_or_default();
        let request_pointer = self.hasher.pointer(PointerDomain::Request, &input);
        let mut identity = Vec::new();
        let operation_sequence = request
            .metadata
            .get(LEARNING_OPERATION_SEQUENCE_METADATA)
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let run = self.hasher.pointer(
            PointerDomain::Run,
            context.config.run_id().to_string().as_bytes(),
        );
        let route = request
            .metadata
            .get(LEARNING_ROUTE_METADATA)
            .and_then(Value::as_str)
            .unwrap_or(&request.tool_name.0);
        for value in [
            run.as_str().as_bytes(),
            request.turn_id.to_string().as_bytes(),
            &operation_sequence.to_be_bytes(),
            route.as_bytes(),
            request.call_id.0.as_bytes(),
            request_pointer.as_str().as_bytes(),
        ] {
            identity.extend_from_slice(&(value.len() as u64).to_be_bytes());
            identity.extend_from_slice(value);
        }
        let call = self.hasher.pointer(PointerDomain::Call, &identity);
        let entry = binding.map(|binding| binding.pinned_entry());
        let common = LearningCommon::new(
            &self.hasher,
            context.config.run_id(),
            tool_learning::next_ordinal(store, context.config.run_id()).unwrap_or(u64::MAX),
            LearningOperation::Invoke,
            surface,
            &identity,
            Some(request_pointer),
            entry.map(|entry| self.capability_pointer(entry.identity())),
            entry.map(|entry| {
                self.hasher.pointer(
                    PointerDomain::Schema,
                    entry
                        .schemas()
                        .input()
                        .schema()
                        .source()
                        .normalized_digest()
                        .to_string()
                        .as_bytes(),
                )
            }),
        );
        let sequence_order = u16::try_from(operation_sequence.saturating_add(1)).ok();
        let sequence = sequence_order.map(|_| {
            self.hasher.pointer(
                PointerDomain::Sequence,
                format!("{}:{}", context.config.run_id(), request.turn_id).as_bytes(),
            )
        });
        EarlyLearningCall {
            common,
            call,
            binding: binding.map(|binding| {
                self.hasher
                    .pointer(PointerDomain::Binding, binding.id().to_string().as_bytes())
            }),
            source: entry.map(|entry| {
                self.hasher.pointer(
                    PointerDomain::Source,
                    entry.identity().source().as_str().as_bytes(),
                )
            }),
            kind: entry.map(|_| LearningCapabilityKind::Tool),
            sequence,
            sequence_order,
        }
    }

    fn call_common_bound(
        &self,
        context: &ToolKernelContext,
        store: &SqliteStore,
        request: &ToolRequest,
        surface: LearningSurface,
        binding: &ToolBinding,
    ) -> EarlyLearningCall {
        let input = serde_json::to_vec(&request.input).unwrap_or_default();
        let request_pointer = self.hasher.pointer(PointerDomain::Request, &input);
        let operation_sequence = request
            .metadata
            .get(LEARNING_OPERATION_SEQUENCE_METADATA)
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let run = self.hasher.pointer(
            PointerDomain::Run,
            context.config.run_id().to_string().as_bytes(),
        );
        let route = request
            .metadata
            .get(LEARNING_ROUTE_METADATA)
            .and_then(Value::as_str)
            .unwrap_or(&request.tool_name.0);
        let mut identity = Vec::new();
        for value in [
            run.as_str().as_bytes(),
            request.turn_id.to_string().as_bytes(),
            &operation_sequence.to_be_bytes(),
            route.as_bytes(),
            request.call_id.0.as_bytes(),
            request_pointer.as_str().as_bytes(),
        ] {
            identity.extend_from_slice(&(value.len() as u64).to_be_bytes());
            identity.extend_from_slice(value);
        }
        let capability = self.hasher.pointer(
            PointerDomain::Capability,
            &serde_json::to_vec(&capability_snapshot(&binding.capability))
                .expect("capability pointer input is serializable"),
        );
        let schema = self.hasher.pointer(
            PointerDomain::Schema,
            binding.bound_schema_digest.to_string().as_bytes(),
        );
        let sequence_order = u16::try_from(operation_sequence.saturating_add(1)).ok();
        EarlyLearningCall {
            common: LearningCommon::new(
                &self.hasher,
                context.config.run_id(),
                tool_learning::next_ordinal(store, context.config.run_id()).unwrap_or(u64::MAX),
                LearningOperation::Invoke,
                surface,
                &identity,
                Some(request_pointer),
                Some(capability),
                Some(schema),
            ),
            call: self.hasher.pointer(PointerDomain::Call, &identity),
            binding: Some(
                self.hasher.pointer(
                    PointerDomain::Binding,
                    binding
                        .external
                        .as_ref()
                        .map_or_else(
                            || binding.capability.implementation_digest().to_string(),
                            |external| external.id().to_string(),
                        )
                        .as_bytes(),
                ),
            ),
            source: Some(self.hasher.pointer(
                PointerDomain::Source,
                binding.capability.source().as_str().as_bytes(),
            )),
            kind: Some(LearningCapabilityKind::Tool),
            sequence: sequence_order.map(|_| {
                self.hasher.pointer(
                    PointerDomain::Sequence,
                    format!("{}:{}", context.config.run_id(), request.turn_id).as_bytes(),
                )
            }),
            sequence_order,
        }
    }
}

fn discovery_completed(request: ToolRequest, value: Value) -> ToolExecutionOutcome {
    ToolExecutionOutcome::Completed(ToolResult::new(ToolResultPart {
        call_id: request.call_id,
        output: ToolOutput::Structured(value),
        is_error: false,
        metadata: MetadataMap::new(),
    }))
}

fn search_input(input: &Value) -> Option<(&str, usize)> {
    let input = input.as_object()?;
    if input.len() != 2 || !input.contains_key("query") || !input.contains_key("limit") {
        return None;
    }
    let query = input.get("query")?.as_str()?;
    let limit = usize::try_from(input.get("limit")?.as_u64()?).ok()?;
    (!query.is_empty()
        && query.chars().count() <= 64
        && query.len() <= crate::capabilities::discovery::MAX_SEARCH_QUERY_BYTES
        && (1..=crate::capabilities::discovery::MAX_SEARCH_RESULTS).contains(&limit))
    .then_some((query, limit))
}

fn discovery_tool_binding(
    discovery: &DiscoveryToolRuntime,
    context: &ToolKernelContext,
    wire_name: String,
    binding: Arc<CapabilityBinding>,
) -> Result<ToolBinding, String> {
    let authority = discovery
        .authorities
        .iter()
        .find(|authority| {
            binding
                .validate(&discovery.session(context, authority))
                .is_ok()
        })
        .ok_or_else(|| "discovery binding has no live authorization".to_owned())?;
    Ok(ToolBinding::mcp(
        ToolSpec::new(
            agentkit_tools_core::ToolName::new(wire_name),
            binding.pinned_entry().search().summary(),
            crate::protocols::mcp::features::model_schema_projection(
                binding
                    .pinned_entry()
                    .schemas()
                    .input()
                    .schema()
                    .value()
                    .clone(),
            ),
        ),
        binding,
        authority.constraints.clone(),
        authority.extension.clone(),
    ))
}

fn learning_surface(request: &ToolRequest) -> Option<LearningSurface> {
    match request
        .metadata
        .get(LEARNING_SURFACE_METADATA)
        .and_then(Value::as_str)
    {
        Some("generic") => Some(LearningSurface::Generic),
        Some("deferred") => Some(LearningSurface::Deferred),
        Some("eager") => Some(LearningSurface::Eager),
        Some("discovery") => Some(LearningSurface::Discovery),
        _ => None,
    }
}

fn prepared_learning_capture(
    discovery: Option<&Arc<Mutex<DiscoveryToolRuntime>>>,
    context: &ToolKernelContext,
    request: &ToolRequest,
    binding: &ToolBinding,
) -> Result<Option<PreparedLearningCapture>, String> {
    let Some(discovery) = discovery else {
        return Ok(None);
    };
    let operation_sequence = request
        .metadata
        .get(LEARNING_OPERATION_SEQUENCE_METADATA)
        .and_then(Value::as_u64)
        .ok_or_else(|| "provider tool call has no operation sequence".to_owned())?;
    let discovery = discovery
        .lock()
        .map_err(|_| "tool discovery runtime lock is poisoned".to_owned())?;
    let capability = serde_json::to_vec(&capability_snapshot(&binding.capability))
        .map_err(|error| error.to_string())?;
    let binding_identity = binding.external.as_ref().map_or_else(
        || binding.capability.implementation_digest().to_string(),
        |external| external.id().to_string(),
    );
    PreparedLearningCapture::new(
        discovery.hasher.clone(),
        context.config.run_id(),
        request.turn_id.to_string(),
        operation_sequence,
        request
            .metadata
            .get(LEARNING_ROUTE_METADATA)
            .and_then(Value::as_str)
            .unwrap_or(&request.tool_name.0),
        request.call_id.0.clone(),
        &serde_json::to_vec(&request.input).map_err(|error| error.to_string())?,
        learning_surface(request).unwrap_or(LearningSurface::Eager),
        &capability,
        binding.bound_schema_digest.to_string().as_bytes(),
        Some(binding_identity.as_bytes()),
        binding.capability.source().as_str().as_bytes(),
        LearningCapabilityKind::Tool,
    )
    .map(|capture| capture.with_telemetry(discovery.telemetry.clone()))
    .map(Some)
    .map_err(|error| error.to_string())
}

fn resolved_mcp_auth(
    store: &SqliteStore,
    owner: AttemptOwnership,
    call_id: &agentkit_core::ToolCallId,
) -> Result<Option<ResolvedMcpAuth>, String> {
    let mut waits = BTreeMap::new();
    let mut resolutions = BTreeMap::new();
    let mut ordinal = 0_u64;
    for record in effect_records(store, owner).map_err(|error| error.to_string())? {
        ordinal = ordinal.saturating_add(1);
        match record {
            LoopRecord::Waiting(crate::agent::driver::waiting::WaitingState {
                wait_id,
                kind:
                    crate::agent::driver::waiting::WaitingKind::Auth {
                        tool_call_id: Some(tool_call_id),
                        challenge_kind,
                        challenge_generation,
                        challenge_id: Some(challenge_id),
                        ..
                    },
                ..
            }) => {
                waits.insert(
                    wait_id,
                    (
                        tool_call_id.to_string(),
                        challenge_id,
                        challenge_kind,
                        challenge_generation,
                        ordinal,
                    ),
                );
            }
            LoopRecord::WaitingResolved(resolved) if waits.contains_key(&resolved.wait_id) => {
                if let crate::agent::driver::waiting::WaitingResolution::Auth { granted } =
                    resolved.resolution
                {
                    resolutions.insert(resolved.wait_id, granted);
                }
            }
            _ => {}
        }
    }
    let latest = waits
        .iter()
        .filter(|(_, (found, ..))| found == call_id.0.as_str())
        .max_by_key(|(_, (_, _, _, generation, ordinal))| (*generation, *ordinal));
    Ok(
        latest.and_then(|(wait_id, (_, challenge_id, kind, generation, _))| {
            let granted = *resolutions.get(wait_id)?;
            Some(ResolvedMcpAuth {
                granted,
                challenge_id: *challenge_id,
                challenge_kind: match kind {
                    crate::agent::driver::waiting::AuthChallengeKind::Broker => {
                        crate::capabilities::broker::AuthChallengeKind::Broker
                    }
                    crate::agent::driver::waiting::AuthChallengeKind::Transport => {
                        crate::capabilities::broker::AuthChallengeKind::Transport
                    }
                    crate::agent::driver::waiting::AuthChallengeKind::Provider => return None,
                },
                challenge_generation: *generation,
            })
        }),
    )
}

#[derive(Clone, Copy)]
struct ResolvedMcpAuth {
    granted: bool,
    challenge_id: crate::domain::ids::ApprovalId,
    challenge_kind: crate::capabilities::broker::AuthChallengeKind,
    challenge_generation: u64,
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
        let mut runtime = match self.runtime.lock() {
            Ok(runtime) => runtime,
            Err(_) => return Vec::new(),
        };
        let bindings = match self.bindings.lock() {
            Ok(bindings) => bindings,
            Err(_) => return Vec::new(),
        };
        let mut specs = bindings
            .values()
            .filter(|binding| binding.external.is_none())
            .map(|binding| binding.spec.clone())
            .collect::<Vec<_>>();
        drop(bindings);
        if let Some(discovery) = &self.discovery
            && let Ok(mut discovery) = discovery.lock()
            && let Ok((mut projected, mode)) = discovery.specs(&self.context)
        {
            specs.append(&mut projected);
            specs.sort_by(|left, right| left.name.0.cmp(&right.name.0));
            let projection = serde_json::to_vec(&specs).unwrap_or_default();
            let offered_names = specs
                .iter()
                .map(|spec| spec.name.0.as_str())
                .collect::<BTreeSet<_>>();
            let generic_available = offered_names.contains("tools_invoke");
            let candidates = self
                .bindings
                .lock()
                .map(|bindings| {
                    specs
                        .iter()
                        .filter_map(|spec| {
                            if let Some(binding) = bindings.get(&spec.name.0) {
                                return Some(LearningCandidate {
                                    capability: discovery.capability_pointer(&binding.capability),
                                    schema: discovery.hasher.pointer(
                                        PointerDomain::Schema,
                                        binding.bound_schema_digest.to_string().as_bytes(),
                                    ),
                                    surface: offered_candidate_surface(
                                        binding.external.is_some(),
                                        true,
                                        mode,
                                        generic_available,
                                    )?,
                                    authorized: true,
                                    offered: true,
                                });
                            }
                            (mode == RegistrationMode::PortableGeneric)
                                .then(|| spec.metadata.get("kit.operation")?.as_str())
                                .flatten()
                                .map(|operation| LearningCandidate {
                                    capability: discovery
                                        .hasher
                                        .pointer(PointerDomain::Capability, operation.as_bytes()),
                                    schema: discovery.hasher.pointer(
                                        PointerDomain::Schema,
                                        &serde_json::to_vec(&spec.input_schema).unwrap_or_default(),
                                    ),
                                    surface: LearningSurface::Generic,
                                    authorized: true,
                                    offered: true,
                                })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let offered = specs.len();
            let generic = candidates
                .iter()
                .filter(|candidate| candidate.surface == LearningSurface::Generic)
                .count();
            if offered > usize::from(tool_learning::MAX_LEARNING_CANDIDATES) || generic > 4 {
                if let Some(telemetry) = &discovery.telemetry {
                    telemetry.mark_learning_failure("tool-learning candidate limit exceeded");
                }
                return specs;
            }
            let event = ToolLearningEvent::Opportunity {
                common: discovery.common(
                    &self.context,
                    &runtime.store,
                    LearningOperation::Projection,
                    LearningSurface::Discovery,
                    format!("opportunity:{}", discovery.opportunity).as_bytes(),
                    None,
                    None,
                ),
                offered: u16::try_from(offered).expect("offered set is bounded"),
                eager: u16::try_from(
                    candidates
                        .iter()
                        .filter(|candidate| candidate.surface == LearningSurface::Eager)
                        .count(),
                )
                .unwrap_or(u16::MAX),
                deferred: u16::try_from(
                    candidates
                        .iter()
                        .filter(|candidate| candidate.surface == LearningSurface::Deferred)
                        .count(),
                )
                .unwrap_or(u16::MAX),
                generic_available,
                projection: discovery.hasher.pointer(PointerDomain::Schema, &projection),
                candidates,
                detail_artifact: None,
            };
            if discovery
                .persist(&self.context, &mut runtime.store, event)
                .is_ok()
            {
                discovery.opportunity = discovery.opportunity.saturating_add(1);
            }
        }
        specs
    }

    fn drain_catalog_events(&self) -> Vec<ToolCatalogEvent> {
        self.catalog_events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
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

fn offered_candidate_surface(
    external: bool,
    directly_offered: bool,
    mode: RegistrationMode,
    generic_available: bool,
) -> Option<LearningSurface> {
    match (external, mode) {
        (false, _) if directly_offered => Some(LearningSurface::Eager),
        (true, RegistrationMode::Deferred) if directly_offered => Some(LearningSurface::Deferred),
        (true, RegistrationMode::PortableGeneric) if generic_available => {
            Some(LearningSurface::Generic)
        }
        _ => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolAdapterError {
    DuplicateTool(String),
}

impl std::fmt::Display for ToolAdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
        DispatchOutcome::Succeeded(output)
        | DispatchOutcome::DurablyCommitted(output)
        | DispatchOutcome::DurablyFailed { output, .. }
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
        DispatchOutcome::DurablyFailed { code, output } => DispatchOutcome::DurablyFailed {
            code: clip_utf8(&code, MAX_RESULT_CODE_BYTES).to_owned(),
            output,
        },
        DispatchOutcome::OutcomeUnknown { code } => DispatchOutcome::OutcomeUnknown {
            code: clip_utf8(&code, MAX_RESULT_CODE_BYTES).to_owned(),
        },
    }
}

fn map_result(
    request: ToolRequest,
    result: InvocationResult,
    presentation: Option<crate::capabilities::result::Presentation>,
    invocation_id: ToolCallId,
) -> ToolExecutionOutcome {
    match result.canonical.status {
        InvocationStatus::Succeeded => {
            completed(request, result, presentation, false, invocation_id)
        }
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
        InvocationStatus::Failed if presentation.is_some() => {
            completed(request, result, presentation, true, invocation_id)
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
    presentation: Option<crate::capabilities::result::Presentation>,
    is_error: bool,
    invocation_id: ToolCallId,
) -> ToolExecutionOutcome {
    let broker_presentation = presentation;
    let Some(output) = result.canonical.output else {
        return internal("presented kernel result has no canonical output");
    };
    let clipped = clip_utf8_bytes(&output.body, MAX_PRESENTATION_BYTES);
    let presentation = if let Some(presentation) = &broker_presentation {
        ToolOutput::Text(presentation.body().to_owned())
    } else if output.media_type == "application/vnd.kit.canonical-result+json" {
        return internal("MCP result has no broker-authoritative presentation");
    } else if output.media_type == "application/json" && !clipped.1 {
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
        Value::String(if is_error { "failed" } else { "succeeded" }.to_owned()),
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
        Value::Bool(broker_presentation.is_none() && clipped.1),
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
            is_error,
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

fn mcp_auth_request(
    request: &ToolRequest,
    invocation_id: ToolCallId,
    challenge: &crate::capabilities::broker::AuthChallenge,
) -> ApprovalRequest {
    let mut auth = ApprovalRequest::new(
        approval_id(invocation_id),
        MCP_AUTH_REQUEST_KIND,
        ApprovalReason::SensitiveAuthScope,
        "Authorize the configured MCP server connection",
    )
    .with_call_id(request.call_id.clone());
    auth.metadata.insert(
        MCP_AUTH_SCOPE_METADATA.to_owned(),
        Value::String(challenge.scope.clone()),
    );
    auth.metadata.insert(
        MCP_AUTH_CHALLENGE_KIND_METADATA.to_owned(),
        Value::String(
            match challenge.kind {
                crate::capabilities::broker::AuthChallengeKind::Broker => "broker",
                crate::capabilities::broker::AuthChallengeKind::Transport => "transport",
            }
            .to_owned(),
        ),
    );
    auth.metadata.insert(
        MCP_AUTH_CHALLENGE_GENERATION_METADATA.to_owned(),
        Value::from(challenge.generation),
    );
    auth.metadata.insert(
        MCP_AUTH_CHALLENGE_ID_METADATA.to_owned(),
        Value::String(challenge.challenge_id.to_string()),
    );
    auth
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
        InvokeError::UnsupportedValidation => {
            invalid_input("tool schema validation is unsupported")
        }
        InvokeError::MissingDriverClaim | InvokeError::StaleFence => {
            internal("tool attempt authority is stale")
        }
        InvokeError::Budget(error) => ToolExecutionOutcome::FailedBeforeInvocation(
            ToolError::Unavailable(format!("tool budget unavailable: {error:?}")),
        ),
        InvokeError::Store(error) => internal(format!("tool outcome persistence failed: {error}")),
        InvokeError::InvalidPersistedOutcome => internal("invalid persisted tool outcome"),
        InvokeError::ArtifactDigestLimit => internal("tool outcome contains too many artifacts"),
        InvokeError::Serialization(error) => {
            internal(format!("tool outcome serialization failed: {error}"))
        }
        InvokeError::InjectedCrash(point) => {
            internal(format!("tool invocation interrupted at {point:?}"))
        }
        InvokeError::NativeCapabilityBinding => internal("native tool binding is unknown"),
        InvokeError::BrokerAuth => internal("native tool entered broker auth handling"),
        InvokeError::Accounting(error) => internal(format!("tool accounting failed: {error}")),
        InvokeError::ToolReservationRequired => invalid_input("tool reservation is missing"),
    }
}

fn transport_effect_status(
    error: &crate::protocols::mcp::transport::TransportError,
    retry_safety: RetrySafety,
) -> EffectStatus {
    match error {
        crate::protocols::mcp::transport::TransportError::Cancelled => EffectStatus::Cancelled,
        crate::protocols::mcp::transport::TransportError::UrlElicitationUnavailable => {
            EffectStatus::OutcomeUnknown
        }
        crate::protocols::mcp::transport::TransportError::AuthRequired(_) => {
            EffectStatus::AuthRequired
        }
        error if retry_safety == RetrySafety::NonIdempotent && uncertain_transport(error) => {
            EffectStatus::OutcomeUnknown
        }
        _ => EffectStatus::Failed,
    }
}

fn map_transport_error(
    error: crate::protocols::mcp::transport::TransportError,
) -> ToolExecutionOutcome {
    use crate::protocols::mcp::transport::TransportError;

    let code = error.completion_code().to_owned();
    match error {
        TransportError::Cancelled => ToolExecutionOutcome::Failed(ToolError::Cancelled),
        TransportError::AuthorizationMismatch
        | TransportError::BindingExpired
        | TransportError::PolicyAuthorizationMismatch
        | TransportError::Broker(_)
        | TransportError::Egress(_) => ToolExecutionOutcome::FailedBeforeInvocation(
            ToolError::PermissionDenied(PermissionDenial {
                code: PermissionCode::CustomPolicyDenied,
                message: code,
                metadata: MetadataMap::new(),
            }),
        ),
        TransportError::AuthRequired(_)
        | TransportError::Credential(_)
        | TransportError::UrlElicitation { .. }
        | TransportError::UrlElicitationUnavailable => {
            ToolExecutionOutcome::Failed(ToolError::Unavailable(code))
        }
        TransportError::UrlElicitationDeclined => {
            ToolExecutionOutcome::Failed(ToolError::ExecutionFailed(code))
        }
        _ => ToolExecutionOutcome::Failed(ToolError::ExecutionFailed(code)),
    }
}

fn uncertain_transport(error: &crate::protocols::mcp::transport::TransportError) -> bool {
    use crate::protocols::mcp::transport::TransportError;
    matches!(
        error,
        TransportError::Timeout(_)
            | TransportError::ConnectionRetired
            | TransportError::Io(_)
            | TransportError::Cleanup { .. }
            | TransportError::SessionExpired
            | TransportError::RefreshClosed
            | TransportError::RefreshRetriesExhausted
            | TransportError::OwnedProcessUnavailable
            | TransportError::Agentkit(_)
    )
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

#[cfg(test)]
mod discovery_input_tests {
    use super::{offered_candidate_surface, search_input};
    use crate::{
        capabilities::registration::RegistrationMode, telemetry::tool_learning::LearningSurface,
    };

    #[test]
    fn search_input_matches_the_advertised_closed_schema() {
        assert_eq!(
            search_input(&serde_json::json!({"query":"database", "limit":100})),
            Some(("database", 100))
        );
        for invalid in [
            serde_json::json!({"query":"database", "limit":0}),
            serde_json::json!({"query":"database", "limit":101}),
            serde_json::json!({"query":"database", "limit":1.5}),
            serde_json::json!({"query":7, "limit":1}),
            serde_json::json!({"query":"", "limit":1}),
            serde_json::json!({"query":"x".repeat(65), "limit":1}),
            serde_json::json!({"query":"database", "limit":1, "extra":true}),
            serde_json::json!(["database", 1]),
        ] {
            assert_eq!(search_input(&invalid), None, "accepted {invalid}");
        }
    }

    #[test]
    fn opportunity_surfaces_cover_native_direct_mcp_generic_and_hide_unoffered_bindings() {
        let cases = [
            (
                false,
                true,
                RegistrationMode::Deferred,
                false,
                Some(LearningSurface::Eager),
            ),
            (
                true,
                true,
                RegistrationMode::Deferred,
                false,
                Some(LearningSurface::Deferred),
            ),
            (
                true,
                true,
                RegistrationMode::Deferred,
                false,
                Some(LearningSurface::Deferred),
            ),
            (
                true,
                false,
                RegistrationMode::PortableGeneric,
                true,
                Some(LearningSurface::Generic),
            ),
            (false, false, RegistrationMode::Deferred, false, None),
            (true, false, RegistrationMode::Deferred, false, None),
            (true, false, RegistrationMode::PortableGeneric, false, None),
        ];
        assert_eq!(
            cases.map(|(external, direct, mode, generic, _)| {
                offered_candidate_surface(external, direct, mode, generic)
            }),
            cases.map(|(_, _, _, _, expected)| expected)
        );
    }
}
