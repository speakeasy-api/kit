use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
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
const CURSOR_STATE_PREFIX: &str = "kitc2_";
const CURSOR_TAG_BYTES: usize = 16;
const CURSOR_HEX_BYTES: usize = 8 + CURSOR_TAG_BYTES;
const MAX_CURSOR_BINARY_BYTES: usize =
    8 + 8 + 4 + 32 + crate::domain::secret::JsonProjectionState::MAX_SERIALIZED_BYTES;
pub const OPAQUE_STREAM_CURSOR_MAX_LENGTH: usize =
    CURSOR_STATE_PREFIX.len() + (MAX_CURSOR_BINARY_BYTES * 4).div_ceil(3);
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
        let encoded = value
            .strip_prefix(CURSOR_PREFIX)
            .or_else(|| value.strip_prefix(CURSOR_STATE_PREFIX))
            .ok_or(CursorParseError)?;
        let valid_length = if value.starts_with(CURSOR_PREFIX) {
            encoded.len() == CURSOR_HEX_BYTES * 2
        } else {
            encoded.len() >= ((8 + 8 + 4 + 32) * 4_usize).div_ceil(3)
                && value.len() <= OPAQUE_STREAM_CURSOR_MAX_LENGTH
                && encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        };
        if !valid_length
            || (value.starts_with(CURSOR_PREFIX)
                && !encoded
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
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
    custody: crate::domain::secret::SecretCustody,
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
            custody: crate::domain::secret::SecretCustody::default(),
        })
    }

    pub fn with_custody(mut self, custody: crate::domain::secret::SecretCustody) -> Self {
        self.custody = custody;
        self
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

    pub(crate) fn encode_page_cursor(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
        filter: &EventFilter,
        position: u64,
        state: &crate::domain::secret::JsonProjectionState,
    ) -> Result<OpaqueStreamCursor, StreamRejection> {
        self.encode_cursor(context, project_id, filter, position, state)
    }

    pub(crate) fn decode_page_cursor(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
        filter: &EventFilter,
        cursor: &OpaqueStreamCursor,
    ) -> Result<(u64, crate::domain::secret::JsonProjectionState), StreamRejection> {
        self.decode_cursor(context, project_id, filter, cursor)
    }

    pub(crate) fn projection_state(&self) -> crate::domain::secret::JsonProjectionState {
        self.custody.projection_state()
    }

    pub(crate) fn accepts_legacy_cursor(&self) -> bool {
        self.custody.is_empty()
    }

    pub fn open(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
        filter: EventFilter,
        cursor: Option<&OpaqueStreamCursor>,
    ) -> Result<SseConnection, StreamRejection> {
        let view = read_head(&self.path, project_id)?;
        authorize(context, project_id, view.snapshot.as_ref())?;

        let (position, projection_state) = match cursor {
            Some(cursor) => self.decode_cursor(context, project_id, &filter, cursor)?,
            None => (view.watermark, self.custody.projection_state()),
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
            let new_cursor = self.encode_cursor(
                context,
                project_id,
                &filter,
                view.watermark,
                &self.custody.projection_state(),
            )?;
            return Err(StreamRejection::cursor_expired(snapshot, new_cursor));
        }

        let durable_cursor =
            self.encode_cursor(context, project_id, &filter, position, &projection_state)?;
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
            durable_projection_state: projection_state.clone(),
            durable_cursor,
            projection_digest: view.digest,
            custody: self.custody.clone(),
            custody_revision: projection_state.custody_revision(),
            projection_state,
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
        let view = read_head(&self.path, project_id)?;
        authorize(context, project_id, view.snapshot.as_ref())?;
        Err(StreamRejection::terminal_unavailable())
    }

    fn encode_cursor(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
        filter: &EventFilter,
        position: u64,
        state: &crate::domain::secret::JsonProjectionState,
    ) -> Result<OpaqueStreamCursor, StreamRejection> {
        encode_state_cursor(
            &self.key,
            context.principal_id(),
            project_id,
            filter,
            self.config.schema_version,
            position,
            state,
            &self.custody,
        )
    }

    fn decode_cursor(
        &self,
        context: &RequestContext,
        project_id: ProjectId,
        filter: &EventFilter,
        cursor: &OpaqueStreamCursor,
    ) -> Result<(u64, crate::domain::secret::JsonProjectionState), StreamRejection> {
        match decode_state_cursor(
            &self.key,
            context.principal_id(),
            project_id,
            filter,
            self.config.schema_version,
            cursor.as_str(),
            &self.custody,
        ) {
            StateCursorDecode::Decoded(position, state) => Ok((position, state)),
            StateCursorDecode::RevisionChanged => Err(StreamRejection::cursor_upgrade_required()),
            StateCursorDecode::Invalid => Err(StreamRejection::invalid_cursor()),
        }
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
    durable_projection_state: crate::domain::secret::JsonProjectionState,
    durable_cursor: OpaqueStreamCursor,
    projection_digest: [u8; 32],
    custody: crate::domain::secret::SecretCustody,
    projection_state: crate::domain::secret::JsonProjectionState,
    custody_revision: u64,
    disconnected: bool,
    disconnect_frame: Option<(OpaqueStreamCursor, &'static str)>,
}

impl SseConnection {
    pub fn pump(&mut self) -> Result<PumpOutcome, StreamRejection> {
        if self.disconnected {
            return Ok(PumpOutcome::Disconnected {
                cursor: self.last_durable_cursor(),
            });
        }
        if self.custody.revision() != self.custody_revision {
            return Ok(self.disconnect_for_custody_change());
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
            self.disconnect_frame = Some((cursor.clone(), "slow_consumer"));
            return Ok(PumpOutcome::Disconnected { cursor });
        }

        let count = batch.events.len();
        for event in batch.events {
            let position = event.position;
            self.scan_position = position;
            let frame = match semantic_frame(
                &self.key,
                self.principal_id,
                self.project_id,
                &self.filter,
                self.schema_version,
                event,
                &self.custody,
                &mut self.projection_state,
            ) {
                Ok(frame) => frame,
                Err(_) if self.custody.revision() != self.custody_revision => {
                    return Ok(self.disconnect_for_custody_change());
                }
                Err(error) => return Err(error),
            };
            if self.custody.revision() != self.custody_revision {
                return Ok(self.disconnect_for_custody_change());
            }
            let cursor = match &frame {
                SseFrame::Semantic { id, .. } => id.clone(),
                _ => return Err(StreamRejection::internal()),
            };
            self.queue.push_back(QueuedEvent {
                position,
                frame,
                projection_state: self.projection_state.clone(),
                cursor,
            });
        }
        if let Some(position) = batch.malformed_position {
            self.scan_position = position;
            let cursor = match encode_state_cursor(
                &self.key,
                self.principal_id,
                self.project_id,
                &self.filter,
                self.schema_version,
                position,
                &self.projection_state,
                &self.custody,
            ) {
                Ok(cursor) => cursor,
                Err(_) if self.custody.revision() != self.custody_revision => {
                    return Ok(self.disconnect_for_custody_change());
                }
                Err(error) => return Err(error),
            };
            self.queue.push_back(QueuedEvent {
                position,
                frame: SseFrame::Disconnect {
                    cursor: cursor.clone(),
                    reason: "stream_error",
                },
                projection_state: self.projection_state.clone(),
                cursor: cursor.clone(),
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
        if !self.disconnected && self.custody.revision() != self.custody_revision {
            self.disconnect_for_custody_change();
        }
        if let Some(event) = self.queue.pop_front() {
            self.durable_position = event.position;
            self.durable_projection_state = event.projection_state;
            self.durable_cursor = event.cursor;
            return Some(event.frame);
        }
        self.disconnect_frame
            .take()
            .map(|(cursor, reason)| SseFrame::Disconnect { cursor, reason })
    }

    pub fn heartbeat(&self) -> SseFrame {
        SseFrame::Heartbeat
    }

    pub fn last_durable_cursor(&self) -> OpaqueStreamCursor {
        self.durable_cursor.clone()
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

    fn disconnect_for_custody_change(&mut self) -> PumpOutcome {
        let cursor = self.durable_cursor.clone();
        self.queue.clear();
        self.scan_position = self.durable_position;
        self.projection_state = self.durable_projection_state.clone();
        self.disconnected = true;
        self.disconnect_frame = Some((cursor.clone(), "cursor_upgrade_required"));
        PumpOutcome::Disconnected { cursor }
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

    pub(crate) fn requires_cursor_upgrade(&self) -> bool {
        self.status == 409
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

    fn cursor_upgrade_required() -> Self {
        Self::problem(
            409,
            "Cursor upgrade required",
            "The cursor predates active projection state. Restart without a cursor.",
            "cursor_upgrade_required",
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
    projection_state: crate::domain::secret::JsonProjectionState,
    cursor: OpaqueStreamCursor,
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
    canonical: Vec<u8>,
    payload_bytes: Vec<u8>,
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
    read_batch_with_hook(path, target, filter, after, limit, || {})
}

fn read_batch_with_hook(
    path: &Path,
    target: ProjectId,
    filter: &EventFilter,
    after: u64,
    limit: usize,
    after_watermark: impl FnOnce(),
) -> Result<ReadBatch, StreamRejection> {
    const MAX_EVENT_BYTES: usize = 1024 * 1024;
    const MAX_BATCH_WORK_BYTES: usize = 8 * 1024 * 1024;
    let mut connection =
        rusqlite::Connection::open(path).map_err(|_| StreamRejection::internal())?;
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)
        .map_err(|_| StreamRejection::internal())?;
    let watermark = transaction
        .query_row(
            "SELECT position FROM commit_watermark WHERE singleton = 1",
            [],
            |row| row.get::<_, u64>(0),
        )
        .map_err(|_| StreamRejection::internal())?;
    after_watermark();
    let mut sql = String::from(
        "SELECT event.commit_position, event.event_id, event.stream, event.sequence,
                event.event_type, event.schema_version, event.occurred_at, event.causation_id,
                event.correlation_id, event.attempt_id, event.trace_id, event.payload,
                event.artifacts, length(event.payload), length(event.artifacts)
         FROM event_projection_index AS index_row
         JOIN events AS event ON event.commit_position = index_row.commit_position
         WHERE index_row.project_id = ?1 AND index_row.erased = 0
           AND event.commit_position > ?2
           AND event.commit_position <= ?3",
    );
    let mut parameters = vec![
        rusqlite::types::Value::Text(target.to_string()),
        rusqlite::types::Value::Integer(
            i64::try_from(after).map_err(|_| StreamRejection::invalid_cursor())?,
        ),
        rusqlite::types::Value::Integer(
            i64::try_from(watermark).map_err(|_| StreamRejection::internal())?,
        ),
    ];
    match filter.scope {
        EventScope::Project => {}
        EventScope::Thread(id) => {
            sql.push_str(" AND index_row.thread_id = ?4");
            parameters.push(rusqlite::types::Value::Text(id.to_string()));
        }
        EventScope::Run(id) => {
            sql.push_str(" AND index_row.run_id = ?4");
            parameters.push(rusqlite::types::Value::Text(id.to_string()));
        }
    }
    if !filter.operations.is_empty() {
        sql.push_str(" AND event.event_type IN (");
        for (index, operation) in filter.operations.iter().enumerate() {
            if index != 0 {
                sql.push(',');
            }
            sql.push('?');
            sql.push_str(&(parameters.len() + 1).to_string());
            parameters.push(rusqlite::types::Value::Text(operation.clone()));
        }
        sql.push(')');
    }
    sql.push_str(" ORDER BY event.commit_position LIMIT ?");
    sql.push_str(&(parameters.len() + 1).to_string());
    parameters.push(rusqlite::types::Value::Integer(
        i64::try_from(limit.saturating_add(1)).map_err(|_| StreamRejection::internal())?,
    ));
    let mut statement = transaction
        .prepare(&sql)
        .map_err(|_| StreamRejection::internal())?;
    let mut rows = statement
        .query(rusqlite::params_from_iter(parameters))
        .map_err(|_| StreamRejection::internal())?;
    let mut events = Vec::with_capacity(limit);
    let mut malformed_position = None;
    let mut has_more = false;
    let mut work_bytes = 0_usize;
    while let Some(row) = rows.next().map_err(|_| StreamRejection::internal())? {
        let position = row
            .get::<_, u64>(0)
            .map_err(|_| StreamRejection::internal())?;
        if events.len() == limit {
            has_more = true;
            break;
        }
        let payload_bytes = usize::try_from(
            row.get::<_, i64>(13)
                .map_err(|_| StreamRejection::internal())?,
        )
        .map_err(|_| StreamRejection::internal())?;
        let artifact_bytes = usize::try_from(
            row.get::<_, i64>(14)
                .map_err(|_| StreamRejection::internal())?,
        )
        .map_err(|_| StreamRejection::internal())?;
        work_bytes = work_bytes
            .checked_add(payload_bytes)
            .and_then(|bytes| bytes.checked_add(artifact_bytes))
            .filter(|bytes| {
                payload_bytes <= MAX_EVENT_BYTES
                    && artifact_bytes <= MAX_EVENT_BYTES
                    && *bytes <= MAX_BATCH_WORK_BYTES
            })
            .ok_or_else(StreamRejection::internal)?;
        let payload = row
            .get::<_, Vec<u8>>(11)
            .map_err(|_| StreamRejection::internal())?;
        let artifacts = row
            .get::<_, Vec<u8>>(12)
            .map_err(|_| StreamRejection::internal())?;
        let Ok(payload_value) = serde_json::from_slice::<&serde_json::value::RawValue>(&payload)
        else {
            malformed_position = Some(position);
            break;
        };
        let Ok(artifacts_value) =
            serde_json::from_slice::<&serde_json::value::RawValue>(&artifacts)
        else {
            malformed_position = Some(position);
            break;
        };
        let event_id = row
            .get::<_, String>(1)
            .map_err(|_| StreamRejection::internal())?;
        let stream = row
            .get::<_, String>(2)
            .map_err(|_| StreamRejection::internal())?;
        let operation = row
            .get::<_, String>(4)
            .map_err(|_| StreamRejection::internal())?;
        let occurred_at = row
            .get::<_, String>(6)
            .map_err(|_| StreamRejection::internal())?;
        let causation_id = row
            .get::<_, String>(7)
            .map_err(|_| StreamRejection::internal())?;
        let correlation_id = row
            .get::<_, String>(8)
            .map_err(|_| StreamRejection::internal())?;
        let attempt_id = row
            .get::<_, Option<String>>(9)
            .map_err(|_| StreamRejection::internal())?;
        let trace_id = row
            .get::<_, String>(10)
            .map_err(|_| StreamRejection::internal())?;
        let canonical = serde_json::to_vec(&CanonicalEventEnvelope {
            operation: &operation,
            stream: &stream,
            payload: payload_value,
            trace_id: &trace_id,
            id: &event_id,
            sequence: row.get(3).map_err(|_| StreamRejection::internal())?,
            commit_position: i64::try_from(position).map_err(|_| StreamRejection::internal())?,
            schema_version: row.get(5).map_err(|_| StreamRejection::internal())?,
            occurred_at: &occurred_at,
            causation_id: &causation_id,
            correlation_id: &correlation_id,
            attempt_id: attempt_id.as_deref(),
            artifacts: artifacts_value,
        })
        .map_err(|_| StreamRejection::internal())?;
        let projection = EventProjection {
            cursor: crate::api::service::EventCursor::new(position),
            opaque_cursor: None,
            project_id: target,
            operation,
            stream,
            payload,
            envelope: canonical,
            authority_digest: String::new(),
            projection_digest: String::new(),
        };
        match semantic_event(&projection) {
            Ok(event) => events.push(event),
            Err(()) => {
                malformed_position = Some(position);
                break;
            }
        }
    }
    Ok(ReadBatch {
        watermark,
        snapshot: None,
        digest: [0; 32],
        events,
        malformed_position,
        has_more,
    })
}

fn read_head(path: &Path, target: ProjectId) -> Result<ReadBatch, StreamRejection> {
    let mut store = crate::store::sqlite::projection::ProjectionStore::open(path)
        .map_err(|_| StreamRejection::internal())?;
    let (state, snapshot) = store
        .update_domain()
        .map_err(|_| StreamRejection::internal())?;
    Ok(ReadBatch {
        watermark: state.committed(),
        snapshot: state.projects.get(&target).cloned(),
        digest: snapshot.digest,
        events: Vec::new(),
        malformed_position: None,
        has_more: false,
    })
}

#[derive(Serialize)]
struct CanonicalEventEnvelope<'a> {
    operation: &'a str,
    stream: &'a str,
    payload: &'a serde_json::value::RawValue,
    trace_id: &'a str,
    id: &'a str,
    sequence: i64,
    commit_position: i64,
    schema_version: u16,
    occurred_at: &'a str,
    causation_id: &'a str,
    correlation_id: &'a str,
    attempt_id: Option<&'a str>,
    artifacts: &'a serde_json::value::RawValue,
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
        canonical: event.envelope.clone(),
        payload_bytes: event.payload.clone(),
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

#[allow(clippy::too_many_arguments)]
fn semantic_frame(
    key: &CursorKey,
    principal_id: PrincipalId,
    project_id: ProjectId,
    filter: &EventFilter,
    schema_version: u16,
    event: SemanticEvent,
    custody: &crate::domain::secret::SecretCustody,
    state: &mut crate::domain::secret::JsonProjectionState,
) -> Result<SseFrame, StreamRejection> {
    if custody.contains(event.operation.as_bytes()) || custody.contains(event.stream.as_bytes()) {
        return Err(StreamRejection::internal());
    }
    let persisted_payload = !event.canonical.is_empty();
    let payload_bytes = if event.payload_bytes.is_empty() {
        serde_json::to_vec(&event.payload).map_err(|_| StreamRejection::internal())?
    } else {
        event.payload_bytes
    };
    let canonical = if event.canonical.is_empty() {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": schema_version,
            "operation": event.operation,
            "stream": event.stream,
            "payload": event.payload,
        }))
        .map_err(|_| StreamRejection::internal())?
    } else {
        event.canonical
    };
    let projected = crate::api::service::project_event_envelopes_with_state(
        custody,
        vec![(canonical, payload_bytes)],
        state,
    )
    .map_err(|_| StreamRejection::internal())?
    .remove(0);
    let payload = if persisted_payload {
        let projection = EventProjection {
            cursor: crate::api::service::EventCursor::new(event.position),
            opaque_cursor: None,
            project_id,
            operation: event.operation.clone(),
            stream: event.stream.clone(),
            payload: projected.payload.clone(),
            envelope: projected.envelope.clone(),
            authority_digest: projected.authority_digest.clone(),
            projection_digest: projected.digest.clone(),
        };
        semantic_event(&projection).map_or_else(
            |_| {
                serde_json::from_slice::<serde_json::Value>(&projection.payload)
                    .ok()
                    .filter(serde_json::Value::is_object)
                    .filter(|payload| {
                        payload["marker"] == crate::domain::secret::REDACTED
                            && payload["projection"]["schema_version"] == 1
                            && payload["projection"]["status"] == "fail_closed"
                    })
                    .ok_or_else(StreamRejection::internal)
            },
            |event| Ok(event.payload),
        )?
    } else {
        serde_json::from_slice::<serde_json::Value>(&projected.payload)
            .map_err(|_| StreamRejection::internal())?
    };
    let projected_envelope =
        String::from_utf8(projected.envelope).map_err(|_| StreamRejection::internal())?;
    let data = serde_json::json!({
        "schema_version": schema_version,
        "operation": event.operation,
        "stream": event.stream,
        "payload": payload,
        "authority_digest": projected.authority_digest,
        "projection_digest": projected.digest,
        "projected_envelope": projected_envelope,
    });
    let id = encode_state_cursor(
        key,
        principal_id,
        project_id,
        filter,
        schema_version,
        event.position,
        state,
        custody,
    )?;
    let operation = data["operation"].as_str().unwrap_or_default().to_owned();
    let data = serde_json::to_vec(&data).map_err(|_| StreamRejection::internal())?;
    Ok(SseFrame::Semantic {
        id,
        operation,
        data,
    })
}

#[allow(clippy::too_many_arguments)]
fn encode_state_cursor(
    key: &CursorKey,
    principal_id: PrincipalId,
    project_id: ProjectId,
    filter: &EventFilter,
    schema_version: u16,
    position: u64,
    state: &crate::domain::secret::JsonProjectionState,
    custody: &crate::domain::secret::SecretCustody,
) -> Result<OpaqueStreamCursor, StreamRejection> {
    if state.custody_revision() != custody.revision() {
        return Err(StreamRejection::cursor_upgrade_required());
    }
    let serialized = state
        .to_bounded_bytes()
        .ok_or_else(StreamRejection::internal)?;
    let associated = cursor_associated(principal_id, project_id, filter, schema_version);
    let mask = hmac_sha256_domain(&key.0, b"KIT-SSE-CURSOR-V2-POSITION\0", &[&associated]);
    let masked = (position ^ u64::from_be_bytes(mask[..8].try_into().expect("eight-byte slice")))
        .to_be_bytes();
    let revision = custody.revision().to_be_bytes();
    let revision_mask = hmac_sha256_domain(
        &key.0,
        b"KIT-SSE-CURSOR-V2-REVISION\0",
        &[&associated, &masked],
    );
    let masked_revision = (u64::from_be_bytes(revision)
        ^ u64::from_be_bytes(revision_mask[..8].try_into().expect("eight-byte slice")))
    .to_be_bytes();
    let length = u32::try_from(serialized.len())
        .map_err(|_| StreamRejection::internal())?
        .to_be_bytes();
    let encrypted = xor_stream_cursor_state(key, &associated, &masked, &serialized);
    let tag = hmac_sha256_domain(
        &key.0,
        b"KIT-SSE-CURSOR-V2-TAG\0",
        &[&associated, &masked, &masked_revision, &length, &encrypted],
    );
    let mut bytes =
        Vec::with_capacity(masked.len() + revision.len() + length.len() + encrypted.len() + 32);
    bytes.extend_from_slice(&masked);
    bytes.extend_from_slice(&masked_revision);
    bytes.extend_from_slice(&length);
    bytes.extend_from_slice(&encrypted);
    bytes.extend_from_slice(&tag);
    let mut cursor =
        String::with_capacity(CURSOR_STATE_PREFIX.len() + (bytes.len() * 4).div_ceil(3));
    cursor.push_str(CURSOR_STATE_PREFIX);
    URL_SAFE_NO_PAD.encode_string(bytes, &mut cursor);
    Ok(OpaqueStreamCursor(cursor))
}

enum StateCursorDecode {
    Decoded(u64, crate::domain::secret::JsonProjectionState),
    RevisionChanged,
    Invalid,
}

fn decode_state_cursor(
    key: &CursorKey,
    principal_id: PrincipalId,
    project_id: ProjectId,
    filter: &EventFilter,
    schema_version: u16,
    cursor: &str,
    custody: &crate::domain::secret::SecretCustody,
) -> StateCursorDecode {
    if cursor.starts_with(CURSOR_PREFIX) {
        return decode_cursor(
            key,
            principal_id,
            project_id,
            filter,
            schema_version,
            cursor,
        )
        .map_or(StateCursorDecode::Invalid, |position| {
            if custody.is_empty() {
                StateCursorDecode::Decoded(position, custody.projection_state())
            } else {
                StateCursorDecode::RevisionChanged
            }
        });
    }
    let Some(encoded) = cursor
        .strip_prefix(CURSOR_STATE_PREFIX)
        .and_then(|encoded| URL_SAFE_NO_PAD.decode(encoded).ok())
    else {
        return StateCursorDecode::Invalid;
    };
    if encoded.len() < 8 + 8 + 4 + 32 {
        return StateCursorDecode::Invalid;
    }
    let Some(masked): Option<[u8; 8]> = encoded[..8].try_into().ok() else {
        return StateCursorDecode::Invalid;
    };
    let Some(masked_revision): Option<[u8; 8]> = encoded[8..16].try_into().ok() else {
        return StateCursorDecode::Invalid;
    };
    let Some(length_bytes): Option<[u8; 4]> = encoded[16..20].try_into().ok() else {
        return StateCursorDecode::Invalid;
    };
    let length = u32::from_be_bytes(length_bytes) as usize;
    if length > crate::domain::secret::JsonProjectionState::MAX_SERIALIZED_BYTES
        || encoded.len() != 20 + length + 32
    {
        return StateCursorDecode::Invalid;
    }
    let encrypted = &encoded[20..20 + length];
    let actual_tag = &encoded[20 + length..];
    let associated = cursor_associated(principal_id, project_id, filter, schema_version);
    let revision_mask = hmac_sha256_domain(
        &key.0,
        b"KIT-SSE-CURSOR-V2-REVISION\0",
        &[&associated, &masked],
    );
    let revision = u64::from_be_bytes(masked_revision)
        ^ u64::from_be_bytes(revision_mask[..8].try_into().expect("eight-byte slice"));
    let expected_tag = hmac_sha256_domain(
        &key.0,
        b"KIT-SSE-CURSOR-V2-TAG\0",
        &[
            &associated,
            &masked,
            &masked_revision,
            &(length as u32).to_be_bytes(),
            encrypted,
        ],
    );
    if !constant_time_eq(actual_tag, &expected_tag) {
        return StateCursorDecode::Invalid;
    }
    let serialized = xor_stream_cursor_state(key, &associated, &masked, encrypted);
    let Some(state) = crate::domain::secret::JsonProjectionState::from_bounded_bytes(&serialized)
    else {
        return StateCursorDecode::Invalid;
    };
    if state.custody_revision() != revision {
        return StateCursorDecode::Invalid;
    }
    let mask = hmac_sha256_domain(&key.0, b"KIT-SSE-CURSOR-V2-POSITION\0", &[&associated]);
    let position = u64::from_be_bytes(masked)
        ^ u64::from_be_bytes(mask[..8].try_into().expect("eight-byte slice"));
    if revision == custody.revision() {
        StateCursorDecode::Decoded(position, state)
    } else {
        StateCursorDecode::RevisionChanged
    }
}

fn xor_stream_cursor_state(
    key: &CursorKey,
    associated: &[u8],
    masked: &[u8; 8],
    bytes: &[u8],
) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    for (counter, chunk) in bytes.chunks(32).enumerate() {
        let mask = hmac_sha256_domain(
            &key.0,
            b"KIT-SSE-CURSOR-V2-STATE\0",
            &[associated, masked, &(counter as u64).to_be_bytes()],
        );
        output.extend(chunk.iter().zip(mask).map(|(byte, mask)| byte ^ mask));
    }
    output
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

#[cfg(test)]
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

#[cfg(test)]
mod redaction_tests {
    use super::*;
    use crate::{
        api::{
            auth::contract::{AuthenticatedPrincipal, GrantSnapshot, ScopedAuthorizer},
            service::{Command, RequestContext, Service, SqliteServiceStore},
        },
        domain::{
            config::Grant,
            events::{SchemaVersion, TraceId},
            ids::ThreadId,
            secret::{REDACTED, SecretCustody, SecretLease},
        },
        store::sqlite::idempotency::IdempotencyKey,
    };

    #[test]
    fn sse_page_excludes_commit_after_captured_watermark_and_resumes_once() {
        let directory = std::env::temp_dir().join(format!(
            "kit-sse-watermark-{}",
            crate::domain::ids::EventId::generate().unwrap()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("store.sqlite3");
        let authority = crate::runtime::daemon::ControlPlaneAuthority::for_test();
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let thread = ThreadId::generate().unwrap();
        let context = |key: &str| {
            RequestContext::authenticated(
                Ok(AuthenticatedPrincipal::from_grants(GrantSnapshot::new(
                    principal,
                    project,
                    [Grant::WorkspaceRead, Grant::WorkspaceWrite],
                ))),
                Some(IdempotencyKey::parse(key).unwrap()),
                TraceId::parse(key).unwrap(),
            )
            .unwrap()
        };
        let mut service = Service::new(
            SqliteServiceStore::open(&path, &authority).unwrap(),
            ScopedAuthorizer,
            &authority,
        );
        service
            .execute(
                &context("watermark-project"),
                Command::CreateProject {
                    schema_version: SchemaVersion::CURRENT,
                    project_id: project,
                },
            )
            .unwrap();
        service
            .execute(
                &context("watermark-thread"),
                Command::CreateThread {
                    schema_version: SchemaVersion::CURRENT,
                    thread_id: thread,
                    project_id: project,
                },
            )
            .unwrap();
        let mut writer = Service::new(
            SqliteServiceStore::open(&path, &authority).unwrap(),
            ScopedAuthorizer,
            &authority,
        );

        let first =
            read_batch_with_hook(&path, project, &EventFilter::thread(thread), 0, 10, || {
                writer
                    .execute(
                        &context("watermark-race"),
                        Command::SetThreadArchived {
                            schema_version: SchemaVersion::CURRENT,
                            thread_id: thread,
                            archived: true,
                            expected_version: 1,
                        },
                    )
                    .unwrap();
            })
            .unwrap();
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| event.position)
                .collect::<Vec<_>>(),
            [first.watermark]
        );

        let second = read_batch(
            &path,
            project,
            &EventFilter::thread(thread),
            first.watermark,
            10,
        )
        .unwrap();
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].position, first.watermark + 1);
        assert!(
            read_batch(
                &path,
                project,
                &EventFilter::thread(thread),
                second.events[0].position,
                10,
            )
            .unwrap()
            .events
            .is_empty()
        );
        drop(writer);
        drop(service);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn authenticated_legacy_cursor_requires_upgrade_but_tampering_does_not_reveal_custody() {
        let key = CursorKey::new([3; 32]);
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let filter = EventFilter::all();
        let associated = cursor_associated(principal, project, &filter, SSE_SCHEMA_VERSION);
        let mask = hmac_sha256_domain(&key.0, b"KIT-SSE-CURSOR-MASK\0", &[&associated]);
        let masked = (7 ^ u64::from_be_bytes(mask[..8].try_into().unwrap())).to_be_bytes();
        let tag = hmac_sha256_domain(&key.0, b"KIT-SSE-CURSOR-TAG\0", &[&associated, &masked]);
        let mut cursor = CURSOR_PREFIX.to_owned();
        push_hex(&mut cursor, &masked);
        push_hex(&mut cursor, &tag[..CURSOR_TAG_BYTES]);
        let custody = SecretCustody::new([Arc::new(SecretLease::new("active-secret"))]);
        assert!(matches!(
            decode_state_cursor(
                &key,
                principal,
                project,
                &filter,
                SSE_SCHEMA_VERSION,
                &cursor,
                &custody,
            ),
            StateCursorDecode::RevisionChanged
        ));

        let replacement = if cursor.ends_with('0') { '1' } else { '0' };
        cursor.pop();
        cursor.push(replacement);
        assert!(matches!(
            decode_state_cursor(
                &key,
                principal,
                project,
                &filter,
                SSE_SCHEMA_VERSION,
                &cursor,
                &custody,
            ),
            StateCursorDecode::Invalid
        ));
    }

    #[test]
    fn subscription_state_does_not_join_nonadjacent_payload_fragments() {
        let custody = SecretCustody::new([Arc::new(SecretLease::new("cross-frame"))]);
        let key = CursorKey::new([1; 32]);
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let filter = EventFilter::all();
        for split in 1.."cross-frame".len() {
            let mut state = crate::domain::secret::JsonProjectionState::default();
            let mut projected = Vec::new();
            for (position, content) in [
                "cross-frame"[..split].to_owned(),
                "cross-frame"[split..].to_owned(),
            ]
            .into_iter()
            .enumerate()
            {
                let SseFrame::Semantic { data, .. } = semantic_frame(
                    &key,
                    principal,
                    project,
                    &filter,
                    SSE_SCHEMA_VERSION,
                    SemanticEvent {
                        position: position as u64 + 1,
                        operation: "run.progress".to_owned(),
                        stream: "run_00000000000000000000000000".to_owned(),
                        payload: serde_json::json!({"content": content}),
                        canonical: Vec::new(),
                        payload_bytes: Vec::new(),
                    },
                    &custody,
                    &mut state,
                )
                .unwrap() else {
                    unreachable!();
                };
                projected.push(serde_json::from_slice::<serde_json::Value>(&data).unwrap());
            }
            assert_eq!(projected[0]["payload"]["content"], &"cross-frame"[..split]);
            assert_eq!(projected[1]["payload"]["content"], &"cross-frame"[split..]);
        }
    }

    #[test]
    fn reconnect_frames_cannot_reconstruct_boundary_safe_events() {
        let custody = SecretCustody::new([Arc::new(SecretLease::new("cross-frame"))]);
        let key = CursorKey::new([7; 32]);
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let filter = EventFilter::all();
        let mut state = crate::domain::secret::JsonProjectionState::default();
        for (position, content) in [(1, "cross-"), (2, "frame")] {
            let frame = semantic_frame(
                &key,
                principal,
                project,
                &filter,
                SSE_SCHEMA_VERSION,
                SemanticEvent {
                    position,
                    operation: "run.progress".to_owned(),
                    stream: "run_00000000000000000000000000".to_owned(),
                    payload: serde_json::json!({"content": content}),
                    canonical: Vec::new(),
                    payload_bytes: Vec::new(),
                },
                &custody,
                &mut state,
            )
            .unwrap();
            let mut reconnect = custody.redactor().scanner();
            reconnect.push(&frame.encode());
            assert!(!reconnect.found());
            assert!(!String::from_utf8_lossy(&frame.encode()).contains(REDACTED));
        }
    }

    #[test]
    fn sse_and_page_fail_closed_saturation_preserves_object_bytes_and_digest() {
        let custody = SecretCustody::new([Arc::new(SecretLease::new("page-secret"))]);
        let payload = serde_json::json!({"content": "page-"});
        let payload_bytes = serde_json::to_vec(&payload).unwrap();
        let canonical = br#"{"operation":"custom.event","stream":"run_00000000000000000000000000","payload":{"content":"page-"},"trace_id":"secret"}"#.to_vec();
        let mut page_state = custody.projection_state();
        let page = crate::api::service::project_event_envelopes_with_state(
            &custody,
            vec![(canonical.clone(), payload_bytes.clone())],
            &mut page_state,
        )
        .unwrap()
        .remove(0);
        let mut stream_state = custody.projection_state();
        let SseFrame::Semantic { data, .. } = semantic_frame(
            &CursorKey::new([9; 32]),
            PrincipalId::generate().unwrap(),
            ProjectId::generate().unwrap(),
            &EventFilter::all(),
            SSE_SCHEMA_VERSION,
            SemanticEvent {
                position: 1,
                operation: "custom.event".to_owned(),
                stream: "run_00000000000000000000000000".to_owned(),
                payload: serde_json::json!({"content": "page-"}),
                canonical,
                payload_bytes,
            },
            &custody,
            &mut stream_state,
        )
        .unwrap() else {
            unreachable!()
        };
        let data: serde_json::Value = serde_json::from_slice(&data).unwrap();
        let redacted = serde_json::json!({
            "marker": REDACTED,
            "projection": {"schema_version": 1, "status": "fail_closed"},
        });
        assert_eq!(page.payload, serde_json::to_vec(&redacted).unwrap());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&page.envelope).unwrap()["payload"],
            redacted
        );
        assert_eq!(data["authority_digest"], page.authority_digest);
        assert_eq!(data["projection_digest"], page.digest);
        assert_eq!(data["payload"], redacted);
        assert_eq!(
            data["projected_envelope"],
            String::from_utf8(page.envelope).unwrap()
        );
    }
}
