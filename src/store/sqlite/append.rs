use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags, Transaction, TransactionBehavior, params};

use crate::api::service::AttemptDriverClaim;
use crate::domain::commands::ExpectedVersion;
use crate::domain::events::{
    CommitPosition, EntityId, EventType, SchemaVersion, StreamSequence, TraceId, UtcDateTime,
};
use crate::domain::ids::{AttemptId, CommandId, EventId};

#[cfg(any(test, debug_assertions))]
use super::idempotency::ClaimOutcome;
use super::idempotency::{
    CanonicalRequestDigest, IdempotencyClaim, IdempotencyKey, IdempotencyScope, IdempotencyStatus,
    IdempotentResponse, PendingToken, StoredRecord, StoredState,
};
use super::idempotency::{insert_pending, insert_position, lookup, positions, set_terminal};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const SCHEMA_VERSION: i64 = 1;

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
        guard_driver_claim(&transaction, claim, false)?;
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
        if changed != 1 {
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
        mut crash: impl FnMut(CrashPoint) -> bool,
    ) -> Result<AppendOutcome, StoreError> {
        validate_command(&command)?;
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
            transaction.execute(
                "INSERT INTO events (
                     event_id, stream, sequence, commit_position, event_type, schema_version,
                     occurred_at, causation_id, correlation_id, attempt_id, trace_id,
                     payload, artifacts
                 ) VALUES (
                     ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13
                 )",
                params![
                    event.id.to_string(),
                    stream,
                    *sequence,
                    watermark,
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
        inject(&mut crash, CrashPoint::BeforeIdempotencyTerminal)?;
        if !set_terminal(&transaction, scope, key, &command.response)? {
            return Err(StoreError::CorruptData(
                "pending idempotency record disappeared during append",
            ));
        }
        inject(&mut crash, CrashPoint::AfterIdempotencyTerminal)?;
        inject(&mut crash, CrashPoint::BeforeCommit)?;
        transaction.commit()?;
        inject(&mut crash, CrashPoint::AfterCommit)?;

        Ok(AppendOutcome::Committed(IdempotentResponse {
            response: command.response,
            commit_positions: committed_positions,
        }))
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

    pub fn committed_through(&self) -> Result<u64, StoreError> {
        let position: i64 = self.connection.query_row(
            "SELECT position FROM commit_watermark WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        u64::try_from(position).map_err(|_| StoreError::CorruptData("negative commit watermark"))
    }
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
         );",
    )?;
    transaction.commit()?;
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
