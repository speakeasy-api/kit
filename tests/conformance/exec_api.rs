use std::convert::Infallible;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::{
    fs, io,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Extension,
    body::{Body, Bytes, to_bytes},
    http::{Method, Request, StatusCode, header},
};
use kit::{
    api::{
        auth::{
            contract::{AuthenticatedPrincipal, Authenticator, GrantSnapshot},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        http::{
            core::{JSON_BODY_LIMIT, ServiceHandler},
            errors::PROBLEM_MEDIA_TYPE,
            exec::{
                AllocateTerminalBody, EXEC_ROUTES, EXECUTOR_IDEMPOTENCY_RETRY_WINDOW_MILLIS,
                ExecError, ExecService, ManagerExecService, ProcessRegistration,
                ProcessResourceState, TerminalResizeBody, WriterLeaseBody, routes,
            },
            router::{RouterConfig, authenticated_router_with_exec},
        },
        service::{Command, CommandReceipt, Query, QueryProjection, RequestContext, ServiceError},
    },
    cli::{
        core::{Invocation, parse},
        exec::{EXEC_CLI_OPERATIONS, ExecRequest, parity_table},
    },
    domain::{
        config::Grant,
        ids::{AttemptId, PrincipalId, ProcessId, ProjectId, TerminalId},
        lifecycle::{AttemptOwnership, FencingToken, ProcessClaim, ProcessOwnership},
    },
    executor::{
        cancel::{CancellationError, ExecutorCancellationCoordinator, ExecutorCancellationOutcome},
        process::{
            own::{ProcessRegistrationContext, ProcessRegistry, ProcessTerminalConfig},
            tree::{BoundaryIdentity, BoundaryKind, Ownership, PersistedBoundary},
        },
        terminal::{
            FakePtyDriver, NativePtyUnavailable, OutputRetention, SqliteTerminalSnapshotStore,
            TerminalManager, TerminalRequest, TerminalSize, TerminalSnapshot,
        },
    },
    store::sqlite::idempotency::IdempotencyKey,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const PROJECT: &str = "project_00000000000000000000000001";
const OTHER_PROJECT: &str = "project_00000000000000000000000002";
const PROCESS: &str = "process_00000000000000000000000001";
const MISSING_PROCESS: &str = "process_00000000000000000000000002";
const TERMINAL: &str = "terminal_00000000000000000000000001";
const ATTACHMENT: &str = "attachment_00000000000000000000000000000000";

#[derive(Clone)]
struct FakeCancellation(Arc<AtomicUsize>);

impl ExecutorCancellationCoordinator for FakeCancellation {
    fn cancel_attempt(
        &self,
        _authority: AttemptOwnership,
    ) -> Result<ExecutorCancellationOutcome, CancellationError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(ExecutorCancellationOutcome::Quiescent)
    }
}

#[derive(Clone)]
struct UnknownThenQuiescent {
    calls: Arc<AtomicUsize>,
    unknown_calls: usize,
}

impl ExecutorCancellationCoordinator for UnknownThenQuiescent {
    fn cancel_attempt(
        &self,
        _authority: AttemptOwnership,
    ) -> Result<ExecutorCancellationOutcome, CancellationError> {
        let call = self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(if call < self.unknown_calls {
            ExecutorCancellationOutcome::OutcomeUnknown
        } else {
            ExecutorCancellationOutcome::Quiescent
        })
    }
}

fn principal(value: &str, project: ProjectId) -> AuthenticatedPrincipal {
    principal_with_grants(
        value,
        project,
        [
            Grant::WorkspaceRead,
            Grant::WorkspaceWrite,
            Grant::ProcessSpawn,
        ],
    )
}

fn principal_with_grants(
    value: &str,
    project: ProjectId,
    grants: impl IntoIterator<Item = Grant>,
) -> AuthenticatedPrincipal {
    let principal = PrincipalId::parse(value).unwrap();
    LocalPeerAuthenticator::new(std::collections::BTreeMap::from([(
        1000,
        GrantSnapshot::new(principal, project, grants),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000))
    .unwrap()
}

fn fixture_with_terminal_request(
    terminal_request: TerminalRequest,
) -> (
    Arc<
        ManagerExecService<
            NativePtyUnavailable,
            impl kit::executor::terminal::TerminalSnapshotStore,
            FakeCancellation,
        >,
    >,
    Arc<AtomicUsize>,
) {
    let project_id = ProjectId::parse(PROJECT).unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
        PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
        FencingToken::new(1),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let manager = TerminalManager::new(
        project_id,
        NativePtyUnavailable,
        |_snapshot: &TerminalSnapshot| Ok(()),
    );
    let service = Arc::new(ManagerExecService::new(
        manager,
        FakeCancellation(calls.clone()),
    ));
    service
        .register_process(ProcessRegistration {
            project_id,
            claim: ProcessClaim::new(
                ProcessId::parse(PROCESS).unwrap(),
                ProcessOwnership::Attempt(owner),
            ),
            execution_id: Some(42),
            state: ProcessResourceState::Started,
            terminal_request,
            boundary_id: "fixture-boundary".to_owned(),
        })
        .unwrap();
    (service, calls)
}

fn fixture() -> (
    Arc<
        ManagerExecService<
            NativePtyUnavailable,
            impl kit::executor::terminal::TerminalSnapshotStore,
            FakeCancellation,
        >,
    >,
    Arc<AtomicUsize>,
) {
    fixture_with_terminal_request(TerminalRequest::default())
}

fn discard_snapshot(_: &TerminalSnapshot) -> io::Result<()> {
    Ok(())
}

fn prepare_pty(
    registry: &dyn ProcessRegistry,
    project_id: ProjectId,
    principal_id: PrincipalId,
    claim: ProcessClaim,
    body: &AllocateTerminalBody,
) {
    let boundary = PersistedBoundary {
        ownership: Ownership::new("attempt-owner", claim.process_id.to_string()).unwrap(),
        identity: BoundaryIdentity::new(
            BoundaryKind::Container,
            format!("pty-{}", claim.process_id),
            "ownership-token",
            "runtime-token",
        )
        .unwrap(),
    };
    ProcessRegistry::prepared(
        registry,
        ProcessRegistrationContext {
            project_id,
            principal_id,
        },
        claim,
        &boundary,
        ProcessTerminalConfig {
            request: TerminalRequest::pty(
                kit::telemetry::redact::CapturePersistencePolicy::no_secrets(),
            ),
            size: TerminalSize::new(body.columns, body.rows).unwrap(),
            retention: OutputRetention::new(body.max_output_bytes, body.max_output_age_millis),
        },
    )
    .unwrap();
}

fn request(method: Method, uri: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", "exec-api-key")
        .body(body.into())
        .unwrap()
}

struct UnusedService;

impl ServiceHandler for UnusedService {
    fn execute(
        &self,
        _context: &RequestContext,
        _command: Command,
    ) -> Result<CommandReceipt, ServiceError> {
        unreachable!("executor transport test dispatched a core command")
    }

    fn query(
        &self,
        _context: &RequestContext,
        _query: Query,
    ) -> Result<QueryProjection, ServiceError> {
        unreachable!("executor transport test dispatched a core query")
    }
}

async fn response_bytes(
    service: Arc<dyn kit::api::http::exec::ExecService>,
    authenticated: AuthenticatedPrincipal,
    request: Request<Body>,
    status: StatusCode,
) -> Vec<u8> {
    let response = routes(service)
        .layer(Extension(authenticated))
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), status);
    if status.is_client_error() || status.is_server_error() {
        assert_eq!(response.headers()[header::CONTENT_TYPE], PROBLEM_MEDIA_TYPE);
    }
    to_bytes(response.into_body(), 128 * 1024)
        .await
        .unwrap()
        .to_vec()
}

#[tokio::test]
async fn cross_principal_and_cross_project_are_byte_identical_to_missing() {
    let (service, _) = fixture();
    let project = ProjectId::parse(PROJECT).unwrap();
    let owner = principal("principal_00000000000000000000000001", project);
    let other_principal = principal("principal_00000000000000000000000002", project);
    let other_project = principal(
        "principal_00000000000000000000000001",
        ProjectId::parse(OTHER_PROJECT).unwrap(),
    );

    let missing = response_bytes(
        service.clone(),
        owner,
        request(Method::GET, &format!("/v1/processes/{MISSING_PROCESS}"), ""),
        StatusCode::NOT_FOUND,
    )
    .await;
    for authenticated in [other_principal, other_project] {
        let hidden = response_bytes(
            service.clone(),
            authenticated.clone(),
            request(Method::GET, &format!("/v1/processes/{PROCESS}"), ""),
            StatusCode::NOT_FOUND,
        )
        .await;
        assert_eq!(hidden, missing);
    }

    let body = String::from_utf8(missing).unwrap();
    assert!(!body.contains(PROCESS));
    assert!(!body.contains(MISSING_PROCESS));
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["instance"],
        "/v1/executor"
    );
}

#[tokio::test]
async fn unavailable_pty_driver_returns_documented_501_without_existence_leak() {
    let (service, _) = fixture_with_terminal_request(TerminalRequest::pty(
        kit::telemetry::redact::CapturePersistencePolicy::no_secrets(),
    ));
    let project = ProjectId::parse(PROJECT).unwrap();
    let owner = principal("principal_00000000000000000000000001", project);
    let body = serde_json::to_vec(&AllocateTerminalBody {
        columns: 80,
        rows: 24,
        max_output_bytes: 4_096,
        max_output_age_millis: 60_000,
    })
    .unwrap();

    let unavailable = response_bytes(
        service.clone(),
        owner.clone(),
        request(
            Method::POST,
            &format!("/v1/processes/{PROCESS}/terminals"),
            body.clone(),
        ),
        StatusCode::NOT_IMPLEMENTED,
    )
    .await;
    let document: Value =
        serde_yaml::from_str(include_str!("../../docs/api/openapi.exec.yaml")).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&unavailable).unwrap(),
        document["components"]["responses"]["PtyUnavailable"]["content"]["application/problem+json"]
            ["examples"]["platform_unavailable"]["value"]
    );
    assert!(!String::from_utf8(unavailable).unwrap().contains(PROCESS));

    let missing = response_bytes(
        service.clone(),
        owner,
        request(
            Method::POST,
            &format!("/v1/processes/{MISSING_PROCESS}/terminals"),
            body.clone(),
        ),
        StatusCode::NOT_FOUND,
    )
    .await;
    let hidden = response_bytes(
        service,
        principal("principal_00000000000000000000000002", project),
        request(
            Method::POST,
            &format!("/v1/processes/{PROCESS}/terminals"),
            body,
        ),
        StatusCode::NOT_FOUND,
    )
    .await;
    assert_eq!(hidden, missing);
}

#[tokio::test]
async fn cancellation_replays_without_a_second_coordinator_call_and_emits_an_event() {
    let (service, calls) = fixture();
    let authenticated = principal(
        "principal_00000000000000000000000001",
        ProjectId::parse(PROJECT).unwrap(),
    );
    let uri = format!("/v1/processes/{PROCESS}/cancel");

    let first: Value = serde_json::from_slice(
        &response_bytes(
            service.clone(),
            authenticated.clone(),
            request(Method::POST, &uri, "{}"),
            StatusCode::OK,
        )
        .await,
    )
    .unwrap();
    let replay: Value = serde_json::from_slice(
        &response_bytes(
            service.clone(),
            authenticated.clone(),
            request(Method::POST, &uri, "{}"),
            StatusCode::OK,
        )
        .await,
    )
    .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(first["changed"], true);
    assert_eq!(first["replayed"], false);
    assert_eq!(replay["changed"], false);
    assert_eq!(replay["replayed"], true);

    let expanded: Value = serde_json::from_slice(
        &response_bytes(
            service.clone(),
            principal_with_grants(
                "principal_00000000000000000000000001",
                ProjectId::parse(PROJECT).unwrap(),
                [
                    Grant::WorkspaceRead,
                    Grant::WorkspaceWrite,
                    Grant::ProcessSpawn,
                    Grant::ModelCall,
                ],
            ),
            request(Method::POST, &uri, "{}"),
            StatusCode::OK,
        )
        .await,
    )
    .unwrap();
    assert_eq!(expanded["replayed"], true);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    for changed_snapshot in [
        principal(
            "principal_00000000000000000000000001",
            ProjectId::parse(OTHER_PROJECT).unwrap(),
        ),
        principal_with_grants(
            "principal_00000000000000000000000001",
            ProjectId::parse(PROJECT).unwrap(),
            [Grant::WorkspaceRead],
        ),
    ] {
        let hidden = response_bytes(
            service.clone(),
            changed_snapshot,
            request(Method::POST, &uri, "{}"),
            StatusCode::NOT_FOUND,
        )
        .await;
        assert_eq!(
            serde_json::from_slice::<Value>(&hidden).unwrap()["instance"],
            "/v1/executor"
        );
    }
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    let other_events: Value = serde_json::from_slice(
        &response_bytes(
            service.clone(),
            principal(
                "principal_00000000000000000000000002",
                ProjectId::parse(PROJECT).unwrap(),
            ),
            request(
                Method::GET,
                &format!("/v1/projects/{PROJECT}/executor/events?cursor=exec_0000000000000000"),
                "",
            ),
            StatusCode::OK,
        )
        .await,
    )
    .unwrap();
    assert!(other_events["items"].as_array().unwrap().is_empty());
    assert_eq!(other_events["next_cursor"], "exec_0000000000000000");
    assert!(
        !serde_json::to_string(&other_events)
            .unwrap()
            .contains(PROCESS)
    );

    let events: Value = serde_json::from_slice(
        &response_bytes(
            service,
            authenticated,
            request(
                Method::GET,
                &format!("/v1/projects/{PROJECT}/executor/events?cursor=exec_0000000000000000"),
                "",
            ),
            StatusCode::OK,
        )
        .await,
    )
    .unwrap();
    assert_eq!(events["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        events["items"][0]["event_type"],
        "process.cancellation_completed"
    );
    assert_eq!(events["next_cursor"], "exec_0000000000000001");
}

#[tokio::test]
async fn cancellation_unknown_stays_unknown_until_quiescence_and_completes_once() {
    let root = std::env::temp_dir().join(format!(
        "kit-exec-cancel-unknown-{}",
        ProcessId::generate().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("state.sqlite3");
    let project_id = ProjectId::parse(PROJECT).unwrap();
    let principal_id = PrincipalId::parse("principal_00000000000000000000000001").unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
        principal_id,
        FencingToken::new(1),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let open = || {
        Arc::new(
            ManagerExecService::open(
                &database,
                TerminalManager::new(
                    project_id,
                    NativePtyUnavailable,
                    discard_snapshot as fn(&TerminalSnapshot) -> io::Result<()>,
                ),
                UnknownThenQuiescent {
                    calls: calls.clone(),
                    unknown_calls: 2,
                },
            )
            .unwrap(),
        )
    };
    let service = open();
    service
        .register_process(ProcessRegistration {
            project_id,
            claim: ProcessClaim::new(
                ProcessId::parse(PROCESS).unwrap(),
                ProcessOwnership::Attempt(owner),
            ),
            execution_id: Some(42),
            state: ProcessResourceState::Started,
            terminal_request: TerminalRequest::default(),
            boundary_id: "unknown-cancellation-boundary".to_owned(),
        })
        .unwrap();
    let authenticated = principal("principal_00000000000000000000000001", project_id);
    let uri = format!("/v1/processes/{PROCESS}/cancel");

    let first = response_bytes(
        service,
        authenticated.clone(),
        request(Method::POST, &uri, "{}"),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(
        serde_json::from_slice::<Value>(&first).unwrap()["code"],
        "outcome_unknown"
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        rusqlite::Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT status FROM executor_api_idempotency WHERE idempotency_key='exec-api-key'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
        "outcome_unknown"
    );

    response_bytes(
        open(),
        authenticated.clone(),
        request(Method::POST, &uri, "{}"),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    let no_events = open()
        .events(&authenticated, project_id, 0)
        .expect("unknown cancellation has a readable empty feed");
    assert!(no_events["items"].as_array().unwrap().is_empty());

    let completed: Value = serde_json::from_slice(
        &response_bytes(
            open(),
            authenticated.clone(),
            request(Method::POST, &uri, "{}"),
            StatusCode::OK,
        )
        .await,
    )
    .unwrap();
    assert_eq!(completed["replayed"], true);
    assert_eq!(completed["changed"], false);
    assert_eq!(calls.load(Ordering::Relaxed), 3);

    let replay: Value = serde_json::from_slice(
        &response_bytes(
            open(),
            authenticated.clone(),
            request(Method::POST, &uri, "{}"),
            StatusCode::OK,
        )
        .await,
    )
    .unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    let events = open().events(&authenticated, project_id, 0).unwrap();
    assert_eq!(events["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        events["items"][0]["event_type"],
        "process.cancellation_completed"
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn idempotency_capacity_rejects_4097th_and_restart_replays_oldest() {
    let root = std::env::temp_dir().join(format!(
        "kit-exec-idempotency-capacity-{}",
        ProcessId::generate().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("state.sqlite3");
    let project_id = ProjectId::parse(PROJECT).unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
        PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
        FencingToken::new(1),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let open = || {
        ManagerExecService::open(
            &database,
            TerminalManager::new(
                project_id,
                NativePtyUnavailable,
                discard_snapshot as fn(&TerminalSnapshot) -> io::Result<()>,
            ),
            FakeCancellation(calls.clone()),
        )
        .unwrap()
    };
    let service = open();
    service
        .register_process(ProcessRegistration {
            project_id,
            claim: ProcessClaim::new(
                ProcessId::parse(PROCESS).unwrap(),
                ProcessOwnership::Attempt(owner),
            ),
            execution_id: Some(42),
            state: ProcessResourceState::Started,
            terminal_request: TerminalRequest::default(),
            boundary_id: "idempotency-capacity-boundary".to_owned(),
        })
        .unwrap();
    let authenticated = principal("principal_00000000000000000000000001", project_id);
    let process_id = ProcessId::parse(PROCESS).unwrap();
    for index in 0..4_096 {
        service
            .cancel_process(
                &authenticated,
                process_id,
                &IdempotencyKey::parse(&format!("capacity-{index:04}")).unwrap(),
            )
            .unwrap();
    }
    assert_eq!(calls.load(Ordering::Relaxed), 4_096);
    assert!(matches!(
        service.cancel_process(
            &authenticated,
            process_id,
            &IdempotencyKey::parse("capacity-4096").unwrap(),
        ),
        Err(ExecError::Unavailable)
    ));
    assert_eq!(calls.load(Ordering::Relaxed), 4_096);
    drop(service);

    let replay = open()
        .cancel_process(
            &authenticated,
            process_id,
            &IdempotencyKey::parse("capacity-0000").unwrap(),
        )
        .unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["changed"], false);
    assert_eq!(calls.load(Ordering::Relaxed), 4_096);
    assert_eq!(
        rusqlite::Connection::open(&database)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM executor_api_idempotency", [], |row| {
                row.get::<_, usize>(0)
            })
            .unwrap(),
        4_096
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn openapi_cli_routes_and_sdk_builders_have_zero_uncovered_operations() {
    let document: Value =
        serde_yaml::from_str(include_str!("../../docs/api/openapi.exec.yaml")).unwrap();
    let api = document["paths"]
        .as_object()
        .unwrap()
        .values()
        .flat_map(|path| path.as_object().unwrap().values())
        .filter_map(|operation| operation.get("x-kit-operation")?.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    let routes = EXEC_ROUTES
        .iter()
        .map(|route| route.operation)
        .collect::<std::collections::BTreeSet<_>>();
    let cli = EXEC_CLI_OPERATIONS
        .iter()
        .map(|operation| operation.service_operation)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(routes, api, "{parity}", parity = parity_table());
    assert_eq!(routes, cli, "{parity}", parity = parity_table());
    assert_eq!(EXEC_ROUTES.len(), 17);
    assert_eq!(EXEC_CLI_OPERATIONS.len(), 17);
    assert!(parity_table().contains("uncovered=0"));

    let project = ProjectId::parse(PROJECT).unwrap();
    let process = ProcessId::parse(PROCESS).unwrap();
    let terminal = TerminalId::parse(TERMINAL).unwrap();
    let key = || IdempotencyKey::parse("sdk-key").unwrap();
    let builders = [
        ExecRequest::list_processes(project),
        ExecRequest::get_process(process),
        ExecRequest::cancel_process(process, key()),
        ExecRequest::allocate_terminal(
            process,
            kit::api::http::exec::AllocateTerminalBody {
                columns: 80,
                rows: 24,
                max_output_bytes: 4096,
                max_output_age_millis: 60_000,
            },
            key(),
        ),
        ExecRequest::get_terminal(terminal),
        ExecRequest::attach_viewer(terminal, key()),
        ExecRequest::claim_writer(terminal, 1_000, key()),
        ExecRequest::get_attachment(ATTACHMENT),
        ExecRequest::renew_writer(ATTACHMENT, 1_000, key()),
        ExecRequest::release_writer(ATTACHMENT, key()),
        ExecRequest::write_input(ATTACHMENT, b"secret input", key()),
        ExecRequest::resolve_input(
            ATTACHMENT,
            kit::api::http::exec::TerminalInputResolution::Applied,
            key(),
        ),
        ExecRequest::resize(ATTACHMENT, 100, 40, key()),
        ExecRequest::read_output(ATTACHMENT, "output_0000000000000001"),
        ExecRequest::read_resizes(ATTACHMENT, "resize_0000000000000001"),
        ExecRequest::detach(ATTACHMENT, key()),
        ExecRequest::events(project, "exec_0000000000000000"),
    ];
    let built = builders
        .iter()
        .map(|request| request.operation)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(built, routes, "SDK-style request builders are incomplete");
    assert_eq!(
        builders
            .iter()
            .filter(|request| request.idempotency_key.is_some())
            .count(),
        10
    );
}

#[test]
fn openapi_input_is_write_only_and_hidden_not_found_contains_no_identifier() {
    let document: Value =
        serde_yaml::from_str(include_str!("../../docs/api/openapi.exec.yaml")).unwrap();
    assert_eq!(
        document["components"]["schemas"]["TerminalInput"]["properties"]["bytes"]["writeOnly"],
        true
    );
    let hidden = &document["components"]["responses"]["HiddenNotFound"]["content"]["application/problem+json"]
        ["example"];
    let encoded = serde_json::to_string(hidden).unwrap();
    assert!(!encoded.contains("process_"));
    assert!(!encoded.contains("terminal_"));
    assert!(!encoded.contains("attachment_"));
    assert_eq!(hidden["instance"], "/v1/executor");

    let canonical: Value =
        serde_yaml::from_str(include_str!("../../docs/api/openapi.yaml")).unwrap();
    assert_eq!(
        canonical["x-kit-executor-idempotency"]["retry-window-millis"],
        EXECUTOR_IDEMPOTENCY_RETRY_WINDOW_MILLIS
    );
    assert_eq!(
        canonical["x-kit-executor-idempotency"]["capacity-policy"],
        "reject-new"
    );
    assert_eq!(
        canonical["x-kit-executor-cursor-recovery"]["attachment-policy"],
        "current-authorized-history-viewers"
    );
    assert_eq!(
        canonical["x-kit-executor-cursor-recovery"]["invalidated-attachment-policy"],
        "excluded"
    );
    assert_eq!(
        canonical["x-kit-executor-pty"]["native-primitive"]["linux"],
        "available"
    );
    assert_eq!(
        canonical["x-kit-executor-pty"]["native-primitive"]["macos"],
        "available"
    );
    assert_eq!(
        canonical["x-kit-executor-pty"]["production-attempt-owned-profile"]["linux"],
        "profile_unavailable"
    );
    assert_eq!(
        canonical["x-kit-executor-pty"]["production-attempt-owned-profile"]["macos"],
        "profile_unavailable"
    );
    assert_eq!(
        canonical["x-kit-executor-pty"]["production-attempt-owned-profile"]["windows"],
        "platform_unavailable"
    );
    assert_eq!(
        canonical["x-kit-executor-pty"]["external-register"]["linux"],
        "EXT-22"
    );
    assert_eq!(canonical["x-kit-executor-pty"]["windows-parity"], false);
    assert_eq!(
        canonical["x-kit-executor-pty"]["windows-external-blockers"],
        json!([
            "ConPTY",
            "Job Objects",
            "Windows container/Hyper-V boundary"
        ])
    );
    assert_eq!(
        document["components"]["responses"]["PtyUnavailable"]["content"]["application/problem+json"]
            ["examples"]["profile_unavailable"]["value"]["code"],
        "profile_unavailable"
    );
    assert_eq!(
        document["components"]["responses"]["PtyUnavailable"]["content"]["application/problem+json"]
            ["examples"]["profile_unavailable"]["value"]["status"],
        501
    );
    let compatibility = include_str!("../../docs/compatibility/exec-contracts.md");
    assert!(
        compatibility.contains("Primitive availability is not production executor availability")
    );
    assert!(compatibility.contains("[`EXT-22`](../operations/ext-register.md#prerequisites)"));
    assert!(compatibility.contains("retained output, resize events, and retention gaps"));
    assert_eq!(canonical["x-kit-provider-correction"]["dependency"], "M004");

    for route in EXEC_ROUTES {
        let method = route.method.to_ascii_lowercase();
        let operation = &document["paths"][route.path][&method];
        assert!(
            operation["responses"].get("504").is_some(),
            "{}",
            route.operation
        );
        if route.mutation {
            assert!(
                operation["responses"].get("413").is_some(),
                "{}",
                route.operation
            );
            assert!(
                operation["responses"].get("415").is_some(),
                "{}",
                route.operation
            );
        }
        let pointer = route.path.replace('~', "~0").replace('/', "~1");
        assert_eq!(
            canonical["paths"][route.path]["$ref"],
            format!("./openapi.exec.yaml#/paths/{pointer}")
        );
    }
}

#[tokio::test]
async fn executor_transport_returns_documented_body_and_timeout_errors() {
    let (exec, _) = fixture();
    let authenticated = principal(
        "principal_00000000000000000000000001",
        ProjectId::parse(PROJECT).unwrap(),
    );
    let authenticator = Arc::new(move |_: &axum::http::request::Parts| Ok(authenticated.clone()));
    let app = authenticated_router_with_exec(
        Arc::new(UnusedService),
        authenticator,
        RouterConfig {
            json_body_limit: JSON_BODY_LIMIT,
            request_timeout: std::time::Duration::from_millis(10),
        },
        exec,
    );
    let uri = format!("/v1/processes/{PROCESS}/cancel");

    let unsupported = Request::builder()
        .method(Method::POST)
        .uri(&uri)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header("idempotency-key", "unsupported-media")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(unsupported).await.unwrap().status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );

    let oversized = request(Method::POST, &uri, vec![b' '; JSON_BODY_LIMIT + 1]);
    assert_eq!(
        app.clone().oneshot(oversized).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );

    let pending = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", "timeout")
        .body(Body::from_stream(futures_util::stream::pending::<
            Result<Bytes, Infallible>,
        >()))
        .unwrap();
    assert_eq!(
        app.oneshot(pending).await.unwrap().status(),
        StatusCode::GATEWAY_TIMEOUT
    );
}

#[test]
fn raw_input_debug_is_redacted() {
    let canary = b"exec-input-canary";
    let request = ExecRequest::write_input(
        ATTACHMENT,
        canary,
        IdempotencyKey::parse("raw-input-debug").unwrap(),
    );
    let debug = format!("{request:?}");
    assert!(!debug.contains(std::str::from_utf8(canary).unwrap()));
    assert!(debug.contains("[REDACTED]"));

    let body = kit::api::http::exec::TerminalInputBody {
        bytes: canary.to_vec(),
    };
    assert!(!format!("{body:?}").contains(std::str::from_utf8(canary).unwrap()));
}

#[test]
fn core_parser_reaches_every_executor_command() {
    let cases = [
        vec!["kit", "process", "list", "--project", PROJECT],
        vec!["kit", "process", "show", "--process", PROCESS],
        vec!["kit", "process", "cancel", "--process", PROCESS],
        vec![
            "kit",
            "terminal",
            "allocate",
            "--process",
            PROCESS,
            "--columns",
            "80",
            "--rows",
            "24",
            "--max-output-bytes",
            "4096",
            "--max-output-age-ms",
            "60000",
        ],
        vec!["kit", "terminal", "show", "--terminal", TERMINAL],
        vec!["kit", "terminal", "attach", "--terminal", TERMINAL],
        vec![
            "kit",
            "terminal",
            "writer-claim",
            "--terminal",
            TERMINAL,
            "--lease-ms",
            "1000",
        ],
        vec![
            "kit",
            "terminal",
            "attachment-show",
            "--attachment",
            ATTACHMENT,
        ],
        vec![
            "kit",
            "terminal",
            "writer-renew",
            "--attachment",
            ATTACHMENT,
            "--lease-ms",
            "1000",
        ],
        vec![
            "kit",
            "terminal",
            "writer-release",
            "--attachment",
            ATTACHMENT,
        ],
        vec![
            "kit",
            "terminal",
            "input",
            "--attachment",
            ATTACHMENT,
            "--idempotency-key",
            "exec-input-key",
        ],
        vec![
            "kit",
            "terminal",
            "input-resolve",
            "--attachment",
            ATTACHMENT,
            "--outcome",
            "applied",
            "--idempotency-key",
            "exec-input-key",
        ],
        vec![
            "kit",
            "terminal",
            "resize",
            "--attachment",
            ATTACHMENT,
            "--columns",
            "100",
            "--rows",
            "40",
        ],
        vec!["kit", "terminal", "output", "--attachment", ATTACHMENT],
        vec!["kit", "terminal", "resizes", "--attachment", ATTACHMENT],
        vec!["kit", "terminal", "detach", "--attachment", ATTACHMENT],
        vec!["kit", "executor", "events", "--project", PROJECT],
    ];
    let parsed = cases
        .into_iter()
        .map(|arguments| match parse(arguments).unwrap().invocation {
            Invocation::Exec(request) => request.operation,
            _ => panic!("executor command used a non-executor dispatch path"),
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        parsed,
        EXEC_CLI_OPERATIONS
            .iter()
            .map(|operation| operation.service_operation)
            .collect()
    );
}

#[test]
fn terminal_input_rejects_direct_values_and_accepts_stdin_or_file_descriptors() {
    assert!(
        parse([
            "kit",
            "terminal",
            "input",
            "--attachment",
            ATTACHMENT,
            "--input",
            "secret",
            "--idempotency-key",
            "exec-input-key",
        ])
        .is_err()
    );
    for arguments in [
        vec![
            "kit",
            "terminal",
            "input",
            "--attachment",
            ATTACHMENT,
            "--idempotency-key",
            "exec-input-key",
        ],
        vec![
            "kit",
            "terminal",
            "input",
            "--attachment",
            ATTACHMENT,
            "--input-file",
            "-",
            "--idempotency-key",
            "exec-input-key",
        ],
    ] {
        let parsed = parse(arguments).unwrap();
        assert!(!format!("{parsed:?}").contains("secret"));
    }
}

#[test]
fn terminal_input_commands_require_and_reuse_an_explicit_key() {
    for arguments in [
        vec!["kit", "terminal", "input", "--attachment", ATTACHMENT],
        vec![
            "kit",
            "terminal",
            "input-resolve",
            "--attachment",
            ATTACHMENT,
            "--outcome",
            "applied",
        ],
    ] {
        let error = parse(arguments).unwrap_err();
        assert!(
            error
                .message
                .contains("required arguments were not provided")
        );
        assert!(error.message.contains("--idempotency-key <KEY>"));
    }

    let key = "retained-input-key";
    for arguments in [
        vec![
            "kit",
            "terminal",
            "input",
            "--attachment",
            ATTACHMENT,
            "--idempotency-key",
            key,
        ],
        vec![
            "kit",
            "terminal",
            "input-resolve",
            "--attachment",
            ATTACHMENT,
            "--outcome",
            "not-applied",
            "--idempotency-key",
            key,
        ],
    ] {
        let Invocation::Exec(request) = parse(arguments).unwrap().invocation else {
            panic!("expected executor request");
        };
        assert_eq!(request.idempotency_key.as_ref().unwrap().as_str(), key);
    }
}

fn insert_pending_mutation(
    database: &std::path::Path,
    authenticated: &AuthenticatedPrincipal,
    project_id: ProjectId,
    operation: &str,
    resource: &str,
    key: &IdempotencyKey,
    request: &Value,
) {
    let mut digest = blake3::Hasher::new();
    digest.update(operation.as_bytes());
    digest.update(&[0]);
    digest.update(resource.as_bytes());
    digest.update(&[0]);
    digest.update(&serde_json::to_vec(request).unwrap());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    rusqlite::Connection::open(database)
        .unwrap()
        .execute(
            "INSERT INTO executor_api_idempotency
             (principal_id, project_id, operation, resource, idempotency_key, digest, status, response, updated_millis)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', NULL, ?7)",
            rusqlite::params![
                authenticated.principal_id().to_string(),
                project_id.to_string(),
                operation,
                resource,
                key.as_str(),
                digest.finalize().as_bytes().as_slice(),
                now,
            ],
        )
        .unwrap();
}

fn mark_outcome_unknown(database: &std::path::Path, key: &IdempotencyKey) {
    rusqlite::Connection::open(database)
        .unwrap()
        .execute(
            "UPDATE executor_api_idempotency SET status='outcome_unknown'
             WHERE operation='terminal.input' AND idempotency_key=?1",
            [key.as_str()],
        )
        .unwrap();
}

#[test]
fn ambiguous_terminal_mutations_never_reconcile_from_matching_live_state() {
    let root = std::env::temp_dir().join(format!(
        "kit-exec-ambiguous-terminal-{}",
        ProcessId::generate().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("state.sqlite3");
    let project_id = ProjectId::parse(PROJECT).unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
        PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
        FencingToken::new(1),
    );
    let service = ManagerExecService::open(
        &database,
        TerminalManager::new(
            project_id,
            FakePtyDriver::default(),
            SqliteTerminalSnapshotStore::open(&database).unwrap(),
        ),
        FakeCancellation(Arc::new(AtomicUsize::new(0))),
    )
    .unwrap();
    let allocation = AllocateTerminalBody {
        columns: 80,
        rows: 24,
        max_output_bytes: 4_096,
        max_output_age_millis: 60_000,
    };
    prepare_pty(
        &service,
        project_id,
        owner.principal_id,
        ProcessClaim::new(
            ProcessId::parse(PROCESS).unwrap(),
            ProcessOwnership::Attempt(owner),
        ),
        &allocation,
    );
    let authenticated = principal("principal_00000000000000000000000001", project_id);
    let process_id = ProcessId::parse(PROCESS).unwrap();
    let terminal_id = service
        .allocate_terminal(
            &authenticated,
            process_id,
            &IdempotencyKey::parse("existing-terminal").unwrap(),
            allocation.clone(),
        )
        .unwrap()["resource"]["terminal_id"]
        .as_str()
        .and_then(|value| TerminalId::parse(value).ok())
        .unwrap();
    service
        .attach_viewer(
            &authenticated,
            terminal_id,
            &IdempotencyKey::parse("existing-viewer").unwrap(),
        )
        .unwrap();
    let writer_id = service
        .claim_writer(
            &authenticated,
            terminal_id,
            &IdempotencyKey::parse("existing-writer").unwrap(),
            WriterLeaseBody {
                lease_millis: 60_000,
            },
        )
        .unwrap()["resource"]["attachment_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let allocate_key = IdempotencyKey::parse("pending-allocation").unwrap();
    insert_pending_mutation(
        &database,
        &authenticated,
        project_id,
        "terminal.allocate",
        PROCESS,
        &allocate_key,
        &serde_json::to_value(&allocation).unwrap(),
    );
    for _ in 0..2 {
        assert!(matches!(
            service.allocate_terminal(
                &authenticated,
                process_id,
                &allocate_key,
                allocation.clone(),
            ),
            Err(ExecError::OutcomeUnknown)
        ));
    }

    let viewer_key = IdempotencyKey::parse("pending-viewer").unwrap();
    insert_pending_mutation(
        &database,
        &authenticated,
        project_id,
        "terminal.viewer.attach",
        &terminal_id.to_string(),
        &viewer_key,
        &serde_json::json!({}),
    );
    for _ in 0..2 {
        assert!(matches!(
            service.attach_viewer(&authenticated, terminal_id, &viewer_key),
            Err(ExecError::OutcomeUnknown)
        ));
    }

    let claim = WriterLeaseBody {
        lease_millis: 60_000,
    };
    let claim_key = IdempotencyKey::parse("pending-writer").unwrap();
    insert_pending_mutation(
        &database,
        &authenticated,
        project_id,
        "terminal.writer.claim",
        &terminal_id.to_string(),
        &claim_key,
        &serde_json::to_value(&claim).unwrap(),
    );
    for _ in 0..2 {
        assert!(matches!(
            service.claim_writer(&authenticated, terminal_id, &claim_key, claim.clone()),
            Err(ExecError::OutcomeUnknown)
        ));
    }

    let renew = WriterLeaseBody {
        lease_millis: 30_000,
    };
    let renew_key = IdempotencyKey::parse("pending-renewal").unwrap();
    insert_pending_mutation(
        &database,
        &authenticated,
        project_id,
        "terminal.writer.renew",
        &writer_id,
        &renew_key,
        &serde_json::to_value(&renew).unwrap(),
    );
    for _ in 0..2 {
        assert!(matches!(
            service.renew_writer(&authenticated, &writer_id, &renew_key, renew.clone()),
            Err(ExecError::OutcomeUnknown)
        ));
    }

    let resize = TerminalResizeBody {
        columns: 80,
        rows: 24,
    };
    let resize_key = IdempotencyKey::parse("pending-resize").unwrap();
    insert_pending_mutation(
        &database,
        &authenticated,
        project_id,
        "terminal.resize",
        &writer_id,
        &resize_key,
        &serde_json::to_value(&resize).unwrap(),
    );
    for _ in 0..2 {
        assert!(matches!(
            service.resize(&authenticated, &writer_id, &resize_key, resize.clone()),
            Err(ExecError::OutcomeUnknown)
        ));
    }
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn attachment_endpoints_hide_every_unauthorized_or_invalidated_target_without_effects() {
    let root = std::env::temp_dir().join(format!(
        "kit-exec-attachment-auth-{}",
        ProcessId::generate().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("state.sqlite3");
    let project_id = ProjectId::parse(PROJECT).unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
        PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
        FencingToken::new(1),
    );
    let driver = FakePtyDriver::default();
    let service = Arc::new(
        ManagerExecService::open(
            &database,
            TerminalManager::new(
                project_id,
                driver.clone(),
                SqliteTerminalSnapshotStore::open(&database).unwrap(),
            ),
            FakeCancellation(Arc::new(AtomicUsize::new(0))),
        )
        .unwrap(),
    );
    let allocation = AllocateTerminalBody {
        columns: 80,
        rows: 24,
        max_output_bytes: 4_096,
        max_output_age_millis: 60_000,
    };
    prepare_pty(
        service.as_ref(),
        project_id,
        owner.principal_id,
        ProcessClaim::new(
            ProcessId::parse(PROCESS).unwrap(),
            ProcessOwnership::Attempt(owner),
        ),
        &allocation,
    );
    let authenticated = principal("principal_00000000000000000000000001", project_id);
    let terminal_id = service
        .allocate_terminal(
            &authenticated,
            ProcessId::parse(PROCESS).unwrap(),
            &IdempotencyKey::parse("attachment-auth-allocate").unwrap(),
            allocation,
        )
        .unwrap()["resource"]["terminal_id"]
        .as_str()
        .and_then(|value| TerminalId::parse(value).ok())
        .unwrap();
    let attachment_id = service
        .claim_writer(
            &authenticated,
            terminal_id,
            &IdempotencyKey::parse("attachment-auth-writer").unwrap(),
            WriterLeaseBody {
                lease_millis: 60_000,
            },
        )
        .unwrap()["resource"]["attachment_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let detached_id = service
        .attach_viewer(
            &authenticated,
            terminal_id,
            &IdempotencyKey::parse("attachment-auth-viewer").unwrap(),
        )
        .unwrap()["resource"]["attachment_id"]
        .as_str()
        .unwrap()
        .to_owned();
    service
        .detach(
            &authenticated,
            &detached_id,
            &IdempotencyKey::parse("attachment-auth-detach").unwrap(),
        )
        .unwrap();
    let baseline_driver = driver.state();
    let baseline_claims = rusqlite::Connection::open(&database)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM executor_api_idempotency", [], |row| {
            row.get::<_, usize>(0)
        })
        .unwrap();

    let restarted: Arc<dyn ExecService> = Arc::new(
        ManagerExecService::open(
            &database,
            TerminalManager::new(
                project_id,
                driver.clone(),
                SqliteTerminalSnapshotStore::open(&database).unwrap(),
            ),
            FakeCancellation(Arc::new(AtomicUsize::new(0))),
        )
        .unwrap(),
    );
    let active: Arc<dyn ExecService> = service;
    let endpoints = [
        (Method::GET, "", ""),
        (Method::POST, "/renew", r#"{"lease_millis":60000}"#),
        (Method::POST, "/release", "{}"),
        (Method::POST, "/input", r#"{"bytes":[1]}"#),
        (
            Method::POST,
            "/input-resolution",
            r#"{"outcome":"applied"}"#,
        ),
        (Method::POST, "/resize", r#"{"columns":100,"rows":40}"#),
        (Method::GET, "/output?cursor=output_0000000000000001", ""),
        (Method::GET, "/resizes?cursor=resize_0000000000000001", ""),
        (Method::POST, "/detach", "{}"),
    ];
    let cases = [
        (
            active.clone(),
            principal("principal_00000000000000000000000002", project_id),
            attachment_id.clone(),
        ),
        (
            active.clone(),
            principal(
                "principal_00000000000000000000000001",
                ProjectId::parse(OTHER_PROJECT).unwrap(),
            ),
            attachment_id.clone(),
        ),
        (
            active.clone(),
            principal_with_grants("principal_00000000000000000000000001", project_id, []),
            attachment_id.clone(),
        ),
        (active.clone(), authenticated.clone(), detached_id),
        (restarted, authenticated.clone(), attachment_id),
    ];
    for (method, suffix, body) in endpoints {
        let missing_uri = format!("/v1/terminal-attachments/{ATTACHMENT}{suffix}");
        let hidden = response_bytes(
            active.clone(),
            authenticated.clone(),
            request(method.clone(), &missing_uri, body),
            StatusCode::NOT_FOUND,
        )
        .await;
        for (case_service, case_principal, target) in &cases {
            let uri = format!("/v1/terminal-attachments/{target}{suffix}");
            let response = response_bytes(
                case_service.clone(),
                case_principal.clone(),
                request(method.clone(), &uri, body),
                StatusCode::NOT_FOUND,
            )
            .await;
            assert_eq!(response, hidden, "{method} {suffix} exposed target state");
        }
    }
    assert_eq!(driver.state(), baseline_driver);
    assert_eq!(
        rusqlite::Connection::open(&database)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM executor_api_idempotency", [], |row| {
                row.get::<_, usize>(0)
            })
            .unwrap(),
        baseline_claims
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn input_resolution_uses_exact_durable_ownership_after_attachment_invalidation() {
    let root = std::env::temp_dir().join(format!(
        "kit-exec-input-resolution-restart-{}",
        ProcessId::generate().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("state.sqlite3");
    let project_id = ProjectId::parse(PROJECT).unwrap();
    let principal_id = PrincipalId::parse("principal_00000000000000000000000001").unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
        principal_id,
        FencingToken::new(1),
    );
    let driver = FakePtyDriver::default();
    let open = || {
        Arc::new(
            ManagerExecService::open(
                &database,
                TerminalManager::new(
                    project_id,
                    driver.clone(),
                    SqliteTerminalSnapshotStore::open(&database).unwrap(),
                ),
                FakeCancellation(Arc::new(AtomicUsize::new(0))),
            )
            .unwrap(),
        )
    };
    let service = open();
    let allocation = AllocateTerminalBody {
        columns: 80,
        rows: 24,
        max_output_bytes: 4_096,
        max_output_age_millis: 60_000,
    };
    prepare_pty(
        service.as_ref(),
        project_id,
        principal_id,
        ProcessClaim::new(
            ProcessId::parse(PROCESS).unwrap(),
            ProcessOwnership::Attempt(owner),
        ),
        &allocation,
    );
    let authenticated = principal("principal_00000000000000000000000001", project_id);
    let terminal_id = service
        .allocate_terminal(
            &authenticated,
            ProcessId::parse(PROCESS).unwrap(),
            &IdempotencyKey::parse("input-resolution-allocation").unwrap(),
            allocation,
        )
        .unwrap()["resource"]["terminal_id"]
        .as_str()
        .and_then(|value| TerminalId::parse(value).ok())
        .unwrap();
    let attachment = service
        .claim_writer(
            &authenticated,
            terminal_id,
            &IdempotencyKey::parse("input-resolution-writer").unwrap(),
            WriterLeaseBody {
                lease_millis: 60_000,
            },
        )
        .unwrap()["resource"]["attachment_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let applied_key = IdempotencyKey::parse("exec-api-key").unwrap();
    let not_applied_key = IdempotencyKey::parse("input-resolution-not-applied").unwrap();
    let applied_bytes = b"applied input";
    let not_applied_bytes = b"not applied input";
    for (key, bytes) in [
        (&applied_key, applied_bytes.as_slice()),
        (&not_applied_key, not_applied_bytes.as_slice()),
    ] {
        insert_pending_mutation(
            &database,
            &authenticated,
            project_id,
            "terminal.input",
            &attachment,
            key,
            &json!({
                "sha256_equivalent": blake3::hash(bytes).to_hex().to_string(),
                "length": bytes.len(),
            }),
        );
        mark_outcome_unknown(&database, key);
    }
    drop(service);

    let restarted = open();
    let applied = kit::api::http::exec::TerminalInputResolutionBody {
        outcome: kit::api::http::exec::TerminalInputResolution::Applied,
    };
    let not_applied = kit::api::http::exec::TerminalInputResolutionBody {
        outcome: kit::api::http::exec::TerminalInputResolution::NotApplied,
    };
    let first = restarted
        .resolve_input(&authenticated, &attachment, &applied_key, applied)
        .unwrap();
    assert_eq!(first["changed"], true);
    assert_eq!(first["replayed"], false);
    let replay = restarted
        .resolve_input(&authenticated, &attachment, &applied_key, applied)
        .unwrap();
    assert_eq!(replay["changed"], false);
    assert_eq!(replay["replayed"], true);
    assert!(matches!(
        restarted.resolve_input(&authenticated, &attachment, &applied_key, not_applied),
        Err(ExecError::Conflict(_))
    ));
    let input_replay = restarted
        .write_input(&authenticated, &attachment, &applied_key, applied_bytes)
        .unwrap();
    assert_eq!(input_replay["replayed"], true);
    assert_eq!(input_replay["changed"], false);
    assert!(matches!(
        restarted.write_input(&authenticated, &attachment, &applied_key, b"different"),
        Err(ExecError::Conflict(_))
    ));

    let first = restarted
        .resolve_input(&authenticated, &attachment, &not_applied_key, not_applied)
        .unwrap();
    assert_eq!(first["changed"], true);
    let replay = restarted
        .resolve_input(&authenticated, &attachment, &not_applied_key, not_applied)
        .unwrap();
    assert_eq!(replay["replayed"], true);
    assert!(matches!(
        restarted.write_input(
            &authenticated,
            &attachment,
            &not_applied_key,
            not_applied_bytes,
        ),
        Err(ExecError::Conflict(_))
    ));
    assert_eq!(driver.state().input_byte_count, 0);

    let missing = response_bytes(
        restarted.clone(),
        authenticated.clone(),
        request(
            Method::POST,
            &format!("/v1/terminal-attachments/{ATTACHMENT}/input-resolution"),
            r#"{"outcome":"applied"}"#,
        ),
        StatusCode::NOT_FOUND,
    )
    .await;
    for denied in [
        principal("principal_00000000000000000000000002", project_id),
        principal(
            "principal_00000000000000000000000001",
            ProjectId::parse(OTHER_PROJECT).unwrap(),
        ),
    ] {
        let hidden = response_bytes(
            restarted.clone(),
            denied,
            request(
                Method::POST,
                &format!("/v1/terminal-attachments/{attachment}/input-resolution"),
                r#"{"outcome":"applied"}"#,
            ),
            StatusCode::NOT_FOUND,
        )
        .await;
        assert_eq!(hidden, missing);
    }
    assert!(matches!(
        restarted.resize(
            &authenticated,
            &attachment,
            &IdempotencyKey::parse("invalidated-resize").unwrap(),
            TerminalResizeBody {
                columns: 100,
                rows: 40,
            },
        ),
        Err(ExecError::NotFound)
    ));
    assert_eq!(
        rusqlite::Connection::open(&database)
            .unwrap()
            .query_row(
                "SELECT invalidated FROM executor_api_attachments WHERE attachment_id=?1",
                [&attachment],
                |row| row.get::<_, u8>(0),
            )
            .unwrap(),
        1
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn durable_startup_marks_prepared_and_started_processes_outcome_unknown() {
    let root = std::env::temp_dir().join(format!(
        "kit-exec-process-recovery-{}",
        ProcessId::generate().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("state.sqlite3");
    let project_id = ProjectId::parse(PROJECT).unwrap();
    let principal_id = PrincipalId::parse("principal_00000000000000000000000001").unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
        principal_id,
        FencingToken::new(1),
    );
    let open = || {
        ManagerExecService::open(
            &database,
            TerminalManager::new(
                project_id,
                NativePtyUnavailable,
                discard_snapshot as fn(&TerminalSnapshot) -> io::Result<()>,
            ),
            FakeCancellation(Arc::new(AtomicUsize::new(0))),
        )
        .unwrap()
    };
    let service = open();
    let context = ProcessRegistrationContext {
        project_id,
        principal_id,
    };
    let process_ids = std::array::from_fn::<_, 4, _>(|_| ProcessId::generate().unwrap());
    for process_id in process_ids {
        let boundary = PersistedBoundary {
            ownership: Ownership::new("attempt-owner", process_id.to_string()).unwrap(),
            identity: BoundaryIdentity::new(
                BoundaryKind::Container,
                process_id.to_string(),
                "ownership-token",
                "runtime-token",
            )
            .unwrap(),
        };
        ProcessRegistry::prepared(
            &service,
            context,
            ProcessClaim::new(process_id, ProcessOwnership::Attempt(owner)),
            &boundary,
            kit::executor::process::own::ProcessTerminalConfig::default(),
        )
        .unwrap();
    }
    service
        .update_process(process_ids[1], ProcessResourceState::Started)
        .unwrap();
    service
        .update_process(
            process_ids[2],
            ProcessResourceState::Exited {
                success: true,
                code: Some(0),
                signal: None,
            },
        )
        .unwrap();
    service
        .update_process(process_ids[3], ProcessResourceState::OutcomeUnknown)
        .unwrap();
    drop(service);

    let authenticated = principal("principal_00000000000000000000000001", project_id);
    for _ in 0..2 {
        let restarted = open();
        assert_eq!(
            restarted
                .get_process(&authenticated, process_ids[0])
                .unwrap()["state"]["status"],
            "outcome_unknown"
        );
        assert_eq!(
            restarted
                .get_process(&authenticated, process_ids[1])
                .unwrap()["state"]["status"],
            "outcome_unknown"
        );
        assert_eq!(
            restarted
                .get_process(&authenticated, process_ids[2])
                .unwrap()["state"]["status"],
            "exited"
        );
        assert_eq!(
            restarted
                .get_process(&authenticated, process_ids[3])
                .unwrap()["state"]["status"],
            "outcome_unknown"
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn executor_event_pruning_keeps_global_rowid_order_across_feeds_and_restarts() {
    const EVENTS_PER_FEED: u64 = 2_048;

    let root = std::env::temp_dir().join(format!(
        "kit-exec-event-order-{}",
        ProcessId::generate().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("state.sqlite3");
    let project_a = ProjectId::parse(PROJECT).unwrap();
    let project_b = ProjectId::parse(OTHER_PROJECT).unwrap();
    let principal_a = PrincipalId::parse("principal_00000000000000000000000001").unwrap();
    let principal_b = PrincipalId::parse("principal_00000000000000000000000002").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let open = || {
        ManagerExecService::open(
            &database,
            TerminalManager::new(
                project_a,
                NativePtyUnavailable,
                discard_snapshot as fn(&TerminalSnapshot) -> io::Result<()>,
            ),
            FakeCancellation(calls.clone()),
        )
        .unwrap()
    };
    let service = open();
    service
        .register_process(ProcessRegistration {
            project_id: project_a,
            claim: ProcessClaim::new(
                ProcessId::parse(PROCESS).unwrap(),
                ProcessOwnership::Attempt(AttemptOwnership::new(
                    AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
                    principal_a,
                    FencingToken::new(1),
                )),
            ),
            execution_id: Some(1),
            state: ProcessResourceState::Started,
            terminal_request: TerminalRequest::default(),
            boundary_id: "event-order-boundary".to_owned(),
        })
        .unwrap();
    drop(service);

    let mut connection = rusqlite::Connection::open(&database).unwrap();
    let transaction = connection.transaction().unwrap();
    for position in 0..EVENTS_PER_FEED {
        for (principal, project, feed) in
            [(principal_b, project_b, "b"), (principal_a, project_a, "a")]
        {
            let event = serde_json::json!({
                "schema_version": 1,
                "cursor": format!("exec_{position:016x}"),
                "event_type": "seed",
                "project_id": project,
                "resource_id": format!("{feed}-{position}"),
            });
            transaction
                .execute(
                    "INSERT INTO executor_api_events (principal_id, project_id, position, event)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![
                        principal.to_string(),
                        project.to_string(),
                        position,
                        serde_json::to_string(&event).unwrap(),
                    ],
                )
                .unwrap();
        }
    }
    for (principal, project) in [(principal_a, project_a), (principal_b, project_b)] {
        transaction
            .execute(
                "INSERT INTO executor_api_event_sequences (principal_id, project_id, next_position)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![principal.to_string(), project.to_string(), EVENTS_PER_FEED],
            )
            .unwrap();
    }
    transaction.commit().unwrap();

    let authenticated_a = principal("principal_00000000000000000000000001", project_a);
    let authenticated_b = principal("principal_00000000000000000000000002", project_b);
    for restart in 0..2 {
        let service = open();
        if restart == 0 {
            service
                .cancel_process(
                    &authenticated_a,
                    ProcessId::parse(PROCESS).unwrap(),
                    &IdempotencyKey::parse("event-order-prune").unwrap(),
                )
                .unwrap();
        }
        assert!(service.events(&authenticated_a, project_a, 0).is_ok());
        assert!(matches!(
            service.events(&authenticated_b, project_b, 0),
            Err(ExecError::CursorExpired(problem))
                if problem["new_cursor"] == format!("exec_{EVENTS_PER_FEED:016x}")
        ));
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pruned_cursor_recovery_includes_interrupted_history_and_filters_invalidated_attachments() {
    const OTHER_FEED_EVENTS: u64 = 4_095;

    let root = std::env::temp_dir().join(format!(
        "kit-exec-terminal-cursor-recovery-{}",
        ProcessId::generate().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("state.sqlite3");
    let project_id = ProjectId::parse(PROJECT).unwrap();
    let principal_a = PrincipalId::parse("principal_00000000000000000000000001").unwrap();
    let principal_b = PrincipalId::parse("principal_00000000000000000000000002").unwrap();
    let process_a = ProcessId::parse(PROCESS).unwrap();
    let process_b = ProcessId::parse(MISSING_PROCESS).unwrap();
    let snapshots = SqliteTerminalSnapshotStore::open(&database).unwrap();
    let driver = FakePtyDriver::default();
    let manager = TerminalManager::new(project_id, driver.clone(), snapshots.clone());
    let service = ManagerExecService::open(
        &database,
        manager,
        FakeCancellation(Arc::new(AtomicUsize::new(0))),
    )
    .unwrap();
    let terminal_allocation = AllocateTerminalBody {
        columns: 100,
        rows: 40,
        max_output_bytes: 4_096,
        max_output_age_millis: 60_000,
    };
    for (process_id, principal_id, attempt) in [
        (process_a, principal_a, "attempt_00000000000000000000000001"),
        (process_b, principal_b, "attempt_00000000000000000000000002"),
    ] {
        let claim = ProcessClaim::new(
            process_id,
            ProcessOwnership::Attempt(AttemptOwnership::new(
                AttemptId::parse(attempt).unwrap(),
                principal_id,
                FencingToken::new(1),
            )),
        );
        if process_id == process_a {
            prepare_pty(
                &service,
                project_id,
                principal_id,
                claim,
                &terminal_allocation,
            );
        } else {
            service
                .register_process(ProcessRegistration {
                    project_id,
                    claim,
                    execution_id: Some(1),
                    state: ProcessResourceState::Started,
                    terminal_request: TerminalRequest::default(),
                    boundary_id: format!("cursor-recovery-{process_id}"),
                })
                .unwrap();
        }
    }
    let authenticated_a = principal("principal_00000000000000000000000001", project_id);
    let terminal_id = service
        .allocate_terminal(
            &authenticated_a,
            process_a,
            &IdempotencyKey::parse("cursor-recovery-allocate").unwrap(),
            terminal_allocation,
        )
        .unwrap()["resource"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let invalidated_attachment = service
        .attach_viewer(
            &authenticated_a,
            TerminalId::parse(&terminal_id).unwrap(),
            &IdempotencyKey::parse("cursor-recovery-viewer").unwrap(),
        )
        .unwrap()["resource"]["attachment_id"]
        .as_str()
        .unwrap()
        .to_owned();
    drop(service);

    let mut connection = rusqlite::Connection::open(&database).unwrap();
    let transaction = connection.transaction().unwrap();
    for position in 0..OTHER_FEED_EVENTS {
        let event = serde_json::json!({
            "schema_version": 1,
            "cursor": format!("exec_{position:016x}"),
            "event_type": "seed",
            "project_id": project_id,
            "resource_id": format!("other-{position}"),
        });
        transaction
            .execute(
                "INSERT INTO executor_api_events (principal_id, project_id, position, event)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    principal_b.to_string(),
                    project_id.to_string(),
                    position,
                    serde_json::to_string(&event).unwrap(),
                ],
            )
            .unwrap();
    }
    transaction
        .execute(
            "INSERT INTO executor_api_event_sequences (principal_id, project_id, next_position)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                principal_b.to_string(),
                project_id.to_string(),
                OTHER_FEED_EVENTS
            ],
        )
        .unwrap();
    transaction.commit().unwrap();

    let restored_snapshots = snapshots.load().unwrap();
    let manager = TerminalManager::new(project_id, driver.clone(), snapshots.clone());
    let controls = manager
        .restore_snapshots(restored_snapshots.clone(), 0, |_| Ok(()))
        .unwrap();
    let service = ManagerExecService::open(
        &database,
        manager,
        FakeCancellation(Arc::new(AtomicUsize::new(0))),
    )
    .unwrap();
    for (control, snapshot) in controls.into_iter().zip(&restored_snapshots) {
        service.restore_terminal(control, snapshot).unwrap();
    }
    service
        .cancel_process(
            &principal("principal_00000000000000000000000002", project_id),
            process_b,
            &IdempotencyKey::parse("cursor-recovery-prune").unwrap(),
        )
        .unwrap();
    drop(service);

    let restored_snapshots = snapshots.load().unwrap();
    let manager = TerminalManager::new(project_id, driver, snapshots.clone());
    let controls = manager
        .restore_snapshots(restored_snapshots.clone(), 0, |_| Ok(()))
        .unwrap();
    let service = ManagerExecService::open(
        &database,
        manager,
        FakeCancellation(Arc::new(AtomicUsize::new(0))),
    )
    .unwrap();
    for (control, snapshot) in controls.into_iter().zip(&restored_snapshots) {
        service.restore_terminal(control, snapshot).unwrap();
    }
    let history_attachment = service
        .attach_viewer(
            &authenticated_a,
            TerminalId::parse(&terminal_id).unwrap(),
            &IdempotencyKey::parse("cursor-recovery-history-viewer").unwrap(),
        )
        .unwrap()["resource"]["attachment_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let stale_process = ProcessId::generate().unwrap();
    let stale_allocation = AllocateTerminalBody {
        columns: 90,
        rows: 30,
        max_output_bytes: 4_096,
        max_output_age_millis: 60_000,
    };
    let replaced_attempt = AttemptId::parse("attempt_00000000000000000000000003").unwrap();
    prepare_pty(
        &service,
        project_id,
        principal_a,
        ProcessClaim::new(
            stale_process,
            ProcessOwnership::Attempt(AttemptOwnership::new(
                replaced_attempt,
                principal_a,
                FencingToken::new(1),
            )),
        ),
        &stale_allocation,
    );
    let stale_terminal = service
        .allocate_terminal(
            &authenticated_a,
            stale_process,
            &IdempotencyKey::parse("cursor-recovery-stale-terminal").unwrap(),
            stale_allocation,
        )
        .unwrap()["resource"]["terminal_id"]
        .as_str()
        .and_then(|value| TerminalId::parse(value).ok())
        .unwrap();
    let stale_attachment = service
        .attach_viewer(
            &authenticated_a,
            stale_terminal,
            &IdempotencyKey::parse("cursor-recovery-stale-viewer").unwrap(),
        )
        .unwrap()["resource"]["attachment_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let successor_process = ProcessId::generate().unwrap();
    let successor_allocation = AllocateTerminalBody {
        columns: 120,
        rows: 50,
        max_output_bytes: 4_096,
        max_output_age_millis: 60_000,
    };
    prepare_pty(
        &service,
        project_id,
        principal_a,
        ProcessClaim::new(
            successor_process,
            ProcessOwnership::Attempt(AttemptOwnership::new(
                replaced_attempt,
                principal_a,
                FencingToken::new(2),
            )),
        ),
        &successor_allocation,
    );
    let successor_terminal = service
        .allocate_terminal(
            &authenticated_a,
            successor_process,
            &IdempotencyKey::parse("cursor-recovery-successor").unwrap(),
            successor_allocation,
        )
        .unwrap()["resource"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let successor_attachment = service
        .attach_viewer(
            &authenticated_a,
            TerminalId::parse(&successor_terminal).unwrap(),
            &IdempotencyKey::parse("cursor-recovery-successor-viewer").unwrap(),
        )
        .unwrap()["resource"]["attachment_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let ExecError::CursorExpired(problem) =
        service.events(&authenticated_a, project_id, 0).unwrap_err()
    else {
        panic!("pruned terminal feed did not require snapshot recovery");
    };
    assert_eq!(problem["new_cursor"], "exec_0000000000000007");
    assert_eq!(
        problem["snapshot"]["processes"].as_array().unwrap().len(),
        3
    );
    assert_eq!(
        problem["snapshot"]["terminals"].as_array().unwrap().len(),
        2
    );
    let terminals = problem["snapshot"]["terminals"].as_array().unwrap();
    assert!(
        terminals.iter().any(|item| {
            item["terminal_id"] == terminal_id && item["lifecycle"] == "interrupted"
        })
    );
    assert!(terminals.iter().any(|item| {
        item["terminal_id"] == successor_terminal && item["lifecycle"] == "active"
    }));
    assert_eq!(
        problem["snapshot"]["attachments"].as_array().unwrap().len(),
        2
    );
    let attachments = problem["snapshot"]["attachments"].as_array().unwrap();
    assert!(attachments.iter().any(|item| {
        item["attachment_id"] == history_attachment && item["terminal_id"] == terminal_id
    }));
    assert!(attachments.iter().any(|item| {
        item["attachment_id"] == successor_attachment && item["terminal_id"] == successor_terminal
    }));
    assert!(
        !attachments
            .iter()
            .any(|item| item["attachment_id"] == invalidated_attachment)
    );
    assert!(
        !attachments
            .iter()
            .any(|item| item["attachment_id"] == stale_attachment)
    );
    assert_ne!(successor_attachment, history_attachment);
    assert_ne!(successor_terminal, terminal_id);
    let resumed = service.events(&authenticated_a, project_id, 7).unwrap();
    assert!(resumed["items"].as_array().unwrap().is_empty());
    assert_eq!(resumed["next_cursor"], "exec_0000000000000007");
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn process_events_and_idempotency_survive_service_restart() {
    let root = std::env::temp_dir().join(format!(
        "kit-exec-api-restart-{}",
        ProcessId::generate().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("state.sqlite3");
    let project_id = ProjectId::parse(PROJECT).unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
        PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
        FencingToken::new(1),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let open = || {
        Arc::new(
            ManagerExecService::open(
                &database,
                TerminalManager::new(
                    project_id,
                    NativePtyUnavailable,
                    discard_snapshot as fn(&TerminalSnapshot) -> io::Result<()>,
                ),
                FakeCancellation(calls.clone()),
            )
            .unwrap(),
        )
    };
    let service = open();
    service
        .register_process(ProcessRegistration {
            project_id,
            claim: ProcessClaim::new(
                ProcessId::parse(PROCESS).unwrap(),
                ProcessOwnership::Attempt(owner),
            ),
            execution_id: Some(42),
            state: ProcessResourceState::Started,
            terminal_request: TerminalRequest::default(),
            boundary_id: "restart-boundary".to_owned(),
        })
        .unwrap();
    let authenticated = principal("principal_00000000000000000000000001", project_id);
    let uri = format!("/v1/processes/{PROCESS}/cancel");
    response_bytes(
        service,
        authenticated.clone(),
        request(Method::POST, &uri, "{}"),
        StatusCode::OK,
    )
    .await;
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    let replay: Value = serde_json::from_slice(
        &response_bytes(
            open(),
            authenticated.clone(),
            request(Method::POST, &uri, "{}"),
            StatusCode::OK,
        )
        .await,
    )
    .unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(calls.load(Ordering::Relaxed), 1);

    let events: Value = serde_json::from_slice(
        &response_bytes(
            open(),
            authenticated.clone(),
            request(
                Method::GET,
                &format!("/v1/projects/{PROJECT}/executor/events?cursor=exec_0000000000000000"),
                "",
            ),
            StatusCode::OK,
        )
        .await,
    )
    .unwrap();
    assert_eq!(events["items"].as_array().unwrap().len(), 1);

    rusqlite::Connection::open(&database)
        .unwrap()
        .execute("DELETE FROM executor_api_events", [])
        .unwrap();
    let expired: Value = serde_json::from_slice(
        &response_bytes(
            open(),
            authenticated,
            request(
                Method::GET,
                &format!("/v1/projects/{PROJECT}/executor/events?cursor=exec_0000000000000000"),
                "",
            ),
            StatusCode::GONE,
        )
        .await,
    )
    .unwrap();
    assert_eq!(expired["new_cursor"], "exec_0000000000000001");
    assert!(expired["snapshot"]["processes"].is_array());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn observed_process_lifecycle_is_api_visible_and_durable() {
    let root = std::env::temp_dir().join(format!(
        "kit-exec-registry-{}",
        ProcessId::generate().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("state.sqlite3");
    let project_id = ProjectId::parse(PROJECT).unwrap();
    let principal_id = PrincipalId::parse("principal_00000000000000000000000001").unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
        principal_id,
        FencingToken::new(1),
    );
    let process_id = ProcessId::generate().unwrap();
    let open = || {
        ManagerExecService::open(
            &database,
            TerminalManager::new(
                project_id,
                NativePtyUnavailable,
                discard_snapshot as fn(&TerminalSnapshot) -> io::Result<()>,
            ),
            FakeCancellation(Arc::new(AtomicUsize::new(0))),
        )
        .unwrap()
    };
    let service = open();
    let boundary = PersistedBoundary {
        ownership: Ownership::new("attempt-owner", process_id.to_string()).unwrap(),
        identity: BoundaryIdentity::new(
            BoundaryKind::Container,
            "registry-test",
            "ownership-token",
            "runtime-token",
        )
        .unwrap(),
    };
    let context = ProcessRegistrationContext {
        project_id,
        principal_id,
    };
    ProcessRegistry::prepared(
        &service,
        context,
        ProcessClaim::new(process_id, ProcessOwnership::Attempt(owner)),
        &boundary,
        kit::executor::process::own::ProcessTerminalConfig::default(),
    )
    .unwrap();
    ProcessRegistry::outcome_unknown(&service, context, process_id).unwrap();

    let resource = open()
        .get_process(
            &principal("principal_00000000000000000000000001", project_id),
            process_id,
        )
        .unwrap();
    assert_eq!(resource["state"]["status"], "outcome_unknown");
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn raw_input_is_absent_from_durable_executor_state() {
    const CANARY: &[u8] = b"raw-input-durable-canary";
    let root = std::env::temp_dir().join(format!(
        "kit-exec-api-input-{}",
        ProcessId::generate().unwrap()
    ));
    fs::create_dir(&root).unwrap();
    let database = root.join("state.sqlite3");
    let project_id = ProjectId::parse(PROJECT).unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::parse("attempt_00000000000000000000000001").unwrap(),
        PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
        FencingToken::new(1),
    );
    let snapshots = SqliteTerminalSnapshotStore::open(&database).unwrap();
    let driver = FakePtyDriver::default();
    let service = Arc::new(
        ManagerExecService::open(
            &database,
            TerminalManager::new(project_id, driver.clone(), snapshots.clone()),
            FakeCancellation(Arc::new(AtomicUsize::new(0))),
        )
        .unwrap(),
    );
    let allocation = AllocateTerminalBody {
        columns: 80,
        rows: 24,
        max_output_bytes: 4_096,
        max_output_age_millis: 60_000,
    };
    prepare_pty(
        service.as_ref(),
        project_id,
        owner.principal_id,
        ProcessClaim::new(
            ProcessId::parse(PROCESS).unwrap(),
            ProcessOwnership::Attempt(owner),
        ),
        &allocation,
    );
    let authenticated = principal("principal_00000000000000000000000001", project_id);
    let terminal = service
        .allocate_terminal(
            &authenticated,
            ProcessId::parse(PROCESS).unwrap(),
            &IdempotencyKey::parse("allocate-input-test").unwrap(),
            allocation,
        )
        .unwrap()["resource"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let attachment = service
        .claim_writer(
            &authenticated,
            TerminalId::parse(&terminal).unwrap(),
            &IdempotencyKey::parse("claim-input-test").unwrap(),
            WriterLeaseBody {
                lease_millis: 60_000,
            },
        )
        .unwrap()["resource"]["attachment_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let read_only = principal_with_grants(
        "principal_00000000000000000000000001",
        project_id,
        [Grant::WorkspaceRead],
    );
    assert!(matches!(
        service.detach(
            &read_only,
            &attachment,
            &IdempotencyKey::parse("read-only-writer-detach").unwrap(),
        ),
        Err(kit::api::http::exec::ExecError::NotFound)
    ));
    let viewer = service
        .attach_viewer(
            &read_only,
            TerminalId::parse(&terminal).unwrap(),
            &IdempotencyKey::parse("read-only-viewer-attach").unwrap(),
        )
        .unwrap()["resource"]["attachment_id"]
        .as_str()
        .unwrap()
        .to_owned();
    service
        .detach(
            &read_only,
            &viewer,
            &IdempotencyKey::parse("read-only-viewer-detach").unwrap(),
        )
        .unwrap();
    let unknown_key = IdempotencyKey::parse("unknown-input-test").unwrap();
    let unknown_bytes = [0, 0xff];
    let request_value = serde_json::json!({
        "sha256_equivalent": blake3::hash(&unknown_bytes).to_hex().to_string(),
        "length": unknown_bytes.len(),
    });
    let mut digest = blake3::Hasher::new();
    digest.update(b"terminal.input\0");
    digest.update(attachment.as_bytes());
    digest.update(&[0]);
    digest.update(&serde_json::to_vec(&request_value).unwrap());
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute(
            "INSERT INTO executor_api_idempotency
             (principal_id, project_id, operation, resource, idempotency_key, digest, status, response, updated_millis)
             VALUES (?1, ?2, 'terminal.input', ?3, ?4, ?5, 'outcome_unknown', NULL, 0)",
            rusqlite::params![
                authenticated.principal_id().to_string(),
                project_id.to_string(),
                attachment,
                unknown_key.as_str(),
                digest.finalize().as_bytes().as_slice(),
            ],
        )
        .unwrap();
    service
        .resolve_input(
            &authenticated,
            &attachment,
            &unknown_key,
            kit::api::http::exec::TerminalInputResolutionBody {
                outcome: kit::api::http::exec::TerminalInputResolution::Applied,
            },
        )
        .unwrap();
    let resolved = service
        .write_input(&authenticated, &attachment, &unknown_key, &unknown_bytes)
        .unwrap();
    assert_eq!(resolved["replayed"], true);
    assert_eq!(resolved["operation"], "terminal.input");
    assert_eq!(driver.state().input_byte_count, 0);
    assert!(matches!(
        service.write_input(&authenticated, &attachment, &unknown_key, b"changed"),
        Err(kit::api::http::exec::ExecError::Conflict(_))
    ));
    service
        .write_input(
            &authenticated,
            &attachment,
            &IdempotencyKey::parse("write-input-test").unwrap(),
            CANARY,
        )
        .unwrap();
    assert_eq!(driver.state().input_byte_count, CANARY.len());

    let max_input = kit::api::http::exec::TerminalInputBody {
        bytes: vec![0xff; 16 * 1024],
    };
    let encoded = serde_json::to_vec(&max_input).unwrap();
    assert!(encoded.len() > 64 * 1024);
    let accepted: Value = serde_json::from_slice(
        &response_bytes(
            service,
            authenticated,
            request(
                Method::POST,
                &format!("/v1/terminal-attachments/{attachment}/input"),
                encoded,
            ),
            StatusCode::OK,
        )
        .await,
    )
    .unwrap();
    assert_eq!(accepted["resource"]["accepted_bytes"], 16 * 1024);
    assert_eq!(driver.state().input_byte_count, CANARY.len() + 16 * 1024);

    let mut durable = fs::read(&database).unwrap();
    let wal = database.with_extension("sqlite3-wal");
    if wal.exists() {
        durable.extend(fs::read(wal).unwrap());
    }
    let encoded_snapshots = serde_json::to_vec(&snapshots.load().unwrap()).unwrap();
    assert!(!durable.windows(CANARY.len()).any(|window| window == CANARY));
    assert!(
        !encoded_snapshots
            .windows(CANARY.len())
            .any(|window| window == CANARY)
    );
    fs::remove_dir_all(root).unwrap();
}
