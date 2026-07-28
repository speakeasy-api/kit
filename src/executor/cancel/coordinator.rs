use std::{
    io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{
    CancellationCommit, CancellationCompletionStatus, CancellationControl, CancellationEffectKind,
    CancellationError, CancellationIntent, CancellationOperationAttempt, CancellationOperationKind,
    CancellationPhase, CancellationPublication, CancellationRecord, CancellationStoreError,
    DurableCancellationStore, ExecutorQuiescence, WorkspaceIdentity, reconcile_cancellation,
    request_cancellation,
};
#[cfg(windows)]
use crate::executor::backends::windows_job::{Job, Recovery, RuntimeBoundary};
#[cfg(windows)]
use crate::executor::process::tree::BoundaryControl;
use crate::{
    domain::{
        ids::{AttemptId, CommandId, PrincipalId, ProcessId, WorkspaceId},
        lifecycle::{AttemptOwnership, FencingToken, ProcessClaim, ProcessOwnership},
    },
    executor::{
        backends::container::limits::{
            ControlEvidence, ControlIdentity, HELPER_PATH, bounded_output,
        },
        process::tree::{BoundaryIdentity, BoundaryKind, Inspection, PersistedBoundary},
    },
};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableBoundaryState {
    Missing,
    NoProcess,
    Active,
    BetweenPhases,
    OutcomeUnknown,
    Quiescent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutorCancellationOutcome {
    Quiescent,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DurableCancellationConfirmation {
    pub request_id: String,
    pub fence: u64,
    pub phase: String,
    pub commit_digest: String,
}

/// Gate used by the run executor before any terminal cancellation transition
/// or scheduler/resource release.
pub trait ExecutorCancellationCoordinator: Send + Sync + 'static {
    fn cancel_attempt(
        &self,
        authority: AttemptOwnership,
    ) -> Result<ExecutorCancellationOutcome, CancellationError>;
}

/// Production coordinator backed by the executor claim registry and durable
/// cancellation journal in the service database.
#[derive(Clone, Debug)]
pub struct SqliteCancellationCoordinator {
    database: PathBuf,
}

impl SqliteCancellationCoordinator {
    pub fn new(database: impl Into<PathBuf>) -> Self {
        Self {
            database: database.into(),
        }
    }

    pub(crate) fn ensure_schema(&self) -> Result<(), CancellationError> {
        let mut connection = open(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_unavailable)?;
        migrate(&transaction)?;
        transaction.commit().map_err(store_unavailable)?;
        Ok(())
    }

    /// Registers the process, boundary, and workspace as one durable claim.
    /// Active execution must call this before releasing the child boundary.
    pub fn register_claim(&self, intent: &CancellationIntent) -> Result<(), CancellationError> {
        let mut connection = open(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_unavailable)?;
        migrate(&transaction)?;
        verify_authority(&transaction, intent.owner)?;
        let changed = transaction
            .execute(
                "INSERT INTO executor_execution_claims
                   (attempt_id, principal_id, fence, request_id, process_id, boundary,
                    workspace_id, acquisition_id, revision, grace_millis,
                    require_kill, require_reap, resolve_unknown)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(attempt_id) DO NOTHING",
                intent_params(intent),
            )
            .map_err(store_unavailable)?;
        if changed == 0
            && load_intent_for_attempt(
                &transaction,
                "executor_execution_claims",
                intent.owner.attempt_id,
            )?
            .as_ref()
                != Some(intent)
        {
            return Err(CancellationStoreError::IdempotencyConflict.into());
        }
        let state_changed = transaction
            .execute(
                "INSERT INTO executor_attempt_boundaries
                   (attempt_id, principal_id, fence, state, request_id)
                 VALUES (?1, ?2, ?3, 'active', ?4)
                 ON CONFLICT(attempt_id) DO UPDATE SET state='active', request_id=excluded.request_id
                     WHERE executor_attempt_boundaries.principal_id=excluded.principal_id
                     AND executor_attempt_boundaries.fence=excluded.fence
                     AND (executor_attempt_boundaries.state='no_process'
                       OR executor_attempt_boundaries.state='between_phases'
                       OR (executor_attempt_boundaries.state='active'
                         AND executor_attempt_boundaries.request_id IS NULL)
                      OR (executor_attempt_boundaries.state='active'
                        AND executor_attempt_boundaries.request_id=excluded.request_id))",
                params![
                    intent.owner.attempt_id.to_string(),
                    intent.owner.principal_id.to_string(),
                    fence(intent.owner)?,
                    intent.request_id.to_string(),
                ],
            )
            .map_err(store_unavailable)?;
        if state_changed != 1 {
            return Err(CancellationStoreError::IdempotencyConflict.into());
        }
        transaction.commit().map_err(store_unavailable)?;
        Ok(())
    }

    pub fn register_no_process(&self, owner: AttemptOwnership) -> Result<(), CancellationError> {
        let mut connection = open(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_unavailable)?;
        migrate(&transaction)?;
        verify_authority(&transaction, owner)?;
        let changed = transaction
            .execute(
                "INSERT INTO executor_attempt_boundaries
                   (attempt_id, principal_id, fence, state, request_id)
                 VALUES (?1, ?2, ?3, 'no_process', NULL)
                 ON CONFLICT(attempt_id) DO NOTHING",
                params![
                    owner.attempt_id.to_string(),
                    owner.principal_id.to_string(),
                    fence(owner)?,
                ],
            )
            .map_err(store_unavailable)?;
        if changed == 0 && boundary_state(&transaction, owner)? != DurableBoundaryState::NoProcess {
            return Err(CancellationStoreError::IdempotencyConflict.into());
        }
        transaction.commit().map_err(store_unavailable)?;
        Ok(())
    }

    pub fn boundary_state(
        &self,
        owner: AttemptOwnership,
    ) -> Result<DurableBoundaryState, CancellationError> {
        let mut connection = open(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_unavailable)?;
        migrate(&transaction)?;
        let state = boundary_state(&transaction, owner)?;
        transaction.commit().map_err(store_unavailable)?;
        Ok(state)
    }

    pub(crate) fn recovery_allows_new_attempt(
        database: impl AsRef<Path>,
        owner: AttemptOwnership,
    ) -> Result<bool, CancellationError> {
        let coordinator = Self::new(database.as_ref());
        coordinator.boundary_state(owner).map(|state| {
            matches!(
                state,
                DurableBoundaryState::NoProcess | DurableBoundaryState::Quiescent
            )
        })
    }

    /// Fences crashed workers and reconciles every durable live boundary before
    /// the daemon admits work. Any unprovable cleanup remains outcome-unknown.
    /// An abandoned proven-quiescent phase reservation is terminalized without
    /// boundary control so it cannot wedge startup.
    pub fn reconcile_startup(&self) -> Result<usize, CancellationError> {
        self.reconcile_startup_with(|intent| ProductionCancellationControl::new(&intent.boundary))
    }

    fn reconcile_startup_with<C>(
        &self,
        mut control_for: impl FnMut(&CancellationIntent) -> io::Result<C>,
    ) -> Result<usize, CancellationError>
    where
        C: CancellationControl,
    {
        let mut connection = open(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_unavailable)?;
        migrate(&transaction)?;
        let attempt_ids = {
            let mut statement = transaction
                .prepare(
                    "SELECT attempt_id FROM executor_attempt_boundaries
                     WHERE state IN ('active', 'outcome_unknown')
                     UNION
                     SELECT claim.attempt_id FROM executor_execution_claims AS claim
                     LEFT JOIN executor_attempt_boundaries AS boundary
                       ON boundary.attempt_id = claim.attempt_id
                     WHERE boundary.attempt_id IS NULL",
                )
                .map_err(store_unavailable)?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(store_unavailable)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(store_unavailable)?
        };
        transaction
            .execute(
                "UPDATE attempt_driver_claims SET quiescent=1
                 WHERE attempt_id IN (
                   SELECT attempt_id FROM executor_attempt_boundaries
                   WHERE state='between_phases'
                 )",
                [],
            )
            .map_err(store_unavailable)?;
        let between_phases = transaction
            .execute(
                "UPDATE executor_attempt_boundaries SET state='quiescent'
                 WHERE state='between_phases'",
                [],
            )
            .map_err(store_unavailable)?;
        for attempt_id in &attempt_ids {
            transaction
                .execute(
                    "UPDATE attempt_driver_claims SET quiescent=1 WHERE attempt_id=?1",
                    [attempt_id],
                )
                .map_err(store_unavailable)?;
        }
        transaction.commit().map_err(store_unavailable)?;

        let mut reconciled = between_phases;
        let mut failures = Vec::new();
        for attempt_id in attempt_ids {
            let attempt_id = match AttemptId::parse(&attempt_id) {
                Ok(attempt_id) => attempt_id,
                Err(error) => {
                    failures.push(format!("invalid executor attempt identity: {error}"));
                    continue;
                }
            };
            let intent =
                load_intent_for_attempt(&connection, "executor_execution_claims", attempt_id)?;
            let Some(intent) = intent else {
                record_recovery_failure(
                    &mut connection,
                    attempt_id,
                    "active boundary has no reconstructible executor claim",
                )?;
                failures.push(format!(
                    "executor boundary {attempt_id} has no reconstructible claim"
                ));
                continue;
            };
            let result = control_for(&intent).and_then(|mut control| {
                let deadline = Instant::now() + CONTROL_TIMEOUT;
                let killed = control.kill_complete_boundary(&intent.boundary, deadline);
                let reaped = control.reap_direct_child(&intent.process, deadline);
                let inspected = control.inspect_boundary(&intent.boundary, deadline);
                let inspection = inspected?;
                if inspection.identity != intent.boundary.identity
                    || inspection.survivors != Some(0)
                    || !inspection.quiescent
                {
                    return Err(io::Error::other(
                        "startup inspection did not prove matching zero-survivor quiescence",
                    ));
                }
                killed?;
                reaped?;
                Ok(())
            });
            match result {
                Ok(()) => {
                    mark_startup_quiescent(&mut connection, &intent)?;
                    reconciled += 1;
                }
                Err(error) => {
                    let reason = format!("startup boundary cleanup was not confirmed: {error}");
                    record_recovery_failure(&mut connection, attempt_id, &reason)?;
                    failures.push(format!("{attempt_id}: {reason}"));
                }
            }
        }
        if failures.is_empty() {
            Ok(reconciled)
        } else {
            Err(CancellationStoreError::Unavailable(failures.join("; ")).into())
        }
    }

    pub(crate) fn confirm_quiescence(
        &self,
        authority: AttemptOwnership,
        request_id: CommandId,
        more_boundaries: bool,
    ) -> Result<DurableCancellationConfirmation, CancellationError> {
        let mut connection = open(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_unavailable)?;
        migrate(&transaction)?;
        verify_authority(&transaction, authority)?;
        let (state, request_id_after) = if more_boundaries {
            ("between_phases", None)
        } else {
            ("quiescent", Some(request_id.to_string()))
        };
        let changed = transaction
            .execute(
                "UPDATE executor_attempt_boundaries SET state=?4, request_id=?5
                 WHERE attempt_id=?1 AND principal_id=?2 AND fence=?3 AND state='active'
                   AND request_id=?6",
                params![
                    authority.attempt_id.to_string(),
                    authority.principal_id.to_string(),
                    fence(authority)?,
                    state,
                    request_id_after,
                    request_id.to_string(),
                ],
            )
            .map_err(store_unavailable)?;
        if changed != 1 {
            return Err(CancellationStoreError::PhaseConflict.into());
        }
        let fence = authority.fencing_token.get();
        let request_id = request_id.to_string();
        let commit_digest = format!(
            "blake3:{}",
            blake3::hash(
                format!(
                    "kit-executor-quiescence-v1\0{request_id}\0{}\0{}\0{fence}\0{state}",
                    authority.attempt_id, authority.principal_id
                )
                .as_bytes()
            )
            .to_hex()
        );
        transaction
            .execute(
                "INSERT INTO executor_quiescence_confirmations
                   (request_id, attempt_id, principal_id, fence, phase, commit_digest)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    request_id,
                    authority.attempt_id.to_string(),
                    authority.principal_id.to_string(),
                    i64::try_from(fence).map_err(|_| CancellationStoreError::StaleOwner)?,
                    state,
                    commit_digest,
                ],
            )
            .map_err(store_unavailable)?;
        transaction
            .execute(
                "DELETE FROM executor_execution_claims WHERE request_id=?1",
                [request_id.to_string()],
            )
            .map_err(store_unavailable)?;
        transaction.commit().map_err(store_unavailable)?;
        Ok(DurableCancellationConfirmation {
            request_id,
            fence,
            phase: state.to_owned(),
            commit_digest,
        })
    }

    /// Closes a phase sequence only when no boundary generation is currently active.
    pub(crate) fn finish_boundary_sequence(
        &self,
        authority: AttemptOwnership,
    ) -> Result<(), CancellationError> {
        let mut connection = open(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_unavailable)?;
        migrate(&transaction)?;
        verify_authority(&transaction, authority)?;
        transaction
            .execute(
                "UPDATE executor_attempt_boundaries SET state='quiescent'
                 WHERE attempt_id=?1 AND principal_id=?2 AND fence=?3
                    AND ((state='active' AND request_id IS NULL)
                      OR state='between_phases')",
                params![
                    authority.attempt_id.to_string(),
                    authority.principal_id.to_string(),
                    fence(authority)?,
                ],
            )
            .map_err(store_unavailable)?;
        transaction.commit().map_err(store_unavailable)?;
        Ok(())
    }
}

impl ExecutorCancellationCoordinator for SqliteCancellationCoordinator {
    fn cancel_attempt(
        &self,
        authority: AttemptOwnership,
    ) -> Result<ExecutorCancellationOutcome, CancellationError> {
        let mut store = SqliteCancellationStore::open(&self.database)?;
        store.verify_authority(authority)?;
        match boundary_state(&store.connection, authority)? {
            DurableBoundaryState::NoProcess | DurableBoundaryState::Quiescent => {
                return Ok(ExecutorCancellationOutcome::Quiescent);
            }
            DurableBoundaryState::BetweenPhases => {
                self.finish_boundary_sequence(authority)?;
                return Ok(ExecutorCancellationOutcome::Quiescent);
            }
            DurableBoundaryState::Missing => {
                return Ok(ExecutorCancellationOutcome::OutcomeUnknown);
            }
            DurableBoundaryState::Active | DurableBoundaryState::OutcomeUnknown => {}
        }
        let intent = store.intent_for_attempt(authority)?;
        let Some(intent) = intent else {
            return Ok(ExecutorCancellationOutcome::OutcomeUnknown);
        };
        let request_id = intent.request_id;
        let mut control = ProductionCancellationControl::new(&intent.boundary)
            .map_err(|error| CancellationStoreError::Unavailable(error.to_string()))?;
        let record = match store.load(request_id, authority) {
            Ok(_) => reconcile_cancellation(
                &mut store,
                &mut control,
                request_id,
                authority,
                CONTROL_TIMEOUT,
            )?,
            Err(CancellationStoreError::NotFound) => {
                request_cancellation(&mut store, &mut control, intent, CONTROL_TIMEOUT)?
            }
            Err(error) => return Err(error.into()),
        };
        Ok(if record.workspace_reassignable() {
            ExecutorCancellationOutcome::Quiescent
        } else {
            ExecutorCancellationOutcome::OutcomeUnknown
        })
    }
}

struct SqliteCancellationStore {
    connection: Connection,
}

impl SqliteCancellationStore {
    fn open(path: &Path) -> Result<Self, CancellationError> {
        let mut connection = open(path)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_unavailable)?;
        migrate(&transaction)?;
        transaction.commit().map_err(store_unavailable)?;
        Ok(Self { connection })
    }

    fn verify_authority(
        &mut self,
        authority: AttemptOwnership,
    ) -> Result<(), CancellationStoreError> {
        verify_authority(&self.connection, authority)
    }

    fn intent_for_attempt(
        &mut self,
        authority: AttemptOwnership,
    ) -> Result<Option<CancellationIntent>, CancellationStoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_unavailable)?;
        verify_authority(&transaction, authority)?;
        let intent = load_intent_for_attempt(
            &transaction,
            "executor_execution_claims",
            authority.attempt_id,
        )?;
        transaction.commit().map_err(store_unavailable)?;
        Ok(intent)
    }
}

impl DurableCancellationStore for SqliteCancellationStore {
    fn request(
        &mut self,
        intent: CancellationIntent,
    ) -> Result<CancellationRecord, CancellationStoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_unavailable)?;
        verify_authority(&transaction, intent.owner)?;
        if let Some(existing) = load_record(&transaction, intent.request_id)? {
            return if existing.intent == intent {
                transaction.commit().map_err(store_unavailable)?;
                Ok(existing)
            } else {
                Err(CancellationStoreError::IdempotencyConflict)
            };
        }
        let changed = transaction
            .execute(
                "INSERT INTO executor_cancellations
                   (attempt_id, principal_id, fence, request_id, process_id, boundary,
                    workspace_id, acquisition_id, revision, grace_millis,
                    require_kill, require_reap, resolve_unknown, phase, unknown_reason,
                    quiescence_attempt_id, quiescence_principal_id, quiescence_fence)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                         'intent', NULL, NULL, NULL, NULL)",
                intent_params(&intent),
            )
            .map_err(store_unavailable)?;
        if changed != 1 {
            return Err(CancellationStoreError::PhaseConflict);
        }
        transaction.commit().map_err(store_unavailable)?;
        Ok(CancellationRecord {
            intent,
            phase: CancellationPhase::IntentPersisted,
            operations: Vec::new(),
            quiescence: None,
            outcome_unknown: None,
        })
    }

    fn load(
        &mut self,
        request_id: CommandId,
        authority: AttemptOwnership,
    ) -> Result<CancellationRecord, CancellationStoreError> {
        verify_authority(&self.connection, authority)?;
        let record =
            load_record(&self.connection, request_id)?.ok_or(CancellationStoreError::NotFound)?;
        same_attempt(&record.intent.owner, &authority)?;
        Ok(record)
    }

    fn commit(
        &mut self,
        commit: CancellationCommit,
    ) -> Result<CancellationRecord, CancellationStoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_unavailable)?;
        verify_authority(&transaction, commit.owner)?;
        let record = load_record(&transaction, commit.request_id)?
            .ok_or(CancellationStoreError::NotFound)?;
        same_attempt(&record.intent.owner, &commit.owner)?;
        if record.phase != commit.expected_phase
            || !valid_transition(record.phase, commit.phase)
            || commit
                .publications
                .iter()
                .any(|publication| publication.owner() != commit.owner)
        {
            return Err(CancellationStoreError::PhaseConflict);
        }
        if let Some(operation) = &commit.operation {
            transaction
                .execute(
                    "INSERT INTO executor_cancellation_operations
                       (request_id, ordinal, kind, operation_attempt, error)
                     VALUES (?1, (SELECT COUNT(*) + 1 FROM executor_cancellation_operations
                                  WHERE request_id = ?1), ?2, ?3, ?4)",
                    params![
                        commit.request_id.to_string(),
                        operation_name(operation.kind),
                        i64::from(operation.attempt),
                        operation.error,
                    ],
                )
                .map_err(store_unavailable)?;
        }
        for publication in &commit.publications {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO executor_cancellation_publications
                       (request_id, publication_key, attempt_id, principal_id, fence)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        commit.request_id.to_string(),
                        publication_key(publication),
                        commit.owner.attempt_id.to_string(),
                        commit.owner.principal_id.to_string(),
                        fence(commit.owner)?,
                    ],
                )
                .map_err(store_unavailable)?;
        }
        let (quiescence_attempt, quiescence_principal, quiescence_fence) = commit
            .quiescence
            .as_ref()
            .map(|quiescence| {
                Ok((
                    Some(quiescence.owner.attempt_id.to_string()),
                    Some(quiescence.owner.principal_id.to_string()),
                    Some(fence(quiescence.owner)?),
                ))
            })
            .transpose()?
            .unwrap_or((None, None, None));
        let changed = transaction
            .execute(
                "UPDATE executor_cancellations SET phase=?2, unknown_reason=?3,
                   quiescence_attempt_id=?4, quiescence_principal_id=?5, quiescence_fence=?6
                 WHERE request_id=?1 AND phase=?7",
                params![
                    commit.request_id.to_string(),
                    phase_name(commit.phase),
                    commit.outcome_unknown,
                    quiescence_attempt,
                    quiescence_principal,
                    quiescence_fence,
                    phase_name(commit.expected_phase),
                ],
            )
            .map_err(store_unavailable)?;
        if changed != 1 {
            return Err(CancellationStoreError::PhaseConflict);
        }
        if commit.phase == CancellationPhase::Quiescent {
            transaction
                .execute(
                    "DELETE FROM executor_execution_claims WHERE request_id=?1",
                    [record.intent.request_id.to_string()],
                )
                .map_err(store_unavailable)?;
            transaction
                .execute(
                    "UPDATE executor_attempt_boundaries SET state='quiescent'
                     WHERE attempt_id=?1 AND request_id=?2",
                    params![
                        record.intent.owner.attempt_id.to_string(),
                        record.intent.request_id.to_string(),
                    ],
                )
                .map_err(store_unavailable)?;
        } else if commit.phase == CancellationPhase::OutcomeUnknown {
            transaction
                .execute(
                    "UPDATE executor_attempt_boundaries SET state='outcome_unknown'
                     WHERE attempt_id=?1 AND request_id=?2",
                    params![
                        record.intent.owner.attempt_id.to_string(),
                        record.intent.request_id.to_string(),
                    ],
                )
                .map_err(store_unavailable)?;
        }
        transaction.commit().map_err(store_unavailable)?;
        load_record(&self.connection, commit.request_id)?.ok_or(CancellationStoreError::NotFound)
    }
}

struct ProductionCancellationControl {
    identity: BoundaryIdentity,
    #[cfg(windows)]
    windows_job: Option<Job>,
    #[cfg(windows)]
    windows_job_boundary: Option<PersistedBoundary>,
    #[cfg(windows)]
    windows_runtime: Option<RuntimeBoundary>,
    #[cfg(windows)]
    windows_runtime_identity: Option<BoundaryIdentity>,
}

impl ProductionCancellationControl {
    fn new(boundary: &PersistedBoundary) -> io::Result<Self> {
        #[cfg(windows)]
        let (windows_job_boundary, windows_runtime_identity) = match boundary
            .windows_layers()
            .map_err(|error| io::Error::other(error.to_string()))?
        {
            Some((job, runtime)) => (
                Some(PersistedBoundary {
                    ownership: boundary.ownership.clone(),
                    identity: job,
                }),
                Some(runtime),
            ),
            None => (None, None),
        };
        Ok(Self {
            identity: boundary.identity.clone(),
            #[cfg(windows)]
            windows_job: None,
            #[cfg(windows)]
            windows_job_boundary,
            #[cfg(windows)]
            windows_runtime: None,
            #[cfg(windows)]
            windows_runtime_identity,
        })
    }

    #[cfg(windows)]
    fn windows_job(&mut self, boundary: &PersistedBoundary) -> io::Result<&mut Job> {
        if self.windows_job.is_none() {
            let job_boundary = self.windows_job_boundary.as_ref().unwrap_or(boundary);
            self.windows_job = Some(match Job::recover(job_boundary, &boundary.ownership) {
                Ok(Recovery::Reopened(job)) => job,
                Ok(Recovery::OutcomeUnknown { detail }) => {
                    return Err(io::Error::other(format!("outcome_unknown: {detail}")));
                }
                Err(error) => return Err(io::Error::other(error.to_string())),
            });
        }
        Ok(self.windows_job.as_mut().expect("Windows Job was set"))
    }

    #[cfg(windows)]
    fn windows_runtime(&mut self) -> io::Result<&mut RuntimeBoundary> {
        if self.windows_runtime.is_none() {
            let identity = self.windows_runtime_identity.as_ref().ok_or_else(|| {
                io::Error::other("persisted Windows composite is missing its runtime layer")
            })?;
            self.windows_runtime = Some(
                RuntimeBoundary::recover(identity)
                    .map_err(|error| io::Error::other(error.to_string()))?,
            );
        }
        Ok(self
            .windows_runtime
            .as_mut()
            .expect("Windows runtime was set"))
    }

    fn helper(&self, operation: &str, deadline: Instant) -> io::Result<ControlEvidence> {
        if !matches!(
            self.identity.kind(),
            BoundaryKind::Container | BoundaryKind::LinuxCgroupV2
        ) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "persisted boundary has no reconstructible production adapter",
            ));
        }
        let identity = ControlIdentity::from_boundary(&self.identity)?;
        let output = bounded_output(
            Path::new(HELPER_PATH),
            identity.arguments(operation),
            deadline,
            4096,
        )?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "trusted helper rejected boundary {operation}"
            )));
        }
        let transcript = String::from_utf8(output.stdout)
            .map_err(|_| io::Error::other("trusted helper returned non-UTF-8 control evidence"))?;
        identity.parse_evidence(&transcript)
    }

    fn inspect(&self, deadline: Instant) -> io::Result<Inspection> {
        let evidence = self.helper("inspect", deadline)?;
        Ok(Inspection {
            identity: self.identity.clone(),
            survivors: Some(evidence.survivors),
            quiescent: evidence.boundary_absent && evidence.survivors == 0,
        })
    }
}

impl CancellationControl for ProductionCancellationControl {
    fn boundary_identity(&self) -> &BoundaryIdentity {
        &self.identity
    }

    fn request_grace_and_wait(
        &mut self,
        _process: &ProcessClaim,
        _boundary: &PersistedBoundary,
        deadline: Instant,
    ) -> io::Result<()> {
        #[cfg(windows)]
        if matches!(
            self.identity.kind(),
            BoundaryKind::WindowsJobObject | BoundaryKind::WindowsComposite
        ) {
            while Instant::now() < deadline {
                let job_empty = self.windows_job(_boundary)?.active_processes()? == 0;
                let runtime_empty = if self.identity.kind() == BoundaryKind::WindowsComposite {
                    let inspection = self.windows_runtime()?.inspect(deadline)?;
                    inspection.survivors == Some(0) && inspection.quiescent
                } else {
                    true
                };
                if job_empty && runtime_empty {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(2));
            }
            return Ok(());
        }
        self.helper("grace", deadline)?;
        while Instant::now() < deadline {
            if self.inspect(deadline)?.quiescent {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(2));
        }
        Ok(())
    }

    fn kill_complete_boundary(
        &mut self,
        _boundary: &PersistedBoundary,
        deadline: Instant,
    ) -> io::Result<()> {
        #[cfg(windows)]
        if matches!(
            self.identity.kind(),
            BoundaryKind::WindowsJobObject | BoundaryKind::WindowsComposite
        ) {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Windows Job kill deadline elapsed",
                ));
            }
            let job = self.windows_job(_boundary).and_then(|job| job.terminate());
            let runtime = if self.identity.kind() == BoundaryKind::WindowsComposite {
                self.windows_runtime()
                    .and_then(|runtime| runtime.kill_boundary(deadline))
            } else {
                Ok(())
            };
            return job.and(runtime);
        }
        self.helper("kill", deadline).map(drop)
    }

    fn reap_direct_child(&mut self, _process: &ProcessClaim, deadline: Instant) -> io::Result<()> {
        #[cfg(windows)]
        if matches!(
            self.identity.kind(),
            BoundaryKind::WindowsJobObject | BoundaryKind::WindowsComposite
        ) {
            let job_result = self
                .windows_job
                .as_mut()
                .ok_or_else(|| io::Error::other("Windows Job was not authenticated before reap"))
                .and_then(|job| job.wait_and_reap(deadline));
            let runtime_result = if self.identity.kind() == BoundaryKind::WindowsComposite {
                self.windows_runtime()
                    .and_then(|runtime| runtime.wait_and_reap(deadline))
            } else {
                Ok(())
            };
            return job_result.and(runtime_result);
        }
        let evidence = self.helper("reap", deadline)?;
        if evidence.direct_child_reaped {
            Ok(())
        } else {
            Err(io::Error::other(
                "trusted helper did not prove execution supervisor reaped or adopted and gone",
            ))
        }
    }

    fn inspect_boundary(
        &mut self,
        _boundary: &PersistedBoundary,
        deadline: Instant,
    ) -> io::Result<Inspection> {
        #[cfg(windows)]
        if matches!(
            self.identity.kind(),
            BoundaryKind::WindowsJobObject | BoundaryKind::WindowsComposite
        ) {
            let job = self
                .windows_job(_boundary)
                .and_then(|job| job.inspect(deadline));
            if self.identity.kind() == BoundaryKind::WindowsJobObject {
                return job;
            }
            let runtime = self
                .windows_runtime()
                .and_then(|runtime| runtime.inspect(deadline));
            let (job, runtime) = match (job, runtime) {
                (Ok(job), Ok(runtime)) => (job, runtime),
                (Err(job), Err(runtime)) => {
                    return Err(io::Error::other(format!(
                        "both Windows containment inspections failed: Job: {job}; runtime: {runtime}"
                    )));
                }
                (Err(error), _) | (_, Err(error)) => return Err(error),
            };
            let survivors = job
                .survivors
                .zip(runtime.survivors)
                .and_then(|(job, runtime)| job.checked_add(runtime));
            return Ok(Inspection {
                identity: self.identity.clone(),
                survivors,
                quiescent: job.quiescent
                    && runtime.quiescent
                    && job.survivors == Some(0)
                    && runtime.survivors == Some(0),
            });
        }
        self.inspect(deadline)
    }
}

fn open(path: &Path) -> Result<Connection, CancellationError> {
    Connection::open(path)
        .map_err(store_unavailable)
        .map_err(CancellationError::Store)
}

fn migrate(transaction: &Transaction<'_>) -> Result<(), CancellationStoreError> {
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS executor_execution_claims (
               attempt_id TEXT PRIMARY KEY, principal_id TEXT NOT NULL, fence INTEGER NOT NULL,
               request_id TEXT NOT NULL UNIQUE, process_id TEXT NOT NULL, boundary TEXT NOT NULL,
               workspace_id TEXT NOT NULL, acquisition_id TEXT NOT NULL, revision TEXT NOT NULL,
               grace_millis INTEGER NOT NULL, require_kill INTEGER NOT NULL,
               require_reap INTEGER NOT NULL, resolve_unknown INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS executor_cancellations (
               attempt_id TEXT NOT NULL, principal_id TEXT NOT NULL, fence INTEGER NOT NULL,
               request_id TEXT PRIMARY KEY, process_id TEXT NOT NULL, boundary TEXT NOT NULL,
               workspace_id TEXT NOT NULL, acquisition_id TEXT NOT NULL, revision TEXT NOT NULL,
               grace_millis INTEGER NOT NULL, require_kill INTEGER NOT NULL,
               require_reap INTEGER NOT NULL, resolve_unknown INTEGER NOT NULL,
               phase TEXT NOT NULL, unknown_reason TEXT,
               quiescence_attempt_id TEXT, quiescence_principal_id TEXT, quiescence_fence INTEGER
             );
             CREATE TABLE IF NOT EXISTS executor_cancellation_operations (
               request_id TEXT NOT NULL, ordinal INTEGER NOT NULL, kind TEXT NOT NULL,
               operation_attempt INTEGER NOT NULL, error TEXT,
               PRIMARY KEY(request_id, ordinal)
             );
              CREATE TABLE IF NOT EXISTS executor_cancellation_publications (
               request_id TEXT NOT NULL, publication_key TEXT NOT NULL,
               attempt_id TEXT NOT NULL, principal_id TEXT NOT NULL, fence INTEGER NOT NULL,
                PRIMARY KEY(request_id, publication_key)
             );
              CREATE TABLE IF NOT EXISTS executor_attempt_boundaries (
                attempt_id TEXT PRIMARY KEY, principal_id TEXT NOT NULL,
                fence INTEGER NOT NULL, state TEXT NOT NULL,
                request_id TEXT,
                 CHECK (state IN (
                   'no_process', 'active', 'between_phases', 'outcome_unknown', 'quiescent'
                 ))
              );
              CREATE TABLE IF NOT EXISTS executor_quiescence_confirmations (
                request_id TEXT PRIMARY KEY, attempt_id TEXT NOT NULL,
                principal_id TEXT NOT NULL, fence INTEGER NOT NULL,
                phase TEXT NOT NULL, commit_digest TEXT NOT NULL
              );
              CREATE TABLE IF NOT EXISTS executor_recovery_failures (
                 attempt_id TEXT PRIMARY KEY, reason TEXT NOT NULL,
                 recorded_at_unix_micros INTEGER NOT NULL
              );",
        )
        .map_err(store_unavailable)?;
    let supports_between_phases = transaction
        .query_row(
            "SELECT sql LIKE '%between_phases%' FROM sqlite_master
             WHERE type='table' AND name='executor_attempt_boundaries'",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(store_unavailable)?;
    if !supports_between_phases {
        transaction
            .execute_batch(
                "ALTER TABLE executor_attempt_boundaries
                   RENAME TO executor_attempt_boundaries_legacy;
                 CREATE TABLE executor_attempt_boundaries (
                   attempt_id TEXT PRIMARY KEY, principal_id TEXT NOT NULL,
                   fence INTEGER NOT NULL, state TEXT NOT NULL, request_id TEXT,
                   CHECK (state IN (
                     'no_process', 'active', 'between_phases', 'outcome_unknown', 'quiescent'
                   ))
                 );
                 INSERT INTO executor_attempt_boundaries
                   (attempt_id, principal_id, fence, state, request_id)
                   SELECT attempt_id, principal_id, fence, state, request_id
                   FROM executor_attempt_boundaries_legacy;
                 DROP TABLE executor_attempt_boundaries_legacy;",
            )
            .map_err(store_unavailable)?;
    }
    Ok(())
}

fn boundary_state(
    connection: &Connection,
    owner: AttemptOwnership,
) -> Result<DurableBoundaryState, CancellationStoreError> {
    let row = connection
        .query_row(
            "SELECT principal_id, fence, state FROM executor_attempt_boundaries
             WHERE attempt_id=?1",
            [owner.attempt_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(store_unavailable)?;
    let Some((principal, stored_fence, state)) = row else {
        return Ok(DurableBoundaryState::Missing);
    };
    let stored_fence = u64::try_from(stored_fence).map_err(|_| {
        CancellationStoreError::Unavailable("invalid durable boundary fence".to_owned())
    })?;
    if principal != owner.principal_id.to_string() || owner.fencing_token.get() < stored_fence {
        return Err(CancellationStoreError::StaleOwner);
    }
    match state.as_str() {
        "no_process" => Ok(DurableBoundaryState::NoProcess),
        "active" => Ok(DurableBoundaryState::Active),
        "between_phases" => Ok(DurableBoundaryState::BetweenPhases),
        "outcome_unknown" => Ok(DurableBoundaryState::OutcomeUnknown),
        "quiescent" => Ok(DurableBoundaryState::Quiescent),
        _ => Err(CancellationStoreError::Unavailable(
            "invalid durable boundary state".to_owned(),
        )),
    }
}

fn verify_authority(
    connection: &Connection,
    authority: AttemptOwnership,
) -> Result<(), CancellationStoreError> {
    let current = connection
        .query_row(
            "SELECT attempt_id, principal_id, fence FROM attempt_driver_claims
             WHERE attempt_id=?1 AND expires_at_unix_micros >
               CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)
               AND quiescent=0",
            [authority.attempt_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .map_err(store_unavailable)?
        .ok_or(CancellationStoreError::Unauthorized)?;
    if current.1 != authority.principal_id.to_string() {
        return Err(CancellationStoreError::Unauthorized);
    }
    if current.0 != authority.attempt_id.to_string()
        || u64::try_from(current.2).ok() != Some(authority.fencing_token.get())
    {
        return Err(CancellationStoreError::StaleOwner);
    }
    Ok(())
}

fn mark_startup_quiescent(
    connection: &mut Connection,
    intent: &CancellationIntent,
) -> Result<(), CancellationStoreError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(store_unavailable)?;
    let changed = transaction
        .execute(
            "UPDATE executor_attempt_boundaries SET state='quiescent', request_id=?4
             WHERE attempt_id=?1 AND principal_id=?2 AND fence=?3
               AND state IN ('active', 'outcome_unknown')",
            params![
                intent.owner.attempt_id.to_string(),
                intent.owner.principal_id.to_string(),
                fence(intent.owner)?,
                intent.request_id.to_string(),
            ],
        )
        .map_err(store_unavailable)?;
    if changed != 1 {
        return Err(CancellationStoreError::PhaseConflict);
    }
    transaction
        .execute(
            "DELETE FROM executor_execution_claims WHERE request_id=?1",
            [intent.request_id.to_string()],
        )
        .map_err(store_unavailable)?;
    transaction
        .execute(
            "DELETE FROM executor_recovery_failures WHERE attempt_id=?1",
            [intent.owner.attempt_id.to_string()],
        )
        .map_err(store_unavailable)?;
    transaction.commit().map_err(store_unavailable)
}

fn record_recovery_failure(
    connection: &mut Connection,
    attempt_id: AttemptId,
    reason: &str,
) -> Result<(), CancellationStoreError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(store_unavailable)?;
    transaction
        .execute(
            "UPDATE executor_attempt_boundaries SET state='outcome_unknown'
             WHERE attempt_id=?1 AND state IN ('active', 'outcome_unknown')",
            [attempt_id.to_string()],
        )
        .map_err(store_unavailable)?;
    transaction
        .execute(
            "INSERT INTO executor_recovery_failures
               (attempt_id, reason, recorded_at_unix_micros)
             VALUES (?1, ?2,
               CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER))
             ON CONFLICT(attempt_id) DO UPDATE SET reason=excluded.reason,
               recorded_at_unix_micros=excluded.recorded_at_unix_micros",
            params![attempt_id.to_string(), reason],
        )
        .map_err(store_unavailable)?;
    transaction.commit().map_err(store_unavailable)
}

fn load_intent_for_attempt(
    connection: &Connection,
    table: &str,
    attempt_id: AttemptId,
) -> Result<Option<CancellationIntent>, CancellationStoreError> {
    load_intent(connection, table, "attempt_id", attempt_id.to_string())
}

fn load_intent_for_request(
    connection: &Connection,
    table: &str,
    request_id: CommandId,
) -> Result<Option<CancellationIntent>, CancellationStoreError> {
    load_intent(connection, table, "request_id", request_id.to_string())
}

fn load_intent(
    connection: &Connection,
    table: &str,
    key: &str,
    value: String,
) -> Result<Option<CancellationIntent>, CancellationStoreError> {
    let sql = format!(
        "SELECT attempt_id, principal_id, fence, request_id, process_id, boundary, workspace_id,
                acquisition_id, revision, grace_millis, require_kill, require_reap,
                resolve_unknown FROM {table} WHERE {key}=?1"
    );
    connection
        .query_row(&sql, [value], |row| {
            let attempt_id = parse_attempt(row.get::<_, String>(0)?).map_err(to_sql_error)?;
            let principal = parse_principal(row.get::<_, String>(1)?)?;
            let fence = parse_fence(row.get::<_, i64>(2)?)?;
            let owner = AttemptOwnership::new(attempt_id, principal, fence);
            let process = ProcessClaim::new(
                parse_process(row.get::<_, String>(4)?)?,
                ProcessOwnership::Attempt(owner),
            );
            let boundary =
                PersistedBoundary::decode(&row.get::<_, String>(5)?).map_err(to_sql_error)?;
            let workspace = WorkspaceIdentity::new(
                parse_workspace(row.get::<_, String>(6)?)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
            )
            .map_err(to_sql_error)?;
            let grace = u64::try_from(row.get::<_, i64>(9)?)
                .map(Duration::from_millis)
                .map_err(to_sql_error)?;
            let mut intent = CancellationIntent::new(
                parse_command(row.get::<_, String>(3)?)?,
                owner,
                process,
                boundary,
                workspace,
                grace,
            )
            .map_err(to_sql_error)?;
            intent.policy.require_kill_confirmation = row.get::<_, bool>(10)?;
            intent.policy.require_reap_confirmation = row.get::<_, bool>(11)?;
            intent
                .policy
                .resolve_unknown_with_matching_zero_survivor_inspection = row.get::<_, bool>(12)?;
            Ok(intent)
        })
        .optional()
        .map_err(store_unavailable)
}

fn load_record(
    connection: &Connection,
    request_id: CommandId,
) -> Result<Option<CancellationRecord>, CancellationStoreError> {
    let Some(intent) = load_intent_for_request(connection, "executor_cancellations", request_id)?
    else {
        return Ok(None);
    };
    let (phase, reason, qa, qp, qf) = connection
        .query_row(
            "SELECT phase, unknown_reason, quiescence_attempt_id,
                    quiescence_principal_id, quiescence_fence
             FROM executor_cancellations WHERE request_id=?1",
            [request_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .map_err(store_unavailable)?;
    let phase = parse_phase(&phase)?;
    let mut statement = connection
        .prepare(
            "SELECT kind, operation_attempt, error FROM executor_cancellation_operations
             WHERE request_id=?1 ORDER BY ordinal",
        )
        .map_err(store_unavailable)?;
    let operations = statement
        .query_map([request_id.to_string()], |row| {
            Ok(CancellationOperationAttempt {
                kind: parse_operation(&row.get::<_, String>(0)?).map_err(to_sql_error)?,
                attempt: u32::try_from(row.get::<_, i64>(1)?).map_err(to_sql_error)?,
                error: row.get(2)?,
            })
        })
        .map_err(store_unavailable)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(store_unavailable)?;
    let quiescence = match (qa, qp, qf) {
        (Some(attempt), Some(principal), Some(fence)) => Some(ExecutorQuiescence {
            owner: AttemptOwnership::new(
                parse_attempt(attempt).map_err(store_unavailable)?,
                PrincipalId::parse(&principal).map_err(store_unavailable)?,
                parse_fence(fence).map_err(store_unavailable)?,
            ),
            process: intent.process,
            boundary: intent.boundary.identity.clone(),
            workspace: intent.workspace.clone(),
            survivors: 0,
        }),
        (None, None, None) => None,
        _ => {
            return Err(CancellationStoreError::Unavailable(
                "partial cancellation quiescence record".to_owned(),
            ));
        }
    };
    Ok(Some(CancellationRecord {
        intent,
        phase,
        operations,
        quiescence,
        outcome_unknown: reason,
    }))
}

fn intent_params(
    intent: &CancellationIntent,
) -> rusqlite::ParamsFromIter<Vec<rusqlite::types::Value>> {
    rusqlite::params_from_iter(vec![
        intent.owner.attempt_id.to_string().into(),
        intent.owner.principal_id.to_string().into(),
        i64::try_from(intent.owner.fencing_token.get())
            .unwrap_or(i64::MAX)
            .into(),
        intent.request_id.to_string().into(),
        intent.process.process_id.to_string().into(),
        intent.boundary.encode().into(),
        intent.workspace.workspace_id.to_string().into(),
        intent.workspace.acquisition_id.clone().into(),
        intent.workspace.revision.clone().into(),
        i64::try_from(intent.grace_period.as_millis())
            .unwrap_or(i64::MAX)
            .into(),
        intent.policy.require_kill_confirmation.into(),
        intent.policy.require_reap_confirmation.into(),
        intent
            .policy
            .resolve_unknown_with_matching_zero_survivor_inspection
            .into(),
    ])
}

fn valid_transition(from: CancellationPhase, to: CancellationPhase) -> bool {
    matches!(
        (from, to),
        (
            CancellationPhase::IntentPersisted,
            CancellationPhase::GraceRequested
        ) | (
            CancellationPhase::GraceRequested,
            CancellationPhase::KillRequested
        ) | (
            CancellationPhase::KillRequested,
            CancellationPhase::ReapRequested
        ) | (
            CancellationPhase::ReapRequested,
            CancellationPhase::InspectRequested
        ) | (
            CancellationPhase::InspectRequested,
            CancellationPhase::Quiescent
        ) | (_, CancellationPhase::OutcomeUnknown)
            | (
                CancellationPhase::OutcomeUnknown,
                CancellationPhase::Quiescent
            )
    )
}

fn same_attempt(
    original: &AttemptOwnership,
    authority: &AttemptOwnership,
) -> Result<(), CancellationStoreError> {
    if original.attempt_id == authority.attempt_id
        && original.principal_id == authority.principal_id
    {
        Ok(())
    } else {
        Err(CancellationStoreError::Unauthorized)
    }
}

fn phase_name(phase: CancellationPhase) -> &'static str {
    match phase {
        CancellationPhase::IntentPersisted => "intent",
        CancellationPhase::GraceRequested => "grace",
        CancellationPhase::KillRequested => "kill",
        CancellationPhase::ReapRequested => "reap",
        CancellationPhase::InspectRequested => "inspect",
        CancellationPhase::Quiescent => "quiescent",
        CancellationPhase::OutcomeUnknown => "unknown",
    }
}

fn parse_phase(value: &str) -> Result<CancellationPhase, CancellationStoreError> {
    match value {
        "intent" => Ok(CancellationPhase::IntentPersisted),
        "grace" => Ok(CancellationPhase::GraceRequested),
        "kill" => Ok(CancellationPhase::KillRequested),
        "reap" => Ok(CancellationPhase::ReapRequested),
        "inspect" => Ok(CancellationPhase::InspectRequested),
        "quiescent" => Ok(CancellationPhase::Quiescent),
        "unknown" => Ok(CancellationPhase::OutcomeUnknown),
        _ => Err(CancellationStoreError::Unavailable(
            "invalid durable cancellation phase".to_owned(),
        )),
    }
}

fn operation_name(kind: CancellationOperationKind) -> &'static str {
    match kind {
        CancellationOperationKind::GraceAndWait => "grace",
        CancellationOperationKind::KillBoundary => "kill",
        CancellationOperationKind::ReapDirectChild => "reap",
        CancellationOperationKind::InspectBoundary => "inspect",
    }
}

fn parse_operation(value: &str) -> Result<CancellationOperationKind, &'static str> {
    match value {
        "grace" => Ok(CancellationOperationKind::GraceAndWait),
        "kill" => Ok(CancellationOperationKind::KillBoundary),
        "reap" => Ok(CancellationOperationKind::ReapDirectChild),
        "inspect" => Ok(CancellationOperationKind::InspectBoundary),
        _ => Err("invalid cancellation operation"),
    }
}

fn publication_key(publication: &CancellationPublication) -> &'static str {
    match publication {
        CancellationPublication::Effect(effect) => match effect.kind {
            CancellationEffectKind::GraceRequested => "effect-grace",
            CancellationEffectKind::BoundaryKilled => "effect-kill",
            CancellationEffectKind::DirectChildReaped => "effect-reap",
            CancellationEffectKind::BoundaryInspected => "effect-inspect",
        },
        CancellationPublication::Completion(completion) => match completion.status {
            CancellationCompletionStatus::Cancelled => "completion-cancelled",
            CancellationCompletionStatus::OutcomeUnknown => "completion-unknown",
        },
    }
}

fn parse_attempt(value: String) -> Result<AttemptId, String> {
    AttemptId::parse(&value).map_err(|error| error.to_string())
}

fn parse_principal(value: String) -> Result<PrincipalId, rusqlite::Error> {
    PrincipalId::parse(&value).map_err(to_sql_error)
}

fn parse_process(value: String) -> Result<ProcessId, rusqlite::Error> {
    ProcessId::parse(&value).map_err(to_sql_error)
}

fn parse_workspace(value: String) -> Result<WorkspaceId, rusqlite::Error> {
    WorkspaceId::parse(&value).map_err(to_sql_error)
}

fn parse_command(value: String) -> Result<CommandId, rusqlite::Error> {
    CommandId::parse(&value).map_err(to_sql_error)
}

fn parse_fence(value: i64) -> Result<FencingToken, rusqlite::Error> {
    u64::try_from(value)
        .map(FencingToken::new)
        .map_err(to_sql_error)
}

fn fence(owner: AttemptOwnership) -> Result<i64, CancellationStoreError> {
    i64::try_from(owner.fencing_token.get()).map_err(|_| CancellationStoreError::StaleOwner)
}

fn to_sql_error(error: impl fmt::Display) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(error.to_string().into())
}

fn store_unavailable(error: impl fmt::Display) -> CancellationStoreError {
    CancellationStoreError::Unavailable(error.to_string())
}

use std::fmt;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::process::tree::Ownership;
    use std::sync::{Arc, Mutex};

    struct RecoveryControl {
        identity: BoundaryIdentity,
        fail: Option<&'static str>,
    }

    struct CompositeRecoveryControl {
        identity: BoundaryIdentity,
        calls: Arc<Mutex<Vec<&'static str>>>,
        fail_runtime_kill: bool,
    }

    impl CancellationControl for CompositeRecoveryControl {
        fn boundary_identity(&self) -> &BoundaryIdentity {
            &self.identity
        }

        fn request_grace_and_wait(
            &mut self,
            _process: &ProcessClaim,
            _boundary: &PersistedBoundary,
            _deadline: Instant,
        ) -> io::Result<()> {
            Ok(())
        }

        fn kill_complete_boundary(
            &mut self,
            _boundary: &PersistedBoundary,
            _deadline: Instant,
        ) -> io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .extend(["job-kill", "runtime-kill"]);
            if self.fail_runtime_kill {
                Err(io::Error::other("injected runtime-layer kill failure"))
            } else {
                Ok(())
            }
        }

        fn reap_direct_child(
            &mut self,
            _process: &ProcessClaim,
            _deadline: Instant,
        ) -> io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .extend(["job-reap", "runtime-reap"]);
            Ok(())
        }

        fn inspect_boundary(
            &mut self,
            _boundary: &PersistedBoundary,
            _deadline: Instant,
        ) -> io::Result<Inspection> {
            self.calls
                .lock()
                .unwrap()
                .extend(["job-inspect", "runtime-inspect"]);
            Ok(Inspection {
                identity: self.identity.clone(),
                survivors: Some(0),
                quiescent: true,
            })
        }
    }

    impl CancellationControl for RecoveryControl {
        fn boundary_identity(&self) -> &BoundaryIdentity {
            &self.identity
        }

        fn request_grace_and_wait(
            &mut self,
            _process: &ProcessClaim,
            _boundary: &PersistedBoundary,
            _deadline: Instant,
        ) -> io::Result<()> {
            Ok(())
        }

        fn kill_complete_boundary(
            &mut self,
            _boundary: &PersistedBoundary,
            _deadline: Instant,
        ) -> io::Result<()> {
            if self.fail == Some("kill") {
                Err(io::Error::other("injected kill failure"))
            } else {
                Ok(())
            }
        }

        fn reap_direct_child(
            &mut self,
            _process: &ProcessClaim,
            _deadline: Instant,
        ) -> io::Result<()> {
            if self.fail == Some("reap") {
                Err(io::Error::other("injected reap failure"))
            } else {
                Ok(())
            }
        }

        fn inspect_boundary(
            &mut self,
            _boundary: &PersistedBoundary,
            _deadline: Instant,
        ) -> io::Result<Inspection> {
            if self.fail == Some("inspect") {
                return Err(io::Error::other("injected inspection failure"));
            }
            Ok(Inspection {
                identity: self.identity.clone(),
                survivors: Some(0),
                quiescent: true,
            })
        }
    }

    fn setup() -> (PathBuf, AttemptOwnership) {
        let path = std::env::temp_dir().join(format!(
            "kit-cancellation-sequence-{}-{}",
            std::process::id(),
            CommandId::generate().unwrap()
        ));
        let owner = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            PrincipalId::generate().unwrap(),
            FencingToken::new(7),
        );
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE attempt_driver_claims (
                   run_id TEXT PRIMARY KEY, attempt_id TEXT NOT NULL UNIQUE,
                   principal_id TEXT NOT NULL, fence INTEGER NOT NULL,
                   lease_version INTEGER NOT NULL, expires_at_unix_micros INTEGER NOT NULL,
                   quiescent INTEGER NOT NULL
                  );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO attempt_driver_claims
                   (run_id, attempt_id, principal_id, fence, lease_version,
                    expires_at_unix_micros, quiescent)
                 VALUES (?1, ?2, ?3, ?4, 1,
                   CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER) + 60000000,
                   0)",
                params![
                    crate::domain::ids::RunId::generate().unwrap().to_string(),
                    owner.attempt_id.to_string(),
                    owner.principal_id.to_string(),
                    i64::try_from(owner.fencing_token.get()).unwrap(),
                ],
            )
            .unwrap();
        (path, owner)
    }

    fn intent(owner: AttemptOwnership, suffix: &str) -> CancellationIntent {
        let process = ProcessClaim::new(
            ProcessId::generate().unwrap(),
            ProcessOwnership::Attempt(owner),
        );
        CancellationIntent::new(
            CommandId::generate().unwrap(),
            owner,
            process,
            PersistedBoundary {
                ownership: Ownership::new(
                    serde_json::to_string(&process.owner).unwrap(),
                    process.process_id.to_string(),
                )
                .unwrap(),
                identity: BoundaryIdentity::new(
                    BoundaryKind::Container,
                    format!("trial-{suffix}"),
                    "a".repeat(64),
                    format!("start-{suffix}"),
                )
                .unwrap(),
            },
            WorkspaceIdentity::new(WorkspaceId::generate().unwrap(), "acquisition", "revision")
                .unwrap(),
            Duration::from_millis(1),
        )
        .unwrap()
    }

    fn composite_intent(owner: AttemptOwnership) -> CancellationIntent {
        let base = intent(owner, "windows-composite");
        let job = BoundaryIdentity::new(
            BoundaryKind::WindowsJobObject,
            "Local\\kit-job-composite",
            "b".repeat(64),
            "v1:42:1337:100:200:3",
        )
        .unwrap();
        let runtime = BoundaryIdentity::new(
            BoundaryKind::WindowsContainerOrVm,
            "hyperv-composite",
            "c".repeat(64),
            "v1|hyper_v|plan|helper|runtime|generation|root|42|1337",
        )
        .unwrap();
        let boundary =
            PersistedBoundary::windows_composite(base.boundary.ownership, job, runtime).unwrap();
        let intent = CancellationIntent::new(
            base.request_id,
            base.owner,
            base.process,
            boundary,
            base.workspace,
            base.grace_period,
        )
        .unwrap();
        assert!(
            !intent
                .policy
                .resolve_unknown_with_matching_zero_survivor_inspection
        );
        intent
    }

    #[test]
    fn attempt_agent_then_grader_uses_distinct_fenced_boundary_claims() {
        let (path, owner) = setup();
        let coordinator = SqliteCancellationCoordinator::new(&path);
        let agent = intent(owner, "agent");
        let grader = intent(owner, "grader");

        coordinator.register_claim(&agent).unwrap();
        coordinator
            .confirm_quiescence(owner, agent.request_id, true)
            .unwrap();
        assert_eq!(
            coordinator.boundary_state(owner).unwrap(),
            DurableBoundaryState::BetweenPhases
        );
        assert!(!SqliteCancellationCoordinator::recovery_allows_new_attempt(&path, owner).unwrap());

        coordinator.register_claim(&grader).unwrap();
        assert!(!SqliteCancellationCoordinator::recovery_allows_new_attempt(&path, owner).unwrap());
        assert!(matches!(
            coordinator.confirm_quiescence(owner, agent.request_id, false),
            Err(CancellationError::Store(
                CancellationStoreError::PhaseConflict
            ))
        ));
        assert_eq!(
            coordinator.boundary_state(owner).unwrap(),
            DurableBoundaryState::Active
        );

        let mut store = SqliteCancellationStore::open(&path).unwrap();
        store.request(agent.clone()).unwrap();
        store.request(grader.clone()).unwrap();
        assert_eq!(store.load(agent.request_id, owner).unwrap().intent, agent);
        assert_eq!(store.load(grader.request_id, owner).unwrap().intent, grader);

        coordinator
            .confirm_quiescence(owner, grader.request_id, false)
            .unwrap();
        assert_eq!(
            coordinator.boundary_state(owner).unwrap(),
            DurableBoundaryState::Quiescent
        );
        let active_claims: i64 = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM executor_execution_claims",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(active_claims, 0);

        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn quiescence_confirmation_survives_coordinator_restart() {
        let (path, owner) = setup();
        let coordinator = SqliteCancellationCoordinator::new(&path);
        let request = intent(owner, "restart-confirmation");
        coordinator.register_claim(&request).unwrap();
        let confirmation = coordinator
            .confirm_quiescence(owner, request.request_id, false)
            .unwrap();
        drop(coordinator);

        let restarted = SqliteCancellationCoordinator::new(&path);
        restarted.ensure_schema().unwrap();
        let persisted: (String, i64, String, String) = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT request_id, fence, phase, commit_digest
                 FROM executor_quiescence_confirmations WHERE request_id=?1",
                [request.request_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(persisted.0, confirmation.request_id);
        assert_eq!(persisted.1, confirmation.fence as i64);
        assert_eq!(persisted.2, confirmation.phase);
        assert_eq!(persisted.3, confirmation.commit_digest);
        assert_eq!(
            restarted.boundary_state(owner).unwrap(),
            DurableBoundaryState::Quiescent
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn stale_agent_fence_cannot_claim_the_reserved_grader_phase() {
        let (path, owner) = setup();
        let coordinator = SqliteCancellationCoordinator::new(&path);
        let agent = intent(owner, "agent");
        coordinator.register_claim(&agent).unwrap();
        coordinator
            .confirm_quiescence(owner, agent.request_id, true)
            .unwrap();
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE attempt_driver_claims SET fence=fence+1 WHERE attempt_id=?1",
                [owner.attempt_id.to_string()],
            )
            .unwrap();

        assert!(matches!(
            coordinator.register_claim(&intent(owner, "stale-grader")),
            Err(CancellationError::Store(CancellationStoreError::StaleOwner))
        ));
        assert_eq!(
            coordinator.boundary_state(owner).unwrap(),
            DurableBoundaryState::BetweenPhases
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn startup_terminalizes_between_phases_without_boundary_control() {
        let (path, owner) = setup();
        let coordinator = SqliteCancellationCoordinator::new(&path);
        let agent = intent(owner, "agent");
        coordinator.register_claim(&agent).unwrap();
        coordinator
            .confirm_quiescence(owner, agent.request_id, true)
            .unwrap();

        let reconciled = coordinator
            .reconcile_startup_with::<RecoveryControl>(|_| {
                panic!("a between-phase reservation has no live boundary")
            })
            .unwrap();

        assert_eq!(reconciled, 1);
        assert_eq!(
            coordinator.boundary_state(owner).unwrap(),
            DurableBoundaryState::Quiescent
        );
        assert!(SqliteCancellationCoordinator::recovery_allows_new_attempt(&path, owner).unwrap());
        let connection = Connection::open(&path).unwrap();
        let (driver_quiescent, active_claims): (bool, i64) = connection
            .query_row(
                "SELECT claim.quiescent,
                    (SELECT COUNT(*) FROM executor_execution_claims)
                 FROM attempt_driver_claims AS claim WHERE attempt_id=?1",
                [owner.attempt_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(driver_quiescent);
        assert_eq!(active_claims, 0);
        drop(connection);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn startup_crashes_around_trial_phase_claims_do_not_wedge_or_target_the_wrong_phase() {
        for crash in [
            "agent-outcome",
            "agent-reserved",
            "before-grader",
            "after-grader",
        ] {
            let (path, owner) = setup();
            let coordinator = SqliteCancellationCoordinator::new(&path);
            let agent = intent(owner, "agent");
            let grader = intent(owner, "grader");
            coordinator.register_claim(&agent).unwrap();

            if crash != "agent-outcome" {
                coordinator
                    .confirm_quiescence(owner, agent.request_id, true)
                    .unwrap();
            }
            if crash == "after-grader" {
                coordinator.register_claim(&grader).unwrap();
            }

            let mut controlled = Vec::new();
            assert_eq!(
                coordinator
                    .reconcile_startup_with(|intent| {
                        controlled.push(intent.request_id);
                        Ok(RecoveryControl {
                            identity: intent.boundary.identity.clone(),
                            fail: None,
                        })
                    })
                    .unwrap(),
                1
            );
            assert_eq!(
                controlled,
                match crash {
                    "agent-outcome" => vec![agent.request_id],
                    "after-grader" => vec![grader.request_id],
                    _ => Vec::new(),
                }
            );
            assert_eq!(
                coordinator.boundary_state(owner).unwrap(),
                DurableBoundaryState::Quiescent
            );
            assert!(
                SqliteCancellationCoordinator::recovery_allows_new_attempt(&path, owner).unwrap()
            );
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn existing_boundary_schema_is_migrated_for_between_phase_state() {
        let (path, _) = setup();
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE executor_attempt_boundaries (
                   attempt_id TEXT PRIMARY KEY, principal_id TEXT NOT NULL,
                   fence INTEGER NOT NULL, state TEXT NOT NULL, request_id TEXT,
                   CHECK (state IN ('no_process', 'active', 'outcome_unknown', 'quiescent'))
                 );",
            )
            .unwrap();

        SqliteCancellationCoordinator::new(&path)
            .ensure_schema()
            .unwrap();

        let schema: String = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT sql FROM sqlite_master
                 WHERE type='table' AND name='executor_attempt_boundaries'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(schema.contains("between_phases"));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn authority_rejects_expired_and_quiescent_driver_claims() {
        for column in ["expires_at_unix_micros=0", "quiescent=1"] {
            let (path, owner) = setup();
            let connection = Connection::open(&path).unwrap();
            connection
                .execute(&format!("UPDATE attempt_driver_claims SET {column}"), [])
                .unwrap();

            assert!(matches!(
                SqliteCancellationCoordinator::new(&path).register_no_process(owner),
                Err(CancellationError::Store(
                    CancellationStoreError::Unauthorized
                ))
            ));
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn startup_reconciliation_requires_confirmed_kill_reap_and_inspection() {
        for failure in [None, Some("kill"), Some("reap"), Some("inspect")] {
            let (path, owner) = setup();
            let coordinator = SqliteCancellationCoordinator::new(&path);
            let intent = intent(owner, "startup");
            coordinator.register_claim(&intent).unwrap();

            let result = coordinator.reconcile_startup_with(|intent| {
                Ok(RecoveryControl {
                    identity: intent.boundary.identity.clone(),
                    fail: failure,
                })
            });
            let connection = Connection::open(&path).unwrap();
            let state: String = connection
                .query_row(
                    "SELECT state FROM executor_attempt_boundaries WHERE attempt_id=?1",
                    [owner.attempt_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            if failure.is_none() {
                assert_eq!(result.unwrap(), 1);
                assert_eq!(state, "quiescent");
            } else {
                assert!(matches!(
                    result,
                    Err(CancellationError::Store(
                        CancellationStoreError::Unavailable(_)
                    ))
                ));
                assert_eq!(state, "outcome_unknown");
                let recorded: i64 = connection
                    .query_row(
                        "SELECT COUNT(*) FROM executor_recovery_failures WHERE attempt_id=?1",
                        [owner.attempt_id.to_string()],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(recorded, 1);
                drop(connection);
                assert_eq!(
                    coordinator
                        .reconcile_startup_with(|intent| {
                            Ok(RecoveryControl {
                                identity: intent.boundary.identity.clone(),
                                fail: None,
                            })
                        })
                        .unwrap(),
                    1
                );
                assert_eq!(
                    coordinator.boundary_state(owner).unwrap(),
                    DurableBoundaryState::Quiescent
                );
            }
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn sqlite_claim_atomically_round_trips_both_windows_layers() {
        let (path, owner) = setup();
        let coordinator = SqliteCancellationCoordinator::new(&path);
        let intent = composite_intent(owner);
        coordinator.register_claim(&intent).unwrap();

        let loaded = load_intent_for_attempt(
            &Connection::open(&path).unwrap(),
            "executor_execution_claims",
            owner.attempt_id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(loaded, intent);
        assert!(loaded.boundary.windows_layers().unwrap().is_some());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn partial_composite_recovery_stays_outcome_unknown_after_controlling_both_layers() {
        let (path, owner) = setup();
        let coordinator = SqliteCancellationCoordinator::new(&path);
        let intent = composite_intent(owner);
        let calls = Arc::new(Mutex::new(Vec::new()));
        coordinator.register_claim(&intent).unwrap();

        assert!(
            coordinator
                .reconcile_startup_with(|intent| Ok(CompositeRecoveryControl {
                    identity: intent.boundary.identity.clone(),
                    calls: calls.clone(),
                    fail_runtime_kill: true,
                }))
                .is_err()
        );
        assert_eq!(
            *calls.lock().unwrap(),
            [
                "job-kill",
                "runtime-kill",
                "job-reap",
                "runtime-reap",
                "job-inspect",
                "runtime-inspect",
            ]
        );
        assert_eq!(
            coordinator.boundary_state(owner).unwrap(),
            DurableBoundaryState::OutcomeUnknown
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn composite_cancellation_cannot_repair_a_partial_kill_with_only_a_later_scan() {
        let (path, owner) = setup();
        let coordinator = SqliteCancellationCoordinator::new(&path);
        let intent = composite_intent(owner);
        coordinator.register_claim(&intent).unwrap();
        let mut store = SqliteCancellationStore::open(&path).unwrap();
        let mut control = RecoveryControl {
            identity: intent.boundary.identity.clone(),
            fail: Some("kill"),
        };
        let record = request_cancellation(
            &mut store,
            &mut control,
            intent.clone(),
            Duration::from_millis(10),
        )
        .unwrap();
        assert_eq!(record.phase, CancellationPhase::OutcomeUnknown);

        control.fail = None;
        let rescanned = reconcile_cancellation(
            &mut store,
            &mut control,
            intent.request_id,
            owner,
            Duration::from_millis(10),
        )
        .unwrap();
        assert_eq!(rescanned.phase, CancellationPhase::OutcomeUnknown);
        assert!(!rescanned.workspace_reassignable());
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}
