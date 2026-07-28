use std::{
    fs::File,
    io::{self, Read},
    path::PathBuf,
};

use reqwest::Method;
use serde_json::Value;

use crate::{
    api::http::repo::REPO_ROUTES, capabilities::native::NativeTool, domain::ids::ProjectId,
    store::sqlite::idempotency::IdempotencyKey,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepoCliOperation {
    pub command: &'static str,
    pub service_operation: &'static str,
    pub openapi_operation_id: &'static str,
    pub mutation: bool,
}

pub const REPO_CLI_OPERATIONS: &[RepoCliOperation] = &[
    operation("repo status", "repo.status", "getRepositoryStatus", false),
    operation(
        "repo revision",
        "repo.revision",
        "getRepositoryRevision",
        false,
    ),
    operation(
        "repo capabilities",
        "repo.capabilities",
        "listRepositoryCapabilities",
        false,
    ),
    operation(
        "repo discover",
        "repo.discover",
        "discoverRepository",
        false,
    ),
    operation("repo search", "repo.search", "searchRepository", false),
    operation("repo read", "repo.read", "readRepository", false),
    operation("repo edit", "repo.edit", "editRepository", true),
    operation("repo run", "repo.run", "runRepositoryCommand", true),
    operation("repo check", "repo.check", "checkRepository", true),
    operation("repo result", "repo.result", "getRepositoryResult", false),
    operation(
        "repo events",
        "repo.result.events",
        "getRepositoryResultEvents",
        false,
    ),
    operation(
        "repo approval",
        "repo.result.approval",
        "resolveRepositoryApproval",
        true,
    ),
    operation(
        "repo cancel",
        "repo.result.cancel",
        "cancelRepositoryOperation",
        true,
    ),
    operation(
        "repo artifact",
        "repo.artifact",
        "getRepositoryArtifact",
        false,
    ),
];

const fn operation(
    command: &'static str,
    service_operation: &'static str,
    openapi_operation_id: &'static str,
    mutation: bool,
) -> RepoCliOperation {
    RepoCliOperation {
        command,
        service_operation,
        openapi_operation_id,
        mutation,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputSource {
    Stdin,
    File(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoRequest {
    pub operation: &'static str,
    pub method: Method,
    pub path: String,
    body: Option<Vec<u8>>,
    source: Option<InputSource>,
    pub idempotency_key: Option<IdempotencyKey>,
}

impl RepoRequest {
    pub fn status() -> Self {
        query("repo.status", "/v1/repository/status".to_owned())
    }
    pub fn revision(project: ProjectId) -> Self {
        query(
            "repo.revision",
            format!("/v1/projects/{project}/repository/revision"),
        )
    }
    pub fn capabilities(project: ProjectId) -> Self {
        query(
            "repo.capabilities",
            format!("/v1/projects/{project}/repository/capabilities"),
        )
    }
    pub fn invoke(
        project: ProjectId,
        tool: NativeTool,
        source: InputSource,
        key: Option<IdempotencyKey>,
    ) -> Self {
        Self {
            operation: match tool {
                NativeTool::Discover => "repo.discover",
                NativeTool::Search => "repo.search",
                NativeTool::Read => "repo.read",
                NativeTool::Edit => "repo.edit",
                NativeTool::Run => "repo.run",
                NativeTool::Check => "repo.check",
            },
            method: Method::POST,
            path: format!("/v1/projects/{project}/repository/{}", tool.short_name()),
            body: None,
            source: Some(source),
            idempotency_key: key,
        }
    }
    pub fn result(id: &str) -> Self {
        query("repo.result", format!("/v1/repository-results/{id}"))
    }
    pub fn events(id: &str) -> Self {
        query(
            "repo.result.events",
            format!("/v1/repository-results/{id}/events"),
        )
    }
    pub fn artifact(reference: &str) -> Self {
        query(
            "repo.artifact",
            format!("/v1/repository-artifacts/{reference}"),
        )
    }
    pub fn approval(id: &str, approved: bool, key: IdempotencyKey) -> Self {
        Self {
            operation: "repo.result.approval",
            method: Method::POST,
            path: format!("/v1/repository-results/{id}/approval"),
            body: Some(
                serde_json::to_vec(
                    &serde_json::json!({"decision":if approved{"approved"}else{"denied"}}),
                )
                .expect("static approval JSON"),
            ),
            source: None,
            idempotency_key: Some(key),
        }
    }
    pub fn cancel(id: &str, key: IdempotencyKey) -> Self {
        Self {
            operation: "repo.result.cancel",
            method: Method::POST,
            path: format!("/v1/repository-results/{id}/cancel"),
            body: Some(b"{}".to_vec()),
            source: None,
            idempotency_key: Some(key),
        }
    }

    pub fn read_input_source(&mut self, stdin: &mut dyn Read) -> io::Result<()> {
        let Some(source) = self.source.take() else {
            return Ok(());
        };
        let mut bytes = Vec::new();
        match source {
            InputSource::Stdin => stdin
                .take(crate::capabilities::native::MAX_NATIVE_INPUT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?,
            InputSource::File(path) => File::open(path)?
                .take(crate::capabilities::native::MAX_NATIVE_INPUT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?,
        };
        if bytes.is_empty() || bytes.len() > crate::capabilities::native::MAX_NATIVE_INPUT_BYTES {
            bytes.fill(0);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "repository input must contain 1 to 1048576 bytes",
            ));
        }
        serde_json::from_slice::<Value>(&bytes).map_err(|error| {
            bytes.fill(0);
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("repository input is not valid JSON: {error}"),
            )
        })?;
        self.body = Some(bytes);
        Ok(())
    }

    pub(crate) fn take_body(&mut self) -> Option<Vec<u8>> {
        self.body.take()
    }
}

pub trait RepoHttpClient {
    type Error;
    fn execute(&mut self, request: RepoRequest) -> Result<Value, Self::Error>;
}

pub fn execute<C: RepoHttpClient>(client: &mut C, request: RepoRequest) -> Result<Value, C::Error> {
    client.execute(request)
}

pub fn parity_table() -> String {
    let routes = REPO_ROUTES
        .iter()
        .map(|route| route.operation)
        .collect::<std::collections::BTreeSet<_>>();
    let commands = REPO_CLI_OPERATIONS
        .iter()
        .map(|operation| operation.service_operation)
        .collect::<std::collections::BTreeSet<_>>();
    let uncovered = routes.symmetric_difference(&commands).collect::<Vec<_>>();
    format!(
        "repository API/CLI parity: routes={} commands={} uncovered={} {uncovered:?}",
        routes.len(),
        commands.len(),
        uncovered.len()
    )
}

fn query(operation: &'static str, path: String) -> RepoRequest {
    RepoRequest {
        operation,
        method: Method::GET,
        path,
        body: None,
        source: None,
        idempotency_key: None,
    }
}
