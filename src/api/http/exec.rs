use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    fmt::Write as _,
    io::Read,
    path::PathBuf,
    process::Command as ProcessCommand,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Extension, Json, Router,
    body::to_bytes,
    extract::{Path, Request},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    api::{
        auth::contract::AuthenticatedPrincipal,
        http::{
            core::{JSON_BODY_LIMIT, RouteDescriptor},
            errors::ProblemDetails,
        },
    },
    domain::{
        config::Grant,
        ids::{DaemonServiceId, PrincipalId, ProcessId, ProjectId, TerminalId},
        lifecycle::{AttemptOwnership, ProcessClaim, ProcessOwnership},
    },
    executor::{
        cancel::{
            CancellationError, CancellationStoreError, ExecutorCancellationCoordinator,
            ExecutorCancellationOutcome,
        },
        process::own::{
            ProcessRecord, ProcessRegistrationContext, ProcessRegistry, ProcessState,
            ProcessTerminalConfig,
        },
        terminal::{
            OutputRead, OutputRetention, PtyDriver, ResizeRead, TerminalAllocation,
            TerminalAttachment, TerminalControl, TerminalError, TerminalLifecycle, TerminalManager,
            TerminalRequest, TerminalSize, TerminalSnapshotStore, TerminalTransport,
        },
    },
    store::sqlite::idempotency::IdempotencyKey,
};

const SCHEMA_VERSION: u16 = 1;
const HIDDEN_INSTANCE: &str = "/v1/executor";
const MAX_EVENTS: usize = 4_096;
const MAX_IDEMPOTENCY_RECORDS: usize = 4_096;
const MAX_INPUT_BYTES: usize = 16 * 1024;

pub const EXECUTOR_IDEMPOTENCY_RETRY_WINDOW_MILLIS: u64 = 24 * 60 * 60 * 1_000;

pub const EXEC_ROUTES: &[ExecRouteDescriptor] = &[
    route(
        "GET",
        "/v1/projects/{project_id}/processes",
        "process.list",
        false,
    ),
    route("GET", "/v1/processes/{process_id}", "process.get", false),
    route(
        "POST",
        "/v1/processes/{process_id}/cancel",
        "process.cancel",
        true,
    ),
    route(
        "POST",
        "/v1/processes/{process_id}/terminals",
        "terminal.allocate",
        true,
    ),
    route("GET", "/v1/terminals/{terminal_id}", "terminal.get", false),
    route(
        "POST",
        "/v1/terminals/{terminal_id}/attachments",
        "terminal.viewer.attach",
        true,
    ),
    route(
        "POST",
        "/v1/terminals/{terminal_id}/writer-claims",
        "terminal.writer.claim",
        true,
    ),
    route(
        "GET",
        "/v1/terminal-attachments/{attachment_id}",
        "terminal.attachment.get",
        false,
    ),
    route(
        "POST",
        "/v1/terminal-attachments/{attachment_id}/renew",
        "terminal.writer.renew",
        true,
    ),
    route(
        "POST",
        "/v1/terminal-attachments/{attachment_id}/release",
        "terminal.writer.release",
        true,
    ),
    route(
        "POST",
        "/v1/terminal-attachments/{attachment_id}/input",
        "terminal.input",
        true,
    ),
    route(
        "POST",
        "/v1/terminal-attachments/{attachment_id}/input-resolution",
        "terminal.input.resolve",
        true,
    ),
    route(
        "POST",
        "/v1/terminal-attachments/{attachment_id}/resize",
        "terminal.resize",
        true,
    ),
    route(
        "GET",
        "/v1/terminal-attachments/{attachment_id}/output",
        "terminal.output",
        false,
    ),
    route(
        "GET",
        "/v1/terminal-attachments/{attachment_id}/resizes",
        "terminal.resizes",
        false,
    ),
    route(
        "POST",
        "/v1/terminal-attachments/{attachment_id}/detach",
        "terminal.detach",
        true,
    ),
    route(
        "GET",
        "/v1/projects/{project_id}/executor/events",
        "executor.events",
        false,
    ),
];

pub type ExecRouteDescriptor = RouteDescriptor;

const fn route(
    method: &'static str,
    path: &'static str,
    operation: &'static str,
    mutation: bool,
) -> ExecRouteDescriptor {
    ExecRouteDescriptor {
        method,
        path,
        operation,
        mutation,
        long_running: false,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProcessResource {
    pub schema_version: u16,
    pub process_id: ProcessId,
    pub project_id: ProjectId,
    pub owner_kind: ProcessOwnerKind,
    pub execution_id: Option<u32>,
    pub state: ProcessResourceState,
    pub terminal_transport: TerminalTransport,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOwnerKind {
    Attempt,
    DaemonService,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProcessResourceState {
    Prepared,
    Started,
    OutcomeUnknown,
    Exited {
        success: bool,
        code: Option<i32>,
        signal: Option<i32>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TerminalResource {
    pub schema_version: u16,
    pub terminal_id: TerminalId,
    pub process_id: ProcessId,
    pub project_id: ProjectId,
    pub lifecycle: TerminalLifecycle,
    pub columns: u16,
    pub rows: u16,
    pub next_output_cursor: String,
    pub next_resize_cursor: String,
    pub retained_output_bytes: usize,
    pub writer_epoch: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AttachmentResource {
    pub schema_version: u16,
    pub attachment_id: String,
    pub terminal_id: TerminalId,
    pub role: &'static str,
    pub writer_epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_millis: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MutationReceipt {
    pub schema_version: u16,
    pub operation: &'static str,
    pub changed: bool,
    pub replayed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AllocateTerminalBody {
    pub columns: u16,
    pub rows: u16,
    pub max_output_bytes: usize,
    pub max_output_age_millis: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyBody {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WriterLeaseBody {
    pub lease_millis: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalInputBody {
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalInputResolution {
    Applied,
    NotApplied,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalInputResolutionBody {
    pub outcome: TerminalInputResolution,
}

impl std::fmt::Debug for TerminalInputBody {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalInputBody")
            .field("bytes", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalResizeBody {
    pub columns: u16,
    pub rows: u16,
}

#[derive(Clone, Debug)]
pub struct ProcessRegistration {
    pub project_id: ProjectId,
    pub claim: ProcessClaim,
    pub execution_id: Option<u32>,
    pub state: ProcessResourceState,
    pub terminal_request: TerminalRequest,
    pub boundary_id: String,
}

impl ProcessRegistration {
    pub fn from_record(
        project_id: ProjectId,
        record: &ProcessRecord,
        terminal_request: TerminalRequest,
        boundary_id: impl Into<String>,
    ) -> Self {
        Self {
            project_id,
            claim: ProcessClaim::new(record.process_id(), record.owner()),
            execution_id: Some(record.execution_id()),
            state: match record.state() {
                ProcessState::Started => ProcessResourceState::Started,
                ProcessState::Exited {
                    success,
                    code,
                    signal,
                } => ProcessResourceState::Exited {
                    success,
                    code,
                    signal,
                },
            },
            terminal_request,
            boundary_id: boundary_id.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecError {
    NotFound,
    Invalid(&'static str),
    Conflict(&'static str),
    Unavailable,
    PlatformUnavailable,
    Unsupported,
    OutcomeUnknown,
    CursorExpired(Value),
    Internal,
}

pub trait DaemonProcessController: Send + Sync + 'static {
    fn cancel(
        &self,
        service_id: DaemonServiceId,
        process_id: ProcessId,
        boundary_id: &str,
    ) -> Result<ExecutorCancellationOutcome, CancellationError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DaemonServiceScope {
    pub principal_id: PrincipalId,
    pub project_id: ProjectId,
    pub service_id: DaemonServiceId,
}

pub trait ExecService: Send + Sync + 'static {
    fn list_processes(
        &self,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
    ) -> Result<Value, ExecError>;
    fn get_process(
        &self,
        authenticated: &AuthenticatedPrincipal,
        process_id: ProcessId,
    ) -> Result<Value, ExecError>;
    fn cancel_process(
        &self,
        authenticated: &AuthenticatedPrincipal,
        process_id: ProcessId,
        key: &IdempotencyKey,
    ) -> Result<Value, ExecError>;
    fn allocate_terminal(
        &self,
        authenticated: &AuthenticatedPrincipal,
        process_id: ProcessId,
        key: &IdempotencyKey,
        body: AllocateTerminalBody,
    ) -> Result<Value, ExecError>;
    fn get_terminal(
        &self,
        authenticated: &AuthenticatedPrincipal,
        terminal_id: TerminalId,
    ) -> Result<Value, ExecError>;
    fn attach_viewer(
        &self,
        authenticated: &AuthenticatedPrincipal,
        terminal_id: TerminalId,
        key: &IdempotencyKey,
    ) -> Result<Value, ExecError>;
    fn claim_writer(
        &self,
        authenticated: &AuthenticatedPrincipal,
        terminal_id: TerminalId,
        key: &IdempotencyKey,
        body: WriterLeaseBody,
    ) -> Result<Value, ExecError>;
    fn get_attachment(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
    ) -> Result<Value, ExecError>;
    fn renew_writer(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
        body: WriterLeaseBody,
    ) -> Result<Value, ExecError>;
    fn release_writer(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
    ) -> Result<Value, ExecError>;
    fn write_input(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
        bytes: &[u8],
    ) -> Result<Value, ExecError>;
    fn resolve_input(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
        body: TerminalInputResolutionBody,
    ) -> Result<Value, ExecError>;
    fn resize(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
        body: TerminalResizeBody,
    ) -> Result<Value, ExecError>;
    fn read_output(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        cursor: u64,
    ) -> Result<Value, ExecError>;
    fn read_resizes(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        cursor: u64,
    ) -> Result<Value, ExecError>;
    fn detach(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
    ) -> Result<Value, ExecError>;
    fn events(
        &self,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
        cursor: u64,
    ) -> Result<Value, ExecError>;
}

struct ProcessEntry {
    registration: ProcessRegistration,
    principal_id: PrincipalId,
    cancellation: ProcessCancellation,
}

enum ProcessCancellation {
    Attempt(AttemptOwnership),
    DaemonService {
        service_id: DaemonServiceId,
        controller: Option<Arc<dyn DaemonProcessController>>,
    },
}

struct TerminalEntry {
    project_id: ProjectId,
    principal_id: PrincipalId,
    process_id: ProcessId,
    control: TerminalControl,
}

struct AttachmentEntry {
    project_id: ProjectId,
    principal_id: PrincipalId,
    attachment: TerminalAttachment,
    expires_at_millis: Option<u64>,
}

#[derive(Clone)]
struct IdempotentResult {
    digest: [u8; 32],
    response: Value,
}

#[derive(Clone, Serialize)]
struct ExecEvent {
    schema_version: u16,
    cursor: String,
    event_type: String,
    #[serde(skip)]
    principal_id: PrincipalId,
    project_id: ProjectId,
    resource_id: String,
}

#[derive(Default)]
struct AdapterState {
    processes: HashMap<ProcessId, ProcessEntry>,
    terminals: HashMap<TerminalId, TerminalEntry>,
    attachments: HashMap<String, AttachmentEntry>,
    idempotency: BTreeMap<(PrincipalId, ProjectId, String, String, String), IdempotentResult>,
    events: VecDeque<(u64, ExecEvent)>,
    next_events: BTreeMap<(PrincipalId, ProjectId), u64>,
}

#[derive(Clone)]
struct SqliteExecJournal {
    database: PathBuf,
}

enum DurableBegin {
    New,
    Pending,
    Complete(Value),
    Failed(ExecError),
    OutcomeUnknown,
    Tombstone,
}

enum ReconciledMutation {
    Complete(Value, Option<(&'static str, ProjectId, String)>),
    Failed(ExecError),
    OutcomeUnknown,
}

impl SqliteExecJournal {
    fn open(database: PathBuf) -> Result<Self, ExecError> {
        let journal = Self { database };
        journal
            .connection()?
            .execute(
                "UPDATE executor_api_attachments SET invalidated=1, expires_at_millis=NULL",
                [],
            )
            .map_err(|_| ExecError::Unavailable)?;
        Ok(journal)
    }

    fn connection(&self) -> Result<Connection, ExecError> {
        let connection = Connection::open(&self.database).map_err(|_| ExecError::Unavailable)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys=ON;
                 CREATE TABLE IF NOT EXISTS executor_api_processes (
                   process_id TEXT PRIMARY KEY,
                   project_id TEXT NOT NULL,
                   principal_id TEXT NOT NULL,
                   owner TEXT NOT NULL,
                   execution_id INTEGER,
                   state TEXT NOT NULL,
                   terminal_transport TEXT NOT NULL,
                   boundary_id TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS executor_api_idempotency (
                   principal_id TEXT NOT NULL,
                   project_id TEXT NOT NULL,
                   operation TEXT NOT NULL,
                   resource TEXT NOT NULL,
                   idempotency_key TEXT NOT NULL,
                   digest BLOB NOT NULL,
                   status TEXT NOT NULL CHECK(status IN ('pending','complete','failed','outcome_unknown','tombstone')),
                   response TEXT,
                   updated_millis INTEGER NOT NULL,
                   PRIMARY KEY(principal_id, project_id, operation, resource, idempotency_key)
                 );
                 CREATE TABLE IF NOT EXISTS executor_api_events (
                   principal_id TEXT NOT NULL,
                   project_id TEXT NOT NULL,
                   position INTEGER NOT NULL,
                   event TEXT NOT NULL,
                   PRIMARY KEY(principal_id, project_id, position)
                 );
                 CREATE TABLE IF NOT EXISTS executor_api_event_sequences (
                   principal_id TEXT NOT NULL,
                   project_id TEXT NOT NULL,
                   next_position INTEGER NOT NULL CHECK(next_position >= 0),
                   PRIMARY KEY(principal_id, project_id)
                 );
                 CREATE TABLE IF NOT EXISTS executor_api_attachments (
                   attachment_id TEXT PRIMARY KEY,
                   terminal_id TEXT NOT NULL,
                   project_id TEXT NOT NULL,
                   principal_id TEXT NOT NULL,
                   role TEXT NOT NULL,
                   writer_epoch INTEGER,
                   expires_at_millis INTEGER,
                   invalidated INTEGER NOT NULL DEFAULT 0
                 );",
            )
            .map_err(|_| ExecError::Unavailable)?;
        Ok(connection)
    }

    fn load(&self) -> Result<AdapterState, ExecError> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ExecError::Unavailable)?;
        transaction
            .execute(
                "UPDATE executor_api_processes SET state=?1 WHERE state IN (?2, ?3)",
                params![
                    serde_json::to_string(&ProcessResourceState::OutcomeUnknown)
                        .map_err(|_| ExecError::Internal)?,
                    serde_json::to_string(&ProcessResourceState::Prepared)
                        .map_err(|_| ExecError::Internal)?,
                    serde_json::to_string(&ProcessResourceState::Started)
                        .map_err(|_| ExecError::Internal)?,
                ],
            )
            .map_err(|_| ExecError::Unavailable)?;
        let mut state = AdapterState::default();
        {
            let mut statement = transaction
                .prepare("SELECT process_id, project_id, principal_id, owner, execution_id, state, terminal_transport, boundary_id FROM executor_api_processes")
                .map_err(|_| ExecError::Unavailable)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<u32>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                })
                .map_err(|_| ExecError::Unavailable)?;
            for row in rows {
                let (
                    process,
                    project,
                    principal,
                    owner,
                    execution,
                    process_state,
                    transport,
                    boundary,
                ) = row.map_err(|_| ExecError::Unavailable)?;
                let process_id = ProcessId::parse(&process).map_err(|_| ExecError::Internal)?;
                let project_id = ProjectId::parse(&project).map_err(|_| ExecError::Internal)?;
                let principal_id =
                    PrincipalId::parse(&principal).map_err(|_| ExecError::Internal)?;
                let owner: ProcessOwnership =
                    serde_json::from_str(&owner).map_err(|_| ExecError::Internal)?;
                let process_state =
                    serde_json::from_str(&process_state).map_err(|_| ExecError::Internal)?;
                let terminal_request = if transport == "pty" {
                    TerminalRequest::pty(
                        crate::telemetry::redact::CapturePersistencePolicy::no_secrets(),
                    )
                } else {
                    TerminalRequest::default()
                };
                let cancellation = match owner {
                    ProcessOwnership::Attempt(owner) => ProcessCancellation::Attempt(owner),
                    ProcessOwnership::DaemonService(service_id) => {
                        ProcessCancellation::DaemonService {
                            service_id,
                            controller: None,
                        }
                    }
                };
                state.processes.insert(
                    process_id,
                    ProcessEntry {
                        registration: ProcessRegistration {
                            project_id,
                            claim: ProcessClaim::new(process_id, owner),
                            execution_id: execution,
                            state: process_state,
                            terminal_request,
                            boundary_id: boundary,
                        },
                        principal_id,
                        cancellation,
                    },
                );
            }
        }
        {
            let mut statement = transaction
                .prepare("SELECT principal_id, project_id, position, event FROM executor_api_events ORDER BY rowid")
                .map_err(|_| ExecError::Unavailable)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|_| ExecError::Unavailable)?;
            for row in rows {
                let (principal, project, position, event) =
                    row.map_err(|_| ExecError::Unavailable)?;
                let principal = PrincipalId::parse(&principal).map_err(|_| ExecError::Internal)?;
                let project = ProjectId::parse(&project).map_err(|_| ExecError::Internal)?;
                #[derive(Deserialize)]
                struct PersistedEvent {
                    schema_version: u16,
                    cursor: String,
                    event_type: String,
                    project_id: ProjectId,
                    resource_id: String,
                }
                let event: PersistedEvent =
                    serde_json::from_str(&event).map_err(|_| ExecError::Internal)?;
                let event = ExecEvent {
                    schema_version: event.schema_version,
                    cursor: event.cursor,
                    event_type: event.event_type,
                    principal_id: principal,
                    project_id: event.project_id,
                    resource_id: event.resource_id,
                };
                state.events.push_back((position, event));
                state
                    .next_events
                    .insert((principal, project), position.saturating_add(1));
            }
        }
        {
            let mut statement = transaction
                .prepare("SELECT principal_id, project_id, next_position FROM executor_api_event_sequences")
                .map_err(|_| ExecError::Unavailable)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                })
                .map_err(|_| ExecError::Unavailable)?;
            for row in rows {
                let (principal, project, next) = row.map_err(|_| ExecError::Unavailable)?;
                state.next_events.insert(
                    (
                        PrincipalId::parse(&principal).map_err(|_| ExecError::Internal)?,
                        ProjectId::parse(&project).map_err(|_| ExecError::Internal)?,
                    ),
                    next,
                );
            }
        }
        transaction.commit().map_err(|_| ExecError::Unavailable)?;
        Ok(state)
    }

    fn save_process(&self, entry: &ProcessEntry) -> Result<(), ExecError> {
        let owner = serde_json::to_string(&entry.registration.claim.owner)
            .map_err(|_| ExecError::Internal)?;
        let process_state =
            serde_json::to_string(&entry.registration.state).map_err(|_| ExecError::Internal)?;
        let changed = self.connection()?
            .execute(
                "INSERT INTO executor_api_processes
                 (process_id, project_id, principal_id, owner, execution_id, state, terminal_transport, boundary_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(process_id) DO UPDATE SET execution_id=excluded.execution_id,
                   state=excluded.state, boundary_id=excluded.boundary_id
                 WHERE project_id=excluded.project_id AND principal_id=excluded.principal_id AND owner=excluded.owner",
                params![
                    entry.registration.claim.process_id.to_string(),
                    entry.registration.project_id.to_string(),
                    entry.principal_id.to_string(),
                    owner,
                    entry.registration.execution_id,
                    process_state,
                    match entry.registration.terminal_request.transport {
                        crate::executor::terminal::TerminalTransport::Pipes => "pipes",
                        crate::executor::terminal::TerminalTransport::Pty => "pty",
                    },
                    entry.registration.boundary_id,
                ],
            )
            .map_err(|_| ExecError::Unavailable)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(ExecError::Conflict("process registration conflicts"))
        }
    }

    fn save_attachment(&self, id: &str, entry: &AttachmentEntry) -> Result<(), ExecError> {
        self.connection()?
            .execute(
                "INSERT INTO executor_api_attachments
                 (attachment_id, terminal_id, project_id, principal_id, role, writer_epoch, expires_at_millis, invalidated)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)
                 ON CONFLICT(attachment_id) DO UPDATE SET role=excluded.role,
                   writer_epoch=excluded.writer_epoch, expires_at_millis=excluded.expires_at_millis,
                   invalidated=0",
                params![
                    id,
                    entry.attachment.terminal_id().to_string(),
                    entry.project_id.to_string(),
                    entry.principal_id.to_string(),
                    if entry.attachment.is_writer() { "writer" } else { "viewer" },
                    entry.attachment.writer_epoch(),
                    entry.expires_at_millis,
                ],
            )
            .map_err(|_| ExecError::Unavailable)?;
        Ok(())
    }

    fn invalidate_attachment(&self, id: &str) -> Result<(), ExecError> {
        self.connection()?
            .execute(
                "UPDATE executor_api_attachments SET invalidated=1, expires_at_millis=NULL WHERE attachment_id=?1",
                [id],
            )
            .map_err(|_| ExecError::Unavailable)?;
        Ok(())
    }

    fn authorize_attachment(
        &self,
        authenticated: &AuthenticatedPrincipal,
        id: &str,
        role_aware: bool,
    ) -> Result<(), ExecError> {
        let row = self
            .connection()?
            .query_row(
                "SELECT principal_id, project_id, role FROM executor_api_attachments
                 WHERE attachment_id=?1 AND invalidated=0",
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
            .map_err(|_| ExecError::Unavailable)?
            .ok_or(ExecError::NotFound)?;
        let snapshot = authenticated.grant_snapshot();
        let required = if role_aware && row.2 == "viewer" {
            Grant::WorkspaceRead
        } else {
            Grant::ProcessSpawn
        };
        if row.0 == snapshot.principal_id().to_string()
            && row.1 == snapshot.project_id().to_string()
            && snapshot.grants().contains(&required)
        {
            Ok(())
        } else {
            Err(ExecError::NotFound)
        }
    }

    fn authorize_idempotency_record(
        &self,
        scope: &(PrincipalId, ProjectId, String, String, String),
    ) -> Result<(), ExecError> {
        self.connection()?
            .query_row(
                "SELECT 1 FROM executor_api_idempotency
                 WHERE principal_id=?1 AND project_id=?2 AND operation=?3 AND resource=?4
                   AND idempotency_key=?5",
                params![
                    scope.0.to_string(),
                    scope.1.to_string(),
                    scope.2,
                    scope.3,
                    scope.4
                ],
                |_| Ok(()),
            )
            .optional()
            .map_err(|_| ExecError::Unavailable)?
            .ok_or(ExecError::NotFound)
    }

    fn begin(
        &self,
        scope: &(PrincipalId, ProjectId, String, String, String),
        digest: [u8; 32],
    ) -> Result<DurableBegin, ExecError> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ExecError::Unavailable)?;
        prune_idempotency(&transaction)?;
        let existing = transaction
            .query_row(
                "SELECT digest, status, response FROM executor_api_idempotency
                 WHERE principal_id=?1 AND project_id=?2 AND operation=?3 AND resource=?4 AND idempotency_key=?5",
                params![scope.0.to_string(), scope.1.to_string(), scope.2, scope.3, scope.4],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?)),
            )
            .optional()
            .map_err(|_| ExecError::Unavailable)?;
        let result = match existing {
            Some((stored, _, _)) if stored.as_slice() != digest => {
                return Err(ExecError::Conflict("idempotency key was reused"));
            }
            Some((_, status, response)) if status == "complete" => DurableBegin::Complete(
                serde_json::from_str(response.as_deref().ok_or(ExecError::Internal)?)
                    .map_err(|_| ExecError::Internal)?,
            ),
            Some((_, status, response)) if status == "failed" => DurableBegin::Failed(
                decode_durable_error(response.as_deref().ok_or(ExecError::Internal)?)?,
            ),
            Some((_, status, _)) if status == "outcome_unknown" => DurableBegin::OutcomeUnknown,
            Some((_, status, _)) if status == "tombstone" => DurableBegin::Tombstone,
            Some(_) => DurableBegin::Pending,
            None => {
                let count = transaction
                    .query_row("SELECT COUNT(*) FROM executor_api_idempotency", [], |row| {
                        row.get::<_, usize>(0)
                    })
                    .map_err(|_| ExecError::Unavailable)?;
                if count >= MAX_IDEMPOTENCY_RECORDS {
                    return Err(ExecError::Unavailable);
                }
                transaction
                    .execute(
                        "INSERT INTO executor_api_idempotency
                         (principal_id, project_id, operation, resource, idempotency_key, digest, status, response, updated_millis)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', NULL, ?7)",
                        params![scope.0.to_string(), scope.1.to_string(), scope.2, scope.3, scope.4, digest.as_slice(), now_millis()?],
                    )
                    .map_err(|_| ExecError::Unavailable)?;
                DurableBegin::New
            }
        };
        transaction.commit().map_err(|_| ExecError::Unavailable)?;
        Ok(result)
    }

    fn complete(
        &self,
        scope: &(PrincipalId, ProjectId, String, String, String),
        digest: [u8; 32],
        response: &Value,
        event: Option<&(u64, ExecEvent)>,
    ) -> Result<(), ExecError> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ExecError::Unavailable)?;
        let response = serde_json::to_string(response).map_err(|_| ExecError::Internal)?;
        let changed = transaction
            .execute(
                "UPDATE executor_api_idempotency SET status='complete', response=?1, updated_millis=?2
                 WHERE principal_id=?3 AND project_id=?4 AND operation=?5 AND resource=?6
                   AND idempotency_key=?7 AND digest=?8 AND status IN ('pending','outcome_unknown')",
                params![response, now_millis()?, scope.0.to_string(), scope.1.to_string(), scope.2, scope.3, scope.4, digest.as_slice()],
            )
            .map_err(|_| ExecError::Unavailable)?;
        if changed != 1 {
            return Err(ExecError::OutcomeUnknown);
        }
        if let Some((position, event)) = event {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO executor_api_events (principal_id, project_id, position, event)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![event.principal_id.to_string(), event.project_id.to_string(), position, serde_json::to_string(event).map_err(|_| ExecError::Internal)?],
                )
                .map_err(|_| ExecError::Unavailable)?;
            transaction
                .execute(
                    "INSERT INTO executor_api_event_sequences
                       (principal_id, project_id, next_position) VALUES (?1, ?2, ?3)
                     ON CONFLICT(principal_id, project_id) DO UPDATE SET
                       next_position=MAX(next_position, excluded.next_position)",
                    params![
                        event.principal_id.to_string(),
                        event.project_id.to_string(),
                        position.checked_add(1).ok_or(ExecError::Internal)?,
                    ],
                )
                .map_err(|_| ExecError::Unavailable)?;
            transaction
                .execute(
                    "DELETE FROM executor_api_events WHERE rowid IN
                     (SELECT rowid FROM executor_api_events ORDER BY rowid DESC LIMIT -1 OFFSET ?1)",
                    [MAX_EVENTS],
                )
                .map_err(|_| ExecError::Unavailable)?;
        }
        transaction.commit().map_err(|_| ExecError::Unavailable)
    }

    fn finish_error(
        &self,
        scope: &(PrincipalId, ProjectId, String, String, String),
        digest: [u8; 32],
        error: &ExecError,
        outcome_unknown: bool,
    ) -> Result<(), ExecError> {
        let response = (!outcome_unknown).then(|| encode_durable_error(error));
        let changed = self
            .connection()?
            .execute(
                "UPDATE executor_api_idempotency SET status=?1, response=?2, updated_millis=?3
                 WHERE principal_id=?4 AND project_id=?5 AND operation=?6 AND resource=?7
                   AND idempotency_key=?8 AND digest=?9 AND status IN ('pending','outcome_unknown')",
                params![
                    if outcome_unknown { "outcome_unknown" } else { "failed" },
                    response,
                    now_millis()?,
                    scope.0.to_string(),
                    scope.1.to_string(),
                    scope.2,
                    scope.3,
                    scope.4,
                    digest.as_slice(),
                ],
            )
            .map_err(|_| ExecError::Unavailable)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(ExecError::OutcomeUnknown)
        }
    }

    fn resolve_input(
        &self,
        scope: &(PrincipalId, ProjectId, String, String, String),
        outcome: TerminalInputResolution,
    ) -> Result<Value, ExecError> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|_| ExecError::Unavailable)?;
        let existing = transaction
            .query_row(
                "SELECT status, response FROM executor_api_idempotency
                 WHERE principal_id=?1 AND project_id=?2 AND operation=?3 AND resource=?4
                   AND idempotency_key=?5",
                params![
                    scope.0.to_string(),
                    scope.1.to_string(),
                    scope.2,
                    scope.3,
                    scope.4
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(|_| ExecError::Unavailable)?
            .ok_or(ExecError::NotFound)?;
        let mut response = receipt(
            "terminal.input.resolve",
            Some(json!({
                "attachment_id": scope.3,
                "outcome": match outcome {
                    TerminalInputResolution::Applied => "applied",
                    TerminalInputResolution::NotApplied => "not_applied",
                },
            })),
        );
        let replayed = match existing.0.as_str() {
            "pending" | "outcome_unknown" => {
                let (status, stored) = match outcome {
                    TerminalInputResolution::Applied => (
                        "complete",
                        serde_json::to_string(&receipt(
                            "terminal.input",
                            Some(json!({
                                "attachment_id": scope.3,
                                "resolution": "applied",
                            })),
                        ))
                        .map_err(|_| ExecError::Internal)?,
                    ),
                    TerminalInputResolution::NotApplied => ("failed", "not_applied".to_owned()),
                };
                let changed = transaction
                    .execute(
                        "UPDATE executor_api_idempotency SET status=?1, response=?2, updated_millis=?3
                         WHERE principal_id=?4 AND project_id=?5 AND operation=?6 AND resource=?7
                           AND idempotency_key=?8 AND status IN ('pending','outcome_unknown')",
                        params![status, stored, now_millis()?, scope.0.to_string(), scope.1.to_string(), scope.2, scope.3, scope.4],
                    )
                    .map_err(|_| ExecError::Unavailable)?;
                if changed != 1 {
                    return Err(ExecError::OutcomeUnknown);
                }
                false
            }
            "complete" if outcome == TerminalInputResolution::Applied => true,
            "failed"
                if outcome == TerminalInputResolution::NotApplied
                    && existing.1.as_deref() == Some("not_applied") =>
            {
                true
            }
            "tombstone" => return Err(ExecError::Conflict("idempotency outcome expired")),
            _ => return Err(ExecError::Conflict("input resolution conflicts")),
        };
        transaction.commit().map_err(|_| ExecError::Unavailable)?;
        response["changed"] = json!(!replayed);
        response["replayed"] = json!(replayed);
        Ok(response)
    }
}

fn prune_idempotency(transaction: &rusqlite::Transaction<'_>) -> Result<(), ExecError> {
    let now = now_millis()?;
    let cutoff = now.saturating_sub(EXECUTOR_IDEMPOTENCY_RETRY_WINDOW_MILLIS);
    transaction
        .execute(
            "UPDATE executor_api_idempotency SET status='tombstone', response=NULL
             WHERE status IN ('complete','failed') AND updated_millis < ?1",
            [cutoff],
        )
        .map_err(|_| ExecError::Unavailable)?;
    transaction
        .execute(
            "UPDATE executor_api_idempotency SET status='outcome_unknown'
             WHERE status='pending' AND updated_millis < ?1",
            [now.saturating_sub(5 * 60 * 1_000)],
        )
        .map_err(|_| ExecError::Unavailable)?;
    transaction
        .execute(
            "DELETE FROM executor_api_idempotency
             WHERE status='tombstone' AND updated_millis < ?1",
            [now.saturating_sub(2 * EXECUTOR_IDEMPOTENCY_RETRY_WINDOW_MILLIS)],
        )
        .map_err(|_| ExecError::Unavailable)?;
    Ok(())
}

pub struct ManagerExecService<D, S, C> {
    terminals: TerminalManager<D, S>,
    cancellation: C,
    state: Mutex<AdapterState>,
    durable: Option<SqliteExecJournal>,
}

impl<D: PtyDriver, S: TerminalSnapshotStore, C: ExecutorCancellationCoordinator>
    ManagerExecService<D, S, C>
{
    pub fn new(terminals: TerminalManager<D, S>, cancellation: C) -> Self {
        Self {
            terminals,
            cancellation,
            state: Mutex::new(AdapterState::default()),
            durable: None,
        }
    }

    pub fn open(
        database: impl Into<PathBuf>,
        terminals: TerminalManager<D, S>,
        cancellation: C,
    ) -> Result<Self, ExecError> {
        let durable = SqliteExecJournal::open(database.into())?;
        let state = durable.load()?;
        Ok(Self {
            terminals,
            cancellation,
            state: Mutex::new(state),
            durable: Some(durable),
        })
    }

    fn resolve_input_outcome(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
        body: TerminalInputResolutionBody,
    ) -> Result<Value, ExecError> {
        let snapshot = authenticated.grant_snapshot();
        if !snapshot.grants().contains(&Grant::ProcessSpawn) {
            return Err(ExecError::NotFound);
        }
        let scope = (
            snapshot.principal_id(),
            snapshot.project_id(),
            "terminal.input".to_owned(),
            attachment_id.to_owned(),
            key.as_str().to_owned(),
        );
        let durable = self.durable.as_ref().ok_or(ExecError::Unsupported)?;
        durable.authorize_idempotency_record(&scope)?;
        durable.resolve_input(&scope, body.outcome)
    }

    pub fn register_process(&self, registration: ProcessRegistration) -> Result<(), ExecError> {
        let process_id = registration.claim.process_id;
        let ProcessOwnership::Attempt(owner) = registration.claim.owner else {
            return Err(ExecError::Invalid(
                "only attempt-owned processes are supported",
            ));
        };
        if registration.boundary_id.trim().is_empty() {
            return Err(ExecError::Invalid("boundary identity is required"));
        }
        let mut state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        let result = match state.processes.get(&registration.claim.process_id) {
            Some(existing)
                if existing.registration.project_id == registration.project_id
                    && existing.registration.terminal_request.transport
                        == registration.terminal_request.transport
                    && existing.registration.boundary_id == registration.boundary_id
                    && matches!(existing.cancellation, ProcessCancellation::Attempt(existing_owner) if existing_owner == owner) =>
            {
                Ok(())
            }
            Some(_) => Err(ExecError::Conflict("process registration conflicts")),
            None => {
                state.processes.insert(
                    registration.claim.process_id,
                    ProcessEntry {
                        registration,
                        principal_id: owner.principal_id,
                        cancellation: ProcessCancellation::Attempt(owner),
                    },
                );
                Ok(())
            }
        };
        if result.is_ok()
            && let Some(entry) = state.processes.get(&process_id)
            && let Some(durable) = &self.durable
        {
            durable.save_process(entry)?;
        }
        result
    }

    pub fn register_daemon_process(
        &self,
        registration: ProcessRegistration,
        scope: DaemonServiceScope,
        controller: Option<Arc<dyn DaemonProcessController>>,
    ) -> Result<(), ExecError> {
        let process_id = registration.claim.process_id;
        if registration.project_id != scope.project_id
            || registration.claim.owner != ProcessOwnership::DaemonService(scope.service_id)
            || registration.boundary_id.trim().is_empty()
        {
            return Err(ExecError::Invalid(
                "daemon service scope does not match process",
            ));
        }
        let mut state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        let result = match state.processes.get(&registration.claim.process_id) {
            Some(existing)
                if existing.registration.project_id == scope.project_id
                    && existing.principal_id == scope.principal_id
                    && matches!(existing.cancellation, ProcessCancellation::DaemonService { service_id, .. } if service_id == scope.service_id) =>
            {
                Ok(())
            }
            Some(_) => Err(ExecError::Conflict("process registration conflicts")),
            None => {
                state.processes.insert(
                    registration.claim.process_id,
                    ProcessEntry {
                        registration,
                        principal_id: scope.principal_id,
                        cancellation: ProcessCancellation::DaemonService {
                            service_id: scope.service_id,
                            controller,
                        },
                    },
                );
                Ok(())
            }
        };
        if result.is_ok()
            && let Some(entry) = state.processes.get(&process_id)
            && let Some(durable) = &self.durable
        {
            durable.save_process(entry)?;
        }
        result
    }

    pub fn update_process(
        &self,
        process_id: ProcessId,
        state_value: ProcessResourceState,
    ) -> Result<(), ExecError> {
        let mut state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        let process = state
            .processes
            .get_mut(&process_id)
            .ok_or(ExecError::NotFound)?;
        process.registration.state = state_value;
        if let Some(durable) = &self.durable {
            durable.save_process(process)?;
        }
        Ok(())
    }

    pub fn restore_terminal(
        &self,
        control: TerminalControl,
        snapshot: &crate::executor::terminal::TerminalSnapshot,
    ) -> Result<(), ExecError> {
        let mut state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        state.terminals.insert(
            snapshot.terminal_id,
            TerminalEntry {
                project_id: snapshot.owner.project_id,
                principal_id: snapshot.owner.principal_id,
                process_id: snapshot.owner.process_id,
                control,
            },
        );
        Ok(())
    }

    pub fn daemon_died(&self) -> Result<(), ExecError> {
        self.terminals.daemon_died().map_err(terminal_error)
    }

    fn update_started(&self, record: &ProcessRecord) -> Result<(), ExecError> {
        let mut state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        let process = state
            .processes
            .get_mut(&record.process_id())
            .ok_or(ExecError::NotFound)?;
        if process.registration.claim.owner != record.owner() {
            return Err(ExecError::Conflict("process owner changed"));
        }
        process.registration.execution_id = Some(record.execution_id());
        process.registration.state = ProcessResourceState::Started;
        if let Some(durable) = &self.durable {
            durable.save_process(process)?;
        }
        Ok(())
    }

    fn reconcile_mutation(
        &self,
        state: &AdapterState,
        authenticated: &AuthenticatedPrincipal,
        operation: &'static str,
        target: &str,
    ) -> Result<ReconciledMutation, ExecError> {
        let reconciled = match operation {
            "process.cancel" => {
                let process_id = ProcessId::parse(target).map_err(|_| ExecError::NotFound)?;
                let entry = visible_process(state, authenticated, process_id, Grant::ProcessSpawn)?;
                let outcome = match &entry.cancellation {
                    ProcessCancellation::Attempt(owner) => self
                        .cancellation
                        .cancel_attempt(*owner)
                        .map_err(cancellation_error)?,
                    ProcessCancellation::DaemonService {
                        service_id,
                        controller: Some(controller),
                    } => controller
                        .cancel(*service_id, process_id, &entry.registration.boundary_id)
                        .map_err(cancellation_error)?,
                    ProcessCancellation::DaemonService {
                        controller: None, ..
                    } => {
                        return Ok(ReconciledMutation::Failed(ExecError::Unsupported));
                    }
                };
                let status = match outcome {
                    ExecutorCancellationOutcome::Quiescent => "cancelled",
                    ExecutorCancellationOutcome::OutcomeUnknown => {
                        return Ok(ReconciledMutation::OutcomeUnknown);
                    }
                };
                ReconciledMutation::Complete(
                    receipt(
                        operation,
                        Some(json!({ "process_id": process_id, "status": status })),
                    ),
                    Some((
                        "process.cancellation_completed",
                        entry.registration.project_id,
                        process_id.to_string(),
                    )),
                )
            }
            "terminal.allocate"
            | "terminal.viewer.attach"
            | "terminal.writer.claim"
            | "terminal.writer.renew"
            | "terminal.resize" => ReconciledMutation::OutcomeUnknown,
            "terminal.writer.release" => {
                let entry = visible_attachment(state, authenticated, target, Grant::ProcessSpawn)?;
                if entry.attachment.is_writer() {
                    return Ok(ReconciledMutation::Failed(ExecError::Conflict(
                        "writer release did not take effect",
                    )));
                }
                ReconciledMutation::Complete(
                    receipt(
                        operation,
                        Some(json!({ "attachment_id": target, "role": "viewer" })),
                    ),
                    Some((
                        "terminal.writer_released",
                        entry.project_id,
                        target.to_owned(),
                    )),
                )
            }
            "terminal.detach" => {
                if state.attachments.contains_key(target) {
                    return Ok(ReconciledMutation::Failed(ExecError::Conflict(
                        "terminal detach did not take effect",
                    )));
                }
                ReconciledMutation::Complete(
                    receipt(
                        operation,
                        Some(json!({ "attachment_id": target, "detached": true })),
                    ),
                    None,
                )
            }
            "terminal.input" => ReconciledMutation::OutcomeUnknown,
            _ => ReconciledMutation::Failed(ExecError::Internal),
        };
        Ok(reconciled)
    }

    fn mutation<F>(
        &self,
        authenticated: &AuthenticatedPrincipal,
        operation: &'static str,
        target: &str,
        key: &IdempotencyKey,
        request: &Value,
        action: F,
    ) -> Result<Value, ExecError>
    where
        F: FnOnce(
            &mut AdapterState,
        ) -> Result<(Value, Option<(&'static str, ProjectId, String)>), ExecError>,
    {
        let grants = authenticated.grant_snapshot();
        let scope = (
            grants.principal_id(),
            grants.project_id(),
            operation.to_owned(),
            target.to_owned(),
            key.as_str().to_owned(),
        );
        let digest = request_digest(operation, target, request);
        let mut state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        if let Err(error) = authorize_mutation_target(&state, authenticated, operation, target) {
            if operation == "terminal.input" && grants.grants().contains(&Grant::ProcessSpawn) {
                self.durable
                    .as_ref()
                    .ok_or(error)?
                    .authorize_idempotency_record(&scope)?;
                // Exact durable input ownership permits reconciliation, not attachment reuse.
            } else if matches!(
                operation,
                "terminal.writer.renew"
                    | "terminal.writer.release"
                    | "terminal.input"
                    | "terminal.resize"
                    | "terminal.detach"
            ) {
                self.durable.as_ref().ok_or(error)?.authorize_attachment(
                    authenticated,
                    target,
                    operation == "terminal.detach",
                )?;
            } else {
                return Err(error);
            }
        }
        if let Some(stored) = state.idempotency.get(&scope) {
            if stored.digest != digest {
                return Err(ExecError::Conflict("idempotency key was reused"));
            }
            let mut response = stored.response.clone();
            response["replayed"] = json!(true);
            response["changed"] = json!(false);
            return Ok(response);
        }
        if let Some(durable) = &self.durable {
            match durable.begin(&scope, digest)? {
                DurableBegin::New => {}
                DurableBegin::Pending | DurableBegin::OutcomeUnknown => {
                    let reconciliation =
                        match self.reconcile_mutation(&state, authenticated, operation, target) {
                            Ok(reconciliation) => reconciliation,
                            Err(error) => {
                                let unknown = effect_failure_unknown(operation, &error);
                                durable.finish_error(&scope, digest, &error, unknown)?;
                                return Err(if unknown {
                                    ExecError::OutcomeUnknown
                                } else {
                                    error
                                });
                            }
                        };
                    match reconciliation {
                        ReconciledMutation::Complete(mut response, event) => {
                            let has_event = event.is_some();
                            if let Some((event_type, project_id, resource_id)) = event {
                                append_event(
                                    &mut state,
                                    event_type,
                                    authenticated.principal_id(),
                                    project_id,
                                    resource_id,
                                )?;
                            }
                            let persisted_event =
                                if has_event { state.events.back() } else { None };
                            durable.complete(&scope, digest, &response, persisted_event)?;
                            response["replayed"] = json!(true);
                            response["changed"] = json!(false);
                            state.idempotency.insert(
                                scope,
                                IdempotentResult {
                                    digest,
                                    response: response.clone(),
                                },
                            );
                            return Ok(response);
                        }
                        ReconciledMutation::Failed(error) => {
                            durable.finish_error(&scope, digest, &error, false)?;
                            return Err(error);
                        }
                        ReconciledMutation::OutcomeUnknown => {
                            durable.finish_error(
                                &scope,
                                digest,
                                &ExecError::OutcomeUnknown,
                                true,
                            )?;
                            return Err(ExecError::OutcomeUnknown);
                        }
                    }
                }
                DurableBegin::Complete(mut response) => {
                    response["replayed"] = json!(true);
                    response["changed"] = json!(false);
                    return Ok(response);
                }
                DurableBegin::Failed(error) => return Err(error),
                DurableBegin::Tombstone => {
                    return Err(ExecError::Conflict("idempotency outcome expired"));
                }
            }
        }
        let (response, event) = match action(&mut state) {
            Ok(result) => result,
            Err(error) => {
                let unknown = effect_failure_unknown(operation, &error);
                if let Some(durable) = &self.durable {
                    durable.finish_error(&scope, digest, &error, unknown)?;
                }
                return Err(if unknown {
                    ExecError::OutcomeUnknown
                } else {
                    error
                });
            }
        };
        let has_event = event.is_some();
        if let Some((event_type, project_id, resource_id)) = event {
            append_event(
                &mut state,
                event_type,
                authenticated.principal_id(),
                project_id,
                resource_id,
            )?;
        }
        if let Some(durable) = &self.durable {
            let persisted_event = if has_event { state.events.back() } else { None };
            durable.complete(&scope, digest, &response, persisted_event)?;
        }
        state.idempotency.insert(
            scope,
            IdempotentResult {
                digest,
                response: response.clone(),
            },
        );
        Ok(response)
    }
}

impl<D: PtyDriver, S: TerminalSnapshotStore, C: ExecutorCancellationCoordinator> ProcessRegistry
    for ManagerExecService<D, S, C>
{
    fn prepared(
        &self,
        context: ProcessRegistrationContext,
        claim: ProcessClaim,
        boundary: &crate::executor::process::tree::PersistedBoundary,
        terminal: ProcessTerminalConfig,
    ) -> std::io::Result<()> {
        let boundary_id = boundary.encode();
        let registration = ProcessRegistration {
            project_id: context.project_id,
            claim,
            execution_id: None,
            state: ProcessResourceState::Prepared,
            terminal_request: terminal.request,
            boundary_id: boundary_id.clone(),
        };
        if matches!(claim.owner, ProcessOwnership::Attempt(owner) if owner.principal_id != context.principal_id)
        {
            return Err(std::io::Error::other(
                "process registry: process principal mismatch",
            ));
        }
        if terminal.request.transport == TerminalTransport::Pty
            && matches!(claim.owner, ProcessOwnership::DaemonService(_))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "daemon-service PTYs are unsupported",
            ));
        }
        #[cfg(windows)]
        let conpty_prepared = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("process registry lock poisoned"))?
            .terminals
            .values()
            .any(|terminal| terminal.process_id == claim.process_id);
        #[cfg(not(windows))]
        let conpty_prepared = false;
        let allocation = if terminal.request.transport == TerminalTransport::Pty && !conpty_prepared
        {
            let TerminalAllocation::Pty {
                terminal_id,
                control,
            } = self
                .terminals
                .allocate_registered(
                    terminal.request,
                    claim,
                    &boundary_id,
                    terminal.size,
                    terminal.retention,
                )
                .map_err(|error| std::io::Error::other(error.to_string()))?
            else {
                return Err(std::io::Error::other("PTY registration returned pipes"));
            };
            Some((terminal_id, control))
        } else {
            None
        };
        let registered = match claim.owner {
            ProcessOwnership::Attempt(owner) if owner.principal_id == context.principal_id => {
                self.register_process(registration)
            }
            ProcessOwnership::DaemonService(service_id) => self.register_daemon_process(
                registration,
                DaemonServiceScope {
                    principal_id: context.principal_id,
                    project_id: context.project_id,
                    service_id,
                },
                None,
            ),
            ProcessOwnership::Attempt(_) => Err(ExecError::Invalid("process principal mismatch")),
        };
        if let Err(error) = registered {
            if let Some((_, control)) = &allocation {
                let _ = self.terminals.interrupt(control);
            }
            return Err(std::io::Error::other(format!(
                "process registry: {error:?}"
            )));
        }
        if let Some((terminal_id, control)) = allocation {
            let mut state = self
                .state
                .lock()
                .map_err(|_| std::io::Error::other("process registry lock poisoned"))?;
            state.terminals.insert(
                terminal_id,
                TerminalEntry {
                    project_id: context.project_id,
                    principal_id: context.principal_id,
                    process_id: claim.process_id,
                    control,
                },
            );
        }
        Ok(())
    }

    #[cfg(windows)]
    fn prepare_conpty(
        &self,
        context: ProcessRegistrationContext,
        claim: ProcessClaim,
        boundary_id: &str,
        terminal: ProcessTerminalConfig,
    ) -> std::io::Result<crate::executor::terminal::ConPtyBinding> {
        if terminal.request.transport != TerminalTransport::Pty
            || !matches!(claim.owner, ProcessOwnership::Attempt(owner) if owner.principal_id == context.principal_id)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "ConPTY requires a matching attempt-owned process",
            ));
        }
        let TerminalAllocation::Pty {
            terminal_id,
            control,
        } = self
            .terminals
            .allocate_registered(
                terminal.request,
                claim,
                boundary_id,
                terminal.size,
                terminal.retention,
            )
            .map_err(|error| std::io::Error::other(error.to_string()))?
        else {
            return Err(std::io::Error::other("ConPTY registration returned pipes"));
        };
        let binding = match self.terminals.conpty_binding(&control) {
            Ok(binding) => binding,
            Err(error) => {
                let _ = self.terminals.interrupt(&control);
                return Err(std::io::Error::other(error.to_string()));
            }
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("process registry lock poisoned"))?;
        if state
            .terminals
            .values()
            .any(|terminal| terminal.process_id == claim.process_id)
        {
            drop(state);
            let _ = self.terminals.interrupt(&control);
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "ConPTY is already prepared for process",
            ));
        }
        state.terminals.insert(
            terminal_id,
            TerminalEntry {
                project_id: context.project_id,
                principal_id: context.principal_id,
                process_id: claim.process_id,
                control,
            },
        );
        Ok(binding)
    }

    #[cfg(windows)]
    fn abort_conpty(
        &self,
        _context: ProcessRegistrationContext,
        process_id: ProcessId,
    ) -> std::io::Result<()> {
        let terminal = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| std::io::Error::other("process registry lock poisoned"))?;
            let terminal_id = state
                .terminals
                .iter()
                .find_map(|(id, terminal)| (terminal.process_id == process_id).then_some(*id));
            terminal_id.and_then(|id| state.terminals.remove(&id))
        };
        if let Some(terminal) = terminal {
            self.terminals
                .interrupt(&terminal.control)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
        }
        Ok(())
    }

    fn bind_terminal(
        &self,
        _context: ProcessRegistrationContext,
        process_id: ProcessId,
        command: &mut ProcessCommand,
    ) -> std::io::Result<Box<dyn Read + Send>> {
        let state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("process registry lock poisoned"))?;
        let terminal = state
            .terminals
            .values()
            .find(|terminal| terminal.process_id == process_id)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "PTY not prepared"))?;
        self.terminals
            .bind_process(&terminal.control, command)
            .map_err(|error| std::io::Error::other(error.to_string()))
    }

    fn append_terminal_output(
        &self,
        _context: ProcessRegistrationContext,
        process_id: ProcessId,
        capture: &crate::telemetry::redact::SanitizedCapture,
    ) -> std::io::Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("process registry lock poisoned"))?;
        let terminal = state
            .terminals
            .values()
            .find(|terminal| terminal.process_id == process_id)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "PTY not prepared"))?;
        self.terminals
            .append_output(
                &terminal.control,
                capture,
                now_millis().map_err(|_| {
                    std::io::Error::other("system time unavailable for terminal output")
                })?,
            )
            .map(|_| ())
            .map_err(|error| std::io::Error::other(error.to_string()))
    }

    fn set_terminal_capture_policy(
        &self,
        _context: ProcessRegistrationContext,
        process_id: ProcessId,
        capture_policy: crate::telemetry::redact::CapturePersistencePolicy,
    ) -> std::io::Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("process registry lock poisoned"))?;
        let terminal = state
            .terminals
            .values()
            .find(|terminal| terminal.process_id == process_id)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "PTY not prepared"))?;
        self.terminals
            .set_capture_policy(&terminal.control, capture_policy)
            .map_err(|error| std::io::Error::other(error.to_string()))
    }

    fn close_terminal(
        &self,
        _context: ProcessRegistrationContext,
        process_id: ProcessId,
    ) -> std::io::Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("process registry lock poisoned"))?;
        let terminal = state
            .terminals
            .values()
            .find(|terminal| terminal.process_id == process_id)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "PTY not prepared"))?;
        self.terminals
            .close(&terminal.control)
            .map_err(|error| std::io::Error::other(error.to_string()))
    }

    fn started(
        &self,
        _context: ProcessRegistrationContext,
        record: &ProcessRecord,
    ) -> std::io::Result<()> {
        self.update_started(record)
            .map_err(|error| std::io::Error::other(format!("process registry: {error:?}")))
    }

    fn exited(
        &self,
        _context: ProcessRegistrationContext,
        record: &ProcessRecord,
    ) -> std::io::Result<()> {
        let state = match record.state() {
            ProcessState::Started => ProcessResourceState::OutcomeUnknown,
            ProcessState::Exited {
                success,
                code,
                signal,
            } => ProcessResourceState::Exited {
                success,
                code,
                signal,
            },
        };
        self.update_process(record.process_id(), state)
            .map_err(|error| std::io::Error::other(format!("process registry: {error:?}")))
    }

    fn outcome_unknown(
        &self,
        _context: ProcessRegistrationContext,
        process_id: ProcessId,
    ) -> std::io::Result<()> {
        self.update_process(process_id, ProcessResourceState::OutcomeUnknown)
            .map_err(|error| std::io::Error::other(format!("process registry: {error:?}")))?;
        let state = self
            .state
            .lock()
            .map_err(|_| std::io::Error::other("process registry lock poisoned"))?;
        if let Some(terminal) = state
            .terminals
            .values()
            .find(|terminal| terminal.process_id == process_id)
        {
            self.terminals
                .interrupt(&terminal.control)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
        }
        Ok(())
    }
}

impl<D: PtyDriver, S: TerminalSnapshotStore, C: ExecutorCancellationCoordinator> ExecService
    for ManagerExecService<D, S, C>
{
    fn list_processes(
        &self,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
    ) -> Result<Value, ExecError> {
        authorize_project(authenticated, project_id, Grant::WorkspaceRead)?;
        let state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        let mut items = state
            .processes
            .values()
            .filter(|entry| {
                entry.registration.project_id == project_id
                    && entry.principal_id == authenticated.principal_id()
            })
            .map(process_resource)
            .collect::<Vec<_>>();
        items.sort_by_key(|item| item.process_id);
        Ok(json!({ "schema_version": SCHEMA_VERSION, "items": items }))
    }

    fn get_process(
        &self,
        authenticated: &AuthenticatedPrincipal,
        process_id: ProcessId,
    ) -> Result<Value, ExecError> {
        let state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        let entry = visible_process(&state, authenticated, process_id, Grant::WorkspaceRead)?;
        serde_json::to_value(process_resource(entry)).map_err(|_| ExecError::Internal)
    }

    fn cancel_process(
        &self,
        authenticated: &AuthenticatedPrincipal,
        process_id: ProcessId,
        key: &IdempotencyKey,
    ) -> Result<Value, ExecError> {
        self.mutation(
            authenticated,
            "process.cancel",
            &process_id.to_string(),
            key,
            &json!({}),
            |state| {
                let entry = visible_process(state, authenticated, process_id, Grant::ProcessSpawn)?;
                let project_id = entry.registration.project_id;
                let outcome = match &entry.cancellation {
                    ProcessCancellation::Attempt(owner) => self
                        .cancellation
                        .cancel_attempt(*owner)
                        .map_err(cancellation_error)?,
                    ProcessCancellation::DaemonService {
                        service_id,
                        controller: Some(controller),
                    } => controller
                        .cancel(*service_id, process_id, &entry.registration.boundary_id)
                        .map_err(cancellation_error)?,
                    ProcessCancellation::DaemonService {
                        controller: None, ..
                    } => return Err(ExecError::Unsupported),
                };
                let status = match outcome {
                    ExecutorCancellationOutcome::Quiescent => "cancelled",
                    ExecutorCancellationOutcome::OutcomeUnknown => {
                        return Err(ExecError::OutcomeUnknown);
                    }
                };
                Ok((
                    receipt(
                        "process.cancel",
                        Some(json!({ "process_id": process_id, "status": status })),
                    ),
                    Some((
                        "process.cancellation_completed",
                        project_id,
                        process_id.to_string(),
                    )),
                ))
            },
        )
    }

    fn allocate_terminal(
        &self,
        authenticated: &AuthenticatedPrincipal,
        process_id: ProcessId,
        key: &IdempotencyKey,
        body: AllocateTerminalBody,
    ) -> Result<Value, ExecError> {
        TerminalSize::new(body.columns, body.rows).map_err(terminal_error)?;
        if body.max_output_bytes == 0 || body.max_output_age_millis == 0 {
            return Err(ExecError::Invalid("terminal retention must be non-zero"));
        }
        {
            let state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
            let entry = visible_process(&state, authenticated, process_id, Grant::ProcessSpawn)?;
            if matches!(
                entry.registration.claim.owner,
                ProcessOwnership::DaemonService(_)
            ) {
                return Err(ExecError::Unsupported);
            }
            if entry.registration.terminal_request.transport != TerminalTransport::Pty {
                return Err(ExecError::Unsupported);
            }
        }
        self.terminals
            .ensure_pty_available()
            .map_err(terminal_error)?;
        let request = serde_json::to_value(&body).map_err(|_| ExecError::Internal)?;
        self.mutation(
            authenticated,
            "terminal.allocate",
            &process_id.to_string(),
            key,
            &request,
            |state| {
                let entry = visible_process(state, authenticated, process_id, Grant::ProcessSpawn)?;
                if entry.registration.terminal_request.transport != TerminalTransport::Pty {
                    return Err(ExecError::Unsupported);
                }
                let terminal = state
                    .terminals
                    .values()
                    .find(|terminal| terminal.process_id == process_id)
                    .ok_or(ExecError::Conflict(
                        "PTY was not bound before process spawn",
                    ))?;
                let terminal_id = terminal.control.terminal_id();
                let snapshot = self
                    .terminals
                    .snapshot(&terminal.control)
                    .map_err(terminal_error)?;
                if snapshot.size.columns != body.columns
                    || snapshot.size.rows != body.rows
                    || snapshot.retention
                        != OutputRetention::new(body.max_output_bytes, body.max_output_age_millis)
                {
                    return Err(ExecError::Conflict(
                        "terminal request differs from the pre-spawn PTY binding",
                    ));
                }
                let mut response = receipt(
                    "terminal.allocate",
                    Some(json!({ "terminal_id": terminal_id, "process_id": process_id })),
                );
                response["changed"] = json!(false);
                Ok((
                    response,
                    Some((
                        "terminal.exposed",
                        entry.registration.project_id,
                        terminal_id.to_string(),
                    )),
                ))
            },
        )
    }

    fn get_terminal(
        &self,
        authenticated: &AuthenticatedPrincipal,
        terminal_id: TerminalId,
    ) -> Result<Value, ExecError> {
        let state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        let entry = visible_terminal(&state, authenticated, terminal_id, Grant::WorkspaceRead)?;
        let snapshot = self
            .terminals
            .snapshot(&entry.control)
            .map_err(terminal_error)?;
        serde_json::to_value(terminal_resource(entry, &snapshot)).map_err(|_| ExecError::Internal)
    }

    fn attach_viewer(
        &self,
        authenticated: &AuthenticatedPrincipal,
        terminal_id: TerminalId,
        key: &IdempotencyKey,
    ) -> Result<Value, ExecError> {
        self.mutation(
            authenticated,
            "terminal.viewer.attach",
            &terminal_id.to_string(),
            key,
            &json!({}),
            |state| {
                let terminal =
                    visible_terminal(state, authenticated, terminal_id, Grant::WorkspaceRead)?;
                let project_id = terminal.project_id;
                let principal_id = terminal.principal_id;
                let attachment = self
                    .terminals
                    .attach_viewer(&terminal.control, authenticated)
                    .map_err(terminal_error)?;
                let attachment_id = new_attachment_id()?;
                state.attachments.insert(
                    attachment_id.clone(),
                    AttachmentEntry {
                        project_id,
                        principal_id,
                        attachment,
                        expires_at_millis: None,
                    },
                );
                let resource = attachment_resource(
                    &attachment_id,
                    state.attachments.get(&attachment_id).expect("inserted"),
                );
                if let Some(durable) = &self.durable {
                    durable.save_attachment(
                        &attachment_id,
                        state.attachments.get(&attachment_id).expect("inserted"),
                    )?;
                }
                Ok((
                    receipt(
                        "terminal.viewer.attach",
                        Some(serde_json::to_value(resource).map_err(|_| ExecError::Internal)?),
                    ),
                    Some(("terminal.viewer_attached", project_id, attachment_id)),
                ))
            },
        )
    }

    fn claim_writer(
        &self,
        authenticated: &AuthenticatedPrincipal,
        terminal_id: TerminalId,
        key: &IdempotencyKey,
        body: WriterLeaseBody,
    ) -> Result<Value, ExecError> {
        now_millis()?
            .checked_add(body.lease_millis)
            .filter(|_| body.lease_millis != 0)
            .ok_or(ExecError::Invalid("writer lease is invalid"))?;
        let request = serde_json::to_value(&body).map_err(|_| ExecError::Internal)?;
        self.mutation(
            authenticated,
            "terminal.writer.claim",
            &terminal_id.to_string(),
            key,
            &request,
            |state| {
                let terminal =
                    visible_terminal(state, authenticated, terminal_id, Grant::ProcessSpawn)?;
                let project_id = terminal.project_id;
                let principal_id = terminal.principal_id;
                let now = now_millis()?;
                let attachment = self
                    .terminals
                    .claim_writer(&terminal.control, authenticated, now, body.lease_millis)
                    .map_err(terminal_error)?;
                let expires = now
                    .checked_add(body.lease_millis)
                    .ok_or(ExecError::Invalid("writer lease overflows"))?;
                let attachment_id = new_attachment_id()?;
                state.attachments.insert(
                    attachment_id.clone(),
                    AttachmentEntry {
                        project_id,
                        principal_id,
                        attachment,
                        expires_at_millis: Some(expires),
                    },
                );
                let resource = attachment_resource(
                    &attachment_id,
                    state.attachments.get(&attachment_id).expect("inserted"),
                );
                if let Some(durable) = &self.durable {
                    durable.save_attachment(
                        &attachment_id,
                        state.attachments.get(&attachment_id).expect("inserted"),
                    )?;
                }
                Ok((
                    receipt(
                        "terminal.writer.claim",
                        Some(serde_json::to_value(resource).map_err(|_| ExecError::Internal)?),
                    ),
                    Some(("terminal.writer_claimed", project_id, attachment_id)),
                ))
            },
        )
    }

    fn get_attachment(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
    ) -> Result<Value, ExecError> {
        let state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        let entry = visible_attachment(&state, authenticated, attachment_id, Grant::WorkspaceRead)?;
        serde_json::to_value(attachment_resource(attachment_id, entry))
            .map_err(|_| ExecError::Internal)
    }

    fn renew_writer(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
        body: WriterLeaseBody,
    ) -> Result<Value, ExecError> {
        now_millis()?
            .checked_add(body.lease_millis)
            .filter(|_| body.lease_millis != 0)
            .ok_or(ExecError::Invalid("writer lease is invalid"))?;
        let request = serde_json::to_value(&body).map_err(|_| ExecError::Internal)?;
        self.mutation(
            authenticated,
            "terminal.writer.renew",
            attachment_id,
            key,
            &request,
            |state| {
                let entry = visible_attachment_mut(
                    state,
                    authenticated,
                    attachment_id,
                    Grant::ProcessSpawn,
                )?;
                let project_id = entry.project_id;
                let expires = self
                    .terminals
                    .renew_writer(&entry.attachment, now_millis()?, body.lease_millis)
                    .map_err(terminal_error)?;
                entry.expires_at_millis = Some(expires);
                if let Some(durable) = &self.durable {
                    durable.save_attachment(attachment_id, entry)?;
                }
                Ok((
                    receipt(
                        "terminal.writer.renew",
                        Some(
                            json!({ "attachment_id": attachment_id, "expires_at_millis": expires }),
                        ),
                    ),
                    Some((
                        "terminal.writer_renewed",
                        project_id,
                        attachment_id.to_owned(),
                    )),
                ))
            },
        )
    }

    fn release_writer(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
    ) -> Result<Value, ExecError> {
        self.mutation(
            authenticated,
            "terminal.writer.release",
            attachment_id,
            key,
            &json!({}),
            |state| {
                let entry = visible_attachment_mut(
                    state,
                    authenticated,
                    attachment_id,
                    Grant::ProcessSpawn,
                )?;
                let project_id = entry.project_id;
                self.terminals
                    .release_writer(&mut entry.attachment)
                    .map_err(terminal_error)?;
                entry.expires_at_millis = None;
                if let Some(durable) = &self.durable {
                    durable.save_attachment(attachment_id, entry)?;
                }
                Ok((
                    receipt(
                        "terminal.writer.release",
                        Some(json!({ "attachment_id": attachment_id, "role": "viewer" })),
                    ),
                    Some((
                        "terminal.writer_released",
                        project_id,
                        attachment_id.to_owned(),
                    )),
                ))
            },
        )
    }

    fn write_input(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
        bytes: &[u8],
    ) -> Result<Value, ExecError> {
        if bytes.is_empty() || bytes.len() > MAX_INPUT_BYTES {
            return Err(ExecError::Invalid(
                "terminal input must contain 1 to 16384 bytes",
            ));
        }
        // Only the digest enters idempotency state; raw input goes straight to the terminal driver.
        let request = json!({ "sha256_equivalent": blake3::hash(bytes).to_hex().to_string(), "length": bytes.len() });
        self.mutation(authenticated, "terminal.input", attachment_id, key, &request, |state| {
            let entry = visible_attachment(state, authenticated, attachment_id, Grant::ProcessSpawn)?;
            let project_id = entry.project_id;
            self.terminals.write_input(&entry.attachment, bytes, now_millis()?).map_err(terminal_error)?;
            Ok((receipt("terminal.input", Some(json!({ "attachment_id": attachment_id, "accepted_bytes": bytes.len() }))), Some(("terminal.input_accepted", project_id, attachment_id.to_owned()))))
        })
    }

    fn resolve_input(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
        body: TerminalInputResolutionBody,
    ) -> Result<Value, ExecError> {
        self.resolve_input_outcome(authenticated, attachment_id, key, body)
    }

    fn resize(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
        body: TerminalResizeBody,
    ) -> Result<Value, ExecError> {
        TerminalSize::new(body.columns, body.rows).map_err(terminal_error)?;
        let request = serde_json::to_value(&body).map_err(|_| ExecError::Internal)?;
        self.mutation(authenticated, "terminal.resize", attachment_id, key, &request, |state| {
            let entry = visible_attachment(state, authenticated, attachment_id, Grant::ProcessSpawn)?;
            let project_id = entry.project_id;
            let event = self.terminals.resize(&entry.attachment, TerminalSize::new(body.columns, body.rows).map_err(terminal_error)?, now_millis()?).map_err(terminal_error)?;
            Ok((receipt("terminal.resize", Some(json!({ "attachment_id": attachment_id, "sequence": event.sequence, "columns": event.size.columns, "rows": event.size.rows }))), Some(("terminal.resized", project_id, attachment_id.to_owned()))))
        })
    }

    fn read_output(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        cursor: u64,
    ) -> Result<Value, ExecError> {
        let state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        let entry = visible_attachment(&state, authenticated, attachment_id, Grant::WorkspaceRead)?;
        match self
            .terminals
            .read_output(&entry.attachment, cursor, now_millis()?)
            .map_err(terminal_error)?
        {
            OutputRead::Gap {
                oldest_available, ..
            } => Ok(
                json!({ "schema_version": SCHEMA_VERSION, "gap": true, "next_cursor": output_cursor(oldest_available), "chunks": [] }),
            ),
            OutputRead::Chunks {
                chunks,
                next_cursor,
            } => Ok(json!({
                "schema_version": SCHEMA_VERSION,
                "gap": false,
                "next_cursor": output_cursor(next_cursor),
                "chunks": chunks.into_iter().map(|chunk| json!({ "sequence": chunk.sequence, "recorded_at_millis": chunk.recorded_at_millis, "bytes": chunk.bytes() })).collect::<Vec<_>>(),
            })),
        }
    }

    fn read_resizes(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        cursor: u64,
    ) -> Result<Value, ExecError> {
        let state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        let entry = visible_attachment(&state, authenticated, attachment_id, Grant::WorkspaceRead)?;
        match self
            .terminals
            .read_resizes(&entry.attachment, cursor, now_millis()?)
            .map_err(terminal_error)?
        {
            ResizeRead::Gap {
                oldest_available, ..
            } => Ok(
                json!({ "schema_version": SCHEMA_VERSION, "gap": true, "next_cursor": resize_cursor(oldest_available), "events": [] }),
            ),
            ResizeRead::Events {
                events,
                next_cursor,
            } => Ok(
                json!({ "schema_version": SCHEMA_VERSION, "gap": false, "next_cursor": resize_cursor(next_cursor), "events": events }),
            ),
        }
    }

    fn detach(
        &self,
        authenticated: &AuthenticatedPrincipal,
        attachment_id: &str,
        key: &IdempotencyKey,
    ) -> Result<Value, ExecError> {
        self.mutation(
            authenticated,
            "terminal.detach",
            attachment_id,
            key,
            &json!({}),
            |state| {
                let required = attachment_mutation_grant(state, authenticated, attachment_id)?;
                let entry = visible_attachment_mut(state, authenticated, attachment_id, required)?;
                let project_id = entry.project_id;
                self.terminals
                    .detach(&mut entry.attachment)
                    .map_err(terminal_error)?;
                if let Some(durable) = &self.durable {
                    durable.invalidate_attachment(attachment_id)?;
                }
                state.attachments.remove(attachment_id);
                Ok((
                    receipt(
                        "terminal.detach",
                        Some(json!({ "attachment_id": attachment_id, "detached": true })),
                    ),
                    Some(("terminal.detached", project_id, attachment_id.to_owned())),
                ))
            },
        )
    }

    fn events(
        &self,
        authenticated: &AuthenticatedPrincipal,
        project_id: ProjectId,
        cursor: u64,
    ) -> Result<Value, ExecError> {
        authorize_project(authenticated, project_id, Grant::WorkspaceRead)?;
        let state = self.state.lock().map_err(|_| ExecError::Unavailable)?;
        let feed = (authenticated.principal_id(), project_id);
        let next = state.next_events.get(&feed).copied().unwrap_or(0);
        let oldest = state
            .events
            .iter()
            .find(|(_, event)| {
                event.principal_id == authenticated.principal_id() && event.project_id == project_id
            })
            .map_or(next, |(cursor, _)| *cursor);
        if cursor < oldest {
            let mut processes = state
                .processes
                .values()
                .filter(|entry| {
                    entry.principal_id == authenticated.principal_id()
                        && entry.registration.project_id == project_id
                })
                .map(process_resource)
                .collect::<Vec<_>>();
            processes.sort_by_key(|process| process.process_id);
            let mut terminals = Vec::new();
            let mut readable_terminals = HashSet::new();
            for entry in state.terminals.values().filter(|entry| {
                entry.principal_id == authenticated.principal_id() && entry.project_id == project_id
            }) {
                match self.terminals.snapshot(&entry.control) {
                    Ok(snapshot) => {
                        readable_terminals.insert(snapshot.terminal_id);
                        terminals.push(terminal_resource(entry, &snapshot));
                    }
                    Err(TerminalError::NotFound | TerminalError::StaleProcessClaim) => {}
                    Err(error) => return Err(terminal_error(error)),
                }
            }
            terminals.sort_by_key(|terminal| terminal.terminal_id);
            let grants = authenticated.grant_snapshot();
            let mut attachments = state
                .attachments
                .iter()
                .filter(|(_, entry)| {
                    entry.principal_id == authenticated.principal_id()
                        && entry.project_id == project_id
                        && readable_terminals.contains(&entry.attachment.terminal_id())
                        && grants.grants().contains(if entry.attachment.is_writer() {
                            &Grant::ProcessSpawn
                        } else {
                            &Grant::WorkspaceRead
                        })
                })
                .map(|(id, entry)| attachment_resource(id, entry))
                .collect::<Vec<_>>();
            attachments.sort_by(|left, right| left.attachment_id.cmp(&right.attachment_id));
            return Err(ExecError::CursorExpired(json!({
                "type": "https://kit.dev/problems/cursor_expired",
                "title": "Executor cursor expired",
                "status": 410,
                "detail": "The requested executor cursor is outside retained history.",
                "instance": HIDDEN_INSTANCE,
                "code": "cursor_expired",
                "snapshot": {
                    "schema_version": SCHEMA_VERSION,
                    "processes": processes,
                    "terminals": terminals,
                    "attachments": attachments,
                },
                "new_cursor": event_cursor(next),
            })));
        }
        let items = state
            .events
            .iter()
            .filter(|(position, event)| {
                *position >= cursor
                    && event.principal_id == authenticated.principal_id()
                    && event.project_id == project_id
            })
            .map(|(_, event)| event)
            .collect::<Vec<_>>();
        Ok(
            json!({ "schema_version": SCHEMA_VERSION, "gap": false, "next_cursor": event_cursor(next), "items": items }),
        )
    }
}

pub fn routes(service: Arc<dyn ExecService>) -> Router {
    EXEC_ROUTES
        .iter()
        .fold(Router::new(), |router, descriptor| {
            let method = match descriptor.operation {
                "process.list" => get(list_processes),
                "process.get" => get(get_process),
                "process.cancel" => post(cancel_process),
                "terminal.allocate" => post(allocate_terminal),
                "terminal.get" => get(get_terminal),
                "terminal.viewer.attach" => post(attach_viewer),
                "terminal.writer.claim" => post(claim_writer),
                "terminal.attachment.get" => get(get_attachment),
                "terminal.writer.renew" => post(renew_writer),
                "terminal.writer.release" => post(release_writer),
                "terminal.input" => post(write_input),
                "terminal.input.resolve" => post(resolve_input),
                "terminal.resize" => post(resize),
                "terminal.output" => get(read_output),
                "terminal.resizes" => get(read_resizes),
                "terminal.detach" => post(detach),
                "executor.events" => get(events),
                _ => unreachable!("executor route descriptor has no handler"),
            };
            router.route(descriptor.path, method)
        })
        .layer(Extension(service))
}

async fn list_processes(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(project_id): Path<String>,
) -> Response {
    let project_id = match ProjectId::parse(&project_id) {
        Ok(value) => value,
        Err(_) => return invalid("project_id").into_response(),
    };
    response(service.list_processes(&authenticated, project_id))
}

async fn get_process(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(process_id): Path<String>,
) -> Response {
    let process_id = match ProcessId::parse(&process_id) {
        Ok(value) => value,
        Err(_) => return invalid("process_id").into_response(),
    };
    response(service.get_process(&authenticated, process_id))
}

async fn cancel_process(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(process_id): Path<String>,
    request: Request,
) -> Response {
    let process_id = match ProcessId::parse(&process_id) {
        Ok(value) => value,
        Err(_) => return invalid("process_id").into_response(),
    };
    let (key, _body) = match mutation_body::<EmptyBody>(request).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    response(service.cancel_process(&authenticated, process_id, &key))
}

async fn allocate_terminal(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(process_id): Path<String>,
    request: Request,
) -> Response {
    let process_id = match ProcessId::parse(&process_id) {
        Ok(value) => value,
        Err(_) => return invalid("process_id").into_response(),
    };
    let (key, body) = match mutation_body(request).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    response(service.allocate_terminal(&authenticated, process_id, &key, body))
}

async fn get_terminal(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(terminal_id): Path<String>,
) -> Response {
    let terminal_id = match TerminalId::parse(&terminal_id) {
        Ok(value) => value,
        Err(_) => return invalid("terminal_id").into_response(),
    };
    response(service.get_terminal(&authenticated, terminal_id))
}

async fn attach_viewer(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(terminal_id): Path<String>,
    request: Request,
) -> Response {
    let terminal_id = match TerminalId::parse(&terminal_id) {
        Ok(value) => value,
        Err(_) => return invalid("terminal_id").into_response(),
    };
    let (key, _body) = match mutation_body::<EmptyBody>(request).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    response(service.attach_viewer(&authenticated, terminal_id, &key))
}

async fn claim_writer(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(terminal_id): Path<String>,
    request: Request,
) -> Response {
    let terminal_id = match TerminalId::parse(&terminal_id) {
        Ok(value) => value,
        Err(_) => return invalid("terminal_id").into_response(),
    };
    let (key, body) = match mutation_body(request).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    response(service.claim_writer(&authenticated, terminal_id, &key, body))
}

async fn get_attachment(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(attachment_id): Path<String>,
) -> Response {
    if !valid_attachment_id(&attachment_id) {
        return invalid("attachment_id").into_response();
    }
    response(service.get_attachment(&authenticated, &attachment_id))
}

async fn renew_writer(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(attachment_id): Path<String>,
    request: Request,
) -> Response {
    if !valid_attachment_id(&attachment_id) {
        return invalid("attachment_id").into_response();
    }
    let (key, body) = match mutation_body(request).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    response(service.renew_writer(&authenticated, &attachment_id, &key, body))
}

async fn release_writer(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(attachment_id): Path<String>,
    request: Request,
) -> Response {
    attachment_empty_mutation(
        service,
        authenticated,
        attachment_id,
        request,
        |service, authenticated, id, key| service.release_writer(authenticated, id, key),
    )
    .await
}

async fn write_input(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(attachment_id): Path<String>,
    request: Request,
) -> Response {
    if !valid_attachment_id(&attachment_id) {
        return invalid("attachment_id").into_response();
    }
    let (key, mut body) = match mutation_body::<TerminalInputBody>(request).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    let result = service.write_input(&authenticated, &attachment_id, &key, &body.bytes);
    body.bytes.fill(0);
    response(result)
}

async fn resolve_input(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(attachment_id): Path<String>,
    request: Request,
) -> Response {
    if !valid_attachment_id(&attachment_id) {
        return invalid("attachment_id").into_response();
    }
    let (key, body) = match mutation_body::<TerminalInputResolutionBody>(request).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    response(service.resolve_input(&authenticated, &attachment_id, &key, body))
}

async fn resize(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(attachment_id): Path<String>,
    request: Request,
) -> Response {
    if !valid_attachment_id(&attachment_id) {
        return invalid("attachment_id").into_response();
    }
    let (key, body) = match mutation_body(request).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    response(service.resize(&authenticated, &attachment_id, &key, body))
}

async fn read_output(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(attachment_id): Path<String>,
    request: Request,
) -> Response {
    if !valid_attachment_id(&attachment_id) {
        return invalid("attachment_id").into_response();
    }
    let cursor = match query_cursor(request.uri().query(), "output", "output_cursor", 1) {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    response(service.read_output(&authenticated, &attachment_id, cursor))
}

async fn read_resizes(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(attachment_id): Path<String>,
    request: Request,
) -> Response {
    if !valid_attachment_id(&attachment_id) {
        return invalid("attachment_id").into_response();
    }
    let cursor = match query_cursor(request.uri().query(), "resize", "resize_cursor", 1) {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    response(service.read_resizes(&authenticated, &attachment_id, cursor))
}

async fn detach(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(attachment_id): Path<String>,
    request: Request,
) -> Response {
    attachment_empty_mutation(
        service,
        authenticated,
        attachment_id,
        request,
        |service, authenticated, id, key| service.detach(authenticated, id, key),
    )
    .await
}

async fn events(
    Extension(service): Extension<Arc<dyn ExecService>>,
    Extension(authenticated): Extension<AuthenticatedPrincipal>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    let project_id = match ProjectId::parse(&project_id) {
        Ok(value) => value,
        Err(_) => return invalid("project_id").into_response(),
    };
    let cursor = match query_cursor(request.uri().query(), "exec", "cursor", 0) {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    response(service.events(&authenticated, project_id, cursor))
}

async fn attachment_empty_mutation<F>(
    service: Arc<dyn ExecService>,
    authenticated: AuthenticatedPrincipal,
    attachment_id: String,
    request: Request,
    action: F,
) -> Response
where
    F: FnOnce(
        &dyn ExecService,
        &AuthenticatedPrincipal,
        &str,
        &IdempotencyKey,
    ) -> Result<Value, ExecError>,
{
    if !valid_attachment_id(&attachment_id) {
        return invalid("attachment_id").into_response();
    }
    let (key, _body) = match mutation_body::<EmptyBody>(request).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    response(action(
        service.as_ref(),
        &authenticated,
        &attachment_id,
        &key,
    ))
}

async fn mutation_body<T: DeserializeOwned>(
    request: Request,
) -> Result<(IdempotencyKey, T), ProblemDetails> {
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
        .and_then(|value| IdempotencyKey::parse(value).ok())
        .ok_or_else(|| ProblemDetails::missing_idempotency_key(HIDDEN_INSTANCE))?;
    let mut bytes = to_bytes(request.into_body(), JSON_BODY_LIMIT)
        .await
        .map_err(|_| ProblemDetails::payload_too_large(HIDDEN_INSTANCE))?
        .to_vec();
    let parsed = serde_json::from_slice(&bytes)
        .map_err(|_| ProblemDetails::invalid(HIDDEN_INSTANCE, "body", "The JSON body is invalid."));
    bytes.fill(0);
    Ok((key, parsed?))
}

fn response(result: Result<Value, ExecError>) -> Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(ExecError::Unavailable) => unavailable_response(),
        Err(ExecError::PlatformUnavailable) => exec_problem(
            StatusCode::NOT_IMPLEMENTED,
            "platform_unavailable",
            "Platform unavailable",
            "Native PTY execution is unavailable on this platform.",
        ),
        Err(ExecError::Unsupported) => exec_problem(
            StatusCode::NOT_IMPLEMENTED,
            "unsupported",
            "Operation unsupported",
            "The authorized process does not support this operation.",
        ),
        Err(ExecError::OutcomeUnknown) => exec_problem(
            StatusCode::CONFLICT,
            "outcome_unknown",
            "Operation outcome unknown",
            "The operation may have taken effect and requires reconciliation.",
        ),
        Err(ExecError::CursorExpired(recovery)) => {
            let mut response = (StatusCode::GONE, Json(recovery)).into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(super::errors::PROBLEM_MEDIA_TYPE),
            );
            response
        }
        Err(error) => problem(error).into_response(),
    }
}

fn exec_problem(status: StatusCode, code: &str, title: &str, detail: &str) -> Response {
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
        HeaderValue::from_static(super::errors::PROBLEM_MEDIA_TYPE),
    );
    response
}

fn unavailable_response() -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "type": "https://kit.dev/problems/executor_unavailable",
            "title": "Executor unavailable",
            "status": 503,
            "detail": "The executor service is not available.",
            "instance": HIDDEN_INSTANCE,
            "code": "executor_unavailable",
        })),
    )
        .into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(super::errors::PROBLEM_MEDIA_TYPE),
    );
    response
}

fn problem(error: ExecError) -> ProblemDetails {
    match error {
        ExecError::NotFound => ProblemDetails::not_found(HIDDEN_INSTANCE),
        ExecError::Invalid(reason) => ProblemDetails::invalid(HIDDEN_INSTANCE, "request", reason),
        ExecError::Conflict(_) => ProblemDetails::service(
            crate::api::service::ServiceError::Conflict(String::new()),
            HIDDEN_INSTANCE,
        ),
        ExecError::Unavailable => unreachable!("unavailable errors are rendered directly"),
        ExecError::PlatformUnavailable
        | ExecError::Unsupported
        | ExecError::OutcomeUnknown
        | ExecError::CursorExpired(_) => {
            unreachable!("typed executor errors are rendered directly")
        }
        ExecError::Internal => ProblemDetails::internal(HIDDEN_INSTANCE),
    }
}

fn invalid(name: &str) -> ProblemDetails {
    ProblemDetails::invalid(
        HIDDEN_INSTANCE,
        name,
        format!("{name} must be a valid opaque identifier."),
    )
}

fn authorize_project(
    authenticated: &AuthenticatedPrincipal,
    project_id: ProjectId,
    grant: Grant,
) -> Result<(), ExecError> {
    let snapshot = authenticated.grant_snapshot();
    if snapshot.project_id() == project_id && snapshot.grants().contains(&grant) {
        Ok(())
    } else {
        Err(ExecError::NotFound)
    }
}

fn authorize_mutation_target(
    state: &AdapterState,
    authenticated: &AuthenticatedPrincipal,
    operation: &str,
    target: &str,
) -> Result<(), ExecError> {
    match operation {
        "process.cancel" | "terminal.allocate" => visible_process(
            state,
            authenticated,
            ProcessId::parse(target).map_err(|_| ExecError::NotFound)?,
            Grant::ProcessSpawn,
        )
        .map(|_| ()),
        "terminal.viewer.attach" => visible_terminal(
            state,
            authenticated,
            TerminalId::parse(target).map_err(|_| ExecError::NotFound)?,
            Grant::WorkspaceRead,
        )
        .map(|_| ()),
        "terminal.writer.claim" => visible_terminal(
            state,
            authenticated,
            TerminalId::parse(target).map_err(|_| ExecError::NotFound)?,
            Grant::ProcessSpawn,
        )
        .map(|_| ()),
        "terminal.detach" => {
            let required = attachment_mutation_grant(state, authenticated, target)?;
            visible_attachment(state, authenticated, target, required).map(|_| ())
        }
        "terminal.writer.renew"
        | "terminal.writer.release"
        | "terminal.input"
        | "terminal.resize" => {
            visible_attachment(state, authenticated, target, Grant::ProcessSpawn).map(|_| ())
        }
        _ => Err(ExecError::Internal),
    }
}

fn visible_process<'a>(
    state: &'a AdapterState,
    authenticated: &AuthenticatedPrincipal,
    process_id: ProcessId,
    grant: Grant,
) -> Result<&'a ProcessEntry, ExecError> {
    let entry = state
        .processes
        .get(&process_id)
        .ok_or(ExecError::NotFound)?;
    let snapshot = authenticated.grant_snapshot();
    if entry.principal_id == snapshot.principal_id()
        && entry.registration.project_id == snapshot.project_id()
        && snapshot.grants().contains(&grant)
    {
        Ok(entry)
    } else {
        Err(ExecError::NotFound)
    }
}

fn visible_terminal<'a>(
    state: &'a AdapterState,
    authenticated: &AuthenticatedPrincipal,
    terminal_id: TerminalId,
    grant: Grant,
) -> Result<&'a TerminalEntry, ExecError> {
    let entry = state
        .terminals
        .get(&terminal_id)
        .ok_or(ExecError::NotFound)?;
    let snapshot = authenticated.grant_snapshot();
    if entry.principal_id == snapshot.principal_id()
        && entry.project_id == snapshot.project_id()
        && snapshot.grants().contains(&grant)
    {
        Ok(entry)
    } else {
        Err(ExecError::NotFound)
    }
}

fn visible_attachment<'a>(
    state: &'a AdapterState,
    authenticated: &AuthenticatedPrincipal,
    attachment_id: &str,
    grant: Grant,
) -> Result<&'a AttachmentEntry, ExecError> {
    let entry = state
        .attachments
        .get(attachment_id)
        .ok_or(ExecError::NotFound)?;
    let snapshot = authenticated.grant_snapshot();
    if entry.principal_id == snapshot.principal_id()
        && entry.project_id == snapshot.project_id()
        && snapshot.grants().contains(&grant)
    {
        Ok(entry)
    } else {
        Err(ExecError::NotFound)
    }
}

fn visible_attachment_mut<'a>(
    state: &'a mut AdapterState,
    authenticated: &AuthenticatedPrincipal,
    attachment_id: &str,
    grant: Grant,
) -> Result<&'a mut AttachmentEntry, ExecError> {
    let entry = state
        .attachments
        .get_mut(attachment_id)
        .ok_or(ExecError::NotFound)?;
    let snapshot = authenticated.grant_snapshot();
    if entry.principal_id == snapshot.principal_id()
        && entry.project_id == snapshot.project_id()
        && snapshot.grants().contains(&grant)
    {
        Ok(entry)
    } else {
        Err(ExecError::NotFound)
    }
}

fn attachment_mutation_grant(
    state: &AdapterState,
    authenticated: &AuthenticatedPrincipal,
    attachment_id: &str,
) -> Result<Grant, ExecError> {
    let entry = state
        .attachments
        .get(attachment_id)
        .ok_or(ExecError::NotFound)?;
    let snapshot = authenticated.grant_snapshot();
    if entry.principal_id != snapshot.principal_id() || entry.project_id != snapshot.project_id() {
        return Err(ExecError::NotFound);
    }
    let required = if entry.attachment.is_writer() {
        Grant::ProcessSpawn
    } else {
        Grant::WorkspaceRead
    };
    if snapshot.grants().contains(&required) {
        Ok(required)
    } else {
        Err(ExecError::NotFound)
    }
}

fn process_resource(entry: &ProcessEntry) -> ProcessResource {
    ProcessResource {
        schema_version: SCHEMA_VERSION,
        process_id: entry.registration.claim.process_id,
        project_id: entry.registration.project_id,
        owner_kind: match entry.registration.claim.owner {
            ProcessOwnership::Attempt(_) => ProcessOwnerKind::Attempt,
            ProcessOwnership::DaemonService(_) => ProcessOwnerKind::DaemonService,
        },
        execution_id: entry.registration.execution_id,
        state: entry.registration.state,
        terminal_transport: entry.registration.terminal_request.transport,
    }
}

fn terminal_resource(
    entry: &TerminalEntry,
    snapshot: &crate::executor::terminal::TerminalSnapshot,
) -> TerminalResource {
    TerminalResource {
        schema_version: SCHEMA_VERSION,
        terminal_id: snapshot.terminal_id,
        process_id: entry.process_id,
        project_id: entry.project_id,
        lifecycle: snapshot.lifecycle,
        columns: snapshot.size.columns,
        rows: snapshot.size.rows,
        next_output_cursor: output_cursor(snapshot.next_output_sequence),
        next_resize_cursor: resize_cursor(snapshot.next_resize_sequence),
        retained_output_bytes: snapshot.retained_output_bytes,
        writer_epoch: snapshot.writer.as_ref().map(|writer| writer.epoch),
    }
}

fn attachment_resource(attachment_id: &str, entry: &AttachmentEntry) -> AttachmentResource {
    AttachmentResource {
        schema_version: SCHEMA_VERSION,
        attachment_id: attachment_id.to_owned(),
        terminal_id: entry.attachment.terminal_id(),
        role: if entry.attachment.is_writer() {
            "writer"
        } else {
            "viewer"
        },
        writer_epoch: entry.attachment.writer_epoch(),
        expires_at_millis: entry.expires_at_millis,
    }
}

fn receipt(operation: &'static str, resource: Option<Value>) -> Value {
    serde_json::to_value(MutationReceipt {
        schema_version: SCHEMA_VERSION,
        operation,
        changed: true,
        replayed: false,
        resource,
    })
    .expect("receipt serializes")
}

fn terminal_error(error: TerminalError) -> ExecError {
    match error {
        TerminalError::NotFound
        | TerminalError::PermissionDenied
        | TerminalError::StaleProcessClaim => ExecError::NotFound,
        TerminalError::InvalidRequest(_)
        | TerminalError::InvalidCursor { .. }
        | TerminalError::ReadOnlyViewer => ExecError::Invalid("terminal request is invalid"),
        TerminalError::WriterOccupied
        | TerminalError::StaleWriter
        | TerminalError::LeaseExpired
        | TerminalError::TerminalInactive => {
            ExecError::Conflict("terminal state conflicts with the request")
        }
        TerminalError::DaemonUnavailable => ExecError::Unavailable,
        TerminalError::PlatformUnavailable => ExecError::PlatformUnavailable,
        TerminalError::AttachmentLimit
        | TerminalError::SequenceExhausted
        | TerminalError::EntropyUnavailable
        | TerminalError::Driver(_)
        | TerminalError::Persistence(_)
        | TerminalError::StatePoisoned
        | TerminalError::UnsanitizedCapture => ExecError::Internal,
    }
}

fn cancellation_error(error: CancellationError) -> ExecError {
    match error {
        CancellationError::Store(
            CancellationStoreError::Unauthorized
            | CancellationStoreError::StaleOwner
            | CancellationStoreError::NotFound,
        ) => ExecError::NotFound,
        CancellationError::Store(
            CancellationStoreError::IdempotencyConflict | CancellationStoreError::PhaseConflict,
        ) => ExecError::Conflict("cancellation state conflicts with the request"),
        CancellationError::Store(CancellationStoreError::Unavailable(_)) => ExecError::Unavailable,
        CancellationError::InvalidTimeout => ExecError::Internal,
    }
}

fn append_event(
    state: &mut AdapterState,
    event_type: &'static str,
    principal_id: PrincipalId,
    project_id: ProjectId,
    resource_id: String,
) -> Result<(), ExecError> {
    let next = state
        .next_events
        .entry((principal_id, project_id))
        .or_default();
    let position = *next;
    *next = next.checked_add(1).ok_or(ExecError::Internal)?;
    state.events.push_back((
        position,
        ExecEvent {
            schema_version: SCHEMA_VERSION,
            cursor: event_cursor(position),
            event_type: event_type.to_owned(),
            principal_id,
            project_id,
            resource_id,
        },
    ));
    while state.events.len() > MAX_EVENTS {
        state.events.pop_front();
    }
    Ok(())
}

fn request_digest(operation: &str, target: &str, request: &Value) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(operation.as_bytes());
    hasher.update(&[0]);
    hasher.update(target.as_bytes());
    hasher.update(&[0]);
    hasher.update(&serde_json::to_vec(request).expect("JSON value serializes"));
    *hasher.finalize().as_bytes()
}

fn effect_failure_unknown(operation: &str, error: &ExecError) -> bool {
    if matches!(error, ExecError::OutcomeUnknown) {
        return true;
    }
    matches!(error, ExecError::Unavailable | ExecError::Internal)
        && matches!(
            operation,
            "process.cancel"
                | "terminal.allocate"
                | "terminal.viewer.attach"
                | "terminal.writer.claim"
                | "terminal.input"
                | "terminal.resize"
                | "terminal.detach"
        )
}

fn encode_durable_error(error: &ExecError) -> String {
    match error {
        ExecError::NotFound => "not_found",
        ExecError::Invalid(_) => "invalid",
        ExecError::Conflict(_) => "conflict",
        ExecError::Unavailable => "unavailable",
        ExecError::PlatformUnavailable => "platform_unavailable",
        ExecError::Unsupported => "unsupported",
        ExecError::OutcomeUnknown => "outcome_unknown",
        ExecError::CursorExpired(_) | ExecError::Internal => "internal",
    }
    .to_owned()
}

fn decode_durable_error(value: &str) -> Result<ExecError, ExecError> {
    match value {
        "not_found" => Ok(ExecError::NotFound),
        "not_applied" => Ok(ExecError::Conflict(
            "terminal input was resolved as not applied",
        )),
        "invalid" => Ok(ExecError::Invalid("request is invalid")),
        "conflict" => Ok(ExecError::Conflict("request conflicts with current state")),
        "unavailable" => Ok(ExecError::Unavailable),
        "platform_unavailable" => Ok(ExecError::PlatformUnavailable),
        "unsupported" => Ok(ExecError::Unsupported),
        "outcome_unknown" => Ok(ExecError::OutcomeUnknown),
        "internal" => Ok(ExecError::Internal),
        _ => Err(ExecError::Internal),
    }
}

fn new_attachment_id() -> Result<String, ExecError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| ExecError::Internal)?;
    let mut id = String::from("attachment_");
    for byte in bytes {
        write!(id, "{byte:02x}").expect("string writes do not fail");
    }
    Ok(id)
}

fn valid_attachment_id(value: &str) -> bool {
    value.len() == 43
        && value.strip_prefix("attachment_").is_some_and(|tail| {
            tail.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
}

fn now_millis() -> Result<u64, ExecError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ExecError::Internal)?
        .as_millis()
        .try_into()
        .map_err(|_| ExecError::Internal)
}

fn output_cursor(value: u64) -> String {
    format!("output_{value:016x}")
}
fn resize_cursor(value: u64) -> String {
    format!("resize_{value:016x}")
}
fn event_cursor(value: u64) -> String {
    format!("exec_{value:016x}")
}

fn query_cursor(
    query: Option<&str>,
    prefix: &str,
    name: &str,
    default: u64,
) -> Result<u64, ProblemDetails> {
    let Some(value) = query.and_then(|query| {
        query.split('&').find_map(|pair| {
            pair.split_once('=')
                .filter(|(key, _)| *key == "cursor")
                .map(|(_, value)| value)
        })
    }) else {
        return Ok(default);
    };
    value
        .strip_prefix(prefix)
        .and_then(|value| value.strip_prefix('_'))
        .and_then(|value| u64::from_str_radix(value, 16).ok())
        .ok_or_else(|| {
            ProblemDetails::invalid(
                HIDDEN_INSTANCE,
                name,
                format!("{name} must be an opaque cursor returned by this API."),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrelated_unsupported_driver_error_is_not_platform_unavailable() {
        assert_eq!(
            terminal_error(TerminalError::Driver(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "unrelated driver feature",
            ))),
            ExecError::Internal
        );
    }
}
