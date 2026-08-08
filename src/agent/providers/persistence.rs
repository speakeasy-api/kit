use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::Duration,
};

use agentkit_core::{DataRef, Delta, ItemKind, MetadataMap, Part, PartKind, ToolOutput, Usage};
use agentkit_loop::{LoopError, ModelTurnEvent, ModelTurnResult, TurnRequest};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::{
    agent::{
        driver::restart::EffectCorrelation,
        providers::{
            adapter::StreamCommitFactory,
            streaming::{StreamCommit, StreamLimits},
        },
    },
    domain::{
        ids::ModelCallId,
        lifecycle::FencingToken,
        secret::{DataClass, REDACTED, classify_field, classify_header},
    },
};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq)]
pub struct CommittedStreamChunk {
    pub sequence: u64,
    pub commit_position: u64,
    pub event: ModelTurnEvent,
    pub artifact_refs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommittedStream {
    pub chunks: Vec<CommittedStreamChunk>,
    pub outcome: Option<ModelTurnResult>,
    pub outcome_artifact_refs: Vec<String>,
    pub outcome_position: Option<u64>,
    pub committed_sequence: u64,
    pub watermark: u64,
}

pub struct SqliteStreamCommitFactory {
    path: PathBuf,
    limits: StreamLimits,
    retain_reasoning_summaries: bool,
}

impl SqliteStreamCommitFactory {
    pub fn open(path: impl AsRef<Path>, limits: StreamLimits) -> Result<Self, LoopError> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() || path == Path::new(":memory:") {
            return Err(persistence_error(
                "SQLite stream commits require a file path",
            ));
        }
        validate_limits(limits)?;
        let factory = Self {
            path: path.to_owned(),
            limits,
            retain_reasoning_summaries: false,
        };
        let mut connection = factory.connection()?;
        migrate(&mut connection)?;
        Ok(factory)
    }

    pub fn with_reasoning_summaries(mut self, retain: bool) -> Self {
        self.retain_reasoning_summaries = retain;
        self
    }

    pub fn read(
        &self,
        correlation: &EffectCorrelation,
    ) -> Result<Option<CommittedStream>, LoopError> {
        validate_correlation(correlation)?;
        let connection = self.connection()?;
        let fence = fence_i64(correlation.owner.fencing_token)?;
        let identity = identity_params(correlation, fence);
        let stream = connection
            .query_row(
                "SELECT committed_sequence, outcome_position, outcome, outcome_artifacts
                 FROM provider_streams
                 WHERE attempt_id = ?1 AND model_call_id = ?2 AND fence = ?3
                   AND idempotency_key = ?4",
                identity,
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, Option<Vec<u8>>>(3)?,
                    ))
                },
            )
            .optional()
            .map_err(persistence_error)?;
        let Some((committed_sequence, outcome_position, outcome, outcome_refs)) = stream else {
            return Ok(None);
        };
        let committed_sequence = nonnegative(committed_sequence, "negative committed sequence")?;
        let global_watermark: i64 = connection
            .query_row(
                "SELECT position FROM provider_stream_watermark WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .map_err(persistence_error)?;
        let global_watermark = nonnegative(global_watermark, "negative stream watermark")?;

        let mut statement = connection
            .prepare(
                "SELECT sequence, commit_position, event, artifact_refs
                 FROM provider_stream_chunks
                 WHERE attempt_id = ?1 AND model_call_id = ?2 AND fence = ?3
                   AND idempotency_key = ?4 AND sequence <= ?5 AND commit_position <= ?6
                 ORDER BY sequence",
            )
            .map_err(persistence_error)?;
        let rows = statement
            .query_map(
                params![
                    correlation.owner.attempt_id.to_string(),
                    correlation.operation_id,
                    fence,
                    correlation.idempotency_key,
                    i64::try_from(committed_sequence).map_err(persistence_error)?,
                    i64::try_from(global_watermark).map_err(persistence_error)?,
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .map_err(persistence_error)?;
        let mut chunks = Vec::new();
        for row in rows {
            let (sequence, position, event, stored_refs) = row.map_err(persistence_error)?;
            let sequence = positive(sequence, "invalid committed chunk sequence")?;
            if sequence != chunks.len() as u64 + 1 {
                return Err(persistence_error("non-contiguous committed chunk sequence"));
            }
            let event = decode_chunk(&event, self.limits)?;
            let artifact_refs = decode_refs(&stored_refs, self.limits)?;
            if artifact_refs != event_artifact_refs(&event) {
                return Err(persistence_error(
                    "stream chunk artifact references are corrupt",
                ));
            }
            chunks.push(CommittedStreamChunk {
                sequence,
                commit_position: positive(position, "invalid chunk commit position")?,
                event,
                artifact_refs,
            });
        }
        if chunks.len() as u64 != committed_sequence {
            return Err(persistence_error("committed stream is missing chunks"));
        }

        let (outcome, outcome_artifact_refs, outcome_position) =
            match (outcome_position, outcome, outcome_refs) {
                (None, None, None) => (None, Vec::new(), None),
                (Some(position), Some(bytes), Some(stored_refs)) => {
                    let position = positive(position, "invalid outcome commit position")?;
                    if position > global_watermark {
                        (None, Vec::new(), None)
                    } else {
                        let result =
                            decode_outcome(&bytes, self.limits, self.retain_reasoning_summaries)?;
                        let artifact_refs = decode_refs(&stored_refs, self.limits)?;
                        if artifact_refs != outcome_artifact_refs(&result) {
                            return Err(persistence_error(
                                "stream outcome artifact references are corrupt",
                            ));
                        }
                        (Some(result), artifact_refs, Some(position))
                    }
                }
                _ => return Err(persistence_error("partial stream outcome record")),
            };
        let watermark = outcome_position
            .or_else(|| chunks.last().map(|chunk| chunk.commit_position))
            .unwrap_or(0);
        if chunks
            .windows(2)
            .any(|pair| pair[0].commit_position >= pair[1].commit_position)
            || chunks
                .last()
                .zip(outcome_position)
                .is_some_and(|(chunk, outcome)| chunk.commit_position >= outcome)
        {
            return Err(persistence_error("stream commit watermark is out of order"));
        }
        Ok(Some(CommittedStream {
            chunks,
            outcome,
            outcome_artifact_refs,
            outcome_position,
            committed_sequence,
            watermark,
        }))
    }

    fn connection(&self) -> Result<Connection, LoopError> {
        let connection = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(persistence_error)?;
        configure(&connection)?;
        Ok(connection)
    }
}

impl StreamCommitFactory for SqliteStreamCommitFactory {
    fn for_request(&self, request: &TurnRequest) -> Result<Box<dyn StreamCommit>, LoopError> {
        let correlation = request
            .metadata
            .get(crate::agent::driver::restart::EFFECT_CORRELATION_METADATA)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(persistence_error)?
            .ok_or_else(|| persistence_error("model request has no effect correlation"))?;
        validate_correlation(&correlation)?;
        Ok(Box::new(SqliteStreamCommit {
            path: self.path.clone(),
            correlation,
            limits: self.limits,
            retain_reasoning_summaries: self.retain_reasoning_summaries,
        }))
    }
}

struct SqliteStreamCommit {
    path: PathBuf,
    correlation: EffectCorrelation,
    limits: StreamLimits,
    retain_reasoning_summaries: bool,
}

impl StreamCommit for SqliteStreamCommit {
    fn commit_chunk(&mut self, sequence: u64, event: &ModelTurnEvent) -> Result<(), LoopError> {
        let sequence_index = usize::try_from(sequence)
            .map_err(|_| persistence_error("stream chunk sequence exceeds bounds"))?;
        if sequence == 0 || sequence_index > self.limits.max_items {
            return Err(persistence_error("stream chunk sequence exceeds bounds"));
        }
        let event_bytes = encode_chunk(event, self.limits)?;
        let artifact_refs = event_artifact_refs(event);
        let refs_bytes = encode_refs(&artifact_refs, self.limits)?;
        let mut connection = open(&self.path)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(persistence_error)?;
        claim_fence(&transaction, &self.correlation)?;
        ensure_stream(&transaction, &self.correlation)?;
        let (committed, outcome): (i64, Option<i64>) = transaction
            .query_row(
                "SELECT committed_sequence, outcome_position FROM provider_streams
                 WHERE attempt_id = ?1 AND model_call_id = ?2 AND fence = ?3
                   AND idempotency_key = ?4",
                identity_params(
                    &self.correlation,
                    fence_i64(self.correlation.owner.fencing_token)?,
                ),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(persistence_error)?;
        if outcome.is_some() {
            return Err(persistence_error("cannot append a chunk after the outcome"));
        }
        let committed = nonnegative(committed, "negative committed sequence")?;
        if sequence <= committed {
            let stored: Option<(Vec<u8>, Vec<u8>)> = transaction
                .query_row(
                    "SELECT event, artifact_refs FROM provider_stream_chunks
                     WHERE attempt_id = ?1 AND model_call_id = ?2 AND fence = ?3
                       AND idempotency_key = ?4 AND sequence = ?5",
                    params![
                        self.correlation.owner.attempt_id.to_string(),
                        self.correlation.operation_id,
                        fence_i64(self.correlation.owner.fencing_token)?,
                        self.correlation.idempotency_key,
                        i64::try_from(sequence).map_err(persistence_error)?,
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(persistence_error)?;
            return if stored == Some((event_bytes, refs_bytes)) {
                transaction.commit().map_err(persistence_error)
            } else {
                Err(persistence_error("idempotent stream chunk conflicts"))
            };
        }
        if sequence != committed + 1 {
            return Err(persistence_error("stream chunk sequence is not contiguous"));
        }
        let position = next_position(&transaction)?;
        transaction
            .execute(
                "INSERT INTO provider_stream_chunks (
                     attempt_id, model_call_id, fence, idempotency_key, sequence,
                     commit_position, event, artifact_refs
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    self.correlation.owner.attempt_id.to_string(),
                    self.correlation.operation_id,
                    fence_i64(self.correlation.owner.fencing_token)?,
                    self.correlation.idempotency_key,
                    i64::try_from(sequence).map_err(persistence_error)?,
                    position,
                    event_bytes,
                    refs_bytes,
                ],
            )
            .map_err(persistence_error)?;
        transaction
            .execute(
                "UPDATE provider_streams SET committed_sequence = ?5
                 WHERE attempt_id = ?1 AND model_call_id = ?2 AND fence = ?3
                   AND idempotency_key = ?4",
                params![
                    self.correlation.owner.attempt_id.to_string(),
                    self.correlation.operation_id,
                    fence_i64(self.correlation.owner.fencing_token)?,
                    self.correlation.idempotency_key,
                    i64::try_from(sequence).map_err(persistence_error)?,
                ],
            )
            .map_err(persistence_error)?;
        publish_position(&transaction, position)?;
        transaction.commit().map_err(persistence_error)
    }

    fn commit_outcome(&mut self, result: &ModelTurnResult) -> Result<(), LoopError> {
        let outcome = encode_outcome(result, self.limits, self.retain_reasoning_summaries)?;
        let refs = encode_refs(&outcome_artifact_refs(result), self.limits)?;
        let mut connection = open(&self.path)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(persistence_error)?;
        claim_fence(&transaction, &self.correlation)?;
        ensure_stream(&transaction, &self.correlation)?;
        let existing: Option<(Vec<u8>, Vec<u8>)> = transaction
            .query_row(
                "SELECT outcome, outcome_artifacts FROM provider_streams
                 WHERE attempt_id = ?1 AND model_call_id = ?2 AND fence = ?3
                   AND idempotency_key = ?4 AND outcome_position IS NOT NULL",
                identity_params(
                    &self.correlation,
                    fence_i64(self.correlation.owner.fencing_token)?,
                ),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(persistence_error)?;
        if let Some(existing) = existing {
            return if existing == (outcome, refs) {
                transaction.commit().map_err(persistence_error)
            } else {
                Err(persistence_error("idempotent stream outcome conflicts"))
            };
        }
        let position = next_position(&transaction)?;
        transaction
            .execute(
                "UPDATE provider_streams
                 SET outcome_position = ?5, outcome = ?6, outcome_artifacts = ?7
                 WHERE attempt_id = ?1 AND model_call_id = ?2 AND fence = ?3
                   AND idempotency_key = ?4",
                params![
                    self.correlation.owner.attempt_id.to_string(),
                    self.correlation.operation_id,
                    fence_i64(self.correlation.owner.fencing_token)?,
                    self.correlation.idempotency_key,
                    position,
                    outcome,
                    refs,
                ],
            )
            .map_err(persistence_error)?;
        publish_position(&transaction, position)?;
        transaction.commit().map_err(persistence_error)
    }
}

fn open(path: &Path) -> Result<Connection, LoopError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(persistence_error)?;
    configure(&connection)?;
    Ok(connection)
}

fn configure(connection: &Connection) -> Result<(), LoopError> {
    connection
        .busy_timeout(BUSY_TIMEOUT)
        .map_err(persistence_error)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(persistence_error)?;
    connection
        .pragma_update(None, "synchronous", "FULL")
        .map_err(persistence_error)?;
    let mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(persistence_error)?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(persistence_error("SQLite WAL mode is unavailable"));
    }
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<(), LoopError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(persistence_error)?;
    transaction
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS provider_stream_watermark (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 position INTEGER NOT NULL CHECK (position >= 0)
             );
             INSERT OR IGNORE INTO provider_stream_watermark (singleton, position) VALUES (1, 0);
             CREATE TABLE IF NOT EXISTS provider_stream_fences (
                 attempt_id TEXT PRIMARY KEY,
                 fence INTEGER NOT NULL CHECK (fence >= 0)
             );
             CREATE TABLE IF NOT EXISTS provider_streams (
                 attempt_id TEXT NOT NULL,
                 model_call_id TEXT NOT NULL,
                 fence INTEGER NOT NULL CHECK (fence >= 0),
                 idempotency_key TEXT NOT NULL,
                 committed_sequence INTEGER NOT NULL DEFAULT 0 CHECK (committed_sequence >= 0),
                 outcome_position INTEGER UNIQUE CHECK (outcome_position > 0),
                 outcome BLOB,
                 outcome_artifacts BLOB,
                 PRIMARY KEY (attempt_id, model_call_id, fence, idempotency_key),
                 CHECK ((outcome_position IS NULL AND outcome IS NULL AND outcome_artifacts IS NULL)
                     OR (outcome_position IS NOT NULL AND outcome IS NOT NULL
                         AND outcome_artifacts IS NOT NULL))
             );
             CREATE TABLE IF NOT EXISTS provider_stream_chunks (
                 attempt_id TEXT NOT NULL,
                 model_call_id TEXT NOT NULL,
                 fence INTEGER NOT NULL CHECK (fence >= 0),
                 idempotency_key TEXT NOT NULL,
                 sequence INTEGER NOT NULL CHECK (sequence > 0),
                 commit_position INTEGER NOT NULL UNIQUE CHECK (commit_position > 0),
                 event BLOB NOT NULL,
                 artifact_refs BLOB NOT NULL,
                 PRIMARY KEY (attempt_id, model_call_id, fence, idempotency_key, sequence),
                 FOREIGN KEY (attempt_id, model_call_id, fence, idempotency_key)
                     REFERENCES provider_streams(attempt_id, model_call_id, fence, idempotency_key)
             );",
        )
        .map_err(persistence_error)?;
    transaction.commit().map_err(persistence_error)
}

fn claim_fence(
    transaction: &Transaction<'_>,
    correlation: &EffectCorrelation,
) -> Result<(), LoopError> {
    let fence = fence_i64(correlation.owner.fencing_token)?;
    let now: i64 = transaction
        .query_row(
            "SELECT CAST((julianday('now') - 2440587.5) * 86400000000 AS INTEGER)",
            [],
            |row| row.get(0),
        )
        .map_err(persistence_error)?;
    let owns: bool = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM attempt_driver_claims
                 WHERE run_id = ?1 AND attempt_id = ?2 AND principal_id = ?3
                   AND fence = ?4 AND lease_version = ?5
                   AND expires_at_unix_micros > ?6 AND quiescent = 0
             )",
            params![
                correlation.claim.run_id.to_string(),
                correlation.claim.attempt_id.to_string(),
                correlation.claim.principal_id.to_string(),
                fence,
                i64::try_from(correlation.claim.lease_version).map_err(persistence_error)?,
                now,
            ],
            |row| row.get(0),
        )
        .map_err(persistence_error)?;
    if !owns {
        return Err(persistence_error("stale provider stream fence"));
    }
    Ok(())
}

fn ensure_stream(
    transaction: &Transaction<'_>,
    correlation: &EffectCorrelation,
) -> Result<(), LoopError> {
    transaction
        .execute(
            "INSERT INTO provider_streams (attempt_id, model_call_id, fence, idempotency_key)
             VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
            identity_params(correlation, fence_i64(correlation.owner.fencing_token)?),
        )
        .map_err(persistence_error)?;
    Ok(())
}

fn next_position(transaction: &Transaction<'_>) -> Result<i64, LoopError> {
    let current: i64 = transaction
        .query_row(
            "SELECT position FROM provider_stream_watermark WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .map_err(persistence_error)?;
    current
        .checked_add(1)
        .filter(|position| *position > 0)
        .ok_or_else(|| persistence_error("stream commit position exhausted"))
}

fn publish_position(transaction: &Transaction<'_>, position: i64) -> Result<(), LoopError> {
    transaction
        .execute(
            "UPDATE provider_stream_watermark SET position = ?1 WHERE singleton = 1",
            [position],
        )
        .map_err(persistence_error)?;
    Ok(())
}

fn identity_params(correlation: &EffectCorrelation, fence: i64) -> [rusqlite::types::Value; 4] {
    [
        correlation.owner.attempt_id.to_string().into(),
        correlation.operation_id.clone().into(),
        fence.into(),
        correlation.idempotency_key.clone().into(),
    ]
}

fn fence_i64(fence: FencingToken) -> Result<i64, LoopError> {
    i64::try_from(fence.get()).map_err(|_| persistence_error("stream fence exceeds SQLite range"))
}

fn validate_limits(limits: StreamLimits) -> Result<(), LoopError> {
    if limits.max_bytes == 0
        || limits.max_items == 0
        || limits.max_delta_bytes == 0
        || limits.max_elapsed.is_zero()
    {
        Err(persistence_error("invalid stream persistence limits"))
    } else {
        Ok(())
    }
}

fn validate_key(key: &str) -> Result<(), LoopError> {
    if key.is_empty() || key.len() > 512 {
        Err(persistence_error("invalid stream idempotency key"))
    } else {
        Ok(())
    }
}

fn validate_correlation(correlation: &EffectCorrelation) -> Result<(), LoopError> {
    validate_key(&correlation.idempotency_key)?;
    if correlation.claim.run_id != correlation.run_id
        || correlation.claim.owner() != correlation.owner
    {
        return Err(persistence_error("provider stream driver claim mismatch"));
    }
    ModelCallId::parse(&correlation.operation_id)
        .map(|_| ())
        .map_err(|_| persistence_error("invalid model effect correlation"))
}

fn encode_chunk(event: &ModelTurnEvent, limits: StreamLimits) -> Result<Vec<u8>, LoopError> {
    if matches!(event, ModelTurnEvent::Finished(_)) {
        return Err(persistence_error(
            "terminal outcome cannot be stored as a chunk",
        ));
    }
    validate_chunk_event(event)?;
    match event {
        ModelTurnEvent::Delta(Delta::AppendText { chunk, .. })
            if chunk.len() > limits.max_delta_bytes =>
        {
            return Err(persistence_error("text delta exceeds persistence limit"));
        }
        ModelTurnEvent::Delta(Delta::AppendBytes { chunk, .. })
            if chunk.len() > limits.max_delta_bytes =>
        {
            return Err(persistence_error("byte delta exceeds persistence limit"));
        }
        _ => {}
    }
    let bytes = serde_json::to_vec(event).map_err(persistence_error)?;
    bounded(
        bytes,
        limits.max_bytes,
        "stream chunk exceeds persistence limit",
    )
}

fn decode_chunk(bytes: &[u8], limits: StreamLimits) -> Result<ModelTurnEvent, LoopError> {
    if bytes.len() > limits.max_bytes {
        return Err(persistence_error(
            "stored stream chunk exceeds persistence limit",
        ));
    }
    let event = serde_json::from_slice(bytes).map_err(persistence_error)?;
    let _ = encode_chunk(&event, limits)?;
    Ok(event)
}

fn encode_outcome(
    result: &ModelTurnResult,
    limits: StreamLimits,
    retain_reasoning_summaries: bool,
) -> Result<Vec<u8>, LoopError> {
    validate_outcome(result, retain_reasoning_summaries)?;
    bounded(
        serde_json::to_vec(result).map_err(persistence_error)?,
        limits.max_bytes,
        "stream outcome exceeds persistence limit",
    )
}

fn decode_outcome(
    bytes: &[u8],
    limits: StreamLimits,
    retain_reasoning_summaries: bool,
) -> Result<ModelTurnResult, LoopError> {
    if bytes.len() > limits.max_bytes {
        return Err(persistence_error(
            "stored stream outcome exceeds persistence limit",
        ));
    }
    let result = serde_json::from_slice(bytes).map_err(persistence_error)?;
    let _ = encode_outcome(&result, limits, retain_reasoning_summaries)?;
    Ok(result)
}

fn validate_chunk_event(event: &ModelTurnEvent) -> Result<(), LoopError> {
    match event {
        ModelTurnEvent::Delta(Delta::BeginPart { kind, .. }) if !allowed_part_kind(*kind) => {
            Err(persistence_error("private stream part kind is not durable"))
        }
        ModelTurnEvent::Delta(Delta::ReplaceStructured { value, .. }) => {
            validate_provider_value(value, false)
        }
        ModelTurnEvent::Delta(Delta::SetMetadata { metadata, .. }) => validate_metadata(metadata),
        ModelTurnEvent::Delta(Delta::CommitPart { part }) => validate_part(part, false),
        ModelTurnEvent::ToolCall(call) => {
            validate_metadata(&call.metadata)?;
            validate_provider_value(&call.input, false)
        }
        ModelTurnEvent::Usage(usage) => validate_usage(usage),
        ModelTurnEvent::Finished(_) => Err(persistence_error(
            "terminal outcome cannot be stored as a chunk",
        )),
        ModelTurnEvent::Delta(
            Delta::BeginPart { .. } | Delta::AppendText { .. } | Delta::AppendBytes { .. },
        ) => Ok(()),
    }
}

fn validate_outcome(
    result: &ModelTurnResult,
    retain_reasoning_summaries: bool,
) -> Result<(), LoopError> {
    validate_metadata(&result.metadata)?;
    if let Some(usage) = &result.usage {
        validate_usage(usage)?;
    }
    for item in &result.output_items {
        if item.kind != ItemKind::Assistant {
            return Err(persistence_error(
                "provider outcome item kind is not durable",
            ));
        }
        validate_metadata(&item.metadata)?;
        if let Some(usage) = &item.usage {
            validate_usage(usage)?;
        }
        for part in &item.parts {
            validate_part(part, retain_reasoning_summaries)?;
        }
    }
    Ok(())
}

fn validate_part(part: &Part, allow_summary: bool) -> Result<(), LoopError> {
    let metadata = match part {
        Part::Text(part) => &part.metadata,
        Part::Media(part) => &part.metadata,
        Part::File(part) => &part.metadata,
        Part::Structured(part) => {
            validate_provider_value(&part.value, false)?;
            if let Some(schema) = &part.schema {
                validate_provider_value(schema, false)?;
            }
            &part.metadata
        }
        Part::ToolCall(part) => {
            validate_provider_value(&part.input, false)?;
            if part
                .metadata
                .contains_key(super::openai_subscription::CONTINUATION_METADATA)
                && !super::openai_subscription::durable_tool_call_metadata(&part.metadata)
            {
                return Err(persistence_error(
                    "OpenAI tool continuation metadata is not durable",
                ));
            }
            &part.metadata
        }
        Part::Reasoning(part) if super::openai_subscription::durable_reasoning(part) => {
            return Ok(());
        }
        Part::Reasoning(part)
            if allow_summary
                && part.redacted
                && part.summary.is_some()
                && part.data.is_none()
                && part.metadata.is_empty() =>
        {
            return Ok(());
        }
        Part::Reasoning(_) => {
            return Err(persistence_error("hidden reasoning is not durable"));
        }
        Part::ToolResult(_) | Part::Custom(_) => {
            return Err(persistence_error(
                "provider content part kind is not durable",
            ));
        }
    };
    validate_metadata(metadata)
}

fn validate_usage(usage: &Usage) -> Result<(), LoopError> {
    validate_metadata(&usage.metadata)
}

fn validate_metadata(metadata: &MetadataMap) -> Result<(), LoopError> {
    for (name, value) in metadata {
        validate_metadata_entry(name, value)?;
    }
    Ok(())
}

pub(super) fn validate_metadata_entry(
    name: &str,
    value: &serde_json::Value,
) -> Result<(), LoopError> {
    if private_field(name) {
        return Err(persistence_error(
            "private reasoning metadata is not durable",
        ));
    }
    if classify_field(name) == DataClass::Secret
        && value != &serde_json::Value::String(REDACTED.to_owned())
    {
        return Err(persistence_error("unredacted secret field is not durable"));
    }
    validate_provider_value(value, name.eq_ignore_ascii_case("headers"))
}

fn validate_provider_value(value: &serde_json::Value, headers: bool) -> Result<(), LoopError> {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                validate_provider_value(value, headers)?;
            }
        }
        serde_json::Value::Object(fields) => {
            for (name, value) in fields {
                if private_field(name) {
                    return Err(persistence_error(
                        "private reasoning content is not durable",
                    ));
                }
                let class = if headers {
                    classify_header(name)
                } else {
                    classify_field(name)
                };
                if class == DataClass::Secret
                    && value != &serde_json::Value::String(REDACTED.to_owned())
                {
                    return Err(persistence_error("unredacted secret field is not durable"));
                }
                validate_provider_value(value, name.eq_ignore_ascii_case("headers"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn private_field(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "chain_of_thought"
            | "cot"
            | "hidden_reasoning"
            | "reasoning"
            | "reasoning_content"
            | "thinking"
            | "thinking_content"
    )
}

fn allowed_part_kind(kind: PartKind) -> bool {
    matches!(
        kind,
        PartKind::Text
            | PartKind::Media
            | PartKind::File
            | PartKind::Structured
            | PartKind::ToolCall
    )
}

fn encode_refs(refs: &[String], limits: StreamLimits) -> Result<Vec<u8>, LoopError> {
    if refs.len() > limits.max_items {
        return Err(persistence_error("too many stream artifact references"));
    }
    bounded(
        serde_json::to_vec(refs).map_err(persistence_error)?,
        limits.max_bytes,
        "stream artifact references exceed persistence limit",
    )
}

fn decode_refs(bytes: &[u8], limits: StreamLimits) -> Result<Vec<String>, LoopError> {
    if bytes.len() > limits.max_bytes {
        return Err(persistence_error(
            "stored artifact references exceed persistence limit",
        ));
    }
    let refs: Vec<String> = serde_json::from_slice(bytes).map_err(persistence_error)?;
    if refs.len() > limits.max_items
        || refs
            .iter()
            .any(|reference| !reference.starts_with("blake3:"))
        || refs.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(persistence_error("invalid stored artifact references"));
    }
    Ok(refs)
}

fn bounded(bytes: Vec<u8>, maximum: usize, message: &'static str) -> Result<Vec<u8>, LoopError> {
    if bytes.len() > maximum {
        Err(persistence_error(message))
    } else {
        Ok(bytes)
    }
}

fn event_artifact_refs(event: &ModelTurnEvent) -> Vec<String> {
    let mut refs = BTreeSet::new();
    if let ModelTurnEvent::Delta(Delta::CommitPart { part }) = event {
        collect_part(part, &mut refs);
    }
    refs.into_iter().collect()
}

fn outcome_artifact_refs(result: &ModelTurnResult) -> Vec<String> {
    let mut refs = BTreeSet::new();
    for item in &result.output_items {
        for part in &item.parts {
            collect_part(part, &mut refs);
        }
    }
    refs.into_iter().collect()
}

fn collect_part(part: &Part, refs: &mut BTreeSet<String>) {
    match part {
        Part::Media(part) => collect_data_ref(&part.data, refs),
        Part::File(part) => collect_data_ref(&part.data, refs),
        Part::Reasoning(part) => {
            if let Some(data) = &part.data {
                collect_data_ref(data, refs);
            }
        }
        Part::ToolResult(part) => collect_tool_output(&part.output, refs),
        Part::Custom(part) => {
            if let Some(data) = &part.data {
                collect_data_ref(data, refs);
            }
        }
        Part::Text(_) | Part::Structured(_) | Part::ToolCall(_) => {}
    }
}

fn collect_tool_output(output: &ToolOutput, refs: &mut BTreeSet<String>) {
    match output {
        ToolOutput::Parts(parts) => {
            for part in parts {
                collect_part(part, refs);
            }
        }
        ToolOutput::Files(files) => {
            for file in files {
                collect_data_ref(&file.data, refs);
            }
        }
        ToolOutput::Text(_) | ToolOutput::Structured(_) => {}
    }
}

fn collect_data_ref(data: &DataRef, refs: &mut BTreeSet<String>) {
    if let DataRef::Handle(handle) = data
        && handle.0.starts_with("blake3:")
    {
        refs.insert(handle.0.clone());
    }
}

fn nonnegative(value: i64, message: &'static str) -> Result<u64, LoopError> {
    u64::try_from(value).map_err(|_| persistence_error(message))
}

fn positive(value: i64, message: &'static str) -> Result<u64, LoopError> {
    nonnegative(value, message).and_then(|value| {
        if value == 0 {
            Err(persistence_error(message))
        } else {
            Ok(value)
        }
    })
}

fn persistence_error(error: impl std::fmt::Display) -> LoopError {
    LoopError::Provider(format!("SQLite provider stream: {error}"))
}
