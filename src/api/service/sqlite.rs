use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use agentkit_core::{Item, ItemKind};

use crate::agent::driver::{
    restart::{
        EFFECT_JOURNAL_EVENT, EffectJournal, EffectJournalAppend, EffectJournalRecord, LoopRecord,
        RecoveryState, RestartProjection,
    },
    waiting::{WaitingKind, WaitingResolution, WaitingState},
};
use crate::api::auth::contract::{AuthenticatedPrincipal, GrantSnapshot, ResourceScope};
use crate::capabilities::kernel::identity::{Digest, DigestAlgorithm};
use crate::domain::commands::ExpectedVersion;
use crate::domain::crypto::sha256;
use crate::domain::deletion::{
    ArchiveStatus, DeletionActor, DeletionAuditEntry, DeletionError, DeletionJob, DeletionJobId,
    DeletionJobState, EffectiveRetention, PublicDeletionBlocker,
};
use crate::domain::events::{
    AttemptState, EntityId, EventType, RunState, SchemaVersion, UtcDateTime,
};
use crate::domain::ids::{
    AttemptId, CommandId, EventId, McpCallbackId, PrincipalId, ProjectId, RunId, ThreadId,
};
use crate::domain::lifecycle::{AttemptOwnership, AttemptTransition, FencingToken, RunTransition};
use crate::domain::projections::{DeletionIntent, DomainReducer, PersistedCommand};
use crate::domain::retention::{
    ArtifactReference, ArtifactReferenceId, BackupGeneration, BackupGenerationId, DeletionBlocker,
    EarliestPhysicalDeletion, Expiration, LegalHold, LegalHoldId, LegalHoldScope, RetainedObject,
    RetentionIntent, RetentionObjectId, RetentionPeriod, StoreTimestamp,
    evaluate_physical_deletion_at,
};
use crate::executor::cancel::SqliteCancellationCoordinator;
use crate::store::artifacts::{ArtifactDigest, ArtifactStore};
use crate::store::backup::BackupGeneration as StoredBackupGeneration;
use crate::store::sqlite::append::{
    AppendCommand, AppendOutcome, CrashPoint, ExpectedStreamVersion, NewEvent, SqliteStore,
    StoreError,
};
use crate::store::sqlite::idempotency::{CanonicalRequestDigest, IdempotencyScope};
use crate::store::sqlite::idempotency::{IdempotencyKey, IdempotencyStatus};
use crate::store::sqlite::projection::{ProjectionStore, StoreTime};

use super::{
    AttemptDriverClaim, Command, CommandReceipt, CursorStatusProjection, EventCursor, EventPage,
    EventProjection, Query, QueryProjection, Resource, RetentionPolicy, RunCompletionRecord,
    RunFailureCode, RunFailureProjection, RunProgressRecord, RunPromptProjection,
    RunSemanticEnvelope, RunTranscriptProjection, ServiceError, ServiceStore, StatusProjection,
    WorkerRun, WorkerStore, WriteRequest,
};

pub struct SqliteServiceStore {
    append: SqliteStore,
    projections: ProjectionStore,
    authority: crate::runtime::daemon::ControlPlaneAuthority,
    database: PathBuf,
}

#[derive(Clone, Debug)]
pub struct DeletionEffect {
    pub job_id: DeletionJobId,
    pub object_id: RetentionObjectId,
    pub artifact_digest: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeletionWorkerReport {
    pub evaluated: usize,
    pub completed: usize,
    pub waiting: usize,
    pub failed: usize,
    pub stale: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct EventCompactionReport {
    pub examined: usize,
    pub erased: usize,
    pub blocked: usize,
}

struct ClaimedDeletion {
    effect: DeletionEffect,
    version: u64,
    fence: u64,
}

#[derive(Clone, Copy)]
enum EventScope {
    Thread(ThreadId),
    Run(RunId),
}

impl SqliteServiceStore {
    pub(crate) fn open(
        path: impl AsRef<Path>,
        authority: &crate::runtime::daemon::ControlPlaneAuthority,
    ) -> Result<Self, ServiceError> {
        let path = path.as_ref();
        let append = SqliteStore::open(path, authority).map_err(store_error)?;
        SqliteCancellationCoordinator::new(path)
            .ensure_schema()
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        let mut projections =
            ProjectionStore::open(path).map_err(|error| ServiceError::Store(error.to_string()))?;
        projections
            .ensure_event_index()
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        crate::store::sqlite::mcp_callback::McpCallbackStore::open(path)
            .map_err(mcp_callback_error)?;
        Ok(Self {
            append,
            projections,
            authority: authority.clone(),
            database: path.to_owned(),
        })
    }

    #[cfg(debug_assertions)]
    pub fn append_store(&self) -> &SqliteStore {
        &self.append
    }

    pub fn projection_digest(&mut self) -> Result<[u8; 32], ServiceError> {
        self.projections
            .update_domain()
            .map(|(_, snapshot)| snapshot.digest)
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    pub fn refresh_backup_policy_inventory(&mut self) -> Result<(), ServiceError> {
        self.state().map(drop)
    }

    pub fn reconcile_deletion_jobs(&mut self) -> Result<usize, ServiceError> {
        self.state()?;
        self.projections
            .with_store_time(|transaction, now| {
                let mut statement = transaction.prepare(
                    "SELECT job_id FROM deletion_jobs WHERE state = 'physically_deleting'",
                )?;
                let ids = statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(statement);
                for id in &ids {
                    transaction.execute(
                        "UPDATE deletion_jobs SET state = 'failed', version = version + 1,
                           failure = 'physical deletion outcome unknown after worker interruption',
                           effect_unknown = 1, worker_id = NULL, lease_until_unix_micros = NULL
                         WHERE job_id = ?1 AND state = 'physically_deleting'",
                        [id],
                    )?;
                    append_audit(transaction, id, "failed", now.unix_micros())?;
                }
                Ok(ids.len())
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    pub fn run_deletion_jobs(
        &mut self,
        worker_id: &str,
        limit: usize,
        mut physically_delete: impl FnMut(&DeletionEffect) -> Result<(), String>,
    ) -> Result<DeletionWorkerReport, ServiceError> {
        if worker_id.is_empty() {
            return Err(ServiceError::Invalid(
                "deletion worker id is empty".to_owned(),
            ));
        }
        self.state()?;
        let mut report = DeletionWorkerReport::default();
        for _ in 0..limit.min(64) {
            let Some(claim) = self.claim_deletion_job(worker_id)? else {
                break;
            };
            report.evaluated += 1;
            let Some(claim) = claim else {
                report.waiting += 1;
                continue;
            };
            match self.finish_deletion_job(worker_id, &claim, &mut physically_delete)? {
                FinishOutcome::Completed => {
                    self.projections
                        .purge_erased_bytes()
                        .map_err(|error| ServiceError::Store(error.to_string()))?;
                    report.completed += 1;
                }
                FinishOutcome::Waiting => report.waiting += 1,
                FinishOutcome::Failed => report.failed += 1,
                FinishOutcome::Stale => report.stale += 1,
            }
        }
        Ok(report)
    }

    pub fn compact_retained_events(
        &mut self,
        now_unix_micros: i64,
        active_cursors: &BTreeMap<ProjectId, EventCursor>,
    ) -> Result<EventCompactionReport, ServiceError> {
        let (state, snapshot) = self
            .projections
            .update_domain()
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        let canonical = state.canonical_bytes()?;
        let policies = state
            .projects
            .values()
            .map(|project| {
                (
                    project.id,
                    (
                        project.principal_id,
                        project.retention.unwrap_or(RetentionPolicy::FOREVER),
                        serde_json::to_vec(project).map_err(|error| {
                            crate::store::sqlite::projection::ProjectionError::Reducer(
                                error.to_string(),
                            )
                        }),
                    ),
                )
            })
            .map(|(id, (principal, policy, snapshot))| {
                snapshot.map(|snapshot| (id, (principal, policy, snapshot)))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        let report = self
            .projections
            .with_store_time(|transaction, _| {
                let mut statement = transaction.prepare(
                    "SELECT event.event_id, event.commit_position, event.stream, index_row.project_id,
                            index_row.event_class, index_row.stored_at_unix_micros,
                            event.artifacts
                     FROM event_projection_index AS index_row
                     JOIN events AS event ON event.commit_position = index_row.commit_position
                     WHERE index_row.erased = 0 ORDER BY event.commit_position",
                )?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, i64>(5)?,
                            row.get::<_, Vec<u8>>(6)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(statement);

                let mut report = EventCompactionReport::default();
                let mut blocked_projects = BTreeSet::new();
                let mut gaps = BTreeMap::<(ProjectId, String), u64>::new();
                for (event_id, position, stream, project, class, stored_at, artifacts) in rows {
                    report.examined += 1;
                    let project_id = ProjectId::parse(&project).map_err(|error| {
                        crate::store::sqlite::projection::ProjectionError::Reducer(
                            error.to_string(),
                        )
                    })?;
                    let Some((principal_id, policy, _)) = policies.get(&project_id) else {
                        blocked_projects.insert(project_id);
                        report.blocked += 1;
                        continue;
                    };
                    let period = match class.as_str() {
                        "event" => policy.event,
                        "terminal" => policy.terminal,
                        "experiment" => policy.experiment,
                        _ => RetentionPeriod::Forever,
                    };
                    let expired = match period {
                        RetentionPeriod::ForMicros(micros) => i64::try_from(micros)
                            .ok()
                            .and_then(|micros| stored_at.checked_add(micros))
                            .is_some_and(|expiry| expiry <= now_unix_micros),
                        RetentionPeriod::Forever => false,
                    };
                    let cursor_safe = active_cursors
                        .get(&project_id)
                        .is_none_or(|cursor| {
                            u64::try_from(position)
                                .is_ok_and(|position| position <= cursor.position())
                        });
                    let event_key = format!("event:{event_id}");
                    let held: bool = transaction.query_row(
                        "SELECT EXISTS(
                           SELECT 1 FROM deletion_legal_holds
                           WHERE placed_at_unix_micros <= ?1
                             AND (released_at_unix_micros IS NULL OR released_at_unix_micros > ?1)
                             AND ((scope_kind = 'principal' AND scope_id = ?2)
                               OR (scope_kind = 'project' AND scope_id = ?3)
                               OR (scope_kind = 'object' AND scope_id = ?4))
                         )",
                        params![
                            now_unix_micros,
                            principal_id.to_string(),
                            project,
                            event_key
                        ],
                        |row| row.get(0),
                    )?;
                    let backed_up: bool = transaction.query_row(
                        "SELECT EXISTS(
                           SELECT 1 FROM deletion_backup_contents AS content
                           JOIN deletion_backup_generations AS generation
                             ON generation.generation_id = content.generation_id
                           WHERE content.object_key = ?1
                             AND (generation.expires_at_unix_micros IS NULL
                               OR generation.expires_at_unix_micros > ?2)
                         )",
                        params![event_key, now_unix_micros],
                        |row| row.get(0),
                    )?;
                    let terminal_callback = class == "terminal" && McpCallbackId::parse(&stream).is_ok();
                    if blocked_projects.contains(&project_id)
                        || !expired
                        || !cursor_safe
                        || held
                        || backed_up
                        || (artifacts != b"[]" && !terminal_callback)
                    {
                        blocked_projects.insert(project_id);
                        report.blocked += 1;
                        continue;
                    }
                    transaction.execute(
                        "UPDATE events SET event_type = 'retention.erased',
                           payload = X'7B22657261736564223A747275652C22736368656D615F76657273696F6E223A317D',
                           artifacts = X'5B5D' WHERE commit_position = ?1",
                        [position],
                    )?;
                    transaction.execute(
                        "UPDATE event_projection_index
                         SET erased = 1, thread_id = NULL, run_id = NULL
                         WHERE commit_position = ?1",
                        [position],
                    )?;
                    transaction.execute(
                        "INSERT INTO deletion_tombstones
                         (target_sha256, object_kind, completed_at_unix_micros,
                          erased_event_count, outcome)
                         VALUES (?1, 'event', ?2, 1, 'erased')
                         ON CONFLICT(target_sha256) DO NOTHING",
                        params![sha256(event_key.as_bytes()).as_slice(), now_unix_micros],
                    )?;
                    if terminal_callback {
                        crate::store::sqlite::mcp_callback::scrub_callback(
                            transaction,
                            &stream,
                            now_unix_micros,
                        )
                        .map_err(|error| {
                            crate::store::sqlite::projection::ProjectionError::Reducer(
                                error.to_string(),
                            )
                        })?;
                    }
                    gaps.insert((project_id, class), position as u64);
                    report.erased += 1;
                }
                if report.erased != 0 {
                    let digest = sha256(&canonical);
                    transaction.execute(
                        "INSERT INTO projection_rebuild_baseline
                         (name, canonical_bytes, digest, checkpoint) VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(name) DO UPDATE SET canonical_bytes = excluded.canonical_bytes,
                           digest = excluded.digest, checkpoint = excluded.checkpoint",
                        params![
                            DomainReducer::NAME,
                            canonical,
                            digest.as_slice(),
                            snapshot.checkpoint
                        ],
                    )?;
                    for ((project_id, class), compacted_through) in gaps {
                        let first_available: i64 = transaction.query_row(
                            "SELECT coalesce(min(commit_position), ?3)
                             FROM event_projection_index
                             WHERE project_id = ?1 AND event_class = ?2 AND erased = 0",
                            params![project_id.to_string(), class, snapshot.checkpoint + 1],
                            |row| row.get(0),
                        )?;
                        let project_snapshot = &policies[&project_id].2;
                        transaction.execute(
                            "INSERT INTO retention_event_gaps
                             (project_id, event_class, first_available_position,
                              compacted_through, cursor_expired_at_unix_micros,
                              cursor_expiry_snapshot)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                             ON CONFLICT(project_id, event_class) DO UPDATE SET
                               first_available_position = max(first_available_position,
                                                              excluded.first_available_position),
                               compacted_through = max(compacted_through,
                                                       excluded.compacted_through),
                               cursor_expired_at_unix_micros = excluded.cursor_expired_at_unix_micros,
                               cursor_expiry_snapshot = excluded.cursor_expiry_snapshot",
                            params![
                                project_id.to_string(),
                                class,
                                first_available,
                                compacted_through,
                                now_unix_micros,
                                project_snapshot
                            ],
                        )?;
                    }
                }
                Ok(report)
            })
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        if report.erased != 0 {
            self.projections
                .purge_erased_bytes()
                .map_err(|error| ServiceError::Store(error.to_string()))?;
        }
        Ok(report)
    }
}

impl WorkerStore for SqliteServiceStore {
    fn claim_queued_run(
        &mut self,
        lease_duration: std::time::Duration,
    ) -> Result<Option<WorkerRun>, ServiceError> {
        let state = self.state()?;
        let Some(run) = state
            .runs
            .values()
            .filter(|run| run.state == RunState::Queued && run.owner.is_none())
            .min_by_key(|run| run.id)
            .cloned()
        else {
            return Ok(None);
        };
        let project_id = state
            .project_for_run(run.id)
            .ok_or(ServiceError::NotFound)?;
        let principal_id = state
            .projects
            .get(&project_id)
            .ok_or(ServiceError::NotFound)?
            .principal_id;
        drop(state);

        let attempt_id =
            AttemptId::generate().map_err(|error| ServiceError::Store(error.to_string()))?;
        let Some(claim) =
            self.acquire_driver_claim(run.id, attempt_id, principal_id, lease_duration)?
        else {
            return Ok(None);
        };
        let owner = claim.owner();
        self.worker_execute(
            principal_id,
            Command::StartAttempt {
                schema_version: SchemaVersion::CURRENT,
                attempt_id,
                run_id: run.id,
                owner,
                expected_version: run.version,
            },
        )?;
        self.load_worker_run(run.id).map(Some)
    }

    fn recoverable_runs(&mut self, limit: usize) -> Result<Vec<WorkerRun>, ServiceError> {
        let state = self.state()?;
        let committed = self.append.events().map_err(store_error)?;
        let mut ids = Vec::new();
        for attempt in state
            .attempts
            .values()
            .filter(|attempt| !attempt.state.is_terminal())
        {
            let Some(run) = state.runs.get(&attempt.run_id) else {
                continue;
            };
            if run.state.is_terminal()
                || !SqliteCancellationCoordinator::recovery_allows_new_attempt(
                    &self.database,
                    attempt.owner,
                )
                .map_err(|error| ServiceError::Store(error.to_string()))?
            {
                continue;
            }
            if !run.state.is_waiting()
                || !matches!(
                    RestartProjection::reconstruct(attempt, &committed),
                    Ok(RecoveryState::Waiting(_))
                )
            {
                ids.push(attempt.run_id);
            }
        }
        ids.sort();
        ids.dedup();
        let mut available = Vec::with_capacity(ids.len());
        for run_id in ids {
            if self.driver_claim_available(run_id)? {
                available.push(run_id);
            }
        }
        let mut ids = available;
        ids.truncate(limit.min(1_024));
        drop(state);
        ids.into_iter().map(|id| self.load_worker_run(id)).collect()
    }

    fn claim_recoverable_run(
        &mut self,
        run_id: RunId,
        lease_duration: std::time::Duration,
    ) -> Result<Option<WorkerRun>, ServiceError> {
        let state = self.state()?;
        let run = state
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(ServiceError::NotFound)?;
        let old_owner = run
            .owner
            .ok_or_else(|| ServiceError::Conflict("recoverable run has no owner".to_owned()))?;
        let old_attempt = state
            .attempts
            .get(&old_owner.attempt_id)
            .cloned()
            .ok_or(ServiceError::NotFound)?;
        if !SqliteCancellationCoordinator::recovery_allows_new_attempt(&self.database, old_owner)
            .map_err(|error| ServiceError::Store(error.to_string()))?
        {
            return Ok(None);
        }
        let committed = self.append.events().map_err(store_error)?;
        let recovery = RestartProjection::reconstruct(&old_attempt, &committed)
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        let old_records = committed
            .iter()
            .filter(|event| {
                event.event.attempt_id == Some(old_owner.attempt_id)
                    && event.event.event_type.as_str() == EFFECT_JOURNAL_EVENT
            })
            .filter_map(|event| {
                serde_json::from_slice::<serde_json::Value>(&event.event.payload)
                    .ok()
                    .and_then(|value| value.get("record").cloned())
                    .and_then(|value| serde_json::from_value::<EffectJournalRecord>(value).ok())
            })
            .collect::<Vec<_>>();
        drop(state);

        let attempt_id =
            AttemptId::generate().map_err(|error| ServiceError::Store(error.to_string()))?;
        let Some(claim) =
            self.acquire_driver_claim(run_id, attempt_id, old_owner.principal_id, lease_duration)?
        else {
            return Ok(None);
        };

        let mut job = self.load_worker_run(run_id)?;
        if old_attempt.state != AttemptState::Quiescing {
            job = self.transition_worker_attempt(old_attempt.id, AttemptState::Quiescing)?;
        }
        if job.attempt.id == old_attempt.id && job.attempt.state != AttemptState::Interrupted {
            job = self.transition_worker_attempt(old_attempt.id, AttemptState::Interrupted)?;
        }
        if job.run.state.is_waiting() {
            job = self.transition_worker_run(run_id, RunState::Running)?;
        }
        if job.run.state != RunState::Interrupted {
            job = self.transition_worker_run(run_id, RunState::Interrupted)?;
        }
        let transition = RunTransition::new(RunState::Interrupted, RunState::Queued)
            .map_err(|error| ServiceError::Conflict(error.to_string()))?;
        self.worker_execute(
            old_owner.principal_id,
            Command::TransitionRun {
                schema_version: SchemaVersion::CURRENT,
                run_id,
                transition,
                expected_version: job.run.version,
                expected_owner: Some(old_owner),
                replacement_owner: Some(claim.owner()),
            },
        )?;
        let job = self.load_worker_run(run_id)?;
        let transfer = recovery_records(recovery, old_records)?;
        for (index, record) in transfer.into_iter().enumerate() {
            let record = rebind_record(record, claim);
            let bytes = serde_json::to_vec(&record)
                .map_err(|error| ServiceError::Store(error.to_string()))?;
            self.append
                .append_effect(EffectJournalAppend {
                    owner: claim.owner(),
                    claim: Some(claim),
                    idempotency_key: IdempotencyKey::parse(&format!(
                        "failover-{}-{index}-{}",
                        claim.lease_version,
                        blake3::hash(&bytes).to_hex()
                    ))
                    .map_err(|error| ServiceError::Store(error.to_string()))?,
                    command_id: CommandId::generate()
                        .map_err(|error| ServiceError::Store(error.to_string()))?,
                    event_id: EventId::generate()
                        .map_err(|error| ServiceError::Store(error.to_string()))?,
                    occurred_at: job.occurred_at.clone(),
                    trace_id: crate::domain::events::TraceId::parse("run-failover")
                        .expect("static trace id is valid"),
                    artifacts: Vec::new(),
                    record,
                })
                .map_err(store_error)?;
        }
        self.load_worker_run(run_id).map(Some)
    }

    fn worker_run(&mut self, run_id: RunId) -> Result<WorkerRun, ServiceError> {
        self.load_worker_run(run_id)
    }

    fn renew_worker_claim(
        &mut self,
        claim: AttemptDriverClaim,
        lease_duration: std::time::Duration,
    ) -> Result<AttemptDriverClaim, ServiceError> {
        let duration = lease_micros(lease_duration)?;
        let mut connection = self.driver_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_service_error)?;
        let now = driver_now(&transaction)?;
        let expires = now
            .checked_add(duration)
            .ok_or_else(|| ServiceError::Invalid("driver lease duration is invalid".to_owned()))?;
        let changed = transaction
            .execute(
                "UPDATE attempt_driver_claims SET expires_at_unix_micros = ?6
                 WHERE run_id = ?1 AND attempt_id = ?2 AND principal_id = ?3
                   AND fence = ?4 AND lease_version = ?5
                   AND expires_at_unix_micros > ?7 AND quiescent = 0",
                params![
                    claim.run_id.to_string(),
                    claim.attempt_id.to_string(),
                    claim.principal_id.to_string(),
                    i64::try_from(claim.fence.get())
                        .map_err(|_| ServiceError::Conflict("driver fence is stale".to_owned()))?,
                    i64::try_from(claim.lease_version).map_err(|_| ServiceError::Conflict(
                        "driver lease version is stale".to_owned()
                    ))?,
                    expires,
                    now,
                ],
            )
            .map_err(sqlite_service_error)?;
        if changed != 1 {
            return Err(ServiceError::Conflict(
                "attempt driver claim is stale".to_owned(),
            ));
        }
        transaction.commit().map_err(sqlite_service_error)?;
        Ok(AttemptDriverClaim {
            expires_at_unix_micros: expires,
            ..claim
        })
    }

    fn ensure_worker_wait(
        &mut self,
        run_id: RunId,
        waiting: &WaitingState,
    ) -> Result<WorkerRun, ServiceError> {
        let state = self.state()?;
        let run = state
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(ServiceError::NotFound)?;
        if run.owner.map(|owner| owner.principal_id) != Some(waiting.principal_id) {
            return Err(ServiceError::Conflict(
                "waiting state owner does not match run owner".to_owned(),
            ));
        }
        let request = match waiting.kind {
            WaitingKind::Approval { approval_id, .. }
                if !state.approvals.contains_key(&approval_id) =>
            {
                Some(Command::RequestApproval {
                    schema_version: SchemaVersion::CURRENT,
                    approval_id,
                    run_id,
                })
            }
            WaitingKind::Auth { run_id: found, .. }
                if found == run_id && !state.auth_requests.contains_key(&run_id) =>
            {
                Some(Command::RequestAuth {
                    schema_version: SchemaVersion::CURRENT,
                    run_id,
                    expected_version: run.version,
                })
            }
            WaitingKind::Auth { run_id: found, .. } if found != run_id => {
                return Err(ServiceError::Conflict(
                    "auth waiting state does not match run".to_owned(),
                ));
            }
            _ => None,
        };
        drop(state);
        if let Some(request) = request {
            self.worker_execute(waiting.principal_id, request)?;
        }
        let target = match waiting.kind {
            WaitingKind::Input => RunState::WaitingForInput,
            WaitingKind::Approval { .. } => RunState::WaitingForApproval,
            WaitingKind::Auth { .. } => RunState::WaitingForAuth,
        };
        self.transition_worker_run(run_id, target)
    }

    fn transition_worker_run(
        &mut self,
        run_id: RunId,
        target: RunState,
    ) -> Result<WorkerRun, ServiceError> {
        let state = self.state()?;
        let run = state
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(ServiceError::NotFound)?;
        if run.state == target {
            drop(state);
            return self.load_worker_run(run_id);
        }
        let transition = RunTransition::new(run.state, target)
            .map_err(|error| ServiceError::Conflict(error.to_string()))?;
        let owner = run
            .owner
            .ok_or_else(|| ServiceError::Conflict("worker run has no attempt owner".to_owned()))?;
        drop(state);
        self.worker_execute(
            owner.principal_id,
            Command::TransitionRun {
                schema_version: SchemaVersion::CURRENT,
                run_id,
                transition,
                expected_version: run.version,
                expected_owner: Some(owner),
                replacement_owner: None,
            },
        )?;
        self.load_worker_run(run_id)
    }

    fn transition_worker_attempt(
        &mut self,
        attempt_id: AttemptId,
        target: AttemptState,
    ) -> Result<WorkerRun, ServiceError> {
        let state = self.state()?;
        let attempt = state
            .attempts
            .get(&attempt_id)
            .cloned()
            .ok_or(ServiceError::NotFound)?;
        if attempt.state == target {
            drop(state);
            return self.load_worker_run(attempt.run_id);
        }
        let transition = AttemptTransition::new(attempt.state, target)
            .map_err(|error| ServiceError::Conflict(error.to_string()))?;
        drop(state);
        self.worker_execute(
            attempt.owner.principal_id,
            Command::TransitionAttempt {
                schema_version: SchemaVersion::CURRENT,
                attempt_id,
                transition,
                expected_version: attempt.version,
                expected_owner: attempt.owner,
            },
        )?;
        self.load_worker_run(attempt.run_id)
    }

    fn publish_run_prompt(
        &mut self,
        run_id: RunId,
        claim: AttemptDriverClaim,
        prompt: RunPromptProjection,
    ) -> Result<(), ServiceError> {
        self.publish_run_semantic(
            run_id,
            claim,
            "run.prompt",
            &format!("prompt-{run_id}"),
            prompt,
            Vec::new(),
        )
    }

    fn publish_run_progress(
        &mut self,
        run_id: RunId,
        claim: AttemptDriverClaim,
        progress: RunProgressRecord,
    ) -> Result<(), ServiceError> {
        let bytes = serde_json::to_vec(&progress)
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        self.publish_run_semantic(
            run_id,
            claim,
            "run.progress",
            &format!(
                "progress-{}-{}-{}",
                claim.attempt_id,
                progress.sequence,
                blake3::hash(&bytes).to_hex()
            ),
            progress,
            Vec::new(),
        )
    }

    fn publish_run_completion(
        &mut self,
        run_id: RunId,
        claim: AttemptDriverClaim,
        completion: RunCompletionRecord,
    ) -> Result<(), ServiceError> {
        let artifact = completion.output.artifact.clone();
        self.publish_run_semantic(
            run_id,
            claim,
            "run.output",
            &format!("output-{run_id}-{}", artifact.as_str()),
            completion,
            vec![artifact],
        )
    }

    fn fail_worker_run(
        &mut self,
        run_id: RunId,
        claim: AttemptDriverClaim,
        failure: RunFailureProjection,
    ) -> Result<(), ServiceError> {
        self.fail_worker_run_with_hook_impl(run_id, claim, failure, |_| false)
    }

    fn worker_append_store(&self) -> Result<SqliteStore, ServiceError> {
        SqliteStore::open(&self.database, &self.authority).map_err(store_error)
    }
}

impl SqliteServiceStore {
    #[cfg(debug_assertions)]
    pub fn fail_worker_run_with_hook(
        &mut self,
        run_id: RunId,
        claim: AttemptDriverClaim,
        failure: RunFailureProjection,
        crash: impl FnMut(CrashPoint) -> bool,
    ) -> Result<(), ServiceError> {
        self.fail_worker_run_with_hook_impl(run_id, claim, failure, crash)
    }

    fn fail_worker_run_with_hook_impl(
        &mut self,
        run_id: RunId,
        claim: AttemptDriverClaim,
        failure: RunFailureProjection,
        crash: impl FnMut(CrashPoint) -> bool,
    ) -> Result<(), ServiceError> {
        if failure.detail.len() > 512 {
            return Err(ServiceError::Invalid(
                "run failure detail exceeds 512 bytes".to_owned(),
            ));
        }
        let state = self.state()?;
        let run = state.runs.get(&run_id).ok_or(ServiceError::NotFound)?;
        let attempt = state
            .attempts
            .get(&claim.attempt_id)
            .ok_or(ServiceError::NotFound)?;
        if run.owner != Some(claim.owner()) || attempt.owner != claim.owner() {
            return Err(ServiceError::Conflict(
                "run failure publisher is not the current owner".to_owned(),
            ));
        }
        if run.failure.is_some() {
            return if run.state == RunState::Failed && attempt.state == AttemptState::Failed {
                Ok(())
            } else {
                Err(ServiceError::Store(
                    "recoverable run already contains a failure".to_owned(),
                ))
            };
        }
        if claim.run_id != run_id || !self.load_driver_claim(run_id)?.same_lease(claim) {
            return Err(ServiceError::Conflict(
                "attempt driver claim is stale".to_owned(),
            ));
        }
        if run.output.is_some() || run.state == RunState::Completed {
            return Err(ServiceError::Conflict(
                "completed run cannot acquire a failure".to_owned(),
            ));
        }
        if run.state.is_terminal() || attempt.state.is_terminal() {
            return Err(ServiceError::Conflict(
                "terminal run or attempt cannot acquire a failure".to_owned(),
            ));
        }

        let project_id = state
            .project_for_run(run_id)
            .ok_or(ServiceError::NotFound)?;
        let thread_id = run.thread_id;
        let run_version = run.version;
        let attempt_version = attempt.version;
        let attempt_state = attempt.state;
        let run_state = run.state;
        drop(state);

        let occurred_at = self
            .projections
            .store_time()
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        let code = match failure.code {
            RunFailureCode::ProviderUnavailable => "provider-unavailable",
            RunFailureCode::ExecutionFailed => "execution-failed",
        };
        let key = IdempotencyKey::parse(&format!("failure-{}-{code}", claim.attempt_id))
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        let identity = serde_json::to_vec(&(run_id, claim.owner(), failure.code))
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        let command_id =
            CommandId::generate().map_err(|error| ServiceError::Store(error.to_string()))?;
        let trace_id = crate::domain::events::TraceId::parse("run-executor")
            .expect("executor trace id is valid");
        let run_stream = EntityId::Run(run_id);
        let attempt_stream = EntityId::Attempt(claim.attempt_id);
        let committed = self.append.events().map_err(store_error)?;
        let stream_version = |stream| {
            committed
                .iter()
                .filter(|event| event.event.stream == stream)
                .map(|event| event.sequence.get())
                .max()
                .unwrap_or(0)
        };
        let failure_payload = serde_json::to_vec(&RunSemanticEnvelope {
            schema_version: 1,
            run_id,
            project_id,
            attempt: claim.owner(),
            stored_at_unix_micros: occurred_at.unix_micros(),
            record: failure,
        })
        .map_err(|error| ServiceError::Store(error.to_string()))?;
        let mut events = vec![NewEvent {
            id: EventId::generate().map_err(|error| ServiceError::Store(error.to_string()))?,
            stream: run_stream,
            event_type: EventType::parse("run.failure").expect("run failure event type is valid"),
            schema_version: SchemaVersion::CURRENT,
            occurred_at: store_timestamp(&occurred_at)?,
            causation_id: command_id,
            correlation_id: run_stream,
            attempt_id: Some(claim.attempt_id),
            trace_id: trace_id.clone(),
            payload: failure_payload,
            artifacts: b"[]".to_vec(),
        }];
        let mut commands = Vec::with_capacity(4);
        let mut next_attempt_version = attempt_version;
        if attempt_state != AttemptState::Quiescing {
            commands.push(Command::TransitionAttempt {
                schema_version: SchemaVersion::CURRENT,
                attempt_id: claim.attempt_id,
                transition: AttemptTransition::new(attempt_state, AttemptState::Quiescing)
                    .map_err(|error| ServiceError::Conflict(error.to_string()))?,
                expected_version: next_attempt_version,
                expected_owner: claim.owner(),
            });
            next_attempt_version += 1;
        }
        commands.push(Command::TransitionAttempt {
            schema_version: SchemaVersion::CURRENT,
            attempt_id: claim.attempt_id,
            transition: AttemptTransition::new(AttemptState::Quiescing, AttemptState::Failed)
                .expect("quiescing attempt can fail"),
            expected_version: next_attempt_version,
            expected_owner: claim.owner(),
        });
        let mut next_run_version = run_version;
        if run_state != RunState::Cancelling {
            commands.push(Command::TransitionRun {
                schema_version: SchemaVersion::CURRENT,
                run_id,
                transition: RunTransition::new(run_state, RunState::Cancelling)
                    .map_err(|error| ServiceError::Conflict(error.to_string()))?,
                expected_version: next_run_version,
                expected_owner: Some(claim.owner()),
                replacement_owner: None,
            });
            next_run_version += 1;
        }
        commands.push(Command::TransitionRun {
            schema_version: SchemaVersion::CURRENT,
            run_id,
            transition: RunTransition::new(RunState::Cancelling, RunState::Failed)
                .expect("cancelling run can fail"),
            expected_version: next_run_version,
            expected_owner: Some(claim.owner()),
            replacement_owner: None,
        });
        for command in commands {
            let stream = command.target();
            let payload = serde_json::to_vec(&PersistedCommand {
                principal_id: claim.principal_id,
                stored_at_unix_micros: occurred_at.unix_micros(),
                idempotency_key: key.as_str().to_owned(),
                apply_projection: true,
                command: command.clone(),
            })
            .map_err(|error| ServiceError::Store(error.to_string()))?;
            events.push(NewEvent {
                id: EventId::generate().map_err(|error| ServiceError::Store(error.to_string()))?,
                stream,
                event_type: EventType::parse(command.operation())
                    .expect("registered operation names are valid event types"),
                schema_version: SchemaVersion::CURRENT,
                occurred_at: store_timestamp(&occurred_at)?,
                causation_id: command_id,
                correlation_id: run_stream,
                attempt_id: Some(claim.attempt_id),
                trace_id: trace_id.clone(),
                payload,
                artifacts: b"[]".to_vec(),
            });
        }
        let response = self
            .append
            .append_with_hook(
                AppendCommand {
                    idempotency_scope: IdempotencyScope::new(
                        claim.principal_id,
                        "run.failure.terminalize",
                        run_stream,
                    )
                    .map_err(|error| ServiceError::Store(error.to_string()))?,
                    idempotency_key: key,
                    request_digest: CanonicalRequestDigest::new(
                        Digest::of(DigestAlgorithm::Sha256, &identity).as_bytes(),
                    ),
                    claim: None,
                    driver_claim: Some(claim),
                    allow_quiescent_driver_claim: false,
                    expected_versions: vec![
                        ExpectedStreamVersion {
                            stream: run_stream,
                            version: ExpectedVersion::new(stream_version(run_stream)),
                        },
                        ExpectedStreamVersion {
                            stream: attempt_stream,
                            version: ExpectedVersion::new(stream_version(attempt_stream)),
                        },
                    ],
                    events,
                    response: b"failed".to_vec(),
                },
                crash,
            )
            .map_err(store_error)?;
        let response = match response {
            AppendOutcome::Committed(response) | AppendOutcome::Replayed(response) => response,
        };
        self.projections
            .index_committed_events(
                &response.commit_positions,
                crate::domain::projections::IndexedEventScope {
                    project_id,
                    thread_id: Some(thread_id),
                    run_id: Some(run_id),
                },
                occurred_at.unix_micros(),
            )
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    fn publish_run_semantic<T: serde::Serialize>(
        &mut self,
        run_id: RunId,
        claim: AttemptDriverClaim,
        event_type: &str,
        key: &str,
        record: T,
        artifacts: Vec<crate::domain::events::ArtifactRef>,
    ) -> Result<(), ServiceError> {
        if claim.run_id != run_id || !self.load_driver_claim(run_id)?.same_lease(claim) {
            return Err(ServiceError::Conflict(
                "attempt driver claim is stale".to_owned(),
            ));
        }
        let state = self.state()?;
        let run = state.runs.get(&run_id).ok_or(ServiceError::NotFound)?;
        if run.owner != Some(claim.owner()) {
            return Err(ServiceError::Conflict(
                "run semantic publisher is not the current owner".to_owned(),
            ));
        }
        if event_type == "run.output" && (run.failure.is_some() || run.state == RunState::Failed) {
            return Err(ServiceError::Conflict(
                "failed run cannot acquire output".to_owned(),
            ));
        }
        if event_type == "run.failure" && (run.output.is_some() || run.state == RunState::Completed)
        {
            return Err(ServiceError::Conflict(
                "completed run cannot acquire failure".to_owned(),
            ));
        }
        let project_id = state
            .project_for_run(run_id)
            .ok_or(ServiceError::NotFound)?;
        let thread_id = run.thread_id;
        drop(state);
        let occurred_at = self
            .projections
            .store_time()
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        let record_bytes =
            serde_json::to_vec(&record).map_err(|error| ServiceError::Store(error.to_string()))?;
        let envelope = RunSemanticEnvelope {
            schema_version: 1,
            run_id,
            project_id,
            attempt: claim.owner(),
            stored_at_unix_micros: occurred_at.unix_micros(),
            record,
        };
        let payload = serde_json::to_vec(&envelope)
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        let request_digest = CanonicalRequestDigest::new(
            Digest::of(DigestAlgorithm::Sha256, &record_bytes).as_bytes(),
        );
        let idempotency_key =
            IdempotencyKey::parse(key).map_err(|error| ServiceError::Store(error.to_string()))?;
        let stream = EntityId::Run(run_id);
        let version = self
            .append
            .events()
            .map_err(store_error)?
            .into_iter()
            .filter(|event| event.event.stream == stream)
            .map(|event| event.sequence.get())
            .max()
            .unwrap_or(0);
        let command_id =
            CommandId::generate().map_err(|error| ServiceError::Store(error.to_string()))?;
        let outcome = self
            .append
            .append(AppendCommand {
                idempotency_scope: IdempotencyScope::new(
                    claim.principal_id,
                    "run.semantic.publish",
                    stream,
                )
                .map_err(|error| ServiceError::Store(error.to_string()))?,
                idempotency_key,
                request_digest,
                claim: None,
                driver_claim: Some(claim),
                allow_quiescent_driver_claim: false,
                expected_versions: vec![ExpectedStreamVersion {
                    stream,
                    version: ExpectedVersion::new(version),
                }],
                events: vec![NewEvent {
                    id: EventId::generate()
                        .map_err(|error| ServiceError::Store(error.to_string()))?,
                    stream,
                    event_type: EventType::parse(event_type)
                        .map_err(|error| ServiceError::Store(error.to_string()))?,
                    schema_version: SchemaVersion::CURRENT,
                    occurred_at: store_timestamp(&occurred_at)?,
                    causation_id: command_id,
                    correlation_id: stream,
                    attempt_id: Some(claim.attempt_id),
                    trace_id: crate::domain::events::TraceId::parse("run-executor")
                        .expect("executor trace id is valid"),
                    payload,
                    artifacts: serde_json::to_vec(&artifacts)
                        .map_err(|error| ServiceError::Store(error.to_string()))?,
                }],
                response: b"published".to_vec(),
            })
            .map_err(store_error)?;
        let response = match outcome {
            AppendOutcome::Committed(response) | AppendOutcome::Replayed(response) => response,
        };
        self.projections
            .index_committed_events(
                &response.commit_positions,
                crate::domain::projections::IndexedEventScope {
                    project_id,
                    thread_id: Some(thread_id),
                    run_id: Some(run_id),
                },
                occurred_at.unix_micros(),
            )
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    fn driver_claim_available(&self, run_id: RunId) -> Result<bool, ServiceError> {
        let connection = self.driver_connection()?;
        let now: i64 = connection
            .query_row(
                "SELECT CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)",
                [],
                |row| row.get(0),
            )
            .map_err(sqlite_service_error)?;
        connection
            .query_row(
                "SELECT NOT EXISTS(
                     SELECT 1 FROM attempt_driver_claims
                     WHERE run_id = ?1 AND expires_at_unix_micros > ?2 AND quiescent = 0
                 )",
                params![run_id.to_string(), now],
                |row| row.get(0),
            )
            .map_err(sqlite_service_error)
    }

    fn acquire_driver_claim(
        &self,
        run_id: RunId,
        attempt_id: AttemptId,
        principal_id: PrincipalId,
        lease_duration: std::time::Duration,
    ) -> Result<Option<AttemptDriverClaim>, ServiceError> {
        let duration = lease_micros(lease_duration)?;
        let mut connection = self.driver_connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sqlite_service_error)?;
        let now = driver_now(&transaction)?;
        let active: bool = transaction
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM attempt_driver_claims
                     WHERE run_id = ?1 AND expires_at_unix_micros > ?2 AND quiescent = 0
                 )",
                params![run_id.to_string(), now],
                |row| row.get(0),
            )
            .map_err(sqlite_service_error)?;
        if active {
            transaction.commit().map_err(sqlite_service_error)?;
            return Ok(None);
        }
        let boundary_blocks_takeover: bool = transaction
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM attempt_driver_claims AS claim
                     LEFT JOIN executor_attempt_boundaries AS boundary
                       ON boundary.attempt_id = claim.attempt_id
                     WHERE claim.run_id = ?1
                       AND (boundary.state IS NULL
                         OR boundary.state NOT IN ('no_process', 'quiescent'))
                 )",
                [run_id.to_string()],
                |row| row.get(0),
            )
            .map_err(sqlite_service_error)?;
        if boundary_blocks_takeover {
            transaction.commit().map_err(sqlite_service_error)?;
            return Ok(None);
        }
        let previous: Option<(i64, i64)> = transaction
            .query_row(
                "SELECT fence, lease_version FROM attempt_driver_fences WHERE run_id = ?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(sqlite_service_error)?;
        let fence = previous.map_or(1, |value| value.0.checked_add(1).unwrap_or(0));
        let lease_version = previous.map_or(1, |value| value.1.checked_add(1).unwrap_or(0));
        if fence <= 0 || lease_version <= 0 {
            return Err(ServiceError::Store(
                "attempt driver fence exhausted".to_owned(),
            ));
        }
        let expires_at_unix_micros = now
            .checked_add(duration)
            .ok_or_else(|| ServiceError::Invalid("driver lease duration is invalid".to_owned()))?;
        transaction
            .execute(
                "INSERT INTO attempt_driver_fences (run_id, fence, lease_version)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(run_id) DO UPDATE SET
                   fence = excluded.fence, lease_version = excluded.lease_version",
                params![run_id.to_string(), fence, lease_version],
            )
            .map_err(sqlite_service_error)?;
        transaction
            .execute(
                "INSERT INTO attempt_driver_claims
                   (run_id, attempt_id, principal_id, fence, lease_version,
                    expires_at_unix_micros, quiescent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
                 ON CONFLICT(run_id) DO UPDATE SET
                   attempt_id = excluded.attempt_id,
                   principal_id = excluded.principal_id,
                   fence = excluded.fence,
                   lease_version = excluded.lease_version,
                   expires_at_unix_micros = excluded.expires_at_unix_micros,
                   quiescent = 0",
                params![
                    run_id.to_string(),
                    attempt_id.to_string(),
                    principal_id.to_string(),
                    fence,
                    lease_version,
                    expires_at_unix_micros,
                ],
            )
            .map_err(sqlite_service_error)?;
        transaction.commit().map_err(sqlite_service_error)?;
        Ok(Some(AttemptDriverClaim {
            run_id,
            attempt_id,
            principal_id,
            fence: FencingToken::new(fence as u64),
            lease_version: lease_version as u64,
            expires_at_unix_micros,
        }))
    }

    fn driver_connection(&self) -> Result<Connection, ServiceError> {
        Connection::open_with_flags(
            &self.database,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(sqlite_service_error)
    }

    fn worker_execute(
        &mut self,
        principal_id: PrincipalId,
        command: Command,
    ) -> Result<(), ServiceError> {
        let request =
            serde_json::to_vec(&command).map_err(|error| ServiceError::Store(error.to_string()))?;
        let key = IdempotencyKey::parse(&format!("executor-{}", blake3::hash(&request).to_hex()))
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        let trace = crate::domain::events::TraceId::parse("run-executor")
            .expect("executor trace id is valid");
        let driver_claim = self.driver_claim_for_command(&command)?;
        self.execute(WriteRequest {
            principal_id,
            idempotency_key: &key,
            trace_id: &trace,
            command: &command,
            driver_claim: Some(driver_claim),
            mcp_callback_request_digest: None,
            mcp_callback_authority: None,
            mcp_callback_recheck: None,
            mcp_callback_workspace_revision: None,
        })?;
        Ok(())
    }

    fn driver_claim_for_command(
        &self,
        command: &Command,
    ) -> Result<AttemptDriverClaim, ServiceError> {
        let run_id = match command {
            Command::StartAttempt { run_id, .. }
            | Command::TransitionRun { run_id, .. }
            | Command::RequestApproval { run_id, .. }
            | Command::RequestAuth { run_id, .. } => *run_id,
            Command::TransitionAttempt { attempt_id, .. } => self
                .driver_connection()?
                .query_row(
                    "SELECT claim.run_id FROM attempt_driver_claims AS claim
                     JOIN events AS event ON event.correlation_id = claim.run_id
                     WHERE event.attempt_id = ?1 LIMIT 1",
                    [attempt_id.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .ok()
                .and_then(|run| RunId::parse(&run).ok())
                .or_else(|| {
                    self.driver_connection()
                        .ok()?
                        .query_row(
                            "SELECT run_id FROM attempt_driver_claims LIMIT 1",
                            [],
                            |row| row.get::<_, String>(0),
                        )
                        .ok()
                        .and_then(|run| RunId::parse(&run).ok())
                })
                .ok_or_else(|| ServiceError::Conflict("attempt has no driver claim".to_owned()))?,
            _ => {
                return Err(ServiceError::Invalid(
                    "worker command is not driver-owned".to_owned(),
                ));
            }
        };
        self.load_driver_claim(run_id)
    }

    fn load_worker_run(&mut self, run_id: RunId) -> Result<WorkerRun, ServiceError> {
        let state = self.state()?;
        let run = state
            .runs
            .get(&run_id)
            .cloned()
            .ok_or(ServiceError::NotFound)?;
        let owner = run
            .owner
            .ok_or_else(|| ServiceError::Conflict("worker run has no attempt owner".to_owned()))?;
        let attempt = state
            .attempts
            .get(&owner.attempt_id)
            .cloned()
            .ok_or(ServiceError::NotFound)?;
        let project_id = state
            .project_for_run(run_id)
            .ok_or(ServiceError::NotFound)?;
        let principal_id = state
            .projects
            .get(&project_id)
            .ok_or(ServiceError::NotFound)?
            .principal_id;
        drop(state);

        let mut start = None;
        for event in self.append.events().map_err(store_error)? {
            if event.event.stream != EntityId::Run(run_id)
                || event.event.event_type.as_str() != "run.start"
            {
                continue;
            }
            let persisted: PersistedCommand = serde_json::from_slice(&event.event.payload)
                .map_err(|error| ServiceError::Store(error.to_string()))?;
            if let Command::StartRun {
                run_id: found,
                effective_config: Some(config),
                ..
            } = persisted.command
                && found == run_id
            {
                start = Some((config, persisted.idempotency_key, event.event.occurred_at));
                break;
            }
        }
        let (effective_config, start_idempotency_key, occurred_at) =
            start.ok_or_else(|| ServiceError::Store("run start snapshot is missing".to_owned()))?;
        let claim = self.load_driver_claim(run_id)?;
        Ok(WorkerRun {
            run,
            attempt,
            principal_id,
            project_id,
            effective_config,
            start_idempotency_key,
            occurred_at,
            claim,
        })
    }

    fn load_driver_claim(&self, run_id: RunId) -> Result<AttemptDriverClaim, ServiceError> {
        self.driver_connection()?
            .query_row(
                "SELECT attempt_id, principal_id, fence, lease_version,
                        expires_at_unix_micros
                 FROM attempt_driver_claims WHERE run_id = ?1",
                [run_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .map_err(sqlite_service_error)
            .and_then(|(attempt, principal, fence, lease_version, expires)| {
                Ok(AttemptDriverClaim {
                    run_id,
                    attempt_id: AttemptId::parse(&attempt)
                        .map_err(|error| ServiceError::Store(error.to_string()))?,
                    principal_id: PrincipalId::parse(&principal)
                        .map_err(|error| ServiceError::Store(error.to_string()))?,
                    fence: FencingToken::new(u64::try_from(fence).map_err(|_| {
                        ServiceError::Store("invalid durable driver fence".to_owned())
                    })?),
                    lease_version: u64::try_from(lease_version).map_err(|_| {
                        ServiceError::Store("invalid durable driver lease version".to_owned())
                    })?,
                    expires_at_unix_micros: expires,
                })
            })
    }

    fn load_driver_claim_for_owner(
        &self,
        owner: AttemptOwnership,
    ) -> Result<AttemptDriverClaim, ServiceError> {
        let (run, lease_version, expires) = self
            .driver_connection()?
            .query_row(
                "SELECT run_id, lease_version, expires_at_unix_micros
                 FROM attempt_driver_claims
                 WHERE attempt_id = ?1 AND principal_id = ?2 AND fence = ?3",
                params![
                    owner.attempt_id.to_string(),
                    owner.principal_id.to_string(),
                    i64::try_from(owner.fencing_token.get()).map_err(|_| {
                        ServiceError::Conflict("attempt driver fence is stale".to_owned())
                    })?,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .map_err(sqlite_service_error)?;
        Ok(AttemptDriverClaim {
            run_id: RunId::parse(&run).map_err(|error| ServiceError::Store(error.to_string()))?,
            attempt_id: owner.attempt_id,
            principal_id: owner.principal_id,
            fence: owner.fencing_token,
            lease_version: u64::try_from(lease_version).map_err(|_| {
                ServiceError::Store("invalid durable driver lease version".to_owned())
            })?,
            expires_at_unix_micros: expires,
        })
    }

    fn waiting_resolution(
        &self,
        state: &DomainReducer,
        principal_id: PrincipalId,
        command: &Command,
    ) -> Result<Option<(AttemptOwnership, Vec<LoopRecord>)>, ServiceError> {
        let run_id = match command {
            Command::ProvideRunInput { run_id, .. } | Command::ResolveAuth { run_id, .. } => {
                *run_id
            }
            Command::ResolveApproval { approval_id, .. } => state
                .approvals
                .get(approval_id)
                .map(|approval| approval.run_id)
                .ok_or(ServiceError::NotFound)?,
            _ => return Ok(None),
        };
        let run = state.runs.get(&run_id).ok_or(ServiceError::NotFound)?;
        let owner = run
            .owner
            .ok_or_else(|| ServiceError::Conflict("waiting run has no owner".to_owned()))?;
        let attempt = state
            .attempts
            .get(&owner.attempt_id)
            .ok_or(ServiceError::NotFound)?;
        let committed = self.append.events().map_err(store_error)?;
        let RecoveryState::Waiting(restored) = RestartProjection::reconstruct(attempt, &committed)
            .map_err(|error| ServiceError::Store(error.to_string()))?
        else {
            return Err(ServiceError::Conflict(
                "run has no unresolved durable waiting state".to_owned(),
            ));
        };
        let project_id = state
            .project_for_run(run_id)
            .ok_or(ServiceError::NotFound)?;
        let authenticated =
            AuthenticatedPrincipal::from_grants(GrantSnapshot::new(principal_id, project_id, []));
        let resolution = match (command, &restored.waiting.kind) {
            (Command::ProvideRunInput { input, .. }, WaitingKind::Input) => {
                let digest = ArtifactDigest::parse(input.as_str())
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?;
                let root = self
                    .database
                    .parent()
                    .ok_or_else(|| ServiceError::Store("database has no state root".to_owned()))?;
                let artifacts = ArtifactStore::open(root.join("artifacts"))
                    .map_err(|error| ServiceError::Store(error.to_string()))?;
                let artifact = artifacts
                    .open_verified(digest)
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?;
                if artifact.manifest().principal != principal_id.to_string()
                    || artifact.manifest().project != project_id.to_string()
                {
                    return Err(ServiceError::Authentication(
                        crate::api::auth::contract::AuthDenial::Unauthorized,
                    ));
                }
                let input = String::from_utf8(
                    artifacts
                        .open_bytes(digest)
                        .map_err(|error| ServiceError::Invalid(error.to_string()))?,
                )
                .map_err(|_| ServiceError::Invalid("run input is not UTF-8".to_owned()))?;
                restored
                    .waiting
                    .resolve_input(&authenticated, vec![Item::text(ItemKind::User, input)])
            }
            (
                Command::ResolveApproval {
                    approval_id,
                    decision,
                    ..
                },
                WaitingKind::Approval {
                    approval_id: waiting_id,
                    ..
                },
            ) if approval_id == waiting_id => restored.waiting.resolve(
                &authenticated,
                WaitingResolution::Approval {
                    decision: *decision,
                },
            ),
            (Command::ResolveAuth { granted, .. }, WaitingKind::Auth { run_id: found, .. })
                if *found == run_id =>
            {
                restored.waiting.resolve(
                    &authenticated,
                    WaitingResolution::Auth { granted: *granted },
                )
            }
            _ => {
                return Err(ServiceError::Conflict(
                    "resolution does not match durable waiting state".to_owned(),
                ));
            }
        }
        .map_err(|error| ServiceError::Conflict(error.to_string()))?;
        let mut records = vec![resolution];
        if matches!(
            command,
            Command::ResolveApproval {
                decision: crate::domain::events::ApprovalDecision::Denied,
                ..
            }
        ) {
            records.push(LoopRecord::CancellationRequested);
        }
        Ok(Some((owner, records)))
    }
}

impl ServiceStore for SqliteServiceStore {
    fn command_scope(
        &mut self,
        principal_id: PrincipalId,
        command: &Command,
    ) -> Result<ResourceScope, ServiceError> {
        if let Command::CreateProject { project_id, .. } = command {
            let state = self.state()?;
            if state.projects.contains_key(project_id) {
                return state.scope(Resource::Project(*project_id));
            }
            return Ok(ResourceScope::project_creation(principal_id, *project_id));
        }
        if let Command::ResolveMcpCallback { callback_id, .. } = command {
            let callback =
                crate::store::sqlite::mcp_callback::McpCallbackStore::open(&self.database)
                    .map_err(mcp_callback_error)?
                    .get(*callback_id)
                    .map_err(mcp_callback_error)?;
            return Ok(ResourceScope::new(
                callback.principal_id,
                callback.project_id,
            ));
        }
        let state = self.state()?;
        state.scope(command.resource())
    }

    fn query_scope(&mut self, query: &Query) -> Result<ResourceScope, ServiceError> {
        if let Query::GetMcpCallback { callback_id } = query {
            let callback =
                crate::store::sqlite::mcp_callback::McpCallbackStore::open(&self.database)
                    .map_err(mcp_callback_error)?
                    .get(*callback_id)
                    .map_err(mcp_callback_error)?;
            return Ok(ResourceScope::new(
                callback.principal_id,
                callback.project_id,
            ));
        }
        if let Query::PendingMcpCallbacks { project_id } = query {
            let state = self.state()?;
            return state.scope(Resource::Project(*project_id));
        }
        self.state()?.scope(query.resource())
    }

    fn execute(&mut self, request: WriteRequest<'_>) -> Result<CommandReceipt, ServiceError> {
        if let Command::ResolveMcpCallback {
            callback_id,
            expected_version,
            challenge_generation,
            schema_digest,
            action,
            content,
            artifact_refs,
            ..
        } = request.command
        {
            if content.is_some() {
                return Err(ServiceError::Invalid(
                    "callback content was not converted to an artifact".to_owned(),
                ));
            }
            let request_digest = request.mcp_callback_request_digest.ok_or_else(|| {
                ServiceError::Store("callback request digest is missing".to_owned())
            })?;
            let (_, replayed, commit_positions) =
                crate::store::sqlite::mcp_callback::McpCallbackStore::open(&self.database)
                    .map_err(mcp_callback_error)?
                    .resolve_with_recheck(
                        request.principal_id,
                        self.command_scope(request.principal_id, request.command)?
                            .project_id(),
                        request.idempotency_key,
                        *callback_id,
                        *expected_version,
                        *challenge_generation,
                        schema_digest,
                        *action,
                        artifact_refs.clone(),
                        request_digest,
                        request.mcp_callback_authority.ok_or_else(|| {
                            ServiceError::Conflict(
                                "callback commit authority is missing".to_owned(),
                            )
                        })?,
                        request.mcp_callback_recheck.ok_or_else(|| {
                            ServiceError::Conflict(
                                "callback commit authority recheck is missing".to_owned(),
                            )
                        })?,
                        request.mcp_callback_workspace_revision.ok_or_else(|| {
                            ServiceError::Conflict(
                                "callback workspace revision lease is missing".to_owned(),
                            )
                        })?,
                    )
                    .map_err(mcp_callback_error)?;
            return Ok(CommandReceipt {
                operation: request.command.operation(),
                commit_positions,
                replayed,
            });
        }
        let state = self.state()?;
        if let Command::CreateThread { thread_id, .. } = request.command
            && self.is_tombstoned(RetentionObjectId::Transcript(*thread_id))?
        {
            return Err(ServiceError::Conflict(
                "deleted thread identifier cannot be recreated".to_owned(),
            ));
        }
        let event_scope = state.scope_for_command(request.command)?;
        let target = request.command.target();
        let request_bytes = serde_json::to_vec(request.command)
            .map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let request_digest = CanonicalRequestDigest::new(
            Digest::of(DigestAlgorithm::Sha256, &request_bytes).as_bytes(),
        );
        let idempotency_scope =
            IdempotencyScope::new(request.principal_id, request.command.operation(), target)
                .map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let idempotency_status = self
            .append
            .idempotency_status(&idempotency_scope, request.idempotency_key)
            .map_err(store_error)?;
        let waiting_resolution = if matches!(idempotency_status, IdempotencyStatus::Missing) {
            state.validate(request.principal_id, request.command)?;
            self.waiting_resolution(&state, request.principal_id, request.command)?
        } else {
            None
        };
        let occurred_at = self
            .projections
            .store_time()
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        let payload = serde_json::to_vec(&PersistedCommand {
            principal_id: request.principal_id,
            stored_at_unix_micros: occurred_at.unix_micros(),
            idempotency_key: request.idempotency_key.as_str().to_owned(),
            apply_projection: true,
            command: request.command.clone(),
        })
        .map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let artifacts = serde_json::to_vec(
            &request
                .command
                .artifact_reference()
                .into_iter()
                .collect::<Vec<_>>(),
        )
        .map_err(|error| ServiceError::Invalid(error.to_string()))?;
        let command_id =
            CommandId::generate().map_err(|error| ServiceError::Store(error.to_string()))?;
        let event_type = EventType::parse(request.command.operation())
            .expect("registered operation names are valid event types");
        // Public progress shares the run stream but does not change the resource CAS version.
        let append_version = self
            .append
            .events()
            .map_err(store_error)?
            .into_iter()
            .filter(|event| event.event.stream == target)
            .map(|event| event.sequence.get())
            .max()
            .unwrap_or(0);
        let mut expected_versions = vec![ExpectedStreamVersion {
            stream: target,
            version: ExpectedVersion::new(append_version),
        }];
        let mut event_streams = vec![(target, payload)];
        if let Some((stream, version)) = request.command.secondary_stream() {
            let version = self
                .append
                .events()
                .map_err(store_error)?
                .into_iter()
                .filter(|event| event.event.stream == stream)
                .map(|event| event.sequence.get())
                .max()
                .unwrap_or(version);
            expected_versions.push(ExpectedStreamVersion {
                stream,
                version: ExpectedVersion::new(version),
            });
            let payload = serde_json::to_vec(&PersistedCommand {
                principal_id: request.principal_id,
                stored_at_unix_micros: occurred_at.unix_micros(),
                idempotency_key: request.idempotency_key.as_str().to_owned(),
                apply_projection: false,
                command: request.command.clone(),
            })
            .map_err(|error| ServiceError::Invalid(error.to_string()))?;
            event_streams.push((stream, payload));
        }
        let mut events = event_streams
            .into_iter()
            .map(|(stream, payload)| {
                Ok(NewEvent {
                    id: EventId::generate()
                        .map_err(|error| ServiceError::Store(error.to_string()))?,
                    stream,
                    event_type: event_type.clone(),
                    schema_version: SchemaVersion::CURRENT,
                    occurred_at: store_timestamp(&occurred_at)?,
                    causation_id: command_id,
                    correlation_id: request.command.correlation(),
                    attempt_id: request.command.attempt_id(),
                    trace_id: request.trace_id.clone(),
                    payload,
                    artifacts: artifacts.clone(),
                })
            })
            .collect::<Result<Vec<_>, ServiceError>>()?;
        let resolution_claim = waiting_resolution
            .as_ref()
            .map(|(owner, _)| self.load_driver_claim_for_owner(*owner))
            .transpose()?;
        if let Some((owner, records)) = waiting_resolution {
            let stream = EntityId::Attempt(owner.attempt_id);
            let version = self
                .append
                .events()
                .map_err(store_error)?
                .into_iter()
                .filter(|event| event.event.stream == stream)
                .map(|event| event.sequence.get())
                .max()
                .unwrap_or(0);
            expected_versions.push(ExpectedStreamVersion {
                stream,
                version: ExpectedVersion::new(version),
            });
            for record in records {
                let payload = serde_json::to_vec(&serde_json::json!({
                    "schema_version": 1,
                    "owner": owner,
                    "claim": resolution_claim.expect("waiting resolution has a claim"),
                    "record": record,
                }))
                .map_err(|error| ServiceError::Invalid(error.to_string()))?;
                events.push(NewEvent {
                    id: EventId::generate()
                        .map_err(|error| ServiceError::Store(error.to_string()))?,
                    stream,
                    event_type: EventType::parse(EFFECT_JOURNAL_EVENT)
                        .expect("effect journal event name is valid"),
                    schema_version: SchemaVersion::CURRENT,
                    occurred_at: store_timestamp(&occurred_at)?,
                    causation_id: command_id,
                    correlation_id: stream,
                    attempt_id: Some(owner.attempt_id),
                    trace_id: request.trace_id.clone(),
                    payload,
                    artifacts: b"[]".to_vec(),
                });
            }
        }
        let outcome = self
            .append
            .append(AppendCommand {
                idempotency_scope,
                idempotency_key: request.idempotency_key.clone(),
                request_digest,
                claim: None,
                driver_claim: request.driver_claim.or(resolution_claim),
                allow_quiescent_driver_claim: resolution_claim.is_some(),
                expected_versions,
                events,
                response: b"accepted".to_vec(),
            })
            .map_err(store_error)?;
        let (response, replayed) = match outcome {
            AppendOutcome::Committed(response) => (response, false),
            AppendOutcome::Replayed(response) => (response, true),
        };
        self.projections
            .index_committed_events(
                &response.commit_positions,
                event_scope,
                occurred_at.unix_micros(),
            )
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        Ok(CommandReceipt {
            operation: request.command.operation(),
            commit_positions: response.commit_positions,
            replayed,
        })
    }

    fn replay_mcp_callback_resolution(
        &mut self,
        principal_id: PrincipalId,
        idempotency_key: &IdempotencyKey,
        command: &Command,
    ) -> Result<Option<CommandReceipt>, ServiceError> {
        let Command::ResolveMcpCallback {
            callback_id,
            expected_version,
            challenge_generation,
            schema_digest,
            ..
        } = command
        else {
            return Ok(None);
        };
        let request_digest = mcp_callback_request_digest(command)?;
        crate::store::sqlite::mcp_callback::McpCallbackStore::open(&self.database)
            .map_err(mcp_callback_error)?
            .replay_resolution(
                principal_id,
                self.command_scope(principal_id, command)?.project_id(),
                *callback_id,
                idempotency_key,
                request_digest,
                *expected_version,
                *challenge_generation,
                schema_digest,
            )
            .map(|replay| {
                replay.map(|(_, commit_positions)| CommandReceipt {
                    operation: command.operation(),
                    commit_positions,
                    replayed: true,
                })
            })
            .map_err(mcp_callback_error)
    }

    fn reserve_mcp_callback_resolution(
        &mut self,
        principal_id: PrincipalId,
        project_id: ProjectId,
        idempotency_key: &IdempotencyKey,
        command: &Command,
    ) -> Result<Option<CommandReceipt>, ServiceError> {
        let Command::ResolveMcpCallback {
            callback_id,
            expected_version,
            challenge_generation,
            schema_digest,
            ..
        } = command
        else {
            return Ok(None);
        };
        crate::store::sqlite::mcp_callback::McpCallbackStore::open(&self.database)
            .map_err(mcp_callback_error)?
            .reserve_resolution(
                principal_id,
                project_id,
                *callback_id,
                idempotency_key,
                mcp_callback_request_digest(command)?,
                *expected_version,
                *challenge_generation,
                schema_digest,
            )
            .map(|replay| {
                replay.map(|(_, commit_positions)| CommandReceipt {
                    operation: command.operation(),
                    commit_positions,
                    replayed: true,
                })
            })
            .map_err(mcp_callback_error)
    }

    fn query(&mut self, query: &Query) -> Result<QueryProjection, ServiceError> {
        let state = self.state()?;
        match query {
            Query::GetProject { project_id } => state
                .projects
                .get(project_id)
                .cloned()
                .map(QueryProjection::Project)
                .ok_or(ServiceError::NotFound),
            Query::GetProjectRetention { project_id } => state
                .projects
                .get(project_id)
                .map(|project| QueryProjection::Retention(project.retention))
                .ok_or(ServiceError::NotFound),
            Query::ListThreads { project_id } => Ok(QueryProjection::Threads(
                state
                    .threads
                    .values()
                    .filter(|thread| thread.project_id == *project_id)
                    .cloned()
                    .collect(),
            )),
            Query::GetThread { thread_id } => state
                .threads
                .get(thread_id)
                .cloned()
                .map(QueryProjection::Thread)
                .ok_or(ServiceError::NotFound),
            Query::GetDeletionJob { .. } => Err(ServiceError::Invalid(
                "deletion job queries are served by the deletion service".to_owned(),
            )),
            Query::ThreadEvents {
                thread_id,
                after,
                limit,
            } => Ok(QueryProjection::Events(self.event_page(
                EventScope::Thread(*thread_id),
                *after,
                *limit,
            )?)),
            Query::ListRuns { project_id } => Ok(QueryProjection::Runs(
                state
                    .runs
                    .values()
                    .filter(|run| state.project_for_run(run.id) == Some(*project_id))
                    .cloned()
                    .collect(),
            )),
            Query::GetRun { run_id } => state
                .runs
                .get(run_id)
                .cloned()
                .map(QueryProjection::Run)
                .ok_or(ServiceError::NotFound),
            Query::GetRunCost { run_id } => {
                if !state.runs.contains_key(run_id) {
                    return Err(ServiceError::NotFound);
                }
                Ok(QueryProjection::RunCost(Box::new(
                    state.run_costs.get(run_id).cloned().unwrap_or_default(),
                )))
            }
            Query::GetRunPrompts { run_id } => {
                if !state.runs.contains_key(run_id) {
                    return Err(ServiceError::NotFound);
                }
                Ok(QueryProjection::RunPrompts(
                    state.run_prompts.get(run_id).cloned().unwrap_or_default(),
                ))
            }
            Query::RunTranscript { run_id } => {
                if !state.runs.contains_key(run_id) {
                    return Err(ServiceError::NotFound);
                }
                Ok(QueryProjection::RunTranscript(RunTranscriptProjection {
                    run_id: *run_id,
                    items: state
                        .run_transcripts
                        .get(run_id)
                        .cloned()
                        .unwrap_or_default(),
                }))
            }
            Query::GetAttempt { attempt_id } => state
                .attempts
                .get(attempt_id)
                .cloned()
                .map(QueryProjection::Attempt)
                .ok_or(ServiceError::NotFound),
            Query::RunTimeline {
                run_id,
                after,
                limit,
            } => Ok(QueryProjection::Events(self.event_page(
                EventScope::Run(*run_id),
                *after,
                *limit,
            )?)),
            Query::PendingApprovals { project_id } => Ok(QueryProjection::Approvals(
                state
                    .approvals
                    .values()
                    .filter(|approval| {
                        approval.decision.is_none()
                            && state.project_for_run(approval.run_id) == Some(*project_id)
                    })
                    .cloned()
                    .collect(),
            )),
            Query::PendingAuthRequests { project_id } => Ok(QueryProjection::AuthRequests(
                state
                    .auth_requests
                    .values()
                    .filter(|request| {
                        request.granted.is_none()
                            && state.project_for_run(request.run_id) == Some(*project_id)
                    })
                    .cloned()
                    .collect(),
            )),
            Query::PendingMcpCallbacks { project_id } => {
                crate::store::sqlite::mcp_callback::McpCallbackStore::open(&self.database)
                    .map_err(mcp_callback_error)?
                    .pending(*project_id)
                    .map(QueryProjection::McpCallbacks)
                    .map_err(mcp_callback_error)
            }
            Query::GetMcpCallback { callback_id } => {
                crate::store::sqlite::mcp_callback::McpCallbackStore::open(&self.database)
                    .map_err(mcp_callback_error)?
                    .get(*callback_id)
                    .map(|callback| QueryProjection::McpCallback(Box::new(callback)))
                    .map_err(mcp_callback_error)
            }
            Query::GetArtifactMetadata { artifact_id } => state
                .artifacts
                .get(artifact_id)
                .cloned()
                .map(QueryProjection::ArtifactMetadata)
                .ok_or(ServiceError::NotFound),
            Query::ListCapabilities { .. } => Err(ServiceError::Invalid(
                "capability queries are served by the capability service".to_owned(),
            )),
            Query::EventCursorStatus { cursor, .. } => {
                let project_id = match query {
                    Query::EventCursorStatus { project_id, .. } => *project_id,
                    _ => unreachable!(),
                };
                let committed = self.project_cursor(project_id)?;
                Ok(QueryProjection::CursorStatus(CursorStatusProjection {
                    requested: *cursor,
                    committed,
                    caught_up: cursor.position() >= committed.position(),
                }))
            }
            Query::Status { project_id } => Ok(QueryProjection::Status(StatusProjection {
                committed: self.project_cursor(*project_id)?,
                ready: true,
            })),
        }
    }

    fn deletion_job(
        &mut self,
        actor: DeletionActor,
        id: DeletionJobId,
    ) -> Result<DeletionJob, DeletionError> {
        self.state().map_err(|_| DeletionError::NotFound)?;
        self.load_deletion_job(actor, "job_id = ?1", id.to_string())
    }

    fn deletion_job_for_request(
        &mut self,
        actor: DeletionActor,
        object_id: RetentionObjectId,
        idempotency_key: &str,
    ) -> Result<DeletionJob, DeletionError> {
        self.state().map_err(|_| DeletionError::NotFound)?;
        self.load_deletion_job_for_request(actor, object_id, idempotency_key)
    }

    fn archive_status(
        &mut self,
        actor: DeletionActor,
        object_id: RetentionObjectId,
    ) -> Result<ArchiveStatus, DeletionError> {
        self.state().map_err(|_| DeletionError::NotFound)?;
        self.projections
            .with_store_time(|transaction, _| {
                transaction
                    .query_row(
                        "SELECT archived FROM deletion_objects
                         WHERE object_key = ?1 AND principal_id = ?2 AND project_id = ?3
                           AND physically_deleted = 0",
                        params![
                            object_key(object_id),
                            actor.principal_id.to_string(),
                            actor.project_id.to_string()
                        ],
                        |row| row.get::<_, bool>(0),
                    )
                    .optional()
                    .map_err(Into::into)
            })
            .map_err(|_| DeletionError::NotFound)?
            .map(|archived| ArchiveStatus {
                object_id,
                archived,
            })
            .ok_or(DeletionError::NotFound)
    }

    fn store_time(&mut self) -> Result<StoreTimestamp, ServiceError> {
        self.projections
            .store_time()
            .map(|time| StoreTimestamp::from_unix_micros(time.unix_micros()))
            .map_err(|error| ServiceError::Store(error.to_string()))
    }
}

impl SqliteServiceStore {
    fn state(&mut self) -> Result<DomainReducer, ServiceError> {
        let (mut state, _) = self
            .projections
            .update_domain()
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        self.sync_deletion_projection(&state)?;
        self.hide_physically_deleted(&mut state)?;
        Ok(state)
    }

    fn event_page(
        &mut self,
        scope: EventScope,
        after: EventCursor,
        limit: usize,
    ) -> Result<EventPage, ServiceError> {
        let (column, value) = match scope {
            EventScope::Thread(id) => ("thread_id", id.to_string()),
            EventScope::Run(id) => ("run_id", id.to_string()),
        };
        let sql = format!(
            "SELECT event.commit_position, index_row.project_id, event.event_type,
                    event.stream, event.payload
             FROM event_projection_index AS index_row
             JOIN events AS event ON event.commit_position = index_row.commit_position
             WHERE index_row.{column} = ?1 AND index_row.erased = 0
               AND event.commit_position > ?2
               AND event.commit_position <= (SELECT position FROM commit_watermark WHERE singleton = 1)
             ORDER BY event.commit_position LIMIT ?3"
        );
        let events = self
            .projections
            .with_store_time(|transaction, _| {
                let mut statement = transaction.prepare(&sql)?;
                let rows = statement.query_map(
                    params![value, after.position(), limit.min(1_000)],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Vec<u8>>(4)?,
                        ))
                    },
                )?;
                rows.map(|row| {
                    let (position, project, operation, stream, payload) = row?;
                    Ok(EventProjection {
                        cursor: EventCursor::new(u64::try_from(position).map_err(|error| {
                            crate::store::sqlite::projection::ProjectionError::Reducer(
                                error.to_string(),
                            )
                        })?),
                        project_id: ProjectId::parse(&project).map_err(|error| {
                            crate::store::sqlite::projection::ProjectionError::Reducer(
                                error.to_string(),
                            )
                        })?,
                        operation,
                        stream,
                        payload,
                    })
                })
                .collect::<Result<Vec<_>, _>>()
            })
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        let next_cursor = events.last().map(|event| event.cursor).unwrap_or(after);
        Ok(EventPage {
            events,
            next_cursor,
        })
    }

    fn project_cursor(&mut self, project_id: ProjectId) -> Result<EventCursor, ServiceError> {
        self.projections
            .with_store_time(|transaction, _| {
                let position: i64 = transaction.query_row(
                    "SELECT coalesce(max(commit_position), 0)
                     FROM event_projection_index WHERE project_id = ?1",
                    [project_id.to_string()],
                    |row| row.get(0),
                )?;
                u64::try_from(position)
                    .map(EventCursor::new)
                    .map_err(|error| {
                        crate::store::sqlite::projection::ProjectionError::Reducer(
                            error.to_string(),
                        )
                    })
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    fn is_tombstoned(&mut self, object_id: RetentionObjectId) -> Result<bool, ServiceError> {
        let digest = sha256(object_key(object_id).as_bytes());
        self.projections
            .with_store_time(|transaction, _| {
                transaction
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM deletion_tombstones WHERE target_sha256 = ?1)",
                        [digest.as_slice()],
                        |row| row.get(0),
                    )
                    .map_err(Into::into)
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    fn hide_physically_deleted(&mut self, state: &mut DomainReducer) -> Result<(), ServiceError> {
        let deleted = self
            .projections
            .with_store_time(|transaction, _| {
                let mut statement = transaction.prepare(
                    "SELECT object_key FROM deletion_objects WHERE physically_deleted = 1",
                )?;
                statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(Into::into)
            })
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        for key in deleted {
            match parse_object_key(&key).map_err(ServiceError::Store)? {
                RetentionObjectId::Transcript(id) => {
                    state.threads.remove(&id);
                    state.runs.retain(|_, run| run.thread_id != id);
                }
                RetentionObjectId::Artifact(id) => {
                    state.artifacts.remove(&id);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn sync_deletion_projection(&mut self, state: &DomainReducer) -> Result<(), ServiceError> {
        self.projections
            .with_store_time(|transaction, _| {
                for thread in state.threads.values() {
                    let project = state.projects.get(&thread.project_id).ok_or_else(|| {
                        crate::store::sqlite::projection::ProjectionError::Reducer(
                            "thread project is missing".to_owned(),
                        )
                    })?;
                    let policy = project.retention.unwrap_or(default_retention_policy());
                    transaction.execute(
                        "INSERT INTO deletion_objects
                         (object_key, object_kind, principal_id, project_id, stored_at_unix_micros,
                          archived, artifact_digest, policy_json)
                         VALUES (?1, 'transcript', ?2, ?3, ?4, ?5, NULL, ?6)
                         ON CONFLICT(object_key) DO UPDATE SET
                           principal_id = excluded.principal_id,
                           project_id = excluded.project_id,
                           stored_at_unix_micros = excluded.stored_at_unix_micros,
                           archived = excluded.archived,
                           policy_json = excluded.policy_json",
                        params![
                            object_key(RetentionObjectId::Transcript(thread.id)),
                            project.principal_id.to_string(),
                            project.id.to_string(),
                            state.thread_stored_at.get(&thread.id).copied().unwrap_or(0),
                            thread.archived,
                            serde_json::to_vec(&policy).map_err(|error| {
                                crate::store::sqlite::projection::ProjectionError::Reducer(
                                    error.to_string(),
                                )
                            })?,
                        ],
                    )?;
                }
                for artifact in state.artifacts.values() {
                    let project = state.projects.get(&artifact.project_id).ok_or_else(|| {
                        crate::store::sqlite::projection::ProjectionError::Reducer(
                            "artifact project is missing".to_owned(),
                        )
                    })?;
                    let policy = project.retention.unwrap_or(default_retention_policy());
                    transaction.execute(
                        "INSERT INTO deletion_objects
                         (object_key, object_kind, principal_id, project_id, stored_at_unix_micros,
                          archived, artifact_digest, policy_json)
                         VALUES (?1, 'artifact', ?2, ?3, ?4, 0, ?5, ?6)
                         ON CONFLICT(object_key) DO UPDATE SET
                           principal_id = excluded.principal_id,
                           project_id = excluded.project_id,
                           stored_at_unix_micros = excluded.stored_at_unix_micros,
                           artifact_digest = excluded.artifact_digest,
                           policy_json = excluded.policy_json",
                        params![
                            object_key(RetentionObjectId::Artifact(artifact.id)),
                            project.principal_id.to_string(),
                            project.id.to_string(),
                            state.artifact_stored_at.get(&artifact.id).copied().unwrap_or(0),
                            artifact.reference.as_str(),
                            serde_json::to_vec(&policy).map_err(|error| {
                                crate::store::sqlite::projection::ProjectionError::Reducer(
                                    error.to_string(),
                                )
                            })?,
                        ],
                    )?;
                }
                for run in state.runs.values() {
                    let Some(artifact) = state
                        .artifacts
                        .values()
                        .find(|artifact| artifact.reference == run.input)
                    else {
                        continue;
                    };
                    let Some(project_id) = state.project_for_run(run.id) else {
                        continue;
                    };
                    let Some(project) = state.projects.get(&project_id) else {
                        continue;
                    };
                    transaction.execute(
                        "INSERT INTO deletion_artifact_references
                         (reference_id, artifact_id, principal_id, project_id, expires_at_unix_micros)
                         VALUES (?1, ?2, ?3, ?4, NULL)
                         ON CONFLICT(reference_id) DO UPDATE SET
                           artifact_id = excluded.artifact_id,
                           principal_id = excluded.principal_id,
                           project_id = excluded.project_id,
                           expires_at_unix_micros = excluded.expires_at_unix_micros",
                        params![
                            format!("run:{}", run.id),
                            artifact.id.to_string(),
                            project.principal_id.to_string(),
                            project_id.to_string(),
                        ],
                    )?;
                }
                let fence: i64 = transaction.query_row(
                    "SELECT fence FROM deletion_inventory_clock WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )?;
                for intent in &state.deletion_intents {
                    let policy = serde_json::to_vec(&intent.policy).map_err(|error| {
                        crate::store::sqlite::projection::ProjectionError::Reducer(
                            error.to_string(),
                        )
                    })?;
                    let earliest = initial_earliest(intent);
                    let inserted = transaction.execute(
                        "INSERT OR IGNORE INTO deletion_jobs
                         (job_id, principal_id, project_id, object_key, idempotency_key,
                          resource_version, policy_snapshot_json, policy_json,
                          earliest_physical_unix_micros, state, version, fence, blockers_json,
                          requested_at_unix_micros)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, 'requested', 1, ?9, '[]', ?10)",
                        params![
                            intent.job_id().to_string(),
                            intent.principal_id.to_string(),
                            intent.project_id.to_string(),
                            object_key(intent.object_id()),
                            intent.idempotency_key,
                            intent.resource_version,
                            policy,
                            earliest,
                            fence,
                            intent.requested_at_unix_micros,
                        ],
                    )?;
                    if inserted == 1 {
                        transaction.execute(
                            "INSERT INTO deletion_job_audit (job_id, sequence, state, at_unix_micros)
                             VALUES (?1, 1, 'requested', ?2)",
                            params![intent.job_id().to_string(), intent.requested_at_unix_micros],
                        )?;
                    }
                }
                Ok(())
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    fn claim_deletion_job(
        &mut self,
        worker_id: &str,
    ) -> Result<Option<Option<ClaimedDeletion>>, ServiceError> {
        self.projections
            .with_store_time(|transaction, now| {
                let fence: i64 = transaction.query_row(
                    "SELECT fence FROM deletion_inventory_clock WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )?;
                let candidate = transaction
                    .query_row(
                        "SELECT job_id, version FROM deletion_jobs AS job
                         WHERE state IN ('requested', 'waiting_for_policy', 'blocked', 'failed')
                           AND (
                               state = 'requested'
                               OR fence <> ?2
                               OR (state = 'failed' AND (lease_until_unix_micros IS NULL
                                                        OR lease_until_unix_micros <= ?1))
                               OR (earliest_physical_unix_micros IS NOT NULL
                                   AND earliest_physical_unix_micros <= ?1
                                   AND (lease_until_unix_micros IS NULL
                                        OR lease_until_unix_micros <= ?1))
                           )
                           AND EXISTS (
                               SELECT 1 FROM deletion_objects AS object
                               WHERE object.object_key = job.object_key
                                 AND object.physically_deleted = 0
                           )
                         ORDER BY requested_at_unix_micros, job_id LIMIT 1",
                        params![now.unix_micros(), fence],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )
                    .optional()?;
                let Some((job_id, version)) = candidate else {
                    return Ok(None);
                };
                let updated = transaction.execute(
                    "UPDATE deletion_jobs SET state = 'evaluating', version = version + 1,
                       fence = ?1, worker_id = ?2, lease_until_unix_micros = ?3, failure = NULL
                     WHERE job_id = ?4 AND version = ?5
                       AND state IN ('requested', 'waiting_for_policy', 'blocked', 'failed')",
                    params![
                        fence,
                        worker_id,
                        now.unix_micros().saturating_add(30_000_000),
                        job_id,
                        version
                    ],
                )?;
                if updated != 1 {
                    return Ok(Some(None));
                }
                append_audit(transaction, &job_id, "evaluating", now.unix_micros())?;
                let evaluation = evaluate_job(transaction, &job_id, now.unix_micros())?;
                let blocker_names = evaluation
                    .decision
                    .blockers
                    .iter()
                    .filter_map(public_blocker)
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let blockers = serde_json::to_vec(&blocker_names).map_err(|error| {
                    crate::store::sqlite::projection::ProjectionError::Reducer(error.to_string())
                })?;
                let earliest = earliest_value(evaluation.decision.earliest);
                let policy = serde_json::to_vec(&evaluation.policy).map_err(|error| {
                    crate::store::sqlite::projection::ProjectionError::Reducer(error.to_string())
                })?;
                let evaluating_version = version + 1;
                if !evaluation.decision.physically_deletable {
                    let state = if blocker_names.iter().any(|name| name == "legal_hold") {
                        "blocked"
                    } else {
                        "waiting_for_policy"
                    };
                    let retry_at = earliest
                        .filter(|at| *at > now.unix_micros())
                        .unwrap_or_else(|| now.unix_micros().saturating_add(250_000));
                    transaction.execute(
                        "UPDATE deletion_jobs SET state = ?1, version = version + 1,
                           policy_json = ?2, earliest_physical_unix_micros = ?3,
                           blockers_json = ?4, worker_id = NULL,
                           lease_until_unix_micros = ?5
                         WHERE job_id = ?6 AND version = ?7 AND worker_id = ?8 AND fence = ?9",
                        params![
                            state,
                            policy,
                            earliest,
                            blockers,
                            retry_at,
                            job_id,
                            evaluating_version,
                            worker_id,
                            fence
                        ],
                    )?;
                    append_audit(transaction, &job_id, state, now.unix_micros())?;
                    return Ok(Some(None));
                }
                transaction.execute(
                    "UPDATE deletion_jobs SET state = 'physically_deleting', version = version + 1,
                       policy_json = ?1, earliest_physical_unix_micros = ?2,
                       blockers_json = ?3, effect_unknown = 0
                     WHERE job_id = ?4 AND version = ?5 AND worker_id = ?6 AND fence = ?7",
                    params![
                        policy,
                        earliest,
                        blockers,
                        job_id,
                        evaluating_version,
                        worker_id,
                        fence
                    ],
                )?;
                append_audit(
                    transaction,
                    &job_id,
                    "physically_deleting",
                    now.unix_micros(),
                )?;
                Ok(Some(Some(ClaimedDeletion {
                    effect: DeletionEffect {
                        job_id: job_id.parse().map_err(
                            |error: crate::domain::deletion::DeletionJobIdParseError| {
                                crate::store::sqlite::projection::ProjectionError::Reducer(
                                    error.to_string(),
                                )
                            },
                        )?,
                        object_id: evaluation.object.id,
                        artifact_digest: evaluation.artifact_digest,
                    },
                    version: u64::try_from(evaluating_version + 1).map_err(|error| {
                        crate::store::sqlite::projection::ProjectionError::Reducer(
                            error.to_string(),
                        )
                    })?,
                    fence: u64::try_from(fence).map_err(|error| {
                        crate::store::sqlite::projection::ProjectionError::Reducer(
                            error.to_string(),
                        )
                    })?,
                })))
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    fn finish_deletion_job(
        &mut self,
        worker_id: &str,
        claim: &ClaimedDeletion,
        physically_delete: &mut impl FnMut(&DeletionEffect) -> Result<(), String>,
    ) -> Result<FinishOutcome, ServiceError> {
        self.projections
            .with_store_time(|transaction, now| {
                let job_id = claim.effect.job_id.to_string();
                let current_fence: i64 = transaction.query_row(
                    "SELECT fence FROM deletion_inventory_clock WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )?;
                let owns: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM deletion_jobs
                     WHERE job_id = ?1 AND state = 'physically_deleting' AND version = ?2
                       AND fence = ?3 AND worker_id = ?4 AND lease_until_unix_micros > ?5)",
                    params![
                        job_id,
                        claim.version,
                        claim.fence,
                        worker_id,
                        now.unix_micros()
                    ],
                    |row| row.get(0),
                )?;
                if !owns || current_fence != claim.fence as i64 {
                    if owns {
                        transaction.execute(
                            "UPDATE deletion_jobs SET state = 'requested', version = version + 1,
                               fence = ?1, worker_id = NULL, lease_until_unix_micros = NULL
                             WHERE job_id = ?2 AND version = ?3 AND worker_id = ?4",
                            params![current_fence, job_id, claim.version, worker_id],
                        )?;
                        append_audit(transaction, &job_id, "requested", now.unix_micros())?;
                    }
                    return Ok(FinishOutcome::Stale);
                }

                let evaluation = evaluate_job(transaction, &job_id, now.unix_micros())?;
                if !evaluation.decision.physically_deletable {
                    let names = evaluation
                        .decision
                        .blockers
                        .iter()
                        .filter_map(public_blocker)
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    let state = if names.iter().any(|name| name == "legal_hold") {
                        "blocked"
                    } else {
                        "waiting_for_policy"
                    };
                    transaction.execute(
                        "UPDATE deletion_jobs SET state = ?1, version = version + 1,
                           earliest_physical_unix_micros = ?2, blockers_json = ?3,
                           worker_id = NULL, lease_until_unix_micros = ?4
                         WHERE job_id = ?5 AND version = ?6 AND fence = ?7 AND worker_id = ?8",
                        params![
                            state,
                            earliest_value(evaluation.decision.earliest),
                            serde_json::to_vec(&names).map_err(|error| {
                                crate::store::sqlite::projection::ProjectionError::Reducer(
                                    error.to_string(),
                                )
                            })?,
                            now.unix_micros().saturating_add(250_000),
                            job_id,
                            claim.version,
                            claim.fence,
                            worker_id,
                        ],
                    )?;
                    append_audit(transaction, &job_id, state, now.unix_micros())?;
                    return Ok(FinishOutcome::Waiting);
                }

                if let Err(error) = physically_delete(&claim.effect) {
                    transaction.execute(
                        "UPDATE deletion_jobs SET state = 'failed', version = version + 1,
                           failure = ?1, worker_id = NULL, lease_until_unix_micros = ?2
                         WHERE job_id = ?3 AND version = ?4 AND fence = ?5 AND worker_id = ?6",
                        params![
                            error,
                            now.unix_micros().saturating_add(250_000),
                            job_id,
                            claim.version,
                            claim.fence,
                            worker_id
                        ],
                    )?;
                    append_audit(transaction, &job_id, "failed", now.unix_micros())?;
                    return Ok(FinishOutcome::Failed);
                }
                erase_retained_object(transaction, &evaluation, now.unix_micros())?;
                transaction.execute(
                    "UPDATE deletion_objects SET physically_deleted = 1
                     WHERE object_key = ?1 AND physically_deleted = 0",
                    [object_key(claim.effect.object_id)],
                )?;
                let completed_fence: i64 = transaction.query_row(
                    "SELECT fence FROM deletion_inventory_clock WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )?;
                transaction.execute(
                    "UPDATE deletion_jobs SET state = 'completed', version = version + 1,
                       fence = ?1, completed_at_unix_micros = ?2, worker_id = NULL,
                       lease_until_unix_micros = NULL, failure = NULL
                     WHERE job_id = ?3 AND version = ?4 AND fence = ?5 AND worker_id = ?6",
                    params![
                        completed_fence,
                        now.unix_micros(),
                        job_id,
                        claim.version,
                        claim.fence,
                        worker_id
                    ],
                )?;
                append_audit(transaction, &job_id, "completed", now.unix_micros())?;
                Ok(FinishOutcome::Completed)
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    fn load_deletion_job(
        &mut self,
        actor: DeletionActor,
        _condition: &str,
        job_id: String,
    ) -> Result<DeletionJob, DeletionError> {
        self.projections
            .with_store_time(|transaction, _| {
                read_job(transaction, &job_id, actor).map_err(|error| {
                    crate::store::sqlite::projection::ProjectionError::Reducer(error.to_string())
                })
            })
            .map_err(|_| DeletionError::NotFound)?
            .ok_or(DeletionError::NotFound)
    }

    fn load_deletion_job_for_request(
        &mut self,
        actor: DeletionActor,
        object_id: RetentionObjectId,
        idempotency_key: &str,
    ) -> Result<DeletionJob, DeletionError> {
        self.projections
            .with_store_time(|transaction, _| {
                let id = transaction
                    .query_row(
                        "SELECT job_id FROM deletion_jobs
                         WHERE principal_id = ?1 AND project_id = ?2 AND object_key = ?3
                           AND idempotency_key = ?4",
                        params![
                            actor.principal_id.to_string(),
                            actor.project_id.to_string(),
                            object_key(object_id),
                            idempotency_key,
                        ],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                id.map(|id| read_job(transaction, &id, actor))
                    .transpose()
                    .map_err(|error| {
                        crate::store::sqlite::projection::ProjectionError::Reducer(
                            error.to_string(),
                        )
                    })
            })
            .map_err(|_| DeletionError::NotFound)?
            .flatten()
            .ok_or(DeletionError::NotFound)
    }

    pub fn put_legal_hold(&mut self, hold: LegalHold) -> Result<(), ServiceError> {
        let (scope_kind, scope_id) = legal_hold_scope(hold.scope);
        self.projections
            .with_store_time(|transaction, _| {
                transaction.execute(
                    "INSERT INTO deletion_legal_holds
                     (hold_id, scope_kind, scope_id, placed_at_unix_micros, released_at_unix_micros)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(hold_id) DO UPDATE SET scope_kind = excluded.scope_kind,
                       scope_id = excluded.scope_id,
                       placed_at_unix_micros = excluded.placed_at_unix_micros,
                       released_at_unix_micros = excluded.released_at_unix_micros",
                    params![
                        hold.id.get().to_string(),
                        scope_kind,
                        scope_id,
                        hold.placed_at.unix_micros(),
                        hold.released_at.map(StoreTimestamp::unix_micros),
                    ],
                )?;
                Ok(())
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    pub fn remove_legal_hold(&mut self, id: LegalHoldId) -> Result<(), ServiceError> {
        self.delete_inventory_row("deletion_legal_holds", "hold_id", &id.get().to_string())
    }

    pub fn put_artifact_reference(
        &mut self,
        reference: ArtifactReference,
    ) -> Result<(), ServiceError> {
        self.projections
            .with_store_time(|transaction, _| {
                transaction.execute(
                    "INSERT INTO deletion_artifact_references
                     (reference_id, artifact_id, principal_id, project_id, expires_at_unix_micros)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(reference_id) DO UPDATE SET artifact_id = excluded.artifact_id,
                       principal_id = excluded.principal_id, project_id = excluded.project_id,
                       expires_at_unix_micros = excluded.expires_at_unix_micros",
                    params![
                        reference.id.get().to_string(),
                        reference.artifact_id.to_string(),
                        reference.principal_id.to_string(),
                        reference.project_id.to_string(),
                        expiration_value(reference.expires_at),
                    ],
                )?;
                Ok(())
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    pub fn remove_artifact_reference(
        &mut self,
        id: ArtifactReferenceId,
    ) -> Result<(), ServiceError> {
        self.delete_inventory_row(
            "deletion_artifact_references",
            "reference_id",
            &id.get().to_string(),
        )
    }

    pub fn put_backup_generation(
        &mut self,
        generation: &BackupGeneration,
    ) -> Result<(), ServiceError> {
        self.projections
            .with_store_time(|transaction, _| {
                let id = generation.id.get().to_string();
                transaction.execute(
                    "INSERT INTO deletion_backup_generations
                     (generation_id, created_at_unix_micros, expires_at_unix_micros)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(generation_id) DO UPDATE SET
                       created_at_unix_micros = excluded.created_at_unix_micros,
                       expires_at_unix_micros = excluded.expires_at_unix_micros",
                    params![
                        id,
                        generation.created_at.unix_micros(),
                        expiration_value(generation.expires_at)
                    ],
                )?;
                transaction.execute(
                    "DELETE FROM deletion_backup_contents WHERE generation_id = ?1",
                    [&id],
                )?;
                for object in &generation.contents {
                    transaction.execute(
                        "INSERT INTO deletion_backup_contents (generation_id, object_key)
                         VALUES (?1, ?2)",
                        params![id, object_key(*object)],
                    )?;
                }
                Ok(())
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    pub fn register_backup_generation(
        &mut self,
        generation: &StoredBackupGeneration,
    ) -> Result<(), ServiceError> {
        let contents = stored_backup_contents(generation)?;
        self.projections
            .with_store_time(|transaction, _| {
                put_stored_backup_generation(transaction, generation, &contents)?;
                transaction.execute(
                    "DELETE FROM deletion_backup_generations
                     WHERE expires_at_unix_micros IS NOT NULL
                       AND expires_at_unix_micros <= ?1",
                    [generation.created_at_unix_micros],
                )?;
                Ok(())
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    pub(crate) fn unregister_backup_generation(
        &mut self,
        generation: &str,
    ) -> Result<(), ServiceError> {
        self.projections
            .with_store_time(|transaction, _| {
                transaction.execute(
                    "DELETE FROM deletion_backup_generations WHERE generation_id = ?1",
                    [generation],
                )?;
                Ok(())
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    pub fn reconcile_backup_generations(
        &mut self,
        generations: &[StoredBackupGeneration],
        now_unix_micros: i64,
    ) -> Result<usize, ServiceError> {
        let generations = generations
            .iter()
            .filter(|generation| generation.expires_at_unix_micros > now_unix_micros)
            .map(|generation| {
                stored_backup_contents(generation).map(|contents| (generation, contents))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.projections
            .with_store_time(|transaction, _| {
                let expired = transaction.execute(
                    "DELETE FROM deletion_backup_generations
                     WHERE expires_at_unix_micros IS NOT NULL
                       AND expires_at_unix_micros <= ?1",
                    [now_unix_micros],
                )?;
                for (generation, contents) in &generations {
                    put_stored_backup_generation(transaction, generation, contents)?;
                }
                Ok(expired)
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    pub fn expire_backup_generations(
        &mut self,
        now_unix_micros: i64,
    ) -> Result<usize, ServiceError> {
        self.projections
            .with_store_time(|transaction, _| {
                transaction
                    .execute(
                        "DELETE FROM deletion_backup_generations
                         WHERE expires_at_unix_micros IS NOT NULL
                           AND expires_at_unix_micros <= ?1",
                        [now_unix_micros],
                    )
                    .map_err(Into::into)
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }

    pub fn remove_backup_generation(&mut self, id: BackupGenerationId) -> Result<(), ServiceError> {
        self.delete_inventory_row(
            "deletion_backup_generations",
            "generation_id",
            &id.get().to_string(),
        )
    }

    fn delete_inventory_row(
        &mut self,
        table: &str,
        column: &str,
        value: &str,
    ) -> Result<(), ServiceError> {
        let sql = format!("DELETE FROM {table} WHERE {column} = ?1");
        self.projections
            .with_store_time(|transaction, _| {
                transaction.execute(&sql, [value])?;
                Ok(())
            })
            .map_err(|error| ServiceError::Store(error.to_string()))
    }
}

fn erase_retained_object(
    transaction: &Transaction<'_>,
    evaluation: &JobEvaluation,
    now: i64,
) -> Result<(), crate::store::sqlite::projection::ProjectionError> {
    let key = object_key(evaluation.object.id);
    let target_digest = sha256(key.as_bytes());
    let kind = match evaluation.object.id {
        RetentionObjectId::Event(_) => "event",
        RetentionObjectId::Transcript(_) => "transcript",
        RetentionObjectId::Terminal(_) => "terminal",
        RetentionObjectId::Artifact(_) => "artifact",
        RetentionObjectId::Experiment(_) => "experiment",
        RetentionObjectId::Backup(_) => "backup",
    };
    let erased_events = match evaluation.object.id {
        RetentionObjectId::Transcript(thread_id) => {
            crate::store::sqlite::mcp_callback::scrub_callback_transcript(
                transaction,
                &thread_id.to_string(),
                now,
            )
            .map_err(|error| {
                crate::store::sqlite::projection::ProjectionError::Reducer(error.to_string())
            })?;
            transaction.execute(
                "DELETE FROM deletion_artifact_references
                 WHERE reference_id IN (
                   SELECT 'run:' || run_id FROM event_projection_index
                   WHERE thread_id = ?1 AND run_id IS NOT NULL
                 )",
                [thread_id.to_string()],
            )?;
            let erased = transaction.execute(
                "UPDATE events SET event_type = 'retention.erased',
                   payload = X'7B22657261736564223A747275652C22736368656D615F76657273696F6E223A317D',
                   artifacts = X'5B5D'
                 WHERE commit_position IN (
                   SELECT commit_position FROM event_projection_index WHERE thread_id = ?1
                 )",
                [thread_id.to_string()],
            )?;
            scrub_domain_projection(transaction, |state| state.erase_transcript(thread_id))?;
            transaction.execute(
                "UPDATE event_projection_index SET erased = 1, thread_id = NULL, run_id = NULL
                 WHERE thread_id = ?1",
                [thread_id.to_string()],
            )?;
            erased
        }
        RetentionObjectId::Artifact(artifact_id) => {
            transaction.execute(
                "DELETE FROM deletion_artifact_references WHERE artifact_id = ?1",
                [artifact_id.to_string()],
            )?;
            scrub_domain_projection(transaction, |state| state.erase_artifact(artifact_id))?;
            0
        }
        RetentionObjectId::Event(event_id) => {
            let erased = transaction.execute(
                "UPDATE events SET event_type = 'retention.erased',
                   payload = X'7B22657261736564223A747275652C22736368656D615F76657273696F6E223A317D',
                   artifacts = X'5B5D'
                 WHERE event_id = ?1",
                [event_id.to_string()],
            )?;
            transaction.execute(
                "UPDATE event_projection_index SET erased = 1, thread_id = NULL, run_id = NULL
                 WHERE commit_position = (SELECT commit_position FROM events WHERE event_id = ?1)",
                [event_id.to_string()],
            )?;
            erased
        }
        _ => 0,
    };
    transaction.execute(
        "INSERT INTO deletion_tombstones
         (target_sha256, object_kind, completed_at_unix_micros, erased_event_count, outcome)
         VALUES (?1, ?2, ?3, ?4, 'erased')
         ON CONFLICT(target_sha256) DO NOTHING",
        params![target_digest.as_slice(), kind, now, erased_events],
    )?;
    Ok(())
}

fn scrub_domain_projection(
    transaction: &Transaction<'_>,
    scrub: impl FnOnce(&mut DomainReducer),
) -> Result<(), crate::store::sqlite::projection::ProjectionError> {
    let bytes = transaction
        .query_row(
            "SELECT canonical_bytes FROM projection_state WHERE name = ?1",
            [DomainReducer::NAME],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    let Some(bytes) = bytes else {
        return Ok(());
    };
    let mut state = DomainReducer::from_canonical_bytes(&bytes).map_err(|error| {
        crate::store::sqlite::projection::ProjectionError::Reducer(error.to_string())
    })?;
    scrub(&mut state);
    let bytes = state.canonical_bytes().map_err(|error| {
        crate::store::sqlite::projection::ProjectionError::Reducer(error.to_string())
    })?;
    let digest = sha256(&bytes);
    transaction.execute(
        "UPDATE projection_state SET canonical_bytes = ?1, digest = ?2 WHERE name = ?3",
        params![bytes, digest.as_slice(), DomainReducer::NAME],
    )?;
    Ok(())
}

impl Command {
    fn resource(&self) -> Resource {
        match self {
            Self::CreateProject { project_id, .. }
            | Self::SetProjectRetention { project_id, .. } => Resource::Project(*project_id),
            Self::CreateThread { project_id, .. }
            | Self::RegisterArtifactMetadata { project_id, .. } => Resource::Project(*project_id),
            Self::SetThreadArchived { thread_id, .. }
            | Self::InitiateThreadDeletion { thread_id, .. } => Resource::Thread(*thread_id),
            Self::StartRun { thread_id, .. } => Resource::Thread(*thread_id),
            Self::TransitionRun { run_id, .. }
            | Self::CancelRun { run_id, .. }
            | Self::ProvideRunInput { run_id, .. }
            | Self::StartAttempt { run_id, .. }
            | Self::RequestApproval { run_id, .. } => Resource::Run(*run_id),
            Self::TransitionAttempt { attempt_id, .. } => Resource::Attempt(*attempt_id),
            Self::ResolveApproval { approval_id, .. } => Resource::Approval(*approval_id),
            Self::RequestAuth { run_id, .. } | Self::ResolveAuth { run_id, .. } => {
                Resource::AuthRequest(*run_id)
            }
            Self::ResolveMcpCallback { callback_id, .. } => Resource::McpCallback(*callback_id),
        }
    }

    fn target(&self) -> EntityId {
        match self {
            Self::CreateProject { project_id, .. }
            | Self::SetProjectRetention { project_id, .. } => EntityId::Project(*project_id),
            Self::CreateThread { thread_id, .. }
            | Self::SetThreadArchived { thread_id, .. }
            | Self::InitiateThreadDeletion { thread_id, .. } => EntityId::Thread(*thread_id),
            Self::StartRun { run_id, .. }
            | Self::TransitionRun { run_id, .. }
            | Self::CancelRun { run_id, .. }
            | Self::ProvideRunInput { run_id, .. }
            | Self::RequestAuth { run_id, .. }
            | Self::ResolveAuth { run_id, .. } => EntityId::Run(*run_id),
            Self::StartAttempt { attempt_id, .. } | Self::TransitionAttempt { attempt_id, .. } => {
                EntityId::Attempt(*attempt_id)
            }
            Self::RequestApproval { approval_id, .. }
            | Self::ResolveApproval { approval_id, .. } => EntityId::Approval(*approval_id),
            Self::RegisterArtifactMetadata { artifact_id, .. } => EntityId::Artifact(*artifact_id),
            Self::ResolveMcpCallback { callback_id, .. } => EntityId::McpCallback(*callback_id),
        }
    }

    fn correlation(&self) -> EntityId {
        match self {
            Self::CreateProject { project_id, .. }
            | Self::SetProjectRetention { project_id, .. }
            | Self::CreateThread { project_id, .. }
            | Self::RegisterArtifactMetadata { project_id, .. } => EntityId::Project(*project_id),
            Self::SetThreadArchived { thread_id, .. }
            | Self::InitiateThreadDeletion { thread_id, .. }
            | Self::StartRun { thread_id, .. } => EntityId::Thread(*thread_id),
            Self::TransitionRun { run_id, .. }
            | Self::CancelRun { run_id, .. }
            | Self::ProvideRunInput { run_id, .. }
            | Self::StartAttempt { run_id, .. }
            | Self::RequestApproval { run_id, .. }
            | Self::RequestAuth { run_id, .. }
            | Self::ResolveAuth { run_id, .. } => EntityId::Run(*run_id),
            Self::TransitionAttempt { attempt_id, .. } => EntityId::Attempt(*attempt_id),
            Self::ResolveApproval { approval_id, .. } => EntityId::Approval(*approval_id),
            Self::ResolveMcpCallback { callback_id, .. } => EntityId::McpCallback(*callback_id),
        }
    }

    fn attempt_id(&self) -> Option<crate::domain::ids::AttemptId> {
        match self {
            Self::StartAttempt { attempt_id, .. } | Self::TransitionAttempt { attempt_id, .. } => {
                Some(*attempt_id)
            }
            _ => None,
        }
    }

    fn secondary_stream(&self) -> Option<(EntityId, u64)> {
        match self {
            Self::StartAttempt {
                run_id,
                expected_version,
                ..
            } => Some((EntityId::Run(*run_id), *expected_version)),
            Self::TransitionRun {
                replacement_owner: Some(owner),
                ..
            } => Some((EntityId::Attempt(owner.attempt_id), 0)),
            _ => None,
        }
    }
}

impl Query {
    fn resource(&self) -> Resource {
        match self {
            Self::GetProject { project_id }
            | Self::GetProjectRetention { project_id }
            | Self::ListThreads { project_id }
            | Self::ListRuns { project_id }
            | Self::PendingApprovals { project_id }
            | Self::PendingAuthRequests { project_id }
            | Self::PendingMcpCallbacks { project_id }
            | Self::ListCapabilities { project_id }
            | Self::EventCursorStatus { project_id, .. }
            | Self::Status { project_id } => Resource::Project(*project_id),
            Self::GetThread { thread_id } | Self::ThreadEvents { thread_id, .. } => {
                Resource::Thread(*thread_id)
            }
            Self::GetDeletionJob { .. } => {
                unreachable!("deletion job scope is resolved by the deletion service")
            }
            Self::GetRun { run_id }
            | Self::GetRunCost { run_id }
            | Self::GetRunPrompts { run_id }
            | Self::RunTranscript { run_id }
            | Self::RunTimeline { run_id, .. } => Resource::Run(*run_id),
            Self::GetAttempt { attempt_id } => Resource::Attempt(*attempt_id),
            Self::GetArtifactMetadata { artifact_id } => Resource::Artifact(*artifact_id),
            Self::GetMcpCallback { callback_id } => Resource::McpCallback(*callback_id),
        }
    }
}

fn mcp_callback_error(error: crate::domain::mcp_callback::McpCallbackError) -> ServiceError {
    use crate::domain::mcp_callback::McpCallbackError;
    match error {
        McpCallbackError::NotFound => ServiceError::NotFound,
        McpCallbackError::Invalid(message) => ServiceError::Invalid(message.to_owned()),
        McpCallbackError::Store(message) => ServiceError::Store(message),
        McpCallbackError::VersionConflict { .. }
        | McpCallbackError::IllegalTransition { .. }
        | McpCallbackError::Terminal(_)
        | McpCallbackError::IdempotencyConflict
        | McpCallbackError::Authority
        | McpCallbackError::Expired => ServiceError::Conflict(error.to_string()),
    }
}

fn mcp_callback_request_digest(command: &Command) -> Result<[u8; 32], ServiceError> {
    let bytes =
        serde_json::to_vec(command).map_err(|error| ServiceError::Invalid(error.to_string()))?;
    let digest = Digest::of(DigestAlgorithm::Sha256, &bytes);
    let mut request_digest = [0_u8; 32];
    request_digest.copy_from_slice(&digest.as_bytes());
    Ok(request_digest)
}

fn store_timestamp(time: &StoreTime) -> Result<UtcDateTime, ServiceError> {
    UtcDateTime::parse(time.as_rfc3339()).map_err(|error| ServiceError::Store(error.to_string()))
}

fn lease_micros(duration: std::time::Duration) -> Result<i64, ServiceError> {
    if duration.is_zero() {
        return Err(ServiceError::Invalid(
            "driver lease duration must be positive".to_owned(),
        ));
    }
    i64::try_from(duration.as_micros())
        .map_err(|_| ServiceError::Invalid("driver lease duration is too large".to_owned()))
}

fn driver_now(transaction: &Transaction<'_>) -> Result<i64, ServiceError> {
    transaction
        .query_row(
            "SELECT CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite_service_error)
}

fn rebind_record(
    mut record: EffectJournalRecord,
    claim: AttemptDriverClaim,
) -> EffectJournalRecord {
    let correlation = match &mut record {
        EffectJournalRecord::EffectIntent(value) => Some(&mut value.correlation),
        EffectJournalRecord::EffectDispatched(value) => Some(&mut value.correlation),
        EffectJournalRecord::EffectOutcome(value) => Some(&mut value.correlation),
        EffectJournalRecord::ToolApprovalRequested(value)
        | EffectJournalRecord::ToolApprovalRestored(value) => Some(&mut value.correlation),
        EffectJournalRecord::Boundary(_)
        | EffectJournalRecord::Waiting(_)
        | EffectJournalRecord::WaitingResolved(_)
        | EffectJournalRecord::CancellationRequested => None,
    };
    if let Some(correlation) = correlation {
        correlation.run_id = claim.run_id;
        correlation.owner = claim.owner();
        correlation.claim = claim;
    }
    record
}

fn recovery_records(
    recovery: RecoveryState,
    old_records: Vec<EffectJournalRecord>,
) -> Result<Vec<EffectJournalRecord>, ServiceError> {
    match recovery {
        RecoveryState::Ready(mut plan) => {
            let Some(mut pending) = plan.approved_tool.take() else {
                return Ok(vec![EffectJournalRecord::Boundary(plan.snapshot)]);
            };
            promote_approved_tool_ids(&mut plan.snapshot, &mut pending)?;
            Ok(vec![
                EffectJournalRecord::Boundary(plan.snapshot),
                EffectJournalRecord::ToolApprovalRestored(pending),
            ])
        }
        RecoveryState::Cancelled(snapshot) => Ok(vec![
            EffectJournalRecord::Boundary(snapshot),
            EffectJournalRecord::CancellationRequested,
        ]),
        RecoveryState::OutcomeUnknown(_) => Ok(old_records),
        RecoveryState::Waiting(_) => Err(ServiceError::Conflict(
            "unresolved waiting attempt is not recoverable".to_owned(),
        )),
    }
}

fn promote_approved_tool_ids(
    snapshot: &mut crate::agent::driver::restart::BoundarySnapshot,
    pending: &mut crate::agent::driver::restart::PendingToolApproval,
) -> Result<(), ServiceError> {
    const IDS: [(&str, &str); 5] = [
        ("kit.invocation_id", "kit.approved_invocation_id"),
        ("kit.command_id", "kit.approved_command_id"),
        ("kit.intent_event_id", "kit.approved_intent_event_id"),
        ("kit.outcome_event_id", "kit.approved_outcome_event_id"),
        ("kit.idempotency_key", "kit.approved_idempotency_key"),
    ];
    let replacements = IDS
        .map(|(target, source)| {
            pending
                .approval
                .metadata
                .get(source)
                .cloned()
                .map(|value| (target, value))
                .ok_or_else(|| {
                    ServiceError::Store("approved tool metadata is incomplete".to_owned())
                })
        })
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    for (name, value) in &replacements {
        pending
            .request
            .metadata
            .insert((*name).to_owned(), value.clone());
    }
    let update = |items: &mut [crate::agent::agentkit_bridge::mapping::CanonicalItem]| {
        for item in items {
            for part in &mut item.parts {
                if let crate::agent::agentkit_bridge::mapping::CanonicalPart::ToolCall {
                    id,
                    metadata,
                    ..
                } = part
                    && id == &pending.request.call_id.to_string()
                {
                    for (name, value) in &replacements {
                        metadata.insert((*name).to_owned(), value.clone());
                    }
                }
            }
        }
    };
    update(&mut snapshot.transcript);
    if let Some(outcome) = &mut snapshot.model_outcome {
        update(&mut outcome.output_items);
    }
    Ok(())
}

fn sqlite_service_error(error: rusqlite::Error) -> ServiceError {
    ServiceError::Store(error.to_string())
}

fn store_error(error: StoreError) -> ServiceError {
    match error {
        StoreError::ExpectedVersion { .. }
        | StoreError::IdempotencyConflict(_)
        | StoreError::IdempotencyPending(_) => ServiceError::Conflict(error.to_string()),
        _ => ServiceError::Store(error.to_string()),
    }
}

fn default_retention_policy() -> super::RetentionPolicy {
    super::RetentionPolicy::FOREVER
}

#[derive(Clone, Copy)]
enum FinishOutcome {
    Completed,
    Waiting,
    Failed,
    Stale,
}

struct JobEvaluation {
    object: RetainedObject,
    policy: RetentionPolicy,
    decision: crate::domain::retention::PhysicalDeletionDecision,
    artifact_digest: Option<String>,
}

fn evaluate_job(
    transaction: &Transaction<'_>,
    job_id: &str,
    now: i64,
) -> Result<JobEvaluation, crate::store::sqlite::projection::ProjectionError> {
    let object_key: String = transaction.query_row(
        "SELECT object_key FROM deletion_jobs WHERE job_id = ?1",
        [job_id],
        |row| row.get(0),
    )?;
    let (principal, project, stored_at, artifact_digest, policy): (
        String,
        String,
        i64,
        Option<String>,
        Vec<u8>,
    ) = transaction.query_row(
        "SELECT principal_id, project_id, stored_at_unix_micros, artifact_digest, policy_json
         FROM deletion_objects WHERE object_key = ?1 AND physically_deleted = 0",
        [&object_key],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    let object = RetainedObject::new(
        parse_object_key(&object_key).map_err(|error| {
            crate::store::sqlite::projection::ProjectionError::Reducer(error.to_string())
        })?,
        PrincipalId::parse(&principal).map_err(|error| {
            crate::store::sqlite::projection::ProjectionError::Reducer(error.to_string())
        })?,
        ProjectId::parse(&project).map_err(|error| {
            crate::store::sqlite::projection::ProjectionError::Reducer(error.to_string())
        })?,
        StoreTimestamp::from_unix_micros(stored_at),
    );
    let policy: RetentionPolicy = serde_json::from_slice(&policy).map_err(|error| {
        crate::store::sqlite::projection::ProjectionError::Reducer(error.to_string())
    })?;

    let mut hold_statement = transaction.prepare(
        "SELECT hold_id, scope_kind, scope_id, placed_at_unix_micros, released_at_unix_micros
         FROM deletion_legal_holds",
    )?;
    let holds = hold_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })?
        .map(|row| {
            let (id, kind, scope, placed, released) = row?;
            let scope = match kind.as_str() {
                "principal" => PrincipalId::parse(&scope)
                    .map(LegalHoldScope::Principal)
                    .map_err(|error| error.to_string()),
                "project" => ProjectId::parse(&scope)
                    .map(LegalHoldScope::Project)
                    .map_err(|error| error.to_string()),
                "object" => parse_object_key(&scope).map(LegalHoldScope::Object),
                _ => Err("invalid legal hold scope".to_owned()),
            }
            .map_err(crate::store::sqlite::projection::ProjectionError::Reducer)?;
            Ok(LegalHold {
                id: LegalHoldId::new(id.parse::<u128>().map_err(|error| {
                    crate::store::sqlite::projection::ProjectionError::Reducer(error.to_string())
                })?),
                scope,
                placed_at: StoreTimestamp::from_unix_micros(placed),
                released_at: released.map(StoreTimestamp::from_unix_micros),
            })
        })
        .collect::<Result<Vec<_>, crate::store::sqlite::projection::ProjectionError>>()?;

    let mut references = Vec::new();
    if let RetentionObjectId::Artifact(artifact_id) = object.id {
        let mut statement = transaction.prepare(
            "SELECT reference_id, principal_id, project_id, expires_at_unix_micros
             FROM deletion_artifact_references WHERE artifact_id = ?1",
        )?;
        references = statement
            .query_map([artifact_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })?
            .map(|row| {
                let (id, principal, project, expires) = row?;
                Ok(ArtifactReference {
                    id: ArtifactReferenceId::new(numeric_inventory_id(&id)),
                    artifact_id,
                    principal_id: PrincipalId::parse(&principal).map_err(|error| {
                        crate::store::sqlite::projection::ProjectionError::Reducer(
                            error.to_string(),
                        )
                    })?,
                    project_id: ProjectId::parse(&project).map_err(|error| {
                        crate::store::sqlite::projection::ProjectionError::Reducer(
                            error.to_string(),
                        )
                    })?,
                    expires_at: expires
                        .map(StoreTimestamp::from_unix_micros)
                        .map(Expiration::At)
                        .unwrap_or(Expiration::Never),
                })
            })
            .collect::<Result<Vec<_>, crate::store::sqlite::projection::ProjectionError>>()?;
    }

    let mut backup_statement = transaction.prepare(
        "SELECT generation.generation_id, generation.created_at_unix_micros,
                generation.expires_at_unix_micros
         FROM deletion_backup_generations AS generation
         JOIN deletion_backup_contents AS content
           ON content.generation_id = generation.generation_id
         WHERE content.object_key = ?1",
    )?;
    let backups = backup_statement
        .query_map([&object_key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?
        .map(|row| {
            let (id, created, expires) = row?;
            Ok(BackupGeneration {
                id: BackupGenerationId::new(numeric_inventory_id(&id)),
                created_at: StoreTimestamp::from_unix_micros(created),
                expires_at: expires
                    .map(StoreTimestamp::from_unix_micros)
                    .map(Expiration::At)
                    .unwrap_or(Expiration::Never),
                contents: [object.id].into_iter().collect(),
            })
        })
        .collect::<Result<Vec<_>, crate::store::sqlite::projection::ProjectionError>>()?;

    let decision = evaluate_physical_deletion_at(
        StoreTimestamp::from_unix_micros(now),
        &object,
        RetentionIntent::Delete,
        policy,
        &holds,
        &references,
        &backups,
    );
    Ok(JobEvaluation {
        object,
        policy,
        decision,
        artifact_digest,
    })
}

fn append_audit(
    transaction: &Transaction<'_>,
    job_id: &str,
    state: &str,
    at: i64,
) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "INSERT INTO deletion_job_audit (job_id, sequence, state, at_unix_micros)
         SELECT ?1, COALESCE(max(sequence), 0) + 1, ?2, ?3
         FROM deletion_job_audit WHERE job_id = ?1",
        params![job_id, state, at],
    )?;
    Ok(())
}

fn numeric_inventory_id(value: &str) -> u128 {
    value.parse().unwrap_or_else(|_| {
        let digest = blake3::hash(value.as_bytes());
        u128::from_be_bytes(digest.as_bytes()[..16].try_into().expect("digest prefix"))
    })
}

fn stored_backup_contents(
    generation: &StoredBackupGeneration,
) -> Result<Vec<String>, ServiceError> {
    let database = generation.path.join("store.sqlite3");
    let connection = Connection::open_with_flags(
        database,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| ServiceError::Store(error.to_string()))?;
    let mut statement = connection
        .prepare(
            "SELECT object_key FROM deletion_objects
             WHERE physically_deleted = 0 ORDER BY object_key",
        )
        .map_err(|error| ServiceError::Store(error.to_string()))?;
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| ServiceError::Store(error.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ServiceError::Store(error.to_string()))
}

fn put_stored_backup_generation(
    transaction: &Transaction<'_>,
    generation: &StoredBackupGeneration,
    contents: &[String],
) -> Result<(), crate::store::sqlite::projection::ProjectionError> {
    transaction.execute(
        "INSERT INTO deletion_backup_generations
         (generation_id, created_at_unix_micros, expires_at_unix_micros)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(generation_id) DO UPDATE SET
           created_at_unix_micros = excluded.created_at_unix_micros,
           expires_at_unix_micros = excluded.expires_at_unix_micros",
        params![
            generation.name,
            generation.created_at_unix_micros,
            generation.expires_at_unix_micros,
        ],
    )?;
    transaction.execute(
        "DELETE FROM deletion_backup_contents WHERE generation_id = ?1",
        [&generation.name],
    )?;
    for object in contents {
        transaction.execute(
            "INSERT INTO deletion_backup_contents (generation_id, object_key) VALUES (?1, ?2)",
            params![generation.name, object],
        )?;
    }
    Ok(())
}

fn earliest_value(value: EarliestPhysicalDeletion) -> Option<i64> {
    match value {
        EarliestPhysicalDeletion::At(at) => Some(at.unix_micros()),
        EarliestPhysicalDeletion::Never => None,
    }
}

fn public_blocker(blocker: &DeletionBlocker) -> Option<&'static str> {
    match blocker {
        DeletionBlocker::ArchiveIntent => None,
        DeletionBlocker::Retention(_) => Some("retention_policy"),
        DeletionBlocker::LegalHold(_) => Some("legal_hold"),
        DeletionBlocker::ArtifactReference(_) => Some("active_reference"),
        DeletionBlocker::BackupGeneration(_) => Some("backup_generation"),
    }
}

fn object_key(id: RetentionObjectId) -> String {
    match id {
        RetentionObjectId::Event(id) => format!("event:{id}"),
        RetentionObjectId::Transcript(id) => format!("transcript:{id}"),
        RetentionObjectId::Terminal(id) => format!("terminal:{id}"),
        RetentionObjectId::Artifact(id) => format!("artifact:{id}"),
        RetentionObjectId::Experiment(id) => format!("experiment:{id}"),
        RetentionObjectId::Backup(id) => format!("backup:{}", id.get()),
    }
}

fn parse_object_key(value: &str) -> Result<RetentionObjectId, String> {
    let (kind, id) = value
        .split_once(':')
        .ok_or_else(|| "invalid deletion object key".to_owned())?;
    match kind {
        "event" => crate::domain::ids::EventId::parse(id)
            .map(RetentionObjectId::Event)
            .map_err(|error| error.to_string()),
        "transcript" => ThreadId::parse(id)
            .map(RetentionObjectId::Transcript)
            .map_err(|error| error.to_string()),
        "terminal" => crate::domain::ids::TerminalId::parse(id)
            .map(RetentionObjectId::Terminal)
            .map_err(|error| error.to_string()),
        "artifact" => crate::domain::ids::ArtifactId::parse(id)
            .map(RetentionObjectId::Artifact)
            .map_err(|error| error.to_string()),
        "experiment" => crate::domain::ids::ExperimentId::parse(id)
            .map(RetentionObjectId::Experiment)
            .map_err(|error| error.to_string()),
        "backup" => id
            .parse::<u128>()
            .map(BackupGenerationId::new)
            .map(RetentionObjectId::Backup)
            .map_err(|error| error.to_string()),
        _ => Err("invalid deletion object kind".to_owned()),
    }
}

fn initial_earliest(intent: &DeletionIntent) -> Option<i64> {
    let object = RetainedObject::new(
        intent.object_id(),
        intent.principal_id,
        intent.project_id,
        StoreTimestamp::from_unix_micros(intent.requested_at_unix_micros),
    );
    match evaluate_physical_deletion_at(
        StoreTimestamp::from_unix_micros(intent.requested_at_unix_micros),
        &object,
        RetentionIntent::Delete,
        intent.policy,
        &[],
        &[],
        &[],
    )
    .earliest
    {
        EarliestPhysicalDeletion::At(at) => Some(at.unix_micros()),
        EarliestPhysicalDeletion::Never => None,
    }
}

fn read_job(
    transaction: &Transaction<'_>,
    job_id: &str,
    actor: DeletionActor,
) -> Result<Option<DeletionJob>, String> {
    type JobRow = (
        String,
        String,
        String,
        String,
        String,
        i64,
        i64,
        Vec<u8>,
        Option<i64>,
        i64,
        Vec<u8>,
        i64,
        Option<i64>,
        Option<String>,
    );
    let row: Option<JobRow> = transaction
        .query_row(
            "SELECT job_id, principal_id, project_id, object_key, state, version,
                    resource_version, policy_json, earliest_physical_unix_micros, fence,
                    blockers_json, requested_at_unix_micros, completed_at_unix_micros, failure
             FROM deletion_jobs
             WHERE job_id = ?1 AND principal_id = ?2 AND project_id = ?3",
            params![
                job_id,
                actor.principal_id.to_string(),
                actor.project_id.to_string()
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get_ref(10)?.as_bytes()?.to_vec(),
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some((
        id,
        principal,
        project,
        object,
        state,
        version,
        resource_version,
        policy,
        earliest,
        fence,
        blockers,
        requested_at,
        completed_at,
        failure,
    )) = row
    else {
        return Ok(None);
    };
    let policy: RetentionPolicy =
        serde_json::from_slice(&policy).map_err(|error| error.to_string())?;
    let blocker_names: Vec<String> =
        serde_json::from_slice(&blockers).map_err(|error| error.to_string())?;
    let blockers = blocker_names
        .iter()
        .map(|value| {
            PublicDeletionBlocker::parse(value).ok_or_else(|| "invalid deletion blocker".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut statement = transaction
        .prepare(
            "SELECT sequence, state, at_unix_micros FROM deletion_job_audit
             WHERE job_id = ?1 ORDER BY sequence",
        )
        .map_err(|error| error.to_string())?;
    let audit = statement
        .query_map([&id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .map(|row| {
            let (sequence, state, at) = row.map_err(|error| error.to_string())?;
            Ok(DeletionAuditEntry {
                sequence: u64::try_from(sequence).map_err(|error| error.to_string())?,
                state: DeletionJobState::parse(&state)
                    .ok_or_else(|| "invalid deletion job audit state".to_owned())?,
                at: StoreTimestamp::from_unix_micros(at),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let earliest_physical_deletion = earliest
        .map(StoreTimestamp::from_unix_micros)
        .map(EarliestPhysicalDeletion::At)
        .unwrap_or(EarliestPhysicalDeletion::Never);
    Ok(Some(DeletionJob {
        id: id
            .parse()
            .map_err(|error: crate::domain::deletion::DeletionJobIdParseError| error.to_string())?,
        object_id: parse_object_key(&object)?,
        state: DeletionJobState::parse(&state)
            .ok_or_else(|| "invalid deletion job state".to_owned())?,
        version: u64::try_from(version).map_err(|error| error.to_string())?,
        actor: DeletionActor::new(
            PrincipalId::parse(&principal).map_err(|error| error.to_string())?,
            ProjectId::parse(&project).map_err(|error| error.to_string())?,
        ),
        resource_version: u64::try_from(resource_version).map_err(|error| error.to_string())?,
        effective_retention: EffectiveRetention {
            policy,
            earliest_physical_deletion,
        },
        blockers,
        fence: FencingToken::new(u64::try_from(fence).map_err(|error| error.to_string())?),
        requested_at: StoreTimestamp::from_unix_micros(requested_at),
        completed_at: completed_at.map(StoreTimestamp::from_unix_micros),
        failure,
        audit,
    }))
}

fn expiration_value(expiration: Expiration) -> Option<i64> {
    match expiration {
        Expiration::At(at) => Some(at.unix_micros()),
        Expiration::Never => None,
    }
}

fn legal_hold_scope(scope: LegalHoldScope) -> (&'static str, String) {
    match scope {
        LegalHoldScope::Principal(id) => ("principal", id.to_string()),
        LegalHoldScope::Project(id) => ("project", id.to_string()),
        LegalHoldScope::Object(id) => ("object", object_key(id)),
    }
}

#[cfg(test)]
mod executor_recovery_tests {
    use super::*;
    use crate::{
        domain::{
            ids::{ProcessId, WorkspaceId},
            lifecycle::{ProcessClaim, ProcessOwnership},
        },
        executor::{
            cancel::{CancellationIntent, WorkspaceIdentity},
            process::tree::{BoundaryIdentity, BoundaryKind, Ownership, PersistedBoundary},
        },
    };
    use std::{
        sync::{Arc, Barrier},
        time::Duration,
    };

    #[test]
    fn stale_registration_racing_successor_takeover_cannot_leave_a_live_old_boundary() {
        let path = std::env::temp_dir().join(format!(
            "kit-executor-takeover-{}-{}",
            std::process::id(),
            CommandId::generate().unwrap()
        ));
        let authority = crate::runtime::daemon::ControlPlaneAuthority::for_test();
        let store = SqliteServiceStore::open(&path, &authority).unwrap();
        let run_id = RunId::generate().unwrap();
        let principal_id = PrincipalId::generate().unwrap();
        let old = store
            .acquire_driver_claim(
                run_id,
                AttemptId::generate().unwrap(),
                principal_id,
                Duration::from_secs(60),
            )
            .unwrap()
            .unwrap();
        let coordinator = SqliteCancellationCoordinator::new(&path);
        coordinator.register_no_process(old.owner()).unwrap();
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE attempt_driver_claims SET expires_at_unix_micros=0 WHERE run_id=?1",
                [run_id.to_string()],
            )
            .unwrap();
        let process = ProcessClaim::new(
            ProcessId::generate().unwrap(),
            ProcessOwnership::Attempt(old.owner()),
        );
        let intent = CancellationIntent::new(
            CommandId::generate().unwrap(),
            old.owner(),
            process,
            PersistedBoundary {
                ownership: Ownership::new(
                    serde_json::to_string(&process.owner).unwrap(),
                    process.process_id.to_string(),
                )
                .unwrap(),
                identity: BoundaryIdentity::new(
                    BoundaryKind::Container,
                    "old-boundary",
                    "a".repeat(64),
                    "old-start",
                )
                .unwrap(),
            },
            WorkspaceIdentity::new(WorkspaceId::generate().unwrap(), "acquisition", "revision")
                .unwrap(),
            Duration::from_millis(1),
        )
        .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let old_barrier = Arc::clone(&barrier);
        let old_path = path.clone();
        let registration = std::thread::spawn(move || {
            old_barrier.wait();
            SqliteCancellationCoordinator::new(old_path).register_claim(&intent)
        });

        barrier.wait();
        let successor = store
            .acquire_driver_claim(
                run_id,
                AttemptId::generate().unwrap(),
                principal_id,
                Duration::from_secs(60),
            )
            .unwrap();
        let registration = registration.join().unwrap();

        assert!(successor.is_some());
        assert!(matches!(
            registration,
            Err(crate::executor::cancel::CancellationError::Store(
                crate::executor::cancel::CancellationStoreError::Unauthorized
            ))
        ));
        let connection = Connection::open(&path).unwrap();
        let old_live: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM executor_attempt_boundaries
                 WHERE attempt_id=?1 AND state IN ('active', 'outcome_unknown')",
                [old.attempt_id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_live, 0);
        drop(connection);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}
