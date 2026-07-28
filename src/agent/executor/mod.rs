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

use agentkit_core::{Delta, FinishReason, Item, ItemKind, TurnCancellation, Usage};
use agentkit_loop::{
    AgentEvent, LoopError, LoopInterrupt, LoopObserver, LoopStep, ModelAdapter, ModelSession,
    ModelTurn, ModelTurnEvent, ModelTurnResult, ObservedEvent, SessionConfig, TranscriptEvent,
    TranscriptObserver, TurnRequest, TurnResult,
};
use agentkit_provider_anthropic::{AnthropicAdapter, AnthropicSession, AnthropicTurn};
use agentkit_provider_ollama::{OllamaAdapter, OllamaSession, OllamaTurn};
use agentkit_provider_openai::{OpenAIAdapter, OpenAISession, OpenAITurn};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterSession, OpenRouterTurn};
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
        adapters::tool::{ToolBinding, ToolExecutorAdapter, ToolKernelContext},
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
            RestartProjection, SafeBoundary,
        },
        extensions::{
            EffectiveExtensionConfig, ExtensionDescriptor, ExtensionError, ExtensionPoint,
            TrustedExtensionToken,
        },
        prompt::{PromptInput, TaskContract, compile},
        providers::{
            adapter::{ModelStreamPolicy, StreamCommitFactory, StreamPolicyAdapter},
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
        events::{AttemptState, EntityId, RunState, TraceId},
        ids::{CommandId, EventId, RunId, WorkspaceId},
        lifecycle::AttemptOwnership,
        secret::SecretLease,
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
        redact::CaptureRedactor,
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
    pub concurrency: usize,
    pub queue_capacity: usize,
    pub poll_interval: Duration,
    pub lease_duration: Duration,
    pub claim_renewal_interval: Duration,
    pub model_reservation: Spend,
    pub cancellation_coordinator: Arc<dyn ExecutorCancellationCoordinator>,
    pub telemetry: Option<Arc<crate::runtime::telemetry::TelemetryRuntime<'static>>>,
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
        Self {
            cancellation_coordinator: Arc::new(SqliteCancellationCoordinator::new(
                database.clone(),
            )),
            database,
            artifacts,
            store,
            scheduler,
            model_adapter,
            concurrency: 4,
            queue_capacity: 64,
            poll_interval: Duration::from_millis(250),
            lease_duration: Duration::from_secs(5),
            claim_renewal_interval: Duration::from_secs(1),
            model_reservation: Spend::new(0, 1, 1, 0, 0),
            telemetry: None,
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
            #[cfg(debug_assertions)]
            native_check_completions: Vec::new(),
        }
    }

    pub fn with_project_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.project_root = root.into();
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
    pub fn start(config: RunExecutorConfig) -> Result<Self, ExecutorError> {
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
        if let Some(task) = task {
            task.await
                .map_err(|error| ExecutorError::Worker(error.to_string()))?;
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

struct ClaimHeartbeat {
    stop: Arc<(Mutex<bool>, Condvar)>,
    failure: watch::Receiver<Option<String>>,
    task: Option<std::thread::JoinHandle<()>>,
}

impl ClaimHeartbeat {
    fn start(config: &RunExecutorConfig, claim: crate::api::service::AttemptDriverClaim) -> Self {
        let store = Arc::clone(&config.store);
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
                let renewed = store
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .renew_worker_claim(claim, lease_duration);
                if let Err(error) = renewed {
                    let _ = failure_tx.send(Some(error.to_string()));
                    return;
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

async fn execute_attempt(
    config: &RunExecutorConfig,
    mut job: WorkerRun,
    progress: broadcast::Sender<ProgressEvent>,
) -> Result<AttemptExit, ExecutorError> {
    let mut heartbeat = ClaimHeartbeat::start(config, job.claim);
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
    config.model_adapter.select(snapshot.effective().provider)?;
    let (tool, native_revision) = tool_adapter(config, &job, &snapshot, true)?;
    let prepared_prompt = prepare_prompt(config, &job, &snapshot, native_revision.as_deref())?;
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
            heartbeat.stop()?;
            return cancel_attempt(config, job);
        }
        RecoveryState::OutcomeUnknown(_) => {
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
        Completed(TurnResult),
        Cancelled,
        Failed(&'static str),
        Waiting {
            waiting: crate::agent::driver::waiting::WaitingState,
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
                    return Ok(DriverExit::Completed(result));
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
                    return Ok(DriverExit::Waiting { waiting, target });
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
    let compiled = compile(&PromptInput {
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
    })?;
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
            secrets: config.model_adapter.secret_leases(selected_provider),
            retain_reasoning_summaries,
            ..ModelStreamPolicy::default()
        },
        stream,
    );
    let workspace = workspace_id(job.attempt.id)?;
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
        &snapshot,
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
    let security = ModelSecurity {
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
    };
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
) -> Result<(ToolExecutorAdapter, Option<String>), ExecutorError> {
    let workspace = workspace_id(job.attempt.id)?;
    let descriptors = crate::capabilities::native::NativeCatalog::enabled(snapshot);
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
    let grants = CapabilityGrantSnapshot::new(
        snapshot,
        configured.iter().map(|(descriptor, constraints)| {
            CapabilityGrant::new(
                job.principal_id,
                job.project_id,
                workspace,
                descriptor.identity().clone(),
                descriptor.schema().normalized_digest(),
                descriptor.effect(),
                constraints.clone(),
            )
        }),
        DigestAlgorithm::Sha256,
    );
    let authenticated = AuthenticatedPrincipal::from_grants(GrantSnapshot::new(
        job.principal_id,
        job.project_id,
        snapshot.effective_authority().iter().copied(),
    ));
    let bindings = configured
        .iter()
        .map(|(descriptor, constraints)| {
            let binding = ToolBinding::new(
                descriptor.spec().clone(),
                descriptor.identity().clone(),
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
    let project_root = std::fs::canonicalize(&config.project_root).map_err(|error| {
        ExecutorError::Worker(format!("trusted project root unavailable: {error}"))
    })?;
    let acquisition = Some(
        crate::workspace::acquire::acquire(crate::workspace::acquire::AcquisitionRequest::new(
            project_root.clone(),
            acquired_root,
            crate::workspace::acquire::WorkspaceId::new(workspace.to_string())
                .map_err(|error| ExecutorError::Worker(error.to_string()))?,
            crate::workspace::acquire::OwnerId::new(job.attempt.owner.attempt_id.to_string())
                .map_err(|error| ExecutorError::Worker(error.to_string()))?,
            crate::workspace::acquire::AcquisitionMode::CopyOnWriteSnapshot,
            crate::workspace::acquire::WriterPolicy::Restricted,
        ))
        .map_err(|error| ExecutorError::Worker(error.to_string()))?,
    );
    let native_root = project_root;
    let process_registration = config.process_registry.as_ref().map(|registry| {
        ProcessRegistryRegistration::new(
            Arc::clone(registry),
            ProcessRegistrationContext {
                project_id: job.project_id,
                principal_id: job.principal_id,
            },
        )
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
    let live_cancellation = Arc::new(AtomicBool::new(false));
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
    let mut dispatcher = crate::capabilities::native::dispatch::NativeDispatcher::open(
        native_root,
        &scratch,
        Arc::clone(&config.artifacts),
        authenticated.clone(),
        snapshot.clone(),
        acquisition,
        crate::capabilities::native::dispatch::NativeRuntime {
            workspace_id: workspace,
            process_registration,
            cancellation: SqliteCancellationCoordinator::new(&config.database),
            live_cancellation: Arc::clone(&live_cancellation),
            container_image: config.native_container_image.clone(),
            verification_registry: config.verification_registry.clone(),
            check_runner,
            secrets: config
                .model_adapter
                .secret_leases(snapshot.effective().provider)
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
            edit_validation_time: config.native_edit_validation_time,
            #[cfg(test)]
            run_runner: None,
        },
    )
    .map_err(ExecutorError::Worker)?;
    let native_revision = resolve_native_revision
        .then(|| dispatcher.revision().map_err(ExecutorError::Worker))
        .transpose()?;
    let budget = durable_tool_budget(config, snapshot)?;
    ToolExecutorAdapter::new(
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
        },
        append_store(config)?,
        move |invocation| dispatcher.dispatch(invocation),
    )
    .map(|adapter| (adapter, native_revision))
    .map_err(|error| ExecutorError::Worker(error.to_string()))
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
    let item_bytes =
        serde_json::to_vec(&item).map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let output_artifact = config
        .artifacts
        .put(
            &item_bytes,
            ArtifactMetadata::new(
                "application/vnd.kit.agent-item+json",
                ArtifactClass::File,
                job.principal_id.to_string(),
                job.project_id.to_string(),
                ArtifactRetention::Forever,
                now_unix_micros().map_err(|error| ExecutorError::Artifact(error.to_string()))?,
            )
            .map_err(|error| ExecutorError::Artifact(error.to_string()))?,
        )
        .map_err(|error| ExecutorError::Artifact(error.to_string()))?;
    let output_ref =
        crate::domain::events::ArtifactRef::parse(&output_artifact.digest().to_string())
            .map_err(|error| ExecutorError::Artifact(error.to_string()))?;
    let preview = output_preview(&item);
    let usage = run_accounting(config, &job, prepared)?;
    let snapshot = RunConfigSnapshot::from_canonical_bytes(&job.effective_config)
        .map_err(|error| ExecutorError::Worker(error.to_string()))?;
    let provider = snapshot.effective().provider;
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
        &CaptureRedactor::new(&[]),
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
    finish_attempt(config, job)
}

fn finish_attempt(config: &RunExecutorConfig, mut job: WorkerRun) -> Result<(), ExecutorError> {
    if job.attempt.state != AttemptState::Quiescing {
        job = transition_attempt(config, job.attempt.id, AttemptState::Quiescing)?;
    }
    transition_attempt(config, job.attempt.id, AttemptState::Succeeded)?;
    transition_run(config, job.run.id, RunState::Completed)?;
    config.scheduler.finish_run(job.run.id, false)?;
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
                let actual = accounting_spend(&provisional);
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
    Ok(AttemptExit::Completed)
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
        let result = ModelTurnResult {
            finish_reason: FinishReason::Completed,
            output_items: vec![Item::new(
                ItemKind::Assistant,
                vec![
                    Part::Reasoning(ReasoningPart {
                        summary: Some(response.hidden_reasoning.clone()),
                        data: Some(DataRef::inline_text(response.hidden_reasoning.clone())),
                        redacted: false,
                        metadata: MetadataMap::new(),
                    }),
                    Part::Text(
                        TextPart::new(response.text.clone())
                            .with_metadata(response.metadata.clone()),
                    ),
                ],
            )],
            usage: Some(response.usage.clone()),
            metadata: response.metadata.clone(),
            model: Some("fake-deterministic-v1".to_owned()),
            response_id: Some("fake-response-1".to_owned()),
        };
        Self {
            events: VecDeque::from([
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
                ModelTurnEvent::Delta(Delta::BeginPart {
                    part_id: text.clone(),
                    kind: PartKind::Text,
                }),
                ModelTurnEvent::Delta(Delta::AppendText {
                    part_id: text,
                    chunk: response.text.clone(),
                }),
                ModelTurnEvent::Delta(Delta::CommitPart {
                    part: Part::Text(TextPart::new(response.text).with_metadata(response.metadata)),
                }),
                ModelTurnEvent::Usage(response.usage),
                ModelTurnEvent::Finished(result),
            ]),
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
    use super::{FakeProvider, FakeResponse};
    use crate::{
        agent::extensions::{
            ExtensionConfigStack, ExtensionPoint, ExtensionRegistry, TrustedExtensionToken,
            built_in_descriptors,
        },
        api::service::RunFailureCode,
        domain::{config::Provider, secret::SecretLease},
    };

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
