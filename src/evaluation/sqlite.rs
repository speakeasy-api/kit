use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};

use crate::{
    domain::ids::RunId, executor::trial::TrialUsage,
    store::sqlite::trial_usage::SqliteTrialUsageReceiptStore,
};

use super::{
    CoordinatorError, EventEvidenceStore, PreparedEventEvidence, ProviderEvidenceStore,
    ToolEvidenceStore, UsageEvidenceStore,
    reports::{BoundTrialEnvelope, TerminalErrorEvidence, TrialRunConfig, sha256},
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct SqliteProviderEvidenceStore(PathBuf);

#[derive(Clone)]
pub struct SqliteToolEvidenceStore(PathBuf);

#[derive(Clone)]
pub struct SqliteUsageEvidenceStore(PathBuf);

#[derive(Clone)]
pub struct SqliteEventEvidenceStore(PathBuf);

#[derive(Clone)]
pub struct SqliteCoordinatorOperationStore(PathBuf);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CoordinatorPhase {
    Admitted,
    Executing,
    EvidenceReady,
    Terminalizing,
    Recorded,
    TerminalError,
}

pub(crate) struct CoordinatorOperation {
    pub phase: CoordinatorPhase,
    pub config: TrialRunConfig,
    pub consumption_receipt: crate::runtime::scheduler::AnchoredConsumptionReceipt,
    pub harness_bytes: Option<Vec<u8>>,
    pub events_bytes: Option<Vec<u8>>,
    pub terminal_error: Option<String>,
    pub terminal_evidence: Option<TerminalErrorEvidence>,
    pub event_source: String,
}

macro_rules! evidence_store {
    ($name:ident) => {
        impl $name {
            pub fn open(database: impl AsRef<Path>) -> Result<Self, CoordinatorError> {
                let database = database.as_ref();
                if !database.is_file() {
                    return Err(CoordinatorError::Evidence(
                        "durable evidence database is unavailable",
                    ));
                }
                open(database)?;
                Ok(Self(database.to_owned()))
            }
        }
    };
}

evidence_store!(SqliteProviderEvidenceStore);
evidence_store!(SqliteToolEvidenceStore);
evidence_store!(SqliteUsageEvidenceStore);
evidence_store!(SqliteEventEvidenceStore);

impl SqliteCoordinatorOperationStore {
    pub fn open(database: impl AsRef<Path>) -> Result<Self, CoordinatorError> {
        let database = database.as_ref();
        let connection = open(database)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS statistical_coordinator_operations (
                     run_id TEXT PRIMARY KEY,
                     phase TEXT NOT NULL CHECK (phase IN
                          ('admitted', 'executing', 'evidence_ready', 'terminalizing', 'recorded', 'terminal_error')),
                     event_source TEXT NOT NULL,
                     run_config_digest TEXT NOT NULL,
                     run_config_bytes BLOB NOT NULL,
                     consumption_receipt_digest TEXT NOT NULL,
                     consumption_receipt_bytes BLOB NOT NULL,
                     harness_digest TEXT,
                     harness_bytes BLOB,
                     events_digest TEXT,
                     events_bytes BLOB,
                     execution_receipt_digest TEXT,
                     execution_receipt_bytes BLOB,
                     terminal_error TEXT,
                     terminal_evidence_digest TEXT,
                     terminal_evidence_bytes BLOB,
                     CHECK ((harness_digest IS NULL AND harness_bytes IS NULL)
                         OR (harness_digest IS NOT NULL AND harness_bytes IS NOT NULL)),
                     CHECK ((events_digest IS NULL AND events_bytes IS NULL)
                         OR (events_digest IS NOT NULL AND events_bytes IS NOT NULL)),
                     CHECK ((execution_receipt_digest IS NULL AND execution_receipt_bytes IS NULL)
                         OR (execution_receipt_digest IS NOT NULL AND execution_receipt_bytes IS NOT NULL))
                 );",
            )
            .map_err(evidence_database)?;
        Ok(Self(database.to_owned()))
    }

    pub(crate) fn load(
        &self,
        run_id: RunId,
    ) -> Result<Option<CoordinatorOperation>, CoordinatorError> {
        let connection = open(&self.0)?;
        let stored = connection
            .query_row(
                "SELECT phase, run_config_digest, run_config_bytes,
                        consumption_receipt_digest, consumption_receipt_bytes,
                        harness_digest, harness_bytes, events_digest, events_bytes,
                        execution_receipt_digest, execution_receipt_bytes, terminal_error,
                        terminal_evidence_digest, terminal_evidence_bytes, event_source
                 FROM statistical_coordinator_operations WHERE run_id = ?1",
                [run_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<Vec<u8>>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<Vec<u8>>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<Vec<u8>>>(10)?,
                        row.get::<_, Option<String>>(11)?,
                        row.get::<_, Option<String>>(12)?,
                        row.get::<_, Option<Vec<u8>>>(13)?,
                        row.get::<_, String>(14)?,
                    ))
                },
            )
            .optional()
            .map_err(evidence_database)?;
        let Some((
            phase,
            config_digest,
            config_bytes,
            consumption_digest,
            consumption_bytes,
            harness_digest,
            harness_bytes,
            events_digest,
            events_bytes,
            execution_digest,
            execution_bytes,
            terminal_error,
            terminal_evidence_digest,
            terminal_evidence_bytes,
            event_source,
        )) = stored
        else {
            return Ok(None);
        };
        let parsed_phase = phase_from_str(&phase)?;
        let terminal_evidence = terminal_evidence_bytes
            .as_deref()
            .map(serde_json::from_slice)
            .transpose()
            .map_err(|_| CoordinatorError::Evidence("terminal evidence is corrupt"))?;
        if sha256(&config_bytes) != config_digest
            || sha256(&consumption_bytes) != consumption_digest
            || harness_digest.as_deref() != harness_bytes.as_deref().map(sha256).as_deref()
            || events_digest.as_deref() != events_bytes.as_deref().map(sha256).as_deref()
            || execution_digest.as_deref() != execution_bytes.as_deref().map(sha256).as_deref()
            || terminal_evidence_digest.as_deref()
                != terminal_evidence_bytes.as_deref().map(sha256).as_deref()
            || matches!(
                parsed_phase,
                CoordinatorPhase::Recorded | CoordinatorPhase::TerminalError
            ) != execution_bytes.is_some()
            || matches!(
                parsed_phase,
                CoordinatorPhase::Terminalizing | CoordinatorPhase::TerminalError
            ) != terminal_evidence_bytes.is_some()
            || terminal_evidence_bytes.is_some()
                && terminal_error.as_deref()
                    != terminal_evidence
                        .as_ref()
                        .map(|evidence: &TerminalErrorEvidence| evidence.reason.as_str())
            || !matches!(
                event_source.as_str(),
                "production_authenticated" | "conformance_source_semantics_fake"
            )
        {
            return Err(CoordinatorError::Evidence(
                "coordinator operation artifact mismatch",
            ));
        }
        let config: TrialRunConfig = serde_json::from_slice(&config_bytes)
            .map_err(|_| CoordinatorError::Evidence("coordinator run config is corrupt"))?;
        let consumption_receipt = serde_json::from_slice(&consumption_bytes).map_err(|_| {
            CoordinatorError::Evidence("coordinator consumption receipt is corrupt")
        })?;
        Ok(Some(CoordinatorOperation {
            phase: parsed_phase,
            config,
            consumption_receipt,
            harness_bytes,
            events_bytes,
            terminal_error,
            terminal_evidence,
            event_source,
        }))
    }

    pub(crate) fn admitted(
        &self,
        run_id: RunId,
        config: &TrialRunConfig,
        receipt: &crate::runtime::scheduler::AnchoredConsumptionReceipt,
        event_source: &str,
    ) -> Result<(), CoordinatorError> {
        let config_bytes = serde_json::to_vec(config).map_err(|_| {
            CoordinatorError::Evidence("coordinator run config serialization failed")
        })?;
        let receipt_bytes = serde_json::to_vec(receipt)
            .map_err(|_| CoordinatorError::Evidence("coordinator receipt serialization failed"))?;
        let connection = open(&self.0)?;
        connection
            .execute(
                "INSERT INTO statistical_coordinator_operations
                 (run_id, phase, run_config_digest, run_config_bytes,
                   consumption_receipt_digest, consumption_receipt_bytes, event_source)
                  VALUES (?1, 'admitted', ?2, ?3, ?4, ?5, ?6) ON CONFLICT(run_id) DO NOTHING",
                params![
                    run_id.to_string(),
                    sha256(&config_bytes),
                    config_bytes,
                    sha256(&receipt_bytes),
                    receipt_bytes,
                    event_source,
                ],
            )
            .map_err(evidence_database)?;
        let stored = self.load(run_id)?.ok_or(CoordinatorError::Evidence(
            "coordinator operation is missing",
        ))?;
        if stored.config != *config
            || stored.consumption_receipt != *receipt
            || stored.event_source != event_source
        {
            return Err(CoordinatorError::Evidence(
                "coordinator admission operation conflicts",
            ));
        }
        Ok(())
    }

    pub(crate) fn executing(&self, run_id: RunId) -> Result<(), CoordinatorError> {
        self.transition(run_id, "admitted", "executing", [], None)
    }

    pub(crate) fn evidence_ready(
        &self,
        run_id: RunId,
        harness: &[u8],
        events: &[u8],
    ) -> Result<(), CoordinatorError> {
        self.transition(
            run_id,
            "executing",
            "evidence_ready",
            [
                ("harness_digest", sha256(harness)),
                ("events_digest", sha256(events)),
            ],
            Some((harness, events)),
        )
    }

    pub(crate) fn recorded(
        &self,
        run_id: RunId,
        receipt: &BoundTrialEnvelope,
    ) -> Result<(), CoordinatorError> {
        let bytes = serde_json::to_vec(receipt.receipt())
            .map_err(|_| CoordinatorError::Evidence("execution receipt serialization failed"))?;
        let connection = open(&self.0)?;
        let changed = connection
            .execute(
                "UPDATE statistical_coordinator_operations SET phase = 'recorded',
                    execution_receipt_digest = ?2, execution_receipt_bytes = ?3
                 WHERE run_id = ?1 AND phase = 'evidence_ready'",
                params![run_id.to_string(), receipt.digest(), bytes],
            )
            .map_err(evidence_database)?;
        if changed == 0
            && self
                .load(run_id)?
                .is_none_or(|operation| operation.phase != CoordinatorPhase::Recorded)
        {
            return Err(CoordinatorError::Evidence(
                "invalid coordinator recorded transition",
            ));
        }
        Ok(())
    }

    pub(crate) fn terminalizing(
        &self,
        run_id: RunId,
        evidence: &TerminalErrorEvidence,
    ) -> Result<(), CoordinatorError> {
        let bytes = serde_json::to_vec(evidence)
            .map_err(|_| CoordinatorError::Evidence("terminal evidence serialization failed"))?;
        let connection = open(&self.0)?;
        let changed = connection
            .execute(
                "UPDATE statistical_coordinator_operations SET phase = 'terminalizing',
                    terminal_error = ?2, terminal_evidence_digest = ?3,
                    terminal_evidence_bytes = ?4
                 WHERE run_id = ?1 AND phase IN ('admitted', 'executing', 'evidence_ready')",
                params![run_id.to_string(), evidence.reason, sha256(&bytes), bytes],
            )
            .map_err(evidence_database)?;
        if changed == 0 {
            let operation = self.load(run_id)?.ok_or(CoordinatorError::Evidence(
                "coordinator operation is missing",
            ))?;
            if operation.phase != CoordinatorPhase::Terminalizing
                || operation.terminal_evidence.as_ref() != Some(evidence)
            {
                return Err(CoordinatorError::Evidence(
                    "invalid coordinator terminalizing transition",
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn terminal_error(
        &self,
        run_id: RunId,
        reason: &str,
        receipt: &BoundTrialEnvelope,
    ) -> Result<(), CoordinatorError> {
        let bytes = serde_json::to_vec(receipt.receipt())
            .map_err(|_| CoordinatorError::Evidence("execution receipt serialization failed"))?;
        let connection = open(&self.0)?;
        let changed = connection
            .execute(
                "UPDATE statistical_coordinator_operations SET phase = 'terminal_error',
                    terminal_error = ?2, execution_receipt_digest = ?3, execution_receipt_bytes = ?4
                  WHERE run_id = ?1 AND phase = 'terminalizing'",
                params![run_id.to_string(), reason, receipt.digest(), bytes],
            )
            .map_err(evidence_database)?;
        if changed == 0 {
            let operation = self.load(run_id)?.ok_or(CoordinatorError::Evidence(
                "coordinator operation is missing",
            ))?;
            if operation.phase != CoordinatorPhase::TerminalError
                || operation.terminal_error.as_deref() != Some(reason)
            {
                return Err(CoordinatorError::Evidence(
                    "invalid coordinator terminal transition",
                ));
            }
        }
        Ok(())
    }

    fn transition<const N: usize>(
        &self,
        run_id: RunId,
        from: &str,
        to: &str,
        digests: [(&str, String); N],
        artifacts: Option<(&[u8], &[u8])>,
    ) -> Result<(), CoordinatorError> {
        let connection = open(&self.0)?;
        let changed = if let Some((harness, events)) = artifacts {
            connection.execute(
                "UPDATE statistical_coordinator_operations SET phase = ?2,
                    harness_digest = ?3, harness_bytes = ?4, events_digest = ?5, events_bytes = ?6
                 WHERE run_id = ?1 AND phase = ?7",
                params![run_id.to_string(), to, digests[0].1, harness, digests[1].1, events, from],
            )
        } else {
            connection.execute(
                "UPDATE statistical_coordinator_operations SET phase = ?2 WHERE run_id = ?1 AND phase = ?3",
                params![run_id.to_string(), to, from],
            )
        }
        .map_err(evidence_database)?;
        if changed == 0
            && self
                .load(run_id)?
                .is_none_or(|operation| phase_name(operation.phase) != to)
        {
            return Err(CoordinatorError::Evidence(
                "invalid coordinator operation transition",
            ));
        }
        Ok(())
    }
}

impl ProviderEvidenceStore for SqliteProviderEvidenceStore {
    fn verify(
        &self,
        run_id: RunId,
        config: &TrialRunConfig,
        provider_request_ids: &[String],
        event_high_watermark: u64,
    ) -> Result<(), CoordinatorError> {
        let connection = open(&self.0)?;
        let binding = binding(&connection, run_id, config)?;
        if event_high_watermark > event_watermark(&connection)? {
            return Err(CoordinatorError::Evidence(
                "provider event watermark mismatch",
            ));
        }
        let provider_watermark: u64 = connection
            .query_row(
                "SELECT position FROM provider_stream_watermark WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(evidence_database)?;
        let rows = effect_events(
            &connection,
            run_id,
            config.event_start_watermark,
            event_high_watermark,
            "model_call.outcome",
        )?;
        let mut durable_ids = Vec::with_capacity(rows.len());
        for event in rows {
            verify_event_owner(&event, &binding)?;
            let request_id = required_str(&event.payload, "provider_request_id")?;
            let model_call_id = required_str(&event.payload, "model_call_id")?;
            let stream: Option<u64> = connection
                .query_row(
                    "SELECT outcome_position FROM provider_streams
                     WHERE attempt_id = ?1 AND model_call_id = ?2 AND fence = ?3
                       AND outcome_position IS NOT NULL",
                    params![binding.attempt_id, model_call_id, binding.attempt_fence],
                    |row| row.get(0),
                )
                .optional()
                .map_err(evidence_database)?;
            if stream.is_none_or(|position| position == 0 || position > provider_watermark)
                || durable_ids.iter().any(|stored| stored == request_id)
            {
                return Err(CoordinatorError::Evidence(
                    "provider stream binding mismatch",
                ));
            }
            durable_ids.push(request_id.to_owned());
        }
        if durable_ids != provider_request_ids || durable_ids.is_empty() {
            return Err(CoordinatorError::Evidence(
                "provider request evidence mismatch",
            ));
        }
        Ok(())
    }
}

impl ToolEvidenceStore for SqliteToolEvidenceStore {
    fn verify(
        &self,
        run_id: RunId,
        config: &TrialRunConfig,
        event_high_watermark: u64,
    ) -> Result<(), CoordinatorError> {
        let connection = open(&self.0)?;
        let binding = binding(&connection, run_id, config)?;
        if event_high_watermark > event_watermark(&connection)?
            || event_high_watermark <= config.event_start_watermark
        {
            return Err(CoordinatorError::Evidence("tool event watermark mismatch"));
        }
        let mut states = std::collections::BTreeMap::<String, u8>::new();
        for event in all_effect_events(
            &connection,
            run_id,
            config.event_start_watermark,
            event_high_watermark,
        )?
        .into_iter()
        .filter(|event| event.kind.starts_with("capability.invocation_"))
        {
            verify_event_owner(&event, &binding)?;
            let id = required_str(&event.payload, "invocation_id")?.to_owned();
            match event.kind.as_str() {
                "capability.invocation_intent" if states.insert(id.clone(), 1).is_none() => {}
                "capability.invocation_dispatched"
                    if states.get_mut(&id).is_some_and(|state| {
                        if *state == 1 {
                            *state = 2;
                            true
                        } else {
                            false
                        }
                    }) => {}
                "capability.invocation_outcome" if states.remove(&id) == Some(2) => {}
                _ => {
                    return Err(CoordinatorError::Evidence(
                        "tool invocation lifecycle mismatch",
                    ));
                }
            }
        }
        if !states.is_empty() {
            return Err(CoordinatorError::Evidence(
                "incomplete tool invocation evidence",
            ));
        }
        Ok(())
    }
}

impl UsageEvidenceStore for SqliteUsageEvidenceStore {
    fn verify(
        &self,
        run_id: RunId,
        config: &TrialRunConfig,
        usage: TrialUsage,
        event_high_watermark: u64,
    ) -> Result<(), CoordinatorError> {
        let connection = open(&self.0)?;
        binding(&connection, run_id, config)?;
        let verified = SqliteTrialUsageReceiptStore::open(&self.0)
            .and_then(|store| store.verify_run(run_id, &config.trial_id))
            .map_err(|_| CoordinatorError::Evidence("scheduler usage receipt mismatch"))?;
        if verified.usage != usage
            || verified.binding.run_id != config.scheduler_run_id
            || normalized(&verified.binding.task_digest) != normalized(&config.task_manifest_digest)
            || normalized(&verified.binding.model_digest) != normalized(&config.model_digest)
            || normalized(&verified.binding.config_digest) != normalized(&config.config_digest)
            || verified.event_high_watermark != event_high_watermark
        {
            return Err(CoordinatorError::Evidence(
                "scheduler usage binding mismatch",
            ));
        }
        Ok(())
    }
}

impl EventEvidenceStore for SqliteEventEvidenceStore {
    fn source(&self) -> &'static str {
        "production_authenticated"
    }

    fn capture_start(
        &self,
        run_id: RunId,
        pending: &crate::runtime::scheduler::PendingStatisticalTrial,
    ) -> Result<PreparedEventEvidence, CoordinatorError> {
        let connection = open(&self.0)?;
        if pending.run_id != run_id {
            return Err(CoordinatorError::Binding);
        }
        Ok(PreparedEventEvidence {
            source: "production_authenticated".to_owned(),
            event_start_watermark: event_watermark(&connection)?,
        })
    }

    fn finalize_terminal(
        &self,
        scheduler: &crate::runtime::scheduler::DurableScheduler,
        run_id: RunId,
        _: &TrialRunConfig,
    ) -> Result<u64, CoordinatorError> {
        scheduler
            .finish_run_with_event_watermark(run_id, false)
            .map_err(Into::into)
    }

    fn trusted_events(
        &self,
        run_id: RunId,
        config: &TrialRunConfig,
        event_high_watermark: u64,
    ) -> Result<Vec<u8>, CoordinatorError> {
        let connection = open(&self.0)?;
        let binding = binding(&connection, run_id, config)?;
        let high = event_high_watermark;
        if high <= config.event_start_watermark || high > event_watermark(&connection)? {
            return Err(CoordinatorError::Evidence(
                "terminal event watermark mismatch",
            ));
        }
        let later: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM events WHERE correlation_id = ?1 AND commit_position > ?2)",
                params![run_id.to_string(), high],
                |row| row.get(0),
            )
            .map_err(evidence_database)?;
        if later {
            return Err(CoordinatorError::Evidence(
                "event appended after terminal watermark",
            ));
        }
        let events = all_events(&connection, run_id, config.event_start_watermark, high)?;
        let mut scheduler = Vec::new();
        let mut provider = Vec::new();
        let mut tools = Vec::new();
        for event in events {
            if event
                .attempt_id
                .as_deref()
                .is_some_and(|attempt| attempt != binding.attempt_id)
            {
                return Err(CoordinatorError::Evidence("event attempt binding mismatch"));
            }
            let value = json!({
                "kind": event.kind,
                "event_position": event.position,
                "admission_token_digest": config.admission_token_digest,
            });
            if event.kind.starts_with("model_call.") {
                verify_event_owner(&event, &binding)?;
                provider.push(value);
            } else if event.kind.starts_with("capability.invocation_") {
                verify_event_owner(&event, &binding)?;
                tools.push(value);
            } else {
                scheduler.push(value);
            }
        }
        if scheduler.is_empty() || provider.is_empty() || tools.is_empty() {
            return Err(CoordinatorError::Evidence(
                "required durable event class is missing",
            ));
        }
        let (started, finished): (u64, u64) = connection
            .query_row(
                "SELECT created_at / 1000, updated_at / 1000 FROM scheduler_runs WHERE run_id = ?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(evidence_database)?;
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "source": "production_authenticated",
            "run_config_digest": config.immutable_digest,
            "admission_position": config.admission_position,
            "admission_nonce": config.admission_nonce,
            "admission_token_digest": config.admission_token_digest,
            "scheduler_run_id": config.scheduler_run_id,
            "scheduler_consumption_position": config.scheduler_consumption_position,
            "scheduler_consumption_digest": config.scheduler_consumption_digest,
            "event_high_watermark": high,
            "trial_id": config.trial_id,
            "pair_id": config.pair_id,
            "task_id": config.task_id,
            "dataset_member_id": config.dataset_member_id,
            "seed": config.seed,
            "arm": config.arm,
            "config_digest": config.config_digest,
            "started_monotonic_millis": started,
            "finished_monotonic_millis": finished.max(started),
            "intervention": false,
            "exclusion_reason": "",
            "scheduler_events": scheduler,
            "provider_events": provider,
            "tool_events": tools,
        }))
        .map_err(|_| CoordinatorError::Evidence("event evidence serialization failed"))
    }
}

struct Binding {
    attempt_id: String,
    attempt_fence: u64,
}

type StoredBinding = (String, String, String, String, String, u64, String, String);

fn binding(
    connection: &Connection,
    run_id: RunId,
    config: &TrialRunConfig,
) -> Result<Binding, CoordinatorError> {
    let stored: Option<StoredBinding> = connection
        .query_row(
            "SELECT trial_id, trial_digest, task_digest, model_digest, config_digest,
                    attempt_fence, attempt_id, admission_token
             FROM run_to_trial WHERE run_id = ?1",
            [run_id.to_string()],
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
                ))
            },
        )
        .optional()
        .map_err(evidence_database)?;
    let (trial, preregistration, task, model, configured, fence, attempt, token) = stored.ok_or(
        CoordinatorError::Evidence("run-to-trial binding is missing"),
    )?;
    let token: Value = serde_json::from_str(&token)
        .map_err(|_| CoordinatorError::Evidence("scheduler admission token is corrupt"))?;
    let anchored: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM consumed_trial_admissions
             WHERE run_id = ?1 AND token_digest = ?2 AND nonce = ?3
               AND authority_position = ?4 AND consumption_position = ?5
               AND consumption_digest = ?6 AND anchor_state = 'anchored')",
            params![
                run_id.to_string(),
                config.admission_token_digest,
                config.admission_nonce,
                config.admission_position,
                config.scheduler_consumption_position,
                config.scheduler_consumption_digest
            ],
            |row| row.get(0),
        )
        .map_err(evidence_database)?;
    if config.scheduler_run_id != run_id.to_string()
        || trial != config.trial_id
        || normalized(&preregistration) != normalized(&config.preregistration_digest)
        || normalized(&task) != normalized(&config.task_manifest_digest)
        || normalized(&model) != normalized(&config.model_digest)
        || normalized(&configured) != normalized(&config.config_digest)
        || fence == 0
        || token.get("token_digest").and_then(Value::as_str)
            != Some(config.admission_token_digest.as_str())
        || token.get("nonce").and_then(Value::as_str) != Some(config.admission_nonce.as_str())
        || !anchored
    {
        return Err(CoordinatorError::Evidence(
            "durable trial authority binding mismatch",
        ));
    }
    Ok(Binding {
        attempt_id: attempt,
        attempt_fence: fence,
    })
}

struct StoredEvent {
    position: u64,
    kind: String,
    attempt_id: Option<String>,
    payload: Value,
}

fn effect_events(
    connection: &Connection,
    run_id: RunId,
    low: u64,
    high: u64,
    kind: &str,
) -> Result<Vec<StoredEvent>, CoordinatorError> {
    let mut events = all_events(connection, run_id, low, high)?;
    events.retain(|event| event.kind == kind);
    Ok(events)
}

fn all_effect_events(
    connection: &Connection,
    run_id: RunId,
    low: u64,
    high: u64,
) -> Result<Vec<StoredEvent>, CoordinatorError> {
    let mut events = all_events(connection, run_id, low, high)?;
    events.retain(|event| {
        event.kind.starts_with("model_call.") || event.kind.starts_with("capability.invocation_")
    });
    Ok(events)
}

fn all_events(
    connection: &Connection,
    run_id: RunId,
    low: u64,
    high: u64,
) -> Result<Vec<StoredEvent>, CoordinatorError> {
    let mut statement = connection
        .prepare(
            "SELECT commit_position, event_type, attempt_id, payload FROM events
             WHERE correlation_id = ?1 AND commit_position > ?2 AND commit_position <= ?3
             ORDER BY commit_position",
        )
        .map_err(evidence_database)?;
    statement
        .query_map(params![run_id.to_string(), low, high], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .map_err(evidence_database)?
        .map(|row| {
            let (position, kind, attempt_id, payload) = row.map_err(evidence_database)?;
            let payload = serde_json::from_slice(&payload)
                .map_err(|_| CoordinatorError::Evidence("durable event payload is corrupt"))?;
            Ok(StoredEvent {
                position,
                kind,
                attempt_id,
                payload,
            })
        })
        .collect()
}

fn verify_event_owner(event: &StoredEvent, binding: &Binding) -> Result<(), CoordinatorError> {
    if event.attempt_id.as_deref() != Some(binding.attempt_id.as_str())
        || event.payload.get("attempt_id").and_then(Value::as_str)
            != Some(binding.attempt_id.as_str())
        || event.payload.get("attempt_fence").and_then(Value::as_u64) != Some(binding.attempt_fence)
    {
        return Err(CoordinatorError::Evidence(
            "effect event ownership mismatch",
        ));
    }
    Ok(())
}

fn event_watermark(connection: &Connection) -> Result<u64, CoordinatorError> {
    connection
        .query_row(
            "SELECT position FROM commit_watermark WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(evidence_database)
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str, CoordinatorError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .ok_or(CoordinatorError::Evidence("durable event field is invalid"))
}

fn open(path: &Path) -> Result<Connection, CoordinatorError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(evidence_database)?;
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(evidence_database)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(evidence_database)?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(evidence_database)?;
    Ok(connection)
}

fn normalized(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

fn phase_from_str(value: &str) -> Result<CoordinatorPhase, CoordinatorError> {
    match value {
        "admitted" => Ok(CoordinatorPhase::Admitted),
        "executing" => Ok(CoordinatorPhase::Executing),
        "evidence_ready" => Ok(CoordinatorPhase::EvidenceReady),
        "terminalizing" => Ok(CoordinatorPhase::Terminalizing),
        "recorded" => Ok(CoordinatorPhase::Recorded),
        "terminal_error" => Ok(CoordinatorPhase::TerminalError),
        _ => Err(CoordinatorError::Evidence(
            "invalid coordinator operation phase",
        )),
    }
}

fn phase_name(phase: CoordinatorPhase) -> &'static str {
    match phase {
        CoordinatorPhase::Admitted => "admitted",
        CoordinatorPhase::Executing => "executing",
        CoordinatorPhase::EvidenceReady => "evidence_ready",
        CoordinatorPhase::Terminalizing => "terminalizing",
        CoordinatorPhase::Recorded => "recorded",
        CoordinatorPhase::TerminalError => "terminal_error",
    }
}

fn evidence_database(_: impl std::fmt::Display) -> CoordinatorError {
    CoordinatorError::Evidence("durable evidence database failed")
}
