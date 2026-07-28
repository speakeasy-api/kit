mod sqlite;

use std::{
    cell::RefCell,
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::agent::accounting::UsageEnvelope;
use crate::agent::driver::waiting::WaitingState;
use crate::api::auth::contract::{
    AuthDecision, AuthDenial, AuthenticatedPrincipal, Authorizer, GrantSnapshot, ResourceScope,
};
pub use crate::domain::commands::Command;
use crate::domain::config::{
    ConfigLayer, EffectiveConfigReference, Grant, RunConfigContext, RunConfigMaterializer,
    StaticRunConfigMaterializer,
};
use crate::domain::deletion::{
    ArchiveStatus, DeletionActor, DeletionError, DeletionJob, DeletionJobId,
};
use crate::domain::events::{
    ApprovalDecision, ArtifactRecordId, ArtifactRef, AttemptState, CommitPosition, RunState,
    TraceId,
};
use crate::domain::ids::{ApprovalId, AttemptId, PrincipalId, ProjectId, RunId, ThreadId};
use crate::domain::lifecycle::{AttemptOwnership, FencingToken};
use crate::domain::retention::{RetentionObjectId, StoreTimestamp};
use crate::store::artifacts::{
    ArtifactClass, ArtifactDigest, ArtifactError, ArtifactMetadata, ArtifactRetention,
    ArtifactStore, ReferenceError, now_unix_micros,
};
use crate::store::sqlite::idempotency::IdempotencyKey;

pub use crate::domain::retention::{RetentionPeriod, RetentionPolicy};
pub use sqlite::{DeletionEffect, DeletionWorkerReport, SqliteServiceStore};

pub const MAX_PROMPT_MESSAGE_BYTES: usize = 8 * 1024;
const PROMPT_ORPHAN_GRACE_MICROS: i64 = 24 * 60 * 60 * 1_000_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptInput {
    Message(String),
    Artifact(ArtifactRef),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptCommand {
    pub thread_id: ThreadId,
    pub run_id: Option<RunId>,
    pub input: PromptInput,
    pub run_config: Option<ConfigLayer>,
    pub experiment_config: Option<ConfigLayer>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PromptReceipt {
    pub run_id: RunId,
    pub receipt: CommandReceipt,
}

thread_local! {
    static PENDING_PROMPT_BYTES: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
}

struct PendingPromptGuard;

impl Drop for PendingPromptGuard {
    fn drop(&mut self) {
        PENDING_PROMPT_BYTES.with(|pending| {
            pending.borrow_mut().take();
        });
    }
}

#[derive(Clone, Debug)]
pub struct RequestContext {
    principal: AuthenticatedPrincipal,
    grant: GrantSnapshot,
    idempotency_key: Option<IdempotencyKey>,
    trace_id: TraceId,
}

impl RequestContext {
    pub fn authenticated(
        decision: AuthDecision,
        idempotency_key: Option<IdempotencyKey>,
        trace_id: TraceId,
    ) -> Result<Self, ServiceError> {
        let principal = decision.map_err(ServiceError::Authentication)?;
        Ok(Self {
            grant: principal.grant_snapshot().clone(),
            principal,
            idempotency_key,
            trace_id,
        })
    }

    pub fn principal(&self) -> &AuthenticatedPrincipal {
        &self.principal
    }

    pub fn principal_id(&self) -> PrincipalId {
        self.principal.principal_id()
    }

    pub fn grant(&self) -> &GrantSnapshot {
        &self.grant
    }

    pub fn idempotency_key(&self) -> Option<&IdempotencyKey> {
        self.idempotency_key.as_ref()
    }

    pub fn trace_id(&self) -> &TraceId {
        &self.trace_id
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "query", rename_all = "snake_case")]
pub enum Query {
    GetProject {
        project_id: ProjectId,
    },
    GetProjectRetention {
        project_id: ProjectId,
    },
    ListThreads {
        project_id: ProjectId,
    },
    GetThread {
        thread_id: ThreadId,
    },
    GetDeletionJob {
        deletion_job_id: String,
    },
    ThreadEvents {
        thread_id: ThreadId,
        after: EventCursor,
        limit: usize,
    },
    ListRuns {
        project_id: ProjectId,
    },
    GetRun {
        run_id: RunId,
    },
    GetRunCost {
        run_id: RunId,
    },
    GetRunPrompts {
        run_id: RunId,
    },
    RunTranscript {
        run_id: RunId,
    },
    GetAttempt {
        attempt_id: AttemptId,
    },
    RunTimeline {
        run_id: RunId,
        after: EventCursor,
        limit: usize,
    },
    PendingApprovals {
        project_id: ProjectId,
    },
    PendingAuthRequests {
        project_id: ProjectId,
    },
    GetArtifactMetadata {
        artifact_id: ArtifactRecordId,
    },
    ListCapabilities {
        project_id: ProjectId,
    },
    EventCursorStatus {
        project_id: ProjectId,
        cursor: EventCursor,
    },
    Status {
        project_id: ProjectId,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct EventCursor(u64);

impl EventCursor {
    pub const START: Self = Self(0);

    pub const fn new(position: u64) -> Self {
        Self(position)
    }

    pub const fn position(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectProjection {
    pub id: ProjectId,
    pub principal_id: PrincipalId,
    pub retention: Option<RetentionPolicy>,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ThreadProjection {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub archived: bool,
    pub deletion_requested: bool,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunProjection {
    pub id: RunId,
    pub thread_id: ThreadId,
    pub state: RunState,
    pub input: ArtifactRef,
    pub auth_granted: Option<bool>,
    pub effective_config: EffectiveConfigReference,
    pub owner: Option<AttemptOwnership>,
    #[serde(default)]
    pub output: Option<RunOutputProjection>,
    #[serde(default)]
    pub failure: Option<RunFailureProjection>,
    pub version: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunFailureCode {
    ProviderUnavailable,
    ExecutionFailed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunFailureProjection {
    pub code: RunFailureCode,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunOutputProjection {
    pub artifact: ArtifactRef,
    pub preview: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunCostProjection {
    pub usage: Option<UsageEnvelope>,
    pub cost: Option<crate::telemetry::run_envelope::CostSnapshot>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunPromptProjection {
    pub template_version: Option<String>,
    pub prompt_digest: Option<String>,
    pub stable_prefix_digest: Option<String>,
    pub first_dynamic_byte: Option<u64>,
    pub context_bytes: Option<u64>,
    pub estimated_tokens: Option<u64>,
    pub token_budget: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunTranscriptEntry {
    pub sequence: u64,
    pub attempt: AttemptOwnership,
    pub model_call_id: Option<String>,
    pub kind: String,
    pub content: serde_json::Value,
    pub artifact: Option<ArtifactRef>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunTranscriptProjection {
    pub run_id: RunId,
    pub items: Vec<RunTranscriptEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunProgressRecord {
    pub sequence: u64,
    pub model_call_id: Option<String>,
    pub kind: String,
    pub content: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RunCompletionRecord {
    pub output: RunOutputProjection,
    pub item_preview: serde_json::Value,
    pub usage: UsageEnvelope,
    pub cost: Option<crate::telemetry::run_envelope::CostSnapshot>,
    pub telemetry_digest: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct RunSemanticEnvelope<T> {
    pub schema_version: u16,
    pub run_id: RunId,
    pub project_id: ProjectId,
    pub attempt: AttemptOwnership,
    pub stored_at_unix_micros: i64,
    pub record: T,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttemptProjection {
    pub id: AttemptId,
    pub run_id: RunId,
    pub state: AttemptState,
    pub owner: AttemptOwnership,
    pub version: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttemptDriverClaim {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub principal_id: PrincipalId,
    pub fence: FencingToken,
    pub lease_version: u64,
    pub expires_at_unix_micros: i64,
}

impl AttemptDriverClaim {
    pub const fn owner(self) -> AttemptOwnership {
        AttemptOwnership::new(self.attempt_id, self.principal_id, self.fence)
    }

    pub fn same_lease(self, other: Self) -> bool {
        self.run_id == other.run_id
            && self.attempt_id == other.attempt_id
            && self.principal_id == other.principal_id
            && self.fence == other.fence
            && self.lease_version == other.lease_version
    }
}

#[derive(Clone, Debug)]
pub struct WorkerRun {
    pub run: RunProjection,
    pub attempt: AttemptProjection,
    pub principal_id: PrincipalId,
    pub project_id: ProjectId,
    pub effective_config: Vec<u8>,
    pub start_idempotency_key: String,
    pub occurred_at: crate::domain::events::UtcDateTime,
    pub claim: AttemptDriverClaim,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApprovalProjection {
    pub id: ApprovalId,
    pub run_id: RunId,
    pub decision: Option<ApprovalDecision>,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AuthRequestProjection {
    pub run_id: RunId,
    pub granted: Option<bool>,
    pub version: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtifactMetadataProjection {
    pub id: ArtifactRecordId,
    pub project_id: ProjectId,
    pub reference: ArtifactRef,
    pub media_type: String,
    pub size: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventProjection {
    pub cursor: EventCursor,
    pub project_id: ProjectId,
    pub operation: String,
    pub stream: String,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventPage {
    pub events: Vec<EventProjection>,
    pub next_cursor: EventCursor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CapabilityProjection {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CursorStatusProjection {
    pub requested: EventCursor,
    pub committed: EventCursor,
    pub caught_up: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StatusProjection {
    pub committed: EventCursor,
    pub ready: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "projection", content = "value", rename_all = "snake_case")]
pub enum QueryProjection {
    Project(ProjectProjection),
    Retention(Option<RetentionPolicy>),
    Threads(Vec<ThreadProjection>),
    Thread(ThreadProjection),
    DeletionJob(serde_json::Value),
    Events(EventPage),
    Runs(Vec<RunProjection>),
    Run(RunProjection),
    RunCost(Box<RunCostProjection>),
    RunPrompts(RunPromptProjection),
    RunTranscript(RunTranscriptProjection),
    Attempt(AttemptProjection),
    Approvals(Vec<ApprovalProjection>),
    AuthRequests(Vec<AuthRequestProjection>),
    ArtifactMetadata(ArtifactMetadataProjection),
    Capabilities(Vec<CapabilityProjection>),
    CursorStatus(CursorStatusProjection),
    Status(StatusProjection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationKind {
    Command,
    Query,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorityPath {
    Service,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HandlerDescriptor {
    pub operation: &'static str,
    pub kind: OperationKind,
    pub handler: &'static str,
    pub authority: AuthorityPath,
    pub authority_path_count: usize,
    pub required_grant: Grant,
}

macro_rules! service_registry {
    (
        commands { $( $command:ident => ($command_name:literal, $command_grant:ident) ),+ $(,)? }
        queries { $( $query:ident => ($query_name:literal, $query_grant:ident) ),+ $(,)? }
    ) => {
        pub const HANDLERS: &[HandlerDescriptor] = &[
            $(HandlerDescriptor {
                operation: $command_name,
                kind: OperationKind::Command,
                handler: concat!("Service::execute::", stringify!($command)),
                authority: AuthorityPath::Service,
                authority_path_count: 1,
                required_grant: Grant::$command_grant,
            },)+
            $(HandlerDescriptor {
                operation: $query_name,
                kind: OperationKind::Query,
                handler: concat!("Service::query::", stringify!($query)),
                authority: AuthorityPath::Service,
                authority_path_count: 1,
                required_grant: Grant::$query_grant,
            },)+
        ];

        impl Command {
            pub const fn descriptor(&self) -> &'static HandlerDescriptor {
                let operation = match self {
                    $(Self::$command { .. } => $command_name,)+
                };
                descriptor(operation)
            }

            pub const fn operation(&self) -> &'static str {
                self.descriptor().operation
            }
        }

        impl Query {
            pub const fn descriptor(&self) -> &'static HandlerDescriptor {
                let operation = match self {
                    $(Self::$query { .. } => $query_name,)+
                };
                descriptor(operation)
            }

            pub const fn operation(&self) -> &'static str {
                self.descriptor().operation
            }
        }
    };
}

service_registry! {
    commands {
        CreateProject => ("project.create", WorkspaceWrite),
        SetProjectRetention => ("project.retention.set", WorkspaceWrite),
        CreateThread => ("thread.create", WorkspaceWrite),
        SetThreadArchived => ("thread.archive", WorkspaceWrite),
        InitiateThreadDeletion => ("thread.delete.initiate", WorkspaceWrite),
        StartRun => ("run.start", ModelCall),
        TransitionRun => ("run.transition", ModelCall),
        CancelRun => ("run.cancel", ModelCall),
        ProvideRunInput => ("run.input", ModelCall),
        StartAttempt => ("attempt.start", ModelCall),
        TransitionAttempt => ("attempt.transition", ModelCall),
        RequestApproval => ("approval.request", ModelCall),
        ResolveApproval => ("approval.resolve", ModelCall),
        RequestAuth => ("auth.request", ModelCall),
        ResolveAuth => ("auth.resolve", ModelCall),
        RegisterArtifactMetadata => ("artifact.metadata.register", WorkspaceWrite),
    }
    queries {
        GetProject => ("project.get", WorkspaceRead),
        GetProjectRetention => ("project.retention.get", WorkspaceRead),
        ListThreads => ("thread.list", WorkspaceRead),
        GetThread => ("thread.get", WorkspaceRead),
        GetDeletionJob => ("thread.deletion.get", WorkspaceRead),
        ThreadEvents => ("thread.events", WorkspaceRead),
        ListRuns => ("run.list", WorkspaceRead),
        GetRun => ("run.get", WorkspaceRead),
        GetRunCost => ("run.cost", WorkspaceRead),
        GetRunPrompts => ("run.prompts", WorkspaceRead),
        RunTranscript => ("run.transcript", WorkspaceRead),
        GetAttempt => ("attempt.get", WorkspaceRead),
        RunTimeline => ("run.timeline", WorkspaceRead),
        PendingApprovals => ("approval.pending", WorkspaceRead),
        PendingAuthRequests => ("auth.pending", WorkspaceRead),
        GetArtifactMetadata => ("artifact.metadata.get", WorkspaceRead),
        ListCapabilities => ("capability.list", WorkspaceRead),
        EventCursorStatus => ("event.cursor.status", WorkspaceRead),
        Status => ("service.status", WorkspaceRead),
    }
}

pub const fn handlers() -> &'static [HandlerDescriptor] {
    HANDLERS
}

const fn descriptor(operation: &str) -> &'static HandlerDescriptor {
    let mut index = 0;
    while index < HANDLERS.len() {
        if const_str_eq(HANDLERS[index].operation, operation) {
            return &HANDLERS[index];
        }
        index += 1;
    }
    panic!("unregistered service operation")
}

const fn const_str_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut index = 0;
    while index < left.len() {
        if left[index] != right[index] {
            return false;
        }
        index += 1;
    }
    true
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Resource {
    Project(ProjectId),
    Thread(ThreadId),
    Run(RunId),
    Attempt(AttemptId),
    Approval(ApprovalId),
    AuthRequest(RunId),
    Artifact(ArtifactRecordId),
}

#[derive(Clone, Debug)]
pub struct WriteRequest<'a> {
    pub principal_id: PrincipalId,
    pub idempotency_key: &'a IdempotencyKey,
    pub trace_id: &'a TraceId,
    pub command: &'a Command,
    pub driver_claim: Option<AttemptDriverClaim>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommandReceipt {
    pub operation: &'static str,
    pub commit_positions: Vec<CommitPosition>,
    pub replayed: bool,
}

pub trait ServiceStore {
    fn command_scope(
        &mut self,
        principal_id: PrincipalId,
        command: &Command,
    ) -> Result<ResourceScope, ServiceError>;
    fn query_scope(&mut self, query: &Query) -> Result<ResourceScope, ServiceError>;
    fn execute(&mut self, request: WriteRequest<'_>) -> Result<CommandReceipt, ServiceError>;
    fn query(&mut self, query: &Query) -> Result<QueryProjection, ServiceError>;
    fn deletion_job(
        &mut self,
        actor: DeletionActor,
        id: DeletionJobId,
    ) -> Result<DeletionJob, DeletionError>;
    fn deletion_job_for_request(
        &mut self,
        actor: DeletionActor,
        object_id: RetentionObjectId,
        idempotency_key: &str,
    ) -> Result<DeletionJob, DeletionError>;
    fn archive_status(
        &mut self,
        actor: DeletionActor,
        object_id: RetentionObjectId,
    ) -> Result<ArchiveStatus, DeletionError>;
    fn store_time(&mut self) -> Result<StoreTimestamp, ServiceError>;
}

pub trait WorkerStore {
    fn claim_queued_run(
        &mut self,
        lease_duration: std::time::Duration,
    ) -> Result<Option<WorkerRun>, ServiceError>;
    fn recoverable_runs(&mut self, limit: usize) -> Result<Vec<WorkerRun>, ServiceError>;
    fn claim_recoverable_run(
        &mut self,
        run_id: RunId,
        lease_duration: std::time::Duration,
    ) -> Result<Option<WorkerRun>, ServiceError>;
    fn worker_run(&mut self, run_id: RunId) -> Result<WorkerRun, ServiceError>;
    fn renew_worker_claim(
        &mut self,
        claim: AttemptDriverClaim,
        lease_duration: std::time::Duration,
    ) -> Result<AttemptDriverClaim, ServiceError>;
    fn ensure_worker_wait(
        &mut self,
        run_id: RunId,
        waiting: &WaitingState,
    ) -> Result<WorkerRun, ServiceError>;
    fn transition_worker_run(
        &mut self,
        run_id: RunId,
        target: RunState,
    ) -> Result<WorkerRun, ServiceError>;
    fn transition_worker_attempt(
        &mut self,
        attempt_id: AttemptId,
        target: AttemptState,
    ) -> Result<WorkerRun, ServiceError>;
    fn publish_run_prompt(
        &mut self,
        run_id: RunId,
        claim: AttemptDriverClaim,
        prompt: RunPromptProjection,
    ) -> Result<(), ServiceError>;
    fn publish_run_progress(
        &mut self,
        run_id: RunId,
        claim: AttemptDriverClaim,
        progress: RunProgressRecord,
    ) -> Result<(), ServiceError>;
    fn publish_run_completion(
        &mut self,
        run_id: RunId,
        claim: AttemptDriverClaim,
        completion: RunCompletionRecord,
    ) -> Result<(), ServiceError>;
    fn fail_worker_run(
        &mut self,
        run_id: RunId,
        claim: AttemptDriverClaim,
        failure: RunFailureProjection,
    ) -> Result<(), ServiceError>;
    fn worker_append_store(
        &self,
    ) -> Result<crate::store::sqlite::append::SqliteStore, ServiceError>;
}

pub trait Scheduler {
    fn admit_command(
        &self,
        _principal_id: PrincipalId,
        _idempotency_key: &IdempotencyKey,
        _command: &Command,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    fn command_rejected(
        &self,
        _principal_id: PrincipalId,
        _idempotency_key: &IdempotencyKey,
        _command: &Command,
    ) {
    }

    fn command_committed(
        &self,
        _principal_id: PrincipalId,
        _idempotency_key: &IdempotencyKey,
        _command: &Command,
    ) -> Result<(), ServiceError> {
        Ok(())
    }

    fn command_completed(&self, _observation: CommandObservation<'_>) {}
}

#[derive(Clone, Copy, Debug)]
pub struct CommandObservation<'a> {
    pub trace_id: &'a TraceId,
    pub operation: &'static str,
    pub start_unix_nanos: u64,
    pub end_unix_nanos: u64,
    pub succeeded: bool,
}

pub trait CapabilityService {
    fn auth_resolved(&self, _run_id: RunId, _granted: bool) {}

    fn list(&self, _project_id: ProjectId) -> Vec<CapabilityProjection> {
        Vec::new()
    }
}

pub trait LeaseService {
    fn deletion_requested(&self, _thread_id: ThreadId) {}
}

pub trait ArtifactService {
    fn commit_verified<T>(
        &self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        command: &Command,
        commit: impl FnOnce() -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError>;

    fn metadata_registered(&self, _metadata: &ArtifactMetadataProjection) {}
}

#[cfg(any(test, debug_assertions))]
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopRuntime;

#[cfg(not(any(test, debug_assertions)))]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NoopRuntime;

impl Scheduler for NoopRuntime {}
impl CapabilityService for NoopRuntime {}
impl LeaseService for NoopRuntime {}
impl ArtifactService for NoopRuntime {
    fn commit_verified<T>(
        &self,
        _principal_id: PrincipalId,
        _project_id: ProjectId,
        command: &Command,
        commit: impl FnOnce() -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        if command.artifact_reference().is_some() {
            Err(invalid_artifact_reference())
        } else {
            commit()
        }
    }
}

impl Scheduler for ArtifactStore {}
impl CapabilityService for ArtifactStore {}
impl LeaseService for ArtifactStore {}

impl Scheduler for crate::runtime::scheduler::DurableScheduler {
    fn admit_command(
        &self,
        principal_id: PrincipalId,
        idempotency_key: &IdempotencyKey,
        command: &Command,
    ) -> Result<(), ServiceError> {
        use crate::domain::events::RunState;
        match command {
            Command::StartRun {
                run_id,
                effective_config,
                ..
            } => {
                let snapshot = effective_config
                    .as_deref()
                    .ok_or_else(|| {
                        ServiceError::Invalid("run config snapshot is missing".to_owned())
                    })
                    .and_then(|bytes| {
                        crate::domain::config::RunConfigSnapshot::from_canonical_bytes(bytes)
                            .map_err(|error| ServiceError::Invalid(error.to_string()))
                    })?;
                self.register_run_with_snapshot(
                    *run_id,
                    principal_id,
                    idempotency_key.as_str(),
                    &snapshot,
                )
                .map_err(scheduler_error)
            }
            Command::TransitionRun {
                run_id, transition, ..
            } if transition.from() == RunState::Queued
                && transition.to() == RunState::AcquiringWorkspace =>
            {
                self.admit_run(*run_id).map_err(scheduler_error)
            }
            Command::TransitionRun {
                run_id, transition, ..
            } if transition.to() == RunState::Running => self
                .reserve(&model_reservation(
                    principal_id,
                    idempotency_key,
                    command,
                    *run_id,
                ))
                .map(|_| ())
                .map_err(scheduler_error),
            Command::ProvideRunInput { run_id, .. } => self
                .reserve(&model_reservation(
                    principal_id,
                    idempotency_key,
                    command,
                    *run_id,
                ))
                .map(|_| ())
                .map_err(scheduler_error),
            _ => Ok(()),
        }
    }

    fn command_rejected(
        &self,
        principal_id: PrincipalId,
        idempotency_key: &IdempotencyKey,
        command: &Command,
    ) {
        use crate::domain::events::RunState;
        let result = match command {
            Command::StartRun { run_id, .. } => self.rollback_run_admission(*run_id),
            Command::TransitionRun {
                run_id, transition, ..
            } if transition.from() == RunState::Queued
                && transition.to() == RunState::AcquiringWorkspace =>
            {
                self.rollback_dispatch(*run_id)
            }
            Command::TransitionRun {
                run_id, transition, ..
            } if transition.to() == RunState::Running => self
                .release(model_reservation(principal_id, idempotency_key, command, *run_id).id)
                .map(|_| ()),
            Command::ProvideRunInput { run_id, .. } => self
                .release(model_reservation(principal_id, idempotency_key, command, *run_id).id)
                .map(|_| ()),
            _ => Ok(()),
        };
        let _ = result;
    }

    fn command_committed(
        &self,
        principal_id: PrincipalId,
        idempotency_key: &IdempotencyKey,
        command: &Command,
    ) -> Result<(), ServiceError> {
        use crate::domain::events::RunState;
        match command {
            Command::TransitionRun {
                run_id, transition, ..
            } if transition.to() == RunState::Running => settle_model_reservation(
                self,
                model_reservation(principal_id, idempotency_key, command, *run_id).id,
            ),
            Command::ProvideRunInput { run_id, .. } => settle_model_reservation(
                self,
                model_reservation(principal_id, idempotency_key, command, *run_id).id,
            ),
            Command::TransitionRun {
                run_id, transition, ..
            } if transition.to().is_terminal() => self
                .finish_run(*run_id, transition.to() == RunState::Cancelled)
                .map_err(scheduler_error),
            Command::TransitionRun {
                run_id, transition, ..
            } if transition.to() == RunState::Interrupted => {
                self.requeue_run(*run_id).map_err(scheduler_error)
            }
            Command::CancelRun { run_id, .. } => {
                self.cancel_reservations(*run_id).map_err(scheduler_error)
            }
            _ => Ok(()),
        }
    }
}

fn model_reservation(
    principal_id: PrincipalId,
    idempotency_key: &IdempotencyKey,
    command: &Command,
    run_id: RunId,
) -> crate::runtime::scheduler::ReservationRequest {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"KIT-SCHEDULER-MODEL\0");
    bytes.extend_from_slice(principal_id.to_string().as_bytes());
    bytes.extend_from_slice(run_id.to_string().as_bytes());
    bytes.extend_from_slice(command.operation().as_bytes());
    bytes.extend_from_slice(idempotency_key.as_str().as_bytes());
    let digest = blake3::hash(&bytes);
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest.as_bytes()[..16]);
    crate::runtime::scheduler::ReservationRequest {
        id: crate::runtime::scheduler::reserve::ReservationId::new(u128::from_be_bytes(id)),
        run_id,
        principal_id,
        attempt: None,
        idempotency_key: format!("model:{}:{}", command.operation(), idempotency_key.as_str()),
        kind: crate::runtime::scheduler::AdmissionKind::Model,
        spend: crate::runtime::scheduler::limits::Spend::new(0, 0, 1, 0, 0),
    }
}

fn settle_model_reservation(
    scheduler: &crate::runtime::scheduler::DurableScheduler,
    id: crate::runtime::scheduler::reserve::ReservationId,
) -> Result<(), ServiceError> {
    use crate::runtime::scheduler::reserve::ReservationStatus;
    match scheduler.snapshot(id).map_err(scheduler_error)?.status() {
        ReservationStatus::Reserved => {
            scheduler.mark_dispatched(id).map_err(scheduler_error)?;
            scheduler.debit(id).map(|_| ()).map_err(scheduler_error)
        }
        ReservationStatus::Debited | ReservationStatus::Reconciled => Ok(()),
        ReservationStatus::Released => Ok(()),
    }
}

fn scheduler_error(error: crate::runtime::scheduler::SchedulerError) -> ServiceError {
    match error {
        crate::runtime::scheduler::SchedulerError::Database(_) => {
            ServiceError::Store(error.to_string())
        }
        _ => ServiceError::Conflict(error.to_string()),
    }
}

impl ArtifactService for ArtifactStore {
    fn commit_verified<T>(
        &self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        command: &Command,
        commit: impl FnOnce() -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        let Some(reference) = command.artifact_reference() else {
            return commit();
        };
        let digest =
            ArtifactDigest::parse(reference.as_str()).map_err(|_| invalid_artifact_reference())?;
        if let Some(bytes) = PENDING_PROMPT_BYTES.with(|pending| pending.borrow_mut().take()) {
            if bytes.is_empty()
                || bytes.len() > MAX_PROMPT_MESSAGE_BYTES
                || ArtifactDigest::digest(&bytes) != digest
            {
                return Err(invalid_artifact_reference());
            }
            match self.open_verified(digest) {
                Ok(artifact)
                    if artifact.manifest().principal == principal_id.to_string()
                        && artifact.manifest().project == project_id.to_string()
                        && artifact.manifest().media_type == "text/plain; charset=utf-8"
                        && artifact.manifest().class == ArtifactClass::File => {}
                Ok(_) => return Err(invalid_artifact_reference()),
                Err(ArtifactError::Missing(_)) => {
                    let stored_at = now_unix_micros().map_err(artifact_error)?;
                    let metadata = ArtifactMetadata::new(
                        "text/plain; charset=utf-8",
                        ArtifactClass::File,
                        principal_id.to_string(),
                        project_id.to_string(),
                        ArtifactRetention::UntilUnixMicros(
                            stored_at.saturating_add(PROMPT_ORPHAN_GRACE_MICROS),
                        ),
                        stored_at,
                    )
                    .map_err(artifact_error)?;
                    let artifact = self.put(&bytes, metadata).map_err(artifact_error)?;
                    if artifact.digest() != digest {
                        return Err(invalid_artifact_reference());
                    }
                }
                Err(error) => return Err(artifact_error(error)),
            }
        }
        let artifact = self.open_verified(digest).map_err(artifact_error)?;
        let manifest = artifact.manifest();
        if manifest.principal != principal_id.to_string()
            || manifest.project != project_id.to_string()
            || command
                .artifact_metadata()
                .is_some_and(|(media_type, size)| {
                    manifest.media_type != media_type || manifest.size != size
                })
        {
            return Err(invalid_artifact_reference());
        }
        self.commit_reference(&artifact, |_| commit())
            .map_err(|error| match error {
                ReferenceError::Artifact(error) => artifact_error(error),
                ReferenceError::Commit(error) => error,
            })
    }
}

pub struct Service<S, A, R, M = StaticRunConfigMaterializer> {
    store: S,
    authorizer: A,
    runtime: R,
    config_materializer: M,
}

impl<S, A> Service<S, A, NoopRuntime, StaticRunConfigMaterializer> {
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn new(
        store: S,
        authorizer: A,
        _authority: &crate::runtime::daemon::ControlPlaneAuthority,
    ) -> Self {
        Self {
            store,
            authorizer,
            runtime: NoopRuntime,
            config_materializer: StaticRunConfigMaterializer::default(),
        }
    }
}

impl<S, A, M> Service<S, A, NoopRuntime, M> {
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn with_config(
        store: S,
        authorizer: A,
        config_materializer: M,
        _authority: &crate::runtime::daemon::ControlPlaneAuthority,
    ) -> Self {
        Self {
            store,
            authorizer,
            runtime: NoopRuntime,
            config_materializer,
        }
    }
}

impl<S, A, R> Service<S, A, R, StaticRunConfigMaterializer> {
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn with_runtime(
        store: S,
        authorizer: A,
        runtime: R,
        _authority: &crate::runtime::daemon::ControlPlaneAuthority,
    ) -> Self {
        Self {
            store,
            authorizer,
            runtime,
            config_materializer: StaticRunConfigMaterializer::default(),
        }
    }
}

impl<S, A, R, M> Service<S, A, R, M> {
    pub(crate) fn with_runtime_and_config(
        store: S,
        authorizer: A,
        runtime: R,
        config_materializer: M,
        _authority: &crate::runtime::daemon::ControlPlaneAuthority,
    ) -> Self {
        Self {
            store,
            authorizer,
            runtime,
            config_materializer,
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }

    pub fn into_store(self) -> S {
        self.store
    }
}

impl<S, A, R, M> Service<S, A, R, M>
where
    S: ServiceStore,
    A: Authorizer,
    R: Scheduler + CapabilityService + LeaseService + ArtifactService,
    M: RunConfigMaterializer,
{
    pub fn prompt(
        &mut self,
        context: &RequestContext,
        request: PromptCommand,
    ) -> Result<PromptReceipt, ServiceError> {
        let run_id = match request.run_id {
            Some(run_id) => run_id,
            None => deterministic_prompt_run_id(context, request.thread_id)?,
        };
        let (input, message) = match request.input {
            PromptInput::Artifact(reference) => (reference, None),
            PromptInput::Message(message) => {
                if message.is_empty() || message.len() > MAX_PROMPT_MESSAGE_BYTES {
                    return Err(ServiceError::Invalid(format!(
                        "message must contain 1 to {MAX_PROMPT_MESSAGE_BYTES} UTF-8 bytes"
                    )));
                }
                let bytes = message.into_bytes();
                let reference = ArtifactRef::parse(&ArtifactDigest::digest(&bytes).to_string())
                    .map_err(|_| {
                        ServiceError::Store("failed to encode prompt artifact".to_owned())
                    })?;
                (reference, Some(bytes))
            }
        };
        let command = Command::StartRun {
            schema_version: crate::domain::events::SchemaVersion::CURRENT,
            run_id,
            thread_id: request.thread_id,
            input,
            run_config: request.run_config.map(Box::new),
            experiment_config: request.experiment_config.map(Box::new),
            effective_config: None,
        };
        let receipt = if let Some(message) = message {
            with_pending_prompt(message, || self.execute(context, command))?
        } else {
            self.execute(context, command)?
        };
        Ok(PromptReceipt { run_id, receipt })
    }

    pub fn execute(
        &mut self,
        context: &RequestContext,
        mut command: Command,
    ) -> Result<CommandReceipt, ServiceError> {
        let operation = command.operation();
        let start_unix_nanos = now_unix_nanos();
        let result = (|| {
            let descriptor = command.descriptor();
            let scope = self.store.command_scope(context.principal_id(), &command)?;
            self.authorizer
                .authorize(context.principal(), scope, descriptor.required_grant)
                .map_err(ServiceError::Authentication)?;
            if let Command::StartRun {
                run_id,
                run_config,
                experiment_config,
                effective_config,
                ..
            } = &mut command
            {
                if effective_config.is_some() {
                    return Err(ServiceError::Invalid(
                        "effective run config is service-owned".to_owned(),
                    ));
                }
                let snapshot = self
                    .config_materializer
                    .materialize(
                        RunConfigContext {
                            principal_id: scope.principal_id(),
                            project_id: scope.project_id(),
                            run_id: *run_id,
                        },
                        context.grant().grants(),
                        run_config.take().map(|layer| *layer),
                        experiment_config.take().map(|layer| *layer),
                    )
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?;
                *effective_config = Some(snapshot.canonical_bytes());
            }
            let idempotency_key = context
                .idempotency_key()
                .ok_or(ServiceError::MissingIdempotencyKey)?;
            let principal_id = scope.principal_id();
            let project_id = scope.project_id();
            let runtime = &self.runtime;
            let store = &mut self.store;
            runtime.admit_command(principal_id, idempotency_key, &command)?;
            let receipt = match runtime.commit_verified(principal_id, project_id, &command, || {
                store.execute(WriteRequest {
                    principal_id: context.principal_id(),
                    idempotency_key,
                    trace_id: context.trace_id(),
                    command: &command,
                    driver_claim: None,
                })
            }) {
                Ok(receipt) => receipt,
                Err(error) => {
                    runtime.command_rejected(principal_id, idempotency_key, &command);
                    return Err(error);
                }
            };
            runtime.command_committed(principal_id, idempotency_key, &command)?;
            if !receipt.replayed {
                self.notify(&command);
            }
            Ok(receipt)
        })();
        self.runtime.command_completed(CommandObservation {
            trace_id: context.trace_id(),
            operation,
            start_unix_nanos,
            end_unix_nanos: now_unix_nanos(),
            succeeded: result.is_ok(),
        });
        result
    }

    pub fn query(
        &mut self,
        context: &RequestContext,
        query: Query,
    ) -> Result<QueryProjection, ServiceError> {
        let descriptor = query.descriptor();
        if let Query::GetDeletionJob { deletion_job_id } = &query {
            self.authorizer
                .authorize(
                    context.principal(),
                    ResourceScope::new(context.principal_id(), context.grant().project_id()),
                    descriptor.required_grant,
                )
                .map_err(|_| ServiceError::NotFound)?;
            let id = deletion_job_id
                .parse()
                .map_err(|_| ServiceError::NotFound)?;
            let actor = DeletionActor::new(context.principal_id(), context.grant().project_id());
            let job = self.store.deletion_job(actor, id).map_err(deletion_error)?;
            return Ok(QueryProjection::DeletionJob(deletion_job_json(&job)));
        }
        let scope = self
            .store
            .query_scope(&query)
            .map_err(ServiceError::hide_existence)?;
        self.authorizer
            .authorize(context.principal(), scope, descriptor.required_grant)
            .map_err(|_| ServiceError::NotFound)?;
        if let Query::ListCapabilities { project_id } = query {
            return Ok(QueryProjection::Capabilities(self.runtime.list(project_id)));
        }
        self.store
            .query(&query)
            .map_err(ServiceError::hide_existence)
    }

    fn notify(&self, command: &Command) {
        match command {
            Command::ResolveAuth {
                run_id, granted, ..
            } => self.runtime.auth_resolved(*run_id, *granted),
            Command::InitiateThreadDeletion { thread_id, .. } => {
                self.runtime.deletion_requested(*thread_id)
            }
            Command::RegisterArtifactMetadata {
                artifact_id,
                project_id,
                reference,
                media_type,
                size,
                ..
            } => self
                .runtime
                .metadata_registered(&ArtifactMetadataProjection {
                    id: *artifact_id,
                    project_id: *project_id,
                    reference: reference.clone(),
                    media_type: media_type.clone(),
                    size: *size,
                }),
            _ => {}
        }
    }

    pub(crate) fn deletion_archive_status(
        &mut self,
        context: &RequestContext,
        thread_id: ThreadId,
    ) -> Result<crate::domain::deletion::ArchiveStatus, DeletionError> {
        self.store.archive_status(
            DeletionActor::new(context.principal_id(), context.grant().project_id()),
            RetentionObjectId::Transcript(thread_id),
        )
    }

    pub(crate) fn deletion_job_for_thread(
        &mut self,
        context: &RequestContext,
        thread_id: ThreadId,
    ) -> Result<crate::domain::deletion::DeletionJob, DeletionError> {
        self.store.deletion_job_for_request(
            DeletionActor::new(context.principal_id(), context.grant().project_id()),
            RetentionObjectId::Transcript(thread_id),
            context
                .idempotency_key()
                .ok_or(DeletionError::InvalidIdempotencyKey)?
                .as_str(),
        )
    }

    pub(crate) fn deletion_job(
        &mut self,
        context: &RequestContext,
        id: crate::domain::deletion::DeletionJobId,
    ) -> Result<crate::domain::deletion::DeletionJob, DeletionError> {
        self.store.deletion_job(
            DeletionActor::new(context.principal_id(), context.grant().project_id()),
            id,
        )
    }

    pub(crate) fn deletion_project_policy(
        &mut self,
        context: &RequestContext,
        project_id: ProjectId,
    ) -> Result<crate::domain::retention::RetentionPolicy, DeletionError> {
        let query = Query::GetProjectRetention { project_id };
        let scope = self
            .store
            .query_scope(&query)
            .map_err(|_| DeletionError::NotFound)?;
        self.authorizer
            .authorize(context.principal(), scope, Grant::WorkspaceRead)
            .map_err(|_| DeletionError::NotFound)?;
        match self
            .store
            .query(&query)
            .map_err(|_| DeletionError::NotFound)?
        {
            QueryProjection::Retention(Some(policy)) => Ok(policy),
            QueryProjection::Retention(None) => Ok(RetentionPolicy::FOREVER),
            _ => Err(DeletionError::NotFound),
        }
    }
}

fn deterministic_prompt_run_id(
    context: &RequestContext,
    thread_id: ThreadId,
) -> Result<RunId, ServiceError> {
    let key = context
        .idempotency_key()
        .ok_or(ServiceError::MissingIdempotencyKey)?;
    let digest = blake3::hash(
        format!(
            "KIT-PROMPT-RUN-V1\0{}\0{}\0{}",
            context.principal_id(),
            thread_id,
            key.as_str()
        )
        .as_bytes(),
    )
    .to_hex();
    RunId::parse(&format!("run_0{}", &digest.as_str()[..25]))
        .map_err(|_| ServiceError::Store("failed to derive prompt run identifier".to_owned()))
}

fn with_pending_prompt<T>(
    bytes: Vec<u8>,
    commit: impl FnOnce() -> Result<T, ServiceError>,
) -> Result<T, ServiceError> {
    PENDING_PROMPT_BYTES.with(|pending| {
        let mut pending = pending.borrow_mut();
        if pending.is_some() {
            return Err(ServiceError::Store(
                "nested prompt command is not supported".to_owned(),
            ));
        }
        *pending = Some(bytes);
        drop(pending);
        let _guard = PendingPromptGuard;
        commit()
    })
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

impl Command {
    fn artifact_reference(&self) -> Option<&ArtifactRef> {
        match self {
            Self::StartRun { input, .. } | Self::ProvideRunInput { input, .. } => Some(input),
            Self::RegisterArtifactMetadata { reference, .. } => Some(reference),
            _ => None,
        }
    }

    fn artifact_metadata(&self) -> Option<(&str, u64)> {
        match self {
            Self::RegisterArtifactMetadata {
                media_type, size, ..
            } => Some((media_type, *size)),
            _ => None,
        }
    }
}

fn invalid_artifact_reference() -> ServiceError {
    ServiceError::Invalid("artifact reference is not a verified published artifact".to_owned())
}

fn artifact_error(error: ArtifactError) -> ServiceError {
    match error {
        ArtifactError::Io(error) => ServiceError::Store(error.to_string()),
        _ => invalid_artifact_reference(),
    }
}

fn deletion_error(error: DeletionError) -> ServiceError {
    match error {
        DeletionError::NotFound => ServiceError::NotFound,
        DeletionError::InvalidIdempotencyKey => ServiceError::MissingIdempotencyKey,
        DeletionError::IdempotencyConflict => {
            ServiceError::Conflict("idempotency key reused with different input".to_owned())
        }
        DeletionError::LegalHold { .. } => {
            ServiceError::Conflict("physical deletion is blocked by legal hold".to_owned())
        }
        DeletionError::StaleFence { .. } => {
            ServiceError::Conflict("deletion worker fence is stale".to_owned())
        }
        DeletionError::InvalidState(_) => {
            ServiceError::Conflict("deletion job is in an invalid state".to_owned())
        }
    }
}

pub(crate) fn deletion_job_json(job: &crate::domain::deletion::DeletionJob) -> serde_json::Value {
    serde_json::json!({
        "id": job.id.to_string(),
        "state": job.state.as_str(),
        "version": job.version,
        "resource_version": job.resource_version,
        "effective_retention": {
            "policy": retention_policy_json(job.effective_retention.policy),
            "earliest_physical_deletion": match job.effective_retention.earliest_physical_deletion {
                crate::domain::retention::EarliestPhysicalDeletion::At(at) => {
                    serde_json::json!({ "at_unix_micros": at.unix_micros() })
                }
                crate::domain::retention::EarliestPhysicalDeletion::Never => serde_json::json!("never"),
            },
        },
        "blockers": job.blockers.iter().map(|blocker| blocker.as_str()).collect::<Vec<_>>(),
        "fence": job.fence.get(),
        "requested_at_unix_micros": job.requested_at.unix_micros(),
        "completed_at_unix_micros": job.completed_at.map(StoreTimestamp::unix_micros),
        "failure": job.failure.as_deref(),
        "audit": job.audit.iter().map(|entry| serde_json::json!({
            "sequence": entry.sequence,
            "state": entry.state.as_str(),
            "at_unix_micros": entry.at.unix_micros(),
        })).collect::<Vec<_>>(),
    })
}

fn retention_policy_json(policy: crate::domain::retention::RetentionPolicy) -> serde_json::Value {
    fn period(period: crate::domain::retention::RetentionPeriod) -> serde_json::Value {
        match period {
            crate::domain::retention::RetentionPeriod::ForMicros(value) => {
                serde_json::json!({ "for_micros": value })
            }
            crate::domain::retention::RetentionPeriod::Forever => serde_json::json!("forever"),
        }
    }
    serde_json::json!({
        "event": period(policy.event),
        "transcript": period(policy.transcript),
        "terminal": period(policy.terminal),
        "artifact": period(policy.artifact),
        "experiment": period(policy.experiment),
        "backup": period(policy.backup),
    })
}

#[derive(Debug)]
pub enum ServiceError {
    Authentication(AuthDenial),
    MissingIdempotencyKey,
    NotFound,
    Conflict(String),
    Invalid(String),
    Store(String),
}

impl ServiceError {
    fn hide_existence(self) -> Self {
        match self {
            Self::Store(_) | Self::Conflict(_) | Self::Invalid(_) => self,
            Self::Authentication(_) | Self::MissingIdempotencyKey | Self::NotFound => {
                Self::NotFound
            }
        }
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authentication(error) => error.fmt(f),
            Self::MissingIdempotencyKey => f.write_str("idempotency key is required"),
            Self::NotFound => f.write_str("resource not found"),
            Self::Conflict(message) => write!(f, "command conflict: {message}"),
            Self::Invalid(message) => write!(f, "invalid command: {message}"),
            Self::Store(message) => write!(f, "service store error: {message}"),
        }
    }
}

impl std::error::Error for ServiceError {}
