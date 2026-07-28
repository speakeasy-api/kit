use std::{
    collections::BTreeSet,
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header, request::Parts},
};
use jsonschema::Validator;
use kit::{
    api::{
        auth::{
            contract::{AuthDecision, AuthDenial, Authenticator, GrantSnapshot, ScopedAuthorizer},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        http::{
            core::{ROUTES, ServiceHandler},
            errors::PROBLEM_MEDIA_TYPE,
            repo::REPO_ROUTES,
            router::{RouterConfig, authenticated_router},
        },
        service::{
            Command, CommandReceipt, Query, QueryProjection, RequestContext, RunFailureCode,
            RunFailureProjection, ServiceError, handlers,
        },
    },
    domain::{
        config::{ConfigLayer, Grant},
        events::{SchemaVersion, TraceId},
        ids::{PrincipalId, ProjectId, ThreadId},
    },
    store::artifacts::{ArtifactDigest, ArtifactStore, Reachability},
    store::sqlite::idempotency::IdempotencyKey,
    test_support::{open_service_store, service_with_runtime},
};
use serde_json::{Value, json};
use tower::ServiceExt;

const PROJECT: &str = "project_00000000000000000000000001";
const THREAD: &str = "thread_00000000000000000000000001";
const RUN: &str = "run_00000000000000000000000001";
const APPROVAL: &str = "approval_00000000000000000000000001";
const ARTIFACT: &str = "artifact_00000000000000000000000001";
const REFERENCE: &str = "blake3:0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Default)]
struct FixtureService {
    calls: AtomicUsize,
    mode: AtomicU8,
    operations: Mutex<Vec<&'static str>>,
}

impl ServiceHandler for FixtureService {
    fn execute(
        &self,
        _context: &RequestContext,
        command: Command,
    ) -> Result<CommandReceipt, ServiceError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.operations.lock().unwrap().push(command.operation());
        match self.mode.load(Ordering::Relaxed) {
            1 => Err(ServiceError::Conflict("fixture conflict".to_owned())),
            2 => Err(ServiceError::Store("fixture failure".to_owned())),
            _ => Ok(CommandReceipt {
                operation: command.operation(),
                commit_positions: Vec::new(),
                replayed: false,
            }),
        }
    }

    fn query(
        &self,
        _context: &RequestContext,
        query: Query,
    ) -> Result<QueryProjection, ServiceError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.operations.lock().unwrap().push(query.operation());
        match self.mode.load(Ordering::Relaxed) {
            2 => Err(ServiceError::Store("fixture failure".to_owned())),
            3 => {
                std::thread::sleep(Duration::from_millis(100));
                Err(ServiceError::NotFound)
            }
            4 => Err(ServiceError::Authentication(AuthDenial::Unauthorized)),
            _ => Err(ServiceError::NotFound),
        }
    }
}

fn authenticator(allow: bool) -> Arc<dyn kit::api::http::core::HttpAuthenticator> {
    let principal = PrincipalId::parse("principal_00000000000000000000000001").unwrap();
    let project = ProjectId::parse(PROJECT).unwrap();
    let peer = LocalPeerAuthenticator::new(std::collections::BTreeMap::from([(
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
    Arc::new(move |_parts: &Parts| -> AuthDecision {
        if !allow {
            return Err(AuthDenial::Unauthenticated);
        }
        peer.authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000))
    })
}

fn app(service: Arc<FixtureService>, allow: bool, timeout: Duration) -> axum::Router {
    authenticated_router(
        service,
        authenticator(allow),
        RouterConfig {
            json_body_limit: 1024,
            request_timeout: timeout,
        },
    )
}

fn request(method: Method, uri: &str, body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("idempotency-key", "contract-key")
        .body(body.into())
        .unwrap()
}

async fn problem(
    router: axum::Router,
    request: Request<Body>,
    expected: StatusCode,
    validator: &Validator,
) -> Value {
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), expected);
    assert_eq!(response.headers()[header::CONTENT_TYPE], PROBLEM_MEDIA_TYPE);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 128 * 1024).await.unwrap()).unwrap();
    assert_eq!(body["status"], expected.as_u16());
    if let Err(error) = validator.validate(&body) {
        panic!("problem response does not match OpenAPI: {error}: {body}");
    }
    body
}

fn openapi() -> Value {
    serde_yaml::from_str(include_str!("../../docs/api/openapi.yaml")).unwrap()
}

fn problem_validator(document: &Value) -> Validator {
    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "components": document["components"],
            "$ref": "#/components/schemas/ProblemDetails"
        }))
        .unwrap()
}

fn component_validator(document: &Value, schema: &str) -> Validator {
    jsonschema::draft202012::options()
        .build(&json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "components": document["components"],
            "$ref": format!("#/components/schemas/{schema}"),
        }))
        .unwrap()
}

#[test]
fn openapi_run_config_and_failure_match_service_serde() {
    let document = openapi();
    let layer: ConfigLayer = serde_json::from_value(json!({
        "schema_version": kit::domain::config::CONFIG_SCHEMA_VERSION,
        "budgets": {"max_tokens": 100, "max_cost_microusd": null, "max_turns": 2},
        "concurrency": {"max_runs": 1, "max_tools": 2},
        "retention": {"event_days": 30, "artifact_days": 7},
        "provider": "open_ai",
        "executor": "restricted_container",
        "grants": ["model_call", "workspace_read"]
    }))
    .unwrap();
    component_validator(&document, "StartRun")
        .validate(&json!({
            "message": "hello",
            "run_config": layer,
            "experiment_config": null
        }))
        .unwrap();
    component_validator(&document, "RunFailure")
        .validate(
            &serde_json::to_value(RunFailureProjection {
                code: RunFailureCode::ProviderUnavailable,
                detail: "provider is unavailable".to_owned(),
            })
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        document["components"]["schemas"]["Run"]["properties"]["failure"]["oneOf"][0]["$ref"],
        "#/components/schemas/RunFailure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_transport_error_is_rfc_9457_problem_details() {
    let schema = openapi();
    let validator = problem_validator(&schema);
    let service = Arc::new(FixtureService::default());

    problem(
        app(service.clone(), false, Duration::from_secs(1)),
        request(
            Method::GET,
            &format!("/v1/projects/{PROJECT}"),
            Body::empty(),
        ),
        StatusCode::UNAUTHORIZED,
        &validator,
    )
    .await;
    problem(
        app(service.clone(), true, Duration::from_secs(1)),
        Request::builder()
            .method(Method::POST)
            .uri("/v1/projects")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"id":"{PROJECT}"}}"#)))
            .unwrap(),
        StatusCode::BAD_REQUEST,
        &validator,
    )
    .await;
    problem(
        app(service.clone(), true, Duration::from_secs(1)),
        Request::builder()
            .method(Method::POST)
            .uri("/v1/projects")
            .header(header::CONTENT_TYPE, "text/plain")
            .header("idempotency-key", "key")
            .body(Body::from("{}"))
            .unwrap(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        &validator,
    )
    .await;
    problem(
        app(service.clone(), true, Duration::from_secs(1)),
        request(Method::POST, "/v1/projects", Body::from("{")),
        StatusCode::BAD_REQUEST,
        &validator,
    )
    .await;
    problem(
        app(service.clone(), true, Duration::from_secs(1)),
        request(Method::POST, "/v1/projects", Body::from(vec![b'x'; 1025])),
        StatusCode::PAYLOAD_TOO_LARGE,
        &validator,
    )
    .await;
    problem(
        app(service.clone(), true, Duration::from_secs(1)),
        request(Method::GET, "/v1/projects/not-an-id", Body::empty()),
        StatusCode::BAD_REQUEST,
        &validator,
    )
    .await;
    problem(
        app(service.clone(), true, Duration::from_secs(1)),
        request(Method::GET, "/v1/unknown", Body::empty()),
        StatusCode::NOT_FOUND,
        &validator,
    )
    .await;
    problem(
        app(service.clone(), true, Duration::from_secs(1)),
        request(
            Method::DELETE,
            &format!("/v1/projects/{PROJECT}"),
            Body::empty(),
        ),
        StatusCode::METHOD_NOT_ALLOWED,
        &validator,
    )
    .await;

    service.mode.store(1, Ordering::Relaxed);
    problem(
        app(service.clone(), true, Duration::from_secs(1)),
        request(
            Method::POST,
            "/v1/projects",
            Body::from(format!(r#"{{"id":"{PROJECT}"}}"#)),
        ),
        StatusCode::CONFLICT,
        &validator,
    )
    .await;
    service.mode.store(2, Ordering::Relaxed);
    problem(
        app(service.clone(), true, Duration::from_secs(1)),
        request(
            Method::POST,
            "/v1/projects",
            Body::from(format!(r#"{{"id":"{PROJECT}"}}"#)),
        ),
        StatusCode::INTERNAL_SERVER_ERROR,
        &validator,
    )
    .await;
    service.mode.store(3, Ordering::Relaxed);
    problem(
        app(service, true, Duration::from_millis(5)),
        request(
            Method::GET,
            &format!("/v1/projects/{PROJECT}"),
            Body::empty(),
        ),
        StatusCode::GATEWAY_TIMEOUT,
        &validator,
    )
    .await;
}

#[tokio::test]
async fn long_running_commands_return_an_existing_resource() {
    let service = Arc::new(FixtureService::default());
    let response = app(service.clone(), true, Duration::from_secs(1))
        .oneshot(request(
            Method::POST,
            &format!("/v1/threads/{THREAD}/runs"),
            Body::from(format!(
                r#"{{"id":"{RUN}","input":"blake3:{}"}}"#,
                "a".repeat(64)
            )),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(
        response.headers()[header::LOCATION],
        format!("/v1/runs/{RUN}")
    );
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(body["resource"]["id"], RUN);
    assert_eq!(body["receipt"]["operation"], "run.start");
    assert_eq!(service.calls.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_publishes_bytes_before_one_idempotent_run_event() {
    let root = std::env::temp_dir().join(format!(
        "kit-http-prompt-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).unwrap();
    let artifact_root = root.join("artifacts");
    let principal = PrincipalId::parse("principal_00000000000000000000000001").unwrap();
    let project_id = ProjectId::parse(PROJECT).unwrap();
    let thread_id = ThreadId::parse(THREAD).unwrap();
    let grant = GrantSnapshot::new(
        principal,
        project_id,
        [
            Grant::WorkspaceRead,
            Grant::WorkspaceWrite,
            Grant::ModelCall,
        ],
    );
    let setup_principal =
        LocalPeerAuthenticator::new(std::collections::BTreeMap::from([(1000, grant.clone())]))
            .authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000))
            .unwrap();
    let setup_context = |key: &str| {
        RequestContext::authenticated(
            Ok(setup_principal.clone()),
            Some(IdempotencyKey::parse(key).unwrap()),
            TraceId::parse(key).unwrap(),
        )
        .unwrap()
    };
    let mut service = service_with_runtime(
        open_service_store(root.join("service.sqlite")).unwrap(),
        ScopedAuthorizer,
        ArtifactStore::open(&artifact_root).unwrap(),
    );
    service
        .execute(
            &setup_context("setup-project"),
            Command::CreateProject {
                schema_version: SchemaVersion::CURRENT,
                project_id,
            },
        )
        .unwrap();
    service
        .execute(
            &setup_context("setup-thread"),
            Command::CreateThread {
                schema_version: SchemaVersion::CURRENT,
                thread_id,
                project_id,
            },
        )
        .unwrap();
    let service: Arc<dyn ServiceHandler> = Arc::new(Mutex::new(service));
    let router_config = RouterConfig {
        json_body_limit: 64 * 1024,
        request_timeout: Duration::from_secs(1),
    };
    let denied = authenticated_router(service.clone(), authenticator(false), router_config);
    let app = authenticated_router(service, authenticator(true), router_config);
    let prompt_request = |key: &str, message: &str| {
        Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/threads/{THREAD}/runs"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("idempotency-key", key)
            .body(Body::from(
                serde_json::to_vec(&json!({ "message": message })).unwrap(),
            ))
            .unwrap()
    };

    let unauthorized = denied
        .oneshot(prompt_request("unauthorized-key", "not stored"))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let artifacts = ArtifactStore::open(&artifact_root).unwrap();
    assert!(
        artifacts
            .open_bytes(ArtifactDigest::digest(b"not stored"))
            .is_err()
    );

    let first = app
        .clone()
        .oneshot(prompt_request("prompt-key", "durable message"))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first: Value =
        serde_json::from_slice(&to_bytes(first.into_body(), 4096).await.unwrap()).unwrap();
    let run_id = first["resource"]["id"].as_str().unwrap();
    assert_eq!(first["receipt"]["replayed"], false);

    let run = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/v1/runs/{run_id}"),
            Body::empty(),
        ))
        .await
        .unwrap();
    let run: Value =
        serde_json::from_slice(&to_bytes(run.into_body(), 4096).await.unwrap()).unwrap();
    let first_digest = ArtifactDigest::parse(run["input"].as_str().unwrap()).unwrap();
    assert_eq!(
        artifacts.open_bytes(first_digest).unwrap(),
        b"durable message"
    );

    let replay = app
        .clone()
        .oneshot(prompt_request("prompt-key", "durable message"))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::ACCEPTED);
    let replay: Value =
        serde_json::from_slice(&to_bytes(replay.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(replay["resource"]["id"], run_id);
    assert_eq!(replay["receipt"]["replayed"], true);

    let conflict = app
        .clone()
        .oneshot(prompt_request("prompt-key", "different message"))
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let orphan = ArtifactDigest::digest(b"different message");
    assert_eq!(artifacts.open_bytes(orphan).unwrap(), b"different message");

    let events = app
        .clone()
        .oneshot(request(
            Method::GET,
            &format!("/v1/threads/{THREAD}/events"),
            Body::empty(),
        ))
        .await
        .unwrap();
    let events: Value =
        serde_json::from_slice(&to_bytes(events.into_body(), 64 * 1024).await.unwrap()).unwrap();
    let starts = events["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["operation"] == "run.start")
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 1);
    assert!(!starts[0].to_string().contains("durable message"));
    assert!(!starts[0].to_string().contains("different message"));

    let report = artifacts
        .collect_garbage(&Reachability {
            now_unix_micros: i64::MAX,
            retained: BTreeSet::from([first_digest]),
            ..Reachability::default()
        })
        .unwrap();
    assert!(report.deleted_artifacts.contains(&orphan));
    assert_eq!(
        artifacts.open_bytes(first_digest).unwrap(),
        b"durable message"
    );

    let oversized = "x".repeat(kit::api::service::MAX_PROMPT_MESSAGE_BYTES + 1);
    let response = app
        .oneshot(prompt_request("oversized-key", &oversized))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn auth_precedes_dispatch_and_existence_is_not_leaked() {
    let schema = openapi();
    let validator = problem_validator(&schema);
    let denied_service = Arc::new(FixtureService::default());
    let denied = app(denied_service.clone(), false, Duration::from_secs(1));
    for route in ROUTES {
        let uri = route
            .path
            .replace("{project_id}", PROJECT)
            .replace("{thread_id}", THREAD)
            .replace("{run_id}", RUN)
            .replace("{approval_id}", APPROVAL)
            .replace("{artifact_id}", ARTIFACT);
        let method = Method::from_bytes(route.method.as_bytes()).unwrap();
        problem(
            denied.clone(),
            request(method, &uri, Body::from("{}")),
            StatusCode::UNAUTHORIZED,
            &validator,
        )
        .await;
    }
    assert_eq!(denied_service.calls.load(Ordering::Relaxed), 0);

    let service = Arc::new(FixtureService::default());
    let missing = problem(
        app(service.clone(), true, Duration::from_secs(1)),
        request(
            Method::GET,
            &format!("/v1/projects/{PROJECT}"),
            Body::empty(),
        ),
        StatusCode::NOT_FOUND,
        &validator,
    )
    .await;
    service.mode.store(4, Ordering::Relaxed);
    let forbidden = problem(
        app(service, true, Duration::from_secs(1)),
        request(
            Method::GET,
            &format!("/v1/projects/{PROJECT}"),
            Body::empty(),
        ),
        StatusCode::NOT_FOUND,
        &validator,
    )
    .await;
    assert_eq!(missing, forbidden);
}

#[tokio::test]
async fn parity_adapters_dispatch_the_registered_service_operation() {
    let service = Arc::new(FixtureService::default());
    let cases = [
        (
            Method::POST,
            format!("/v1/threads/{THREAD}/archive"),
            r#"{"archived":true,"expected_version":1}"#.to_owned(),
            "thread.archive",
            StatusCode::OK,
        ),
        (
            Method::POST,
            format!("/v1/threads/{THREAD}/deletion"),
            r#"{"expected_version":1}"#.to_owned(),
            "thread.delete.initiate",
            StatusCode::ACCEPTED,
        ),
        (
            Method::POST,
            format!("/v1/runs/{RUN}/input"),
            format!(r#"{{"input":"{REFERENCE}","expected_version":1}}"#),
            "run.input",
            StatusCode::OK,
        ),
        (
            Method::GET,
            format!("/v1/threads/{THREAD}/events"),
            String::new(),
            "thread.events",
            StatusCode::NOT_FOUND,
        ),
        (
            Method::GET,
            format!("/v1/projects/{PROJECT}/approvals"),
            String::new(),
            "approval.pending",
            StatusCode::NOT_FOUND,
        ),
        (
            Method::POST,
            format!("/v1/approvals/{APPROVAL}/resolve"),
            r#"{"decision":"approved","expected_version":1}"#.to_owned(),
            "approval.resolve",
            StatusCode::OK,
        ),
        (
            Method::GET,
            format!("/v1/projects/{PROJECT}/auth-requests"),
            String::new(),
            "auth.pending",
            StatusCode::NOT_FOUND,
        ),
        (
            Method::POST,
            format!("/v1/runs/{RUN}/auth/resolve"),
            r#"{"granted":true,"expected_version":1}"#.to_owned(),
            "auth.resolve",
            StatusCode::OK,
        ),
        (
            Method::POST,
            format!("/v1/projects/{PROJECT}/artifacts"),
            format!(
                r#"{{"id":"{ARTIFACT}","reference":"{REFERENCE}","media_type":"application/octet-stream","size":0}}"#
            ),
            "artifact.metadata.register",
            StatusCode::CREATED,
        ),
        (
            Method::GET,
            format!("/v1/artifacts/{ARTIFACT}"),
            String::new(),
            "artifact.metadata.get",
            StatusCode::NOT_FOUND,
        ),
        (
            Method::GET,
            format!("/v1/projects/{PROJECT}/retention"),
            String::new(),
            "project.retention.get",
            StatusCode::NOT_FOUND,
        ),
        (
            Method::POST,
            format!("/v1/projects/{PROJECT}/retention"),
            r#"{"policy":{"event":"forever","transcript":"forever","terminal":"forever","artifact":"forever","experiment":"forever","backup":"forever"},"expected_version":1}"#.to_owned(),
            "project.retention.set",
            StatusCode::OK,
        ),
    ];

    for (method, uri, body, operation, status) in cases {
        let response = app(service.clone(), true, Duration::from_secs(1))
            .oneshot(request(method, &uri, Body::from(body)))
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{operation}");
        assert_eq!(
            service.operations.lock().unwrap().last().copied(),
            Some(operation)
        );
    }
}

#[tokio::test]
async fn every_mutation_rejects_a_missing_idempotency_key_before_dispatch() {
    let schema = openapi();
    let validator = problem_validator(&schema);
    let service = Arc::new(FixtureService::default());
    for route in ROUTES.iter().filter(|route| route.mutation) {
        let uri = route
            .path
            .replace("{project_id}", PROJECT)
            .replace("{thread_id}", THREAD)
            .replace("{run_id}", RUN)
            .replace("{approval_id}", APPROVAL)
            .replace("{artifact_id}", ARTIFACT);
        let request = Request::builder()
            .method(route.method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let body = problem(
            app(service.clone(), true, Duration::from_secs(1)),
            request,
            StatusCode::BAD_REQUEST,
            &validator,
        )
        .await;
        assert_eq!(body["code"], "invalid_request", "{}", route.operation);
        assert_eq!(
            body["invalid_parameters"][0]["name"], "Idempotency-Key",
            "{}",
            route.operation
        );
    }
    assert_eq!(service.calls.load(Ordering::Relaxed), 0);
}

#[test]
fn openapi_routes_match_authenticated_router_inventory() {
    let document = openapi();
    assert!(
        document["security"]
            .as_array()
            .is_some_and(|value| !value.is_empty())
    );
    let documented = document["paths"]
        .as_object()
        .unwrap()
        .iter()
        .flat_map(|(path, item)| {
            let item = if path.contains("/repository/") {
                item["$ref"]
                    .as_str()
                    .and_then(|reference| reference.strip_prefix("#/components/pathItems/"))
                    .map_or(item, |name| &document["components"]["pathItems"][name])
            } else {
                item
            };
            ["get", "post", "put", "patch", "delete"]
                .into_iter()
                .filter_map(move |method| {
                    item.get(method)
                        .map(|_| (method.to_uppercase(), path.clone()))
                })
        })
        .collect::<BTreeSet<_>>();
    let implemented = ROUTES
        .iter()
        .chain(REPO_ROUTES)
        .map(|route| (route.method.to_owned(), route.path.to_owned()))
        .collect::<BTreeSet<_>>();
    assert_eq!(documented, implemented);
    let service_operations = handlers()
        .iter()
        .map(|handler| handler.operation)
        .collect::<BTreeSet<_>>();
    assert!(
        ROUTES
            .iter()
            .all(|route| service_operations.contains(route.operation))
    );

    for route in ROUTES.iter().filter(|route| route.mutation) {
        let operation = &document["paths"][route.path][route.method.to_ascii_lowercase()];
        let parameters = operation["parameters"].as_array().unwrap();
        assert!(parameters.iter().any(|parameter| {
            parameter["$ref"].as_str() == Some("#/components/parameters/IdempotencyKey")
        }));
        if route.long_running {
            assert!(operation["responses"].get("202").is_some());
        }
    }
}
