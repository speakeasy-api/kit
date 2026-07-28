use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::events::{CommitPosition, EventEnvelope, EventPayload, SchemaVersion};
use super::ids::{ApprovalId, ArtifactId, AttemptId, PrincipalId, ProjectId, RunId, ThreadId};
use super::retention::{RetentionObjectId, RetentionPolicy};
use crate::api::auth::contract::ResourceScope;
use crate::api::service::{
    ApprovalProjection, ArtifactMetadataProjection, AttemptProjection, AuthRequestProjection,
    Command, EventProjection, OperationKind, ProjectProjection, Resource, RunCompletionRecord,
    RunCostProjection, RunProgressRecord, RunProjection, RunPromptProjection, RunSemanticEnvelope,
    RunTranscriptEntry, ServiceError, ThreadProjection, handlers,
};
use crate::domain::config::RunConfigSnapshot;
use crate::domain::crypto::sha256;
use crate::domain::deletion::DeletionJobId;
use crate::domain::events::{AttemptState, RunState};
use crate::store::sqlite::append::StoredEvent;

pub trait DeterministicReducer<E: EventPayload> {
    fn reduce(&mut self, event: &E);
}

pub trait ProjectionContract<E: EventPayload>: DeterministicReducer<E> {
    const NAME: &'static str;
    const SCHEMA_VERSION: SchemaVersion;
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectionState<T> {
    pub schema_version: SchemaVersion,
    pub applied_through: Option<CommitPosition>,
    pub state: T,
}

impl<T> ProjectionState<T> {
    pub fn new(state: T) -> Self {
        Self {
            schema_version: SchemaVersion::CURRENT,
            applied_through: None,
            state,
        }
    }

    pub fn apply<E>(&mut self, event: &EventEnvelope<E>) -> Result<(), ProjectionError>
    where
        E: EventPayload,
        T: ProjectionContract<E>,
    {
        if let Some(previous) = self.applied_through
            && event.commit_position <= previous
        {
            return Err(ProjectionError::OutOfOrder {
                previous,
                incoming: event.commit_position,
            });
        }
        self.state.reduce(&event.payload);
        self.applied_through = Some(event.commit_position);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionError {
    OutOfOrder {
        previous: CommitPosition,
        incoming: CommitPosition,
    },
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfOrder { previous, incoming } => write!(
                f,
                "projection event position {} does not follow {}",
                incoming.get(),
                previous.get()
            ),
        }
    }
}

impl std::error::Error for ProjectionError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PersistedCommand {
    pub principal_id: PrincipalId,
    #[serde(default)]
    pub stored_at_unix_micros: i64,
    #[serde(default)]
    pub idempotency_key: String,
    #[serde(default = "default_true")]
    pub apply_projection: bool,
    pub command: Command,
}

const fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DeletionIntent {
    pub id: u128,
    pub principal_id: PrincipalId,
    pub project_id: ProjectId,
    pub thread_id: ThreadId,
    pub idempotency_key: String,
    pub resource_version: u64,
    pub policy: RetentionPolicy,
    pub requested_at_unix_micros: i64,
}

impl DeletionIntent {
    pub(crate) const fn job_id(&self) -> DeletionJobId {
        DeletionJobId::new(self.id)
    }

    pub(crate) const fn object_id(&self) -> RetentionObjectId {
        RetentionObjectId::Transcript(self.thread_id)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DomainReducer {
    pub(crate) projects: BTreeMap<ProjectId, ProjectProjection>,
    pub(crate) threads: BTreeMap<ThreadId, ThreadProjection>,
    pub(crate) runs: BTreeMap<RunId, RunProjection>,
    #[serde(default)]
    pub(crate) run_costs: BTreeMap<RunId, RunCostProjection>,
    #[serde(default)]
    pub(crate) run_prompts: BTreeMap<RunId, RunPromptProjection>,
    #[serde(default)]
    pub(crate) run_transcripts: BTreeMap<RunId, Vec<RunTranscriptEntry>>,
    pub(crate) attempts: BTreeMap<AttemptId, AttemptProjection>,
    pub(crate) approvals: BTreeMap<ApprovalId, ApprovalProjection>,
    pub(crate) auth_requests: BTreeMap<RunId, AuthRequestProjection>,
    pub(crate) artifacts: BTreeMap<ArtifactId, ArtifactMetadataProjection>,
    pub(crate) thread_stored_at: BTreeMap<ThreadId, i64>,
    pub(crate) artifact_stored_at: BTreeMap<ArtifactId, i64>,
    pub(crate) deletion_intents: Vec<DeletionIntent>,
    #[serde(skip)]
    pub(crate) events: Vec<EventProjection>,
    committed: u64,
}

const DOMAIN_PROJECTION_SCHEMA_VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
struct DomainProjectionEnvelope<T> {
    schema_version: u32,
    state: T,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IndexedEventScope {
    pub project_id: ProjectId,
    pub thread_id: Option<ThreadId>,
    pub run_id: Option<RunId>,
}

impl DomainReducer {
    pub const NAME: &'static str = "domain";

    pub fn replay(events: impl IntoIterator<Item = StoredEvent>) -> Result<Self, ServiceError> {
        let mut state = Self::default();
        for event in events {
            state.reduce(&event)?;
        }
        Ok(state)
    }

    pub fn reduce(&mut self, event: &StoredEvent) -> Result<(), ServiceError> {
        self.reduce_indexed(event).map(|_| ())
    }

    pub(crate) fn reduce_indexed(
        &mut self,
        event: &StoredEvent,
    ) -> Result<Option<(IndexedEventScope, i64)>, ServiceError> {
        let position = event.commit_position.get();
        if position != self.committed + 1 {
            return Err(ServiceError::Store(
                "event log is not a gapless committed prefix".to_owned(),
            ));
        }
        self.committed = position;
        let operation = event.event.event_type.as_str();
        if operation == "run.prompt" {
            let stored: RunSemanticEnvelope<RunPromptProjection> =
                serde_json::from_slice(&event.event.payload)
                    .map_err(|error| ServiceError::Store(error.to_string()))?;
            self.require_semantic_run(&stored, event)?;
            self.run_prompts.insert(stored.run_id, stored.record);
            return Ok(Some((
                self.semantic_scope(stored.project_id, stored.run_id)?,
                stored.stored_at_unix_micros,
            )));
        }
        if operation == "run.progress" {
            let stored: RunSemanticEnvelope<RunProgressRecord> =
                serde_json::from_slice(&event.event.payload)
                    .map_err(|error| ServiceError::Store(error.to_string()))?;
            self.require_semantic_run(&stored, event)?;
            let sequence = self
                .run_transcripts
                .get(&stored.run_id)
                .and_then(|items| items.last())
                .map_or(1, |item| item.sequence.saturating_add(1));
            self.run_transcripts
                .entry(stored.run_id)
                .or_default()
                .push(RunTranscriptEntry {
                    sequence,
                    attempt: stored.attempt,
                    model_call_id: stored.record.model_call_id,
                    kind: stored.record.kind,
                    content: stored.record.content,
                    artifact: None,
                });
            return Ok(Some((
                self.semantic_scope(stored.project_id, stored.run_id)?,
                stored.stored_at_unix_micros,
            )));
        }
        if operation == "run.output" {
            let stored: RunSemanticEnvelope<RunCompletionRecord> =
                serde_json::from_slice(&event.event.payload)
                    .map_err(|error| ServiceError::Store(error.to_string()))?;
            self.require_semantic_run(&stored, event)?;
            let run = self.run(stored.run_id).ok_or(ServiceError::NotFound)?;
            if run.failure.is_some() || run.state == RunState::Failed {
                return Err(ServiceError::Store(
                    "run output conflicts with a failed run projection".to_owned(),
                ));
            }
            if run.output.is_some() {
                return Err(ServiceError::Store(
                    "run projection contains more than one output".to_owned(),
                ));
            }
            let sequence = self
                .run_transcripts
                .get(&stored.run_id)
                .and_then(|items| items.last())
                .map_or(1, |item| item.sequence.saturating_add(1));
            self.run_transcripts
                .entry(stored.run_id)
                .or_default()
                .push(RunTranscriptEntry {
                    sequence,
                    attempt: stored.attempt,
                    model_call_id: None,
                    kind: "assistant_final".to_owned(),
                    content: stored.record.item_preview,
                    artifact: Some(stored.record.output.artifact.clone()),
                });
            self.run_costs.insert(
                stored.run_id,
                RunCostProjection {
                    usage: Some(stored.record.usage),
                    cost: stored.record.cost,
                },
            );
            self.run_mut(stored.run_id)?.output = Some(stored.record.output);
            return Ok(Some((
                self.semantic_scope(stored.project_id, stored.run_id)?,
                stored.stored_at_unix_micros,
            )));
        }
        if operation == "run.failure" {
            let stored: RunSemanticEnvelope<crate::api::service::RunFailureProjection> =
                serde_json::from_slice(&event.event.payload)
                    .map_err(|error| ServiceError::Store(error.to_string()))?;
            self.require_semantic_run(&stored, event)?;
            let run = self.run(stored.run_id).ok_or(ServiceError::NotFound)?;
            if run.output.is_some() || run.state == RunState::Completed {
                return Err(ServiceError::Store(
                    "run failure conflicts with a completed run projection".to_owned(),
                ));
            }
            if run.failure.is_some() {
                return Err(ServiceError::Store(
                    "run projection contains more than one failure".to_owned(),
                ));
            }
            self.run_mut(stored.run_id)?.failure = Some(stored.record);
            return Ok(Some((
                self.semantic_scope(stored.project_id, stored.run_id)?,
                stored.stored_at_unix_micros,
            )));
        }
        if !handlers().iter().any(|descriptor| {
            descriptor.kind == OperationKind::Command && descriptor.operation == operation
        }) {
            return Ok(None);
        }
        let stored: PersistedCommand = serde_json::from_slice(&event.event.payload)
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        if stored.command.operation() != operation {
            return Err(ServiceError::Store(
                "stored command does not match event type".to_owned(),
            ));
        }
        let project_id = self.command_project(&stored.command)?;
        if stored.apply_projection {
            self.apply(
                stored.principal_id,
                stored.stored_at_unix_micros,
                position,
                &stored.idempotency_key,
                &stored.command,
            )?;
        }
        Ok(Some((
            self.indexed_scope(project_id, &stored.command)?,
            stored.stored_at_unix_micros,
        )))
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ServiceError> {
        serde_json::to_vec(&DomainProjectionEnvelope {
            schema_version: DOMAIN_PROJECTION_SCHEMA_VERSION,
            state: self,
        })
        .map_err(|error| ServiceError::Store(error.to_string()))
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, ServiceError> {
        let envelope: DomainProjectionEnvelope<Self> = serde_json::from_slice(bytes)
            .map_err(|error| ServiceError::Store(error.to_string()))?;
        if envelope.schema_version != DOMAIN_PROJECTION_SCHEMA_VERSION {
            return Err(ServiceError::Store(format!(
                "unsupported domain projection schema version {}",
                envelope.schema_version
            )));
        }
        Ok(envelope.state)
    }

    pub fn reduce_canonical_bytes(
        bytes: &mut Vec<u8>,
        event: &StoredEvent,
    ) -> Result<(), ServiceError> {
        let mut state = Self::from_canonical_bytes(bytes)?;
        state.reduce(event)?;
        *bytes = state.canonical_bytes()?;
        Ok(())
    }

    pub fn digest(&self) -> Result<[u8; 32], ServiceError> {
        self.canonical_bytes().map(|bytes| sha256(&bytes))
    }

    pub const fn committed(&self) -> u64 {
        self.committed
    }

    pub(crate) fn erase_transcript(&mut self, thread_id: ThreadId) {
        let run_ids = self
            .runs
            .values()
            .filter(|run| run.thread_id == thread_id)
            .map(|run| run.id)
            .collect::<Vec<_>>();
        self.threads.remove(&thread_id);
        self.thread_stored_at.remove(&thread_id);
        self.runs.retain(|_, run| run.thread_id != thread_id);
        self.attempts
            .retain(|_, attempt| !run_ids.contains(&attempt.run_id));
        self.approvals
            .retain(|_, approval| !run_ids.contains(&approval.run_id));
        self.auth_requests
            .retain(|run_id, _| !run_ids.contains(run_id));
        self.run_costs.retain(|run_id, _| !run_ids.contains(run_id));
        self.run_prompts
            .retain(|run_id, _| !run_ids.contains(run_id));
        self.run_transcripts
            .retain(|run_id, _| !run_ids.contains(run_id));
        self.deletion_intents
            .retain(|intent| intent.thread_id != thread_id);
        self.events.retain(|event| {
            event.stream != thread_id.to_string()
                && !run_ids
                    .iter()
                    .any(|run_id| event.stream == run_id.to_string())
        });
    }

    pub(crate) fn erase_artifact(&mut self, artifact_id: ArtifactId) {
        self.artifacts.remove(&artifact_id);
        self.artifact_stored_at.remove(&artifact_id);
    }

    pub fn project(&self, id: ProjectId) -> Option<&ProjectProjection> {
        self.projects.get(&id)
    }

    pub fn thread(&self, id: ThreadId) -> Option<&ThreadProjection> {
        self.threads.get(&id)
    }

    pub fn run(&self, id: RunId) -> Option<&RunProjection> {
        self.runs.get(&id)
    }

    pub fn attempt(&self, id: AttemptId) -> Option<&AttemptProjection> {
        self.attempts.get(&id)
    }

    fn apply(
        &mut self,
        principal_id: PrincipalId,
        stored_at_unix_micros: i64,
        commit_position: u64,
        idempotency_key: &str,
        command: &Command,
    ) -> Result<(), ServiceError> {
        match command {
            Command::CreateProject { project_id, .. } => {
                self.projects.insert(
                    *project_id,
                    ProjectProjection {
                        id: *project_id,
                        principal_id,
                        retention: None,
                        version: 1,
                    },
                );
            }
            Command::SetProjectRetention {
                project_id, policy, ..
            } => {
                let project = self.project_mut(*project_id)?;
                project.retention = Some(*policy);
                project.version += 1;
            }
            Command::CreateThread {
                thread_id,
                project_id,
                ..
            } => {
                self.threads.insert(
                    *thread_id,
                    ThreadProjection {
                        id: *thread_id,
                        project_id: *project_id,
                        archived: false,
                        deletion_requested: false,
                        version: 1,
                    },
                );
                self.thread_stored_at
                    .insert(*thread_id, stored_at_unix_micros);
            }
            Command::SetThreadArchived {
                thread_id,
                archived,
                ..
            } => {
                let thread = self.thread_mut(*thread_id)?;
                thread.archived = *archived;
                thread.version += 1;
            }
            Command::InitiateThreadDeletion { thread_id, .. } => {
                let thread = self.threads.get(thread_id).ok_or(ServiceError::NotFound)?;
                let project = self
                    .projects
                    .get(&thread.project_id)
                    .ok_or(ServiceError::NotFound)?;
                self.deletion_intents.push(DeletionIntent {
                    id: u128::from(commit_position),
                    principal_id,
                    project_id: project.id,
                    thread_id: *thread_id,
                    idempotency_key: idempotency_key.to_owned(),
                    resource_version: thread.version,
                    policy: project.retention.unwrap_or(RetentionPolicy::FOREVER),
                    requested_at_unix_micros: stored_at_unix_micros,
                });
                let thread = self.thread_mut(*thread_id)?;
                thread.deletion_requested = true;
                thread.version += 1;
            }
            Command::StartRun {
                run_id,
                thread_id,
                input,
                effective_config,
                ..
            } => {
                let snapshot = RunConfigSnapshot::from_canonical_bytes(
                    effective_config.as_deref().ok_or_else(|| {
                        ServiceError::Store("run config snapshot is missing".to_owned())
                    })?,
                )
                .map_err(|error| ServiceError::Store(error.to_string()))?;
                self.runs.insert(
                    *run_id,
                    RunProjection {
                        id: *run_id,
                        thread_id: *thread_id,
                        state: RunState::Queued,
                        input: input.clone(),
                        auth_granted: None,
                        effective_config: snapshot.reference(),
                        owner: None,
                        output: None,
                        failure: None,
                        version: 1,
                    },
                );
            }
            Command::TransitionRun {
                run_id,
                transition,
                replacement_owner,
                ..
            } => {
                let run = self.run_mut(*run_id)?;
                if (transition.to() == RunState::Completed && run.failure.is_some())
                    || (transition.to() == RunState::Failed && run.output.is_some())
                {
                    return Err(ServiceError::Store(
                        "run terminal state conflicts with its outcome projection".to_owned(),
                    ));
                }
                run.state = transition.to();
                if let Some(owner) = replacement_owner {
                    run.owner = Some(*owner);
                }
                run.version += 1;
                if let Some(owner) = replacement_owner {
                    self.attempts.insert(
                        owner.attempt_id,
                        AttemptProjection {
                            id: owner.attempt_id,
                            run_id: *run_id,
                            state: AttemptState::Leased,
                            owner: *owner,
                            version: 1,
                        },
                    );
                }
            }
            Command::CancelRun { run_id, .. } => {
                let run = self.run_mut(*run_id)?;
                run.state = RunState::Cancelling;
                run.version += 1;
            }
            Command::ProvideRunInput { run_id, input, .. } => {
                let run = self.run_mut(*run_id)?;
                run.input = input.clone();
                run.state = RunState::Running;
                run.version += 1;
            }
            Command::StartAttempt {
                attempt_id,
                run_id,
                owner,
                ..
            } => {
                self.attempts.insert(
                    *attempt_id,
                    AttemptProjection {
                        id: *attempt_id,
                        run_id: *run_id,
                        state: AttemptState::Leased,
                        owner: *owner,
                        version: 1,
                    },
                );
                let run = self.run_mut(*run_id)?;
                run.owner = Some(*owner);
                run.version += 1;
            }
            Command::TransitionAttempt {
                attempt_id,
                transition,
                ..
            } => {
                let attempt = self
                    .attempts
                    .get_mut(attempt_id)
                    .ok_or(ServiceError::NotFound)?;
                attempt.state = transition.to();
                attempt.version += 1;
            }
            Command::RequestApproval {
                approval_id,
                run_id,
                ..
            } => {
                self.approvals.insert(
                    *approval_id,
                    ApprovalProjection {
                        id: *approval_id,
                        run_id: *run_id,
                        decision: None,
                        version: 1,
                    },
                );
            }
            Command::ResolveApproval {
                approval_id,
                decision,
                ..
            } => {
                let approval = self
                    .approvals
                    .get_mut(approval_id)
                    .ok_or(ServiceError::NotFound)?;
                approval.decision = Some(*decision);
                approval.version += 1;
            }
            Command::RequestAuth { run_id, .. } => {
                self.auth_requests.insert(
                    *run_id,
                    AuthRequestProjection {
                        run_id: *run_id,
                        granted: None,
                        version: 1,
                    },
                );
                self.run_mut(*run_id)?.version += 1;
            }
            Command::ResolveAuth {
                run_id, granted, ..
            } => {
                let request = self
                    .auth_requests
                    .get_mut(run_id)
                    .ok_or(ServiceError::NotFound)?;
                request.granted = Some(*granted);
                request.version += 1;
                let run = self.run_mut(*run_id)?;
                run.auth_granted = Some(*granted);
                run.version += 1;
            }
            Command::RegisterArtifactMetadata {
                artifact_id,
                project_id,
                reference,
                media_type,
                size,
                ..
            } => {
                self.artifacts.insert(
                    *artifact_id,
                    ArtifactMetadataProjection {
                        id: *artifact_id,
                        project_id: *project_id,
                        reference: reference.clone(),
                        media_type: media_type.clone(),
                        size: *size,
                    },
                );
                self.artifact_stored_at
                    .insert(*artifact_id, stored_at_unix_micros);
            }
        }
        Ok(())
    }

    pub(crate) fn validate(
        &self,
        principal_id: PrincipalId,
        command: &Command,
    ) -> Result<(), ServiceError> {
        match command {
            Command::CreateProject { project_id, .. } if self.projects.contains_key(project_id) => {
                Err(conflict("project already exists"))
            }
            Command::SetProjectRetention {
                project_id,
                expected_version,
                ..
            } => check_version(
                self.projects.get(project_id).map(|item| item.version),
                *expected_version,
            ),
            Command::CreateThread {
                thread_id,
                project_id,
                ..
            } => {
                require(self.projects.contains_key(project_id))?;
                if self.threads.contains_key(thread_id) {
                    Err(conflict("thread already exists"))
                } else {
                    Ok(())
                }
            }
            Command::SetThreadArchived {
                thread_id,
                expected_version,
                ..
            }
            | Command::InitiateThreadDeletion {
                thread_id,
                expected_version,
                ..
            } => check_version(
                self.threads.get(thread_id).map(|item| item.version),
                *expected_version,
            ),
            Command::StartRun {
                run_id,
                thread_id,
                effective_config,
                ..
            } => {
                require(self.threads.contains_key(thread_id))?;
                if self.runs.contains_key(run_id) {
                    Err(conflict("run already exists"))
                } else {
                    let snapshot = RunConfigSnapshot::from_canonical_bytes(
                        effective_config.as_deref().ok_or_else(|| {
                            ServiceError::Invalid("run config snapshot is missing".to_owned())
                        })?,
                    )
                    .map_err(|error| ServiceError::Invalid(error.to_string()))?;
                    let project_id = self
                        .threads
                        .get(thread_id)
                        .map(|thread| thread.project_id)
                        .ok_or(ServiceError::NotFound)?;
                    if snapshot.run_id() != *run_id
                        || snapshot.project_id() != project_id
                        || snapshot.principal_id() != principal_id
                    {
                        Err(ServiceError::Invalid(
                            "run config snapshot identity does not match command".to_owned(),
                        ))
                    } else {
                        Ok(())
                    }
                }
            }
            Command::TransitionRun {
                run_id,
                transition,
                expected_version,
                expected_owner,
                replacement_owner,
                ..
            } => {
                let run = self.runs.get(run_id).ok_or(ServiceError::NotFound)?;
                check_version(Some(run.version), *expected_version)?;
                if run.owner != *expected_owner {
                    return Err(conflict("expected attempt owner or fence does not match"));
                }
                if run.state != transition.from() {
                    Err(conflict(
                        "run transition source does not match current state",
                    ))
                } else if run.state == RunState::Interrupted && transition.to() == RunState::Queued
                {
                    let old = run
                        .owner
                        .ok_or_else(|| conflict("retry requires an owner"))?;
                    let replacement = replacement_owner
                        .ok_or_else(|| conflict("retry requires a new attempt"))?;
                    if replacement.attempt_id == old.attempt_id
                        || replacement.fencing_token <= old.fencing_token
                        || replacement.principal_id != old.principal_id
                        || self.attempts.contains_key(&replacement.attempt_id)
                    {
                        Err(conflict("retry requires a new attempt and advanced fence"))
                    } else {
                        Ok(())
                    }
                } else if replacement_owner.is_some() {
                    Err(conflict("owner replacement is only valid for retry"))
                } else {
                    Ok(())
                }
            }
            Command::CancelRun {
                run_id,
                expected_version,
                ..
            } => {
                let run = self.runs.get(run_id).ok_or(ServiceError::NotFound)?;
                check_version(Some(run.version), *expected_version)?;
                super::lifecycle::RunTransition::new(run.state, RunState::Cancelling)
                    .map(|_| ())
                    .map_err(|_| conflict("run cannot be cancelled from its current state"))
            }
            Command::ProvideRunInput {
                run_id,
                expected_version,
                ..
            } => {
                let run = self.runs.get(run_id).ok_or(ServiceError::NotFound)?;
                check_version(Some(run.version), *expected_version)?;
                if run.state == RunState::WaitingForInput {
                    super::lifecycle::RunTransition::new(run.state, RunState::Running)
                        .map(|_| ())
                        .map_err(|_| conflict("run is not waiting for input"))
                } else {
                    Err(conflict("run is not waiting for input"))
                }
            }
            Command::StartAttempt {
                attempt_id,
                run_id,
                owner,
                expected_version,
                ..
            } => {
                let run = self.runs.get(run_id).ok_or(ServiceError::NotFound)?;
                check_version(Some(run.version), *expected_version)?;
                if owner.attempt_id != *attempt_id || owner.principal_id != principal_id {
                    Err(conflict(
                        "attempt ownership does not match command principal",
                    ))
                } else if run.owner.is_some() {
                    Err(conflict("run already has an attempt owner"))
                } else if self.attempts.contains_key(attempt_id) {
                    Err(conflict("attempt already exists"))
                } else {
                    Ok(())
                }
            }
            Command::TransitionAttempt {
                attempt_id,
                transition,
                expected_version,
                expected_owner,
                ..
            } => {
                let attempt = self
                    .attempts
                    .get(attempt_id)
                    .ok_or(ServiceError::NotFound)?;
                check_version(Some(attempt.version), *expected_version)?;
                if attempt.owner != *expected_owner || expected_owner.principal_id != principal_id {
                    Err(conflict("expected attempt owner or fence does not match"))
                } else if attempt.state != transition.from() {
                    Err(conflict(
                        "attempt transition source does not match current state",
                    ))
                } else {
                    Ok(())
                }
            }
            Command::RequestApproval {
                approval_id,
                run_id,
                ..
            } => {
                require(self.runs.contains_key(run_id))?;
                if self.approvals.contains_key(approval_id) {
                    Err(conflict("approval already exists"))
                } else {
                    Ok(())
                }
            }
            Command::ResolveApproval {
                approval_id,
                expected_version,
                ..
            } => {
                let approval = self
                    .approvals
                    .get(approval_id)
                    .ok_or(ServiceError::NotFound)?;
                check_version(Some(approval.version), *expected_version)?;
                if approval.decision.is_some() {
                    Err(conflict("approval is already resolved"))
                } else {
                    Ok(())
                }
            }
            Command::RequestAuth {
                run_id,
                expected_version,
                ..
            } => {
                let run = self.runs.get(run_id).ok_or(ServiceError::NotFound)?;
                check_version(Some(run.version), *expected_version)?;
                if self.auth_requests.contains_key(run_id) {
                    Err(conflict("auth request already exists"))
                } else {
                    Ok(())
                }
            }
            Command::ResolveAuth {
                run_id,
                expected_version,
                ..
            } => {
                let request = self
                    .auth_requests
                    .get(run_id)
                    .ok_or(ServiceError::NotFound)?;
                let run = self.runs.get(run_id).ok_or(ServiceError::NotFound)?;
                check_version(Some(run.version), *expected_version)?;
                if request.granted.is_some() {
                    Err(conflict("auth request is already resolved"))
                } else {
                    Ok(())
                }
            }
            Command::RegisterArtifactMetadata {
                artifact_id,
                project_id,
                media_type,
                ..
            } => {
                require(self.projects.contains_key(project_id))?;
                if self.artifacts.contains_key(artifact_id) {
                    return Err(conflict("artifact metadata already exists"));
                }
                if media_type.is_empty() || !media_type.contains('/') {
                    Err(ServiceError::Invalid(
                        "invalid artifact media type".to_owned(),
                    ))
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn scope(&self, resource: Resource) -> Result<ResourceScope, ServiceError> {
        let project = match resource {
            Resource::Project(id) => self.projects.get(&id).map(|project| project.id),
            Resource::Thread(id) => self.threads.get(&id).map(|thread| thread.project_id),
            Resource::Run(id) | Resource::AuthRequest(id) => self.project_for_run(id),
            Resource::Attempt(id) => self
                .attempts
                .get(&id)
                .and_then(|attempt| self.project_for_run(attempt.run_id)),
            Resource::Approval(id) => self
                .approvals
                .get(&id)
                .and_then(|approval| self.project_for_run(approval.run_id)),
            Resource::Artifact(id) => self.artifacts.get(&id).map(|artifact| artifact.project_id),
        }
        .ok_or(ServiceError::NotFound)?;
        let principal = self
            .projects
            .get(&project)
            .map(|project| project.principal_id)
            .ok_or(ServiceError::NotFound)?;
        Ok(ResourceScope::new(principal, project))
    }

    pub(crate) fn project_for_run(&self, run_id: RunId) -> Option<ProjectId> {
        self.runs
            .get(&run_id)
            .and_then(|run| self.threads.get(&run.thread_id))
            .map(|thread| thread.project_id)
    }

    pub(crate) fn scope_for_command(
        &self,
        command: &Command,
    ) -> Result<IndexedEventScope, ServiceError> {
        let project_id = self.command_project(command)?;
        self.indexed_scope(project_id, command)
    }

    fn indexed_scope(
        &self,
        project_id: ProjectId,
        command: &Command,
    ) -> Result<IndexedEventScope, ServiceError> {
        let (thread_id, run_id) = match command {
            Command::CreateThread { thread_id, .. }
            | Command::SetThreadArchived { thread_id, .. }
            | Command::InitiateThreadDeletion { thread_id, .. } => (Some(*thread_id), None),
            Command::StartRun {
                thread_id, run_id, ..
            } => (Some(*thread_id), Some(*run_id)),
            Command::TransitionRun { run_id, .. }
            | Command::CancelRun { run_id, .. }
            | Command::ProvideRunInput { run_id, .. }
            | Command::StartAttempt { run_id, .. }
            | Command::RequestApproval { run_id, .. }
            | Command::RequestAuth { run_id, .. }
            | Command::ResolveAuth { run_id, .. } => (
                Some(
                    self.runs
                        .get(run_id)
                        .ok_or(ServiceError::NotFound)?
                        .thread_id,
                ),
                Some(*run_id),
            ),
            Command::TransitionAttempt { attempt_id, .. } => {
                let run_id = self
                    .attempts
                    .get(attempt_id)
                    .ok_or(ServiceError::NotFound)?
                    .run_id;
                (
                    Some(
                        self.runs
                            .get(&run_id)
                            .ok_or(ServiceError::NotFound)?
                            .thread_id,
                    ),
                    Some(run_id),
                )
            }
            Command::ResolveApproval { approval_id, .. } => {
                let run_id = self
                    .approvals
                    .get(approval_id)
                    .ok_or(ServiceError::NotFound)?
                    .run_id;
                (
                    Some(
                        self.runs
                            .get(&run_id)
                            .ok_or(ServiceError::NotFound)?
                            .thread_id,
                    ),
                    Some(run_id),
                )
            }
            _ => (None, None),
        };
        Ok(IndexedEventScope {
            project_id,
            thread_id,
            run_id,
        })
    }

    fn semantic_scope(
        &self,
        project_id: ProjectId,
        run_id: RunId,
    ) -> Result<IndexedEventScope, ServiceError> {
        let run = self.runs.get(&run_id).ok_or(ServiceError::NotFound)?;
        Ok(IndexedEventScope {
            project_id,
            thread_id: Some(run.thread_id),
            run_id: Some(run_id),
        })
    }

    fn require_semantic_run<T>(
        &self,
        stored: &RunSemanticEnvelope<T>,
        event: &StoredEvent,
    ) -> Result<(), ServiceError> {
        if stored.schema_version != 1
            || event.event.stream != crate::domain::events::EntityId::Run(stored.run_id)
            || event.event.correlation_id != crate::domain::events::EntityId::Run(stored.run_id)
            || event.event.attempt_id != Some(stored.attempt.attempt_id)
            || self.project_for_run(stored.run_id) != Some(stored.project_id)
            || self.runs.get(&stored.run_id).and_then(|run| run.owner) != Some(stored.attempt)
        {
            return Err(ServiceError::Store(
                "run semantic event correlation is invalid".to_owned(),
            ));
        }
        Ok(())
    }

    fn command_project(&self, command: &Command) -> Result<ProjectId, ServiceError> {
        match command {
            Command::CreateProject { project_id, .. }
            | Command::SetProjectRetention { project_id, .. }
            | Command::CreateThread { project_id, .. }
            | Command::RegisterArtifactMetadata { project_id, .. } => Ok(*project_id),
            Command::SetThreadArchived { thread_id, .. }
            | Command::InitiateThreadDeletion { thread_id, .. }
            | Command::StartRun { thread_id, .. } => self
                .threads
                .get(thread_id)
                .map(|thread| thread.project_id)
                .ok_or(ServiceError::NotFound),
            Command::TransitionRun { run_id, .. }
            | Command::CancelRun { run_id, .. }
            | Command::ProvideRunInput { run_id, .. }
            | Command::StartAttempt { run_id, .. }
            | Command::RequestApproval { run_id, .. }
            | Command::RequestAuth { run_id, .. }
            | Command::ResolveAuth { run_id, .. } => {
                self.project_for_run(*run_id).ok_or(ServiceError::NotFound)
            }
            Command::TransitionAttempt { attempt_id, .. } => self
                .attempts
                .get(attempt_id)
                .and_then(|attempt| self.project_for_run(attempt.run_id))
                .ok_or(ServiceError::NotFound),
            Command::ResolveApproval { approval_id, .. } => self
                .approvals
                .get(approval_id)
                .and_then(|approval| self.project_for_run(approval.run_id))
                .ok_or(ServiceError::NotFound),
        }
    }

    fn project_mut(&mut self, id: ProjectId) -> Result<&mut ProjectProjection, ServiceError> {
        self.projects.get_mut(&id).ok_or(ServiceError::NotFound)
    }

    fn thread_mut(&mut self, id: ThreadId) -> Result<&mut ThreadProjection, ServiceError> {
        self.threads.get_mut(&id).ok_or(ServiceError::NotFound)
    }

    fn run_mut(&mut self, id: RunId) -> Result<&mut RunProjection, ServiceError> {
        self.runs.get_mut(&id).ok_or(ServiceError::NotFound)
    }
}

fn check_version(actual: Option<u64>, expected: u64) -> Result<(), ServiceError> {
    let actual = actual.ok_or(ServiceError::NotFound)?;
    if actual == expected {
        Ok(())
    } else {
        Err(conflict("expected version does not match"))
    }
}

fn require(condition: bool) -> Result<(), ServiceError> {
    condition.then_some(()).ok_or(ServiceError::NotFound)
}

fn conflict(message: &str) -> ServiceError {
    ServiceError::Conflict(message.to_owned())
}
