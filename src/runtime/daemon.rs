use std::{
    collections::BTreeMap,
    fmt, fs,
    future::IntoFuture,
    io::{self, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agentkit_provider_anthropic::{AnthropicAdapter, AnthropicConfig};
use agentkit_provider_ollama::{OllamaAdapter, OllamaConfig};
use agentkit_provider_openai::{OpenAIAdapter, OpenAIConfig};
use agentkit_provider_openrouter::{OpenRouterAdapter, OpenRouterConfig};
use axum::{extract::ConnectInfo, http::request::Parts};
use serde::{Deserialize, Serialize};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};

use crate::{
    agent::{
        executor::{RunExecutor, RunExecutorConfig, SelectedAdapter, SelectedModelAdapter},
        extensions::{
            ContentDigest, ExtensionConfigStack, ExtensionPoint, ExtensionRegistry,
            TrustedExtensionToken, built_in_descriptors,
        },
    },
    api::{
        auth::{
            contract::{
                AuthDecision, AuthReadiness, Authenticator, GrantSnapshot, PrincipalGrant,
                ScopedAuthorizer,
            },
            loopback::{
                LoopbackAuthenticator, LoopbackObservation, LoopbackReplayPolicy,
                LoopbackRequestTime,
            },
        },
        http::{
            core::{HttpAuthenticator, ServiceHandler},
            exec::ManagerExecService,
            health::HealthState,
            router::{RouterConfig, daemon_router_with_services},
        },
        service::{
            ArtifactService, CapabilityService, Command, DeletionEffect, LeaseService, Scheduler,
            Service, ServiceError, SqliteServiceStore,
        },
        stream::{CursorKey, SqliteStreamAdapter, StreamCancellation, StreamConfig},
    },
    domain::{
        config::{Grant, Provider as ConfigProvider, StaticRunConfigMaterializer},
        ids::{PrincipalId, ProjectId},
        secret::{SecretHandle, SecretLease},
    },
    executor::{
        cancel::SqliteCancellationCoordinator,
        process::own::{ProcessRegistrationContext, ProcessRegistry, ProcessRegistryRegistration},
        terminal::{NativePtyDriver, SqliteTerminalSnapshotStore, TerminalManager},
    },
    runtime::{
        backup::{BackupRuntime, BackupRuntimeConfig, BackupRuntimeError, SystemBackupClock},
        lease::{LeaseError, LocalLeaseRuntime, ReconciliationAction, StateRootLockError},
        scheduler::DurableScheduler,
        telemetry::{InstrumentedRuntime, TelemetryReadinessPolicy, TelemetryRuntime},
    },
    store::artifacts::{
        ArtifactDigest, ArtifactError, ArtifactStore, Reachability, now_unix_micros,
    },
    store::backup::{BackupConfig, BackupGeneration, BackupManager},
    store::sqlite::idempotency::IdempotencyKey,
    telemetry::otel::{
        AttributeValue, DropPolicy, DurableLocalExporter, LogRecord, LogSeverity, Resource,
        TelemetryItem,
    },
};

#[cfg(debug_assertions)]
use crate::agent::executor::{FakeBarrierCheckpoint, FakeProviderBarrier};
#[cfg(debug_assertions)]
use crate::agent::executor::{FakeProvider, FakeResponse, FakeScenario};

pub const DISCOVERY_FILE: &str = "daemon.json";
const IDENTITY_FILE: &str = "daemon-identity.json";
const DATABASE_FILE: &str = "state.sqlite3";
pub const TELEMETRY_FILE: &str = "telemetry.otel.enc";

#[derive(Clone)]
pub struct ExecutorRuntimeServices {
    registry: Arc<dyn ProcessRegistry>,
    cancellation: SqliteCancellationCoordinator,
}

impl ExecutorRuntimeServices {
    pub fn process_registration(
        &self,
        context: ProcessRegistrationContext,
    ) -> ProcessRegistryRegistration {
        ProcessRegistryRegistration::new(Arc::clone(&self.registry), context)
    }

    pub const fn cancellation_coordinator(&self) -> &SqliteCancellationCoordinator {
        &self.cancellation
    }
}

#[derive(Clone)]
pub(crate) struct ControlPlaneAuthority(());

impl ControlPlaneAuthority {
    fn new() -> Self {
        Self(())
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn for_test() -> Self {
        Self::new()
    }
}

pub struct DaemonSignal {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    interrupt: tokio::signal::windows::CtrlC,
}

impl DaemonSignal {
    pub fn install() -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                interrupt: tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::interrupt(),
                )?,
                terminate: tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate(),
                )?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                interrupt: tokio::signal::windows::ctrl_c()?,
            })
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
        #[cfg(windows)]
        {
            self.interrupt.recv().await;
        }
    }
}

struct DaemonRuntime {
    scheduler: DurableScheduler,
    artifacts: Arc<ArtifactStore>,
    backup_snapshot: Arc<Mutex<()>>,
    executor: Arc<RunExecutor>,
}

impl Scheduler for DaemonRuntime {
    fn admit_command(
        &self,
        principal_id: PrincipalId,
        idempotency_key: &IdempotencyKey,
        command: &Command,
    ) -> Result<(), ServiceError> {
        self.scheduler
            .admit_command(principal_id, idempotency_key, command)
    }

    fn command_rejected(
        &self,
        principal_id: PrincipalId,
        idempotency_key: &IdempotencyKey,
        command: &Command,
    ) {
        self.scheduler
            .command_rejected(principal_id, idempotency_key, command);
    }

    fn command_committed(
        &self,
        principal_id: PrincipalId,
        idempotency_key: &IdempotencyKey,
        command: &Command,
    ) -> Result<(), ServiceError> {
        self.scheduler
            .command_committed(principal_id, idempotency_key, command)?;
        let message_prompt = match command {
            Command::StartRun { input, .. } => ArtifactDigest::parse(input.as_str())
                .ok()
                .and_then(|digest| self.artifacts.open_verified(digest).ok())
                .is_some_and(|artifact| {
                    artifact.manifest().media_type == "text/plain; charset=utf-8"
                }),
            _ => false,
        };
        if message_prompt
            || matches!(
                command,
                Command::CancelRun { .. }
                    | Command::ProvideRunInput { .. }
                    | Command::ResolveApproval { .. }
                    | Command::ResolveAuth { .. }
            )
        {
            self.executor.notify();
        }
        Ok(())
    }
}

impl CapabilityService for DaemonRuntime {}
impl LeaseService for DaemonRuntime {}

impl ArtifactService for DaemonRuntime {
    fn commit_verified<T>(
        &self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        command: &Command,
        commit: impl FnOnce() -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        let _snapshot = self
            .backup_snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.artifacts
            .commit_verified(principal_id, project_id, command, commit)
    }
}

#[derive(Clone)]
pub struct DaemonConfig {
    pub state_root: PathBuf,
    pub project_root: PathBuf,
    pub bind_addr: SocketAddr,
    pub router: RouterConfig,
    pub backup_destination: PathBuf,
    pub backup_runtime: BackupRuntimeConfig,
    pub backup_retain_generations: usize,
    pub telemetry_capacity: usize,
    pub telemetry_max_bytes: usize,
    pub telemetry_readiness: TelemetryReadinessPolicy,
    pub auth_session_lifetime: Duration,
    pub auth_replay_window: Duration,
    pub auth_replay_capacity: usize,
    pub model_adapter: Option<DaemonModelAdapterConfig>,
    pub evaluation_anchor: Option<Arc<dyn crate::evaluation::reports::LedgerAnchor>>,
    pub native_container_image: Option<String>,
    pub verification_registry: crate::verify::profiles::VerificationRegistry,
    pub native_formatter_descriptor: Option<crate::workspace::edit::format::FormatterDescriptor>,
    pub native_formatter_required: bool,
    pub native_diagnostic_adapters: BTreeMap<String, crate::verify::feedback::DiagnosticAdapter>,
    pub native_feedback_limits: crate::verify::feedback::FeedbackLimits,
    pub native_edit_validation_time: Duration,
    native_config_error: Option<String>,
    model_config_error: Option<String>,
    #[cfg(debug_assertions)]
    pub native_check_completions: Vec<crate::executor::check::ConformanceCheck>,
}

#[derive(Clone)]
pub struct DaemonModelAdapterConfig {
    pub extensions: ExtensionConfigStack,
    pub schema_digest: ContentDigest,
    pub implementation_digest: ContentDigest,
    default_provider: ConfigProvider,
    implementations: BTreeMap<ConfigProvider, ModelAdapterImplementation>,
}

#[derive(Clone)]
enum ModelAdapterImplementation {
    OpenAi {
        config: OpenAIConfig,
        credential: Arc<SecretLease>,
    },
    Anthropic {
        config: AnthropicConfig,
        credential: Arc<SecretLease>,
    },
    OpenRouter {
        config: OpenRouterConfig,
        credential: Arc<SecretLease>,
    },
    Ollama(OllamaConfig),
    #[cfg(debug_assertions)]
    DeterministicTest {
        response: FakeResponse,
        scenario: FakeScenario,
        native_auto_approval: bool,
    },
}

impl DaemonConfig {
    pub fn new(state_root: impl Into<PathBuf>) -> Self {
        assert!(
            !crate::domain::config::GrammarEditExperiment::default().enabled,
            "grammar edit experiment must default off at boot"
        );
        let state_root = state_root.into();
        let (model_adapter, model_config_error) = match configured_model_adapter() {
            Ok(config) => (config, None),
            Err(error) => (None, Some(error)),
        };
        let project_root = std::env::var_os("KIT_PROJECT_ROOT")
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| state_root.join("unconfigured-project"));
        let native_config = load_native_config(&project_root);
        let (
            verification_registry,
            native_diagnostic_adapters,
            native_edit_validation_time,
            native_config_error,
        ) = match native_config {
            Ok(Some((registry, adapters, validation_time))) => {
                (registry, adapters, validation_time, None)
            }
            Ok(None) => (
                crate::verify::profiles::VerificationRegistry::empty(),
                BTreeMap::new(),
                crate::workspace::edit::ir::EditLimits::default().max_validation_time,
                None,
            ),
            Err(error) => (
                crate::verify::profiles::VerificationRegistry::empty(),
                BTreeMap::new(),
                crate::workspace::edit::ir::EditLimits::default().max_validation_time,
                Some(error),
            ),
        };
        Self {
            backup_destination: default_backup_destination(&state_root),
            state_root,
            project_root,
            bind_addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            router: RouterConfig::default(),
            backup_runtime: BackupRuntimeConfig::default(),
            backup_retain_generations: 3,
            telemetry_capacity: 1_024,
            telemetry_max_bytes: 16 * 1024 * 1024,
            telemetry_readiness: TelemetryReadinessPolicy::BestEffort,
            auth_session_lifetime: Duration::from_secs(24 * 60 * 60),
            auth_replay_window: Duration::from_secs(60),
            auth_replay_capacity: 4_096,
            model_adapter,
            evaluation_anchor: None,
            native_container_image: std::env::var("KIT_NATIVE_CONTAINER_IMAGE").ok(),
            verification_registry,
            native_formatter_descriptor: None,
            native_formatter_required: false,
            native_diagnostic_adapters,
            native_feedback_limits: crate::verify::feedback::FeedbackLimits::default(),
            native_edit_validation_time,
            native_config_error,
            model_config_error,
            #[cfg(debug_assertions)]
            native_check_completions: match std::env::var("KIT_FAKE_CHECKS").as_deref() {
                Ok("pass") => (0..64)
                    .map(|_| crate::executor::check::ConformanceCheck::pass("", ""))
                    .collect(),
                Ok(sequence) => sequence
                    .split(',')
                    .filter_map(|outcome| match outcome {
                        "pass" => Some(crate::executor::check::ConformanceCheck::pass(
                            "check passed",
                            "",
                        )),
                        "fail" => Some(crate::executor::check::ConformanceCheck::exit(
                            1,
                            "",
                            "check failed",
                        )),
                        "unavailable" => {
                            Some(crate::executor::check::ConformanceCheck::Unavailable)
                        }
                        _ => None,
                    })
                    .collect(),
                Err(_) => Vec::new(),
            },
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

    pub fn with_evaluation_anchor(
        mut self,
        anchor: Arc<dyn crate::evaluation::reports::LedgerAnchor>,
    ) -> Self {
        self.evaluation_anchor = Some(anchor);
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

    #[cfg(debug_assertions)]
    pub fn with_native_check_completions(
        mut self,
        completions: impl IntoIterator<Item = crate::executor::check::ConformanceCheck>,
    ) -> Self {
        self.native_check_completions = completions.into_iter().collect();
        self
    }

    #[cfg(debug_assertions)]
    pub fn with_development_provider(
        mut self,
        provider: ConfigProvider,
        response: FakeResponse,
        scenario: FakeScenario,
    ) -> Self {
        self.project_root = self.state_root.join("native-project");
        self.model_adapter = Some(DaemonModelAdapterConfig::development(
            provider, response, scenario,
        ));
        self
    }
}

impl DaemonModelAdapterConfig {
    fn new(
        default_provider: ConfigProvider,
        implementations: BTreeMap<ConfigProvider, ModelAdapterImplementation>,
    ) -> Self {
        let descriptor = built_in_descriptors()
            .into_iter()
            .find(|descriptor| descriptor.extension_point() == ExtensionPoint::ModelAdapter)
            .expect("built-in model adapter descriptor exists");
        Self {
            extensions: ExtensionConfigStack::built_ins(),
            schema_digest: descriptor.schema_digest().clone(),
            implementation_digest: descriptor.implementation_digest().clone(),
            default_provider,
            implementations,
        }
    }

    #[cfg(debug_assertions)]
    fn development(
        provider: ConfigProvider,
        response: FakeResponse,
        scenario: FakeScenario,
    ) -> Self {
        Self::new(
            provider,
            BTreeMap::from([(
                provider,
                ModelAdapterImplementation::DeterministicTest {
                    response,
                    scenario,
                    native_auto_approval: std::env::var("KIT_FAKE_NATIVE_AUTO_APPROVE").as_deref()
                        == Ok("1"),
                },
            )]),
        )
    }
}

fn configured_model_adapter() -> Result<Option<DaemonModelAdapterConfig>, String> {
    if std::env::var_os("KIT_PROVIDER").is_some() {
        return Ok(configured_environment_model_adapter());
    }
    let Some(registry) = crate::agent::providers::config::ProviderRegistry::load()
        .map_err(|error| format!("persistent provider configuration: {error}"))?
    else {
        return Ok(None);
    };
    let (_, profile) = registry.current();
    let provider = profile.provider();
    let implementation = match profile
        .configure()
        .map_err(|error| format!("persistent provider configuration: {error}"))?
    {
        crate::agent::providers::config::ConfiguredProvider::OpenAi { config, credential } => {
            ModelAdapterImplementation::OpenAi { config, credential }
        }
        crate::agent::providers::config::ConfiguredProvider::Anthropic { config, credential } => {
            ModelAdapterImplementation::Anthropic { config, credential }
        }
        crate::agent::providers::config::ConfiguredProvider::OpenRouter { config, credential } => {
            ModelAdapterImplementation::OpenRouter { config, credential }
        }
        crate::agent::providers::config::ConfiguredProvider::Ollama(config) => {
            ModelAdapterImplementation::Ollama(config)
        }
    };
    Ok(Some(DaemonModelAdapterConfig::new(
        provider,
        BTreeMap::from([(provider, implementation)]),
    )))
}

fn configured_environment_model_adapter() -> Option<DaemonModelAdapterConfig> {
    let selected = std::env::var("KIT_PROVIDER").ok()?;
    #[cfg(debug_assertions)]
    if selected == "deterministic-test" {
        return development_model_adapter();
    }
    let default_provider = parse_provider(&selected)?;
    let mut implementations = BTreeMap::new();
    if let Ok(config) = OpenAIConfig::from_env() {
        let credential = Arc::new(SecretLease::new(config.api_key.as_bytes().to_vec()));
        implementations.insert(
            ConfigProvider::OpenAi,
            ModelAdapterImplementation::OpenAi { config, credential },
        );
    }
    if let Ok(config) = AnthropicConfig::from_env()
        && let Some(credential) = config
            .auth_token
            .as_deref()
            .or(config.api_key.as_deref())
            .map(|value| Arc::new(SecretLease::new(value.as_bytes().to_vec())))
    {
        implementations.insert(
            ConfigProvider::Anthropic,
            ModelAdapterImplementation::Anthropic { config, credential },
        );
    }
    if let Ok(config) = OpenRouterConfig::from_env() {
        let credential = Arc::new(SecretLease::new(config.api_key.as_bytes().to_vec()));
        implementations.insert(
            ConfigProvider::OpenRouter,
            ModelAdapterImplementation::OpenRouter { config, credential },
        );
    }
    if let Ok(config) = OllamaConfig::from_env() {
        implementations.insert(
            ConfigProvider::Ollama,
            ModelAdapterImplementation::Ollama(config),
        );
    }
    implementations
        .contains_key(&default_provider)
        .then(|| DaemonModelAdapterConfig::new(default_provider, implementations))
}

fn parse_provider(value: &str) -> Option<ConfigProvider> {
    match value {
        "openai" => Some(ConfigProvider::OpenAi),
        "anthropic" => Some(ConfigProvider::Anthropic),
        "openrouter" => Some(ConfigProvider::OpenRouter),
        "ollama" => Some(ConfigProvider::Ollama),
        _ => None,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedNativeConfig {
    version: u16,
    edit_validation_wall_time_millis: u64,
    checks: Vec<TrustedCheck>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedCheck {
    id: String,
    class: crate::verify::profiles::CheckClass,
    requirement: crate::verify::profiles::CheckRequirement,
    program: String,
    #[serde(default)]
    arguments: Vec<String>,
    image: String,
    tool_digest: String,
    config_digest: String,
    resources: crate::executor::profile::ResourceLimits,
    #[serde(default)]
    changed_path_prefixes: std::collections::BTreeSet<String>,
    #[serde(default)]
    post_commit_safe: bool,
    diagnostic_adapter: crate::verify::feedback::DiagnosticAdapter,
}

type TrustedNativeServices = (
    crate::verify::profiles::VerificationRegistry,
    BTreeMap<String, crate::verify::feedback::DiagnosticAdapter>,
    Duration,
);

fn load_native_config(project_root: &Path) -> Result<Option<TrustedNativeServices>, String> {
    const MAX_CONFIG_BYTES: u64 = 256 * 1024;
    let path = project_root.join(".kit/native.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("trusted native config metadata: {error}")),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(
            "trusted native config must be a regular file no larger than 262144 bytes".to_owned(),
        );
    }
    let bytes = fs::read(&path).map_err(|error| format!("trusted native config read: {error}"))?;
    let config: TrustedNativeConfig = serde_json::from_slice(&bytes)
        .map_err(|error| format!("trusted native config parse: {error}"))?;
    let validation_time = Duration::from_millis(config.edit_validation_wall_time_millis);
    if config.version != 1
        || validation_time.is_zero()
        || validation_time > crate::capabilities::native::dispatch::MAX_EDIT_VALIDATION_TIME
        || config.checks.is_empty()
        || config.checks.len() > 64
    {
        return Err(
            "trusted native config requires version 1, a 1..=300000 ms edit validation policy, and 1 to 64 checks"
                .to_owned(),
        );
    }
    let mut adapters = BTreeMap::new();
    let mut checks = Vec::with_capacity(config.checks.len());
    for check in config.checks {
        let command = crate::executor::check::CheckCommand::new(
            &check.id,
            check.program,
            check.arguments,
            check.image,
            check.tool_digest,
            check.config_digest,
            check.resources,
        )
        .map_err(|error| format!("trusted native check {}: {error}", check.id))?;
        adapters.insert(check.id.clone(), check.diagnostic_adapter);
        checks.push(
            crate::verify::profiles::DeclaredCheck::new(
                check.class,
                command,
                check.requirement,
                check.changed_path_prefixes,
                check.post_commit_safe,
            )
            .map_err(|error| format!("trusted native check {}: {error}", check.id))?,
        );
    }
    let registry = crate::verify::profiles::VerificationRegistry::new(checks)
        .map_err(|error| format!("trusted native config registry: {error}"))?;
    Ok(Some((registry, adapters, validation_time)))
}

#[cfg(debug_assertions)]
fn development_model_adapter() -> Option<DaemonModelAdapterConfig> {
    let provider = parse_provider(&std::env::var("KIT_FAKE_PROVIDER").ok()?)?;
    let mut response =
        FakeResponse::completed("hello from kit's deterministic development provider");
    if let Ok(delay) = std::env::var("KIT_FAKE_DELAY_MS") {
        response.delay = Duration::from_millis(delay.parse().ok()?);
    }
    let scenario = match std::env::var("KIT_FAKE_SCENARIO").as_deref() {
        Err(_) | Ok("complete") => FakeScenario::Complete,
        Ok("tool") => FakeScenario::Tool,
        Ok("native-coding") => FakeScenario::NativeCoding,
        Ok("tool-barrier") => FakeScenario::ToolBarrier(FakeProviderBarrier::new(
            std::env::var_os("KIT_FAKE_BARRIER_ROOT").map(PathBuf::from)?,
            FakeBarrierCheckpoint::parse(&std::env::var("KIT_FAKE_BARRIER_AT").ok()?)?,
        )),
        Ok("input") => FakeScenario::Input,
        Ok("approval") => FakeScenario::Approval,
        Ok("auth") => FakeScenario::Auth {
            scope: "provider.read".to_owned(),
        },
        Ok("barrier") => FakeScenario::Barrier(FakeProviderBarrier::new(
            std::env::var_os("KIT_FAKE_BARRIER_ROOT").map(PathBuf::from)?,
            FakeBarrierCheckpoint::parse(&std::env::var("KIT_FAKE_BARRIER_AT").ok()?)?,
        )),
        Ok(_) => return None,
    };
    Some(DaemonModelAdapterConfig::development(
        provider, response, scenario,
    ))
}

#[derive(Debug)]
pub enum DaemonError {
    StateRootLock(StateRootLockError),
    Lease(LeaseError),
    Io(io::Error),
    Setup(String),
    Server(io::Error),
    Task(String),
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StateRootLock(error) => error.fmt(f),
            Self::Lease(error) => error.fmt(f),
            Self::Io(error) => write!(f, "daemon I/O failed: {error}"),
            Self::Setup(error) => write!(f, "daemon setup failed: {error}"),
            Self::Server(error) => write!(f, "daemon server failed: {error}"),
            Self::Task(error) => write!(f, "daemon lifecycle task failed: {error}"),
        }
    }
}

impl std::error::Error for DaemonError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::StateRootLock(error) => Some(error),
            Self::Lease(error) => Some(error),
            Self::Io(error) | Self::Server(error) => Some(error),
            Self::Setup(_) | Self::Task(_) => None,
        }
    }
}

#[derive(Clone)]
pub struct ShutdownHandle {
    sender: watch::Sender<bool>,
    health: HealthState,
    stream_cancellation: StreamCancellation,
}

impl ShutdownHandle {
    pub fn request(&self) {
        self.health.set_admission_ready(false);
        self.health.begin_shutdown();
        self.stream_cancellation.cancel();
        let _ = self.sender.send(true);
    }
}

pub struct Daemon {
    endpoint: SocketAddr,
    state_root: PathBuf,
    principal_id: PrincipalId,
    project_id: ProjectId,
    health: HealthState,
    backup: Arc<Mutex<Option<BackupRuntime>>>,
    executor_runtime_services: ExecutorRuntimeServices,
    evaluation: Option<crate::evaluation::ProductionEvaluationService>,
    evaluation_unavailable: Option<String>,
    shutdown: ShutdownHandle,
    task: Option<JoinHandle<Result<Vec<ReconciliationAction>, DaemonError>>>,
}

impl Daemon {
    pub async fn start(
        config: DaemonConfig,
        mut signal: DaemonSignal,
    ) -> Result<Self, DaemonError> {
        if !config.bind_addr.ip().is_loopback() {
            return Err(DaemonError::Setup(
                "daemon listener must use a loopback address".to_owned(),
            ));
        }
        if let Some(error) = &config.native_config_error {
            return Err(DaemonError::Setup(error.clone()));
        }
        if let Some(error) = &config.model_config_error {
            return Err(DaemonError::Setup(error.clone()));
        }
        let authority = ControlPlaneAuthority::new();
        secure_state_root(&config.state_root)?;
        let mut lease_runtime =
            LocalLeaseRuntime::open(&config.state_root, &authority).map_err(map_lease)?;
        let state_root = lease_runtime
            .leases()
            .reconcile_startup()
            .map_err(DaemonError::Lease)
            .and_then(|_| fs::canonicalize(&config.state_root).map_err(DaemonError::Io))?;
        remove_discovery(&state_root)?;

        let identity = load_or_create_identity(&state_root)?;
        let database = state_root.join(DATABASE_FILE);
        let store = SqliteServiceStore::open(&database, &authority)
            .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let scheduler = DurableScheduler::open(&database)
            .map_err(|error| DaemonError::Setup(error.to_string()))?;
        scheduler
            .reconcile_startup()
            .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let (evaluation, evaluation_unavailable) =
            match crate::evaluation::ProductionEvaluationService::open(
                &state_root,
                config.evaluation_anchor.clone(),
            ) {
                Ok(service) => (Some(service), None),
                Err(crate::evaluation::ProductionEvaluationError::Unavailable(detail)) => {
                    (None, Some(detail.to_owned()))
                }
                Err(error) => return Err(DaemonError::Setup(error.to_string())),
            };
        let artifact_store = Arc::new(
            ArtifactStore::open(state_root.join("artifacts"))
                .map_err(|error| DaemonError::Setup(error.to_string()))?,
        );
        let model_adapter_config = config.model_adapter.ok_or_else(|| {
            DaemonError::Setup(
                "model adapter unavailable; configure a profile with `kit provider add`, or set KIT_PROVIDER and its provider settings".to_owned(),
            )
        })?;
        let default_provider = model_adapter_config.default_provider;
        let trusted = TrustedExtensionToken::daemon_bootstrap();
        let mut extension_registry = ExtensionRegistry::default();
        for descriptor in built_in_descriptors() {
            extension_registry
                .register_in_process(&trusted, descriptor)
                .map_err(|error| DaemonError::Setup(error.to_string()))?;
        }
        let effective_extensions = model_adapter_config
            .extensions
            .materialize(&extension_registry)
            .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let model_adapter_reference = effective_extensions.selection(ExtensionPoint::ModelAdapter);
        extension_registry
            .assert_schema(model_adapter_reference, &model_adapter_config.schema_digest)
            .and_then(|_| {
                extension_registry.assert_implementation(
                    model_adapter_reference,
                    &model_adapter_config.implementation_digest,
                )
            })
            .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let descriptor = extension_registry
            .get(model_adapter_reference)
            .expect("materialized extension selection exists")
            .clone();
        let model_adapter = build_model_adapter(
            model_adapter_config.implementations,
            &trusted,
            descriptor,
            effective_extensions,
        )?;
        let telemetry_exporter = DurableLocalExporter::open(
            state_root.join(TELEMETRY_FILE),
            &identity.cursor_key,
            config.telemetry_max_bytes,
        )
        .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let telemetry: Arc<TelemetryRuntime<'static>> = Arc::new(
            TelemetryRuntime::encrypted_local(
                Resource::default(),
                &[],
                config.telemetry_capacity,
                DropPolicy::DropNewest,
                telemetry_exporter,
                config.telemetry_readiness,
            )
            .map_err(|error| DaemonError::Setup(error.to_string()))?,
        );
        let worker_store = Arc::new(Mutex::new(
            SqliteServiceStore::open(&database, &authority)
                .map_err(|error| DaemonError::Setup(error.to_string()))?,
        ));
        let terminal_store = SqliteTerminalSnapshotStore::open(&database)
            .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let terminal_snapshots = terminal_store
            .load()
            .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let terminal_manager =
            TerminalManager::new(identity.project_id, NativePtyDriver::new(), terminal_store);
        let restored_snapshots = terminal_snapshots.clone();
        let restored_controls = terminal_manager
            .restore_snapshots(terminal_snapshots, now_unix_millis()?, |_| Ok(()))
            .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let cancellation_coordinator = SqliteCancellationCoordinator::new(&database);
        cancellation_coordinator
            .reconcile_startup()
            .map_err(|error| DaemonError::Setup(format!("executor recovery: {error}")))?;
        let exec_manager = Arc::new(
            ManagerExecService::open(
                &database,
                terminal_manager,
                cancellation_coordinator.clone(),
            )
            .map_err(|error| DaemonError::Setup(format!("executor API: {error:?}")))?,
        );
        let shutdown_exec_manager = Arc::clone(&exec_manager);
        for (control, snapshot) in restored_controls.into_iter().zip(&restored_snapshots) {
            exec_manager
                .restore_terminal(control, snapshot)
                .map_err(|error| DaemonError::Setup(format!("executor API: {error:?}")))?;
        }
        let mut executor_config = RunExecutorConfig::new(
            &database,
            Arc::clone(&artifact_store),
            worker_store,
            scheduler.clone(),
            model_adapter,
        )
        .with_project_root(&config.project_root)
        .with_process_registry(exec_manager.clone())
        .with_verification_registry(config.verification_registry.clone())
        .with_native_feedback(
            config.native_diagnostic_adapters.clone(),
            config.native_feedback_limits.clone(),
        )
        .with_native_edit_validation_time(config.native_edit_validation_time);
        if let Some(descriptor) = &config.native_formatter_descriptor {
            executor_config = executor_config
                .with_native_formatter(descriptor.clone(), config.native_formatter_required);
        }
        if let Some(image) = &config.native_container_image {
            executor_config = executor_config.with_native_container_image(image);
        }
        #[cfg(debug_assertions)]
        {
            executor_config = executor_config
                .with_native_check_completions(config.native_check_completions.clone());
        }
        executor_config.poll_interval = Duration::from_secs(5);
        executor_config.telemetry = Some(Arc::clone(&telemetry));
        let executor = Arc::new(
            RunExecutor::start(executor_config)
                .map_err(|error| DaemonError::Setup(error.to_string()))?,
        );
        let backup_snapshot = Arc::new(Mutex::new(()));
        let backup_manager = BackupManager::open(BackupConfig {
            state_root: state_root.clone(),
            database_path: database.clone(),
            artifact_root: state_root.join("artifacts"),
            destination: config.backup_destination,
            retain_generations: config.backup_retain_generations,
            backup_expires_at_unix_micros: i64::MAX,
            build_version: env!("CARGO_PKG_VERSION").to_owned(),
        })
        .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let backup_inventory = SqliteServiceStore::open(&database, &authority)
            .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let backup_runtime = Arc::new(Mutex::new(Some(
            BackupRuntime::start(
                backup_manager,
                backup_inventory,
                Arc::clone(&backup_snapshot),
                config.backup_runtime,
                Arc::new(SystemBackupClock),
            )
            .map_err(|error| DaemonError::Setup(error.to_string()))?,
        )));
        let startup_nanos = now_unix_nanos()?;
        telemetry
            .emit(TelemetryItem::Log(LogRecord {
                timestamp_unix_nanos: startup_nanos,
                severity: LogSeverity::Info,
                body: AttributeValue::String("daemon lifecycle".to_owned()),
                attributes: BTreeMap::from([(
                    "kit.daemon.state".to_owned(),
                    AttributeValue::String("started".to_owned()),
                )]),
                trace_id: None,
                span_id: None,
            }))
            .and_then(|_| telemetry.flush().map(|_| ()))
            .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let health = HealthState::new();
        let backup_probe = Arc::clone(&backup_runtime);
        health.install_backup_probe(move || {
            backup_probe
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_ref()
                .map(BackupRuntime::health)
                .unwrap_or_default()
        });
        let telemetry_probe = Arc::clone(&telemetry);
        health.install_telemetry_probe(move || telemetry_probe.health());
        let executor_probe = Arc::clone(&executor);
        health.install_executor_probe(move || executor_probe.health());
        let mut deletion_store = SqliteServiceStore::open(&database, &authority)
            .map_err(|error| DaemonError::Setup(error.to_string()))?;
        deletion_store
            .reconcile_deletion_jobs()
            .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let authorizer = ScopedAuthorizer;
        let exec_service: Arc<dyn crate::api::http::exec::ExecService> = exec_manager.clone();
        let repo_service: Arc<dyn crate::api::http::repo::RepoService> =
            Arc::new(crate::api::http::repo::LazyNativeRepoService::new(
                crate::api::http::repo::NativeRepoOptions {
                    database: database.clone(),
                    project_root: config.project_root.clone(),
                    scratch: state_root.join("repository-runtime"),
                    artifacts: Arc::clone(&artifact_store),
                    principal_id: identity.principal_id,
                    project_id: identity.project_id,
                    provider: default_provider,
                    process_registration: ProcessRegistryRegistration::new(
                        exec_manager.clone(),
                        ProcessRegistrationContext {
                            project_id: identity.project_id,
                            principal_id: identity.principal_id,
                        },
                    ),
                    cancellation: cancellation_coordinator.clone(),
                    container_image: config.native_container_image.clone(),
                    verification_registry: config.verification_registry.clone(),
                    formatter: config.native_formatter_descriptor.clone(),
                    formatter_required: config.native_formatter_required,
                    diagnostic_adapters: config.native_diagnostic_adapters.clone(),
                    feedback_limits: config.native_feedback_limits.clone(),
                    edit_validation_time: config.native_edit_validation_time,
                    #[cfg(debug_assertions)]
                    check_completions: config.native_check_completions.clone(),
                },
                authority.clone(),
            ));
        let service: Arc<dyn ServiceHandler> =
            Arc::new(Mutex::new(Service::with_runtime_and_config(
                store,
                authorizer,
                InstrumentedRuntime::flushing(
                    DaemonRuntime {
                        scheduler: scheduler.clone(),
                        artifacts: artifact_store.clone(),
                        backup_snapshot: Arc::clone(&backup_snapshot),
                        executor: Arc::clone(&executor),
                    },
                    telemetry.clone(),
                ),
                StaticRunConfigMaterializer::for_provider(default_provider),
                &authority,
            )));
        let stream = SqliteStreamAdapter::new(
            &database,
            CursorKey::new(identity.cursor_key),
            StreamConfig::default(),
        )
        .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let listener = TcpListener::bind(config.bind_addr)
            .await
            .map_err(DaemonError::Io)?;
        let endpoint = listener.local_addr().map_err(DaemonError::Io)?;
        let host = endpoint.to_string();
        let origin = format!("http://{host}");
        let grants = GrantSnapshot::new(
            identity.principal_id,
            identity.project_id,
            [
                Grant::WorkspaceRead,
                Grant::WorkspaceWrite,
                Grant::ModelCall,
                Grant::ProcessSpawn,
                Grant::NetworkEgress,
                Grant::VerificationTargeted,
                Grant::VerificationFull,
            ],
        )
        .with_principal_grant(PrincipalGrant::CreateProject)
        .with_principal_grant(PrincipalGrant::ResolveApproval);
        let issued_at = now_unix_secs()?;
        let session_seconds = config.auth_session_lifetime.as_secs();
        let session_lifetime = Duration::from_secs(session_seconds);
        let auth_deadline = tokio::time::Instant::now()
            .checked_add(session_lifetime)
            .ok_or_else(|| DaemonError::Setup("invalid loopback session lifetime".to_owned()))?;
        let replay_seconds = config.auth_replay_window.as_secs();
        let expires_at = issued_at
            .checked_add(session_seconds)
            .filter(|expiry| *expiry != u64::MAX)
            .ok_or_else(|| DaemonError::Setup("invalid loopback session lifetime".to_owned()))?;
        let (loopback, credential) = LoopbackAuthenticator::issue(
            SecretHandle::parse("memory:daemon/loopback")
                .map_err(|error| DaemonError::Setup(error.to_string()))?,
            grants,
            [host.clone()],
            [origin.clone()],
            issued_at,
            expires_at,
            LoopbackReplayPolicy::new(replay_seconds, config.auth_replay_capacity),
        )
        .map_err(|error| DaemonError::Setup(error.to_string()))?;
        let loopback = Arc::new(loopback);
        let auth_readiness = AuthReadiness::new();
        auth_readiness.install_authenticator::<LoopbackObservation<'static>, _>(loopback.as_ref());
        auth_readiness.install_authorizer(&authorizer);

        let transport_authenticator = loopback.clone();
        let authenticator: Arc<dyn HttpAuthenticator> =
            Arc::new(move |parts: &Parts| -> AuthDecision {
                let peer_ip = parts
                    .extensions
                    .get::<ConnectInfo<SocketAddr>>()
                    .map(|peer| peer.0.ip())
                    .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
                let header = |name: &str| {
                    parts
                        .headers
                        .get(name)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                };
                let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
                    return Err(crate::api::auth::contract::AuthDenial::Unauthenticated);
                };
                let Ok(timestamp) = header("x-kit-timestamp").parse::<u64>() else {
                    return Err(crate::api::auth::contract::AuthDenial::Unauthenticated);
                };
                transport_authenticator.authenticate(&LoopbackObservation::from_transport(
                    peer_ip,
                    header("authorization"),
                    header("host"),
                    header("origin"),
                    header("x-kit-nonce").as_bytes(),
                    header("x-kit-signature").as_bytes(),
                    LoopbackRequestTime::new(timestamp, now.as_secs()),
                ))
            });

        let shutdown_health = health.clone();
        let stream_cancellation =
            StreamCancellation::linked(move || shutdown_health.is_shutting_down());
        let app = daemon_router_with_services(
            service,
            authenticator,
            config.router,
            stream,
            health.clone(),
            stream_cancellation.clone(),
            Some(exec_service),
            Some(repo_service),
        );
        let discovery = Discovery {
            endpoint: origin,
            credential: std::str::from_utf8(credential.expose())
                .map_err(|error| DaemonError::Setup(error.to_string()))?,
        };
        write_json_atomic(&state_root, DISCOVERY_FILE, &discovery)?;

        health.set_auth_ready(auth_readiness.is_ready());
        health.set_store_ready(true);
        health.set_lease_ready(true);
        health.set_startup_reconciliation_ready(true);
        health.set_admission_ready(true);

        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let shutdown = ShutdownHandle {
            sender: shutdown_sender,
            health: health.clone(),
            stream_cancellation: stream_cancellation.clone(),
        };
        let task_health = health.clone();
        let task_root = state_root.clone();
        let task_backup_runtime = Arc::clone(&backup_runtime);
        let signal_shutdown = shutdown.sender.clone();
        let task = tokio::spawn(async move {
            let auth_health = task_health.clone();
            let auth_expiry_task = tokio::spawn(async move {
                tokio::time::sleep_until(auth_deadline).await;
                auth_health.set_auth_ready(false);
            });
            let mut lifecycle_shutdown = shutdown_receiver.clone();
            let server_shutdown = shutdown_receiver.clone();
            let deletion_task = tokio::spawn(run_deletion_worker(
                deletion_store,
                artifact_store,
                backup_snapshot,
                shutdown_receiver,
            ));
            let server_result = {
                let server = axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .with_graceful_shutdown(wait_for_shutdown(server_shutdown))
                .into_future();
                tokio::pin!(server);
                tokio::select! {
                    result = &mut server => result,
                    _ = lifecycle_shutdown.changed() => {
                        task_health.set_admission_ready(false);
                        task_health.begin_shutdown();
                        stream_cancellation.cancel();
                        lease_runtime.begin_shutdown();
                        server.await
                    }
                    _ = signal.recv() => {
                        task_health.set_admission_ready(false);
                        task_health.begin_shutdown();
                        stream_cancellation.cancel();
                        lease_runtime.begin_shutdown();
                        let _ = signal_shutdown.send(true);
                        server.await
                    }
                }
            };

            task_health.set_admission_ready(false);
            task_health.begin_shutdown();
            stream_cancellation.cancel();
            lease_runtime.begin_shutdown();
            let discovery_result = remove_discovery(&task_root);
            wait_for_stream_producers(&stream_cancellation).await;
            let telemetry_result = telemetry
                .shutdown()
                .map_err(|error| DaemonError::Setup(error.to_string()));
            let terminal_result = shutdown_exec_manager
                .daemon_died()
                .map_err(|error| DaemonError::Setup(format!("terminal shutdown: {error:?}")));
            let executor_result = executor.shutdown().await;
            let backup = task_backup_runtime
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            let backup_result = match backup {
                Some(backup) => {
                    match tokio::task::spawn_blocking(move || backup.shutdown()).await {
                        Ok(result) => result.map_err(|error| DaemonError::Setup(error.to_string())),
                        Err(error) => Err(DaemonError::Task(error.to_string())),
                    }
                }
                None => Ok(()),
            };
            let scheduler_result = scheduler
                .shutdown()
                .map_err(|error| DaemonError::Setup(error.to_string()));
            deletion_task.abort();
            let _ = deletion_task.await;
            auth_expiry_task.abort();
            let _ = auth_expiry_task.await;
            let reconciliation = lease_runtime.shutdown().map_err(DaemonError::Lease);
            task_health.set_process_loop_healthy(false);
            discovery_result?;
            server_result.map_err(DaemonError::Server)?;
            telemetry_result?;
            terminal_result?;
            executor_result.map_err(|error| DaemonError::Task(error.to_string()))?;
            backup_result?;
            scheduler_result?;
            reconciliation
        });

        let executor_runtime_services = ExecutorRuntimeServices {
            registry: exec_manager,
            cancellation: cancellation_coordinator,
        };
        Ok(Self {
            endpoint,
            state_root,
            principal_id: identity.principal_id,
            project_id: identity.project_id,
            health,
            backup: backup_runtime,
            executor_runtime_services,
            evaluation,
            evaluation_unavailable,
            shutdown,
            task: Some(task),
        })
    }

    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub fn principal_id(&self) -> PrincipalId {
        self.principal_id
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub fn health(&self) -> &HealthState {
        &self.health
    }

    pub fn executor_runtime_services(&self) -> ExecutorRuntimeServices {
        self.executor_runtime_services.clone()
    }

    pub fn evaluation_service(
        &mut self,
    ) -> Result<&mut crate::evaluation::ProductionEvaluationService, &str> {
        self.evaluation.as_mut().ok_or_else(|| {
            self.evaluation_unavailable
                .as_deref()
                .unwrap_or("production evaluation is unavailable")
        })
    }

    pub fn trigger_backup(&self) -> Result<BackupGeneration, BackupRuntimeError> {
        self.backup
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .ok_or(BackupRuntimeError::Stopped)?
            .trigger()
    }

    pub fn shutdown_handle(&self) -> ShutdownHandle {
        self.shutdown.clone()
    }

    pub async fn shutdown(mut self) -> Result<Vec<ReconciliationAction>, DaemonError> {
        self.shutdown.request();
        self.join().await
    }

    pub async fn wait(mut self) -> Result<Vec<ReconciliationAction>, DaemonError> {
        self.join().await
    }

    async fn join(&mut self) -> Result<Vec<ReconciliationAction>, DaemonError> {
        self.task
            .take()
            .expect("daemon lifecycle task is joined once")
            .await
            .map_err(|error| DaemonError::Task(error.to_string()))?
    }
}

fn build_model_adapter(
    implementations: BTreeMap<ConfigProvider, ModelAdapterImplementation>,
    trusted: &TrustedExtensionToken,
    descriptor: crate::agent::extensions::ExtensionDescriptor,
    extensions: crate::agent::extensions::EffectiveExtensionConfig,
) -> Result<SelectedModelAdapter, DaemonError> {
    let mut providers = Vec::with_capacity(implementations.len());
    for (provider, implementation) in implementations {
        let (adapter, model, secrets) = match implementation {
            ModelAdapterImplementation::OpenAi { config, credential } => {
                let model = config.model.clone();
                let adapter = OpenAIAdapter::new(config)
                    .map_err(|error| DaemonError::Setup(error.to_string()))?;
                (SelectedAdapter::OpenAi(adapter), model, vec![credential])
            }
            ModelAdapterImplementation::Anthropic { config, credential } => {
                let model = config.model.clone();
                let adapter = AnthropicAdapter::new(config)
                    .map_err(|error| DaemonError::Setup(error.to_string()))?;
                (SelectedAdapter::Anthropic(adapter), model, vec![credential])
            }
            ModelAdapterImplementation::OpenRouter { config, credential } => {
                let model = config.model.clone();
                let adapter = OpenRouterAdapter::new(config)
                    .map_err(|error| DaemonError::Setup(error.to_string()))?;
                (
                    SelectedAdapter::OpenRouter(adapter),
                    model,
                    vec![credential],
                )
            }
            ModelAdapterImplementation::Ollama(config) => {
                let model = config.model.clone();
                let adapter = OllamaAdapter::new(config)
                    .map_err(|error| DaemonError::Setup(error.to_string()))?;
                (SelectedAdapter::Ollama(adapter), model, Vec::new())
            }
            #[cfg(debug_assertions)]
            ModelAdapterImplementation::DeterministicTest {
                response,
                scenario,
                native_auto_approval,
            } => (
                SelectedAdapter::Deterministic(Box::new(
                    FakeProvider::with_scenario(response, scenario)
                        .with_native_auto_approval(native_auto_approval),
                )),
                "fake-deterministic-v1".to_owned(),
                Vec::new(),
            ),
        };
        providers.push((provider, adapter, model, secrets));
    }
    SelectedModelAdapter::new(trusted, descriptor, providers, extensions)
        .map_err(|error| DaemonError::Setup(error.to_string()))
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.task.is_some() {
            self.shutdown.request();
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
struct Identity {
    principal_id: PrincipalId,
    project_id: ProjectId,
    cursor_key: [u8; 32],
}

#[derive(Serialize)]
struct Discovery<'a> {
    endpoint: String,
    credential: &'a str,
}

fn map_lease(error: LeaseError) -> DaemonError {
    match error {
        LeaseError::StateRoot(error) => DaemonError::StateRootLock(error),
        error => DaemonError::Lease(error),
    }
}

async fn wait_for_shutdown(mut receiver: watch::Receiver<bool>) {
    while !*receiver.borrow() && receiver.changed().await.is_ok() {}
}

async fn wait_for_stream_producers(cancellation: &StreamCancellation) {
    while cancellation.active_producers() != 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn run_deletion_worker(
    mut store: SqliteServiceStore,
    artifacts: Arc<ArtifactStore>,
    backup_snapshot: Arc<Mutex<()>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let worker_id = format!("daemon-{}", std::process::id());
    while !*shutdown.borrow() {
        let _snapshot = backup_snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = store.run_deletion_jobs(&worker_id, 16, |effect| {
            physically_delete(&artifacts, effect)
        });
        drop(_snapshot);
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            _ = shutdown.changed() => {}
        }
    }
}

fn physically_delete(artifacts: &ArtifactStore, effect: &DeletionEffect) -> Result<(), String> {
    let Some(digest) = effect.artifact_digest.as_deref() else {
        return Ok(());
    };
    let digest = ArtifactDigest::parse(digest).map_err(|error| error.to_string())?;
    let report = artifacts
        .collect_garbage(&Reachability {
            now_unix_micros: now_unix_micros().map_err(|error| error.to_string())?,
            ..Reachability::default()
        })
        .map_err(|error| error.to_string())?;
    if report.deleted_artifacts.contains(&digest) {
        return Ok(());
    }
    match artifacts.open_bytes(digest) {
        Err(ArtifactError::Missing(_)) => Ok(()),
        Ok(_) => Err("eligible artifact bytes remained after physical deletion".to_owned()),
        Err(error) => Err(error.to_string()),
    }
}

fn default_backup_destination(state_root: &Path) -> PathBuf {
    let name = state_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("kit-state");
    state_root
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!("{name}.backups"))
}

fn secure_state_root(root: &Path) -> Result<(), DaemonError> {
    fs::create_dir_all(root).map_err(DaemonError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).map_err(DaemonError::Io)?;
    }
    Ok(())
}

fn now_unix_secs() -> Result<u64, DaemonError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| DaemonError::Setup("system clock is before the Unix epoch".to_owned()))
}

fn now_unix_millis() -> Result<u64, DaemonError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| DaemonError::Setup(error.to_string()))?
        .as_millis()
        .try_into()
        .map_err(|_| DaemonError::Setup("system clock exceeds terminal range".to_owned()))
}

fn now_unix_nanos() -> Result<u64, DaemonError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| DaemonError::Setup("system clock is before the Unix epoch".to_owned()))?
        .as_nanos()
        .try_into()
        .map_err(|_| DaemonError::Setup("system clock exceeds telemetry range".to_owned()))
}

fn load_or_create_identity(root: &Path) -> Result<Identity, DaemonError> {
    let path = root.join(IDENTITY_FILE);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| DaemonError::Setup(format!("invalid daemon identity: {error}"))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut cursor_key = [0_u8; 32];
            getrandom::fill(&mut cursor_key)
                .map_err(|error| DaemonError::Setup(error.to_string()))?;
            let identity = Identity {
                principal_id: PrincipalId::generate()
                    .map_err(|error| DaemonError::Setup(error.to_string()))?,
                project_id: ProjectId::generate()
                    .map_err(|error| DaemonError::Setup(error.to_string()))?,
                cursor_key,
            };
            write_json_atomic(root, IDENTITY_FILE, &identity)?;
            Ok(identity)
        }
        Err(error) => Err(DaemonError::Io(error)),
    }
}

fn write_json_atomic(root: &Path, name: &str, value: &impl Serialize) -> Result<(), DaemonError> {
    let mut random = [0_u8; 8];
    getrandom::fill(&mut random).map_err(|error| DaemonError::Setup(error.to_string()))?;
    let temporary = root.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        u64::from_ne_bytes(random)
    ));
    let path = root.join(name);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(DaemonError::Io)?;
    let bytes = serde_json::to_vec(value).map_err(|error| DaemonError::Setup(error.to_string()))?;
    file.write_all(&bytes).map_err(DaemonError::Io)?;
    file.sync_all().map_err(DaemonError::Io)?;
    drop(file);
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(DaemonError::Io(error));
    }
    fs::File::open(root)
        .and_then(|directory| directory.sync_all())
        .map_err(DaemonError::Io)
}

fn remove_discovery(root: &Path) -> Result<(), DaemonError> {
    match fs::remove_file(root.join(DISCOVERY_FILE)) {
        Ok(()) => fs::File::open(root)
            .and_then(|directory| directory.sync_all())
            .map_err(DaemonError::Io),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DaemonError::Io(error)),
    }
}
