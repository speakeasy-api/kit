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
use crate::domain::ids::{
    ApprovalId, AttemptId, McpCallbackId, PrincipalId, ProjectId, RunId, ThreadId,
};
use crate::domain::lifecycle::{AttemptOwnership, FencingToken};
use crate::domain::mcp_callback::{
    McpCallbackAction, McpCallbackArtifactRef, McpCallbackProjection,
};
use crate::domain::retention::{RetentionObjectId, StoreTimestamp};
use crate::store::artifacts::{
    ArtifactClass, ArtifactDigest, ArtifactError, ArtifactMetadata, ArtifactReference,
    ArtifactRetention, ArtifactStore, ReferenceError, now_unix_micros,
};
use crate::store::sqlite::idempotency::IdempotencyKey;

pub use crate::domain::retention::{RetentionPeriod, RetentionPolicy};
pub(crate) use sqlite::project_event_envelopes_with_state;
pub use sqlite::{DeletionEffect, DeletionWorkerReport, SqliteServiceStore};
pub(crate) use sqlite::{ProjectedEventEnvelope, project_event_envelopes};

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
    static PENDING_CALLBACK_ARTIFACT: RefCell<Option<PendingCallbackArtifact>> = const { RefCell::new(None) };
}

struct PendingCallbackArtifact {
    bytes: Vec<u8>,
    principal_id: PrincipalId,
    project_id: ProjectId,
    run_id: RunId,
    callback_id: McpCallbackId,
    reference: ArtifactReference,
    expires_at_unix_micros: i64,
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
        #[serde(default)]
        opaque_cursor: Option<String>,
    },
    #[doc(hidden)]
    ThreadEventsProjected {
        thread_id: ThreadId,
        after: EventCursor,
        limit: usize,
        projection_state: crate::domain::secret::JsonProjectionState,
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
        #[serde(default)]
        opaque_cursor: Option<String>,
    },
    #[doc(hidden)]
    RunTimelineProjected {
        run_id: RunId,
        after: EventCursor,
        limit: usize,
        projection_state: crate::domain::secret::JsonProjectionState,
    },
    PendingApprovals {
        project_id: ProjectId,
    },
    PendingAuthRequests {
        project_id: ProjectId,
    },
    PendingMcpCallbacks {
        project_id: ProjectId,
    },
    GetMcpCallback {
        callback_id: McpCallbackId,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opaque_cursor: Option<String>,
    pub project_id: ProjectId,
    pub operation: String,
    pub stream: String,
    pub payload: Vec<u8>,
    pub envelope: Vec<u8>,
    pub authority_digest: String,
    pub projection_digest: String,
}

pub(crate) const MAX_EVENT_PAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventPage {
    pub events: Vec<EventProjection>,
    pub next_cursor: EventCursor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opaque_next_cursor: Option<String>,
    pub truncated: bool,
}

#[doc(hidden)]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectedEventPage {
    pub page: EventPage,
    pub projection_state: crate::domain::secret::JsonProjectionState,
    pub item_projection_states: Vec<crate::domain::secret::JsonProjectionState>,
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
    #[doc(hidden)]
    ProjectedEvents(ProjectedEventPage),
    Runs(Vec<RunProjection>),
    Run(RunProjection),
    RunCost(Box<RunCostProjection>),
    RunPrompts(RunPromptProjection),
    RunTranscript(RunTranscriptProjection),
    Attempt(AttemptProjection),
    Approvals(Vec<ApprovalProjection>),
    AuthRequests(Vec<AuthRequestProjection>),
    McpCallbacks(Vec<McpCallbackProjection>),
    McpCallback(Box<McpCallbackProjection>),
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
                    Self::ThreadEventsProjected { .. } => "thread.events",
                    Self::RunTimelineProjected { .. } => "run.timeline",
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
        ResolveMcpCallback => ("mcp_callback.resolve", ModelCall),
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
        PendingMcpCallbacks => ("mcp_callback.pending", WorkspaceRead),
        GetMcpCallback => ("mcp_callback.get", WorkspaceRead),
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
    McpCallback(McpCallbackId),
}

pub struct WriteRequest<'a> {
    pub principal_id: PrincipalId,
    pub idempotency_key: &'a IdempotencyKey,
    pub trace_id: &'a TraceId,
    pub command: &'a Command,
    pub driver_claim: Option<AttemptDriverClaim>,
    pub mcp_callback_request_digest: Option<[u8; 32]>,
    pub mcp_callback_authority: Option<&'a McpCallbackProjection>,
    pub mcp_callback_recheck: Option<&'a dyn Fn(&McpCallbackProjection) -> bool>,
    pub mcp_callback_workspace_revision: Option<&'a str>,
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
    fn replay_mcp_callback_resolution(
        &mut self,
        _principal_id: PrincipalId,
        _idempotency_key: &IdempotencyKey,
        _command: &Command,
    ) -> Result<Option<CommandReceipt>, ServiceError> {
        Ok(None)
    }
    fn reserve_mcp_callback_resolution(
        &mut self,
        _principal_id: PrincipalId,
        _project_id: ProjectId,
        _idempotency_key: &IdempotencyKey,
        _command: &Command,
    ) -> Result<Option<CommandReceipt>, ServiceError> {
        Ok(None)
    }
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

    #[allow(clippy::too_many_arguments)]
    fn store_mcp_callback_content(
        &self,
        _principal_id: PrincipalId,
        _project_id: ProjectId,
        _run_id: RunId,
        _callback_id: McpCallbackId,
        _idempotency_key: &IdempotencyKey,
        _bytes: &[u8],
        _expires_at_unix_micros: i64,
    ) -> Result<McpCallbackArtifactRef, ServiceError> {
        Err(ServiceError::Store(
            "MCP callback artifact storage is unavailable".to_owned(),
        ))
    }

    fn mcp_callback_revision_live(&self, _revision: &str) -> bool {
        false
    }

    fn mcp_callback_content_public(
        &self,
        _callback: &McpCallbackProjection,
        _content: &serde_json::Value,
    ) -> bool {
        false
    }

    fn with_mcp_callback_revision<T>(
        &self,
        revision: &str,
        commit: impl FnOnce(&str) -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        if self.mcp_callback_revision_live(revision) {
            commit(revision)
        } else {
            Err(ServiceError::Conflict(
                "callback workspace revision is stale".to_owned(),
            ))
        }
    }
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
        ReservationStatus::ActualOverage => Err(ServiceError::Conflict(
            "provider actual usage exceeded its reservation".to_owned(),
        )),
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
        if matches!(command, Command::ResolveMcpCallback { .. }) {
            let pending = PENDING_CALLBACK_ARTIFACT.with(|pending| pending.borrow_mut().take());
            let Some(pending) = pending else {
                return commit();
            };
            if pending.principal_id != principal_id || pending.project_id != project_id {
                return Err(invalid_artifact_reference());
            }
            let stored_at = pending.expires_at_unix_micros.saturating_sub(1);
            let metadata = ArtifactMetadata::new(
                "application/vnd.kit.artifact-envelope",
                ArtifactClass::File,
                principal_id.to_string(),
                project_id.to_string(),
                ArtifactRetention::UntilUnixMicros(pending.expires_at_unix_micros),
                stored_at,
            )
            .map_err(artifact_error)?;
            let envelope = crate::store::artifacts::ArtifactEnvelopeBinding {
                principal: pending.principal_id.to_string(),
                project: pending.project_id.to_string(),
                run: pending.run_id.to_string(),
                purpose: "mcp_callback_content".to_owned(),
                invocation_id: None,
                callback_id: Some(pending.callback_id.to_string()),
            }
            .seal(&pending.bytes)
            .map_err(artifact_error)?;
            let publication = self
                .stage_publication(&envelope, metadata, pending.reference)
                .map_err(artifact_error)?;
            return match commit() {
                Ok(result) => Ok(result),
                Err(error) => {
                    self.remove_publication_stage(publication.reference())
                        .map_err(artifact_error)?;
                    Err(error)
                }
            };
        }
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

    fn store_mcp_callback_content(
        &self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        run_id: RunId,
        callback_id: McpCallbackId,
        idempotency_key: &IdempotencyKey,
        bytes: &[u8],
        expires_at_unix_micros: i64,
    ) -> Result<McpCallbackArtifactRef, ServiceError> {
        let mut identity = Vec::new();
        for field in [
            principal_id.to_string(),
            project_id.to_string(),
            run_id.to_string(),
            callback_id.to_string(),
            idempotency_key.as_str().to_owned(),
            ArtifactDigest::digest(bytes).to_string(),
        ] {
            identity.extend_from_slice(&(field.len() as u64).to_be_bytes());
            identity.extend_from_slice(field.as_bytes());
        }
        let reference = ArtifactReference::derive(b"kit-mcp-callback-artifact-v1", &identity);
        PENDING_CALLBACK_ARTIFACT.with(|pending| {
            let mut pending = pending.borrow_mut();
            if let Some(existing) = pending.as_ref() {
                if existing.bytes == bytes
                    && existing.principal_id == principal_id
                    && existing.project_id == project_id
                    && existing.run_id == run_id
                    && existing.callback_id == callback_id
                    && existing.reference == reference
                    && existing.expires_at_unix_micros == expires_at_unix_micros
                {
                    return McpCallbackArtifactRef::parse(&reference.to_string()).map_err(|_| {
                        ServiceError::Store("failed to encode callback artifact".to_owned())
                    });
                }
                return Err(ServiceError::Store(
                    "nested callback artifact preparation is not supported".to_owned(),
                ));
            }
            *pending = Some(PendingCallbackArtifact {
                bytes: bytes.to_vec(),
                principal_id,
                project_id,
                run_id,
                callback_id,
                reference,
                expires_at_unix_micros,
            });
            McpCallbackArtifactRef::parse(&reference.to_string())
                .map_err(|_| ServiceError::Store("failed to encode callback artifact".to_owned()))
        })
    }
}

pub struct Service<S, A, R, M = StaticRunConfigMaterializer> {
    store: S,
    authorizer: A,
    runtime: R,
    config_materializer: M,
    custody: crate::domain::secret::SecretCustody,
}

impl<S, A> Service<S, A, NoopRuntime, StaticRunConfigMaterializer> {
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn new(
        store: S,
        authorizer: A,
        authority: &crate::runtime::daemon::ControlPlaneAuthority,
    ) -> Self {
        Self {
            store,
            authorizer,
            runtime: NoopRuntime,
            config_materializer: StaticRunConfigMaterializer::default(),
            custody: authority.secret_custody(),
        }
    }
}

impl<S, A, M> Service<S, A, NoopRuntime, M> {
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn with_config(
        store: S,
        authorizer: A,
        config_materializer: M,
        authority: &crate::runtime::daemon::ControlPlaneAuthority,
    ) -> Self {
        Self {
            store,
            authorizer,
            runtime: NoopRuntime,
            config_materializer,
            custody: authority.secret_custody(),
        }
    }
}

impl<S, A, R> Service<S, A, R, StaticRunConfigMaterializer> {
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn with_runtime(
        store: S,
        authorizer: A,
        runtime: R,
        authority: &crate::runtime::daemon::ControlPlaneAuthority,
    ) -> Self {
        Self {
            store,
            authorizer,
            runtime,
            config_materializer: StaticRunConfigMaterializer::default(),
            custody: authority.secret_custody(),
        }
    }
}

impl<S, A, R, M> Service<S, A, R, M> {
    pub(crate) fn with_runtime_and_config(
        store: S,
        authorizer: A,
        runtime: R,
        config_materializer: M,
        authority: &crate::runtime::daemon::ControlPlaneAuthority,
    ) -> Self {
        Self {
            store,
            authorizer,
            runtime,
            config_materializer,
            custody: authority.secret_custody(),
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
                let bytes = self
                    .custody
                    .project_text_references(
                        crate::telemetry::redact::CaptureBoundary::Prompt,
                        &message,
                    )
                    .into_bytes();
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
            let mut preloaded_mcp_callback = None;
            let required_grant = if let Command::ResolveMcpCallback { callback_id, .. } = &command {
                let callback = match self.store.query(&Query::GetMcpCallback {
                    callback_id: *callback_id,
                })? {
                    QueryProjection::McpCallback(callback) => callback,
                    _ => {
                        return Err(ServiceError::Store(
                            "invalid callback projection".to_owned(),
                        ));
                    }
                };
                let required = callback
                    .url_binding
                    .as_ref()
                    .map_or(descriptor.required_grant, |binding| {
                        binding.original_effect.required_grant()
                    });
                preloaded_mcp_callback = Some(callback);
                required
            } else {
                descriptor.required_grant
            };
            self.authorizer
                .authorize(context.principal(), scope, required_grant)
                .map_err(ServiceError::Authentication)?;
            let url_accept = matches!(
                &command,
                Command::ResolveMcpCallback {
                    mode: crate::domain::mcp_callback::McpCallbackMode::Url,
                    action: McpCallbackAction::Accept,
                    ..
                }
            );
            if url_accept {
                self.authorizer
                    .authorize(context.principal(), scope, Grant::NetworkEgress)
                    .map_err(ServiceError::Authentication)?;
            }
            let mut mcp_callback_request_digest = None;
            let mut mcp_callback_authority = None;
            let original_callback_command =
                matches!(command, Command::ResolveMcpCallback { .. }).then(|| command.clone());
            if let Command::ResolveMcpCallback {
                callback_id: _,
                kind,
                mode,
                challenge_generation,
                schema_digest,
                action,
                content,
                artifact_refs,
                ..
            } = &mut command
            {
                let idempotency_key = context
                    .idempotency_key()
                    .ok_or(ServiceError::MissingIdempotencyKey)?;
                let replay = self.store.replay_mcp_callback_resolution(
                    context.principal_id(),
                    idempotency_key,
                    original_callback_command
                        .as_ref()
                        .expect("callback command was captured"),
                )?;
                if let Some(replay) = replay {
                    return Ok(replay);
                }
                let request_bytes = serde_json::to_vec(
                    original_callback_command
                        .as_ref()
                        .expect("callback command was captured"),
                )
                .map_err(|error| ServiceError::Invalid(error.to_string()))?;
                let request_digest = crate::capabilities::kernel::identity::Digest::of(
                    crate::capabilities::kernel::identity::DigestAlgorithm::Sha256,
                    &request_bytes,
                );
                let mut request_digest_bytes = [0_u8; 32];
                request_digest_bytes.copy_from_slice(&request_digest.as_bytes());
                mcp_callback_request_digest = Some(request_digest_bytes);
                if !artifact_refs.is_empty() {
                    return Err(ServiceError::Invalid(
                        "callback artifact references are service-owned".to_owned(),
                    ));
                }
                let callback = preloaded_mcp_callback
                    .take()
                    .expect("callback authority was preloaded");
                if callback.principal_id != context.principal_id()
                    || callback.project_id != context.grant().project_id()
                    || callback.kind != *kind
                    || callback.mode != *mode
                    || callback.challenge_generation != *challenge_generation
                    || callback.schema_digest != *schema_digest
                {
                    return Err(ServiceError::Conflict(
                        "callback challenge authority does not match".to_owned(),
                    ));
                }
                let content_bytes = match (callback.mode, action) {
                    (
                        crate::domain::mcp_callback::McpCallbackMode::SamplingRequest
                        | crate::domain::mcp_callback::McpCallbackMode::SamplingResponse
                        | crate::domain::mcp_callback::McpCallbackMode::Url,
                        _,
                    ) => {
                        if content.is_some() {
                            return Err(ServiceError::Invalid(
                                "sampling approval resolution cannot include content".to_owned(),
                            ));
                        }
                        None
                    }
                    (_, McpCallbackAction::Accept) => {
                        let value = content.as_ref().ok_or_else(|| {
                            ServiceError::Invalid(
                                "accepted callback resolution requires content".to_owned(),
                            )
                        })?;
                        Some(validate_mcp_callback_content(
                            value,
                            &callback.schema,
                            callback.max_content_bytes,
                            self.runtime.mcp_callback_content_public(&callback, value),
                        )?)
                    }
                    (_, McpCallbackAction::Decline | McpCallbackAction::Cancel) => {
                        if content.is_some() {
                            return Err(ServiceError::Invalid(
                                "declined or cancelled callback resolution cannot include content"
                                    .to_owned(),
                            ));
                        }
                        None
                    }
                };
                if let Some(bytes) = content_bytes {
                    artifact_refs.push(self.runtime.with_mcp_callback_revision(
                        &callback.workspace_revision,
                        |_| {
                            self.runtime.store_mcp_callback_content(
                                callback.principal_id,
                                callback.project_id,
                                callback.run_id,
                                callback.id,
                                idempotency_key,
                                &bytes,
                                callback.artifact_expires_at.unix_micros(),
                            )
                        },
                    )?);
                    *content = None;
                }
                mcp_callback_authority = Some(*callback);
            }
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
            let workspace_revision = mcp_callback_authority
                .as_ref()
                .map(|callback| callback.workspace_revision.clone());
            let callback_recheck = |callback: &McpCallbackProjection| {
                callback.principal_id == context.principal_id()
                    && callback.project_id == context.grant().project_id()
                    && context.grant().grants().contains(&required_grant)
                    && (!url_accept || context.grant().grants().contains(&Grant::NetworkEgress))
                    && runtime.mcp_callback_revision_live(&callback.workspace_revision)
            };
            runtime.admit_command(principal_id, idempotency_key, &command)?;
            let mut commit = || {
                if let Some(callback) = mcp_callback_authority.as_ref() {
                    store.reserve_mcp_callback_resolution(
                        context.principal_id(),
                        callback.project_id,
                        idempotency_key,
                        original_callback_command
                            .as_ref()
                            .expect("callback command was captured"),
                    )?;
                }
                runtime.commit_verified(principal_id, project_id, &command, || {
                    store.execute(WriteRequest {
                        principal_id: context.principal_id(),
                        idempotency_key,
                        trace_id: context.trace_id(),
                        command: &command,
                        driver_claim: None,
                        mcp_callback_request_digest,
                        mcp_callback_authority: mcp_callback_authority.as_ref(),
                        mcp_callback_recheck: Some(&callback_recheck),
                        mcp_callback_workspace_revision: workspace_revision.as_deref(),
                    })
                })
            };
            let receipt = match if let Some(callback) = mcp_callback_authority.as_ref() {
                runtime.with_mcp_callback_revision(&callback.workspace_revision, |revision| {
                    let _ = revision;
                    commit()
                })
            } else {
                commit()
            } {
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
        PENDING_CALLBACK_ARTIFACT.with(|pending| pending.borrow_mut().take());
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

fn validate_mcp_callback_content(
    content: &serde_json::Value,
    schema: &serde_json::Value,
    maximum: usize,
    content_is_public: bool,
) -> Result<Vec<u8>, ServiceError> {
    let object = content.as_object().ok_or_else(|| {
        ServiceError::Invalid("callback content must be a JSON object".to_owned())
    })?;
    let allowed = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| ServiceError::Store("callback schema properties are invalid".to_owned()))?;
    if object.keys().any(|name| !allowed.contains_key(name)) {
        return Err(ServiceError::Invalid(
            "callback content contains a field outside the configured safe allowlist".to_owned(),
        ));
    }
    let bytes =
        serde_json::to_vec(content).map_err(|error| ServiceError::Invalid(error.to_string()))?;
    if bytes.len() > maximum {
        return Err(ServiceError::Invalid(
            "callback content exceeds the configured size limit".to_owned(),
        ));
    }
    if !content_is_public {
        return Err(ServiceError::Invalid(
            "callback content contains configured secret material".to_owned(),
        ));
    }
    let mut schema = schema.clone();
    schema
        .as_object_mut()
        .ok_or_else(|| ServiceError::Store("callback schema is not an object".to_owned()))?
        .insert(
            "additionalProperties".to_owned(),
            serde_json::Value::Bool(false),
        );
    if !jsonschema::validator_for(&schema)
        .map_err(|_| ServiceError::Store("callback schema is invalid".to_owned()))?
        .is_valid(content)
    {
        return Err(ServiceError::Invalid(
            "callback content does not match the pinned schema".to_owned(),
        ));
    }
    Ok(bytes)
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

#[cfg(test)]
mod mcp_callback_tests {
    use std::{
        collections::BTreeSet,
        fs,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::{
        api::auth::contract::ScopedAuthorizer,
        domain::{
            events::UtcDateTime,
            ids::{AttemptId, RunId, WorkspaceId},
            lifecycle::FencingToken,
            mcp_callback::{
                McpCallbackKind, McpCallbackMode, McpCallbackState, McpUndispatchedProof,
                McpUrlCallbackBinding, McpUrlDestination,
            },
        },
        runtime::daemon::ControlPlaneAuthority,
    };

    struct OrderingStore {
        callback: McpCallbackProjection,
        calls: Arc<Mutex<Vec<&'static str>>>,
        replay: bool,
    }

    impl ServiceStore for OrderingStore {
        fn command_scope(
            &mut self,
            _principal_id: PrincipalId,
            _command: &Command,
        ) -> Result<ResourceScope, ServiceError> {
            self.calls.lock().unwrap().push("scope");
            Ok(ResourceScope::new(
                self.callback.principal_id,
                self.callback.project_id,
            ))
        }

        fn query_scope(&mut self, _query: &Query) -> Result<ResourceScope, ServiceError> {
            unreachable!()
        }

        fn execute(&mut self, request: WriteRequest<'_>) -> Result<CommandReceipt, ServiceError> {
            assert!(request.mcp_callback_request_digest.is_some());
            self.calls.lock().unwrap().push("commit");
            Ok(CommandReceipt {
                operation: request.command.operation(),
                commit_positions: vec![
                    CommitPosition::new(if self.replay { 7 } else { 1 }).unwrap(),
                ],
                replayed: self.replay,
            })
        }

        fn replay_mcp_callback_resolution(
            &mut self,
            _principal_id: PrincipalId,
            _idempotency_key: &IdempotencyKey,
            _command: &Command,
        ) -> Result<Option<CommandReceipt>, ServiceError> {
            self.calls.lock().unwrap().push("replay");
            Ok(self.replay.then(|| CommandReceipt {
                operation: "mcp_callback.resolve",
                commit_positions: vec![CommitPosition::new(7).unwrap()],
                replayed: true,
            }))
        }

        fn reserve_mcp_callback_resolution(
            &mut self,
            _principal_id: PrincipalId,
            _project_id: ProjectId,
            _idempotency_key: &IdempotencyKey,
            _command: &Command,
        ) -> Result<Option<CommandReceipt>, ServiceError> {
            self.calls.lock().unwrap().push("reserve");
            Ok(None)
        }

        fn query(&mut self, _query: &Query) -> Result<QueryProjection, ServiceError> {
            self.calls.lock().unwrap().push("query");
            Ok(QueryProjection::McpCallback(Box::new(
                self.callback.clone(),
            )))
        }

        fn deletion_job(
            &mut self,
            _actor: DeletionActor,
            _id: DeletionJobId,
        ) -> Result<DeletionJob, DeletionError> {
            unreachable!()
        }

        fn deletion_job_for_request(
            &mut self,
            _actor: DeletionActor,
            _object_id: RetentionObjectId,
            _idempotency_key: &str,
        ) -> Result<DeletionJob, DeletionError> {
            unreachable!()
        }

        fn archive_status(
            &mut self,
            _actor: DeletionActor,
            _object_id: RetentionObjectId,
        ) -> Result<ArchiveStatus, DeletionError> {
            unreachable!()
        }

        fn store_time(&mut self) -> Result<StoreTimestamp, ServiceError> {
            unreachable!()
        }
    }

    struct OrderingRuntime {
        calls: Arc<Mutex<Vec<&'static str>>>,
        revision_live: bool,
    }

    impl Scheduler for OrderingRuntime {}
    impl CapabilityService for OrderingRuntime {}
    impl LeaseService for OrderingRuntime {}

    impl ArtifactService for OrderingRuntime {
        fn commit_verified<T>(
            &self,
            _principal_id: PrincipalId,
            _project_id: ProjectId,
            _command: &Command,
            commit: impl FnOnce() -> Result<T, ServiceError>,
        ) -> Result<T, ServiceError> {
            self.calls.lock().unwrap().push("stage");
            commit()
        }

        fn store_mcp_callback_content(
            &self,
            _principal_id: PrincipalId,
            _project_id: ProjectId,
            _run_id: RunId,
            _callback_id: McpCallbackId,
            _idempotency_key: &IdempotencyKey,
            _bytes: &[u8],
            _expires_at_unix_micros: i64,
        ) -> Result<McpCallbackArtifactRef, ServiceError> {
            self.calls.lock().unwrap().push("artifact");
            McpCallbackArtifactRef::parse(&format!("artifact-ref:{}", "0".repeat(64)))
                .map_err(|error| ServiceError::Store(error.to_string()))
        }

        fn mcp_callback_revision_live(&self, _revision: &str) -> bool {
            self.calls.lock().unwrap().push("drift");
            self.revision_live
        }

        fn mcp_callback_content_public(
            &self,
            _callback: &McpCallbackProjection,
            _content: &serde_json::Value,
        ) -> bool {
            true
        }
    }

    #[test]
    fn exact_replay_returns_before_content_recreation_or_stale_authority_work() {
        let callback = callback_fixture();
        let command = Command::ResolveMcpCallback {
            schema_version: crate::domain::events::SchemaVersion::CURRENT,
            callback_id: callback.id,
            kind: callback.kind,
            mode: callback.mode,
            expected_version: callback.version,
            challenge_generation: callback.challenge_generation,
            schema_digest: callback.schema_digest.clone(),
            action: McpCallbackAction::Accept,
            content: Some(serde_json::json!({"name":"Ada"})),
            artifact_refs: Vec::new(),
        };
        let key = IdempotencyKey::parse("resolve-order").unwrap();
        let context = RequestContext::authenticated(
            Ok(AuthenticatedPrincipal::from_grants(GrantSnapshot::new(
                callback.principal_id,
                callback.project_id,
                [Grant::ModelCall],
            ))),
            Some(key),
            TraceId::parse("callback-order").unwrap(),
        )
        .unwrap();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut service = Service::with_runtime(
            OrderingStore {
                callback: callback.clone(),
                calls: Arc::clone(&calls),
                replay: false,
            },
            ScopedAuthorizer,
            OrderingRuntime {
                calls: Arc::clone(&calls),
                revision_live: true,
            },
            &ControlPlaneAuthority::for_test(),
        );
        service.execute(&context, command.clone()).unwrap();
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [
                "scope", "query", "replay", "drift", "artifact", "drift", "reserve", "stage",
                "commit"
            ]
        );

        calls.lock().unwrap().clear();
        let mut service = Service::with_runtime(
            OrderingStore {
                callback,
                calls: Arc::clone(&calls),
                replay: true,
            },
            ScopedAuthorizer,
            OrderingRuntime {
                calls: Arc::clone(&calls),
                revision_live: false,
            },
            &ControlPlaneAuthority::for_test(),
        );
        assert!(service.execute(&context, command).unwrap().replayed);
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["scope", "query", "replay"]
        );
    }

    #[test]
    fn url_resolution_keeps_original_authority_and_adds_egress_only_for_accept() {
        let callback = url_callback_fixture();
        let command = |callback: &McpCallbackProjection, action| Command::ResolveMcpCallback {
            schema_version: crate::domain::events::SchemaVersion::CURRENT,
            callback_id: callback.id,
            kind: callback.kind,
            mode: callback.mode,
            expected_version: callback.version,
            challenge_generation: callback.challenge_generation,
            schema_digest: callback.schema_digest.clone(),
            action,
            content: None,
            artifact_refs: Vec::new(),
        };
        let execute = |callback: &McpCallbackProjection, grants, action| {
            let context = RequestContext::authenticated(
                Ok(AuthenticatedPrincipal::from_grants(GrantSnapshot::new(
                    callback.principal_id,
                    callback.project_id,
                    grants,
                ))),
                Some(IdempotencyKey::parse(&format!("url-authority-{action:?}")).unwrap()),
                TraceId::parse("url-authority").unwrap(),
            )
            .unwrap();
            Service::with_runtime(
                OrderingStore {
                    callback: callback.clone(),
                    calls: Arc::new(Mutex::new(Vec::new())),
                    replay: false,
                },
                ScopedAuthorizer,
                OrderingRuntime {
                    calls: Arc::new(Mutex::new(Vec::new())),
                    revision_live: true,
                },
                &ControlPlaneAuthority::for_test(),
            )
            .execute(&context, command(callback, action))
        };

        assert!(matches!(
            execute(&callback, [Grant::NetworkEgress], McpCallbackAction::Accept),
            Err(ServiceError::Authentication(_))
        ));
        assert!(matches!(
            execute(&callback, [Grant::ModelCall], McpCallbackAction::Accept),
            Err(ServiceError::Authentication(_))
        ));
        assert!(execute(&callback, [Grant::ModelCall], McpCallbackAction::Decline).is_ok());

        let mut invocation_callback = callback;
        invocation_callback
            .url_binding
            .as_mut()
            .unwrap()
            .original_effect = crate::capabilities::kernel::grant::EffectClass::WorkspaceRead;
        assert!(
            execute(
                &invocation_callback,
                [Grant::WorkspaceRead],
                McpCallbackAction::Decline,
            )
            .is_ok()
        );
        assert!(matches!(
            execute(
                &invocation_callback,
                [Grant::ModelCall],
                McpCallbackAction::Decline,
            ),
            Err(ServiceError::Authentication(_))
        ));
    }

    fn url_callback_fixture() -> McpCallbackProjection {
        let mut callback = callback_fixture();
        let url = "https://auth.example.com/complete";
        let response_digest = format!("sha256:{}", "2".repeat(64));
        callback.mode = McpCallbackMode::Url;
        callback.request_id = "invocation".to_owned();
        callback.request = serde_json::json!({
            "error_code": -32042,
            "error_response_digest": response_digest,
            "message": "authenticate",
            "url": url,
            "elicitation_id": "challenge"
        });
        callback.schema = serde_json::json!({});
        callback.max_content_bytes = 1;
        callback.request_digest = crate::capabilities::kernel::identity::Digest::of(
            crate::capabilities::kernel::identity::DigestAlgorithm::Sha256,
            &serde_json::to_vec(&callback.request).unwrap(),
        )
        .to_string();
        callback.schema_digest = callback.request_digest.clone();
        callback.url_binding = Some(McpUrlCallbackBinding {
            invocation_id: callback.request_id.clone(),
            idempotency_digest: format!("sha256:{}", "3".repeat(64)),
            server_id: callback.server_id.clone(),
            generation: callback.challenge_generation,
            operation: "tools/call".to_owned(),
            invocation_request_digest: format!("sha256:{}", "4".repeat(64)),
            error_response_digest: response_digest.clone(),
            url_digest: crate::capabilities::kernel::identity::Digest::of(
                crate::capabilities::kernel::identity::DigestAlgorithm::Sha256,
                url.as_bytes(),
            )
            .to_string(),
            original_effect: crate::capabilities::kernel::grant::EffectClass::ModelCall,
            grant_digest: format!("sha256:{}", "5".repeat(64)),
            accept_destination: McpUrlDestination::from_url(url).unwrap(),
            retry_safety: "idempotent".to_owned(),
            undispatched_proof: McpUndispatchedProof::from_terminal_url_elicitation(
                response_digest,
            ),
        });
        callback.validate().unwrap();
        callback
    }

    fn callback_fixture() -> McpCallbackProjection {
        McpCallbackProjection {
            id: McpCallbackId::from_stable_bytes(b"callback"),
            server_id: "server".to_owned(),
            kind: McpCallbackKind::Elicitation,
            mode: McpCallbackMode::Form,
            principal_id: PrincipalId::from_stable_bytes(b"principal"),
            project_id: ProjectId::from_stable_bytes(b"project"),
            run_id: RunId::from_stable_bytes(b"run"),
            attempt_id: AttemptId::from_stable_bytes(b"attempt"),
            fence: FencingToken::new(1),
            claim_generation: 1,
            workspace_id: WorkspaceId::from_stable_bytes(b"workspace"),
            workspace_revision: "revision".to_owned(),
            request_id: "1".to_owned(),
            request: serde_json::json!({"message":"Name"}),
            schema: serde_json::json!({
                "type":"object",
                "properties":{"name":{"type":"string"}},
                "required":["name"]
            }),
            request_digest: format!("sha256:{}", "0".repeat(64)),
            schema_digest: format!("sha256:{}", "1".repeat(64)),
            challenge_generation: 1,
            operation_sequence: 1,
            expires_at: UtcDateTime::parse("2099-01-01T00:00:00Z").unwrap(),
            artifact_expires_at: UtcDateTime::parse("2100-01-01T00:00:00Z").unwrap(),
            max_response_bytes: 1024,
            max_content_bytes: 900,
            secret_policy_id: "authorized-secrets-v1".to_owned(),
            url_binding: None,
            state: McpCallbackState::AwaitingResolution,
            version: 2,
            resolver_actor: None,
            action: None,
            artifact_refs: Vec::new(),
            terminal_error: None,
        }
    }

    #[test]
    fn callback_content_is_schema_bounded_secret_free_and_owned() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "required": ["name"]
        });
        let valid = serde_json::json!({"name": "Ada"});
        let bytes = validate_mcp_callback_content(&valid, &schema, 64, true).unwrap();
        assert!(
            validate_mcp_callback_content(
                &serde_json::json!({"name": "Ada", "extra": true}),
                &schema,
                64,
                true
            )
            .is_err()
        );
        assert!(
            validate_mcp_callback_content(&serde_json::json!({"name": 1}), &schema, 64, true)
                .is_err()
        );
        assert!(
            validate_mcp_callback_content(
                &serde_json::json!({"name": "Ada", "password": "no"}),
                &schema,
                64,
                true
            )
            .is_err()
        );
        assert!(validate_mcp_callback_content(&valid, &schema, 4, true).is_err());
        for secret in ["Ada", "QWRh"] {
            let content = serde_json::json!({"name": secret});
            assert!(validate_mcp_callback_content(&content, &schema, 64, false).is_err());
        }
        let fragmented_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "first": {"type": "string"},
                "second": {"type": "string"}
            },
            "required": ["first", "second"]
        });
        assert!(
            validate_mcp_callback_content(
                &serde_json::json!({"first": "QW", "second": "Rh"}),
                &fragmented_schema,
                64,
                false,
            )
            .is_err()
        );

        let root = std::env::temp_dir().join(format!(
            "kit-mcp-callback-artifact-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&root);
        let artifacts = ArtifactStore::open(&root).unwrap();
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let run = RunId::generate().unwrap();
        let callback = McpCallbackId::generate().unwrap();
        let key = IdempotencyKey::parse("callback-artifact").unwrap();
        let expires_at = 4_102_444_800_000_000_i64;
        let reference = artifacts
            .store_mcp_callback_content(principal, project, run, callback, &key, &bytes, expires_at)
            .unwrap();
        assert_eq!(
            artifacts
                .store_mcp_callback_content(
                    principal, project, run, callback, &key, &bytes, expires_at,
                )
                .unwrap()
                .as_str(),
            reference.as_str()
        );
        assert!(
            artifacts
                .store_mcp_callback_content(
                    PrincipalId::generate().unwrap(),
                    project,
                    run,
                    callback,
                    &key,
                    &bytes,
                    expires_at,
                )
                .is_err()
        );
        assert!(ArtifactReference::parse(reference.as_str()).is_ok());
        PENDING_CALLBACK_ARTIFACT.with(|pending| pending.borrow_mut().take());
        let other = artifacts
            .store_mcp_callback_content(
                PrincipalId::generate().unwrap(),
                project,
                run,
                callback,
                &key,
                &bytes,
                expires_at,
            )
            .unwrap();
        assert_ne!(reference, other);
        PENDING_CALLBACK_ARTIFACT.with(|pending| pending.borrow_mut().take());

        let principal = PrincipalId::generate().unwrap();
        let reference = artifacts
            .store_mcp_callback_content(principal, project, run, callback, &key, &bytes, expires_at)
            .unwrap();
        let command = Command::ResolveMcpCallback {
            schema_version: crate::domain::events::SchemaVersion::CURRENT,
            callback_id: callback,
            kind: McpCallbackKind::Elicitation,
            mode: McpCallbackMode::Form,
            expected_version: 2,
            challenge_generation: 1,
            schema_digest: format!("sha256:{}", "0".repeat(64)),
            action: McpCallbackAction::Accept,
            content: None,
            artifact_refs: vec![reference.clone()],
        };
        assert!(
            artifacts
                .commit_verified(principal, project, &command, || {
                    Err::<(), _>(ServiceError::Conflict("commit fault".to_owned()))
                })
                .is_err()
        );
        assert!(
            artifacts
                .open_reference(ArtifactReference::parse(reference.as_str()).unwrap())
                .is_err()
        );
        assert_eq!(fs::read_dir(root.join("staging")).unwrap().count(), 0);

        let repaired = artifacts
            .store_mcp_callback_content(principal, project, run, callback, &key, &bytes, expires_at)
            .unwrap();
        artifacts
            .commit_verified(principal, project, &command, || Ok(()))
            .unwrap();
        let staged = artifacts
            .staged_publication(ArtifactReference::parse(reference.as_str()).unwrap())
            .unwrap();
        artifacts.promote_publication(&staged).unwrap();
        assert_eq!(repaired, reference);
        let artifact = artifacts
            .open_reference(ArtifactReference::parse(reference.as_str()).unwrap())
            .unwrap();
        assert_eq!(
            artifact.manifest().retention,
            ArtifactRetention::UntilUnixMicros(expires_at)
        );
        let digest = artifact.digest();
        artifacts
            .collect_garbage(&crate::store::artifacts::Reachability {
                now_unix_micros: expires_at.saturating_add(1),
                retained: BTreeSet::from([digest]),
                ..crate::store::artifacts::Reachability::default()
            })
            .unwrap();
        assert!(artifacts.open_bytes(digest).is_ok());
        artifacts
            .collect_garbage(&crate::store::artifacts::Reachability {
                now_unix_micros: expires_at.saturating_add(1),
                ..crate::store::artifacts::Reachability::default()
            })
            .unwrap();
        assert!(artifacts.open_bytes(digest).is_err());
        artifacts
            .store_mcp_callback_content(principal, project, run, callback, &key, &bytes, expires_at)
            .unwrap();
        artifacts
            .commit_verified(principal, project, &command, || Ok(()))
            .unwrap();
        let staged = artifacts
            .staged_publication(ArtifactReference::parse(reference.as_str()).unwrap())
            .unwrap();
        artifacts.promote_publication(&staged).unwrap();
        assert!(
            artifacts
                .open_reference(ArtifactReference::parse(reference.as_str()).unwrap())
                .is_ok()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
