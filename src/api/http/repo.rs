use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    time::{Duration, Instant},
};

use axum::{
    Extension, Json, Router,
    body::{Body, to_bytes},
    extract::{Path as AxumPath, Request},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::{
    api::{
        auth::contract::{AuthenticatedPrincipal, GrantSnapshot, PrincipalGrant},
        http::{
            core::{JSON_BODY_LIMIT, RouteDescriptor},
            errors::{PROBLEM_MEDIA_TYPE, ProblemDetails},
        },
    },
    capabilities::{
        kernel::{
            grant::{ArgumentConstraints, CapabilityGrant, CapabilityGrantSnapshot},
            identity::DigestAlgorithm,
            invoke::{
                ApprovalState, CanonicalInvocationResult, InvocationEnvelope, InvocationStatus,
            },
        },
        native::{NativeCatalog, NativeTool},
    },
    domain::{
        config::{Grant, LayerStack, Provider, RunConfigContext},
        events::{TraceId, UtcDateTime},
        ids::{
            ApprovalId, AttemptId, CommandId, EventId, ProjectId, RunId, ToolCallId, WorkspaceId,
        },
        lifecycle::{AttemptOwnership, FencingToken},
    },
    executor::{
        cancel::{SqliteCancellationCoordinator, WorkspaceIdentity},
        check::CheckRunner,
        process::own::ProcessRegistryRegistration,
    },
    runtime::daemon::ControlPlaneAuthority,
    store::{
        artifacts::{
            ArtifactClass, ArtifactDigest, ArtifactMetadata, ArtifactReference, ArtifactRetention,
            ArtifactStore, now_unix_micros,
        },
        sqlite::{
            append::SqliteStore,
            idempotency::{CanonicalRequestDigest, IdempotencyKey},
        },
    },
    workspace::acquire::{
        AcquisitionMode, AcquisitionRequest, OwnerId, WorkspaceId as AcquisitionWorkspaceId,
        WriterPolicy, acquire,
    },
};

const SCHEMA_VERSION: u16 = 1;
const HIDDEN_INSTANCE: &str = "/v1/repository";
const REPOSITORY_QUEUE_CAPACITY: usize = 64;
const REPOSITORY_WORKERS: usize = 2;
const AVAILABILITY_TTL: Duration = Duration::from_secs(5);
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const FINALIZATION_RETRIES: usize = 3;
const TERMINAL_EVENT_MIGRATION_VERSION: u16 = 1;
const OPERATION_METADATA_MIGRATION: &str = "repository_operation_metadata";
const OPERATION_METADATA_MIGRATION_VERSION: u16 = 1;
const MIGRATION_PAGE_SIZE: usize = 64;
const STARTUP_MIGRATION_BUDGET: Duration = Duration::from_millis(25);
const BACKGROUND_MIGRATION_BUDGET: Duration = Duration::from_millis(100);

pub const REPO_ROUTES: &[RouteDescriptor] = &[
    route("GET", "/v1/repository/status", "repo.status", false, false),
    route(
        "GET",
        "/v1/projects/{project_id}/repository/revision",
        "repo.revision",
        false,
        false,
    ),
    route(
        "GET",
        "/v1/projects/{project_id}/repository/capabilities",
        "repo.capabilities",
        false,
        false,
    ),
    route(
        "POST",
        "/v1/projects/{project_id}/repository/discover",
        "repo.discover",
        false,
        true,
    ),
    route(
        "POST",
        "/v1/projects/{project_id}/repository/search",
        "repo.search",
        false,
        true,
    ),
    route(
        "POST",
        "/v1/projects/{project_id}/repository/read",
        "repo.read",
        false,
        true,
    ),
    route(
        "POST",
        "/v1/projects/{project_id}/repository/edit",
        "repo.edit",
        true,
        true,
    ),
    route(
        "POST",
        "/v1/projects/{project_id}/repository/run",
        "repo.run",
        true,
        true,
    ),
    route(
        "POST",
        "/v1/projects/{project_id}/repository/check",
        "repo.check",
        true,
        true,
    ),
    route(
        "GET",
        "/v1/repository-results/{result_id}",
        "repo.result",
        false,
        false,
    ),
    route(
        "GET",
        "/v1/repository-results/{result_id}/events",
        "repo.result.events",
        false,
        false,
    ),
    route(
        "POST",
        "/v1/repository-results/{result_id}/approval",
        "repo.result.approval",
        true,
        false,
    ),
    route(
        "POST",
        "/v1/repository-results/{result_id}/cancel",
        "repo.result.cancel",
        true,
        false,
    ),
    route(
        "GET",
        "/v1/repository-artifacts/{artifact_ref}",
        "repo.artifact",
        false,
        false,
    ),
];

const fn route(
    method: &'static str,
    path: &'static str,
    operation: &'static str,
    mutation: bool,
    long_running: bool,
) -> RouteDescriptor {
    RouteDescriptor {
        method,
        path,
        operation,
        mutation,
        long_running,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoError {
    NotFound,
    Invalid(&'static str),
    Conflict,
    Stale,
    Unavailable(String),
    Unsupported(String),
    Internal,
}

pub struct RepoArtifact {
    pub bytes: Vec<u8>,
    pub digest: String,
    pub media_type: String,
    pub class: String,
    pub principal: String,
    pub project: String,
}

pub trait RepoService: Send + Sync + 'static {
    fn status(&self, principal: &AuthenticatedPrincipal) -> Result<Value, RepoError> {
        Ok(json!({
            "schema_version":SCHEMA_VERSION,
            "principal_id":principal.principal_id(),
            "project_id":principal.grant_snapshot().project_id(),
        }))
    }
    fn revision(
        &self,
        principal: &AuthenticatedPrincipal,
        project: ProjectId,
    ) -> Result<Value, RepoError>;
    fn capabilities(
        &self,
        principal: &AuthenticatedPrincipal,
        project: ProjectId,
    ) -> Result<Value, RepoError>;
    fn invoke(
        &self,
        principal: &AuthenticatedPrincipal,
        project: ProjectId,
        tool: NativeTool,
        input: Value,
        key: Option<&IdempotencyKey>,
    ) -> Result<Value, RepoError>;
    fn result(&self, principal: &AuthenticatedPrincipal, id: &str) -> Result<Value, RepoError>;
    fn events(&self, principal: &AuthenticatedPrincipal, id: &str) -> Result<Value, RepoError>;
    fn artifact(
        &self,
        principal: &AuthenticatedPrincipal,
        reference: &str,
    ) -> Result<RepoArtifact, RepoError>;
    fn resolve_approval(
        &self,
        _principal: &AuthenticatedPrincipal,
        _id: &str,
        _approved: bool,
        _key: &IdempotencyKey,
    ) -> Result<Value, RepoError> {
        Err(RepoError::Unsupported(
            "repository_approval_unavailable".to_owned(),
        ))
    }
    fn cancel(
        &self,
        _principal: &AuthenticatedPrincipal,
        _id: &str,
        _key: &IdempotencyKey,
    ) -> Result<Value, RepoError> {
        Err(RepoError::Unsupported(
            "repository_cancellation_unavailable".to_owned(),
        ))
    }
}

pub struct NativeRepoOptions {
    pub database: PathBuf,
    pub project_root: PathBuf,
    pub scratch: PathBuf,
    pub artifacts: Arc<ArtifactStore>,
    pub principal_id: crate::domain::ids::PrincipalId,
    pub project_id: ProjectId,
    pub provider: Provider,
    pub process_registration: ProcessRegistryRegistration,
    pub cancellation: SqliteCancellationCoordinator,
    pub container_image: Option<String>,
    pub verification_registry: crate::verify::profiles::VerificationRegistry,
    pub formatter: Option<crate::workspace::edit::format::FormatterDescriptor>,
    pub formatter_required: bool,
    pub diagnostic_adapters: BTreeMap<String, crate::verify::feedback::DiagnosticAdapter>,
    pub feedback_limits: crate::verify::feedback::FeedbackLimits,
    pub edit_validation_time: Duration,
    #[cfg(debug_assertions)]
    pub check_completions: Vec<crate::executor::check::ConformanceCheck>,
}

struct NativeRepoState {
    dispatcher: crate::capabilities::native::dispatch::NativeDispatcher,
}

struct NativeRepoWorker {
    database: PathBuf,
    artifacts: Arc<ArtifactStore>,
    authority: ControlPlaneAuthority,
    provider: Provider,
    workspace_id: WorkspaceId,
    verification_registry: crate::verify::profiles::VerificationRegistry,
    state: Arc<Mutex<NativeRepoState>>,
    cancellations: Arc<Mutex<BTreeMap<String, Arc<AtomicBool>>>>,
    scheduled: Arc<Mutex<BTreeSet<String>>>,
    queue: Option<Arc<SyncSender<String>>>,
    queue_pump: Option<SyncSender<()>>,
}

impl Clone for NativeRepoWorker {
    fn clone(&self) -> Self {
        Self {
            database: self.database.clone(),
            artifacts: Arc::clone(&self.artifacts),
            authority: self.authority.clone(),
            provider: self.provider,
            workspace_id: self.workspace_id,
            verification_registry: self.verification_registry.clone(),
            state: Arc::clone(&self.state),
            cancellations: Arc::clone(&self.cancellations),
            scheduled: Arc::clone(&self.scheduled),
            queue: None,
            queue_pump: self.queue_pump.clone(),
        }
    }
}

pub struct NativeRepoService {
    worker: NativeRepoWorker,
    project_id: ProjectId,
    availability: Mutex<AvailabilityProbe>,
}

struct AvailabilityProbe {
    root: PathBuf,
    image: Option<String>,
    registry: crate::verify::profiles::VerificationRegistry,
    feedback_configured: bool,
    formatter: Option<crate::workspace::edit::format::FormatterDescriptor>,
    formatter_required: bool,
    syntax_available: bool,
    mechanical_executor: bool,
    cached: Option<AvailabilitySnapshot>,
}

#[derive(Clone)]
struct AvailabilitySnapshot {
    checked: Instant,
    checked_at: i64,
    generation: String,
    unavailable: BTreeMap<NativeTool, Vec<String>>,
    image_digest: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SubmitState {
    Submitted,
    AlreadyScheduled,
    QueueFull,
}

enum LazyNativeRepoState {
    Uninitialized {
        options: Box<NativeRepoOptions>,
        authority: ControlPlaneAuthority,
        semantic_evidence: crate::capabilities::native::dispatch::NativeSemanticEvidenceStore,
    },
    Ready(Box<NativeRepoService>),
    Failed(String),
}

pub(crate) struct LazyNativeRepoService {
    state: Mutex<LazyNativeRepoState>,
    database: PathBuf,
    artifacts: Arc<ArtifactStore>,
}

impl LazyNativeRepoService {
    pub(crate) fn with_semantic_evidence(
        options: NativeRepoOptions,
        authority: ControlPlaneAuthority,
        semantic_evidence: crate::capabilities::native::dispatch::NativeSemanticEvidenceStore,
    ) -> Self {
        let database = options.database.clone();
        let artifacts = Arc::clone(&options.artifacts);
        let state = match migrate(&database) {
            Ok(()) => LazyNativeRepoState::Uninitialized {
                options: Box::new(options),
                authority,
                semantic_evidence,
            },
            Err(error) => LazyNativeRepoState::Failed(error),
        };
        Self {
            state: Mutex::new(state),
            database,
            artifacts,
        }
    }

    fn with_service<T>(
        &self,
        operation: impl FnOnce(&NativeRepoService) -> Result<T, RepoError>,
    ) -> Result<T, RepoError> {
        let mut state = self.state.lock().map_err(|_| RepoError::Internal)?;
        initialize(&mut state);
        match &*state {
            LazyNativeRepoState::Ready(service) => operation(service),
            LazyNativeRepoState::Failed(reason) => Err(RepoError::Unavailable(reason.clone())),
            LazyNativeRepoState::Uninitialized { .. } => unreachable!(),
        }
    }

    #[cfg(test)]
    fn shares_semantic_evidence_with(
        &self,
        evidence: &crate::capabilities::native::dispatch::NativeSemanticEvidenceStore,
    ) -> bool {
        self.state.lock().is_ok_and(|state| match &*state {
            LazyNativeRepoState::Uninitialized {
                semantic_evidence, ..
            } => semantic_evidence.shares_state_with(evidence),
            LazyNativeRepoState::Ready(_) | LazyNativeRepoState::Failed(_) => false,
        })
    }
}

fn initialize(state: &mut LazyNativeRepoState) {
    if !matches!(state, LazyNativeRepoState::Uninitialized { .. }) {
        return;
    }
    let LazyNativeRepoState::Uninitialized {
        options,
        authority,
        semantic_evidence,
    } = std::mem::replace(
        state,
        LazyNativeRepoState::Failed("repository initialization interrupted".to_owned()),
    )
    else {
        unreachable!()
    };
    *state = match NativeRepoService::open(*options, &authority, semantic_evidence) {
        Ok(service) => LazyNativeRepoState::Ready(Box::new(service)),
        Err(error) => LazyNativeRepoState::Failed(error),
    };
}

impl RepoService for LazyNativeRepoService {
    fn status(&self, principal: &AuthenticatedPrincipal) -> Result<Value, RepoError> {
        let mut state = self.state.lock().map_err(|_| RepoError::Internal)?;
        initialize(&mut state);
        let (available, reason) = match &*state {
            LazyNativeRepoState::Ready(_) => (true, None),
            LazyNativeRepoState::Failed(reason) => (false, Some(reason.as_str())),
            LazyNativeRepoState::Uninitialized { .. } => unreachable!(),
        };
        Ok(
            json!({"schema_version":SCHEMA_VERSION,"principal_id":principal.principal_id(),"project_id":principal.grant_snapshot().project_id(),"available":available,"unavailable_reason":reason}),
        )
    }
    fn revision(
        &self,
        principal: &AuthenticatedPrincipal,
        project: ProjectId,
    ) -> Result<Value, RepoError> {
        self.with_service(|service| service.revision(principal, project))
    }

    fn capabilities(
        &self,
        principal: &AuthenticatedPrincipal,
        project: ProjectId,
    ) -> Result<Value, RepoError> {
        self.with_service(|service| service.capabilities(principal, project))
    }

    fn invoke(
        &self,
        principal: &AuthenticatedPrincipal,
        project: ProjectId,
        tool: NativeTool,
        input: Value,
        key: Option<&IdempotencyKey>,
    ) -> Result<Value, RepoError> {
        self.with_service(|service| service.invoke(principal, project, tool, input, key))
    }

    fn result(&self, principal: &AuthenticatedPrincipal, id: &str) -> Result<Value, RepoError> {
        require_workspace_read(principal)?;
        load_reconciled_result(&self.database, &self.artifacts, principal, id, "result")
    }

    fn events(&self, principal: &AuthenticatedPrincipal, id: &str) -> Result<Value, RepoError> {
        require_workspace_read(principal)?;
        load_reconciled_result(&self.database, &self.artifacts, principal, id, "events")
    }

    fn artifact(
        &self,
        principal: &AuthenticatedPrincipal,
        reference: &str,
    ) -> Result<RepoArtifact, RepoError> {
        load_artifact(&self.artifacts, principal, reference)
    }

    fn resolve_approval(
        &self,
        principal: &AuthenticatedPrincipal,
        id: &str,
        approved: bool,
        key: &IdempotencyKey,
    ) -> Result<Value, RepoError> {
        self.with_service(|service| service.resolve_approval(principal, id, approved, key))
    }

    fn cancel(
        &self,
        principal: &AuthenticatedPrincipal,
        id: &str,
        key: &IdempotencyKey,
    ) -> Result<Value, RepoError> {
        self.with_service(|service| service.cancel(principal, id, key))
    }
}

impl NativeRepoService {
    pub(crate) fn open(
        options: NativeRepoOptions,
        authority: &ControlPlaneAuthority,
        semantic_evidence: crate::capabilities::native::dispatch::NativeSemanticEvidenceStore,
    ) -> Result<Self, String> {
        let root = std::fs::canonicalize(&options.project_root)
            .map_err(|error| format!("trusted project root unavailable: {error}"))?;
        let scratch = options.scratch;
        std::fs::create_dir_all(&scratch).map_err(|error| error.to_string())?;
        let workspace_id =
            WorkspaceId::from_stable_bytes(options.project_id.to_string().as_bytes());
        let attempt_id =
            AttemptId::from_stable_bytes(format!("repo:{}", options.project_id).as_bytes());
        let run_id = RunId::from_stable_bytes(format!("repo:{}", options.project_id).as_bytes());
        let attempt = AttemptOwnership::new(attempt_id, options.principal_id, FencingToken::new(1));
        // This bootstrap identity is never dispatched. Each operation binds its
        // persisted caller/config snapshot before entering the kernel.
        let authority_grants = BTreeSet::new();
        let config = LayerStack::safe_defaults_for(options.provider)
            .materialize(
                RunConfigContext {
                    principal_id: options.principal_id,
                    project_id: options.project_id,
                    run_id,
                },
                &authority_grants,
            )
            .map_err(|error| error.to_string())?;
        let authenticated = AuthenticatedPrincipal::from_grants(GrantSnapshot::new(
            options.principal_id,
            options.project_id,
            authority_grants,
        ));
        let constraints = NativeCatalog::all()
            .iter()
            .map(|descriptor| {
                ArgumentConstraints::new([format!(
                    "native={}@{}",
                    descriptor.tool().short_name(),
                    descriptor.identity().version().as_str()
                )
                .into_bytes()])
            })
            .collect::<Vec<_>>();
        let _ = constraints;

        let acquired = scratch.join("acquired");
        std::fs::create_dir_all(&acquired).map_err(|error| error.to_string())?;
        let acquisition = acquire(AcquisitionRequest::new(
            root.clone(),
            std::fs::canonicalize(&acquired).map_err(|error| error.to_string())?,
            AcquisitionWorkspaceId::new(workspace_id.to_string())
                .map_err(|error| error.to_string())?,
            OwnerId::new(attempt_id.to_string()).map_err(|error| error.to_string())?,
            AcquisitionMode::CopyOnWriteSnapshot,
            WriterPolicy::Restricted,
        ))
        .map_err(|error| error.to_string())?;
        let registry_missing = options.verification_registry.is_empty();
        let feedback_missing = options.diagnostic_adapters.is_empty();
        #[cfg(debug_assertions)]
        let mechanical_executor = !options.check_completions.is_empty();
        #[cfg(not(debug_assertions))]
        let mechanical_executor = false;
        let check_runner = (!registry_missing).then(|| {
            #[cfg(debug_assertions)]
            if !options.check_completions.is_empty() {
                return CheckRunner::conformance(options.check_completions.clone());
            }
            CheckRunner::registered_attempt_container(
                attempt,
                options.cancellation.clone(),
                WorkspaceIdentity::from_acquisition(workspace_id, &acquisition),
                options.process_registration.clone(),
            )
        });
        let formatter_descriptor = options.formatter.clone();
        let formatter = options.formatter.map(|descriptor| {
            crate::capabilities::native::dispatch::NativeFormatterRuntime {
                descriptor,
                executor:
                    crate::executor::formatter::FormatterExecutor::registered_attempt_container(
                        attempt,
                        options.cancellation.clone(),
                        WorkspaceIdentity::from_acquisition(workspace_id, &acquisition),
                        options.process_registration.clone(),
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
        if !options.check_completions.is_empty() {
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
        let syntax_available = syntax_executors.iter().all(|executor| executor.available());
        let availability = AvailabilityProbe {
            root: root.clone(),
            image: options.container_image.clone(),
            registry: options.verification_registry.clone(),
            feedback_configured: !feedback_missing,
            formatter: formatter_descriptor,
            formatter_required: options.formatter_required,
            syntax_available,
            mechanical_executor,
            cached: None,
        };
        let verification_registry = options.verification_registry.clone();
        let mut dispatcher = crate::capabilities::native::dispatch::NativeDispatcher::open(
            root,
            &scratch,
            Arc::clone(&options.artifacts),
            authenticated.clone(),
            config.clone(),
            Some(acquisition),
            crate::capabilities::native::dispatch::NativeRuntime {
                workspace_id,
                process_registration: Some(options.process_registration),
                cancellation: options.cancellation,
                live_cancellation: Arc::new(AtomicBool::new(false)),
                container_image: options.container_image,
                verification_registry: options.verification_registry,
                check_runner,
                secrets: Vec::new(),
                syntax_executors,
                formatter_required: options.formatter_required,
                formatter,
                feedback: Some(
                    crate::capabilities::native::dispatch::NativeFeedbackRuntime {
                        database: options.database.clone(),
                        adapters: options.diagnostic_adapters,
                        limits: options.feedback_limits,
                    },
                ),
                semantic_evidence: semantic_evidence.clone(),
                edit_validation_time: options.edit_validation_time,
                #[cfg(test)]
                run_runner: None,
            },
        )?;
        // Probe and cache the managed workspace before the HTTP server starts. A
        // first request must not inherit the cost of indexing a large checkout.
        dispatcher.revision()?;
        let _store =
            SqliteStore::open(&options.database, authority).map_err(|error| error.to_string())?;
        migrate(&options.database)?;
        let state = Arc::new(Mutex::new(NativeRepoState { dispatcher }));
        let worker = NativeRepoWorker {
            database: options.database,
            artifacts: options.artifacts,
            authority: authority.clone(),
            provider: options.provider,
            workspace_id,
            verification_registry,
            state,
            cancellations: Arc::new(Mutex::new(BTreeMap::new())),
            scheduled: Arc::new(Mutex::new(BTreeSet::new())),
            queue: None,
            queue_pump: None,
        };
        let worker = worker.start()?;
        worker.reenqueue_operations()?;
        start_operation_metadata_migration(
            worker.database.clone(),
            Arc::clone(&worker.artifacts),
            Arc::clone(&worker.scheduled),
            Arc::clone(
                worker
                    .queue
                    .as_ref()
                    .ok_or("repository scheduler unavailable")?,
            ),
            worker
                .queue_pump
                .clone()
                .ok_or("repository queue pump unavailable")?,
        )?;
        Ok(Self {
            worker,
            project_id: options.project_id,
            availability: Mutex::new(availability),
        })
    }

    fn authorize(
        &self,
        principal: &AuthenticatedPrincipal,
        project: ProjectId,
        grant: Grant,
    ) -> Result<(), RepoError> {
        let snapshot = principal.grant_snapshot();
        if project == self.project_id
            && snapshot.project_id() == project
            && snapshot.grants().contains(&grant)
        {
            Ok(())
        } else {
            Err(RepoError::NotFound)
        }
    }
}

impl RepoService for NativeRepoService {
    fn status(&self, principal: &AuthenticatedPrincipal) -> Result<Value, RepoError> {
        Ok(
            json!({"schema_version":SCHEMA_VERSION,"principal_id":principal.principal_id(),"project_id":principal.grant_snapshot().project_id()}),
        )
    }
    fn revision(
        &self,
        principal: &AuthenticatedPrincipal,
        project: ProjectId,
    ) -> Result<Value, RepoError> {
        self.authorize(principal, project, Grant::WorkspaceRead)?;
        let mut state = self.worker.state.lock().map_err(|_| RepoError::Internal)?;
        let (revision, digest) = state
            .dispatcher
            .revision_state()
            .map_err(RepoError::Unavailable)?;
        Ok(json!({"schema_version": SCHEMA_VERSION, "revision": revision, "digest": digest}))
    }

    fn capabilities(
        &self,
        principal: &AuthenticatedPrincipal,
        project: ProjectId,
    ) -> Result<Value, RepoError> {
        self.authorize(principal, project, Grant::WorkspaceRead)?;
        let grants = principal.grant_snapshot().grants();
        let run_id = RunId::from_stable_bytes(
            format!("repo-capabilities:{}:{}", principal.principal_id(), project).as_bytes(),
        );
        let config = LayerStack::safe_defaults_for(self.worker.provider)
            .materialize(
                RunConfigContext {
                    principal_id: principal.principal_id(),
                    project_id: project,
                    run_id,
                },
                grants,
            )
            .map_err(|_| RepoError::Internal)?;
        let mut availability = self.availability.lock().map_err(|_| RepoError::Internal)?;
        let availability_snapshot = availability.snapshot()?;
        let items = NativeCatalog::all()
            .iter()
            .map(|descriptor| {
                let service_errors = availability_snapshot
                    .unavailable
                    .get(&descriptor.tool())
                    .into_iter()
                    .flatten()
                    .cloned();
                let missing_grants = descriptor
                    .required_grants()
                    .iter()
                    .filter(|grant| {
                        !grants.contains(grant) || !config.effective_authority().contains(grant)
                    })
                    .map(|grant| {
                        format!(
                            "missing_grant:{}",
                            serde_json::to_value(grant)
                                .ok()
                                .and_then(|value| value.as_str().map(str::to_owned))
                                .unwrap_or_else(|| "unknown".to_owned())
                        )
                    })
                    .collect::<Vec<_>>();
                let mut reasons = service_errors.collect::<Vec<_>>();
                reasons.extend(missing_grants);
                let configured = availability.configured(descriptor.tool());
                json!({
                    "name": descriptor.tool().short_name(),
                    "provider_name": descriptor.spec().name.0,
                    "version": descriptor.identity().version().as_str(),
                    "effect": format!("{:?}", descriptor.effect()).to_ascii_lowercase(),
                    "input_schema": descriptor.spec().input_schema,
                    "configured": configured,
                    "available": reasons.is_empty(),
                    "unavailable_code": reasons.first(),
                    "unavailable_reasons": reasons,
                    "availability": {
                        "checked_at_unix_micros": availability_snapshot.checked_at,
                        "expires_at_unix_micros": availability_snapshot.checked_at + AVAILABILITY_TTL.as_micros() as i64,
                        "generation": availability_snapshot.generation,
                        "image": (descriptor.tool() == NativeTool::Run).then(|| json!({
                            "reference": availability.image.as_deref(),
                            "resolved_digest": availability_snapshot.image_digest.as_deref(),
                            "immutable": availability_snapshot.image_digest.is_some(),
                        })),
                    },
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"schema_version": SCHEMA_VERSION, "items": items}))
    }

    fn invoke(
        &self,
        principal: &AuthenticatedPrincipal,
        project: ProjectId,
        tool: NativeTool,
        input: Value,
        key: Option<&IdempotencyKey>,
    ) -> Result<Value, RepoError> {
        let descriptor = NativeCatalog::all()
            .iter()
            .find(|descriptor| descriptor.tool() == tool)
            .ok_or(RepoError::NotFound)?;
        for grant in descriptor.required_grants() {
            self.authorize(principal, project, *grant)?;
        }
        if !jsonschema::validator_for(&descriptor.spec().input_schema)
            .map_err(|_| RepoError::Internal)?
            .is_valid(&input)
        {
            return Err(RepoError::Invalid("body"));
        }
        let generated_key;
        let key = if descriptor.effect()
            == crate::capabilities::kernel::grant::EffectClass::WorkspaceRead
        {
            generated_key = IdempotencyKey::parse(&format!(
                "repo-query-{}",
                ToolCallId::generate().map_err(|_| RepoError::Internal)?
            ))
            .map_err(|_| RepoError::Internal)?;
            &generated_key
        } else {
            key.ok_or(RepoError::Invalid("Idempotency-Key"))?
        };
        self.worker.enqueue(principal, project, tool, input, key)
    }

    fn result(&self, principal: &AuthenticatedPrincipal, id: &str) -> Result<Value, RepoError> {
        require_workspace_read(principal)?;
        load_reconciled_result(
            &self.worker.database,
            &self.worker.artifacts,
            principal,
            id,
            "result",
        )
    }

    fn events(&self, principal: &AuthenticatedPrincipal, id: &str) -> Result<Value, RepoError> {
        require_workspace_read(principal)?;
        load_reconciled_result(
            &self.worker.database,
            &self.worker.artifacts,
            principal,
            id,
            "events",
        )
    }

    fn artifact(
        &self,
        principal: &AuthenticatedPrincipal,
        reference: &str,
    ) -> Result<RepoArtifact, RepoError> {
        load_artifact(&self.worker.artifacts, principal, reference)
    }

    fn resolve_approval(
        &self,
        principal: &AuthenticatedPrincipal,
        id: &str,
        approved: bool,
        key: &IdempotencyKey,
    ) -> Result<Value, RepoError> {
        self.worker.resolve_approval(principal, id, approved, key)
    }

    fn cancel(
        &self,
        principal: &AuthenticatedPrincipal,
        id: &str,
        key: &IdempotencyKey,
    ) -> Result<Value, RepoError> {
        self.worker.cancel(principal, id, key)
    }
}

#[derive(Clone)]
struct RepositoryOperation {
    id: ToolCallId,
    principal: AuthenticatedPrincipal,
    project: ProjectId,
    tool: NativeTool,
    key: IdempotencyKey,
    input: Vec<u8>,
    config: crate::domain::config::RunConfigSnapshot,
    run_id: RunId,
    attempt: AttemptOwnership,
    claim: crate::api::service::AttemptDriverClaim,
    approval: ApprovalState,
    cancelled: bool,
    reservation: crate::runtime::scheduler::limits::Spend,
    created_at: i64,
}

enum EffectJournalState {
    Predispatch,
    Dispatched,
    Outcome(CanonicalInvocationResult),
}

enum WorkerErrorDisposition {
    Finalized,
    Failed,
    FinalizationPending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinalizationPoint {
    KernelEvent,
    CostArtifact,
    EventsArtifact,
    ResultArtifact,
    ResultRow,
    TerminalEvent,
}

impl FinalizationPoint {
    #[cfg(test)]
    const ALL: [Self; 6] = [
        Self::KernelEvent,
        Self::CostArtifact,
        Self::EventsArtifact,
        Self::ResultArtifact,
        Self::ResultRow,
        Self::TerminalEvent,
    ];
}

impl NativeRepoWorker {
    fn enqueue(
        &self,
        principal: &AuthenticatedPrincipal,
        project: ProjectId,
        tool: NativeTool,
        input: Value,
        key: &IdempotencyKey,
    ) -> Result<Value, RepoError> {
        let descriptor = NativeCatalog::all()
            .iter()
            .find(|item| item.tool() == tool)
            .ok_or(RepoError::NotFound)?;
        let seed = format!(
            "{}:{}:{}:{}",
            principal.principal_id(),
            project,
            tool.short_name(),
            key.as_str()
        );
        let id = ToolCallId::from_stable_bytes(seed.as_bytes());
        let run_id = RunId::from_stable_bytes(format!("repository-operation:{id}").as_bytes());
        let attempt_id =
            AttemptId::from_stable_bytes(format!("repository-attempt:{id}").as_bytes());
        let grants = principal.grant_snapshot().grants().clone();
        let config = LayerStack::safe_defaults_for(self.provider)
            .materialize(
                RunConfigContext {
                    principal_id: principal.principal_id(),
                    project_id: project,
                    run_id,
                },
                &grants,
            )
            .map_err(|_| RepoError::Internal)?;
        let reservation = descriptor
            .estimate_reservation(
                &input,
                &self.verification_registry,
                principal.grant_snapshot(),
                &config,
            )
            .map_err(|_| RepoError::Invalid("body"))?;
        let input = serde_json::to_vec(&input).map_err(|_| RepoError::Invalid("body"))?;
        let request_digest = canonical_request_digest(
            "repository.invoke",
            principal,
            project,
            &id.to_string(),
            &input,
        );
        let grants_json = serde_json::to_string(&grants).map_err(|_| RepoError::Internal)?;
        let now = now_unix_micros().map_err(|_| RepoError::Internal)?;
        let approval_id = (descriptor.approval() == ApprovalState::Pending)
            .then(|| ApprovalId::from_stable_bytes(format!("repository-approval:{id}").as_bytes()));
        let mut connection = open_repository_connection(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql_error)?;
        let existing = transaction
            .query_row(
                "SELECT result_id,request_digest,status FROM repository_operations
             WHERE principal_id=?1 AND project_id=?2 AND operation=?3 AND idempotency_key=?4",
                params![
                    principal.principal_id().to_string(),
                    project.to_string(),
                    format!("repo.{}", tool.short_name()),
                    key.as_str()
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sql_error)?;
        if let Some((existing_id, digest, status)) = existing {
            if digest.as_slice() != request_digest.as_bytes() {
                return Err(RepoError::Conflict);
            }
            transaction.commit().map_err(map_sql_error)?;
            if status == "queued" {
                self.submit(existing_id.clone())?;
            }
            let resource = load_result(&self.database, principal, &existing_id, "result")?;
            return Ok(resource);
        }
        let admitted: u64 = transaction
            .query_row(
                "SELECT count(*) FROM repository_operations WHERE status IN ('queued','running')",
                [],
                |row| row.get(0),
            )
            .map_err(map_sql_error)?;
        if admitted >= (REPOSITORY_QUEUE_CAPACITY + REPOSITORY_WORKERS) as u64 {
            return Err(RepoError::Unavailable(
                "repository_queue_saturated".to_owned(),
            ));
        }
        transaction.execute(
            "INSERT INTO repository_operations
             (result_id,principal_id,project_id,operation,tool,idempotency_key,request_digest,input,grants,config,run_id,attempt_id,fence,lease_version,status,approval_id,approval_state,reservation_cost,reservation_tokens,reservation_turns,reservation_tools,reservation_processes,migration_version,created_at,updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,1,1,'queued',?13,?14,?15,?16,?17,?18,?19,?20,?21,?21)",
            params![id.to_string(), principal.principal_id().to_string(), project.to_string(), format!("repo.{}", tool.short_name()), tool.short_name(), key.as_str(), request_digest.as_bytes().as_slice(), input, grants_json, config.canonical_bytes(), run_id.to_string(), attempt_id.to_string(), approval_id.map(|id| id.to_string()), if approval_id.is_some() { "pending" } else { "not_required" }, reservation.cost_microusd(), reservation.tokens(), reservation.turns(), reservation.tools(), reservation.processes(), OPERATION_METADATA_MIGRATION_VERSION, now],
        ).map_err(map_sql_error)?;
        transaction.execute(
            "INSERT INTO attempt_driver_fences(run_id,fence,lease_version) VALUES(?1,1,1)
             ON CONFLICT(run_id) DO UPDATE SET fence=excluded.fence,lease_version=excluded.lease_version",
            [run_id.to_string()],
        ).map_err(map_sql_error)?;
        transaction.execute(
            "INSERT INTO attempt_driver_claims(run_id,attempt_id,principal_id,fence,lease_version,expires_at_unix_micros,quiescent)
             VALUES(?1,?2,?3,1,1,?4,0)
             ON CONFLICT(run_id) DO UPDATE SET attempt_id=excluded.attempt_id,principal_id=excluded.principal_id,fence=1,lease_version=1,expires_at_unix_micros=excluded.expires_at_unix_micros,quiescent=0",
            params![run_id.to_string(), attempt_id.to_string(), principal.principal_id().to_string(), i64::MAX],
        ).map_err(map_sql_error)?;
        append_operation_event_tx(
            &transaction,
            &id.to_string(),
            "repository.operation_queued",
            json!({
                "operation_id":id,"native_result_id":id,"operation":format!("repo.{}",tool.short_name()),
                "owner":{"run_id":run_id,"attempt_id":attempt_id,"fence":1},
                "config_snapshot_digest":config.digest_hex(),
            }),
            now,
        )?;
        if approval_id.is_some() {
            transaction.execute(
                "UPDATE repository_operations SET status='waiting_approval',updated_at=?2 WHERE result_id=?1",
                params![id.to_string(), now],
            ).map_err(map_sql_error)?;
            append_operation_event_tx(
                &transaction,
                &id.to_string(),
                "repository.approval_requested",
                json!({
                    "operation_id":id,"approval_id":approval_id,"effect":"workspace_write_or_process","zero_effect":true,
                }),
                now,
            )?;
        }
        transaction.commit().map_err(map_sql_error)?;
        let resource = load_result(&self.database, principal, &id.to_string(), "result")?;
        if approval_id.is_none() {
            self.submit(id.to_string())?;
        }
        Ok(resource)
    }

    fn start(mut self) -> Result<Self, String> {
        let (sender, receiver) = sync_channel::<String>(REPOSITORY_QUEUE_CAPACITY);
        let sender = Arc::new(sender);
        let (pump_sender, pump_receiver) = sync_channel::<()>(1);
        let receiver = Arc::new(Mutex::new(receiver));
        self.queue = Some(Arc::clone(&sender));
        self.queue_pump = Some(pump_sender);
        for index in 0..REPOSITORY_WORKERS {
            let worker = self.clone();
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name(format!("kit-repository-worker-{index}"))
                .spawn(move || {
                    while let Ok(id) = receiver
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .recv()
                    {
                        let disposition = worker.execute(&id).err().map(|error| {
                            worker
                                .handle_execution_error(&id, &format!("{error:?}"))
                                .unwrap_or(WorkerErrorDisposition::FinalizationPending)
                        });
                        worker
                            .scheduled
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&id);
                        if matches!(
                            disposition,
                            Some(WorkerErrorDisposition::FinalizationPending)
                        ) {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        worker.wake_queue_pump();
                    }
                })
                .map_err(|error| error.to_string())?;
        }
        start_queue_pump(
            self.database.clone(),
            Arc::downgrade(&self.scheduled),
            Arc::downgrade(&sender),
            pump_receiver,
        )?;
        Ok(self)
    }

    fn wake_queue_pump(&self) {
        if let Some(queue_pump) = &self.queue_pump {
            let _ = queue_pump.try_send(());
        }
    }

    fn reenqueue_operations(&self) -> Result<(), String> {
        reenqueue_operations(
            &self.database,
            &self.scheduled,
            self.queue
                .as_deref()
                .ok_or("repository scheduler unavailable")?,
        )
    }

    fn submit(&self, id: String) -> Result<(), RepoError> {
        match self.try_submit(id)? {
            SubmitState::Submitted | SubmitState::AlreadyScheduled => Ok(()),
            SubmitState::QueueFull => Err(RepoError::Unavailable(
                "repository_queue_saturated".to_owned(),
            )),
        }
    }

    fn try_submit(&self, id: String) -> Result<SubmitState, RepoError> {
        try_schedule(
            &self.scheduled,
            self.queue.as_deref().ok_or(RepoError::Internal)?,
            id,
        )
    }

    fn operation(&self, id: &str) -> Result<RepositoryOperation, RepoError> {
        load_operation(&self.database, id)
    }

    fn execute(&self, id: &str) -> Result<(), RepoError> {
        let operation = self.operation(id)?;
        if operation.approval == ApprovalState::Pending && !operation.cancelled {
            return Ok(());
        }
        let now = now_unix_micros().map_err(|_| RepoError::Internal)?;
        let connection = open_repository_connection(&self.database)?;
        let changed = connection
            .execute(
                "UPDATE repository_operations SET status='running',updated_at=?2
             WHERE result_id=?1 AND status IN ('queued','waiting_approval')",
                params![id, now],
            )
            .map_err(map_sql_error)?;
        if changed == 0 {
            return match effect_journal_state(&self.database, operation.run_id, id)? {
                EffectJournalState::Outcome(outcome) => {
                    Self::complete(&self.database, &self.artifacts, operation, outcome, false)
                }
                EffectJournalState::Dispatched => Self::complete(
                    &self.database,
                    &self.artifacts,
                    operation,
                    interrupted_after_dispatch(),
                    false,
                ),
                EffectJournalState::Predispatch => Ok(()),
            };
        }
        append_operation_event(
            &self.database,
            id,
            "repository.operation_running",
            json!({
                "operation_id":id,"native_result_id":id,"attempt_id":operation.attempt.attempt_id,"fence":operation.attempt.fencing_token.get(),
            }),
        )?;
        let cancellation = Arc::new(AtomicBool::new(operation.cancelled));
        self.cancellations
            .lock()
            .map_err(|_| RepoError::Internal)?
            .insert(id.to_owned(), Arc::clone(&cancellation));
        let descriptor = NativeCatalog::all()
            .iter()
            .find(|item| item.tool() == operation.tool)
            .ok_or(RepoError::NotFound)?;
        let constraints = ArgumentConstraints::new([format!(
            "native={}@{}",
            operation.tool.short_name(),
            descriptor.identity().version().as_str()
        )
        .into_bytes()]);
        let grants = CapabilityGrantSnapshot::new(
            &operation.config,
            NativeCatalog::all().iter().map(|item| {
                CapabilityGrant::new(
                    operation.principal.principal_id(),
                    operation.project,
                    self.workspace_id,
                    item.identity().clone(),
                    item.schema().normalized_digest(),
                    item.effect(),
                    ArgumentConstraints::new([format!(
                        "native={}@{}",
                        item.tool().short_name(),
                        item.identity().version().as_str()
                    )
                    .into_bytes()]),
                )
            }),
            DigestAlgorithm::Sha256,
        );
        let mut store =
            SqliteStore::open(&self.database, &self.authority).map_err(map_store_error)?;
        let events = store.events().map_err(map_store_error)?;
        let budget = crate::agent::executor::tool_budget_from_events(&events, &operation.config)
            .map_err(|_| RepoError::Internal)?;
        let command_id =
            CommandId::from_stable_bytes(format!("repository-command:{id}").as_bytes());
        let intent_event_id =
            EventId::from_stable_bytes(format!("repository-intent:{id}").as_bytes());
        let outcome_event_id =
            EventId::from_stable_bytes(format!("repository-outcome:{id}").as_bytes());
        let trace_id =
            TraceId::parse(&format!("repository-{id}")).map_err(|_| RepoError::Internal)?;
        let occurred_at = UtcDateTime::now().map_err(|_| RepoError::Internal)?;
        let current_fence = AtomicU64::new(operation.attempt.fencing_token.get());
        let mut state = self.state.lock().map_err(|_| RepoError::Internal)?;
        state.dispatcher.bind_authority(
            operation.principal.clone(),
            operation.config.clone(),
            operation.attempt,
            Arc::clone(&cancellation),
        );
        let result = crate::capabilities::native::orchestrate::OrchestratedNativeInvocation::new(
            InvocationEnvelope {
                authenticated: &operation.principal,
                config: &operation.config,
                grants: &grants,
                delegation: None,
                capability: descriptor.identity(),
                discovered_schema_digest: descriptor.schema().normalized_digest(),
                bound_schema_digest: descriptor.schema().normalized_digest(),
                effect: descriptor.effect(),
                argument_constraints: &constraints,
                arguments: &operation.input,
                workspace_id: self.workspace_id,
                project_id: operation.project,
                invocation_id: operation.id,
                idempotency_key: &operation.key,
                reservation: operation.reservation,
                retry_safety: descriptor.retry_safety(),
                approval: operation.approval,
                cancellation: &cancellation,
                attempt: operation.attempt,
                driver_claim: Some(operation.claim),
                current_fence: &current_fence,
                command_id,
                intent_event_id,
                outcome_event_id,
                occurred_at: &occurred_at,
                trace_id: &trace_id,
            },
            &mut store,
            &budget,
        )
        .execute(&mut |invocation| state.dispatcher.dispatch(invocation));
        drop(state);
        self.cancellations
            .lock()
            .map_err(|_| RepoError::Internal)?
            .remove(id);
        let result = result.map_err(map_invoke_error)?;
        Self::complete(
            &self.database,
            &self.artifacts,
            operation,
            result.canonical,
            result.replayed,
        )
    }

    fn complete(
        database: &Path,
        artifacts_store: &ArtifactStore,
        operation: RepositoryOperation,
        canonical: CanonicalInvocationResult,
        replayed: bool,
    ) -> Result<(), RepoError> {
        Self::complete_with_hook(
            database,
            artifacts_store,
            operation,
            canonical,
            replayed,
            |_| false,
        )
    }

    fn complete_with_hook(
        database: &Path,
        artifacts_store: &ArtifactStore,
        operation: RepositoryOperation,
        canonical: CanonicalInvocationResult,
        replayed: bool,
        mut fail: impl FnMut(FinalizationPoint) -> bool,
    ) -> Result<(), RepoError> {
        let (status, resource) = Self::materialize_completion(
            database,
            artifacts_store,
            &operation,
            &canonical,
            replayed,
            &mut fail,
        )?;
        Self::persist_completion(
            database, &operation, &canonical, replayed, &status, &resource, false, &mut fail,
        )
    }

    fn materialize_completion(
        database: &Path,
        artifacts_store: &ArtifactStore,
        operation: &RepositoryOperation,
        canonical: &CanonicalInvocationResult,
        replayed: bool,
        fail: &mut impl FnMut(FinalizationPoint) -> bool,
    ) -> Result<(String, Value), RepoError> {
        let status = match canonical.status {
            InvocationStatus::Succeeded => "completed",
            InvocationStatus::Failed => "failed",
            InvocationStatus::Cancelled => "cancelled",
            InvocationStatus::OutcomeUnknown => "outcome_unknown",
            InvocationStatus::ApprovalRequired => "waiting_approval",
            InvocationStatus::ApprovalDenied => "denied",
        };
        let output = canonical
            .output
            .as_ref()
            .map(|output| serde_json::from_slice(&output.body))
            .transpose()
            .map_err(|_| RepoError::Internal)?;
        let invocation_id = operation.id.to_string();
        let kernel_events = kernel_operation_events(database, operation.run_id, &invocation_id)?;
        for event in &kernel_events {
            append_kernel_operation_event(
                database,
                &invocation_id,
                event["type"].as_str().unwrap_or("capability.event"),
                event["payload"].clone(),
            )?;
            inject_finalization(fail, FinalizationPoint::KernelEvent)?;
        }
        let reservation = operation.reservation;
        let cost = json!({
            "schema_version":SCHEMA_VERSION,"operation_id":operation.id,"native_result_id":operation.id,
            "provider":{"calls":0,"cost_microusd":0,"reason":"direct_native_operation"},
            "reservations":[{"kind":"tool","cost_microusd":reservation.cost_microusd(),"tokens":reservation.tokens(),"turns":reservation.turns(),"tools":reservation.tools(),"processes":reservation.processes()}],
            "debited":spend_json(canonical.charged.then_some(reservation)),
            "reserved":spend_json(None),
            "released":spend_json((!canonical.charged).then_some(reservation)),
            "charged":canonical.charged,
        });
        let events_report = json!({"schema_version":SCHEMA_VERSION,"operation_id":operation.id,"native_result_id":operation.id,"items":kernel_events});
        let cost_artifact = put_report(
            artifacts_store,
            &operation.principal,
            operation.project,
            &invocation_id,
            "cost",
            &cost,
            operation.created_at,
        )?;
        inject_finalization(fail, FinalizationPoint::CostArtifact)?;
        let edit_events_artifact = put_report(
            artifacts_store,
            &operation.principal,
            operation.project,
            &invocation_id,
            "edit_events",
            &events_report,
            operation.created_at,
        )?;
        inject_finalization(fail, FinalizationPoint::EventsArtifact)?;
        let mut artifacts = explicit_artifacts(
            artifacts_store,
            operation,
            output.as_ref(),
            cost_artifact,
            edit_events_artifact,
        )?;
        let mut resource = json!({
            "schema_version":SCHEMA_VERSION,"id":operation.id,"native_operation_id":operation.id,"native_result_id":operation.id,
            "operation":format!("repo.{}",operation.tool.short_name()),"status":status,"replayed":replayed,
            "approval":{"state":match operation.approval { ApprovalState::Approved=>"approved",ApprovalState::Denied=>"denied",ApprovalState::Pending=>"pending",ApprovalState::NotRequired=>"not_required"}},
            "owner":{"run_id":operation.run_id,"attempt_id":operation.attempt.attempt_id,"fence":operation.attempt.fencing_token.get()},
            "output":output,"error":canonical.code.as_deref().map(|code| operation_error(code,status)),"cost":cost,"artifacts":artifacts,
        });
        let repository_result = put_report(
            artifacts_store,
            &operation.principal,
            operation.project,
            &invocation_id,
            "repository_result",
            &resource,
            operation.created_at,
        )?;
        inject_finalization(fail, FinalizationPoint::ResultArtifact)?;
        artifacts["repository_result"] = repository_result;
        resource["artifacts"] = artifacts;
        Ok((status.to_owned(), resource))
    }

    #[allow(clippy::too_many_arguments)]
    fn persist_completion(
        database: &Path,
        operation: &RepositoryOperation,
        canonical: &CanonicalInvocationResult,
        replayed: bool,
        status: &str,
        resource: &Value,
        reconcile: bool,
        fail: &mut impl FnMut(FinalizationPoint) -> bool,
    ) -> Result<(), RepoError> {
        let now = now_unix_micros().map_err(|_| RepoError::Internal)?;
        let mut connection = open_repository_connection(database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql_error)?;
        let terminal = terminal_event_payload(operation.id, Some(canonical), resource)?;
        if reconcile
            && completion_matches(
                &transaction,
                &operation.id.to_string(),
                status,
                resource,
                &terminal,
            )?
        {
            transaction
                .execute(
                    "UPDATE repository_operations SET migration_version=?2 WHERE result_id=?1",
                    params![
                        operation.id.to_string(),
                        OPERATION_METADATA_MIGRATION_VERSION
                    ],
                )
                .map_err(map_sql_error)?;
            return transaction.commit().map_err(map_sql_error);
        }
        transaction.execute(
            "UPDATE repository_operations SET status=?2,result=?3,replayed=?4,updated_at=?5,migration_version=?6 WHERE result_id=?1",
            params![operation.id.to_string(),status,resource.to_string(),replayed,now,OPERATION_METADATA_MIGRATION_VERSION],
        ).map_err(map_sql_error)?;
        inject_finalization(fail, FinalizationPoint::ResultRow)?;
        upsert_terminal_event_tx(
            &transaction,
            &operation.id.to_string(),
            "repository.operation_terminal",
            terminal,
            now,
        )?;
        inject_finalization(fail, FinalizationPoint::TerminalEvent)?;
        transaction.commit().map_err(map_sql_error)
    }

    fn reconcile_completion(
        database: &Path,
        artifacts_store: &ArtifactStore,
        operation: RepositoryOperation,
        canonical: CanonicalInvocationResult,
        replayed: bool,
    ) -> Result<(), RepoError> {
        let mut fail = |_| false;
        let (status, resource) = Self::materialize_completion(
            database,
            artifacts_store,
            &operation,
            &canonical,
            replayed,
            &mut fail,
        )?;
        Self::persist_completion(
            database, &operation, &canonical, replayed, &status, &resource, true, &mut fail,
        )
    }

    fn resolve_approval(
        &self,
        principal: &AuthenticatedPrincipal,
        id: &str,
        approved: bool,
        key: &IdempotencyKey,
    ) -> Result<Value, RepoError> {
        authorize_operation_effect(&self.database, principal, id, true)?;
        let request_body = serde_json::to_vec(&json!({
            "decision": if approved { "approved" } else { "denied" },
        }))
        .map_err(|_| RepoError::Internal)?;
        let digest = canonical_request_digest(
            "repository.approval",
            principal,
            principal.grant_snapshot().project_id(),
            id,
            &request_body,
        );
        let now = now_unix_micros().map_err(|_| RepoError::Internal)?;
        let mut connection = open_repository_connection(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql_error)?;
        if let Some(response) = mutation_receipt(
            &transaction,
            principal,
            "repository.approval",
            id,
            key,
            digest,
        )? {
            drop(transaction);
            if response["status"] == "queued" {
                self.submit(id.to_owned())?;
            }
            return Ok(response);
        }
        ensure_action_queue_admission(&transaction, id)?;
        let changed = transaction.execute(
            "UPDATE repository_operations SET approval_state=?4,status='queued',updated_at=?5
             WHERE result_id=?1 AND principal_id=?2 AND project_id=?3 AND status='waiting_approval' AND approval_state='pending'",
            params![id,principal.principal_id().to_string(),principal.grant_snapshot().project_id().to_string(),if approved{"approved"}else{"denied"},now],
        ).map_err(map_sql_error)?;
        if changed != 1 {
            return Err(RepoError::Conflict);
        }
        append_operation_event_tx(
            &transaction,
            id,
            "repository.approval_resolved",
            json!({"operation_id":id,"decision":if approved{"approved"}else{"denied"}}),
            now,
        )?;
        let response = load_result_from_connection(&transaction, principal, id, "result")?;
        insert_mutation_receipt(
            &transaction,
            principal,
            "repository.approval",
            id,
            key,
            digest,
            &response,
            now,
        )?;
        transaction.commit().map_err(map_sql_error)?;
        self.submit(id.to_owned())?;
        Ok(response)
    }

    fn cancel(
        &self,
        principal: &AuthenticatedPrincipal,
        id: &str,
        key: &IdempotencyKey,
    ) -> Result<Value, RepoError> {
        authorize_operation_effect(&self.database, principal, id, false)?;
        let digest = canonical_request_digest(
            "repository.cancel",
            principal,
            principal.grant_snapshot().project_id(),
            id,
            b"{}",
        );
        let now = now_unix_micros().map_err(|_| RepoError::Internal)?;
        let mut connection = open_repository_connection(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql_error)?;
        if let Some(response) = mutation_receipt(
            &transaction,
            principal,
            "repository.cancel",
            id,
            key,
            digest,
        )? {
            drop(transaction);
            if response["status"] == "queued" {
                self.submit(id.to_owned())?;
            }
            return Ok(response);
        }
        ensure_action_queue_admission(&transaction, id)?;
        let changed = transaction.execute(
            "UPDATE repository_operations SET cancellation_requested=1,status=CASE WHEN status='waiting_approval' THEN 'queued' ELSE status END,updated_at=?4
             WHERE result_id=?1 AND principal_id=?2 AND project_id=?3 AND status IN ('queued','running','waiting_approval')",
            params![id,principal.principal_id().to_string(),principal.grant_snapshot().project_id().to_string(),now],
        ).map_err(map_sql_error)?;
        if changed != 1 {
            return Err(RepoError::Conflict);
        }
        append_operation_event_tx(
            &transaction,
            id,
            "repository.cancellation_requested",
            json!({"operation_id":id}),
            now,
        )?;
        let response = load_result_from_connection(&transaction, principal, id, "result")?;
        insert_mutation_receipt(
            &transaction,
            principal,
            "repository.cancel",
            id,
            key,
            digest,
            &response,
            now,
        )?;
        transaction.commit().map_err(map_sql_error)?;
        if let Some(cancellation) = self
            .cancellations
            .lock()
            .map_err(|_| RepoError::Internal)?
            .get(id)
        {
            cancellation.store(true, Ordering::Release);
        } else {
            self.submit(id.to_owned())?;
        }
        Ok(response)
    }

    fn fail_operation(&self, id: &str, code: &str, _detail: &str) -> Result<bool, RepoError> {
        let now = now_unix_micros().map_err(|_| RepoError::Internal)?;
        let mut connection = open_repository_connection(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sql_error)?;
        let changed = transaction.execute(
            "UPDATE repository_operations SET status='failed',result=json_object('schema_version',1,'id',result_id,'native_operation_id',result_id,'native_result_id',result_id,'operation',operation,'status','failed','replayed',replayed,'output',NULL,'error',json_object('code',?2,'effect_state','none','retryable',0),'cost',NULL,'artifacts',NULL),updated_at=?3
             WHERE result_id=?1 AND status NOT IN ('completed','failed','cancelled','outcome_unknown','denied')
               AND NOT EXISTS (
                 SELECT 1 FROM events
                 WHERE stream=repository_operations.result_id
                   AND event_type='capability.invocation_outcome'
                   AND commit_position <= (SELECT position FROM commit_watermark WHERE singleton=1)
               )",
            params![id,code,now],
        ).map_err(map_sql_error)?;
        if changed == 1 {
            let result = transaction
                .query_row(
                    "SELECT result FROM repository_operations WHERE result_id=?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .map_err(map_sql_error)?;
            let result = serde_json::from_str(&result).map_err(|_| RepoError::Internal)?;
            upsert_terminal_event_tx(
                &transaction,
                id,
                "repository.operation_terminal",
                terminal_event_payload(
                    ToolCallId::parse(id).map_err(|_| RepoError::Internal)?,
                    None,
                    &result,
                )?,
                now,
            )?;
        }
        transaction.commit().map_err(map_sql_error)?;
        Ok(changed == 1)
    }

    fn handle_execution_error(
        &self,
        id: &str,
        detail: &str,
    ) -> Result<WorkerErrorDisposition, RepoError> {
        let operation = self.operation(id)?;
        for _ in 0..FINALIZATION_RETRIES {
            match effect_journal_state(&self.database, operation.run_id, id) {
                Ok(EffectJournalState::Outcome(outcome)) => {
                    if Self::complete(
                        &self.database,
                        &self.artifacts,
                        operation.clone(),
                        outcome,
                        false,
                    )
                    .is_ok()
                    {
                        return Ok(WorkerErrorDisposition::Finalized);
                    }
                }
                Ok(EffectJournalState::Dispatched) => {
                    if Self::complete(
                        &self.database,
                        &self.artifacts,
                        operation.clone(),
                        interrupted_after_dispatch(),
                        false,
                    )
                    .is_ok()
                    {
                        return Ok(WorkerErrorDisposition::Finalized);
                    }
                }
                Ok(EffectJournalState::Predispatch) => {
                    return Ok(
                        if self.fail_operation(id, "repository_worker_failed", detail)? {
                            WorkerErrorDisposition::Failed
                        } else {
                            WorkerErrorDisposition::FinalizationPending
                        },
                    );
                }
                Err(_) => {}
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(WorkerErrorDisposition::FinalizationPending)
    }
}

fn try_schedule(
    scheduled: &Mutex<BTreeSet<String>>,
    queue: &SyncSender<String>,
    id: String,
) -> Result<SubmitState, RepoError> {
    {
        let mut scheduled = scheduled.lock().map_err(|_| RepoError::Internal)?;
        if !scheduled.insert(id.clone()) {
            return Ok(SubmitState::AlreadyScheduled);
        }
    }
    match queue.try_send(id.clone()) {
        Ok(()) => Ok(SubmitState::Submitted),
        Err(error) => {
            scheduled
                .lock()
                .map_err(|_| RepoError::Internal)?
                .remove(&id);
            match error {
                TrySendError::Full(_) => Ok(SubmitState::QueueFull),
                TrySendError::Disconnected(_) => Err(RepoError::Unavailable(
                    "repository_dispatcher_unavailable".to_owned(),
                )),
            }
        }
    }
}

fn map_invoke_error(error: crate::capabilities::kernel::invoke::InvokeError) -> RepoError {
    use crate::capabilities::kernel::invoke::InvokeError;
    match error {
        InvokeError::AuthorizationDenied(_) => RepoError::NotFound,
        InvokeError::Store(crate::store::sqlite::append::StoreError::IdempotencyConflict(_)) => {
            RepoError::Conflict
        }
        InvokeError::Store(error) => map_store_error(error),
        InvokeError::InvalidArguments | InvokeError::SchemaBindingMismatch => {
            RepoError::Invalid("body")
        }
        _ => RepoError::Internal,
    }
}

impl AvailabilityProbe {
    fn configured(&self, tool: NativeTool) -> bool {
        match tool {
            NativeTool::Discover | NativeTool::Search | NativeTool::Read => true,
            NativeTool::Edit => {
                !self.registry.is_empty()
                    && self.feedback_configured
                    && (!self.formatter_required || self.formatter.is_some())
            }
            NativeTool::Run => self.image.is_some(),
            NativeTool::Check => !self.registry.is_empty() && self.feedback_configured,
        }
    }

    fn snapshot(&mut self) -> Result<AvailabilitySnapshot, RepoError> {
        let generation = self.generation();
        if let Some(cached) = &self.cached
            && cached.generation == generation
            && cached.checked.elapsed() < AVAILABILITY_TTL
        {
            return Ok(cached.clone());
        }
        let mut unavailable = BTreeMap::<NativeTool, Vec<String>>::new();
        let mut mark = |tool, reason: &'static str| {
            let reasons = unavailable.entry(tool).or_default();
            if !reasons.iter().any(|found| found == reason) {
                reasons.push(reason.to_owned());
            }
        };
        if !self.root.is_dir() {
            for tool in NativeTool::ALL {
                mark(tool, "trusted_workspace_unavailable");
            }
        }
        if self.registry.is_empty() {
            mark(NativeTool::Edit, "trusted_edit_registry_unavailable");
            mark(NativeTool::Check, "trusted_check_registry_unavailable");
        }
        if !self.feedback_configured {
            mark(NativeTool::Edit, "trusted_edit_feedback_unavailable");
            mark(NativeTool::Check, "trusted_check_feedback_unavailable");
        }
        if self.formatter_required && self.formatter.is_none() {
            mark(NativeTool::Edit, "trusted_edit_formatter_unavailable");
        }
        if !self.syntax_available {
            mark(NativeTool::Edit, "trusted_edit_syntax_unavailable");
        }
        if self.formatter.is_some()
            && !self.mechanical_executor
            && !crate::executor::formatter::FormatterExecutor::production_available()
        {
            mark(
                NativeTool::Edit,
                "trusted_edit_formatter_platform_unavailable",
            );
        }
        let image_digest = self.image.as_deref().and_then(pinned_image_digest);
        if self.image.is_none() {
            mark(NativeTool::Run, "trusted_run_image_unavailable");
        } else if image_digest.is_none() {
            mark(NativeTool::Run, "trusted_run_image_not_immutable");
        }
        if !self.mechanical_executor {
            match crate::executor::backends::container::limits::probe_backend() {
                Ok(evidence) => {
                    let image_availability =
                        probe_images(evidence.runtime_path(), self.dependency_images());
                    if let Some(image) = self.image.as_deref()
                        && image_digest.is_some()
                        && image_availability.get(image) != Some(&true)
                    {
                        mark(NativeTool::Run, "trusted_run_image_unavailable");
                    }
                    for check in self.registry.checks() {
                        if image_availability.get(check.command().image()) != Some(&true) {
                            mark(NativeTool::Edit, "trusted_edit_check_image_unavailable");
                            mark(NativeTool::Check, "trusted_check_image_unavailable");
                        }
                    }
                    if let Some(formatter) = &self.formatter {
                        match formatter.command() {
                            Some(command)
                                if image_availability.get(command.image()) != Some(&true) =>
                            {
                                mark(NativeTool::Edit, "trusted_edit_formatter_image_unavailable");
                            }
                            None => mark(
                                NativeTool::Edit,
                                "trusted_edit_formatter_command_unavailable",
                            ),
                            Some(_) => {}
                        }
                    }
                }
                Err(_) => {
                    for tool in [NativeTool::Edit, NativeTool::Run, NativeTool::Check] {
                        mark(tool, "trusted_executor_helper_unavailable");
                    }
                }
            }
        }
        let snapshot = AvailabilitySnapshot {
            checked: Instant::now(),
            checked_at: now_unix_micros().map_err(|_| RepoError::Internal)?,
            generation,
            unavailable,
            image_digest: image_digest.map(str::to_owned),
        };
        self.cached = Some(snapshot.clone());
        Ok(snapshot)
    }

    fn generation(&self) -> String {
        let runtime = [Path::new("/usr/bin/podman"), Path::new("/usr/bin/docker")]
            .into_iter()
            .find(|path| path.is_file());
        self.generation_with_runtime(runtime)
    }

    fn generation_with_runtime(&self, runtime: Option<&Path>) -> String {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"KIT_REPOSITORY_AVAILABILITY_V1\0");
        let executable = std::env::current_exe().ok();
        for path in [
            Some(self.root.as_path()),
            Some(Path::new(
                crate::executor::backends::container::limits::helper_path(),
            )),
            Some(Path::new("/usr/bin/podman")),
            Some(Path::new("/usr/bin/docker")),
            executable.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            bytes.extend_from_slice(path.as_os_str().as_encoded_bytes());
            if let Ok(metadata) = path.metadata() {
                bytes.extend_from_slice(&metadata.len().to_be_bytes());
                if let Ok(modified) = metadata.modified()
                    && let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH)
                {
                    bytes.extend_from_slice(&duration.as_nanos().to_be_bytes());
                }
            }
            bytes.push(0xff);
        }
        if let Some(image) = &self.image {
            bytes.extend_from_slice(image.as_bytes());
        }
        if let Some(runtime) = runtime {
            for (image, available) in probe_images(runtime, self.dependency_images().into_iter()) {
                bytes.extend_from_slice(image.as_bytes());
                bytes.push(available as u8);
            }
        }
        for check in self.registry.checks() {
            bytes.extend_from_slice(check.command().id().as_bytes());
            bytes.extend_from_slice(check.command().image().as_bytes());
            bytes.extend_from_slice(check.command().program().as_bytes());
            bytes.extend_from_slice(check.command().tool_digest().as_bytes());
            bytes.extend_from_slice(check.command().config_digest().as_bytes());
        }
        if let Some(formatter) = &self.formatter {
            bytes.extend_from_slice(formatter.id().as_bytes());
            bytes.extend_from_slice(formatter.version().as_bytes());
            if let Some(command) = formatter.command() {
                bytes.extend_from_slice(command.image().as_bytes());
                bytes.extend_from_slice(command.program().as_bytes());
                bytes.extend_from_slice(command.requested_binary_digest().as_bytes());
                bytes.extend_from_slice(command.requested_config_digest().as_bytes());
            }
        }
        bytes.extend_from_slice(&[
            (!self.registry.is_empty()) as u8,
            self.feedback_configured as u8,
            self.formatter.is_some() as u8,
            self.formatter_required as u8,
            self.syntax_available as u8,
            self.mechanical_executor as u8,
        ]);
        blake3::hash(&bytes).to_hex().to_string()
    }

    fn dependency_images(&self) -> BTreeSet<String> {
        self.image
            .iter()
            .cloned()
            .chain(
                self.registry
                    .checks()
                    .iter()
                    .map(|check| check.command().image().to_owned()),
            )
            .chain(
                self.formatter
                    .iter()
                    .filter_map(|formatter| formatter.command())
                    .map(|command| command.image().to_owned()),
            )
            .collect()
    }
}

fn pinned_image_digest(image: &str) -> Option<&str> {
    let digest = image
        .strip_prefix("sha256:")
        .or_else(|| image.rsplit_once("@sha256:").map(|(_, digest)| digest))?;
    (digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(digest)
}

fn probe_images(
    runtime: &Path,
    images: impl IntoIterator<Item = String>,
) -> BTreeMap<String, bool> {
    let deadline = Instant::now() + Duration::from_secs(2);
    images
        .into_iter()
        .map(|image| {
            let available = probe_image_until(runtime, &image, deadline);
            (image, available)
        })
        .collect()
}

fn probe_image_until(runtime: &Path, image: &str, deadline: Instant) -> bool {
    let Ok(mut child) = std::process::Command::new(runtime)
        .args(["image", "inspect", image])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn require_workspace_read(principal: &AuthenticatedPrincipal) -> Result<(), RepoError> {
    principal
        .grant_snapshot()
        .grants()
        .contains(&Grant::WorkspaceRead)
        .then_some(())
        .ok_or(RepoError::NotFound)
}

fn authorize_operation_effect(
    database: &Path,
    principal: &AuthenticatedPrincipal,
    id: &str,
    approval: bool,
) -> Result<(), RepoError> {
    if ToolCallId::parse(id).is_err() {
        return Err(RepoError::NotFound);
    }
    let connection = open_repository_connection(database)?;
    let row = connection
        .query_row(
            "SELECT principal_id,project_id,tool FROM repository_operations WHERE result_id=?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(map_sql_error)?
        .ok_or(RepoError::NotFound)?;
    let snapshot = principal.grant_snapshot();
    let tool = parse_tool(&row.2)?;
    let descriptor = NativeCatalog::all()
        .iter()
        .find(|descriptor| descriptor.tool() == tool)
        .ok_or(RepoError::NotFound)?;
    let authorized = row.0 == principal.principal_id().to_string()
        && row.1 == snapshot.project_id().to_string()
        && snapshot
            .grants()
            .contains(&descriptor.effect().required_grant())
        && (!approval
            || snapshot
                .principal_grants()
                .contains(&PrincipalGrant::ResolveApproval));
    authorized.then_some(()).ok_or(RepoError::NotFound)
}

fn canonical_request_digest(
    operation: &str,
    principal: &AuthenticatedPrincipal,
    project: ProjectId,
    target: &str,
    body: &[u8],
) -> CanonicalRequestDigest {
    let mut bytes = Vec::new();
    let principal_id = principal.principal_id().to_string();
    let project_id = project.to_string();
    for value in [
        operation.as_bytes(),
        principal_id.as_bytes(),
        project_id.as_bytes(),
        target.as_bytes(),
        body,
    ] {
        bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
        bytes.extend_from_slice(value);
    }
    CanonicalRequestDigest::new(*blake3::hash(&bytes).as_bytes())
}

fn mutation_receipt(
    transaction: &rusqlite::Transaction<'_>,
    principal: &AuthenticatedPrincipal,
    operation: &str,
    target: &str,
    key: &IdempotencyKey,
    digest: CanonicalRequestDigest,
) -> Result<Option<Value>, RepoError> {
    let receipt = transaction
        .query_row(
            "SELECT request_digest,response FROM repository_mutation_receipts
             WHERE principal_id=?1 AND project_id=?2 AND operation=?3 AND target=?4 AND idempotency_key=?5",
            params![principal.principal_id().to_string(),principal.grant_snapshot().project_id().to_string(),operation,target,key.as_str()],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(map_sql_error)?;
    match receipt {
        Some((found, _)) if found.as_slice() != digest.as_bytes() => Err(RepoError::Conflict),
        Some((_, response)) => serde_json::from_str(&response)
            .map(Some)
            .map_err(|_| RepoError::Internal),
        None => Ok(None),
    }
}

fn ensure_action_queue_admission(
    transaction: &rusqlite::Transaction<'_>,
    target: &str,
) -> Result<(), RepoError> {
    let status: String = transaction
        .query_row(
            "SELECT status FROM repository_operations WHERE result_id=?1",
            [target],
            |row| row.get(0),
        )
        .map_err(map_sql_error)?;
    if status != "waiting_approval" {
        return Ok(());
    }
    let admitted: u64 = transaction
        .query_row(
            "SELECT count(*) FROM repository_operations WHERE status IN ('queued','running')",
            [],
            |row| row.get(0),
        )
        .map_err(map_sql_error)?;
    if admitted >= (REPOSITORY_QUEUE_CAPACITY + REPOSITORY_WORKERS) as u64 {
        return Err(RepoError::Unavailable(
            "repository_queue_saturated".to_owned(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_mutation_receipt(
    transaction: &rusqlite::Transaction<'_>,
    principal: &AuthenticatedPrincipal,
    operation: &str,
    target: &str,
    key: &IdempotencyKey,
    digest: CanonicalRequestDigest,
    response: &Value,
    now: i64,
) -> Result<(), RepoError> {
    transaction
        .execute(
            "INSERT INTO repository_mutation_receipts
             (principal_id,project_id,operation,target,idempotency_key,request_digest,response,created_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![principal.principal_id().to_string(),principal.grant_snapshot().project_id().to_string(),operation,target,key.as_str(),digest.as_bytes().as_slice(),response.to_string(),now],
        )
        .map(|_| ())
        .map_err(map_sql_error)
}

fn open_repository_connection(database: &Path) -> Result<Connection, RepoError> {
    let connection = Connection::open(database).map_err(map_sql_error)?;
    connection
        .busy_timeout(SQLITE_BUSY_TIMEOUT)
        .map_err(map_sql_error)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(map_sql_error)?;
    Ok(connection)
}

fn map_sql_error(error: rusqlite::Error) -> RepoError {
    match &error {
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(
                code.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            RepoError::Unavailable("repository_store_busy".to_owned())
        }
        _ => RepoError::Internal,
    }
}

fn map_store_error(error: crate::store::sqlite::append::StoreError) -> RepoError {
    use crate::store::sqlite::append::StoreError;
    match error {
        StoreError::Busy => RepoError::Unavailable("repository_store_busy".to_owned()),
        StoreError::Database(error) => map_sql_error(error),
        _ => RepoError::Internal,
    }
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    name: &str,
    definition: &str,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| error.to_string())?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    if !columns.iter().any(|column| column == name) {
        connection
            .execute(
                &format!("ALTER TABLE {table} ADD COLUMN {name} {definition}"),
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn parse_tool(value: &str) -> Result<NativeTool, RepoError> {
    NativeTool::ALL
        .into_iter()
        .find(|tool| tool.short_name() == value)
        .ok_or(RepoError::Internal)
}

fn load_operation(database: &Path, id: &str) -> Result<RepositoryOperation, RepoError> {
    let row = open_repository_connection(database)?.query_row(
        "SELECT principal_id,project_id,tool,idempotency_key,input,grants,config,run_id,attempt_id,fence,lease_version,approval_state,cancellation_requested
         ,reservation_cost,reservation_tokens,reservation_turns,reservation_tools,reservation_processes,created_at FROM repository_operations WHERE result_id=?1",
        [id],
        |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?,row.get::<_, String>(2)?,row.get::<_, String>(3)?,row.get::<_, Vec<u8>>(4)?,row.get::<_, String>(5)?,row.get::<_, Vec<u8>>(6)?,row.get::<_, String>(7)?,row.get::<_, String>(8)?,row.get::<_, u64>(9)?,row.get::<_, u64>(10)?,row.get::<_, String>(11)?,row.get::<_, bool>(12)?,row.get::<_, u64>(13)?,row.get::<_, u64>(14)?,row.get::<_, u64>(15)?,row.get::<_, u64>(16)?,row.get::<_, u64>(17)?,row.get::<_, i64>(18)?)),
    ).optional().map_err(map_sql_error)?.ok_or(RepoError::NotFound)?;
    let principal_id = row.0.parse().map_err(|_| RepoError::Internal)?;
    let project = row.1.parse().map_err(|_| RepoError::Internal)?;
    let grants: BTreeSet<Grant> = serde_json::from_str(&row.5).map_err(|_| RepoError::Internal)?;
    let principal =
        AuthenticatedPrincipal::from_grants(GrantSnapshot::new(principal_id, project, grants));
    let run_id = row.7.parse().map_err(|_| RepoError::Internal)?;
    let attempt_id = row.8.parse().map_err(|_| RepoError::Internal)?;
    let fence = FencingToken::new(row.9);
    Ok(RepositoryOperation {
        id: ToolCallId::parse(id).map_err(|_| RepoError::Invalid("result_id"))?,
        principal,
        project,
        tool: parse_tool(&row.2)?,
        key: IdempotencyKey::parse(&row.3).map_err(|_| RepoError::Internal)?,
        input: row.4,
        config: crate::domain::config::RunConfigSnapshot::from_canonical_bytes(&row.6)
            .map_err(|_| RepoError::Internal)?,
        run_id,
        attempt: AttemptOwnership::new(attempt_id, principal_id, fence),
        claim: crate::api::service::AttemptDriverClaim {
            run_id,
            attempt_id,
            principal_id,
            fence,
            lease_version: row.10,
            expires_at_unix_micros: i64::MAX,
        },
        approval: match row.11.as_str() {
            "not_required" => ApprovalState::NotRequired,
            "approved" => ApprovalState::Approved,
            "denied" => ApprovalState::Denied,
            _ => ApprovalState::Pending,
        },
        cancelled: row.12,
        reservation: crate::runtime::scheduler::limits::Spend::new(
            row.13, row.14, row.15, row.16, row.17,
        ),
        created_at: row.18,
    })
}

fn kernel_operation_events(
    database: &Path,
    run_id: RunId,
    invocation_id: &str,
) -> Result<Vec<Value>, RepoError> {
    let connection = open_repository_connection(database)?;
    let mut statement = connection
        .prepare(
            "SELECT commit_position,event_type,payload FROM events
             WHERE correlation_id=?1 AND stream=?2
               AND commit_position <= (SELECT position FROM commit_watermark WHERE singleton=1)
             ORDER BY commit_position",
        )
        .map_err(map_sql_error)?;
    let rows = statement
        .query_map(params![run_id.to_string(), invocation_id], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })
        .map_err(map_sql_error)?;
    let mut events = Vec::new();
    for row in rows {
        let (position, event_type, payload) = row.map_err(map_sql_error)?;
        let payload = serde_json::from_slice::<Value>(&payload).map_err(|_| RepoError::Internal)?;
        if payload.get("invocation_id").and_then(Value::as_str) == Some(invocation_id) {
            events.push(json!({
                "cursor":format!("kernel-event-{position:016x}"),
                "type":event_type,
                "payload":payload,
            }));
        }
    }
    Ok(events)
}

fn effect_journal_state(
    database: &Path,
    run_id: RunId,
    invocation_id: &str,
) -> Result<EffectJournalState, RepoError> {
    let events = kernel_operation_events(database, run_id, invocation_id)?;
    if let Some(event) = events
        .iter()
        .rev()
        .find(|event| event["type"] == "capability.invocation_outcome")
    {
        let outcome = serde_json::from_value(event["payload"]["result"].clone())
            .map_err(|_| RepoError::Internal)?;
        return Ok(EffectJournalState::Outcome(outcome));
    }
    if events
        .iter()
        .any(|event| event["type"] == "capability.invocation_dispatched")
    {
        Ok(EffectJournalState::Dispatched)
    } else {
        Ok(EffectJournalState::Predispatch)
    }
}

fn interrupted_after_dispatch() -> CanonicalInvocationResult {
    CanonicalInvocationResult {
        status: InvocationStatus::OutcomeUnknown,
        output: None,
        code: Some("restart_requires_reconciliation".to_owned()),
        charged: true,
    }
}

fn inject_finalization(
    fail: &mut impl FnMut(FinalizationPoint) -> bool,
    point: FinalizationPoint,
) -> Result<(), RepoError> {
    if fail(point) {
        Err(RepoError::Internal)
    } else {
        Ok(())
    }
}

fn append_operation_event(
    database: &Path,
    id: &str,
    event_type: &str,
    payload: Value,
) -> Result<(), RepoError> {
    let mut connection = open_repository_connection(database)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sql_error)?;
    append_operation_event_tx(
        &transaction,
        id,
        event_type,
        payload,
        now_unix_micros().map_err(|_| RepoError::Internal)?,
    )?;
    transaction.commit().map_err(map_sql_error)
}

fn append_operation_event_tx(
    transaction: &rusqlite::Transaction<'_>,
    id: &str,
    event_type: &str,
    payload: Value,
    now: i64,
) -> Result<(), RepoError> {
    transaction.execute(
        "INSERT INTO repository_operation_events(result_id,sequence,event_type,payload,created_at)
         SELECT ?1,COALESCE(MAX(sequence),0)+1,?2,?3,?4 FROM repository_operation_events WHERE result_id=?1",
        params![id,event_type,payload.to_string(),now],
    ).map(|_| ()).map_err(map_sql_error)
}

fn append_kernel_operation_event(
    database: &Path,
    id: &str,
    event_type: &str,
    payload: Value,
) -> Result<(), RepoError> {
    let mut connection = open_repository_connection(database)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sql_error)?;
    let payload = payload.to_string();
    let now = now_unix_micros().map_err(|_| RepoError::Internal)?;
    transaction.execute(
        "INSERT INTO repository_operation_events(result_id,sequence,event_type,payload,created_at)
         SELECT ?1,(SELECT COALESCE(MAX(sequence),0)+1 FROM repository_operation_events WHERE result_id=?1),?2,?3,?4
         WHERE NOT EXISTS (
             SELECT 1 FROM repository_operation_events
             WHERE result_id=?1 AND event_type=?2 AND payload=?3
           )",
        params![id, event_type, payload, now],
    ).map_err(map_sql_error)?;
    transaction.commit().map_err(map_sql_error)
}

fn upsert_terminal_event_tx(
    transaction: &rusqlite::Transaction<'_>,
    id: &str,
    event_type: &str,
    payload: Value,
    now: i64,
) -> Result<(), RepoError> {
    let payload = payload.to_string();
    let payload_digest = digest(&payload);
    let sequence = transaction
        .query_row(
            "SELECT MIN(sequence) FROM repository_operation_events
             WHERE result_id=?1 AND event_type='repository.operation_terminal'",
            [id],
            |row| row.get::<_, Option<u64>>(0),
        )
        .map_err(map_sql_error)?;
    if let Some(sequence) = sequence {
        transaction
            .execute(
                "UPDATE repository_operation_events
                 SET event_type=?3,payload=?4,payload_digest=?5,migration_version=?6,created_at=?7
              WHERE result_id=?1 AND sequence=?2",
                params![
                    id,
                    sequence,
                    event_type,
                    payload,
                    payload_digest,
                    TERMINAL_EVENT_MIGRATION_VERSION,
                    now
                ],
            )
            .map_err(map_sql_error)?;
        transaction
            .execute(
                "DELETE FROM repository_operation_events
             WHERE result_id=?1 AND event_type='repository.operation_terminal' AND sequence<>?2",
                params![id, sequence],
            )
            .map_err(map_sql_error)?;
        Ok(())
    } else {
        transaction
            .execute(
                "INSERT INTO repository_operation_events
                 (result_id,sequence,event_type,payload,payload_digest,migration_version,created_at)
                 SELECT ?1,COALESCE(MAX(sequence),0)+1,?2,?3,?4,?5,?6
                 FROM repository_operation_events WHERE result_id=?1",
                params![
                    id,
                    event_type,
                    payload,
                    payload_digest,
                    TERMINAL_EVENT_MIGRATION_VERSION,
                    now
                ],
            )
            .map(|_| ())
            .map_err(map_sql_error)
    }
}

fn terminal_event_payload(
    id: ToolCallId,
    outcome: Option<&CanonicalInvocationResult>,
    result: &Value,
) -> Result<Value, RepoError> {
    let result_bytes = serde_json::to_vec(result).map_err(|_| RepoError::Internal)?;
    Ok(json!({
        "schema_version": SCHEMA_VERSION,
        "migration_version": TERMINAL_EVENT_MIGRATION_VERSION,
        "operation_id": id,
        "native_result_id": id,
        "outcome": outcome,
        "result_digest": digest_bytes(&result_bytes),
        "result": result,
        "cost": result["cost"],
        "artifacts": result["artifacts"],
    }))
}

fn completion_matches(
    transaction: &rusqlite::Transaction<'_>,
    id: &str,
    status: &str,
    result: &Value,
    terminal: &Value,
) -> Result<bool, RepoError> {
    let row = transaction
        .query_row(
            "SELECT status,result FROM repository_operations WHERE result_id=?1",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .map_err(map_sql_error)?;
    let stored_result = row
        .1
        .as_deref()
        .and_then(|value| serde_json::from_str::<Value>(value).ok());
    if row.0 != status || stored_result.as_ref() != Some(result) {
        return Ok(false);
    }

    let mut statement = transaction
        .prepare(
            "SELECT payload,payload_digest,migration_version
             FROM repository_operation_events
             WHERE result_id=?1 AND event_type='repository.operation_terminal'
             ORDER BY sequence",
        )
        .map_err(map_sql_error)?;
    let events = statement
        .query_map([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u16>(2)?,
            ))
        })
        .map_err(map_sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sql_error)?;
    let expected = terminal.to_string();
    Ok(
        matches!(events.as_slice(), [(payload, payload_digest, migration_version)]
        if payload == &expected
            && payload_digest == &digest(&expected)
            && *migration_version == TERMINAL_EVENT_MIGRATION_VERSION),
    )
}

fn digest(value: &str) -> String {
    digest_bytes(value.as_bytes())
}

fn digest_bytes(value: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(value).to_hex())
}

fn put_report(
    artifacts: &ArtifactStore,
    principal: &AuthenticatedPrincipal,
    project: ProjectId,
    correlation: &str,
    kind: &str,
    value: &Value,
    stored_at: i64,
) -> Result<Value, RepoError> {
    let bytes = serde_json::to_vec(value).map_err(|_| RepoError::Internal)?;
    let reference = ArtifactReference::derive(
        b"kit-repository-report-v1",
        format!("{correlation}:{kind}").as_bytes(),
    );
    let artifact = artifacts
        .put_with_reference(
            &bytes,
            ArtifactMetadata::new(
                "application/json",
                ArtifactClass::Report,
                principal.principal_id().to_string(),
                project.to_string(),
                ArtifactRetention::Forever,
                stored_at,
            )
            .map_err(|_| RepoError::Internal)?,
            reference,
        )
        .map_err(|_| RepoError::Internal)?;
    Ok(json!({
        "reference":artifact.reference().to_string(),"digest":artifact.digest().to_string(),"media_type":"application/json","size":bytes.len(),
        "provenance":{"kind":kind,"operation_id":correlation,"native_result_id":correlation,"principal_id":principal.principal_id(),"project_id":project},
    }))
}

fn referenced_artifact(
    artifacts: &ArtifactStore,
    operation: &RepositoryOperation,
    reference: Option<&str>,
    kind: &str,
) -> Result<Value, RepoError> {
    let Some(reference) = reference else {
        return Ok(Value::Null);
    };
    let artifact = ArtifactReference::parse(reference)
        .ok()
        .and_then(|reference| artifacts.open_reference(reference).ok())
        .filter(|artifact| {
            artifact.manifest().principal == operation.principal.principal_id().to_string()
                && artifact.manifest().project == operation.project.to_string()
        });
    Ok(artifact.map_or(Value::Null, |artifact| json!({
        "reference":reference,"digest":artifact.digest().to_string(),"media_type":artifact.manifest().media_type,"size":artifact.manifest().size,
        "provenance":{"kind":kind,"operation_id":operation.id,"native_result_id":operation.id,"principal_id":operation.principal.principal_id(),"project_id":operation.project},
    })))
}

fn referenced_diff_artifact(
    artifacts: &ArtifactStore,
    operation: &RepositoryOperation,
    value: Option<&Value>,
) -> Result<Value, RepoError> {
    let Some(value) = value else {
        return Ok(Value::Null);
    };
    let reference = value
        .get("reference")
        .and_then(Value::as_str)
        .and_then(|value| ArtifactReference::parse(value).ok())
        .ok_or(RepoError::Internal)?;
    let artifact = artifacts
        .open_reference(reference)
        .map_err(|_| RepoError::Internal)?;
    let manifest = artifact.manifest();
    let provenance = value.get("provenance").ok_or(RepoError::Internal)?;
    let principal = operation.principal.principal_id().to_string();
    let project = operation.project.to_string();
    let transaction = provenance
        .get("transaction_id")
        .and_then(Value::as_str)
        .ok_or(RepoError::Internal)?;
    let revision = provenance
        .get("revision_id")
        .and_then(Value::as_str)
        .ok_or(RepoError::Internal)?;
    let digest = artifact.digest().to_string();
    if value.get("digest").and_then(Value::as_str) != Some(digest.as_str())
        || value.get("media_type").and_then(Value::as_str) != Some("text/x-diff; charset=utf-8")
        || value.get("class").and_then(Value::as_str) != Some("diff")
        || manifest.media_type != "text/x-diff; charset=utf-8"
        || manifest.class != ArtifactClass::Diff
        || manifest.principal != principal
        || manifest.project != project
        || provenance.get("principal_id").and_then(Value::as_str) != Some(principal.as_str())
        || provenance.get("project_id").and_then(Value::as_str) != Some(project.as_str())
    {
        return Err(RepoError::Internal);
    }
    let mut prefix = Vec::new();
    artifacts
        .with_reference_reader(reference, |_, reader| {
            reader.take(4096).read_to_end(&mut prefix)?;
            Ok(())
        })
        .map_err(|_| RepoError::Internal)?;
    let expected = format!(
        "kit-actual-diff-v1\ntransaction={transaction}\nrevision={revision}\nprincipal={principal}\nproject={project}\n"
    );
    if !prefix.starts_with(expected.as_bytes()) {
        return Err(RepoError::Internal);
    }
    Ok(json!({
        "reference": reference.to_string(),
        "digest": artifact.digest().to_string(),
        "media_type": manifest.media_type,
        "size": manifest.size,
        "provenance": {
            "kind": "actual_diff",
            "operation_id": operation.id,
            "native_result_id": operation.id,
            "principal_id": operation.principal.principal_id(),
            "project_id": operation.project,
            "transaction_id": transaction,
            "revision_id": revision,
        },
    }))
}

fn spend_json(spend: Option<crate::runtime::scheduler::limits::Spend>) -> Value {
    let spend = spend.unwrap_or_default();
    json!({
        "cost_microusd":spend.cost_microusd(),
        "tokens":spend.tokens(),
        "turns":spend.turns(),
        "tools":spend.tools(),
        "processes":spend.processes(),
    })
}

fn operation_error(code: &str, status: &str) -> Value {
    let (code, detail) = code
        .split_once(':')
        .map_or((code, None), |(code, detail)| (code, Some(detail)));
    json!({
        "code":code,
        "detail":detail,
        "effect_state":match status {
            "cancelled" | "denied" | "waiting_approval" => "none",
            "outcome_unknown" => "unknown",
            _ => "attempted",
        },
        "retryable":matches!(status,"outcome_unknown"),
    })
}

fn explicit_artifacts(
    artifacts: &ArtifactStore,
    operation: &RepositoryOperation,
    output: Option<&Value>,
    cost: Value,
    events: Value,
) -> Result<Value, RepoError> {
    let output = output.unwrap_or(&Value::Null);
    let verification = output
        .get("data")
        .and_then(|value| value.get("verification"))
        .or_else(|| output.get("verification"));
    let feedback = output
        .get("data")
        .and_then(|value| value.get("feedback_artifacts"))
        .or_else(|| output.get("feedback_artifacts"));
    let diff = output
        .get("data")
        .and_then(|value| value.get("diff_artifact"))
        .or_else(|| output.get("diff_artifact"));
    let receipt = verification
        .and_then(|value| value.pointer("/result_artifact/reference"))
        .and_then(Value::as_str);
    let logs = verification
        .and_then(|value| value.pointer("/stdout_artifacts/0/reference"))
        .and_then(Value::as_str);
    Ok(json!({
        "repository_result":Value::Null,
        "actual_diff":referenced_diff_artifact(artifacts,operation,diff)?,
        "verification_receipt":referenced_artifact(artifacts,operation,receipt,"verification_receipt")?,
        "verification_logs":referenced_artifact(artifacts,operation,logs,"verification_logs")?,
        "feedback_payload":referenced_artifact(artifacts,operation,feedback.and_then(|value| value.get("payload_artifact")).and_then(Value::as_str),"feedback_payload")?,
        "feedback_report":referenced_artifact(artifacts,operation,feedback.and_then(|value| value.get("report_artifact")).and_then(Value::as_str),"feedback_report")?,
        "edit_events":events,
        "cost":cost,
    }))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MigrationPage {
    processed: usize,
    pending: bool,
    queued: Vec<String>,
}

fn start_operation_metadata_migration(
    database: PathBuf,
    artifacts: Arc<ArtifactStore>,
    scheduled: Arc<Mutex<BTreeSet<String>>>,
    queue: Arc<SyncSender<String>>,
    queue_pump: SyncSender<()>,
) -> Result<(), String> {
    let current: bool = open_repository_connection(&database)
        .map_err(|error| format!("{error:?}"))?
        .query_row(
            "SELECT version >= ?2 FROM repository_schema_migrations WHERE name=?1",
            params![
                OPERATION_METADATA_MIGRATION,
                OPERATION_METADATA_MIGRATION_VERSION
            ],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if current {
        return Ok(());
    }
    std::thread::Builder::new()
        .name("kit-repository-migration".to_owned())
        .spawn(move || {
            let mut budget = STARTUP_MIGRATION_BUDGET;
            loop {
                match reconcile_operation_metadata_page(
                    &database,
                    &artifacts,
                    Some(Instant::now() + budget),
                ) {
                    Ok(page) => {
                        for id in page.queued {
                            match try_schedule(&scheduled, &queue, id) {
                                Ok(SubmitState::Submitted | SubmitState::AlreadyScheduled) => {}
                                Ok(SubmitState::QueueFull) | Err(_) => {
                                    let _ = queue_pump.try_send(());
                                    break;
                                }
                            }
                        }
                        if !page.pending {
                            break;
                        }
                        std::thread::yield_now();
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
                budget = BACKGROUND_MIGRATION_BUDGET;
            }
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn reconcile_operation_metadata_page(
    database: &Path,
    artifacts: &ArtifactStore,
    deadline: Option<Instant>,
) -> Result<MigrationPage, String> {
    let connection = open_repository_connection(database).map_err(|error| format!("{error:?}"))?;
    let (version, last_key): (u16, String) = connection
        .query_row(
            "SELECT version,last_key FROM repository_schema_migrations WHERE name=?1",
            [OPERATION_METADATA_MIGRATION],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| error.to_string())?;
    if version >= OPERATION_METADATA_MIGRATION_VERSION {
        return Ok(MigrationPage {
            processed: 0,
            pending: false,
            queued: Vec::new(),
        });
    }
    let ids = {
        let mut statement = connection
            .prepare(
                "SELECT result_id FROM repository_operations
                 WHERE result_id > ?1
                 ORDER BY result_id LIMIT ?2",
            )
            .map_err(|error| error.to_string())?;
        statement
            .query_map(params![last_key, MIGRATION_PAGE_SIZE], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
    };
    drop(connection);

    let selected = ids.len();
    let mut processed = 0;
    let mut queued = Vec::new();
    for id in ids {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
        if reconcile_operation_metadata_row(database, artifacts, &id)
            .map_err(|error| format!("{error:?}"))?
        {
            queued.push(id.clone());
        }
        open_repository_connection(database)
            .map_err(|error| format!("{error:?}"))?
            .execute(
                "UPDATE repository_schema_migrations SET last_key=?2
                 WHERE name=?1 AND version < ?3 AND last_key < ?2",
                params![
                    OPERATION_METADATA_MIGRATION,
                    id,
                    OPERATION_METADATA_MIGRATION_VERSION
                ],
            )
            .map_err(|error| error.to_string())?;
        processed += 1;
    }
    let complete = processed == selected && selected < MIGRATION_PAGE_SIZE;
    if complete {
        open_repository_connection(database)
            .map_err(|error| format!("{error:?}"))?
            .execute(
                "UPDATE repository_schema_migrations SET version=?2
                 WHERE name=?1 AND version < ?2",
                params![
                    OPERATION_METADATA_MIGRATION,
                    OPERATION_METADATA_MIGRATION_VERSION
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(MigrationPage {
        processed,
        pending: !complete,
        queued,
    })
}

fn reconcile_operation_metadata_row(
    database: &Path,
    artifacts: &ArtifactStore,
    id: &str,
) -> Result<bool, RepoError> {
    let connection = open_repository_connection(database)?;
    let row = connection
        .query_row(
            "SELECT migration_version,run_id,status,replayed
             FROM repository_operations WHERE result_id=?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, u16>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            },
        )
        .optional()
        .map_err(map_sql_error)?
        .ok_or(RepoError::NotFound)?;
    if row.0 >= OPERATION_METADATA_MIGRATION_VERSION {
        return Ok(false);
    }
    connection
        .execute(
            "UPDATE repository_operations SET reservation_processes=1
             WHERE result_id=?1 AND tool IN ('run','check') AND reservation_processes=0",
            [id],
        )
        .map_err(map_sql_error)?;
    let run_id = row.1.parse().map_err(|_| RepoError::Internal)?;
    match effect_journal_state(database, run_id, id)? {
        EffectJournalState::Outcome(outcome) => {
            NativeRepoWorker::reconcile_completion(
                database,
                artifacts,
                load_operation(database, id)?,
                outcome,
                row.3,
            )?;
            Ok(false)
        }
        EffectJournalState::Dispatched => {
            NativeRepoWorker::complete(
                database,
                artifacts,
                load_operation(database, id)?,
                interrupted_after_dispatch(),
                false,
            )?;
            Ok(false)
        }
        EffectJournalState::Predispatch => reconcile_predispatch_operation(database, id, &row.2),
    }
}

fn reconcile_predispatch_operation(
    database: &Path,
    id: &str,
    observed_status: &str,
) -> Result<bool, RepoError> {
    let now = now_unix_micros().map_err(|_| RepoError::Internal)?;
    let mut connection = open_repository_connection(database)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sql_error)?;
    let (version, status, result): (u16, String, Option<String>) = transaction
        .query_row(
            "SELECT migration_version,status,result FROM repository_operations WHERE result_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(map_sql_error)?;
    if version >= OPERATION_METADATA_MIGRATION_VERSION {
        transaction.commit().map_err(map_sql_error)?;
        return Ok(false);
    }
    let mut queued = false;
    if status == "running" && observed_status == "running" {
        transaction
            .execute(
                "UPDATE repository_operations SET status='queued',result=NULL,updated_at=?2
                 WHERE result_id=?1",
                params![id, now],
            )
            .map_err(map_sql_error)?;
        queued = true;
        transaction
            .execute(
                "DELETE FROM repository_operation_events
                 WHERE result_id=?1 AND event_type='repository.operation_terminal'",
                [id],
            )
            .map_err(map_sql_error)?;
        append_operation_event_tx(
            &transaction,
            id,
            "repository.operation_requeued",
            json!({"operation_id":id,"native_result_id":id,"reason":"interrupted_before_dispatch"}),
            now,
        )?;
    } else if let Some(result) = result {
        let result = serde_json::from_str(&result).map_err(|_| RepoError::Internal)?;
        upsert_terminal_event_tx(
            &transaction,
            id,
            "repository.operation_terminal",
            terminal_event_payload(
                ToolCallId::parse(id).map_err(|_| RepoError::Internal)?,
                None,
                &result,
            )?,
            now,
        )?;
    }
    transaction
        .execute(
            "UPDATE repository_operations SET migration_version=?2 WHERE result_id=?1",
            params![id, OPERATION_METADATA_MIGRATION_VERSION],
        )
        .map_err(map_sql_error)?;
    transaction.commit().map_err(map_sql_error)?;
    Ok(queued)
}

#[cfg(test)]
fn reconcile_operation_metadata(database: &Path, artifacts: &ArtifactStore) -> Result<(), String> {
    loop {
        let page = reconcile_operation_metadata_page(database, artifacts, None)?;
        if !page.pending {
            return Ok(());
        }
    }
}

fn reenqueue_operations(
    database: &Path,
    scheduled: &Mutex<BTreeSet<String>>,
    queue: &SyncSender<String>,
) -> Result<(), String> {
    let connection = open_repository_connection(database).map_err(|error| format!("{error:?}"))?;
    let mut statement = connection
        .prepare(
            "SELECT result_id FROM repository_operations
             WHERE status='queued'
                OR (status='running' AND EXISTS (
                  SELECT 1 FROM events
                  WHERE stream=repository_operations.result_id
                    AND event_type IN ('capability.invocation_dispatched','capability.invocation_outcome')
                    AND commit_position <= (SELECT position FROM commit_watermark WHERE singleton=1)
                ))
             ORDER BY created_at,result_id LIMIT ?1",
        )
        .map_err(|error| error.to_string())?;
    let ids = statement
        .query_map([REPOSITORY_QUEUE_CAPACITY as u64], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    for id in ids {
        match try_schedule(scheduled, queue, id).map_err(|error| format!("{error:?}"))? {
            SubmitState::Submitted | SubmitState::AlreadyScheduled => {}
            SubmitState::QueueFull => break,
        }
    }
    Ok(())
}

fn start_queue_pump(
    database: PathBuf,
    scheduled: std::sync::Weak<Mutex<BTreeSet<String>>>,
    queue: std::sync::Weak<SyncSender<String>>,
    wake: std::sync::mpsc::Receiver<()>,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("kit-repository-queue-pump".to_owned())
        .spawn(move || {
            loop {
                let (Some(scheduled), Some(queue)) = (scheduled.upgrade(), queue.upgrade()) else {
                    break;
                };
                let _ = reenqueue_operations(&database, &scheduled, &queue);
                drop((scheduled, queue));
                match wake.recv_timeout(Duration::from_millis(100)) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn migrate(database: &Path) -> Result<(), String> {
    let mut connection = Connection::open(database).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(SQLITE_BUSY_TIMEOUT)
        .map_err(|error| error.to_string())?;
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .map_err(|error| error.to_string())?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|error| error.to_string())?;
    let operations_existed = table_exists(&connection, "repository_operations")?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS repository_operations (
           result_id TEXT PRIMARY KEY,
           principal_id TEXT NOT NULL,
           project_id TEXT NOT NULL,
           operation TEXT NOT NULL,
           tool TEXT NOT NULL,
           idempotency_key TEXT NOT NULL,
           request_digest BLOB NOT NULL,
           input BLOB NOT NULL,
           grants TEXT NOT NULL,
           config BLOB NOT NULL,
           run_id TEXT NOT NULL UNIQUE,
           attempt_id TEXT NOT NULL UNIQUE,
           fence INTEGER NOT NULL,
           lease_version INTEGER NOT NULL,
           status TEXT NOT NULL CHECK(status IN ('queued','running','waiting_approval','completed','failed','cancelled','outcome_unknown','denied')),
           approval_id TEXT,
           approval_state TEXT NOT NULL CHECK(approval_state IN ('not_required','pending','approved','denied')),
           cancellation_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancellation_requested IN (0,1)),
           replayed INTEGER NOT NULL DEFAULT 0 CHECK(replayed IN (0,1)),
           reservation_cost INTEGER NOT NULL DEFAULT 0,
           reservation_tokens INTEGER NOT NULL DEFAULT 0,
           reservation_turns INTEGER NOT NULL DEFAULT 0,
           reservation_tools INTEGER NOT NULL DEFAULT 1,
           reservation_processes INTEGER NOT NULL DEFAULT 0,
           migration_version INTEGER NOT NULL DEFAULT 1,
           result TEXT,
           created_at INTEGER NOT NULL,
           updated_at INTEGER NOT NULL,
           UNIQUE(principal_id, project_id, operation, idempotency_key)
         );
          CREATE TABLE IF NOT EXISTS repository_operation_events (
            result_id TEXT NOT NULL,
            sequence INTEGER NOT NULL,
            event_type TEXT NOT NULL,
            payload TEXT NOT NULL,
            payload_digest TEXT NOT NULL DEFAULT '',
            migration_version INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL,
           PRIMARY KEY(result_id, sequence),
           FOREIGN KEY(result_id) REFERENCES repository_operations(result_id) ON DELETE CASCADE
          );
          CREATE TABLE IF NOT EXISTS repository_mutation_receipts (
            principal_id TEXT NOT NULL,
            project_id TEXT NOT NULL,
            operation TEXT NOT NULL,
            target TEXT NOT NULL,
            idempotency_key TEXT NOT NULL,
            request_digest BLOB NOT NULL,
            response TEXT NOT NULL,
            created_at INTEGER NOT NULL,
           PRIMARY KEY(principal_id, project_id, operation, target, idempotency_key)
          );
          CREATE TABLE IF NOT EXISTS repository_schema_migrations (
            name TEXT PRIMARY KEY,
            version INTEGER NOT NULL,
            last_key TEXT NOT NULL
          );
          CREATE INDEX IF NOT EXISTS repository_operations_status_idx ON repository_operations(status, created_at);",
    ).map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())?;
    for (name, definition) in [
        ("reservation_cost", "INTEGER NOT NULL DEFAULT 0"),
        ("reservation_tokens", "INTEGER NOT NULL DEFAULT 0"),
        ("reservation_turns", "INTEGER NOT NULL DEFAULT 0"),
        ("reservation_tools", "INTEGER NOT NULL DEFAULT 1"),
        ("reservation_processes", "INTEGER NOT NULL DEFAULT 0"),
        ("migration_version", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        ensure_column(&connection, "repository_operations", name, definition)?;
    }
    for (name, definition) in [
        ("payload_digest", "TEXT NOT NULL DEFAULT ''"),
        ("migration_version", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        ensure_column(&connection, "repository_operation_events", name, definition)?;
    }
    connection
        .execute(
            "INSERT OR IGNORE INTO repository_schema_migrations(name,version,last_key)
             VALUES(?1,?2,'')",
            params![
                OPERATION_METADATA_MIGRATION,
                if operations_existed {
                    0
                } else {
                    OPERATION_METADATA_MIGRATION_VERSION
                }
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, String> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}

fn load_result(
    database: &Path,
    principal: &AuthenticatedPrincipal,
    id: &str,
    column: &str,
) -> Result<Value, RepoError> {
    let connection = open_repository_connection(database)?;
    load_result_from_connection(&connection, principal, id, column)
}

fn load_reconciled_result(
    database: &Path,
    artifacts: &ArtifactStore,
    principal: &AuthenticatedPrincipal,
    id: &str,
    column: &str,
) -> Result<Value, RepoError> {
    if ToolCallId::parse(id).is_err() {
        return Err(RepoError::Invalid("result_id"));
    }
    let version = open_repository_connection(database)?
        .query_row(
            "SELECT migration_version FROM repository_operations
             WHERE result_id=?1 AND principal_id=?2 AND project_id=?3",
            params![
                id,
                principal.principal_id().to_string(),
                principal.grant_snapshot().project_id().to_string()
            ],
            |row| row.get::<_, u16>(0),
        )
        .optional()
        .map_err(map_sql_error)?
        .ok_or(RepoError::NotFound)?;
    if version < OPERATION_METADATA_MIGRATION_VERSION {
        reconcile_operation_metadata_row(database, artifacts, id)?;
    }
    load_result(database, principal, id, column)
}

fn load_result_from_connection(
    connection: &Connection,
    principal: &AuthenticatedPrincipal,
    id: &str,
    column: &str,
) -> Result<Value, RepoError> {
    if ToolCallId::parse(id).is_err() {
        return Err(RepoError::Invalid("result_id"));
    }
    let row = connection.query_row(
            "SELECT operation,status,approval_id,approval_state,cancellation_requested,replayed,result,run_id,attempt_id,fence,created_at,updated_at
             FROM repository_operations WHERE result_id=?1 AND principal_id=?2 AND project_id=?3",
            params![
                id,
                principal.principal_id().to_string(),
                principal.grant_snapshot().project_id().to_string()
            ],
            |row| Ok((
                row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?, row.get::<_, bool>(4)?, row.get::<_, bool>(5)?,
                row.get::<_, Option<String>>(6)?, row.get::<_, String>(7)?, row.get::<_, String>(8)?,
                row.get::<_, u64>(9)?, row.get::<_, i64>(10)?, row.get::<_, i64>(11)?,
            )),
        )
        .optional()
        .map_err(map_sql_error)?
        .ok_or(RepoError::NotFound)?;
    if column == "events" {
        let mut statement = connection.prepare(
            "SELECT sequence,event_type,payload,created_at FROM repository_operation_events WHERE result_id=?1 ORDER BY sequence",
        ).map_err(map_sql_error)?;
        let items = statement
            .query_map([id], |row| {
                let payload: String = row.get(2)?;
                Ok(json!({
                    "cursor": format!("repo-operation-{:016x}", row.get::<_, u64>(0)?),
                    "sequence": row.get::<_, u64>(0)?,
                    "type": row.get::<_, String>(1)?,
                    "payload": serde_json::from_str::<Value>(&payload).unwrap_or(Value::Null),
                    "created_at_unix_micros": row.get::<_, i64>(3)?,
                }))
            })
            .map_err(map_sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_error)?;
        return Ok(json!({"schema_version":SCHEMA_VERSION,"result_id":id,"items":items}));
    }
    if let Some(result) = row.6 {
        return serde_json::from_str(&result).map_err(|_| RepoError::Internal);
    }
    Ok(json!({
        "schema_version": SCHEMA_VERSION,
        "id": id,
        "native_operation_id": id,
        "native_result_id": id,
        "operation": row.0,
        "status": row.1,
        "replayed": row.5,
        "approval": {"id": row.2, "state": row.3},
        "cancellation_requested": row.4,
        "owner": {"run_id":row.7,"attempt_id":row.8,"fence":row.9},
        "created_at_unix_micros": row.10,
        "updated_at_unix_micros": row.11,
        "output": Value::Null,
        "error": Value::Null,
        "cost": Value::Null,
        "artifacts": Value::Null,
    }))
}

fn load_artifact(
    artifacts: &ArtifactStore,
    principal: &AuthenticatedPrincipal,
    reference: &str,
) -> Result<RepoArtifact, RepoError> {
    let artifact = match ArtifactReference::parse(reference) {
        Ok(reference) => artifacts
            .resolve_reference(principal, reference)
            .map_err(|_| RepoError::NotFound)?,
        Err(_) => {
            let digest =
                ArtifactDigest::parse(reference).map_err(|_| RepoError::Invalid("artifact_ref"))?;
            let artifact = artifacts
                .open_verified(digest)
                .map_err(|_| RepoError::NotFound)?;
            let grants = principal.grant_snapshot();
            if !grants.grants().contains(&Grant::WorkspaceRead)
                || artifact.manifest().principal != grants.principal_id().to_string()
                || artifact.manifest().project != grants.project_id().to_string()
            {
                return Err(RepoError::NotFound);
            }
            artifact
        }
    };
    let bytes = artifacts
        .open_bytes(artifact.digest())
        .map_err(|_| RepoError::NotFound)?;
    Ok(RepoArtifact {
        bytes,
        digest: artifact.digest().to_string(),
        media_type: artifact.manifest().media_type.clone(),
        class: artifact.manifest().class.as_str().to_owned(),
        principal: artifact.manifest().principal.clone(),
        project: artifact.manifest().project.clone(),
    })
}

pub fn routes(service: Arc<dyn RepoService>) -> Router {
    Router::new()
        .route("/v1/repository/status", get(repository_status))
        .route(
            "/v1/projects/{project_id}/repository/revision",
            get(revision),
        )
        .route(
            "/v1/projects/{project_id}/repository/capabilities",
            get(capabilities),
        )
        .route(
            "/v1/projects/{project_id}/repository/discover",
            post(discover),
        )
        .route("/v1/projects/{project_id}/repository/search", post(search))
        .route("/v1/projects/{project_id}/repository/read", post(read))
        .route("/v1/projects/{project_id}/repository/edit", post(edit))
        .route("/v1/projects/{project_id}/repository/run", post(run))
        .route("/v1/projects/{project_id}/repository/check", post(check))
        .route("/v1/repository-results/{result_id}", get(result))
        .route("/v1/repository-results/{result_id}/events", get(events))
        .route(
            "/v1/repository-results/{result_id}/approval",
            post(resolve_repository_approval),
        )
        .route(
            "/v1/repository-results/{result_id}/cancel",
            post(cancel_repository_operation),
        )
        .route("/v1/repository-artifacts/{artifact_ref}", get(artifact))
        .layer(Extension(service))
}

async fn repository_status(
    Extension(service): Extension<Arc<dyn RepoService>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
) -> Response {
    repo_response(service.status(&principal), false)
}

async fn revision(
    Extension(service): Extension<Arc<dyn RepoService>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(project): AxumPath<String>,
) -> Response {
    project_query(
        service,
        principal,
        project,
        |service, principal, project| service.revision(principal, project),
    )
}

async fn capabilities(
    Extension(service): Extension<Arc<dyn RepoService>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(project): AxumPath<String>,
) -> Response {
    project_query(
        service,
        principal,
        project,
        |service, principal, project| service.capabilities(principal, project),
    )
}

macro_rules! invoke_handler {
    ($name:ident, $tool:expr, $mutation:expr) => {
        async fn $name(
            Extension(service): Extension<Arc<dyn RepoService>>,
            Extension(principal): Extension<AuthenticatedPrincipal>,
            AxumPath(project): AxumPath<String>,
            request: Request,
        ) -> Response {
            let project = match ProjectId::parse(&project) {
                Ok(project) => project,
                Err(_) => return problem(RepoError::Invalid("project_id")),
            };
            let (key, body) = match request_body(request, $mutation).await {
                Ok(value) => value,
                Err(problem) => return problem.into_response(),
            };
            repo_response(
                service.invoke(&principal, project, $tool, body, key.as_ref()),
                true,
            )
        }
    };
}

invoke_handler!(discover, NativeTool::Discover, false);
invoke_handler!(search, NativeTool::Search, false);
invoke_handler!(read, NativeTool::Read, false);
invoke_handler!(edit, NativeTool::Edit, true);
invoke_handler!(run, NativeTool::Run, true);
invoke_handler!(check, NativeTool::Check, true);

async fn result(
    Extension(service): Extension<Arc<dyn RepoService>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    repo_response(service.result(&principal, &id), false)
}

async fn events(
    Extension(service): Extension<Arc<dyn RepoService>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    repo_response(service.events(&principal, &id), false)
}

async fn resolve_repository_approval(
    Extension(service): Extension<Arc<dyn RepoService>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
    request: Request,
) -> Response {
    let (key, body) = match request_body(request, true).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    let approved = match body.get("decision").and_then(Value::as_str) {
        Some("approved") => true,
        Some("denied") => false,
        _ => return problem(RepoError::Invalid("decision")),
    };
    repo_response(
        service.resolve_approval(
            &principal,
            &id,
            approved,
            key.as_ref().expect("mutation key was validated"),
        ),
        true,
    )
}

async fn cancel_repository_operation(
    Extension(service): Extension<Arc<dyn RepoService>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(id): AxumPath<String>,
    request: Request,
) -> Response {
    let (key, body) = match request_body(request, true).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    if body.as_object().is_none_or(|body| !body.is_empty()) {
        return problem(RepoError::Invalid("body"));
    }
    repo_response(
        service.cancel(
            &principal,
            &id,
            key.as_ref().expect("mutation key was validated"),
        ),
        true,
    )
}

async fn artifact(
    Extension(service): Extension<Arc<dyn RepoService>>,
    Extension(principal): Extension<AuthenticatedPrincipal>,
    AxumPath(reference): AxumPath<String>,
) -> Response {
    match service.artifact(&principal, &reference) {
        Ok(artifact) => {
            let mut response = Body::from(artifact.bytes).into_response();
            if let Ok(media_type) = HeaderValue::from_str(&artifact.media_type) {
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, media_type);
            }
            for (name, value) in [
                ("x-kit-artifact-digest", artifact.digest),
                ("x-kit-artifact-class", artifact.class),
                ("x-kit-artifact-principal", artifact.principal),
                ("x-kit-artifact-project", artifact.project),
            ] {
                if let (Ok(name), Ok(value)) = (
                    header::HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(&value),
                ) {
                    response.headers_mut().insert(name, value);
                }
            }
            response
        }
        Err(error) => problem(error),
    }
}

fn project_query(
    service: Arc<dyn RepoService>,
    principal: AuthenticatedPrincipal,
    project: String,
    query: impl FnOnce(&dyn RepoService, &AuthenticatedPrincipal, ProjectId) -> Result<Value, RepoError>,
) -> Response {
    let project = match ProjectId::parse(&project) {
        Ok(project) => project,
        Err(_) => return problem(RepoError::Invalid("project_id")),
    };
    repo_response(query(service.as_ref(), &principal, project), false)
}

async fn request_body(
    request: Request,
    mutation: bool,
) -> Result<(Option<IdempotencyKey>, Value), ProblemDetails> {
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        != Some("application/json")
    {
        return Err(ProblemDetails::unsupported_media_type(HIDDEN_INSTANCE));
    }
    let key = request
        .headers()
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok());
    if mutation && key.is_none() {
        return Err(ProblemDetails::missing_idempotency_key(HIDDEN_INSTANCE));
    }
    let bytes = to_bytes(request.into_body(), JSON_BODY_LIMIT)
        .await
        .map_err(|_| ProblemDetails::payload_too_large(HIDDEN_INSTANCE))?;
    let body = serde_json::from_slice(&bytes).map_err(|_| {
        ProblemDetails::invalid(HIDDEN_INSTANCE, "body", "The JSON body is invalid.")
    })?;
    Ok((key, body))
}

fn repo_response(result: Result<Value, RepoError>, accepted: bool) -> Response {
    match result {
        Ok(value) if accepted => {
            let location = value
                .get("id")
                .and_then(Value::as_str)
                .map(|id| format!("/v1/repository-results/{id}"));
            let mut response = (StatusCode::ACCEPTED, Json(value)).into_response();
            if let Some(location) = location.and_then(|value| HeaderValue::from_str(&value).ok()) {
                response.headers_mut().insert(header::LOCATION, location);
            }
            response
        }
        Ok(value) => Json(value).into_response(),
        Err(error) => problem(error),
    }
}

fn problem(error: RepoError) -> Response {
    let (status, code, title, detail) = match error {
        RepoError::NotFound => (
            StatusCode::NOT_FOUND,
            "not_found",
            "Resource not found",
            "The requested resource was not found.",
        ),
        RepoError::Invalid(_) => (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid request",
            "The request did not satisfy the repository API contract.",
        ),
        RepoError::Conflict => (
            StatusCode::CONFLICT,
            "conflict",
            "Request conflict",
            "The idempotency key or resource state conflicts with this request.",
        ),
        RepoError::Stale => (
            StatusCode::CONFLICT,
            "stale_revision",
            "Stale revision",
            "The repository revision or cursor is stale.",
        ),
        RepoError::Unavailable(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "repository_service_unavailable",
            "Repository service unavailable",
            "A required native repository service is unavailable.",
        ),
        RepoError::Unsupported(_) => (
            StatusCode::NOT_IMPLEMENTED,
            "platform_unavailable",
            "Platform unavailable",
            "The requested native isolation, syntax, or formatter service is unsupported on this platform.",
        ),
        RepoError::Internal => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Internal server error",
            "The request could not be completed.",
        ),
    };
    let mut response = (
        status,
        Json(json!({
            "type": format!("https://kit.dev/problems/{code}"),
            "title": title,
            "status": status.as_u16(),
            "detail": detail,
            "instance": HIDDEN_INSTANCE,
            "code": code,
        })),
    )
        .into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(PROBLEM_MEDIA_TYPE),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::PrincipalId;

    struct Registry;

    impl crate::executor::process::own::ProcessRegistry for Registry {
        fn prepared(
            &self,
            _: crate::executor::process::own::ProcessRegistrationContext,
            _: crate::domain::lifecycle::ProcessClaim,
            _: &crate::executor::process::tree::PersistedBoundary,
            _: crate::executor::process::own::ProcessTerminalConfig,
        ) -> std::io::Result<()> {
            Ok(())
        }

        fn started(
            &self,
            _: crate::executor::process::own::ProcessRegistrationContext,
            _: &crate::executor::process::own::ProcessRecord,
        ) -> std::io::Result<()> {
            Ok(())
        }

        fn exited(
            &self,
            _: crate::executor::process::own::ProcessRegistrationContext,
            _: &crate::executor::process::own::ProcessRecord,
        ) -> std::io::Result<()> {
            Ok(())
        }

        fn outcome_unknown(
            &self,
            _: crate::executor::process::own::ProcessRegistrationContext,
            _: crate::domain::ids::ProcessId,
        ) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn database(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "kit-repository-api-{name}-{}-{}",
            std::process::id(),
            ToolCallId::generate().unwrap()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory.join("state.sqlite3")
    }

    fn principal(
        principal_id: PrincipalId,
        project_id: ProjectId,
        grants: impl IntoIterator<Item = Grant>,
        approval: bool,
    ) -> AuthenticatedPrincipal {
        let snapshot = GrantSnapshot::new(principal_id, project_id, grants);
        let snapshot = if approval {
            snapshot.with_principal_grant(PrincipalGrant::ResolveApproval)
        } else {
            snapshot
        };
        AuthenticatedPrincipal::from_grants(snapshot)
    }

    #[test]
    fn lazy_repo_service_uses_the_injected_native_semantic_evidence_store() {
        let database = database("shared-semantic-evidence");
        let directory = database.parent().unwrap().to_owned();
        let project_root = directory.join("project");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(project_root.join("lib.rs"), "fn source() {}\n").unwrap();
        let principal_id = PrincipalId::from_stable_bytes(b"semantic-evidence-owner");
        let project_id = ProjectId::from_stable_bytes(b"semantic-evidence-project");
        let evidence =
            crate::capabilities::native::dispatch::NativeSemanticEvidenceStore::default();
        let service = LazyNativeRepoService::with_semantic_evidence(
            NativeRepoOptions {
                database: database.clone(),
                project_root,
                scratch: directory.join("scratch"),
                artifacts: Arc::new(ArtifactStore::open(directory.join("artifacts")).unwrap()),
                principal_id,
                project_id,
                provider: Provider::Anthropic,
                process_registration: ProcessRegistryRegistration::new(
                    Arc::new(Registry),
                    crate::executor::process::own::ProcessRegistrationContext {
                        project_id,
                        principal_id,
                    },
                ),
                cancellation: SqliteCancellationCoordinator::new(&database),
                container_image: None,
                verification_registry: crate::verify::profiles::VerificationRegistry::empty(),
                formatter: None,
                formatter_required: false,
                diagnostic_adapters: BTreeMap::new(),
                feedback_limits: crate::verify::feedback::FeedbackLimits::default(),
                edit_validation_time: crate::workspace::edit::ir::EditLimits::default()
                    .max_validation_time,
                #[cfg(debug_assertions)]
                check_completions: Vec::new(),
            },
            ControlPlaneAuthority::for_test(),
            evidence.clone(),
        );

        assert!(service.shares_semantic_evidence_with(&evidence));
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn insert_operation(
        connection: &Connection,
        id: ToolCallId,
        principal_id: PrincipalId,
        project_id: ProjectId,
        tool: NativeTool,
        status: &str,
    ) {
        let run_id = RunId::from_stable_bytes(format!("run-{id}").as_bytes());
        let config = LayerStack::safe_defaults_for(Provider::Anthropic)
            .materialize(
                RunConfigContext {
                    principal_id,
                    project_id,
                    run_id,
                },
                &BTreeSet::new(),
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO repository_operations
                 (result_id,principal_id,project_id,operation,tool,idempotency_key,request_digest,input,grants,config,run_id,attempt_id,fence,lease_version,status,approval_state,migration_version,created_at,updated_at)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,'[]',?9,?10,?11,1,1,?12,?13,0,1,1)",
                params![
                    id.to_string(),
                    principal_id.to_string(),
                    project_id.to_string(),
                    format!("repo.{}", tool.short_name()),
                    tool.short_name(),
                    format!("key-{id}"),
                    vec![0_u8; 32],
                    b"{}".as_slice(),
                    config.canonical_bytes(),
                    run_id.to_string(),
                    AttemptId::from_stable_bytes(format!("attempt-{id}").as_bytes()).to_string(),
                    status,
                    if status == "waiting_approval" { "pending" } else { "not_required" },
                ],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE repository_schema_migrations SET version=0,last_key=''
                 WHERE name=?1",
                [OPERATION_METADATA_MIGRATION],
            )
            .unwrap();
    }

    fn insert_many_queued_operations(
        database: &Path,
        count: usize,
        migration_version: u16,
    ) -> (PrincipalId, ProjectId) {
        let principal_id = PrincipalId::from_stable_bytes(b"migration-owner");
        let project_id = ProjectId::from_stable_bytes(b"migration-project");
        let config_run = RunId::from_stable_bytes(b"migration-config-run");
        let config = LayerStack::safe_defaults_for(Provider::Anthropic)
            .materialize(
                RunConfigContext {
                    principal_id,
                    project_id,
                    run_id: config_run,
                },
                &BTreeSet::new(),
            )
            .unwrap();
        let mut connection = open_repository_connection(database).unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO repository_operations
                     (result_id,principal_id,project_id,operation,tool,idempotency_key,
                      request_digest,input,grants,config,run_id,attempt_id,fence,lease_version,
                      status,approval_state,migration_version,created_at,updated_at)
                     VALUES(?1,?2,?3,'repo.read','read',?4,zeroblob(32),'{}','[]',?5,
                            ?6,?7,1,1,'queued','not_required',?8,?9,?9)",
                )
                .unwrap();
            for index in 0..count {
                let id = ToolCallId::from_stable_bytes(format!("migration-{index}").as_bytes());
                statement
                    .execute(params![
                        id.to_string(),
                        principal_id.to_string(),
                        project_id.to_string(),
                        format!("migration-key-{index}"),
                        config.canonical_bytes(),
                        RunId::from_stable_bytes(format!("migration-run-{index}").as_bytes())
                            .to_string(),
                        AttemptId::from_stable_bytes(
                            format!("migration-attempt-{index}").as_bytes()
                        )
                        .to_string(),
                        migration_version,
                        index as i64,
                    ])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
        open_repository_connection(database)
            .unwrap()
            .execute(
                "UPDATE repository_schema_migrations SET version=?2,last_key=''
                 WHERE name=?1",
                params![
                    OPERATION_METADATA_MIGRATION,
                    if migration_version >= OPERATION_METADATA_MIGRATION_VERSION {
                        OPERATION_METADATA_MIGRATION_VERSION
                    } else {
                        0
                    }
                ],
            )
            .unwrap();
        (principal_id, project_id)
    }

    #[test]
    fn queued_legacy_discover_payload_remains_compatible_with_current_descriptor() {
        let database = database("legacy-discover-queued");
        let _store = crate::test_support::open_sqlite_store(&database).unwrap();
        migrate(&database).unwrap();
        let principal_id = PrincipalId::from_stable_bytes(b"legacy-discover-owner");
        let project_id = ProjectId::from_stable_bytes(b"legacy-discover-project");
        let id = ToolCallId::from_stable_bytes(b"legacy-discover-operation");
        let connection = open_repository_connection(&database).unwrap();
        insert_operation(
            &connection,
            id,
            principal_id,
            project_id,
            NativeTool::Discover,
            "queued",
        );
        let legacy = serde_json::to_vec(&json!({
            "expected_revision": format!("r:{}", "a".repeat(64)),
            "terms": ["Config"],
            "roots": [],
            "languages": ["rust"],
            "cursor": null
        }))
        .unwrap();
        connection
            .execute(
                "UPDATE repository_operations SET input=?2 WHERE result_id=?1",
                params![id.to_string(), legacy],
            )
            .unwrap();

        let queued = load_operation(&database, &id.to_string()).unwrap();
        let input: Value = serde_json::from_slice(&queued.input).unwrap();
        let descriptor = NativeCatalog::all()
            .iter()
            .find(|descriptor| descriptor.tool() == NativeTool::Discover)
            .unwrap();
        assert_eq!(descriptor.identity().version().as_str(), "1.0.0");
        assert!(
            jsonschema::validator_for(&descriptor.spec().input_schema)
                .unwrap()
                .is_valid(&input)
        );
        assert!(input.get("map").is_none());
    }

    fn create_legacy_repository(database: &Path, count: usize) {
        let _store = crate::test_support::open_sqlite_store(database).unwrap();
        let connection = open_repository_connection(database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE repository_operations (
                   result_id TEXT PRIMARY KEY,
                   principal_id TEXT NOT NULL,
                   project_id TEXT NOT NULL,
                   operation TEXT NOT NULL,
                   tool TEXT NOT NULL,
                   idempotency_key TEXT NOT NULL,
                   request_digest BLOB NOT NULL,
                   input BLOB NOT NULL,
                   grants TEXT NOT NULL,
                   config BLOB NOT NULL,
                   run_id TEXT NOT NULL UNIQUE,
                   attempt_id TEXT NOT NULL UNIQUE,
                   fence INTEGER NOT NULL,
                   lease_version INTEGER NOT NULL,
                   status TEXT NOT NULL,
                   approval_id TEXT,
                   approval_state TEXT NOT NULL,
                   cancellation_requested INTEGER NOT NULL DEFAULT 0,
                   replayed INTEGER NOT NULL DEFAULT 0,
                   result TEXT,
                   created_at INTEGER NOT NULL,
                   updated_at INTEGER NOT NULL,
                   UNIQUE(principal_id, project_id, operation, idempotency_key)
                 );
                 CREATE INDEX repository_operations_status_idx
                   ON repository_operations(status, created_at);
                 CREATE TABLE repository_operation_events (
                   result_id TEXT NOT NULL,
                   sequence INTEGER NOT NULL,
                   event_type TEXT NOT NULL,
                   payload TEXT NOT NULL,
                   created_at INTEGER NOT NULL,
                   PRIMARY KEY(result_id, sequence),
                   FOREIGN KEY(result_id) REFERENCES repository_operations(result_id) ON DELETE CASCADE
                 );",
            )
            .unwrap();
        let principal_id = PrincipalId::from_stable_bytes(b"legacy-schema-owner");
        let project_id = ProjectId::from_stable_bytes(b"legacy-schema-project");
        let config = LayerStack::safe_defaults_for(Provider::Anthropic)
            .materialize(
                RunConfigContext {
                    principal_id,
                    project_id,
                    run_id: RunId::from_stable_bytes(b"legacy-schema-config"),
                },
                &BTreeSet::new(),
            )
            .unwrap();
        let mut connection = open_repository_connection(database).unwrap();
        let transaction = connection.transaction().unwrap();
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO repository_operations
                     (result_id,principal_id,project_id,operation,tool,idempotency_key,
                      request_digest,input,grants,config,run_id,attempt_id,fence,lease_version,
                      status,approval_state,created_at,updated_at)
                     VALUES(?1,?2,?3,'repo.read','read',?4,zeroblob(32),'{}','[]',?5,
                            ?6,?7,1,1,'queued','not_required',?8,?8)",
                )
                .unwrap();
            for index in 0..count {
                statement
                    .execute(params![
                        ToolCallId::from_stable_bytes(format!("legacy-schema-{index}").as_bytes())
                            .to_string(),
                        principal_id.to_string(),
                        project_id.to_string(),
                        format!("legacy-schema-key-{index}"),
                        config.canonical_bytes(),
                        RunId::from_stable_bytes(format!("legacy-schema-run-{index}").as_bytes())
                            .to_string(),
                        AttemptId::from_stable_bytes(
                            format!("legacy-schema-attempt-{index}").as_bytes()
                        )
                        .to_string(),
                        index as i64,
                    ])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
    }

    #[test]
    fn legacy_upgrade_uses_primary_key_pages_without_building_an_index() {
        let database = database("legacy-schema-upgrade");
        create_legacy_repository(&database, 10_000);

        migrate(&database).unwrap();

        let connection = open_repository_connection(&database).unwrap();
        let secondary_indexes = connection
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type='index' AND tbl_name='repository_operations' AND sql IS NOT NULL
                 ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(secondary_indexes, ["repository_operations_status_idx"]);
        assert_eq!(
            connection
                .query_row(
                    "SELECT version FROM repository_schema_migrations WHERE name=?1",
                    [OPERATION_METADATA_MIGRATION],
                    |row| row.get::<_, u16>(0)
                )
                .unwrap(),
            0
        );
        let plan = connection
            .prepare(
                "EXPLAIN QUERY PLAN SELECT result_id FROM repository_operations
                 WHERE result_id > ?1 ORDER BY result_id LIMIT ?2",
            )
            .unwrap()
            .query_map(params!["", MIGRATION_PAGE_SIZE], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        assert!(plan.contains("SEARCH repository_operations"), "{plan}");
        assert!(!plan.contains("SCAN repository_operations"), "{plan}");
    }

    #[test]
    fn new_database_records_operation_metadata_migration_current() {
        let database = database("migration-new-database");

        migrate(&database).unwrap();

        assert_eq!(
            open_repository_connection(&database)
                .unwrap()
                .query_row(
                    "SELECT version FROM repository_schema_migrations WHERE name=?1",
                    [OPERATION_METADATA_MIGRATION],
                    |row| row.get::<_, u16>(0)
                )
                .unwrap(),
            OPERATION_METADATA_MIGRATION_VERSION
        );
    }

    #[test]
    fn current_migration_skips_ten_thousand_historical_rows() {
        let database = database("migration-current");
        migrate(&database).unwrap();
        insert_many_queued_operations(&database, 10_000, OPERATION_METADATA_MIGRATION_VERSION);
        let artifacts = ArtifactStore::open(database.parent().unwrap().join("artifacts")).unwrap();

        let page = reconcile_operation_metadata_page(
            &database,
            &artifacts,
            Some(Instant::now() + STARTUP_MIGRATION_BUDGET),
        )
        .unwrap();

        assert_eq!(
            page,
            MigrationPage {
                processed: 0,
                pending: false,
                queued: Vec::new(),
            }
        );
    }

    #[test]
    fn ten_thousand_legacy_rows_resume_from_durable_bounded_progress() {
        let database = database("migration-progress");
        let _store = crate::test_support::open_sqlite_store(&database).unwrap();
        migrate(&database).unwrap();
        insert_many_queued_operations(&database, 10_000, 0);
        let artifacts = ArtifactStore::open(database.parent().unwrap().join("artifacts")).unwrap();
        let started = Instant::now();

        let first = reconcile_operation_metadata_page(
            &database,
            &artifacts,
            Some(Instant::now() + STARTUP_MIGRATION_BUDGET),
        )
        .unwrap();
        assert!(first.processed <= MIGRATION_PAGE_SIZE);
        assert!(first.pending);
        assert!(started.elapsed() < Duration::from_secs(1));
        let connection = open_repository_connection(&database).unwrap();
        let first_progress: String = connection
            .query_row(
                "SELECT last_key FROM repository_schema_migrations WHERE name=?1",
                [OPERATION_METADATA_MIGRATION],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM repository_operations WHERE migration_version=?1",
                    [OPERATION_METADATA_MIGRATION_VERSION],
                    |row| row.get::<_, usize>(0)
                )
                .unwrap(),
            first.processed
        );
        drop(connection);

        migrate(&database).unwrap();
        let second = reconcile_operation_metadata_page(&database, &artifacts, None).unwrap();
        assert_eq!(second.processed, MIGRATION_PAGE_SIZE);
        let second_progress: String = open_repository_connection(&database)
            .unwrap()
            .query_row(
                "SELECT last_key FROM repository_schema_migrations WHERE name=?1",
                [OPERATION_METADATA_MIGRATION],
                |row| row.get(0),
            )
            .unwrap();
        assert!(second_progress > first_progress);
    }

    #[test]
    fn old_result_read_reconciles_result_and_terminal_event_before_return() {
        let (database, artifacts, owner, id) = completed_fixture("migration-lazy-read");
        let connection = open_repository_connection(&database).unwrap();
        let contradictory = json!({"schema_version":1,"id":id,"status":"failed"});
        connection
            .execute(
                "UPDATE repository_operations SET result=?2 WHERE result_id=?1",
                params![id.to_string(), contradictory.to_string()],
            )
            .unwrap();
        mark_operation_legacy(&connection, id);
        drop(connection);

        let result =
            load_reconciled_result(&database, &artifacts, &owner, &id.to_string(), "result")
                .unwrap();
        let events = load_result(&database, &owner, &id.to_string(), "events").unwrap();
        let terminal = events["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["type"] == "repository.operation_terminal")
            .unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(terminal["payload"]["result"], result);
        assert_eq!(
            open_repository_connection(&database)
                .unwrap()
                .query_row(
                    "SELECT migration_version FROM repository_operations WHERE result_id=?1",
                    [id.to_string()],
                    |row| row.get::<_, u16>(0)
                )
                .unwrap(),
            OPERATION_METADATA_MIGRATION_VERSION
        );
    }

    #[test]
    fn background_migration_completion_sets_global_version() {
        let database = database("migration-background");
        let _store = crate::test_support::open_sqlite_store(&database).unwrap();
        migrate(&database).unwrap();
        insert_many_queued_operations(&database, MIGRATION_PAGE_SIZE * 2 + 1, 0);
        let artifacts =
            Arc::new(ArtifactStore::open(database.parent().unwrap().join("artifacts")).unwrap());

        let scheduled = Arc::new(Mutex::new(BTreeSet::new()));
        let (queue, _receiver) = sync_channel(REPOSITORY_QUEUE_CAPACITY);
        let (queue_pump, _pump_receiver) = sync_channel(1);
        start_operation_metadata_migration(
            database.clone(),
            artifacts,
            scheduled,
            Arc::new(queue),
            queue_pump,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let version: u16 = open_repository_connection(&database)
                .unwrap()
                .query_row(
                    "SELECT version FROM repository_schema_migrations WHERE name=?1",
                    [OPERATION_METADATA_MIGRATION],
                    |row| row.get(0),
                )
                .unwrap();
            if version == OPERATION_METADATA_MIGRATION_VERSION {
                break;
            }
            assert!(Instant::now() < deadline, "background migration timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            open_repository_connection(&database)
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM repository_operations WHERE migration_version=0",
                    [],
                    |row| row.get::<_, usize>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn reduced_and_cross_scope_operation_authority_is_hidden() {
        let database = database("authority");
        migrate(&database).unwrap();
        let owner = PrincipalId::from_stable_bytes(b"repository-owner");
        let other = PrincipalId::from_stable_bytes(b"repository-other");
        let project = ProjectId::from_stable_bytes(b"repository-project");
        let id = ToolCallId::from_stable_bytes(b"repository-edit");
        insert_operation(
            &open_repository_connection(&database).unwrap(),
            id,
            owner,
            project,
            NativeTool::Edit,
            "waiting_approval",
        );

        let effect_only = principal(owner, project, [Grant::WorkspaceWrite], false);
        assert_eq!(
            authorize_operation_effect(&database, &effect_only, &id.to_string(), true),
            Err(RepoError::NotFound)
        );
        assert!(
            authorize_operation_effect(&database, &effect_only, &id.to_string(), false).is_ok()
        );
        let approval_without_effect = principal(owner, project, [Grant::WorkspaceRead], true);
        assert_eq!(
            authorize_operation_effect(&database, &approval_without_effect, &id.to_string(), true),
            Err(RepoError::NotFound)
        );
        let authorized = principal(owner, project, [Grant::WorkspaceWrite], true);
        assert!(authorize_operation_effect(&database, &authorized, &id.to_string(), true).is_ok());
        let cross_principal = principal(other, project, [Grant::WorkspaceWrite], true);
        assert_eq!(
            authorize_operation_effect(&database, &cross_principal, &id.to_string(), true),
            Err(RepoError::NotFound)
        );
        assert_eq!(
            require_workspace_read(&effect_only),
            Err(RepoError::NotFound)
        );
    }

    #[test]
    fn diff_finalization_preserves_scoped_reference_when_content_was_cross_principal_preseeded() {
        let database = database("diff-reference-scope");
        migrate(&database).unwrap();
        let preseed_owner = PrincipalId::from_stable_bytes(b"diff-preseed-owner");
        let edit_owner = PrincipalId::from_stable_bytes(b"diff-edit-owner");
        let project = ProjectId::from_stable_bytes(b"diff-reference-project");
        let id = ToolCallId::from_stable_bytes(b"diff-reference-edit");
        insert_operation(
            &open_repository_connection(&database).unwrap(),
            id,
            edit_owner,
            project,
            NativeTool::Edit,
            "running",
        );
        let operation = load_operation(&database, &id.to_string()).unwrap();
        let artifact_root = database.parent().unwrap().join("artifacts");
        let artifacts = ArtifactStore::open(&artifact_root).unwrap();
        let transaction = "edit:0123456789abcdef0123456789abcdef";
        let revision = format!("r:{}", "1".repeat(64));
        let bytes = format!(
            "kit-actual-diff-v1\ntransaction={transaction}\nrevision={revision}\nprincipal={edit_owner}\nproject={project}\nplan=blake3:{}\nstage=blake3:{}\n\n",
            "2".repeat(64),
            "3".repeat(64),
        );
        let preseed = artifacts
            .put(
                bytes.as_bytes(),
                ArtifactMetadata::new(
                    "text/x-diff; charset=utf-8",
                    ArtifactClass::Diff,
                    preseed_owner.to_string(),
                    project.to_string(),
                    ArtifactRetention::Forever,
                    1,
                )
                .unwrap(),
            )
            .unwrap();
        let edit = artifacts
            .put(
                bytes.as_bytes(),
                ArtifactMetadata::new(
                    "text/x-diff; charset=utf-8",
                    ArtifactClass::Diff,
                    edit_owner.to_string(),
                    project.to_string(),
                    ArtifactRetention::Forever,
                    2,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(preseed.digest(), edit.digest());
        assert_ne!(preseed.reference(), edit.reference());

        for (owner, reference) in [
            (preseed_owner, preseed.reference()),
            (edit_owner, edit.reference()),
        ] {
            let authenticated = principal(owner, project, [Grant::WorkspaceRead], false);
            assert_eq!(
                artifacts
                    .resolve_reference(&authenticated, reference)
                    .unwrap()
                    .reference(),
                reference
            );
        }

        let published = referenced_diff_artifact(
            &artifacts,
            &operation,
            Some(&json!({
                "reference": edit.reference().to_string(),
                "digest": edit.digest().to_string(),
                "media_type": "text/x-diff; charset=utf-8",
                "class": "diff",
                "provenance": {
                    "principal_id": edit_owner,
                    "project_id": project,
                    "transaction_id": transaction,
                    "revision_id": revision,
                },
            })),
        )
        .unwrap();
        assert_eq!(published["reference"], edit.reference().to_string());
        assert_ne!(published["reference"], preseed.reference().to_string());
    }

    #[test]
    fn mutation_receipt_replays_across_reopen_and_rejects_divergence() {
        let database = database("receipt");
        migrate(&database).unwrap();
        let principal_id = PrincipalId::from_stable_bytes(b"receipt-owner");
        let project = ProjectId::from_stable_bytes(b"receipt-project");
        let principal = principal(principal_id, project, [Grant::WorkspaceWrite], true);
        let key = IdempotencyKey::parse("approval-receipt").unwrap();
        let digest = canonical_request_digest(
            "repository.approval",
            &principal,
            project,
            "result",
            br#"{"decision":"approved"}"#,
        );
        let response = json!({"status":"queued","receipt":"stable"});
        {
            let mut connection = open_repository_connection(&database).unwrap();
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            insert_mutation_receipt(
                &transaction,
                &principal,
                "repository.approval",
                "result",
                &key,
                digest,
                &response,
                1,
            )
            .unwrap();
            transaction.commit().unwrap();
        }
        let mut connection = open_repository_connection(&database).unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert_eq!(
            mutation_receipt(
                &transaction,
                &principal,
                "repository.approval",
                "result",
                &key,
                digest,
            )
            .unwrap(),
            Some(response)
        );
        let divergent = canonical_request_digest(
            "repository.approval",
            &principal,
            project,
            "result",
            br#"{"decision":"denied"}"#,
        );
        assert_eq!(
            mutation_receipt(
                &transaction,
                &principal,
                "repository.approval",
                "result",
                &key,
                divergent,
            ),
            Err(RepoError::Conflict)
        );
    }

    #[test]
    fn boot_reconciliation_requeues_when_kernel_never_dispatched() {
        let database = database("reconcile");
        let _store = crate::test_support::open_sqlite_store(&database).unwrap();
        migrate(&database).unwrap();
        let principal_id = PrincipalId::from_stable_bytes(b"reconcile-owner");
        let project = ProjectId::from_stable_bytes(b"reconcile-project");
        let running = ToolCallId::from_stable_bytes(b"reconcile-running");
        let queued = ToolCallId::from_stable_bytes(b"reconcile-queued");
        let connection = open_repository_connection(&database).unwrap();
        insert_operation(
            &connection,
            running,
            principal_id,
            project,
            NativeTool::Read,
            "running",
        );
        insert_operation(
            &connection,
            queued,
            principal_id,
            project,
            NativeTool::Read,
            "queued",
        );
        insert_kernel_event(
            &connection,
            running,
            "capability.invocation_intent",
            json!({"schema_version":1,"invocation_id":running}),
        );
        drop(connection);
        let artifacts = ArtifactStore::open(database.parent().unwrap().join("artifacts")).unwrap();
        reconcile_operation_metadata(&database, &artifacts).unwrap();
        let principal = principal(principal_id, project, [Grant::WorkspaceRead], false);
        let recovered = load_result(&database, &principal, &running.to_string(), "result").unwrap();
        assert_eq!(recovered["status"], "queued");
        assert_eq!(
            load_result(&database, &principal, &queued.to_string(), "result").unwrap()["status"],
            "queued"
        );
    }

    #[test]
    fn background_migration_dispatches_a_lone_row_after_the_initial_scan() {
        let database = database("migration-dispatch");
        let _store = crate::test_support::open_sqlite_store(&database).unwrap();
        migrate(&database).unwrap();
        let principal_id = PrincipalId::from_stable_bytes(b"migration-dispatch-owner");
        let project = ProjectId::from_stable_bytes(b"migration-dispatch-project");
        let id = ToolCallId::from_stable_bytes(b"migration-dispatch-operation");
        let connection = open_repository_connection(&database).unwrap();
        insert_operation(
            &connection,
            id,
            principal_id,
            project,
            NativeTool::Read,
            "running",
        );
        insert_kernel_event(
            &connection,
            id,
            "capability.invocation_intent",
            json!({"schema_version":1,"invocation_id":id}),
        );
        drop(connection);

        let scheduled = Arc::new(Mutex::new(BTreeSet::new()));
        let (queue, receiver) = sync_channel(REPOSITORY_QUEUE_CAPACITY);
        let queue = Arc::new(queue);
        reenqueue_operations(&database, &scheduled, &queue).unwrap();
        assert!(receiver.try_recv().is_err());

        let executor_database = database.clone();
        let executor = std::thread::spawn(move || {
            let dispatched = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
            open_repository_connection(&executor_database)
                .unwrap()
                .execute(
                    "UPDATE repository_operations SET status='completed' WHERE result_id=?1",
                    [&dispatched],
                )
                .unwrap();
            dispatched
        });
        let artifacts =
            Arc::new(ArtifactStore::open(database.parent().unwrap().join("artifacts")).unwrap());
        let (queue_pump, _pump_receiver) = sync_channel(1);
        start_operation_metadata_migration(
            database.clone(),
            artifacts,
            Arc::clone(&scheduled),
            Arc::clone(&queue),
            queue_pump,
        )
        .unwrap();

        assert_eq!(executor.join().unwrap(), id.to_string());
        assert_eq!(
            open_repository_connection(&database)
                .unwrap()
                .query_row(
                    "SELECT status FROM repository_operations WHERE result_id=?1",
                    [id.to_string()],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "completed"
        );
    }

    fn insert_kernel_event(
        connection: &Connection,
        id: ToolCallId,
        event_type: &str,
        payload: Value,
    ) {
        let run_id = RunId::from_stable_bytes(format!("run-{id}").as_bytes());
        connection
            .execute(
                "INSERT INTO stream_heads(stream,version) VALUES(?1,1)",
                [id.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO events(event_id,stream,sequence,commit_position,event_type,schema_version,occurred_at,causation_id,correlation_id,attempt_id,trace_id,payload,artifacts)
                 VALUES(?1,?2,1,1,?3,1,'2026-07-26T00:00:00Z',?4,?5,NULL,'repository-recovery-test',?6,'[]')",
                params![
                    EventId::from_stable_bytes(format!("event-{id}-{event_type}").as_bytes()).to_string(),
                    id.to_string(),
                    event_type,
                    CommandId::from_stable_bytes(format!("command-{id}").as_bytes()).to_string(),
                    run_id.to_string(),
                    serde_json::to_vec(&payload).unwrap(),
                ],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE commit_watermark SET position=1 WHERE singleton=1",
                [],
            )
            .unwrap();
    }

    fn completed_fixture(
        name: &str,
    ) -> (PathBuf, ArtifactStore, AuthenticatedPrincipal, ToolCallId) {
        let database = database(name);
        let _store = crate::test_support::open_sqlite_store(&database).unwrap();
        migrate(&database).unwrap();
        let principal_id = PrincipalId::from_stable_bytes(format!("{name}-owner").as_bytes());
        let project_id = ProjectId::from_stable_bytes(format!("{name}-project").as_bytes());
        let id = ToolCallId::from_stable_bytes(format!("{name}-result").as_bytes());
        let outcome = CanonicalInvocationResult {
            status: InvocationStatus::Succeeded,
            output: Some(crate::capabilities::kernel::invoke::CanonicalOutput {
                media_type: "application/json".to_owned(),
                body: br#"{"committed":true}"#.to_vec(),
            }),
            code: None,
            charged: true,
        };
        let connection = open_repository_connection(&database).unwrap();
        insert_operation(
            &connection,
            id,
            principal_id,
            project_id,
            NativeTool::Read,
            "running",
        );
        insert_kernel_event(
            &connection,
            id,
            "capability.invocation_outcome",
            json!({"schema_version":1,"invocation_id":id,"result":outcome}),
        );
        drop(connection);
        let artifacts = ArtifactStore::open(database.parent().unwrap().join("artifacts")).unwrap();
        NativeRepoWorker::complete(
            &database,
            &artifacts,
            load_operation(&database, &id.to_string()).unwrap(),
            outcome,
            false,
        )
        .unwrap();
        (
            database,
            artifacts,
            principal(principal_id, project_id, [Grant::WorkspaceRead], false),
            id,
        )
    }

    #[test]
    fn boot_reconciliation_preserves_committed_kernel_outcome() {
        for (seed, canonical, expected_status, expected_code) in [
            (
                "success",
                CanonicalInvocationResult {
                    status: InvocationStatus::Succeeded,
                    output: Some(crate::capabilities::kernel::invoke::CanonicalOutput {
                        media_type: "application/json".to_owned(),
                        body: br#"{"answer":42}"#.to_vec(),
                    }),
                    code: None,
                    charged: true,
                },
                "completed",
                None,
            ),
            (
                "failure",
                CanonicalInvocationResult {
                    status: InvocationStatus::Failed,
                    output: None,
                    code: Some("trusted_failure".to_owned()),
                    charged: true,
                },
                "failed",
                Some("trusted_failure"),
            ),
        ] {
            let database = database(&format!("reconcile-{seed}"));
            let _store = crate::test_support::open_sqlite_store(&database).unwrap();
            migrate(&database).unwrap();
            let principal_id = PrincipalId::from_stable_bytes(b"reconcile-outcome-owner");
            let project = ProjectId::from_stable_bytes(b"reconcile-outcome-project");
            let id = ToolCallId::from_stable_bytes(seed.as_bytes());
            let connection = open_repository_connection(&database).unwrap();
            insert_operation(
                &connection,
                id,
                principal_id,
                project,
                NativeTool::Read,
                "running",
            );
            insert_kernel_event(
                &connection,
                id,
                "capability.invocation_outcome",
                json!({"schema_version":1,"invocation_id":id,"result":canonical}),
            );
            drop(connection);
            let artifacts =
                ArtifactStore::open(database.parent().unwrap().join("artifacts")).unwrap();
            reconcile_operation_metadata(&database, &artifacts).unwrap();
            let owner = principal(principal_id, project, [Grant::WorkspaceRead], false);
            let result = load_result(&database, &owner, &id.to_string(), "result").unwrap();
            assert_eq!(result["status"], expected_status);
            assert_eq!(
                result.pointer("/error/code").and_then(Value::as_str),
                expected_code
            );
            if seed == "success" {
                assert_eq!(result["output"]["answer"], 42);
            }
            assert_eq!(result["cost"]["charged"], true);
            assert!(result["artifacts"]["repository_result"].is_object());
        }
    }

    #[test]
    fn every_post_outcome_finalization_failure_recovers_exactly_once() {
        for point in FinalizationPoint::ALL {
            let database = database(&format!("finalize-{point:?}"));
            let _store = crate::test_support::open_sqlite_store(&database).unwrap();
            migrate(&database).unwrap();
            let principal_id = PrincipalId::from_stable_bytes(b"finalize-owner");
            let project = ProjectId::from_stable_bytes(b"finalize-project");
            let id = ToolCallId::from_stable_bytes(format!("finalize-{point:?}").as_bytes());
            let canonical = CanonicalInvocationResult {
                status: InvocationStatus::Succeeded,
                output: Some(crate::capabilities::kernel::invoke::CanonicalOutput {
                    media_type: "application/json".to_owned(),
                    body: br#"{"answer":42}"#.to_vec(),
                }),
                code: None,
                charged: true,
            };
            let connection = open_repository_connection(&database).unwrap();
            insert_operation(
                &connection,
                id,
                principal_id,
                project,
                NativeTool::Read,
                "running",
            );
            insert_kernel_event(
                &connection,
                id,
                "capability.invocation_outcome",
                json!({"schema_version":1,"invocation_id":id,"result":canonical}),
            );
            drop(connection);
            let artifacts =
                ArtifactStore::open(database.parent().unwrap().join("artifacts")).unwrap();
            let operation = load_operation(&database, &id.to_string()).unwrap();
            assert_eq!(
                NativeRepoWorker::complete_with_hook(
                    &database,
                    &artifacts,
                    operation,
                    canonical,
                    false,
                    |candidate| candidate == point,
                ),
                Err(RepoError::Internal)
            );
            if matches!(
                point,
                FinalizationPoint::ResultRow | FinalizationPoint::TerminalEvent
            ) {
                let connection = open_repository_connection(&database).unwrap();
                let row = connection
                    .query_row(
                        "SELECT status,result FROM repository_operations WHERE result_id=?1",
                        [id.to_string()],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
                    )
                    .unwrap();
                assert_eq!(row, ("running".to_owned(), None));
                assert_eq!(terminal_event_count(&connection, id), 0);
            }

            reconcile_operation_metadata(&database, &artifacts)
                .unwrap_or_else(|error| panic!("{point:?} recovery failed: {error}"));
            let owner = principal(principal_id, project, [Grant::WorkspaceRead], false);
            let result = load_result(&database, &owner, &id.to_string(), "result").unwrap();
            let events = load_result(&database, &owner, &id.to_string(), "events").unwrap();
            assert_eq!(result["status"], "completed");
            assert_eq!(result["output"]["answer"], 42);
            assert_eq!(result["cost"]["charged"], true);
            let references = ["cost", "edit_events", "repository_result"]
                .map(|kind| result["artifacts"][kind]["reference"].as_str().unwrap());
            assert_eq!(references.into_iter().collect::<BTreeSet<_>>().len(), 3);
            let connection = open_repository_connection(&database).unwrap();
            assert_eq!(terminal_event_count(&connection, id), 1);
            assert_eq!(
                connection
                    .query_row(
                        "SELECT count(*) FROM repository_operation_events
                         WHERE result_id=?1 AND event_type='capability.invocation_outcome'",
                        [id.to_string()],
                        |row| row.get::<_, u64>(0),
                    )
                    .unwrap(),
                1
            );
            drop(connection);
            reconcile_operation_metadata(&database, &artifacts).unwrap();
            assert_eq!(
                load_result(&database, &owner, &id.to_string(), "result").unwrap(),
                result
            );
            assert_eq!(
                load_result(&database, &owner, &id.to_string(), "events").unwrap(),
                events
            );
        }
    }

    #[test]
    fn reconciliation_rebuilds_legacy_results_from_success_and_failure_outcomes() {
        for (seed, outcome, stored_code, expected_status) in [
            (
                "legacy-success",
                CanonicalInvocationResult {
                    status: InvocationStatus::Succeeded,
                    output: Some(crate::capabilities::kernel::invoke::CanonicalOutput {
                        media_type: "application/json".to_owned(),
                        body: br#"{"committed":true}"#.to_vec(),
                    }),
                    code: None,
                    charged: true,
                },
                "repository_worker_failed",
                "completed",
            ),
            (
                "kernel-failure",
                CanonicalInvocationResult {
                    status: InvocationStatus::Failed,
                    output: None,
                    code: Some("trusted_failure".to_owned()),
                    charged: true,
                },
                "trusted_failure",
                "failed",
            ),
        ] {
            let database = database(seed);
            let _store = crate::test_support::open_sqlite_store(&database).unwrap();
            migrate(&database).unwrap();
            let principal_id = PrincipalId::from_stable_bytes(b"legacy-owner");
            let project = ProjectId::from_stable_bytes(b"legacy-project");
            let id = ToolCallId::from_stable_bytes(seed.as_bytes());
            let connection = open_repository_connection(&database).unwrap();
            insert_operation(
                &connection,
                id,
                principal_id,
                project,
                NativeTool::Read,
                "running",
            );
            insert_kernel_event(
                &connection,
                id,
                "capability.invocation_outcome",
                json!({"schema_version":1,"invocation_id":id,"result":outcome}),
            );
            let legacy_result = json!({
                "schema_version":1,"id":id,"status":"failed",
                "error":{"code":stored_code,"effect_state":"attempted","retryable":false}
            });
            connection
                .execute(
                    "UPDATE repository_operations SET status='failed',result=?2 WHERE result_id=?1",
                    params![id.to_string(), legacy_result.to_string()],
                )
                .unwrap();
            append_operation_event(
                &database,
                &id.to_string(),
                "repository.operation_terminal",
                json!({"operation_id":id,"status":"failed","error":stored_code}),
            )
            .unwrap();
            drop(connection);
            let artifacts =
                ArtifactStore::open(database.parent().unwrap().join("artifacts")).unwrap();
            reconcile_operation_metadata(&database, &artifacts).unwrap();
            let owner = principal(principal_id, project, [Grant::WorkspaceRead], false);
            let result = load_result(&database, &owner, &id.to_string(), "result").unwrap();
            assert_eq!(result["status"], expected_status);
            if expected_status == "completed" {
                assert_eq!(result["output"]["committed"], true);
            } else {
                assert_eq!(result["error"]["code"], "trusted_failure");
            }
            assert_eq!(result["cost"]["charged"], true);
            assert!(result["artifacts"]["repository_result"].is_object());
            assert_eq!(
                terminal_event_count(&open_repository_connection(&database).unwrap(), id),
                1
            );
        }
    }

    #[test]
    fn reconciliation_replaces_completed_failed_terminal_events_in_either_order() {
        for failed_first in [false, true] {
            let name = format!("terminal-order-{failed_first}");
            let (database, artifacts, owner, id) = completed_fixture(&name);
            let connection = open_repository_connection(&database).unwrap();
            let (sequence, completed, completed_digest) = connection
                .query_row(
                    "SELECT sequence,payload,payload_digest FROM repository_operation_events
                     WHERE result_id=?1 AND event_type='repository.operation_terminal'",
                    [id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, u64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .unwrap();
            let failed = json!({"operation_id":id,"status":"failed"}).to_string();
            if failed_first {
                connection
                    .execute(
                        "UPDATE repository_operation_events
                         SET payload=?3,payload_digest=?4,migration_version=0
                         WHERE result_id=?1 AND sequence=?2",
                        params![id.to_string(), sequence, failed, digest(&failed)],
                    )
                    .unwrap();
                connection
                    .execute(
                        "INSERT INTO repository_operation_events
                         (result_id,sequence,event_type,payload,payload_digest,migration_version,created_at)
                         VALUES(?1,?2,'repository.operation_terminal',?3,?4,?5,2)",
                        params![
                            id.to_string(),
                            sequence + 1,
                            completed,
                            completed_digest,
                            TERMINAL_EVENT_MIGRATION_VERSION
                        ],
                    )
                    .unwrap();
            } else {
                connection
                    .execute(
                        "INSERT INTO repository_operation_events
                         (result_id,sequence,event_type,payload,payload_digest,migration_version,created_at)
                         VALUES(?1,?2,'repository.operation_terminal',?3,?4,0,2)",
                        params![id.to_string(), sequence + 1, failed, digest(&failed)],
                    )
                    .unwrap();
            }
            mark_operation_legacy(&connection, id);
            drop(connection);

            reconcile_operation_metadata(&database, &artifacts).unwrap();
            let result = load_result(&database, &owner, &id.to_string(), "result").unwrap();
            let connection = open_repository_connection(&database).unwrap();
            let repaired = connection
                .query_row(
                    "SELECT sequence,payload,payload_digest,migration_version
                     FROM repository_operation_events
                     WHERE result_id=?1 AND event_type='repository.operation_terminal'",
                    [id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, u64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, u16>(3)?,
                        ))
                    },
                )
                .unwrap();
            let payload: Value = serde_json::from_str(&repaired.1).unwrap();
            assert_eq!(terminal_event_count(&connection, id), 1);
            assert_eq!(repaired.0, sequence);
            assert_eq!(repaired.2, digest(&repaired.1));
            assert_eq!(repaired.3, TERMINAL_EVENT_MIGRATION_VERSION);
            assert_eq!(payload["outcome"]["status"], "succeeded");
            assert_eq!(payload["result"], result);
        }
    }

    #[test]
    fn reconciliation_repairs_terminal_cost_and_artifact_references() {
        let (database, artifacts, owner, id) = completed_fixture("terminal-projection");
        let expected_result = load_result(&database, &owner, &id.to_string(), "result").unwrap();
        let connection = open_repository_connection(&database).unwrap();
        let (sequence, stored) = connection
            .query_row(
                "SELECT sequence,payload FROM repository_operation_events
                 WHERE result_id=?1 AND event_type='repository.operation_terminal'",
                [id.to_string()],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
            )
            .unwrap();
        let mut contradictory: Value = serde_json::from_str(&stored).unwrap();
        let mut contradictory_result = expected_result.clone();
        contradictory_result["cost"]["charged"] = json!(false);
        contradictory_result["artifacts"]["cost"]["reference"] =
            json!("artifact-ref:0000000000000000000000000000000000000000000000000000000000000000");
        contradictory["result"] = contradictory_result.clone();
        contradictory["cost"] = contradictory_result["cost"].clone();
        contradictory["artifacts"] = contradictory_result["artifacts"].clone();
        let contradictory = contradictory.to_string();
        connection
            .execute(
                "UPDATE repository_operations SET result=?2 WHERE result_id=?1",
                params![id.to_string(), contradictory_result.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE repository_operation_events SET payload=?3,payload_digest=?4
                 WHERE result_id=?1 AND sequence=?2",
                params![
                    id.to_string(),
                    sequence,
                    contradictory,
                    digest(&contradictory)
                ],
            )
            .unwrap();
        mark_operation_legacy(&connection, id);
        drop(connection);

        reconcile_operation_metadata(&database, &artifacts).unwrap();
        let result = load_result(&database, &owner, &id.to_string(), "result").unwrap();
        assert_eq!(result, expected_result);
        let connection = open_repository_connection(&database).unwrap();
        let (payload, payload_digest): (String, String) = connection
            .query_row(
                "SELECT payload,payload_digest FROM repository_operation_events
                 WHERE result_id=?1 AND event_type='repository.operation_terminal'",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let payload: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload_digest, digest(&payload.to_string()));
        assert_eq!(payload["result"], result);
        assert_eq!(payload["cost"], result["cost"]);
        assert_eq!(payload["artifacts"], result["artifacts"]);
    }

    #[test]
    fn matching_terminal_reconciliation_is_restart_idempotent() {
        let (database, artifacts, _, id) = completed_fixture("terminal-matching");
        let snapshot = |connection: &Connection| {
            connection
                .query_row(
                    "SELECT o.status,o.result,o.updated_at,e.sequence,e.payload,e.payload_digest,
                            e.migration_version,e.created_at
                     FROM repository_operations o
                     JOIN repository_operation_events e ON e.result_id=o.result_id
                     WHERE o.result_id=?1 AND e.event_type='repository.operation_terminal'",
                    [id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, u64>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, u16>(6)?,
                            row.get::<_, i64>(7)?,
                        ))
                    },
                )
                .unwrap()
        };
        let before = snapshot(&open_repository_connection(&database).unwrap());
        reconcile_operation_metadata(&database, &artifacts).unwrap();
        let after_first = snapshot(&open_repository_connection(&database).unwrap());
        reconcile_operation_metadata(&database, &artifacts).unwrap();
        let after_restart = snapshot(&open_repository_connection(&database).unwrap());
        assert_eq!(before, after_first);
        assert_eq!(after_first, after_restart);
    }

    #[test]
    fn committed_outcome_finalization_waits_for_sqlite_contention() {
        let database = database("finalize-contention");
        let _store = crate::test_support::open_sqlite_store(&database).unwrap();
        migrate(&database).unwrap();
        let principal_id = PrincipalId::from_stable_bytes(b"contention-owner");
        let project = ProjectId::from_stable_bytes(b"contention-project");
        let id = ToolCallId::from_stable_bytes(b"contention-result");
        let connection = open_repository_connection(&database).unwrap();
        insert_operation(
            &connection,
            id,
            principal_id,
            project,
            NativeTool::Read,
            "running",
        );
        let canonical = CanonicalInvocationResult {
            status: InvocationStatus::Succeeded,
            output: Some(crate::capabilities::kernel::invoke::CanonicalOutput {
                media_type: "application/json".to_owned(),
                body: br#"{"contention":"recovered"}"#.to_vec(),
            }),
            code: None,
            charged: true,
        };
        insert_kernel_event(
            &connection,
            id,
            "capability.invocation_outcome",
            json!({"schema_version":1,"invocation_id":id,"result":canonical}),
        );
        drop(connection);
        let mut blocker = open_repository_connection(&database).unwrap();
        let transaction = blocker
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let artifacts =
            Arc::new(ArtifactStore::open(database.parent().unwrap().join("artifacts")).unwrap());
        let child_database = database.clone();
        let child_artifacts = Arc::clone(&artifacts);
        let handle = std::thread::spawn(move || {
            let operation = load_operation(&child_database, &id.to_string()).unwrap();
            NativeRepoWorker::complete(
                &child_database,
                &child_artifacts,
                operation,
                canonical,
                false,
            )
        });
        std::thread::sleep(Duration::from_millis(50));
        transaction.commit().unwrap();
        handle.join().unwrap().unwrap();
        let owner = principal(principal_id, project, [Grant::WorkspaceRead], false);
        let result = load_result(&database, &owner, &id.to_string(), "result").unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(result["output"]["contention"], "recovered");
        assert_eq!(
            terminal_event_count(&open_repository_connection(&database).unwrap(), id),
            1
        );
    }

    fn terminal_event_count(connection: &Connection, id: ToolCallId) -> u64 {
        connection
            .query_row(
                "SELECT count(*) FROM repository_operation_events
                 WHERE result_id=?1 AND event_type='repository.operation_terminal'",
                [id.to_string()],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn mark_operation_legacy(connection: &Connection, id: ToolCallId) {
        connection
            .execute(
                "UPDATE repository_operations SET migration_version=0 WHERE result_id=?1",
                [id.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE repository_schema_migrations SET version=0,last_key=''
                 WHERE name=?1",
                [OPERATION_METADATA_MIGRATION],
            )
            .unwrap();
    }

    #[test]
    fn boot_reconciliation_marks_dispatched_without_outcome_unknown() {
        let database = database("reconcile-dispatched");
        let _store = crate::test_support::open_sqlite_store(&database).unwrap();
        migrate(&database).unwrap();
        let principal_id = PrincipalId::from_stable_bytes(b"reconcile-dispatched-owner");
        let project = ProjectId::from_stable_bytes(b"reconcile-dispatched-project");
        let id = ToolCallId::from_stable_bytes(b"reconcile-dispatched");
        let connection = open_repository_connection(&database).unwrap();
        insert_operation(
            &connection,
            id,
            principal_id,
            project,
            NativeTool::Read,
            "running",
        );
        insert_kernel_event(
            &connection,
            id,
            "capability.invocation_dispatched",
            json!({"schema_version":1,"invocation_id":id}),
        );
        drop(connection);
        let artifacts = ArtifactStore::open(database.parent().unwrap().join("artifacts")).unwrap();
        reconcile_operation_metadata(&database, &artifacts).unwrap();
        let owner = principal(principal_id, project, [Grant::WorkspaceRead], false);
        let result = load_result(&database, &owner, &id.to_string(), "result").unwrap();
        assert_eq!(result["status"], "outcome_unknown");
        assert_eq!(result["error"]["code"], "restart_requires_reconciliation");
        assert_eq!(result["cost"]["charged"], true);
    }

    #[test]
    fn repository_store_wal_handles_repeated_adjacent_writers() {
        let database = database("contention");
        migrate(&database).unwrap();
        open_repository_connection(&database)
            .unwrap()
            .execute("CREATE TABLE contention(value INTEGER NOT NULL)", [])
            .unwrap();
        open_repository_connection(&database)
            .unwrap()
            .execute("INSERT INTO contention VALUES(0)", [])
            .unwrap();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let database = &database;
                scope.spawn(move || {
                    for _ in 0..25 {
                        let mut connection = open_repository_connection(database).unwrap();
                        let transaction = connection
                            .transaction_with_behavior(TransactionBehavior::Immediate)
                            .unwrap();
                        transaction
                            .execute("UPDATE contention SET value=value+1", [])
                            .unwrap();
                        transaction.commit().unwrap();
                    }
                });
            }
        });
        let connection = open_repository_connection(&database).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT value FROM contention", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            100
        );
        assert_eq!(
            connection
                .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .unwrap()
                .to_ascii_lowercase(),
            "wal"
        );
    }

    #[test]
    fn sustained_repository_store_lock_is_typed_unavailable() {
        let database = database("busy");
        migrate(&database).unwrap();
        let mut blocker = open_repository_connection(&database).unwrap();
        let transaction = blocker
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let contender = open_repository_connection(&database).unwrap();
        contender.busy_timeout(Duration::from_millis(20)).unwrap();
        let error = contender
            .execute(
                "INSERT INTO repository_mutation_receipts
                 (principal_id,project_id,operation,target,idempotency_key,request_digest,response,created_at)
                 VALUES('p','q','op','target','key',zeroblob(32),'{}',1)",
                [],
            )
            .unwrap_err();
        assert_eq!(
            map_sql_error(error),
            RepoError::Unavailable("repository_store_busy".to_owned())
        );
        drop(transaction);
    }

    #[test]
    fn action_admission_rejects_a_saturated_durable_queue() {
        let database = database("saturation");
        migrate(&database).unwrap();
        let principal_id = PrincipalId::from_stable_bytes(b"saturation-owner");
        let project = ProjectId::from_stable_bytes(b"saturation-project");
        let waiting = ToolCallId::from_stable_bytes(b"saturation-waiting");
        let connection = open_repository_connection(&database).unwrap();
        insert_operation(
            &connection,
            waiting,
            principal_id,
            project,
            NativeTool::Edit,
            "waiting_approval",
        );
        for index in 0..(REPOSITORY_QUEUE_CAPACITY + REPOSITORY_WORKERS) {
            insert_operation(
                &connection,
                ToolCallId::from_stable_bytes(format!("saturation-{index}").as_bytes()),
                principal_id,
                project,
                NativeTool::Read,
                "queued",
            );
        }
        let mut connection = open_repository_connection(&database).unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        assert_eq!(
            ensure_action_queue_admission(&transaction, &waiting.to_string()),
            Err(RepoError::Unavailable(
                "repository_queue_saturated".to_owned()
            ))
        );
    }

    #[test]
    fn legacy_queue_larger_than_capacity_drains_without_scheduler_lock() {
        let database = database("queue-pump-saturation");
        let _store = crate::test_support::open_sqlite_store(&database).unwrap();
        migrate(&database).unwrap();
        insert_many_queued_operations(&database, 100, OPERATION_METADATA_MIGRATION_VERSION);
        let scheduled = Arc::new(Mutex::new(BTreeSet::new()));
        let (sender, receiver) = sync_channel(2);
        let sender = Arc::new(sender);
        let (wake, wake_receiver) = sync_channel(1);
        start_queue_pump(
            database.clone(),
            Arc::downgrade(&scheduled),
            Arc::downgrade(&sender),
            wake_receiver,
        )
        .unwrap();

        let mut drained = BTreeSet::new();
        while drained.len() < 100 {
            let id = receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|error| {
                    panic!(
                        "queue pump stopped after {} operations ({error}); scheduled={:?}",
                        drained.len(),
                        scheduled.lock().unwrap()
                    )
                });
            if drained.insert(id.clone()) {
                open_repository_connection(&database)
                    .unwrap()
                    .execute(
                        "UPDATE repository_operations SET status='completed' WHERE result_id=?1",
                        [&id],
                    )
                    .unwrap();
            }
            scheduled.lock().unwrap().remove(&id);
            let _ = wake.try_send(());
        }
        assert_eq!(drained.len(), 100);
        assert!(scheduled.lock().unwrap().is_empty());
    }

    #[test]
    fn dependency_generation_invalidates_cached_availability() {
        let database = database("availability");
        let root = database.parent().unwrap().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let mut probe = AvailabilityProbe {
            root: root.clone(),
            image: None,
            registry: crate::verify::profiles::VerificationRegistry::empty(),
            feedback_configured: true,
            formatter: None,
            formatter_required: false,
            syntax_available: true,
            mechanical_executor: true,
            cached: None,
        };
        let available = probe.snapshot().unwrap();
        assert!(!available.unavailable.contains_key(&NativeTool::Discover));
        std::fs::remove_dir(&root).unwrap();
        let unavailable = probe.snapshot().unwrap();
        assert_ne!(available.generation, unavailable.generation);
        assert_eq!(
            unavailable.unavailable[&NativeTool::Discover],
            ["trusted_workspace_unavailable"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn image_probe_reports_dependency_loss_for_every_configured_image() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = database("image-probe").parent().unwrap().to_owned();
        let runtime = root.join("runtime");
        std::fs::write(
            &runtime,
            "#!/bin/sh\n[ \"$1\" = image ] && [ \"$2\" = inspect ] && [ -f \"$3\" ]\n",
        )
        .unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        let images = (0..3)
            .map(|index| root.join(format!("image-{index}")))
            .collect::<Vec<_>>();
        for image in &images {
            std::fs::write(image, b"present").unwrap();
        }
        let references = images
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let probe = AvailabilityProbe {
            root: root.clone(),
            image: Some(references[0].clone()),
            registry: crate::verify::profiles::VerificationRegistry::empty(),
            feedback_configured: true,
            formatter: None,
            formatter_required: false,
            syntax_available: true,
            mechanical_executor: true,
            cached: None,
        };
        let generation = probe.generation_with_runtime(Some(&runtime));
        assert!(
            probe_images(&runtime, references.clone())
                .values()
                .all(|available| *available)
        );
        for image in &images {
            std::fs::remove_file(image).unwrap();
            let state = probe_images(&runtime, references.clone());
            assert!(!state[image.to_string_lossy().as_ref()]);
        }
        assert_ne!(generation, probe.generation_with_runtime(Some(&runtime)));
    }
}
