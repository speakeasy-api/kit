use std::collections::HashSet;

use kit::domain::entities::{
    AgentLink, Approval, Artifact, Attempt, Checkpoint, DaemonService, EntityKind, Experiment,
    ExternalTask, ModelCall, Principal, Process, ProcessOwner, Project, Run, Task, Terminal,
    Thread, ToolCall, Turn, Workspace,
};
use kit::domain::ids::{
    AgentLinkId, ApprovalId, ArtifactId, AttemptId, CheckpointId, CommandId, DaemonServiceId,
    EventId, ExperimentId, ExternalTaskId, IdParseError, ModelCallId, PrincipalId, ProcessId,
    ProjectId, RunId, TaskId, TerminalId, ThreadId, ToolCallId, TurnId, WorkspaceId,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

fn roundtrip<T>(value: T)
where
    T: Serialize + DeserializeOwned + Eq + std::fmt::Debug,
{
    let wire = serde_json::to_string(&value).unwrap();
    assert_eq!(serde_json::from_str::<T>(&wire).unwrap(), value);
}

macro_rules! assert_ids_roundtrip {
    ($($type:ty),+ $(,)?) => {
        $(
            let id = <$type>::generate().unwrap();
            let wire = id.to_string();
            assert_eq!(wire.parse::<$type>().unwrap(), id);
            assert_eq!(serde_json::to_string(&id).unwrap(), format!("\"{wire}\""));
            assert_eq!(format!("{id:?}"), format!("{}({id})", stringify!($type)));
            roundtrip(id);
        )+
    };
}

#[test]
fn typed_ids_have_canonical_roundtrippable_wire_forms() {
    assert_ids_roundtrip!(
        PrincipalId,
        ProjectId,
        ThreadId,
        RunId,
        AttemptId,
        TurnId,
        ModelCallId,
        ToolCallId,
        TaskId,
        AgentLinkId,
        ExternalTaskId,
        DaemonServiceId,
        WorkspaceId,
        ProcessId,
        TerminalId,
        ApprovalId,
        CheckpointId,
        ArtifactId,
        ExperimentId,
        CommandId,
        EventId,
    );
}

#[test]
fn typed_ids_reject_other_types_and_noncanonical_values() {
    let principal = PrincipalId::generate().unwrap().to_string();
    assert!(matches!(
        ProjectId::parse(&principal),
        Err(IdParseError::InvalidPrefix { .. })
    ));

    for malformed in [
        "principal_",
        "principal_0000000000000000000000000",
        "principal_000000000000000000000000000",
        "principal_0000000000000000000000000i",
        "principal_0000000000000000000000000U",
    ] {
        assert!(
            PrincipalId::parse(malformed).is_err(),
            "accepted {malformed}"
        );
        assert!(serde_json::from_str::<PrincipalId>(&format!("\"{malformed}\"")).is_err());
    }
    assert!(matches!(
        PrincipalId::parse("principal_80000000000000000000000000"),
        Err(IdParseError::Overflow)
    ));
}

#[test]
fn generated_ids_are_unique() {
    let ids: HashSet<_> = (0..10_000).map(|_| EventId::generate().unwrap()).collect();
    assert_eq!(ids.len(), 10_000);
}

#[test]
fn entity_set_matches_rfc_section_10() {
    let actual: HashSet<_> = EntityKind::ALL.into_iter().map(EntityKind::name).collect();
    let expected: HashSet<_> = [
        "Principal",
        "Project",
        "Thread",
        "Run",
        "Attempt",
        "Turn",
        "ModelCall",
        "ToolCall",
        "Task",
        "AgentLink",
        "ExternalTask",
        "DaemonService",
        "Workspace",
        "Process",
        "Terminal",
        "Approval",
        "Checkpoint",
        "Artifact",
        "Experiment",
    ]
    .into_iter()
    .collect();
    assert_eq!(actual, expected);
}

#[test]
fn entities_roundtrip_with_static_ownership_and_versions() {
    let principal = Principal::new().unwrap();
    let project = Project::new(principal.id).unwrap();
    let thread = Thread::new(project.id).unwrap();
    let run = Run::new(thread.id).unwrap();
    let attempt = Attempt::new(run.id).unwrap();
    let turn = Turn::new(attempt.id).unwrap();
    let model_call = ModelCall::new(attempt.id, turn.id).unwrap();
    let tool_call = ToolCall::new(attempt.id, turn.id, None).unwrap();
    let nested_tool_call = ToolCall::new(attempt.id, turn.id, Some(tool_call.id)).unwrap();
    let task = Task::new(run.id).unwrap();
    let agent_link = AgentLink::new(attempt.id).unwrap();
    let external_task = ExternalTask::new(agent_link.id).unwrap();
    let workspace = Workspace::new(project.id).unwrap();
    let daemon_service = DaemonService::new(principal.id, project.id, workspace.id).unwrap();
    let process = Process::new(ProcessOwner::Attempt(attempt.id)).unwrap();
    let service_process = Process::new(ProcessOwner::DaemonService(daemon_service.id)).unwrap();
    let terminal = Terminal::new(process.id, attempt.id).unwrap();
    let approval = Approval::new(run.id).unwrap();
    let checkpoint = Checkpoint::new(run.id).unwrap();
    let artifact = Artifact::new(project.id).unwrap();
    let experiment = Experiment::new(project.id).unwrap();

    roundtrip(principal);
    roundtrip(project);
    roundtrip(thread);
    roundtrip(run);
    roundtrip(attempt);
    roundtrip(turn);
    roundtrip(model_call);
    roundtrip(tool_call);
    roundtrip(nested_tool_call);
    roundtrip(task);
    roundtrip(agent_link);
    roundtrip(external_task);
    roundtrip(workspace);
    roundtrip(daemon_service);
    roundtrip(process);
    roundtrip(service_process);
    roundtrip(terminal);
    roundtrip(approval);
    roundtrip(checkpoint);
    roundtrip(artifact);
    roundtrip(experiment);
}

#[test]
fn ids_do_not_offer_unchecked_string_construction() {
    let source = include_str!("../../src/domain/ids.rs");
    assert!(!source.contains("From<String> for"));
    assert!(!source.contains("Into<String>"));
    assert!(!source.contains("pub String"));
}
