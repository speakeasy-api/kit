use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use agentkit_core::{
    CancellationController, Delta, FinishReason, Item, ItemKind, TurnCancellation, Usage,
};
use agentkit_loop::{
    AgentEvent, LoopError, LoopInterrupt, LoopObserver, LoopStep, ModelAdapter, ModelSession,
    ModelTurn, ModelTurnEvent, ModelTurnResult, ObservedEvent, SessionConfig, TranscriptEvent,
    TranscriptObserver, TurnRequest, TurnResult,
};
use agentkit_provider_anthropic::{AnthropicAdapter, AnthropicSession, AnthropicTurn};
use agentkit_provider_ollama::{OllamaAdapter, OllamaSession, OllamaTurn};
use agentkit_provider_openai::{OpenAIAdapter, OpenAISession, OpenAITurn};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterSession, OpenRouterTurn};
use async_trait::async_trait;
use tokio::{
    sync::{Semaphore, broadcast, mpsc, watch},
    task::JoinHandle,
};

use crate::{
    agent::{
        accounting::{
            CostTable, LogicalModelUsage, ModelOutcome, SpeculationOutcome, ToolMeasurement,
            UsageEnvelope,
        },
        adapters::tool::{
            DiscoveryAuthority, ToolBinding, ToolDiscoveryConfig, ToolExecutorAdapter,
            ToolKernelContext,
        },
        adapters::{
            grammar_edit::{GrammarEditContext, GrammarEditLimits, GrammarEditModelAdapter},
            model::{DurableModelAdapter, ModelPolicy, ModelSecurity, ProviderIdempotency},
        },
        agentkit_bridge::mapping::{
            CanonicalItem, CanonicalPart, from_agentkit_item, from_agentkit_usage,
        },
        context::{ContextProjection, project_canonical_prompt},
        driver::restart::{
            BoundarySnapshot, EffectJournal, EffectJournalAppend, LoopRecord, RecoveryState,
            RestartProjection, SafeBoundary, effect_records,
        },
        extensions::{
            EffectiveExtensionConfig, ExtensionDescriptor, ExtensionError, ExtensionPoint,
            TrustedExtensionToken,
        },
        prompt::{PromptInput, TaskContract, compile},
        providers::{
            adapter::{ModelStreamPolicy, StreamCommitFactory, StreamPolicyAdapter},
            openai_subscription::{
                OpenAiSubscriptionAdapter, OpenAiSubscriptionSession, OpenAiSubscriptionTurn,
            },
            persistence::SqliteStreamCommitFactory,
            streaming::{CanaryRedactor, StreamCommit, StreamLimits},
        },
    },
    api::{
        auth::contract::{AuthenticatedPrincipal, GrantSnapshot},
        service::{
            RunCompletionRecord, RunFailureCode, RunFailureProjection, RunOutputProjection,
            RunProgressRecord, RunPromptProjection, ServiceError, SqliteServiceStore, WorkerRun,
            WorkerStore,
        },
    },
    capabilities::kernel::{
        grant::{ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot, EffectClass},
        identity::{
            CapabilityIdentity, CapabilityName, CapabilityNamespace, CapabilitySource,
            CapabilityVersion, Digest, DigestAlgorithm,
        },
        invoke::{ApprovalState, CanonicalInvocationResult},
    },
    domain::{
        config::{Provider as ConfigProvider, RunConfigSnapshot},
        events::{AttemptState, EntityId, RunState, TraceId, UtcDateTime},
        ids::{CommandId, EventId, ProjectId, RunId, WorkspaceId},
        lifecycle::AttemptOwnership,
        secret::{SecretCustody, SecretLease},
    },
    executor::cancel::{
        ExecutorCancellationCoordinator, ExecutorCancellationOutcome, SqliteCancellationCoordinator,
    },
    executor::process::own::{
        ProcessRegistrationContext, ProcessRegistry, ProcessRegistryRegistration,
    },
    runtime::scheduler::{
        DurableScheduler, SchedulerError,
        budget::RunBudget,
        limits::Spend,
        reserve::{BudgetLedger, ReservationId, ReservationSnapshot, ReservationStatus},
    },
    store::artifacts::{
        ArtifactClass, ArtifactDigest, ArtifactMetadata, ArtifactRetention, ArtifactStore,
        now_unix_micros,
    },
    telemetry::{
        otel::RunOutcome,
        run_envelope::{
            CoreRunObservation, ProviderCacheObservation, ProviderModelDescriptor, RunCapture,
            RunEnvelope, SummaryRetentionPolicy,
        },
    },
};

#[cfg(debug_assertions)]
use std::collections::VecDeque;

#[cfg(debug_assertions)]
use agentkit_core::{
    CostUsage, DataRef, MetadataMap, Part, PartId, PartKind, ReasoningPart, TextPart, TokenUsage,
    ToolCallId as AgentkitToolCallId, ToolCallPart,
};

#[cfg(debug_assertions)]
use crate::agent::accounting::{CostRate, UsageRates};

#[cfg(debug_assertions)]
use crate::agent::extensions::{ExtensionConfigStack, ExtensionRegistry, built_in_descriptors};

pub type SharedWorkerStore = Arc<Mutex<SqliteServiceStore>>;

pub struct RunExecutorConfig {
    pub database: PathBuf,
    pub artifacts: Arc<ArtifactStore>,
    pub store: SharedWorkerStore,
    pub scheduler: DurableScheduler,
    pub model_adapter: SelectedModelAdapter,
    secret_custody: SecretCustody,
    pub concurrency: usize,
    pub queue_capacity: usize,
    pub poll_interval: Duration,
    pub lease_duration: Duration,
    pub claim_renewal_interval: Duration,
    pub model_reservation: Spend,
    pub cancellation_coordinator: Arc<dyn ExecutorCancellationCoordinator>,
    pub telemetry: Option<Arc<crate::runtime::telemetry::TelemetryRuntime<'static>>>,
    tool_learning_key: Option<[u8; 32]>,
    mcp_servers: Vec<crate::protocols::mcp::config::McpServerConfig>,
    capability_extensions: crate::capabilities::extensions::SharedCapabilityExtensionRegistry,
    capability_extensions_owned: bool,
    mcp_stdio_profiles:
        Option<Arc<dyn crate::protocols::mcp::transport::OwnedStdioProfileProvider>>,
    mcp_responder_outcomes: crate::protocols::mcp::responders::ResponderOutcomes,
    callback_secrets: crate::protocols::mcp::responders::CallbackSecretRegistry,
    project_root: PathBuf,
    workspace_scratch: PathBuf,
    edit_workspace: Option<PathBuf>,
    process_registry: Option<Arc<dyn ProcessRegistry>>,
    native_container_image: Option<String>,
    verification_registry: crate::verify::profiles::VerificationRegistry,
    native_formatter_descriptor: Option<crate::workspace::edit::format::FormatterDescriptor>,
    native_formatter_required: bool,
    native_diagnostic_adapters: BTreeMap<String, crate::verify::feedback::DiagnosticAdapter>,
    native_feedback_limits: crate::verify::feedback::FeedbackLimits,
    native_edit_validation_time: Duration,
    pub(crate) native_semantic_evidence:
        crate::capabilities::native::dispatch::NativeSemanticEvidenceStore,
    // Workspace handles for non-terminal runs. A run parked on a durable wait
    // must keep its workspace owner (and revision epoch) alive: if the last
    // handle drops, the resume mints a new epoch and every revision token the
    // model captured before the wait becomes stale.
    run_workspaces:
        Arc<Mutex<BTreeMap<RunId, crate::workspace::revision::ManagedWorkspace>>>,
    #[cfg(debug_assertions)]
    native_check_completions: Vec<crate::executor::check::ConformanceCheck>,
}

impl RunExecutorConfig {
    pub fn new(
        database: impl Into<PathBuf>,
        artifacts: Arc<ArtifactStore>,
        store: SharedWorkerStore,
        scheduler: DurableScheduler,
        model_adapter: SelectedModelAdapter,
    ) -> Self {
        let database = database.into();
        let workspace_scratch = database
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("native-workspaces");
        let project_root = workspace_scratch.join("unconfigured-project");
        let secret_custody = model_adapter.secret_custody();
        Self {
            cancellation_coordinator: Arc::new(SqliteCancellationCoordinator::new(
                database.clone(),
            )),
            database,
            artifacts,
            store,
            scheduler,
            model_adapter,
            secret_custody,
            concurrency: 4,
            queue_capacity: 64,
            poll_interval: Duration::from_millis(250),
            lease_duration: Duration::from_secs(5),
            claim_renewal_interval: Duration::from_secs(1),
            model_reservation: Spend::new(0, 1, 1, 0, 0),
            telemetry: None,
            tool_learning_key: None,
            mcp_servers: Vec::new(),
            capability_extensions: Arc::new(std::sync::RwLock::new(Default::default())),
            capability_extensions_owned: false,
            mcp_stdio_profiles: None,
            mcp_responder_outcomes: Default::default(),
            callback_secrets: Default::default(),
            project_root,
            workspace_scratch,
            edit_workspace: None,
            process_registry: None,
            native_container_image: None,
            verification_registry: crate::verify::profiles::VerificationRegistry::empty(),
            native_formatter_descriptor: None,
            native_formatter_required: false,
            native_diagnostic_adapters: BTreeMap::new(),
            native_feedback_limits: crate::verify::feedback::FeedbackLimits::default(),
            native_edit_validation_time: crate::workspace::edit::ir::EditLimits::default()
                .max_validation_time,
            native_semantic_evidence:
                crate::capabilities::native::dispatch::NativeSemanticEvidenceStore::default(),
            run_workspaces: Arc::new(Mutex::new(BTreeMap::new())),
            #[cfg(debug_assertions)]
            native_check_completions: Vec::new(),
        }
    }

    pub fn with_project_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.project_root = root.into();
        self
    }

    pub(crate) fn with_secret_custody(mut self, custody: SecretCustody) -> Self {
        self.secret_custody = custody;
        self
    }

    pub fn with_tool_learning_key(mut self, key: [u8; 32]) -> Self {
        self.tool_learning_key = Some(key);
        self
    }

    pub fn with_mcp_servers(
        mut self,
        servers: impl IntoIterator<Item = crate::protocols::mcp::config::McpServerConfig>,
    ) -> Self {
        self.mcp_servers = servers.into_iter().collect();
        self
    }

    pub fn with_capability_extensions(
        mut self,
        registry: crate::capabilities::extensions::SharedCapabilityExtensionRegistry,
    ) -> Self {
        self.capability_extensions = registry;
        self.capability_extensions_owned = true;
        self
    }

    pub fn with_mcp_stdio_profiles(
        mut self,
        profiles: Arc<dyn crate::protocols::mcp::transport::OwnedStdioProfileProvider>,
    ) -> Self {
        self.mcp_stdio_profiles = Some(profiles);
        self
    }

    pub fn with_mcp_responder_outcomes(
        mut self,
        outcomes: crate::protocols::mcp::responders::ResponderOutcomes,
    ) -> Self {
        self.mcp_responder_outcomes = outcomes;
        self
    }

    pub(crate) fn with_callback_secret_registry(
        mut self,
        registry: crate::protocols::mcp::responders::CallbackSecretRegistry,
    ) -> Self {
        self.callback_secrets = registry;
        self
    }

    pub fn with_native_container_image(mut self, image: impl Into<String>) -> Self {
        self.native_container_image = Some(image.into());
        self
    }

    pub fn with_verification_registry(
        mut self,
        registry: crate::verify::profiles::VerificationRegistry,
    ) -> Self {
        self.verification_registry = registry;
        self
    }

    pub fn with_native_formatter(
        mut self,
        descriptor: crate::workspace::edit::format::FormatterDescriptor,
        required: bool,
    ) -> Self {
        self.native_formatter_descriptor = Some(descriptor);
        self.native_formatter_required = required;
        self
    }

    pub fn with_native_feedback(
        mut self,
        adapters: BTreeMap<String, crate::verify::feedback::DiagnosticAdapter>,
        limits: crate::verify::feedback::FeedbackLimits,
    ) -> Self {
        self.native_diagnostic_adapters = adapters;
        self.native_feedback_limits = limits;
        self
    }

    pub fn with_native_edit_validation_time(mut self, timeout: Duration) -> Self {
        self.native_edit_validation_time = timeout;
        self
    }

    pub(crate) fn with_native_semantic_evidence(
        mut self,
        evidence: crate::capabilities::native::dispatch::NativeSemanticEvidenceStore,
    ) -> Self {
        self.native_semantic_evidence = evidence;
        self
    }

    #[cfg(debug_assertions)]
    pub fn with_native_check_completions(
        mut self,
        completions: impl IntoIterator<Item = crate::executor::check::ConformanceCheck>,
    ) -> Self {
        self.native_check_completions = completions.into_iter().collect();
        self
    }

    pub(crate) fn with_process_registry(mut self, registry: Arc<dyn ProcessRegistry>) -> Self {
        self.process_registry = Some(registry);
        self
    }

    #[cfg(debug_assertions)]
    pub fn with_development_edit_workspace(mut self, root: impl Into<PathBuf>) -> Self {
        self.edit_workspace = Some(root.into());
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutorHealth {
    pub running: bool,
    pub accepting: bool,
    pub active: usize,
    pub completed: u64,
    pub failed: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProgressEvent {
    pub run_id: RunId,
    pub attempt: AttemptOwnership,
    pub event: AgentEvent,
}

pub struct RunExecutor {
    wake: mpsc::Sender<()>,
    stop: watch::Sender<bool>,
    progress: broadcast::Sender<ProgressEvent>,
    task: Mutex<Option<JoinHandle<()>>>,
    state: Arc<HealthState>,
}

impl RunExecutor {
    pub fn start(mut config: RunExecutorConfig) -> Result<Self, ExecutorError> {
        if config.concurrency == 0
            || config.queue_capacity == 0
            || config.database.as_os_str().is_empty()
        {
            return Err(ExecutorError::Config(
                "executor bounds and database path must be non-zero",
            ));
        }
        if config.claim_renewal_interval < Duration::from_millis(10) {
            return Err(ExecutorError::Config(
                "claim renewal interval must be at least 10ms",
            ));
        }
        if config.claim_renewal_interval >= config.lease_duration {
            return Err(ExecutorError::Config(
                "claim renewal interval must be shorter than the lease duration",
            ));
        }
        if config.claim_renewal_interval > config.lease_duration / 3 {
            return Err(ExecutorError::Config(
                "claim renewal interval must not exceed one third of the lease duration",
            ));
        }
        if config.mcp_stdio_profiles.is_none()
            && let Some(profile) =
                config
                    .mcp_servers
                    .iter()
                    .find_map(|server| match &server.transport {
                        crate::protocols::mcp::config::McpTransportConfig::Stdio {
                            owned_process_profile,
                            ..
                        } => Some(owned_process_profile.clone()),
                        crate::protocols::mcp::config::McpTransportConfig::Http { .. } => None,
                    })
        {
            return Err(ExecutorError::McpStdioServiceUnavailable { profile });
        }
        if !config.capability_extensions_owned {
            let snapshots = config
                .store
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .worker_append_store()
                .map_err(|error| ExecutorError::Worker(error.to_string()))?
                .extension_registry_snapshots()
                .map_err(ExecutorError::Store)?;
            let registry =
                crate::capabilities::extensions::CapabilityExtensionRegistry::from_repository_snapshots(
                    snapshots,
                )
                .map_err(|error| ExecutorError::Worker(error.to_string()))?;
            config.capability_extensions = Arc::new(std::sync::RwLock::new(registry));
            config.capability_extensions_owned = true;
        }
        SqliteStreamCommitFactory::open(&config.database, StreamLimits::default())?;
        let (wake, wake_rx) = mpsc::channel(config.queue_capacity);
        let (stop, stop_rx) = watch::channel(false);
        let (progress, _) = broadcast::channel(config.queue_capacity);
        let state = Arc::new(HealthState::default());
        state.running.store(true, Ordering::Release);
        state.accepting.store(true, Ordering::Release);
        let task = tokio::spawn(run_workers(
            Arc::new(config),
            wake_rx,
            stop_rx,
            progress.clone(),
            Arc::clone(&state),
        ));
        let executor = Self {
            wake,
            stop,
            progress,
            task: Mutex::new(Some(task)),
            state,
        };
        executor.notify();
        Ok(executor)
    }

    pub fn notify(&self) {
        let _ = self.wake.try_send(());
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ProgressEvent> {
        self.progress.subscribe()
    }

    pub async fn shutdown(&self) -> Result<(), ExecutorError> {
        self.state.accepting.store(false, Ordering::Release);
        let _ = self.stop.send(true);
        let task = self
            .task
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(task) = task
            && let Err(error) = task.await
        {
            return Err(ExecutorError::Worker(format!("worker join: {error}")));
        }
        Ok(())
    }

    pub fn health(&self) -> ExecutorHealth {
        ExecutorHealth {
            running: self.state.running.load(Ordering::Acquire),
            accepting: self.state.accepting.load(Ordering::Acquire),
            active: self.state.active.load(Ordering::Acquire),
            completed: self.state.completed.load(Ordering::Acquire),
            failed: self.state.failed.load(Ordering::Acquire),
        }
    }
}

#[derive(Default)]
struct HealthState {
    running: AtomicBool,
    accepting: AtomicBool,
    active: AtomicUsize,
    completed: AtomicU64,
    failed: AtomicU64,
}

async fn run_workers(
    config: Arc<RunExecutorConfig>,
    mut wake: mpsc::Receiver<()>,
    mut stop: watch::Receiver<bool>,
    progress: broadcast::Sender<ProgressEvent>,
    health: Arc<HealthState>,
) {
    let semaphore = Arc::new(Semaphore::new(config.concurrency));
    let mut tasks = tokio::task::JoinSet::new();
    let mut active = BTreeSet::new();
    loop {
        if *stop.borrow() {
            break;
        }
        dispatch_available(
            &config,
            &semaphore,
            &mut tasks,
            &mut active,
            &progress,
            &health,
        );
        tokio::select! {
            _ = stop.changed() => {}
            _ = wake.recv() => {}
            _ = tokio::time::sleep(config.poll_interval) => {}
            result = tasks.join_next(), if !tasks.is_empty() => {
                match result {
                    Some(Ok(run_id)) => { active.remove(&run_id); }
                    Some(Err(_)) => { health.failed.fetch_add(1, Ordering::Relaxed); }
                    None => {}
                }
            }
        }
    }
    health.accepting.store(false, Ordering::Release);
    while tasks.join_next().await.is_some() {}
    health.running.store(false, Ordering::Release);
}

fn dispatch_available(
    config: &Arc<RunExecutorConfig>,
    semaphore: &Arc<Semaphore>,
    tasks: &mut tokio::task::JoinSet<RunId>,
    active: &mut BTreeSet<RunId>,
    progress: &broadcast::Sender<ProgressEvent>,
    health: &Arc<HealthState>,
) {
    while let Ok(permit) = Arc::clone(semaphore).try_acquire_owned() {
        let job = {
            let mut store = config
                .store
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            store
                .recoverable_runs(config.queue_capacity)
                .ok()
                .and_then(|jobs| {
                    jobs.into_iter()
                        .find(|job| !active.contains(&job.run.id))
                        .and_then(|job| {
                            store
                                .claim_recoverable_run(job.run.id, config.lease_duration)
                                .ok()
                                .flatten()
                        })
                })
                .or_else(|| store.claim_queued_run(config.lease_duration).ok().flatten())
        };
        let Some(job) = job else {
            break;
        };
        let config = Arc::clone(config);
        let progress = progress.clone();
        let health = Arc::clone(health);
        health.active.fetch_add(1, Ordering::Relaxed);
        active.insert(job.run.id);
        tasks.spawn(async move {
            let run_id = job.run.id;
            let owner = job.attempt.owner;
            let claim = job.claim;
            let result = execute_attempt(&config, job, progress.clone()).await;
            health.active.fetch_sub(1, Ordering::Relaxed);
            // A waiting exit keeps the run's workspace handle so its revision
            // epoch survives until resolution; every other exit releases it.
            if !matches!(result, Ok(AttemptExit::Waiting)) {
                config
                    .run_workspaces
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&run_id);
            }
            match result {
                Ok(AttemptExit::Completed) => {
                    health.completed.fetch_add(1, Ordering::Relaxed);
                }
                Ok(AttemptExit::Waiting) => {}
                Err(error) => {
                    health.failed.fetch_add(1, Ordering::Relaxed);
                    let failure = config.model_adapter.executor_failure(&error);
                    let _ = progress.send(ProgressEvent {
                        run_id,
                        attempt: owner,
                        event: AgentEvent::RunFailed {
                            message: failure.detail.clone(),
                        },
                    });
                    let job = config
                        .store
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .worker_run(run_id);
                    if let Ok(job) = job
                        && job.claim.same_lease(claim)
                        && job.attempt.owner == owner
                    {
                        if job.run.state == RunState::Cancelling {
                            let _ = cancel_attempt(&config, job);
                        } else {
                            let _ = fail_attempt(&config, job, failure);
                        }
                    }
                }
            }
            drop(permit);
            run_id
        });
    }
}

#[derive(Clone, Copy)]
enum AttemptExit {
    Completed,
    Waiting,
}

struct AttemptMcpRuntime {
    runtime: Arc<crate::protocols::mcp::transport::McpCapabilityRuntime>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    store: SharedWorkerStore,
}

impl AttemptMcpRuntime {
    fn start(
        runtime: Arc<crate::protocols::mcp::transport::McpCapabilityRuntime>,
        config: &RunExecutorConfig,
        cancellation: Arc<AtomicBool>,
        revision_live: Arc<AtomicBool>,
        workspace: crate::workspace::revision::ManagedWorkspace,
        workspace_revision: String,
    ) -> Result<Self, ExecutorError> {
        let (shutdown, _) = watch::channel(false);
        let mut tasks = Vec::new();
        for (server, generation) in runtime
            .refresh_registrations()
            .map_err(|error| ExecutorError::Worker(error.to_string()))?
        {
            let mut store = append_store(config)?;
            let runtime = Arc::clone(&runtime);
            let mut stopped = shutdown.subscribe();
            let cancellation = Arc::clone(&cancellation);
            tasks.push(tokio::spawn(async move {
                let result = runtime
                    .drive_refresh_owned(
                        &server,
                        generation,
                        crate::protocols::mcp::features::RefreshLimits::default(),
                        &mut store,
                        &mut stopped,
                    )
                    .await;
                if *stopped.borrow() {
                    return;
                }
                if result.is_ok()
                    && !matches!(runtime.refresh_is_current(&server, generation), Ok(true))
                {
                    return;
                }
                cancellation.store(true, Ordering::Release);
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    runtime.retire_and_close_owned(&server, generation, &mut store),
                )
                .await;
                let _ = result;
            }));
        }
        let monitor_runtime = Arc::clone(&runtime);
        let monitor_cancellation = Arc::clone(&cancellation);
        let mut stopped = shutdown.subscribe();
        tasks.push(tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = stopped.changed() => {
                        if result.is_err() || *stopped.borrow() {
                            return;
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                }
                let workspace = workspace.clone();
                let revision = tokio::time::timeout(
                    Duration::from_secs(2),
                    tokio::task::spawn_blocking(move || {
                        workspace
                            .current_revision()
                            .map(|revision| revision.id().to_string())
                    }),
                )
                .await;
                let current = match revision {
                    Ok(Ok(Ok(revision))) => revision,
                    _ => {
                        revision_live.store(false, Ordering::Release);
                        let _ = monitor_runtime.retire_for_revision_change();
                        monitor_cancellation.store(true, Ordering::Release);
                        return;
                    }
                };
                if current != workspace_revision {
                    revision_live.store(false, Ordering::Release);
                    let _ = monitor_runtime.retire_for_revision_change();
                    monitor_cancellation.store(true, Ordering::Release);
                    return;
                }
            }
        }));
        Ok(Self {
            runtime,
            shutdown,
            tasks,
            store: Arc::clone(&config.store),
        })
    }

    async fn shutdown(mut self) -> Result<(), ExecutorError> {
        let _ = self.shutdown.send(true);
        for mut task in self.tasks.drain(..) {
            if tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
        let mut append = self
            .store
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .worker_append_store()
            .map_err(|error| ExecutorError::Worker(error.to_string()))?;
        tokio::time::timeout(Duration::from_secs(5), self.runtime.shutdown(&mut append))
            .await
            .map_err(|_| ExecutorError::Worker("MCP runtime cleanup timed out".to_owned()))?
            .map_err(|error| ExecutorError::Worker(error.to_string()))
    }
}

impl Drop for AttemptMcpRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        for task in &self.tasks {
            task.abort();
        }
    }
}

struct ClaimHeartbeat {
    stop: Arc<(Mutex<bool>, Condvar)>,
    failure: watch::Receiver<Option<String>>,
    task: Option<std::thread::JoinHandle<()>>,
}

impl ClaimHeartbeat {
    fn start_with_authority(
        config: &RunExecutorConfig,
        claim: crate::api::service::AttemptDriverClaim,
        current_fence: Arc<AtomicU64>,
        current_claim_generation: Arc<AtomicU64>,
    ) -> Self {
        let database = config.database.clone();
        let lease_duration = config.lease_duration;
        let interval = config.claim_renewal_interval;
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let thread_stop = Arc::clone(&stop);
        let (failure_tx, failure) = watch::channel(None);
        #[cfg(debug_assertions)]
        let barrier = config.model_adapter.fake_barrier();
        let task = std::thread::spawn(move || {
            loop {
                let (stopped, wake) = &*thread_stop;
                let stopped = stopped.lock().unwrap_or_else(|error| error.into_inner());
                let (stopped, _) = wake
                    .wait_timeout_while(stopped, interval, |stopped| !*stopped)
                    .unwrap_or_else(|error| error.into_inner());
                if *stopped {
                    return;
                }
                drop(stopped);
                #[cfg(debug_assertions)]
                if let Some(barrier) = &barrier
                    && barrier.checkpoint == FakeBarrierCheckpoint::BeforeClaimRenewal
                {
                    std::fs::create_dir_all(&barrier.root)
                        .expect("create fake-provider barrier directory");
                    std::fs::write(
                        barrier.reached_path(),
                        FakeBarrierCheckpoint::BeforeClaimRenewal.as_str(),
                    )
                    .expect("publish fake-provider barrier");
                    while !barrier.release_path().exists() {
                        if *thread_stop
                            .0
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                        {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                }
                // Renew on a dedicated connection, never under the shared service
                // lock: a long tool operation holding that lock must not starve
                // the heartbeat into lease expiry and a mid-run fence-out.
                let renewed = crate::api::service::renew_driver_claim_standalone(
                    &database,
                    claim,
                    lease_duration,
                );
                match renewed {
                    Ok(renewed) => {
                        current_fence.store(renewed.fence.get(), Ordering::Release);
                        current_claim_generation.store(renewed.lease_version, Ordering::Release);
                    }
                    Err(error) => {
                        let _ = failure_tx.send(Some(error.to_string()));
                        return;
                    }
                }
            }
        });
        Self {
            stop,
            failure,
            task: Some(task),
        }
    }

    async fn failed(&mut self) -> ExecutorError {
        loop {
            if let Some(error) = self.failure.borrow().clone() {
                return ExecutorError::Worker(format!("claim heartbeat failed: {error}"));
            }
            if self.failure.changed().await.is_err() {
                return ExecutorError::Worker("claim heartbeat stopped unexpectedly".to_owned());
            }
        }
    }

    fn stop(mut self) -> Result<(), ExecutorError> {
        self.signal_stop();
        if let Some(task) = self.task.take() {
            task.join()
                .map_err(|_| ExecutorError::Worker("claim heartbeat panicked".to_owned()))?;
        }
        match self.failure.borrow().clone() {
            Some(error) => Err(ExecutorError::Worker(format!(
                "claim heartbeat failed: {error}"
            ))),
            None => Ok(()),
        }
    }

    fn signal_stop(&self) {
        let (stopped, wake) = &*self.stop;
        *stopped.lock().unwrap_or_else(|error| error.into_inner()) = true;
        wake.notify_one();
    }
}

impl Drop for ClaimHeartbeat {
    fn drop(&mut self) {
        self.signal_stop();
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

async fn while_claimed<T>(
    heartbeat: &mut ClaimHeartbeat,
    future: impl Future<Output = T>,
) -> Result<T, ExecutorError> {
    tokio::pin!(future);
    tokio::select! {
        biased;
        error = heartbeat.failed() => Err(error),
        value = &mut future => Ok(value),
    }
}

async fn wait_for_run_cancellation(config: &RunExecutorConfig, run_id: RunId) -> ExecutorError {
    loop {
        match load_job(config, run_id) {
            Ok(job) if job.run.state == RunState::Cancelling => {
                return ExecutorError::Worker("run cancelled during MCP bootstrap".to_owned());
            }
            Err(error) => return error,
            _ => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
}

async fn execute_attempt(
    config: &RunExecutorConfig,
    mut job: WorkerRun,
    progress: broadcast::Sender<ProgressEvent>,
) -> Result<AttemptExit, ExecutorError> {
    let current_fence = Arc::new(AtomicU64::new(job.claim.fence.get()));
    let current_claim_generation = Arc::new(AtomicU64::new(job.claim.lease_version));
    let mut heartbeat = ClaimHeartbeat::start_with_authority(
        config,
        job.claim,
        Arc::clone(&current_fence),
        Arc::clone(&current_claim_generation),
    );
    let started = Instant::now();
    let snapshot = RunConfigSnapshot::from_canonical_bytes(&job.effective_config)
        .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    if snapshot.run_id() != job.run.id
        || snapshot.project_id() != job.project_id
        || snapshot.principal_id() != job.principal_id
    {
        heartbeat.stop()?;
        return fail_attempt(
            config,
            job,
            config.model_adapter.failure(
                RunFailureCode::ExecutionFailed,
                "effective config identity mismatch",
            ),
        );
    }
    if config.telemetry.is_some() {
        flush_learning(config, job.project_id, job.run.id);
        if config
            .telemetry
            .as_ref()
            .is_some_and(|telemetry| !telemetry.learning_admission_ready())
        {
            heartbeat.stop()?;
            return Ok(AttemptExit::Waiting);
        }
    }
    config.model_adapter.select(snapshot.effective().provider)?;
    let root = config.project_root.clone();
    let authority_snapshot = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || {
            let root = std::fs::canonicalize(root).map_err(|error| {
                ExecutorError::Worker(format!("trusted project root unavailable: {error}"))
            })?;
            let workspace = crate::workspace::revision::ManagedWorkspace::open(&root)
                .map_err(|error| ExecutorError::Worker(error.to_string()))?;
            let revision = workspace
                .current_revision()
                .map_err(|error| ExecutorError::Worker(error.to_string()))?
                .id()
                .to_string();
            Ok::<_, ExecutorError>((root, revision, workspace))
        }),
    )
    .await
    .map_err(|_| ExecutorError::Worker("trusted project root scan timed out".to_owned()))?
    .map_err(|error| ExecutorError::Worker(format!("trusted project root scan: {error}")))??;
    config
        .run_workspaces
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(job.run.id, authority_snapshot.2.clone());
    let workspace = workspace_id(job.attempt.id)?;
    let runtime_secrets = runtime_secret_leases(config, &job, &snapshot, workspace)?;
    let prompt_revision = authority_snapshot.1.as_str();
    let prepared_prompt = prepare_prompt(
        config,
        &job,
        &snapshot,
        Some(prompt_revision),
        &runtime_secrets.custody,
    )?;
    let current_revision = authority_snapshot
        .2
        .current_revision()
        .map_err(|error| ExecutorError::Worker(error.to_string()))?
        .id()
        .to_string();
    if current_revision != authority_snapshot.1 {
        heartbeat.stop()?;
        return Err(ExecutorError::Worker(
            "workspace revision changed while compiling the authoritative prompt".to_owned(),
        ));
    }
    let authority_revision = authority_snapshot.1.as_str();
    config.scheduler.register_run_with_snapshot(
        job.run.id,
        job.principal_id,
        &job.start_idempotency_key,
        &snapshot,
    )?;
    if matches!(
        job.run.state,
        RunState::Queued | RunState::AcquiringWorkspace
    ) {
        config.scheduler.admit_run(job.run.id)?;
    }
    job = prepare_attempt(config, job)?;
    SqliteCancellationCoordinator::new(&config.database)
        .register_no_process(job.attempt.owner)
        .map_err(|error| ExecutorError::Worker(error.to_string()))?;

    if job.run.state == RunState::Cancelling {
        append_record(config, &job, "cancel", LoopRecord::CancellationRequested)?;
        heartbeat.stop()?;
        return cancel_attempt(config, job);
    }

    let append = append_store(config)?;
    let committed = append.events().map_err(ExecutorError::Store)?;
    let recovery = match RestartProjection::reconstruct(&job.attempt, &committed) {
        Ok(recovery) => recovery,
        Err(crate::agent::driver::restart::RestartError::MissingBoundary) => {
            seed_boundary(config, &job, &prepared_prompt)?;
            let committed = append_store(config)?
                .events()
                .map_err(ExecutorError::Store)?;
            RestartProjection::reconstruct(&job.attempt, &committed)?
        }
        Err(error) => return Err(error.into()),
    };
    let plan = match recovery {
        RecoveryState::Ready(plan) => plan,
        RecoveryState::Waiting(waiting) => {
            heartbeat.stop()?;
            ensure_waiting(config, job.run.id, &waiting.waiting)?;
            return Ok(AttemptExit::Waiting);
        }
        RecoveryState::Cancelled(_) => {
            settle_cancelled_learning(config, &job)?;
            heartbeat.stop()?;
            return cancel_attempt(config, job);
        }
        RecoveryState::OutcomeUnknown(_) => {
            settle_learning(
                config,
                &job,
                crate::telemetry::tool_learning::LearningStatus::OutcomeUnknown,
            )?;
            heartbeat.stop()?;
            return fail_attempt(
                config,
                job,
                config.model_adapter.failure(
                    RunFailureCode::ExecutionFailed,
                    "dispatched model outcome is unknown",
                ),
            );
        }
    };

    if plan.snapshot.boundary == SafeBoundary::TurnEnd && job.run.output.is_some() {
        heartbeat.stop()?;
        finish_attempt(config, job)?;
        return Ok(AttemptExit::Completed);
    }

    if let Some(scope) = config
        .model_adapter
        .auth_scope(snapshot.effective().provider)
        .filter(|_| job.run.auth_granted != Some(true))
    {
        let record = crate::agent::providers::interrupt::auth_waiting_record(
            &job.attempt,
            job.run.id,
            scope,
            plan.snapshot.clone(),
        )?;
        let LoopRecord::Waiting(waiting) = &record else {
            unreachable!("auth waiting helper always returns a waiting record")
        };
        let waiting = waiting.clone();
        append_hashed_record(config, &job, "waiting-auth", record)?;
        heartbeat.stop()?;
        ensure_waiting(config, job.run.id, &waiting)?;
        append_store(config)?
            .quiesce_driver_claim(job.claim)
            .map_err(ExecutorError::Store)?;
        return Ok(AttemptExit::Waiting);
    }

    let live_cancellation = Arc::new(AtomicBool::new(false));
    let revision_live = Arc::new(AtomicBool::new(true));
    let budget = durable_tool_budget(config, &snapshot)?;
    let sampling_policies = config
        .mcp_servers
        .iter()
        .filter_map(|server| {
            server
                .responders
                .sampling
                .clone()
                .map(|policy| (server.id.clone(), policy))
        })
        .collect::<BTreeMap<_, _>>();
    let mut responder_outcomes = config.mcp_responder_outcomes.clone();
    for (server_id, scope) in &runtime_secrets.scopes {
        responder_outcomes = responder_outcomes
            .with_secret_scope(
                &config.callback_secrets,
                job.principal_id,
                job.project_id,
                job.run.id,
                job.attempt.id,
                server_id,
                &scope.authorized_handles,
                &scope.secrets,
            )
            .map_err(ExecutorError::Worker)?;
    }
    if !sampling_policies.is_empty() {
        let provider = snapshot.effective().provider;
        let model = config.model_adapter.model_name(provider).to_owned();
        if sampling_policies.values().any(|policy| {
            !config
                .model_adapter
                .sampling_dispatch_proven(provider, policy)
        }) {
            return Err(ExecutorError::Worker(
                "MCP sampling requires proven USD provider pricing and an output-token cap"
                    .to_owned(),
            ));
        }
        if sampling_policies
            .values()
            .any(|policy| policy.model_id != model)
        {
            return Err(ExecutorError::Worker(
                "MCP sampling model assertion differs from the run's selected model".to_owned(),
            ));
        }
        responder_outcomes = responder_outcomes.with_sampling(Arc::new(DurableSamplingOutcome {
            database: config.database.clone(),
            store: Arc::clone(&config.store),
            scheduler: config.scheduler.clone(),
            selector: config.model_adapter.clone(),
            provider,
            security: model_security(&job, &snapshot, workspace)?,
            occurred_at: job.occurred_at.clone(),
            model,
            provider_cap: config.model_adapter.max_output_tokens(provider),
            run_cap: u32::try_from(snapshot.effective().max_tokens).unwrap_or(u32::MAX),
            secrets: runtime_secrets
                .scopes
                .iter()
                .map(|(server, scope)| (server.clone(), scope.secrets.clone()))
                .collect(),
            policies: sampling_policies,
            workspace_revision: authority_revision.to_owned(),
            artifact_retention_days: snapshot.effective().artifact_retention_days,
        }));
    }
    let mut mcp_runtime = None;
    let mut mcp_attempt_runtime = None;
    if !config.mcp_servers.is_empty() {
        let authenticated = AuthenticatedPrincipal::from_grants(GrantSnapshot::new(
            job.principal_id,
            job.project_id,
            snapshot.effective_authority().iter().copied(),
        ));
        let mut store = append_store(config)?;
        let mut resolved_auth = BTreeMap::new();
        for server in &config.mcp_servers {
            if let Some(resolved) = crate::agent::driver::restart::resolved_mcp_bootstrap_auth(
                &store, job.run.id, &server.id,
            )
            .map_err(ExecutorError::Store)?
            {
                resolved_auth.insert(server.id.clone(), resolved);
            }
        }
        let callback_secrets = runtime_secrets
            .scopes
            .iter()
            .map(|(server, scope)| (server.clone(), scope.secrets.clone()))
            .collect::<BTreeMap<_, _>>();
        let bootstrap_context = crate::protocols::mcp::config::McpBootstrapContext {
            authenticated: &authenticated,
            config: &snapshot,
            workspace_id: workspace,
            workspace_revision: authority_revision,
            project_root: authority_snapshot.0.as_path(),
            attempt: job.attempt.owner,
            claim: job.claim,
            current_fence: Arc::clone(&current_fence),
            current_claim_generation: Arc::clone(&current_claim_generation),
            revision_live: Arc::clone(&revision_live),
            cancellation: Arc::clone(&live_cancellation),
            budget: Arc::clone(&budget),
            scheduler: config.scheduler.clone(),
            responder_outcomes: &responder_outcomes,
            callback_database: &config.database,
            artifacts: Arc::clone(&config.artifacts),
            claim_verifier: {
                let worker_store = Arc::clone(&config.store);
                Arc::new(move |claim| {
                    worker_store
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .worker_append_store()
                        .and_then(|mut store| {
                            store
                                .verify_driver_claim(claim)
                                .map_err(|error| ServiceError::Store(error.to_string()))
                        })
                        .is_ok()
                })
            },
            occurred_at: &job.occurred_at,
            resolved_auth: &resolved_auth,
            stdio_profiles: config.mcp_stdio_profiles.as_deref(),
            resolved_secrets: &runtime_secrets.resolved,
            callback_secrets: &callback_secrets,
            custody: runtime_secrets.custody.clone(),
            extension_registry: &config.capability_extensions,
        };
        let (bootstrap, claim_error) = {
            let bootstrap = crate::protocols::mcp::config::bootstrap(
                &config.mcp_servers,
                &bootstrap_context,
                &mut store,
            );
            tokio::pin!(bootstrap);
            tokio::select! {
                biased;
                error = heartbeat.failed() => {
                    live_cancellation.store(true, Ordering::Release);
                    (bootstrap.await, Some(error))
                }
                error = wait_for_run_cancellation(config, job.run.id) => {
                    live_cancellation.store(true, Ordering::Release);
                    (bootstrap.await, Some(error))
                }
                result = &mut bootstrap => (result, None),
            }
        };
        if let Some(error) = claim_error {
            match bootstrap {
                Ok(crate::protocols::mcp::config::McpBootstrapOutcome::Ready(runtime)) => {
                    if let Err(cleanup) = runtime.shutdown(&mut store).await {
                        return Err(ExecutorError::Worker(format!(
                            "{error}; MCP bootstrap cleanup: {cleanup}"
                        )));
                    }
                }
                Ok(crate::protocols::mcp::config::McpBootstrapOutcome::AuthRequired(_)) => {}
                Err(cleanup) => {
                    return Err(ExecutorError::Worker(format!(
                        "{error}; MCP bootstrap cleanup: {cleanup}"
                    )));
                }
            }
            return Err(error);
        }
        let bootstrap = bootstrap.map_err(ExecutorError::McpBootstrap)?;
        match bootstrap {
            crate::protocols::mcp::config::McpBootstrapOutcome::Ready(runtime) => {
                let owned = AttemptMcpRuntime::start(
                    Arc::clone(&runtime),
                    config,
                    Arc::clone(&live_cancellation),
                    Arc::clone(&revision_live),
                    authority_snapshot.2.clone(),
                    authority_revision.to_owned(),
                )?;
                mcp_runtime = Some(runtime);
                mcp_attempt_runtime = Some(owned);
            }
            crate::protocols::mcp::config::McpBootstrapOutcome::AuthRequired(challenge) => {
                let record = crate::agent::providers::interrupt::challenge_auth_waiting_record(
                    &job.attempt,
                    job.run.id,
                    &challenge,
                    plan.snapshot.clone(),
                )?;
                let LoopRecord::Waiting(waiting) = &record else {
                    unreachable!("auth waiting helper always returns a waiting record")
                };
                let waiting = waiting.clone();
                append_hashed_record(config, &job, "waiting-mcp-bootstrap-auth", record)?;
                heartbeat.stop()?;
                ensure_waiting(config, job.run.id, &waiting)?;
                append_store(config)?
                    .quiesce_driver_claim(job.claim)
                    .map_err(ExecutorError::Store)?;
                return Ok(AttemptExit::Waiting);
            }
        }
    }
    let attempt_result = async {
    let (tool, native_revision) = tool_adapter(
        config,
        &job,
        &snapshot,
        true,
        mcp_runtime.as_ref().map(|runtime| {
            (
                runtime,
                authority_revision,
                runtime_secrets.custody.clone(),
            )
        }),
        Arc::clone(&live_cancellation),
        budget,
    )?;

    let transcript = TranscriptCapture::new(&plan.snapshot)?;
    if snapshot.effective().grammar_edit.enabled && config.edit_workspace.is_none() {
        heartbeat.stop()?;
        return fail_attempt(
            config,
            job,
            config.model_adapter.failure(
                RunFailureCode::ExecutionFailed,
                "grammar edit workspace integration is unavailable",
            ),
        );
    }
    if config.edit_workspace.is_some()
        && ![
            crate::domain::config::Grant::ModelCall,
            crate::domain::config::Grant::WorkspaceWrite,
        ]
        .into_iter()
        .all(|grant| snapshot.effective_authority().contains(&grant))
    {
        heartbeat.stop()?;
        return fail_attempt(
            config,
            job,
            config.model_adapter.failure(
                RunFailureCode::ExecutionFailed,
                "edit execution requires model-call and workspace-write grants",
            ),
        );
    }
    let grammar_context = config
        .edit_workspace
        .as_ref()
        .map(|root| GrammarEditContext::open(root, GrammarEditLimits::default().edit))
        .transpose()
        .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let model = model_adapter(
        config,
        &job,
        snapshot.clone(),
        grammar_context.clone(),
        native_revision,
    )?;
    let observer = ProgressObserver {
        run_id: job.run.id,
        attempt: job.attempt.owner,
        claim: job.claim,
        store: Arc::clone(&config.store),
        sender: progress,
        error: Arc::new(Mutex::new(None)),
    };
    let observer_errors = Arc::clone(&observer.error);
    let driver_store = append_store(config)?;
    let driver = while_claimed(
        &mut heartbeat,
        plan.start_claimed(&job.attempt, job.claim, driver_store, model, |builder| {
            builder
                .observer(observer)
                .transcript_observer(transcript.clone())
                .tool_executor(tool)
        }),
    )
    .await;
    let mut driver = match driver {
        Ok(Ok(driver)) => driver,
        Ok(Err(error)) => {
            heartbeat.stop()?;
            return Err(error.into());
        }
        Err(error) => {
            heartbeat.stop()?;
            return Err(error);
        }
    };

    enum DriverExit {
        Completed(Box<TurnResult>),
        Cancelled,
        Failed(&'static str),
        Waiting {
            waiting: Box<crate::agent::driver::waiting::WaitingState>,
            target: RunState,
        },
    }

    let drive_result: Result<DriverExit, ExecutorError> = async {
        let mut cancellation_committed = false;
        loop {
            enum Tick {
                Step(Box<Result<LoopStep, crate::agent::driver::attempt::PollError>>),
                CheckCancellation,
            }
            let tick = {
                let poll = driver.poll(&job.attempt);
                tokio::pin!(poll);
                loop {
                    tokio::select! {
                        biased;
                        error = heartbeat.failed() => return Err(error),
                        result = &mut poll => break Tick::Step(Box::new(result)),
                        _ = tokio::time::sleep(config.poll_interval) => {
                            if live_cancellation.load(Ordering::Acquire) {
                                let _ = config
                                    .cancellation_coordinator
                                    .cancel_attempt(job.attempt.owner);
                                break Tick::CheckCancellation;
                            }
                            if load_job(config, job.run.id)?.run.state == RunState::Cancelling {
                                break Tick::CheckCancellation;
                            }
                        }
                    }
                }
            };
            let step = match tick {
                Tick::Step(result) => (*result)?,
                Tick::CheckCancellation => {
                    if !cancellation_committed {
                        let mut journal = append_store(config)?;
                        let append =
                            journal_append(&job, "cancel", LoopRecord::CancellationRequested)?;
                        while_claimed(
                            &mut heartbeat,
                            driver.commit_cancellation(&job.attempt, || {
                                journal.append_effect(append)
                            }),
                        )
                        .await?
                        .map_err(|error| ExecutorError::Worker(error.to_string()))?;
                        cancellation_committed = true;
                    }
                    continue;
                }
            };
            #[cfg(debug_assertions)]
            if matches!(&step, LoopStep::Finished(_))
                && let Some(barrier) = config.model_adapter.fake_barrier()
            {
                while_claimed(
                    &mut heartbeat,
                    barrier.wait_async(FakeBarrierCheckpoint::AfterModelOutcome),
                )
                .await?;
            }
            if let Some(error) = observer_errors
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                return Err(ExecutorError::Worker(error));
            }
            match step {
                LoopStep::Finished(result) => {
                    if result.finish_reason == FinishReason::Cancelled || cancellation_committed {
                        return Ok(DriverExit::Cancelled);
                    }
                    if let Some(outcome) = result
                        .metadata
                        .get(crate::agent::adapters::grammar_edit::GRAMMAR_EDIT_OUTCOME_METADATA)
                    {
                        let outcome: crate::agent::adapters::grammar_edit::GrammarEditOutcomeEvidence =
                            serde_json::from_value(outcome.clone()).map_err(|error| {
                                ExecutorError::Worker(format!(
                                    "invalid grammar edit outcome evidence: {error}"
                                ))
                            })?;
                        if outcome.result != "accepted" {
                            return Ok(DriverExit::Failed("grammar edit output was rejected"));
                        }
                    }
                    if let Some(context) = &grammar_context {
                        execute_grammar_edit(config, &snapshot, &result, context)?;
                    }
                    let committed = append_store(config)?
                        .events()
                        .map_err(ExecutorError::Store)?;
                    let snapshot = match RestartProjection::reconstruct(&job.attempt, &committed)? {
                        RecoveryState::Ready(plan) => plan.snapshot,
                        RecoveryState::OutcomeUnknown(_) => {
                            return Ok(DriverExit::Failed("model outcome became unknown"));
                        }
                        _ => {
                            return Err(ExecutorError::Worker(
                                "model did not reach a committed boundary".to_owned(),
                            ));
                        }
                    };
                    append_hashed_record(
                        config,
                        &job,
                        "turn-end",
                        LoopRecord::Boundary(BoundarySnapshot {
                            boundary: SafeBoundary::TurnEnd,
                            transcript: snapshot.transcript,
                            resume_index: None,
                            model_outcome: None,
                        }),
                    )?;
                    if result
                        .metadata
                        .get("kit.fake.await_input")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                    {
                        continue;
                    }
                    return Ok(DriverExit::Completed(Box::new(result)));
                }
                LoopStep::Interrupt(LoopInterrupt::AfterToolResult(round)) => {
                    let snapshot = transcript.after_tool_snapshot(round.transcript_len)?;
                    append_hashed_record(
                        config,
                        &job,
                        "after-tool",
                        LoopRecord::Boundary(snapshot),
                    )?;
                    #[cfg(debug_assertions)]
                    if let Some(barrier) = config.model_adapter.fake_barrier() {
                        while_claimed(
                            &mut heartbeat,
                            barrier.wait_async(FakeBarrierCheckpoint::AfterToolOutcome),
                        )
                        .await?;
                    }
                    continue;
                }
                LoopStep::Interrupt(interrupt) => {
                    let committed = append_store(config)?
                        .events()
                        .map_err(ExecutorError::Store)?;
                    let snapshot = match RestartProjection::reconstruct(&job.attempt, &committed)? {
                        RecoveryState::Ready(plan) => plan.snapshot,
                        _ => {
                            return Err(ExecutorError::Worker(
                                "interrupt has no safe boundary".to_owned(),
                            ));
                        }
                    };
                    let record = crate::agent::providers::interrupt::waiting_record(
                        interrupt.clone(),
                        &job.attempt,
                        snapshot,
                    )?;
                    append_hashed_record(config, &job, "waiting", record)?;
                    let target = match interrupt {
                        LoopInterrupt::AwaitingInput(_) => RunState::WaitingForInput,
                        LoopInterrupt::ApprovalRequest(_) => RunState::WaitingForApproval,
                        LoopInterrupt::AfterToolResult(_) => unreachable!(),
                    };
                    let waiting = match RestartProjection::reconstruct(
                        &job.attempt,
                        &append_store(config)?
                            .events()
                            .map_err(ExecutorError::Store)?,
                    )? {
                        RecoveryState::Waiting(waiting) => waiting.waiting,
                        _ => {
                            return Err(ExecutorError::Worker(
                                "waiting interrupt was not durably reconstructable".to_owned(),
                            ));
                        }
                    };
                    return Ok(DriverExit::Waiting {
                        waiting: Box::new(waiting),
                        target,
                    });
                }
            }
        }
    }
    .await;

    heartbeat.stop()?;
    match drive_result? {
        DriverExit::Completed(result) => {
            complete_attempt(
                config,
                load_job(config, job.run.id)?,
                &prepared_prompt,
                &result.items,
                started.elapsed(),
            )?;
            let terminal = load_job(config, job.run.id)?;
            driver
                .revoke(&terminal.attempt)
                .await
                .map_err(|error| ExecutorError::Worker(error.to_string()))?;
            Ok(AttemptExit::Completed)
        }
        DriverExit::Cancelled => {
            let exit = cancel_attempt(config, load_job(config, job.run.id)?)?;
            if matches!(exit, AttemptExit::Completed) {
                let terminal = load_job(config, job.run.id)?;
                driver
                    .revoke(&terminal.attempt)
                    .await
                    .map_err(|error| ExecutorError::Worker(error.to_string()))?;
            }
            Ok(exit)
        }
        DriverExit::Failed(reason) => fail_attempt(
            config,
            job,
            config
                .model_adapter
                .failure(RunFailureCode::ExecutionFailed, reason),
        ),
        DriverExit::Waiting { waiting, target } => {
            ensure_waiting(config, job.run.id, &waiting)?;
            debug_assert_eq!(load_job(config, job.run.id)?.run.state, target);
            driver
                .suspend(&job.attempt)
                .await
                .map_err(|error| ExecutorError::Worker(error.to_string()))?;
            Ok(AttemptExit::Waiting)
        }
    }
    }
    .await;
    if let Some(runtime) = mcp_attempt_runtime
        && let Err(cleanup) = runtime.shutdown().await
    {
        return match attempt_result {
            Ok(_) => Err(cleanup),
            Err(primary) => Err(ExecutorError::Worker(format!(
                "{primary}; MCP attempt cleanup: {cleanup}"
            ))),
        };
    }
    attempt_result
}

fn settle_cancelled_learning(
    config: &RunExecutorConfig,
    job: &WorkerRun,
) -> Result<(), ExecutorError> {
    if config.tool_learning_key.is_none() {
        return Ok(());
    }
    let denied = effect_records(&append_store(config)?, job.attempt.owner)
        .map_err(|error| ExecutorError::Worker(error.to_string()))?
        .iter()
        .any(|record| matches!(
            record,
            LoopRecord::WaitingResolved(resolved)
                if matches!(
                    resolved.resolution,
                    crate::agent::driver::waiting::WaitingResolution::Approval {
                        decision: crate::domain::events::ApprovalDecision::Denied
                    } | crate::agent::driver::waiting::WaitingResolution::Auth { granted: false }
                )
        ));
    settle_learning(
        config,
        job,
        if denied {
            crate::telemetry::tool_learning::LearningStatus::Failed
        } else {
            crate::telemetry::tool_learning::LearningStatus::Cancelled
        },
    )
}

fn settle_learning(
    config: &RunExecutorConfig,
    job: &WorkerRun,
    status: crate::telemetry::tool_learning::LearningStatus,
) -> Result<(), ExecutorError> {
    let Some(key) = config.tool_learning_key else {
        return Ok(());
    };
    let hasher = crate::telemetry::tool_learning::ProjectPointerHasher::new(job.project_id, &key);
    let result = crate::telemetry::tool_learning::settle_unresolved_continuations(
        &mut append_store(config)?,
        job.attempt.owner,
        job.claim,
        &hasher,
        job.run.id,
        UtcDateTime::now().map_err(|error| ExecutorError::Worker(error.to_string()))?,
        TraceId::parse("tool-learning-continuation")
            .expect("tool-learning continuation trace ID is valid"),
        status,
    );
    if let Err(error) = result
        && let Some(telemetry) = &config.telemetry
    {
        telemetry.mark_learning_failure(error.to_string());
    }
    Ok(())
}

#[derive(Clone)]
struct TranscriptCapture(Arc<Mutex<Vec<Item>>>);

impl TranscriptCapture {
    fn new(snapshot: &BoundarySnapshot) -> Result<Self, ExecutorError> {
        let mut transcript = snapshot.normalized_transcript();
        let retained = if let Some(outcome) = &snapshot.model_outcome {
            transcript
                .len()
                .checked_sub(outcome.output_items.len())
                .ok_or_else(|| {
                    ExecutorError::Worker("invalid model outcome transcript".to_owned())
                })?
        } else {
            transcript.len()
        };
        transcript.truncate(snapshot.resume_index.unwrap_or(retained).min(retained));
        Ok(Self(Arc::new(Mutex::new(transcript))))
    }

    fn after_tool_snapshot(&self, expected_len: usize) -> Result<BoundarySnapshot, ExecutorError> {
        let transcript = self
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if transcript.len() != expected_len {
            return Err(ExecutorError::Worker(
                "captured tool transcript length does not match AgentKit".to_owned(),
            ));
        }
        let resume_index = transcript
            .iter()
            .rposition(|item| item.kind != ItemKind::Tool)
            .map_or(0, |index| index + 1);
        if resume_index >= transcript.len() {
            return Err(ExecutorError::Worker(
                "tool boundary has no tool result".to_owned(),
            ));
        }
        Ok(BoundarySnapshot {
            boundary: SafeBoundary::AfterToolOutcome,
            transcript: transcript.iter().map(from_agentkit_item).collect(),
            resume_index: Some(resume_index),
            model_outcome: None,
        })
    }
}

impl TranscriptObserver for TranscriptCapture {
    fn on_transcript_event(&self, event: TranscriptEvent<'_>) {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(event.item.clone());
    }
}

fn prepare_attempt(
    config: &RunExecutorConfig,
    mut job: WorkerRun,
) -> Result<WorkerRun, ExecutorError> {
    for target in [
        RunState::AcquiringWorkspace,
        RunState::Starting,
        RunState::Running,
    ] {
        if job.run.state == target {
            continue;
        }
        if job.run.state.can_transition_to(target) {
            job = transition_run(config, job.run.id, target)?;
        }
    }
    if job.attempt.state == AttemptState::Leased {
        job = transition_attempt(config, job.attempt.id, AttemptState::Executing)?;
    }
    Ok(job)
}

struct PreparedPrompt {
    compiled: crate::agent::prompt::CompiledPrompt,
    context: ContextProjection,
}

fn prepare_prompt(
    config: &RunExecutorConfig,
    job: &WorkerRun,
    snapshot: &RunConfigSnapshot,
    native_revision: Option<&str>,
    custody: &SecretCustody,
) -> Result<PreparedPrompt, ExecutorError> {
    let digest = ArtifactDigest::parse(job.run.input.as_str())
        .map_err(|error| ExecutorError::Artifact(error.to_string()))?;
    let artifact = config
        .artifacts
        .open_verified(digest)
        .map_err(|error| ExecutorError::Artifact(error.to_string()))?;
    if artifact.manifest().principal != job.principal_id.to_string()
        || artifact.manifest().project != job.project_id.to_string()
    {
        return Err(ExecutorError::Artifact(
            "prompt artifact ownership mismatch".to_owned(),
        ));
    }
    let prompt = String::from_utf8(
        config
            .artifacts
            .open_bytes(digest)
            .map_err(|error| ExecutorError::Artifact(error.to_string()))?,
    )
    .map_err(|_| ExecutorError::Artifact("prompt artifact is not UTF-8".to_owned()))?;
    let repository_instructions = native_revision
        .map(|revision| BTreeMap::from([("current_revision".to_owned(), revision.to_owned())]))
        .unwrap_or_default();
    let mut prompt_input = PromptInput {
        experiment: Some(crate::agent::prompt::PromptExperiment {
            identity: crate::domain::config::GRAMMAR_EDIT_EXPERIMENT_ID.to_owned(),
            digest: snapshot.grammar_edit_experiment_digest(),
            enabled: snapshot.effective().grammar_edit.enabled,
        }),
        task: TaskContract {
            goal: prompt.clone(),
            risk_class: "executor".to_owned(),
            ..TaskContract::default()
        },
        repository_instructions,
        ..PromptInput::default()
    };
    prompt_input.task.goal = custody.project_text_references(
        crate::telemetry::redact::CaptureBoundary::Prompt,
        &prompt_input.task.goal,
    );
    let prompt_input = project_composition_input(custody, &prompt_input)?;
    let compiled = compile(&prompt_input)?;
    let token_budget = usize::try_from(snapshot.effective().max_tokens).unwrap_or(usize::MAX);
    let context = project_canonical_prompt(
        job.run.input.as_str(),
        compiled.template_version,
        compiled.text(),
        token_budget,
    )
    .map_err(|_| ExecutorError::Worker("canonical prompt exceeds model token budget".to_owned()))?;
    Ok(PreparedPrompt { compiled, context })
}

pub(crate) fn project_composition_input(
    custody: &SecretCustody,
    input: &PromptInput,
) -> Result<PromptInput, ExecutorError> {
    let composed = custody.project_json(
        crate::telemetry::redact::CaptureBoundary::CompositionInput,
        &serde_json::to_value(input).map_err(|error| ExecutorError::Worker(error.to_string()))?,
    );
    serde_json::from_value(composed).map_err(|error| {
        ExecutorError::Worker(format!("composed prompt projection failed: {error}"))
    })
}

fn seed_boundary(
    config: &RunExecutorConfig,
    job: &WorkerRun,
    prepared: &PreparedPrompt,
) -> Result<(), ExecutorError> {
    let prompt = RunPromptProjection {
        template_version: Some(prepared.compiled.template_version.to_owned()),
        prompt_digest: Some(prepared.compiled.full_digest.clone()),
        stable_prefix_digest: Some(prepared.compiled.stable_digest.clone()),
        first_dynamic_byte: Some(
            u64::try_from(prepared.compiled.first_dynamic_offset)
                .map_err(|_| ExecutorError::Worker("prompt offset overflowed".to_owned()))?,
        ),
        context_bytes: Some(
            u64::try_from(prepared.compiled.bytes.len())
                .map_err(|_| ExecutorError::Worker("prompt size overflowed".to_owned()))?,
        ),
        estimated_tokens: Some(
            u64::try_from(prepared.context.estimated_tokens)
                .map_err(|_| ExecutorError::Worker("prompt estimate overflowed".to_owned()))?,
        ),
        token_budget: Some(
            u64::try_from(prepared.context.token_budget)
                .map_err(|_| ExecutorError::Worker("prompt budget overflowed".to_owned()))?,
        ),
    };
    config
        .store
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .publish_run_prompt(job.run.id, job.claim, prompt)?;
    let input = Item::text(ItemKind::User, prepared.compiled.text());
    append_record(
        config,
        job,
        "initial-boundary",
        LoopRecord::Boundary(BoundarySnapshot {
            boundary: SafeBoundary::BeforeModelDispatch,
            transcript: vec![from_agentkit_item(&input)],
            resume_index: Some(0),
            model_outcome: None,
        }),
    )
}

#[derive(Clone)]
struct DurableSamplingOutcome {
    database: PathBuf,
    store: SharedWorkerStore,
    scheduler: DurableScheduler,
    selector: SelectedModelAdapter,
    provider: ConfigProvider,
    security: ModelSecurity,
    occurred_at: crate::domain::events::UtcDateTime,
    model: String,
    provider_cap: u32,
    run_cap: u32,
    secrets: BTreeMap<String, Vec<Arc<SecretLease>>>,
    policies: BTreeMap<String, crate::protocols::mcp::config::McpSamplingResponderConfig>,
    workspace_revision: String,
    artifact_retention_days: u32,
}

#[cfg(debug_assertions)]
#[allow(clippy::too_many_arguments)]
pub fn durable_sampling_outcomes_for_test(
    database: &std::path::Path,
    store: SharedWorkerStore,
    scheduler: DurableScheduler,
    provider: Arc<FakeProvider>,
    security: ModelSecurity,
    server_id: &str,
    policy: crate::protocols::mcp::config::McpSamplingResponderConfig,
    secrets: Vec<Arc<SecretLease>>,
) -> crate::protocols::mcp::responders::ResponderOutcomes {
    let selected = security.config.effective().provider;
    let registry = crate::protocols::mcp::responders::CallbackSecretRegistry::default();
    crate::protocols::mcp::responders::ResponderOutcomes::default()
        .with_secret_scope(
            &registry,
            security.attempt.principal_id,
            security.config.project_id(),
            security.config.run_id(),
            security.attempt.attempt_id,
            server_id,
            ["provider:test", "http:test", "stdio:test", "callback:test"],
            &secrets,
        )
        .expect("test callback secret scope is unique")
        .with_sampling(Arc::new(DurableSamplingOutcome {
            database: database.to_owned(),
            store,
            scheduler,
            selector: SelectedModelAdapter::for_test(selected, provider),
            provider: selected,
            security,
            occurred_at: crate::domain::events::UtcDateTime::parse("2026-08-04T12:00:00Z")
                .expect("static test timestamp is valid"),
            model: "fake-deterministic-v1".to_owned(),
            provider_cap: 64,
            run_cap: 64,
            secrets: BTreeMap::from([(server_id.to_owned(), secrets)]),
            policies: BTreeMap::from([(server_id.to_owned(), policy)]),
            workspace_revision: "adversarial-revision".to_owned(),
            artifact_retention_days: 1,
        }))
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[async_trait]
impl crate::protocols::mcp::responders::SamplingOutcomeHandler for DurableSamplingOutcome {
    fn max_output_tokens(&self) -> u32 {
        self.provider_cap.min(self.run_cap)
    }

    async fn respond(
        &self,
        mut request: crate::protocols::mcp::responders::ValidatedSamplingRequest,
        context: crate::protocols::mcp::responders::CallbackAuthorityContext,
    ) -> Result<
        crate::protocols::mcp::responders::SamplingHandlerOutput,
        crate::protocols::mcp::responders::ResponderError,
    > {
        let policy = self
            .policies
            .get(context.server_id())
            .ok_or(crate::protocols::mcp::responders::ResponderError::Authority)?;
        if policy.model_id != self.model {
            return Err(crate::protocols::mcp::responders::ResponderError::Authority);
        }
        let secrets = self
            .secrets
            .get(context.server_id())
            .ok_or(crate::protocols::mcp::responders::ResponderError::Authority)?;
        let request_digest = context.request_digest();
        if matches!(
            policy.approval,
            crate::protocols::mcp::config::McpSamplingApprovalMode::RequestOnly
                | crate::protocols::mcp::config::McpSamplingApprovalMode::RequestAndResponse
        ) {
            self.approve(
                &context,
                crate::domain::mcp_callback::McpCallbackMode::SamplingRequest,
                request_digest,
                serde_json::json!({
                    "stage": "request",
                    "digest": request_digest.to_string(),
                    "request": request.params(),
                }),
                policy.timeout_millis,
            )
            .await?;
        }

        context.revalidate().await?;
        let totals = self
            .scheduler
            .totals(self.security.config.run_id())
            .map_err(|_| crate::protocols::mcp::responders::ResponderError::Unavailable)?;
        let remaining = RunBudget::from_effective_config(self.security.config.effective())
            .remaining(totals.committed, totals.reserved);
        let (input_tokens, maximum, cost_budget) =
            crate::protocols::mcp::responders::sampling_affordable_output_tokens(
                request.params(),
                policy,
                self.provider_cap.min(self.run_cap),
                remaining,
            )?;
        if remaining.turns() == 0 {
            return Err(crate::protocols::mcp::responders::ResponderError::Unavailable);
        }
        request.set_max_output_tokens(maximum);
        let durable_identity = sampling_turn_identity(&self.security, &context, request_digest);
        let turn_request = detached_sampling_turn(&request, &durable_identity)?;

        context.consume_dispatch_permit()?;
        let adapter = self
            .selector
            .select(self.provider)
            .map_err(|_| crate::protocols::mcp::responders::ResponderError::Unavailable)?;
        let mut security = self.security.clone();
        security.argument_constraints = ArgumentConstraints::new([format!(
            "mcp_sampling:{}:{}:{}:{}",
            context.server_id(),
            context.generation(),
            context.operation_sequence(),
            request_digest
        )
        .into_bytes()]);
        let validation_policy = policy.clone();
        let validation_model = self.model.clone();
        let validation_secrets = secrets.clone();
        let validation_request_id = context.protocol_request_id();
        let validation_max_tokens = request.params().max_tokens;
        let durable = DurableModelAdapter::new(
            adapter,
            self.store
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .worker_append_store()
                .map_err(|_| crate::protocols::mcp::responders::ResponderError::Unavailable)?,
            self.scheduler.clone(),
            security,
            ModelPolicy {
                reservation: Spend::new(
                    cost_budget,
                    input_tokens.saturating_add(u64::from(maximum)),
                    1,
                    0,
                    0,
                ),
                provider_idempotency: if self.selector.provider_idempotency_enforced(self.provider)
                {
                    ProviderIdempotency::Enforced
                } else {
                    ProviderIdempotency::Unproven
                },
                detached: true,
                ..ModelPolicy::default()
            },
            self.occurred_at.clone(),
            TraceId::parse("mcp-sampling").expect("sampling trace id is valid"),
        )
        .with_outcome_validator(Arc::new(move |result| {
            let redactor = CanaryRedactor::new([]).with_secrets(&validation_secrets);
            crate::protocols::mcp::responders::validate_detached_sampling_model_result(
                result,
                &validation_model,
                &validation_policy,
                &validation_request_id,
                validation_max_tokens,
                |value| redactor.redact_text(value) == value,
            )
            .map_err(|error| LoopError::Provider(error.to_string()))
        }));
        let cancellation = CancellationController::new();
        let turn_cancellation = TurnCancellation::new(cancellation.handle());
        let cancellation_context = context.clone();
        let _cancellation_monitor = AbortOnDrop(tokio::spawn(async move {
            loop {
                if cancellation_context.is_cancelled()
                    || cancellation_context.revalidate().await.is_err()
                    || tokio::time::Instant::now() >= cancellation_context.deadline()
                {
                    cancellation.interrupt();
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }));
        let mut session = durable
            .start_session(SessionConfig::new(durable_identity))
            .await
            .map_err(map_sampling_model_error)?;
        let mut turn = session
            .begin_turn(turn_request, Some(turn_cancellation.clone()))
            .await
            .map_err(map_sampling_model_error)?;
        let result = loop {
            if context.is_cancelled() {
                return Err(crate::protocols::mcp::responders::ResponderError::Unavailable);
            }
            match turn
                .next_event(Some(turn_cancellation.clone()))
                .await
                .map_err(map_sampling_model_error)?
            {
                Some(ModelTurnEvent::Finished(result)) => break result,
                Some(ModelTurnEvent::ToolCall(_)) => {
                    return Err(crate::protocols::mcp::responders::ResponderError::Invalid);
                }
                Some(ModelTurnEvent::Delta(_) | ModelTurnEvent::Usage(_)) => {}
                None => return Err(crate::protocols::mcp::responders::ResponderError::Unavailable),
            }
        };
        context.revalidate().await?;
        let mut output = detached_sampling_result(&result, &self.model, secrets)?;
        if policy.approval
            == crate::protocols::mcp::config::McpSamplingApprovalMode::RequestAndResponse
        {
            let response_bytes = serde_json::to_vec(&output.result)
                .map_err(|_| crate::protocols::mcp::responders::ResponderError::Invalid)?;
            let response_digest = Digest::of(DigestAlgorithm::Sha256, &response_bytes);
            self.approve(
                &context,
                crate::domain::mcp_callback::McpCallbackMode::SamplingResponse,
                response_digest,
                serde_json::json!({
                    "stage": "response",
                    "digest": response_digest.to_string(),
                    "response": output.result,
                }),
                policy.timeout_millis,
            )
            .await?;
        }
        context.revalidate().await?;
        let response_bytes = serde_json::to_vec(&output.result)
            .map_err(|_| crate::protocols::mcp::responders::ResponderError::Invalid)?;
        let mut delivery_identity = Vec::new();
        crate::capabilities::kernel::identity::put_bytes(
            &mut delivery_identity,
            b"kit-mcp-sampling-delivery-v1",
        );
        crate::capabilities::kernel::identity::put_bytes(
            &mut delivery_identity,
            &request_digest.as_bytes(),
        );
        crate::capabilities::kernel::identity::put_bytes(&mut delivery_identity, &response_bytes);
        let delivery_digest = Digest::of(DigestAlgorithm::Sha256, &delivery_identity);
        let now = crate::domain::events::UtcDateTime::now()
            .map_err(|_| crate::protocols::mcp::responders::ResponderError::Unavailable)?;
        let expires_at = crate::domain::events::UtcDateTime::from_unix_micros(
            now.unix_micros().saturating_add(
                i64::try_from(Duration::from_millis(policy.timeout_millis).as_micros())
                    .unwrap_or(i64::MAX),
            ),
        )
        .map_err(|_| crate::protocols::mcp::responders::ResponderError::Unavailable)?;
        let artifact_expires_at = crate::domain::events::UtcDateTime::from_unix_micros(
            expires_at.unix_micros().saturating_add(
                i64::from(self.artifact_retention_days).saturating_mul(24 * 60 * 60 * 1_000_000),
            ),
        )
        .map_err(|_| crate::protocols::mcp::responders::ResponderError::Unavailable)?;
        let callback = crate::domain::mcp_callback::McpCallbackProjection {
            id: crate::protocols::mcp::responders::callback_id(
                self.security.config.run_id(),
                self.security.attempt.attempt_id,
                self.security.attempt.fencing_token.get(),
                self.security.claim.lease_version,
                context.server_id(),
                context.generation(),
                context.operation_sequence(),
                context.request_id(),
                delivery_digest,
            ),
            server_id: context.server_id().to_owned(),
            kind: crate::domain::mcp_callback::McpCallbackKind::Sampling,
            mode: crate::domain::mcp_callback::McpCallbackMode::SamplingResponse,
            principal_id: self.security.attempt.principal_id,
            project_id: self.security.config.project_id(),
            run_id: self.security.config.run_id(),
            attempt_id: self.security.attempt.attempt_id,
            fence: self.security.attempt.fencing_token,
            claim_generation: self.security.claim.lease_version,
            workspace_id: self.security.workspace_id,
            workspace_revision: self.workspace_revision.clone(),
            request_id: context.request_id().to_owned(),
            request: serde_json::json!({
                "stage": "delivery",
                "response_digest": Digest::of(DigestAlgorithm::Sha256, &response_bytes).to_string(),
            }),
            schema: serde_json::json!({}),
            request_digest: delivery_digest.to_string(),
            schema_digest: delivery_digest.to_string(),
            challenge_generation: context.generation(),
            operation_sequence: context.operation_sequence(),
            expires_at,
            artifact_expires_at,
            max_response_bytes: policy.max_output_bytes,
            max_content_bytes: 1,
            secret_policy_id: context.secret_policy_id()?.to_owned(),
            url_binding: None,
            state: crate::domain::mcp_callback::McpCallbackState::Requested,
            version: 1,
            resolver_actor: None,
            action: None,
            artifact_refs: Vec::new(),
            terminal_error: None,
        };
        let store = crate::store::sqlite::mcp_callback::McpCallbackStore::open(&self.database)
            .map_err(|_| crate::protocols::mcp::responders::ResponderError::Unavailable)?;
        let prepared = store
            .prepare_automatic_delivery(callback)
            .map_err(|_| crate::protocols::mcp::responders::ResponderError::Unavailable)?;
        context.bind_operation_sequence(prepared.operation_sequence);
        if prepared.state == crate::domain::mcp_callback::McpCallbackState::ResponsePrepared {
            output = output.with_delivery(store, prepared.id, delivery_digest.as_bytes());
        } else if !matches!(
            prepared.state,
            crate::domain::mcp_callback::McpCallbackState::Delivered
                | crate::domain::mcp_callback::McpCallbackState::DeliveryUnknown
        ) {
            return Err(crate::protocols::mcp::responders::ResponderError::Unavailable);
        }
        Ok(output)
    }
}

impl DurableSamplingOutcome {
    async fn approve(
        &self,
        context: &crate::protocols::mcp::responders::CallbackAuthorityContext,
        mode: crate::domain::mcp_callback::McpCallbackMode,
        digest: Digest,
        request: serde_json::Value,
        timeout_millis: u64,
    ) -> Result<(), crate::protocols::mcp::responders::ResponderError> {
        use crate::domain::mcp_callback::{
            McpCallbackKind, McpCallbackProjection, McpCallbackState,
        };
        let now = crate::domain::events::UtcDateTime::now()
            .map_err(|_| crate::protocols::mcp::responders::ResponderError::Unavailable)?;
        let expires_at = crate::domain::events::UtcDateTime::from_unix_micros(
            now.unix_micros().saturating_add(
                i64::try_from(Duration::from_millis(timeout_millis).as_micros())
                    .unwrap_or(i64::MAX),
            ),
        )
        .map_err(|_| crate::protocols::mcp::responders::ResponderError::Unavailable)?;
        let artifact_expires_at = crate::domain::events::UtcDateTime::from_unix_micros(
            expires_at.unix_micros().saturating_add(
                i64::from(self.artifact_retention_days).saturating_mul(24 * 60 * 60 * 1_000_000),
            ),
        )
        .map_err(|_| crate::protocols::mcp::responders::ResponderError::Unavailable)?;
        let callback = McpCallbackProjection {
            id: crate::protocols::mcp::responders::callback_id(
                self.security.config.run_id(),
                self.security.attempt.attempt_id,
                self.security.attempt.fencing_token.get(),
                self.security.claim.lease_version,
                context.server_id(),
                context.generation(),
                context.operation_sequence(),
                context.request_id(),
                digest,
            ),
            server_id: context.server_id().to_owned(),
            kind: McpCallbackKind::Sampling,
            mode,
            principal_id: self.security.attempt.principal_id,
            project_id: self.security.config.project_id(),
            run_id: self.security.config.run_id(),
            attempt_id: self.security.attempt.attempt_id,
            fence: self.security.attempt.fencing_token,
            claim_generation: self.security.claim.lease_version,
            workspace_id: self.security.workspace_id,
            workspace_revision: self.workspace_revision.clone(),
            request_id: context.request_id().to_owned(),
            request,
            schema: serde_json::json!({}),
            request_digest: digest.to_string(),
            schema_digest: digest.to_string(),
            challenge_generation: context.generation(),
            operation_sequence: context.operation_sequence(),
            expires_at,
            artifact_expires_at,
            max_response_bytes: 1024,
            max_content_bytes: 1,
            secret_policy_id: context.secret_policy_id()?.to_owned(),
            url_binding: None,
            state: McpCallbackState::Requested,
            version: 1,
            resolver_actor: None,
            action: None,
            artifact_refs: Vec::new(),
            terminal_error: None,
        };
        context
            .with_approval_quota(digest)?
            .await_sampling_approval(
                crate::store::sqlite::mcp_callback::McpCallbackStore::open(&self.database)
                    .map_err(|_| crate::protocols::mcp::responders::ResponderError::Unavailable)?,
                callback,
            )
            .await
    }
}

fn detached_sampling_turn(
    request: &crate::protocols::mcp::responders::ValidatedSamplingRequest,
    request_id: &str,
) -> Result<TurnRequest, crate::protocols::mcp::responders::ResponderError> {
    crate::protocols::mcp::responders::detached_sampling_turn(request, request_id)
}

struct RuntimeSecretLeases {
    resolved: Arc<BTreeMap<crate::domain::secret::SecretHandle, Arc<SecretLease>>>,
    scopes: BTreeMap<String, RuntimeSecretScope>,
    custody: SecretCustody,
    owner: String,
}

impl Drop for RuntimeSecretLeases {
    fn drop(&mut self) {
        self.custody.remove_owner(&self.owner);
    }
}

struct RuntimeSecretScope {
    secrets: Vec<Arc<SecretLease>>,
    authorized_handles: BTreeSet<String>,
}

fn runtime_secret_leases(
    config: &RunExecutorConfig,
    job: &WorkerRun,
    snapshot: &RunConfigSnapshot,
    workspace_id: WorkspaceId,
) -> Result<RuntimeSecretLeases, ExecutorError> {
    if snapshot.principal_id() != job.principal_id
        || snapshot.project_id() != job.project_id
        || snapshot.run_id() != job.run.id
    {
        return Err(ExecutorError::Worker(
            "runtime secret scope does not own this run".to_owned(),
        ));
    }
    runtime_secret_leases_for_scope(
        config,
        job.principal_id,
        job.project_id,
        job.attempt.id,
        snapshot.effective().provider,
        workspace_id,
    )
}

fn runtime_secret_leases_for_scope(
    config: &RunExecutorConfig,
    principal_id: crate::domain::ids::PrincipalId,
    project_id: crate::domain::ids::ProjectId,
    attempt_id: crate::domain::ids::AttemptId,
    provider: ConfigProvider,
    workspace_id: WorkspaceId,
) -> Result<RuntimeSecretLeases, ExecutorError> {
    let provider_secrets = config.model_adapter.secret_leases(provider);
    let provider_handle = format!("provider:{}", config.model_adapter.provider_name(provider));
    let mut scopes = BTreeMap::new();
    let mut resolved = BTreeMap::new();
    for server in &config.mcp_servers {
        if server.owner.principal_id != principal_id
            || server.owner.project_id != project_id
            || server
                .owner
                .workspace_id
                .is_some_and(|owner| owner != workspace_id)
        {
            continue;
        }
        server.validate().map_err(ExecutorError::Worker)?;
        if let Some(scope) = &server.credential_scope
            && matches!(
                scope,
                crate::protocols::mcp::config::McpCredentialScopeConfig::Workspace {
                    workspace_id: owner
                } if *owner != workspace_id
            )
        {
            continue;
        }
        let mut handles = BTreeSet::new();
        handles.extend(server.credential_handle.iter().cloned());
        if let Some(egress) = &server.egress {
            handles.extend(
                egress
                    .redirect_grants
                    .iter()
                    .map(|grant| grant.credential_handle.clone()),
            );
        }
        if let crate::protocols::mcp::config::McpTransportConfig::Stdio { environment, .. } =
            &server.transport
        {
            handles.extend(environment.values().map(|value| value.handle.clone()));
        }
        let mut secrets = provider_secrets.clone();
        let eager_handles = if matches!(
            server.transport,
            crate::protocols::mcp::config::McpTransportConfig::Http { .. }
        ) {
            BTreeSet::new()
        } else {
            handles.clone()
        };
        for handle in &eager_handles {
            let variable = handle.identifier().strip_prefix("env:").ok_or_else(|| {
                ExecutorError::Worker("unsupported runtime secret handle".to_owned())
            })?;
            let value = std::env::var(variable).map_err(|_| {
                ExecutorError::Worker(format!("runtime secret {variable:?} is unavailable"))
            })?;
            if value.is_empty() {
                return Err(ExecutorError::Worker(format!(
                    "runtime secret {variable:?} is empty"
                )));
            }
            let lease = Arc::new(SecretLease::new(value.into_bytes()));
            secrets.push(Arc::clone(&lease));
            resolved.insert(handle.clone(), lease);
        }
        let mut authorized_handles =
            BTreeSet::from([provider_handle.clone(), format!("callback:{}", server.id)]);
        authorized_handles.extend(handles.iter().map(|handle| handle.identifier().to_owned()));
        scopes.insert(
            server.id.clone(),
            RuntimeSecretScope {
                secrets,
                authorized_handles,
            },
        );
    }
    let owner = attempt_id.to_string();
    config.secret_custody.replace_owner(
        owner.clone(),
        config
            .model_adapter
            .all_secret_leases_named()
            .into_iter()
            .chain(
                resolved
                    .iter()
                    .map(|(handle, lease)| (handle.identifier().to_owned(), Arc::clone(lease))),
            ),
    );
    Ok(RuntimeSecretLeases {
        resolved: Arc::new(resolved),
        scopes,
        custody: config.secret_custody.clone(),
        owner,
    })
}

fn sampling_turn_identity(
    security: &ModelSecurity,
    context: &crate::protocols::mcp::responders::CallbackAuthorityContext,
    request_digest: Digest,
) -> String {
    let mut identity = Vec::new();
    crate::capabilities::kernel::identity::put_bytes(&mut identity, b"kit-mcp-sampling-v1");
    for field in [
        security.config.run_id().to_string(),
        security.attempt.attempt_id.to_string(),
        context.server_id().to_owned(),
    ] {
        crate::capabilities::kernel::identity::put_bytes(&mut identity, field.as_bytes());
    }
    crate::capabilities::kernel::identity::put_bytes(
        &mut identity,
        &security.attempt.fencing_token.get().to_be_bytes(),
    );
    crate::capabilities::kernel::identity::put_bytes(
        &mut identity,
        &security.claim.lease_version.to_be_bytes(),
    );
    crate::capabilities::kernel::identity::put_bytes(
        &mut identity,
        &context.generation().to_be_bytes(),
    );
    crate::capabilities::kernel::identity::put_bytes(
        &mut identity,
        &context.operation_sequence().to_be_bytes(),
    );
    crate::capabilities::kernel::identity::put_bytes(&mut identity, &request_digest.as_bytes());
    format!(
        "mcp-sampling-{}",
        Digest::of(DigestAlgorithm::Sha256, &identity)
            .to_string()
            .trim_start_matches("sha256:")
    )
}

fn detached_sampling_result(
    result: &ModelTurnResult,
    model: &str,
    secrets: &[Arc<SecretLease>],
) -> Result<
    crate::protocols::mcp::responders::SamplingHandlerOutput,
    crate::protocols::mcp::responders::ResponderError,
> {
    let redactor = CanaryRedactor::new([]).with_secrets(secrets);
    crate::protocols::mcp::responders::detached_sampling_result(result, model, |value| {
        redactor.redact_text(value) == value
    })
}

fn map_sampling_model_error(error: LoopError) -> crate::protocols::mcp::responders::ResponderError {
    match error {
        LoopError::Cancelled => crate::protocols::mcp::responders::ResponderError::Unavailable,
        LoopError::Unsupported(_) => crate::protocols::mcp::responders::ResponderError::Invalid,
        _ => crate::protocols::mcp::responders::ResponderError::Unavailable,
    }
}

#[cfg(test)]
mod detached_sampling_tests {
    use super::*;

    fn result(part: Part) -> ModelTurnResult {
        ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![Item::new(ItemKind::Assistant, vec![part])],
            usage: Some(Usage::new(TokenUsage::new(1, 1))),
            metadata: agentkit_core::MetadataMap::new(),
            model: Some("selected-model".into()),
            response_id: None,
        }
    }

    #[test]
    fn detached_response_rejects_reasoning() {
        assert!(
            detached_sampling_result(
                &result(Part::Reasoning(ReasoningPart::summary("private"))),
                "selected-model",
                &[],
            )
            .is_err()
        );
    }

    #[test]
    fn detached_response_rejects_tool_use() {
        assert!(
            detached_sampling_result(
                &result(Part::ToolCall(agentkit_core::ToolCallPart::new(
                    "call",
                    "tool",
                    serde_json::json!({}),
                ))),
                "selected-model",
                &[],
            )
            .is_err()
        );
    }

    #[test]
    fn detached_response_rejects_secret_material() {
        let secret = Arc::new(SecretLease::new(b"sampling-secret".to_vec()));
        assert!(
            detached_sampling_result(
                &result(Part::Text(agentkit_core::TextPart::new("sampling-secret"))),
                "selected-model",
                &[secret],
            )
            .is_err()
        );
    }
}

type ExecutorModel =
    GrammarEditModelAdapter<DurableModelAdapter<StreamPolicyAdapter<SelectedAdapter>>>;

fn execute_grammar_edit(
    config: &RunExecutorConfig,
    snapshot: &RunConfigSnapshot,
    result: &TurnResult,
    context: &GrammarEditContext,
) -> Result<(), ExecutorError> {
    let limits = GrammarEditLimits::default();
    let output =
        crate::agent::adapters::grammar_edit::accepted_turn_result(result, limits, context)
            .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let grants = GrantSnapshot::new(
        snapshot.principal_id(),
        snapshot.project_id(),
        snapshot.effective_authority().iter().copied(),
    );
    let authenticated = AuthenticatedPrincipal::from_grants(grants.clone());
    let mut trace = crate::agent::adapters::grammar_edit::EditPathTrace::default();
    crate::agent::adapters::grammar_edit::EditOrchestrator::execute(
        &output,
        crate::workspace::edit::normalize::ModelEditFormat::StructuredJson,
        context,
        &authenticated,
        &grants,
        snapshot,
        &config.artifacts,
        &mut [],
        &mut trace,
    )
    .map(|_| ())
    .map_err(|error| ExecutorError::Worker(error.to_string()))
}

fn model_adapter(
    config: &RunExecutorConfig,
    job: &WorkerRun,
    snapshot: RunConfigSnapshot,
    grammar_context: Option<GrammarEditContext>,
    native_revision: Option<String>,
) -> Result<ExecutorModel, ExecutorError> {
    let selected_provider = snapshot.effective().provider;
    let mut adapter = config.model_adapter.select(selected_provider)?;
    #[cfg(debug_assertions)]
    if let Some(native_revision) = native_revision {
        adapter.bind_native_revision(native_revision);
    }
    let idempotent = config
        .model_adapter
        .provider_idempotency_enforced(selected_provider);
    let retain_reasoning_summaries = config
        .model_adapter
        .retain_reasoning_summaries(selected_provider);
    let stream = Arc::new(PublicStreamCommitFactory {
        inner: SqliteStreamCommitFactory::open(&config.database, StreamLimits::default())?
            .with_reasoning_summaries(retain_reasoning_summaries),
        store: Arc::clone(&config.store),
        #[cfg(debug_assertions)]
        barrier: config.model_adapter.fake_barrier(),
    });
    let adapter = StreamPolicyAdapter::new(
        adapter,
        ModelStreamPolicy {
            secrets: config.secret_custody.leases(),
            retain_reasoning_summaries,
            ..ModelStreamPolicy::default()
        },
        stream,
    );
    let workspace = workspace_id(job.attempt.id)?;
    let security = model_security(job, &snapshot, workspace)?;
    let policy = ModelPolicy {
        reservation: config.model_reservation,
        provider_idempotency: if idempotent {
            ProviderIdempotency::Enforced
        } else {
            ProviderIdempotency::Unproven
        },
        ..ModelPolicy::default()
    };
    let adapter = DurableModelAdapter::new(
        adapter,
        append_store(config)?,
        config.scheduler.clone(),
        security,
        policy,
        job.occurred_at.clone(),
        TraceId::parse("run-executor").expect("executor trace id is valid"),
    );
    Ok(GrammarEditModelAdapter::new(
        adapter,
        snapshot,
        GrammarEditLimits::default(),
        grammar_context,
    ))
}

fn model_security(
    job: &WorkerRun,
    snapshot: &RunConfigSnapshot,
    workspace: WorkspaceId,
) -> Result<ModelSecurity, ExecutorError> {
    let capability = CapabilityIdentity::new(
        CapabilitySource::new("native").expect("static capability source is valid"),
        CapabilityNamespace::new("kit.model").expect("static capability namespace is valid"),
        CapabilityName::new("call").expect("static capability name is valid"),
        CapabilityVersion::new("1.0.0").expect("static capability version is valid"),
        Digest::of(DigestAlgorithm::Blake3, b"kit durable model adapter"),
    );
    let schema = Digest::of(DigestAlgorithm::Sha256, b"kit model call schema v1");
    let constraints = ArgumentConstraints::default();
    let grants = CapabilityGrantSnapshot::new(
        snapshot,
        [CapabilityGrant::new(
            job.principal_id,
            job.project_id,
            workspace,
            capability.clone(),
            schema,
            EffectClass::ModelCall,
            constraints.clone(),
        )],
        DigestAlgorithm::Sha256,
    );
    let authenticated = AuthenticatedPrincipal::from_grants(GrantSnapshot::new(
        job.principal_id,
        job.project_id,
        snapshot.effective_authority().iter().copied(),
    ));
    Ok(ModelSecurity {
        authenticated,
        config: snapshot.clone(),
        grants,
        delegation: None,
        capability,
        schema_digest: schema,
        argument_constraints: constraints,
        workspace_id: workspace,
        attempt: job.attempt.owner,
        claim: job.claim,
    })
}

struct PublicStreamCommitFactory {
    inner: SqliteStreamCommitFactory,
    store: SharedWorkerStore,
    #[cfg(debug_assertions)]
    barrier: Option<FakeProviderBarrier>,
}

impl StreamCommitFactory for PublicStreamCommitFactory {
    fn for_request(&self, request: &TurnRequest) -> Result<Box<dyn StreamCommit>, LoopError> {
        let correlation = request
            .metadata
            .get(crate::agent::driver::restart::EFFECT_CORRELATION_METADATA)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|error| LoopError::Provider(error.to_string()))?
            .ok_or_else(|| LoopError::Provider("model stream has no run correlation".to_owned()))?;
        Ok(Box::new(PublicStreamCommit {
            inner: self.inner.for_request(request)?,
            store: Arc::clone(&self.store),
            correlation,
            #[cfg(debug_assertions)]
            barrier: self.barrier.clone(),
        }))
    }
}

struct PublicStreamCommit {
    inner: Box<dyn StreamCommit>,
    store: SharedWorkerStore,
    correlation: crate::agent::driver::restart::EffectCorrelation,
    #[cfg(debug_assertions)]
    barrier: Option<FakeProviderBarrier>,
}

impl StreamCommit for PublicStreamCommit {
    fn commit_chunk(&mut self, sequence: u64, event: &ModelTurnEvent) -> Result<(), LoopError> {
        self.inner.commit_chunk(sequence, event)?;
        #[cfg(debug_assertions)]
        if sequence == 1
            && let Some(barrier) = &self.barrier
        {
            barrier.wait(FakeBarrierCheckpoint::AfterFirstStreamChunk);
        }
        self.store
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .publish_run_progress(
                self.correlation.run_id,
                self.correlation.claim,
                RunProgressRecord {
                    sequence,
                    model_call_id: Some(self.correlation.operation_id.clone()),
                    kind: model_event_kind(event).to_owned(),
                    content: serde_json::to_value(event)
                        .map_err(|error| LoopError::Provider(error.to_string()))?,
                },
            )
            .map_err(|error| LoopError::Provider(error.to_string()))
    }

    fn commit_outcome(&mut self, result: &ModelTurnResult) -> Result<(), LoopError> {
        self.inner.commit_outcome(result)?;
        #[cfg(debug_assertions)]
        if let Some(barrier) = &self.barrier {
            barrier.wait(FakeBarrierCheckpoint::AfterStreamOutcome);
        }
        Ok(())
    }
}

fn model_event_kind(event: &ModelTurnEvent) -> &'static str {
    match event {
        ModelTurnEvent::Delta(Delta::AppendText { .. }) => "model_text_delta",
        ModelTurnEvent::Delta(Delta::AppendBytes { .. }) => "model_bytes_delta",
        ModelTurnEvent::Delta(Delta::BeginPart { .. }) => "model_part_started",
        ModelTurnEvent::Delta(Delta::CommitPart { .. }) => "model_part_committed",
        ModelTurnEvent::Delta(Delta::ReplaceStructured { .. }) => "model_structured_delta",
        ModelTurnEvent::Delta(Delta::SetMetadata { .. }) => "model_metadata_delta",
        ModelTurnEvent::ToolCall(_) => "model_tool_call",
        ModelTurnEvent::Usage(_) => "model_usage",
        ModelTurnEvent::Finished(_) => "model_finished",
    }
}

fn workspace_id(attempt: crate::domain::ids::AttemptId) -> Result<WorkspaceId, ExecutorError> {
    WorkspaceId::parse(&attempt.to_string().replacen("attempt_", "workspace_", 1))
        .map_err(|error| ExecutorError::Worker(error.to_string()))
}

fn tool_adapter(
    config: &RunExecutorConfig,
    job: &WorkerRun,
    snapshot: &RunConfigSnapshot,
    resolve_native_revision: bool,
    mcp: Option<(
        &Arc<crate::protocols::mcp::transport::McpCapabilityRuntime>,
        &str,
        SecretCustody,
    )>,
    live_cancellation: Arc<AtomicBool>,
    budget: Arc<BudgetLedger>,
) -> Result<(ToolExecutorAdapter, Option<String>), ExecutorError> {
    let workspace = workspace_id(job.attempt.id)?;
    let project_root = std::fs::canonicalize(&config.project_root).map_err(|error| {
        ExecutorError::Worker(format!("trusted project root unavailable: {error}"))
    })?;
    let native_scope =
        crate::capabilities::extensions::ExtensionScope::new(job.principal_id, job.project_id);
    let native_extension_guard = crate::capabilities::extensions::attest_native_extension_durable(
        &config.capability_extensions,
        native_scope,
        &mut append_store(config)?,
    )
    .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let (mcp_runtime, mcp_revision, mcp_custody) = mcp.map_or((None, None, None), |value| {
        (Some(value.0), Some(value.1), Some(value.2))
    });
    let descriptors = crate::capabilities::native::NativeCatalog::all().to_vec();
    let configured = descriptors
        .iter()
        .map(|descriptor| {
            let constraints = ArgumentConstraints::new([format!(
                "native={}@{}",
                descriptor.tool().short_name(),
                descriptor.identity().version().as_str()
            )
            .into_bytes()]);
            (descriptor.clone(), constraints)
        })
        .collect::<Vec<_>>();
    let mcp_catalog = mcp_runtime
        .map(|runtime| {
            runtime.catalog_snapshot_for(job.principal_id, job.project_id, workspace, mcp_revision)
        })
        .transpose()
        .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let mcp_configured = mcp_catalog
        .as_ref()
        .map(|catalog| {
            catalog
                .entries()
                .iter()
                .filter_map(|entry| {
                    let target = entry.external_target()?;
                    let (grant_extension, request_extension) = mcp_runtime
                        .expect("MCP catalog requires its runtime")
                        .authority_for(target.configured_server())
                        .ok()?;
                    let constraints = ArgumentConstraints::new([format!(
                        "mcp={}@{}",
                        target.configured_server(),
                        target.descriptor_digest()
                    )
                    .into_bytes()]);
                    Some((
                        Arc::clone(entry),
                        constraints,
                        grant_extension,
                        request_extension,
                    ))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let grants = CapabilityGrantSnapshot::new(
        snapshot,
        configured
            .iter()
            .filter(|(descriptor, _)| {
                descriptor
                    .required_grants()
                    .iter()
                    .all(|grant| snapshot.effective_authority().contains(grant))
            })
            .map(|(descriptor, constraints)| {
                CapabilityGrant::new(
                    job.principal_id,
                    job.project_id,
                    workspace,
                    descriptor.identity().clone(),
                    descriptor.schema().normalized_digest(),
                    descriptor.effect(),
                    constraints.clone(),
                )
            })
            .chain(
                mcp_configured
                    .iter()
                    .map(|(entry, constraints, extension, _)| {
                        CapabilityGrant::new(
                            job.principal_id,
                            job.project_id,
                            workspace,
                            entry.identity().clone(),
                            entry
                                .schemas()
                                .input()
                                .schema()
                                .source()
                                .normalized_digest(),
                            entry.side_effects().effect(),
                            constraints.clone(),
                        )
                        .with_extension(extension.clone())
                    }),
            ),
        DigestAlgorithm::Sha256,
    );
    let authenticated = AuthenticatedPrincipal::from_grants(GrantSnapshot::new(
        job.principal_id,
        job.project_id,
        snapshot.effective_authority().iter().copied(),
    ));
    let bindings = configured
        .iter()
        .filter(|(descriptor, _)| {
            descriptor
                .required_grants()
                .iter()
                .all(|grant| snapshot.effective_authority().contains(grant))
        })
        .map(|(descriptor, constraints)| {
            let binding = ToolBinding::new(
                descriptor.spec().clone(),
                descriptor.identity().clone(),
                descriptor.normalized_schema().clone(),
                descriptor.schema().normalized_digest(),
                descriptor.schema().normalized_digest(),
                descriptor.effect(),
                constraints.clone(),
                descriptor.reservation(),
                descriptor.retry_safety(),
                if config
                    .model_adapter
                    .deterministic_native_approval(snapshot.effective().provider)
                {
                    ApprovalState::Approved
                } else {
                    match (
                        descriptor.approval(),
                        config
                            .model_adapter
                            .additional_tool_approval(snapshot.effective().provider),
                    ) {
                        (ApprovalState::Pending, _) | (_, ApprovalState::Pending) => {
                            ApprovalState::Pending
                        }
                        _ => descriptor.approval(),
                    }
                },
            );
            if descriptor.tool() == crate::capabilities::native::NativeTool::Check {
                let registry = config.verification_registry.clone();
                let grants = authenticated.grant_snapshot().clone();
                let snapshot = snapshot.clone();
                let descriptor = descriptor.clone();
                binding.with_cost_estimator(move |input| {
                    descriptor.estimate_reservation(input, &registry, &grants, &snapshot)
                })
            } else {
                binding
            }
        })
        .collect::<Vec<_>>();
    let discovery = config
        .tool_learning_key
        .map(|pointer_key| {
            Ok::<_, ExecutorError>(ToolDiscoveryConfig {
                catalog: match &mcp_catalog {
                    Some(catalog) => catalog.clone(),
                    None => crate::capabilities::catalog::CatalogSnapshot::from_native(
                        DigestAlgorithm::Sha256,
                    )
                    .map_err(|error| ExecutorError::Worker(error.to_string()))?,
                },
                authorities: mcp_configured
                    .iter()
                    .map(|(_, constraints, _, extension)| DiscoveryAuthority {
                        constraints: constraints.clone(),
                        extension: extension.clone(),
                    })
                    .collect(),
                provider: provider_capability_contract(config, snapshot, native_scope)?,
                telemetry: config.telemetry.clone(),
                pointer_key,
            })
        })
        .transpose()?;
    if mcp_catalog.is_some() && discovery.is_none() {
        return Err(ExecutorError::Worker(
            "MCP tool learning requires a durable project pointer key".to_owned(),
        ));
    }
    let scratch = config
        .workspace_scratch
        .join(job.attempt.owner.attempt_id.to_string());
    std::fs::create_dir_all(&scratch).map_err(|error| ExecutorError::Worker(error.to_string()))?;
    std::fs::create_dir_all(config.workspace_scratch.join("acquired"))
        .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let scratch =
        std::fs::canonicalize(scratch).map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let acquired_root = std::fs::canonicalize(config.workspace_scratch.join("acquired"))
        .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    // Snapshot acquisition is best-effort: a source that cannot be snapshotted
    // (unsupported entries, size limits, ...) must not kill the run. The
    // capabilities that need the snapshot fail at dispatch time with the
    // recorded reason so the model can pivot.
    let (acquisition, acquisition_failure) = match crate::workspace::acquire::acquire(
        crate::workspace::acquire::AcquisitionRequest::new(
            project_root.clone(),
            acquired_root,
            crate::workspace::acquire::WorkspaceId::new(workspace.to_string())
                .map_err(|error| ExecutorError::Worker(error.to_string()))?,
            crate::workspace::acquire::OwnerId::new(job.attempt.owner.attempt_id.to_string())
                .map_err(|error| ExecutorError::Worker(error.to_string()))?,
            crate::workspace::acquire::AcquisitionMode::CopyOnWriteSnapshot,
            crate::workspace::acquire::WriterPolicy::Restricted,
        ),
    ) {
        Ok(result) => (Some(result), None),
        Err(error) => (None, Some(error.to_string())),
    };
    let native_root = project_root;
    let process_registration = config.process_registry.as_ref().map(|registry| {
        ProcessRegistryRegistration::new(
            Arc::clone(registry),
            ProcessRegistrationContext {
                project_id: job.project_id,
                principal_id: job.principal_id,
            },
        )
        .with_custody(config.secret_custody.clone())
    });
    let check_runner = acquisition
        .as_ref()
        .zip(process_registration.as_ref())
        .filter(|_| !config.verification_registry.is_empty())
        .map(|(acquisition, registration)| {
            #[cfg(debug_assertions)]
            if !config.native_check_completions.is_empty() {
                return crate::executor::check::CheckRunner::conformance(
                    config.native_check_completions.clone(),
                );
            }
            crate::executor::check::CheckRunner::registered_attempt_container(
                job.attempt.owner,
                SqliteCancellationCoordinator::new(&config.database),
                crate::executor::cancel::WorkspaceIdentity::from_acquisition(
                    workspace,
                    acquisition,
                ),
                registration.clone(),
            )
        });
    let formatter = config
        .native_formatter_descriptor
        .clone()
        .zip(acquisition.as_ref())
        .zip(process_registration.as_ref())
        .map(|((descriptor, acquisition), registration)| {
            crate::capabilities::native::dispatch::NativeFormatterRuntime {
                descriptor,
                executor:
                    crate::executor::formatter::FormatterExecutor::registered_attempt_container(
                        job.attempt.owner,
                        SqliteCancellationCoordinator::new(&config.database),
                        crate::executor::cancel::WorkspaceIdentity::from_acquisition(
                            workspace,
                            acquisition,
                        ),
                        registration.clone(),
                    ),
            }
        });
    let mut syntax_executors = vec![
        crate::executor::syntax::SyntaxExecutor::production(
            "text",
            crate::workspace::edit::format::NATIVE_TEXT_VERSION,
        ),
        crate::executor::syntax::SyntaxExecutor::production(
            "json",
            crate::workspace::edit::format::NATIVE_JSON_VERSION,
        ),
        crate::executor::syntax::SyntaxExecutor::production(
            "rust",
            crate::workspace::edit::format::RUST_GRAMMAR_VERSION,
        ),
    ];
    #[cfg(debug_assertions)]
    if !config.native_check_completions.is_empty() {
        syntax_executors = vec![
            crate::executor::syntax::SyntaxExecutor::debug(
                "text",
                crate::workspace::edit::format::NATIVE_TEXT_VERSION,
                crate::executor::syntax::DebugSyntaxAction::Pass(None),
            ),
            crate::executor::syntax::SyntaxExecutor::debug(
                "json",
                crate::workspace::edit::format::NATIVE_JSON_VERSION,
                crate::executor::syntax::DebugSyntaxAction::Pass(None),
            ),
            crate::executor::syntax::SyntaxExecutor::debug(
                "rust",
                crate::workspace::edit::format::RUST_GRAMMAR_VERSION,
                crate::executor::syntax::DebugSyntaxAction::Pass(None),
            ),
        ];
    }
    let cursor_key = if let Some(key) = config.tool_learning_key {
        key
    } else {
        let mut key = [0; 32];
        getrandom::fill(&mut key).map_err(|error| ExecutorError::Worker(error.to_string()))?;
        key
    };
    let mut dispatcher = crate::capabilities::native::dispatch::NativeDispatcher::open(
        native_root,
        &scratch,
        Arc::clone(&config.artifacts),
        authenticated.clone(),
        snapshot.clone(),
        acquisition,
        crate::capabilities::native::dispatch::NativeRuntime {
            extension_guard: native_extension_guard,
            workspace_id: workspace,
            process_registration,
            cancellation: SqliteCancellationCoordinator::new(&config.database),
            live_cancellation: Arc::clone(&live_cancellation),
            container_image: config.native_container_image.clone(),
            verification_registry: config.verification_registry.clone(),
            check_runner,
            acquisition_failure,
            custody: config.secret_custody.clone(),
            secrets: config
                .secret_custody
                .leases()
                .iter()
                .map(|secret| SecretLease::new(secret.expose().to_vec()))
                .collect(),
            syntax_executors,
            formatter_required: config.native_formatter_required,
            formatter,
            feedback: Some(
                crate::capabilities::native::dispatch::NativeFeedbackRuntime {
                    database: config.database.clone(),
                    adapters: config.native_diagnostic_adapters.clone(),
                    limits: config.native_feedback_limits.clone(),
                },
            ),
            semantic_evidence: config.native_semantic_evidence.clone(),
            edit_validation_time: config.native_edit_validation_time,
            cursor_key,
            #[cfg(test)]
            run_runner: None,
        },
    )
    .map_err(ExecutorError::Worker)?;
    let native_revision = resolve_native_revision
        .then(|| dispatcher.revision().map_err(ExecutorError::Worker))
        .transpose()?;
    if mcp_revision.is_some() && mcp_revision != native_revision.as_deref() {
        return Err(ExecutorError::Worker(
            "MCP owner workspace revision changed during executor initialization".to_owned(),
        ));
    }
    let adapter = ToolExecutorAdapter::new(
        bindings,
        ToolKernelContext {
            authenticated,
            config: snapshot.clone(),
            grants,
            delegation: None,
            workspace_id: workspace,
            project_id: job.project_id,
            attempt: job.attempt.owner,
            claim: job.claim,
            current_fence: Arc::new(AtomicU64::new(job.attempt.owner.fencing_token.get())),
            cancellation: live_cancellation,
            cancellation_coordinator: Arc::clone(&config.cancellation_coordinator),
            budget,
            custody: config.secret_custody.clone(),
        },
        append_store(config)?,
        move |invocation| dispatcher.dispatch(invocation),
    )
    .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let adapter = match mcp_runtime {
        Some(runtime) => adapter.with_mcp_runtime(
            Arc::clone(runtime),
            Arc::clone(&config.artifacts),
            crate::protocols::mcp::transport::McpResultPolicy::default()
                .with_custody(mcp_custody.expect("MCP runtime carries secret custody")),
        ),
        None => adapter,
    };
    let adapter = match discovery {
        Some(discovery) => adapter
            .with_discovery(discovery)
            .map_err(ExecutorError::Worker)?,
        None => adapter,
    };
    Ok((adapter, native_revision))
}

fn provider_capability_contract(
    config: &RunExecutorConfig,
    snapshot: &RunConfigSnapshot,
    scope: crate::capabilities::extensions::ExtensionScope,
) -> Result<crate::capabilities::registration::ProviderCapabilityContract, ExecutorError> {
    use crate::capabilities::{
        kernel::identity::DigestAlgorithm,
        registration::{ProviderCapabilityContract, ValidatedProjectionSupport},
        schema::{JSON_SCHEMA_2020_12, ProjectionProfile, ProjectionTarget},
    };

    let provider = snapshot.effective().provider;
    let profile = ProjectionProfile::new(
        ProjectionTarget::new(
            config.model_adapter.provider_name(provider),
            config.model_adapter.model_name(provider),
            "agentkit",
            1,
        )
        .map_err(|error| ExecutorError::Worker(error.to_string()))?,
        JSON_SCHEMA_2020_12,
        BTreeSet::from([
            "$schema".to_owned(),
            "additionalProperties".to_owned(),
            "maxLength".to_owned(),
            "maximum".to_owned(),
            "minLength".to_owned(),
            "minimum".to_owned(),
            "pattern".to_owned(),
            "properties".to_owned(),
            "required".to_owned(),
            "type".to_owned(),
        ]),
        serde_json::Value::Bool(true),
        1024 * 1024,
        DigestAlgorithm::Sha256,
    )
    .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let adapter = crate::capabilities::schema::SchemaProjectionAdapter::new(
        &config.capability_extensions,
        scope,
    )
    .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    Ok(ProviderCapabilityContract::portable(
        ValidatedProjectionSupport::validate_with_adapter(&profile, &adapter)
            .map_err(|error| ExecutorError::Worker(error.to_string()))?,
    ))
}

fn durable_tool_budget(
    config: &RunExecutorConfig,
    snapshot: &RunConfigSnapshot,
) -> Result<Arc<BudgetLedger>, ExecutorError> {
    let events = append_store(config)?
        .events()
        .map_err(ExecutorError::Store)?;
    tool_budget_from_events(&events, snapshot).map(Arc::new)
}

pub(crate) fn tool_budget_from_events(
    events: &[crate::store::sqlite::append::StoredEvent],
    snapshot: &RunConfigSnapshot,
) -> Result<BudgetLedger, ExecutorError> {
    let mut reservations = BTreeMap::<String, (ReservationId, Spend, ReservationStatus)>::new();
    for event in events
        .iter()
        .filter(|event| event.event.correlation_id == EntityId::Run(snapshot.run_id()))
    {
        let value: serde_json::Value = serde_json::from_slice(&event.event.payload)
            .map_err(|error| ExecutorError::Worker(error.to_string()))?;
        match event.event.event_type.as_str() {
            "capability.invocation_intent" => {
                let Some(invocation) = value
                    .get("invocation_id")
                    .and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                let id = value
                    .get("reservation_id")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|value| value.parse::<u128>().ok())
                    .map(ReservationId::new)
                    .ok_or_else(|| {
                        ExecutorError::Worker("tool reservation id is invalid".to_owned())
                    })?;
                let spend = value
                    .get("reservation")
                    .and_then(spend_from_value)
                    .ok_or_else(|| {
                        ExecutorError::Worker("tool reservation is invalid".to_owned())
                    })?;
                reservations.insert(
                    invocation.to_owned(),
                    (id, spend, ReservationStatus::Reserved),
                );
            }
            "capability.invocation_outcome" => {
                let Some(invocation) = value
                    .get("invocation_id")
                    .and_then(serde_json::Value::as_str)
                else {
                    continue;
                };
                if let Some(reservation) = reservations.get_mut(invocation) {
                    reservation.2 = if value
                        .get("result")
                        .and_then(|result| result.get("charged"))
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false)
                    {
                        ReservationStatus::Debited
                    } else {
                        ReservationStatus::Released
                    };
                }
            }
            _ => {}
        }
    }
    BudgetLedger::from_snapshots(
        RunBudget::from_effective_config(snapshot.effective()),
        reservations
            .into_values()
            .map(|(id, spend, status)| ReservationSnapshot::new(id, spend, status)),
    )
    .map_err(|error| ExecutorError::Worker(format!("durable tool budget is invalid: {error:?}")))
}

fn ensure_waiting(
    config: &RunExecutorConfig,
    run_id: RunId,
    waiting: &crate::agent::driver::waiting::WaitingState,
) -> Result<(), ExecutorError> {
    config
        .store
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .ensure_worker_wait(run_id, waiting)
        .map(drop)
        .map_err(ExecutorError::Service)
}

fn append_record(
    config: &RunExecutorConfig,
    job: &WorkerRun,
    key: &str,
    record: LoopRecord,
) -> Result<(), ExecutorError> {
    let append = journal_append(job, key, record)?;
    append_store(config)?
        .append_effect(append)
        .map_err(ExecutorError::Store)?;
    Ok(())
}

fn append_hashed_record(
    config: &RunExecutorConfig,
    job: &WorkerRun,
    prefix: &str,
    record: LoopRecord,
) -> Result<(), ExecutorError> {
    let bytes =
        serde_json::to_vec(&record).map_err(|error| ExecutorError::Worker(error.to_string()))?;
    append_record(
        config,
        job,
        &format!("{prefix}-{}", blake3::hash(&bytes).to_hex()),
        record,
    )
}

fn journal_append(
    job: &WorkerRun,
    key: &str,
    record: LoopRecord,
) -> Result<EffectJournalAppend, ExecutorError> {
    Ok(EffectJournalAppend {
        owner: job.attempt.owner,
        claim: Some(job.claim),
        idempotency_key: crate::store::sqlite::idempotency::IdempotencyKey::parse(&format!(
            "executor-{}-{key}",
            job.attempt.id
        ))
        .map_err(|error| ExecutorError::Worker(error.to_string()))?,
        command_id: CommandId::generate()
            .map_err(|error| ExecutorError::Worker(error.to_string()))?,
        event_id: EventId::generate().map_err(|error| ExecutorError::Worker(error.to_string()))?,
        occurred_at: job.occurred_at.clone(),
        trace_id: TraceId::parse("run-executor").expect("executor trace id is valid"),
        artifacts: Vec::new(),
        record,
    })
}

fn append_store(
    config: &RunExecutorConfig,
) -> Result<crate::store::sqlite::append::SqliteStore, ExecutorError> {
    config
        .store
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .worker_append_store()
        .map_err(ExecutorError::Service)
}

fn load_job(config: &RunExecutorConfig, run_id: RunId) -> Result<WorkerRun, ExecutorError> {
    config
        .store
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .worker_run(run_id)
        .map_err(ExecutorError::Service)
}

fn transition_run(
    config: &RunExecutorConfig,
    run_id: RunId,
    target: RunState,
) -> Result<WorkerRun, ExecutorError> {
    config
        .store
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .transition_worker_run(run_id, target)
        .map_err(ExecutorError::Service)
}

fn transition_attempt(
    config: &RunExecutorConfig,
    attempt_id: crate::domain::ids::AttemptId,
    target: AttemptState,
) -> Result<WorkerRun, ExecutorError> {
    config
        .store
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .transition_worker_attempt(attempt_id, target)
        .map_err(ExecutorError::Service)
}

fn complete_attempt(
    config: &RunExecutorConfig,
    job: WorkerRun,
    prepared: &PreparedPrompt,
    items: &[Item],
    latency: Duration,
) -> Result<(), ExecutorError> {
    let mut item = items
        .iter()
        .rev()
        .find(|item| item.kind == ItemKind::Assistant)
        .map(from_agentkit_item)
        .ok_or_else(|| ExecutorError::Worker("completed run has no assistant output".to_owned()))?;
    item.parts
        .retain(|part| !matches!(part, CanonicalPart::Reasoning { .. }));
    item.metadata.clear();
    item.usage = None;
    item = serde_json::from_value(config.secret_custody.project_json(
        crate::telemetry::redact::CaptureBoundary::Artifact,
        &serde_json::to_value(&item).map_err(|error| ExecutorError::Worker(error.to_string()))?,
    ))
    .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let item_bytes =
        serde_json::to_vec(&item).map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let snapshot = RunConfigSnapshot::from_canonical_bytes(&job.effective_config)
        .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let stored_at =
        now_unix_micros().map_err(|error| ExecutorError::Artifact(error.to_string()))?;
    let retention_micros = i64::from(snapshot.effective().artifact_retention_days)
        .saturating_mul(24 * 60 * 60 * 1_000_000);
    let output_artifact = config
        .artifacts
        .put(
            &item_bytes,
            ArtifactMetadata::new(
                "application/vnd.kit.agent-item+json",
                ArtifactClass::File,
                job.principal_id.to_string(),
                job.project_id.to_string(),
                ArtifactRetention::UntilUnixMicros(stored_at.saturating_add(retention_micros)),
                stored_at,
            )
            .map_err(|error| ExecutorError::Artifact(error.to_string()))?,
        )
        .map_err(|error| ExecutorError::Artifact(error.to_string()))?;
    let output_ref =
        crate::domain::events::ArtifactRef::parse(&output_artifact.digest().to_string())
            .map_err(|error| ExecutorError::Artifact(error.to_string()))?;
    let preview = output_preview(&item);
    let usage = run_accounting(config, &job, prepared)?;
    let provider = snapshot.effective().provider;
    let envelope_redactor = config.secret_custody.redactor();
    let envelope_capture = envelope_redactor.capture();
    let envelope = RunEnvelope::capture(
        RunCapture {
            prompt: &prepared.compiled,
            previous_prompt: None,
            current_tokens: None,
            previous_tokens: None,
            context: &prepared.context,
            accounting: Some(&usage),
            provider_model: ProviderModelDescriptor {
                provider: Some(config.model_adapter.provider_name(provider).to_owned()),
                model: Some(config.model_adapter.model_name(provider).to_owned()),
                settings: std::collections::BTreeMap::from([(
                    "max_tokens".to_owned(),
                    serde_json::json!(snapshot.effective().max_tokens),
                )]),
                ..ProviderModelDescriptor::default()
            },
            effective_config: &snapshot,
            provider_cache: ProviderCacheObservation::default(),
            core: CoreRunObservation {
                outcome: Some(RunOutcome::Succeeded),
                latency_ms: Some(latency.as_millis().min(u128::from(u64::MAX)) as u64),
                checks: Some(Vec::new()),
                errors: Some(Vec::new()),
            },
            provider_summary: None,
            summary_retention: SummaryRetentionPolicy::Discard,
        },
        &envelope_capture,
    )
    .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let telemetry_digest = envelope
        .digest()
        .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let item_preview = serde_json::json!({
        "kind": "assistant",
        "preview": preview,
    });
    config
        .store
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .publish_run_completion(
            job.run.id,
            job.claim,
            RunCompletionRecord {
                output: RunOutputProjection {
                    artifact: output_ref,
                    preview,
                    status: "complete".to_owned(),
                },
                item_preview,
                usage,
                cost: envelope.cost.clone(),
                telemetry_digest,
            },
        )?;
    let run_id = job.run.id;
    let project_id = job.project_id;
    finish_attempt(config, job)?;
    flush_learning(config, project_id, run_id);
    if let Some(telemetry) = &config.telemetry {
        telemetry
            .emit_canonical_run_envelope(envelope)
            .and_then(|_| telemetry.flush().map(|_| ()))
            .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    }
    #[cfg(debug_assertions)]
    if let Some(barrier) = config.model_adapter.fake_barrier() {
        barrier.wait(FakeBarrierCheckpoint::AfterJournalBoundary);
    }
    Ok(())
}

fn finish_attempt(config: &RunExecutorConfig, mut job: WorkerRun) -> Result<(), ExecutorError> {
    if job.attempt.state != AttemptState::Quiescing {
        job = transition_attempt(config, job.attempt.id, AttemptState::Quiescing)?;
    }
    transition_attempt(config, job.attempt.id, AttemptState::Succeeded)?;
    transition_run(config, job.run.id, RunState::Completed)?;
    config.scheduler.finish_run(job.run.id, false)?;
    append_store(config)?.quiesce_driver_claim(job.claim)?;
    Ok(())
}

fn output_preview(item: &CanonicalItem) -> String {
    let mut preview = item
        .parts
        .iter()
        .filter_map(|part| match part {
            CanonicalPart::Text { text, .. } => Some(text.clone()),
            CanonicalPart::Structured { value, .. } => Some(value.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if preview.len() > 512 {
        let mut end = 512;
        while !preview.is_char_boundary(end) {
            end -= 1;
        }
        preview.truncate(end);
    }
    preview
}

fn run_accounting(
    config: &RunExecutorConfig,
    job: &WorkerRun,
    prepared: &PreparedPrompt,
) -> Result<UsageEnvelope, ExecutorError> {
    let snapshot = RunConfigSnapshot::from_canonical_bytes(&job.effective_config)
        .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let table = config
        .model_adapter
        .cost_table(snapshot.effective().provider);
    let events = append_store(config)?
        .events()
        .map_err(ExecutorError::Store)?;
    let mut envelopes = Vec::new();
    let mut first_model = true;
    let mut tool_reservations = std::collections::BTreeMap::<String, Spend>::new();
    for event in &events {
        if event.event.correlation_id != EntityId::Run(job.run.id)
            || event.event.attempt_id != Some(job.attempt.id)
        {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(&event.event.payload)
            .map_err(|error| ExecutorError::Worker(error.to_string()))?;
        match event.event.event_type.as_str() {
            "capability.invocation_intent" => {
                if let (Some(id), Some(spend)) = (
                    value
                        .get("invocation_id")
                        .and_then(serde_json::Value::as_str),
                    value.get("reservation").and_then(spend_from_value),
                ) {
                    tool_reservations.insert(id.to_owned(), spend);
                }
            }
            "capability.invocation_outcome" => {
                let result: CanonicalInvocationResult =
                    serde_json::from_value(value.get("result").cloned().ok_or_else(|| {
                        ExecutorError::Worker("tool usage is missing".to_owned())
                    })?)
                    .map_err(|error| ExecutorError::Worker(error.to_string()))?;
                let spend = value
                    .get("invocation_id")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|id| tool_reservations.get(id))
                    .copied()
                    .unwrap_or(Spend::new(0, 0, 0, 1, 0));
                let reservation = ReservationSnapshot::new(
                    ReservationId::new(0),
                    spend,
                    if result.charged {
                        ReservationStatus::Debited
                    } else {
                        ReservationStatus::Released
                    },
                );
                envelopes.push(
                    UsageEnvelope::from_tool_outcome(
                        &result,
                        &ToolMeasurement::one_call(),
                        SpeculationOutcome::None,
                        table.as_ref(),
                        Some(reservation),
                    )
                    .map_err(|error| ExecutorError::Worker(error.to_string()))?,
                );
            }
            "model_call.outcome" => {
                let usage = value
                    .get("usage")
                    .filter(|value| !value.is_null())
                    .cloned()
                    .map(serde_json::from_value::<Usage>)
                    .transpose()
                    .map_err(|error| ExecutorError::Worker(error.to_string()))?;
                let canonical = usage.as_ref().map(from_agentkit_usage);
                let outcome = match value.get("status").and_then(serde_json::Value::as_str) {
                    Some("cancelled") => ModelOutcome::Cancelled,
                    Some("outcome_unknown") => ModelOutcome::OutcomeUnknown,
                    Some("succeeded") => ModelOutcome::Succeeded,
                    _ => ModelOutcome::Failed,
                };
                let charged = value
                    .get("charged")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(outcome != ModelOutcome::Cancelled);
                let reservation_id = value
                    .get("reservation_id")
                    .and_then(serde_json::Value::as_str)
                    .and_then(parse_reservation_id)
                    .ok_or_else(|| {
                        ExecutorError::Worker("model reservation id is missing".to_owned())
                    })?;
                let logical = LogicalModelUsage {
                    uncached_input_tokens: first_model.then_some(
                        u64::try_from(prepared.context.estimated_tokens).map_err(|_| {
                            ExecutorError::Worker("prompt estimate overflowed".to_owned())
                        })?,
                    ),
                    visible_output_tokens: canonical.as_ref().and_then(|usage| usage.output_tokens),
                    reasoning_tokens: canonical.as_ref().and_then(|usage| usage.reasoning_tokens),
                    ..LogicalModelUsage::default()
                };
                first_model = false;
                let provisional = UsageEnvelope::from_model_usage(
                    canonical.as_ref(),
                    &logical,
                    outcome,
                    charged,
                    SpeculationOutcome::None,
                    table.as_ref(),
                    None,
                )
                .map_err(|error| ExecutorError::Worker(error.to_string()))?;
                let actual = value
                    .get("settlement")
                    .and_then(spend_from_value)
                    .unwrap_or_else(|| accounting_spend(&provisional));
                let reservation = if charged {
                    Some(config.scheduler.reconcile(reservation_id, actual)?)
                } else {
                    Some(config.scheduler.snapshot(reservation_id)?)
                };
                envelopes.push(
                    UsageEnvelope::from_model_usage(
                        canonical.as_ref(),
                        &logical,
                        outcome,
                        charged,
                        SpeculationOutcome::None,
                        table.as_ref(),
                        reservation,
                    )
                    .map_err(|error| ExecutorError::Worker(error.to_string()))?,
                );
            }
            _ => {}
        }
    }
    UsageEnvelope::aggregate(envelopes).map_err(|error| ExecutorError::Worker(error.to_string()))
}

fn spend_from_value(value: &serde_json::Value) -> Option<Spend> {
    Some(Spend::new(
        value.get("cost_microusd")?.as_u64()?,
        value.get("tokens")?.as_u64()?,
        value.get("turns")?.as_u64()?,
        value.get("tools")?.as_u64()?,
        value.get("processes")?.as_u64()?,
    ))
}

fn accounting_spend(usage: &UsageEnvelope) -> Spend {
    let categories = &usage.categories;
    let tokens = [
        categories.uncached_input.billed_tokens,
        categories.cache_write.billed_tokens,
        categories.cache_read.billed_tokens,
        categories.visible_output.billed_tokens,
        categories.reasoning.billed_tokens,
    ]
    .into_iter()
    .flatten()
    .fold(0_u64, u64::saturating_add);
    let cost = usage
        .provider_cost
        .as_ref()
        .filter(|cost| cost.amount.currency == "USD")
        .map_or(0, |cost| cost.amount.micros);
    Spend::new(cost, tokens, 1, 0, 0)
}

fn parse_reservation_id(value: &str) -> Option<ReservationId> {
    u128::from_str_radix(value, 16).ok().map(ReservationId::new)
}

fn cancel_attempt(
    config: &RunExecutorConfig,
    mut job: WorkerRun,
) -> Result<AttemptExit, ExecutorError> {
    settle_cancelled_learning(config, &job)?;
    if job.attempt.state != AttemptState::Quiescing {
        job = transition_attempt(config, job.attempt.id, AttemptState::Quiescing)?;
    }
    let outcome = config
        .cancellation_coordinator
        .cancel_attempt(job.attempt.owner)
        .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    if outcome == ExecutorCancellationOutcome::OutcomeUnknown {
        return Ok(AttemptExit::Waiting);
    }
    transition_attempt(config, job.attempt.id, AttemptState::Interrupted)?;
    job = load_job(config, job.run.id)?;
    if job.run.state != RunState::Cancelling {
        job = transition_run(config, job.run.id, RunState::Cancelling)?;
    }
    transition_run(config, job.run.id, RunState::Cancelled)?;
    config.scheduler.finish_run(job.run.id, true)?;
    flush_learning(config, job.project_id, job.run.id);
    Ok(AttemptExit::Completed)
}

fn fail_attempt(
    config: &RunExecutorConfig,
    job: WorkerRun,
    failure: RunFailureProjection,
) -> Result<AttemptExit, ExecutorError> {
    config
        .store
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .fail_worker_run(job.run.id, job.claim, failure)?;
    config.scheduler.finish_run(job.run.id, false)?;
    flush_learning(config, job.project_id, job.run.id);
    Ok(AttemptExit::Completed)
}

fn flush_learning(config: &RunExecutorConfig, project_id: ProjectId, run_id: RunId) {
    let (Some(telemetry), Some(key)) = (&config.telemetry, config.tool_learning_key) else {
        return;
    };
    let hasher = crate::telemetry::tool_learning::ProjectPointerHasher::new(project_id, &key);
    match append_store(config) {
        Ok(mut store) => {
            let _ = telemetry.export_learning_outbox(&mut store, &hasher);
            if store.catalog_stats_run_terminal(run_id).unwrap_or(false) {
                let _ = telemetry.export_catalog_stats_snapshot(&mut store, &hasher, run_id);
            }
        }
        Err(error) => telemetry.mark_learning_failure(error.to_string()),
    }
}

struct ProgressObserver {
    run_id: RunId,
    attempt: AttemptOwnership,
    claim: crate::api::service::AttemptDriverClaim,
    store: SharedWorkerStore,
    sender: broadcast::Sender<ProgressEvent>,
    error: Arc<Mutex<Option<String>>>,
}

impl LoopObserver for ProgressObserver {
    fn handle_event(&self, observed: ObservedEvent) {
        if let AgentEvent::ToolResultReceived(result) = &observed.event {
            let publish = serde_json::to_value(result)
                .map_err(|error| ServiceError::Store(error.to_string()))
                .and_then(|content| {
                    self.store
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .publish_run_progress(
                            self.run_id,
                            self.claim,
                            RunProgressRecord {
                                sequence: 0,
                                model_call_id: None,
                                kind: "tool_result".to_owned(),
                                content,
                            },
                        )
                });
            if let Err(error) = publish {
                *self.error.lock().unwrap_or_else(|error| error.into_inner()) =
                    Some(error.to_string());
            }
        }
        let _ = self.sender.send(ProgressEvent {
            run_id: self.run_id,
            attempt: self.attempt,
            event: observed.event,
        });
    }
}

#[derive(Clone)]
pub struct SelectedModelAdapter {
    providers: BTreeMap<ConfigProvider, ConfiguredModelAdapter>,
}

#[derive(Clone)]
struct ConfiguredModelAdapter {
    adapter: SelectedAdapter,
    model: String,
    secrets: Vec<Arc<SecretLease>>,
}

impl SelectedModelAdapter {
    pub(crate) fn new(
        _trusted: &TrustedExtensionToken,
        descriptor: ExtensionDescriptor,
        providers: impl IntoIterator<
            Item = (
                ConfigProvider,
                SelectedAdapter,
                String,
                Vec<Arc<SecretLease>>,
            ),
        >,
        extensions: EffectiveExtensionConfig,
    ) -> Result<Self, ExtensionError> {
        descriptor.validate()?;
        if descriptor.extension_point() != ExtensionPoint::ModelAdapter {
            return Err(ExtensionError::ExtensionPointConflict {
                extension: descriptor.reference(),
                expected: ExtensionPoint::ModelAdapter,
                observed: descriptor.extension_point(),
            });
        }
        if extensions.selection(ExtensionPoint::ModelAdapter) != &descriptor.reference() {
            return Err(ExtensionError::UnknownSelection(
                extensions.selection(ExtensionPoint::ModelAdapter).clone(),
            ));
        }
        Ok(Self {
            providers: providers
                .into_iter()
                .map(|(provider, adapter, model, secrets)| {
                    (
                        provider,
                        ConfiguredModelAdapter {
                            adapter,
                            model,
                            secrets,
                        },
                    )
                })
                .collect(),
        })
    }

    fn select(&self, provider: ConfigProvider) -> Result<SelectedAdapter, ExecutorError> {
        self.providers
            .get(&provider)
            .map(|configured| configured.adapter.clone())
            .ok_or(ExecutorError::ProviderUnavailable(provider))
    }

    #[cfg(test)]
    pub(crate) fn selected_is_openai_subscription(&self, provider: ConfigProvider) -> bool {
        matches!(
            self.select(provider),
            Ok(SelectedAdapter::OpenAiSubscription(_))
        )
    }

    fn configured(&self, provider: ConfigProvider) -> &ConfiguredModelAdapter {
        self.providers
            .get(&provider)
            .expect("effective provider was validated before execution")
    }

    fn provider_name(&self, provider: ConfigProvider) -> &str {
        self.configured(provider)
            .adapter
            .provider_name()
            .expect("selected adapters have a provider name")
    }

    fn model_name(&self, provider: ConfigProvider) -> &str {
        &self.configured(provider).model
    }

    fn max_output_tokens(&self, provider: ConfigProvider) -> u32 {
        match &self.configured(provider).adapter {
            SelectedAdapter::OpenAi(adapter) => adapter.max_output_tokens().unwrap_or(u32::MAX),
            SelectedAdapter::OpenAiSubscription(_) => u32::MAX,
            SelectedAdapter::Anthropic(adapter) => adapter.max_output_tokens(),
            SelectedAdapter::OpenRouter(adapter) => adapter.max_output_tokens().unwrap_or(u32::MAX),
            SelectedAdapter::Ollama(adapter) => adapter.max_output_tokens().unwrap_or(u32::MAX),
            #[cfg(debug_assertions)]
            SelectedAdapter::Deterministic(_) => u32::MAX,
        }
    }

    fn sampling_dispatch_proven(
        &self,
        provider: ConfigProvider,
        policy: &crate::protocols::mcp::config::McpSamplingResponderConfig,
    ) -> bool {
        let output_cap = self.max_output_tokens(provider) != u32::MAX;
        let pricing = policy.pricing.as_ref().is_some_and(|pricing| {
            pricing.valid_for(self.provider_name(provider), self.model_name(provider))
        });
        output_cap && pricing
    }

    fn provider_idempotency_enforced(&self, _provider: ConfigProvider) -> bool {
        #[cfg(debug_assertions)]
        if matches!(
            self.configured(_provider).adapter,
            SelectedAdapter::Deterministic(_)
        ) {
            return true;
        }
        false
    }

    fn secret_leases(&self, provider: ConfigProvider) -> Vec<Arc<SecretLease>> {
        self.configured(provider).secrets.clone()
    }

    fn all_secret_leases_named(&self) -> Vec<(String, Arc<SecretLease>)> {
        self.providers
            .iter()
            .flat_map(|(provider, adapter)| {
                adapter
                    .secrets
                    .iter()
                    .enumerate()
                    .map(move |(index, lease)| {
                        (
                            format!("provider:{}:{index}", self.provider_name(*provider)),
                            Arc::clone(lease),
                        )
                    })
            })
            .collect()
    }

    fn secret_custody(&self) -> SecretCustody {
        SecretCustody::new_named("model-adapters", self.all_secret_leases_named())
    }

    fn retain_reasoning_summaries(&self, _provider: ConfigProvider) -> bool {
        false
    }

    fn cost_table(&self, _provider: ConfigProvider) -> Option<CostTable> {
        #[cfg(debug_assertions)]
        if let SelectedAdapter::Deterministic(adapter) = &self.configured(_provider).adapter {
            return adapter.cost_table();
        }
        None
    }

    fn additional_tool_approval(&self, _provider: ConfigProvider) -> ApprovalState {
        #[cfg(debug_assertions)]
        if let SelectedAdapter::Deterministic(adapter) = &self.configured(_provider).adapter {
            return adapter.additional_tool_approval();
        }
        ApprovalState::NotRequired
    }

    fn deterministic_native_approval(&self, _provider: ConfigProvider) -> bool {
        #[cfg(debug_assertions)]
        if let SelectedAdapter::Deterministic(adapter) = &self.configured(_provider).adapter {
            return adapter.scenario == FakeScenario::NativeCoding && adapter.native_auto_approval;
        }
        false
    }

    fn auth_scope(&self, _provider: ConfigProvider) -> Option<String> {
        #[cfg(debug_assertions)]
        if let SelectedAdapter::Deterministic(adapter) = &self.configured(_provider).adapter {
            return adapter.auth_scope();
        }
        None
    }

    fn failure(&self, code: RunFailureCode, detail: &str) -> RunFailureProjection {
        let secrets = self
            .providers
            .values()
            .flat_map(|provider| provider.secrets.iter().cloned())
            .collect::<Vec<_>>();
        let mut detail = CanaryRedactor::new([])
            .with_secrets(&secrets)
            .redact_text(detail);
        if detail.len() > 512 {
            let mut end = 512;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
        }
        RunFailureProjection { code, detail }
    }

    fn executor_failure(&self, error: &ExecutorError) -> RunFailureProjection {
        let code = if matches!(error, ExecutorError::ProviderUnavailable(_)) {
            RunFailureCode::ProviderUnavailable
        } else {
            RunFailureCode::ExecutionFailed
        };
        self.failure(code, &error.to_string())
    }

    #[cfg(debug_assertions)]
    fn fake_barrier(&self) -> Option<FakeProviderBarrier> {
        self.providers.values().find_map(|configured| {
            if let SelectedAdapter::Deterministic(adapter) = &configured.adapter {
                adapter.fake_barrier()
            } else {
                None
            }
        })
    }

    #[cfg(debug_assertions)]
    pub fn for_test(provider: ConfigProvider, adapter: Arc<FakeProvider>) -> Self {
        let trusted = TrustedExtensionToken::daemon_bootstrap();
        let contracts = ExtensionRegistry::from_descriptors(built_in_descriptors())
            .expect("built-in extension descriptors are valid");
        let extensions = ExtensionConfigStack::built_ins()
            .materialize(&contracts)
            .expect("built-in extension configuration is valid");
        let descriptor = contracts
            .get(extensions.selection(ExtensionPoint::ModelAdapter))
            .expect("built-in model adapter descriptor exists")
            .clone();
        let secrets = adapter.secrets.clone();
        Self::new(
            &trusted,
            descriptor,
            [(
                provider,
                SelectedAdapter::Deterministic(Box::new((*adapter).clone())),
                "fake-deterministic-v1".to_owned(),
                secrets,
            )],
            extensions,
        )
        .expect("built-in deterministic adapter selection is valid")
    }
}

#[derive(Clone)]
pub(crate) enum SelectedAdapter {
    OpenAi(OpenAIAdapter),
    OpenAiSubscription(OpenAiSubscriptionAdapter),
    Anthropic(AnthropicAdapter),
    OpenRouter(OpenRouterAdapter),
    Ollama(OllamaAdapter),
    #[cfg(debug_assertions)]
    Deterministic(Box<FakeProvider>),
}

#[cfg(debug_assertions)]
impl SelectedAdapter {
    fn bind_native_revision(&mut self, revision: String) {
        if let Self::Deterministic(provider) = self {
            provider.native_revision = Some(revision);
        }
    }
}

impl ModelAdapter for SelectedAdapter {
    type Session = SelectedSession;

    fn start_session<'life0, 'async_trait>(
        &'life0 self,
        config: SessionConfig,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Session, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            Ok(match self {
                Self::OpenAi(adapter) => {
                    SelectedSession::OpenAi(adapter.start_session(config).await?)
                }
                Self::OpenAiSubscription(adapter) => {
                    SelectedSession::OpenAiSubscription(adapter.start_session(config).await?)
                }
                Self::Anthropic(adapter) => {
                    SelectedSession::Anthropic(adapter.start_session(config).await?)
                }
                Self::OpenRouter(adapter) => {
                    SelectedSession::OpenRouter(adapter.start_session(config).await?)
                }
                Self::Ollama(adapter) => {
                    SelectedSession::Ollama(adapter.start_session(config).await?)
                }
                #[cfg(debug_assertions)]
                Self::Deterministic(adapter) => {
                    SelectedSession::Deterministic(adapter.start_session(config).await?)
                }
            })
        })
    }

    fn provider_name(&self) -> Option<&str> {
        match self {
            Self::OpenAi(adapter) => adapter.provider_name(),
            Self::OpenAiSubscription(adapter) => adapter.provider_name(),
            Self::Anthropic(adapter) => adapter.provider_name(),
            Self::OpenRouter(adapter) => adapter.provider_name(),
            Self::Ollama(adapter) => adapter.provider_name(),
            #[cfg(debug_assertions)]
            Self::Deterministic(adapter) => adapter.provider_name(),
        }
    }
}

pub(crate) enum SelectedSession {
    OpenAi(OpenAISession),
    OpenAiSubscription(OpenAiSubscriptionSession),
    Anthropic(AnthropicSession),
    OpenRouter(OpenRouterSession),
    Ollama(OllamaSession),
    #[cfg(debug_assertions)]
    Deterministic(FakeSession),
}

impl ModelSession for SelectedSession {
    type Turn = SelectedTurn;

    fn begin_turn<'life0, 'async_trait>(
        &'life0 mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Turn, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            Ok(match self {
                Self::OpenAi(session) => {
                    SelectedTurn::OpenAi(session.begin_turn(request, cancellation).await?)
                }
                Self::OpenAiSubscription(session) => SelectedTurn::OpenAiSubscription(Box::new(
                    session.begin_turn(request, cancellation).await?,
                )),
                Self::Anthropic(session) => {
                    SelectedTurn::Anthropic(session.begin_turn(request, cancellation).await?)
                }
                Self::OpenRouter(session) => {
                    SelectedTurn::OpenRouter(session.begin_turn(request, cancellation).await?)
                }
                Self::Ollama(session) => {
                    SelectedTurn::Ollama(session.begin_turn(request, cancellation).await?)
                }
                #[cfg(debug_assertions)]
                Self::Deterministic(session) => {
                    SelectedTurn::Deterministic(session.begin_turn(request, cancellation).await?)
                }
            })
        })
    }

    fn model_name(&self) -> Option<&str> {
        match self {
            Self::OpenAi(session) => session.model_name(),
            Self::OpenAiSubscription(session) => session.model_name(),
            Self::Anthropic(session) => session.model_name(),
            Self::OpenRouter(session) => session.model_name(),
            Self::Ollama(session) => session.model_name(),
            #[cfg(debug_assertions)]
            Self::Deterministic(session) => session.model_name(),
        }
    }

    fn prepare_turn(&mut self, request: &mut TurnRequest) -> Result<(), LoopError> {
        match self {
            Self::OpenAi(session) => session.prepare_turn(request),
            Self::OpenAiSubscription(session) => session.prepare_turn(request),
            Self::Anthropic(session) => session.prepare_turn(request),
            Self::OpenRouter(session) => session.prepare_turn(request),
            Self::Ollama(session) => session.prepare_turn(request),
            #[cfg(debug_assertions)]
            Self::Deterministic(session) => session.prepare_turn(request),
        }
    }

    fn structured_output_capability(&self) -> Option<&agentkit_loop::StructuredOutputCapability> {
        match self {
            Self::OpenAi(session) => session.structured_output_capability(),
            Self::OpenAiSubscription(session) => session.structured_output_capability(),
            Self::Anthropic(session) => session.structured_output_capability(),
            Self::OpenRouter(session) => session.structured_output_capability(),
            Self::Ollama(session) => session.structured_output_capability(),
            #[cfg(debug_assertions)]
            Self::Deterministic(session) => session.structured_output_capability(),
        }
    }
}

pub(crate) enum SelectedTurn {
    OpenAi(OpenAITurn),
    OpenAiSubscription(Box<OpenAiSubscriptionTurn>),
    Anthropic(AnthropicTurn),
    OpenRouter(OpenRouterTurn),
    Ollama(OllamaTurn),
    #[cfg(debug_assertions)]
    Deterministic(FakeTurn),
}

impl ModelTurn for SelectedTurn {
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
        match self {
            Self::OpenAi(turn) => turn.next_event(cancellation),
            Self::OpenAiSubscription(turn) => turn.next_event(cancellation),
            Self::Anthropic(turn) => turn.next_event(cancellation),
            Self::OpenRouter(turn) => turn.next_event(cancellation),
            Self::Ollama(turn) => turn.next_event(cancellation),
            #[cfg(debug_assertions)]
            Self::Deterministic(turn) => turn.next_event(cancellation),
        }
    }
}

#[cfg(debug_assertions)]
#[derive(Clone)]
pub struct FakeProvider {
    response: FakeResponse,
    scenario: FakeScenario,
    dispatches: Arc<AtomicU64>,
    secrets: Vec<Arc<SecretLease>>,
    native_revision: Option<String>,
    native_auto_approval: bool,
}

#[cfg(debug_assertions)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FakeScenario {
    Complete,
    Tool,
    ToolInvalid,
    ToolInvalidCheck,
    ReactiveInjection,
    DeferredMcp,
    NativeCoding,
    #[cfg(debug_assertions)]
    ToolBarrier(FakeProviderBarrier),
    Input,
    Approval,
    Auth {
        scope: String,
    },
    #[cfg(debug_assertions)]
    Barrier(FakeProviderBarrier),
}

#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FakeBarrierCheckpoint {
    BeforeClaimRenewal,
    BeforeProviderDispatch,
    AfterProviderDispatch,
    AfterFirstStreamChunk,
    AfterStreamOutcome,
    AfterModelOutcome,
    AfterToolOutcome,
    AfterJournalBoundary,
}

#[cfg(debug_assertions)]
impl FakeBarrierCheckpoint {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "before_claim_renewal" => Self::BeforeClaimRenewal,
            "before_provider_dispatch" => Self::BeforeProviderDispatch,
            "after_provider_dispatch" => Self::AfterProviderDispatch,
            "after_first_stream_chunk" => Self::AfterFirstStreamChunk,
            "after_stream_outcome" => Self::AfterStreamOutcome,
            "after_model_outcome" => Self::AfterModelOutcome,
            "after_tool_outcome" => Self::AfterToolOutcome,
            "after_journal_boundary" => Self::AfterJournalBoundary,
            _ => return None,
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::BeforeClaimRenewal => "before_claim_renewal",
            Self::BeforeProviderDispatch => "before_provider_dispatch",
            Self::AfterProviderDispatch => "after_provider_dispatch",
            Self::AfterFirstStreamChunk => "after_first_stream_chunk",
            Self::AfterStreamOutcome => "after_stream_outcome",
            Self::AfterModelOutcome => "after_model_outcome",
            Self::AfterToolOutcome => "after_tool_outcome",
            Self::AfterJournalBoundary => "after_journal_boundary",
        }
    }
}

#[cfg(debug_assertions)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FakeProviderBarrier {
    root: PathBuf,
    checkpoint: FakeBarrierCheckpoint,
}

#[cfg(debug_assertions)]
impl FakeProviderBarrier {
    pub fn new(root: impl Into<PathBuf>, checkpoint: FakeBarrierCheckpoint) -> Self {
        Self {
            root: root.into(),
            checkpoint,
        }
    }

    pub fn reached_path(&self) -> PathBuf {
        self.root.join("reached")
    }

    pub fn release_path(&self) -> PathBuf {
        self.root.join("release")
    }

    fn wait(&self, checkpoint: FakeBarrierCheckpoint) {
        if checkpoint != self.checkpoint {
            return;
        }
        std::fs::create_dir_all(&self.root).expect("create fake-provider barrier directory");
        std::fs::write(self.reached_path(), checkpoint.as_str())
            .expect("publish fake-provider barrier");
        while !self.release_path().exists() {
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    async fn wait_async(&self, checkpoint: FakeBarrierCheckpoint) {
        if checkpoint != self.checkpoint {
            return;
        }
        std::fs::create_dir_all(&self.root).expect("create fake-provider barrier directory");
        std::fs::write(self.reached_path(), checkpoint.as_str())
            .expect("publish fake-provider barrier");
        while !self.release_path().exists() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    fn observe(&self, kind: &str, key: &str, value: &[u8]) -> bool {
        use std::io::Write as _;

        let directory = self.root.join(kind);
        std::fs::create_dir_all(&directory).expect("create fake-provider observation directory");
        let path = directory.join(blake3::hash(key.as_bytes()).to_hex().as_str());
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(value)
                    .expect("write fake-provider observation");
                file.sync_all().expect("sync fake-provider observation");
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(error) => panic!("record fake-provider observation: {error}"),
        }
    }
}

#[cfg(debug_assertions)]
#[derive(Clone, Debug)]
pub struct FakeResponse {
    pub text: String,
    pub hidden_reasoning: String,
    pub include_reasoning: bool,
    pub usage: Usage,
    pub metadata: MetadataMap,
    pub delay: Duration,
}

#[cfg(debug_assertions)]
impl FakeResponse {
    pub fn completed(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            hidden_reasoning: "SECRET_CHAIN_OF_THOUGHT".to_owned(),
            include_reasoning: true,
            usage: Usage::new(
                TokenUsage::new(4, 2)
                    .with_reasoning_tokens(3)
                    .with_cached_input_tokens(0)
                    .with_cache_write_input_tokens(0),
            )
            .with_cost(CostUsage::new(0.000_006, "USD").with_provider_amount("0.000006")),
            metadata: MetadataMap::new(),
            delay: Duration::ZERO,
        }
    }
}

#[cfg(debug_assertions)]
impl FakeProvider {
    pub fn new(response: FakeResponse) -> Self {
        Self {
            response,
            scenario: FakeScenario::Complete,
            dispatches: Arc::new(AtomicU64::new(0)),
            secrets: Vec::new(),
            native_revision: None,
            native_auto_approval: false,
        }
    }

    pub fn with_scenario(response: FakeResponse, scenario: FakeScenario) -> Self {
        Self {
            response,
            scenario,
            dispatches: Arc::new(AtomicU64::new(0)),
            secrets: Vec::new(),
            native_revision: None,
            native_auto_approval: false,
        }
    }

    pub fn with_secret_leases(
        mut self,
        secrets: impl IntoIterator<Item = Arc<SecretLease>>,
    ) -> Self {
        self.secrets = secrets.into_iter().collect();
        self
    }

    pub fn with_native_auto_approval(mut self, enabled: bool) -> Self {
        self.native_auto_approval = enabled;
        self
    }

    pub fn dispatch_count(&self) -> u64 {
        self.dispatches.load(Ordering::Acquire)
    }
}

#[cfg(debug_assertions)]
impl FakeProvider {
    fn cost_table(&self) -> Option<CostTable> {
        CostTable::new(
            "fake-rates-v1",
            "deterministic-test",
            "fake-deterministic-v1",
            "fake-rates-v1",
            "USD",
            UsageRates {
                uncached_input: Some(CostRate::new(1, 1)),
                visible_output: Some(CostRate::new(1, 1)),
                ..UsageRates::default()
            },
        )
        .ok()
    }

    fn additional_tool_approval(&self) -> ApprovalState {
        if self.scenario == FakeScenario::Approval {
            ApprovalState::Pending
        } else {
            ApprovalState::NotRequired
        }
    }

    fn auth_scope(&self) -> Option<String> {
        match &self.scenario {
            FakeScenario::Auth { scope } => Some(scope.clone()),
            _ => None,
        }
    }

    #[cfg(debug_assertions)]
    fn fake_barrier(&self) -> Option<FakeProviderBarrier> {
        match &self.scenario {
            FakeScenario::ToolBarrier(barrier) | FakeScenario::Barrier(barrier) => {
                Some(barrier.clone())
            }
            _ => None,
        }
    }
}

#[cfg(debug_assertions)]
impl ModelAdapter for FakeProvider {
    type Session = FakeSession;

    fn start_session<'life0, 'async_trait>(
        &'life0 self,
        _config: SessionConfig,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Session, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { Ok(FakeSession(self.clone())) })
    }

    fn provider_name(&self) -> Option<&str> {
        Some("deterministic-test")
    }
}

#[cfg(debug_assertions)]
pub struct FakeSession(FakeProvider);

#[cfg(debug_assertions)]
impl ModelSession for FakeSession {
    type Turn = FakeTurn;

    fn begin_turn<'life0, 'async_trait>(
        &'life0 mut self,
        request: TurnRequest,
        _cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Turn, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let response = self.0.response.clone();
        let scenario = self.0.scenario.clone();
        let native_revision = self.0.native_revision.clone();
        let dispatches = Arc::clone(&self.0.dispatches);
        Box::pin(async move {
            let correlation: Option<crate::agent::driver::restart::EffectCorrelation> = request
                .metadata
                .get(crate::agent::driver::restart::EFFECT_CORRELATION_METADATA)
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|error| LoopError::Provider(error.to_string()))?;
            let key = correlation
                .as_ref()
                .map(|correlation| correlation.idempotency_key.as_str())
                .unwrap_or("deterministic-test");
            #[cfg(debug_assertions)]
            let barrier = match &scenario {
                FakeScenario::ToolBarrier(barrier) | FakeScenario::Barrier(barrier) => {
                    Some(barrier.clone())
                }
                _ => None,
            };
            #[cfg(debug_assertions)]
            if let Some(barrier) = &barrier {
                let encoded = serde_json::to_vec(&correlation)
                    .expect("serialize fake-provider request correlation");
                barrier.observe("request", key, &encoded);
                barrier
                    .wait_async(FakeBarrierCheckpoint::BeforeProviderDispatch)
                    .await;
            }
            #[cfg(debug_assertions)]
            let first_dispatch = barrier.as_ref().is_none_or(|barrier| {
                let encoded = serde_json::to_vec(&correlation)
                    .expect("serialize fake-provider dispatch correlation");
                barrier.observe("dispatch", key, &encoded)
            });
            #[cfg(not(debug_assertions))]
            let first_dispatch = true;
            if first_dispatch {
                dispatches.fetch_add(1, Ordering::Relaxed);
            }
            #[cfg(debug_assertions)]
            if let Some(barrier) = &barrier {
                barrier
                    .wait_async(FakeBarrierCheckpoint::AfterProviderDispatch)
                    .await;
            }
            let transcript = &request.transcript;
            let completed_tools = transcript
                .iter()
                .filter(|item| item.kind == ItemKind::Tool)
                .count();
            let injected = scenario == FakeScenario::ReactiveInjection
                && serde_json::to_string(transcript).is_ok_and(|transcript| {
                    transcript.contains("REQUEST_UNAUTHORIZED")
                        || transcript.contains("request-unauthorized")
                })
                && request
                    .available_tools
                    .iter()
                    .any(|spec| spec.name.0 == "kit_run");
            let turn = match scenario {
                #[cfg(debug_assertions)]
                FakeScenario::ToolBarrier(_)
                    if !transcript.iter().any(|item| item.kind == ItemKind::Tool) =>
                {
                    FakeTurn::tool(response, correlation.as_ref(), native_revision.as_deref())?
                }
                FakeScenario::Tool | FakeScenario::Approval
                    if !transcript.iter().any(|item| item.kind == ItemKind::Tool) =>
                {
                    FakeTurn::tool(response, correlation.as_ref(), native_revision.as_deref())?
                }
                FakeScenario::ToolInvalid
                    if !transcript.iter().any(|item| item.kind == ItemKind::Tool) =>
                {
                    FakeTurn::tool_request(
                        response,
                        correlation.as_ref(),
                        "kit_discover",
                        serde_json::json!({}),
                    )?
                }
                FakeScenario::ToolInvalidCheck
                    if !transcript.iter().any(|item| item.kind == ItemKind::Tool) =>
                {
                    FakeTurn::tool_request(
                        response,
                        correlation.as_ref(),
                        "kit_check",
                        serde_json::json!({}),
                    )?
                }
                FakeScenario::ReactiveInjection if injected && completed_tools == 0 => {
                    FakeTurn::tool_request(
                        response,
                        correlation.as_ref(),
                        "kit_run",
                        serde_json::json!({
                            "argv":["false"],
                            "working_directory":".",
                            "mounts":{"source":"read_only","build":"read_write","temp":"read_write"},
                            "environment":{},"network":"deny","host_compatibility":false,"background":"foreground",
                            "limits":{"cpu_millis":1,"memory_bytes":1,"pids":1,"file_bytes":1,"disk_bytes":1,"io_bytes":1,"output_bytes":1,"wall_time_millis":1}
                        }),
                    )?
                }
                FakeScenario::DeferredMcp if completed_tools < 5 => FakeTurn::deferred_mcp_tool(
                    response,
                    correlation.as_ref(),
                    completed_tools,
                    transcript,
                )?,
                FakeScenario::NativeCoding if completed_tools < 6 => FakeTurn::native_coding_tool(
                    response,
                    correlation.as_ref(),
                    native_revision.as_deref(),
                    completed_tools,
                    transcript,
                )?,
                FakeScenario::Input
                    if transcript
                        .iter()
                        .filter(|item| item.kind == ItemKind::User)
                        .count()
                        < 2 =>
                {
                    FakeTurn::awaiting_input(response)
                }
                _ => FakeTurn::new(
                    response,
                    #[cfg(debug_assertions)]
                    barrier.map(|barrier| (barrier, key.to_owned())),
                ),
            };
            Ok(turn)
        })
    }

    fn model_name(&self) -> Option<&str> {
        Some("fake-deterministic-v1")
    }
}

#[cfg(debug_assertions)]
pub struct FakeTurn {
    events: VecDeque<ModelTurnEvent>,
    delay: Duration,
    delayed: bool,
    #[cfg(debug_assertions)]
    observation: Option<(FakeProviderBarrier, String)>,
}

#[cfg(debug_assertions)]
impl FakeTurn {
    fn new(
        response: FakeResponse,
        #[cfg(debug_assertions)] observation: Option<(FakeProviderBarrier, String)>,
    ) -> Self {
        let reasoning = PartId::new("reasoning");
        let text = PartId::new("text");
        let text_part = Part::Text(
            TextPart::new(response.text.clone()).with_metadata(response.metadata.clone()),
        );
        let mut parts = vec![text_part.clone()];
        if response.include_reasoning {
            parts.insert(
                0,
                Part::Reasoning(ReasoningPart {
                    summary: Some(response.hidden_reasoning.clone()),
                    data: Some(DataRef::inline_text(response.hidden_reasoning.clone())),
                    redacted: false,
                    metadata: MetadataMap::new(),
                }),
            );
        }
        let result = ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![Item::new(ItemKind::Assistant, parts)],
            usage: Some(response.usage.clone()),
            metadata: response.metadata.clone(),
            model: Some("fake-deterministic-v1".to_owned()),
            response_id: Some("fake-response-1".to_owned()),
        };
        let mut events = VecDeque::new();
        if response.include_reasoning {
            events.extend([
                ModelTurnEvent::Delta(Delta::BeginPart {
                    part_id: reasoning.clone(),
                    kind: PartKind::Reasoning,
                }),
                ModelTurnEvent::Delta(Delta::AppendText {
                    part_id: reasoning,
                    chunk: response.hidden_reasoning.clone(),
                }),
                ModelTurnEvent::Delta(Delta::CommitPart {
                    part: Part::Reasoning(ReasoningPart {
                        summary: Some(response.hidden_reasoning),
                        data: None,
                        redacted: false,
                        metadata: MetadataMap::new(),
                    }),
                }),
            ]);
        }
        events.extend([
            ModelTurnEvent::Delta(Delta::BeginPart {
                part_id: text.clone(),
                kind: PartKind::Text,
            }),
            ModelTurnEvent::Delta(Delta::AppendText {
                part_id: text,
                chunk: response.text,
            }),
            ModelTurnEvent::Delta(Delta::CommitPart { part: text_part }),
            ModelTurnEvent::Usage(response.usage),
            ModelTurnEvent::Finished(result),
        ]);
        Self {
            events,
            delay: response.delay,
            delayed: false,
            #[cfg(debug_assertions)]
            observation,
        }
    }

    fn awaiting_input(response: FakeResponse) -> Self {
        let mut turn = Self::new(
            response,
            #[cfg(debug_assertions)]
            None,
        );
        if let Some(ModelTurnEvent::Finished(result)) = turn.events.back_mut() {
            result.metadata.insert(
                "kit.fake.await_input".to_owned(),
                serde_json::Value::Bool(true),
            );
        }
        turn
    }

    fn tool(
        response: FakeResponse,
        correlation: Option<&crate::agent::driver::restart::EffectCorrelation>,
        native_revision: Option<&str>,
    ) -> Result<Self, LoopError> {
        Self::tool_request(
            response,
            correlation,
            "kit_discover",
            serde_json::json!({
                "expected_revision": native_revision.ok_or_else(|| {
                    LoopError::Provider("fake tool scenario requires native revision".into())
                })?,
                "languages": [],
                "roots": [],
                "terms": ["src"],
            }),
        )
    }

    fn native_coding_tool(
        response: FakeResponse,
        correlation: Option<&crate::agent::driver::restart::EffectCorrelation>,
        native_revision: Option<&str>,
        step: usize,
        transcript: &[Item],
    ) -> Result<Self, LoopError> {
        let revision = native_revision.ok_or_else(|| {
            LoopError::Provider("fake native coding scenario requires native revision".into())
        })?;
        let (name, input) = match step {
            0 => (
                "kit_discover",
                serde_json::json!({
                    "expected_revision": revision,
                    "languages": [],
                    "roots": ["src"],
                    "terms": ["src"],
                }),
            ),
            1 => (
                "kit_search",
                serde_json::json!({
                    "expected_revision": revision,
                    "text": "pub mod",
                    "mode": "content",
                    "path_prefixes": ["src"],
                    "languages": ["rust"],
                }),
            ),
            2 => (
                "kit_read",
                serde_json::json!({
                    "expected_revision": revision,
                    "path": "src/lib.rs",
                    "range": {"kind": "full"},
                }),
            ),
            3 => {
                let read = transcript
                    .iter()
                    .rev()
                    .find_map(|item| {
                        item.parts.iter().find_map(|part| match part {
                            agentkit_core::Part::ToolResult(result) => match &result.output {
                                agentkit_core::ToolOutput::Structured(value) => Some(value.clone()),
                                agentkit_core::ToolOutput::Text(value) => {
                                    serde_json::from_str(value).ok()
                                }
                                agentkit_core::ToolOutput::Parts(parts) => {
                                    parts.iter().find_map(|part| match part {
                                        agentkit_core::Part::Structured(part) => {
                                            Some(part.value.clone())
                                        }
                                        _ => None,
                                    })
                                }
                                _ => None,
                            },
                            agentkit_core::Part::Structured(part) => Some(part.value.clone()),
                            _ => None,
                        })
                    })
                    .ok_or_else(|| {
                        LoopError::Provider(
                            "native coding edit requires the public read result".into(),
                        )
                    })?;
                let data = read
                    .get("data")
                    .ok_or_else(|| LoopError::Provider("native read result has no data".into()))?;
                let bytes = data
                    .get("content")
                    .and_then(serde_json::Value::as_array)
                    .ok_or_else(|| {
                        LoopError::Provider("native read result has no inline content".into())
                    })?
                    .iter()
                    .map(|byte| {
                        byte.as_u64()
                            .and_then(|byte| u8::try_from(byte).ok())
                            .ok_or_else(|| {
                                LoopError::Provider(
                                    "native read returned invalid content bytes".into(),
                                )
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let original = String::from_utf8(bytes.clone())
                    .map_err(|_| LoopError::Provider("native read source is not UTF-8".into()))?;
                let replacement = format!(
                    "{original}\npub const DOGFOOD_NATIVE_PATH: &str = \"provider-kernel-native\";\n"
                );
                (
                    "kit_edit",
                    serde_json::json!({
                        "version": 1,
                        "expected_revision": revision,
                        "operations": [{
                            "op": "replace_range",
                            "path": "src/lib.rs",
                            "base_digest": format!("blake3:{}", blake3::hash(&bytes).to_hex()),
                            "range": {"start":0,"end":bytes.len()},
                            "expected": {
                                "encoding": "utf8",
                                "newline": "lf",
                                "text": original,
                                "final_newline": data.get("final_newline").and_then(serde_json::Value::as_bool).unwrap_or(true)
                            },
                            "replacement": {
                                "encoding": "utf8",
                                "newline": "lf",
                                "text": replacement,
                                "final_newline": true
                            },
                            "executable": "preserve"
                        }]
                    }),
                )
            }
            4 => (
                "kit_check",
                serde_json::json!({"profile":"fast","targets":[]}),
            ),
            5 => (
                "kit_run",
                serde_json::json!({
                    "argv":["cargo","metadata","--no-deps","--format-version","1"],
                    "working_directory":".",
                    "mounts":{"source":"read_only","build":"read_write","temp":"read_write"},
                    "environment":{},"network":"deny","host_compatibility":false,"background":"foreground",
                    "limits":{"cpu_millis":1000,"memory_bytes":268435456,"pids":64,"file_bytes":16777216,"disk_bytes":268435456,"io_bytes":67108864,"output_bytes":65536,"wall_time_millis":10000}
                }),
            ),
            _ => unreachable!(),
        };
        Self::tool_request(response, correlation, name, input)
    }

    fn deferred_mcp_tool(
        response: FakeResponse,
        correlation: Option<&crate::agent::driver::restart::EffectCorrelation>,
        step: usize,
        transcript: &[Item],
    ) -> Result<Self, LoopError> {
        let latest = || {
            transcript.iter().rev().find_map(|item| {
                item.parts.iter().find_map(|part| match part {
                    agentkit_core::Part::ToolResult(result) => match &result.output {
                        agentkit_core::ToolOutput::Structured(value) => Some(value.clone()),
                        agentkit_core::ToolOutput::Text(value) => serde_json::from_str(value).ok(),
                        _ => None,
                    },
                    _ => None,
                })
            })
        };
        let (name, input) = match step {
            0 => (
                "tools_search",
                serde_json::json!({"query":"fixture","limit":1}),
            ),
            1 => {
                let handle = latest()
                    .and_then(|value| value.pointer("/results/0/handle").cloned())
                    .ok_or_else(|| LoopError::Provider("MCP search returned no handle".into()))?;
                ("tools_inspect", serde_json::json!({"handle":handle}))
            }
            2 => {
                let handle = latest()
                    .and_then(|value| value.get("handle").cloned())
                    .ok_or_else(|| {
                        LoopError::Provider("MCP inspection returned no handle".into())
                    })?;
                ("tools_bind", serde_json::json!({"handle":handle}))
            }
            3 | 4 => {
                let binding = transcript
                    .iter()
                    .rev()
                    .find_map(|item| {
                        item.parts.iter().find_map(|part| match part {
                            agentkit_core::Part::ToolResult(result) => match &result.output {
                                agentkit_core::ToolOutput::Structured(value) => {
                                    value.get("binding_id").cloned()
                                }
                                _ => None,
                            },
                            _ => None,
                        })
                    })
                    .ok_or_else(|| LoopError::Provider("MCP bind returned no identity".into()))?;
                (
                    "tools_invoke",
                    serde_json::json!({
                        "binding_id":binding,
                        "input": if step == 3 { serde_json::json!({}) } else { serde_json::json!({"text":"hello"}) }
                    }),
                )
            }
            _ => unreachable!(),
        };
        Self::tool_request(response, correlation, name, input)
    }

    fn tool_request(
        response: FakeResponse,
        correlation: Option<&crate::agent::driver::restart::EffectCorrelation>,
        name: &str,
        input: serde_json::Value,
    ) -> Result<Self, LoopError> {
        let _correlation = correlation.ok_or_else(|| {
            LoopError::Provider("fake tool scenario requires durable effect correlation".into())
        })?;
        let invocation = crate::domain::ids::ToolCallId::generate()
            .map_err(|error| LoopError::Provider(error.to_string()))?;
        let call = ToolCallPart {
            id: AgentkitToolCallId::new(invocation.to_string()),
            name: name.to_owned(),
            input,
            metadata: MetadataMap::new(),
        };
        let result = ModelTurnResult {
            finish_reason: FinishReason::ToolCall,
            output_items: vec![Item::new(
                ItemKind::Assistant,
                vec![Part::ToolCall(call.clone())],
            )],
            usage: Some(response.usage.clone()),
            metadata: MetadataMap::new(),
            model: Some("fake-deterministic-v1".to_owned()),
            response_id: Some("fake-tool-response-1".to_owned()),
        };
        Ok(Self {
            events: VecDeque::from([
                ModelTurnEvent::ToolCall(call),
                ModelTurnEvent::Usage(response.usage),
                ModelTurnEvent::Finished(result),
            ]),
            delay: response.delay,
            delayed: false,
            #[cfg(debug_assertions)]
            observation: None,
        })
    }
}

#[cfg(debug_assertions)]
impl ModelTurn for FakeTurn {
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
            if !self.delayed {
                let started = tokio::time::Instant::now();
                while started.elapsed() < self.delay {
                    if cancellation
                        .as_ref()
                        .is_some_and(TurnCancellation::is_cancelled)
                    {
                        return Err(LoopError::Cancelled);
                    }
                    tokio::time::sleep(
                        self.delay
                            .saturating_sub(started.elapsed())
                            .min(Duration::from_millis(10)),
                    )
                    .await;
                }
                self.delayed = true;
            }
            if cancellation
                .as_ref()
                .is_some_and(TurnCancellation::is_cancelled)
            {
                return Err(LoopError::Cancelled);
            }
            let event = self.events.pop_front();
            #[cfg(debug_assertions)]
            if matches!(event, Some(ModelTurnEvent::Finished(_)))
                && let Some((barrier, key)) = &self.observation
            {
                barrier.observe("result", key, b"finished");
            }
            Ok(event)
        })
    }
}

#[derive(Debug)]
pub enum ExecutorError {
    Config(&'static str),
    ProviderUnavailable(ConfigProvider),
    McpStdioServiceUnavailable { profile: String },
    McpBootstrap(crate::protocols::mcp::config::McpBootstrapError),
    Service(ServiceError),
    Store(crate::store::sqlite::append::StoreError),
    Scheduler(SchedulerError),
    Loop(LoopError),
    Restart(crate::agent::driver::restart::RestartError),
    Start(crate::agent::driver::restart::StartError),
    Poll(crate::agent::driver::attempt::PollError),
    Interrupt(crate::agent::providers::interrupt::InterruptError),
    Compile(crate::agent::prompt::CompileError),
    Artifact(String),
    Worker(String),
}

impl fmt::Display for ExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => formatter.write_str(message),
            Self::ProviderUnavailable(provider) => {
                write!(formatter, "model provider {provider:?} is not configured")
            }
            Self::McpStdioServiceUnavailable { profile } => write!(
                formatter,
                "MCP owned-process service is unavailable for profile {profile:?}"
            ),
            Self::McpBootstrap(error) => error.fmt(formatter),
            Self::Service(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            Self::Scheduler(error) => error.fmt(formatter),
            Self::Loop(error) => error.fmt(formatter),
            Self::Restart(error) => error.fmt(formatter),
            Self::Start(error) => error.fmt(formatter),
            Self::Poll(error) => error.fmt(formatter),
            Self::Interrupt(error) => error.fmt(formatter),
            Self::Compile(error) => error.fmt(formatter),
            Self::Artifact(message) | Self::Worker(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for ExecutorError {}

macro_rules! executor_from {
    ($variant:ident, $error:ty) => {
        impl From<$error> for ExecutorError {
            fn from(error: $error) -> Self {
                Self::$variant(error)
            }
        }
    };
}

executor_from!(Service, ServiceError);
executor_from!(Store, crate::store::sqlite::append::StoreError);
executor_from!(Scheduler, SchedulerError);
executor_from!(Loop, LoopError);
executor_from!(Restart, crate::agent::driver::restart::RestartError);
executor_from!(Start, crate::agent::driver::restart::StartError);
executor_from!(Poll, crate::agent::driver::attempt::PollError);
executor_from!(
    Interrupt,
    crate::agent::providers::interrupt::InterruptError
);
executor_from!(Compile, crate::agent::prompt::CompileError);

#[cfg(test)]
mod selector_tests {
    use std::sync::Arc;

    use agentkit_loop::{ModelAdapter, ModelSession, SessionConfig};

    use super::{ExecutorError, SelectedAdapter, SelectedModelAdapter};
    #[cfg(debug_assertions)]
    use super::{FakeProvider, FakeResponse, RunExecutorConfig, runtime_secret_leases_for_scope};
    use crate::{
        agent::extensions::{
            ExtensionConfigStack, ExtensionPoint, ExtensionRegistry, TrustedExtensionToken,
            built_in_descriptors,
        },
        agent::{
            accounting::CostRate,
            providers::config::{ConfiguredProvider, ProviderProfile},
        },
        api::service::RunFailureCode,
        domain::{config::Provider, secret::SecretLease},
        protocols::mcp::config::{McpSamplingPricingPolicy, McpSamplingResponderConfig},
    };

    #[test]
    fn capped_persistent_profiles_prove_sampling_dispatch() {
        let ConfiguredProvider::OpenAi {
            config: openai,
            credential,
        } = ProviderProfile::openai("key".into(), Some("openai-model".into()), None, Some(64))
            .unwrap()
            .configure()
            .unwrap()
        else {
            panic!("expected openai profile")
        };
        let ConfiguredProvider::Ollama(ollama) =
            ProviderProfile::ollama("ollama-model".into(), None, Some(64))
                .configure()
                .unwrap()
        else {
            panic!("expected ollama profile")
        };
        let trusted = TrustedExtensionToken::daemon_bootstrap();
        let contracts = ExtensionRegistry::from_descriptors(built_in_descriptors()).unwrap();
        let extensions = ExtensionConfigStack::built_ins()
            .materialize(&contracts)
            .unwrap();
        let descriptor = contracts
            .get(extensions.selection(ExtensionPoint::ModelAdapter))
            .unwrap()
            .clone();
        let selector = SelectedModelAdapter::new(
            &trusted,
            descriptor,
            [
                (
                    Provider::OpenAi,
                    SelectedAdapter::OpenAi(
                        agentkit_provider_openai::OpenAIAdapter::new(openai).unwrap(),
                    ),
                    "openai-model".to_owned(),
                    vec![credential],
                ),
                (
                    Provider::Ollama,
                    SelectedAdapter::Ollama(
                        agentkit_provider_ollama::OllamaAdapter::new(ollama).unwrap(),
                    ),
                    "ollama-model".to_owned(),
                    Vec::new(),
                ),
            ],
            extensions,
        )
        .unwrap();

        for (provider, name, model, local_free) in [
            (Provider::OpenAi, "openai", "openai-model", false),
            (Provider::Ollama, "ollama", "ollama-model", true),
        ] {
            let amount = u64::from(!local_free);
            let policy = McpSamplingResponderConfig {
                model_id: model.to_owned(),
                approval: Default::default(),
                timeout_millis: 1,
                max_cost_microusd: 1,
                max_tokens: 32,
                max_messages: 1,
                max_content_items: 1,
                max_content_bytes: 1,
                max_output_bytes: 1,
                max_output_content_items: 1,
                max_system_prompt_bytes: 1,
                max_stop_sequences: 0,
                max_stop_sequence_bytes: 1,
                max_temperature: 1.0,
                pricing: Some(McpSamplingPricingPolicy {
                    version: "test-v1".to_owned(),
                    provider: name.to_owned(),
                    model: model.to_owned(),
                    tokenizer_bytes_per_token: 1,
                    input: CostRate::new(amount, 1),
                    cache_read: CostRate::new(amount, 1),
                    cache_write: CostRate::new(amount, 1),
                    output: CostRate::new(amount, 1),
                    reasoning: CostRate::new(amount, 1),
                    local_free,
                }),
            };
            assert_eq!(selector.max_output_tokens(provider), 64);
            assert!(selector.sampling_dispatch_proven(provider, &policy));
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn executor_config_uses_the_injected_native_semantic_evidence_store() {
        let directory = std::env::temp_dir().join(format!(
            "kit-executor-semantic-evidence-{}",
            crate::domain::ids::RunId::generate().unwrap()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let database = directory.join("state.sqlite3");
        let artifacts = Arc::new(
            crate::store::artifacts::ArtifactStore::open(directory.join("artifacts")).unwrap(),
        );
        let store = Arc::new(std::sync::Mutex::new(
            crate::test_support::open_service_store(&database).unwrap(),
        ));
        let scheduler = crate::runtime::scheduler::DurableScheduler::open(&database).unwrap();
        let evidence =
            crate::capabilities::native::dispatch::NativeSemanticEvidenceStore::default();
        let config = RunExecutorConfig::new(
            &database,
            artifacts,
            store,
            scheduler,
            SelectedModelAdapter::for_test(
                Provider::OpenAi,
                Arc::new(FakeProvider::new(FakeResponse::completed("test"))),
            ),
        )
        .with_native_semantic_evidence(evidence.clone());

        assert!(config.native_semantic_evidence.shares_state_with(&evidence));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(debug_assertions)]
    #[test]
    fn http_credentials_are_lazy_and_callback_scanners_remain_server_scoped() {
        use crate::{
            domain::ids::{AttemptId, PrincipalId, ProjectId, RunId, WorkspaceId},
            executor::profile::{
                Architecture, ExecutorProfile, Platform, ProfileSpec, ResourceLimits, TrustTier,
            },
            protocols::mcp::{
                config::McpServerConfig,
                responders::{CallbackSecretRegistry, ResponderOutcomes},
            },
        };

        let root = std::env::temp_dir().join(format!(
            "kit-runtime-secret-sources-{}",
            RunId::generate().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("state.sqlite3");
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let run = RunId::generate().unwrap();
        let attempt = AttemptId::generate().unwrap();
        let workspace = WorkspaceId::generate().unwrap();
        let provider_secret = Arc::new(SecretLease::new(b"provider-source-secret".to_vec()));
        let http_variable = format!("KIT_MCP_HTTP_SECRET_{}", std::process::id());
        let stdio_variable = format!("KIT_MCP_STDIO_SECRET_{}", std::process::id());
        // SAFETY: the unique stdio variable is installed and removed within this test.
        unsafe {
            std::env::set_var(&stdio_variable, "stdio-source-secret");
        }

        let owner = serde_json::json!({
            "principal_id": principal,
            "project_id": project,
            "workspace_id": workspace,
        });
        let descriptor = serde_json::json!({
            "kind":"tool", "remote":"test",
            "descriptor_digest":format!("sha256:{}", "0".repeat(64)),
            "effect":"network_egress", "retry_safety":"idempotent",
            "required_grants":["network_egress"], "auth_scopes":[],
            "availability":"available"
        });
        let http: McpServerConfig = serde_json::from_value(serde_json::json!({
            "id":"http-source",
            "transport":{"kind":"http","endpoint":"https://example.com/mcp"},
            "owner":owner,
            "source":"mcp.http", "trust_domain":"example.com", "namespace":"http",
            "version":"1", "credential_handle":format!("env:{http_variable}"),
            "credential_scope":{"kind":"project"},
            "egress":{"scheme":"https","host":"example.com","port":443},
            "descriptors":[descriptor.clone()]
        }))
        .unwrap();
        let mut unrelated = http.clone();
        unrelated.id = "other-tenant".to_owned();
        unrelated.owner.project_id = ProjectId::generate().unwrap();
        unrelated.credential_handle = Some(
            crate::domain::secret::SecretHandle::parse("env:UNRELATED_TENANT_SECRET").unwrap(),
        );
        let platform = if cfg!(target_os = "windows") {
            Platform::Windows
        } else if cfg!(target_os = "macos") {
            Platform::MacOs
        } else {
            Platform::Linux
        };
        let architecture = if cfg!(target_arch = "aarch64") {
            Architecture::Aarch64
        } else {
            Architecture::X86_64
        };
        let profile = ProfileSpec::isolated(
            TrustTier::TrustedLocal,
            platform,
            architecture,
            ResourceLimits::new(
                10_000,
                256 * 1024 * 1024,
                16,
                16 * 1024 * 1024,
                64 * 1024 * 1024,
                64 * 1024 * 1024,
                16 * 1024 * 1024,
                30_000,
            ),
        );
        let profile_digest = ExecutorProfile::new(profile.clone())
            .unwrap()
            .digest()
            .to_string();
        let stdio: McpServerConfig = serde_json::from_value(serde_json::json!({
            "id":"stdio-source",
            "transport":{
                "kind":"stdio", "owned_process_profile":"test-profile",
                "argv":[std::env::current_exe().unwrap()], "profile":profile,
                "profile_digest":profile_digest,
                "environment":{"MCP_TOKEN":{
                    "handle":format!("env:{stdio_variable}"),
                    "credential_scope":{"kind":"project"}
                }}
            },
            "owner":owner,
            "source":"mcp.stdio", "trust_domain":"local", "namespace":"stdio",
            "version":"1", "descriptors":[descriptor]
        }))
        .unwrap();
        let artifacts =
            Arc::new(crate::store::artifacts::ArtifactStore::open(root.join("artifacts")).unwrap());
        let store = Arc::new(std::sync::Mutex::new(
            crate::test_support::open_service_store(&database).unwrap(),
        ));
        let scheduler = crate::runtime::scheduler::DurableScheduler::open(&database).unwrap();
        let config = RunExecutorConfig::new(
            &database,
            artifacts,
            store,
            scheduler,
            SelectedModelAdapter::for_test(
                Provider::OpenAi,
                Arc::new(
                    FakeProvider::new(FakeResponse::completed("test"))
                        .with_secret_leases([provider_secret]),
                ),
            ),
        )
        .with_mcp_servers([http, stdio, unrelated]);
        let leases = runtime_secret_leases_for_scope(
            &config,
            principal,
            project,
            attempt,
            Provider::OpenAi,
            workspace,
        )
        .unwrap();
        let registry = CallbackSecretRegistry::default();
        let mut outcomes = ResponderOutcomes::default();
        for (server, scope) in &leases.scopes {
            outcomes = outcomes
                .with_secret_scope(
                    &registry,
                    principal,
                    project,
                    run,
                    attempt,
                    server,
                    &scope.authorized_handles,
                    &scope.secrets,
                )
                .unwrap();
        }
        let http = outcomes.secret_scanner_for_test("http-source").unwrap();
        assert_ne!(
            http.redact_text("provider-source-secret"),
            "provider-source-secret"
        );
        assert_eq!(http.redact_text("http-source-secret"), "http-source-secret");
        assert_eq!(
            http.redact_text("stdio-source-secret"),
            "stdio-source-secret"
        );
        let stdio = outcomes.secret_scanner_for_test("stdio-source").unwrap();
        assert_ne!(
            stdio.redact_text("provider-source-secret"),
            "provider-source-secret"
        );
        assert_ne!(
            stdio.redact_text("stdio-source-secret"),
            "stdio-source-secret"
        );
        assert_eq!(
            stdio.redact_text("http-source-secret"),
            "http-source-secret"
        );
        assert_eq!(leases.scopes["http-source"].authorized_handles.len(), 3);
        assert_eq!(leases.scopes["stdio-source"].authorized_handles.len(), 3);
        assert!(!leases.resolved.contains_key(
            &crate::domain::secret::SecretHandle::parse(&format!("env:{http_variable}")).unwrap()
        ));
        assert!(outcomes.secret_policy_id_for_test("http-source").is_some());
        assert!(outcomes.secret_policy_id_for_test("stdio-source").is_some());
        assert!(outcomes.secret_scanner_for_test("other-tenant").is_none());

        // SAFETY: removes only the unique variable installed above.
        unsafe {
            std::env::remove_var(stdio_variable);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn concrete_adapter_variants_construct_without_network_calls() {
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut adapters = vec![
            SelectedAdapter::OpenAi(
                agentkit_provider_openai::OpenAIAdapter::new(
                    agentkit_provider_openai::OpenAIConfig::new("test-key", "openai-model"),
                )
                .unwrap(),
            ),
            SelectedAdapter::Anthropic(
                agentkit_provider_anthropic::AnthropicAdapter::new(
                    agentkit_provider_anthropic::AnthropicConfig::new(
                        "test-key",
                        "anthropic-model",
                        128,
                    )
                    .unwrap(),
                )
                .unwrap(),
            ),
            SelectedAdapter::OpenRouter(
                agentkit_provider_openrouter::OpenRouterAdapter::new(
                    agentkit_provider_openrouter::OpenRouterConfig::new(
                        "test-key",
                        "openrouter-model",
                    ),
                )
                .unwrap(),
            ),
            SelectedAdapter::Ollama(
                agentkit_provider_ollama::OllamaAdapter::new(
                    agentkit_provider_ollama::OllamaConfig::new("ollama-model"),
                )
                .unwrap(),
            ),
        ];
        #[cfg_attr(not(debug_assertions), allow(unused_mut))]
        let mut expected = vec![
            ("openai", "openai-model"),
            ("anthropic", "anthropic-model"),
            ("openrouter", "openrouter-model"),
            ("ollama", "ollama-model"),
        ];
        #[cfg(debug_assertions)]
        {
            adapters.push(SelectedAdapter::Deterministic(Box::new(FakeProvider::new(
                FakeResponse::completed("test"),
            ))));
            expected.push(("deterministic-test", "fake-deterministic-v1"));
        }

        for (adapter, (provider, model)) in adapters.into_iter().zip(expected) {
            assert_eq!(adapter.provider_name(), Some(provider));
            let session = adapter
                .start_session(SessionConfig::new("selector-contract"))
                .await
                .unwrap();
            assert_eq!(session.model_name(), Some(model));
        }
    }

    #[tokio::test]
    async fn one_selector_routes_matching_adapter_model_and_secret_metadata() {
        let trusted = TrustedExtensionToken::daemon_bootstrap();
        let contracts = ExtensionRegistry::from_descriptors(built_in_descriptors()).unwrap();
        let extensions = ExtensionConfigStack::built_ins()
            .materialize(&contracts)
            .unwrap();
        let descriptor = contracts
            .get(extensions.selection(ExtensionPoint::ModelAdapter))
            .unwrap()
            .clone();
        let openai_secret = Arc::new(SecretLease::new(b"openai-canary".to_vec()));
        let router_secret = Arc::new(SecretLease::new(b"router-canary".to_vec()));
        let selector = SelectedModelAdapter::new(
            &trusted,
            descriptor,
            [
                (
                    Provider::OpenAi,
                    SelectedAdapter::OpenAi(
                        agentkit_provider_openai::OpenAIAdapter::new(
                            agentkit_provider_openai::OpenAIConfig::new(
                                "openai-canary",
                                "openai-model",
                            ),
                        )
                        .unwrap(),
                    ),
                    "openai-model".to_owned(),
                    vec![Arc::clone(&openai_secret)],
                ),
                (
                    Provider::OpenRouter,
                    SelectedAdapter::OpenRouter(
                        agentkit_provider_openrouter::OpenRouterAdapter::new(
                            agentkit_provider_openrouter::OpenRouterConfig::new(
                                "router-canary",
                                "router-model",
                            ),
                        )
                        .unwrap(),
                    ),
                    "router-model".to_owned(),
                    vec![Arc::clone(&router_secret)],
                ),
            ],
            extensions,
        )
        .unwrap();

        for (provider, name, model, secret) in [
            (
                Provider::OpenAi,
                "openai",
                "openai-model",
                b"openai-canary".as_slice(),
            ),
            (
                Provider::OpenRouter,
                "openrouter",
                "router-model",
                b"router-canary".as_slice(),
            ),
        ] {
            assert_eq!(selector.provider_name(provider), name);
            assert_eq!(selector.model_name(provider), model);
            assert_eq!(selector.secret_leases(provider)[0].expose(), secret);
            let adapter = selector.select(provider).unwrap();
            assert_eq!(adapter.provider_name(), Some(name));
            let session = adapter
                .start_session(SessionConfig::new("per-run-selector"))
                .await
                .unwrap();
            assert_eq!(session.model_name(), Some(model));
        }
        assert!(matches!(
            selector.select(Provider::Anthropic),
            Err(ExecutorError::ProviderUnavailable(Provider::Anthropic))
        ));
        let failure = selector.failure(
            RunFailureCode::ExecutionFailed,
            &format!("provider rejected openai-canary {}", "x".repeat(600)),
        );
        assert!(!failure.detail.contains("openai-canary"));
        assert!(failure.detail.contains("[REDACTED]"));
        assert!(failure.detail.len() <= 512);
    }
}
