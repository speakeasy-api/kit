use serde::{Deserialize, Serialize};

use super::config::ConfigLayer;
use super::events::{
    ApprovalDecision, ArtifactRecordId, ArtifactRef, AttemptTransition, RunTransition,
    SchemaVersion,
};
use super::ids::{ApprovalId, AttemptId, ProjectId, RunId, ThreadId};
use super::lifecycle::AttemptOwnership;
use super::retention::RetentionPolicy;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ExpectedVersion(u64);

impl ExpectedVersion {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    CreateProject {
        schema_version: SchemaVersion,
        project_id: ProjectId,
    },
    SetProjectRetention {
        schema_version: SchemaVersion,
        project_id: ProjectId,
        policy: RetentionPolicy,
        expected_version: u64,
    },
    CreateThread {
        schema_version: SchemaVersion,
        thread_id: ThreadId,
        project_id: ProjectId,
    },
    SetThreadArchived {
        schema_version: SchemaVersion,
        thread_id: ThreadId,
        archived: bool,
        expected_version: u64,
    },
    InitiateThreadDeletion {
        schema_version: SchemaVersion,
        thread_id: ThreadId,
        expected_version: u64,
    },
    StartRun {
        schema_version: SchemaVersion,
        run_id: RunId,
        thread_id: ThreadId,
        input: ArtifactRef,
        #[serde(default)]
        run_config: Option<Box<ConfigLayer>>,
        #[serde(default)]
        experiment_config: Option<Box<ConfigLayer>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effective_config: Option<Vec<u8>>,
    },
    TransitionRun {
        schema_version: SchemaVersion,
        run_id: RunId,
        transition: RunTransition,
        expected_version: u64,
        #[serde(default)]
        expected_owner: Option<AttemptOwnership>,
        #[serde(default)]
        replacement_owner: Option<AttemptOwnership>,
    },
    CancelRun {
        schema_version: SchemaVersion,
        run_id: RunId,
        expected_version: u64,
    },
    ProvideRunInput {
        schema_version: SchemaVersion,
        run_id: RunId,
        input: ArtifactRef,
        expected_version: u64,
    },
    StartAttempt {
        schema_version: SchemaVersion,
        attempt_id: AttemptId,
        run_id: RunId,
        owner: AttemptOwnership,
        expected_version: u64,
    },
    TransitionAttempt {
        schema_version: SchemaVersion,
        attempt_id: AttemptId,
        transition: AttemptTransition,
        expected_version: u64,
        expected_owner: AttemptOwnership,
    },
    RequestApproval {
        schema_version: SchemaVersion,
        approval_id: ApprovalId,
        run_id: RunId,
    },
    ResolveApproval {
        schema_version: SchemaVersion,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
        expected_version: u64,
    },
    RequestAuth {
        schema_version: SchemaVersion,
        run_id: RunId,
        expected_version: u64,
    },
    ResolveAuth {
        schema_version: SchemaVersion,
        run_id: RunId,
        granted: bool,
        expected_version: u64,
    },
    RegisterArtifactMetadata {
        schema_version: SchemaVersion,
        artifact_id: ArtifactRecordId,
        project_id: ProjectId,
        reference: ArtifactRef,
        media_type: String,
        size: u64,
    },
}

impl Command {
    pub const fn schema_version(&self) -> SchemaVersion {
        match self {
            Self::CreateProject { schema_version, .. }
            | Self::SetProjectRetention { schema_version, .. }
            | Self::CreateThread { schema_version, .. }
            | Self::SetThreadArchived { schema_version, .. }
            | Self::InitiateThreadDeletion { schema_version, .. }
            | Self::StartRun { schema_version, .. }
            | Self::TransitionRun { schema_version, .. }
            | Self::CancelRun { schema_version, .. }
            | Self::ProvideRunInput { schema_version, .. }
            | Self::StartAttempt { schema_version, .. }
            | Self::TransitionAttempt { schema_version, .. }
            | Self::RequestApproval { schema_version, .. }
            | Self::ResolveApproval { schema_version, .. }
            | Self::RequestAuth { schema_version, .. }
            | Self::ResolveAuth { schema_version, .. }
            | Self::RegisterArtifactMetadata { schema_version, .. } => *schema_version,
        }
    }
}
