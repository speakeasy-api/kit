use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::Serialize;

use crate::api::service::AttemptDriverClaim;
use crate::domain::commands::ExpectedVersion;
use crate::domain::events::{
    CommitPosition, EntityId, EventType, SchemaVersion, StreamSequence, TraceId, UtcDateTime,
};
use crate::domain::ids::{AttemptId, CommandId, EventId, PrincipalId, ProjectId, RunId};
use crate::domain::secret::SecretCustody;

#[cfg(any(test, debug_assertions))]
use super::idempotency::ClaimOutcome;
use super::idempotency::{
    CanonicalRequestDigest, IdempotencyClaim, IdempotencyKey, IdempotencyScope, IdempotencyStatus,
    IdempotentResponse, PendingToken, StoredRecord, StoredState,
};
use super::idempotency::{insert_pending, insert_position, lookup, positions, set_terminal};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SCHEMA_VERSION: i64 = 1;
const TOOL_LEARNING_EVENT: &str = "tool_learning.recorded";
const MAX_RETAINED_EXPORTED_LEARNING: i64 = 10_000;
const MAX_LEARNING_OUTBOX_ROWS: i64 = 30_000;
const MAX_PROJECT_LEARNING_OUTBOX_ROWS: i64 = 30_000;
const RESERVED_TERMINAL_OUTBOX_ROWS: i64 = 20_000;
const MAX_LEARNING_OUTBOX_BYTES: i64 = 128 * 1024 * 1024;
const MAX_PROJECT_LEARNING_OUTBOX_BYTES: i64 = 128 * 1024 * 1024;
const RESERVED_TERMINAL_OUTBOX_BYTES: i64 = 64 * 1024 * 1024;
const MAX_LEARNING_MARKER_ROWS: i64 = 10_000;
const MAX_PROJECT_LEARNING_MARKER_ROWS: i64 = 5_000;
const MAX_LEARNING_MARKER_BYTES: i64 = 4 * 1024 * 1024;
const MAX_PROJECT_LEARNING_MARKER_BYTES: i64 = 2 * 1024 * 1024;
const RESERVED_TERMINAL_MARKER_ROWS: i64 = 2;
const RESERVED_TERMINAL_MARKER_BYTES: i64 = 16 * 1024;
const MAX_CATALOG_STATS_ROWS: i64 = 30_000;
const MAX_PROJECT_CATALOG_STATS_ROWS: i64 = 10_000;
const MAX_CATALOG_STATS_BYTES: i64 = 32 * 1024 * 1024;
const MAX_PROJECT_CATALOG_STATS_BYTES: i64 = 16 * 1024 * 1024;
const MAX_CATALOG_SNAPSHOT_ROWS: i64 = 30_000;
const MAX_PROJECT_CATALOG_SNAPSHOT_ROWS: i64 = 10_000;
const MAX_CATALOG_SNAPSHOT_BYTES: i64 = 64 * 1024 * 1024;
const MAX_PROJECT_CATALOG_SNAPSHOT_BYTES: i64 = 32 * 1024 * 1024;
const MAX_RETAINED_EXPORTED_SNAPSHOTS: i64 = 10_000;
const MAX_DISCOVERY_BINDING_ROWS: i64 = 30_000;
const MAX_PROJECT_DISCOVERY_BINDING_ROWS: i64 = 10_000;
const MAX_DISCOVERY_BINDING_BYTES: i64 = 8 * 1024 * 1024;
const MAX_PROJECT_DISCOVERY_BINDING_BYTES: i64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExpectedStreamVersion {
    pub stream: EntityId,
    pub version: ExpectedVersion,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewEvent {
    pub id: EventId,
    pub stream: EntityId,
    pub event_type: EventType,
    pub schema_version: SchemaVersion,
    pub occurred_at: UtcDateTime,
    pub causation_id: CommandId,
    pub correlation_id: EntityId,
    pub attempt_id: Option<AttemptId>,
    pub trace_id: TraceId,
    pub payload: Vec<u8>,
    pub artifacts: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredEvent {
    pub event: NewEvent,
    pub sequence: StreamSequence,
    pub commit_position: CommitPosition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LearningOutboxRow {
    pub cursor: u64,
    pub frame_id: String,
    pub event_id: EventId,
    pub run_id: String,
    pub project: String,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DurableCatalogStats {
    pub project: String,
    pub run: String,
    pub binding: String,
    pub attempts: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub outcome_unknown: u64,
    pub source_event: String,
    pub revision: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogStatsSnapshot {
    pub project: String,
    pub raw_run_id: RunId,
    pub generation: u64,
    pub overlay_digest: String,
    pub frame_id: String,
    pub payload_bytes: u64,
    pub payload: Vec<u8>,
    pub entries: Vec<DurableCatalogStats>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionRegistryCommit {
    Committed,
    Stale,
    LimitExceeded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendCommand {
    pub idempotency_scope: IdempotencyScope,
    pub idempotency_key: IdempotencyKey,
    pub request_digest: CanonicalRequestDigest,
    pub claim: Option<IdempotencyClaim>,
    pub driver_claim: Option<AttemptDriverClaim>,
    pub allow_quiescent_driver_claim: bool,
    pub expected_versions: Vec<ExpectedStreamVersion>,
    pub events: Vec<NewEvent>,
    pub response: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppendOutcome {
    Committed(IdempotentResponse),
    Replayed(IdempotentResponse),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingArtifactPublication {
    pub reference: String,
    pub digest: String,
    pub purpose: String,
    pub subject_id: String,
    pub principal_id: String,
    pub project_id: String,
    pub run_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrashPoint {
    AfterTransactionBegin,
    AfterIdempotencyCheck,
    AfterExpectedVersionCheck,
    AfterEventInsert,
    AfterStreamHeadsUpdate,
    AfterWatermarkUpdate,
    BeforeIdempotencyTerminal,
    AfterIdempotencyTerminal,
    BeforeCommit,
    AfterCommit,
}

#[derive(Debug)]
pub enum StoreError {
    Busy,
    Database(rusqlite::Error),
    InvalidDatabaseLocation,
    WalUnavailable(String),
    UnsupportedSchemaVersion(i64),
    InvalidRequest(&'static str),
    CorruptData(&'static str),
    ExpectedVersion {
        stream: String,
        expected: u64,
        actual: u64,
    },
    DuplicateEvent(String),
    IdempotencyConflict(IdempotencyKey),
    IdempotencyPending(IdempotencyKey),
    StaleDriverClaim,
    PositionExhausted,
    RandomnessUnavailable,
    InjectedCrash(CrashPoint),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("SQLite store remained busy past its bounded timeout"),
            Self::Database(error) => write!(f, "SQLite store error: {error}"),
            Self::InvalidDatabaseLocation => {
                f.write_str("SQLite WAL store requires a local-disk database path")
            }
            Self::WalUnavailable(mode) => {
                write!(f, "SQLite WAL mode unavailable (selected {mode})")
            }
            Self::UnsupportedSchemaVersion(version) => {
                write!(f, "unsupported SQLite store schema version {version}")
            }
            Self::InvalidRequest(message) => write!(f, "invalid append request: {message}"),
            Self::CorruptData(message) => write!(f, "corrupt SQLite store data: {message}"),
            Self::ExpectedVersion {
                stream,
                expected,
                actual,
            } => write!(
                f,
                "stream {stream} expected version {expected}, actual version {actual}"
            ),
            Self::DuplicateEvent(event_id) => write!(f, "event {event_id} already exists"),
            Self::IdempotencyConflict(key) => {
                write!(f, "idempotency key {key} was used for a different request")
            }
            Self::IdempotencyPending(key) => {
                write!(f, "idempotency key {key} has a pending request")
            }
            Self::StaleDriverClaim => f.write_str("attempt driver claim is stale or inactive"),
            Self::PositionExhausted => f.write_str("SQLite event position space exhausted"),
            Self::RandomnessUnavailable => {
                f.write_str("secure randomness unavailable for idempotency claim")
            }
            Self::InjectedCrash(point) => write!(f, "injected store crash at {point:?}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        match &error {
            rusqlite::Error::SqliteFailure(failure, _)
                if matches!(
                    failure.code,
                    ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
                ) =>
            {
                Self::Busy
            }
            _ => Self::Database(error),
        }
    }
}

pub struct SqliteStore {
    connection: Connection,
    pending_artifact_publication: Option<PendingArtifactPublication>,
    custody: SecretCustody,
    _authority: crate::runtime::daemon::ControlPlaneAuthority,
}

impl SqliteStore {
    pub(crate) fn open(
        path: impl AsRef<Path>,
        authority: &crate::runtime::daemon::ControlPlaneAuthority,
    ) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() || path == Path::new(":memory:") {
            return Err(StoreError::InvalidDatabaseLocation);
        }
        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        configure_connection(&connection)?;
        migrate(&mut connection)?;
        Ok(Self {
            connection,
            pending_artifact_publication: None,
            custody: authority.secret_custody(),
            _authority: authority.clone(),
        })
    }

    pub(crate) fn validate(path: impl AsRef<Path>) -> Result<(), StoreError> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() || path == Path::new(":memory:") {
            return Err(StoreError::InvalidDatabaseLocation);
        }
        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        configure_connection(&connection)?;
        migrate(&mut connection)
    }

    #[allow(clippy::type_complexity)]
    pub fn extension_registry_state(&mut self) -> Result<(u64, Vec<(u64, Vec<u8>)>), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let revision = transaction.query_row(
            "SELECT revision FROM capability_extension_registry_state WHERE singleton=1",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let snapshots = {
            let mut statement = transaction.prepare(
                "SELECT revision, snapshot FROM capability_extension_registry
                 ORDER BY principal_id, project_id",
            )?;
            statement
                .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?
        };
        transaction.commit()?;
        let revision = u64::try_from(revision)
            .map_err(|_| StoreError::CorruptData("negative global registry revision"))?;
        let snapshots = snapshots
            .into_iter()
            .map(|(revision, snapshot)| {
                Ok((
                    u64::try_from(revision)
                        .map_err(|_| StoreError::CorruptData("negative registry revision"))?,
                    snapshot,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok((revision, snapshots))
    }

    pub fn extension_registry_snapshots(&mut self) -> Result<Vec<(u64, Vec<u8>)>, StoreError> {
        self.extension_registry_state()
            .map(|(_, snapshots)| snapshots)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn persist_extension_registry_snapshot(
        &mut self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        expected_revision: u64,
        revision: u64,
        snapshot: &[u8],
        entry_count: usize,
        max_entries: usize,
        max_snapshot_bytes: usize,
    ) -> Result<ExtensionRegistryCommit, StoreError> {
        if revision
            != expected_revision
                .checked_add(1)
                .ok_or(StoreError::PositionExhausted)?
        {
            return Err(StoreError::InvalidRequest(
                "extension registry revision is not monotonic",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (stored_revision, total_entries, total_bytes) = transaction.query_row(
            "SELECT revision, entry_count, snapshot_bytes
             FROM capability_extension_registry_state WHERE singleton=1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        if stored_revision
            != i64::try_from(expected_revision).map_err(|_| StoreError::PositionExhausted)?
        {
            return Ok(ExtensionRegistryCommit::Stale);
        }
        let (previous_entries, previous_bytes) = transaction
            .query_row(
                "SELECT entry_count, length(snapshot) FROM capability_extension_registry
                 WHERE principal_id=?1 AND project_id=?2",
                params![principal_id.to_string(), project_id.to_string()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .unwrap_or((0, 0));
        let next_entries = total_entries
            .checked_sub(previous_entries)
            .and_then(|value| value.checked_add(i64::try_from(entry_count).ok()?))
            .ok_or(StoreError::PositionExhausted)?;
        let next_bytes = total_bytes
            .checked_sub(previous_bytes)
            .and_then(|value| value.checked_add(i64::try_from(snapshot.len()).ok()?))
            .ok_or(StoreError::PositionExhausted)?;
        if next_entries > i64::try_from(max_entries).map_err(|_| StoreError::PositionExhausted)?
            || next_bytes
                > i64::try_from(max_snapshot_bytes).map_err(|_| StoreError::PositionExhausted)?
        {
            return Ok(ExtensionRegistryCommit::LimitExceeded);
        }
        transaction.execute(
            "UPDATE capability_extension_registry_state
             SET revision=?1, entry_count=?2, snapshot_bytes=?3
             WHERE singleton=1 AND revision=?4",
            params![
                i64::try_from(revision).map_err(|_| StoreError::PositionExhausted)?,
                next_entries,
                next_bytes,
                stored_revision,
            ],
        )?;
        transaction.execute(
            "INSERT INTO capability_extension_registry
                 (principal_id, project_id, revision, snapshot, entry_count)
             VALUES (?1, ?2, ?4, ?5, ?6)
             ON CONFLICT(principal_id, project_id) DO UPDATE SET
                 revision=excluded.revision, snapshot=excluded.snapshot,
                 entry_count=excluded.entry_count",
            params![
                principal_id.to_string(),
                project_id.to_string(),
                i64::try_from(expected_revision).map_err(|_| StoreError::PositionExhausted)?,
                i64::try_from(revision).map_err(|_| StoreError::PositionExhausted)?,
                snapshot,
                i64::try_from(entry_count).map_err(|_| StoreError::PositionExhausted)?,
            ],
        )?;
        transaction.commit()?;
        Ok(ExtensionRegistryCommit::Committed)
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn claim(
        &mut self,
        scope: IdempotencyScope,
        key: IdempotencyKey,
        digest: CanonicalRequestDigest,
    ) -> Result<ClaimOutcome, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let outcome = match lookup(&transaction, &scope, &key)? {
            Some(record) => match checked_record(record, digest, &key)? {
                CheckedRecord::Pending(_) => ClaimOutcome::Pending,
                CheckedRecord::Terminal(response) => ClaimOutcome::Replay(IdempotentResponse {
                    response,
                    commit_positions: checked_positions(positions(&transaction, &scope, &key)?)?,
                }),
            },
            None => {
                let token =
                    PendingToken::generate().map_err(|_| StoreError::RandomnessUnavailable)?;
                insert_pending(&transaction, &scope, &key, digest, &token)?;
                ClaimOutcome::Claimed(IdempotencyClaim {
                    scope,
                    key,
                    digest,
                    token,
                })
            }
        };
        transaction.commit()?;
        Ok(outcome)
    }

    pub fn idempotency_status(
        &mut self,
        scope: &IdempotencyScope,
        key: &IdempotencyKey,
    ) -> Result<IdempotencyStatus, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let status = match lookup(&transaction, scope, key)? {
            None => IdempotencyStatus::Missing,
            Some(record) => {
                let digest = digest_from_bytes(&record.digest)?;
                match record.state {
                    StoredState::Pending(_) => IdempotencyStatus::Pending {
                        request_digest: digest,
                    },
                    StoredState::Terminal(response) => IdempotencyStatus::Terminal {
                        request_digest: digest,
                        result: IdempotentResponse {
                            response,
                            commit_positions: checked_positions(positions(
                                &transaction,
                                scope,
                                key,
                            )?)?,
                        },
                    },
                    StoredState::Invalid => {
                        return Err(StoreError::CorruptData("invalid idempotency state"));
                    }
                }
            }
        };
        transaction.commit()?;
        Ok(status)
    }

    pub fn verify_driver_claim(&mut self, claim: AttemptDriverClaim) -> Result<(), StoreError> {
        self.verify_driver_claim_inner(claim, false)
    }

    pub(crate) fn verify_quiescent_driver_claim(
        &mut self,
        claim: AttemptDriverClaim,
    ) -> Result<(), StoreError> {
        self.verify_driver_claim_inner(claim, true)
    }

    fn verify_driver_claim_inner(
        &mut self,
        claim: AttemptDriverClaim,
        allow_quiescent: bool,
    ) -> Result<(), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        guard_driver_claim(&transaction, claim, allow_quiescent)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn quiesce_driver_claim(&mut self, claim: AttemptDriverClaim) -> Result<(), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        guard_driver_claim(&transaction, claim, true)?;
        let changed = transaction.execute(
            "UPDATE attempt_driver_claims SET quiescent = 1
             WHERE run_id = ?1 AND attempt_id = ?2 AND principal_id = ?3
               AND fence = ?4 AND lease_version = ?5 AND quiescent = 0",
            params![
                claim.run_id.to_string(),
                claim.attempt_id.to_string(),
                claim.principal_id.to_string(),
                i64::try_from(claim.fence.get()).map_err(|_| StoreError::StaleDriverClaim)?,
                i64::try_from(claim.lease_version).map_err(|_| StoreError::StaleDriverClaim)?,
            ],
        )?;
        if changed > 1 {
            return Err(StoreError::StaleDriverClaim);
        }
        transaction.commit()?;
        Ok(())
    }

    #[cfg(any(test, debug_assertions))]
    pub fn install_driver_claim_for_test(
        &mut self,
        mut claim: AttemptDriverClaim,
    ) -> Result<AttemptDriverClaim, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now: i64 = transaction.query_row(
            "SELECT CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)",
            [],
            |row| row.get(0),
        )?;
        claim.expires_at_unix_micros = now + 3_600_000_000;
        transaction.execute(
            "INSERT INTO attempt_driver_fences (run_id, fence, lease_version)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(run_id) DO UPDATE SET fence = excluded.fence,
               lease_version = excluded.lease_version",
            params![
                claim.run_id.to_string(),
                claim.fence.get(),
                claim.lease_version,
            ],
        )?;
        transaction.execute(
            "INSERT INTO attempt_driver_claims
               (run_id, attempt_id, principal_id, fence, lease_version,
                expires_at_unix_micros, quiescent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
             ON CONFLICT(run_id) DO UPDATE SET attempt_id = excluded.attempt_id,
               principal_id = excluded.principal_id, fence = excluded.fence,
               lease_version = excluded.lease_version,
               expires_at_unix_micros = excluded.expires_at_unix_micros, quiescent = 0",
            params![
                claim.run_id.to_string(),
                claim.attempt_id.to_string(),
                claim.principal_id.to_string(),
                claim.fence.get(),
                claim.lease_version,
                claim.expires_at_unix_micros,
            ],
        )?;
        transaction.commit()?;
        Ok(claim)
    }

    pub(crate) fn append(&mut self, command: AppendCommand) -> Result<AppendOutcome, StoreError> {
        self.append_with_hook(command, |_| false)
    }

    pub(crate) fn append_with_hook(
        &mut self,
        command: AppendCommand,
        crash: impl FnMut(CrashPoint) -> bool,
    ) -> Result<AppendOutcome, StoreError> {
        self.append_with_options(command, crash, None, None)
    }

    pub(crate) fn append_with_discovery_binding(
        &mut self,
        command: AppendCommand,
        project: &str,
        run_id: RunId,
        binding_id: &str,
    ) -> Result<AppendOutcome, StoreError> {
        self.append_with_options(
            command,
            |_| false,
            Some((project, run_id, binding_id)),
            None,
        )
    }

    pub(crate) fn append_with_learning_reconciliation(
        &mut self,
        command: AppendCommand,
        project: &str,
        operation_id: &str,
        required: bool,
    ) -> Result<AppendOutcome, StoreError> {
        self.append_with_options(
            command,
            |_| false,
            None,
            Some((project, operation_id, required)),
        )
    }

    fn append_with_options(
        &mut self,
        command: AppendCommand,
        mut crash: impl FnMut(CrashPoint) -> bool,
        discovery_binding: Option<(&str, RunId, &str)>,
        reconciliation: Option<(&str, &str, bool)>,
    ) -> Result<AppendOutcome, StoreError> {
        reject_secret_authority_identifiers(&self.custody, &command.events)?;
        validate_command(&command)?;
        let publication = self.pending_artifact_publication.take();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        inject(&mut crash, CrashPoint::AfterTransactionBegin)?;
        if let Some(claim) = command.driver_claim {
            guard_driver_claim(&transaction, claim, command.allow_quiescent_driver_claim)?;
        }

        let scope = &command.idempotency_scope;
        let key = &command.idempotency_key;
        match lookup(&transaction, scope, key)? {
            Some(record) => match checked_record(record, command.request_digest, key)? {
                CheckedRecord::Terminal(response) => {
                    let result = IdempotentResponse {
                        response,
                        commit_positions: checked_positions(positions(&transaction, scope, key)?)?,
                    };
                    journal_artifact_publication(&transaction, &command, publication.as_ref())?;
                    transaction.commit()?;
                    return Ok(AppendOutcome::Replayed(result));
                }
                CheckedRecord::Pending(token) => {
                    if command
                        .claim
                        .as_ref()
                        .is_some_and(|claim| claim.token != token)
                    {
                        return Err(StoreError::IdempotencyPending(key.clone()));
                    }
                }
            },
            None => {
                if command.claim.is_some() {
                    return Err(StoreError::InvalidRequest(
                        "idempotency claim does not exist in the store",
                    ));
                }
                let token =
                    PendingToken::generate().map_err(|_| StoreError::RandomnessUnavailable)?;
                insert_pending(&transaction, scope, key, command.request_digest, &token)?;
            }
        }
        inject(&mut crash, CrashPoint::AfterIdempotencyCheck)?;

        let mut versions = check_expected_versions(&transaction, &command)?;
        inject(&mut crash, CrashPoint::AfterExpectedVersionCheck)?;
        let mut watermark: i64 = transaction.query_row(
            "SELECT position FROM commit_watermark WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        if watermark < 0 {
            return Err(StoreError::CorruptData("negative commit watermark"));
        }

        let mut committed_positions = Vec::with_capacity(command.events.len());
        if reconciliation.is_none() {
            admit_learning_outbox(&transaction, &command.events)?;
        }
        let mut reconciliation_range = None::<(i64, i64, i64, i64)>;
        for event in &command.events {
            let stream = event.stream.to_string();
            let sequence = versions.get_mut(&stream).ok_or(StoreError::InvalidRequest(
                "event stream has no expected version",
            ))?;
            *sequence = sequence
                .checked_add(1)
                .ok_or(StoreError::PositionExhausted)?;
            watermark = watermark
                .checked_add(1)
                .ok_or(StoreError::PositionExhausted)?;
            ensure_event_absent(&transaction, event.id)?;
            insert_event_row(&transaction, event, *sequence, watermark)?;
            if event.event_type.as_str() == TOOL_LEARNING_EVENT {
                let learning: crate::telemetry::tool_learning::ToolLearningEvent =
                    serde_json::from_slice(&event.payload)
                        .map_err(|_| StoreError::InvalidRequest("invalid tool learning payload"))?;
                if reconciliation.is_some() {
                    let bytes = i64::try_from(event.payload.len())
                        .map_err(|_| StoreError::PositionExhausted)?;
                    reconciliation_range = Some(match reconciliation_range {
                        Some((first, _, rows, total)) => (
                            first,
                            watermark,
                            rows.checked_add(1).ok_or(StoreError::PositionExhausted)?,
                            total
                                .checked_add(bytes)
                                .ok_or(StoreError::PositionExhausted)?,
                        ),
                        None => (watermark, watermark, 1, bytes),
                    });
                } else {
                    insert_learning_indexes(&transaction, event, &learning)?;
                }
            }
            insert_position(
                &transaction,
                scope,
                key,
                committed_positions.len(),
                watermark,
            )?;
            committed_positions.push(
                CommitPosition::new(watermark as u64)
                    .map_err(|_| StoreError::CorruptData("invalid commit position"))?,
            );
            inject(&mut crash, CrashPoint::AfterEventInsert)?;
        }

        if let (Some((project, operation_id, required)), Some(range)) =
            (reconciliation, reconciliation_range)
        {
            upsert_learning_marker(&transaction, project, operation_id, required, Some(range))?;
        }

        for (stream, version) in versions {
            transaction.execute(
                "UPDATE stream_heads SET version = ?2 WHERE stream = ?1",
                params![stream, version],
            )?;
        }
        inject(&mut crash, CrashPoint::AfterStreamHeadsUpdate)?;
        transaction.execute(
            "UPDATE commit_watermark SET position = ?1 WHERE singleton = 1",
            [watermark],
        )?;
        inject(&mut crash, CrashPoint::AfterWatermarkUpdate)?;
        if let Some((project, run_id, binding_id)) = discovery_binding {
            persist_discovery_binding(&transaction, project, run_id, binding_id)?;
        }
        inject(&mut crash, CrashPoint::BeforeIdempotencyTerminal)?;
        if !set_terminal(&transaction, scope, key, &command.response)? {
            return Err(StoreError::CorruptData(
                "pending idempotency record disappeared during append",
            ));
        }
        inject(&mut crash, CrashPoint::AfterIdempotencyTerminal)?;
        journal_artifact_publication(&transaction, &command, publication.as_ref())?;
        inject(&mut crash, CrashPoint::BeforeCommit)?;
        transaction.commit()?;
        inject(&mut crash, CrashPoint::AfterCommit)?;

        Ok(AppendOutcome::Committed(IdempotentResponse {
            response: command.response,
            commit_positions: committed_positions,
        }))
    }

    pub(crate) fn reserve_learning_reconciliation(
        &mut self,
        project: &str,
        operation_id: &str,
    ) -> Result<(), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        upsert_learning_marker(&transaction, project, operation_id, true, None)?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn arm_artifact_publication(
        &mut self,
        publication: PendingArtifactPublication,
    ) -> Result<(), StoreError> {
        if self.pending_artifact_publication.is_some() {
            return Err(StoreError::InvalidRequest(
                "artifact publication is already armed",
            ));
        }
        self.pending_artifact_publication = Some(publication);
        Ok(())
    }

    pub(crate) fn clear_artifact_publication(&mut self, reference: &str) -> Result<(), StoreError> {
        self.connection.execute(
            "DELETE FROM artifact_publication_journal WHERE artifact_reference=?1",
            [reference],
        )?;
        Ok(())
    }

    pub fn events(&self) -> Result<Vec<StoredEvent>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT event_id, stream, sequence, commit_position, event_type, schema_version,
                    occurred_at, causation_id, correlation_id, attempt_id, trace_id,
                    payload, artifacts
             FROM events
             WHERE commit_position <= (SELECT position FROM commit_watermark WHERE singleton = 1)
             ORDER BY commit_position",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, u16>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, Vec<u8>>(11)?,
                row.get::<_, Vec<u8>>(12)?,
            ))
        })?;
        rows.map(|row| decode_event(row?)).collect()
    }

    pub(crate) fn invocation_was_dispatched(
        &self,
        invocation_id: crate::domain::ids::ToolCallId,
    ) -> Result<bool, StoreError> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM events
                 WHERE stream=?1 AND event_type='capability.invocation_dispatched'
                   AND commit_position <= (SELECT position FROM commit_watermark WHERE singleton=1))",
                [EntityId::ToolCall(invocation_id).to_string()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub(crate) fn events_for_correlation(
        &self,
        correlation: EntityId,
        event_type: &str,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT event_id, stream, sequence, commit_position, event_type, schema_version,
                    occurred_at, causation_id, correlation_id, attempt_id, trace_id,
                    payload, artifacts
             FROM events
             WHERE correlation_id=?1 AND event_type=?2
               AND commit_position <= (SELECT position FROM commit_watermark WHERE singleton=1)
             ORDER BY commit_position LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                correlation.to_string(),
                event_type,
                i64::try_from(limit).map_err(|_| StoreError::PositionExhausted)?,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u16>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, Vec<u8>>(11)?,
                    row.get::<_, Vec<u8>>(12)?,
                ))
            },
        )?;
        rows.map(|row| decode_event(row?)).collect()
    }

    pub(crate) fn stream_version(&self, stream: EntityId) -> Result<u64, StoreError> {
        let version = self
            .connection
            .query_row(
                "SELECT version FROM stream_heads WHERE stream=?1",
                [stream.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .or_else(|error| match error {
                rusqlite::Error::QueryReturnedNoRows => Ok(0),
                error => Err(error),
            })?;
        u64::try_from(version).map_err(|_| StoreError::CorruptData("negative stream version"))
    }

    pub fn committed_through(&self) -> Result<u64, StoreError> {
        let position: i64 = self.connection.query_row(
            "SELECT position FROM commit_watermark WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        u64::try_from(position).map_err(|_| StoreError::CorruptData("negative commit watermark"))
    }

    pub fn pending_learning_outbox(
        &self,
        project: &str,
        limit: usize,
    ) -> Result<Vec<LearningOutboxRow>, StoreError> {
        if limit == 0 || limit > 10_000 {
            return Err(StoreError::InvalidRequest("invalid learning outbox limit"));
        }
        let mut statement = self.connection.prepare(
            "SELECT cursor,frame_id,event_id,run_id,project,payload
             FROM tool_learning_outbox WHERE project=?1 AND exported=0 ORDER BY cursor LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![
                project,
                i64::try_from(limit).map_err(|_| StoreError::PositionExhausted)?
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Vec<u8>>(5)?,
                ))
            },
        )?;
        rows.map(|row| {
            let (cursor, frame_id, event_id, run_id, project, payload) = row?;
            Ok(LearningOutboxRow {
                cursor: u64::try_from(cursor)
                    .map_err(|_| StoreError::CorruptData("invalid learning outbox cursor"))?,
                frame_id,
                event_id: event_id.parse().map_err(|_| {
                    StoreError::CorruptData("invalid learning outbox event identifier")
                })?,
                run_id,
                project,
                payload,
            })
        })
        .collect()
    }

    pub(crate) fn pending_learning_projects(&self) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT project FROM (
                 SELECT project FROM tool_learning_outbox WHERE exported=0
                 UNION SELECT project FROM tool_learning_reconciliation
             ) ORDER BY project LIMIT 10001",
        )?;
        let projects = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if projects.len() > 10_000 {
            return Err(StoreError::CorruptData(
                "pending learning project capacity exceeded",
            ));
        }
        Ok(projects)
    }

    pub(crate) fn reconcile_learning_markers(
        &mut self,
        project: &str,
        limit: usize,
    ) -> Result<usize, StoreError> {
        if limit == 0 || limit > 10_000 {
            return Err(StoreError::InvalidRequest(
                "invalid learning reconciliation limit",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut marker_statement = transaction.prepare(
            "SELECT operation_id,first_position,last_position,row_count FROM tool_learning_reconciliation
             WHERE project=?1 AND first_position IS NOT NULL
             ORDER BY created_at,operation_id LIMIT ?2",
        )?;
        let markers = marker_statement
            .query_map(
                params![
                    project,
                    i64::try_from(limit).map_err(|_| StoreError::PositionExhausted)?
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(marker_statement);
        if markers.is_empty() {
            transaction.commit()?;
            return Ok(0);
        }
        let mut reconciled = 0;
        for (operation_id, first, last, expected_rows) in markers {
            let mut statement = transaction.prepare(
                "SELECT event_id,stream,sequence,commit_position,event_type,schema_version,
                        occurred_at,causation_id,correlation_id,attempt_id,trace_id,payload,artifacts
                 FROM events WHERE commit_position BETWEEN ?1 AND ?2 AND event_type=?3
                 ORDER BY commit_position",
            )?;
            let additions = statement
                .query_map(params![first, last, TOOL_LEARNING_EVENT], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, u16>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, Vec<u8>>(11)?,
                        row.get::<_, Vec<u8>>(12)?,
                    ))
                })?
                .map(|row| decode_event(row?).map(|stored| stored.event))
                .collect::<Result<Vec<_>, StoreError>>()?;
            drop(statement);
            if additions.len() != usize::try_from(expected_rows).unwrap_or(usize::MAX) {
                return Err(StoreError::CorruptData(
                    "learning reconciliation marker range mismatch",
                ));
            }
            transaction.execute(
                "DELETE FROM tool_learning_reconciliation WHERE operation_id=?1",
                [&operation_id],
            )?;
            match admit_learning_outbox(&transaction, &additions) {
                Ok(()) => {}
                Err(StoreError::InvalidRequest(_)) => return Ok(0),
                Err(error) => return Err(error),
            }
            for event in &additions {
                let learning: crate::telemetry::tool_learning::ToolLearningEvent =
                    serde_json::from_slice(&event.payload).map_err(|_| {
                        StoreError::CorruptData("invalid durable tool learning payload")
                    })?;
                insert_learning_indexes(&transaction, event, &learning)?;
            }
            reconciled += 1;
        }
        transaction.commit()?;
        Ok(reconciled)
    }

    pub(crate) fn has_learning_markers(&self, project: &str) -> Result<bool, StoreError> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tool_learning_reconciliation WHERE project=?1)",
                [project],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub(crate) fn has_learning_marker(&self, operation_id: &str) -> Result<bool, StoreError> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tool_learning_reconciliation WHERE operation_id=?1)",
                [operation_id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub(crate) fn claim_learning_export(
        &mut self,
        project: &str,
    ) -> Result<Option<String>, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now: i64 = transaction.query_row(
            "SELECT CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)",
            [],
            |row| row.get(0),
        )?;
        transaction.execute(
            "DELETE FROM tool_learning_export_claims
             WHERE project=?1 AND claimed_at < ?2",
            params![project, now.saturating_sub(3_600_000_000)],
        )?;
        let token = EventId::generate()
            .map_err(|_| StoreError::RandomnessUnavailable)?
            .to_string();
        let changed = transaction.execute(
            "INSERT INTO tool_learning_export_claims (project,token,claimed_at)
             VALUES (?1,?2,?3) ON CONFLICT(project) DO NOTHING",
            params![project, token, now],
        )?;
        transaction.commit()?;
        Ok((changed == 1).then_some(token))
    }

    pub(crate) fn release_learning_export(
        &mut self,
        project: &str,
        token: &str,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "DELETE FROM tool_learning_export_claims WHERE project=?1 AND token=?2",
            params![project, token],
        )?;
        Ok(())
    }

    pub fn acknowledge_learning_outbox(
        &mut self,
        project: &str,
        frame_id: &str,
    ) -> Result<(), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE tool_learning_outbox SET exported=1
             WHERE project=?1 AND frame_id=?2 AND exported=0",
            params![project, frame_id],
        )?;
        if changed > 1 {
            return Err(StoreError::CorruptData("duplicate learning outbox frame"));
        }
        transaction.execute(
            "DELETE FROM tool_learning_outbox
             WHERE project=?1 AND exported=1 AND cursor <= COALESCE(
               (SELECT MAX(cursor)-?2 FROM tool_learning_outbox
                 WHERE project=?1 AND exported=1), 0)",
            params![project, MAX_RETAINED_EXPORTED_LEARNING],
        )?;
        transaction.execute(
            "DELETE FROM tool_learning_outbox
             WHERE exported=1 AND cursor <= COALESCE(
               (SELECT MAX(cursor)-?1 FROM tool_learning_outbox WHERE exported=1), 0)",
            [MAX_RETAINED_EXPORTED_LEARNING],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn catalog_stats(&self, run: &str) -> Result<Vec<DurableCatalogStats>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT project,run_id,binding,attempts,succeeded,failed,cancelled,outcome_unknown,source_event,revision
             FROM catalog_stats_overlay WHERE run_id=?1 ORDER BY binding",
        )?;
        let rows = statement.query_map([run], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, i64>(9)?,
            ))
        })?;
        rows.map(|row| {
            let (
                project,
                run,
                binding,
                attempts,
                succeeded,
                failed,
                cancelled,
                outcome_unknown,
                source_event,
                revision,
            ) = row?;
            let value = |value| {
                u64::try_from(value)
                    .map_err(|_| StoreError::CorruptData("negative durable catalog statistic"))
            };
            Ok(DurableCatalogStats {
                project,
                run,
                binding,
                attempts: value(attempts)?,
                succeeded: value(succeeded)?,
                failed: value(failed)?,
                cancelled: value(cancelled)?,
                outcome_unknown: value(outcome_unknown)?,
                source_event,
                revision: value(revision)?,
            })
        })
        .collect()
    }

    pub(crate) fn catalog_stats_snapshot(
        &mut self,
        raw_run_id: RunId,
        run: &str,
    ) -> Result<Option<CatalogStatsSnapshot>, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        prune_catalog_snapshots(&transaction)?;
        if let Some((
            stored_project,
            generation,
            overlay_digest,
            row_count,
            payload_bytes,
            frame_id,
        )) = transaction
            .query_row(
                "SELECT project,generation,overlay_digest,row_count,payload_bytes,frame_id
                 FROM catalog_stats_snapshots
                 WHERE raw_run_id=?1 AND status='pending' ORDER BY generation LIMIT 1",
                [raw_run_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?
        {
            let entries = catalog_snapshot_entries(&transaction, &frame_id)?;
            let generation = u64::try_from(generation)
                .map_err(|_| StoreError::CorruptData("invalid catalog snapshot generation"))?;
            let snapshot =
                catalog_snapshot(raw_run_id, generation, overlay_digest, frame_id, entries)?;
            if snapshot.project != stored_project
                || snapshot.entries.len() != usize::try_from(row_count).unwrap_or(usize::MAX)
                || snapshot.payload_bytes != u64::try_from(payload_bytes).unwrap_or(u64::MAX)
            {
                return Err(StoreError::CorruptData(
                    "catalog statistics snapshot metadata mismatch",
                ));
            }
            transaction.commit()?;
            return Ok(Some(snapshot));
        }
        let entries = catalog_stats_in(&transaction, run)?;
        if entries.is_empty() {
            transaction.commit()?;
            return Ok(None);
        }
        let project = entries[0].project.clone();
        if entries
            .iter()
            .any(|entry| entry.project != project || entry.run != run)
        {
            return Err(StoreError::CorruptData(
                "catalog statistics snapshot crosses authorities",
            ));
        }
        let generation: i64 = transaction.query_row(
            "SELECT COALESCE(MAX(generation),0)+1 FROM catalog_stats_snapshots WHERE raw_run_id=?1",
            [raw_run_id.to_string()],
            |row| row.get(0),
        )?;
        if generation <= 0 {
            return Err(StoreError::PositionExhausted);
        }
        let generation_u64 =
            u64::try_from(generation).map_err(|_| StoreError::PositionExhausted)?;
        let payload = catalog_stats_payload(run, generation_u64, &entries)?;
        let digest = crate::domain::crypto::sha256(&payload);
        let overlay_digest = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let frame_id = format!("catalog_stats_v1_{overlay_digest}");
        let payload_bytes =
            i64::try_from(payload.len()).map_err(|_| StoreError::PositionExhausted)?;
        let (global_rows, global_bytes, project_rows, project_bytes): (i64, i64, i64, i64) =
            transaction.query_row(
                "SELECT COUNT(*),COALESCE(SUM(payload_bytes),0),
                        COALESCE(SUM(CASE WHEN project=?1 THEN 1 ELSE 0 END),0),
                        COALESCE(SUM(CASE WHEN project=?1 THEN payload_bytes ELSE 0 END),0)
                 FROM catalog_stats_snapshots",
                [&project],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        if global_rows >= MAX_CATALOG_SNAPSHOT_ROWS
            || project_rows >= MAX_PROJECT_CATALOG_SNAPSHOT_ROWS
            || global_bytes.saturating_add(payload_bytes) > MAX_CATALOG_SNAPSHOT_BYTES
            || project_bytes.saturating_add(payload_bytes) > MAX_PROJECT_CATALOG_SNAPSHOT_BYTES
        {
            return Err(StoreError::InvalidRequest(
                "catalog statistics snapshot capacity exceeded",
            ));
        }
        transaction.execute(
            "INSERT INTO catalog_stats_snapshots
             (project,raw_run_id,generation,overlay_digest,row_count,payload_bytes,frame_id,status)
             VALUES (?1,?2,?3,?4,?5,?6,?7,'pending')",
            params![
                project,
                raw_run_id.to_string(),
                generation,
                overlay_digest,
                i64::try_from(entries.len()).map_err(|_| StoreError::PositionExhausted)?,
                payload_bytes,
                frame_id,
            ],
        )?;
        for entry in &entries {
            transaction.execute(
                "INSERT INTO catalog_stats_snapshot_rows
                 (frame_id,project,run_id,binding,attempts,succeeded,failed,cancelled,
                  outcome_unknown,source_event,revision)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    frame_id,
                    entry.project,
                    entry.run,
                    entry.binding,
                    i64::try_from(entry.attempts).map_err(|_| StoreError::PositionExhausted)?,
                    i64::try_from(entry.succeeded).map_err(|_| StoreError::PositionExhausted)?,
                    i64::try_from(entry.failed).map_err(|_| StoreError::PositionExhausted)?,
                    i64::try_from(entry.cancelled).map_err(|_| StoreError::PositionExhausted)?,
                    i64::try_from(entry.outcome_unknown)
                        .map_err(|_| StoreError::PositionExhausted)?,
                    entry.source_event,
                    i64::try_from(entry.revision).map_err(|_| StoreError::PositionExhausted)?,
                ],
            )?;
        }
        transaction.execute("DELETE FROM catalog_stats_overlay WHERE run_id=?1", [run])?;
        transaction.commit()?;
        Ok(Some(CatalogStatsSnapshot {
            project,
            raw_run_id,
            generation: generation_u64,
            overlay_digest,
            frame_id,
            payload_bytes: u64::try_from(payload_bytes)
                .map_err(|_| StoreError::CorruptData("negative snapshot size"))?,
            payload,
            entries,
        }))
    }

    pub(crate) fn pending_catalog_stats_runs(
        &self,
        project: &str,
    ) -> Result<Vec<String>, StoreError> {
        let scheduler_exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='scheduler_runs')",
            [],
            |row| row.get(0),
        )?;
        if !scheduler_exists {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT raw_run_id FROM (
                 SELECT overlay.raw_run_id FROM catalog_stats_overlay AS overlay
                 JOIN scheduler_runs AS run ON run.run_id=overlay.raw_run_id
                 WHERE overlay.project=?1 AND run.phase IN ('terminal','canceled')
                 UNION
                 SELECT raw_run_id FROM catalog_stats_snapshots
                 WHERE project=?1 AND status='pending'
             ) ORDER BY raw_run_id LIMIT 10001",
        )?;
        let runs = statement
            .query_map([project], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if runs.len() > 10_000 {
            return Err(StoreError::CorruptData(
                "pending catalog statistics run capacity exceeded",
            ));
        }
        Ok(runs)
    }

    pub(crate) fn catalog_stats_run_terminal(&self, run_id: RunId) -> Result<bool, StoreError> {
        let scheduler_exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='scheduler_runs')",
            [],
            |row| row.get(0),
        )?;
        if !scheduler_exists {
            return Ok(false);
        }
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM scheduler_runs
                 WHERE run_id=?1 AND phase IN ('terminal','canceled'))",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub(crate) fn pending_catalog_stats_projects(&self) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT project FROM (
                 SELECT project FROM catalog_stats_overlay
                 UNION SELECT project FROM catalog_stats_snapshots WHERE status='pending'
             ) ORDER BY project LIMIT 10001",
        )?;
        let projects = statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if projects.len() > 10_000 {
            return Err(StoreError::CorruptData(
                "pending catalog statistics project capacity exceeded",
            ));
        }
        Ok(projects)
    }

    pub(crate) fn learning_backlog_drained(&self, project: &str) -> Result<bool, StoreError> {
        let scheduler_exists: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='scheduler_runs')",
            [],
            |row| row.get(0),
        )?;
        let query = if scheduler_exists {
            "SELECT
               NOT EXISTS(SELECT 1 FROM tool_learning_outbox
                          WHERE project=?1 AND exported=0)
               AND NOT EXISTS(SELECT 1 FROM tool_learning_reconciliation
                              WHERE project=?1)
               AND NOT EXISTS(SELECT 1 FROM catalog_stats_snapshots
                              WHERE project=?1 AND status='pending')
               AND NOT EXISTS(
                 SELECT 1 FROM catalog_stats_overlay AS overlay
                 JOIN scheduler_runs AS run ON run.run_id=overlay.raw_run_id
                 WHERE overlay.project=?1 AND run.phase IN ('terminal','canceled')
               )"
        } else {
            "SELECT
               NOT EXISTS(SELECT 1 FROM tool_learning_outbox
                          WHERE project=?1 AND exported=0)
               AND NOT EXISTS(SELECT 1 FROM tool_learning_reconciliation
                              WHERE project=?1)
               AND NOT EXISTS(SELECT 1 FROM catalog_stats_snapshots
                              WHERE project=?1 AND status='pending')
               AND NOT EXISTS(SELECT 1 FROM catalog_stats_overlay WHERE project=?1)"
        };
        self.connection
            .query_row(query, [project], |row| row.get(0))
            .map_err(Into::into)
    }

    pub(crate) fn acknowledge_catalog_stats(
        &mut self,
        snapshot: &CatalogStatsSnapshot,
    ) -> Result<bool, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let terminal: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduler_runs
             WHERE run_id=?1 AND phase IN ('terminal','canceled'))",
            [snapshot.raw_run_id.to_string()],
            |row| row.get(0),
        )?;
        if !terminal || snapshot.entries.is_empty() {
            return Err(StoreError::InvalidRequest(
                "catalog statistics snapshot requires a terminal run",
            ));
        }
        let stored: Option<(String, i64, i64, String, i64, String)> = transaction
            .query_row(
                "SELECT project,row_count,payload_bytes,frame_id,generation,status FROM catalog_stats_snapshots
                 WHERE raw_run_id=?1 AND overlay_digest=?2",
                params![snapshot.raw_run_id.to_string(), snapshot.overlay_digest],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .optional()?;
        let expected = (
            snapshot.project.clone(),
            i64::try_from(snapshot.entries.len()).map_err(|_| StoreError::PositionExhausted)?,
            i64::try_from(snapshot.payload_bytes).map_err(|_| StoreError::PositionExhausted)?,
            snapshot.frame_id.clone(),
            i64::try_from(snapshot.generation).map_err(|_| StoreError::PositionExhausted)?,
        );
        let Some((project, rows, bytes, frame, generation, status)) = stored else {
            return Err(StoreError::CorruptData(
                "catalog statistics snapshot authority mismatch",
            ));
        };
        if (project, rows, bytes, frame, generation) != expected {
            return Err(StoreError::CorruptData(
                "catalog statistics snapshot authority mismatch",
            ));
        }
        if status == "exported" {
            transaction.commit()?;
            return Ok(true);
        }
        if status != "pending" {
            return Err(StoreError::CorruptData(
                "catalog statistics snapshot has invalid status",
            ));
        }
        if catalog_snapshot_entries(&transaction, &snapshot.frame_id)? != snapshot.entries {
            return Err(StoreError::CorruptData(
                "catalog statistics snapshot rows changed",
            ));
        }
        let changed = transaction.execute(
            "UPDATE catalog_stats_snapshots SET status='exported'
             WHERE raw_run_id=?1 AND overlay_digest=?2 AND status='pending'",
            params![snapshot.raw_run_id.to_string(), snapshot.overlay_digest],
        )?;
        if changed == 0 {
            transaction.commit()?;
            return Ok(true);
        }
        transaction.execute(
            "DELETE FROM catalog_stats_snapshot_rows WHERE frame_id=?1",
            [&snapshot.frame_id],
        )?;
        prune_catalog_snapshots(&transaction)?;
        transaction.commit()?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn persist_discovery_binding(
        &mut self,
        project: &str,
        run_id: RunId,
        binding_id: &str,
    ) -> Result<(), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        persist_discovery_binding(&transaction, project, run_id, binding_id)?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn discovery_bindings(
        &mut self,
        project: &str,
        run_id: RunId,
    ) -> Result<Vec<String>, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let scheduler_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='scheduler_runs')",
            [],
            |row| row.get(0),
        )?;
        if scheduler_exists {
            transaction.execute(
                "DELETE FROM discovery_bindings
                 WHERE run_id IN (SELECT run_id FROM scheduler_runs
                                  WHERE phase IN ('terminal','canceled'))
                   AND run_id<>?1",
                [run_id.to_string()],
            )?;
        }
        let mut statement = transaction.prepare(
            "SELECT binding_id FROM discovery_bindings
             WHERE project=?1 AND run_id=?2 ORDER BY binding_id LIMIT 10001",
        )?;
        let rows = statement.query_map(params![project, run_id.to_string()], |row| row.get(0))?;
        let bindings = rows.collect::<Result<Vec<_>, _>>()?;
        if bindings.len() > 10_000 {
            return Err(StoreError::CorruptData(
                "discovery binding capacity exceeded",
            ));
        }
        drop(statement);
        transaction.commit()?;
        Ok(bindings)
    }

    pub(crate) fn remove_discovery_binding(
        &mut self,
        project: &str,
        run_id: RunId,
        binding_id: &str,
    ) -> Result<(), StoreError> {
        self.connection.execute(
            "DELETE FROM discovery_bindings
             WHERE project=?1 AND run_id=?2 AND binding_id=?3",
            params![project, run_id.to_string(), binding_id],
        )?;
        Ok(())
    }
}

fn reject_secret_authority_identifiers(
    custody: &SecretCustody,
    events: &[NewEvent],
) -> Result<(), StoreError> {
    for event in events {
        let attempt = event.attempt_id.map(|value| value.to_string());
        for identifier in [
            event.id.to_string(),
            event.stream.to_string(),
            event.event_type.as_str().to_owned(),
            event.causation_id.to_string(),
            event.correlation_id.to_string(),
            event.trace_id.as_str().to_owned(),
        ]
        .into_iter()
        .chain(attempt)
        {
            if custody.contains(identifier.as_bytes()) {
                return Err(StoreError::InvalidRequest(
                    "active secret is forbidden in an event authority identifier",
                ));
            }
        }
    }
    Ok(())
}

fn catalog_stats_in(
    transaction: &Transaction<'_>,
    run: &str,
) -> Result<Vec<DurableCatalogStats>, StoreError> {
    let mut statement = transaction.prepare(
        "SELECT project,run_id,binding,attempts,succeeded,failed,cancelled,outcome_unknown,
                source_event,revision
         FROM catalog_stats_overlay WHERE run_id=?1 ORDER BY binding",
    )?;
    let rows = statement.query_map([run], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, i64>(9)?,
        ))
    })?;
    rows.map(|row| {
        let (
            project,
            run,
            binding,
            attempts,
            succeeded,
            failed,
            cancelled,
            outcome_unknown,
            source_event,
            revision,
        ) = row?;
        let value = |value| {
            u64::try_from(value)
                .map_err(|_| StoreError::CorruptData("negative durable catalog statistic"))
        };
        Ok(DurableCatalogStats {
            project,
            run,
            binding,
            attempts: value(attempts)?,
            succeeded: value(succeeded)?,
            failed: value(failed)?,
            cancelled: value(cancelled)?,
            outcome_unknown: value(outcome_unknown)?,
            source_event,
            revision: value(revision)?,
        })
    })
    .collect()
}

fn catalog_snapshot_entries(
    transaction: &Transaction<'_>,
    frame_id: &str,
) -> Result<Vec<DurableCatalogStats>, StoreError> {
    let mut statement = transaction.prepare(
        "SELECT project,run_id,binding,attempts,succeeded,failed,cancelled,outcome_unknown,
                source_event,revision FROM catalog_stats_snapshot_rows
         WHERE frame_id=?1 ORDER BY binding",
    )?;
    let rows = statement.query_map([frame_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, i64>(9)?,
        ))
    })?;
    rows.map(|row| {
        let (
            project,
            run,
            binding,
            attempts,
            succeeded,
            failed,
            cancelled,
            outcome_unknown,
            source_event,
            revision,
        ) = row?;
        let value = |value| {
            u64::try_from(value)
                .map_err(|_| StoreError::CorruptData("negative durable catalog statistic"))
        };
        Ok(DurableCatalogStats {
            project,
            run,
            binding,
            attempts: value(attempts)?,
            succeeded: value(succeeded)?,
            failed: value(failed)?,
            cancelled: value(cancelled)?,
            outcome_unknown: value(outcome_unknown)?,
            source_event,
            revision: value(revision)?,
        })
    })
    .collect()
}

fn catalog_stats_payload(
    run: &str,
    generation: u64,
    entries: &[DurableCatalogStats],
) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(&serde_json::json!({
        "format": "tool_learning.catalog_stats.v1",
        "run": run,
        "generation": generation,
        "entries": entries.iter().map(|entry| serde_json::json!({
            "binding": entry.binding,
            "attempts": entry.attempts,
            "succeeded": entry.succeeded,
            "failed": entry.failed,
            "cancelled": entry.cancelled,
            "outcome_unknown": entry.outcome_unknown,
            "source_event": entry.source_event,
        })).collect::<Vec<_>>(),
    }))
    .map_err(|_| StoreError::CorruptData("catalog statistics snapshot is invalid"))
}

fn catalog_snapshot(
    raw_run_id: RunId,
    generation: u64,
    overlay_digest: String,
    frame_id: String,
    entries: Vec<DurableCatalogStats>,
) -> Result<CatalogStatsSnapshot, StoreError> {
    let first = entries.first().ok_or(StoreError::CorruptData(
        "catalog snapshot has no immutable rows",
    ))?;
    if entries
        .iter()
        .any(|entry| entry.project != first.project || entry.run != first.run)
    {
        return Err(StoreError::CorruptData(
            "catalog statistics snapshot crosses authorities",
        ));
    }
    let project = first.project.clone();
    let payload = catalog_stats_payload(&first.run, generation, &entries)?;
    let expected = crate::domain::crypto::sha256(&payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if expected != overlay_digest || frame_id != format!("catalog_stats_v1_{expected}") {
        return Err(StoreError::CorruptData(
            "catalog statistics snapshot digest mismatch",
        ));
    }
    Ok(CatalogStatsSnapshot {
        project,
        raw_run_id,
        generation,
        overlay_digest,
        frame_id,
        payload_bytes: payload.len() as u64,
        payload,
        entries,
    })
}

fn prune_catalog_snapshots(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    transaction.execute(
        "DELETE FROM catalog_stats_snapshots
         WHERE status IN ('exported','superseded') AND rowid NOT IN (
             SELECT rowid FROM catalog_stats_snapshots
             WHERE status IN ('exported','superseded') ORDER BY rowid DESC LIMIT ?1
         )",
        [MAX_RETAINED_EXPORTED_SNAPSHOTS],
    )?;
    Ok(())
}

fn persist_discovery_binding(
    transaction: &Transaction<'_>,
    project: &str,
    run_id: RunId,
    binding_id: &str,
) -> Result<(), StoreError> {
    let run = run_id.to_string();
    let scheduler_exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='scheduler_runs')",
        [],
        |row| row.get(0),
    )?;
    if scheduler_exists {
        transaction.execute(
            "DELETE FROM discovery_bindings
             WHERE run_id IN (SELECT run_id FROM scheduler_runs
                              WHERE phase IN ('terminal','canceled'))
               AND run_id<>?1",
            [&run],
        )?;
    }
    let exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM discovery_bindings
         WHERE project=?1 AND run_id=?2 AND binding_id=?3)",
        params![project, run, binding_id],
        |row| row.get(0),
    )?;
    if !exists {
        let (global_rows, project_rows, run_rows, global_bytes, project_bytes):
            (i64, i64, i64, i64, i64) = transaction.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(CASE WHEN project=?1 THEN 1 ELSE 0 END),0),
                    COALESCE(SUM(CASE WHEN project=?1 AND run_id=?2 THEN 1 ELSE 0 END),0),
                    COALESCE(SUM(length(project)+length(run_id)+length(binding_id)),0),
                    COALESCE(SUM(CASE WHEN project=?1 THEN length(project)+length(run_id)+length(binding_id) ELSE 0 END),0)
             FROM discovery_bindings",
            params![project, run],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )?;
        let added_bytes = i64::try_from(project.len() + run.len() + binding_id.len())
            .map_err(|_| StoreError::PositionExhausted)?;
        if global_rows >= MAX_DISCOVERY_BINDING_ROWS
            || project_rows >= MAX_PROJECT_DISCOVERY_BINDING_ROWS
            || run_rows >= 10_000
            || global_bytes.saturating_add(added_bytes) > MAX_DISCOVERY_BINDING_BYTES
            || project_bytes.saturating_add(added_bytes) > MAX_PROJECT_DISCOVERY_BINDING_BYTES
        {
            return Err(StoreError::InvalidRequest(
                "discovery binding capacity exceeded",
            ));
        }
    }
    transaction.execute(
        "INSERT INTO discovery_bindings (project,run_id,binding_id,created_at)
         VALUES (?1,?2,?3,CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER))
         ON CONFLICT(project,run_id,binding_id) DO NOTHING",
        params![project, run, binding_id],
    )?;
    Ok(())
}

pub(crate) fn append_canonical_event(
    transaction: &Transaction<'_>,
    event: &NewEvent,
    expected_version: u64,
) -> Result<CommitPosition, StoreError> {
    let stream = event.stream.to_string();
    transaction.execute(
        "INSERT INTO stream_heads (stream, version) VALUES (?1, 0)
         ON CONFLICT(stream) DO NOTHING",
        [&stream],
    )?;
    let actual: i64 = transaction.query_row(
        "SELECT version FROM stream_heads WHERE stream=?1",
        [&stream],
        |row| row.get(0),
    )?;
    if actual < 0 {
        return Err(StoreError::CorruptData("negative stream version"));
    }
    if actual as u64 != expected_version {
        return Err(StoreError::ExpectedVersion {
            stream,
            expected: expected_version,
            actual: actual.max(0) as u64,
        });
    }
    ensure_event_absent(transaction, event.id)?;
    let sequence = actual.checked_add(1).ok_or(StoreError::PositionExhausted)?;
    let watermark: i64 = transaction.query_row(
        "SELECT position FROM commit_watermark WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    if watermark < 0 {
        return Err(StoreError::CorruptData("negative commit watermark"));
    }
    let position = watermark
        .checked_add(1)
        .ok_or(StoreError::PositionExhausted)?;
    insert_event_row(transaction, event, sequence, position)?;
    transaction.execute(
        "UPDATE stream_heads SET version=?2 WHERE stream=?1",
        params![event.stream.to_string(), sequence],
    )?;
    transaction.execute(
        "UPDATE commit_watermark SET position=?1 WHERE singleton=1",
        [position],
    )?;
    CommitPosition::new(
        u64::try_from(position).map_err(|_| StoreError::CorruptData("negative commit position"))?,
    )
    .map_err(|_| StoreError::CorruptData("invalid callback commit position"))
}

fn insert_event_row(
    transaction: &Transaction<'_>,
    event: &NewEvent,
    sequence: i64,
    position: i64,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO events (
             event_id, stream, sequence, commit_position, event_type, schema_version,
             occurred_at, causation_id, correlation_id, attempt_id, trace_id,
             payload, artifacts
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            event.id.to_string(),
            event.stream.to_string(),
            sequence,
            position,
            event.event_type.as_str(),
            u16::from(event.schema_version),
            event.occurred_at.as_str(),
            event.causation_id.to_string(),
            event.correlation_id.to_string(),
            event.attempt_id.map(|id| id.to_string()),
            event.trace_id.as_str(),
            event.payload,
            event.artifacts,
        ],
    )?;
    Ok(())
}

fn journal_artifact_publication(
    transaction: &Transaction<'_>,
    command: &AppendCommand,
    publication: Option<&PendingArtifactPublication>,
) -> Result<(), StoreError> {
    let Some(publication) = publication else {
        return Ok(());
    };
    let expected = crate::domain::events::ArtifactRef::parse(&publication.digest)
        .map_err(|_| StoreError::InvalidRequest("invalid artifact publication digest"))?;
    let mut referenced = false;
    for event in &command.events {
        let artifacts: Vec<crate::domain::events::ArtifactRef> =
            serde_json::from_slice(&event.artifacts).map_err(|_| {
                StoreError::InvalidRequest(
                    "event artifacts must be a JSON array of content digests",
                )
            })?;
        if artifacts.len() > crate::capabilities::kernel::invoke::MAX_INVOCATION_ARTIFACT_DIGESTS {
            return Err(StoreError::InvalidRequest(
                "event artifact digest limit exceeded",
            ));
        }
        referenced |= artifacts.contains(&expected);
    }
    if !referenced {
        return Ok(());
    }
    transaction.execute(
        "INSERT INTO artifact_publication_journal
         (artifact_reference,artifact_digest,purpose,subject_id,principal_id,project_id,run_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(artifact_reference) DO UPDATE SET
           artifact_digest=excluded.artifact_digest,
           purpose=excluded.purpose,
           subject_id=excluded.subject_id,
           principal_id=excluded.principal_id,
           project_id=excluded.project_id,
           run_id=excluded.run_id",
        params![
            publication.reference,
            publication.digest,
            publication.purpose,
            publication.subject_id,
            publication.principal_id,
            publication.project_id,
            publication.run_id,
        ],
    )?;
    Ok(())
}

fn configure_connection(connection: &Connection) -> Result<(), StoreError> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    let foreign_keys: i64 = connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
    if foreign_keys != 1 {
        return Err(StoreError::CorruptData("SQLite foreign keys are disabled"));
    }
    let mut journal_mode: String =
        connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        journal_mode = connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    }
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::WalUnavailable(journal_mode));
    }
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match version {
        SCHEMA_VERSION => {}
        0 => {
            transaction.execute_batch(
                "CREATE TABLE commit_watermark (
                  singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                  position INTEGER NOT NULL CHECK (position >= 0)
             );
             INSERT OR IGNORE INTO commit_watermark (singleton, position) VALUES (1, 0);
             CREATE TABLE stream_heads (
                 stream TEXT PRIMARY KEY,
                 version INTEGER NOT NULL CHECK (version >= 0)
             );
             CREATE TABLE events (
                 event_id TEXT PRIMARY KEY,
                 stream TEXT NOT NULL REFERENCES stream_heads(stream),
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 commit_position INTEGER NOT NULL UNIQUE CHECK (commit_position > 0),
                 event_type TEXT NOT NULL,
                 schema_version INTEGER NOT NULL CHECK (schema_version > 0),
                 occurred_at TEXT NOT NULL,
                 causation_id TEXT NOT NULL,
                 correlation_id TEXT NOT NULL,
                 attempt_id TEXT,
                 trace_id TEXT NOT NULL,
                 payload BLOB NOT NULL,
                 artifacts BLOB NOT NULL,
                 UNIQUE (stream, sequence)
             );
             CREATE TABLE idempotency (
                 principal_id TEXT NOT NULL,
                 command_name TEXT NOT NULL,
                 target TEXT NOT NULL,
                 idempotency_key TEXT NOT NULL,
                 request_digest BLOB NOT NULL CHECK (length(request_digest) = 32),
                 state TEXT NOT NULL CHECK (state IN ('pending', 'terminal')),
                 claim_token BLOB CHECK (claim_token IS NULL OR length(claim_token) = 16),
                 response BLOB,
                 CHECK (
                     (state = 'pending' AND claim_token IS NOT NULL AND response IS NULL) OR
                     (state = 'terminal' AND claim_token IS NULL AND response IS NOT NULL)
                  ),
                 PRIMARY KEY (principal_id, command_name, target, idempotency_key)
             );
             CREATE TABLE idempotency_events (
                 principal_id TEXT NOT NULL,
                 command_name TEXT NOT NULL,
                 target TEXT NOT NULL,
                 idempotency_key TEXT NOT NULL,
                 ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
                 commit_position INTEGER NOT NULL UNIQUE REFERENCES events(commit_position),
                 PRIMARY KEY (principal_id, command_name, target, idempotency_key, ordinal),
                 FOREIGN KEY (principal_id, command_name, target, idempotency_key)
                     REFERENCES idempotency(principal_id, command_name, target, idempotency_key)
              );",
            )?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        version => return Err(StoreError::UnsupportedSchemaVersion(version)),
    }
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS attempt_driver_fences (
             run_id TEXT PRIMARY KEY,
             fence INTEGER NOT NULL CHECK (fence > 0),
             lease_version INTEGER NOT NULL CHECK (lease_version > 0)
         );
         CREATE TABLE IF NOT EXISTS attempt_driver_claims (
             run_id TEXT PRIMARY KEY,
             attempt_id TEXT NOT NULL UNIQUE,
             principal_id TEXT NOT NULL,
             fence INTEGER NOT NULL CHECK (fence > 0),
             lease_version INTEGER NOT NULL CHECK (lease_version > 0),
             expires_at_unix_micros INTEGER NOT NULL CHECK (expires_at_unix_micros >= 0),
              quiescent INTEGER NOT NULL DEFAULT 0 CHECK (quiescent IN (0, 1))
          );
           CREATE TABLE IF NOT EXISTS artifact_publication_journal (
              artifact_reference TEXT PRIMARY KEY, artifact_digest TEXT NOT NULL,
              purpose TEXT NOT NULL, subject_id TEXT NOT NULL, principal_id TEXT NOT NULL,
               project_id TEXT NOT NULL, run_id TEXT NOT NULL
           );
           CREATE TABLE IF NOT EXISTS tool_learning_outbox (
               cursor INTEGER PRIMARY KEY AUTOINCREMENT,
               frame_id TEXT NOT NULL UNIQUE,
               event_id TEXT NOT NULL UNIQUE,
               run_id TEXT NOT NULL,
               project TEXT NOT NULL,
               payload BLOB NOT NULL,
               exported INTEGER NOT NULL DEFAULT 0 CHECK (exported IN (0, 1))
           );
           CREATE TABLE IF NOT EXISTS catalog_stats_overlay (
               project TEXT NOT NULL,
               run_id TEXT NOT NULL,
               raw_run_id TEXT NOT NULL DEFAULT '',
               binding TEXT NOT NULL,
               attempts INTEGER NOT NULL CHECK (attempts >= 0),
               succeeded INTEGER NOT NULL CHECK (succeeded >= 0),
               failed INTEGER NOT NULL CHECK (failed >= 0),
               cancelled INTEGER NOT NULL CHECK (cancelled >= 0),
               outcome_unknown INTEGER NOT NULL CHECK (outcome_unknown >= 0),
               source_event TEXT NOT NULL,
               revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
               PRIMARY KEY (run_id,binding)
           );
           CREATE TABLE IF NOT EXISTS catalog_stats_snapshots (
               project TEXT NOT NULL,
               raw_run_id TEXT NOT NULL,
               generation INTEGER NOT NULL CHECK (generation > 0),
               overlay_digest TEXT NOT NULL,
               row_count INTEGER NOT NULL CHECK (row_count > 0),
               payload_bytes INTEGER NOT NULL CHECK (payload_bytes > 0),
               frame_id TEXT NOT NULL UNIQUE,
               status TEXT NOT NULL CHECK (status IN ('pending','exported','superseded')),
               PRIMARY KEY (raw_run_id,overlay_digest)
           );
           CREATE TABLE IF NOT EXISTS catalog_stats_snapshot_rows (
               frame_id TEXT NOT NULL REFERENCES catalog_stats_snapshots(frame_id),
               project TEXT NOT NULL,
               run_id TEXT NOT NULL,
               binding TEXT NOT NULL,
               attempts INTEGER NOT NULL CHECK (attempts >= 0),
               succeeded INTEGER NOT NULL CHECK (succeeded >= 0),
               failed INTEGER NOT NULL CHECK (failed >= 0),
               cancelled INTEGER NOT NULL CHECK (cancelled >= 0),
               outcome_unknown INTEGER NOT NULL CHECK (outcome_unknown >= 0),
               source_event TEXT NOT NULL,
               revision INTEGER NOT NULL CHECK (revision > 0),
               PRIMARY KEY (frame_id,binding)
           );
           CREATE TABLE IF NOT EXISTS discovery_bindings (
               project TEXT NOT NULL,
               run_id TEXT NOT NULL,
               binding_id TEXT NOT NULL,
               created_at INTEGER NOT NULL DEFAULT 0 CHECK (created_at >= 0),
               PRIMARY KEY (project,run_id,binding_id)
           );
           CREATE TABLE IF NOT EXISTS tool_learning_export_claims (
               project TEXT PRIMARY KEY,
               token TEXT NOT NULL UNIQUE,
               claimed_at INTEGER NOT NULL CHECK (claimed_at >= 0)
           );
           CREATE TABLE IF NOT EXISTS tool_learning_reconciliation (
               operation_id TEXT PRIMARY KEY,
               project TEXT NOT NULL,
               first_position INTEGER,
               last_position INTEGER,
               row_count INTEGER NOT NULL CHECK (row_count > 0),
               payload_bytes INTEGER NOT NULL CHECK (payload_bytes > 0),
               required INTEGER NOT NULL CHECK (required IN (0,1)),
               created_at INTEGER NOT NULL CHECK (created_at >= 0),
                CHECK ((first_position IS NULL) = (last_position IS NULL))
            );
            CREATE TABLE IF NOT EXISTS capability_extension_registry (
                principal_id TEXT NOT NULL,
                project_id TEXT NOT NULL,
                revision INTEGER NOT NULL CHECK (revision > 0),
                snapshot BLOB NOT NULL,
                entry_count INTEGER NOT NULL CHECK (entry_count >= 0),
                PRIMARY KEY (principal_id, project_id)
            );
            CREATE TABLE IF NOT EXISTS capability_extension_registry_state (
                singleton INTEGER PRIMARY KEY CHECK (singleton=1),
                revision INTEGER NOT NULL CHECK (revision >= 0),
                entry_count INTEGER NOT NULL CHECK (entry_count >= 0),
                snapshot_bytes INTEGER NOT NULL CHECK (snapshot_bytes >= 0)
            );",
    )?;
    let has_extension_revision: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('capability_extension_registry')
         WHERE name='revision')",
        [],
        |row| row.get(0),
    )?;
    if !has_extension_revision {
        transaction.execute_batch(
            "ALTER TABLE capability_extension_registry
             ADD COLUMN revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0);",
        )?;
    }
    let has_extension_entry_count: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('capability_extension_registry')
         WHERE name='entry_count')",
        [],
        |row| row.get(0),
    )?;
    if !has_extension_entry_count {
        transaction.execute_batch(
            "ALTER TABLE capability_extension_registry
             ADD COLUMN entry_count INTEGER NOT NULL DEFAULT 0 CHECK (entry_count >= 0);",
        )?;
    }
    transaction.execute_batch(
        "UPDATE capability_extension_registry
         SET entry_count=json_array_length(snapshot, '$.entries')
         WHERE entry_count=0;
         INSERT OR IGNORE INTO capability_extension_registry_state
             (singleton, revision, entry_count, snapshot_bytes)
         SELECT 1, coalesce(max(revision), 0), coalesce(sum(entry_count), 0),
                coalesce(sum(length(snapshot)), 0)
         FROM capability_extension_registry;",
    )?;
    let has_learning_project: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tool_learning_outbox') WHERE name='project')",
        [],
        |row| row.get(0),
    )?;
    if !has_learning_project {
        transaction.execute_batch(
            "ALTER TABLE tool_learning_outbox
             ADD COLUMN project TEXT NOT NULL DEFAULT '';",
        )?;
    }
    let has_raw_stats_run: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('catalog_stats_overlay')
         WHERE name='raw_run_id')",
        [],
        |row| row.get(0),
    )?;
    if !has_raw_stats_run {
        transaction.execute_batch(
            "ALTER TABLE catalog_stats_overlay
             ADD COLUMN raw_run_id TEXT NOT NULL DEFAULT '';",
        )?;
    }
    let has_stats_revision: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('catalog_stats_overlay')
         WHERE name='revision')",
        [],
        |row| row.get(0),
    )?;
    if !has_stats_revision {
        transaction.execute_batch(
            "ALTER TABLE catalog_stats_overlay
             ADD COLUMN revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0);",
        )?;
    }
    let content_addressed_snapshots: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('catalog_stats_snapshots')
         WHERE name='overlay_digest')",
        [],
        |row| row.get(0),
    )?;
    if !content_addressed_snapshots {
        transaction.execute_batch(
            "ALTER TABLE catalog_stats_snapshots RENAME TO catalog_stats_snapshots_legacy;
             CREATE TABLE catalog_stats_snapshots (
                 project TEXT NOT NULL,
                 raw_run_id TEXT NOT NULL,
                 generation INTEGER NOT NULL CHECK (generation > 0),
                 overlay_digest TEXT NOT NULL,
                 row_count INTEGER NOT NULL CHECK (row_count > 0),
                 payload_bytes INTEGER NOT NULL CHECK (payload_bytes > 0),
                 frame_id TEXT NOT NULL UNIQUE,
                 status TEXT NOT NULL CHECK (status IN ('pending','exported','superseded')),
                 PRIMARY KEY (raw_run_id,overlay_digest)
             );
             INSERT INTO catalog_stats_snapshots
                 (project,raw_run_id,generation,overlay_digest,row_count,payload_bytes,frame_id,status)
             SELECT '',raw_run_id,1,frame_id,1,1,frame_id,'exported'
             FROM catalog_stats_snapshots_legacy;
             DROP TABLE catalog_stats_snapshots_legacy;",
        )?;
    }
    let has_snapshot_generation: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('catalog_stats_snapshots')
         WHERE name='generation')",
        [],
        |row| row.get(0),
    )?;
    if !has_snapshot_generation {
        transaction.execute_batch(
            "ALTER TABLE catalog_stats_snapshots
             ADD COLUMN generation INTEGER NOT NULL DEFAULT 1 CHECK (generation > 0);
             UPDATE catalog_stats_snapshots SET status='superseded' WHERE status='pending';",
        )?;
    }
    let compact_learning_markers: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('tool_learning_reconciliation')
         WHERE name='operation_id')",
        [],
        |row| row.get(0),
    )?;
    if !compact_learning_markers {
        transaction.execute_batch(
            "ALTER TABLE tool_learning_reconciliation RENAME TO tool_learning_reconciliation_legacy;
             CREATE TABLE tool_learning_reconciliation (
                 operation_id TEXT PRIMARY KEY,
                 project TEXT NOT NULL,
                 first_position INTEGER,
                 last_position INTEGER,
                 row_count INTEGER NOT NULL CHECK (row_count > 0),
                 payload_bytes INTEGER NOT NULL CHECK (payload_bytes > 0),
                 required INTEGER NOT NULL CHECK (required IN (0,1)),
                 created_at INTEGER NOT NULL CHECK (created_at >= 0),
                 CHECK ((first_position IS NULL) = (last_position IS NULL))
             );
             INSERT INTO tool_learning_reconciliation
                 (operation_id,project,first_position,last_position,row_count,payload_bytes,required,created_at)
             SELECT MIN(marker.event_id),marker.project,MIN(event.commit_position),MAX(event.commit_position),
                    COUNT(*),SUM(length(event.payload)),0,MIN(event.commit_position)
             FROM tool_learning_reconciliation_legacy AS marker
             JOIN events AS event ON event.event_id=marker.event_id
             GROUP BY marker.project,event.stream;
             DROP TABLE tool_learning_reconciliation_legacy;",
        )?;
    }
    let has_binding_created_at: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('discovery_bindings')
         WHERE name='created_at')",
        [],
        |row| row.get(0),
    )?;
    if !has_binding_created_at {
        transaction.execute_batch(
            "ALTER TABLE discovery_bindings
             ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0 CHECK (created_at >= 0);",
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn marker_bytes(project: &str, operation_id: &str) -> Result<i64, StoreError> {
    i64::try_from(project.len() + operation_id.len() + 64)
        .map_err(|_| StoreError::PositionExhausted)
}

fn upsert_learning_marker(
    transaction: &Transaction<'_>,
    project: &str,
    operation_id: &str,
    required: bool,
    range: Option<(i64, i64, i64, i64)>,
) -> Result<(), StoreError> {
    if project.is_empty() || operation_id.is_empty() {
        return Err(StoreError::InvalidRequest(
            "learning reconciliation marker has no authority",
        ));
    }
    let existing: Option<(String, i64, i64, bool)> = transaction
        .query_row(
            "SELECT project,row_count,payload_bytes,required FROM tool_learning_reconciliation
             WHERE operation_id=?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    if let Some((found_project, reserved_rows, reserved_bytes, found_required)) = &existing {
        if found_project != project || *found_required != required {
            return Err(StoreError::CorruptData(
                "learning reconciliation marker authority mismatch",
            ));
        }
        if let Some((_, _, rows, bytes)) = range
            && required
            && (rows > *reserved_rows || bytes > *reserved_bytes)
        {
            return Err(StoreError::InvalidRequest(
                "required learning terminal exceeded its reservation",
            ));
        }
    }
    if existing.is_none() {
        let added_bytes = marker_bytes(project, operation_id)?;
        loop {
            let (global_rows, project_rows, global_bytes, project_bytes): (i64, i64, i64, i64) =
                transaction.query_row(
                    "SELECT COUNT(*),
                            COALESCE(SUM(CASE WHEN project=?1 THEN 1 ELSE 0 END),0),
                            COALESCE(SUM(length(project)+length(operation_id)+64),0),
                            COALESCE(SUM(CASE WHEN project=?1 THEN length(project)+length(operation_id)+64 ELSE 0 END),0)
                     FROM tool_learning_reconciliation",
                    [project],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?;
            if global_rows < MAX_LEARNING_MARKER_ROWS
                && project_rows < MAX_PROJECT_LEARNING_MARKER_ROWS
                && global_bytes.saturating_add(added_bytes) <= MAX_LEARNING_MARKER_BYTES
                && project_bytes.saturating_add(added_bytes) <= MAX_PROJECT_LEARNING_MARKER_BYTES
            {
                break;
            }
            let evicted = transaction.execute(
                "DELETE FROM tool_learning_reconciliation WHERE operation_id=(
                     SELECT operation_id FROM tool_learning_reconciliation
                     WHERE required=0 AND (?1=0 OR project=?2)
                     ORDER BY created_at,operation_id LIMIT 1
                 )",
                params![
                    if project_rows >= MAX_PROJECT_LEARNING_MARKER_ROWS
                        || project_bytes.saturating_add(added_bytes)
                            > MAX_PROJECT_LEARNING_MARKER_BYTES
                    {
                        1
                    } else {
                        0
                    },
                    project
                ],
            )?;
            if evicted == 0 {
                if required {
                    return Err(StoreError::InvalidRequest(
                        "learning reconciliation marker capacity exceeded",
                    ));
                }
                return Ok(());
            }
        }
        let (rows, bytes) = range.map(|(_, _, rows, bytes)| (rows, bytes)).unwrap_or((
            RESERVED_TERMINAL_MARKER_ROWS,
            RESERVED_TERMINAL_MARKER_BYTES,
        ));
        let (pending_rows, pending_bytes, project_pending_rows, project_pending_bytes):
            (i64, i64, i64, i64) = transaction.query_row(
            "SELECT
                 (SELECT COUNT(*) FROM tool_learning_outbox WHERE exported=0)
                   +COALESCE(SUM(row_count),0),
                 (SELECT COALESCE(SUM(length(payload)),0) FROM tool_learning_outbox WHERE exported=0)
                   +COALESCE(SUM(payload_bytes),0),
                 (SELECT COUNT(*) FROM tool_learning_outbox WHERE exported=0 AND project=?1)
                   +COALESCE(SUM(CASE WHEN project=?1 THEN row_count ELSE 0 END),0),
                 (SELECT COALESCE(SUM(length(payload)),0) FROM tool_learning_outbox WHERE exported=0 AND project=?1)
                   +COALESCE(SUM(CASE WHEN project=?1 THEN payload_bytes ELSE 0 END),0)
             FROM tool_learning_reconciliation",
            [project],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        if pending_rows.saturating_add(rows) > MAX_LEARNING_OUTBOX_ROWS
            || project_pending_rows.saturating_add(rows) > MAX_PROJECT_LEARNING_OUTBOX_ROWS
            || pending_bytes.saturating_add(bytes) > MAX_LEARNING_OUTBOX_BYTES
            || project_pending_bytes.saturating_add(bytes) > MAX_PROJECT_LEARNING_OUTBOX_BYTES
        {
            if required {
                return Err(StoreError::InvalidRequest(
                    "required learning terminal reservation capacity exceeded",
                ));
            }
            return Ok(());
        }
        transaction.execute(
            "INSERT INTO tool_learning_reconciliation
             (operation_id,project,first_position,last_position,row_count,payload_bytes,required,created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,
                     CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER))",
            params![
                operation_id,
                project,
                range.map(|value| value.0),
                range.map(|value| value.1),
                rows,
                bytes,
                required,
            ],
        )?;
        return Ok(());
    }
    if let Some((first, last, rows, bytes)) = range {
        transaction.execute(
            "UPDATE tool_learning_reconciliation
             SET first_position=?2,last_position=?3,row_count=?4,payload_bytes=?5
             WHERE operation_id=?1",
            params![operation_id, first, last, rows, bytes],
        )?;
    }
    Ok(())
}

fn admit_learning_outbox(
    transaction: &Transaction<'_>,
    events: &[NewEvent],
) -> Result<(), StoreError> {
    use crate::telemetry::tool_learning::ToolLearningEvent;

    let learning = events
        .iter()
        .filter(|event| event.event_type.as_str() == TOOL_LEARNING_EVENT)
        .map(|event| {
            serde_json::from_slice::<ToolLearningEvent>(&event.payload)
                .map(|record| (record, event.payload.len()))
                .map_err(|_| StoreError::InvalidRequest("invalid tool learning payload"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if learning.is_empty() {
        return Ok(());
    }
    let project = learning[0].0.common().project.as_str();
    if learning
        .iter()
        .any(|(record, _)| record.common().project.as_str() != project)
    {
        return Err(StoreError::InvalidRequest(
            "tool learning append crosses projects",
        ));
    }
    let (global_rows, global_bytes): (i64, i64) = transaction.query_row(
        "SELECT
             (SELECT COUNT(*) FROM tool_learning_outbox WHERE exported=0)
               +(SELECT COALESCE(SUM(row_count),0) FROM tool_learning_reconciliation),
             (SELECT COALESCE(SUM(length(payload)),0) FROM tool_learning_outbox WHERE exported=0)
               +(SELECT COALESCE(SUM(payload_bytes),0) FROM tool_learning_reconciliation)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (project_rows, project_bytes): (i64, i64) = transaction.query_row(
        "SELECT
             (SELECT COUNT(*) FROM tool_learning_outbox WHERE project=?1 AND exported=0)
               +(SELECT COALESCE(SUM(row_count),0) FROM tool_learning_reconciliation WHERE project=?1),
             (SELECT COALESCE(SUM(length(payload)),0) FROM tool_learning_outbox WHERE project=?1 AND exported=0)
               +(SELECT COALESCE(SUM(payload_bytes),0) FROM tool_learning_reconciliation WHERE project=?1)",
        [project],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let added_rows = i64::try_from(learning.len()).map_err(|_| StoreError::PositionExhausted)?;
    let added_bytes = learning.iter().try_fold(0_i64, |total, (_, bytes)| {
        total
            .checked_add(i64::try_from(*bytes).map_err(|_| StoreError::PositionExhausted)?)
            .ok_or(StoreError::PositionExhausted)
    })?;
    let terminal_batch = learning
        .iter()
        .any(|(event, _)| matches!(event, ToolLearningEvent::Outcome { .. }));
    let (global_row_limit, project_row_limit, global_byte_limit, project_byte_limit) =
        if !terminal_batch {
            (
                MAX_LEARNING_OUTBOX_ROWS - RESERVED_TERMINAL_OUTBOX_ROWS,
                MAX_PROJECT_LEARNING_OUTBOX_ROWS - RESERVED_TERMINAL_OUTBOX_ROWS,
                MAX_LEARNING_OUTBOX_BYTES - RESERVED_TERMINAL_OUTBOX_BYTES,
                MAX_PROJECT_LEARNING_OUTBOX_BYTES - RESERVED_TERMINAL_OUTBOX_BYTES,
            )
        } else {
            (
                MAX_LEARNING_OUTBOX_ROWS,
                MAX_PROJECT_LEARNING_OUTBOX_ROWS,
                MAX_LEARNING_OUTBOX_BYTES,
                MAX_PROJECT_LEARNING_OUTBOX_BYTES,
            )
        };
    if global_rows.saturating_add(added_rows) > global_row_limit
        || project_rows.saturating_add(added_rows) > project_row_limit
        || global_bytes.saturating_add(added_bytes) > global_byte_limit
        || project_bytes.saturating_add(added_bytes) > project_byte_limit
    {
        return Err(StoreError::InvalidRequest(
            "tool learning outbox capacity exceeded",
        ));
    }
    Ok(())
}

fn insert_learning_indexes(
    transaction: &Transaction<'_>,
    event: &NewEvent,
    learning: &crate::telemetry::tool_learning::ToolLearningEvent,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO tool_learning_outbox
         (frame_id,event_id,run_id,project,payload,exported)
         VALUES (?1,?2,?3,?4,?5,0) ON CONFLICT(event_id) DO NOTHING",
        params![
            learning.common().event_id.as_str(),
            event.id.to_string(),
            event.correlation_id.to_string(),
            learning.common().project.as_str(),
            event.payload,
        ],
    )?;
    update_catalog_stats(transaction, event)
}

fn update_catalog_stats(transaction: &Transaction<'_>, event: &NewEvent) -> Result<(), StoreError> {
    use crate::telemetry::tool_learning::{LearningStatus, ToolLearningEvent};

    let outcome: ToolLearningEvent = serde_json::from_slice(&event.payload)
        .map_err(|_| StoreError::InvalidRequest("invalid tool learning payload"))?;
    let ToolLearningEvent::Outcome {
        common,
        call,
        status,
        ..
    } = outcome
    else {
        return Ok(());
    };
    let mut statement = transaction.prepare(
        "SELECT payload FROM events WHERE event_type=?1 AND correlation_id=?2
         ORDER BY commit_position",
    )?;
    let payloads = statement.query_map(
        params![TOOL_LEARNING_EVENT, event.correlation_id.to_string()],
        |row| row.get::<_, Vec<u8>>(0),
    )?;
    let mut binding = None;
    for payload in payloads {
        let record: ToolLearningEvent = serde_json::from_slice(&payload?)
            .map_err(|_| StoreError::CorruptData("invalid durable tool learning payload"))?;
        if let ToolLearningEvent::Call {
            call: found,
            binding: found_binding,
            ..
        } = record
            && found == call
        {
            binding = found_binding;
            break;
        }
    }
    let Some(binding) = binding else {
        return Ok(());
    };
    let exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM catalog_stats_overlay WHERE run_id=?1 AND binding=?2)",
        params![common.run.as_str(), binding.as_str()],
        |row| row.get(0),
    )?;
    if !exists {
        let (global, project, global_bytes, project_bytes): (i64, i64, i64, i64) = transaction.query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN project=?1 THEN 1 ELSE 0 END),0),
                    COALESCE(SUM(length(project)+length(run_id)+length(raw_run_id)+length(binding)+length(source_event)+72),0),
                    COALESCE(SUM(CASE WHEN project=?1 THEN length(project)+length(run_id)+length(raw_run_id)+length(binding)+length(source_event)+72 ELSE 0 END),0)
             FROM catalog_stats_overlay",
            [common.project.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let added_bytes = i64::try_from(
            common.project.as_str().len()
                + common.run.as_str().len()
                + event.correlation_id.to_string().len()
                + binding.as_str().len()
                + common.event_id.as_str().len()
                + 72,
        )
        .map_err(|_| StoreError::PositionExhausted)?;
        if global >= MAX_CATALOG_STATS_ROWS
            || project >= MAX_PROJECT_CATALOG_STATS_ROWS
            || global_bytes.saturating_add(added_bytes) > MAX_CATALOG_STATS_BYTES
            || project_bytes.saturating_add(added_bytes) > MAX_PROJECT_CATALOG_STATS_BYTES
        {
            return Err(StoreError::InvalidRequest(
                "catalog statistics capacity exceeded",
            ));
        }
    }
    let (succeeded, failed, cancelled, unknown) = match status {
        LearningStatus::Succeeded => (1, 0, 0, 0),
        LearningStatus::Cancelled => (0, 0, 1, 0),
        LearningStatus::OutcomeUnknown => (0, 0, 0, 1),
        LearningStatus::Failed | LearningStatus::Interrupted | LearningStatus::Unavailable => {
            (0, 1, 0, 0)
        }
    };
    transaction.execute(
        "INSERT INTO catalog_stats_overlay
         (project,run_id,raw_run_id,binding,attempts,succeeded,failed,cancelled,outcome_unknown,source_event)
         VALUES (?1,?2,?3,?4,1,?5,?6,?7,?8,?9)
         ON CONFLICT(run_id,binding) DO UPDATE SET
           raw_run_id=excluded.raw_run_id,
           attempts=attempts+1,
           succeeded=succeeded+excluded.succeeded,
           failed=failed+excluded.failed,
           cancelled=cancelled+excluded.cancelled,
           outcome_unknown=outcome_unknown+excluded.outcome_unknown,
           source_event=excluded.source_event,
           revision=revision+1",
        params![
            common.project.as_str(),
            common.run.as_str(),
            event.correlation_id.to_string(),
            binding.as_str(),
            succeeded,
            failed,
            cancelled,
            unknown,
            common.event_id.as_str(),
        ],
    )?;
    Ok(())
}

fn guard_driver_claim(
    transaction: &Transaction<'_>,
    claim: AttemptDriverClaim,
    allow_quiescent: bool,
) -> Result<(), StoreError> {
    let now: i64 = transaction.query_row(
        "SELECT CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)",
        [],
        |row| row.get(0),
    )?;
    // A quiescent claim belongs to a driver that parked on a durable wait and
    // stopped heartbeating, so its lease is expected to lapse; callers that
    // allow quiescent claims (waiting resolutions) are still fenced by the
    // fence and lease_version equality checks.
    let owns: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM attempt_driver_claims
              WHERE run_id = ?1 AND attempt_id = ?2 AND principal_id = ?3
                AND fence = ?4 AND lease_version = ?5
                AND ((expires_at_unix_micros > ?6 AND quiescent = 0)
                     OR (?7 AND quiescent = 1))
         )",
        params![
            claim.run_id.to_string(),
            claim.attempt_id.to_string(),
            claim.principal_id.to_string(),
            i64::try_from(claim.fence.get()).map_err(|_| StoreError::StaleDriverClaim)?,
            i64::try_from(claim.lease_version).map_err(|_| StoreError::StaleDriverClaim)?,
            now,
            allow_quiescent,
        ],
        |row| row.get(0),
    )?;
    if owns {
        Ok(())
    } else {
        Err(StoreError::StaleDriverClaim)
    }
}

enum CheckedRecord {
    Pending(PendingToken),
    Terminal(Vec<u8>),
}

fn checked_record(
    record: StoredRecord,
    expected_digest: CanonicalRequestDigest,
    key: &IdempotencyKey,
) -> Result<CheckedRecord, StoreError> {
    let actual_digest = digest_from_bytes(&record.digest)?;
    if actual_digest != expected_digest {
        return Err(StoreError::IdempotencyConflict(key.clone()));
    }
    match record.state {
        StoredState::Pending(token) => Ok(CheckedRecord::Pending(token)),
        StoredState::Terminal(response) => Ok(CheckedRecord::Terminal(response)),
        StoredState::Invalid => Err(StoreError::CorruptData("invalid idempotency state")),
    }
}

fn digest_from_bytes(bytes: &[u8]) -> Result<CanonicalRequestDigest, StoreError> {
    bytes
        .try_into()
        .map(CanonicalRequestDigest::new)
        .map_err(|_| StoreError::CorruptData("invalid request digest"))
}

fn checked_positions(positions: Vec<i64>) -> Result<Vec<CommitPosition>, StoreError> {
    positions
        .into_iter()
        .map(|position| {
            u64::try_from(position)
                .ok()
                .and_then(|position| CommitPosition::new(position).ok())
                .ok_or(StoreError::CorruptData(
                    "invalid idempotency commit position",
                ))
        })
        .collect()
}

fn validate_command(command: &AppendCommand) -> Result<(), StoreError> {
    if command.claim.as_ref().is_some_and(|claim| {
        claim.scope != command.idempotency_scope
            || claim.key != command.idempotency_key
            || claim.digest != command.request_digest
    }) {
        return Err(StoreError::InvalidRequest(
            "idempotency claim belongs to a different scope, key, or request",
        ));
    }
    let expected_streams: BTreeSet<_> = command
        .expected_versions
        .iter()
        .map(|expected| expected.stream.to_string())
        .collect();
    if expected_streams.len() != command.expected_versions.len() {
        return Err(StoreError::InvalidRequest("duplicate expected stream"));
    }
    let event_streams: BTreeSet<_> = command
        .events
        .iter()
        .map(|event| event.stream.to_string())
        .collect();
    if event_streams != expected_streams {
        return Err(StoreError::InvalidRequest(
            "expected versions must exactly match affected streams",
        ));
    }
    let event_ids: BTreeSet<_> = command
        .events
        .iter()
        .map(|event| event.id.to_string())
        .collect();
    if event_ids.len() != command.events.len() {
        return Err(StoreError::InvalidRequest("duplicate event identifier"));
    }
    Ok(())
}

fn check_expected_versions(
    transaction: &Transaction<'_>,
    command: &AppendCommand,
) -> Result<BTreeMap<String, i64>, StoreError> {
    let mut versions = BTreeMap::new();
    for expected in &command.expected_versions {
        let stream = expected.stream.to_string();
        transaction.execute(
            "INSERT INTO stream_heads (stream, version) VALUES (?1, 0)
             ON CONFLICT(stream) DO NOTHING",
            [&stream],
        )?;
        let actual: i64 = transaction.query_row(
            "SELECT version FROM stream_heads WHERE stream = ?1",
            [&stream],
            |row| row.get(0),
        )?;
        let expected_version =
            i64::try_from(expected.version.get()).map_err(|_| StoreError::PositionExhausted)?;
        if actual != expected_version {
            return Err(StoreError::ExpectedVersion {
                stream,
                expected: expected.version.get(),
                actual: u64::try_from(actual)
                    .map_err(|_| StoreError::CorruptData("negative stream version"))?,
            });
        }
        versions.insert(stream, actual);
    }
    Ok(versions)
}

fn ensure_event_absent(transaction: &Transaction<'_>, event_id: EventId) -> Result<(), StoreError> {
    let present: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE event_id = ?1)",
        [event_id.to_string()],
        |row| row.get(0),
    )?;
    if present {
        Err(StoreError::DuplicateEvent(event_id.to_string()))
    } else {
        Ok(())
    }
}

fn inject(crash: &mut impl FnMut(CrashPoint) -> bool, point: CrashPoint) -> Result<(), StoreError> {
    if crash(point) {
        Err(StoreError::InjectedCrash(point))
    } else {
        Ok(())
    }
}

type EventRow = (
    String,
    String,
    i64,
    i64,
    String,
    u16,
    String,
    String,
    String,
    Option<String>,
    String,
    Vec<u8>,
    Vec<u8>,
);

fn decode_event(row: EventRow) -> Result<StoredEvent, StoreError> {
    let (
        id,
        stream,
        sequence,
        commit_position,
        event_type,
        schema_version,
        occurred_at,
        causation_id,
        correlation_id,
        attempt_id,
        trace_id,
        payload,
        artifacts,
    ) = row;
    Ok(StoredEvent {
        event: NewEvent {
            id: id.parse().map_err(corrupt_event)?,
            stream: stream.parse().map_err(corrupt_event)?,
            event_type: event_type.parse().map_err(corrupt_event)?,
            schema_version: SchemaVersion::try_from(schema_version).map_err(corrupt_event)?,
            occurred_at: occurred_at.parse().map_err(corrupt_event)?,
            causation_id: causation_id.parse().map_err(corrupt_event)?,
            correlation_id: correlation_id.parse().map_err(corrupt_event)?,
            attempt_id: attempt_id
                .map(|id| id.parse().map_err(corrupt_event))
                .transpose()?,
            trace_id: trace_id.parse().map_err(corrupt_event)?,
            payload,
            artifacts,
        },
        sequence: StreamSequence::new(u64::try_from(sequence).map_err(corrupt_event)?)
            .map_err(corrupt_event)?,
        commit_position: CommitPosition::new(
            u64::try_from(commit_position).map_err(corrupt_event)?,
        )
        .map_err(corrupt_event)?,
    })
}

fn corrupt_event<T>(_: T) -> StoreError {
    StoreError::CorruptData("invalid stored event envelope")
}

#[cfg(test)]
mod learning_persistence_tests {
    use super::*;

    #[test]
    fn event_writer_preserves_canonical_content_and_rejects_secret_authority() {
        let root = std::env::temp_dir().join(format!(
            "kit-event-custody-{}",
            crate::domain::ids::EventId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("state.sqlite3");
        let secret = "event-writer-canary";
        let custody = SecretCustody::new([std::sync::Arc::new(
            crate::domain::secret::SecretLease::new(secret),
        )]);
        let mut store =
            crate::test_support::open_project_store(&database, custody.clone()).unwrap();
        let principal = PrincipalId::generate().unwrap();
        let run = RunId::generate().unwrap();
        let stream = EntityId::Run(run);
        let event_id = EventId::generate().unwrap();
        let command_id = CommandId::generate().unwrap();
        let event = NewEvent {
            id: event_id,
            stream,
            event_type: EventType::parse("run.progress").unwrap(),
            schema_version: SchemaVersion::CURRENT,
            occurred_at: UtcDateTime::parse("2026-08-06T12:00:00Z").unwrap(),
            causation_id: command_id,
            correlation_id: stream,
            attempt_id: None,
            trace_id: TraceId::parse("event-writer").unwrap(),
            payload: serde_json::to_vec(&serde_json::json!({
                "content": format!("{secret} 6576656e742d7772697465722d63616e617279")
            }))
            .unwrap(),
            artifacts: b"[]".to_vec(),
        };
        let canonical_payload = event.payload.clone();
        store
            .append(AppendCommand {
                idempotency_scope: IdempotencyScope::new(principal, "event.writer", stream)
                    .unwrap(),
                idempotency_key: IdempotencyKey::parse("event-writer").unwrap(),
                request_digest: CanonicalRequestDigest::new([7; 32]),
                claim: None,
                driver_claim: None,
                allow_quiescent_driver_claim: false,
                expected_versions: vec![ExpectedStreamVersion {
                    stream,
                    version: ExpectedVersion::new(0),
                }],
                events: vec![event],
                response: b"ok".to_vec(),
            })
            .unwrap();
        let stored = store.events().unwrap().remove(0);
        assert_eq!(stored.event.payload, canonical_payload);
        let projected =
            crate::test_support::project_event_export_projection(&stored, &custody).unwrap();
        assert!(!custody.contains(&projected.envelope));
        assert_eq!(
            projected.digest,
            crate::capabilities::kernel::identity::Digest::of(
                crate::capabilities::kernel::identity::DigestAlgorithm::Sha256,
                &projected.envelope,
            )
            .to_string()
        );
        assert_ne!(projected.authority_digest, projected.digest);

        let authority_secret = event_id.to_string();
        let authority_custody = SecretCustody::new([std::sync::Arc::new(
            crate::domain::secret::SecretLease::new(authority_secret),
        )]);
        let mut authority_store = crate::test_support::open_project_store(
            root.join("authority.sqlite3"),
            authority_custody,
        )
        .unwrap();
        let mut rejected = stored.event;
        rejected.payload = b"{}".to_vec();
        assert!(matches!(
            authority_store.append(AppendCommand {
                idempotency_scope: IdempotencyScope::new(principal, "event.authority", stream)
                    .unwrap(),
                idempotency_key: IdempotencyKey::parse("event-authority").unwrap(),
                request_digest: CanonicalRequestDigest::new([8; 32]),
                claim: None,
                driver_claim: None,
                allow_quiescent_driver_claim: false,
                expected_versions: vec![ExpectedStreamVersion {
                    stream,
                    version: ExpectedVersion::new(0),
                }],
                events: vec![rejected],
                response: b"ok".to_vec(),
            }),
            Err(StoreError::InvalidRequest(
                "active secret is forbidden in an event authority identifier"
            ))
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_binding_authority_replays_idempotently_after_store_restart() {
        let root = std::env::temp_dir().join(format!(
            "kit-discovery-binding-store-{}",
            crate::domain::ids::EventId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("state.sqlite3");
        let run = RunId::generate().unwrap();
        let binding = format!("binding_v1_{}", "ab".repeat(32));
        let mut store = crate::test_support::open_sqlite_store(&database).unwrap();
        store
            .persist_discovery_binding("project-authority", run, &binding)
            .unwrap();
        store
            .persist_discovery_binding("project-authority", run, &binding)
            .unwrap();
        drop(store);

        let mut store = crate::test_support::open_sqlite_store(&database).unwrap();
        assert_eq!(
            store.discovery_bindings("project-authority", run).unwrap(),
            [binding]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn catalog_snapshot_detaches_exact_rows_before_late_overlay_revision() {
        let root = std::env::temp_dir().join(format!(
            "kit-catalog-snapshot-cas-{}",
            crate::domain::ids::EventId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("state.sqlite3");
        let run = RunId::generate().unwrap();
        let pointer = "run-pointer";
        let mut store = crate::test_support::open_sqlite_store(&database).unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TABLE scheduler_runs (run_id TEXT PRIMARY KEY, phase TEXT NOT NULL);",
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO scheduler_runs (run_id,phase) VALUES (?1,'terminal')",
                [run.to_string()],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO catalog_stats_overlay
                 (project,run_id,raw_run_id,binding,attempts,succeeded,failed,cancelled,
                  outcome_unknown,source_event,revision)
                 VALUES ('project',?1,?2,'binding',1,1,0,0,0,'event-1',1)",
                params![pointer, run.to_string()],
            )
            .unwrap();

        let first = store.catalog_stats_snapshot(run, pointer).unwrap().unwrap();
        assert!(store.catalog_stats(pointer).unwrap().is_empty());
        store
            .connection
            .execute(
                "INSERT INTO catalog_stats_overlay
                 (project,run_id,raw_run_id,binding,attempts,succeeded,failed,cancelled,
                  outcome_unknown,source_event,revision)
                 VALUES ('project',?1,?2,'binding',1,1,0,0,0,'event-2',1)",
                params![pointer, run.to_string()],
            )
            .unwrap();
        let retry = store.catalog_stats_snapshot(run, pointer).unwrap().unwrap();
        assert_eq!(retry.frame_id, first.frame_id);
        assert!(store.acknowledge_catalog_stats(&first).unwrap());
        assert_eq!(
            store.catalog_stats(pointer).unwrap()[0].source_event,
            "event-2"
        );

        let second = store.catalog_stats_snapshot(run, pointer).unwrap().unwrap();
        assert_ne!(first.frame_id, second.frame_id);
        assert_eq!(second.generation, first.generation + 1);
        assert!(store.acknowledge_catalog_stats(&second).unwrap());
        assert!(store.catalog_stats(pointer).unwrap().is_empty());
        assert!(
            store
                .catalog_stats_snapshot(run, pointer)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sustained_learning_backlog_keeps_markers_bounded_and_reserves_required_work() {
        let root = std::env::temp_dir().join(format!(
            "kit-learning-marker-cap-{}",
            crate::domain::ids::EventId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("state.sqlite3");
        let mut store = crate::test_support::open_sqlite_store(&database).unwrap();
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        for index in 0..MAX_PROJECT_LEARNING_MARKER_ROWS {
            transaction
                .execute(
                    "INSERT INTO tool_learning_reconciliation
                     (operation_id,project,first_position,last_position,row_count,payload_bytes,
                      required,created_at) VALUES (?1,'project',1,1,1,1,0,?2)",
                    params![format!("best-effort-{index:05}"), index],
                )
                .unwrap();
        }
        upsert_learning_marker(
            &transaction,
            "project",
            "best-effort-new",
            false,
            Some((1, 1, 1, 1)),
        )
        .unwrap();
        let (rows, bytes): (i64, i64) = transaction
            .query_row(
                "SELECT COUNT(*),SUM(length(project)+length(operation_id)+64)
                 FROM tool_learning_reconciliation WHERE project='project'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(rows, MAX_PROJECT_LEARNING_MARKER_ROWS);
        assert!(bytes <= MAX_PROJECT_LEARNING_MARKER_BYTES);
        transaction
            .execute("UPDATE tool_learning_reconciliation SET required=1", [])
            .unwrap();
        assert!(
            upsert_learning_marker(&transaction, "project", "required-over-cap", true, None)
                .is_err()
        );
        upsert_learning_marker(
            &transaction,
            "project",
            "best-effort-over-cap",
            false,
            Some((1, 1, 1, 1)),
        )
        .unwrap();
        let dropped: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM tool_learning_reconciliation
                 WHERE operation_id='best-effort-over-cap')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!dropped);
        transaction.rollback().unwrap();
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_binding_lifecycle_prunes_terminal_runs_but_keeps_current_ids() {
        let root = std::env::temp_dir().join(format!(
            "kit-discovery-binding-lifecycle-{}",
            crate::domain::ids::EventId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("state.sqlite3");
        let active = RunId::generate().unwrap();
        let terminal = RunId::generate().unwrap();
        let mut store = crate::test_support::open_sqlite_store(&database).unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TABLE scheduler_runs (run_id TEXT PRIMARY KEY, phase TEXT NOT NULL);",
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO scheduler_runs (run_id,phase) VALUES (?1,'terminal'),(?2,'running')",
                params![terminal.to_string(), active.to_string()],
            )
            .unwrap();
        store
            .persist_discovery_binding("project", terminal, "terminal-binding")
            .unwrap();
        store
            .persist_discovery_binding("project", active, "active-binding")
            .unwrap();
        drop(store);

        let mut store = crate::test_support::open_sqlite_store(&database).unwrap();
        assert_eq!(
            store.discovery_bindings("project", active).unwrap(),
            ["active-binding"]
        );
        let terminal_count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM discovery_bindings WHERE run_id=?1",
                [terminal.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(terminal_count, 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
