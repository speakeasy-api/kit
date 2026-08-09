use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    body::{Body, to_bytes},
    http::{Method, Request, StatusCode, header, request::Parts},
};
use kit::{
    api::{
        auth::{
            contract::{
                AuthDecision, AuthDenial, AuthenticatedPrincipal, Authenticator, GrantSnapshot,
            },
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        http::{
            core::{RouteDescriptor, ServiceHandler},
            repo::{REPO_ROUTES, RepoArtifact, RepoError, RepoService},
            router::{RouterConfig, authenticated_router_with_repo},
        },
        service::{Command, CommandReceipt, Query, QueryProjection, RequestContext, ServiceError},
    },
    capabilities::native::NativeTool,
    domain::{
        config::Grant,
        ids::{PrincipalId, ProjectId},
    },
    store::sqlite::idempotency::IdempotencyKey,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const PROJECT: &str = "project_00000000000000000000000001";

#[derive(Default)]
struct Core;

impl ServiceHandler for Core {
    fn execute(&self, _: &RequestContext, _: Command) -> Result<CommandReceipt, ServiceError> {
        unreachable!()
    }
    fn query(&self, _: &RequestContext, _: Query) -> Result<QueryProjection, ServiceError> {
        Err(ServiceError::NotFound)
    }
}

#[derive(Default)]
struct Repo {
    keys: Mutex<Vec<Option<String>>>,
    stale: AtomicBool,
}

impl RepoService for Repo {
    fn revision(&self, _: &AuthenticatedPrincipal, _: ProjectId) -> Result<Value, RepoError> {
        Ok(json!({"schema_version":1,"revision":format!("r:{}", "0".repeat(64))}))
    }
    fn capabilities(&self, _: &AuthenticatedPrincipal, _: ProjectId) -> Result<Value, RepoError> {
        Ok(json!({"schema_version":1,"items":[]}))
    }
    fn invoke(
        &self,
        _: &AuthenticatedPrincipal,
        _: ProjectId,
        tool: NativeTool,
        _: Value,
        key: Option<&IdempotencyKey>,
    ) -> Result<Value, RepoError> {
        if self.stale.load(Ordering::Relaxed) {
            return Err(RepoError::Stale);
        }
        self.keys
            .lock()
            .unwrap()
            .push(key.map(|key| key.as_str().to_owned()));
        Ok(
            json!({"schema_version":1,"id":"tool_call_00000000000000000000000000","operation":format!("repo.{}",tool.short_name()),"status":"completed","replayed":false,"cost":{}}),
        )
    }
    fn result(&self, _: &AuthenticatedPrincipal, _: &str) -> Result<Value, RepoError> {
        Err(RepoError::NotFound)
    }
    fn events(&self, _: &AuthenticatedPrincipal, _: &str) -> Result<Value, RepoError> {
        Err(RepoError::NotFound)
    }
    fn artifact(&self, _: &AuthenticatedPrincipal, _: &str) -> Result<RepoArtifact, RepoError> {
        Err(RepoError::NotFound)
    }
    fn resolve_approval(
        &self,
        _: &AuthenticatedPrincipal,
        _: &str,
        _: bool,
        key: &IdempotencyKey,
    ) -> Result<Value, RepoError> {
        self.keys
            .lock()
            .unwrap()
            .push(Some(key.as_str().to_owned()));
        Ok(
            json!({"schema_version":1,"id":"tool_call_00000000000000000000000000","operation":"repo.edit","status":"queued","replayed":false,"output":null,"error":null,"cost":null,"artifacts":null}),
        )
    }
    fn cancel(
        &self,
        _: &AuthenticatedPrincipal,
        _: &str,
        key: &IdempotencyKey,
    ) -> Result<Value, RepoError> {
        self.keys
            .lock()
            .unwrap()
            .push(Some(key.as_str().to_owned()));
        Ok(
            json!({"schema_version":1,"id":"tool_call_00000000000000000000000000","operation":"repo.edit","status":"queued","replayed":false,"output":null,"error":null,"cost":null,"artifacts":null}),
        )
    }
}

#[tokio::test]
async fn every_repository_route_authenticates_before_dispatch() {
    let repo = Arc::new(Repo::default());
    for route in REPO_ROUTES {
        let path = route
            .path
            .replace("{project_id}", PROJECT)
            .replace("{result_id}", "tool_call_00000000000000000000000000")
            .replace(
                "{artifact_ref}",
                &format!("artifact-ref:{}", "0".repeat(64)),
            );
        let response = app(Arc::clone(&repo), false)
            .oneshot(request(
                Method::from_bytes(route.method.as_bytes()).unwrap(),
                &path,
                route.mutation.then_some("auth-key"),
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{}",
            route.operation
        );
    }
    assert!(repo.keys.lock().unwrap().is_empty());
}

#[tokio::test]
async fn stale_revision_is_a_typed_rfc9457_conflict() {
    let repo = Arc::new(Repo::default());
    repo.stale.store(true, Ordering::Relaxed);
    let response = app(repo, true)
        .oneshot(request(
            Method::POST,
            &format!("/v1/projects/{PROJECT}/repository/search"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(body["code"], "stale_revision");
}

fn auth(allow: bool) -> Arc<dyn kit::api::http::core::HttpAuthenticator> {
    Arc::new(move |_: &Parts| -> AuthDecision {
        if !allow {
            return Err(AuthDenial::Unauthenticated);
        }
        LocalPeerAuthenticator::new(std::collections::BTreeMap::from([(
            1000,
            GrantSnapshot::new(
                PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
                ProjectId::parse(PROJECT).unwrap(),
                [
                    Grant::WorkspaceRead,
                    Grant::WorkspaceWrite,
                    Grant::ProcessSpawn,
                    Grant::VerificationTargeted,
                ],
            ),
        )]))
        .authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000))
    })
}

fn app(repo: Arc<Repo>, allow: bool) -> axum::Router {
    authenticated_router_with_repo(
        Arc::new(Core),
        auth(allow),
        RouterConfig {
            json_body_limit: 1024 * 1024,
            request_timeout: Duration::from_secs(1),
        },
        repo,
    )
}

fn request(method: Method, path: &str, key: Option<&str>) -> Request<Body> {
    json_request(method, path, key, "{}")
}

fn json_request(method: Method, path: &str, key: Option<&str>, body: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    request.body(Body::from(body.to_owned())).unwrap()
}

#[tokio::test]
async fn repository_routes_authenticate_and_mutations_require_retained_keys() {
    let repo = Arc::new(Repo::default());
    let denied = app(Arc::clone(&repo), false)
        .oneshot(request(
            Method::GET,
            &format!("/v1/projects/{PROJECT}/repository/revision"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    assert!(repo.keys.lock().unwrap().is_empty());

    let missing = app(Arc::clone(&repo), true)
        .oneshot(request(
            Method::POST,
            &format!("/v1/projects/{PROJECT}/repository/edit"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        missing.headers()[header::CONTENT_TYPE],
        "application/problem+json"
    );

    for operation in ["edit", "run"] {
        let response = app(Arc::clone(&repo), true)
            .oneshot(request(
                Method::POST,
                &format!("/v1/projects/{PROJECT}/repository/{operation}"),
                Some("retained-key"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            response.headers()[header::LOCATION],
            "/v1/repository-results/tool_call_00000000000000000000000000"
        );
    }
    for (path, body) in [
        (
            "/v1/repository-results/tool_call_00000000000000000000000000/approval",
            r#"{"decision":"approved"}"#,
        ),
        (
            "/v1/repository-results/tool_call_00000000000000000000000000/cancel",
            "{}",
        ),
    ] {
        let missing = app(Arc::clone(&repo), true)
            .oneshot(json_request(Method::POST, path, None, body))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
        let response = app(Arc::clone(&repo), true)
            .oneshot(json_request(Method::POST, path, Some("action-key"), body))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }
    assert_eq!(
        repo.keys.lock().unwrap().as_slice(),
        [
            Some("retained-key".to_owned()),
            Some("retained-key".to_owned()),
            Some("action-key".to_owned()),
            Some("action-key".to_owned())
        ]
    );
}

#[test]
fn repository_api_cli_and_openapi_are_exactly_parallel() {
    let api = REPO_ROUTES
        .iter()
        .map(|route| route.operation)
        .collect::<BTreeSet<_>>();
    let cli = kit::cli::repo::REPO_CLI_OPERATIONS
        .iter()
        .map(|operation| operation.service_operation)
        .collect::<BTreeSet<_>>();
    assert_eq!(api, cli, "{}", kit::cli::repo::parity_table());
    let document: Value =
        serde_yaml::from_str(include_str!("../../docs/api/openapi.yaml")).unwrap();
    let operation_ids = kit::cli::repo::REPO_CLI_OPERATIONS
        .iter()
        .map(|operation| operation.openapi_operation_id)
        .collect::<BTreeSet<_>>();
    let encoded = document.to_string();
    assert!(
        operation_ids
            .iter()
            .all(|operation| encoded.contains(operation))
    );
    assert_eq!(REPO_ROUTES.len(), 13);
    assert!(
        REPO_ROUTES
            .iter()
            .filter(|route| route.mutation)
            .all(|route| [
                "repo.edit",
                "repo.run",
                "repo.result.approval",
                "repo.result.cancel"
            ]
            .contains(&route.operation))
    );
}

#[tokio::test]
async fn repository_errors_are_rfc9457_and_hide_service_details() {
    let response = app(Arc::new(Repo::default()), true)
        .oneshot(request(
            Method::GET,
            "/v1/repository-results/not-an-id",
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    for field in ["type", "title", "status", "detail", "instance", "code"] {
        assert!(body.get(field).is_some(), "missing {field}");
    }
}

#[test]
fn route_descriptors_are_unique_and_long_operations_are_resources() {
    let unique = REPO_ROUTES
        .iter()
        .map(|RouteDescriptor { method, path, .. }| (*method, *path))
        .collect::<BTreeSet<_>>();
    assert_eq!(unique.len(), REPO_ROUTES.len());
    assert!(
        REPO_ROUTES
            .iter()
            .filter(|route| route.long_running)
            .all(|route| [
                "repo.discover",
                "repo.search",
                "repo.read",
                "repo.edit",
                "repo.run"
            ]
            .contains(&route.operation))
    );
}

#[test]
fn repository_cli_contract_covers_method_path_header_schema_and_status() {
    let document: Value =
        serde_yaml::from_str(include_str!("../../docs/api/openapi.yaml")).unwrap();
    for (tool, operation_id, schema) in [
        (
            NativeTool::Discover,
            "discoverRepository",
            "RepositoryDiscoverInput",
        ),
        (
            NativeTool::Search,
            "searchRepository",
            "RepositorySearchInput",
        ),
        (NativeTool::Read, "readRepository", "RepositoryReadInput"),
        (NativeTool::Edit, "editRepository", "RepositoryEditInput"),
        (
            NativeTool::Run,
            "runRepositoryCommand",
            "RepositoryRunInput",
        ),
    ] {
        let request = kit::cli::repo::RepoRequest::invoke(
            ProjectId::parse(PROJECT).unwrap(),
            tool,
            kit::cli::repo::InputSource::Stdin,
            matches!(tool, NativeTool::Edit | NativeTool::Run)
                .then(|| IdempotencyKey::parse("contract-key").unwrap()),
        );
        assert_eq!(request.method, Method::POST);
        assert_eq!(
            request.path,
            format!("/v1/projects/{PROJECT}/repository/{}", tool.short_name())
        );
        let contract = &document["components"]["pathItems"][format!("Repository{}", {
            let mut name = tool.short_name().to_owned();
            name[0..1].make_ascii_uppercase();
            name
        })];
        let path_item = &contract["post"];
        assert_eq!(path_item["operationId"], operation_id);
        assert!(path_item["responses"].get("202").is_some());
        assert_eq!(
            path_item["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{schema}")
        );
        assert!(document["components"]["schemas"].get(schema).is_some());
        let parameters = contract["parameters"].as_array().unwrap();
        assert_eq!(parameters[0]["$ref"], "#/components/parameters/ProjectId");
        if matches!(tool, NativeTool::Edit | NativeTool::Run) {
            assert!(
                parameters
                    .iter()
                    .any(|parameter| parameter["$ref"] == "#/components/parameters/IdempotencyKey")
            );
            assert!(request.idempotency_key.is_some());
        }
    }
    for (path, operation_id, schema, request) in [
        (
            "/v1/repository-results/{result_id}/approval",
            "resolveRepositoryApproval",
            "RepositoryApprovalResolution",
            kit::cli::repo::RepoRequest::approval(
                "tool_call_00000000000000000000000000",
                true,
                IdempotencyKey::parse("approval-contract").unwrap(),
            ),
        ),
        (
            "/v1/repository-results/{result_id}/cancel",
            "cancelRepositoryOperation",
            "RepositoryCancellation",
            kit::cli::repo::RepoRequest::cancel(
                "tool_call_00000000000000000000000000",
                IdempotencyKey::parse("cancel-contract").unwrap(),
            ),
        ),
    ] {
        let operation = &document["paths"][path]["post"];
        assert_eq!(operation["operationId"], operation_id);
        assert_eq!(
            operation["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{schema}")
        );
        assert!(
            operation["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .any(|parameter| parameter["$ref"] == "#/components/parameters/IdempotencyKey")
        );
        assert!(request.idempotency_key.is_some());
    }
}

#[test]
fn openapi_embeds_the_exact_native_catalog_input_schemas() {
    let document: Value =
        serde_yaml::from_str(include_str!("../../docs/api/openapi.yaml")).unwrap();
    for (tool, schema) in [
        (NativeTool::Discover, "RepositoryDiscoverInput"),
        (NativeTool::Search, "RepositorySearchInput"),
        (NativeTool::Read, "RepositoryReadInput"),
        (NativeTool::Edit, "RepositoryEditInput"),
        (NativeTool::Run, "RepositoryRunInput"),
    ] {
        let embedded = resolve_schema_refs(&document["components"]["schemas"][schema], &document);
        let catalog = kit::capabilities::native::NativeCatalog::all()
            .iter()
            .find(|descriptor| descriptor.tool() == tool)
            .unwrap();
        assert_eq!(embedded, catalog.spec().input_schema, "{schema}");
    }
}

fn resolve_schema_refs(value: &Value, document: &Value) -> Value {
    match value {
        Value::Object(object) if object.len() == 1 && object.contains_key("$ref") => {
            let reference = object["$ref"].as_str().unwrap();
            let name = reference
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("unsupported schema reference {reference}"));
            resolve_schema_refs(&document["components"]["schemas"][name], document)
        }
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), resolve_schema_refs(value, document)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| resolve_schema_refs(value, document))
                .collect(),
        ),
        value => value.clone(),
    }
}
