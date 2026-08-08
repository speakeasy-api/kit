use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::api::service::{EventCursor, EventProjection};
use crate::domain::crypto::sha256;
use crate::domain::events::{
    CommitPosition, EntityId, EventType, SchemaVersion, StreamSequence, TraceId, UtcDateTime,
};
use crate::domain::ids::{AttemptId, CommandId, EventId};
use crate::domain::projections::{DomainReducer, IndexedEventScope};
use crate::store::sqlite::append::{NewEvent, StoredEvent};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const APPEND_SCHEMA_VERSION: i64 = 1;

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "projections",
        up: include_str!("../migrations/sqlite/0001_projections.up.sql"),
        down: include_str!("../migrations/sqlite/0001_projections.down.sql"),
    },
    Migration {
        version: 2,
        name: "deletion_jobs",
        up: include_str!("../migrations/sqlite/0002_deletion_jobs.up.sql"),
        down: include_str!("../migrations/sqlite/0002_deletion_jobs.down.sql"),
    },
    Migration {
        version: 3,
        name: "erasure_and_event_index",
        up: include_str!("../migrations/sqlite/0003_erasure_and_event_index.up.sql"),
        down: include_str!("../migrations/sqlite/0003_erasure_and_event_index.down.sql"),
    },
];

#[derive(Clone, Copy, Debug)]
struct Migration {
    version: i64,
    name: &'static str,
    up: &'static str,
    down: &'static str,
}

impl Migration {
    fn checksum(self) -> [u8; 32] {
        let mut bytes = Vec::with_capacity(self.up.len() + self.down.len() + 1);
        bytes.extend_from_slice(self.up.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(self.down.as_bytes());
        sha256(&bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionCrashPoint {
    AfterRead,
    AfterReduce,
    AfterWrite,
    BeforeCommit,
}

impl StoredEvent {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"KIT-PROJECTION-EVENT\0\x01");
        put_bytes(&mut bytes, self.event.id.to_string().as_bytes());
        put_bytes(&mut bytes, self.event.stream.to_string().as_bytes());
        bytes.extend_from_slice(&self.sequence.get().to_be_bytes());
        bytes.extend_from_slice(&self.commit_position.get().to_be_bytes());
        put_bytes(&mut bytes, self.event.event_type.as_str().as_bytes());
        bytes.extend_from_slice(&u16::from(self.event.schema_version).to_be_bytes());
        put_bytes(&mut bytes, self.event.occurred_at.as_str().as_bytes());
        put_bytes(&mut bytes, self.event.causation_id.to_string().as_bytes());
        put_bytes(&mut bytes, self.event.correlation_id.to_string().as_bytes());
        match &self.event.attempt_id {
            Some(attempt_id) => {
                bytes.push(1);
                put_bytes(&mut bytes, attempt_id.to_string().as_bytes());
            }
            None => bytes.push(0),
        }
        put_bytes(&mut bytes, self.event.trace_id.as_str().as_bytes());
        put_bytes(&mut bytes, &self.event.payload);
        put_bytes(&mut bytes, &self.event.artifacts);
        bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionSnapshot {
    pub name: String,
    pub canonical_bytes: Vec<u8>,
    pub digest: [u8; 32],
    pub checkpoint: u64,
    pub updated_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreTime {
    unix_micros: i64,
    rfc3339: String,
}

impl StoreTime {
    pub fn unix_micros(&self) -> i64 {
        self.unix_micros
    }

    pub fn as_rfc3339(&self) -> &str {
        &self.rfc3339
    }
}

#[derive(Debug)]
pub enum ProjectionError {
    Busy,
    Database(rusqlite::Error),
    InvalidDatabaseLocation,
    UnsupportedAppendSchema(i64),
    MissingAppendSchema,
    UnknownMigration(i64),
    MigrationDrift(i64),
    InvalidProjectionName,
    CorruptData(&'static str),
    Reducer(String),
    InjectedCrash(ProjectionCrashPoint),
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("SQLite store remained busy past its bounded timeout"),
            Self::Database(error) => write!(f, "SQLite projection error: {error}"),
            Self::InvalidDatabaseLocation => {
                f.write_str("SQLite projections require a local database path")
            }
            Self::UnsupportedAppendSchema(version) => {
                write!(f, "unsupported SQLite append schema version {version}")
            }
            Self::MissingAppendSchema => f.write_str("SQLite append schema is missing"),
            Self::UnknownMigration(version) => {
                write!(f, "unknown newer SQLite projection migration {version}")
            }
            Self::MigrationDrift(version) => {
                write!(f, "SQLite projection migration {version} checksum drift")
            }
            Self::InvalidProjectionName => f.write_str("projection name must not be empty"),
            Self::CorruptData(message) => write!(f, "corrupt SQLite projection data: {message}"),
            Self::Reducer(message) => write!(f, "projection reducer failed: {message}"),
            Self::InjectedCrash(point) => write!(f, "injected projection crash at {point:?}"),
        }
    }
}

impl std::error::Error for ProjectionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for ProjectionError {
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

pub struct ProjectionStore {
    connection: Connection,
}

impl ProjectionStore {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, ProjectionError> {
        let mut connection = open_connection(path.as_ref())?;
        migrate(&mut connection)?;
        Ok(Self { connection })
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn update(
        &mut self,
        name: &str,
        initial: &[u8],
        reducer: impl FnMut(&mut Vec<u8>, &StoredEvent) -> Result<(), ProjectionError>,
    ) -> Result<ProjectionSnapshot, ProjectionError> {
        self.update_with_hook(name, initial, false, reducer, |_| false)
    }

    pub(crate) fn update_domain(
        &mut self,
    ) -> Result<(DomainReducer, ProjectionSnapshot), ProjectionError> {
        self.project_domain(false)
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn rebuild_domain(
        &mut self,
    ) -> Result<(DomainReducer, ProjectionSnapshot), ProjectionError> {
        self.project_domain(true)
    }

    fn project_domain(
        &mut self,
        rebuild: bool,
    ) -> Result<(DomainReducer, ProjectionSnapshot), ProjectionError> {
        let default = DomainReducer::default()
            .canonical_bytes()
            .map_err(|error| ProjectionError::Reducer(error.to_string()))?;
        let baseline = if rebuild {
            read_rebuild_baseline(&self.connection, DomainReducer::NAME)?
        } else {
            None
        };
        let (initial, initial_checkpoint) = baseline.unwrap_or((default, 0));
        let snapshot = self.update_with_hook_from(
            DomainReducer::NAME,
            &initial,
            initial_checkpoint,
            rebuild,
            |bytes, event| {
                DomainReducer::reduce_canonical_bytes(bytes, event)
                    .map_err(|error| ProjectionError::Reducer(error.to_string()))
            },
            |_| false,
        )?;
        let mut state = DomainReducer::from_canonical_bytes(&snapshot.canonical_bytes)
            .map_err(|error| ProjectionError::Reducer(error.to_string()))?;
        state.events = read_indexed_event_projections(&self.connection)?;
        Ok((state, snapshot))
    }

    pub(crate) fn ensure_event_index(&mut self) -> Result<(), ProjectionError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let indexed: i64 = transaction.query_row(
            "SELECT indexed_through FROM event_projection_index_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let committed: i64 = transaction.query_row(
            "SELECT position FROM commit_watermark WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        if indexed == committed {
            transaction.commit()?;
            return Ok(());
        }
        transaction.execute("DELETE FROM event_projection_index", [])?;
        let committed = checked_nonnegative(committed, "negative commit watermark")?;
        let events = read_events(&transaction, 0, committed)?;
        let mut state = DomainReducer::default();
        for event in events {
            if let Some((scope, stored_at_unix_micros)) = state
                .reduce_indexed(&event)
                .map_err(|error| ProjectionError::Reducer(error.to_string()))?
            {
                insert_event_index(
                    &transaction,
                    event.commit_position.get(),
                    scope,
                    stored_at_unix_micros,
                )?;
            }
        }
        transaction.execute(
            "UPDATE event_projection_index_state SET indexed_through = ?1 WHERE singleton = 1",
            [committed],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn index_committed_events(
        &mut self,
        positions: &[CommitPosition],
        scope: IndexedEventScope,
        stored_at_unix_micros: i64,
    ) -> Result<(), ProjectionError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for position in positions {
            insert_event_index(&transaction, position.get(), scope, stored_at_unix_micros)?;
        }
        if let Some(last) = positions.last() {
            transaction.execute(
                "UPDATE event_projection_index_state
                 SET indexed_through = max(indexed_through, ?1) WHERE singleton = 1",
                [last.get()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn purge_erased_bytes(&mut self) -> Result<(), ProjectionError> {
        self.connection.execute_batch(
            "PRAGMA wal_checkpoint(TRUNCATE);
             VACUUM;
             PRAGMA wal_checkpoint(TRUNCATE);",
        )?;
        Ok(())
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn rebuild(
        &mut self,
        name: &str,
        initial: &[u8],
        reducer: impl FnMut(&mut Vec<u8>, &StoredEvent) -> Result<(), ProjectionError>,
    ) -> Result<ProjectionSnapshot, ProjectionError> {
        self.update_with_hook(name, initial, true, reducer, |_| false)
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn update_with_hook(
        &mut self,
        name: &str,
        initial: &[u8],
        rebuild: bool,
        reducer: impl FnMut(&mut Vec<u8>, &StoredEvent) -> Result<(), ProjectionError>,
        crash: impl FnMut(ProjectionCrashPoint) -> bool,
    ) -> Result<ProjectionSnapshot, ProjectionError> {
        self.update_with_hook_from(name, initial, 0, rebuild, reducer, crash)
    }

    fn update_with_hook_from(
        &mut self,
        name: &str,
        initial: &[u8],
        initial_checkpoint: u64,
        rebuild: bool,
        mut reducer: impl FnMut(&mut Vec<u8>, &StoredEvent) -> Result<(), ProjectionError>,
        mut crash: impl FnMut(ProjectionCrashPoint) -> bool,
    ) -> Result<ProjectionSnapshot, ProjectionError> {
        if name.is_empty() {
            return Err(ProjectionError::InvalidProjectionName);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let committed: i64 = transaction.query_row(
            "SELECT position FROM commit_watermark WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let committed = checked_nonnegative(committed, "negative commit watermark")?;
        let existing = read_projection(&transaction, name)?;
        let (mut canonical_bytes, checkpoint) = if rebuild {
            (initial.to_vec(), initial_checkpoint)
        } else if let Some(snapshot) = existing {
            if sha256(&snapshot.canonical_bytes) != snapshot.digest {
                return Err(ProjectionError::CorruptData("projection digest mismatch"));
            }
            (snapshot.canonical_bytes, snapshot.checkpoint)
        } else {
            (initial.to_vec(), 0)
        };
        if checkpoint > committed {
            return Err(ProjectionError::CorruptData(
                "projection checkpoint exceeds committed prefix",
            ));
        }
        inject(&mut crash, ProjectionCrashPoint::AfterRead)?;

        let events = read_events(&transaction, checkpoint, committed)?;
        let mut applied = checkpoint;
        for event in events {
            if event.commit_position.get() != applied + 1 {
                return Err(ProjectionError::CorruptData(
                    "event log is not a gapless committed prefix",
                ));
            }
            reducer(&mut canonical_bytes, &event)?;
            applied = event.commit_position.get();
        }
        if applied != committed {
            return Err(ProjectionError::CorruptData(
                "event log ends before committed watermark",
            ));
        }
        inject(&mut crash, ProjectionCrashPoint::AfterReduce)?;

        let digest = sha256(&canonical_bytes);
        let now = authoritative_time(&transaction)?;
        transaction.execute(
            "INSERT INTO projection_state (name, canonical_bytes, digest, checkpoint, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(name) DO UPDATE SET
                 canonical_bytes = excluded.canonical_bytes,
                 digest = excluded.digest,
                 checkpoint = excluded.checkpoint,
                 updated_at = excluded.updated_at",
            params![
                name,
                canonical_bytes,
                digest.as_slice(),
                applied,
                now.as_rfc3339()
            ],
        )?;
        inject(&mut crash, ProjectionCrashPoint::AfterWrite)?;
        inject(&mut crash, ProjectionCrashPoint::BeforeCommit)?;
        transaction.commit()?;
        Ok(ProjectionSnapshot {
            name: name.to_owned(),
            canonical_bytes,
            digest,
            checkpoint: applied,
            updated_at: now.rfc3339,
        })
    }

    pub fn load(&mut self, name: &str) -> Result<Option<ProjectionSnapshot>, ProjectionError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let snapshot = read_projection(&transaction, name)?;
        if let Some(snapshot) = &snapshot {
            let committed: i64 = transaction.query_row(
                "SELECT position FROM commit_watermark WHERE singleton = 1",
                [],
                |row| row.get(0),
            )?;
            if snapshot.checkpoint > checked_nonnegative(committed, "negative commit watermark")? {
                return Err(ProjectionError::CorruptData(
                    "projection checkpoint exceeds committed prefix",
                ));
            }
            if sha256(&snapshot.canonical_bytes) != snapshot.digest {
                return Err(ProjectionError::CorruptData("projection digest mismatch"));
            }
        }
        transaction.commit()?;
        Ok(snapshot)
    }

    pub(crate) fn with_store_time<T>(
        &mut self,
        operation: impl FnOnce(&Transaction<'_>, &StoreTime) -> Result<T, ProjectionError>,
    ) -> Result<T, ProjectionError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = authoritative_time(&transaction)?;
        let result = operation(&transaction, &now)?;
        transaction.commit()?;
        Ok(result)
    }

    pub(crate) fn store_time(&mut self) -> Result<StoreTime, ProjectionError> {
        self.with_store_time(|_, now| Ok(now.clone()))
    }
}

fn read_rebuild_baseline(
    connection: &Connection,
    name: &str,
) -> Result<Option<(Vec<u8>, u64)>, ProjectionError> {
    let row = connection
        .query_row(
            "SELECT canonical_bytes, digest, checkpoint
             FROM projection_rebuild_baseline WHERE name = ?1",
            [name],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|(bytes, digest, checkpoint)| {
        if digest.as_slice() != sha256(&bytes) {
            return Err(ProjectionError::CorruptData(
                "rebuild baseline digest mismatch",
            ));
        }
        DomainReducer::from_canonical_bytes(&bytes)
            .map_err(|error| ProjectionError::Reducer(error.to_string()))?;
        Ok((
            bytes,
            checked_nonnegative(checkpoint, "negative rebuild baseline checkpoint")?,
        ))
    })
    .transpose()
}

#[cfg(any(test, debug_assertions))]
pub(crate) fn rollback_latest_migration(
    path: impl AsRef<Path>,
) -> Result<Option<i64>, ProjectionError> {
    let mut connection = open_connection(path.as_ref())?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_append_schema(&transaction)?;
    create_migration_table(&transaction)?;
    validate_migration_history(&transaction)?;
    let version: Option<i64> = transaction.query_row(
        "SELECT max(version) FROM kit_sqlite_migrations",
        [],
        |row| row.get(0),
    )?;
    let Some(version) = version else {
        transaction.commit()?;
        return Ok(None);
    };
    let migration = MIGRATIONS
        .iter()
        .find(|migration| migration.version == version)
        .ok_or(ProjectionError::UnknownMigration(version))?;
    transaction.execute_batch(migration.down)?;
    transaction.execute(
        "DELETE FROM kit_sqlite_migrations WHERE version = ?1",
        [version],
    )?;
    transaction.commit()?;
    Ok(Some(version))
}

#[cfg(any(test, debug_assertions))]
pub(crate) fn migration_versions() -> impl DoubleEndedIterator<Item = i64> {
    MIGRATIONS.iter().map(|migration| migration.version)
}

fn open_connection(path: &Path) -> Result<Connection, ProjectionError> {
    if path.as_os_str().is_empty() || path == Path::new(":memory:") {
        return Err(ProjectionError::InvalidDatabaseLocation);
    }
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "secure_delete", "ON")?;
    let journal_mode: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(ProjectionError::CorruptData(
            "SQLite append store is not in WAL mode",
        ));
    }
    Ok(connection)
}

fn migrate(connection: &mut Connection) -> Result<(), ProjectionError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    validate_append_schema(&transaction)?;
    create_migration_table(&transaction)?;
    validate_migration_history(&transaction)?;
    for migration in MIGRATIONS {
        let applied: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM kit_sqlite_migrations WHERE version = ?1)",
            [migration.version],
            |row| row.get(0),
        )?;
        if !applied {
            transaction.execute_batch(migration.up)?;
            transaction.execute(
                "INSERT INTO kit_sqlite_migrations (version, name, checksum) VALUES (?1, ?2, ?3)",
                params![
                    migration.version,
                    migration.name,
                    migration.checksum().as_slice()
                ],
            )?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn validate_append_schema(transaction: &Transaction<'_>) -> Result<(), ProjectionError> {
    let version: i64 = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != APPEND_SCHEMA_VERSION {
        return Err(ProjectionError::UnsupportedAppendSchema(version));
    }
    let tables: i64 = transaction.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name IN ('events', 'commit_watermark')",
        [],
        |row| row.get(0),
    )?;
    if tables != 2 {
        return Err(ProjectionError::MissingAppendSchema);
    }
    Ok(())
}

fn create_migration_table(transaction: &Transaction<'_>) -> Result<(), ProjectionError> {
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS kit_sqlite_migrations (
             version INTEGER PRIMARY KEY CHECK (version > 0),
             name TEXT NOT NULL,
             checksum BLOB NOT NULL CHECK (length(checksum) = 32)
         );",
    )?;
    Ok(())
}

fn validate_migration_history(transaction: &Transaction<'_>) -> Result<(), ProjectionError> {
    let mut statement = transaction
        .prepare("SELECT version, name, checksum FROM kit_sqlite_migrations ORDER BY version")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    for (expected_index, row) in rows.enumerate() {
        let (version, name, checksum) = row?;
        let Some(migration) = MIGRATIONS.get(expected_index) else {
            return Err(ProjectionError::UnknownMigration(version));
        };
        if version != migration.version {
            return if version > migration.version {
                Err(ProjectionError::UnknownMigration(version))
            } else {
                Err(ProjectionError::MigrationDrift(version))
            };
        }
        if name != migration.name || checksum.as_slice() != migration.checksum() {
            return Err(ProjectionError::MigrationDrift(version));
        }
    }
    Ok(())
}

fn read_projection(
    transaction: &Transaction<'_>,
    name: &str,
) -> Result<Option<ProjectionSnapshot>, ProjectionError> {
    let row = transaction.query_row(
        "SELECT canonical_bytes, digest, checkpoint, updated_at FROM projection_state WHERE name = ?1",
        [name],
        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?)),
    ).optional()?;
    row.map(|(canonical_bytes, digest, checkpoint, updated_at)| {
        let digest: [u8; 32] = digest
            .try_into()
            .map_err(|_| ProjectionError::CorruptData("invalid projection digest"))?;
        Ok(ProjectionSnapshot {
            name: name.to_owned(),
            canonical_bytes,
            digest,
            checkpoint: checked_nonnegative(checkpoint, "negative projection checkpoint")?,
            updated_at,
        })
    })
    .transpose()
}

fn insert_event_index(
    transaction: &Transaction<'_>,
    position: u64,
    scope: IndexedEventScope,
    stored_at_unix_micros: i64,
) -> Result<(), ProjectionError> {
    transaction.execute(
        "INSERT INTO event_projection_index
         (commit_position, project_id, thread_id, run_id, event_class,
          stored_at_unix_micros, erased)
         VALUES (?1, ?2, ?3, ?4, 'event', ?5, 0)
         ON CONFLICT(commit_position) DO NOTHING",
        params![
            position,
            scope.project_id.to_string(),
            scope.thread_id.map(|id| id.to_string()),
            scope.run_id.map(|id| id.to_string()),
            stored_at_unix_micros,
        ],
    )?;
    Ok(())
}

fn read_indexed_event_projections(
    connection: &Connection,
) -> Result<Vec<EventProjection>, ProjectionError> {
    let mut statement = connection.prepare(
        "SELECT event.commit_position, index_row.project_id, event.event_type,
                event.stream, event.payload
         FROM event_projection_index AS index_row
         JOIN events AS event ON event.commit_position = index_row.commit_position
         WHERE index_row.erased = 0
           AND event.commit_position <= (SELECT position FROM commit_watermark WHERE singleton = 1)
         ORDER BY event.commit_position",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    })?;
    rows.map(|row| {
        let (position, project, operation, stream, payload) = row?;
        Ok(EventProjection {
            cursor: EventCursor::new(checked_positive(position, "invalid event position")?),
            opaque_cursor: None,
            project_id: crate::domain::ids::ProjectId::parse(&project)
                .map_err(|error| ProjectionError::Reducer(error.to_string()))?,
            operation,
            stream,
            payload,
            envelope: Vec::new(),
            authority_digest: String::new(),
            projection_digest: String::new(),
        })
    })
    .collect()
}

fn read_events(
    transaction: &Transaction<'_>,
    after: u64,
    through: u64,
) -> Result<Vec<StoredEvent>, ProjectionError> {
    let mut statement = transaction.prepare(
        "SELECT event_id, stream, sequence, commit_position, event_type, schema_version,
                occurred_at, causation_id, correlation_id, attempt_id, trace_id, payload, artifacts
         FROM events
         WHERE commit_position > ?1 AND commit_position <= ?2
         ORDER BY commit_position",
    )?;
    let rows = statement.query_map(params![after, through], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, Option<String>>(9)?,
            row.get::<_, String>(10)?,
            row.get::<_, Vec<u8>>(11)?,
            row.get::<_, Vec<u8>>(12)?,
        ))
    })?;
    rows.map(|row| {
        let (
            event_id,
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
        ) = row?;
        let parse = |error: &dyn std::error::Error| ProjectionError::Reducer(error.to_string());
        Ok(StoredEvent {
            event: NewEvent {
                id: EventId::parse(&event_id).map_err(|error| parse(&error))?,
                stream: EntityId::from_str(&stream).map_err(|error| parse(&error))?,
                event_type: EventType::parse(&event_type).map_err(|error| parse(&error))?,
                schema_version: SchemaVersion::try_from(
                    u16::try_from(schema_version).map_err(|error| parse(&error))?,
                )
                .map_err(|error| parse(&error))?,
                occurred_at: UtcDateTime::parse(&occurred_at).map_err(|error| parse(&error))?,
                causation_id: CommandId::parse(&causation_id).map_err(|error| parse(&error))?,
                correlation_id: EntityId::from_str(&correlation_id)
                    .map_err(|error| parse(&error))?,
                attempt_id: attempt_id
                    .map(|id| AttemptId::parse(&id))
                    .transpose()
                    .map_err(|error| parse(&error))?,
                trace_id: TraceId::parse(&trace_id).map_err(|error| parse(&error))?,
                payload,
                artifacts,
            },
            sequence: StreamSequence::new(checked_positive(sequence, "invalid event sequence")?)
                .map_err(|error| parse(&error))?,
            commit_position: CommitPosition::new(checked_positive(
                commit_position,
                "invalid commit position",
            )?)
            .map_err(|error| parse(&error))?,
        })
    })
    .collect()
}

fn authoritative_time(transaction: &Transaction<'_>) -> Result<StoreTime, ProjectionError> {
    let unix_micros: i64 = transaction.query_row(
        "UPDATE store_clock
         SET unix_micros = max(
             unix_micros,
             CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)
         )
         WHERE singleton = 1
         RETURNING unix_micros",
        [],
        |row| row.get(0),
    )?;
    if unix_micros < 0 {
        return Err(ProjectionError::CorruptData(
            "negative authoritative store time",
        ));
    }
    let rfc3339: String = transaction.query_row(
        "SELECT strftime('%Y-%m-%dT%H:%M:%fZ', ?1 / 1000000.0, 'unixepoch')",
        [unix_micros],
        |row| row.get(0),
    )?;
    Ok(StoreTime {
        unix_micros,
        rfc3339,
    })
}

fn checked_nonnegative(value: i64, message: &'static str) -> Result<u64, ProjectionError> {
    u64::try_from(value).map_err(|_| ProjectionError::CorruptData(message))
}

fn checked_positive(value: i64, message: &'static str) -> Result<u64, ProjectionError> {
    checked_nonnegative(value, message).and_then(|value| {
        if value == 0 {
            Err(ProjectionError::CorruptData(message))
        } else {
            Ok(value)
        }
    })
}

fn inject(
    crash: &mut impl FnMut(ProjectionCrashPoint) -> bool,
    point: ProjectionCrashPoint,
) -> Result<(), ProjectionError> {
    if crash(point) {
        Err(ProjectionError::InjectedCrash(point))
    } else {
        Ok(())
    }
}

fn put_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    output.extend_from_slice(bytes);
}
