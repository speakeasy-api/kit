use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use zeroize::Zeroize;

use crate::{
    api::{
        auth::contract::{Authorizer, ResourceScope, ScopedAuthorizer},
        service::{
            EventProjection, OperationKind, ProjectProjection, RequestContext, RunCompletionRecord,
            RunProgressRecord, RunPromptProjection, RunSemanticEnvelope, handlers,
        },
    },
    domain::{
        config::Grant,
        crypto::{constant_time_eq, hmac_sha256_domain},
        ids::{PrincipalId, ProjectId, RunId, TerminalId, ThreadId},
        projections::PersistedCommand,
    },
};

pub const SSE_SCHEMA_VERSION: u16 = 1;
pub const SSE_MEDIA_TYPE: &str = "text/event-stream";
pub const PROBLEM_MEDIA_TYPE: &str = "application/problem+json";
pub const TERMINAL_WEBSOCKET_PATH: &str = "/v1/terminals/{terminal_id}/attach";

const CURSOR_PREFIX: &str = "kitc1_";
const CURSOR_TAG_BYTES: usize = 16;
const CURSOR_HEX_BYTES: usize = 8 + CURSOR_TAG_BYTES;
const NOT_FOUND_INSTANCE: &str = "/v1/projects/_/events/stream";

#[derive(Clone)]
pub struct StreamCancellation(Arc<StreamCancellationState>);

struct StreamCancellationState {
    cancelled: AtomicBool,
    active: AtomicUsize,
    parent_cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl StreamCancellation {
    pub fn new() -> Self {
        Self::with_parent(None)
    }

    pub fn linked(parent_cancelled: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self::with_parent(Some(Arc::new(parent_cancelled)))
    }

    fn with_parent(parent_cancelled: Option<Arc<dyn Fn() -> bool + Send + Sync>>) -> Self {
        Self(Arc::new(StreamCancellationState {
            cancelled: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            parent_cancelled,
        }))
    }

    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
            || self
                .0
                .parent_cancelled
                .as_ref()
                .is_some_and(|cancelled| cancelled())
    }

    pub fn active_producers(&self) -> usize {
        self.0.active.load(Ordering::Acquire)
    }

    pub(crate) fn register(&self) -> Option<StreamProducerGuard> {
        if self.is_cancelled() {
            return None;
        }
        self.0.active.fetch_add(1, Ordering::AcqRel);
        if self.is_cancelled() {
            self.0.active.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(StreamProducerGuard(self.clone()))
    }
}

impl Default for StreamCancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for StreamCancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamCancellation")
            .field("cancelled", &self.is_cancelled())
            .field("active_producers", &self.active_producers())
            .finish()
    }
}

pub(crate) struct StreamProducerGuard(StreamCancellation);

impl Drop for StreamProducerGuard {
    fn drop(&mut self) {
        self.0.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub struct CursorKey([u8; 32]);

impl CursorKey {
    pub const fn new(secret: [u8; 32]) -> Self {
        Self(secret)
    }

    pub fn generate() -> Result<Self, StreamRejection> {
        let mut secret = [0_u8; 32];
        getrandom::fill(&mut secret).map_err(|_| StreamRejection::internal())?;
        Ok(Self(secret))
    }
}

impl fmt::Debug for CursorKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CursorKey([REDACTED])")
    }
}

impl Drop for CursorKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpaqueStreamCursor(String);

impl OpaqueStreamCursor {
    pub fn parse(value: impl Into<String>) -> Result<Self, CursorParseError> {
        let value = value.into();
        let encoded = value.strip_prefix(CURSOR_PREFIX).ok_or(CursorParseError)?;
        if encoded.len() != CURSOR_HEX_BYTES * 2
            || !encoded
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(CursorParseError);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OpaqueStreamCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for OpaqueStreamCursor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for OpaqueStreamCursor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CursorParseError;

impl fmt::Display for CursorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid opaque stream cursor")
    }
}

impl std::error::Error for CursorParseError {}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EventFilter {
    scope: EventScope,
    operations: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum EventScope {
    #[default]
    Project,
    Thread(ThreadId),
    Run(RunId),
}

impl EventFilter {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn project() -> Self {
        Self::default()
    }

    pub fn thread(thread_id: ThreadId) -> Self {
        Self {
            scope: EventScope::Thread(thread_id),
            operations: BTreeSet::new(),
        }
    }

    pub fn run(run_id: RunId) -> Self {
        Self {
            scope: EventScope::Run(run_id),
            operations: BTreeSet::new(),
        }
    }

    pub fn operations(
        operations: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, FilterError> {
        let operations = operations
            .into_iter()
            .map(Into::into)
            .collect::<BTreeSet<_>>();
        if operations.iter().any(|operation| operation.is_empty()) {
            return Err(FilterError);
        }
        Ok(Self {
            scope: EventScope::Project,
            operations,
        })
    }

    fn includes(
        &self,
        state: &crate::domain::projections::DomainReducer,
        event: &EventProjection,
    ) -> bool {
        (self.operations.is_empty() || self.operations.contains(&event.operation))
            && match self.scope {
                EventScope::Project => true,
                EventScope::Thread(thread_id) => {
                    event.stream == thread_id.to_string()
                        || event
                            .stream
                            .parse::<RunId>()
                            .ok()
                            .and_then(|run_id| state.run(run_id))
                            .is_some_and(|run| run.thread_id == thread_id)
                }
                EventScope::Run(run_id) => event.stream == run_id.to_string(),
            }
    }

    fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        match self.scope {
            EventScope::Project => bytes.push(b'p'),
            EventScope::Thread(id) => {
                bytes.push(b't');
                put_bytes(&mut bytes, id.to_string().as_bytes());
            }
            EventScope::Run(id) => {
                bytes.push(b'r');
                put_bytes(&mut bytes, id.to_string().as_bytes());
            }
        }
        for operation in &self.operations {
            put_bytes(&mut bytes, operation.as_bytes());
        }
        bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilterError;

impl fmt::Display for FilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("stream filter operations must not be empty")
    }
}

impl std::error::Error for FilterError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamConfig {
    pub buffer_capacity: usize,
    pub schema_version: u16,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            buffer_capacity: 64,
            schema_version: SSE_SCHEMA_VERSION,
        }
    }
}

#[derive(Clone)]
pub struct SqliteStreamAdapter {
    path: PathBuf,
    key: CursorKey,
    config: StreamConfig,
    first_available_positions: Arc<Mutex<BTreeMap<ProjectId, u64>>>,
}

impl SqliteStreamAdapter {
    pub fn new(
        path: impl AsRef<Path>,
        key: CursorKey,
        config: StreamConfig,
    ) -> Result<Self, StreamRejection> {
        if config.buffer_capacity == 0 || config.schema_version == 0 {
            return Err(StreamRejection::invalid_configuration());
        }
        Ok(Self {
            path: path.as_ref().to_owned(),
            key,
            config,
            first_available_positions: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn set_first_available_position(
        &self,
        project_id: ProjectId,
        position: u64,
    ) -> Result<(), StreamRejection> {
        let mut positions = self
            .first_available_positions
            .lock()
            .map_err(|_| StreamRejection::internal())?;
        let current = positions.entry(project_id).or_insert(1);
        *current = (*current).max(position.max(1));
        Ok(())
    }

    pub fn open(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
        filter: EventFilter,
        cursor: Option<&OpaqueStreamCursor>,
    ) -> Result<SseConnection, StreamRejection> {
        let view = read_batch(&self.path, project_id, &filter, 0, 0)?;
        authorize(context, project_id, view.snapshot.as_ref())?;

        let position = match cursor {
            Some(cursor) => self.decode_cursor(context, project_id, &filter, cursor)?,
            None => view.watermark,
        };
        if position > view.watermark {
            return Err(StreamRejection::invalid_cursor());
        }
        let first_available = self
            .first_available_positions
            .lock()
            .map_err(|_| StreamRejection::internal())?
            .get(&project_id)
            .copied()
            .unwrap_or(1);
        if position < first_available.saturating_sub(1) {
            let snapshot = view.snapshot.expect("authorized projects have a snapshot");
            let new_cursor = self.encode_cursor(context, project_id, &filter, view.watermark);
            return Err(StreamRejection::cursor_expired(snapshot, new_cursor));
        }

        Ok(SseConnection {
            path: self.path.clone(),
            key: self.key.clone(),
            principal_id: context.principal_id(),
            project_id,
            filter,
            schema_version: self.config.schema_version,
            capacity: self.config.buffer_capacity,
            queue: VecDeque::with_capacity(self.config.buffer_capacity),
            scan_position: position,
            durable_position: position,
            projection_digest: view.digest,
            disconnected: false,
            disconnect_frame: None,
        })
    }

    pub fn reserve_terminal_websocket(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
        _terminal_id: TerminalId,
    ) -> Result<(), StreamRejection> {
        let view = read_batch(&self.path, project_id, &EventFilter::all(), 0, 0)?;
        authorize(context, project_id, view.snapshot.as_ref())?;
        Err(StreamRejection::terminal_unavailable())
    }

    fn encode_cursor(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
        filter: &EventFilter,
        position: u64,
    ) -> OpaqueStreamCursor {
        encode_cursor(
            &self.key,
            context.principal_id(),
            project_id,
            filter,
            self.config.schema_version,
            position,
        )
    }

    fn decode_cursor(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
        filter: &EventFilter,
        cursor: &OpaqueStreamCursor,
    ) -> Result<u64, StreamRejection> {
        decode_cursor(
            &self.key,
            context.principal_id(),
            project_id,
            filter,
            self.config.schema_version,
            cursor.as_str(),
        )
        .ok_or_else(StreamRejection::invalid_cursor)
    }
}

pub struct SseConnection {
    path: PathBuf,
    key: CursorKey,
    principal_id: PrincipalId,
    project_id: ProjectId,
    filter: EventFilter,
    schema_version: u16,
    capacity: usize,
    queue: VecDeque<QueuedEvent>,
    scan_position: u64,
    durable_position: u64,
    projection_digest: [u8; 32],
    disconnected: bool,
    disconnect_frame: Option<OpaqueStreamCursor>,
}

impl SseConnection {
    pub fn pump(&mut self) -> Result<PumpOutcome, StreamRejection> {
        if self.disconnected {
            return Ok(PumpOutcome::Disconnected {
                cursor: self.last_durable_cursor(),
            });
        }

        let available = self.capacity - self.queue.len();
        let batch = read_batch(
            &self.path,
            self.project_id,
            &self.filter,
            self.scan_position,
            available.max(1),
        )?;
        if available == 0 && (!batch.events.is_empty() || batch.malformed_position.is_some()) {
            let cursor = self.last_durable_cursor();
            self.queue.clear();
            self.disconnected = true;
            self.disconnect_frame = Some(cursor.clone());
            return Ok(PumpOutcome::Disconnected { cursor });
        }

        let count = batch.events.len();
        for event in batch.events {
            self.scan_position = event.position;
            self.queue.push_back(QueuedEvent {
                position: event.position,
                frame: semantic_frame(
                    &self.key,
                    self.principal_id,
                    self.project_id,
                    &self.filter,
                    self.schema_version,
                    event,
                )?,
            });
        }
        if let Some(position) = batch.malformed_position {
            self.scan_position = position;
            let cursor = encode_cursor(
                &self.key,
                self.principal_id,
                self.project_id,
                &self.filter,
                self.schema_version,
                position,
            );
            self.queue.push_back(QueuedEvent {
                position,
                frame: SseFrame::Disconnect {
                    cursor: cursor.clone(),
                    reason: "stream_error",
                },
            });
            self.disconnected = true;
            return Ok(PumpOutcome::Disconnected { cursor });
        }
        if !batch.has_more {
            self.scan_position = batch.watermark;
        }
        Ok(PumpOutcome::Ready { queued: count })
    }

    pub fn next_frame(&mut self) -> Option<SseFrame> {
        if let Some(event) = self.queue.pop_front() {
            self.durable_position = event.position;
            return Some(event.frame);
        }
        self.disconnect_frame
            .take()
            .map(|cursor| SseFrame::Disconnect {
                cursor,
                reason: "slow_consumer",
            })
    }

    pub fn heartbeat(&self) -> SseFrame {
        SseFrame::Heartbeat
    }

    pub fn last_durable_cursor(&self) -> OpaqueStreamCursor {
        encode_cursor(
            &self.key,
            self.principal_id,
            self.project_id,
            &self.filter,
            self.schema_version,
            self.durable_position,
        )
    }

    pub fn queued_len(&self) -> usize {
        self.queue.len()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub const fn projection_digest(&self) -> [u8; 32] {
        self.projection_digest
    }

    pub fn is_disconnected(&self) -> bool {
        self.disconnected
    }
}

impl fmt::Debug for SseConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SseConnection")
            .field("project_id", &self.project_id)
            .field("queued", &self.queue.len())
            .field("disconnected", &self.disconnected)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PumpOutcome {
    Ready { queued: usize },
    Disconnected { cursor: OpaqueStreamCursor },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SseFrame {
    Semantic {
        id: OpaqueStreamCursor,
        operation: String,
        data: Vec<u8>,
    },
    Heartbeat,
    Disconnect {
        cursor: OpaqueStreamCursor,
        reason: &'static str,
    },
}

impl SseFrame {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Self::Semantic {
                id,
                operation,
                data,
            } => {
                let mut encoded = Vec::new();
                encoded.extend_from_slice(b"id: ");
                encoded.extend_from_slice(id.as_str().as_bytes());
                encoded.extend_from_slice(b"\nevent: ");
                encoded.extend_from_slice(operation.as_bytes());
                encoded.extend_from_slice(b"\ndata: ");
                encoded.extend_from_slice(data);
                encoded.extend_from_slice(b"\n\n");
                encoded
            }
            Self::Heartbeat => b": heartbeat\n\n".to_vec(),
            Self::Disconnect { cursor, reason } => {
                serde_json::to_vec(&DisconnectData { cursor, reason })
                    .map(|data| {
                        let mut encoded = b"event: stream.disconnect\ndata: ".to_vec();
                        encoded.extend_from_slice(&data);
                        encoded.extend_from_slice(b"\n\n");
                        encoded
                    })
                    .expect("disconnect data is serializable")
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamRejection {
    status: u16,
    body: Vec<u8>,
}

impl StreamRejection {
    pub fn status(&self) -> u16 {
        self.status
    }

    pub fn content_type(&self) -> &'static str {
        PROBLEM_MEDIA_TYPE
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    fn not_found() -> Self {
        Self::problem(
            404,
            "Resource not found",
            "The requested resource was not found.",
            "not_found",
            NOT_FOUND_INSTANCE,
            None,
        )
    }

    fn invalid_cursor() -> Self {
        Self::problem(
            400,
            "Invalid cursor",
            "The stream cursor is invalid or does not match this request.",
            "invalid_cursor",
            NOT_FOUND_INSTANCE,
            None,
        )
    }

    fn cursor_expired(snapshot: ProjectProjection, new_cursor: OpaqueStreamCursor) -> Self {
        Self::problem(
            410,
            "Cursor expired",
            "The requested event history is no longer retained.",
            "cursor_expired",
            NOT_FOUND_INSTANCE,
            Some(CursorRecovery {
                snapshot,
                new_cursor,
            }),
        )
    }

    fn terminal_unavailable() -> Self {
        Self::problem(
            503,
            "Terminal attachment unavailable",
            "The terminal owner service is not available.",
            "terminal_unavailable",
            TERMINAL_WEBSOCKET_PATH,
            None,
        )
    }

    fn invalid_configuration() -> Self {
        Self::problem(
            500,
            "Invalid stream configuration",
            "The stream adapter configuration is invalid.",
            "invalid_stream_configuration",
            NOT_FOUND_INSTANCE,
            None,
        )
    }

    fn internal() -> Self {
        Self::problem(
            500,
            "Internal server error",
            "The stream could not be read.",
            "internal_error",
            NOT_FOUND_INSTANCE,
            None,
        )
    }

    fn problem(
        status: u16,
        title: &'static str,
        detail: &'static str,
        code: &'static str,
        instance: &'static str,
        recovery: Option<CursorRecovery>,
    ) -> Self {
        let body = serde_json::to_vec(&ProblemDocument {
            problem_type: format!("https://kit.dev/problems/{code}"),
            title,
            status,
            detail,
            instance,
            code,
            recovery,
        })
        .expect("problem details are serializable");
        Self { status, body }
    }
}

impl fmt::Display for StreamRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stream request rejected with status {}", self.status)
    }
}

impl std::error::Error for StreamRejection {}

#[derive(Serialize)]
struct ProblemDocument {
    #[serde(rename = "type")]
    problem_type: String,
    title: &'static str,
    status: u16,
    detail: &'static str,
    instance: &'static str,
    code: &'static str,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    recovery: Option<CursorRecovery>,
}

#[derive(Serialize)]
struct CursorRecovery {
    snapshot: ProjectProjection,
    new_cursor: OpaqueStreamCursor,
}

#[derive(Serialize)]
struct DisconnectData<'a> {
    cursor: &'a OpaqueStreamCursor,
    reason: &'a str,
}

struct QueuedEvent {
    position: u64,
    frame: SseFrame,
}

struct ReadBatch {
    watermark: u64,
    snapshot: Option<ProjectProjection>,
    digest: [u8; 32],
    events: Vec<SemanticEvent>,
    malformed_position: Option<u64>,
    has_more: bool,
}

struct SemanticEvent {
    position: u64,
    operation: String,
    stream: String,
    payload: serde_json::Value,
}

fn authorize(
    context: &RequestContext,
    project_id: ProjectId,
    snapshot: Option<&ProjectProjection>,
) -> Result<(), StreamRejection> {
    let Some(snapshot) = snapshot else {
        return Err(StreamRejection::not_found());
    };
    ScopedAuthorizer
        .authorize(
            context.principal(),
            ResourceScope::new(snapshot.principal_id, project_id),
            Grant::WorkspaceRead,
        )
        .map(|_| ())
        .map_err(|_| StreamRejection::not_found())
}

fn read_batch(
    path: &Path,
    target: ProjectId,
    filter: &EventFilter,
    after: u64,
    limit: usize,
) -> Result<ReadBatch, StreamRejection> {
    let mut store = crate::store::sqlite::projection::ProjectionStore::open(path)
        .map_err(|_| StreamRejection::internal())?;
    let (state, snapshot) = store
        .update_domain()
        .map_err(|_| StreamRejection::internal())?;
    let watermark = state.committed();
    let mut events = Vec::with_capacity(limit);
    let mut malformed_position = None;
    let mut has_more = false;
    for event in &state.events {
        if event.project_id == target
            && event.cursor.position() > after
            && filter.includes(&state, event)
        {
            if events.len() < limit {
                match semantic_event(event) {
                    Ok(event) => events.push(event),
                    Err(()) => {
                        malformed_position = Some(event.cursor.position());
                        break;
                    }
                }
            } else {
                has_more = true;
            }
        }
    }
    Ok(ReadBatch {
        watermark,
        snapshot: state.projects.get(&target).cloned(),
        digest: snapshot.digest,
        events,
        malformed_position,
        has_more,
    })
}

fn semantic_event(event: &EventProjection) -> Result<SemanticEvent, ()> {
    let payload = match event.operation.as_str() {
        "run.prompt" => run_semantic_payload::<RunPromptProjection>(event)?,
        "run.progress" => run_semantic_payload::<RunProgressRecord>(event)?,
        "run.output" => run_semantic_payload::<RunCompletionRecord>(event)?,
        "run.failure" => run_semantic_payload::<crate::api::service::RunFailureProjection>(event)?,
        operation
            if handlers().iter().any(|descriptor| {
                descriptor.kind == OperationKind::Command && descriptor.operation == operation
            }) =>
        {
            let stored: PersistedCommand =
                serde_json::from_slice(&event.payload).map_err(|_| ())?;
            if stored.command.operation() != operation {
                return Err(());
            }
            serde_json::to_value(stored.command).map_err(|_| ())?
        }
        _ => return Err(()),
    };
    Ok(SemanticEvent {
        position: event.cursor.position(),
        operation: event.operation.clone(),
        stream: event.stream.clone(),
        payload,
    })
}

fn run_semantic_payload<T>(event: &EventProjection) -> Result<serde_json::Value, ()>
where
    T: DeserializeOwned + Serialize,
{
    let stored: RunSemanticEnvelope<T> = serde_json::from_slice(&event.payload).map_err(|_| ())?;
    if stored.schema_version != 1
        || stored.project_id != event.project_id
        || stored.run_id.to_string() != event.stream
    {
        return Err(());
    }
    serde_json::to_value(stored.record).map_err(|_| ())
}

fn semantic_frame(
    key: &CursorKey,
    principal_id: PrincipalId,
    project_id: ProjectId,
    filter: &EventFilter,
    schema_version: u16,
    event: SemanticEvent,
) -> Result<SseFrame, StreamRejection> {
    let data = serde_json::to_vec(&SemanticData {
        schema_version,
        operation: &event.operation,
        stream: &event.stream,
        payload: &event.payload,
    })
    .map_err(|_| StreamRejection::internal())?;
    Ok(SseFrame::Semantic {
        id: encode_cursor(
            key,
            principal_id,
            project_id,
            filter,
            schema_version,
            event.position,
        ),
        operation: event.operation,
        data,
    })
}

#[derive(Serialize)]
struct SemanticData<'a> {
    schema_version: u16,
    operation: &'a str,
    stream: &'a str,
    payload: &'a serde_json::Value,
}

fn encode_cursor(
    key: &CursorKey,
    principal_id: PrincipalId,
    project_id: ProjectId,
    filter: &EventFilter,
    schema_version: u16,
    position: u64,
) -> OpaqueStreamCursor {
    let associated = cursor_associated(principal_id, project_id, filter, schema_version);
    let mask = hmac_sha256_domain(&key.0, b"KIT-SSE-CURSOR-MASK\0", &[&associated]);
    let masked = position ^ u64::from_be_bytes(mask[..8].try_into().expect("eight-byte slice"));
    let masked_bytes = masked.to_be_bytes();
    let tag = hmac_sha256_domain(
        &key.0,
        b"KIT-SSE-CURSOR-TAG\0",
        &[&associated, &masked_bytes],
    );
    let mut cursor = String::with_capacity(CURSOR_PREFIX.len() + 16 + CURSOR_TAG_BYTES * 2);
    cursor.push_str(CURSOR_PREFIX);
    push_hex(&mut cursor, &masked_bytes);
    push_hex(&mut cursor, &tag[..CURSOR_TAG_BYTES]);
    OpaqueStreamCursor(cursor)
}

fn decode_cursor(
    key: &CursorKey,
    principal_id: PrincipalId,
    project_id: ProjectId,
    filter: &EventFilter,
    schema_version: u16,
    cursor: &str,
) -> Option<u64> {
    let encoded = cursor.strip_prefix(CURSOR_PREFIX)?;
    if encoded.len() != CURSOR_HEX_BYTES * 2 {
        return None;
    }
    let masked_bytes: [u8; 8] = decode_hex(&encoded[..16])?.try_into().ok()?;
    let actual_tag = decode_hex(&encoded[16..])?;
    let associated = cursor_associated(principal_id, project_id, filter, schema_version);
    let expected_tag = hmac_sha256_domain(
        &key.0,
        b"KIT-SSE-CURSOR-TAG\0",
        &[&associated, &masked_bytes],
    );
    if !constant_time_eq(&actual_tag, &expected_tag[..CURSOR_TAG_BYTES]) {
        return None;
    }
    let mask = hmac_sha256_domain(&key.0, b"KIT-SSE-CURSOR-MASK\0", &[&associated]);
    Some(
        u64::from_be_bytes(masked_bytes)
            ^ u64::from_be_bytes(mask[..8].try_into().expect("eight-byte slice")),
    )
}

fn cursor_associated(
    principal_id: PrincipalId,
    project_id: ProjectId,
    filter: &EventFilter,
    schema_version: u16,
) -> Vec<u8> {
    let mut bytes = b"KIT-SSE-CURSOR\0\x01".to_vec();
    put_bytes(&mut bytes, principal_id.to_string().as_bytes());
    put_bytes(&mut bytes, project_id.to_string().as_bytes());
    put_bytes(&mut bytes, &filter.canonical_bytes());
    bytes.extend_from_slice(&schema_version.to_be_bytes());
    bytes
}

fn put_bytes(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value);
}

fn push_hex(output: &mut String, bytes: &[u8]) {
    use fmt::Write as _;
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to a string cannot fail");
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Some((high << 4) | low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}
