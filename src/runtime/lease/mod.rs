use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::domain::ids::{ArtifactId, AttemptId, DaemonServiceId};
use crate::domain::lifecycle::FencingToken;
use crate::store::sqlite::append::SqliteStore;
use crate::store::sqlite::projection::ProjectionStore;

const DATABASE_NAME: &str = "state.sqlite3";
const LOCK_NAME: &str = ".kit.lock";
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LeaseKey(String);

impl LeaseKey {
    pub fn new(value: impl Into<String>) -> Result<Self, LeaseError> {
        let value = value.into();
        if value.is_empty() || value.len() > 255 || value.chars().any(char::is_control) {
            return Err(LeaseError::InvalidKey);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LeaseOwner {
    Attempt(AttemptId),
    Service(DaemonServiceId),
}

impl LeaseOwner {
    fn parts(self) -> (&'static str, String) {
        match self {
            Self::Attempt(id) => ("attempt", id.to_string()),
            Self::Service(id) => ("service", id.to_string()),
        }
    }

    fn parse(kind: &str, id: &str) -> Result<Self, LeaseError> {
        match kind {
            "attempt" => AttemptId::parse(id)
                .map(Self::Attempt)
                .map_err(|_| LeaseError::CorruptData("invalid attempt lease owner")),
            "service" => DaemonServiceId::parse(id)
                .map(Self::Service)
                .map_err(|_| LeaseError::CorruptData("invalid service lease owner")),
            _ => Err(LeaseError::CorruptData("invalid lease owner kind")),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lease {
    pub key: LeaseKey,
    pub owner: LeaseOwner,
    pub expires_at_unix_micros: i64,
    pub fence: FencingToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseLoss {
    Released,
    Expired,
    OwnerChanged,
    FenceChanged,
}

#[derive(Debug)]
pub enum StateRootLockError {
    AlreadyLocked { root: PathBuf },
    Io(io::Error),
}

impl fmt::Display for StateRootLockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyLocked { root } => {
                write!(f, "state root {} is already locked", root.display())
            }
            Self::Io(error) => write!(f, "state-root lock failed: {error}"),
        }
    }
}

impl std::error::Error for StateRootLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::AlreadyLocked { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum LeaseError {
    Busy,
    Database(rusqlite::Error),
    StateRoot(StateRootLockError),
    StoreSetup(String),
    InvalidKey,
    InvalidDuration,
    LeaseHeld(Lease),
    LeaseLost { key: LeaseKey, reason: LeaseLoss },
    FenceExhausted,
    CorruptData(&'static str),
    AdmissionClosed,
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("SQLite lease store remained busy past its bounded timeout"),
            Self::Database(error) => write!(f, "SQLite lease store error: {error}"),
            Self::StateRoot(error) => error.fmt(f),
            Self::StoreSetup(error) => write!(f, "SQLite store setup failed: {error}"),
            Self::InvalidKey => f.write_str("lease key is invalid"),
            Self::InvalidDuration => {
                f.write_str("lease duration must be positive and representable")
            }
            Self::LeaseHeld(lease) => write!(
                f,
                "lease {} is held through {}",
                lease.key.as_str(),
                lease.expires_at_unix_micros
            ),
            Self::LeaseLost { key, reason } => {
                write!(f, "lease {} was lost: {reason:?}", key.as_str())
            }
            Self::FenceExhausted => f.write_str("lease fencing counter exhausted"),
            Self::CorruptData(message) => write!(f, "corrupt lease data: {message}"),
            Self::AdmissionClosed => f.write_str("runtime admission is closed"),
        }
    }
}

impl std::error::Error for LeaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            Self::StateRoot(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for LeaseError {
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

impl From<StateRootLockError> for LeaseError {
    fn from(error: StateRootLockError) -> Self {
        Self::StateRoot(error)
    }
}

#[derive(Debug)]
pub struct StateRootLock {
    root: PathBuf,
    file: File,
}

impl StateRootLock {
    pub fn acquire(root: impl AsRef<Path>) -> Result<Self, StateRootLockError> {
        fs::create_dir_all(root.as_ref()).map_err(StateRootLockError::Io)?;
        let root = fs::canonicalize(root.as_ref()).map_err(StateRootLockError::Io)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .append(false)
            .open(root.join(LOCK_NAME))
            .map_err(StateRootLockError::Io)?;
        match file.try_lock() {
            Ok(()) => Ok(Self { root, file }),
            Err(TryLockError::WouldBlock) => Err(StateRootLockError::AlreadyLocked { root }),
            Err(TryLockError::Error(error)) => Err(StateRootLockError::Io(error)),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for StateRootLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

pub struct LeaseStore {
    connection: Connection,
}

impl LeaseStore {
    fn open(path: &Path) -> Result<Self, LeaseError> {
        let mut connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS runtime_fence_counters (
                 lease_key TEXT PRIMARY KEY CHECK (lease_key <> ''),
                 fence INTEGER NOT NULL CHECK (fence > 0)
             );
             CREATE TABLE IF NOT EXISTS runtime_leases (
                 lease_key TEXT PRIMARY KEY CHECK (lease_key <> ''),
                 owner_kind TEXT NOT NULL CHECK (owner_kind IN ('attempt', 'service')),
                 owner_id TEXT NOT NULL CHECK (owner_id <> ''),
                 expires_at_unix_micros INTEGER NOT NULL CHECK (expires_at_unix_micros >= 0),
                 fence INTEGER NOT NULL CHECK (fence > 0)
             );
             CREATE TABLE IF NOT EXISTS runtime_recovery_items (
                 item_id TEXT PRIMARY KEY CHECK (item_id <> ''),
                 item_kind TEXT NOT NULL CHECK (item_kind IN ('intent', 'prepared_artifact')),
                 lease_key TEXT NOT NULL CHECK (lease_key <> ''),
                 owner_kind TEXT NOT NULL CHECK (owner_kind IN ('attempt', 'service')),
                 owner_id TEXT NOT NULL CHECK (owner_id <> ''),
                 fence INTEGER NOT NULL CHECK (fence > 0),
                 retry_safe INTEGER NOT NULL CHECK (retry_safe IN (0, 1)),
                 local_effect_quiescent INTEGER NOT NULL CHECK (local_effect_quiescent IN (0, 1))
             );",
        )?;
        transaction.commit()?;
        Ok(Self { connection })
    }

    pub fn acquire(
        &mut self,
        key: LeaseKey,
        owner: LeaseOwner,
        duration: Duration,
    ) -> Result<Lease, LeaseError> {
        let duration = duration_micros(duration)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = authoritative_time(&transaction)?;
        if let Some(lease) = read_lease(&transaction, &key)?
            && lease.expires_at_unix_micros > now
        {
            return Err(LeaseError::LeaseHeld(lease));
        }
        let fence = advance_fence(&transaction, &key)?;
        let expires_at_unix_micros = now
            .checked_add(duration)
            .ok_or(LeaseError::InvalidDuration)?;
        let (owner_kind, owner_id) = owner.parts();
        transaction.execute(
            "INSERT INTO runtime_leases
                 (lease_key, owner_kind, owner_id, expires_at_unix_micros, fence)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(lease_key) DO UPDATE SET
                 owner_kind = excluded.owner_kind,
                 owner_id = excluded.owner_id,
                 expires_at_unix_micros = excluded.expires_at_unix_micros,
                 fence = excluded.fence",
            params![
                key.as_str(),
                owner_kind,
                owner_id,
                expires_at_unix_micros,
                fence
            ],
        )?;
        transaction.commit()?;
        Ok(Lease {
            key,
            owner,
            expires_at_unix_micros,
            fence: FencingToken::new(fence as u64),
        })
    }

    pub fn renew(&mut self, lease: &Lease, duration: Duration) -> Result<Lease, LeaseError> {
        let duration = duration_micros(duration)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = authoritative_time(&transaction)?;
        guard_current(&transaction, now, lease)?;
        let expires_at_unix_micros = now
            .checked_add(duration)
            .ok_or(LeaseError::InvalidDuration)?;
        transaction.execute(
            "UPDATE runtime_leases SET expires_at_unix_micros = ?2
             WHERE lease_key = ?1",
            params![lease.key.as_str(), expires_at_unix_micros],
        )?;
        transaction.commit()?;
        Ok(Lease {
            expires_at_unix_micros,
            ..lease.clone()
        })
    }

    pub fn release(&mut self, lease: &Lease) -> Result<(), LeaseError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = authoritative_time(&transaction)?;
        guard_current(&transaction, now, lease)?;
        advance_fence(&transaction, &lease.key)?;
        transaction.execute(
            "DELETE FROM runtime_leases WHERE lease_key = ?1",
            [lease.key.as_str()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn guarded_commit<T>(
        &mut self,
        lease: &Lease,
        commit: impl FnOnce(&Transaction<'_>) -> rusqlite::Result<T>,
    ) -> Result<T, LeaseError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = authoritative_time(&transaction)?;
        guard_current(&transaction, now, lease)?;
        let result = commit(&transaction)?;
        let commit_time = authoritative_time(&transaction)?;
        guard_current(&transaction, commit_time, lease)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn record_intent(
        &mut self,
        lease: &Lease,
        intent_id: &str,
        retry_safe: bool,
        local_effect_quiescent: bool,
    ) -> Result<(), LeaseError> {
        validate_item_id(intent_id)?;
        let (owner_kind, owner_id) = lease.owner.parts();
        self.guarded_commit(lease, |transaction| {
            transaction.execute(
                "INSERT INTO runtime_recovery_items
                     (item_id, item_kind, lease_key, owner_kind, owner_id, fence,
                      retry_safe, local_effect_quiescent)
                 VALUES (?1, 'intent', ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    intent_id,
                    lease.key.as_str(),
                    owner_kind,
                    owner_id,
                    lease.fence.get(),
                    retry_safe,
                    local_effect_quiescent
                ],
            )?;
            Ok(())
        })
    }

    pub fn record_prepared_artifact(
        &mut self,
        lease: &Lease,
        artifact_id: ArtifactId,
    ) -> Result<(), LeaseError> {
        let item_id = artifact_id.to_string();
        let (owner_kind, owner_id) = lease.owner.parts();
        self.guarded_commit(lease, |transaction| {
            transaction.execute(
                "INSERT INTO runtime_recovery_items
                     (item_id, item_kind, lease_key, owner_kind, owner_id, fence,
                      retry_safe, local_effect_quiescent)
                 VALUES (?1, 'prepared_artifact', ?2, ?3, ?4, ?5, 0, 1)",
                params![
                    item_id,
                    lease.key.as_str(),
                    owner_kind,
                    owner_id,
                    lease.fence.get()
                ],
            )?;
            Ok(())
        })
    }

    pub fn resolve_recovery_item(
        &mut self,
        lease: &Lease,
        item_id: &str,
    ) -> Result<(), LeaseError> {
        validate_item_id(item_id)?;
        self.guarded_commit(lease, |transaction| {
            transaction.execute(
                "DELETE FROM runtime_recovery_items WHERE item_id = ?1",
                [item_id],
            )?;
            Ok(())
        })
    }

    pub fn reconcile_startup(&mut self) -> Result<Vec<ReconciliationAction>, LeaseError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = authoritative_time(&transaction)?;
        let mut actions = Vec::new();
        {
            let mut statement = transaction.prepare(
                "SELECT lease_key, owner_kind, owner_id, fence
                 FROM runtime_leases
                 WHERE expires_at_unix_micros <= ?1
                 ORDER BY lease_key",
            )?;
            let rows = statement.query_map([now], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            for row in rows {
                let (key, owner_kind, owner_id, fence) = row?;
                actions.push(ReconciliationAction::ExpiredLease {
                    key: LeaseKey::new(key)?,
                    owner: LeaseOwner::parse(&owner_kind, &owner_id)?,
                    fence: checked_fence(fence)?,
                });
            }
        }
        {
            let mut statement = transaction.prepare(
                "SELECT item_id, item_kind, lease_key, owner_kind, owner_id, fence,
                        retry_safe, local_effect_quiescent
                 FROM runtime_recovery_items
                 ORDER BY item_id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, bool>(6)?,
                    row.get::<_, bool>(7)?,
                ))
            })?;
            for row in rows {
                let (item_id, kind, key, owner_kind, owner_id, fence, retry_safe, quiescent) = row?;
                let owner = LeaseOwner::parse(&owner_kind, &owner_id)?;
                let key = LeaseKey::new(key)?;
                let fence = checked_fence(fence)?;
                match kind.as_str() {
                    "intent" if retry_safe && quiescent => {
                        actions.push(ReconciliationAction::RetryIntent {
                            intent_id: item_id,
                            key,
                            owner,
                            fence,
                        });
                    }
                    "intent" => actions.push(ReconciliationAction::OutcomeUnknown {
                        intent_id: item_id,
                        key,
                        owner,
                        fence,
                    }),
                    "prepared_artifact" => {
                        let artifact_id = ArtifactId::parse(&item_id)
                            .map_err(|_| LeaseError::CorruptData("invalid prepared artifact ID"))?;
                        actions.push(ReconciliationAction::InspectPreparedArtifact {
                            artifact_id,
                            key,
                            owner,
                            fence,
                        });
                    }
                    _ => return Err(LeaseError::CorruptData("invalid recovery item kind")),
                }
            }
        }
        transaction.commit()?;
        Ok(actions)
    }

    fn flush(&mut self) -> Result<(), LeaseError> {
        self.connection
            .query_row("PRAGMA wal_checkpoint(FULL)", [], |_| Ok(()))?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReconciliationAction {
    ExpiredLease {
        key: LeaseKey,
        owner: LeaseOwner,
        fence: FencingToken,
    },
    RetryIntent {
        intent_id: String,
        key: LeaseKey,
        owner: LeaseOwner,
        fence: FencingToken,
    },
    OutcomeUnknown {
        intent_id: String,
        key: LeaseKey,
        owner: LeaseOwner,
        fence: FencingToken,
    },
    InspectPreparedArtifact {
        artifact_id: ArtifactId,
        key: LeaseKey,
        owner: LeaseOwner,
        fence: FencingToken,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShutdownPhase {
    Serving,
    Quiescing,
    Stopped,
}

struct GateState {
    phase: ShutdownPhase,
    local_effects: usize,
}

struct ShutdownGate {
    state: Mutex<GateState>,
    quiescent: Condvar,
}

pub struct LocalEffectGuard {
    gate: Arc<ShutdownGate>,
}

impl Drop for LocalEffectGuard {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.local_effects -= 1;
        if state.local_effects == 0 {
            self.gate.quiescent.notify_all();
        }
    }
}

pub struct LocalLeaseRuntime {
    lock: Option<StateRootLock>,
    leases: LeaseStore,
    gate: Arc<ShutdownGate>,
}

impl LocalLeaseRuntime {
    pub(crate) fn open(
        root: impl AsRef<Path>,
        authority: &crate::runtime::daemon::ControlPlaneAuthority,
    ) -> Result<Self, LeaseError> {
        let lock = StateRootLock::acquire(root)?;
        let database = lock.root().join(DATABASE_NAME);
        SqliteStore::open(&database, authority)
            .map_err(|error| LeaseError::StoreSetup(error.to_string()))?;
        ProjectionStore::open(&database)
            .map_err(|error| LeaseError::StoreSetup(error.to_string()))?;
        let leases = LeaseStore::open(&database)?;
        Ok(Self {
            lock: Some(lock),
            leases,
            gate: Arc::new(ShutdownGate {
                state: Mutex::new(GateState {
                    phase: ShutdownPhase::Serving,
                    local_effects: 0,
                }),
                quiescent: Condvar::new(),
            }),
        })
    }

    pub fn leases(&mut self) -> &mut LeaseStore {
        &mut self.leases
    }

    pub fn phase(&self) -> ShutdownPhase {
        self.gate
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase
    }

    pub fn admit_local_effect(&self) -> Result<LocalEffectGuard, LeaseError> {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.phase != ShutdownPhase::Serving {
            return Err(LeaseError::AdmissionClosed);
        }
        state.local_effects = state
            .local_effects
            .checked_add(1)
            .ok_or(LeaseError::CorruptData("local effect count overflow"))?;
        Ok(LocalEffectGuard {
            gate: Arc::clone(&self.gate),
        })
    }

    pub fn begin_shutdown(&self) {
        let mut state = self
            .gate
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.phase == ShutdownPhase::Serving {
            state.phase = ShutdownPhase::Quiescing;
        }
    }

    pub fn shutdown(mut self) -> Result<Vec<ReconciliationAction>, LeaseError> {
        self.begin_shutdown();
        {
            let mut state = self
                .gate
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            while state.local_effects != 0 {
                state = self
                    .gate
                    .quiescent
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
        }
        let actions = self.leases.reconcile_startup();
        let flush = self.leases.flush();
        self.gate
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase = ShutdownPhase::Stopped;
        self.lock.take();
        match (actions, flush) {
            (Ok(actions), Ok(())) => Ok(actions),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }
}

fn duration_micros(duration: Duration) -> Result<i64, LeaseError> {
    if duration.is_zero() {
        return Err(LeaseError::InvalidDuration);
    }
    i64::try_from(duration.as_micros()).map_err(|_| LeaseError::InvalidDuration)
}

fn validate_item_id(item_id: &str) -> Result<(), LeaseError> {
    if item_id.is_empty() || item_id.len() > 255 || item_id.chars().any(char::is_control) {
        Err(LeaseError::CorruptData("invalid recovery item ID"))
    } else {
        Ok(())
    }
}

fn authoritative_time(transaction: &Transaction<'_>) -> Result<i64, LeaseError> {
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
        return Err(LeaseError::CorruptData("negative authoritative store time"));
    }
    Ok(unix_micros)
}

fn advance_fence(transaction: &Transaction<'_>, key: &LeaseKey) -> Result<i64, LeaseError> {
    let current: Option<i64> = transaction
        .query_row(
            "SELECT fence FROM runtime_fence_counters WHERE lease_key = ?1",
            [key.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let next = current
        .unwrap_or(0)
        .checked_add(1)
        .filter(|value| *value > 0)
        .ok_or(LeaseError::FenceExhausted)?;
    transaction.execute(
        "INSERT INTO runtime_fence_counters (lease_key, fence) VALUES (?1, ?2)
         ON CONFLICT(lease_key) DO UPDATE SET fence = excluded.fence",
        params![key.as_str(), next],
    )?;
    Ok(next)
}

fn read_lease(transaction: &Transaction<'_>, key: &LeaseKey) -> Result<Option<Lease>, LeaseError> {
    let row = transaction
        .query_row(
            "SELECT owner_kind, owner_id, expires_at_unix_micros, fence
             FROM runtime_leases WHERE lease_key = ?1",
            [key.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(owner_kind, owner_id, expires_at_unix_micros, fence)| {
        if expires_at_unix_micros < 0 {
            return Err(LeaseError::CorruptData("negative lease expiry"));
        }
        Ok(Lease {
            key: key.clone(),
            owner: LeaseOwner::parse(&owner_kind, &owner_id)?,
            expires_at_unix_micros,
            fence: checked_fence(fence)?,
        })
    })
    .transpose()
}

fn checked_fence(fence: i64) -> Result<FencingToken, LeaseError> {
    if fence <= 0 {
        Err(LeaseError::CorruptData("invalid fencing counter"))
    } else {
        Ok(FencingToken::new(fence as u64))
    }
}

fn guard_current(
    transaction: &Transaction<'_>,
    now: i64,
    expected: &Lease,
) -> Result<(), LeaseError> {
    let Some(actual) = read_lease(transaction, &expected.key)? else {
        return Err(LeaseError::LeaseLost {
            key: expected.key.clone(),
            reason: LeaseLoss::Released,
        });
    };
    let reason = if actual.owner != expected.owner {
        Some(LeaseLoss::OwnerChanged)
    } else if actual.fence != expected.fence {
        Some(LeaseLoss::FenceChanged)
    } else if actual.expires_at_unix_micros <= now {
        Some(LeaseLoss::Expired)
    } else {
        None
    };
    match reason {
        Some(reason) => Err(LeaseError::LeaseLost {
            key: expected.key.clone(),
            reason,
        }),
        None => Ok(()),
    }
}
