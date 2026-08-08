use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    future::Future,
    pin::Pin,
    sync::Mutex,
};

use agentkit_core::{CancellationController, FinishReason, Item, Part, TurnCancellation, Usage};
use agentkit_loop::{
    Agent, AgentBuilder, LoopError, ModelAdapter, ModelSession, ModelTurn, ModelTurnEvent,
    ModelTurnResult, SessionConfig, TurnRequest,
};
use agentkit_tools_core::{ApprovalRequest, ToolRequest};
use serde::{Deserialize, Serialize};

use crate::{
    agent::{
        agentkit_bridge::mapping::{CanonicalItem, from_agentkit_item, to_agentkit_item},
        driver::{
            attempt::{AttemptDriver, AttemptDriverError},
            waiting::{WaitingKind, WaitingResolution, WaitingResolved, WaitingState},
        },
    },
    api::service::{AttemptDriverClaim, AttemptProjection},
    domain::{
        commands::ExpectedVersion,
        crypto::sha256,
        events::{ArtifactRef, EntityId, EventType, SchemaVersion, TraceId, UtcDateTime},
        ids::{CommandId, EventId, RunId},
        lifecycle::AttemptOwnership,
    },
    store::sqlite::{
        append::{
            AppendCommand, AppendOutcome, CrashPoint, ExpectedStreamVersion, NewEvent, SqliteStore,
            StoreError, StoredEvent,
        },
        idempotency::{CanonicalRequestDigest, IdempotencyKey, IdempotencyScope},
    },
};

pub const EFFECT_JOURNAL_EVENT: &str = "agent.effect_journal";
pub const EFFECT_CORRELATION_METADATA: &str = "kit.effect_correlation";
const LOOP_COMMAND: &str = "agent.effect_journal.append";
const RECORD_SCHEMA_VERSION: u16 = 1;

type DriverParts = (Vec<Item>, Vec<Item>, Option<CommittedModelOutcome>);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeBoundary {
    BeforeModelDispatch,
    AfterModelOutcome,
    AfterToolOutcome,
    TurnEnd,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommittedModelOutcome {
    pub finish_reason: FinishReason,
    pub output_items: Vec<CanonicalItem>,
    pub usage: Option<Usage>,
    pub metadata: agentkit_core::MetadataMap,
    pub model: Option<String>,
    pub response_id: Option<String>,
}

impl CommittedModelOutcome {
    pub fn from_agentkit(result: &ModelTurnResult) -> Self {
        Self {
            finish_reason: result.finish_reason.clone(),
            output_items: result.output_items.iter().map(from_agentkit_item).collect(),
            usage: result.usage.clone(),
            metadata: result.metadata.clone(),
            model: result.model.clone(),
            response_id: result.response_id.clone(),
        }
    }

    fn to_agentkit(&self) -> ModelTurnResult {
        ModelTurnResult {
            finish_reason: self.finish_reason.clone(),
            output_items: self.output_items.iter().map(to_agentkit_item).collect(),
            usage: self.usage.clone(),
            metadata: self.metadata.clone(),
            model: self.model.clone(),
            response_id: self.response_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct BoundarySnapshot {
    pub boundary: SafeBoundary,
    pub transcript: Vec<CanonicalItem>,
    /// Items at and after this index are submitted through AgentKit's input queue.
    pub resume_index: Option<usize>,
    /// A committed provider result replayed in-process, never dispatched again.
    pub model_outcome: Option<CommittedModelOutcome>,
}

impl BoundarySnapshot {
    pub fn normalized_transcript(&self) -> Vec<Item> {
        self.transcript.iter().map(to_agentkit_item).collect()
    }

    fn validate(&self) -> Result<(), RestartError> {
        if self
            .resume_index
            .is_some_and(|index| index > self.transcript.len())
        {
            return Err(RestartError::InvalidBoundary(
                "resume index exceeds transcript length",
            ));
        }
        match self.boundary {
            SafeBoundary::BeforeModelDispatch
                if self
                    .resume_index
                    .is_none_or(|index| index >= self.transcript.len()) =>
            {
                Err(RestartError::InvalidBoundary(
                    "model dispatch boundary has no pending input",
                ))
            }
            SafeBoundary::AfterModelOutcome => {
                let outcome = self
                    .model_outcome
                    .as_ref()
                    .ok_or(RestartError::InvalidBoundary(
                        "model outcome boundary has no outcome",
                    ))?;
                let pre_outcome_len = self
                    .transcript
                    .len()
                    .checked_sub(outcome.output_items.len())
                    .ok_or(RestartError::InvalidBoundary(
                        "model outcome is longer than transcript",
                    ))?;
                if !self.transcript.ends_with(&outcome.output_items)
                    || self
                        .resume_index
                        .is_none_or(|index| index >= pre_outcome_len)
                {
                    Err(RestartError::InvalidBoundary(
                        "model outcome does not match transcript or dispatched input",
                    ))
                } else {
                    Ok(())
                }
            }
            SafeBoundary::AfterToolOutcome
                if self
                    .resume_index
                    .is_none_or(|index| index >= self.transcript.len()) =>
            {
                Err(RestartError::InvalidBoundary(
                    "tool outcome boundary has no resumable result",
                ))
            }
            SafeBoundary::TurnEnd
                if self.resume_index.is_some() || self.model_outcome.is_some() =>
            {
                Err(RestartError::InvalidBoundary(
                    "turn-end boundary must be passive",
                ))
            }
            _ if self.boundary != SafeBoundary::AfterModelOutcome
                && self.model_outcome.is_some() =>
            {
                Err(RestartError::InvalidBoundary(
                    "model replay is only valid after a model outcome",
                ))
            }
            _ => Ok(()),
        }
    }

    fn driver_parts(&self) -> Result<DriverParts, RestartError> {
        self.validate()?;
        let mut transcript = self.normalized_transcript();
        if let Some(outcome) = &self.model_outcome {
            let retained = transcript
                .len()
                .checked_sub(outcome.output_items.len())
                .ok_or(RestartError::InvalidBoundary(
                    "model outcome is longer than transcript",
                ))?;
            transcript.truncate(retained);
            let pending = transcript.split_off(
                self.resume_index
                    .expect("validated model outcome has a resume index"),
            );
            return Ok((transcript, pending, Some(outcome.clone())));
        }
        let pending = self
            .resume_index
            .map(|index| transcript.split_off(index))
            .unwrap_or_default();
        Ok((transcript, pending, None))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    Model,
    Tool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EffectCorrelation {
    pub run_id: RunId,
    pub owner: AttemptOwnership,
    pub claim: AttemptDriverClaim,
    pub operation_id: String,
    pub idempotency_key: String,
    pub command_id: CommandId,
    pub intent_event_id: EventId,
    pub dispatch_event_id: EventId,
    pub outcome_event_id: EventId,
    pub occurred_at: UtcDateTime,
    pub trace_id: TraceId,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "intent", rename_all = "snake_case")]
pub enum EffectIntentPayload {
    Model {
        provider: Option<String>,
        model: Option<String>,
        prompt_digest: String,
        config_digest: String,
        model_digest: String,
        #[serde(default)]
        model_intent: Option<serde_json::Value>,
    },
    Capability {
        tool_name: String,
        capability: serde_json::Value,
        effect: String,
        input: serde_json::Value,
        binding: serde_json::Value,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EffectIntent {
    pub kind: EffectKind,
    pub correlation: EffectCorrelation,
    pub payload: EffectIntentPayload,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EffectDispatched {
    pub kind: EffectKind,
    pub correlation: EffectCorrelation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    Succeeded,
    Failed,
    Cancelled,
    AuthRequired,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EffectOutcome {
    pub kind: EffectKind,
    pub correlation: EffectCorrelation,
    pub status: EffectStatus,
    pub snapshot: Option<BoundarySnapshot>,
}

impl EffectOutcome {
    fn validate(&self) -> Result<(), RestartError> {
        if self.status == EffectStatus::OutcomeUnknown {
            if self.snapshot.is_some() {
                return Err(RestartError::InvalidRecord(
                    "unknown outcome cannot advance the transcript",
                ));
            }
            return Ok(());
        }
        if self.status == EffectStatus::AuthRequired {
            return if self.kind == EffectKind::Tool && self.snapshot.is_none() {
                Ok(())
            } else {
                Err(RestartError::InvalidRecord(
                    "auth interruption must be a snapshot-free tool outcome",
                ))
            };
        }
        let Some(snapshot) = self.snapshot.as_ref() else {
            return if self.status == EffectStatus::Succeeded && self.kind == EffectKind::Model {
                Err(RestartError::InvalidRecord(
                    "successful outcome has no boundary snapshot",
                ))
            } else {
                Ok(())
            };
        };
        let expected = match self.kind {
            EffectKind::Model => SafeBoundary::AfterModelOutcome,
            EffectKind::Tool => SafeBoundary::AfterToolOutcome,
        };
        if snapshot.boundary != expected {
            return Err(RestartError::InvalidRecord(
                "effect outcome boundary does not match effect kind",
            ));
        }
        snapshot.validate()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PendingToolApproval {
    pub correlation: EffectCorrelation,
    pub request: ToolRequest,
    pub approval: ApprovalRequest,
    pub binding: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "record", content = "value", rename_all = "snake_case")]
pub enum EffectJournalRecord {
    Boundary(BoundarySnapshot),
    EffectIntent(EffectIntent),
    EffectDispatched(EffectDispatched),
    EffectOutcome(EffectOutcome),
    ToolApprovalRequested(PendingToolApproval),
    ToolApprovalRestored(PendingToolApproval),
    Waiting(WaitingState),
    WaitingResolved(WaitingResolved),
    CancellationRequested,
}

impl EffectJournalRecord {
    fn correlation(&self) -> Option<&EffectCorrelation> {
        match self {
            Self::EffectIntent(intent) => Some(&intent.correlation),
            Self::EffectDispatched(dispatched) => Some(&dispatched.correlation),
            Self::EffectOutcome(outcome) => Some(&outcome.correlation),
            Self::ToolApprovalRequested(pending) | Self::ToolApprovalRestored(pending) => {
                Some(&pending.correlation)
            }
            Self::Boundary(_)
            | Self::Waiting(_)
            | Self::WaitingResolved(_)
            | Self::CancellationRequested => None,
        }
    }
}

pub type LoopRecord = EffectJournalRecord;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DurableRecord {
    schema_version: u16,
    owner: AttemptOwnership,
    claim: Option<AttemptDriverClaim>,
    record: EffectJournalRecord,
}

pub struct LoopCommit {
    pub owner: AttemptOwnership,
    pub claim: Option<AttemptDriverClaim>,
    pub expected_stream_version: u64,
    pub idempotency_key: IdempotencyKey,
    pub command_id: CommandId,
    pub event_id: EventId,
    pub occurred_at: UtcDateTime,
    pub trace_id: TraceId,
    pub artifacts: Vec<ArtifactRef>,
    pub record: EffectJournalRecord,
}

pub fn append_loop_record(
    store: &mut SqliteStore,
    commit: LoopCommit,
) -> Result<AppendOutcome, StoreError> {
    append_loop_record_with_hook(store, commit, |_| false)
}

pub fn append_loop_record_with_hook(
    store: &mut SqliteStore,
    commit: LoopCommit,
    crash: impl FnMut(CrashPoint) -> bool,
) -> Result<AppendOutcome, StoreError> {
    if commit
        .record
        .correlation()
        .is_some_and(|correlation| correlation.owner != commit.owner)
    {
        return Err(StoreError::InvalidRequest(
            "effect correlation owner does not match journal owner",
        ));
    }
    if commit
        .claim
        .is_some_and(|claim| claim.owner() != commit.owner)
    {
        return Err(StoreError::InvalidRequest(
            "driver claim does not match journal owner",
        ));
    }
    let payload = serde_json::to_vec(&DurableRecord {
        schema_version: RECORD_SCHEMA_VERSION,
        owner: commit.owner,
        claim: commit.claim,
        record: commit.record,
    })
    .map_err(|_| StoreError::InvalidRequest("loop record is not serializable"))?;
    let artifacts = serde_json::to_vec(&commit.artifacts)
        .map_err(|_| StoreError::InvalidRequest("artifact references are not serializable"))?;
    let digest = CanonicalRequestDigest::new(sha256(&payload));
    let scope = IdempotencyScope::new(
        commit.owner.principal_id,
        LOOP_COMMAND,
        EntityId::Attempt(commit.owner.attempt_id),
    )
    .map_err(|_| StoreError::InvalidRequest("invalid loop idempotency scope"))?;

    store.append_with_hook(
        AppendCommand {
            idempotency_scope: scope,
            idempotency_key: commit.idempotency_key,
            request_digest: digest,
            claim: None,
            driver_claim: commit.claim,
            allow_quiescent_driver_claim: false,
            expected_versions: vec![ExpectedStreamVersion {
                stream: EntityId::Attempt(commit.owner.attempt_id),
                version: ExpectedVersion::new(commit.expected_stream_version),
            }],
            events: vec![NewEvent {
                id: commit.event_id,
                stream: EntityId::Attempt(commit.owner.attempt_id),
                event_type: EventType::parse(EFFECT_JOURNAL_EVENT)
                    .expect("effect journal event name is valid"),
                schema_version: SchemaVersion::CURRENT,
                occurred_at: commit.occurred_at,
                causation_id: commit.command_id,
                correlation_id: EntityId::Attempt(commit.owner.attempt_id),
                attempt_id: Some(commit.owner.attempt_id),
                trace_id: commit.trace_id,
                payload,
                artifacts,
            }],
            response: b"loop-record-v1".to_vec(),
        },
        crash,
    )
}

pub struct EffectJournalAppend {
    pub owner: AttemptOwnership,
    pub claim: Option<AttemptDriverClaim>,
    pub idempotency_key: IdempotencyKey,
    pub command_id: CommandId,
    pub event_id: EventId,
    pub occurred_at: UtcDateTime,
    pub trace_id: TraceId,
    pub artifacts: Vec<ArtifactRef>,
    pub record: EffectJournalRecord,
}

pub trait EffectJournal {
    fn append_effect(&mut self, append: EffectJournalAppend) -> Result<AppendOutcome, StoreError>;
}

impl EffectJournal for SqliteStore {
    fn append_effect(&mut self, append: EffectJournalAppend) -> Result<AppendOutcome, StoreError> {
        append_effect_with_events(self, append, Vec::new(), Vec::new())
    }
}

pub(crate) fn append_effect_with_events(
    store: &mut SqliteStore,
    append: EffectJournalAppend,
    mut additional_versions: Vec<ExpectedStreamVersion>,
    mut additional_events: Vec<NewEvent>,
) -> Result<AppendOutcome, StoreError> {
    if append
        .record
        .correlation()
        .is_some_and(|correlation| correlation.owner != append.owner)
        || append
            .claim
            .is_some_and(|claim| claim.owner() != append.owner)
    {
        return Err(StoreError::InvalidRequest(
            "effect journal authority does not match its owner",
        ));
    }
    let stream = EntityId::Attempt(append.owner.attempt_id);
    let expected_stream_version = store
        .events()?
        .into_iter()
        .filter(|event| event.event.stream == stream)
        .map(|event| event.sequence.get())
        .max()
        .unwrap_or(0);
    let payload = serde_json::to_vec(&DurableRecord {
        schema_version: RECORD_SCHEMA_VERSION,
        owner: append.owner,
        claim: append.claim,
        record: append.record,
    })
    .map_err(|_| StoreError::InvalidRequest("loop record is not serializable"))?;
    let artifacts = serde_json::to_vec(&append.artifacts)
        .map_err(|_| StoreError::InvalidRequest("artifact references are not serializable"))?;
    let mut request = payload.clone();
    for event in &additional_events {
        request.extend_from_slice(&(event.payload.len() as u64).to_be_bytes());
        request.extend_from_slice(&event.payload);
    }
    let scope = IdempotencyScope::new(append.owner.principal_id, LOOP_COMMAND, stream)
        .map_err(|_| StoreError::InvalidRequest("invalid loop idempotency scope"))?;
    additional_versions.insert(
        0,
        ExpectedStreamVersion {
            stream,
            version: ExpectedVersion::new(expected_stream_version),
        },
    );
    let mut events = vec![NewEvent {
        id: append.event_id,
        stream,
        event_type: EventType::parse(EFFECT_JOURNAL_EVENT)
            .expect("effect journal event name is valid"),
        schema_version: SchemaVersion::CURRENT,
        occurred_at: append.occurred_at,
        causation_id: append.command_id,
        correlation_id: EntityId::Attempt(append.owner.attempt_id),
        attempt_id: Some(append.owner.attempt_id),
        trace_id: append.trace_id,
        payload,
        artifacts,
    }];
    events.append(&mut additional_events);
    store.append(AppendCommand {
        idempotency_scope: scope,
        idempotency_key: append.idempotency_key,
        request_digest: CanonicalRequestDigest::new(sha256(&request)),
        claim: None,
        driver_claim: append.claim,
        allow_quiescent_driver_claim: false,
        expected_versions: additional_versions,
        events,
        response: b"loop-record-v1".to_vec(),
    })
}

pub fn effect_records(
    store: &SqliteStore,
    owner: AttemptOwnership,
) -> Result<Vec<EffectJournalRecord>, StoreError> {
    store
        .events()?
        .into_iter()
        .filter(|event| {
            event.event.attempt_id == Some(owner.attempt_id)
                && event.event.event_type.as_str() == EFFECT_JOURNAL_EVENT
        })
        .map(|event| {
            let durable: DurableRecord = serde_json::from_slice(&event.event.payload)
                .map_err(|_| StoreError::CorruptData("invalid effect journal record"))?;
            if durable.schema_version != RECORD_SCHEMA_VERSION
                || durable.owner != owner
                || durable.claim.is_some_and(|claim| claim.owner() != owner)
            {
                return Err(StoreError::CorruptData(
                    "invalid effect journal owner or version",
                ));
            }
            Ok(durable.record)
        })
        .collect()
}

#[derive(Clone, Copy)]
pub struct ResolvedMcpBootstrapAuth {
    pub owner: AttemptOwnership,
    pub claim: AttemptDriverClaim,
    pub granted: bool,
    pub challenge_id: crate::domain::ids::ApprovalId,
    pub challenge_kind: &'static str,
    pub challenge_generation: u64,
}

pub fn resolved_mcp_bootstrap_auth(
    store: &SqliteStore,
    run_id: crate::domain::ids::RunId,
    server: &str,
) -> Result<Option<ResolvedMcpBootstrapAuth>, StoreError> {
    let events = store.events()?;
    let challenges = events
        .iter()
        .filter(|event| {
            event.event.event_type.as_str() == "capability.broker_transport_auth_challenged"
                && event.event.correlation_id == EntityId::Run(run_id)
        })
        .filter_map(|event| {
            let record = serde_json::from_slice::<serde_json::Value>(&event.event.payload).ok()?;
            if record
                .get("transport_binding")
                .and_then(|binding| binding.get("server_id"))
                .and_then(serde_json::Value::as_str)
                != Some(server)
            {
                return None;
            }
            let challenge_id = record
                .get("challenge_id")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| crate::domain::ids::ApprovalId::parse(value).ok())?;
            let kind = record
                .get("challenge_kind")
                .and_then(serde_json::Value::as_str)?;
            let generation = record
                .get("generation")
                .and_then(serde_json::Value::as_u64)?;
            Some((
                challenge_id,
                (event.event.attempt_id?, kind.to_owned(), generation),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let mut waits = BTreeMap::new();
    let mut resolved: Option<ResolvedMcpBootstrapAuth> = None;
    for event in events {
        if event.event.event_type.as_str() == "capability.broker_transport_outcome"
            && resolved.is_some()
            && event.event.correlation_id == EntityId::Run(run_id)
        {
            let completed = serde_json::from_slice::<serde_json::Value>(&event.event.payload)
                .ok()
                .is_some_and(|record| {
                    record.get("operation").and_then(serde_json::Value::as_str)
                        == Some("initialize")
                        && record.get("status").and_then(serde_json::Value::as_str)
                            == Some("completed")
                        && record
                            .get("binding")
                            .and_then(|binding| binding.get("server_id"))
                            .and_then(serde_json::Value::as_str)
                            == Some(server)
                });
            if completed {
                resolved = None;
            }
            continue;
        }
        if event.event.event_type.as_str() != EFFECT_JOURNAL_EVENT {
            continue;
        }
        let durable: DurableRecord = serde_json::from_slice(&event.event.payload)
            .map_err(|_| StoreError::CorruptData("invalid effect journal record"))?;
        match durable.record {
            EffectJournalRecord::Waiting(WaitingState {
                wait_id,
                kind:
                    WaitingKind::Auth {
                        run_id: found,
                        tool_call_id: None,
                        challenge_kind,
                        challenge_generation,
                        challenge_id: Some(challenge_id),
                        ..
                    },
                ..
            }) if found == run_id => {
                if let Some(claim) = durable.claim
                    && challenges
                        .get(&challenge_id)
                        .is_some_and(|(attempt, kind, generation)| {
                            *attempt == durable.owner.attempt_id
                                && kind == challenge_kind.as_str()
                                && *generation == challenge_generation
                        })
                {
                    waits.insert(
                        wait_id,
                        (
                            durable.owner,
                            claim,
                            challenge_id,
                            challenge_kind,
                            challenge_generation,
                        ),
                    );
                }
            }
            EffectJournalRecord::WaitingResolved(waiting) => {
                if let Some((owner, claim, challenge_id, kind, generation)) =
                    waits.get(&waiting.wait_id)
                    && let WaitingResolution::Auth { granted } = waiting.resolution
                    && resolved
                        .as_ref()
                        .is_none_or(|current| *generation >= current.challenge_generation)
                {
                    resolved = Some(ResolvedMcpBootstrapAuth {
                        owner: *owner,
                        claim: *claim,
                        granted,
                        challenge_id: *challenge_id,
                        challenge_kind: kind.as_str(),
                        challenge_generation: *generation,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(resolved)
}

#[derive(Clone, Debug, PartialEq)]
pub struct RestartPlan {
    pub owner: AttemptOwnership,
    pub claim: Option<AttemptDriverClaim>,
    pub snapshot: BoundarySnapshot,
    pub approved_tool: Option<PendingToolApproval>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RestoredWaiting {
    pub owner: AttemptOwnership,
    pub claim: Option<AttemptDriverClaim>,
    pub waiting: WaitingState,
    pub cancellation_requested: bool,
    pub pending_tool: Option<PendingToolApproval>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RecoveryState {
    Ready(RestartPlan),
    Waiting(RestoredWaiting),
    OutcomeUnknown(Vec<EffectOutcome>),
    Cancelled(BoundarySnapshot),
}

pub struct RestartProjection;

impl RestartProjection {
    pub fn reconstruct(
        projection: &AttemptProjection,
        committed: &[StoredEvent],
    ) -> Result<RecoveryState, RestartError> {
        if projection.id != projection.owner.attempt_id {
            return Err(RestartError::OwnerMismatch);
        }
        let mut snapshot = None;
        let mut in_flight = BTreeMap::<String, (EffectIntent, bool)>::new();
        let mut waiting = None::<WaitingState>;
        let mut pending_tools = BTreeMap::<String, PendingToolApproval>::new();
        let mut unknown = Vec::new();
        let mut completed_operations = BTreeSet::new();
        let mut cancellation_requested = false;
        let mut terminal_denial = false;
        let mut approved_tool = None;
        let mut driver_claim = None;
        let mut previous_position = 0;

        for event in committed.iter().filter(|event| {
            event.event.attempt_id == Some(projection.id)
                && event.event.event_type.as_str() == EFFECT_JOURNAL_EVENT
        }) {
            if event.commit_position.get() <= previous_position {
                return Err(RestartError::EventsOutOfOrder);
            }
            previous_position = event.commit_position.get();
            let durable: DurableRecord = serde_json::from_slice(&event.event.payload)
                .map_err(|_| RestartError::InvalidRecord("loop record is not valid JSON"))?;
            if durable.schema_version != RECORD_SCHEMA_VERSION {
                return Err(RestartError::UnsupportedVersion(durable.schema_version));
            }
            if durable.owner != projection.owner {
                return Err(RestartError::OwnerMismatch);
            }
            if durable.claim.is_some_and(|claim| {
                claim.owner() != durable.owner || claim.run_id != projection.run_id
            }) {
                return Err(RestartError::OwnerMismatch);
            }
            if let Some(claim) = durable.claim {
                if driver_claim
                    .is_some_and(|current: AttemptDriverClaim| !current.same_lease(claim))
                {
                    return Err(RestartError::OwnerMismatch);
                }
                driver_claim = Some(claim);
            }
            if durable
                .record
                .correlation()
                .is_some_and(|correlation| correlation.owner != durable.owner)
            {
                return Err(RestartError::OwnerMismatch);
            }
            match durable.record {
                LoopRecord::Boundary(boundary) => {
                    boundary.validate()?;
                    if !in_flight.is_empty() {
                        return Err(RestartError::InvalidRecord(
                            "safe boundary committed while an effect is in flight",
                        ));
                    }
                    snapshot = Some(boundary);
                    waiting = None;
                }
                LoopRecord::EffectIntent(intent) => {
                    if snapshot.is_none() {
                        return Err(RestartError::MissingBoundary);
                    }
                    if in_flight
                        .insert(intent.correlation.operation_id.clone(), (intent, false))
                        .is_some()
                    {
                        return Err(RestartError::InvalidRecord("duplicate effect intent"));
                    }
                }
                LoopRecord::EffectDispatched(dispatched) => {
                    let (intent, was_dispatched) = in_flight
                        .get_mut(&dispatched.correlation.operation_id)
                        .ok_or(RestartError::InvalidRecord(
                            "effect dispatch has no committed intent",
                        ))?;
                    if *was_dispatched
                        || intent.kind != dispatched.kind
                        || intent.correlation != dispatched.correlation
                    {
                        return Err(RestartError::InvalidRecord(
                            "effect dispatch does not match intent",
                        ));
                    }
                    *was_dispatched = true;
                }
                LoopRecord::EffectOutcome(outcome) => {
                    outcome.validate()?;
                    let operation_id = outcome.correlation.operation_id.clone();
                    let (intent, dispatched) =
                        in_flight.remove(&outcome.correlation.operation_id).ok_or(
                            RestartError::InvalidRecord("effect outcome has no committed intent"),
                        )?;
                    if intent.kind != outcome.kind || intent.correlation != outcome.correlation {
                        return Err(RestartError::InvalidRecord(
                            "effect outcome kind does not match intent",
                        ));
                    }
                    if !dispatched
                        && !matches!(
                            outcome.status,
                            EffectStatus::Cancelled
                                | EffectStatus::Failed
                                | EffectStatus::AuthRequired
                        )
                    {
                        return Err(RestartError::InvalidRecord(
                            "successful or unknown effect outcome was never dispatched",
                        ));
                    }
                    if outcome.status == EffectStatus::OutcomeUnknown {
                        unknown.push(outcome);
                    } else if outcome.status == EffectStatus::AuthRequired {
                        // The matching waiting record owns resumption; this is
                        // deliberately non-terminal for the tool operation.
                    } else {
                        if outcome.snapshot.is_some() {
                            snapshot = outcome.snapshot.clone();
                        }
                        completed_operations.insert(operation_id);
                    }
                }
                LoopRecord::ToolApprovalRequested(pending) => {
                    if pending
                        .approval
                        .call_id
                        .as_ref()
                        .is_none_or(|call_id| call_id != &pending.request.call_id)
                    {
                        return Err(RestartError::InvalidRecord(
                            "approval request does not match tool request",
                        ));
                    }
                    pending_tools.insert(pending.request.call_id.0.clone(), pending);
                }
                LoopRecord::ToolApprovalRestored(pending) => {
                    if pending
                        .approval
                        .call_id
                        .as_ref()
                        .is_none_or(|call_id| call_id != &pending.request.call_id)
                    {
                        return Err(RestartError::InvalidRecord(
                            "restored approval does not match tool request",
                        ));
                    }
                    approved_tool = Some(pending);
                }
                LoopRecord::Waiting(state) => {
                    state.snapshot.validate()?;
                    if !in_flight.is_empty() || waiting.is_some() {
                        return Err(RestartError::InvalidRecord(
                            "waiting state is not at a safe boundary",
                        ));
                    }
                    if state.principal_id != projection.owner.principal_id {
                        return Err(RestartError::OwnerMismatch);
                    }
                    snapshot = Some(state.snapshot.clone());
                    waiting = Some(state);
                }
                LoopRecord::WaitingResolved(resolved) => {
                    let state = waiting.take().ok_or(RestartError::InvalidRecord(
                        "waiting resolution has no pending wait",
                    ))?;
                    if state.wait_id != resolved.wait_id
                        || state.principal_id != resolved.resolved_by
                    {
                        return Err(RestartError::OwnerMismatch);
                    }
                    resolved.snapshot.validate()?;
                    terminal_denial = matches!(
                        resolved.resolution,
                        crate::agent::driver::waiting::WaitingResolution::Auth { granted: false }
                    );
                    if matches!(
                        resolved.resolution,
                        crate::agent::driver::waiting::WaitingResolution::Approval {
                            decision: crate::domain::events::ApprovalDecision::Approved
                        }
                    ) && let crate::agent::driver::waiting::WaitingKind::Approval {
                        tool_call_id,
                        ..
                    } = state.kind
                    {
                        approved_tool = take_pending_tool(&mut pending_tools, tool_call_id);
                        if approved_tool.is_none() {
                            return Err(RestartError::InvalidRecord(
                                "approved tool resolution has no durable pending request",
                            ));
                        }
                    }
                    snapshot = Some(resolved.snapshot);
                }
                LoopRecord::CancellationRequested => cancellation_requested = true,
            }
        }

        if !in_flight.is_empty() {
            let uncertain = in_flight
                .into_values()
                .filter_map(|(intent, dispatched)| {
                    dispatched.then_some(EffectOutcome {
                        kind: intent.kind,
                        correlation: intent.correlation,
                        status: EffectStatus::OutcomeUnknown,
                        snapshot: None,
                    })
                })
                .collect::<Vec<_>>();
            if !uncertain.is_empty() {
                return Ok(RecoveryState::OutcomeUnknown(uncertain));
            }
        }
        if !unknown.is_empty() {
            return Ok(RecoveryState::OutcomeUnknown(unknown));
        }
        if cancellation_requested {
            return Ok(RecoveryState::Cancelled(
                snapshot.ok_or(RestartError::MissingBoundary)?,
            ));
        }
        if let Some(waiting) = waiting {
            return Ok(RecoveryState::Waiting(RestoredWaiting {
                owner: projection.owner,
                claim: driver_claim,
                pending_tool: match &waiting.kind {
                    crate::agent::driver::waiting::WaitingKind::Approval {
                        tool_call_id, ..
                    } => take_pending_tool(&mut pending_tools, *tool_call_id),
                    _ => None,
                },
                waiting,
                cancellation_requested,
            }));
        }
        let snapshot = snapshot.ok_or(RestartError::MissingBoundary)?;
        if approved_tool.as_ref().is_some_and(|pending| {
            pending
                .approval
                .metadata
                .get("kit.approved_invocation_id")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|operation_id| completed_operations.contains(operation_id))
        }) {
            approved_tool = None;
        }
        if cancellation_requested || terminal_denial {
            return Ok(RecoveryState::Cancelled(snapshot));
        }
        Ok(RecoveryState::Ready(RestartPlan {
            owner: projection.owner,
            claim: driver_claim,
            snapshot,
            approved_tool,
        }))
    }
}

fn take_pending_tool(
    pending: &mut BTreeMap<String, PendingToolApproval>,
    tool_call_id: crate::domain::ids::ToolCallId,
) -> Option<PendingToolApproval> {
    pending.remove(&tool_call_id.to_string()).or_else(|| {
        if pending.len() == 1 {
            let key = pending.keys().next()?.clone();
            pending.remove(&key)
        } else {
            None
        }
    })
}

impl RestartPlan {
    pub async fn start_claimed<M, F>(
        self,
        projection: &AttemptProjection,
        claim: AttemptDriverClaim,
        store: SqliteStore,
        model: M,
        configure: F,
    ) -> Result<AttemptDriver<ReplaySession<M::Session>>, StartError>
    where
        M: ModelAdapter,
        F: FnOnce(AgentBuilder<ReplayAdapter<M>>) -> AgentBuilder<ReplayAdapter<M>>,
    {
        if projection.owner != self.owner || projection.id != self.owner.attempt_id {
            return Err(StartError::Restart(RestartError::OwnerMismatch));
        }
        if self.claim.is_none_or(|durable| !durable.same_lease(claim)) {
            return Err(StartError::Restart(RestartError::OwnerMismatch));
        }
        let (transcript, input, replay) = self.snapshot.driver_parts()?;
        let cancellation = CancellationController::new();
        let adapter = ReplayAdapter::new(model, replay);
        let builder = Agent::builder()
            .model(adapter)
            .transcript(transcript)
            .input(input)
            .cancellation(cancellation.handle());
        let agent = configure(builder).build().map_err(StartError::Loop)?;
        let driver = agent
            .start(SessionConfig::new(projection.run_id.to_string()))
            .await
            .map_err(StartError::Loop)?;
        let mut driver = AttemptDriver::claim(projection, claim, store, driver, cancellation)
            .await
            .map_err(StartError::Ownership)?;
        if let Some(pending) = self.approved_tool {
            driver
                .restore_approved_tool(projection, pending.request.call_id)
                .await
                .map_err(|error| match error {
                    crate::agent::driver::attempt::PollError::Ownership(error) => {
                        StartError::Ownership(error)
                    }
                    crate::agent::driver::attempt::PollError::Loop(error) => {
                        StartError::Loop(error)
                    }
                })?;
        }
        Ok(driver)
    }

    #[cfg(debug_assertions)]
    pub async fn start<M, F>(
        self,
        projection: &AttemptProjection,
        model: M,
        configure: F,
    ) -> Result<AttemptDriver<ReplaySession<M::Session>>, StartError>
    where
        M: ModelAdapter,
        F: FnOnce(AgentBuilder<ReplayAdapter<M>>) -> AgentBuilder<ReplayAdapter<M>>,
    {
        if projection.owner != self.owner || projection.id != self.owner.attempt_id {
            return Err(StartError::Restart(RestartError::OwnerMismatch));
        }
        let (transcript, input, replay) = self.snapshot.driver_parts()?;
        let cancellation = CancellationController::new();
        let adapter = ReplayAdapter::new(model, replay);
        let builder = Agent::builder()
            .model(adapter)
            .transcript(transcript)
            .input(input)
            .cancellation(cancellation.handle());
        let agent = configure(builder).build().map_err(StartError::Loop)?;
        let driver = agent
            .start(SessionConfig::new(projection.run_id.to_string()))
            .await
            .map_err(StartError::Loop)?;
        let mut driver = AttemptDriver::unclaimed_for_test(projection, driver, cancellation)
            .map_err(StartError::Ownership)?;
        if let Some(pending) = self.approved_tool {
            driver
                .restore_approved_tool(projection, pending.request.call_id)
                .await
                .map_err(|error| match error {
                    crate::agent::driver::attempt::PollError::Ownership(error) => {
                        StartError::Ownership(error)
                    }
                    crate::agent::driver::attempt::PollError::Loop(error) => {
                        StartError::Loop(error)
                    }
                })?;
        }
        Ok(driver)
    }
}

pub struct ReplayAdapter<M> {
    model: M,
    replay: Mutex<Option<CommittedModelOutcome>>,
}

impl<M> ReplayAdapter<M> {
    fn new(model: M, replay: Option<CommittedModelOutcome>) -> Self {
        Self {
            model,
            replay: Mutex::new(replay),
        }
    }
}

impl<M> ModelAdapter for ReplayAdapter<M>
where
    M: ModelAdapter,
{
    type Session = ReplaySession<M::Session>;

    fn start_session<'life0, 'async_trait>(
        &'life0 self,
        config: SessionConfig,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Session, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let replay = self
                .replay
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            let session = self.model.start_session(config).await?;
            Ok(ReplaySession { session, replay })
        })
    }

    fn provider_name(&self) -> Option<&str> {
        self.model.provider_name()
    }
}

pub struct ReplaySession<S> {
    session: S,
    replay: Option<CommittedModelOutcome>,
}

impl<S> ModelSession for ReplaySession<S>
where
    S: ModelSession,
{
    type Turn = ReplayTurn<S::Turn>;

    fn begin_turn<'life0, 'async_trait>(
        &'life0 mut self,
        request: TurnRequest,
        cancellation: Option<TurnCancellation>,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Turn, LoopError>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            if let Some(outcome) = self.replay.take() {
                let result = outcome.to_agentkit();
                let mut events = result
                    .output_items
                    .iter()
                    .flat_map(|item| item.parts.iter())
                    .filter_map(|part| match part {
                        Part::ToolCall(call) => Some(ModelTurnEvent::ToolCall(call.clone())),
                        _ => None,
                    })
                    .collect::<VecDeque<_>>();
                events.push_back(ModelTurnEvent::Finished(result));
                Ok(ReplayTurn::Committed(events))
            } else {
                self.session
                    .begin_turn(request, cancellation)
                    .await
                    .map(ReplayTurn::Live)
            }
        })
    }

    fn model_name(&self) -> Option<&str> {
        self.session.model_name()
    }
}

pub enum ReplayTurn<T> {
    Committed(VecDeque<ModelTurnEvent>),
    Live(T),
}

impl<T> ModelTurn for ReplayTurn<T>
where
    T: ModelTurn,
{
    fn next_event<'life0, 'async_trait>(
        &'life0 mut self,
        cancellation: Option<TurnCancellation>,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<ModelTurnEvent>, LoopError>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            match self {
                Self::Committed(events) => Ok(events.pop_front()),
                Self::Live(turn) => turn.next_event(cancellation).await,
            }
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RestartError {
    OwnerMismatch,
    EventsOutOfOrder,
    MissingBoundary,
    UnsupportedVersion(u16),
    InvalidBoundary(&'static str),
    InvalidRecord(&'static str),
}

impl fmt::Display for RestartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OwnerMismatch => f.write_str("committed loop owner does not match projection"),
            Self::EventsOutOfOrder => f.write_str("committed loop events are out of order"),
            Self::MissingBoundary => f.write_str("attempt has no committed safe loop boundary"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported durable loop record version {version}")
            }
            Self::InvalidBoundary(message) | Self::InvalidRecord(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RestartError {}

#[derive(Debug)]
pub enum StartError {
    Restart(RestartError),
    Loop(LoopError),
    Ownership(AttemptDriverError),
}

impl From<RestartError> for StartError {
    fn from(error: RestartError) -> Self {
        Self::Restart(error)
    }
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Restart(error) => error.fmt(f),
            Self::Loop(error) => error.fmt(f),
            Self::Ownership(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Restart(error) => Some(error),
            Self::Loop(error) => Some(error),
            Self::Ownership(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod authority_bytes_tests {
    use super::*;
    use crate::domain::ids::{AttemptId, PrincipalId};

    #[test]
    fn pre_upgrade_nonlexical_record_retries_and_restarts_with_old_digest() {
        let root = std::env::temp_dir().join(format!(
            "kit-effect-authority-{}",
            EventId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("state.sqlite3");
        let owner = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            PrincipalId::generate().unwrap(),
            crate::domain::lifecycle::FencingToken::new(7),
        );
        let owner_json = serde_json::to_string(&owner).unwrap();
        let payload = format!(
            r#"{{"record":{{"record":"cancellation_requested"}},"claim":null,"owner":{owner_json},"schema_version":1}}"#
        )
        .into_bytes();
        let old_digest = CanonicalRequestDigest::new(sha256(&payload));
        let scope = IdempotencyScope::new(
            owner.principal_id,
            LOOP_COMMAND,
            EntityId::Attempt(owner.attempt_id),
        )
        .unwrap();
        let key = IdempotencyKey::parse("pre-upgrade-effect").unwrap();
        let command = AppendCommand {
            idempotency_scope: scope.clone(),
            idempotency_key: key.clone(),
            request_digest: old_digest,
            claim: None,
            driver_claim: None,
            allow_quiescent_driver_claim: false,
            expected_versions: vec![ExpectedStreamVersion {
                stream: EntityId::Attempt(owner.attempt_id),
                version: ExpectedVersion::new(0),
            }],
            events: vec![NewEvent {
                id: EventId::generate().unwrap(),
                stream: EntityId::Attempt(owner.attempt_id),
                event_type: EventType::parse(EFFECT_JOURNAL_EVENT).unwrap(),
                schema_version: SchemaVersion::CURRENT,
                occurred_at: UtcDateTime::parse("2026-08-06T12:00:00Z").unwrap(),
                causation_id: CommandId::generate().unwrap(),
                correlation_id: EntityId::Attempt(owner.attempt_id),
                attempt_id: Some(owner.attempt_id),
                trace_id: TraceId::parse("pre-upgrade-effect").unwrap(),
                payload: payload.clone(),
                artifacts: b"[]".to_vec(),
            }],
            response: b"loop-record-v1".to_vec(),
        };
        let custody = crate::domain::secret::SecretCustody::new([std::sync::Arc::new(
            crate::domain::secret::SecretLease::new("unrelated-active-custody"),
        )]);
        let mut store =
            crate::test_support::open_project_store(&database, custody.clone()).unwrap();
        assert!(matches!(
            store.append(command.clone()).unwrap(),
            AppendOutcome::Committed(_)
        ));
        assert_eq!(store.events().unwrap()[0].event.payload, payload);
        drop(store);

        let mut store = crate::test_support::open_project_store(&database, custody).unwrap();
        assert!(matches!(
            store.append(command).unwrap(),
            AppendOutcome::Replayed(_)
        ));
        assert_eq!(store.events().unwrap()[0].event.payload, payload);
        assert!(matches!(
            store.idempotency_status(&scope, &key).unwrap(),
            crate::store::sqlite::idempotency::IdempotencyStatus::Terminal {
                request_digest,
                ..
            } if request_digest == old_digest
        ));
        assert_eq!(
            effect_records(&store, owner).unwrap(),
            [EffectJournalRecord::CancellationRequested]
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
