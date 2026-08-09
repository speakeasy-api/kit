use std::{fmt, thread, time::Duration};

use crate::{
    api::{
        auth::contract::AuthenticatedPrincipal,
        http::core::ServiceHandler,
        service::{
            Command, CommandReceipt, PromptCommand, PromptReceipt, Query, QueryProjection,
            RequestContext, RunProjection, ServiceError,
        },
        stream::OpaqueStreamCursor,
    },
    domain::{events::TraceId, ids::RunId},
    store::sqlite::idempotency::IdempotencyKey,
};

#[derive(Clone, Debug)]
pub struct MutationRequest {
    pub operation: &'static str,
    pub resource_id: String,
    pub command: Command,
    idempotency_key: IdempotencyKey,
}

#[derive(Clone, Debug)]
pub struct PromptRequest {
    pub command: PromptCommand,
    pub wait: bool,
    idempotency_key: IdempotencyKey,
}

impl PromptRequest {
    pub fn new(command: PromptCommand, idempotency_key: IdempotencyKey) -> Self {
        Self {
            command,
            wait: false,
            idempotency_key,
        }
    }

    pub fn with_wait(mut self, wait: bool) -> Self {
        self.wait = wait;
        self
    }

    pub fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }
}

impl MutationRequest {
    pub fn new(
        command: Command,
        resource_id: impl Into<String>,
        idempotency_key: IdempotencyKey,
    ) -> Self {
        Self {
            operation: command.operation(),
            resource_id: resource_id.into(),
            command,
            idempotency_key,
        }
    }

    pub fn idempotency_key(&self) -> &IdempotencyKey {
        &self.idempotency_key
    }
}

#[derive(Clone, Debug)]
pub enum ClientRequest {
    Mutation(MutationRequest),
    Prompt(PromptRequest),
    Query {
        operation: &'static str,
        query: Query,
        stream: bool,
        stream_cursor: Option<OpaqueStreamCursor>,
    },
}

impl ClientRequest {
    pub fn operation(&self) -> &'static str {
        match self {
            Self::Mutation(request) => request.operation,
            Self::Prompt(_) => "run.start",
            Self::Query { operation, .. } => operation,
        }
    }

    pub fn is_stream(&self) -> bool {
        matches!(self, Self::Query { stream: true, .. })
    }
}

#[derive(Clone, Debug)]
pub enum ClientResponse {
    Mutation {
        resource_id: String,
        receipt: CommandReceipt,
    },
    Query(Box<QueryProjection>),
}

pub trait Client {
    fn execute(&mut self, request: &MutationRequest) -> Result<CommandReceipt, ClientError>;
    fn prompt(&mut self, _request: &PromptRequest) -> Result<PromptReceipt, ClientError> {
        Err(ClientError::internal(
            "client does not support prompt requests",
        ))
    }
    fn query(&mut self, query: Query) -> Result<QueryProjection, ClientError>;
}

pub struct EmbeddedClient<'a> {
    service: &'a dyn ServiceHandler,
    principal: AuthenticatedPrincipal,
    request_sequence: u64,
}

impl<'a> EmbeddedClient<'a> {
    pub fn new(service: &'a dyn ServiceHandler, principal: AuthenticatedPrincipal) -> Self {
        Self {
            service,
            principal,
            request_sequence: 0,
        }
    }

    fn context(
        &mut self,
        idempotency_key: Option<IdempotencyKey>,
    ) -> Result<RequestContext, ClientError> {
        self.request_sequence = self.request_sequence.wrapping_add(1);
        let trace = TraceId::parse(&format!("cli-{:016x}", self.request_sequence))
            .map_err(|error| ClientError::internal(error.to_string()))?;
        RequestContext::authenticated(Ok(self.principal.clone()), idempotency_key, trace)
            .map_err(ClientError::from)
    }
}

impl Client for EmbeddedClient<'_> {
    fn execute(&mut self, request: &MutationRequest) -> Result<CommandReceipt, ClientError> {
        let context = self.context(Some(request.idempotency_key().clone()))?;
        self.service
            .execute(&context, request.command.clone())
            .map_err(ClientError::from)
    }

    fn query(&mut self, query: Query) -> Result<QueryProjection, ClientError> {
        let context = self.context(None)?;
        self.service
            .query(&context, query)
            .map_err(ClientError::from)
    }

    fn prompt(&mut self, request: &PromptRequest) -> Result<PromptReceipt, ClientError> {
        let context = self.context(Some(request.idempotency_key().clone()))?;
        self.service
            .prompt(&context, request.command.clone())
            .map_err(ClientError::from)
    }
}

pub fn execute_with_retry(
    client: &mut dyn Client,
    request: &ClientRequest,
    attempts: usize,
) -> Result<ClientResponse, ClientError> {
    let attempts = attempts.max(1);
    for attempt in 0..attempts {
        let result = match request {
            ClientRequest::Mutation(request) => {
                client
                    .execute(request)
                    .map(|receipt| ClientResponse::Mutation {
                        resource_id: request.resource_id.clone(),
                        receipt,
                    })
            }
            ClientRequest::Prompt(request) => {
                client
                    .prompt(request)
                    .map(|result| ClientResponse::Mutation {
                        resource_id: result.run_id.to_string(),
                        receipt: result.receipt,
                    })
            }
            ClientRequest::Query { query, .. } => client
                .query(query.clone())
                .map(|projection| ClientResponse::Query(Box::new(projection))),
        };
        match result {
            Err(error) if error.retryable() && attempt + 1 < attempts => {
                thread::sleep(Duration::from_millis(25));
            }
            result => return result,
        }
    }
    unreachable!("attempt count is at least one")
}

#[cfg(test)]
mod wait_tests {
    use super::*;
    use crate::{
        domain::config::EffectiveConfigReference,
        domain::events::{ArtifactRef, RunState},
    };

    struct SequencedClient {
        states: Vec<RunState>,
        queries: usize,
    }

    impl Client for SequencedClient {
        fn execute(&mut self, _request: &MutationRequest) -> Result<CommandReceipt, ClientError> {
            Err(ClientError::internal("mutation is unused"))
        }

        fn query(&mut self, query: Query) -> Result<QueryProjection, ClientError> {
            let Query::GetRun { run_id } = query else {
                return Err(ClientError::internal("unexpected query"));
            };
            let state = self.states[self.queries.min(self.states.len() - 1)];
            self.queries += 1;
            Ok(QueryProjection::Run(RunProjection {
                id: run_id,
                thread_id: crate::domain::ids::ThreadId::parse(
                    "thread_00000000000000000000000001",
                )
                .unwrap(),
                state,
                input: ArtifactRef::parse(&format!("blake3:{}", "a".repeat(64))).unwrap(),
                auth_granted: None,
                effective_config: EffectiveConfigReference {
                    digest: "digest".to_owned(),
                    experiment_identity: "identity".to_owned(),
                    experiment_digest: "digest".to_owned(),
                    provenance: Default::default(),
                },
                owner: None,
                output: None,
                failure: None,
                version: self.queries as u64,
            }))
        }
    }

    #[test]
    fn wait_polls_through_non_terminal_and_waiting_states() {
        let mut client = SequencedClient {
            states: vec![
                RunState::Queued,
                RunState::Running,
                RunState::WaitingForApproval,
                RunState::Completed,
            ],
            queries: 0,
        };
        let run = wait_for_terminal_run(
            &mut client,
            RunId::parse("run_00000000000000000000000001").unwrap(),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(run.state, RunState::Completed);
        assert_eq!(client.queries, 4);
    }

    #[test]
    fn wait_returns_failed_runs_as_success_with_failed_state() {
        let mut client = SequencedClient {
            states: vec![RunState::Running, RunState::Failed],
            queries: 0,
        };
        let run = wait_for_terminal_run(
            &mut client,
            RunId::parse("run_00000000000000000000000001").unwrap(),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(run.state, RunState::Failed);
    }
}

/// Poll a run until it reaches a terminal state and return the final
/// projection. Waiting states are not terminal: a parked run keeps the wait
/// alive until an approval/auth/input resolution moves it on.
pub fn wait_for_terminal_run(
    client: &mut dyn Client,
    run_id: RunId,
    poll_interval: Duration,
) -> Result<RunProjection, ClientError> {
    loop {
        let query = Query::GetRun { run_id };
        let request = ClientRequest::Query {
            operation: query.operation(),
            query,
            stream: false,
            stream_cursor: None,
        };
        let ClientResponse::Query(projection) = execute_with_retry(client, &request, 3)? else {
            return Err(ClientError::internal("run query returned a mutation"));
        };
        let QueryProjection::Run(run) = *projection else {
            return Err(ClientError::internal("run query returned a foreign projection"));
        };
        if run.state.is_terminal() {
            return Ok(run);
        }
        thread::sleep(poll_interval);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientErrorKind {
    Authentication,
    NotFound,
    Conflict,
    Invalid,
    Unavailable,
    Timeout,
    Internal,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientError {
    pub kind: ClientErrorKind,
    pub message: String,
    pub code: Option<String>,
}

impl ClientError {
    pub fn new(kind: ClientErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            code: None,
        }
    }

    pub(crate) fn problem(kind: ClientErrorKind, message: impl Into<String>, code: String) -> Self {
        Self {
            kind,
            message: message.into(),
            code: Some(code),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ClientErrorKind::Unavailable, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(ClientErrorKind::Timeout, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ClientErrorKind::Internal, message)
    }

    pub fn retryable(&self) -> bool {
        matches!(
            self.kind,
            ClientErrorKind::Unavailable | ClientErrorKind::Timeout
        )
    }
}

impl From<ServiceError> for ClientError {
    fn from(error: ServiceError) -> Self {
        let message = error.to_string();
        let kind = match error {
            ServiceError::Authentication(_) => ClientErrorKind::Authentication,
            ServiceError::MissingIdempotencyKey | ServiceError::Invalid(_) => {
                ClientErrorKind::Invalid
            }
            ServiceError::NotFound => ClientErrorKind::NotFound,
            ServiceError::Conflict(_) => ClientErrorKind::Conflict,
            ServiceError::Store(_) => ClientErrorKind::Internal,
        };
        Self::new(kind, message)
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClientError {}
