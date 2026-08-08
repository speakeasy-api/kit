use std::{
    convert::Infallible,
    pin::Pin,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::{Body, Bytes, to_bytes},
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::{Stream, wrappers::ReceiverStream};

use crate::{
    api::{
        auth::contract::{AuthDenial, AuthenticatedPrincipal},
        service::{
            Command, EventCursor, MAX_EVENT_PAGE_BYTES, PromptCommand, PromptInput, PromptReceipt,
            Query, QueryProjection, RequestContext, ServiceError,
        },
        stream::{
            EventFilter, OpaqueStreamCursor, SSE_MEDIA_TYPE, SqliteStreamAdapter,
            StreamCancellation, StreamRejection,
        },
    },
    domain::{
        events::{ApprovalDecision, ArtifactRef, SchemaVersion, TraceId},
        ids::{ApprovalId, ArtifactId, McpCallbackId, ProjectId, RunId, TerminalId, ThreadId},
        mcp_callback::McpCallbackAction,
    },
    store::sqlite::idempotency::IdempotencyKey,
};

use super::{
    core::{
        DEFAULT_TIMEOUT_SECONDS, HttpAuthenticator, JSON_BODY_LIMIT, MAX_PAGE_SIZE, ServiceHandler,
        decode_cursor, encode_cursor,
    },
    errors::{ProblemDetails, problem_type},
    health::{self, HealthState},
    retention,
};

#[derive(Clone, Copy, Debug)]
pub struct RouterConfig {
    pub json_body_limit: usize,
    pub request_timeout: Duration,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            json_body_limit: JSON_BODY_LIMIT,
            request_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
        }
    }
}

#[derive(Clone)]
struct ApiState {
    service: Arc<dyn ServiceHandler>,
    authenticator: Arc<dyn HttpAuthenticator>,
    stream: Option<SqliteStreamAdapter>,
    stream_cancellation: StreamCancellation,
    config: RouterConfig,
}

struct ReceiptService(Arc<dyn ServiceHandler>);

impl ServiceHandler for ReceiptService {
    fn execute(
        &self,
        context: &RequestContext,
        command: Command,
    ) -> Result<crate::api::service::CommandReceipt, ServiceError> {
        self.0.execute(context, command)
    }

    fn query(
        &self,
        context: &RequestContext,
        query: Query,
    ) -> Result<QueryProjection, ServiceError> {
        self.0.query(context, query)
    }

    fn prompt(
        &self,
        context: &RequestContext,
        request: PromptCommand,
    ) -> Result<PromptReceipt, ServiceError> {
        self.0.prompt(context, request)
    }
}

pub fn authenticated_router(
    service: Arc<dyn ServiceHandler>,
    authenticator: Arc<dyn HttpAuthenticator>,
    config: RouterConfig,
) -> Router {
    build_router(
        service,
        authenticator,
        config,
        None,
        StreamCancellation::new(),
    )
}

pub fn authenticated_router_with_exec(
    service: Arc<dyn ServiceHandler>,
    authenticator: Arc<dyn HttpAuthenticator>,
    config: RouterConfig,
    exec: Arc<dyn super::exec::ExecService>,
) -> Router {
    build_router_with_exec(
        service,
        authenticator,
        config,
        None,
        StreamCancellation::new(),
        Some(exec),
        None,
    )
}

pub fn authenticated_router_with_repo(
    service: Arc<dyn ServiceHandler>,
    authenticator: Arc<dyn HttpAuthenticator>,
    config: RouterConfig,
    repo: Arc<dyn super::repo::RepoService>,
) -> Router {
    build_router_with_exec(
        service,
        authenticator,
        config,
        None,
        StreamCancellation::new(),
        None,
        Some(repo),
    )
}

pub fn authenticated_router_with_stream(
    service: Arc<dyn ServiceHandler>,
    authenticator: Arc<dyn HttpAuthenticator>,
    config: RouterConfig,
    stream: SqliteStreamAdapter,
) -> Router {
    authenticated_router_with_stream_cancellation(
        service,
        authenticator,
        config,
        stream,
        StreamCancellation::new(),
    )
}

pub fn authenticated_router_with_stream_cancellation(
    service: Arc<dyn ServiceHandler>,
    authenticator: Arc<dyn HttpAuthenticator>,
    config: RouterConfig,
    stream: SqliteStreamAdapter,
    stream_cancellation: StreamCancellation,
) -> Router {
    build_router(
        service,
        authenticator,
        config,
        Some(stream),
        stream_cancellation,
    )
}

pub fn daemon_router(
    service: Arc<dyn ServiceHandler>,
    authenticator: Arc<dyn HttpAuthenticator>,
    config: RouterConfig,
    stream: SqliteStreamAdapter,
    health: HealthState,
    stream_cancellation: StreamCancellation,
) -> Router {
    daemon_router_with_exec(
        service,
        authenticator,
        config,
        stream,
        health,
        stream_cancellation,
        None,
    )
}

pub fn daemon_router_with_exec(
    service: Arc<dyn ServiceHandler>,
    authenticator: Arc<dyn HttpAuthenticator>,
    config: RouterConfig,
    stream: SqliteStreamAdapter,
    health: HealthState,
    stream_cancellation: StreamCancellation,
    exec: Option<Arc<dyn super::exec::ExecService>>,
) -> Router {
    daemon_router_with_services(
        service,
        authenticator,
        config,
        stream,
        health,
        stream_cancellation,
        exec,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn daemon_router_with_services(
    service: Arc<dyn ServiceHandler>,
    authenticator: Arc<dyn HttpAuthenticator>,
    config: RouterConfig,
    stream: SqliteStreamAdapter,
    health: HealthState,
    stream_cancellation: StreamCancellation,
    exec: Option<Arc<dyn super::exec::ExecService>>,
    repo: Option<Arc<dyn super::repo::RepoService>>,
) -> Router {
    build_router_with_exec(
        service,
        authenticator,
        config,
        Some(stream),
        stream_cancellation,
        exec,
        repo,
    )
    .layer(middleware::from_fn_with_state(
        health.clone(),
        require_readiness,
    ))
    .merge(health::routes(health))
}

fn build_router(
    service: Arc<dyn ServiceHandler>,
    authenticator: Arc<dyn HttpAuthenticator>,
    config: RouterConfig,
    stream: Option<SqliteStreamAdapter>,
    stream_cancellation: StreamCancellation,
) -> Router {
    build_router_with_exec(
        service,
        authenticator,
        config,
        stream,
        stream_cancellation,
        None,
        None,
    )
}

fn build_router_with_exec(
    service: Arc<dyn ServiceHandler>,
    authenticator: Arc<dyn HttpAuthenticator>,
    config: RouterConfig,
    stream: Option<SqliteStreamAdapter>,
    stream_cancellation: StreamCancellation,
    exec: Option<Arc<dyn super::exec::ExecService>>,
    repo: Option<Arc<dyn super::repo::RepoService>>,
) -> Router {
    let state = ApiState {
        service,
        authenticator,
        stream,
        stream_cancellation,
        config,
    };

    let retention_service: Arc<dyn ServiceHandler> =
        Arc::new(ReceiptService(state.service.clone()));

    let mut router = Router::new()
        .route("/v1/projects", post(create_project))
        .route("/v1/projects/{project_id}", get(get_project))
        .route(
            "/v1/projects/{project_id}/threads",
            get(list_threads).post(create_thread),
        )
        .route("/v1/threads/{thread_id}", get(get_thread))
        .route("/v1/threads/{thread_id}/runs", post(start_run))
        .route("/v1/threads/{thread_id}/events", get(thread_events))
        .route("/v1/projects/{project_id}/runs", get(list_runs))
        .route("/v1/runs/{run_id}", get(get_run))
        .route("/v1/runs/{run_id}/cost", get(get_run_cost))
        .route("/v1/runs/{run_id}/prompts", get(get_run_prompts))
        .route("/v1/runs/{run_id}/transcript", get(get_run_transcript))
        .route("/v1/runs/{run_id}/cancel", post(cancel_run))
        .route("/v1/runs/{run_id}/input", post(provide_run_input))
        .route("/v1/runs/{run_id}/auth/resolve", post(resolve_auth))
        .route("/v1/runs/{run_id}/events", get(run_events))
        .route(
            "/v1/projects/{project_id}/approvals",
            get(list_pending_approvals),
        )
        .route(
            "/v1/approvals/{approval_id}/resolve",
            post(resolve_approval),
        )
        .route(
            "/v1/projects/{project_id}/auth-requests",
            get(list_pending_auth_requests),
        )
        .route(
            "/v1/projects/{project_id}/mcp-callbacks",
            get(list_pending_mcp_callbacks),
        )
        .route("/v1/mcp-callbacks/{callback_id}", get(get_mcp_callback))
        .route(
            "/v1/mcp-callbacks/{callback_id}/resolve",
            post(resolve_mcp_callback),
        )
        .route(
            "/v1/projects/{project_id}/artifacts",
            post(register_artifact_metadata),
        )
        .route("/v1/artifacts/{artifact_id}", get(get_artifact_metadata))
        .route(
            "/v1/projects/{project_id}/capabilities",
            get(list_capabilities),
        )
        .route(
            "/v1/projects/{project_id}/events/status",
            get(cursor_status),
        )
        .route(
            "/v1/projects/{project_id}/events/stream",
            get(project_event_stream),
        )
        .route("/v1/terminals/{terminal_id}/attach", get(terminal_attach))
        .route("/v1/projects/{project_id}/status", get(project_status))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state.clone())
        .merge(retention::routes(retention_service));
    if let Some(exec) = exec {
        router = router.merge(super::exec::routes(exec));
    }
    if let Some(repo) = repo {
        router = router.merge(super::repo::routes(repo));
    }
    router
        .layer(middleware::from_fn_with_state(
            state.clone(),
            request_timeout,
        ))
        .layer(middleware::from_fn_with_state(state, authenticate))
}

async fn project_event_stream(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let project_id = match parse_id(&project_id, "project_id", &instance, ProjectId::from_str) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    event_stream(state, request, StreamResource::Project(project_id)).await
}

#[derive(Clone, Copy)]
enum StreamResource {
    Project(ProjectId),
    Thread(ThreadId),
    Run(RunId),
}

async fn event_stream(state: ApiState, request: Request, resource: StreamResource) -> Response {
    let instance = request.uri().to_string();
    let context = match query_context(&request) {
        Ok(context) => context,
        Err(problem) => return problem.into_response(),
    };
    let cursor = match stream_cursor(&request, &instance) {
        Ok(cursor) => cursor,
        Err(problem) => return problem.into_response(),
    };
    let Some(stream) = state.stream.clone() else {
        return unavailable(
            &instance,
            "Event stream unavailable",
            "The event stream service is not available.",
            "stream_unavailable",
        );
    };

    let (project_id, filter) = match resource {
        StreamResource::Project(project_id) => (project_id, EventFilter::project()),
        StreamResource::Thread(thread_id) => {
            match query(&state, context.clone(), Query::GetThread { thread_id }).await {
                Ok(QueryProjection::Thread(thread)) => {
                    (thread.project_id, EventFilter::thread(thread_id))
                }
                Ok(_) => return ProblemDetails::internal(instance).into_response(),
                Err(error) => return ProblemDetails::service(error, instance).into_response(),
            }
        }
        StreamResource::Run(run_id) => {
            let thread_id = match query(&state, context.clone(), Query::GetRun { run_id }).await {
                Ok(QueryProjection::Run(run)) => run.thread_id,
                Ok(_) => return ProblemDetails::internal(instance).into_response(),
                Err(error) => return ProblemDetails::service(error, instance).into_response(),
            };
            match query(&state, context.clone(), Query::GetThread { thread_id }).await {
                Ok(QueryProjection::Thread(thread)) => {
                    (thread.project_id, EventFilter::run(run_id))
                }
                Ok(_) => return ProblemDetails::internal(instance).into_response(),
                Err(error) => return ProblemDetails::service(error, instance).into_response(),
            }
        }
    };

    let result = tokio::task::spawn_blocking(move || {
        stream.open(&context, project_id, filter, cursor.as_ref())
    })
    .await;

    match result {
        Ok(Ok(connection)) => {
            let cursor = connection.last_durable_cursor();
            let (sender, receiver) = mpsc::channel(connection.capacity());
            let Some(registration) = state.stream_cancellation.register() else {
                return unavailable(
                    &instance,
                    "Event stream unavailable",
                    "The event stream service is shutting down.",
                    "stream_unavailable",
                );
            };
            let local_cancellation = Arc::new(AtomicBool::new(false));
            let producer_cancellation = local_cancellation.clone();
            let stream_cancellation = state.stream_cancellation.clone();
            let producer = match std::thread::Builder::new()
                .name("kit-sse-producer".to_owned())
                .spawn(move || {
                    let _registration = registration;
                    produce_stream(
                        connection,
                        sender,
                        &stream_cancellation,
                        &producer_cancellation,
                    );
                }) {
                Ok(producer) => producer,
                Err(_) => return ProblemDetails::internal(instance).into_response(),
            };
            let body = SseBodyStream {
                receiver: ReceiverStream::new(receiver),
                producer: Some(producer),
                cancellation: local_cancellation,
            };
            let mut response = Body::from_stream(body).into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(SSE_MEDIA_TYPE),
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache, no-store"),
            );
            if let Ok(cursor) = HeaderValue::from_str(cursor.as_str()) {
                response.headers_mut().insert("x-kit-cursor", cursor);
            }
            response
        }
        Ok(Err(rejection)) => stream_rejection(rejection),
        Err(_) => ProblemDetails::internal(instance).into_response(),
    }
}

fn produce_stream(
    mut connection: crate::api::stream::SseConnection,
    sender: mpsc::Sender<Result<Bytes, Infallible>>,
    shutdown: &StreamCancellation,
    local_cancellation: &AtomicBool,
) {
    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
    const BACKPRESSURE_LIMIT: Duration = Duration::from_millis(500);

    let mut last_heartbeat = Instant::now();
    let mut blocked_since = None;
    loop {
        if shutdown.is_cancelled()
            || local_cancellation.load(Ordering::Acquire)
            || sender.is_closed()
        {
            return;
        }
        if sender.capacity() == 0 {
            let blocked = blocked_since.get_or_insert_with(Instant::now);
            if blocked.elapsed() >= BACKPRESSURE_LIMIT {
                return;
            }
            std::thread::sleep(POLL_INTERVAL);
            continue;
        }
        blocked_since = None;

        match connection.pump() {
            Ok(_) => {}
            Err(_) => {
                let frame = crate::api::stream::SseFrame::Disconnect {
                    cursor: connection.last_durable_cursor(),
                    reason: "stream_error",
                };
                let _ = sender.try_send(Ok(Bytes::from(frame.encode())));
                return;
            }
        }
        if shutdown.is_cancelled() || local_cancellation.load(Ordering::Acquire) {
            return;
        }
        while sender.capacity() > 0
            && let Some(frame) = connection.next_frame()
        {
            if sender.try_send(Ok(Bytes::from(frame.encode()))).is_err() {
                return;
            }
        }
        if connection.is_disconnected() {
            return;
        }
        if connection.queued_len() == 0 && last_heartbeat.elapsed() >= HEARTBEAT_INTERVAL {
            if sender
                .try_send(Ok(Bytes::from(connection.heartbeat().encode())))
                .is_err()
            {
                return;
            }
            last_heartbeat = Instant::now();
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

struct SseBodyStream {
    receiver: ReceiverStream<Result<Bytes, Infallible>>,
    producer: Option<JoinHandle<()>>,
    cancellation: Arc<AtomicBool>,
}

impl Stream for SseBodyStream {
    type Item = Result<Bytes, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = Pin::new(&mut self.receiver).poll_next(context);
        if result.is_ready() && matches!(result, Poll::Ready(None)) {
            self.join_producer();
        }
        result
    }
}

impl SseBodyStream {
    fn join_producer(&mut self) {
        if let Some(producer) = self.producer.take() {
            let _ = producer.join();
        }
    }
}

impl Drop for SseBodyStream {
    fn drop(&mut self) {
        self.cancellation.store(true, Ordering::Release);
        self.join_producer();
    }
}

async fn terminal_attach(
    State(state): State<ApiState>,
    Path(terminal_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let terminal_id = match parse_id(&terminal_id, "terminal_id", &instance, TerminalId::from_str) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let context = match query_context(&request) {
        Ok(context) => context,
        Err(problem) => return problem.into_response(),
    };
    let project_id = context.grant().project_id();
    let Some(stream) = state.stream else {
        return unavailable(
            &instance,
            "Terminal attachment unavailable",
            "The terminal owner service is not available.",
            "terminal_unavailable",
        );
    };

    match tokio::task::spawn_blocking(move || {
        stream.reserve_terminal_websocket(&context, project_id, terminal_id)
    })
    .await
    {
        Ok(Ok(())) => unavailable(
            &instance,
            "Terminal attachment unavailable",
            "The terminal owner service is not available.",
            "terminal_unavailable",
        ),
        Ok(Err(rejection)) => stream_rejection(rejection),
        Err(_) => ProblemDetails::internal(instance).into_response(),
    }
}

async fn authenticate(State(state): State<ApiState>, request: Request, next: Next) -> Response {
    let instance = request.uri().to_string();
    let (parts, body) = request.into_parts();
    match state.authenticator.authenticate(&parts) {
        Ok(principal) => {
            let mut request = Request::from_parts(parts, body);
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(AuthDenial::Unauthenticated | AuthDenial::Unauthorized) => {
            ProblemDetails::unauthenticated(instance).into_response()
        }
    }
}

async fn require_readiness(
    State(health): State<HealthState>,
    request: Request,
    next: Next,
) -> Response {
    if health.is_ready() {
        next.run(request).await
    } else {
        unavailable(
            &request.uri().to_string(),
            "Service unavailable",
            "The daemon is not ready to accept requests.",
            "not_ready",
        )
    }
}

async fn request_timeout(State(state): State<ApiState>, request: Request, next: Next) -> Response {
    let instance = request.uri().to_string();
    match tokio::time::timeout(state.config.request_timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => ProblemDetails::timeout(instance).into_response(),
    }
}

#[derive(Deserialize)]
struct CreateProjectBody {
    id: ProjectId,
}

async fn create_project(State(state): State<ApiState>, request: Request) -> Response {
    let instance = request.uri().to_string();
    let (body, context) = match command_body::<CreateProjectBody>(request, state.config).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    let id = body.id.to_string();
    match execute(
        &state,
        context,
        Command::CreateProject {
            schema_version: SchemaVersion::CURRENT,
            project_id: body.id,
        },
    )
    .await
    {
        Ok(receipt) => resource_response(
            StatusCode::CREATED,
            &format!("/v1/projects/{id}"),
            id,
            receipt,
        ),
        Err(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

async fn get_project(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &project_id,
        "project_id",
        ProjectId::from_str,
        |project_id| Query::GetProject { project_id },
        |projection| match projection {
            QueryProjection::Project(value) => Some(json!(value)),
            _ => None,
        },
    )
    .await
}

#[derive(Deserialize)]
struct CreateThreadBody {
    id: ThreadId,
}

async fn create_thread(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let project_id = match parse_id(&project_id, "project_id", &instance, ProjectId::from_str) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let (body, context) = match command_body::<CreateThreadBody>(request, state.config).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    let id = body.id.to_string();
    match execute(
        &state,
        context,
        Command::CreateThread {
            schema_version: SchemaVersion::CURRENT,
            thread_id: body.id,
            project_id,
        },
    )
    .await
    {
        Ok(receipt) => resource_response(
            StatusCode::CREATED,
            &format!("/v1/threads/{id}"),
            id,
            receipt,
        ),
        Err(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

async fn list_threads(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &project_id,
        "project_id",
        ProjectId::from_str,
        |project_id| Query::ListThreads { project_id },
        |projection| match projection {
            QueryProjection::Threads(value) => Some(json!({ "items": value })),
            _ => None,
        },
    )
    .await
}

async fn get_thread(
    State(state): State<ApiState>,
    Path(thread_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &thread_id,
        "thread_id",
        ThreadId::from_str,
        |thread_id| Query::GetThread { thread_id },
        |projection| match projection {
            QueryProjection::Thread(value) => Some(json!(value)),
            _ => None,
        },
    )
    .await
}

#[derive(Deserialize)]
struct StartRunBody {
    #[serde(default, alias = "id")]
    run_id: Option<RunId>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default, alias = "input")]
    artifact_ref: Option<ArtifactRef>,
    #[serde(default)]
    run_config: Option<crate::domain::config::ConfigLayer>,
    #[serde(default)]
    experiment_config: Option<crate::domain::config::ConfigLayer>,
}

async fn start_run(
    State(state): State<ApiState>,
    Path(thread_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let thread_id = match parse_id(&thread_id, "thread_id", &instance, ThreadId::from_str) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let (body, context) = match command_body::<StartRunBody>(request, state.config).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    let input = match (body.message, body.artifact_ref) {
        (Some(message), None) => PromptInput::Message(message),
        (None, Some(reference)) => PromptInput::Artifact(reference),
        _ => {
            return ProblemDetails::invalid(
                instance,
                "body",
                "exactly one of message or artifact_ref is required",
            )
            .into_response();
        }
    };
    match prompt(
        &state,
        context,
        PromptCommand {
            thread_id,
            run_id: body.run_id,
            input,
            run_config: body.run_config,
            experiment_config: body.experiment_config,
        },
    )
    .await
    {
        Ok(result) => {
            let id = result.run_id.to_string();
            resource_response(
                StatusCode::ACCEPTED,
                &format!("/v1/runs/{id}"),
                id,
                result.receipt,
            )
        }
        Err(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

async fn list_runs(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &project_id,
        "project_id",
        ProjectId::from_str,
        |project_id| Query::ListRuns { project_id },
        |projection| match projection {
            QueryProjection::Runs(value) => Some(json!({ "items": value })),
            _ => None,
        },
    )
    .await
}

async fn get_run(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &run_id,
        "run_id",
        RunId::from_str,
        |run_id| Query::GetRun { run_id },
        |projection| match projection {
            QueryProjection::Run(value) => Some(json!(value)),
            _ => None,
        },
    )
    .await
}

async fn get_run_cost(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &run_id,
        "run_id",
        RunId::from_str,
        |run_id| Query::GetRunCost { run_id },
        |projection| match projection {
            QueryProjection::RunCost(value) => Some(json!(value)),
            _ => None,
        },
    )
    .await
}

async fn get_run_prompts(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &run_id,
        "run_id",
        RunId::from_str,
        |run_id| Query::GetRunPrompts { run_id },
        |projection| match projection {
            QueryProjection::RunPrompts(value) => Some(json!(value)),
            _ => None,
        },
    )
    .await
}

async fn get_run_transcript(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &run_id,
        "run_id",
        RunId::from_str,
        |run_id| Query::RunTranscript { run_id },
        |projection| match projection {
            QueryProjection::RunTranscript(value) => Some(json!(value)),
            _ => None,
        },
    )
    .await
}

#[derive(Deserialize)]
struct ExpectedVersionBody {
    expected_version: u64,
}

async fn cancel_run(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let run_id = match parse_id(&run_id, "run_id", &instance, RunId::from_str) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let (body, context) = match command_body::<ExpectedVersionBody>(request, state.config).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    let id = run_id.to_string();
    match execute(
        &state,
        context,
        Command::CancelRun {
            schema_version: SchemaVersion::CURRENT,
            run_id,
            expected_version: body.expected_version,
        },
    )
    .await
    {
        Ok(receipt) => {
            resource_response(StatusCode::ACCEPTED, &format!("/v1/runs/{id}"), id, receipt)
        }
        Err(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

#[derive(Deserialize)]
struct ProvideRunInputBody {
    input: ArtifactRef,
    expected_version: u64,
}

async fn provide_run_input(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let run_id = match parse_id(&run_id, "run_id", &instance, RunId::from_str) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let (body, context) = match command_body::<ProvideRunInputBody>(request, state.config).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    let id = run_id.to_string();
    match execute(
        &state,
        context,
        Command::ProvideRunInput {
            schema_version: SchemaVersion::CURRENT,
            run_id,
            input: body.input,
            expected_version: body.expected_version,
        },
    )
    .await
    {
        Ok(receipt) => resource_response(StatusCode::OK, &format!("/v1/runs/{id}"), id, receipt),
        Err(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

async fn run_events(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let run_id = match parse_id(&run_id, "run_id", &instance, RunId::from_str) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    if accepts_sse(request.headers()) {
        return event_stream(state, request, StreamResource::Run(run_id)).await;
    }
    event_page(
        &state,
        request,
        EventFilter::run(run_id),
        |after, limit, projection_state| match projection_state {
            Some(projection_state) => Query::RunTimelineProjected {
                run_id,
                after,
                limit,
                projection_state,
            },
            None => Query::RunTimeline {
                run_id,
                after,
                limit,
                opaque_cursor: None,
            },
        },
    )
    .await
}

async fn thread_events(
    State(state): State<ApiState>,
    Path(thread_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let thread_id = match parse_id(&thread_id, "thread_id", &instance, ThreadId::from_str) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    if accepts_sse(request.headers()) {
        return event_stream(state, request, StreamResource::Thread(thread_id)).await;
    }
    event_page(
        &state,
        request,
        EventFilter::thread(thread_id),
        |after, limit, projection_state| match projection_state {
            Some(projection_state) => Query::ThreadEventsProjected {
                thread_id,
                after,
                limit,
                projection_state,
            },
            None => Query::ThreadEvents {
                thread_id,
                after,
                limit,
                opaque_cursor: None,
            },
        },
    )
    .await
}

fn accepts_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|media_type| {
                media_type.split(';').next().map(str::trim) == Some(SSE_MEDIA_TYPE)
            })
        })
}

async fn event_page(
    state: &ApiState,
    request: Request,
    filter: EventFilter,
    build_query: impl FnOnce(
        EventCursor,
        usize,
        Option<crate::domain::secret::JsonProjectionState>,
    ) -> Query,
) -> Response {
    let instance = request.uri().to_string();
    let limit = match page_limit(request.uri(), &instance) {
        Ok(limit) => limit,
        Err(problem) => return problem.into_response(),
    };
    let context = match query_context(&request) {
        Ok(context) => context,
        Err(problem) => return problem.into_response(),
    };
    let (after, projection_state) = match query_value(request.uri(), "cursor") {
        None => (
            EventCursor::START,
            state
                .stream
                .as_ref()
                .filter(|stream| !stream.accepts_legacy_cursor())
                .map(SqliteStreamAdapter::projection_state),
        ),
        Some(value) if value.starts_with("kitc") => {
            let Some(stream) = &state.stream else {
                return ProblemDetails::invalid(
                    instance,
                    "cursor",
                    "Cursor must be an opaque cursor returned by this API.",
                )
                .into_response();
            };
            let cursor = match OpaqueStreamCursor::parse(value.to_owned()) {
                Ok(cursor) => cursor,
                Err(_) => {
                    return ProblemDetails::invalid(
                        instance,
                        "cursor",
                        "Cursor must be an opaque cursor returned by this API.",
                    )
                    .into_response();
                }
            };
            match stream.decode_page_cursor(
                &context,
                context.grant().project_id(),
                &filter,
                &cursor,
            ) {
                Ok((position, projection_state)) => {
                    (EventCursor::new(position), Some(projection_state))
                }
                Err(rejection) if rejection.requires_cursor_upgrade() => {
                    return ProblemDetails::cursor_upgrade_required(instance).into_response();
                }
                Err(_) => {
                    return ProblemDetails::invalid(
                        instance,
                        "cursor",
                        "Cursor must be an opaque cursor returned by this API.",
                    )
                    .into_response();
                }
            }
        }
        Some(value) => match decode_cursor(value) {
            Some(0) if state.stream.is_some() => (
                EventCursor::START,
                state
                    .stream
                    .as_ref()
                    .filter(|stream| !stream.accepts_legacy_cursor())
                    .map(SqliteStreamAdapter::projection_state),
            ),
            Some(position)
                if state
                    .stream
                    .as_ref()
                    .is_none_or(SqliteStreamAdapter::accepts_legacy_cursor) =>
            {
                (EventCursor::new(position), None)
            }
            Some(_) => {
                return ProblemDetails::cursor_upgrade_required(instance).into_response();
            }
            None => {
                return ProblemDetails::invalid(
                    instance,
                    "cursor",
                    "Cursor must be an opaque cursor returned by this API.",
                )
                .into_response();
            }
        },
    };
    match query(
        state,
        context.clone(),
        build_query(after, limit, projection_state),
    )
    .await
    {
        Ok(projection) => {
            let (page, projection_state, item_projection_states) = match projection {
                QueryProjection::Events(page) => (page, None, None),
                QueryProjection::ProjectedEvents(page) => (
                    page.page,
                    Some(page.projection_state),
                    Some(page.item_projection_states),
                ),
                _ => return ProblemDetails::internal(instance).into_response(),
            };
            #[derive(serde::Serialize)]
            struct EventResponse {
                cursor: String,
                project_id: crate::domain::ids::ProjectId,
                operation: String,
                stream: String,
                payload: Box<serde_json::value::RawValue>,
                authority_digest: String,
                projection_digest: String,
                projected_envelope: String,
            }

            #[derive(serde::Serialize)]
            struct EventPageResponse<'a> {
                items: &'a [EventResponse],
                next_cursor: &'a str,
                truncated: bool,
            }

            if item_projection_states
                .as_ref()
                .is_some_and(|states| states.len() != page.events.len())
            {
                return ProblemDetails::internal(instance).into_response();
            }
            let source_len = page.events.len();
            let source_truncated = page.truncated;
            let mut events = Vec::with_capacity(source_len);
            let mut next_cursor = None;
            for (index, event) in page.events.into_iter().enumerate() {
                let payload = match serde_json::from_slice(&event.payload) {
                    Ok(payload) => payload,
                    Err(_) => return ProblemDetails::internal(instance).into_response(),
                };
                let projected_envelope = match String::from_utf8(event.envelope) {
                    Ok(envelope) => envelope,
                    Err(_) => return ProblemDetails::internal(instance).into_response(),
                };
                let cursor = if let (Some(stream), Some(states)) =
                    (&state.stream, item_projection_states.as_ref())
                {
                    match stream.encode_page_cursor(
                        &context,
                        context.grant().project_id(),
                        &filter,
                        event.cursor.position(),
                        &states[index],
                    ) {
                        Ok(cursor) => cursor.to_string(),
                        Err(_) => return ProblemDetails::internal(instance).into_response(),
                    }
                } else {
                    encode_cursor(event.cursor.position())
                };
                events.push(EventResponse {
                    cursor: cursor.clone(),
                    project_id: event.project_id,
                    operation: event.operation,
                    stream: event.stream,
                    payload,
                    authority_digest: event.authority_digest,
                    projection_digest: event.projection_digest,
                    projected_envelope,
                });
                let truncated = source_truncated || events.len() < source_len;
                let candidate = EventPageResponse {
                    items: &events,
                    next_cursor: &cursor,
                    truncated,
                };
                if !serialized_within(&candidate, MAX_EVENT_PAGE_BYTES) {
                    events.pop();
                    break;
                }
                next_cursor = Some(cursor);
            }
            let truncated = source_truncated || events.len() < source_len;
            let next_cursor = match next_cursor {
                Some(cursor) => cursor,
                None if source_len != 0 => {
                    return ProblemDetails::internal(instance).into_response();
                }
                None => {
                    if let (Some(stream), Some(projection_state)) =
                        (&state.stream, projection_state.as_ref())
                    {
                        match stream.encode_page_cursor(
                            &context,
                            context.grant().project_id(),
                            &filter,
                            page.next_cursor.position(),
                            projection_state,
                        ) {
                            Ok(cursor) => cursor.to_string(),
                            Err(_) => {
                                return ProblemDetails::internal(instance).into_response();
                            }
                        }
                    } else {
                        encode_cursor(page.next_cursor.position())
                    }
                }
            };
            let response = EventPageResponse {
                items: &events,
                next_cursor: &next_cursor,
                truncated,
            };
            let body = match serde_json::to_vec(&response) {
                Ok(body) if body.len() <= MAX_EVENT_PAGE_BYTES => body,
                _ => return ProblemDetails::internal(instance).into_response(),
            };
            let mut response = Body::from(body).into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
            response
        }
        Err(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

fn serialized_within(value: &impl serde::Serialize, limit: usize) -> bool {
    struct Counter {
        bytes: usize,
        limit: usize,
    }

    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(bytes.len())
                .filter(|bytes| *bytes <= self.limit)
                .ok_or_else(|| std::io::Error::other("serialized response exceeds byte bound"))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    serde_json::to_writer(&mut Counter { bytes: 0, limit }, value).is_ok()
}

async fn list_pending_approvals(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &project_id,
        "project_id",
        ProjectId::from_str,
        |project_id| Query::PendingApprovals { project_id },
        |projection| match projection {
            QueryProjection::Approvals(value) => Some(json!({ "items": value })),
            _ => None,
        },
    )
    .await
}

async fn list_pending_mcp_callbacks(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &project_id,
        "project_id",
        ProjectId::from_str,
        |project_id| Query::PendingMcpCallbacks { project_id },
        |projection| match projection {
            QueryProjection::McpCallbacks(value) => Some(json!({ "items": value })),
            _ => None,
        },
    )
    .await
}

async fn get_mcp_callback(
    State(state): State<ApiState>,
    Path(callback_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &callback_id,
        "mcp_callback_id",
        McpCallbackId::from_str,
        |callback_id| Query::GetMcpCallback { callback_id },
        |projection| match projection {
            QueryProjection::McpCallback(value) => Some(json!(value)),
            _ => None,
        },
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveMcpCallbackBody {
    kind: crate::domain::mcp_callback::McpCallbackKind,
    mode: crate::domain::mcp_callback::McpCallbackMode,
    expected_version: u64,
    challenge_generation: u64,
    schema_digest: String,
    action: McpCallbackAction,
    #[serde(default)]
    content: Option<Value>,
}

async fn resolve_mcp_callback(
    State(state): State<ApiState>,
    Path(callback_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let (body, context) = match command_body::<ResolveMcpCallbackBody>(request, state.config).await
    {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    let callback_id = match parse_id(
        &callback_id,
        "mcp_callback_id",
        &instance,
        McpCallbackId::from_str,
    ) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let valid_content = match (body.kind, body.mode, body.action) {
        (
            crate::domain::mcp_callback::McpCallbackKind::Elicitation,
            crate::domain::mcp_callback::McpCallbackMode::Form,
            McpCallbackAction::Accept,
        ) => body.content.is_some(),
        (
            crate::domain::mcp_callback::McpCallbackKind::Elicitation,
            crate::domain::mcp_callback::McpCallbackMode::Url,
            _,
        ) => body.content.is_none(),
        (
            crate::domain::mcp_callback::McpCallbackKind::Elicitation,
            crate::domain::mcp_callback::McpCallbackMode::Form,
            McpCallbackAction::Decline | McpCallbackAction::Cancel,
        )
        | (
            crate::domain::mcp_callback::McpCallbackKind::Sampling,
            crate::domain::mcp_callback::McpCallbackMode::SamplingRequest
            | crate::domain::mcp_callback::McpCallbackMode::SamplingResponse,
            _,
        ) => body.content.is_none(),
        _ => false,
    };
    if !valid_content {
        return ProblemDetails::service(
            ServiceError::Invalid(
                "callback kind, mode, action, and content do not match".to_owned(),
            ),
            instance,
        )
        .into_response();
    }
    let id = callback_id.to_string();
    match execute(
        &state,
        context,
        Command::ResolveMcpCallback {
            schema_version: SchemaVersion::CURRENT,
            callback_id,
            kind: body.kind,
            mode: body.mode,
            expected_version: body.expected_version,
            challenge_generation: body.challenge_generation,
            schema_digest: body.schema_digest,
            action: body.action,
            content: body.content,
            artifact_refs: Vec::new(),
        },
    )
    .await
    {
        Ok(receipt) => resource_response(
            StatusCode::OK,
            &format!("/v1/mcp-callbacks/{id}"),
            id,
            receipt,
        ),
        Err(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

#[derive(Deserialize)]
struct ResolveApprovalBody {
    decision: ApprovalDecision,
    expected_version: u64,
}

async fn resolve_approval(
    State(state): State<ApiState>,
    Path(approval_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let approval_id = match parse_id(&approval_id, "approval_id", &instance, ApprovalId::from_str) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let (body, context) = match command_body::<ResolveApprovalBody>(request, state.config).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    let id = approval_id.to_string();
    match execute(
        &state,
        context,
        Command::ResolveApproval {
            schema_version: SchemaVersion::CURRENT,
            approval_id,
            decision: body.decision,
            expected_version: body.expected_version,
        },
    )
    .await
    {
        Ok(receipt) => {
            resource_response(StatusCode::OK, &format!("/v1/approvals/{id}"), id, receipt)
        }
        Err(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

async fn list_pending_auth_requests(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &project_id,
        "project_id",
        ProjectId::from_str,
        |project_id| Query::PendingAuthRequests { project_id },
        |projection| match projection {
            QueryProjection::AuthRequests(value) => Some(json!({ "items": value })),
            _ => None,
        },
    )
    .await
}

#[derive(Deserialize)]
struct ResolveAuthBody {
    granted: bool,
    expected_version: u64,
}

async fn resolve_auth(
    State(state): State<ApiState>,
    Path(run_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let run_id = match parse_id(&run_id, "run_id", &instance, RunId::from_str) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let (body, context) = match command_body::<ResolveAuthBody>(request, state.config).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    let id = run_id.to_string();
    match execute(
        &state,
        context,
        Command::ResolveAuth {
            schema_version: SchemaVersion::CURRENT,
            run_id,
            granted: body.granted,
            expected_version: body.expected_version,
        },
    )
    .await
    {
        Ok(receipt) => resource_response(StatusCode::OK, &format!("/v1/runs/{id}"), id, receipt),
        Err(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

#[derive(Deserialize)]
struct RegisterArtifactBody {
    id: ArtifactId,
    reference: ArtifactRef,
    media_type: String,
    size: u64,
}

async fn register_artifact_metadata(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let project_id = match parse_id(&project_id, "project_id", &instance, ProjectId::from_str) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let (body, context) = match command_body::<RegisterArtifactBody>(request, state.config).await {
        Ok(value) => value,
        Err(problem) => return problem.into_response(),
    };
    let id = body.id.to_string();
    match execute(
        &state,
        context,
        Command::RegisterArtifactMetadata {
            schema_version: SchemaVersion::CURRENT,
            artifact_id: body.id,
            project_id,
            reference: body.reference,
            media_type: body.media_type,
            size: body.size,
        },
    )
    .await
    {
        Ok(receipt) => resource_response(
            StatusCode::CREATED,
            &format!("/v1/artifacts/{id}"),
            id,
            receipt,
        ),
        Err(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

async fn get_artifact_metadata(
    State(state): State<ApiState>,
    Path(artifact_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &artifact_id,
        "artifact_id",
        ArtifactId::from_str,
        |artifact_id| Query::GetArtifactMetadata { artifact_id },
        |projection| match projection {
            QueryProjection::ArtifactMetadata(value) => Some(json!(value)),
            _ => None,
        },
    )
    .await
}

async fn list_capabilities(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &project_id,
        "project_id",
        ProjectId::from_str,
        |project_id| Query::ListCapabilities { project_id },
        |projection| match projection {
            QueryProjection::Capabilities(value) => Some(json!({ "items": value })),
            _ => None,
        },
    )
    .await
}

async fn cursor_status(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let project_id = match parse_id(&project_id, "project_id", &instance, ProjectId::from_str) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let cursor = match query_value(request.uri(), "cursor").and_then(decode_cursor) {
        Some(position) => EventCursor::new(position),
        None => {
            return ProblemDetails::invalid(
                instance,
                "cursor",
                "A valid opaque cursor is required.",
            )
            .into_response();
        }
    };
    let context = match query_context(&request) {
        Ok(context) => context,
        Err(problem) => return problem.into_response(),
    };
    match query(
        &state,
        context,
        Query::EventCursorStatus { project_id, cursor },
    )
    .await
    {
        Ok(QueryProjection::CursorStatus(value)) => Json(json!({
            "requested": encode_cursor(value.requested.position()),
            "committed": encode_cursor(value.committed.position()),
            "caught_up": value.caught_up,
        }))
        .into_response(),
        Ok(_) => ProblemDetails::internal(instance).into_response(),
        Err(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

async fn project_status(
    State(state): State<ApiState>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    query_id(
        state,
        request,
        &project_id,
        "project_id",
        ProjectId::from_str,
        |project_id| Query::Status { project_id },
        |projection| match projection {
            QueryProjection::Status(value) => Some(json!({
                "committed": encode_cursor(value.committed.position()),
                "ready": value.ready,
            })),
            _ => None,
        },
    )
    .await
}

async fn query_id<I, P, Q, W>(
    state: ApiState,
    request: Request,
    wire_id: &str,
    parameter: &str,
    parse: P,
    build_query: Q,
    wire: W,
) -> Response
where
    P: FnOnce(&str) -> Result<I, crate::domain::ids::IdParseError>,
    Q: FnOnce(I) -> Query,
    W: FnOnce(QueryProjection) -> Option<Value>,
{
    let instance = request.uri().to_string();
    let id = match parse_id(wire_id, parameter, &instance, parse) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let context = match query_context(&request) {
        Ok(context) => context,
        Err(problem) => return problem.into_response(),
    };
    match query(&state, context, build_query(id)).await {
        Ok(projection) => match wire(projection) {
            Some(value) => Json(value).into_response(),
            None => ProblemDetails::internal(instance).into_response(),
        },
        Err(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

async fn execute(
    state: &ApiState,
    context: RequestContext,
    command: Command,
) -> Result<crate::api::service::CommandReceipt, ServiceError> {
    let service = state.service.clone();
    tokio::task::spawn_blocking(move || service.execute(&context, command))
        .await
        .map_err(|_| ServiceError::Store("service task failed".to_owned()))?
}

async fn prompt(
    state: &ApiState,
    context: RequestContext,
    request: PromptCommand,
) -> Result<PromptReceipt, ServiceError> {
    let service = state.service.clone();
    tokio::task::spawn_blocking(move || service.prompt(&context, request))
        .await
        .map_err(|_| ServiceError::Store("service task failed".to_owned()))?
}

async fn query(
    state: &ApiState,
    context: RequestContext,
    request: Query,
) -> Result<QueryProjection, ServiceError> {
    let service = state.service.clone();
    tokio::task::spawn_blocking(move || service.query(&context, request))
        .await
        .map_err(|_| ServiceError::Store("service task failed".to_owned()))?
}

async fn command_body<T: DeserializeOwned>(
    request: Request,
    config: RouterConfig,
) -> Result<(T, RequestContext), ProblemDetails> {
    let instance = request.uri().to_string();
    require_json(request.headers(), &instance)?;
    let principal = principal(&request, &instance)?;
    let idempotency_key = idempotency_key(request.headers(), &instance)?;
    let trace_id = trace_id(request.headers(), &instance)?;
    let bytes = to_bytes(request.into_body(), config.json_body_limit)
        .await
        .map_err(|_| ProblemDetails::payload_too_large(&instance))?;
    let body = serde_json::from_slice(&bytes)
        .map_err(|error| ProblemDetails::invalid(&instance, "body", error.to_string()))?;
    let context = RequestContext::authenticated(Ok(principal), Some(idempotency_key), trace_id)
        .map_err(|error| ProblemDetails::service(error, &instance))?;
    Ok((body, context))
}

fn query_context(request: &Request) -> Result<RequestContext, ProblemDetails> {
    let instance = request.uri().to_string();
    RequestContext::authenticated(
        Ok(principal(request, &instance)?),
        None,
        trace_id(request.headers(), &instance)?,
    )
    .map_err(|error| ProblemDetails::service(error, instance))
}

fn principal(request: &Request, instance: &str) -> Result<AuthenticatedPrincipal, ProblemDetails> {
    request
        .extensions()
        .get::<AuthenticatedPrincipal>()
        .cloned()
        .ok_or_else(|| ProblemDetails::unauthenticated(instance))
}

fn require_json(headers: &HeaderMap, instance: &str) -> Result<(), ProblemDetails> {
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if media_type == Some("application/json") {
        Ok(())
    } else {
        Err(ProblemDetails::unsupported_media_type(instance))
    }
}

fn idempotency_key(headers: &HeaderMap, instance: &str) -> Result<IdempotencyKey, ProblemDetails> {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok())
        .ok_or_else(|| ProblemDetails::missing_idempotency_key(instance))
}

fn trace_id(headers: &HeaderMap, instance: &str) -> Result<TraceId, ProblemDetails> {
    if let Some(value) = headers.get("x-request-id") {
        return value
            .to_str()
            .ok()
            .and_then(|value| TraceId::parse(value).ok())
            .ok_or_else(|| {
                ProblemDetails::invalid(
                    instance,
                    "X-Request-Id",
                    "X-Request-Id must contain 1 to 128 visible ASCII characters.",
                )
            });
    }
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| ProblemDetails::internal(instance))?;
    let value = random
        .iter()
        .fold(String::from("http-"), |mut value, byte| {
            value.push_str(&format!("{byte:02x}"));
            value
        });
    TraceId::parse(&value).map_err(|_| ProblemDetails::internal(instance))
}

fn parse_id<I, P>(value: &str, name: &str, instance: &str, parse: P) -> Result<I, ProblemDetails>
where
    P: FnOnce(&str) -> Result<I, crate::domain::ids::IdParseError>,
{
    parse(value).map_err(|_| {
        ProblemDetails::invalid(
            instance,
            name,
            format!("{name} must be a valid opaque identifier."),
        )
    })
}

fn query_value<'a>(uri: &'a Uri, name: &str) -> Option<&'a str> {
    uri.query()?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

fn stream_cursor(
    request: &Request,
    instance: &str,
) -> Result<Option<OpaqueStreamCursor>, ProblemDetails> {
    let header_cursor = request
        .headers()
        .get("last-event-id")
        .map(|value| {
            value
                .to_str()
                .map_err(|_| {
                    ProblemDetails::invalid(
                        instance,
                        "Last-Event-ID",
                        "Last-Event-ID must be ASCII.",
                    )
                })
                .and_then(|value| {
                    OpaqueStreamCursor::parse(value.to_owned()).map_err(|_| {
                        ProblemDetails::invalid(
                            instance,
                            "Last-Event-ID",
                            "Last-Event-ID must be an opaque stream cursor returned by this API.",
                        )
                    })
                })
        })
        .transpose()?;
    let query_cursor = query_value(request.uri(), "cursor")
        .map(|value| {
            OpaqueStreamCursor::parse(value.to_owned()).map_err(|_| {
                ProblemDetails::invalid(
                    instance,
                    "cursor",
                    "cursor must be an opaque stream cursor returned by this API.",
                )
            })
        })
        .transpose()?;
    if header_cursor.is_some() && query_cursor.is_some() && header_cursor != query_cursor {
        return Err(ProblemDetails::invalid(
            instance,
            "cursor",
            "cursor and Last-Event-ID must match when both are supplied.",
        ));
    }
    Ok(header_cursor.or(query_cursor))
}

fn stream_rejection(rejection: StreamRejection) -> Response {
    let status =
        StatusCode::from_u16(rejection.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = (status, Body::from(rejection.body().to_vec())).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(rejection.content_type()),
    );
    response
}

fn unavailable(instance: &str, title: &str, detail: &str, code: &str) -> Response {
    let body = json!({
        "type": problem_type(code),
        "title": title,
        "status": StatusCode::SERVICE_UNAVAILABLE.as_u16(),
        "detail": detail,
        "instance": instance,
        "code": code,
    });
    let mut response = (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(super::errors::PROBLEM_MEDIA_TYPE),
    );
    response
}

fn page_limit(uri: &Uri, instance: &str) -> Result<usize, ProblemDetails> {
    match query_value(uri, "limit") {
        None => Ok(100),
        Some(value) => value
            .parse::<usize>()
            .ok()
            .filter(|value| (1..=MAX_PAGE_SIZE).contains(value))
            .ok_or_else(|| {
                ProblemDetails::invalid(
                    instance,
                    "limit",
                    format!("limit must be between 1 and {MAX_PAGE_SIZE}."),
                )
            }),
    }
}

fn resource_response(
    status: StatusCode,
    location: &str,
    id: String,
    receipt: crate::api::service::CommandReceipt,
) -> Response {
    let mut response = (
        status,
        Json(json!({
            "resource": { "id": id },
            "receipt": receipt,
        })),
    )
        .into_response();
    if let Ok(location) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}

async fn not_found(request: Request) -> Response {
    ProblemDetails::not_found(request.uri().to_string()).into_response()
}

async fn method_not_allowed(request: Request) -> Response {
    ProblemDetails::method_not_allowed(request.uri().to_string()).into_response()
}
