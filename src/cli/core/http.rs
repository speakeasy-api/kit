use std::{
    io::Read,
    net::{IpAddr, SocketAddr},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{
    Client as StreamClient, Method, StatusCode, Url,
    blocking::{Client as RequestClient, RequestBuilder, Response},
    header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderValue, ORIGIN},
    redirect::Policy,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    api::{
        http::{
            core::{JSON_BODY_LIMIT, ROUTES, RouteDescriptor, decode_cursor, encode_cursor},
            errors::PROBLEM_MEDIA_TYPE,
        },
        service::{
            ApprovalProjection, ArtifactMetadataProjection, AuthRequestProjection,
            CapabilityProjection, Command, CommandReceipt, CursorStatusProjection, EventCursor,
            EventPage, EventProjection, PromptInput, PromptReceipt, Query, QueryProjection,
            RunProjection, StatusProjection, ThreadProjection,
        },
        stream::OpaqueStreamCursor,
    },
    domain::crypto::{hmac_sha256_domain, sha256},
    domain::events::CommitPosition,
    domain::ids::{ProjectId, RunId},
};

use super::{
    Client, ClientError, ClientErrorKind, DaemonDiscovery, MutationRequest, PromptRequest,
};

const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
pub const DEFAULT_FOLLOW_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);
type Parameters = Vec<(&'static str, String)>;

pub fn operation_route(operation: &str) -> Option<&'static RouteDescriptor> {
    ROUTES
        .iter()
        .chain(crate::api::http::exec::EXEC_ROUTES)
        .chain(crate::api::http::repo::REPO_ROUTES)
        .find(|route| route.operation == operation)
}

pub struct HttpClient {
    client: RequestClient,
    stream_client: StreamClient,
    endpoint: String,
    host: String,
    credential: HeaderValue,
    signing_key: [u8; 32],
    timeout: Duration,
    follow_inactivity_timeout: Duration,
}

impl HttpClient {
    pub fn connect(discovery: &DaemonDiscovery, timeout: Duration) -> Result<Self, ClientError> {
        Self::connect_with_follow_inactivity_timeout(
            discovery,
            timeout,
            DEFAULT_FOLLOW_INACTIVITY_TIMEOUT,
        )
    }

    pub fn connect_with_follow_inactivity_timeout(
        discovery: &DaemonDiscovery,
        timeout: Duration,
        follow_inactivity_timeout: Duration,
    ) -> Result<Self, ClientError> {
        if follow_inactivity_timeout.is_zero() {
            return Err(ClientError::new(
                ClientErrorKind::Invalid,
                "follow inactivity timeout must be greater than zero",
            ));
        }
        let endpoint = endpoint(&discovery.endpoint)?;
        let host = endpoint
            .socket_addrs(|| None)
            .map_err(|error| ClientError::new(ClientErrorKind::Invalid, error.to_string()))?
            .into_iter()
            .next()
            .ok_or_else(|| ClientError::new(ClientErrorKind::Invalid, "missing daemon address"))?
            .to_string();
        let mut credential = HeaderValue::from_str(&format!("Bearer {}", discovery.credential))
            .map_err(|_| ClientError::new(ClientErrorKind::Invalid, "invalid daemon credential"))?;
        credential.set_sensitive(true);
        let signing_key = sha256(discovery.credential.as_bytes());
        let client = RequestClient::builder()
            .connect_timeout(timeout.min(Duration::from_secs(2)))
            .timeout(timeout)
            .redirect(Policy::none())
            .no_proxy()
            .referer(false)
            .build()
            .map_err(request_error)?;
        let stream_client = StreamClient::builder()
            .connect_timeout(timeout.min(Duration::from_secs(2)))
            .redirect(Policy::none())
            .no_proxy()
            .referer(false)
            .build()
            .map_err(request_error)?;
        let client = Self {
            client,
            stream_client,
            endpoint: discovery.endpoint.clone(),
            host,
            credential,
            signing_key,
            timeout,
            follow_inactivity_timeout,
        };
        client.ready()?;
        Ok(client)
    }

    fn ready(&self) -> Result<(), ClientError> {
        let response = self.send(self.request(Method::GET, "/health/ready")?)?;
        if response.status() == StatusCode::OK {
            Ok(())
        } else {
            Err(problem(response))
        }
    }

    fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, ClientError> {
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce)
            .map_err(|_| ClientError::internal("secure randomness unavailable"))?;
        let nonce = nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let timestamp = request_timestamp()?;
        let signature = request_signature(&self.signing_key, timestamp, nonce.as_bytes());
        Ok(self
            .client
            .request(method, format!("{}{path}", self.endpoint))
            .header(HOST, &self.host)
            .header(ORIGIN, &self.endpoint)
            .header(AUTHORIZATION, self.credential.clone())
            .header("x-kit-nonce", nonce)
            .header("x-kit-timestamp", timestamp)
            .header("x-kit-signature", signature)
            .header(ACCEPT, format!("application/json, {PROBLEM_MEDIA_TYPE}")))
    }

    fn send(&self, request: RequestBuilder) -> Result<Response, ClientError> {
        request.send().map_err(request_error)
    }

    pub fn follow(
        &mut self,
        query: &Query,
        initial_cursor: Option<&OpaqueStreamCursor>,
        mut emit: impl FnMut(&[u8]) -> Result<(), ClientError>,
    ) -> Result<(), ClientError> {
        let (project_id, path) = self.stream_target(query)?;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| ClientError::internal(error.to_string()))?
            .block_on(self.follow_stream(project_id, &path, initial_cursor, &mut emit))
    }

    async fn follow_stream(
        &self,
        project_id: ProjectId,
        path: &str,
        initial_cursor: Option<&OpaqueStreamCursor>,
        emit: &mut impl FnMut(&[u8]) -> Result<(), ClientError>,
    ) -> Result<(), ClientError> {
        let mut cursor = initial_cursor.cloned();

        loop {
            let mut request = self
                .stream_request(Method::GET, path)?
                .header(ACCEPT, crate::api::stream::SSE_MEDIA_TYPE);
            if let Some(cursor) = &cursor {
                request = request.header("last-event-id", cursor.as_str());
            }
            let mut response = tokio::time::timeout(self.timeout, request.send())
                .await
                .map_err(|_| ClientError::timeout("event stream connection timed out"))?
                .map_err(request_error)?;
            if response.status() == StatusCode::GONE {
                let recovery = stream_recovery_async(response, self.timeout).await?;
                cursor = Some(recovery.new_cursor.clone());
                emit_jsonl(
                    json!({
                        "event": "stream.snapshot",
                        "cursor": recovery.new_cursor.as_str(),
                        "snapshot": recovery.snapshot,
                    }),
                    emit,
                )?;
                continue;
            }
            if !response.status().is_success() {
                return Err(problem_async(response, self.timeout).await);
            }
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split(';').next())
                .map(str::trim);
            if content_type != Some(crate::api::stream::SSE_MEDIA_TYPE) {
                return Err(ClientError::internal(
                    "daemon returned a non-SSE stream response",
                ));
            }
            if cursor.is_none() {
                cursor = response
                    .headers()
                    .get("x-kit-cursor")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| OpaqueStreamCursor::parse(value.to_owned()).ok());
            }

            let mut frame = SseFields::default();
            let mut pending = Vec::new();
            let mut disconnected = false;
            let mut inactivity_deadline =
                tokio::time::Instant::now() + self.follow_inactivity_timeout;
            loop {
                let remaining =
                    inactivity_deadline.saturating_duration_since(tokio::time::Instant::now());
                let chunk = tokio::time::timeout(remaining, response.chunk())
                    .await
                    .map_err(|_| ClientError::timeout("event stream inactivity timeout"))?
                    .map_err(request_error)?;
                let Some(chunk) = chunk else {
                    break;
                };
                pending.extend_from_slice(&chunk);
                while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                    let mut line = pending.drain(..=newline).collect::<Vec<_>>();
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    let line = std::str::from_utf8(&line)
                        .map_err(|_| ClientError::internal("invalid UTF-8 in SSE stream"))?;
                    let (disconnect, activity) =
                        process_sse_line(line, &mut frame, project_id, &mut cursor, emit)?;
                    if activity {
                        inactivity_deadline =
                            tokio::time::Instant::now() + self.follow_inactivity_timeout;
                    }
                    if disconnect {
                        disconnected = true;
                        break;
                    }
                }
                if disconnected {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn stream_request(
        &self,
        method: Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, ClientError> {
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce)
            .map_err(|_| ClientError::internal("secure randomness unavailable"))?;
        let nonce = nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let timestamp = request_timestamp()?;
        let signature = request_signature(&self.signing_key, timestamp, nonce.as_bytes());
        Ok(self
            .stream_client
            .request(method, format!("{}{path}", self.endpoint))
            .header(HOST, &self.host)
            .header(ORIGIN, &self.endpoint)
            .header(AUTHORIZATION, self.credential.clone())
            .header("x-kit-nonce", nonce)
            .header("x-kit-timestamp", timestamp)
            .header("x-kit-signature", signature))
    }

    fn stream_target(&mut self, query: &Query) -> Result<(ProjectId, String), ClientError> {
        let (thread_id, path) = match query {
            Query::ThreadEvents { thread_id, .. } => {
                (*thread_id, format!("/v1/threads/{thread_id}/events"))
            }
            Query::RunTimeline { run_id, .. } => {
                let thread_id =
                    match <Self as Client>::query(self, Query::GetRun { run_id: *run_id })? {
                        QueryProjection::Run(run) => run.thread_id,
                        _ => return Err(ClientError::internal("daemon returned an invalid run")),
                    };
                (thread_id, format!("/v1/runs/{run_id}/events"))
            }
            _ => return Err(ClientError::internal("query is not an event stream")),
        };
        match <Self as Client>::query(self, Query::GetThread { thread_id })? {
            QueryProjection::Thread(thread) => Ok((thread.project_id, path)),
            _ => Err(ClientError::internal("daemon returned an invalid thread")),
        }
    }
}

fn request_timestamp() -> Result<u64, ClientError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| ClientError::internal("system clock is before the Unix epoch"))
}

fn request_signature(key: &[u8; 32], timestamp: u64, nonce: &[u8]) -> String {
    hmac_sha256_domain(
        key,
        b"KIT-LOOPBACK-REQUEST-V1\0",
        &[&timestamp.to_be_bytes(), nonce],
    )
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect()
}

#[derive(Default)]
struct SseFields {
    id: Option<String>,
    event: Option<String>,
    data: String,
}

fn process_sse_line(
    line: &str,
    frame: &mut SseFields,
    project_id: ProjectId,
    cursor: &mut Option<OpaqueStreamCursor>,
    emit: &mut impl FnMut(&[u8]) -> Result<(), ClientError>,
) -> Result<(bool, bool), ClientError> {
    if line.is_empty() {
        let has_frame = frame.event.is_some() || frame.id.is_some() || !frame.data.is_empty();
        dispatch_sse_frame(frame, project_id, cursor, emit)
            .map(|disconnect| (disconnect, has_frame))
    } else if line.starts_with(':') {
        emit_jsonl(json!({ "event": "heartbeat" }), emit)?;
        Ok((false, true))
    } else {
        if let Some(value) = line.strip_prefix("id:") {
            frame.id = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("event:") {
            frame.event = Some(value.trim_start().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            if !frame.data.is_empty() {
                frame.data.push('\n');
            }
            frame.data.push_str(value.trim_start());
        }
        Ok((false, false))
    }
}

fn dispatch_sse_frame(
    frame: &mut SseFields,
    project_id: ProjectId,
    cursor: &mut Option<OpaqueStreamCursor>,
    emit: &mut impl FnMut(&[u8]) -> Result<(), ClientError>,
) -> Result<bool, ClientError> {
    if frame.event.is_none() && frame.id.is_none() && frame.data.is_empty() {
        return Ok(false);
    }
    let event = frame.event.take().unwrap_or_else(|| "message".to_owned());
    let id = frame.id.take();
    let data = std::mem::take(&mut frame.data);
    let mut value = serde_json::from_str::<Value>(&data)
        .map_err(|error| ClientError::internal(format!("invalid SSE data: {error}")))?;
    if event == "stream.disconnect" {
        if let Some(next) = value.get("cursor").and_then(Value::as_str) {
            *cursor = Some(
                OpaqueStreamCursor::parse(next.to_owned())
                    .map_err(|_| ClientError::internal("daemon returned an invalid cursor"))?,
            );
        }
        if let Value::Object(object) = &mut value {
            object.insert("event".to_owned(), Value::String(event));
        }
        emit_jsonl(value, emit)?;
        return Ok(true);
    }
    let id = id.ok_or_else(|| ClientError::internal("SSE event is missing an id"))?;
    let cursor_id = OpaqueStreamCursor::parse(id.clone())
        .map_err(|_| ClientError::internal("daemon returned an invalid cursor"))?;
    *cursor = Some(cursor_id);
    let object = value
        .as_object_mut()
        .ok_or_else(|| ClientError::internal("SSE data is not an object"))?;
    object.insert("cursor".to_owned(), Value::String(id));
    object.insert("project_id".to_owned(), json!(project_id));
    emit_jsonl(value, emit)?;
    Ok(false)
}

fn emit_jsonl(
    value: Value,
    emit: &mut impl FnMut(&[u8]) -> Result<(), ClientError>,
) -> Result<(), ClientError> {
    let mut line =
        serde_json::to_vec(&value).map_err(|error| ClientError::internal(error.to_string()))?;
    line.push(b'\n');
    emit(&line)
}

async fn stream_recovery_async(
    response: reqwest::Response,
    timeout: Duration,
) -> Result<StreamRecovery, ClientError> {
    let status = response.status();
    let bytes = response_bytes_async(response, timeout).await?;
    let recovery: StreamRecovery = serde_json::from_slice(&bytes)
        .map_err(|error| ClientError::internal(format!("invalid daemon response: {error}")))?;
    if recovery.status != status.as_u16() || recovery.code != "cursor_expired" {
        return Err(ClientError::internal(
            "daemon returned an invalid cursor recovery response",
        ));
    }
    Ok(recovery)
}

impl Client for HttpClient {
    fn execute(&mut self, request: &MutationRequest) -> Result<CommandReceipt, ClientError> {
        let (path, body) = command_wire(&request.command)?;
        let bytes =
            serde_json::to_vec(&body).map_err(|error| ClientError::internal(error.to_string()))?;
        if bytes.len() > JSON_BODY_LIMIT {
            return Err(ClientError::new(
                ClientErrorKind::Invalid,
                "request body exceeds the public API limit",
            ));
        }
        let response = self.send(
            self.request(Method::POST, &path)?
                .header(CONTENT_TYPE, "application/json")
                .header("idempotency-key", request.idempotency_key().as_str())
                .body(bytes),
        )?;
        if !response.status().is_success() {
            return Err(problem(response));
        }
        let response: ResourceReceipt = json_response(response)?;
        if response.resource.id != request.resource_id
            || response.receipt.operation != request.operation
        {
            return Err(ClientError::internal(
                "daemon returned a mismatched resource receipt",
            ));
        }
        Ok(CommandReceipt {
            operation: request.operation,
            commit_positions: response.receipt.commit_positions,
            replayed: response.receipt.replayed,
        })
    }

    fn query(&mut self, query: Query) -> Result<QueryProjection, ClientError> {
        let path = query_path(&query)?;
        let response = self.send(self.request(Method::GET, &path)?)?;
        if !response.status().is_success() {
            return Err(problem(response));
        }
        query_response(query, response)
    }

    fn prompt(&mut self, request: &PromptRequest) -> Result<PromptReceipt, ClientError> {
        let mut body = json!({});
        if let Some(run_id) = request.command.run_id {
            body["run_id"] = json!(run_id);
        }
        if let Some(run_config) = &request.command.run_config {
            body["run_config"] = json!(run_config);
        }
        if let Some(experiment_config) = &request.command.experiment_config {
            body["experiment_config"] = json!(experiment_config);
        }
        match &request.command.input {
            PromptInput::Message(message) => body["message"] = json!(message),
            PromptInput::Artifact(reference) => body["artifact_ref"] = json!(reference),
        }
        let path = route_path(
            "run.start",
            vec![("thread_id", request.command.thread_id.to_string())],
        )?;
        let bytes =
            serde_json::to_vec(&body).map_err(|error| ClientError::internal(error.to_string()))?;
        if bytes.len() > JSON_BODY_LIMIT {
            return Err(ClientError::new(
                ClientErrorKind::Invalid,
                "request body exceeds the public API limit",
            ));
        }
        let response = self.send(
            self.request(Method::POST, &path)?
                .header(CONTENT_TYPE, "application/json")
                .header("idempotency-key", request.idempotency_key().as_str())
                .body(bytes),
        )?;
        if !response.status().is_success() {
            return Err(problem(response));
        }
        let response: ResourceReceipt = json_response(response)?;
        let run_id = RunId::parse(&response.resource.id)
            .map_err(|_| ClientError::internal("daemon returned an invalid run receipt"))?;
        if request
            .command
            .run_id
            .is_some_and(|expected| expected != run_id)
            || response.receipt.operation != "run.start"
        {
            return Err(ClientError::internal(
                "daemon returned a mismatched resource receipt",
            ));
        }
        Ok(PromptReceipt {
            run_id,
            receipt: CommandReceipt {
                operation: "run.start",
                commit_positions: response.receipt.commit_positions,
                replayed: response.receipt.replayed,
            },
        })
    }
}

impl crate::cli::exec::ExecHttpClient for HttpClient {
    type Error = ClientError;

    fn execute(
        &mut self,
        mut request: crate::cli::exec::ExecRequest,
    ) -> Result<Value, Self::Error> {
        let mut builder = self.request(request.method.clone(), &request.path)?;
        if let Some(key) = &request.idempotency_key {
            builder = builder.header("idempotency-key", key.as_str());
        }
        if let Some(mut bytes) = request
            .take_body_bytes()
            .map_err(|error| ClientError::internal(error.to_string()))?
        {
            if bytes.len() > JSON_BODY_LIMIT {
                bytes.fill(0);
                return Err(ClientError::new(
                    ClientErrorKind::Invalid,
                    "request body exceeds the public API limit",
                ));
            }
            builder = builder.header(CONTENT_TYPE, "application/json").body(bytes);
        }
        let response = self.send(builder)?;
        if !response.status().is_success() {
            return Err(problem(response));
        }
        json_response(response)
    }
}

impl crate::cli::repo::RepoHttpClient for HttpClient {
    type Error = ClientError;

    fn execute(
        &mut self,
        mut request: crate::cli::repo::RepoRequest,
    ) -> Result<Value, Self::Error> {
        let mut builder = self.request(request.method.clone(), &request.path)?;
        if let Some(key) = &request.idempotency_key {
            builder = builder.header("idempotency-key", key.as_str());
        }
        if let Some(mut bytes) = request.take_body() {
            if bytes.len() > crate::capabilities::native::MAX_NATIVE_INPUT_BYTES {
                bytes.fill(0);
                return Err(ClientError::new(
                    ClientErrorKind::Invalid,
                    "request body exceeds the public repository API limit",
                ));
            }
            builder = builder.header(CONTENT_TYPE, "application/json").body(bytes);
        }
        let response = self.send(builder)?;
        if !response.status().is_success() {
            return Err(problem(response));
        }
        if request.operation == "repo.artifact" {
            let media_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_owned();
            let manifest_field = |name: &str| {
                response
                    .headers()
                    .get(name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            };
            let digest = manifest_field("x-kit-artifact-digest");
            let class = manifest_field("x-kit-artifact-class");
            let principal = manifest_field("x-kit-artifact-principal");
            let project = manifest_field("x-kit-artifact-project");
            let bytes = response_bytes(response)?;
            return Ok(json!({
                "digest": digest,
                "media_type": media_type,
                "class": class,
                "principal_id": principal,
                "project_id": project,
                "bytes": bytes,
            }));
        }
        json_response(response)
    }
}

fn endpoint(value: &str) -> Result<Url, ClientError> {
    let url = Url::parse(value)
        .map_err(|error| ClientError::new(ClientErrorKind::Invalid, error.to_string()))?;
    let ip = url
        .host_str()
        .and_then(|host| host.parse::<IpAddr>().ok())
        .filter(IpAddr::is_loopback);
    if url.scheme() != "http"
        || ip.is_none()
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ClientError::new(
            ClientErrorKind::Invalid,
            "daemon endpoint must be canonical loopback HTTP",
        ));
    }
    let canonical = format!(
        "http://{}",
        SocketAddr::new(
            ip.expect("checked above"),
            url.port().expect("checked above")
        )
    );
    if canonical != value {
        return Err(ClientError::new(
            ClientErrorKind::Invalid,
            "daemon endpoint must be canonical loopback HTTP",
        ));
    }
    Ok(url)
}

fn command_wire(command: &Command) -> Result<(String, Value), ClientError> {
    let (parameters, body): (Parameters, Value) = match command {
        Command::CreateProject { project_id, .. } => (vec![], json!({ "id": project_id })),
        Command::SetProjectRetention {
            project_id,
            policy,
            expected_version,
            ..
        } => (
            vec![("project_id", project_id.to_string())],
            json!({ "policy": policy, "expected_version": expected_version }),
        ),
        Command::CreateThread {
            thread_id,
            project_id,
            ..
        } => (
            vec![("project_id", project_id.to_string())],
            json!({ "id": thread_id }),
        ),
        Command::SetThreadArchived {
            thread_id,
            archived,
            expected_version,
            ..
        } => (
            vec![("thread_id", thread_id.to_string())],
            json!({ "archived": archived, "expected_version": expected_version }),
        ),
        Command::InitiateThreadDeletion {
            thread_id,
            expected_version,
            ..
        } => (
            vec![("thread_id", thread_id.to_string())],
            json!({ "expected_version": expected_version }),
        ),
        Command::StartRun {
            run_id,
            thread_id,
            input,
            run_config,
            experiment_config,
            ..
        } => (
            vec![("thread_id", thread_id.to_string())],
            json!({
                "id": run_id,
                "input": input,
                "run_config": run_config,
                "experiment_config": experiment_config,
            }),
        ),
        Command::CancelRun {
            run_id,
            expected_version,
            ..
        } => (
            vec![("run_id", run_id.to_string())],
            json!({ "expected_version": expected_version }),
        ),
        Command::ProvideRunInput {
            run_id,
            input,
            expected_version,
            ..
        } => (
            vec![("run_id", run_id.to_string())],
            json!({ "input": input, "expected_version": expected_version }),
        ),
        Command::ResolveApproval {
            approval_id,
            decision,
            expected_version,
            ..
        } => (
            vec![("approval_id", approval_id.to_string())],
            json!({ "decision": decision, "expected_version": expected_version }),
        ),
        Command::ResolveAuth {
            run_id,
            granted,
            expected_version,
            ..
        } => (
            vec![("run_id", run_id.to_string())],
            json!({ "granted": granted, "expected_version": expected_version }),
        ),
        Command::RegisterArtifactMetadata {
            artifact_id,
            project_id,
            reference,
            media_type,
            size,
            ..
        } => (
            vec![("project_id", project_id.to_string())],
            json!({
                "id": artifact_id,
                "reference": reference,
                "media_type": media_type,
                "size": size,
            }),
        ),
        _ => {
            return Err(ClientError::internal(
                "command has no public CLI HTTP route",
            ));
        }
    };
    Ok((route_path(command.operation(), parameters)?, body))
}

fn query_path(query: &Query) -> Result<String, ClientError> {
    let (parameters, query_parameters): (Parameters, Parameters) = match query {
        Query::GetProject { project_id } | Query::GetProjectRetention { project_id } => {
            (vec![("project_id", project_id.to_string())], vec![])
        }
        Query::ListThreads { project_id }
        | Query::ListRuns { project_id }
        | Query::PendingApprovals { project_id }
        | Query::PendingAuthRequests { project_id }
        | Query::ListCapabilities { project_id }
        | Query::Status { project_id } => (vec![("project_id", project_id.to_string())], vec![]),
        Query::GetThread { thread_id } => (vec![("thread_id", thread_id.to_string())], vec![]),
        Query::GetDeletionJob { deletion_job_id } => {
            (vec![("deletion_job_id", deletion_job_id.clone())], vec![])
        }
        Query::ThreadEvents {
            thread_id,
            after,
            limit,
        } => (
            vec![("thread_id", thread_id.to_string())],
            vec![
                ("cursor", encode_cursor(after.position())),
                ("limit", limit.to_string()),
            ],
        ),
        Query::GetRun { run_id }
        | Query::GetRunCost { run_id }
        | Query::GetRunPrompts { run_id }
        | Query::RunTranscript { run_id } => (vec![("run_id", run_id.to_string())], vec![]),
        Query::RunTimeline {
            run_id,
            after,
            limit,
        } => (
            vec![("run_id", run_id.to_string())],
            vec![
                ("cursor", encode_cursor(after.position())),
                ("limit", limit.to_string()),
            ],
        ),
        Query::GetArtifactMetadata { artifact_id } => {
            (vec![("artifact_id", artifact_id.to_string())], vec![])
        }
        Query::EventCursorStatus { project_id, cursor } => (
            vec![("project_id", project_id.to_string())],
            vec![("cursor", encode_cursor(cursor.position()))],
        ),
        Query::GetAttempt { .. } => {
            return Err(ClientError::internal("query has no public CLI HTTP route"));
        }
    };
    let path = route_path(query.operation(), parameters)?;
    if query_parameters.is_empty() {
        return Ok(path);
    }
    let mut url = Url::parse(&format!("http://localhost{path}"))
        .map_err(|error| ClientError::internal(error.to_string()))?;
    url.query_pairs_mut().extend_pairs(query_parameters);
    Ok(match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    })
}

fn route_path(operation: &str, parameters: Parameters) -> Result<String, ClientError> {
    let route = operation_route(operation)
        .ok_or_else(|| ClientError::internal("operation has no public HTTP route"))?;
    let mut path = route.path.to_owned();
    for (name, value) in parameters {
        path = path.replace(&format!("{{{name}}}"), &encode_segment(&value)?);
    }
    if path.contains('{') {
        return Err(ClientError::internal(
            "HTTP route parameter was not supplied",
        ));
    }
    Ok(path)
}

fn encode_segment(value: &str) -> Result<String, ClientError> {
    let mut url = Url::parse("http://localhost/")
        .map_err(|error| ClientError::internal(error.to_string()))?;
    url.path_segments_mut()
        .map_err(|_| ClientError::internal("failed to encode HTTP route parameter"))?
        .push(value);
    Ok(url.path().trim_start_matches('/').to_owned())
}

fn query_response(query: Query, response: Response) -> Result<QueryProjection, ClientError> {
    Ok(match query {
        Query::GetProject { .. } => QueryProjection::Project(json_response(response)?),
        Query::GetProjectRetention { .. } => {
            let response: EffectiveRetention = json_response(response)?;
            QueryProjection::Retention(Some(response.effective))
        }
        Query::ListThreads { .. } => {
            QueryProjection::Threads(json_response::<Items<ThreadProjection>>(response)?.items)
        }
        Query::GetThread { .. } => QueryProjection::Thread(json_response(response)?),
        Query::GetDeletionJob { .. } => QueryProjection::DeletionJob(json_response(response)?),
        Query::ThreadEvents { .. } | Query::RunTimeline { .. } => {
            let page: WireEventPage = json_response(response)?;
            QueryProjection::Events(page.try_into()?)
        }
        Query::ListRuns { .. } => {
            QueryProjection::Runs(json_response::<Items<RunProjection>>(response)?.items)
        }
        Query::GetRun { .. } => QueryProjection::Run(json_response(response)?),
        Query::GetRunCost { .. } => QueryProjection::RunCost(Box::new(json_response(response)?)),
        Query::GetRunPrompts { .. } => QueryProjection::RunPrompts(json_response(response)?),
        Query::RunTranscript { .. } => QueryProjection::RunTranscript(json_response(response)?),
        Query::PendingApprovals { .. } => {
            QueryProjection::Approvals(json_response::<Items<ApprovalProjection>>(response)?.items)
        }
        Query::PendingAuthRequests { .. } => QueryProjection::AuthRequests(
            json_response::<Items<AuthRequestProjection>>(response)?.items,
        ),
        Query::GetArtifactMetadata { .. } => QueryProjection::ArtifactMetadata(json_response::<
            ArtifactMetadataProjection,
        >(response)?),
        Query::ListCapabilities { .. } => QueryProjection::Capabilities(
            json_response::<Items<CapabilityProjection>>(response)?.items,
        ),
        Query::EventCursorStatus { .. } => {
            let value: WireCursorStatus = json_response(response)?;
            QueryProjection::CursorStatus(CursorStatusProjection {
                requested: cursor(&value.requested)?,
                committed: cursor(&value.committed)?,
                caught_up: value.caught_up,
            })
        }
        Query::Status { .. } => {
            let value: WireStatus = json_response(response)?;
            QueryProjection::Status(StatusProjection {
                committed: cursor(&value.committed)?,
                ready: value.ready,
            })
        }
        Query::GetAttempt { .. } => {
            return Err(ClientError::internal("query has no public CLI HTTP route"));
        }
    })
}

fn cursor(value: &str) -> Result<EventCursor, ClientError> {
    decode_cursor(value)
        .map(EventCursor::new)
        .ok_or_else(|| ClientError::internal("daemon returned an invalid cursor"))
}

fn json_response<T: DeserializeOwned>(response: Response) -> Result<T, ClientError> {
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if content_type != Some("application/json") {
        return Err(ClientError::internal(
            "daemon returned a non-JSON success response",
        ));
    }
    let bytes = response_bytes(response)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| ClientError::internal(format!("invalid daemon response: {error}")))
}

fn problem(response: Response) -> ClientError {
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    let parsed = (content_type == Some(PROBLEM_MEDIA_TYPE))
        .then(|| response_bytes(response).ok())
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<ProblemDetails>(&bytes).ok())
        .filter(|problem| problem.status == status.as_u16());
    let message = parsed
        .map(|problem| {
            let _ = (
                problem.problem_type,
                problem.title,
                problem.instance,
                problem.code,
            );
            problem.detail
        })
        .unwrap_or_else(|| format!("daemon returned HTTP {}", status.as_u16()));
    let kind = match status.as_u16() {
        401 | 403 => ClientErrorKind::Authentication,
        404 => ClientErrorKind::NotFound,
        409 | 423 => ClientErrorKind::Conflict,
        400..=499 => ClientErrorKind::Invalid,
        503 => ClientErrorKind::Unavailable,
        504 => ClientErrorKind::Timeout,
        _ => ClientErrorKind::Internal,
    };
    ClientError::new(kind, message)
}

async fn problem_async(response: reqwest::Response, timeout: Duration) -> ClientError {
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    let parsed = if content_type == Some(PROBLEM_MEDIA_TYPE) {
        response_bytes_async(response, timeout)
            .await
            .ok()
            .and_then(|bytes| serde_json::from_slice::<ProblemDetails>(&bytes).ok())
            .filter(|problem| problem.status == status.as_u16())
    } else {
        None
    };
    let message = parsed
        .map(|problem| {
            let _ = (
                problem.problem_type,
                problem.title,
                problem.instance,
                problem.code,
            );
            problem.detail
        })
        .unwrap_or_else(|| format!("daemon returned HTTP {}", status.as_u16()));
    let kind = match status.as_u16() {
        401 | 403 => ClientErrorKind::Authentication,
        404 => ClientErrorKind::NotFound,
        409 | 423 => ClientErrorKind::Conflict,
        400..=499 => ClientErrorKind::Invalid,
        503 => ClientErrorKind::Unavailable,
        504 => ClientErrorKind::Timeout,
        _ => ClientErrorKind::Internal,
    };
    ClientError::new(kind, message)
}

fn response_bytes(mut response: Response) -> Result<Vec<u8>, ClientError> {
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err(ClientError::internal(
            "daemon response exceeds the client limit",
        ));
    }
    let mut bytes = Vec::new();
    response
        .by_ref()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut {
                ClientError::timeout("daemon response timed out")
            } else {
                ClientError::unavailable(format!("daemon response failed: {error}"))
            }
        })?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(ClientError::internal(
            "daemon response exceeds the client limit",
        ));
    }
    Ok(bytes)
}

async fn response_bytes_async(
    mut response: reqwest::Response,
    timeout: Duration,
) -> Result<Vec<u8>, ClientError> {
    if response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err(ClientError::internal(
            "daemon response exceeds the client limit",
        ));
    }
    tokio::time::timeout(timeout, async {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(request_error)? {
            if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES as usize {
                return Err(ClientError::internal(
                    "daemon response exceeds the client limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| ClientError::timeout("daemon response timed out"))?
}

fn request_error(error: reqwest::Error) -> ClientError {
    if error.is_timeout() {
        ClientError::timeout("daemon request timed out")
    } else if error.is_connect() || error.is_request() || error.is_body() {
        ClientError::unavailable(format!("daemon transport failed: {error}"))
    } else {
        ClientError::internal(format!("daemon transport failed: {error}"))
    }
}

#[derive(Deserialize)]
struct ResourceReceipt {
    resource: Resource,
    receipt: WireReceipt,
}

#[derive(Deserialize)]
struct Resource {
    id: String,
}

#[derive(Deserialize)]
struct WireReceipt {
    operation: String,
    commit_positions: Vec<CommitPosition>,
    replayed: bool,
}

#[derive(Deserialize)]
struct Items<T> {
    items: Vec<T>,
}

#[derive(Deserialize)]
struct EffectiveRetention {
    effective: crate::api::service::RetentionPolicy,
}

#[derive(Deserialize)]
struct WireEventPage {
    items: Vec<WireEvent>,
    next_cursor: String,
}

#[derive(Deserialize)]
struct WireEvent {
    cursor: String,
    project_id: crate::domain::ids::ProjectId,
    operation: String,
    stream: String,
    payload: Value,
}

impl TryFrom<WireEventPage> for EventPage {
    type Error = ClientError;

    fn try_from(value: WireEventPage) -> Result<Self, Self::Error> {
        let events = value
            .items
            .into_iter()
            .map(|event| {
                Ok(EventProjection {
                    cursor: cursor(&event.cursor)?,
                    project_id: event.project_id,
                    operation: event.operation,
                    stream: event.stream,
                    payload: serde_json::to_vec(&event.payload)
                        .map_err(|error| ClientError::internal(error.to_string()))?,
                })
            })
            .collect::<Result<Vec<_>, ClientError>>()?;
        Ok(Self {
            events,
            next_cursor: cursor(&value.next_cursor)?,
        })
    }
}

#[derive(Deserialize)]
struct WireCursorStatus {
    requested: String,
    committed: String,
    caught_up: bool,
}

#[derive(Deserialize)]
struct WireStatus {
    committed: String,
    ready: bool,
}

#[derive(Deserialize)]
struct ProblemDetails {
    #[serde(rename = "type")]
    problem_type: String,
    title: String,
    status: u16,
    detail: String,
    instance: String,
    code: String,
}

#[derive(Deserialize)]
struct StreamRecovery {
    status: u16,
    code: String,
    snapshot: Value,
    new_cursor: OpaqueStreamCursor,
}
