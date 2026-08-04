use std::{
    fmt,
    path::Path,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::{
    config::RunConfigSnapshot,
    ids::{PrincipalId, RunId},
    lifecycle::AttemptOwnership,
};

use super::{
    budget::{Exhaustion, RunBudget},
    limits::Spend,
    reserve::{BudgetTotals, ReservationId, ReservationSnapshot, ReservationStatus},
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionKind {
    Run,
    Model,
    Callback,
    Tool,
    Process,
}

impl AdmissionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Model => "model",
            Self::Callback => "callback",
            Self::Tool => "tool",
            Self::Process => "process",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchState {
    Undispatched,
    Dispatched,
    Canceled,
    Unknown,
}

impl DispatchState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Undispatched => "undispatched",
            Self::Dispatched => "dispatched",
            Self::Canceled => "canceled",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SchedulerConfig {
    pub run_budget: RunBudget,
    pub queue_capacity: usize,
    pub global_concurrency: usize,
    pub principal_concurrency: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            run_budget: RunBudget::new(100_000_000, 1_000_000, 1_000, 10_000, 1_000),
            queue_capacity: 1_024,
            global_concurrency: 64,
            principal_concurrency: 8,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReservationRequest {
    pub id: ReservationId,
    pub run_id: RunId,
    pub principal_id: PrincipalId,
    pub attempt: Option<AttemptOwnership>,
    pub idempotency_key: String,
    pub kind: AdmissionKind,
    pub spend: Spend,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrialRunBinding {
    pub trial_id: String,
    pub trial_digest: String,
    pub task_digest: String,
    pub model_digest: String,
    pub config_digest: String,
    pub attempt: AttemptOwnership,
    pub admission: Option<TrialAdmissionToken>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrialAdmissionToken {
    pub authority_id: String,
    pub authority_position: u64,
    pub registration_sequence: u64,
    pub preregistration_digest: String,
    pub schedule_index: usize,
    pub trial_id: String,
    pub pair_id: String,
    pub task_id: String,
    pub dataset_member_id: String,
    pub task_manifest_digest: String,
    pub seed: u64,
    pub arm: String,
    pub nonce: String,
    pub token_digest: String,
    pub authentication: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingStatisticalTrial {
    pub run_id: RunId,
    pub admission_token_digest: String,
    pub admission_nonce: String,
    pub admission_position: u64,
    pub consumption_position: u64,
    pub consumption_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AnchoredConsumptionReceipt {
    pub authority_id: String,
    pub scheduler_run_id: String,
    pub admission_token_digest: String,
    pub admission_nonce: String,
    pub scheduler_consumption_position: u64,
    pub scheduler_consumption_digest: String,
    pub ledger_position: u64,
    pub ledger_head_digest: String,
    pub anchor_source: String,
    pub anchor_identity: String,
    pub anchor_counter: u64,
    pub anchor_signature: String,
    pub authentication_algorithm: String,
    pub authentication_key_id: String,
    pub authentication_tag: String,
}

pub trait TrialAdmissionVerifier {
    fn verify(&self, token: &TrialAdmissionToken) -> bool;
}

pub trait AnchoredConsumptionVerifier {
    fn verify(
        &self,
        pending: &PendingStatisticalTrial,
        receipt: &AnchoredConsumptionReceipt,
    ) -> bool;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReconciliationReport {
    pub released: usize,
    pub dispatched_unknown: usize,
    pub orphan_runs: usize,
}

#[derive(Debug)]
pub enum SchedulerError {
    Database(rusqlite::Error),
    InvalidConfig,
    ShuttingDown,
    QueueFull { capacity: usize },
    NotQueueHead,
    PrincipalCap { limit: usize },
    GlobalCap { limit: usize },
    UnknownRun,
    UnknownReservation,
    Conflict,
    AnchorPending,
    StaleFence,
    InvalidTransition,
    Exhausted(Exhaustion),
    BudgetBreach(Exhaustion),
    ActualOverage,
    BudgetBlocked,
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "scheduler SQLite error: {error}"),
            Self::InvalidConfig => {
                formatter.write_str("scheduler limits must be finite and non-zero")
            }
            Self::ShuttingDown => formatter.write_str("scheduler admission is shutting down"),
            Self::QueueFull { capacity } => {
                write!(formatter, "scheduler queue is full ({capacity})")
            }
            Self::NotQueueHead => formatter.write_str("run is not at the head of the FIFO queue"),
            Self::PrincipalCap { limit } => {
                write!(formatter, "principal concurrency cap reached ({limit})")
            }
            Self::GlobalCap { limit } => {
                write!(formatter, "global concurrency cap reached ({limit})")
            }
            Self::UnknownRun => formatter.write_str("scheduler run is unknown"),
            Self::UnknownReservation => formatter.write_str("scheduler reservation is unknown"),
            Self::Conflict => formatter.write_str("scheduler idempotency conflict"),
            Self::AnchorPending => {
                formatter.write_str("statistical trial admission is pending external anchoring")
            }
            Self::StaleFence => formatter.write_str("scheduler attempt fence is stale"),
            Self::InvalidTransition => {
                formatter.write_str("invalid scheduler reservation transition")
            }
            Self::Exhausted(exhaustion) => write!(
                formatter,
                "{} budget exhausted: maximum {}, debited {}, reserved {}, requested {}",
                exhaustion.resource,
                exhaustion.maximum,
                exhaustion.committed,
                exhaustion.reserved,
                exhaustion.requested
            ),
            Self::BudgetBreach(exhaustion) => write!(
                formatter,
                "{} budget breached: maximum {}, committed {}, reserved {}, actual {}",
                exhaustion.resource,
                exhaustion.maximum,
                exhaustion.committed,
                exhaustion.reserved,
                exhaustion.requested
            ),
            Self::BudgetBlocked => {
                formatter.write_str("run budget is blocked by unreconciled overage")
            }
            Self::ActualOverage => {
                formatter.write_str("provider actual usage exceeded its reservation")
            }
        }
    }
}

impl std::error::Error for SchedulerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for SchedulerError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

struct Inner {
    connection: Mutex<Connection>,
    config: SchedulerConfig,
    accepting: AtomicBool,
}

#[derive(Clone)]
pub struct DurableScheduler {
    inner: Arc<Inner>,
}

impl DurableScheduler {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SchedulerError> {
        Self::open_with_config(path, SchedulerConfig::default())
    }

    pub fn open_with_config(
        path: impl AsRef<Path>,
        config: SchedulerConfig,
    ) -> Result<Self, SchedulerError> {
        if config.queue_capacity == 0
            || config.global_concurrency == 0
            || config.principal_concurrency == 0
            || config.run_budget.limits() == Spend::ZERO
        {
            return Err(SchedulerError::InvalidConfig);
        }
        checked_spend(config.run_budget.limits())?;
        let mut connection = Connection::open(path)?;
        connection.busy_timeout(BUSY_TIMEOUT)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        migrate(&mut connection)?;
        Ok(Self {
            inner: Arc::new(Inner {
                connection: Mutex::new(connection),
                config,
                accepting: AtomicBool::new(true),
            }),
        })
    }

    pub fn register_run(
        &self,
        run_id: RunId,
        principal_id: PrincipalId,
        idempotency_key: &str,
    ) -> Result<(), SchedulerError> {
        self.register_run_with_budget(
            run_id,
            principal_id,
            idempotency_key,
            self.inner.config.run_budget,
            "legacy",
        )
    }

    pub fn register_run_with_snapshot(
        &self,
        run_id: RunId,
        principal_id: PrincipalId,
        idempotency_key: &str,
        snapshot: &RunConfigSnapshot,
    ) -> Result<(), SchedulerError> {
        self.register_run_with_budget(
            run_id,
            principal_id,
            idempotency_key,
            RunBudget::from_effective_config(snapshot.effective()),
            &snapshot.digest_hex(),
        )
    }

    pub fn register_statistical_trial_run(
        &self,
        run_id: RunId,
        principal_id: PrincipalId,
        idempotency_key: &str,
        config_digest: &str,
    ) -> Result<(), SchedulerError> {
        if config_digest.is_empty() {
            return Err(SchedulerError::Conflict);
        }
        self.register_run_with_budget(
            run_id,
            principal_id,
            idempotency_key,
            self.inner.config.run_budget,
            config_digest,
        )
    }

    fn register_run_with_budget(
        &self,
        run_id: RunId,
        principal_id: PrincipalId,
        idempotency_key: &str,
        budget: RunBudget,
        config_digest: &str,
    ) -> Result<(), SchedulerError> {
        self.ensure_accepting()?;
        if idempotency_key.is_empty() {
            return Err(SchedulerError::Conflict);
        }
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        if let Some((stored_principal, stored_key, limits, stored_digest)) =
            run_record(&transaction, run_id)?
        {
            if stored_principal == principal_id.to_string()
                && stored_key == idempotency_key
                && limits == budget.limits()
                && stored_digest == config_digest
            {
                transaction.commit()?;
                return Ok(());
            }
            return Err(SchedulerError::Conflict);
        }
        let queued: i64 = transaction.query_row(
            "SELECT count(*) FROM scheduler_runs WHERE phase = 'queued'",
            [],
            |row| row.get(0),
        )?;
        if usize::try_from(queued).unwrap_or(usize::MAX) >= self.inner.config.queue_capacity {
            return Err(SchedulerError::QueueFull {
                capacity: self.inner.config.queue_capacity,
            });
        }
        let position: i64 = transaction.query_row(
            "SELECT COALESCE(max(queue_position), 0) + 1 FROM scheduler_runs",
            [],
            |row| row.get(0),
        )?;
        let limits = checked_spend(budget.limits())?;
        let now = store_time(&transaction)?;
        transaction.execute(
            "INSERT INTO scheduler_runs (
                run_id, principal_id, idempotency_key, cost_limit, token_limit, turn_limit,
                tool_limit, process_limit, config_digest, queue_position, phase, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'queued', ?11, ?11)",
            params![
                run_id.to_string(),
                principal_id.to_string(),
                idempotency_key,
                limits[0],
                limits[1],
                limits[2],
                limits[3],
                limits[4],
                config_digest,
                position,
                now
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn run_budget(&self, run_id: RunId) -> Result<RunBudget, SchedulerError> {
        reservation_run(&self.lock(), run_id)?
            .map(|run| run.budget)
            .ok_or(SchedulerError::UnknownRun)
    }

    pub fn admit_run(&self, run_id: RunId) -> Result<(), SchedulerError> {
        self.ensure_accepting()?;
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        admit_run_transaction(&transaction, run_id, self.inner.config)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn admit_trial_run(
        &self,
        run_id: RunId,
        binding: &TrialRunBinding,
    ) -> Result<(), SchedulerError> {
        if binding.admission.is_some() {
            return Err(SchedulerError::Conflict);
        }
        self.admit_verified_trial_run(run_id, binding)
    }

    pub fn admit_statistical_trial_run(
        &self,
        run_id: RunId,
        binding: &TrialRunBinding,
        verifier: &impl TrialAdmissionVerifier,
    ) -> Result<PendingStatisticalTrial, SchedulerError> {
        let admission = binding.admission.as_ref().ok_or(SchedulerError::Conflict)?;
        if admission.trial_id != binding.trial_id || !verifier.verify(admission) {
            return Err(SchedulerError::Conflict);
        }
        self.consume_statistical_trial_admission(run_id, binding)
    }

    fn consume_statistical_trial_admission(
        &self,
        run_id: RunId,
        binding: &TrialRunBinding,
    ) -> Result<PendingStatisticalTrial, SchedulerError> {
        self.ensure_accepting()?;
        validate_trial_binding(binding)?;
        let admission = binding.admission.as_ref().ok_or(SchedulerError::Conflict)?;
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let (principal, idempotency_key, config_digest, phase) =
            run_admission_record(&transaction, run_id)?;
        if principal != binding.attempt.principal_id.to_string()
            || normalize_digest(&config_digest) != normalize_digest(&binding.config_digest)
        {
            return Err(SchedulerError::Conflict);
        }
        if let Some(existing) = statistical_consumption(&transaction, run_id)? {
            if existing.admission_token_digest == admission.token_digest
                && existing.admission_nonce == admission.nonce
                && existing.admission_position == admission.authority_position
                && trial_binding_matches(
                    &transaction,
                    run_id,
                    binding,
                    &principal,
                    &idempotency_key,
                )?
            {
                transaction.commit()?;
                return Ok(existing);
            }
            return Err(SchedulerError::Conflict);
        }
        if phase != "queued" {
            return Err(SchedulerError::InvalidTransition);
        }
        let consumed_elsewhere: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM consumed_trial_admissions
             WHERE token_digest = ?1 OR authority_position = ?2 OR nonce = ?3)",
            params![
                admission.token_digest,
                to_i64(admission.authority_position)?,
                admission.nonce
            ],
            |row| row.get(0),
        )?;
        if consumed_elsewhere {
            return Err(SchedulerError::Conflict);
        }
        check_run_admission(&transaction, run_id, &principal, self.inner.config)?;
        let effects: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM scheduler_reservations WHERE run_id = ?1
                 UNION ALL SELECT 1 FROM events WHERE correlation_id = ?1
             )",
            [run_id.to_string()],
            |row| row.get(0),
        )?;
        if effects {
            return Err(SchedulerError::InvalidTransition);
        }
        let admission_token =
            serde_json::to_string(admission).map_err(|_| SchedulerError::Conflict)?;
        let now = store_time(&transaction)?;
        insert_trial_binding(
            &transaction,
            run_id,
            binding,
            &principal,
            &idempotency_key,
            Some(&admission_token),
            now,
        )?;
        let (previous_position, previous_digest) = transaction
            .query_row(
                "SELECT position, entry_digest FROM statistical_admission_ledger ORDER BY position DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .unwrap_or((0, "statistical-admission-genesis-v1".to_owned()));
        let consumption_position = previous_position
            .checked_add(1)
            .ok_or(SchedulerError::InvalidConfig)?;
        let payload = serde_json::to_vec(&serde_json::json!({
            "event_type": "AdmissionConsumed",
            "run_id": run_id,
            "authority_id": admission.authority_id,
            "authority_position": admission.authority_position,
            "registration_sequence": admission.registration_sequence,
            "preregistration_digest": admission.preregistration_digest,
            "schedule_index": admission.schedule_index,
            "trial_id": admission.trial_id,
            "nonce": admission.nonce,
            "token_digest": admission.token_digest,
        }))
        .map_err(|_| SchedulerError::Conflict)?;
        let payload_digest = sha256(&payload);
        let consumption_digest = sha256(
            &serde_json::to_vec(&serde_json::json!({
                "domain": "kit-scheduler-statistical-admission-ledger-v1",
                "position": consumption_position,
                "previous_digest": previous_digest,
                "payload_digest": payload_digest,
            }))
            .map_err(|_| SchedulerError::Conflict)?,
        );
        transaction.execute(
            "INSERT INTO statistical_admission_ledger
                 (position, previous_digest, entry_digest, run_id, token_digest, payload_digest, payload_bytes, anchor_state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending_anchor', ?8)",
            params![
                consumption_position,
                previous_digest,
                consumption_digest,
                run_id.to_string(),
                admission.token_digest,
                payload_digest,
                payload,
                now
            ],
        )?;
        transaction
            .execute(
                "INSERT INTO consumed_trial_admissions
                     (token_digest, run_id, authority_position, nonce, consumption_position, consumption_digest, anchor_state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending_anchor')",
                params![
                    admission.token_digest,
                    run_id.to_string(),
                    to_i64(admission.authority_position)?,
                    admission.nonce,
                    consumption_position,
                    consumption_digest,
                ],
            )
            .map_err(|_| SchedulerError::Conflict)?;
        transaction.execute(
            "UPDATE scheduler_runs SET attempt_id = ?2, attempt_fence = ?3, updated_at = ?4 WHERE run_id = ?1",
            params![
                run_id.to_string(),
                binding.attempt.attempt_id.to_string(),
                to_i64(binding.attempt.fencing_token.get())?,
                now,
            ],
        )?;
        transaction.commit()?;
        Ok(PendingStatisticalTrial {
            run_id,
            admission_token_digest: admission.token_digest.clone(),
            admission_nonce: admission.nonce.clone(),
            admission_position: admission.authority_position,
            consumption_position,
            consumption_digest,
        })
    }

    pub fn finalize_statistical_trial_anchor(
        &self,
        pending: &PendingStatisticalTrial,
        receipt: &AnchoredConsumptionReceipt,
        verifier: &impl AnchoredConsumptionVerifier,
    ) -> Result<(), SchedulerError> {
        if receipt.scheduler_run_id != pending.run_id.to_string()
            || receipt.admission_token_digest != pending.admission_token_digest
            || receipt.admission_nonce != pending.admission_nonce
            || receipt.scheduler_consumption_position != pending.consumption_position
            || receipt.scheduler_consumption_digest != pending.consumption_digest
            || !verifier.verify(pending, receipt)
        {
            return Err(SchedulerError::Conflict);
        }
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let stored = statistical_consumption(&transaction, pending.run_id)?
            .ok_or(SchedulerError::UnknownRun)?;
        if &stored != pending {
            return Err(SchedulerError::Conflict);
        }
        let state: String = transaction.query_row(
            "SELECT anchor_state FROM consumed_trial_admissions WHERE run_id = ?1",
            [pending.run_id.to_string()],
            |row| row.get(0),
        )?;
        if state == "anchored" {
            transaction.commit()?;
            return Ok(());
        }
        let (principal, _, _, phase) = run_admission_record(&transaction, pending.run_id)?;
        if phase != "queued" {
            return Err(SchedulerError::InvalidTransition);
        }
        check_run_admission(&transaction, pending.run_id, &principal, self.inner.config)?;
        let now = store_time(&transaction)?;
        transaction.execute(
            "UPDATE consumed_trial_admissions SET anchor_state = 'anchored' WHERE run_id = ?1 AND anchor_state = 'pending_anchor'",
            [pending.run_id.to_string()],
        )?;
        transaction.execute(
            "UPDATE statistical_admission_ledger SET anchor_state = 'anchored' WHERE run_id = ?1 AND entry_digest = ?2 AND anchor_state = 'pending_anchor'",
            params![pending.run_id.to_string(), pending.consumption_digest],
        )?;
        transaction.execute(
            "UPDATE scheduler_runs SET phase = 'admitted', updated_at = ?2 WHERE run_id = ?1 AND phase = 'queued'",
            params![pending.run_id.to_string(), now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn admit_verified_trial_run(
        &self,
        run_id: RunId,
        binding: &TrialRunBinding,
    ) -> Result<(), SchedulerError> {
        self.ensure_accepting()?;
        validate_trial_binding(binding)?;
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let admission_token = binding
            .admission
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| SchedulerError::Conflict)?;
        let (principal, idempotency_key, config_digest) =
            admit_run_transaction(&transaction, run_id, self.inner.config)?;
        if principal != binding.attempt.principal_id.to_string()
            || normalize_digest(&config_digest) != normalize_digest(&binding.config_digest)
        {
            return Err(SchedulerError::Conflict);
        }
        let existing = transaction
            .query_row(
                "SELECT trial_id, trial_digest, task_digest, model_digest, config_digest,
                            attempt_id, attempt_fence, scheduler_principal_id,
                            scheduler_idempotency_key, admission_token
                     FROM run_to_trial WHERE run_id = ?1",
                [run_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, Option<String>>(9)?,
                    ))
                },
            )
            .optional()?;
        let expected = (
            binding.trial_id.clone(),
            binding.trial_digest.clone(),
            binding.task_digest.clone(),
            binding.model_digest.clone(),
            binding.config_digest.clone(),
            binding.attempt.attempt_id.to_string(),
            to_i64(binding.attempt.fencing_token.get())?,
            principal.clone(),
            idempotency_key.clone(),
            admission_token.clone(),
        );
        if let Some(existing) = existing {
            if existing != expected {
                return Err(SchedulerError::Conflict);
            }
            transaction.commit()?;
            return Ok(());
        }
        let effects: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM scheduler_reservations WHERE run_id = ?1
                 UNION ALL SELECT 1 FROM events WHERE correlation_id = ?1
             )",
            [run_id.to_string()],
            |row| row.get(0),
        )?;
        if effects {
            return Err(SchedulerError::InvalidTransition);
        }
        if let Some(admission) = &binding.admission {
            transaction
                .execute(
                    "INSERT INTO consumed_trial_admissions (token_digest, run_id, authority_position, nonce) VALUES (?1, ?2, ?3, ?4)",
                    params![admission.token_digest, run_id.to_string(), to_i64(admission.authority_position)?, admission.nonce],
                )
                .map_err(|_| SchedulerError::Conflict)?;
        }
        let now = store_time(&transaction)?;
        transaction.execute(
            "INSERT INTO run_to_trial (
                 run_id, trial_id, trial_digest, task_digest, model_digest, config_digest,
                 attempt_id, attempt_fence, scheduler_principal_id,
                 scheduler_idempotency_key, admission_token, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                run_id.to_string(),
                binding.trial_id,
                binding.trial_digest,
                binding.task_digest,
                binding.model_digest,
                binding.config_digest,
                binding.attempt.attempt_id.to_string(),
                to_i64(binding.attempt.fencing_token.get())?,
                principal,
                idempotency_key,
                admission_token,
                now,
            ],
        )?;
        transaction.execute(
            "UPDATE scheduler_runs SET attempt_id = ?2, attempt_fence = ?3 WHERE run_id = ?1",
            params![
                run_id.to_string(),
                binding.attempt.attempt_id.to_string(),
                to_i64(binding.attempt.fencing_token.get())?,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn reserve(
        &self,
        request: &ReservationRequest,
    ) -> Result<ReservationSnapshot, SchedulerError> {
        self.ensure_accepting()?;
        validate_category(request.kind, request.spend)?;
        let requested = checked_spend(request.spend)?;
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let Some(run) = reservation_run(&transaction, request.run_id)? else {
            return Err(SchedulerError::UnknownRun);
        };
        let anchor_pending: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM consumed_trial_admissions WHERE run_id = ?1 AND anchor_state = 'pending_anchor')",
            [request.run_id.to_string()],
            |row| row.get(0),
        )?;
        if anchor_pending {
            return Err(SchedulerError::AnchorPending);
        }
        if run.principal != request.principal_id.to_string() {
            return Err(SchedulerError::Conflict);
        }
        if request.kind != AdmissionKind::Run && run.phase != "admitted" {
            return Err(SchedulerError::InvalidTransition);
        }
        if let Some(existing) = reservation_by_id(&transaction, request.id)? {
            if existing.run_id == request.run_id
                && existing.idempotency_key == request.idempotency_key
                && existing.kind == request.kind
                && existing.snapshot.spend() == request.spend
                && existing.attempt == request.attempt
            {
                transaction.commit()?;
                return Ok(existing.snapshot);
            }
            return Err(SchedulerError::Conflict);
        }
        if run.budget_breached {
            return Err(SchedulerError::BudgetBlocked);
        }
        let duplicate: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM scheduler_reservations WHERE run_id = ?1 AND idempotency_key = ?2)",
            params![request.run_id.to_string(), request.idempotency_key],
            |row| row.get(0),
        )?;
        if duplicate {
            return Err(SchedulerError::Conflict);
        }
        if let Some(owner) = request.attempt {
            if owner.principal_id != request.principal_id
                || owner.fencing_token.get() < run.fence
                || (owner.fencing_token.get() == run.fence
                    && run
                        .attempt_id
                        .as_deref()
                        .is_some_and(|id| id != owner.attempt_id.to_string()))
            {
                return Err(SchedulerError::StaleFence);
            }
            if owner.fencing_token.get() > run.fence || run.attempt_id.is_none() {
                transaction.execute(
                    "UPDATE scheduler_runs SET attempt_id = ?2, attempt_fence = ?3 WHERE run_id = ?1",
                    params![request.run_id.to_string(), owner.attempt_id.to_string(), to_i64(owner.fencing_token.get())?],
                )?;
            }
        }
        let totals = totals_tx(&transaction, request.run_id)?;
        run.budget
            .check(totals.committed, totals.reserved, request.spend)
            .map_err(SchedulerError::Exhausted)?;
        let now = store_time(&transaction)?;
        let (attempt, attempt_fence) = match request.attempt {
            Some(owner) => (
                Some(owner.attempt_id.to_string()),
                Some(to_i64(owner.fencing_token.get())?),
            ),
            None => (None, None),
        };
        transaction.execute(
            "INSERT INTO scheduler_reservations (
                reservation_id, run_id, principal_id, attempt_id, attempt_fence,
                idempotency_key, kind, cost, tokens, turns, tools, processes,
                state, dispatch_state, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                       'reserved', 'undispatched', ?13, ?13)",
            params![
                reservation_key(request.id),
                request.run_id.to_string(),
                request.principal_id.to_string(),
                attempt,
                attempt_fence,
                request.idempotency_key,
                request.kind.as_str(),
                requested[0],
                requested[1],
                requested[2],
                requested[3],
                requested[4],
                now
            ],
        )?;
        transaction.commit()?;
        Ok(ReservationSnapshot::new(
            request.id,
            request.spend,
            ReservationStatus::Reserved,
        ))
    }

    pub fn mark_dispatched(&self, id: ReservationId) -> Result<(), SchedulerError> {
        self.set_dispatch(id, DispatchState::Dispatched)
    }

    pub fn cancel(&self, id: ReservationId) -> Result<ReservationSnapshot, SchedulerError> {
        self.set_dispatch(id, DispatchState::Canceled)?;
        self.release(id)
    }

    pub fn debit(&self, id: ReservationId) -> Result<ReservationSnapshot, SchedulerError> {
        self.transition(id, ReservationStatus::Debited)
    }

    pub fn release(&self, id: ReservationId) -> Result<ReservationSnapshot, SchedulerError> {
        self.transition(id, ReservationStatus::Released)
    }

    pub fn snapshot(&self, id: ReservationId) -> Result<ReservationSnapshot, SchedulerError> {
        let connection = self.lock();
        reservation_by_id(&connection, id)?
            .map(|record| record.snapshot)
            .ok_or(SchedulerError::UnknownReservation)
    }

    pub fn reconcile(
        &self,
        id: ReservationId,
        actual: Spend,
    ) -> Result<ReservationSnapshot, SchedulerError> {
        let actual_values = checked_spend(actual)?;
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let record =
            reservation_by_id(&transaction, id)?.ok_or(SchedulerError::UnknownReservation)?;
        if matches!(
            record.snapshot.status(),
            ReservationStatus::Reconciled | ReservationStatus::ActualOverage
        ) {
            if record.snapshot.spend() == actual {
                transaction.commit()?;
                return if record.snapshot.status() == ReservationStatus::ActualOverage {
                    Err(SchedulerError::ActualOverage)
                } else {
                    Ok(record.snapshot)
                };
            }
            return Err(SchedulerError::Conflict);
        }
        if record.snapshot.status() != ReservationStatus::Debited {
            return Err(SchedulerError::InvalidTransition);
        }
        let run =
            reservation_run(&transaction, record.run_id)?.ok_or(SchedulerError::UnknownRun)?;
        let totals = totals_tx(&transaction, record.run_id)?;
        let committed = totals
            .committed
            .checked_sub(record.snapshot.spend())
            .ok_or(SchedulerError::Conflict)?;
        let breach = run.budget.check(committed, totals.reserved, actual).err();
        let reservation_overage = Spend::new(
            actual
                .cost_microusd()
                .saturating_sub(record.snapshot.spend().cost_microusd()),
            actual
                .tokens()
                .saturating_sub(record.snapshot.spend().tokens()),
            actual
                .turns()
                .saturating_sub(record.snapshot.spend().turns()),
            actual
                .tools()
                .saturating_sub(record.snapshot.spend().tools()),
            actual
                .processes()
                .saturating_sub(record.snapshot.spend().processes()),
        );
        let now = store_time(&transaction)?;
        let terminal_status = if reservation_overage == Spend::ZERO {
            ReservationStatus::Reconciled
        } else {
            ReservationStatus::ActualOverage
        };
        transaction.execute(
            "UPDATE scheduler_reservations
             SET cost = ?2, tokens = ?3, turns = ?4, tools = ?5, processes = ?6,
                  state = ?7, updated_at = ?8
             WHERE reservation_id = ?1",
            params![
                reservation_key(id),
                actual_values[0],
                actual_values[1],
                actual_values[2],
                actual_values[3],
                actual_values[4],
                status_name(terminal_status),
                now,
            ],
        )?;
        if reservation_overage != Spend::ZERO {
            transaction.execute(
                "UPDATE scheduler_runs
                 SET budget_breached = 1,
                     cost_overage = max(cost_overage, ?2),
                     token_overage = max(token_overage, ?3),
                     turn_overage = max(turn_overage, ?4), updated_at = ?5
                 WHERE run_id = ?1",
                params![
                    record.run_id.to_string(),
                    to_i64(reservation_overage.cost_microusd())?,
                    to_i64(reservation_overage.tokens())?,
                    to_i64(reservation_overage.turns())?,
                    now,
                ],
            )?;
        }
        refresh_budget_breach(&transaction, record.run_id, now)?;
        transaction.commit()?;
        if reservation_overage != Spend::ZERO {
            return Err(SchedulerError::ActualOverage);
        }
        if let Some(exhaustion) = breach {
            return Err(SchedulerError::BudgetBreach(exhaustion));
        }
        Ok(ReservationSnapshot::new(
            id,
            actual,
            ReservationStatus::Reconciled,
        ))
    }

    pub fn totals(&self, run_id: RunId) -> Result<BudgetTotals, SchedulerError> {
        totals_tx(&self.lock(), run_id)
    }

    pub fn finish_run(&self, run_id: RunId, canceled: bool) -> Result<(), SchedulerError> {
        self.finish_run_inner(run_id, canceled, false).map(drop)
    }

    pub fn finish_run_with_event_watermark(
        &self,
        run_id: RunId,
        canceled: bool,
    ) -> Result<u64, SchedulerError> {
        self.finish_run_inner(run_id, canceled, true)?
            .ok_or(SchedulerError::InvalidTransition)
    }

    fn finish_run_inner(
        &self,
        run_id: RunId,
        canceled: bool,
        capture_event_watermark: bool,
    ) -> Result<Option<u64>, SchedulerError> {
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let now = store_time(&transaction)?;
        let event_watermark = capture_event_watermark
            .then(|| {
                transaction.query_row(
                    "SELECT position FROM commit_watermark WHERE singleton = 1",
                    [],
                    |row| row.get::<_, u64>(0),
                )
            })
            .transpose()?;
        if canceled {
            transaction.execute(
                "UPDATE scheduler_reservations SET dispatch_state = 'canceled', state = 'released', updated_at = ?2
                 WHERE run_id = ?1 AND state = 'reserved'",
                params![run_id.to_string(), now],
            )?;
        } else {
            transaction.execute(
                "UPDATE scheduler_reservations SET state = 'released', updated_at = ?2
                 WHERE run_id = ?1 AND state = 'reserved' AND dispatch_state = 'undispatched'",
                params![run_id.to_string(), now],
            )?;
        }
        let changed = transaction.execute(
            "UPDATE scheduler_runs
             SET phase = ?2, executor_quiescent = 1,
                 terminal_version = terminal_version + 1, updated_at = ?3,
                 terminal_event_watermark = COALESCE(?4, terminal_event_watermark)
             WHERE run_id = ?1 AND phase = 'admitted'",
            params![
                run_id.to_string(),
                if canceled { "canceled" } else { "terminal" },
                now,
                event_watermark,
            ],
        )?;
        if changed == 0 {
            let state: Option<(String, Option<u64>)> = transaction
                .query_row(
                    "SELECT phase, terminal_event_watermark FROM scheduler_runs WHERE run_id = ?1",
                    [run_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if state.as_ref().map(|state| state.0.as_str())
                == Some(if canceled { "canceled" } else { "terminal" })
                && (!capture_event_watermark
                    || state.as_ref().and_then(|state| state.1) == event_watermark)
            {
                transaction.commit()?;
                return Ok(state.and_then(|state| state.1));
            }
            return Err(if state.is_some() {
                SchedulerError::InvalidTransition
            } else {
                SchedulerError::UnknownRun
            });
        }
        transaction.commit()?;
        Ok(event_watermark)
    }

    pub fn cancel_reservations(&self, run_id: RunId) -> Result<(), SchedulerError> {
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let now = store_time(&transaction)?;
        transaction.execute(
            "UPDATE scheduler_reservations SET dispatch_state = 'canceled', state = 'released', updated_at = ?2
             WHERE run_id = ?1 AND state = 'reserved' AND dispatch_state = 'undispatched'",
            params![run_id.to_string(), now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn requeue_run(&self, run_id: RunId) -> Result<(), SchedulerError> {
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let now = store_time(&transaction)?;
        transaction.execute(
            "UPDATE scheduler_reservations SET state = 'released', updated_at = ?2
             WHERE run_id = ?1 AND state = 'reserved' AND dispatch_state = 'undispatched'",
            params![run_id.to_string(), now],
        )?;
        let position: i64 = transaction.query_row(
            "SELECT COALESCE(max(queue_position), 0) + 1 FROM scheduler_runs",
            [],
            |row| row.get(0),
        )?;
        let changed = transaction.execute(
            "UPDATE scheduler_runs SET phase = 'queued', executor_quiescent = 0,
                    queue_position = ?2, updated_at = ?3 WHERE run_id = ?1",
            params![run_id.to_string(), position, now],
        )?;
        if changed == 0 {
            return Err(SchedulerError::UnknownRun);
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn rollback_run_admission(&self, run_id: RunId) -> Result<(), SchedulerError> {
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let committed: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM events WHERE stream = ?1 AND event_type = 'run.start')",
            [run_id.to_string()],
            |row| row.get(0),
        )?;
        if !committed {
            transaction.execute(
                "DELETE FROM scheduler_runs WHERE run_id = ?1",
                [run_id.to_string()],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn rollback_dispatch(&self, run_id: RunId) -> Result<(), SchedulerError> {
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let now = store_time(&transaction)?;
        transaction.execute(
            "UPDATE scheduler_runs SET phase = 'queued', executor_quiescent = 0,
                    updated_at = ?2 WHERE run_id = ?1 AND phase = 'admitted'",
            params![run_id.to_string(), now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn reconcile_startup(&self) -> Result<ReconciliationReport, SchedulerError> {
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let orphan_runs = remove_orphan_runs(&transaction)?;
        let now = store_time(&transaction)?;
        let released = transaction.execute(
            "UPDATE scheduler_reservations SET state = 'released', updated_at = ?1
             WHERE state = 'reserved' AND dispatch_state IN ('undispatched', 'canceled')",
            [now],
        )?;
        transaction.execute(
            "UPDATE scheduler_reservations AS reservation
             SET cost = CAST((SELECT json_extract(CAST(event.payload AS TEXT), '$.settlement.cost_microusd')
                              FROM events AS event
                              WHERE event.event_type = 'model_call.outcome'
                                AND json_extract(CAST(event.payload AS TEXT), '$.reservation_id') = reservation.reservation_id
                                AND json_extract(CAST(event.payload AS TEXT), '$.charged') = 1
                                AND json_type(CAST(event.payload AS TEXT), '$.settlement') = 'object'
                              ORDER BY event.commit_position DESC LIMIT 1) AS INTEGER),
                 tokens = CAST((SELECT json_extract(CAST(event.payload AS TEXT), '$.settlement.tokens')
                                FROM events AS event
                                WHERE event.event_type = 'model_call.outcome'
                                  AND json_extract(CAST(event.payload AS TEXT), '$.reservation_id') = reservation.reservation_id
                                  AND json_extract(CAST(event.payload AS TEXT), '$.charged') = 1
                                  AND json_type(CAST(event.payload AS TEXT), '$.settlement') = 'object'
                                ORDER BY event.commit_position DESC LIMIT 1) AS INTEGER),
                 turns = 1, tools = 0, processes = 0,
                 state = CASE
                   WHEN json_extract(CAST((SELECT event.payload FROM events AS event
                     WHERE event.event_type = 'model_call.outcome'
                       AND json_extract(CAST(event.payload AS TEXT), '$.reservation_id') = reservation.reservation_id
                     ORDER BY event.commit_position DESC LIMIT 1) AS TEXT), '$.policy_violation')
                     LIKE '%provider_%_overage%'
                   THEN 'actual_overage' ELSE 'reconciled' END,
                 updated_at = ?1
             WHERE state IN ('reserved', 'debited') AND dispatch_state = 'dispatched'
               AND EXISTS (
                 SELECT 1 FROM events AS event
                 WHERE event.event_type = 'model_call.outcome'
                   AND json_extract(CAST(event.payload AS TEXT), '$.reservation_id') = reservation.reservation_id
                   AND json_extract(CAST(event.payload AS TEXT), '$.charged') = 1
                   AND json_type(CAST(event.payload AS TEXT), '$.settlement') = 'object'
               )",
            [now],
        )?;
        let dispatched_unknown = transaction.execute(
            "UPDATE scheduler_reservations SET state = 'reconciled', dispatch_state = 'unknown', updated_at = ?1
             WHERE state IN ('reserved', 'debited') AND dispatch_state IN ('dispatched', 'unknown')
               AND NOT (kind = 'callback' AND state = 'debited')",
            [now],
        )?;
        let run_ids = transaction
            .prepare("SELECT run_id FROM scheduler_runs")?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for run_id in run_ids {
            refresh_budget_breach(
                &transaction,
                run_id.parse().map_err(|_| SchedulerError::Conflict)?,
                now,
            )?;
        }
        transaction.commit()?;
        Ok(ReconciliationReport {
            released,
            dispatched_unknown,
            orphan_runs,
        })
    }

    pub fn shutdown(&self) -> Result<ReconciliationReport, SchedulerError> {
        self.inner.accepting.store(false, Ordering::Release);
        let report = self.reconcile_startup()?;
        self.lock()
            .query_row("PRAGMA wal_checkpoint(FULL)", [], |_| Ok(()))?;
        Ok(report)
    }

    fn transition(
        &self,
        id: ReservationId,
        target: ReservationStatus,
    ) -> Result<ReservationSnapshot, SchedulerError> {
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let record =
            reservation_by_id(&transaction, id)?.ok_or(SchedulerError::UnknownReservation)?;
        if record.snapshot.status() == target
            || matches!(
                record.snapshot.status(),
                ReservationStatus::Reconciled | ReservationStatus::ActualOverage
            ) && target == ReservationStatus::Debited
        {
            transaction.commit()?;
            return Ok(record.snapshot);
        }
        if record.snapshot.status() != ReservationStatus::Reserved
            || target == ReservationStatus::Released
                && !matches!(
                    record.dispatch,
                    DispatchState::Undispatched | DispatchState::Canceled
                )
        {
            return Err(SchedulerError::InvalidTransition);
        }
        let now = store_time(&transaction)?;
        transaction.execute(
            "UPDATE scheduler_reservations SET state = ?2, updated_at = ?3 WHERE reservation_id = ?1",
            params![reservation_key(id), status_name(target), now],
        )?;
        transaction.commit()?;
        Ok(ReservationSnapshot::new(
            id,
            record.snapshot.spend(),
            target,
        ))
    }

    fn set_dispatch(&self, id: ReservationId, target: DispatchState) -> Result<(), SchedulerError> {
        let mut connection = self.lock();
        let transaction = immediate(&mut connection)?;
        let record =
            reservation_by_id(&transaction, id)?.ok_or(SchedulerError::UnknownReservation)?;
        if record.snapshot.status() != ReservationStatus::Reserved {
            return Err(SchedulerError::InvalidTransition);
        }
        if record.dispatch == target {
            transaction.commit()?;
            return Ok(());
        }
        if record.dispatch != DispatchState::Undispatched {
            return Err(SchedulerError::InvalidTransition);
        }
        let now = store_time(&transaction)?;
        transaction.execute(
            "UPDATE scheduler_reservations SET dispatch_state = ?2, updated_at = ?3 WHERE reservation_id = ?1",
            params![reservation_key(id), target.as_str(), now],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn ensure_accepting(&self) -> Result<(), SchedulerError> {
        if self.inner.accepting.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(SchedulerError::ShuttingDown)
        }
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.inner
            .connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

fn admit_run_transaction(
    transaction: &Transaction<'_>,
    run_id: RunId,
    config: SchedulerConfig,
) -> Result<(String, String, String), SchedulerError> {
    let Some((principal, idempotency_key, config_digest, phase)) = transaction
        .query_row(
            "SELECT principal_id, idempotency_key, config_digest, phase
             FROM scheduler_runs WHERE run_id = ?1",
            [run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?
    else {
        return Err(SchedulerError::UnknownRun);
    };
    if phase == "admitted" {
        return Ok((principal, idempotency_key, config_digest));
    }
    if phase != "queued" {
        return Err(SchedulerError::InvalidTransition);
    }
    let head: String = transaction.query_row(
        "SELECT run_id FROM scheduler_runs WHERE phase = 'queued' ORDER BY queue_position LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    if head != run_id.to_string() {
        return Err(SchedulerError::NotQueueHead);
    }
    let global: i64 = transaction.query_row(
        "SELECT count(*) FROM scheduler_runs WHERE phase = 'admitted'",
        [],
        |row| row.get(0),
    )?;
    if usize::try_from(global).unwrap_or(usize::MAX) >= config.global_concurrency {
        return Err(SchedulerError::GlobalCap {
            limit: config.global_concurrency,
        });
    }
    let principal_active: i64 = transaction.query_row(
        "SELECT count(*) FROM scheduler_runs WHERE phase = 'admitted' AND principal_id = ?1",
        [&principal],
        |row| row.get(0),
    )?;
    if usize::try_from(principal_active).unwrap_or(usize::MAX) >= config.principal_concurrency {
        return Err(SchedulerError::PrincipalCap {
            limit: config.principal_concurrency,
        });
    }
    let now = store_time(transaction)?;
    transaction.execute(
        "UPDATE scheduler_runs SET phase = 'admitted', updated_at = ?2 WHERE run_id = ?1",
        params![run_id.to_string(), now],
    )?;
    Ok((principal, idempotency_key, config_digest))
}

struct StoredReservation {
    snapshot: ReservationSnapshot,
    run_id: RunId,
    idempotency_key: String,
    kind: AdmissionKind,
    attempt: Option<AttemptOwnership>,
    dispatch: DispatchState,
}

struct ReservationRun {
    principal: String,
    budget: RunBudget,
    attempt_id: Option<String>,
    fence: u64,
    phase: String,
    budget_breached: bool,
}

fn migrate(connection: &mut Connection) -> Result<(), SchedulerError> {
    let transaction = immediate(connection)?;
    let store_clock: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'store_clock')",
        [],
        |row| row.get(0),
    )?;
    if !store_clock {
        return Err(SchedulerError::InvalidConfig);
    }
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS scheduler_runs (
             run_id TEXT PRIMARY KEY,
             principal_id TEXT NOT NULL,
             idempotency_key TEXT NOT NULL,
             cost_limit INTEGER NOT NULL CHECK (cost_limit >= 0),
             token_limit INTEGER NOT NULL CHECK (token_limit >= 0),
             turn_limit INTEGER NOT NULL CHECK (turn_limit >= 0),
             tool_limit INTEGER NOT NULL CHECK (tool_limit >= 0),
             process_limit INTEGER NOT NULL CHECK (process_limit >= 0),
             config_digest TEXT NOT NULL DEFAULT 'legacy',
             queue_position INTEGER NOT NULL UNIQUE CHECK (queue_position > 0),
               phase TEXT NOT NULL CHECK (phase IN ('queued', 'admitted', 'terminal', 'canceled')),
               budget_breached INTEGER NOT NULL DEFAULT 0 CHECK (budget_breached IN (0, 1)),
               cost_overage INTEGER NOT NULL DEFAULT 0 CHECK (cost_overage >= 0),
               token_overage INTEGER NOT NULL DEFAULT 0 CHECK (token_overage >= 0),
               turn_overage INTEGER NOT NULL DEFAULT 0 CHECK (turn_overage >= 0),
              attempt_id TEXT,
              attempt_fence INTEGER NOT NULL DEFAULT 0 CHECK (attempt_fence >= 0),
              executor_quiescent INTEGER NOT NULL DEFAULT 0 CHECK (executor_quiescent IN (0, 1)),
              terminal_version INTEGER NOT NULL DEFAULT 0 CHECK (terminal_version >= 0),
              terminal_event_watermark INTEGER CHECK (terminal_event_watermark >= 0),
              created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS scheduler_reservations (
             reservation_id TEXT PRIMARY KEY,
             run_id TEXT NOT NULL REFERENCES scheduler_runs(run_id) ON DELETE CASCADE,
             principal_id TEXT NOT NULL,
             attempt_id TEXT,
             attempt_fence INTEGER,
             idempotency_key TEXT NOT NULL,
             kind TEXT NOT NULL CHECK (kind IN ('run', 'model', 'callback', 'tool', 'process')),
             cost INTEGER NOT NULL CHECK (cost >= 0),
             tokens INTEGER NOT NULL CHECK (tokens >= 0),
             turns INTEGER NOT NULL CHECK (turns >= 0),
             tools INTEGER NOT NULL CHECK (tools >= 0),
             processes INTEGER NOT NULL CHECK (processes >= 0),
             state TEXT NOT NULL CHECK (state IN ('reserved', 'debited', 'released', 'reconciled', 'actual_overage')),
             dispatch_state TEXT NOT NULL CHECK (dispatch_state IN ('undispatched', 'dispatched', 'canceled', 'unknown')),
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             UNIQUE (run_id, idempotency_key)
         );
          CREATE INDEX IF NOT EXISTS scheduler_reservations_run ON scheduler_reservations(run_id, state);",
    )?;
    if !column_exists(&transaction, "scheduler_runs", "config_digest")? {
        transaction.execute(
            "ALTER TABLE scheduler_runs ADD COLUMN config_digest TEXT NOT NULL DEFAULT 'legacy'",
            [],
        )?;
    }
    if !column_exists(&transaction, "scheduler_runs", "executor_quiescent")? {
        transaction.execute(
            "ALTER TABLE scheduler_runs ADD COLUMN executor_quiescent INTEGER NOT NULL DEFAULT 0 CHECK (executor_quiescent IN (0, 1))",
            [],
        )?;
    }
    if !column_exists(&transaction, "scheduler_runs", "terminal_version")? {
        transaction.execute(
            "ALTER TABLE scheduler_runs ADD COLUMN terminal_version INTEGER NOT NULL DEFAULT 0 CHECK (terminal_version >= 0)",
            [],
        )?;
    }
    if !column_exists(&transaction, "scheduler_runs", "terminal_event_watermark")? {
        transaction.execute(
            "ALTER TABLE scheduler_runs ADD COLUMN terminal_event_watermark INTEGER CHECK (terminal_event_watermark >= 0)",
            [],
        )?;
    }
    if !column_exists(&transaction, "scheduler_runs", "budget_breached")? {
        transaction.execute(
            "ALTER TABLE scheduler_runs ADD COLUMN budget_breached INTEGER NOT NULL DEFAULT 0 CHECK (budget_breached IN (0, 1))",
            [],
        )?;
    }
    for (column, definition) in [
        (
            "cost_overage",
            "INTEGER NOT NULL DEFAULT 0 CHECK (cost_overage >= 0)",
        ),
        (
            "token_overage",
            "INTEGER NOT NULL DEFAULT 0 CHECK (token_overage >= 0)",
        ),
        (
            "turn_overage",
            "INTEGER NOT NULL DEFAULT 0 CHECK (turn_overage >= 0)",
        ),
    ] {
        if !column_exists(&transaction, "scheduler_runs", column)? {
            transaction.execute(
                &format!("ALTER TABLE scheduler_runs ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }
    let reservation_schema: String = transaction.query_row(
        "SELECT sql FROM sqlite_schema WHERE type='table' AND name='scheduler_reservations'",
        [],
        |row| row.get(0),
    )?;
    if !reservation_schema.contains("'callback'")
        || !reservation_schema.contains("'actual_overage'")
    {
        transaction.execute_batch(
            "DROP TRIGGER IF EXISTS run_to_trial_effect_delete_guard;
             ALTER TABLE scheduler_reservations RENAME TO scheduler_reservations_legacy;
             CREATE TABLE scheduler_reservations (
                 reservation_id TEXT PRIMARY KEY,
                 run_id TEXT NOT NULL REFERENCES scheduler_runs(run_id) ON DELETE CASCADE,
                 principal_id TEXT NOT NULL, attempt_id TEXT, attempt_fence INTEGER,
                 idempotency_key TEXT NOT NULL,
                 kind TEXT NOT NULL CHECK (kind IN ('run', 'model', 'callback', 'tool', 'process')),
                 cost INTEGER NOT NULL CHECK (cost >= 0), tokens INTEGER NOT NULL CHECK (tokens >= 0),
                 turns INTEGER NOT NULL CHECK (turns >= 0), tools INTEGER NOT NULL CHECK (tools >= 0),
                 processes INTEGER NOT NULL CHECK (processes >= 0),
                 state TEXT NOT NULL CHECK (state IN ('reserved', 'debited', 'released', 'reconciled', 'actual_overage')),
                 dispatch_state TEXT NOT NULL CHECK (dispatch_state IN ('undispatched', 'dispatched', 'canceled', 'unknown')),
                 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                 UNIQUE (run_id, idempotency_key)
             );
             INSERT INTO scheduler_reservations SELECT * FROM scheduler_reservations_legacy;
             DROP TABLE scheduler_reservations_legacy;
             CREATE INDEX scheduler_reservations_run ON scheduler_reservations(run_id, state);",
        )?;
    }
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS run_to_trial (
             run_id TEXT PRIMARY KEY REFERENCES scheduler_runs(run_id) ON DELETE CASCADE,
             trial_id TEXT NOT NULL,
             trial_digest TEXT NOT NULL,
             task_digest TEXT NOT NULL,
             model_digest TEXT NOT NULL,
             config_digest TEXT NOT NULL,
             attempt_id TEXT NOT NULL,
             attempt_fence INTEGER NOT NULL CHECK (attempt_fence > 0),
             scheduler_principal_id TEXT NOT NULL,
             scheduler_idempotency_key TEXT NOT NULL,
             admission_token TEXT,
             created_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS consumed_trial_admissions (
              token_digest TEXT PRIMARY KEY,
              run_id TEXT NOT NULL UNIQUE,
              authority_position INTEGER NOT NULL UNIQUE CHECK (authority_position > 0),
              nonce TEXT NOT NULL UNIQUE,
              consumption_position INTEGER NOT NULL UNIQUE CHECK (consumption_position > 0),
              consumption_digest TEXT NOT NULL UNIQUE,
              anchor_state TEXT NOT NULL CHECK (anchor_state IN ('pending_anchor', 'anchored'))
          );
         CREATE TRIGGER IF NOT EXISTS run_to_trial_immutable
         BEFORE UPDATE ON run_to_trial BEGIN
             SELECT RAISE(ABORT, 'run-to-trial binding is immutable');
         END;
         CREATE TRIGGER IF NOT EXISTS run_to_trial_effect_delete_guard
         BEFORE DELETE ON run_to_trial
         WHEN EXISTS (SELECT 1 FROM scheduler_reservations WHERE run_id = OLD.run_id)
           OR EXISTS (SELECT 1 FROM events WHERE correlation_id = OLD.run_id)
         BEGIN
             SELECT RAISE(ABORT, 'run-to-trial binding has effects');
         END;
         CREATE TRIGGER IF NOT EXISTS consumed_trial_admissions_no_delete
         BEFORE DELETE ON consumed_trial_admissions BEGIN
             SELECT RAISE(ABORT, 'consumed trial admission is append-only');
         END;",
    )?;
    if !column_exists(&transaction, "run_to_trial", "admission_token")? {
        transaction.execute(
            "ALTER TABLE run_to_trial ADD COLUMN admission_token TEXT",
            [],
        )?;
    }
    for (column, definition) in [
        ("consumption_position", "INTEGER NOT NULL DEFAULT 1"),
        ("consumption_digest", "TEXT NOT NULL DEFAULT 'legacy'"),
        ("anchor_state", "TEXT NOT NULL DEFAULT 'anchored'"),
    ] {
        if !column_exists(&transaction, "consumed_trial_admissions", column)? {
            transaction.execute(
                &format!("ALTER TABLE consumed_trial_admissions ADD COLUMN {column} {definition}"),
                [],
            )?;
        }
    }
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS consumed_trial_admissions_immutable;
         CREATE TABLE IF NOT EXISTS statistical_admission_ledger (
             position INTEGER PRIMARY KEY CHECK (position > 0),
             previous_digest TEXT NOT NULL,
             entry_digest TEXT NOT NULL UNIQUE,
             run_id TEXT NOT NULL UNIQUE,
             token_digest TEXT NOT NULL UNIQUE,
             payload_digest TEXT NOT NULL,
             payload_bytes BLOB NOT NULL,
             anchor_state TEXT NOT NULL CHECK (anchor_state IN ('pending_anchor', 'anchored')),
             created_at INTEGER NOT NULL
         );
         CREATE TRIGGER consumed_trial_admissions_immutable
         BEFORE UPDATE ON consumed_trial_admissions
         WHEN NOT (OLD.anchor_state = 'pending_anchor' AND NEW.anchor_state = 'anchored'
                   AND OLD.token_digest = NEW.token_digest AND OLD.run_id = NEW.run_id
                   AND OLD.authority_position = NEW.authority_position AND OLD.nonce = NEW.nonce
                   AND OLD.consumption_position = NEW.consumption_position
                   AND OLD.consumption_digest = NEW.consumption_digest) BEGIN
             SELECT RAISE(ABORT, 'consumed trial admission is immutable');
         END;
         CREATE TRIGGER IF NOT EXISTS statistical_admission_ledger_immutable
         BEFORE UPDATE ON statistical_admission_ledger
         WHEN NOT (OLD.anchor_state = 'pending_anchor' AND NEW.anchor_state = 'anchored'
                   AND OLD.position = NEW.position AND OLD.previous_digest = NEW.previous_digest
                   AND OLD.entry_digest = NEW.entry_digest AND OLD.run_id = NEW.run_id
                   AND OLD.token_digest = NEW.token_digest AND OLD.payload_digest = NEW.payload_digest
                   AND OLD.payload_bytes = NEW.payload_bytes AND OLD.created_at = NEW.created_at) BEGIN
             SELECT RAISE(ABORT, 'statistical admission ledger is immutable');
         END;
         CREATE TRIGGER IF NOT EXISTS statistical_admission_ledger_no_delete
         BEFORE DELETE ON statistical_admission_ledger BEGIN
             SELECT RAISE(ABORT, 'statistical admission ledger is append-only');
         END;",
    )?;
    verify_statistical_admission_ledger(&transaction)?;
    transaction.commit()?;
    Ok(())
}

fn normalize_digest(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

fn verify_statistical_admission_ledger(
    transaction: &Transaction<'_>,
) -> Result<(), SchedulerError> {
    let mut statement = transaction.prepare(
        "SELECT position, previous_digest, entry_digest, run_id, token_digest,
                payload_digest, payload_bytes, anchor_state
         FROM statistical_admission_ledger ORDER BY position",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, u64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, Vec<u8>>(6)?,
            row.get::<_, String>(7)?,
        ))
    })?;
    let mut expected_position = 1;
    let mut previous_digest = "statistical-admission-genesis-v1".to_owned();
    let mut pending = 0;
    for row in rows {
        let (position, previous, digest, run_id, token, payload_digest, bytes, state) = row?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| SchedulerError::Conflict)?;
        let expected_digest = sha256(
            &serde_json::to_vec(&serde_json::json!({
                "domain": "kit-scheduler-statistical-admission-ledger-v1",
                "position": position,
                "previous_digest": previous,
                "payload_digest": payload_digest,
            }))
            .map_err(|_| SchedulerError::Conflict)?,
        );
        if position != expected_position
            || previous != previous_digest
            || payload_digest != sha256(&bytes)
            || digest != expected_digest
            || serde_json::to_vec(&value).map_err(|_| SchedulerError::Conflict)? != bytes
            || value.get("event_type").and_then(serde_json::Value::as_str)
                != Some("AdmissionConsumed")
            || value.get("run_id").and_then(serde_json::Value::as_str) != Some(&run_id)
            || value
                .get("token_digest")
                .and_then(serde_json::Value::as_str)
                != Some(&token)
            || !matches!(state.as_str(), "pending_anchor" | "anchored")
        {
            return Err(SchedulerError::Conflict);
        }
        if state == "pending_anchor" {
            pending += 1;
        }
        let stored: (u64, String, String) = transaction.query_row(
            "SELECT consumption_position, consumption_digest, anchor_state
             FROM consumed_trial_admissions WHERE run_id = ?1 AND token_digest = ?2",
            params![run_id, token],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if stored != (position, digest.clone(), state) {
            return Err(SchedulerError::Conflict);
        }
        expected_position += 1;
        previous_digest = digest;
    }
    if pending > 1 {
        return Err(SchedulerError::Conflict);
    }
    let ledger_count = expected_position - 1;
    let consumption_count: u64 = transaction.query_row(
        "SELECT COUNT(*) FROM consumed_trial_admissions WHERE consumption_digest != 'legacy'",
        [],
        |row| row.get(0),
    )?;
    if ledger_count != consumption_count {
        return Err(SchedulerError::Conflict);
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn validate_trial_binding(binding: &TrialRunBinding) -> Result<(), SchedulerError> {
    if binding.trial_id.is_empty()
        || binding.trial_digest.is_empty()
        || binding.task_digest.is_empty()
        || binding.model_digest.is_empty()
        || binding.config_digest.is_empty()
        || binding.attempt.fencing_token.get() == 0
    {
        Err(SchedulerError::Conflict)
    } else {
        Ok(())
    }
}

fn run_admission_record(
    transaction: &Transaction<'_>,
    run_id: RunId,
) -> Result<(String, String, String, String), SchedulerError> {
    transaction
        .query_row(
            "SELECT principal_id, idempotency_key, config_digest, phase FROM scheduler_runs WHERE run_id = ?1",
            [run_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?
        .ok_or(SchedulerError::UnknownRun)
}

fn check_run_admission(
    transaction: &Transaction<'_>,
    run_id: RunId,
    principal: &str,
    config: SchedulerConfig,
) -> Result<(), SchedulerError> {
    let head: String = transaction.query_row(
        "SELECT run_id FROM scheduler_runs WHERE phase = 'queued' ORDER BY queue_position LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    if head != run_id.to_string() {
        return Err(SchedulerError::NotQueueHead);
    }
    let global: i64 = transaction.query_row(
        "SELECT count(*) FROM scheduler_runs WHERE phase = 'admitted'",
        [],
        |row| row.get(0),
    )?;
    if usize::try_from(global).unwrap_or(usize::MAX) >= config.global_concurrency {
        return Err(SchedulerError::GlobalCap {
            limit: config.global_concurrency,
        });
    }
    let principal_active: i64 = transaction.query_row(
        "SELECT count(*) FROM scheduler_runs WHERE phase = 'admitted' AND principal_id = ?1",
        [principal],
        |row| row.get(0),
    )?;
    if usize::try_from(principal_active).unwrap_or(usize::MAX) >= config.principal_concurrency {
        return Err(SchedulerError::PrincipalCap {
            limit: config.principal_concurrency,
        });
    }
    Ok(())
}

fn insert_trial_binding(
    transaction: &Transaction<'_>,
    run_id: RunId,
    binding: &TrialRunBinding,
    principal: &str,
    idempotency_key: &str,
    admission_token: Option<&str>,
    now: i64,
) -> Result<(), SchedulerError> {
    transaction.execute(
        "INSERT INTO run_to_trial (
             run_id, trial_id, trial_digest, task_digest, model_digest, config_digest,
             attempt_id, attempt_fence, scheduler_principal_id,
             scheduler_idempotency_key, admission_token, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            run_id.to_string(),
            binding.trial_id,
            binding.trial_digest,
            binding.task_digest,
            binding.model_digest,
            binding.config_digest,
            binding.attempt.attempt_id.to_string(),
            to_i64(binding.attempt.fencing_token.get())?,
            principal,
            idempotency_key,
            admission_token,
            now,
        ],
    )?;
    Ok(())
}

fn trial_binding_matches(
    transaction: &Transaction<'_>,
    run_id: RunId,
    binding: &TrialRunBinding,
    principal: &str,
    idempotency_key: &str,
) -> Result<bool, SchedulerError> {
    let admission_token = binding
        .admission
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| SchedulerError::Conflict)?;
    let existing = transaction.query_row(
        "SELECT trial_id, trial_digest, task_digest, model_digest, config_digest,
                attempt_id, attempt_fence, scheduler_principal_id, scheduler_idempotency_key, admission_token
         FROM run_to_trial WHERE run_id = ?1",
        [run_id.to_string()],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
            row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?, row.get::<_, String>(7)?, row.get::<_, String>(8)?,
            row.get::<_, Option<String>>(9)?)),
    ).optional()?;
    Ok(existing
        == Some((
            binding.trial_id.clone(),
            binding.trial_digest.clone(),
            binding.task_digest.clone(),
            binding.model_digest.clone(),
            binding.config_digest.clone(),
            binding.attempt.attempt_id.to_string(),
            to_i64(binding.attempt.fencing_token.get())?,
            principal.to_owned(),
            idempotency_key.to_owned(),
            admission_token,
        )))
}

fn statistical_consumption(
    transaction: &Transaction<'_>,
    run_id: RunId,
) -> Result<Option<PendingStatisticalTrial>, SchedulerError> {
    transaction.query_row(
        "SELECT token_digest, nonce, authority_position, consumption_position, consumption_digest
         FROM consumed_trial_admissions WHERE run_id = ?1",
        [run_id.to_string()],
        |row| Ok(PendingStatisticalTrial {
            run_id,
            admission_token_digest: row.get(0)?,
            admission_nonce: row.get(1)?,
            admission_position: row.get(2)?,
            consumption_position: row.get(3)?,
            consumption_digest: row.get(4)?,
        }),
    ).optional().map_err(Into::into)
}

fn immediate(connection: &mut Connection) -> Result<Transaction<'_>, SchedulerError> {
    Ok(connection.transaction_with_behavior(TransactionBehavior::Immediate)?)
}

fn store_time(transaction: &Transaction<'_>) -> Result<i64, SchedulerError> {
    Ok(transaction.query_row(
        "UPDATE store_clock SET unix_micros = max(
             unix_micros, CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)
         ) WHERE singleton = 1 RETURNING unix_micros",
        [],
        |row| row.get(0),
    )?)
}

fn run_record(
    transaction: &Transaction<'_>,
    run_id: RunId,
) -> Result<Option<(String, String, Spend, String)>, SchedulerError> {
    transaction
        .query_row(
            "SELECT principal_id, idempotency_key, cost_limit, token_limit, turn_limit, tool_limit, process_limit, config_digest
             FROM scheduler_runs WHERE run_id = ?1",
            [run_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    Spend::new(
                        row.get::<_, u64>(2)?, row.get::<_, u64>(3)?, row.get::<_, u64>(4)?,
                        row.get::<_, u64>(5)?, row.get::<_, u64>(6)?,
                    ),
                    row.get(7)?,
                ))
            },
        )
        .optional()
        .map_err(Into::into)
}

fn column_exists(
    transaction: &Transaction<'_>,
    table: &str,
    column: &str,
) -> Result<bool, SchedulerError> {
    let mut statement = transaction.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(columns.iter().any(|name| name == column))
}

fn reservation_run(
    transaction: &Connection,
    run_id: RunId,
) -> Result<Option<ReservationRun>, SchedulerError> {
    transaction
        .query_row(
            "SELECT principal_id, cost_limit, token_limit, turn_limit, tool_limit, process_limit,
                    attempt_id, attempt_fence, phase, budget_breached FROM scheduler_runs WHERE run_id = ?1",
            [run_id.to_string()],
            |row| {
                Ok(ReservationRun {
                    principal: row.get(0)?,
                    budget: RunBudget::new(
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ),
                    attempt_id: row.get(6)?,
                    fence: row.get(7)?,
                    phase: row.get(8)?,
                    budget_breached: row.get(9)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn reservation_by_id(
    connection: &Connection,
    id: ReservationId,
) -> Result<Option<StoredReservation>, SchedulerError> {
    connection
        .query_row(
            "SELECT run_id, principal_id, idempotency_key, kind, cost, tokens, turns, tools,
                    processes, state, attempt_id, attempt_fence, dispatch_state
             FROM scheduler_reservations WHERE reservation_id = ?1",
            [reservation_key(id)],
            |row| {
                let run_id = row
                    .get::<_, String>(0)?
                    .parse()
                    .map_err(|_| rusqlite::Error::InvalidQuery)?;
                let principal_id = row
                    .get::<_, String>(1)?
                    .parse()
                    .map_err(|_| rusqlite::Error::InvalidQuery)?;
                let kind = parse_kind(&row.get::<_, String>(3)?)?;
                let state = parse_status(&row.get::<_, String>(9)?)?;
                let attempt_id = row.get::<_, Option<String>>(10)?;
                let attempt_fence = row.get::<_, Option<u64>>(11)?;
                let attempt = match (attempt_id, attempt_fence) {
                    (Some(attempt), Some(fence)) => Some(AttemptOwnership::new(
                        attempt.parse().map_err(|_| rusqlite::Error::InvalidQuery)?,
                        principal_id,
                        crate::domain::lifecycle::FencingToken::new(fence),
                    )),
                    (None, None) => None,
                    _ => return Err(rusqlite::Error::InvalidQuery),
                };
                Ok(StoredReservation {
                    snapshot: ReservationSnapshot::new(
                        id,
                        Spend::new(
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                        ),
                        state,
                    ),
                    run_id,
                    idempotency_key: row.get(2)?,
                    kind,
                    attempt,
                    dispatch: parse_dispatch(&row.get::<_, String>(12)?)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn totals_tx(connection: &Connection, run_id: RunId) -> Result<BudgetTotals, SchedulerError> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM scheduler_runs WHERE run_id = ?1)",
        [run_id.to_string()],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(SchedulerError::UnknownRun);
    }
    let sum = |states: &str| -> Result<Spend, SchedulerError> {
        let sql = format!(
            "SELECT COALESCE(sum(cost), 0), COALESCE(sum(tokens), 0), COALESCE(sum(turns), 0),
                    COALESCE(sum(tools), 0), COALESCE(sum(processes), 0)
             FROM scheduler_reservations WHERE run_id = ?1 AND state IN ({states})"
        );
        Ok(connection.query_row(&sql, [run_id.to_string()], |row| {
            Ok(Spend::new(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?)
    };
    Ok(BudgetTotals {
        committed: sum("'debited', 'reconciled', 'actual_overage'")?,
        reserved: sum("'reserved'")?,
    })
}

fn refresh_budget_breach(
    transaction: &Transaction<'_>,
    run_id: RunId,
    now: i64,
) -> Result<(), SchedulerError> {
    let run = reservation_run(transaction, run_id)?.ok_or(SchedulerError::UnknownRun)?;
    let totals = totals_tx(transaction, run_id)?.committed;
    let limits = run.budget.limits();
    let overage = Spend::new(
        totals
            .cost_microusd()
            .saturating_sub(limits.cost_microusd()),
        totals.tokens().saturating_sub(limits.tokens()),
        totals.turns().saturating_sub(limits.turns()),
        totals.tools().saturating_sub(limits.tools()),
        totals.processes().saturating_sub(limits.processes()),
    );
    let actual_overage: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM scheduler_reservations WHERE run_id = ?1 AND state = 'actual_overage')",
        [run_id.to_string()],
        |row| row.get(0),
    )?;
    transaction.execute(
        "UPDATE scheduler_runs
         SET budget_breached = budget_breached OR ?2,
             cost_overage = max(cost_overage, ?3),
             token_overage = max(token_overage, ?4),
             turn_overage = max(turn_overage, ?5), updated_at = ?6 WHERE run_id = ?1",
        params![
            run_id.to_string(),
            actual_overage || overage != Spend::ZERO,
            to_i64(overage.cost_microusd())?,
            to_i64(overage.tokens())?,
            to_i64(overage.turns())?,
            now,
        ],
    )?;
    Ok(())
}

fn remove_orphan_runs(transaction: &Transaction<'_>) -> Result<usize, SchedulerError> {
    let mut statement = transaction.prepare(
        "SELECT run_id FROM scheduler_runs
         WHERE NOT EXISTS (SELECT 1 FROM events WHERE events.stream = scheduler_runs.run_id AND event_type = 'run.start')
           AND NOT EXISTS (SELECT 1 FROM scheduler_reservations WHERE scheduler_reservations.run_id = scheduler_runs.run_id)",
    )?;
    let runs = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for run in &runs {
        transaction.execute("DELETE FROM scheduler_runs WHERE run_id = ?1", [run])?;
    }
    Ok(runs.len())
}

fn validate_category(kind: AdmissionKind, spend: Spend) -> Result<(), SchedulerError> {
    let valid = match kind {
        AdmissionKind::Run => true,
        AdmissionKind::Model => spend.turns() > 0 && spend.tools() == 0 && spend.processes() == 0,
        AdmissionKind::Callback => {
            spend.turns() > 0
                && spend.cost_microusd() == 0
                && spend.tokens() == 0
                && spend.tools() == 0
                && spend.processes() == 0
        }
        AdmissionKind::Tool => spend.tools() > 0 && spend.processes() == 0,
        AdmissionKind::Process => spend.processes() > 0,
    };
    if valid {
        Ok(())
    } else {
        Err(SchedulerError::Conflict)
    }
}

fn checked_spend(spend: Spend) -> Result<[i64; 5], SchedulerError> {
    Ok([
        to_i64(spend.cost_microusd())?,
        to_i64(spend.tokens())?,
        to_i64(spend.turns())?,
        to_i64(spend.tools())?,
        to_i64(spend.processes())?,
    ])
}

fn to_i64(value: u64) -> Result<i64, SchedulerError> {
    i64::try_from(value).map_err(|_| SchedulerError::InvalidConfig)
}

fn reservation_key(id: ReservationId) -> String {
    format!("{:032x}", id.get())
}

fn status_name(status: ReservationStatus) -> &'static str {
    match status {
        ReservationStatus::Reserved => "reserved",
        ReservationStatus::Debited => "debited",
        ReservationStatus::Released => "released",
        ReservationStatus::Reconciled => "reconciled",
        ReservationStatus::ActualOverage => "actual_overage",
    }
}

fn parse_status(value: &str) -> Result<ReservationStatus, rusqlite::Error> {
    match value {
        "reserved" => Ok(ReservationStatus::Reserved),
        "debited" => Ok(ReservationStatus::Debited),
        "released" => Ok(ReservationStatus::Released),
        "reconciled" => Ok(ReservationStatus::Reconciled),
        "actual_overage" => Ok(ReservationStatus::ActualOverage),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn parse_kind(value: &str) -> Result<AdmissionKind, rusqlite::Error> {
    match value {
        "run" => Ok(AdmissionKind::Run),
        "model" => Ok(AdmissionKind::Model),
        "callback" => Ok(AdmissionKind::Callback),
        "tool" => Ok(AdmissionKind::Tool),
        "process" => Ok(AdmissionKind::Process),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn parse_dispatch(value: &str) -> Result<DispatchState, rusqlite::Error> {
    match value {
        "undispatched" => Ok(DispatchState::Undispatched),
        "dispatched" => Ok(DispatchState::Dispatched),
        "canceled" => Ok(DispatchState::Canceled),
        "unknown" => Ok(DispatchState::Unknown),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}
