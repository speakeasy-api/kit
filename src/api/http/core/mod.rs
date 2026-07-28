use std::{fmt, sync::Mutex};

use axum::http::request::Parts;

use crate::api::auth::contract::{AuthDecision, Authorizer};
use crate::api::service::{
    ArtifactService, CapabilityService, Command, CommandReceipt, LeaseService, PromptCommand,
    PromptInput, PromptReceipt, Query, QueryProjection, RequestContext, Scheduler, Service,
    ServiceError, ServiceStore,
};
use crate::domain::deletion::{ArchiveStatus, DeletionError, DeletionJob, DeletionJobId};
use crate::domain::events::SchemaVersion;
use crate::domain::ids::{ProjectId, ThreadId};
use crate::domain::retention::{RetentionPolicy, StoreTimestamp};

// Includes `{\"bytes\":[` plus 16,384 comma-separated three-digit byte values.
pub const JSON_BODY_LIMIT: usize = 64 * 1024 + 16;
pub const DEFAULT_TIMEOUT_SECONDS: u64 = 30;
pub const MAX_PAGE_SIZE: usize = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RouteDescriptor {
    pub method: &'static str,
    pub path: &'static str,
    pub operation: &'static str,
    pub mutation: bool,
    pub long_running: bool,
}

pub const ROUTES: &[RouteDescriptor] = &[
    route("POST", "/v1/projects", "project.create", true, false),
    route(
        "GET",
        "/v1/projects/{project_id}",
        "project.get",
        false,
        false,
    ),
    route(
        "POST",
        "/v1/projects/{project_id}/threads",
        "thread.create",
        true,
        false,
    ),
    route(
        "GET",
        "/v1/projects/{project_id}/threads",
        "thread.list",
        false,
        false,
    ),
    route("GET", "/v1/threads/{thread_id}", "thread.get", false, false),
    route(
        "POST",
        "/v1/threads/{thread_id}/archive",
        "thread.archive",
        true,
        false,
    ),
    route(
        "POST",
        "/v1/threads/{thread_id}/deletion",
        "thread.delete.initiate",
        true,
        true,
    ),
    route(
        "GET",
        "/v1/deletion-jobs/{deletion_job_id}",
        "thread.deletion.get",
        false,
        false,
    ),
    route(
        "POST",
        "/v1/threads/{thread_id}/runs",
        "run.start",
        true,
        true,
    ),
    route(
        "GET",
        "/v1/projects/{project_id}/runs",
        "run.list",
        false,
        false,
    ),
    route("GET", "/v1/runs/{run_id}", "run.get", false, false),
    route("GET", "/v1/runs/{run_id}/cost", "run.cost", false, false),
    route(
        "GET",
        "/v1/runs/{run_id}/prompts",
        "run.prompts",
        false,
        false,
    ),
    route(
        "GET",
        "/v1/runs/{run_id}/transcript",
        "run.transcript",
        false,
        false,
    ),
    route("POST", "/v1/runs/{run_id}/cancel", "run.cancel", true, true),
    route("POST", "/v1/runs/{run_id}/input", "run.input", true, false),
    route(
        "GET",
        "/v1/runs/{run_id}/events",
        "run.timeline",
        false,
        false,
    ),
    route(
        "GET",
        "/v1/threads/{thread_id}/events",
        "thread.events",
        false,
        false,
    ),
    route(
        "GET",
        "/v1/projects/{project_id}/approvals",
        "approval.pending",
        false,
        false,
    ),
    route(
        "POST",
        "/v1/approvals/{approval_id}/resolve",
        "approval.resolve",
        true,
        false,
    ),
    route(
        "GET",
        "/v1/projects/{project_id}/auth-requests",
        "auth.pending",
        false,
        false,
    ),
    route(
        "POST",
        "/v1/runs/{run_id}/auth/resolve",
        "auth.resolve",
        true,
        false,
    ),
    route(
        "POST",
        "/v1/projects/{project_id}/artifacts",
        "artifact.metadata.register",
        true,
        false,
    ),
    route(
        "GET",
        "/v1/artifacts/{artifact_id}",
        "artifact.metadata.get",
        false,
        false,
    ),
    route(
        "GET",
        "/v1/projects/{project_id}/retention",
        "project.retention.get",
        false,
        false,
    ),
    route(
        "POST",
        "/v1/projects/{project_id}/retention",
        "project.retention.set",
        true,
        false,
    ),
    route(
        "GET",
        "/v1/projects/{project_id}/capabilities",
        "capability.list",
        false,
        false,
    ),
    route(
        "GET",
        "/v1/projects/{project_id}/events/status",
        "event.cursor.status",
        false,
        false,
    ),
    route(
        "GET",
        "/v1/projects/{project_id}/status",
        "service.status",
        false,
        false,
    ),
];

const fn route(
    method: &'static str,
    path: &'static str,
    operation: &'static str,
    mutation: bool,
    long_running: bool,
) -> RouteDescriptor {
    RouteDescriptor {
        method,
        path,
        operation,
        mutation,
        long_running,
    }
}

pub trait HttpAuthenticator: Send + Sync + 'static {
    fn authenticate(&self, request: &Parts) -> AuthDecision;
}

impl<F> HttpAuthenticator for F
where
    F: Fn(&Parts) -> AuthDecision + Send + Sync + 'static,
{
    fn authenticate(&self, request: &Parts) -> AuthDecision {
        self(request)
    }
}

pub trait ServiceHandler: Send + Sync + 'static {
    fn execute(
        &self,
        context: &RequestContext,
        command: Command,
    ) -> Result<CommandReceipt, ServiceError>;

    fn query(
        &self,
        context: &RequestContext,
        query: Query,
    ) -> Result<QueryProjection, ServiceError>;

    fn prompt(
        &self,
        context: &RequestContext,
        request: PromptCommand,
    ) -> Result<PromptReceipt, ServiceError> {
        let run_id = request.run_id.ok_or_else(|| {
            ServiceError::Invalid("run_id is required by this service adapter".to_owned())
        })?;
        let PromptInput::Artifact(input) = request.input else {
            return Err(ServiceError::Store(
                "prompt artifact storage is unavailable".to_owned(),
            ));
        };
        self.execute(
            context,
            Command::StartRun {
                schema_version: SchemaVersion::CURRENT,
                run_id,
                thread_id: request.thread_id,
                input,
                run_config: request.run_config.map(Box::new),
                experiment_config: request.experiment_config.map(Box::new),
                effective_config: None,
            },
        )
        .map(|receipt| PromptReceipt { run_id, receipt })
    }

    fn set_thread_archived(
        &self,
        context: &RequestContext,
        thread_id: ThreadId,
        archived: bool,
        expected_version: Option<u64>,
    ) -> Result<LifecycleMutation<ArchiveStatus>, LifecycleError> {
        let expected_version = expected_version
            .or_else(
                || match self.query(context, Query::GetThread { thread_id }) {
                    Ok(QueryProjection::Thread(thread)) => Some(thread.version),
                    _ => None,
                },
            )
            .ok_or(ServiceError::NotFound)?;
        self.execute(
            context,
            Command::SetThreadArchived {
                schema_version: SchemaVersion::CURRENT,
                thread_id,
                archived,
                expected_version,
            },
        )
        .map(LifecycleMutation::Command)
        .map_err(Into::into)
    }

    fn request_thread_deletion(
        &self,
        context: &RequestContext,
        thread_id: ThreadId,
        expected_version: Option<u64>,
        _now: Option<StoreTimestamp>,
    ) -> Result<LifecycleMutation<DeletionJob>, LifecycleError> {
        let expected_version = expected_version
            .or_else(
                || match self.query(context, Query::GetThread { thread_id }) {
                    Ok(QueryProjection::Thread(thread)) => Some(thread.version),
                    _ => None,
                },
            )
            .ok_or(ServiceError::NotFound)?;
        self.execute(
            context,
            Command::InitiateThreadDeletion {
                schema_version: SchemaVersion::CURRENT,
                thread_id,
                expected_version,
            },
        )
        .map(LifecycleMutation::Command)
        .map_err(Into::into)
    }

    fn get_deletion_job(
        &self,
        context: &RequestContext,
        id: DeletionJobId,
    ) -> Result<LifecycleQuery<DeletionJob>, LifecycleError> {
        match self.query(
            context,
            Query::GetDeletionJob {
                deletion_job_id: id.to_string(),
            },
        )? {
            QueryProjection::DeletionJob(value) => Ok(LifecycleQuery::Projection(value)),
            _ => Err(ServiceError::Store("invalid deletion job projection".to_owned()).into()),
        }
    }

    fn get_project_retention(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
    ) -> Result<RetentionPolicy, LifecycleError> {
        match self.query(context, Query::GetProjectRetention { project_id })? {
            QueryProjection::Retention(Some(policy)) => Ok(policy),
            QueryProjection::Retention(None) => Err(ServiceError::NotFound.into()),
            _ => Err(ServiceError::Store("invalid retention projection".to_owned()).into()),
        }
    }

    fn set_project_retention(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
        policy: RetentionPolicy,
        expected_version: Option<u64>,
    ) -> Result<LifecycleMutation<RetentionPolicy>, LifecycleError> {
        let expected_version = expected_version
            .or_else(
                || match self.query(context, Query::GetProject { project_id }) {
                    Ok(QueryProjection::Project(project)) => Some(project.version),
                    _ => None,
                },
            )
            .ok_or(ServiceError::NotFound)?;
        self.execute(
            context,
            Command::SetProjectRetention {
                schema_version: SchemaVersion::CURRENT,
                project_id,
                policy,
                expected_version,
            },
        )
        .map(LifecycleMutation::Command)
        .map_err(Into::into)
    }
}

pub enum LifecycleMutation<T> {
    Domain(T),
    Command(CommandReceipt),
}

pub enum LifecycleQuery<T> {
    Domain(T),
    Projection(serde_json::Value),
}

#[derive(Debug)]
pub enum LifecycleError {
    Deletion(DeletionError),
    Service(ServiceError),
}

impl From<DeletionError> for LifecycleError {
    fn from(error: DeletionError) -> Self {
        Self::Deletion(error)
    }
}

impl From<ServiceError> for LifecycleError {
    fn from(error: ServiceError) -> Self {
        Self::Service(error)
    }
}

impl<S, A, R, M> ServiceHandler for Mutex<Service<S, A, R, M>>
where
    S: ServiceStore + Send + 'static,
    A: Authorizer + Send + 'static,
    R: Scheduler + CapabilityService + LeaseService + ArtifactService + Send + 'static,
    M: crate::domain::config::RunConfigMaterializer + Send + 'static,
{
    fn execute(
        &self,
        context: &RequestContext,
        command: Command,
    ) -> Result<CommandReceipt, ServiceError> {
        self.lock()
            .map_err(|_| ServiceError::Store("service lock poisoned".to_owned()))?
            .execute(context, command)
    }

    fn query(
        &self,
        context: &RequestContext,
        query: Query,
    ) -> Result<QueryProjection, ServiceError> {
        self.lock()
            .map_err(|_| ServiceError::Store("service lock poisoned".to_owned()))?
            .query(context, query)
    }

    fn prompt(
        &self,
        context: &RequestContext,
        request: PromptCommand,
    ) -> Result<PromptReceipt, ServiceError> {
        self.lock()
            .map_err(|_| ServiceError::Store("service lock poisoned".to_owned()))?
            .prompt(context, request)
    }

    fn set_thread_archived(
        &self,
        context: &RequestContext,
        thread_id: ThreadId,
        archived: bool,
        expected_version: Option<u64>,
    ) -> Result<LifecycleMutation<ArchiveStatus>, LifecycleError> {
        let mut service = self
            .lock()
            .map_err(|_| ServiceError::Store("service lock poisoned".to_owned()))?;
        let expected_version = expected_version
            .or_else(
                || match service.query(context, Query::GetThread { thread_id }) {
                    Ok(QueryProjection::Thread(thread)) => Some(thread.version),
                    _ => None,
                },
            )
            .ok_or(ServiceError::NotFound)?;
        service.execute(
            context,
            Command::SetThreadArchived {
                schema_version: SchemaVersion::CURRENT,
                thread_id,
                archived,
                expected_version,
            },
        )?;
        service
            .deletion_archive_status(context, thread_id)
            .map(LifecycleMutation::Domain)
            .map_err(Into::into)
    }

    fn request_thread_deletion(
        &self,
        context: &RequestContext,
        thread_id: ThreadId,
        expected_version: Option<u64>,
        _now: Option<StoreTimestamp>,
    ) -> Result<LifecycleMutation<DeletionJob>, LifecycleError> {
        let mut service = self
            .lock()
            .map_err(|_| ServiceError::Store("service lock poisoned".to_owned()))?;
        let expected_version = expected_version
            .or_else(
                || match service.query(context, Query::GetThread { thread_id }) {
                    Ok(QueryProjection::Thread(thread)) => Some(thread.version),
                    _ => None,
                },
            )
            .ok_or(ServiceError::NotFound)?;
        service.execute(
            context,
            Command::InitiateThreadDeletion {
                schema_version: SchemaVersion::CURRENT,
                thread_id,
                expected_version,
            },
        )?;
        service
            .deletion_job_for_thread(context, thread_id)
            .map(LifecycleMutation::Domain)
            .map_err(Into::into)
    }

    fn get_deletion_job(
        &self,
        context: &RequestContext,
        id: DeletionJobId,
    ) -> Result<LifecycleQuery<DeletionJob>, LifecycleError> {
        self.lock()
            .map_err(|_| ServiceError::Store("service lock poisoned".to_owned()))?
            .deletion_job(context, id)
            .map(LifecycleQuery::Domain)
            .map_err(Into::into)
    }

    fn get_project_retention(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
    ) -> Result<RetentionPolicy, LifecycleError> {
        self.lock()
            .map_err(|_| ServiceError::Store("service lock poisoned".to_owned()))?
            .deletion_project_policy(context, project_id)
            .map_err(Into::into)
    }

    fn set_project_retention(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
        policy: RetentionPolicy,
        expected_version: Option<u64>,
    ) -> Result<LifecycleMutation<RetentionPolicy>, LifecycleError> {
        let mut service = self
            .lock()
            .map_err(|_| ServiceError::Store("service lock poisoned".to_owned()))?;
        let expected_version = expected_version
            .or_else(
                || match service.query(context, Query::GetProject { project_id }) {
                    Ok(QueryProjection::Project(project)) => Some(project.version),
                    _ => None,
                },
            )
            .ok_or(ServiceError::NotFound)?;
        service.execute(
            context,
            Command::SetProjectRetention {
                schema_version: SchemaVersion::CURRENT,
                project_id,
                policy,
                expected_version,
            },
        )?;
        service
            .deletion_project_policy(context, project_id)
            .map(LifecycleMutation::Domain)
            .map_err(Into::into)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PageCursor(u64);

impl PageCursor {
    pub const fn new(position: u64) -> Self {
        Self(position)
    }

    pub fn parse(value: &str) -> Option<Self> {
        let value = value.strip_prefix("cursor_")?;
        (value.len() == 16)
            .then(|| u64::from_str_radix(value, 16).ok().map(Self))
            .flatten()
    }

    pub const fn position(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PageCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cursor_{:016x}", self.0)
    }
}

pub fn encode_cursor(position: u64) -> String {
    PageCursor::new(position).to_string()
}

pub fn decode_cursor(value: &str) -> Option<u64> {
    PageCursor::parse(value).map(PageCursor::position)
}
