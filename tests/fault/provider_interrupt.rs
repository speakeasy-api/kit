use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use agentkit_core::{
    FinishReason, Item, ItemKind, MetadataMap, Part, TextPart, ToolCallId as AgentkitToolCallId,
};
use agentkit_loop::{InputRequest, LoopInterrupt, ModelTurnResult, PendingApproval};
use agentkit_tools_core::{ApprovalReason, ApprovalRequest};
use kit::agent::executor::{
    FakeProvider, FakeResponse, RunExecutor, RunExecutorConfig, SelectedModelAdapter,
};
use kit::{
    agent::{
        agentkit_bridge::mapping::from_agentkit_item,
        driver::{
            restart::{
                BoundarySnapshot, CommittedModelOutcome, LoopCommit, LoopRecord, RecoveryState,
                RestartProjection, SafeBoundary, append_loop_record,
            },
            waiting::{WaitingKind, WaitingResolution, WaitingState},
        },
        providers::{
            cancel::{CancellationOutcome, DispatchState, cancellation_outcome},
            interrupt::{auth_waiting_record, waiting_record},
        },
    },
    api::{
        auth::{
            contract::{Authenticator, GrantSnapshot, ScopedAuthorizer},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        service::{
            AttemptProjection, Command, Query, QueryProjection, RequestContext, RunFailureCode,
            RunFailureProjection, ServiceStore, WorkerStore,
        },
    },
    domain::{
        config::{Grant, Provider as ConfigProvider, StaticRunConfigMaterializer},
        events::{ApprovalDecision, EntityId, RunState, SchemaVersion, TraceId, UtcDateTime},
        ids::{AttemptId, CommandId, EventId, PrincipalId, ProjectId, RunId, ThreadId, ToolCallId},
        lifecycle::{AttemptOwnership, AttemptState, FencingToken},
    },
    executor::cancel::SqliteCancellationCoordinator,
    runtime::scheduler::DurableScheduler,
    store::{
        artifacts::{
            ArtifactClass, ArtifactMetadata, ArtifactRetention, ArtifactStore, now_unix_micros,
        },
        sqlite::{
            append::{CrashPoint, SqliteStore},
            idempotency::IdempotencyKey,
        },
    },
    test_support::{self, open_sqlite_store},
};

const UID: u32 = 501;

struct Journal {
    path: PathBuf,
    store: SqliteStore,
}

impl Journal {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kit-provider-interrupt-{}-{}.sqlite3",
            std::process::id(),
            EventId::generate().unwrap()
        ));
        Self {
            store: open_sqlite_store(&path).unwrap(),
            path,
        }
    }

    fn append(&mut self, owner: AttemptOwnership, version: u64, record: LoopRecord) {
        append_loop_record(&mut self.store, commit(owner, version, record)).unwrap();
    }

    fn events(&self) -> Vec<kit::store::sqlite::append::StoredEvent> {
        self.store.events().unwrap()
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(self.path.with_extension("sqlite3-shm"));
    }
}

fn owner() -> AttemptOwnership {
    AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        PrincipalId::generate().unwrap(),
        FencingToken::new(7),
    )
}

fn projection(owner: AttemptOwnership) -> AttemptProjection {
    AttemptProjection {
        id: owner.attempt_id,
        run_id: RunId::generate().unwrap(),
        state: AttemptState::Executing,
        owner,
        version: 3,
    }
}

fn commit(owner: AttemptOwnership, version: u64, record: LoopRecord) -> LoopCommit {
    LoopCommit {
        owner,
        claim: None,
        expected_stream_version: version,
        idempotency_key: IdempotencyKey::parse(&format!(
            "provider-interrupt-{version}-{}",
            CommandId::generate().unwrap()
        ))
        .unwrap(),
        command_id: CommandId::generate().unwrap(),
        event_id: EventId::generate().unwrap(),
        occurred_at: UtcDateTime::parse("2026-07-22T12:00:00Z").unwrap(),
        trace_id: TraceId::parse("provider-interrupt-test").unwrap(),
        artifacts: Vec::new(),
        record,
    }
}

fn text(kind: ItemKind, value: &str) -> Item {
    Item::new(kind, vec![Part::Text(TextPart::new(value))])
}

fn canonical(items: &[Item]) -> Vec<kit::agent::agentkit_bridge::mapping::CanonicalItem> {
    items.iter().map(from_agentkit_item).collect()
}

fn boundary(
    kind: SafeBoundary,
    transcript: &[Item],
    resume_index: Option<usize>,
    model_outcome: Option<&ModelTurnResult>,
) -> BoundarySnapshot {
    BoundarySnapshot {
        boundary: kind,
        transcript: canonical(transcript),
        resume_index,
        model_outcome: model_outcome.map(CommittedModelOutcome::from_agentkit),
    }
}

fn authenticated(principal: PrincipalId) -> kit::api::auth::contract::AuthenticatedPrincipal {
    let project = ProjectId::generate().unwrap();
    LocalPeerAuthenticator::new(BTreeMap::from([(
        UID,
        GrantSnapshot::new(principal, project, [Grant::WorkspaceRead]),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(UID, 42, UID))
    .unwrap()
}

fn waiting(record: LoopRecord) -> WaitingState {
    match record {
        LoopRecord::Waiting(waiting) => waiting,
        other => panic!("expected waiting record, got {other:?}"),
    }
}

#[test]
fn input_approval_and_auth_interruptions_survive_100_restarts_each() {
    let mut journal = Journal::new();

    for case in 0..100 {
        let owner = owner();
        let projection = projection(owner);
        let principal = authenticated(owner.principal_id);
        let state = waiting(
            waiting_record(
                LoopInterrupt::AwaitingInput(InputRequest {
                    session_id: agentkit_core::SessionId::new(format!("input-{case}")),
                    reason: "input required".into(),
                }),
                &projection,
                boundary(SafeBoundary::TurnEnd, &[], None, None),
            )
            .unwrap(),
        );
        journal.append(owner, 0, LoopRecord::Waiting(state.clone()));
        assert!(matches!(
            RestartProjection::reconstruct(&projection, &journal.events()).unwrap(),
            RecoveryState::Waiting(_)
        ));
        journal.append(
            owner,
            1,
            state
                .resolve_input(&principal, vec![text(ItemKind::User, "resume")])
                .unwrap(),
        );
        assert!(matches!(
            RestartProjection::reconstruct(&projection, &journal.events()).unwrap(),
            RecoveryState::Ready(_)
        ));
    }

    for case in 0..100 {
        let owner = owner();
        let projection = projection(owner);
        let principal = authenticated(owner.principal_id);
        let tool_call_id = ToolCallId::generate().unwrap();
        let output = ModelTurnResult {
            finish_reason: FinishReason::ToolCall,
            output_items: vec![text(ItemKind::Assistant, "tool call")],
            usage: None,
            metadata: MetadataMap::new(),
            model: Some("pinned".into()),
            response_id: None,
        };
        let transcript = [
            text(ItemKind::User, "approve"),
            output.output_items[0].clone(),
        ];
        let request = ApprovalRequest::new(
            format!("approval-{case}"),
            "tool",
            ApprovalReason::PolicyRequiresConfirmation,
            "approve tool",
        )
        .with_call_id(AgentkitToolCallId::new(tool_call_id.to_string()));
        let state = waiting(
            waiting_record(
                LoopInterrupt::ApprovalRequest(PendingApproval { request }),
                &projection,
                boundary(
                    SafeBoundary::AfterModelOutcome,
                    &transcript,
                    Some(0),
                    Some(&output),
                ),
            )
            .unwrap(),
        );
        assert!(matches!(
            state.kind,
            WaitingKind::Approval {
                tool_call_id: mapped,
                ..
            } if mapped == tool_call_id
        ));
        journal.append(owner, 0, LoopRecord::Waiting(state.clone()));
        assert!(matches!(
            RestartProjection::reconstruct(&projection, &journal.events()).unwrap(),
            RecoveryState::Waiting(_)
        ));
        journal.append(
            owner,
            1,
            state
                .resolve(
                    &principal,
                    WaitingResolution::Approval {
                        decision: ApprovalDecision::Denied,
                    },
                )
                .unwrap(),
        );
        assert!(matches!(
            RestartProjection::reconstruct(&projection, &journal.events()).unwrap(),
            RecoveryState::Ready(_)
        ));
    }

    for case in 0..100 {
        let owner = owner();
        let projection = projection(owner);
        let principal = authenticated(owner.principal_id);
        let input = text(ItemKind::User, &format!("auth-{case}"));
        let state = waiting(
            auth_waiting_record(
                &projection,
                projection.run_id,
                "provider.read",
                boundary(SafeBoundary::BeforeModelDispatch, &[input], Some(0), None),
            )
            .unwrap(),
        );
        journal.append(owner, 0, LoopRecord::Waiting(state.clone()));
        assert!(matches!(
            RestartProjection::reconstruct(&projection, &journal.events()).unwrap(),
            RecoveryState::Waiting(_)
        ));
        journal.append(
            owner,
            1,
            state
                .resolve(&principal, WaitingResolution::Auth { granted: true })
                .unwrap(),
        );
        assert!(matches!(
            RestartProjection::reconstruct(&projection, &journal.events()).unwrap(),
            RecoveryState::Ready(_)
        ));
    }
}

#[test]
fn unauthenticated_resolution_and_invalid_auth_boundary_are_rejected() {
    let owner = owner();
    let projection = projection(owner);
    let other = authenticated(PrincipalId::generate().unwrap());
    let state = waiting(
        auth_waiting_record(
            &projection,
            projection.run_id,
            "provider.read",
            boundary(
                SafeBoundary::BeforeModelDispatch,
                &[text(ItemKind::User, "auth")],
                Some(0),
                None,
            ),
        )
        .unwrap(),
    );
    assert!(
        state
            .resolve(&other, WaitingResolution::Auth { granted: true })
            .is_err()
    );
    assert!(
        auth_waiting_record(
            &projection,
            projection.run_id,
            "provider.read",
            boundary(SafeBoundary::TurnEnd, &[], None, None),
        )
        .is_err()
    );
}

#[test]
fn cancellation_races_preserve_dispatch_uncertainty() {
    for _ in 0..100 {
        let controller = agentkit_core::CancellationController::new();
        let checkpoint = controller.handle().checkpoint();
        controller.interrupt();
        assert!(checkpoint.is_cancelled());
        assert_eq!(
            cancellation_outcome(DispatchState::PreDispatch),
            CancellationOutcome::Cancelled
        );
        assert_eq!(
            cancellation_outcome(DispatchState::Dispatched),
            CancellationOutcome::OutcomeUnknown
        );
        assert_eq!(
            cancellation_outcome(DispatchState::OutcomeCommitted),
            CancellationOutcome::AlreadyCommitted
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failure_transaction_crash_restarts_terminal_without_provider_redispatch() {
    let root = std::env::temp_dir().join(format!(
        "kit-provider-failure-crash-{}-{}",
        std::process::id(),
        EventId::generate().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("project")).unwrap();
    fs::write(root.join("project/README.md"), "fixture\n").unwrap();
    for arguments in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Kit Test",
            "-c",
            "user.email=kit@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(arguments)
                .current_dir(root.join("project"))
                .status()
                .unwrap()
                .success()
        );
    }
    let database = root.join("state.sqlite3");
    let artifact_root = root.join("artifacts");
    let artifacts = Arc::new(ArtifactStore::open(&artifact_root).unwrap());
    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let thread_id = ThreadId::generate().unwrap();
    let run_id = RunId::generate().unwrap();
    let grants = [
        Grant::ModelCall,
        Grant::WorkspaceRead,
        Grant::WorkspaceWrite,
        Grant::ProcessSpawn,
        Grant::NetworkEgress,
    ];
    let principal = LocalPeerAuthenticator::new(BTreeMap::from([(
        UID,
        GrantSnapshot::new(principal_id, project_id, grants),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(UID, 42, UID))
    .unwrap();
    let input = artifacts
        .put(
            b"must fail before dispatch",
            ArtifactMetadata::new(
                "text/plain",
                ArtifactClass::File,
                principal_id.to_string(),
                project_id.to_string(),
                ArtifactRetention::Forever,
                now_unix_micros().unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let mut service = test_support::service_with_runtime_and_config(
        test_support::open_service_store(&database).unwrap(),
        ScopedAuthorizer,
        ArtifactStore::open(&artifact_root).unwrap(),
        StaticRunConfigMaterializer::for_provider(ConfigProvider::Anthropic),
    );
    for (key, command) in [
        (
            "failure-project",
            Command::CreateProject {
                schema_version: SchemaVersion::CURRENT,
                project_id,
            },
        ),
        (
            "failure-thread",
            Command::CreateThread {
                schema_version: SchemaVersion::CURRENT,
                thread_id,
                project_id,
            },
        ),
        (
            "failure-run",
            Command::StartRun {
                schema_version: SchemaVersion::CURRENT,
                run_id,
                thread_id,
                input: input.digest().to_string().parse().unwrap(),
                run_config: None,
                experiment_config: None,
                effective_config: None,
            },
        ),
    ] {
        let context = RequestContext::authenticated(
            Ok(principal.clone()),
            Some(IdempotencyKey::parse(key).unwrap()),
            TraceId::parse(key).unwrap(),
        )
        .unwrap();
        service.execute(&context, command).unwrap();
    }
    let mut store = service.into_store();
    let mut job = store
        .claim_queued_run(Duration::from_secs(1))
        .unwrap()
        .unwrap();
    for state in [
        RunState::AcquiringWorkspace,
        RunState::Starting,
        RunState::Running,
    ] {
        job = store.transition_worker_run(run_id, state).unwrap();
    }
    job = store
        .transition_worker_attempt(job.attempt.id, AttemptState::Executing)
        .unwrap();
    SqliteCancellationCoordinator::new(&database)
        .register_no_process(job.attempt.owner)
        .unwrap();
    let mut append = store.worker_append_store().unwrap();
    let attempt_stream = EntityId::Attempt(job.attempt.id);
    let version = append
        .events()
        .unwrap()
        .iter()
        .filter(|event| event.event.stream == attempt_stream)
        .count() as u64;
    let mut boundary = commit(
        job.attempt.owner,
        version,
        LoopRecord::Boundary(BoundarySnapshot {
            boundary: SafeBoundary::BeforeModelDispatch,
            transcript: canonical(&[text(ItemKind::User, "must fail before dispatch")]),
            resume_index: Some(0),
            model_outcome: None,
        }),
    );
    boundary.claim = Some(job.claim);
    append_loop_record(&mut append, boundary).unwrap();
    assert!(
        store
            .fail_worker_run_with_hook(
                run_id,
                job.claim,
                RunFailureProjection {
                    code: RunFailureCode::ProviderUnavailable,
                    detail: "first sanitized detail".to_owned(),
                },
                |point| point == CrashPoint::AfterEventInsert,
            )
            .is_err()
    );
    assert_eq!(
        store
            .worker_append_store()
            .unwrap()
            .events()
            .unwrap()
            .iter()
            .filter(|event| event.event.event_type.as_str() == "run.failure")
            .count(),
        0
    );
    drop(store);
    std::thread::sleep(Duration::from_millis(1_100));

    let mut reopened = test_support::open_service_store(&database).unwrap();
    assert_eq!(reopened.recoverable_runs(10).unwrap().len(), 1);
    let store = Arc::new(Mutex::new(reopened));
    let scheduler = DurableScheduler::open(&database).unwrap();
    let provider = Arc::new(FakeProvider::new(FakeResponse::completed(
        "must not dispatch",
    )));
    let mut config = RunExecutorConfig::new(
        &database,
        artifacts,
        Arc::clone(&store),
        scheduler,
        SelectedModelAdapter::for_test(ConfigProvider::OpenAi, Arc::clone(&provider)),
    );
    config.poll_interval = Duration::from_millis(5);
    config = config.with_project_root(root.join("project"));
    let executor = RunExecutor::start(config).unwrap();
    executor.notify();
    let run = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let QueryProjection::Run(run) = store
                .lock()
                .unwrap()
                .query(&Query::GetRun { run_id })
                .unwrap()
            else {
                panic!("unexpected projection")
            };
            if run.state == RunState::Failed {
                break run;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let projection = store
            .lock()
            .unwrap()
            .query(&Query::GetRun { run_id })
            .unwrap();
        panic!(
            "failure recovery timed out: {projection:?}, health: {:?}",
            executor.health()
        )
    });
    executor.shutdown().await.unwrap();

    assert_eq!(run.state, RunState::Failed);
    assert!(run.failure.is_some());
    assert!(run.output.is_none());
    assert_eq!(provider.dispatch_count(), 0);
    let events = store
        .lock()
        .unwrap()
        .worker_append_store()
        .unwrap()
        .events()
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event.event_type.as_str() == "run.failure")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event.event_type.as_str() == "run.output")
            .count(),
        0
    );
    fs::remove_dir_all(root).unwrap();
}
