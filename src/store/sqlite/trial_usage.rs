use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    domain::ids::RunId,
    executor::trial::{
        TrialError, TrialUsage, TrialUsageReceipt, TrialUsageReceiptBinding,
        TrialUsageReceiptStore, UsageMeasure, VerifiedTrialUsage,
    },
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct SqliteTrialUsageReceiptStore {
    database: PathBuf,
}

impl SqliteTrialUsageReceiptStore {
    pub fn open(database: impl AsRef<Path>) -> Result<Self, TrialError> {
        let database = database.as_ref().to_owned();
        let mut connection = open(&database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(mismatch)?;
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS trial_usage_receipts (
                     receipt TEXT PRIMARY KEY,
                     run_id TEXT NOT NULL,
                     trial_id TEXT NOT NULL,
                     binding BLOB NOT NULL,
                     evidence_digest TEXT NOT NULL,
                     event_high_watermark INTEGER NOT NULL,
                     terminal_version INTEGER NOT NULL,
                     created_at INTEGER NOT NULL
                 );",
            )
            .map_err(mismatch)?;
        transaction.commit().map_err(mismatch)?;
        Ok(Self { database })
    }

    pub fn mint(&self, run_id: RunId, trial_id: &str) -> Result<TrialUsageReceipt, TrialError> {
        if trial_id.is_empty() {
            return Err(TrialError::UsageReceiptMismatch);
        }
        let mut connection = open(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(mismatch)?;
        let binding = load_binding(&transaction, run_id, trial_id)?;
        let event_high_watermark = load_event_high_watermark(&transaction)?;
        let terminal_version = load_terminal_version(&transaction, run_id)?;
        let evidence = canonical_evidence(
            &transaction,
            run_id,
            &binding,
            event_high_watermark,
            terminal_version,
        )?;
        let evidence_bytes = serde_json::to_vec(&evidence).map_err(serialization)?;
        let digest = sha256(&evidence_bytes);
        let receipt = TrialUsageReceipt::parse(format!("usage-{digest}"))?;
        let binding_bytes = serde_json::to_vec(&binding).map_err(serialization)?;
        let now: i64 = transaction
            .query_row(
                "SELECT CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)",
                [],
                |row| row.get(0),
            )
            .map_err(mismatch)?;
        transaction
            .execute(
                "INSERT INTO trial_usage_receipts
                     (receipt, run_id, trial_id, binding, evidence_digest,
                      event_high_watermark, terminal_version, created_at)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                  ON CONFLICT(receipt) DO NOTHING",
                params![
                    receipt.opaque_id(),
                    run_id.to_string(),
                    trial_id,
                    binding_bytes,
                    digest,
                    event_high_watermark,
                    terminal_version,
                    now
                ],
            )
            .map_err(mismatch)?;
        let stored: (String, String, Vec<u8>, String, u64, u64) = transaction
            .query_row(
                "SELECT run_id, trial_id, binding, evidence_digest,
                        event_high_watermark, terminal_version FROM trial_usage_receipts
                 WHERE receipt = ?1",
                [receipt.opaque_id()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .map_err(mismatch)?;
        if stored
            != (
                run_id.to_string(),
                trial_id.to_owned(),
                serde_json::to_vec(&binding).map_err(serialization)?,
                digest,
                event_high_watermark,
                terminal_version,
            )
        {
            return Err(TrialError::UsageReceiptMismatch);
        }
        transaction.commit().map_err(mismatch)?;
        Ok(receipt)
    }

    pub fn verify_run(
        &self,
        run_id: RunId,
        trial_id: &str,
    ) -> Result<VerifiedTrialUsage, TrialError> {
        let connection = open(&self.database)?;
        let receipts = connection
            .prepare("SELECT receipt FROM trial_usage_receipts WHERE run_id = ?1 AND trial_id = ?2")
            .map_err(mismatch)?
            .query_map(params![run_id.to_string(), trial_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(mismatch)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(mismatch)?;
        let [receipt] = receipts.as_slice() else {
            return Err(TrialError::UsageReceiptMismatch);
        };
        self.verify(&TrialUsageReceipt::parse(receipt.clone())?, trial_id)
    }

    #[cfg(test)]
    pub fn receipt_count(&self) -> Result<u64, TrialError> {
        open(&self.database)?
            .query_row("SELECT count(*) FROM trial_usage_receipts", [], |row| {
                row.get(0)
            })
            .map_err(mismatch)
    }
}

impl TrialUsageReceiptStore for SqliteTrialUsageReceiptStore {
    fn verify(
        &self,
        receipt: &TrialUsageReceipt,
        trial_id: &str,
    ) -> Result<VerifiedTrialUsage, TrialError> {
        let mut connection = open(&self.database)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(mismatch)?;
        let stored: Option<(String, String, Vec<u8>, String, u64, u64)> = transaction
            .query_row(
                "SELECT run_id, trial_id, binding, evidence_digest,
                        event_high_watermark, terminal_version FROM trial_usage_receipts
                  WHERE receipt = ?1",
                [receipt.opaque_id()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(mismatch)?;
        let (
            run_id,
            stored_trial_id,
            stored_binding,
            stored_digest,
            event_high_watermark,
            terminal_version,
        ) = stored.ok_or(TrialError::UsageReceiptMismatch)?;
        if stored_trial_id != trial_id {
            return Err(TrialError::UsageReceiptMismatch);
        }
        let run_id = RunId::parse(&run_id).map_err(|_| TrialError::UsageReceiptMismatch)?;
        let binding = serde_json::from_slice::<TrialUsageReceiptBinding>(&stored_binding)
            .map_err(serialization)?;
        if load_binding(&transaction, run_id, trial_id)? != binding {
            return Err(TrialError::UsageReceiptMismatch);
        }
        let evidence = canonical_evidence(
            &transaction,
            run_id,
            &binding,
            event_high_watermark,
            terminal_version,
        )?;
        let digest = sha256(&serde_json::to_vec(&evidence).map_err(serialization)?);
        if digest != stored_digest || receipt.opaque_id() != format!("usage-{digest}") {
            return Err(TrialError::UsageReceiptMismatch);
        }
        transaction.commit().map_err(mismatch)?;
        Ok(VerifiedTrialUsage {
            binding: binding.clone(),
            provider_request_ids: evidence.provider_request_ids,
            durable_event_positions: evidence.event_positions,
            event_high_watermark,
            terminal_version,
            usage: evidence.usage,
        })
    }
}

#[derive(Debug, Serialize)]
struct CanonicalEvidence {
    schema_version: u16,
    binding: TrialUsageReceiptBinding,
    run_id: String,
    provider: Option<String>,
    model: Option<String>,
    model_snapshot_digest: Option<String>,
    config_snapshot_digest: String,
    attempt_id: String,
    attempt_fence: u64,
    event_high_watermark: u64,
    terminal_version: u64,
    provider_request_ids: Vec<String>,
    event_positions: Vec<u64>,
    reservations: Vec<CanonicalReservation>,
    usage: TrialUsage,
}

#[derive(Debug, Serialize)]
struct CanonicalReservation {
    id: String,
    kind: String,
    state: String,
    dispatch_state: String,
    cost_microusd: u64,
    tokens: u64,
    turns: u64,
    tools: u64,
    processes: u64,
}

#[derive(Debug)]
struct ModelCall {
    reservation_id: String,
    provider: String,
    model: String,
    model_digest: String,
    config_digest: String,
    dispatched: bool,
    completed: bool,
}

fn load_binding(
    transaction: &Transaction<'_>,
    run_id: RunId,
    trial_id: &str,
) -> Result<TrialUsageReceiptBinding, TrialError> {
    transaction
        .query_row(
            "SELECT trial_id, trial_digest, task_digest, model_digest, config_digest,
                    attempt_id, attempt_fence, scheduler_principal_id,
                    scheduler_idempotency_key
             FROM run_to_trial WHERE run_id = ?1 AND trial_id = ?2",
            params![run_id.to_string(), trial_id],
            |row| {
                Ok(TrialUsageReceiptBinding {
                    run_id: run_id.to_string(),
                    trial_id: row.get(0)?,
                    trial_digest: row.get(1)?,
                    task_digest: row.get(2)?,
                    model_digest: row.get(3)?,
                    config_digest: row.get(4)?,
                    attempt_id: row.get(5)?,
                    attempt_fence: row.get(6)?,
                    scheduler_principal_id: row.get(7)?,
                    scheduler_idempotency_key: row.get(8)?,
                })
            },
        )
        .optional()
        .map_err(mismatch)?
        .ok_or(TrialError::UsageReceiptMismatch)
}

fn load_event_high_watermark(transaction: &Transaction<'_>) -> Result<u64, TrialError> {
    let watermark: i64 = transaction
        .query_row(
            "SELECT position FROM commit_watermark WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(mismatch)?;
    u64::try_from(watermark).map_err(|_| TrialError::UsageReceiptMismatch)
}

fn load_terminal_version(transaction: &Transaction<'_>, run_id: RunId) -> Result<u64, TrialError> {
    let version: i64 = transaction
        .query_row(
            "SELECT terminal_version FROM scheduler_runs WHERE run_id = ?1",
            [run_id.to_string()],
            |row| row.get(0),
        )
        .map_err(mismatch)?;
    u64::try_from(version).map_err(|_| TrialError::UsageReceiptMismatch)
}

fn canonical_evidence(
    transaction: &Transaction<'_>,
    run_id: RunId,
    binding: &TrialUsageReceiptBinding,
    event_high_watermark: u64,
    terminal_version: u64,
) -> Result<CanonicalEvidence, TrialError> {
    let run = transaction
        .query_row(
            "SELECT attempt_id, attempt_fence, config_digest, principal_id, idempotency_key,
                    phase, executor_quiescent, terminal_version
             FROM scheduler_runs WHERE run_id = ?1",
            [run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()
        .map_err(mismatch)?;
    let (
        attempt_id,
        attempt_fence,
        scheduler_config,
        scheduler_principal,
        scheduler_idempotency_key,
        phase,
        executor_quiescent,
        stored_terminal_version,
    ) = run.ok_or(TrialError::UsageReceiptMismatch)?;
    let attempt_fence =
        u64::try_from(attempt_fence).map_err(|_| TrialError::UsageReceiptMismatch)?;
    if attempt_id.is_empty()
        || attempt_fence == 0
        || phase != "terminal"
        || executor_quiescent != 1
        || u64::try_from(stored_terminal_version).ok() != Some(terminal_version)
        || binding.run_id != run_id.to_string()
        || binding.attempt_id != attempt_id
        || binding.attempt_fence != attempt_fence
        || binding.scheduler_principal_id != scheduler_principal
        || binding.scheduler_idempotency_key != scheduler_idempotency_key
        || normalize_digest(&binding.config_digest) != normalize_digest(&scheduler_config)
        || event_high_watermark == 0
        || event_high_watermark > load_event_high_watermark(transaction)?
    {
        return Err(TrialError::UsageReceiptMismatch);
    }
    let late_effect: bool = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM events
                 WHERE correlation_id = ?1 AND commit_position > ?2
                   AND event_type IN (
                       'model_call.intent', 'model_call.dispatched', 'model_call.outcome',
                       'capability.invocation_intent', 'capability.invocation_dispatched',
                       'capability.invocation_outcome')
             )",
            params![run_id.to_string(), event_high_watermark],
            |row| row.get(0),
        )
        .map_err(mismatch)?;
    if late_effect {
        return Err(TrialError::UsageReceiptMismatch);
    }

    let mut statement = transaction
        .prepare(
            "SELECT commit_position, event_type, attempt_id, payload
             FROM events
             WHERE correlation_id = ?1
               AND commit_position <= ?2
               AND event_type IN (
                   'model_call.intent', 'model_call.dispatched', 'model_call.outcome',
                   'capability.invocation_intent', 'capability.invocation_dispatched',
                   'capability.invocation_outcome')
             ORDER BY commit_position",
        )
        .map_err(mismatch)?;
    let rows = statement
        .query_map(params![run_id.to_string(), event_high_watermark], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .map_err(mismatch)?;

    let mut calls = BTreeMap::<String, ModelCall>::new();
    let mut tools = BTreeMap::<String, bool>::new();
    let mut request_ids = Vec::new();
    let mut positions = Vec::new();
    let mut input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    let mut cost_microusd = 0_u64;
    let mut provider = None;
    let mut model = None;
    let mut model_digest = None;
    let mut config_digest = None;

    for row in rows {
        let (position, event_type, event_attempt, payload) = row.map_err(mismatch)?;
        let position = u64::try_from(position).map_err(|_| TrialError::UsageReceiptMismatch)?;
        if position == 0 || event_attempt.as_deref() != Some(attempt_id.as_str()) {
            return Err(TrialError::UsageReceiptMismatch);
        }
        let value: serde_json::Value = serde_json::from_slice(&payload).map_err(serialization)?;
        if value
            .get("schema_version")
            .and_then(serde_json::Value::as_u64)
            != Some(1)
            || value.get("attempt_id").and_then(serde_json::Value::as_str)
                != Some(attempt_id.as_str())
            || value
                .get("attempt_fence")
                .and_then(serde_json::Value::as_u64)
                != Some(attempt_fence)
        {
            return Err(TrialError::UsageReceiptMismatch);
        }
        positions.push(position);
        match event_type.as_str() {
            "model_call.intent" => {
                let id = required_str(&value, "model_call_id")?;
                let call = ModelCall {
                    reservation_id: required_str(&value, "reservation_id")?.to_owned(),
                    provider: required_str(&value, "provider")?.to_owned(),
                    model: required_str(&value, "model")?.to_owned(),
                    model_digest: required_str(&value, "model_snapshot_digest")?.to_owned(),
                    config_digest: required_str(&value, "config_snapshot_digest")?.to_owned(),
                    dispatched: false,
                    completed: false,
                };
                if calls.insert(id.to_owned(), call).is_some() {
                    return Err(TrialError::UsageReceiptMismatch);
                }
            }
            "model_call.dispatched" => {
                let call = calls
                    .get_mut(required_str(&value, "model_call_id")?)
                    .ok_or(TrialError::UsageReceiptMismatch)?;
                if call.dispatched || required_str(&value, "reservation_id")? != call.reservation_id
                {
                    return Err(TrialError::UsageReceiptMismatch);
                }
                call.dispatched = true;
            }
            "model_call.outcome" => {
                let call = calls
                    .get_mut(required_str(&value, "model_call_id")?)
                    .ok_or(TrialError::UsageReceiptMismatch)?;
                if !call.dispatched
                    || call.completed
                    || required_str(&value, "reservation_id")? != call.reservation_id
                    || value.get("status").and_then(serde_json::Value::as_str) != Some("succeeded")
                    || value.get("charged").and_then(serde_json::Value::as_bool) != Some(true)
                {
                    return Err(TrialError::UsageReceiptMismatch);
                }
                call.completed = true;
                let request_id = required_str(&value, "provider_request_id")?;
                if request_id.len() > 256
                    || !request_id.bytes().all(|byte| byte.is_ascii_graphic())
                    || request_ids.iter().any(|stored| stored == request_id)
                {
                    return Err(TrialError::UsageReceiptMismatch);
                }
                request_ids.push(request_id.to_owned());
                let usage = value.get("usage").ok_or(TrialError::UsageReceiptMismatch)?;
                let tokens = usage
                    .get("tokens")
                    .ok_or(TrialError::UsageReceiptMismatch)?;
                let input = required_u64(tokens, "input_tokens")?
                    .checked_add(optional_u64(tokens, "cached_input_tokens")?)
                    .and_then(|value| {
                        value.checked_add(optional_u64(tokens, "cache_write_input_tokens").ok()?)
                    })
                    .ok_or(TrialError::UsageReceiptMismatch)?;
                let output = required_u64(tokens, "output_tokens")?
                    .checked_add(optional_u64(tokens, "reasoning_tokens")?)
                    .ok_or(TrialError::UsageReceiptMismatch)?;
                let cost = usage.get("cost").ok_or(TrialError::UsageReceiptMismatch)?;
                if required_str(cost, "currency")? != "USD" {
                    return Err(TrialError::UsageReceiptMismatch);
                }
                let micros = decimal_microusd(required_str(cost, "provider_amount")?)?;
                if input == 0 || output == 0 || micros == 0 {
                    return Err(TrialError::UsageReceiptMismatch);
                }
                input_tokens = input_tokens
                    .checked_add(input)
                    .ok_or(TrialError::UsageReceiptMismatch)?;
                output_tokens = output_tokens
                    .checked_add(output)
                    .ok_or(TrialError::UsageReceiptMismatch)?;
                cost_microusd = cost_microusd
                    .checked_add(micros)
                    .ok_or(TrialError::UsageReceiptMismatch)?;
            }
            "capability.invocation_intent" => {
                let id = required_str(&value, "invocation_id")?;
                if tools.insert(id.to_owned(), false).is_some() {
                    return Err(TrialError::UsageReceiptMismatch);
                }
            }
            "capability.invocation_dispatched" => {
                let state = tools
                    .get_mut(required_str(&value, "invocation_id")?)
                    .ok_or(TrialError::UsageReceiptMismatch)?;
                if *state {
                    return Err(TrialError::UsageReceiptMismatch);
                }
                *state = true;
            }
            "capability.invocation_outcome" => {
                let state = tools
                    .remove(required_str(&value, "invocation_id")?)
                    .ok_or(TrialError::UsageReceiptMismatch)?;
                if !state {
                    return Err(TrialError::UsageReceiptMismatch);
                }
            }
            _ => unreachable!(),
        }
    }
    if calls.values().any(|call| !call.completed) || !tools.is_empty() {
        return Err(TrialError::UsageReceiptMismatch);
    }

    for call in calls.values() {
        set_same(&mut provider, &call.provider)?;
        set_same(&mut model, &call.model)?;
        set_same(&mut model_digest, &call.model_digest)?;
        set_same(&mut config_digest, &call.config_digest)?;
    }
    if model_digest.as_deref() != Some(binding.model_digest.as_str()) {
        return Err(TrialError::UsageReceiptMismatch);
    }
    let reservations = load_reservations(transaction, run_id, &attempt_id, attempt_fence)?;
    let model_reservation_ids = calls
        .values()
        .map(|call| call.reservation_id.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if reservations.iter().any(|reservation| {
        !matches!(reservation.state.as_str(), "reconciled" | "released")
            || reservation.state == "reconciled" && reservation.dispatch_state != "dispatched"
            || reservation.state == "released"
                && !matches!(
                    reservation.dispatch_state.as_str(),
                    "undispatched" | "canceled"
                )
            || reservation.kind == "model"
                && !model_reservation_ids.contains(reservation.id.as_str())
    }) || model_reservation_ids.iter().any(|id| {
        !reservations
            .iter()
            .any(|reservation| reservation.id == **id && reservation.kind == "model")
    }) {
        return Err(TrialError::UsageReceiptMismatch);
    }
    let normalized_scheduler = normalize_digest(&scheduler_config);
    if config_digest
        .as_deref()
        .map(|digest| digest.strip_prefix("sha256:").unwrap_or(digest))
        .is_some_and(|digest| digest != normalized_scheduler)
    {
        return Err(TrialError::UsageReceiptMismatch);
    }
    let turns = u64::try_from(calls.len()).map_err(|_| TrialError::UsageReceiptMismatch)?;
    let tool_event_count = positions
        .len()
        .checked_sub(
            calls
                .len()
                .checked_mul(3)
                .ok_or(TrialError::UsageReceiptMismatch)?,
        )
        .filter(|count| count % 3 == 0)
        .ok_or(TrialError::UsageReceiptMismatch)?;
    let tool_calls =
        u64::try_from(tool_event_count / 3).map_err(|_| TrialError::UsageReceiptMismatch)?;
    let model_reservations = reservations
        .iter()
        .filter(|reservation| reservation.kind == "model")
        .collect::<Vec<_>>();
    let reservation_cost = model_reservations
        .iter()
        .try_fold(0_u64, |total, reservation| {
            total.checked_add(reservation.cost_microusd)
        });
    let reservation_tokens = model_reservations
        .iter()
        .try_fold(0_u64, |total, reservation| {
            total.checked_add(reservation.tokens)
        });
    let reservation_turns = model_reservations
        .iter()
        .try_fold(0_u64, |total, reservation| {
            total.checked_add(reservation.turns)
        });
    let processes = reservations
        .iter()
        .filter(|reservation| {
            reservation.kind == "process"
                && matches!(reservation.state.as_str(), "debited" | "reconciled")
        })
        .try_fold(0_u64, |total, reservation| {
            total.checked_add(reservation.processes)
        })
        .ok_or(TrialError::UsageReceiptMismatch)?;
    let total_tokens = input_tokens.checked_add(output_tokens);
    if turns > 0
        && (request_ids.len() != calls.len()
            || model_reservations.iter().any(|reservation| {
                reservation.state != "reconciled"
                    || reservation.dispatch_state != "dispatched"
                    || reservation.kind != "model"
            })
            || reservation_cost != Some(cost_microusd)
            || reservation_tokens != total_tokens
            || reservation_turns != Some(turns))
    {
        return Err(TrialError::UsageReceiptMismatch);
    }
    if positions.is_empty() {
        return Err(TrialError::UsageReceiptMismatch);
    }
    Ok(CanonicalEvidence {
        schema_version: 1,
        binding: binding.clone(),
        run_id: run_id.to_string(),
        provider,
        model,
        model_snapshot_digest: model_digest,
        config_snapshot_digest: config_digest.unwrap_or(scheduler_config),
        attempt_id,
        attempt_fence,
        event_high_watermark,
        terminal_version,
        provider_request_ids: request_ids,
        event_positions: positions,
        reservations,
        usage: TrialUsage {
            turns: UsageMeasure::Measured(turns),
            input_tokens: UsageMeasure::Measured(input_tokens),
            output_tokens: UsageMeasure::Measured(output_tokens),
            cost_microusd: UsageMeasure::Measured(cost_microusd),
            tool_calls: UsageMeasure::Measured(tool_calls),
            processes: UsageMeasure::Measured(processes),
        },
    })
}

fn load_reservations(
    transaction: &Transaction<'_>,
    run_id: RunId,
    attempt_id: &str,
    attempt_fence: u64,
) -> Result<Vec<CanonicalReservation>, TrialError> {
    let mut statement = transaction
        .prepare(
            "SELECT reservation_id, kind, state, dispatch_state, cost, tokens, turns, tools, processes
             FROM scheduler_reservations
             WHERE run_id = ?1 AND attempt_id = ?2 AND attempt_fence = ?3
             ORDER BY reservation_id",
        )
        .map_err(mismatch)?;
    statement
        .query_map(
            params![run_id.to_string(), attempt_id, attempt_fence],
            |row| {
                Ok(CanonicalReservation {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    state: row.get(2)?,
                    dispatch_state: row.get(3)?,
                    cost_microusd: row.get(4)?,
                    tokens: row.get(5)?,
                    turns: row.get(6)?,
                    tools: row.get(7)?,
                    processes: row.get(8)?,
                })
            },
        )
        .map_err(mismatch)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(mismatch)
}

fn required_str<'a>(value: &'a serde_json::Value, field: &str) -> Result<&'a str, TrialError> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .ok_or(TrialError::UsageReceiptMismatch)
}

fn required_u64(value: &serde_json::Value, field: &str) -> Result<u64, TrialError> {
    value
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .ok_or(TrialError::UsageReceiptMismatch)
}

fn optional_u64(value: &serde_json::Value, field: &str) -> Result<u64, TrialError> {
    match value.get(field) {
        None | Some(serde_json::Value::Null) => Ok(0),
        Some(value) => value.as_u64().ok_or(TrialError::UsageReceiptMismatch),
    }
}

fn decimal_microusd(value: &str) -> Result<u64, TrialError> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 6
    {
        return Err(TrialError::UsageReceiptMismatch);
    }
    whole
        .parse::<u64>()
        .ok()
        .and_then(|whole| whole.checked_mul(1_000_000))
        .and_then(|micros| {
            let fraction = if fraction.is_empty() {
                0
            } else {
                fraction.parse::<u64>().ok()? * 10_u64.pow((6 - fraction.len()) as u32)
            };
            micros.checked_add(fraction)
        })
        .ok_or(TrialError::UsageReceiptMismatch)
}

fn set_same(slot: &mut Option<String>, value: &str) -> Result<(), TrialError> {
    match slot {
        Some(stored) if stored != value => Err(TrialError::UsageReceiptMismatch),
        Some(_) => Ok(()),
        None => {
            *slot = Some(value.to_owned());
            Ok(())
        }
    }
}

fn normalize_digest(digest: &str) -> &str {
    digest.strip_prefix("sha256:").unwrap_or(digest)
}

fn open(path: &Path) -> Result<Connection, TrialError> {
    let connection = Connection::open(path).map_err(mismatch)?;
    connection.busy_timeout(BUSY_TIMEOUT).map_err(mismatch)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(mismatch)?;
    Ok(connection)
}

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn mismatch(_: impl std::fmt::Display) -> TrialError {
    TrialError::UsageReceiptMismatch
}

fn serialization(error: impl std::fmt::Display) -> TrialError {
    TrialError::Serialization(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (PathBuf, RunId, TrialUsageReceiptBinding) {
        let path = std::env::temp_dir().join(format!(
            "kit-usage-receipt-{}-{}.sqlite",
            std::process::id(),
            getrandom::u64().unwrap()
        ));
        let run_id = RunId::from_stable_bytes(b"receipt-run");
        let attempt = "attempt_00000000000000000000000001";
        let reservation = "00112233445566778899aabbccddeeff";
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE commit_watermark (singleton INTEGER PRIMARY KEY, position INTEGER NOT NULL);
                 INSERT INTO commit_watermark VALUES (1, 3);
                 CREATE TABLE events (
                     commit_position INTEGER PRIMARY KEY, event_type TEXT NOT NULL,
                     correlation_id TEXT NOT NULL, attempt_id TEXT, payload BLOB NOT NULL
                 );
                 CREATE TABLE scheduler_runs (
                      run_id TEXT PRIMARY KEY, attempt_id TEXT, attempt_fence INTEGER NOT NULL,
                      config_digest TEXT NOT NULL, principal_id TEXT NOT NULL,
                      idempotency_key TEXT NOT NULL, phase TEXT NOT NULL,
                      executor_quiescent INTEGER NOT NULL, terminal_version INTEGER NOT NULL
                  );
                  CREATE TABLE run_to_trial (
                      run_id TEXT PRIMARY KEY, trial_id TEXT NOT NULL, trial_digest TEXT NOT NULL,
                      task_digest TEXT NOT NULL, model_digest TEXT NOT NULL,
                      config_digest TEXT NOT NULL, attempt_id TEXT NOT NULL,
                      attempt_fence INTEGER NOT NULL, scheduler_principal_id TEXT NOT NULL,
                      scheduler_idempotency_key TEXT NOT NULL, created_at INTEGER NOT NULL
                  );
                 CREATE TABLE scheduler_reservations (
                     reservation_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, attempt_id TEXT,
                     attempt_fence INTEGER, kind TEXT NOT NULL, state TEXT NOT NULL,
                     dispatch_state TEXT NOT NULL, cost INTEGER NOT NULL, tokens INTEGER NOT NULL,
                     turns INTEGER NOT NULL, tools INTEGER NOT NULL, processes INTEGER NOT NULL
                 );",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO scheduler_runs VALUES
                 (?1, ?2, 1, ?3, 'principal_00000000000000000000000001',
                  'receipt-test', 'terminal', 1, 1)",
                params![
                    run_id.to_string(),
                    attempt,
                    "3333333333333333333333333333333333333333333333333333333333333333"
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO scheduler_reservations VALUES
                 (?1, ?2, ?3, 1, 'model', 'reconciled', 'dispatched', 6, 9, 1, 0, 0)",
                params![reservation, run_id.to_string(), attempt],
            )
            .unwrap();
        let common = serde_json::json!({
            "schema_version": 1,
            "model_call_id": "model_call_00000000000000000000000001",
            "attempt_id": attempt,
            "attempt_fence": 1,
            "reservation_id": reservation,
        });
        let mut intent = common.clone();
        intent["provider"] = "deterministic-test".into();
        intent["model"] = "fake-deterministic-v1".into();
        intent["model_snapshot_digest"] =
            "sha256:2222222222222222222222222222222222222222222222222222222222222222".into();
        intent["config_snapshot_digest"] =
            "sha256:3333333333333333333333333333333333333333333333333333333333333333".into();
        let mut outcome = common.clone();
        outcome["status"] = "succeeded".into();
        outcome["charged"] = true.into();
        outcome["provider_request_id"] = "fake-response-1".into();
        outcome["usage"] = serde_json::json!({
            "tokens": {
                "input_tokens": 4,
                "output_tokens": 2,
                "reasoning_tokens": 3,
                "cached_input_tokens": 0,
                "cache_write_input_tokens": 0
            },
            "cost": {"amount": 0.000006, "currency": "USD", "provider_amount": "0.000006"},
            "metadata": {}
        });
        for (position, event_type, payload) in [
            (1, "model_call.intent", intent),
            (2, "model_call.dispatched", common),
            (3, "model_call.outcome", outcome),
        ] {
            connection
                .execute(
                    "INSERT INTO events VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        position,
                        event_type,
                        run_id.to_string(),
                        attempt,
                        serde_json::to_vec(&payload).unwrap()
                    ],
                )
                .unwrap();
        }
        let binding = TrialUsageReceiptBinding {
            run_id: run_id.to_string(),
            trial_id: "receipt-trial".to_owned(),
            trial_digest: "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                .to_owned(),
            task_digest: "sha256:4444444444444444444444444444444444444444444444444444444444444444"
                .to_owned(),
            model_digest: "sha256:2222222222222222222222222222222222222222222222222222222222222222"
                .to_owned(),
            config_digest:
                "sha256:3333333333333333333333333333333333333333333333333333333333333333".to_owned(),
            attempt_id: attempt.to_owned(),
            attempt_fence: 1,
            scheduler_principal_id: "principal_00000000000000000000000001".to_owned(),
            scheduler_idempotency_key: "receipt-test".to_owned(),
        };
        connection
            .execute(
                "INSERT INTO run_to_trial VALUES
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0)",
                params![
                    binding.run_id,
                    binding.trial_id,
                    binding.trial_digest,
                    binding.task_digest,
                    binding.model_digest,
                    binding.config_digest,
                    binding.attempt_id,
                    binding.attempt_fence,
                    binding.scheduler_principal_id,
                    binding.scheduler_idempotency_key,
                ],
            )
            .unwrap();
        (path, run_id, binding)
    }

    #[test]
    fn receipt_is_canonical_and_reverified_from_durable_events() {
        let (path, run_id, binding) = fixture();
        let store = SqliteTrialUsageReceiptStore::open(&path).unwrap();
        let receipt = store.mint(run_id, &binding.trial_id).unwrap();
        let verified = store.verify(&receipt, &binding.trial_id).unwrap();
        assert_eq!(verified.provider_request_ids, ["fake-response-1"]);
        assert_eq!(verified.durable_event_positions, [1, 2, 3]);
        assert_eq!(verified.usage.turns, UsageMeasure::Measured(1));
        assert_eq!(verified.usage.input_tokens, UsageMeasure::Measured(4));
        assert_eq!(verified.usage.output_tokens, UsageMeasure::Measured(5));
        assert_eq!(verified.usage.cost_microusd, UsageMeasure::Measured(6));

        Connection::open(&path)
            .unwrap()
            .execute("UPDATE scheduler_reservations SET state = 'released'", [])
            .unwrap();
        assert!(matches!(
            store.verify(&receipt, &binding.trial_id),
            Err(TrialError::UsageReceiptMismatch)
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn provider_call_with_missing_or_zero_usage_cannot_mint() {
        let (path, run_id, binding) = fixture();
        let connection = Connection::open(&path).unwrap();
        let payload: Vec<u8> = connection
            .query_row(
                "SELECT payload FROM events WHERE event_type = 'model_call.outcome'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let mut payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        payload["usage"]["tokens"]["input_tokens"] = 0.into();
        connection
            .execute(
                "UPDATE events SET payload = ?1 WHERE event_type = 'model_call.outcome'",
                [serde_json::to_vec(&payload).unwrap()],
            )
            .unwrap();
        let store = SqliteTrialUsageReceiptStore::open(&path).unwrap();
        assert!(matches!(
            store.mint(run_id, &binding.trial_id),
            Err(TrialError::UsageReceiptMismatch)
        ));
        assert_eq!(store.receipt_count().unwrap(), 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nonterminal_or_pending_run_cannot_mint() {
        for update in [
            "UPDATE scheduler_runs SET phase = 'admitted', executor_quiescent = 0, terminal_version = 0",
            "UPDATE scheduler_reservations SET state = 'reserved'",
        ] {
            let (path, run_id, binding) = fixture();
            Connection::open(&path)
                .unwrap()
                .execute(update, [])
                .unwrap();
            let store = SqliteTrialUsageReceiptStore::open(&path).unwrap();
            assert!(matches!(
                store.mint(run_id, &binding.trial_id),
                Err(TrialError::UsageReceiptMismatch)
            ));
            assert_eq!(store.receipt_count().unwrap(), 0);
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn run_model_config_attempt_and_fence_substitution_cannot_mint() {
        let (path, run_id, binding) = fixture();
        let store = SqliteTrialUsageReceiptStore::open(&path).unwrap();
        assert!(matches!(
            store.mint(RunId::from_stable_bytes(b"other-run"), &binding.trial_id),
            Err(TrialError::UsageReceiptMismatch)
        ));
        assert!(matches!(
            store.mint(run_id, "other-trial"),
            Err(TrialError::UsageReceiptMismatch)
        ));
        let _ = std::fs::remove_file(path);

        for (field, value) in [
            (
                "model_snapshot_digest",
                serde_json::Value::String(format!("sha256:{}", "9".repeat(64))),
            ),
            (
                "config_snapshot_digest",
                serde_json::Value::String(format!("sha256:{}", "8".repeat(64))),
            ),
            (
                "attempt_id",
                serde_json::Value::String("attempt_00000000000000000000000002".to_owned()),
            ),
            ("attempt_fence", serde_json::Value::from(2)),
        ] {
            let (path, run_id, binding) = fixture();
            let connection = Connection::open(&path).unwrap();
            let payload: Vec<u8> = connection
                .query_row(
                    "SELECT payload FROM events WHERE event_type = 'model_call.intent'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let mut payload: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            payload[field] = value;
            connection
                .execute(
                    "UPDATE events SET payload = ?1 WHERE event_type = 'model_call.intent'",
                    [serde_json::to_vec(&payload).unwrap()],
                )
                .unwrap();
            let store = SqliteTrialUsageReceiptStore::open(&path).unwrap();
            assert!(matches!(
                store.mint(run_id, &binding.trial_id),
                Err(TrialError::UsageReceiptMismatch)
            ));
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn finalized_receipt_rejects_late_effects_and_keeps_exact_terminal_usage() {
        let (path, run_id, binding) = fixture();
        let store = SqliteTrialUsageReceiptStore::open(&path).unwrap();
        let receipt = store.mint(run_id, &binding.trial_id).unwrap();
        let verified = store.verify(&receipt, &binding.trial_id).unwrap();
        assert_eq!(verified.event_high_watermark, 3);
        assert_eq!(verified.terminal_version, 1);
        assert_eq!(verified.usage.input_tokens, UsageMeasure::Measured(4));
        assert_eq!(verified.usage.output_tokens, UsageMeasure::Measured(5));
        assert_eq!(verified.usage.cost_microusd, UsageMeasure::Measured(6));

        let connection = Connection::open(&path).unwrap();
        let payload: Vec<u8> = connection
            .query_row(
                "SELECT payload FROM events WHERE event_type = 'model_call.intent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO events VALUES (4, 'model_call.intent', ?1, ?2, ?3)",
                params![run_id.to_string(), binding.attempt_id, payload],
            )
            .unwrap();
        connection
            .execute("UPDATE commit_watermark SET position = 4", [])
            .unwrap();
        assert!(matches!(
            store.verify(&receipt, &binding.trial_id),
            Err(TrialError::UsageReceiptMismatch)
        ));
        assert_eq!(store.receipt_count().unwrap(), 1);
        let _ = std::fs::remove_file(path);
    }
}
