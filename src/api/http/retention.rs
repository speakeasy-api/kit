use std::{
    str::FromStr,
    sync::{Arc, Mutex},
};

use axum::{
    Extension, Json, Router,
    body::to_bytes,
    extract::{Path, Request},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    api::{
        auth::contract::AuthenticatedPrincipal,
        http::{
            core::{LifecycleError, LifecycleMutation, LifecycleQuery, ServiceHandler},
            errors::ProblemDetails,
        },
        service::RequestContext,
    },
    domain::{
        config::Grant,
        deletion::{
            DeletionActor, DeletionError, DeletionJob, DeletionJobId, DeletionService,
            EffectiveRetention,
        },
        events::TraceId,
        ids::{ProjectId, ThreadId},
        retention::{
            EarliestPhysicalDeletion, RetentionObjectId, RetentionPeriod, RetentionPolicy,
            StoreTimestamp,
        },
    },
    store::sqlite::idempotency::IdempotencyKey,
};

use super::errors::PROBLEM_MEDIA_TYPE;

pub type SharedDeletionService = Arc<Mutex<DeletionService>>;

#[derive(Clone)]
pub enum RetentionRouteService {
    Deletion(SharedDeletionService),
    Service(Arc<dyn ServiceHandler>),
}

impl From<SharedDeletionService> for RetentionRouteService {
    fn from(service: SharedDeletionService) -> Self {
        Self::Deletion(service)
    }
}

impl From<Arc<dyn ServiceHandler>> for RetentionRouteService {
    fn from(service: Arc<dyn ServiceHandler>) -> Self {
        Self::Service(service)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct AuthoritativeStoreTime(pub StoreTimestamp);

pub fn routes(service: impl Into<RetentionRouteService>) -> Router {
    Router::new()
        .route("/v1/threads/{thread_id}/archive", post(set_thread_archived))
        .route(
            "/v1/threads/{thread_id}/deletion",
            post(request_thread_deletion),
        )
        .route("/v1/deletion-jobs/{deletion_job_id}", get(get_deletion_job))
        .route(
            "/v1/projects/{project_id}/retention",
            get(get_project_retention).post(set_project_retention),
        )
        .layer(Extension(service.into()))
}

#[derive(Deserialize)]
struct ArchiveBody {
    archived: bool,
    #[serde(default)]
    expected_version: Option<u64>,
}

pub async fn set_thread_archived(
    Extension(service): Extension<RetentionRouteService>,
    Path(thread_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let actor = match actor(&request, Grant::WorkspaceWrite) {
        Ok(actor) => actor,
        Err(problem) => return problem.into_response(),
    };
    let thread_id = match parse_thread(&thread_id, &instance) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let key = match mutation_headers(request.headers(), &instance) {
        Ok(key) => key,
        Err(problem) => return problem.into_response(),
    };
    let context = match &service {
        RetentionRouteService::Service(_) => match service_context(&request, Some(key.clone())) {
            Ok(context) => Some(context),
            Err(problem) => return problem.into_response(),
        },
        RetentionRouteService::Deletion(_) => None,
    };
    let body = match body::<ArchiveBody>(request, &instance).await {
        Ok(body) => body,
        Err(problem) => return problem.into_response(),
    };
    let object_id = RetentionObjectId::Transcript(thread_id);
    let result = match &service {
        RetentionRouteService::Deletion(service) => {
            let mut service = match lock(service, &instance) {
                Ok(service) => service,
                Err(problem) => return problem.into_response(),
            };
            match service.archive(actor, object_id, body.archived, key.as_str()) {
                Ok(status) => Ok(LifecycleMutation::Domain(status)),
                Err(error) => Err(deletion_problem(error, &instance)),
            }
        }
        RetentionRouteService::Service(service) => service
            .set_thread_archived(
                context.as_ref().expect("service context exists"),
                thread_id,
                body.archived,
                body.expected_version,
            )
            .map_err(|error| lifecycle_problem(error, &instance)),
    };
    match result {
        Ok(LifecycleMutation::Domain(status)) => Json(json!({
            "object_id": thread_id,
            "archived": status.archived,
        }))
        .into_response(),
        Ok(LifecycleMutation::Command(receipt)) => command_response(
            StatusCode::OK,
            &format!("/v1/threads/{thread_id}"),
            thread_id.to_string(),
            receipt,
        ),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct DeletionBody {
    #[serde(default)]
    expected_version: Option<u64>,
}

pub async fn request_thread_deletion(
    Extension(service): Extension<RetentionRouteService>,
    Path(thread_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let actor = match actor(&request, Grant::WorkspaceWrite) {
        Ok(actor) => actor,
        Err(problem) => return problem.into_response(),
    };
    let thread_id = match parse_thread(&thread_id, &instance) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let key = match mutation_headers(request.headers(), &instance) {
        Ok(key) => key,
        Err(problem) => return problem.into_response(),
    };
    let now = request
        .extensions()
        .get::<AuthoritativeStoreTime>()
        .map(|now| now.0);
    let context = match &service {
        RetentionRouteService::Service(_) => match service_context(&request, Some(key.clone())) {
            Ok(context) => Some(context),
            Err(problem) => return problem.into_response(),
        },
        RetentionRouteService::Deletion(_) => None,
    };
    let body = match body::<DeletionBody>(request, &instance).await {
        Ok(body) => body,
        Err(problem) => return problem.into_response(),
    };
    let object_id = RetentionObjectId::Transcript(thread_id);
    let result = match &service {
        RetentionRouteService::Deletion(service) => {
            let Some(now) = now else {
                return ProblemDetails::internal(instance).into_response();
            };
            let mut service = match lock(service, &instance) {
                Ok(service) => service,
                Err(problem) => return problem.into_response(),
            };
            match service.request_deletion(actor, object_id, key.as_str(), now) {
                Ok(job) => Ok(LifecycleMutation::Domain(job)),
                Err(error) => Err(deletion_problem(error, &instance)),
            }
        }
        RetentionRouteService::Service(service) => service
            .request_thread_deletion(
                context.as_ref().expect("service context exists"),
                thread_id,
                body.expected_version,
                now,
            )
            .map_err(|error| lifecycle_problem(error, &instance)),
    };
    match result {
        Ok(LifecycleMutation::Domain(job)) => job_response(StatusCode::ACCEPTED, &job),
        Ok(LifecycleMutation::Command(receipt)) => {
            let Some(position) = receipt.commit_positions.first() else {
                return command_response(
                    StatusCode::ACCEPTED,
                    &format!("/v1/threads/{thread_id}"),
                    thread_id.to_string(),
                    receipt,
                );
            };
            let job_id = DeletionJobId::new(position.get() as u128);
            let RetentionRouteService::Service(service) = &service else {
                unreachable!("domain deletion service returns a domain job")
            };
            match service
                .get_deletion_job(context.as_ref().expect("service context exists"), job_id)
            {
                Ok(LifecycleQuery::Domain(job)) => job_response(StatusCode::ACCEPTED, &job),
                Ok(LifecycleQuery::Projection(value)) => {
                    projection_job_response(StatusCode::ACCEPTED, job_id, thread_id, receipt, value)
                }
                Err(_) => command_response(
                    StatusCode::ACCEPTED,
                    &format!("/v1/threads/{thread_id}"),
                    thread_id.to_string(),
                    receipt,
                ),
            }
        }
        Err(response) => response,
    }
}

pub async fn get_deletion_job(
    Extension(service): Extension<RetentionRouteService>,
    Path(job_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let actor = match actor(&request, Grant::WorkspaceRead) {
        Ok(actor) => actor,
        Err(problem) => return problem.into_response(),
    };
    let job_id = match DeletionJobId::from_str(&job_id) {
        Ok(id) => id,
        Err(_) => {
            return ProblemDetails::invalid(
                instance,
                "deletion_job_id",
                "deletion_job_id must be a valid opaque identifier.",
            )
            .into_response();
        }
    };
    let result = match &service {
        RetentionRouteService::Deletion(service) => {
            let service = match lock(service, &instance) {
                Ok(service) => service,
                Err(problem) => return problem.into_response(),
            };
            match service.job(actor, job_id) {
                Ok(job) => Ok(LifecycleQuery::Domain(job)),
                Err(error) => Err(deletion_problem(error, &instance)),
            }
        }
        RetentionRouteService::Service(service) => {
            let context = match service_context(&request, None) {
                Ok(context) => context,
                Err(problem) => return problem.into_response(),
            };
            service
                .get_deletion_job(&context, job_id)
                .map_err(|error| lifecycle_problem(error, &instance))
        }
    };
    match result {
        Ok(LifecycleQuery::Domain(job)) => job_response(StatusCode::OK, &job),
        Ok(LifecycleQuery::Projection(value)) => Json(value).into_response(),
        Err(response) => response,
    }
}

pub async fn get_project_retention(
    Extension(service): Extension<RetentionRouteService>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let actor = match actor(&request, Grant::WorkspaceRead) {
        Ok(actor) => actor,
        Err(problem) => return problem.into_response(),
    };
    let project_id = match parse_project(&project_id, &instance) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let result = match &service {
        RetentionRouteService::Deletion(service) => {
            let service = match lock(service, &instance) {
                Ok(service) => service,
                Err(problem) => return problem.into_response(),
            };
            match service.effective_project_policy(actor, project_id) {
                Ok(policy) => Ok(policy),
                Err(error) => Err(deletion_problem(error, &instance)),
            }
        }
        RetentionRouteService::Service(service) => {
            let context = match service_context(&request, None) {
                Ok(context) => context,
                Err(problem) => return problem.into_response(),
            };
            service
                .get_project_retention(&context, project_id)
                .map_err(|error| lifecycle_problem(error, &instance))
        }
    };
    match result {
        Ok(policy) => Json(json!({ "effective": policy_json(policy) })).into_response(),
        Err(response) => response,
    }
}

#[derive(Deserialize)]
struct SetRetentionBody {
    policy: PolicyBody,
    #[serde(default)]
    expected_version: Option<u64>,
}

#[derive(Clone, Copy, Deserialize)]
struct PolicyBody {
    event: PeriodBody,
    transcript: PeriodBody,
    terminal: PeriodBody,
    artifact: PeriodBody,
    experiment: PeriodBody,
    backup: PeriodBody,
}

impl From<PolicyBody> for RetentionPolicy {
    fn from(value: PolicyBody) -> Self {
        Self {
            event: value.event.into(),
            transcript: value.transcript.into(),
            terminal: value.terminal.into(),
            artifact: value.artifact.into(),
            experiment: value.experiment.into(),
            backup: value.backup.into(),
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PeriodBody {
    ForMicros(u64),
    Forever,
}

impl From<PeriodBody> for RetentionPeriod {
    fn from(value: PeriodBody) -> Self {
        match value {
            PeriodBody::ForMicros(value) => Self::ForMicros(value),
            PeriodBody::Forever => Self::Forever,
        }
    }
}

pub async fn set_project_retention(
    Extension(service): Extension<RetentionRouteService>,
    Path(project_id): Path<String>,
    request: Request,
) -> Response {
    let instance = request.uri().to_string();
    let actor = match actor(&request, Grant::WorkspaceWrite) {
        Ok(actor) => actor,
        Err(problem) => return problem.into_response(),
    };
    let project_id = match parse_project(&project_id, &instance) {
        Ok(id) => id,
        Err(problem) => return problem.into_response(),
    };
    let key = match mutation_headers(request.headers(), &instance) {
        Ok(key) => key,
        Err(problem) => return problem.into_response(),
    };
    let context = match &service {
        RetentionRouteService::Service(_) => match service_context(&request, Some(key.clone())) {
            Ok(context) => Some(context),
            Err(problem) => return problem.into_response(),
        },
        RetentionRouteService::Deletion(_) => None,
    };
    let body = match body::<SetRetentionBody>(request, &instance).await {
        Ok(body) => body,
        Err(problem) => return problem.into_response(),
    };
    let policy = body.policy.into();
    let result = match &service {
        RetentionRouteService::Deletion(service) => {
            let mut service = match lock(service, &instance) {
                Ok(service) => service,
                Err(problem) => return problem.into_response(),
            };
            match service.set_effective_policy(actor, project_id, policy, key.as_str()) {
                Ok(policy) => Ok(LifecycleMutation::Domain(policy)),
                Err(error) => Err(deletion_problem(error, &instance)),
            }
        }
        RetentionRouteService::Service(service) => service
            .set_project_retention(
                context.as_ref().expect("service context exists"),
                project_id,
                policy,
                body.expected_version,
            )
            .map_err(|error| lifecycle_problem(error, &instance)),
    };
    match result {
        Ok(LifecycleMutation::Domain(policy)) => {
            Json(json!({ "effective": policy_json(policy) })).into_response()
        }
        Ok(LifecycleMutation::Command(receipt)) => command_response(
            StatusCode::OK,
            &format!("/v1/projects/{project_id}"),
            project_id.to_string(),
            receipt,
        ),
        Err(response) => response,
    }
}

fn actor(request: &Request, required: Grant) -> Result<DeletionActor, ProblemDetails> {
    let instance = request.uri().to_string();
    let principal = request
        .extensions()
        .get::<AuthenticatedPrincipal>()
        .ok_or_else(|| ProblemDetails::unauthenticated(&instance))?;
    let grant = principal.grant_snapshot();
    if !grant.grants().contains(&required) {
        return Err(ProblemDetails::not_found(instance));
    }
    Ok(DeletionActor::new(
        principal.principal_id(),
        grant.project_id(),
    ))
}

fn service_context(
    request: &Request,
    idempotency_key: Option<IdempotencyKey>,
) -> Result<RequestContext, ProblemDetails> {
    let instance = request.uri().to_string();
    let principal = request
        .extensions()
        .get::<AuthenticatedPrincipal>()
        .cloned()
        .ok_or_else(|| ProblemDetails::unauthenticated(&instance))?;
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|_| ProblemDetails::internal(&instance))?;
    let trace = TraceId::parse(&format!(
        "http-{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
    .map_err(|_| ProblemDetails::internal(&instance))?;
    RequestContext::authenticated(Ok(principal), idempotency_key, trace)
        .map_err(|error| ProblemDetails::service(error, instance))
}

fn mutation_headers(headers: &HeaderMap, instance: &str) -> Result<IdempotencyKey, ProblemDetails> {
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if media_type != Some("application/json") {
        return Err(ProblemDetails::unsupported_media_type(instance));
    }
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| IdempotencyKey::parse(value).ok())
        .ok_or_else(|| ProblemDetails::missing_idempotency_key(instance))
}

async fn body<T: serde::de::DeserializeOwned>(
    request: Request,
    instance: &str,
) -> Result<T, ProblemDetails> {
    let bytes = to_bytes(request.into_body(), super::core::JSON_BODY_LIMIT)
        .await
        .map_err(|_| ProblemDetails::payload_too_large(instance))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ProblemDetails::invalid(instance, "body", error.to_string()))
}

fn parse_thread(value: &str, instance: &str) -> Result<ThreadId, ProblemDetails> {
    ThreadId::parse(value).map_err(|_| {
        ProblemDetails::invalid(
            instance,
            "thread_id",
            "thread_id must be a valid opaque identifier.",
        )
    })
}

fn parse_project(value: &str, instance: &str) -> Result<ProjectId, ProblemDetails> {
    ProjectId::parse(value).map_err(|_| {
        ProblemDetails::invalid(
            instance,
            "project_id",
            "project_id must be a valid opaque identifier.",
        )
    })
}

fn lock<'a>(
    service: &'a SharedDeletionService,
    instance: &str,
) -> Result<std::sync::MutexGuard<'a, DeletionService>, ProblemDetails> {
    service
        .lock()
        .map_err(|_| ProblemDetails::internal(instance))
}

fn deletion_problem(error: DeletionError, instance: &str) -> Response {
    match error {
        DeletionError::NotFound => ProblemDetails::not_found(instance).into_response(),
        DeletionError::InvalidIdempotencyKey => {
            ProblemDetails::missing_idempotency_key(instance).into_response()
        }
        DeletionError::IdempotencyConflict => problem(
            StatusCode::CONFLICT,
            "Idempotency conflict",
            "The idempotency key was already used with different input.",
            "idempotency_conflict",
            instance,
            None,
        ),
        DeletionError::LegalHold {
            job_id,
            earliest_physical_deletion,
        } => legal_hold_problem(job_id, earliest_physical_deletion, instance),
        DeletionError::StaleFence { .. } => problem(
            StatusCode::CONFLICT,
            "Stale deletion fence",
            "Deletion policy changed; the job must be evaluated again.",
            "stale_deletion_fence",
            instance,
            None,
        ),
        DeletionError::InvalidState(_) => problem(
            StatusCode::CONFLICT,
            "Deletion job conflict",
            "The deletion job cannot execute from its current state.",
            "deletion_job_conflict",
            instance,
            None,
        ),
    }
}

fn lifecycle_problem(error: LifecycleError, instance: &str) -> Response {
    match error {
        LifecycleError::Deletion(error) => deletion_problem(error, instance),
        LifecycleError::Service(error) => ProblemDetails::service(error, instance).into_response(),
    }
}

fn command_response(
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

fn legal_hold_problem(
    job_id: DeletionJobId,
    earliest: EarliestPhysicalDeletion,
    instance: &str,
) -> Response {
    let value = json!({
        "type": "https://kit.dev/problems/legal_hold",
        "title": "Deletion blocked by legal hold",
        "status": StatusCode::LOCKED.as_u16(),
        "detail": "Physical deletion is prohibited while a legal hold applies.",
        "instance": instance,
        "code": "legal_hold",
        "deletion_job_id": job_id.to_string(),
        "blockers": ["legal_hold"],
        "earliest_physical_deletion": earliest_json(earliest),
    });
    let mut response = (StatusCode::LOCKED, Json(value)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(PROBLEM_MEDIA_TYPE),
    );
    response
}

fn problem(
    status: StatusCode,
    title: &str,
    detail: &str,
    code: &str,
    instance: &str,
    job_id: Option<DeletionJobId>,
) -> Response {
    let mut value = json!({
        "type": format!("https://kit.dev/problems/{code}"),
        "title": title,
        "status": status.as_u16(),
        "detail": detail,
        "instance": instance,
        "code": code,
    });
    if let Some(job_id) = job_id {
        value["deletion_job_id"] = json!(job_id.to_string());
    }
    let mut response = (status, Json(value)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(PROBLEM_MEDIA_TYPE),
    );
    response
}

fn job_response(status: StatusCode, job: &DeletionJob) -> Response {
    let mut response = (status, Json(job_json(job))).into_response();
    if let Ok(location) = HeaderValue::from_str(&format!("/v1/deletion-jobs/{}", job.id)) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}

fn projection_job_response(
    status: StatusCode,
    job_id: DeletionJobId,
    thread_id: ThreadId,
    receipt: crate::api::service::CommandReceipt,
    mut value: Value,
) -> Response {
    value["resource"] = json!({ "id": thread_id });
    value["receipt"] = json!(receipt);
    let mut response = (status, Json(value)).into_response();
    if let Ok(location) = HeaderValue::from_str(&format!("/v1/deletion-jobs/{job_id}")) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
}

fn job_json(job: &DeletionJob) -> Value {
    json!({
        "id": job.id.to_string(),
        "state": job.state.as_str(),
        "version": job.version,
        "resource_version": job.resource_version,
        "effective_retention": effective_retention_json(job.effective_retention),
        "blockers": job.blockers.iter().map(|blocker| blocker.as_str()).collect::<Vec<_>>(),
        "fence": job.fence.get(),
        "requested_at_unix_micros": job.requested_at.unix_micros(),
        "completed_at_unix_micros": job.completed_at.map(StoreTimestamp::unix_micros),
        "failure": job.failure.as_deref(),
        "audit": job.audit.iter().map(|entry| json!({
            "sequence": entry.sequence,
            "state": entry.state.as_str(),
            "at_unix_micros": entry.at.unix_micros(),
        })).collect::<Vec<_>>(),
    })
}

fn effective_retention_json(retention: EffectiveRetention) -> Value {
    json!({
        "policy": policy_json(retention.policy),
        "earliest_physical_deletion": earliest_json(retention.earliest_physical_deletion),
    })
}

fn earliest_json(earliest: EarliestPhysicalDeletion) -> Value {
    match earliest {
        EarliestPhysicalDeletion::At(at) => json!({ "at_unix_micros": at.unix_micros() }),
        EarliestPhysicalDeletion::Never => json!("never"),
    }
}

fn policy_json(policy: RetentionPolicy) -> Value {
    json!({
        "event": period_json(policy.event),
        "transcript": period_json(policy.transcript),
        "terminal": period_json(policy.terminal),
        "artifact": period_json(policy.artifact),
        "experiment": period_json(policy.experiment),
        "backup": period_json(policy.backup),
    })
}

fn period_json(period: RetentionPeriod) -> Value {
    match period {
        RetentionPeriod::ForMicros(value) => json!({ "for_micros": value }),
        RetentionPeriod::Forever => json!("forever"),
    }
}
