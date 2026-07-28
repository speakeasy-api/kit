use serde::{Deserialize, Serialize};

use super::ids::{
    AgentLinkId, ApprovalId, ArtifactId, AttemptId, CheckpointId, DaemonServiceId, ExperimentId,
    ExternalTaskId, IdGenerationError, ModelCallId, PrincipalId, ProcessId, ProjectId, RunId,
    TaskId, TerminalId, ThreadId, ToolCallId, TurnId, WorkspaceId,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Principal,
    Project,
    Thread,
    Run,
    Attempt,
    Turn,
    ModelCall,
    ToolCall,
    Task,
    AgentLink,
    ExternalTask,
    DaemonService,
    Workspace,
    Process,
    Terminal,
    Approval,
    Checkpoint,
    Artifact,
    Experiment,
}

impl EntityKind {
    pub const ALL: [Self; 19] = [
        Self::Principal,
        Self::Project,
        Self::Thread,
        Self::Run,
        Self::Attempt,
        Self::Turn,
        Self::ModelCall,
        Self::ToolCall,
        Self::Task,
        Self::AgentLink,
        Self::ExternalTask,
        Self::DaemonService,
        Self::Workspace,
        Self::Process,
        Self::Terminal,
        Self::Approval,
        Self::Checkpoint,
        Self::Artifact,
        Self::Experiment,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Principal => "Principal",
            Self::Project => "Project",
            Self::Thread => "Thread",
            Self::Run => "Run",
            Self::Attempt => "Attempt",
            Self::Turn => "Turn",
            Self::ModelCall => "ModelCall",
            Self::ToolCall => "ToolCall",
            Self::Task => "Task",
            Self::AgentLink => "AgentLink",
            Self::ExternalTask => "ExternalTask",
            Self::DaemonService => "DaemonService",
            Self::Workspace => "Workspace",
            Self::Process => "Process",
            Self::Terminal => "Terminal",
            Self::Approval => "Approval",
            Self::Checkpoint => "Checkpoint",
            Self::Artifact => "Artifact",
            Self::Experiment => "Experiment",
        }
    }
}

macro_rules! entity {
    ($(#[$meta:meta])* $name:ident, $id:ty { $($field:ident: $type:ty),* $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
        pub struct $name {
            pub id: $id,
            $(pub $field: $type,)*
            pub version: u64,
        }

        impl $name {
            pub fn new($($field: $type),*) -> Result<Self, IdGenerationError> {
                Ok(Self {
                    id: <$id>::generate()?,
                    $($field,)*
                    version: 0,
                })
            }
        }
    };
}

entity!(
    /// A user, service, or agent identity and policy subject.
    Principal, PrincipalId {}
);
entity!(
    /// Repository configuration, prompt policy, indexes, and evaluation attribution.
    Project, ProjectId { principal_id: PrincipalId }
);
entity!(
    /// Durable user-visible conversation and task history.
    Thread, ThreadId { project_id: ProjectId }
);
entity!(
    /// One requested agent execution.
    Run, RunId { thread_id: ThreadId }
);
entity!(
    /// One lease-bound execution of a run.
    Attempt, AttemptId { run_id: RunId }
);
entity!(
    /// One user-visible prompt-to-yield interval.
    Turn, TurnId { owner_attempt_id: AttemptId }
);
entity!(
    /// One provider inference request.
    ModelCall, ModelCallId {
        owner_attempt_id: AttemptId,
        turn_id: TurnId
    }
);
entity!(
    /// One direct or nested capability invocation.
    ToolCall, ToolCallId {
        owner_attempt_id: AttemptId,
        turn_id: TurnId,
        parent_tool_call_id: Option<ToolCallId>
    }
);
entity!(
    /// Background tool or process work owned by a run.
    Task, TaskId { owner_run_id: RunId }
);
entity!(
    /// An ACP subagent or A2A peer relationship.
    AgentLink, AgentLinkId { owner_attempt_id: AttemptId }
);
entity!(
    /// Durable A2A task and message exchange attached to a peer relationship.
    ExternalTask, ExternalTaskId { agent_link_id: AgentLinkId }
);
entity!(
    /// The scoped owner of long-lived daemon processes.
    DaemonService, DaemonServiceId {
        principal_id: PrincipalId,
        project_id: ProjectId,
        workspace_id: WorkspaceId
    }
);
entity!(
    /// A revisioned mutable checkout or immutable snapshot.
    Workspace, WorkspaceId { project_id: ProjectId }
);

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum ProcessOwner {
    Attempt(AttemptId),
    DaemonService(DaemonServiceId),
}

entity!(
    /// An OS or sandbox process with exactly one declared owner.
    Process, ProcessId { owner: ProcessOwner }
);
entity!(
    /// A PTY attached to an attempt-owned process.
    Terminal, TerminalId {
        process_id: ProcessId,
        owner_attempt_id: AttemptId
    }
);
entity!(
    /// A durable human or policy decision owned by a run.
    Approval, ApprovalId { owner_run_id: RunId }
);
entity!(
    /// A model-selected semantic compaction boundary owned by a run.
    Checkpoint, CheckpointId { owner_run_id: RunId }
);
entity!(
    /// A project-owned reference to content-addressed data.
    Artifact, ArtifactId { project_id: ProjectId }
);
entity!(
    /// A project-owned configuration assignment and measured outcome.
    Experiment, ExperimentId { project_id: ProjectId }
);
