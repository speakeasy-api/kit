use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use kit::test_support;
use kit::{
    api::{
        auth::{
            contract::{Authenticator, GrantSnapshot, ScopedAuthorizer},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        service::{
            ArtifactService, CapabilityService, Command, LeaseService, Query, QueryProjection,
            RequestContext, RetentionPeriod, RetentionPolicy, Scheduler, Service, ServiceError,
            SqliteServiceStore,
        },
        stream::{
            CursorKey, EventFilter, OpaqueStreamCursor, PROBLEM_MEDIA_TYPE, PumpOutcome,
            SSE_MEDIA_TYPE, SqliteStreamAdapter, SseFrame, StreamCancellation, StreamConfig,
            TERMINAL_WEBSOCKET_PATH,
        },
    },
    domain::{
        config::{ConfigField, ConfigLayer, Grant, LayerKind, LayerStack, RunConfigSnapshot},
        events::{ArtifactRef, AttemptTransition, RunTransition, SchemaVersion, TraceId},
        ids::{AttemptId, CommandId, EventId, PrincipalId, ProjectId, RunId, TerminalId, ThreadId},
        lifecycle::{AttemptOwnership, AttemptState, FencingToken, RunState},
        secret::{SecretCustody, SecretLease},
    },
    store::sqlite::idempotency::IdempotencyKey,
};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct TestDatabase {
    directory: PathBuf,
    path: PathBuf,
}

#[test]
fn service_persists_effective_config_and_rejects_grant_expansion() {
    let database = TestDatabase::new("effective-config");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let thread = ThreadId::generate().unwrap();
    let run = RunId::generate().unwrap();
    let mut user = ConfigLayer::empty();
    user.budgets.max_tokens = Some(200);
    let mut project_layer = ConfigLayer::empty();
    project_layer.budgets.max_tokens = Some(300);
    let layers = LayerStack {
        built_in: ConfigLayer::safe_defaults(),
        user: Some(user),
        project: Some(project_layer),
        run: None,
        experiment: None,
    };
    let mut service = test_support::service_with_runtime_and_config(
        test_support::open_service_store(&database.path).unwrap(),
        ScopedAuthorizer,
        TestRuntime,
        layers.clone(),
    );
    service
        .execute(
            &context(principal, project, 1),
            Command::CreateProject {
                schema_version: SchemaVersion::CURRENT,
                project_id: project,
            },
        )
        .unwrap();
    service
        .execute(
            &context(principal, project, 2),
            Command::CreateThread {
                schema_version: SchemaVersion::CURRENT,
                thread_id: thread,
                project_id: project,
            },
        )
        .unwrap();

    let mut expansion = ConfigLayer::empty();
    expansion.grants = Some(
        [Grant::ModelCall, Grant::NetworkEgress]
            .into_iter()
            .collect(),
    );
    assert!(matches!(
        service.execute(
            &context(principal, project, 3),
            Command::StartRun {
                schema_version: SchemaVersion::CURRENT,
                run_id: RunId::generate().unwrap(),
                thread_id: thread,
                input: ArtifactRef::parse(&format!("blake3:{}", "b".repeat(64))).unwrap(),
                run_config: Some(Box::new(expansion)),
                experiment_config: None,
                effective_config: None,
            },
        ),
        Err(ServiceError::Invalid(_))
    ));

    let mut run_layer = ConfigLayer::empty();
    run_layer.budgets.max_tokens = Some(400);
    let mut experiment = ConfigLayer::empty();
    experiment.budgets.max_tokens = Some(500);
    service
        .execute(
            &context(principal, project, 4),
            Command::StartRun {
                schema_version: SchemaVersion::CURRENT,
                run_id: run,
                thread_id: thread,
                input: ArtifactRef::parse(&format!("blake3:{}", "c".repeat(64))).unwrap(),
                run_config: Some(Box::new(run_layer)),
                experiment_config: Some(Box::new(experiment)),
                effective_config: None,
            },
        )
        .unwrap();
    let expected = match service
        .query(
            &context(principal, project, 5),
            Query::GetRun { run_id: run },
        )
        .unwrap()
    {
        QueryProjection::Run(run) => run,
        other => panic!("unexpected projection: {other:?}"),
    };
    assert_eq!(
        expected.effective_config.provenance[&ConfigField::MaxTokens],
        LayerKind::Experiment
    );
    drop(service);

    let mut restarted = test_support::service_with_runtime_and_config(
        test_support::open_service_store(&database.path).unwrap(),
        ScopedAuthorizer,
        TestRuntime,
        layers,
    );
    let actual = match restarted
        .query(
            &context(principal, project, 6),
            Query::GetRun { run_id: run },
        )
        .unwrap()
    {
        QueryProjection::Run(run) => run,
        other => panic!("unexpected projection: {other:?}"),
    };
    assert_eq!(actual, expected);
    let event = restarted
        .store()
        .append_store()
        .events()
        .unwrap()
        .into_iter()
        .find(|event| event.event.event_type.as_str() == "run.start")
        .unwrap();
    let stored: kit::domain::projections::PersistedCommand =
        serde_json::from_slice(&event.event.payload).unwrap();
    let Command::StartRun {
        effective_config: Some(bytes),
        ..
    } = stored.command
    else {
        panic!("run start did not persist its config snapshot")
    };
    let snapshot = RunConfigSnapshot::from_canonical_bytes(&bytes).unwrap();
    assert_eq!(snapshot.effective().max_tokens, 500);
    assert_eq!(snapshot.reference(), expected.effective_config);
}

#[test]
fn service_attempt_fencing_rejects_ten_thousand_stale_commits_then_accepts_owner() {
    let mut fixture = Fixture::new("attempt-fencing");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let thread = ThreadId::generate().unwrap();
    let run = RunId::generate().unwrap();
    let attempt = AttemptId::generate().unwrap();
    let owner = AttemptOwnership::new(attempt, principal, FencingToken::new(7));
    fixture.create_project(principal, project);
    fixture.execute(
        principal,
        project,
        Command::CreateThread {
            schema_version: SchemaVersion::CURRENT,
            thread_id: thread,
            project_id: project,
        },
    );
    fixture.execute(
        principal,
        project,
        Command::StartRun {
            schema_version: SchemaVersion::CURRENT,
            run_id: run,
            thread_id: thread,
            input: ArtifactRef::parse(&format!("blake3:{}", "d".repeat(64))).unwrap(),
            run_config: None,
            experiment_config: None,
            effective_config: None,
        },
    );
    fixture.execute(
        principal,
        project,
        Command::StartAttempt {
            schema_version: SchemaVersion::CURRENT,
            attempt_id: attempt,
            run_id: run,
            owner,
            expected_version: 1,
        },
    );
    let stale = AttemptOwnership::new(attempt, principal, FencingToken::new(6));
    let transition = AttemptTransition::new(AttemptState::Leased, AttemptState::Executing).unwrap();
    let mut accepted = 0;
    for _ in 0..10_000 {
        let context = fixture.context(principal, project);
        if fixture
            .service
            .execute(
                &context,
                Command::TransitionAttempt {
                    schema_version: SchemaVersion::CURRENT,
                    attempt_id: attempt,
                    transition,
                    expected_version: 1,
                    expected_owner: stale,
                },
            )
            .is_ok()
        {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 0);
    fixture.execute(
        principal,
        project,
        Command::TransitionAttempt {
            schema_version: SchemaVersion::CURRENT,
            attempt_id: attempt,
            transition,
            expected_version: 1,
            expected_owner: owner,
        },
    );
    assert!(matches!(
        fixture.query(
            principal,
            project,
            Query::GetAttempt {
                attempt_id: attempt
            }
        ),
        QueryProjection::Attempt(attempt) if attempt.state == AttemptState::Executing
    ));

    let mut version = 2;
    for (from, to) in [
        (RunState::Queued, RunState::AcquiringWorkspace),
        (RunState::AcquiringWorkspace, RunState::Starting),
        (RunState::Starting, RunState::Running),
        (RunState::Running, RunState::Interrupted),
    ] {
        fixture.execute(
            principal,
            project,
            Command::TransitionRun {
                schema_version: SchemaVersion::CURRENT,
                run_id: run,
                transition: RunTransition::new(from, to).unwrap(),
                expected_version: version,
                expected_owner: Some(owner),
                replacement_owner: None,
            },
        );
        version += 1;
    }
    let replacement_attempt = AttemptId::generate().unwrap();
    let replacement = AttemptOwnership::new(
        replacement_attempt,
        principal,
        FencingToken::new(owner.fencing_token.get() + 1),
    );
    fixture.execute(
        principal,
        project,
        Command::TransitionRun {
            schema_version: SchemaVersion::CURRENT,
            run_id: run,
            transition: RunTransition::new(RunState::Interrupted, RunState::Queued).unwrap(),
            expected_version: version,
            expected_owner: Some(owner),
            replacement_owner: Some(replacement),
        },
    );
    assert!(matches!(
        fixture.query(
            principal,
            project,
            Query::GetAttempt {
                attempt_id: replacement_attempt
            }
        ),
        QueryProjection::Attempt(attempt)
            if attempt.state == AttemptState::Leased && attempt.owner == replacement
    ));
    assert!(matches!(
        fixture.query(principal, project, Query::GetRun { run_id: run }),
        QueryProjection::Run(run)
            if run.state == RunState::Queued && run.owner == Some(replacement)
    ));
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let directory =
            std::env::temp_dir().join(format!("kit-sse-{name}-{}", EventId::generate().unwrap()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("store.sqlite3");
        Self { directory, path }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

struct Fixture {
    database: TestDatabase,
    service: Service<SqliteServiceStore, ScopedAuthorizer, TestRuntime>,
    serial: usize,
}

#[derive(Clone, Copy)]
struct TestRuntime;

impl Scheduler for TestRuntime {}
impl CapabilityService for TestRuntime {}
impl LeaseService for TestRuntime {}

impl ArtifactService for TestRuntime {
    fn commit_verified<T>(
        &self,
        _principal_id: PrincipalId,
        _project_id: ProjectId,
        _command: &Command,
        commit: impl FnOnce() -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        commit()
    }
}

impl Fixture {
    fn new(name: &str) -> Self {
        let database = TestDatabase::new(name);
        let store = test_support::open_service_store(&database.path).unwrap();
        Self {
            database,
            service: test_support::service_with_runtime(store, ScopedAuthorizer, TestRuntime),
            serial: 0,
        }
    }

    fn with_custody(name: &str, custody: SecretCustody) -> Self {
        let database = TestDatabase::new(name);
        let store =
            test_support::open_project_service_store(&database.path, custody.clone()).unwrap();
        Self {
            database,
            service: test_support::project_service_with_runtime(
                store,
                ScopedAuthorizer,
                TestRuntime,
                custody,
            ),
            serial: 0,
        }
    }

    fn adapter(&self, capacity: usize) -> SqliteStreamAdapter {
        SqliteStreamAdapter::new(
            &self.database.path,
            CursorKey::new([0x5a; 32]),
            StreamConfig {
                buffer_capacity: capacity,
                schema_version: 1,
            },
        )
        .unwrap()
    }

    fn context(&mut self, principal: PrincipalId, project: ProjectId) -> RequestContext {
        self.serial += 1;
        context(principal, project, self.serial)
    }

    fn execute(&mut self, principal: PrincipalId, project: ProjectId, command: Command) {
        let context = self.context(principal, project);
        self.service.execute(&context, command).unwrap();
    }

    fn query(
        &mut self,
        principal: PrincipalId,
        project: ProjectId,
        query: Query,
    ) -> QueryProjection {
        let context = self.context(principal, project);
        self.service.query(&context, query).unwrap()
    }

    fn create_project(&mut self, principal: PrincipalId, project: ProjectId) {
        self.execute(
            principal,
            project,
            Command::CreateProject {
                schema_version: SchemaVersion::CURRENT,
                project_id: project,
            },
        );
    }

    fn create_threads(
        &mut self,
        principal: PrincipalId,
        project: ProjectId,
        count: usize,
    ) -> Vec<ThreadId> {
        (0..count)
            .map(|_| {
                let thread_id = ThreadId::generate().unwrap();
                self.execute(
                    principal,
                    project,
                    Command::CreateThread {
                        schema_version: SchemaVersion::CURRENT,
                        thread_id,
                        project_id: project,
                    },
                );
                thread_id
            })
            .collect()
    }
}

#[test]
fn service_sqlite_and_sse_share_one_projection_across_restart() {
    let mut fixture = Fixture::new("single-projection");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let thread = ThreadId::generate().unwrap();
    let run = RunId::generate().unwrap();
    let attempt = AttemptId::generate().unwrap();
    let owner = AttemptOwnership::new(attempt, principal, FencingToken::new(1));
    let input = ArtifactRef::parse(&format!("blake3:{}", "a".repeat(64))).unwrap();
    fixture.create_project(principal, project);
    fixture.execute(
        principal,
        project,
        Command::SetProjectRetention {
            schema_version: SchemaVersion::CURRENT,
            project_id: project,
            policy: RetentionPolicy {
                event: RetentionPeriod::ForMicros(1),
                transcript: RetentionPeriod::Forever,
                terminal: RetentionPeriod::ForMicros(3),
                artifact: RetentionPeriod::Forever,
                experiment: RetentionPeriod::ForMicros(5),
                backup: RetentionPeriod::Forever,
            },
            expected_version: 1,
        },
    );
    fixture.execute(
        principal,
        project,
        Command::CreateThread {
            schema_version: SchemaVersion::CURRENT,
            thread_id: thread,
            project_id: project,
        },
    );
    fixture.execute(
        principal,
        project,
        Command::StartRun {
            schema_version: SchemaVersion::CURRENT,
            run_id: run,
            thread_id: thread,
            input,
            run_config: None,
            experiment_config: None,
            effective_config: None,
        },
    );
    fixture.execute(
        principal,
        project,
        Command::TransitionRun {
            schema_version: SchemaVersion::CURRENT,
            run_id: run,
            transition: RunTransition::new(RunState::Queued, RunState::AcquiringWorkspace).unwrap(),
            expected_version: 1,
            expected_owner: None,
            replacement_owner: None,
        },
    );
    fixture.execute(
        principal,
        project,
        Command::StartAttempt {
            schema_version: SchemaVersion::CURRENT,
            attempt_id: attempt,
            run_id: run,
            owner,
            expected_version: 2,
        },
    );
    fixture.execute(
        principal,
        project,
        Command::TransitionAttempt {
            schema_version: SchemaVersion::CURRENT,
            attempt_id: attempt,
            transition: AttemptTransition::new(AttemptState::Leased, AttemptState::Executing)
                .unwrap(),
            expected_version: 1,
            expected_owner: owner,
        },
    );
    fixture.execute(
        principal,
        project,
        Command::InitiateThreadDeletion {
            schema_version: SchemaVersion::CURRENT,
            thread_id: thread,
            expected_version: 1,
        },
    );

    let expected_run = fixture.query(principal, project, Query::GetRun { run_id: run });
    let expected_attempt = fixture.query(
        principal,
        project,
        Query::GetAttempt {
            attempt_id: attempt,
        },
    );
    let service_digest = fixture.service.store_mut().projection_digest().unwrap();
    let mut projections = test_support::open_projection_store(&fixture.database.path).unwrap();
    let (incremental, incremental_snapshot) =
        test_support::update_domain_projection(&mut projections).unwrap();
    let (rebuilt, rebuilt_snapshot) =
        test_support::rebuild_domain_projection(&mut projections).unwrap();
    assert_eq!(incremental, rebuilt);
    assert_eq!(incremental_snapshot.digest, rebuilt_snapshot.digest);
    assert_eq!(service_digest, rebuilt_snapshot.digest);
    assert_eq!(
        expected_run,
        QueryProjection::Run(rebuilt.run(run).unwrap().clone())
    );
    assert_eq!(
        expected_attempt,
        QueryProjection::Attempt(rebuilt.attempt(attempt).unwrap().clone())
    );
    assert!(rebuilt.thread(thread).unwrap().deletion_requested);

    let adapter = fixture.adapter(8);
    let read = context(principal, project, 90_000);
    let stream = adapter
        .open(&read, project, EventFilter::all(), None)
        .unwrap();
    assert_eq!(stream.projection_digest(), rebuilt_snapshot.digest);

    fixture.service = test_support::service_with_runtime(
        test_support::open_service_store(&fixture.database.path).unwrap(),
        ScopedAuthorizer,
        TestRuntime,
    );
    assert_eq!(
        fixture.query(principal, project, Query::GetRun { run_id: run }),
        expected_run
    );
    assert_eq!(
        fixture.query(
            principal,
            project,
            Query::GetAttempt {
                attempt_id: attempt,
            },
        ),
        expected_attempt
    );
    assert_eq!(
        fixture.service.store_mut().projection_digest().unwrap(),
        rebuilt_snapshot.digest
    );
}

fn context(principal: PrincipalId, project: ProjectId, serial: usize) -> RequestContext {
    let authenticator = LocalPeerAuthenticator::new(BTreeMap::from([(
        1000,
        GrantSnapshot::new(
            principal,
            project,
            [
                Grant::WorkspaceRead,
                Grant::WorkspaceWrite,
                Grant::ModelCall,
            ],
        ),
    )]));
    let decision = authenticator.authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000));
    RequestContext::authenticated(
        decision,
        Some(IdempotencyKey::parse(&format!("sse-{serial}")).unwrap()),
        TraceId::parse(&format!("sse-trace-{serial}")).unwrap(),
    )
    .unwrap()
}

fn semantic(frame: SseFrame) -> (OpaqueStreamCursor, String) {
    let SseFrame::Semantic {
        id,
        operation,
        data,
    } = frame
    else {
        panic!("expected semantic event")
    };
    assert_eq!(operation, "thread.create");
    let data: Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(data["schema_version"], 1);
    assert_eq!(data["operation"], "thread.create");
    assert_eq!(data["payload"]["command"], "create_thread");
    assert!(data["payload"].get("principal_id").is_none());
    assert!(data["payload"].get("idempotency_key").is_none());
    (id, data["stream"].as_str().unwrap().to_owned())
}

#[test]
fn slow_consumer_disconnects_at_a_durable_cursor_and_reconnect_has_no_gap() {
    let mut fixture = Fixture::new("slow");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    fixture.create_project(principal, project);
    let adapter = fixture.adapter(2);
    let read = context(principal, project, 10_000);
    let mut connection = adapter
        .open(&read, project, EventFilter::all(), None)
        .unwrap();
    let initial = connection.last_durable_cursor();
    let expected = fixture.create_threads(principal, project, 3);

    assert_eq!(connection.pump().unwrap(), PumpOutcome::Ready { queued: 2 });
    assert_eq!(connection.queued_len(), 2);
    assert_eq!(
        connection.pump().unwrap(),
        PumpOutcome::Disconnected {
            cursor: initial.clone()
        }
    );
    assert!(connection.is_disconnected());
    assert!(matches!(
        connection.next_frame(),
        Some(SseFrame::Disconnect {
            cursor,
            reason: "slow_consumer"
        }) if cursor == initial
    ));

    let mut reconnected = adapter
        .open(&read, project, EventFilter::all(), Some(&initial))
        .unwrap();
    reconnected.pump().unwrap();
    let mut actual = Vec::new();
    while actual.len() < expected.len() {
        while let Some(frame) = reconnected.next_frame() {
            actual.push(semantic(frame).1);
        }
        reconnected.pump().unwrap();
    }
    assert_eq!(
        actual,
        expected.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
}

#[test]
fn expired_cursor_is_rfc_9457_with_current_snapshot_and_recovery_cursor() {
    let mut fixture = Fixture::new("expired");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    fixture.create_project(principal, project);
    let adapter = fixture.adapter(4);
    let read = context(principal, project, 20_000);
    let old_cursor = adapter
        .open(&read, project, EventFilter::all(), None)
        .unwrap()
        .last_durable_cursor();
    let forever = RetentionPolicy {
        event: RetentionPeriod::Forever,
        transcript: RetentionPeriod::Forever,
        terminal: RetentionPeriod::Forever,
        artifact: RetentionPeriod::Forever,
        experiment: RetentionPeriod::Forever,
        backup: RetentionPeriod::Forever,
    };
    fixture.execute(
        principal,
        project,
        Command::SetProjectRetention {
            schema_version: SchemaVersion::CURRENT,
            project_id: project,
            policy: forever,
            expected_version: 1,
        },
    );
    adapter.set_first_available_position(project, 3).unwrap();

    let rejection = adapter
        .open(&read, project, EventFilter::all(), Some(&old_cursor))
        .unwrap_err();
    assert_eq!(rejection.status(), 410);
    assert_eq!(rejection.content_type(), PROBLEM_MEDIA_TYPE);
    let body: Value = serde_json::from_slice(rejection.body()).unwrap();
    assert_eq!(body["type"], "/problems/cursor_expired");
    assert_eq!(body["status"], 410);
    assert_eq!(body["code"], "cursor_expired");
    assert_eq!(body["snapshot"]["id"], project.to_string());
    assert_eq!(body["snapshot"]["version"], 2);
    let recovery =
        OpaqueStreamCursor::parse(body["new_cursor"].as_str().unwrap().to_owned()).unwrap();
    let recovered = adapter
        .open(&read, project, EventFilter::all(), Some(&recovery))
        .unwrap();
    let before = recovered.last_durable_cursor();
    assert_eq!(recovered.heartbeat().encode(), b": heartbeat\n\n");
    assert_eq!(recovered.last_durable_cursor(), before);
}

#[test]
fn cursor_is_tamper_evident_and_bound_to_principal_project_filter_and_schema() {
    let mut fixture = Fixture::new("cursor-auth");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    fixture.create_project(principal, project);
    let adapter = fixture.adapter(4);
    let read = context(principal, project, 30_000);
    let cursor = adapter
        .open(&read, project, EventFilter::all(), None)
        .unwrap()
        .last_durable_cursor();
    assert!(!cursor.as_str().contains("0000000000000001"));

    let mut tampered = cursor.to_string().into_bytes();
    let last = tampered.last_mut().unwrap();
    *last = if *last == b'0' { b'1' } else { b'0' };
    let tampered = OpaqueStreamCursor::parse(String::from_utf8(tampered).unwrap()).unwrap();
    let rejection = adapter
        .open(&read, project, EventFilter::all(), Some(&tampered))
        .unwrap_err();
    assert_eq!(rejection.status(), 400);
    assert_eq!(
        serde_json::from_slice::<Value>(rejection.body()).unwrap()["code"],
        "invalid_cursor"
    );

    let filtered = EventFilter::operations(["thread.create"]).unwrap();
    assert_eq!(
        adapter
            .open(&read, project, filtered, Some(&cursor))
            .unwrap_err()
            .status(),
        400
    );
    let thread_cursor = adapter
        .open(
            &read,
            project,
            EventFilter::thread(ThreadId::generate().unwrap()),
            None,
        )
        .unwrap()
        .last_durable_cursor();
    assert_eq!(
        adapter
            .open(&read, project, EventFilter::project(), Some(&thread_cursor))
            .unwrap_err()
            .status(),
        400
    );
    assert_eq!(
        adapter
            .open(
                &read,
                project,
                EventFilter::run(RunId::generate().unwrap()),
                Some(&thread_cursor),
            )
            .unwrap_err()
            .status(),
        400
    );
    let other_schema = SqliteStreamAdapter::new(
        &fixture.database.path,
        CursorKey::new([0x5a; 32]),
        StreamConfig {
            buffer_capacity: 4,
            schema_version: 2,
        },
    )
    .unwrap();
    assert_eq!(
        other_schema
            .open(&read, project, EventFilter::all(), Some(&cursor))
            .unwrap_err()
            .status(),
        400
    );
}

#[test]
fn active_custody_rejects_forged_legacy_cursor_and_fresh_start_emits_kitc2() {
    let mut fixture = Fixture::new("legacy-upgrade");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    fixture.create_project(principal, project);
    let custody = SecretCustody::new([Arc::new(SecretLease::new("active-secret"))]);
    let adapter = fixture.adapter(4).with_custody(custody);
    let read = context(principal, project, 31_000);
    let legacy = OpaqueStreamCursor::parse(format!("kitc1_{}", "0".repeat(48))).unwrap();

    let rejection = adapter
        .open(&read, project, EventFilter::all(), Some(&legacy))
        .unwrap_err();
    assert_eq!(rejection.status(), 400);
    assert_eq!(
        serde_json::from_slice::<Value>(rejection.body()).unwrap()["code"],
        "invalid_cursor"
    );

    let fresh = adapter
        .open(&read, project, EventFilter::all(), None)
        .unwrap()
        .last_durable_cursor();
    assert!(fresh.as_str().starts_with("kitc2_"));
}

#[tokio::test]
async fn event_pages_map_stale_custody_to_409_without_a_cursor_oracle() {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use kit::api::http::{
        core::HttpAuthenticator,
        router::{RouterConfig, authenticated_router_with_stream},
    };
    use tower::ServiceExt;

    let custody = SecretCustody::new([Arc::new(SecretLease::new("unrelated-secret"))]);
    let mut fixture = Fixture::with_custody("page-custody-upgrade", custody.clone());
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    fixture.create_project(principal, project);
    let thread = fixture.create_threads(principal, project, 1)[0];
    assert!(matches!(
        fixture.query(principal, project, Query::GetThread { thread_id: thread }),
        QueryProjection::Thread(_)
    ));
    fixture.execute(
        principal,
        project,
        Command::SetThreadArchived {
            schema_version: SchemaVersion::CURRENT,
            thread_id: thread,
            archived: true,
            expected_version: 1,
        },
    );
    let adapter = fixture.adapter(4).with_custody(custody.clone());
    let authenticator = LocalPeerAuthenticator::new(BTreeMap::from([(
        1000,
        GrantSnapshot::new(
            principal,
            project,
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
        ),
    )]));
    let authenticator: Arc<dyn HttpAuthenticator> =
        Arc::new(move |_: &axum::http::request::Parts| {
            authenticator.authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000))
        });
    let app = authenticated_router_with_stream(
        Arc::new(Mutex::new(fixture.service)),
        authenticator,
        RouterConfig::default(),
        adapter,
    );
    let page_path = format!("/v1/threads/{thread}/events");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{page_path}?limit=1"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let page: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    let cursor = page["next_cursor"].as_str().unwrap().to_owned();
    assert_eq!(page["truncated"], true);
    assert!(cursor.starts_with("kitc2_"));
    let item_cursor = page["items"][0]["cursor"].as_str().unwrap().to_owned();
    assert!(item_cursor.starts_with("kitc2_"));
    assert_eq!(item_cursor, cursor);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{page_path}?limit=1&cursor={item_cursor}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let resumed: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(resumed["items"].as_array().unwrap().len(), 1);
    assert_eq!(resumed["truncated"], false);
    assert_ne!(resumed["items"][0]["cursor"], item_cursor);
    assert!(
        resumed["items"][0]["cursor"]
            .as_str()
            .unwrap()
            .starts_with("kitc2_")
    );
    assert!(
        resumed["next_cursor"]
            .as_str()
            .unwrap()
            .starts_with("kitc2_")
    );

    custody.register(
        "page-test",
        "new-secret",
        Arc::new(SecretLease::new("new-secret")),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{page_path}?cursor={cursor}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let stale: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap();
    assert_eq!(stale["code"], "cursor_upgrade_required");

    let mut tampered = cursor;
    let replacement = if tampered.ends_with('0') { '1' } else { '0' };
    tampered.pop();
    tampered.push(replacement);
    for invalid in [tampered, "kitc2_deadbeef".to_owned()] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("{page_path}?cursor={invalid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(body["code"], "invalid_request");
        assert_eq!(body["invalid_parameters"][0]["name"], "cursor");
    }
}

#[tokio::test]
async fn event_pages_bound_max_state_and_wire_and_resume_one_thousand_without_gaps() {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use base64::Engine;
    use kit::api::http::{
        core::HttpAuthenticator,
        router::{RouterConfig, authenticated_router_with_stream},
    };
    use std::collections::BTreeSet;
    use tower::ServiceExt;

    const PAGE_BYTES: usize = 8 * 1024 * 1024;
    let custody = SecretCustody::new([Arc::new(SecretLease::new("active-secret"))]);
    let mut fixture = Fixture::with_custody("page-bounds", custody.clone());
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    fixture.create_project(principal, project);
    let thread = fixture.create_threads(principal, project, 1)[0];
    let mut connection = rusqlite::Connection::open(&fixture.database.path).unwrap();
    let transaction = connection.transaction().unwrap();
    let first_position: u64 = transaction
        .query_row(
            "SELECT position FROM commit_watermark WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for index in 0_u64..999 {
        let position = first_position + index + 1;
        let payload = serde_json::to_vec(&kit::domain::projections::PersistedCommand {
            principal_id: principal,
            stored_at_unix_micros: 0,
            idempotency_key: if index < 15 {
                "i".repeat(64 * 1024)
            } else {
                format!("page-{index}")
            },
            apply_projection: true,
            command: Command::SetThreadArchived {
                schema_version: SchemaVersion::CURRENT,
                thread_id: thread,
                archived: index.is_multiple_of(2),
                expected_version: index + 1,
            },
        })
        .unwrap();
        let event_id = EventId::generate().unwrap();
        let command_id = CommandId::generate().unwrap();
        transaction
            .execute(
                "INSERT INTO events
                 (event_id, stream, sequence, commit_position, event_type, schema_version,
                  occurred_at, causation_id, correlation_id, attempt_id, trace_id, payload,
                  artifacts)
                 VALUES (?1, ?2, ?3, ?4, 'thread.archive', 1,
                         '2026-08-08T00:00:00.000000Z', ?5, ?2, NULL, 'trace-test', ?6, ?7)",
                rusqlite::params![
                    event_id.to_string(),
                    thread.to_string(),
                    index + 2,
                    position,
                    command_id.to_string(),
                    payload,
                    b"[]".as_slice(),
                ],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO event_projection_index
                 (commit_position, project_id, thread_id, run_id, event_class,
                  stored_at_unix_micros, erased)
                 VALUES (?1, ?2, ?3, NULL, 'event', 0, 0)",
                rusqlite::params![position, project.to_string(), thread.to_string()],
            )
            .unwrap();
    }
    let last_position = first_position + 999;
    transaction
        .execute(
            "UPDATE stream_heads SET version = 1000 WHERE stream = ?1",
            [thread.to_string()],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE commit_watermark SET position = ?1 WHERE singleton = 1",
            [last_position],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE event_projection_index_state SET indexed_through = ?1 WHERE singleton = 1",
            [last_position],
        )
        .unwrap();
    transaction.commit().unwrap();

    let adapter = fixture.adapter(4).with_custody(custody);
    let authenticator = LocalPeerAuthenticator::new(BTreeMap::from([(
        1000,
        GrantSnapshot::new(
            principal,
            project,
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
        ),
    )]));
    let authenticator: Arc<dyn HttpAuthenticator> =
        Arc::new(move |_: &axum::http::request::Parts| {
            authenticator.authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000))
        });
    let app = authenticated_router_with_stream(
        Arc::new(Mutex::new(fixture.service)),
        authenticator,
        RouterConfig::default(),
        adapter,
    );
    let page_path = format!("/v1/threads/{thread}/events?limit=1000");
    let mut cursor = None;
    let mut digests = BTreeSet::new();
    let mut pages = 0;
    loop {
        let uri = cursor.as_ref().map_or_else(
            || page_path.clone(),
            |cursor| format!("{page_path}&cursor={cursor}"),
        );
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), PAGE_BYTES).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert!(body.len() <= PAGE_BYTES);
        let page: Value = serde_json::from_slice(&body).unwrap();
        let items = page["items"].as_array().unwrap();
        assert!(!items.is_empty());
        assert!(items.len() <= 16);
        let state_bytes = items
            .iter()
            .map(|item| {
                let encoded = item["cursor"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("kitc2_")
                    .unwrap();
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(encoded)
                    .unwrap()
                    .len()
                    - 52
            })
            .sum::<usize>();
        assert!(state_bytes <= PAGE_BYTES);
        for item in items {
            assert!(digests.insert(item["authority_digest"].as_str().unwrap().to_owned()));
        }
        pages += 1;
        cursor = Some(page["next_cursor"].as_str().unwrap().to_owned());
        if page["truncated"] == false {
            break;
        }
    }
    assert!(pages > 1);
    assert_eq!(digests.len(), 1000);
}

#[test]
fn custody_mutation_disconnects_without_emitting_queued_raw_frames() {
    let mut fixture = Fixture::new("custody-mutation");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    fixture.create_project(principal, project);
    let custody = SecretCustody::default();
    let adapter = fixture.adapter(4).with_custody(custody.clone());
    let read = context(principal, project, 32_000);
    let mut stream = adapter
        .open(&read, project, EventFilter::all(), None)
        .unwrap();
    let authoritative = stream.last_durable_cursor();
    let thread = fixture.create_threads(principal, project, 1)[0];
    assert_eq!(stream.pump().unwrap(), PumpOutcome::Ready { queued: 1 });

    custody.register(
        "stream-test",
        "thread-id",
        Arc::new(SecretLease::new(thread.to_string())),
    );

    assert!(matches!(
        stream.next_frame(),
        Some(SseFrame::Disconnect {
            cursor,
            reason: "cursor_upgrade_required"
        }) if cursor == authoritative
    ));
    assert_eq!(stream.last_durable_cursor(), authoritative);
    assert!(stream.is_disconnected());
    assert_eq!(stream.next_frame(), None);
    let rejection = adapter
        .open(&read, project, EventFilter::all(), Some(&authoritative))
        .unwrap_err();
    assert_eq!(rejection.status(), 409);
    assert_eq!(
        serde_json::from_slice::<Value>(rejection.body()).unwrap()["code"],
        "cursor_upgrade_required"
    );
    adapter
        .open(&read, project, EventFilter::all(), None)
        .unwrap();
}

#[test]
fn project_thread_and_run_filters_exclude_unrelated_entities_in_one_project() {
    let mut fixture = Fixture::new("entity-filters");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    fixture.create_project(principal, project);
    let threads = fixture.create_threads(principal, project, 2);
    let runs = [RunId::generate().unwrap(), RunId::generate().unwrap()];
    for (thread_id, run_id) in threads.iter().zip(runs) {
        fixture.execute(
            principal,
            project,
            Command::StartRun {
                schema_version: SchemaVersion::CURRENT,
                run_id,
                thread_id: *thread_id,
                input: ArtifactRef::parse(&format!("blake3:{}", "e".repeat(64))).unwrap(),
                run_config: None,
                experiment_config: None,
                effective_config: None,
            },
        );
    }
    let adapter = fixture.adapter(8);
    let read = context(principal, project, 34_000);
    let mut project_stream = adapter
        .open(&read, project, EventFilter::project(), None)
        .unwrap();
    let mut thread_stream = adapter
        .open(&read, project, EventFilter::thread(threads[0]), None)
        .unwrap();
    let mut run_stream = adapter
        .open(&read, project, EventFilter::run(runs[0]), None)
        .unwrap();

    fixture.execute(
        principal,
        project,
        Command::SetThreadArchived {
            schema_version: SchemaVersion::CURRENT,
            thread_id: threads[1],
            archived: true,
            expected_version: 1,
        },
    );
    fixture.execute(
        principal,
        project,
        Command::TransitionRun {
            schema_version: SchemaVersion::CURRENT,
            run_id: runs[1],
            transition: RunTransition::new(RunState::Queued, RunState::AcquiringWorkspace).unwrap(),
            expected_version: 1,
            expected_owner: None,
            replacement_owner: None,
        },
    );
    fixture.execute(
        principal,
        project,
        Command::SetThreadArchived {
            schema_version: SchemaVersion::CURRENT,
            thread_id: threads[0],
            archived: true,
            expected_version: 1,
        },
    );
    fixture.execute(
        principal,
        project,
        Command::TransitionRun {
            schema_version: SchemaVersion::CURRENT,
            run_id: runs[0],
            transition: RunTransition::new(RunState::Queued, RunState::AcquiringWorkspace).unwrap(),
            expected_version: 1,
            expected_owner: None,
            replacement_owner: None,
        },
    );

    assert_eq!(stream_entities(&mut project_stream).len(), 4);
    assert_eq!(
        stream_entities(&mut thread_stream),
        vec![threads[0].to_string(), runs[0].to_string()]
    );
    assert_eq!(stream_entities(&mut run_stream), vec![runs[0].to_string()]);
}

fn stream_entities(connection: &mut kit::api::stream::SseConnection) -> Vec<String> {
    connection.pump().unwrap();
    let mut streams = Vec::new();
    while let Some(frame) = connection.next_frame() {
        let SseFrame::Semantic { data, .. } = frame else {
            continue;
        };
        streams.push(
            serde_json::from_slice::<Value>(&data).unwrap()["stream"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    streams
}

#[test]
fn stream_never_reads_past_the_sqlite_committed_watermark() {
    let mut fixture = Fixture::new("committed-only");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    fixture.create_project(principal, project);
    let connection = rusqlite::Connection::open(&fixture.database.path).unwrap();
    connection
        .execute(
            "INSERT INTO events (
                 event_id, stream, sequence, commit_position, event_type, schema_version,
                 occurred_at, causation_id, correlation_id, attempt_id, trace_id,
                 payload, artifacts
             ) VALUES (?1, ?2, 2, 2, 'thread.create', 1, 'not-visible', 'not-visible',
                       'not-visible', NULL, 'not-visible', X'00', X'5B5D')",
            [
                EventId::generate().unwrap().to_string(),
                project.to_string(),
            ],
        )
        .unwrap();
    drop(connection);

    let adapter = fixture.adapter(4);
    let read = context(principal, project, 35_000);
    let mut stream = adapter
        .open(&read, project, EventFilter::all(), None)
        .unwrap();
    let cursor = stream.last_durable_cursor();
    assert_eq!(stream.pump().unwrap(), PumpOutcome::Ready { queued: 0 });
    assert_eq!(stream.next_frame(), None);
    assert_eq!(stream.last_durable_cursor(), cursor);
}

#[test]
fn malformed_semantic_row_disconnects_once_at_its_cursor() {
    let mut fixture = Fixture::new("malformed-row");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    fixture.create_project(principal, project);
    let adapter = fixture.adapter(4);
    let read = context(principal, project, 36_000);
    let mut stream = adapter
        .open(&read, project, EventFilter::all(), None)
        .unwrap();
    fixture.create_threads(principal, project, 1);
    adapter
        .open(&read, project, EventFilter::all(), None)
        .unwrap();
    let database = rusqlite::Connection::open(&fixture.database.path).unwrap();
    database
        .execute(
            "UPDATE events SET payload = X'00' WHERE commit_position = 2",
            [],
        )
        .unwrap();

    let PumpOutcome::Disconnected { cursor } = stream.pump().unwrap() else {
        panic!("malformed row did not disconnect the stream")
    };
    assert!(matches!(
        stream.next_frame(),
        Some(SseFrame::Disconnect {
            cursor: frame_cursor,
            reason: "stream_error"
        }) if frame_cursor == cursor
    ));
    assert_eq!(stream.last_durable_cursor(), cursor);

    let mut resumed = adapter
        .open(&read, project, EventFilter::all(), Some(&cursor))
        .unwrap();
    assert_eq!(resumed.pump().unwrap(), PumpOutcome::Ready { queued: 0 });
    assert_eq!(resumed.next_frame(), None);
}

#[test]
fn one_hundred_reconnect_schedules_deliver_every_semantic_event_once_in_order() {
    let mut fixture = Fixture::new("reconnect");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    fixture.create_project(principal, project);
    let adapter = fixture.adapter(4);
    let read = context(principal, project, 40_000);
    let initial = adapter
        .open(&read, project, EventFilter::all(), None)
        .unwrap()
        .last_durable_cursor();
    let expected = fixture
        .create_threads(principal, project, 17)
        .into_iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>();

    for schedule in 0..100 {
        let mut cursor = initial.clone();
        let mut actual = Vec::new();
        let mut round = 0;
        while actual.len() < expected.len() {
            let mut connection = adapter
                .open(&read, project, EventFilter::all(), Some(&cursor))
                .unwrap();
            connection.pump().unwrap();
            let take = (schedule + round) % 4 + 1;
            for _ in 0..take {
                let Some(frame) = connection.next_frame() else {
                    break;
                };
                let (event_cursor, stream) = semantic(frame);
                cursor = event_cursor;
                actual.push(stream);
            }
            assert_eq!(cursor, connection.last_durable_cursor());
            round += 1;
        }
        assert_eq!(actual, expected, "reconnect schedule {schedule}");
    }
}

#[test]
fn cross_principal_and_nonexistent_streams_are_byte_identical() {
    let mut fixture = Fixture::new("existence");
    let owner = PrincipalId::generate().unwrap();
    let owner_project = ProjectId::generate().unwrap();
    let caller = PrincipalId::generate().unwrap();
    let caller_project = ProjectId::generate().unwrap();
    fixture.create_project(owner, owner_project);
    fixture.create_project(caller, caller_project);
    let adapter = fixture.adapter(4);
    let read = context(caller, caller_project, 50_000);

    let cross = adapter
        .open(&read, owner_project, EventFilter::all(), None)
        .unwrap_err();
    let missing = adapter
        .open(
            &read,
            ProjectId::generate().unwrap(),
            EventFilter::all(),
            None,
        )
        .unwrap_err();
    assert_eq!(cross.status(), 404);
    assert_eq!(cross, missing);
    assert_eq!(cross.body(), missing.body());
}

#[test]
fn terminal_websocket_path_is_authenticated_reserved_and_never_sse() {
    let mut fixture = Fixture::new("terminal");
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    fixture.create_project(principal, project);
    let adapter = fixture.adapter(4);
    let read = context(principal, project, 60_000);
    let rejection = adapter
        .reserve_terminal_websocket(&read, project, TerminalId::generate().unwrap())
        .unwrap_err();
    assert_eq!(
        TERMINAL_WEBSOCKET_PATH,
        "/v1/terminals/{terminal_id}/attach"
    );
    assert_eq!(rejection.status(), 503);
    assert_eq!(rejection.content_type(), PROBLEM_MEDIA_TYPE);
    assert_ne!(rejection.content_type(), SSE_MEDIA_TYPE);
    assert_eq!(
        serde_json::from_slice::<Value>(rejection.body()).unwrap()["code"],
        "terminal_unavailable"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tcp_stream_tails_post_connect_commits_heartbeats_and_denies_cross_principal() {
    let database = TestDatabase::new("tcp");
    let service = Arc::new(Mutex::new(test_support::service(
        test_support::open_service_store(&database.path).unwrap(),
        ScopedAuthorizer,
    )));
    let owner = PrincipalId::generate().unwrap();
    let owner_project = ProjectId::generate().unwrap();
    let caller = PrincipalId::generate().unwrap();
    let caller_project = ProjectId::generate().unwrap();
    service
        .lock()
        .unwrap()
        .execute(
            &context(owner, owner_project, 70_000),
            Command::CreateProject {
                schema_version: SchemaVersion::CURRENT,
                project_id: owner_project,
            },
        )
        .unwrap();
    service
        .lock()
        .unwrap()
        .execute(
            &context(caller, caller_project, 70_001),
            Command::CreateProject {
                schema_version: SchemaVersion::CURRENT,
                project_id: caller_project,
            },
        )
        .unwrap();

    let adapter = SqliteStreamAdapter::new(
        &database.path,
        CursorKey::new([0x71; 32]),
        StreamConfig {
            buffer_capacity: 2,
            schema_version: 1,
        },
    )
    .unwrap();
    let service_handler: Arc<dyn kit::api::http::core::ServiceHandler> = service.clone();
    let (address, server) =
        tcp_server(service_handler, adapter.clone(), owner, owner_project).await;
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!(
                "GET /v1/projects/{owner_project}/events/stream HTTP/1.1\r\nHost: {address}\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut received = Vec::new();
    read_tcp_until(
        &mut stream,
        &mut received,
        b": heartbeat",
        Duration::from_secs(10),
    )
    .await;
    assert!(received.starts_with(b"HTTP/1.1 200"));

    let thread_id = ThreadId::generate().unwrap();
    service
        .lock()
        .unwrap()
        .execute(
            &context(owner, owner_project, 70_002),
            Command::CreateThread {
                schema_version: SchemaVersion::CURRENT,
                thread_id,
                project_id: owner_project,
            },
        )
        .unwrap();
    read_tcp_until(
        &mut stream,
        &mut received,
        thread_id.to_string().as_bytes(),
        Duration::from_secs(10),
    )
    .await;
    assert!(contains(&received, b"event: thread.create"));
    read_tcp_until(
        &mut stream,
        &mut received,
        b": heartbeat",
        Duration::from_secs(10),
    )
    .await;
    drop(stream);
    server.abort();

    let (address, server) = tcp_server(service, adapter, caller, caller_project).await;
    let mut denied = tokio::net::TcpStream::connect(address).await.unwrap();
    denied
        .write_all(
            format!(
                "GET /v1/projects/{owner_project}/events/stream HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = Vec::new();
    read_tcp_until(
        &mut denied,
        &mut response,
        b"\r\n\r\n",
        Duration::from_secs(10),
    )
    .await;
    assert!(response.starts_with(b"HTTP/1.1 404"));
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_shutdown_closes_and_joins_active_sse_producers() {
    use kit::api::http::{
        core::HttpAuthenticator,
        router::{RouterConfig, authenticated_router_with_stream_cancellation},
    };

    let database = TestDatabase::new("shutdown");
    let service = Arc::new(Mutex::new(test_support::service(
        test_support::open_service_store(&database.path).unwrap(),
        ScopedAuthorizer,
    )));
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    service
        .lock()
        .unwrap()
        .execute(
            &context(principal, project, 80_000),
            Command::CreateProject {
                schema_version: SchemaVersion::CURRENT,
                project_id: project,
            },
        )
        .unwrap();
    let authenticator = LocalPeerAuthenticator::new(BTreeMap::from([(
        1000,
        GrantSnapshot::new(
            principal,
            project,
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
        ),
    )]));
    let authenticator: Arc<dyn HttpAuthenticator> =
        Arc::new(move |_: &axum::http::request::Parts| {
            authenticator.authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000))
        });
    let cancellation = StreamCancellation::new();
    let app = authenticated_router_with_stream_cancellation(
        service,
        authenticator,
        RouterConfig::default(),
        SqliteStreamAdapter::new(
            &database.path,
            CursorKey::new([0x81; 32]),
            StreamConfig::default(),
        )
        .unwrap(),
        cancellation.clone(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(
            format!("GET /v1/projects/{project}/events/stream HTTP/1.1\r\nHost: {address}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut received = Vec::new();
    read_tcp_until(
        &mut stream,
        &mut received,
        b"\r\n\r\n",
        Duration::from_secs(2),
    )
    .await;
    assert_eq!(cancellation.active_producers(), 1);

    cancellation.cancel();
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut received))
        .await
        .expect("SSE body did not close during shutdown")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("graceful shutdown waited on an SSE producer")
        .unwrap();
    assert_eq!(cancellation.active_producers(), 0);
}

async fn tcp_server(
    service: Arc<dyn kit::api::http::core::ServiceHandler>,
    stream: SqliteStreamAdapter,
    principal: PrincipalId,
    project: ProjectId,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use kit::api::http::{
        core::HttpAuthenticator,
        router::{RouterConfig, authenticated_router_with_stream},
    };

    let authenticator = LocalPeerAuthenticator::new(BTreeMap::from([(
        1000,
        GrantSnapshot::new(
            principal,
            project,
            [Grant::WorkspaceRead, Grant::WorkspaceWrite],
        ),
    )]));
    let authenticator: Arc<dyn HttpAuthenticator> =
        Arc::new(move |_: &axum::http::request::Parts| {
            authenticator.authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000))
        });
    let app = authenticated_router_with_stream(
        service,
        authenticator,
        RouterConfig {
            json_body_limit: 64 * 1024,
            request_timeout: Duration::from_secs(5),
        },
        stream,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (address, server)
}

async fn read_tcp_until(
    stream: &mut tokio::net::TcpStream,
    received: &mut Vec<u8>,
    expected: &[u8],
    timeout: Duration,
) {
    tokio::time::timeout(timeout, async {
        let mut chunk = [0_u8; 4096];
        while !contains(received, expected) {
            let count = stream.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0, "TCP stream closed before expected bytes arrived");
            received.extend_from_slice(&chunk[..count]);
        }
    })
    .await
    .expect("TCP stream read timed out");
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
