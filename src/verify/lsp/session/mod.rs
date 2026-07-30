use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    hash::{Hash, Hasher},
    io::{self, BufRead, Read, Write},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use crate::{
    domain::{
        entities::DaemonService,
        events::ContentDigest,
        ids::{DaemonServiceId, PrincipalId, ProcessId, ProjectId, WorkspaceId},
        lifecycle::{ProcessClaim, ProcessOwnership},
    },
    executor::profile::{ExecutorProfile, ProfileDigest, ResourceLimits},
    workspace::revision::RevisionId,
};

pub const LSP_PROTOCOL_VERSION: &str = "3.18";
const MAX_LSP_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_JSON_NESTING: usize = 128;
const MAX_JSON_ITEMS: usize = 100_000;
const MAX_LSP_RELEVANT_FIELDS: usize = 20_000;
const MAX_LSP_URI_BYTES: usize = 16 * 1024;
const MAX_LSP_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_LSP_CODE_BYTES: usize = 1_024;
const MAX_LSP_SOURCE_BYTES: usize = 1_024;
const MAX_LSP_METHOD_BYTES: usize = 256;
const MAX_LSP_NEW_TEXT_BYTES: usize = MAX_LSP_BODY_BYTES;
const MAP_ENTRY_OVERHEAD: usize = 8 * std::mem::size_of::<usize>();

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RevisionPolicy {
    ManagedLive,
    Pinned(RevisionId),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum SessionPurpose {
    Live,
    Shadow(ContentDigest),
}

impl PositionEncoding {
    const fn lsp_name(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Utf32 => "utf-32",
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ServerIdentity {
    pub server_artifact: ContentDigest,
    pub configuration: ContentDigest,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionProfileIdentity {
    digest: ProfileDigest,
    resources: ResourceLimits,
}

impl ExecutionProfileIdentity {
    pub const fn from_profile(profile: &ExecutorProfile) -> Self {
        Self {
            digest: profile.digest(),
            resources: profile.resources(),
        }
    }

    pub const fn digest(&self) -> ProfileDigest {
        self.digest
    }

    pub const fn resources(&self) -> ResourceLimits {
        self.resources
    }
}

impl Hash for ExecutionProfileIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.digest.hash(state);
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionScope {
    pub principal_id: PrincipalId,
    pub project_id: ProjectId,
    pub workspace_id: WorkspaceId,
    pub canonical_root_identity: ContentDigest,
    pub purpose: SessionPurpose,
    pub revision_policy: RevisionPolicy,
    pub server: ServerIdentity,
    pub position_encoding: PositionEncoding,
    pub execution_profile: ExecutionProfileIdentity,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DocumentVersion(i32);

impl DocumentVersion {
    pub const fn new(value: i32) -> Self {
        Self(value)
    }

    pub fn from_i64(value: i64) -> Result<Self, SessionError> {
        i32::try_from(value)
            .map(Self)
            .map_err(|_| SessionError::DocumentVersionOutOfRange)
    }

    pub const fn get(self) -> i32 {
        self.0
    }

    pub fn next(self) -> Result<Self, SessionError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(SessionError::DocumentVersionOverflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RequestId(u32);

impl RequestId {
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RequestMethod(String);

impl RequestMethod {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingToken {
    pub generation: u64,
    pub request_id: RequestId,
    pub workspace_revision: RevisionId,
    pub document_epoch: u64,
    pub uri: String,
    pub document_version: DocumentVersion,
    pub method: RequestMethod,
    pub server: ServerIdentity,
    pub position_encoding: PositionEncoding,
    pub request_position: Option<RequestPosition>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestPosition {
    line: u32,
    character: u32,
}

impl RequestPosition {
    pub const fn line(self) -> u32 {
        self.line
    }

    pub const fn character(self) -> u32 {
        self.character
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PendingTermination {
    ServerRestarted,
    StaleWorkspaceRevision,
    StaleDocumentVersion,
    StaleDocumentEpoch,
    Cancelled,
    DeadlineExceeded,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiscardReason {
    StaleGeneration,
    WrongRequestId,
    TokenMismatch,
    StaleWorkspaceRevision,
    StaleDocumentVersion,
    StaleDocumentEpoch,
    Cancelled,
    DeadlineExceeded,
    ServerRestarted,
    Shutdown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ResponseDisposition {
    Accepted(AcceptedResponse),
    Discarded(DiscardReason),
}

#[derive(Clone, Debug, PartialEq)]
pub struct AcceptedResponse {
    token: PendingToken,
    payload: Value,
}

impl AcceptedResponse {
    pub const fn token(&self) -> &PendingToken {
        &self.token
    }

    pub const fn payload(&self) -> &Value {
        &self.payload
    }

    pub fn result(&self) -> Option<&Value> {
        self.payload.get("result")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AcceptedNotification {
    generation: u64,
    workspace_revision: RevisionId,
    document_epoch: u64,
    uri: String,
    document_version: DocumentVersion,
    server: ServerIdentity,
    position_encoding: PositionEncoding,
    payload: Value,
}

impl AcceptedNotification {
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub const fn workspace_revision(&self) -> RevisionId {
        self.workspace_revision
    }

    pub const fn document_epoch(&self) -> u64 {
        self.document_epoch
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub const fn document_version(&self) -> DocumentVersion {
        self.document_version
    }

    pub const fn server(&self) -> &ServerIdentity {
        &self.server
    }

    pub const fn position_encoding(&self) -> PositionEncoding {
        self.position_encoding
    }

    pub const fn payload(&self) -> &Value {
        &self.payload
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum NotificationDisposition {
    Accepted(AcceptedNotification),
    Discarded(DiscardReason),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionCounters {
    pub accepted: u64,
    pub discarded: u64,
    pub restarts: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    Running,
    Restarting,
    Faulted,
    ShuttingDown,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ManagerUsage {
    pub sessions: usize,
    pub documents: usize,
    pub pending_requests: usize,
    pub live_transports: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionSnapshot {
    pub service_id: DaemonServiceId,
    pub process_id: Option<ProcessId>,
    pub generation: u64,
    pub document_epoch: u64,
    pub state: SessionState,
    pub documents: usize,
    pub pending_requests: usize,
    pub tombstones: usize,
    pub counters: SessionCounters,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnershipRecord {
    pub service: DaemonService,
    pub scope: SessionScope,
    pub process_claim: Option<ProcessClaim>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodecLimits {
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    pub max_frame_bytes: usize,
}

impl Default for CodecLimits {
    fn default() -> Self {
        Self {
            max_header_bytes: 8 * 1024,
            max_body_bytes: 4 * 1024 * 1024,
            max_frame_bytes: 4 * 1024 * 1024 + 8 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionLimits {
    pub max_sessions: usize,
    pub max_documents_per_session: usize,
    pub max_document_bytes: usize,
    pub max_total_document_bytes: usize,
    pub max_pending_requests: usize,
    pub max_tombstones: usize,
    pub max_recent_reaped_process_ids: usize,
    pub max_restarts: u64,
    pub max_uri_bytes: usize,
    pub max_method_bytes: usize,
    pub lifecycle_send_timeout_ticks: u64,
    pub codec: CodecLimits,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_sessions: 32,
            max_documents_per_session: 4_096,
            max_document_bytes: 4 * 1024 * 1024,
            max_total_document_bytes: 128 * 1024 * 1024,
            max_pending_requests: 1_024,
            max_tombstones: 2_048,
            max_recent_reaped_process_ids: 1_024,
            max_restarts: 16,
            max_uri_bytes: 16 * 1024,
            max_method_bytes: 256,
            lifecycle_send_timeout_ticks: 5_000,
            codec: CodecLimits::default(),
        }
    }
}

impl SessionLimits {
    pub(crate) fn valid(self) -> bool {
        self.max_sessions > 0
            && self.max_documents_per_session > 0
            && self.max_document_bytes > 0
            && self.max_total_document_bytes >= self.max_document_bytes
            && self.max_pending_requests > 0
            && self.max_tombstones > 0
            && self.max_recent_reaped_process_ids > 0
            && self.max_restarts > 0
            && self.max_uri_bytes > 0
            && self.max_method_bytes > 0
            && self.lifecycle_send_timeout_ticks > 0
            && self.codec.max_header_bytes > 0
            && self.codec.max_body_bytes > 0
            && self.codec.max_frame_bytes
                >= self
                    .codec
                    .max_header_bytes
                    .saturating_add(self.codec.max_body_bytes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportError {
    LaunchFailed,
    WriteFailed,
    WriteDeadlineExceeded,
    ReadFailed,
    ReadDeadlineExceeded,
    CloseOrReapFailed,
    CloseOrReapDeadlineExceeded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SendContext {
    deadline_tick: u64,
    remaining: Duration,
}

impl SendContext {
    pub const fn deadline_tick(self) -> u64 {
        self.deadline_tick
    }

    /// Concrete budget for a bounded production pipe write and flush.
    pub const fn remaining(self) -> Duration {
        self.remaining
    }

    fn new<C: TickClock>(clock: &C, deadline_tick: u64) -> Self {
        Self {
            deadline_tick,
            remaining: clock.remaining_until(deadline_tick),
        }
    }

    fn refreshed<C: TickClock>(self, clock: &C) -> Self {
        Self {
            deadline_tick: self.deadline_tick,
            remaining: self
                .remaining
                .min(clock.remaining_until(self.deadline_tick)),
        }
    }
}

/// The production implementation must own an `OwnedProcess`/`ProcessTree` lifecycle.
/// Sends and receives must configure pipe I/O from `context.remaining()` and either complete
/// within that budget or return the corresponding deadline error. `receive_frame` must reject a
/// frame larger than `codec_limits.max_frame_bytes` before returning it. The manager also
/// authoritatively checks its clock before and after every call.
/// `Drop` must make one bounded best-effort attempt to close the owned process boundary.
pub trait OwnedLspTransport {
    fn claim(&self) -> ProcessClaim;
    fn initialize(
        &mut self,
        request_frame: &[u8],
        codec_limits: CodecLimits,
        context: SendContext,
    ) -> Result<(), TransportError>;
    fn send_frame(&mut self, frame: &[u8], context: SendContext) -> Result<(), TransportError>;
    fn receive_frame(
        &mut self,
        codec_limits: CodecLimits,
        context: SendContext,
    ) -> Result<Vec<u8>, TransportError>;
    /// Boundedly closes the complete owned process boundary and proves it was reaped.
    fn close_and_reap(&mut self, context: SendContext) -> Result<(), TransportError>;
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReceivedNotification {
    service_id: DaemonServiceId,
    process_id: ProcessId,
    generation: u64,
    frame_bytes: usize,
    disposition: NotificationDisposition,
}

impl ReceivedNotification {
    pub(crate) const fn service_id(&self) -> DaemonServiceId {
        self.service_id
    }

    pub(crate) const fn process_id(&self) -> ProcessId {
        self.process_id
    }

    pub(crate) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) const fn frame_bytes(&self) -> usize {
        self.frame_bytes
    }

    pub(crate) fn into_disposition(self) -> NotificationDisposition {
        self.disposition
    }
}

pub struct LaunchRequest<'a> {
    pub service: &'a DaemonService,
    pub ownership: ProcessOwnership,
    pub scope: &'a SessionScope,
    pub generation: u64,
    pub execution_profile: &'a ExecutionProfileIdentity,
}

/// Production launchers must create their transport through the existing owned-process
/// boundary; test launchers fake this adapter contract, not process ownership itself.
pub trait OwnedLspLauncher {
    type Transport: OwnedLspTransport;

    fn launch(&mut self, request: LaunchRequest<'_>) -> Result<Self::Transport, TransportError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodecError {
    HeaderTooLarge,
    MissingHeaderTerminator,
    InvalidHeader,
    MissingContentLength,
    DuplicateContentLength,
    InvalidContentLength,
    BodyTooLarge,
    FrameTooLarge,
    TruncatedBody,
    TrailingBytes,
    ReadFailed,
    MalformedJson,
    EscapedObjectKey,
    JsonTokenTooLarge,
    InvalidEnvelope,
}

impl fmt::Display for CodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid LSP frame: {self:?}")
    }
}

impl std::error::Error for CodecError {}

#[derive(Clone, Debug, PartialEq)]
pub struct DecodedFrame(Value);

impl DecodedFrame {
    pub fn value(&self) -> &Value {
        &self.0
    }
}

pub struct LspCodec;

impl LspCodec {
    pub fn encode(value: &Value, limits: CodecLimits) -> Result<Vec<u8>, CodecError> {
        validate_envelope(value)?;
        let mut body = CappedWriter::new(limits.max_body_bytes.min(MAX_LSP_BODY_BYTES));
        if serde_json::to_writer(&mut body, value).is_err() {
            return Err(if body.exceeded {
                CodecError::BodyTooLarge
            } else {
                CodecError::MalformedJson
            });
        }
        let body = body.bytes;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let frame_len = header
            .len()
            .checked_add(body.len())
            .ok_or(CodecError::FrameTooLarge)?;
        if header.len() > limits.max_header_bytes || frame_len > limits.max_frame_bytes {
            return Err(CodecError::FrameTooLarge);
        }
        let mut frame = Vec::with_capacity(frame_len);
        frame.extend_from_slice(header.as_bytes());
        frame.extend_from_slice(&body);
        Ok(frame)
    }

    pub fn decode(frame: &[u8], limits: CodecLimits) -> Result<DecodedFrame, CodecError> {
        if frame.len() > limits.max_frame_bytes {
            return Err(CodecError::FrameTooLarge);
        }
        let mut reader = io::Cursor::new(frame);
        let decoded = Self::decode_from(&mut reader, limits)?;
        if usize::try_from(reader.position()).ok() != Some(frame.len()) {
            return Err(CodecError::TrailingBytes);
        }
        Ok(decoded)
    }

    pub fn decode_from(
        reader: &mut impl BufRead,
        limits: CodecLimits,
    ) -> Result<DecodedFrame, CodecError> {
        let mut header = Vec::new();
        loop {
            let remaining = limits
                .max_header_bytes
                .checked_sub(header.len())
                .ok_or(CodecError::HeaderTooLarge)?;
            if remaining == 0 {
                return Err(CodecError::HeaderTooLarge);
            }
            let before = header.len();
            let read = reader
                .take(u64::try_from(remaining.saturating_add(1)).unwrap_or(u64::MAX))
                .read_until(b'\n', &mut header)
                .map_err(|_| CodecError::ReadFailed)?;
            if header.len().saturating_sub(before) > remaining {
                return Err(CodecError::HeaderTooLarge);
            }
            if read == 0 {
                return Err(CodecError::MissingHeaderTerminator);
            }
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let body_len = parse_content_length(&header[..header.len() - 4])?;
        if body_len > limits.max_body_bytes.min(MAX_LSP_BODY_BYTES) {
            return Err(CodecError::BodyTooLarge);
        }
        let frame_len = header
            .len()
            .checked_add(body_len)
            .ok_or(CodecError::FrameTooLarge)?;
        if frame_len > limits.max_frame_bytes {
            return Err(CodecError::FrameTooLarge);
        }
        let mut body = vec![0; body_len];
        if let Err(error) = reader.read_exact(&mut body) {
            return Err(if error.kind() == io::ErrorKind::UnexpectedEof {
                CodecError::TruncatedBody
            } else {
                CodecError::ReadFailed
            });
        }
        preflight_lsp_json(&body)?;
        let value = serde_json::from_slice(&body).map_err(|_| CodecError::MalformedJson)?;
        validate_envelope(&value)?;
        Ok(DecodedFrame(value))
    }
}

fn preflight_lsp_json(bytes: &[u8]) -> Result<(), CodecError> {
    if bytes.len() > MAX_LSP_BODY_BYTES {
        return Err(CodecError::BodyTooLarge);
    }
    let mut depth = 0_usize;
    let mut items = 0_usize;
    let mut relevant_fields = 0_usize;
    let mut pending_string_limit = None;
    let mut pending_annotation_map = false;
    let mut annotation_map_depth = None;
    let mut index = 0_usize;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                let start = index;
                let mut escaped = false;
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        b'"' => {
                            index += 1;
                            break;
                        }
                        b'\\' => {
                            escaped = true;
                            index = index.checked_add(2).ok_or(CodecError::MalformedJson)?;
                        }
                        _ => index += 1,
                    }
                    if index.saturating_sub(start) > MAX_LSP_BODY_BYTES {
                        return Err(CodecError::BodyTooLarge);
                    }
                }
                if index > bytes.len() || bytes.get(index.saturating_sub(1)) != Some(&b'"') {
                    return Err(CodecError::MalformedJson);
                }
                let raw = &bytes[start + 1..index - 1];
                let mut next = index;
                while bytes.get(next).is_some_and(u8::is_ascii_whitespace) {
                    next += 1;
                }
                if bytes.get(next) == Some(&b':') {
                    if escaped {
                        return Err(CodecError::EscapedObjectKey);
                    }
                    let key_limit = if annotation_map_depth == Some(depth) {
                        MAX_LSP_CODE_BYTES
                    } else {
                        MAX_LSP_URI_BYTES
                    };
                    if raw.len() > key_limit {
                        return Err(CodecError::JsonTokenTooLarge);
                    }
                    if matches!(
                        raw,
                        b"newText"
                            | b"uri"
                            | b"targetUri"
                            | b"oldUri"
                            | b"newUri"
                            | b"annotationId"
                            | b"documentChanges"
                            | b"changeAnnotations"
                            | b"diagnostics"
                            | b"message"
                            | b"source"
                            | b"code"
                            | b"label"
                            | b"description"
                            | b"method"
                    ) {
                        relevant_fields = relevant_fields
                            .checked_add(1)
                            .ok_or(CodecError::BodyTooLarge)?;
                        if relevant_fields > MAX_LSP_RELEVANT_FIELDS {
                            return Err(CodecError::BodyTooLarge);
                        }
                    }
                    pending_string_limit = match raw {
                        b"uri" | b"targetUri" | b"oldUri" | b"newUri" => Some(MAX_LSP_URI_BYTES),
                        b"message" | b"label" | b"description" => Some(MAX_LSP_MESSAGE_BYTES),
                        b"source" => Some(MAX_LSP_SOURCE_BYTES),
                        b"code" | b"annotationId" => Some(MAX_LSP_CODE_BYTES),
                        b"method" => Some(MAX_LSP_METHOD_BYTES),
                        b"newText" => Some(MAX_LSP_NEW_TEXT_BYTES),
                        _ => None,
                    };
                    pending_annotation_map =
                        raw == b"changeAnnotations" && annotation_map_depth.is_none();
                } else {
                    pending_annotation_map = false;
                    if let Some(limit) = pending_string_limit.take()
                        && raw.len() > limit
                    {
                        // Escaped JSON is never shorter on the wire than its decoded UTF-8 output.
                        return Err(CodecError::JsonTokenTooLarge);
                    }
                }
            }
            b'{' | b'[' => {
                pending_string_limit = None;
                depth = depth.checked_add(1).ok_or(CodecError::MalformedJson)?;
                if pending_annotation_map && bytes[index] == b'{' {
                    annotation_map_depth = Some(depth);
                }
                pending_annotation_map = false;
                items = items.checked_add(1).ok_or(CodecError::MalformedJson)?;
                if depth > MAX_JSON_NESTING || items > MAX_JSON_ITEMS {
                    return Err(CodecError::BodyTooLarge);
                }
                index += 1;
            }
            b'}' | b']' => {
                if annotation_map_depth == Some(depth) {
                    annotation_map_depth = None;
                }
                depth = depth.checked_sub(1).ok_or(CodecError::MalformedJson)?;
                index += 1;
            }
            b',' => {
                pending_annotation_map = false;
                items = items.checked_add(1).ok_or(CodecError::MalformedJson)?;
                if items > MAX_JSON_ITEMS {
                    return Err(CodecError::BodyTooLarge);
                }
                index += 1;
            }
            byte => {
                if pending_string_limit.is_some() && !byte.is_ascii_whitespace() && byte != b':' {
                    pending_string_limit = None;
                }
                if pending_annotation_map && !byte.is_ascii_whitespace() && byte != b':' {
                    pending_annotation_map = false;
                }
                index += 1;
            }
        }
    }
    if depth == 0 {
        Ok(())
    } else {
        Err(CodecError::MalformedJson)
    }
}

fn document_retained_bytes(uri: &str, text: &str) -> Option<usize> {
    let mut lines = 1_usize;
    for byte in text.bytes() {
        if byte == b'\n' {
            lines = lines.checked_add(1)?;
        }
    }
    lines
        .checked_mul(std::mem::size_of::<usize>())
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Vec<usize>>()))
        .and_then(|bytes| bytes.checked_add(2 * std::mem::size_of::<usize>()))
        .and_then(|bytes| bytes.checked_add(uri.len()))
        .and_then(|bytes| bytes.checked_add(text.len()))
        .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Document>()))
        .and_then(|bytes| bytes.checked_add(MAP_ENTRY_OVERHEAD))
}

struct CappedWriter {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl CappedWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }
}

impl Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("LSP body limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn parse_content_length(header: &[u8]) -> Result<usize, CodecError> {
    let header = std::str::from_utf8(header).map_err(|_| CodecError::InvalidHeader)?;
    let mut content_length = None;
    for line in header.split("\r\n") {
        let (name, value) = line.split_once(':').ok_or(CodecError::InvalidHeader)?;
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err(CodecError::DuplicateContentLength);
            }
            let value = value.trim();
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(CodecError::InvalidContentLength);
            }
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| CodecError::InvalidContentLength)?,
            );
        } else if !name.eq_ignore_ascii_case("content-type") || value.trim().is_empty() {
            return Err(CodecError::InvalidHeader);
        }
    }
    content_length.ok_or(CodecError::MissingContentLength)
}

fn validate_envelope(value: &Value) -> Result<(), CodecError> {
    let object = value.as_object().ok_or(CodecError::InvalidEnvelope)?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(CodecError::InvalidEnvelope);
    }
    if let Some(method) = object.get("method") {
        if method.as_str().is_none()
            || object.contains_key("result")
            || object.contains_key("error")
        {
            return Err(CodecError::InvalidEnvelope);
        }
        if let Some(id) = object.get("id") {
            validate_json_rpc_id(id)?;
        }
        return Ok(());
    }
    let id = object.get("id").ok_or(CodecError::InvalidEnvelope)?;
    validate_json_rpc_id(id)?;
    if object.contains_key("result") == object.contains_key("error") {
        return Err(CodecError::InvalidEnvelope);
    }
    if let Some(error) = object.get("error") {
        let error = error.as_object().ok_or(CodecError::InvalidEnvelope)?;
        if error
            .get("code")
            .and_then(Value::as_i64)
            .and_then(|code| i32::try_from(code).ok())
            .is_none()
            || error.get("message").and_then(Value::as_str).is_none()
        {
            return Err(CodecError::InvalidEnvelope);
        }
    }
    Ok(())
}

fn validate_response(value: &Value) -> Result<(), CodecError> {
    let object = value.as_object().ok_or(CodecError::InvalidEnvelope)?;
    if object.contains_key("method")
        || object.get("id").is_none()
        || object.contains_key("result") == object.contains_key("error")
    {
        return Err(CodecError::InvalidEnvelope);
    }
    Ok(())
}

fn validate_json_rpc_id(id: &Value) -> Result<(), CodecError> {
    if id.as_str().is_some()
        || id
            .as_i64()
            .and_then(|value| i32::try_from(value).ok())
            .is_some()
    {
        Ok(())
    } else {
        Err(CodecError::InvalidEnvelope)
    }
}

fn semantic_request_position(
    method: &str,
    uri: &str,
    params: &Value,
) -> Result<Option<RequestPosition>, SessionError> {
    if !matches!(
        method,
        "textDocument/declaration"
            | "textDocument/definition"
            | "textDocument/typeDefinition"
            | "textDocument/implementation"
            | "textDocument/references"
    ) {
        return Ok(None);
    }
    let text_document = params
        .get("textDocument")
        .and_then(Value::as_object)
        .ok_or(SessionError::InvalidRequestPosition)?;
    if text_document.get("uri").and_then(Value::as_str) != Some(uri) {
        return Err(SessionError::InvalidRequestPosition);
    }
    let position = params
        .get("position")
        .and_then(Value::as_object)
        .ok_or(SessionError::InvalidRequestPosition)?;
    let line = position
        .get("line")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value <= i32::MAX as u32)
        .ok_or(SessionError::InvalidRequestPosition)?;
    let character = position
        .get("character")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value <= i32::MAX as u32)
        .ok_or(SessionError::InvalidRequestPosition)?;
    Ok(Some(RequestPosition { line, character }))
}

fn valid_request_position(
    text: &str,
    position: RequestPosition,
    encoding: PositionEncoding,
) -> bool {
    let target_line = position.line as usize;
    let mut line_start = 0_usize;
    for _ in 0..target_line {
        let Some(newline) = text[line_start..].find('\n') else {
            return false;
        };
        line_start += newline + 1;
    }
    let physical_end = text[line_start..]
        .find('\n')
        .map_or(text.len(), |newline| line_start + newline);
    let line_end =
        if physical_end > line_start && text.as_bytes().get(physical_end - 1) == Some(&b'\r') {
            physical_end - 1
        } else {
            physical_end
        };
    let line = &text[line_start..line_end];
    let character = position.character as usize;
    match encoding {
        PositionEncoding::Utf8 => character <= line.len() && line.is_char_boundary(character),
        PositionEncoding::Utf16 => {
            character == 0
                || line
                    .chars()
                    .scan(0_usize, |units, character| {
                        *units += character.len_utf16();
                        Some(*units)
                    })
                    .any(|units| units == character)
        }
        PositionEncoding::Utf32 => character <= line.chars().count(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionError {
    InvalidLimits,
    InvalidScope,
    InvalidRequestPosition,
    CapacityExceeded,
    DocumentCapacityExceeded,
    DocumentTooLarge,
    DocumentNotFound,
    DocumentAlreadyOpen,
    DocumentVersionOutOfRange,
    DocumentVersionOverflow,
    DocumentEpochOverflow,
    PendingCapacityExceeded,
    RequestIdOverflow,
    GenerationOverflow,
    RestartLimitExceeded,
    SessionNotFound,
    AdmissionClosed,
    RevisionMismatch,
    DeadlineExceeded,
    RequestNotFound,
    InvalidNotification,
    Transport(TransportError),
    InvalidTransportOwner,
    ProcessIdentityReused,
    ShutdownFailed {
        failed_sessions: usize,
        first: TransportError,
    },
    Codec(CodecError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "LSP session error: {self:?}")
    }
}

impl std::error::Error for SessionError {}

impl From<TransportError> for SessionError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl From<CodecError> for SessionError {
    fn from(error: CodecError) -> Self {
        Self::Codec(error)
    }
}

pub trait TickClock {
    fn now_tick(&self) -> u64;
    fn remaining_until(&self, deadline_tick: u64) -> Duration;
}

pub struct MonotonicClock(Instant);

impl Default for MonotonicClock {
    fn default() -> Self {
        Self(Instant::now())
    }
}

impl TickClock for MonotonicClock {
    fn now_tick(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn remaining_until(&self, deadline_tick: u64) -> Duration {
        Duration::from_millis(deadline_tick.saturating_sub(self.now_tick()))
    }
}

struct ProcessIds {
    active: HashSet<ProcessId>,
    recent: HashSet<ProcessId>,
    recent_order: VecDeque<ProcessId>,
    max_recent: usize,
}

impl ProcessIds {
    fn new(max_recent: usize) -> Self {
        Self {
            active: HashSet::new(),
            recent: HashSet::new(),
            recent_order: VecDeque::new(),
            max_recent,
        }
    }

    fn admit(&mut self, process_id: ProcessId) -> bool {
        if self.active.contains(&process_id) || self.recent.contains(&process_id) {
            return false;
        }
        self.active.insert(process_id)
    }

    fn reaped(&mut self, process_id: ProcessId) {
        if !self.active.remove(&process_id) || !self.recent.insert(process_id) {
            return;
        }
        self.recent_order.push_back(process_id);
        while self.recent_order.len() > self.max_recent {
            if let Some(oldest) = self.recent_order.pop_front() {
                self.recent.remove(&oldest);
            }
        }
    }

    fn retained_len(&self) -> usize {
        self.active.len() + self.recent.len()
    }
}

struct Document {
    version: DocumentVersion,
    text: String,
}

struct Pending {
    token: PendingToken,
    deadline_tick: u64,
}

struct Session<T> {
    service: DaemonService,
    scope: SessionScope,
    workspace_revision: RevisionId,
    generation: u64,
    document_epoch: u64,
    transport: Option<T>,
    documents: HashMap<String, Document>,
    document_bytes: usize,
    pending: HashMap<RequestId, Pending>,
    tombstones: HashMap<(u64, RequestId), PendingTermination>,
    tombstone_order: VecDeque<(u64, RequestId)>,
    next_request_id: u32,
    state: SessionState,
    counters: SessionCounters,
}

pub struct LspSessionManager<L: OwnedLspLauncher, C: TickClock = MonotonicClock> {
    launcher: L,
    clock: C,
    limits: SessionLimits,
    sessions: HashMap<DaemonServiceId, Session<L::Transport>>,
    scopes: HashMap<SessionScope, DaemonServiceId>,
    // G03's durable process registry remains the global identity authority; this is the bounded
    // manager-local replay fence.
    process_ids: ProcessIds,
    admission_open: bool,
}

impl<L: OwnedLspLauncher> LspSessionManager<L, MonotonicClock> {
    pub fn new(launcher: L, limits: SessionLimits) -> Result<Self, SessionError> {
        Self::with_clock(launcher, limits, MonotonicClock::default())
    }
}

impl<L: OwnedLspLauncher, C: TickClock> LspSessionManager<L, C> {
    pub fn with_clock(launcher: L, limits: SessionLimits, clock: C) -> Result<Self, SessionError> {
        if !limits.valid() {
            return Err(SessionError::InvalidLimits);
        }
        Ok(Self {
            launcher,
            clock,
            limits,
            sessions: HashMap::new(),
            scopes: HashMap::new(),
            process_ids: ProcessIds::new(limits.max_recent_reaped_process_ids),
            admission_open: true,
        })
    }

    pub fn open(
        &mut self,
        scope: SessionScope,
        workspace_revision: RevisionId,
    ) -> Result<DaemonServiceId, SessionError> {
        let deadline_tick = self.lifecycle_deadline_tick();
        self.open_until(scope, workspace_revision, deadline_tick)
    }

    pub(crate) fn open_until(
        &mut self,
        scope: SessionScope,
        workspace_revision: RevisionId,
        deadline_tick: u64,
    ) -> Result<DaemonServiceId, SessionError> {
        self.require_admission()?;
        if deadline_tick <= self.clock.now_tick() {
            return Err(SessionError::DeadlineExceeded);
        }
        self.validate_scope(&scope, workspace_revision)?;
        if let Some(service_id) = self.scopes.get(&scope).copied() {
            self.set_workspace_revision(service_id, workspace_revision)?;
            let healthy = self.sessions.get(&service_id).is_some_and(|session| {
                session.state == SessionState::Running && session.transport.is_some()
            });
            if !healthy {
                self.server_crashed(service_id)?;
            }
            return Ok(service_id);
        }
        if self.sessions.len() >= self.limits.max_sessions {
            let idle = self
                .sessions
                .iter()
                .filter(|(_, session)| session.documents.is_empty() && session.pending.is_empty())
                .map(|(id, _)| *id)
                .min();
            let Some(idle) = idle else {
                return Err(SessionError::CapacityExceeded);
            };
            self.close_session_until(idle, deadline_tick)?;
        }
        let service = DaemonService::new(scope.principal_id, scope.project_id, scope.workspace_id)
            .map_err(|_| SessionError::CapacityExceeded)?;
        let generation = 1;
        let send_context = SendContext::new(&self.clock, deadline_tick);
        let mut transport = self.launch(&service, &scope, generation)?;
        if let Err(error) = send_initialize(
            self.limits,
            &self.clock,
            send_context,
            &mut transport,
            &service,
            &scope,
            generation,
        ) {
            reap_transport(
                &mut transport,
                &mut self.process_ids,
                &self.clock,
                send_context,
            )?;
            return Err(error);
        }
        let service_id = service.id;
        self.scopes.insert(scope.clone(), service_id);
        self.sessions.insert(
            service_id,
            Session {
                service,
                scope,
                workspace_revision,
                generation,
                document_epoch: 0,
                transport: Some(transport),
                documents: HashMap::new(),
                document_bytes: 0,
                pending: HashMap::new(),
                tombstones: HashMap::new(),
                tombstone_order: VecDeque::new(),
                next_request_id: 1,
                state: SessionState::Running,
                counters: SessionCounters::default(),
            },
        );
        Ok(service_id)
    }

    pub fn open_document(
        &mut self,
        service_id: DaemonServiceId,
        uri: String,
        version: DocumentVersion,
        text: String,
    ) -> Result<(), SessionError> {
        let deadline_tick = self.lifecycle_deadline_tick();
        self.open_document_until(service_id, uri, version, text, deadline_tick)
    }

    pub(crate) fn open_document_until(
        &mut self,
        service_id: DaemonServiceId,
        mut uri: String,
        version: DocumentVersion,
        mut text: String,
        deadline_tick: u64,
    ) -> Result<(), SessionError> {
        self.require_admission()?;
        if deadline_tick <= self.clock.now_tick() {
            return Err(SessionError::DeadlineExceeded);
        }
        uri.shrink_to_fit();
        text.shrink_to_fit();
        self.validate_uri(&uri)?;
        if text.len() > self.limits.max_document_bytes {
            return Err(SessionError::DocumentTooLarge);
        }
        let limits = self.limits;
        let context = SendContext::new(&self.clock, deadline_tick);
        let clock = &self.clock;
        let process_ids = &mut self.process_ids;
        let session = self
            .sessions
            .get_mut(&service_id)
            .ok_or(SessionError::SessionNotFound)?;
        require_running(session)?;
        if session.documents.contains_key(&uri) {
            return Err(SessionError::DocumentAlreadyOpen);
        }
        let retained =
            document_retained_bytes(&uri, &text).ok_or(SessionError::DocumentCapacityExceeded)?;
        if session.documents.len() >= limits.max_documents_per_session
            || session
                .document_bytes
                .checked_add(retained)
                .is_none_or(|total| total > limits.max_total_document_bytes)
        {
            return Err(SessionError::DocumentCapacityExceeded);
        }
        let next_epoch = next_document_epoch(session)?;
        send_session(
            session,
            limits,
            clock,
            context,
            process_ids,
            &did_open(&uri, version, &text),
        )?;
        session.document_bytes += retained;
        session.documents.insert(uri, Document { version, text });
        session.document_epoch = next_epoch;
        cancel_and_terminate_matching(
            session,
            limits,
            clock,
            context,
            process_ids,
            |_| true,
            PendingTermination::StaleDocumentEpoch,
        )?;
        Ok(())
    }

    pub fn update_document(
        &mut self,
        service_id: DaemonServiceId,
        uri: &str,
        text: String,
    ) -> Result<DocumentVersion, SessionError> {
        let deadline_tick = self.lifecycle_deadline_tick();
        self.update_document_until(service_id, uri, text, deadline_tick)
    }

    pub(crate) fn update_document_until(
        &mut self,
        service_id: DaemonServiceId,
        uri: &str,
        mut text: String,
        deadline_tick: u64,
    ) -> Result<DocumentVersion, SessionError> {
        self.require_admission()?;
        if deadline_tick <= self.clock.now_tick() {
            return Err(SessionError::DeadlineExceeded);
        }
        text.shrink_to_fit();
        if text.len() > self.limits.max_document_bytes {
            return Err(SessionError::DocumentTooLarge);
        }
        let limits = self.limits;
        let context = SendContext::new(&self.clock, deadline_tick);
        let clock = &self.clock;
        let process_ids = &mut self.process_ids;
        let session = self
            .sessions
            .get_mut(&service_id)
            .ok_or(SessionError::SessionNotFound)?;
        require_running(session)?;
        let document = session
            .documents
            .get(uri)
            .ok_or(SessionError::DocumentNotFound)?;
        let version = document.version.next()?;
        let old_retained = document_retained_bytes(uri, &document.text)
            .ok_or(SessionError::DocumentCapacityExceeded)?;
        let new_retained =
            document_retained_bytes(uri, &text).ok_or(SessionError::DocumentCapacityExceeded)?;
        let total = session
            .document_bytes
            .checked_sub(old_retained)
            .and_then(|total| total.checked_add(new_retained))
            .ok_or(SessionError::DocumentCapacityExceeded)?;
        if total > limits.max_total_document_bytes {
            return Err(SessionError::DocumentCapacityExceeded);
        }
        let next_epoch = next_document_epoch(session)?;
        send_session(
            session,
            limits,
            clock,
            context,
            process_ids,
            &did_change(uri, version, &text),
        )?;
        let document = session
            .documents
            .get_mut(uri)
            .expect("document was checked");
        document.version = version;
        document.text = text;
        session.document_bytes = total;
        session.document_epoch = next_epoch;
        cancel_and_terminate_matching(
            session,
            limits,
            clock,
            context,
            process_ids,
            |_| true,
            PendingTermination::StaleDocumentEpoch,
        )?;
        Ok(version)
    }

    pub fn close_document(
        &mut self,
        service_id: DaemonServiceId,
        uri: &str,
    ) -> Result<(), SessionError> {
        let deadline_tick = self.lifecycle_deadline_tick();
        self.close_document_until(service_id, uri, deadline_tick)
    }

    pub(crate) fn close_document_until(
        &mut self,
        service_id: DaemonServiceId,
        uri: &str,
        deadline_tick: u64,
    ) -> Result<(), SessionError> {
        let limits = self.limits;
        let context = SendContext::new(&self.clock, deadline_tick);
        let clock = &self.clock;
        let process_ids = &mut self.process_ids;
        let session = self
            .sessions
            .get_mut(&service_id)
            .ok_or(SessionError::SessionNotFound)?;
        require_running(session)?;
        let document = session
            .documents
            .get(uri)
            .ok_or(SessionError::DocumentNotFound)?;
        let bytes = document_retained_bytes(uri, &document.text)
            .ok_or(SessionError::DocumentCapacityExceeded)?;
        let next_epoch = next_document_epoch(session)?;
        send_session(
            session,
            limits,
            clock,
            context,
            process_ids,
            &did_close(uri),
        )?;
        session.documents.remove(uri);
        session.document_bytes -= bytes;
        session.document_epoch = next_epoch;
        cancel_and_terminate_matching(
            session,
            limits,
            clock,
            context,
            process_ids,
            |_| true,
            PendingTermination::StaleDocumentEpoch,
        )?;
        Ok(())
    }

    pub fn set_workspace_revision(
        &mut self,
        service_id: DaemonServiceId,
        revision: RevisionId,
    ) -> Result<(), SessionError> {
        let limits = self.limits;
        let context = self.lifecycle_send_context();
        let clock = &self.clock;
        let process_ids = &mut self.process_ids;
        let session = self
            .sessions
            .get_mut(&service_id)
            .ok_or(SessionError::SessionNotFound)?;
        if let RevisionPolicy::Pinned(pinned) = session.scope.revision_policy
            && pinned != revision
        {
            return Err(SessionError::RevisionMismatch);
        }
        if session.workspace_revision != revision {
            let next_epoch = next_document_epoch(session)?;
            session.workspace_revision = revision;
            session.document_epoch = next_epoch;
            cancel_and_terminate_matching(
                session,
                limits,
                clock,
                context,
                process_ids,
                |_| true,
                PendingTermination::StaleWorkspaceRevision,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn request(
        &mut self,
        service_id: DaemonServiceId,
        workspace_revision: RevisionId,
        uri: &str,
        method: &str,
        params: Value,
        deadline_tick: u64,
    ) -> Result<PendingToken, SessionError> {
        self.require_admission()?;
        if deadline_tick <= self.clock.now_tick() {
            return Err(SessionError::DeadlineExceeded);
        }
        if method.is_empty()
            || method.len() > self.limits.max_method_bytes
            || method.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(SessionError::InvalidScope);
        }
        let request_position = semantic_request_position(method, uri, &params)?;
        let limits = self.limits;
        let clock = &self.clock;
        let process_ids = &mut self.process_ids;
        let session = self
            .sessions
            .get_mut(&service_id)
            .ok_or(SessionError::SessionNotFound)?;
        require_running(session)?;
        if workspace_revision != session.workspace_revision {
            return Err(SessionError::RevisionMismatch);
        }
        let document = session
            .documents
            .get(uri)
            .ok_or(SessionError::DocumentNotFound)?;
        if request_position.is_some_and(|position| {
            !valid_request_position(&document.text, position, session.scope.position_encoding)
        }) {
            return Err(SessionError::InvalidRequestPosition);
        }
        if session.pending.len() >= limits.max_pending_requests {
            return Err(SessionError::PendingCapacityExceeded);
        }
        if session.next_request_id > i32::MAX as u32 {
            return Err(SessionError::RequestIdOverflow);
        }
        let request_id = RequestId(session.next_request_id);
        session.next_request_id = session
            .next_request_id
            .checked_add(1)
            .ok_or(SessionError::RequestIdOverflow)?;
        let token = PendingToken {
            generation: session.generation,
            request_id,
            workspace_revision,
            document_epoch: session.document_epoch,
            uri: uri.to_owned(),
            document_version: document.version,
            method: RequestMethod(method.to_owned()),
            server: session.scope.server.clone(),
            position_encoding: session.scope.position_encoding,
            request_position,
        };
        send_session(
            session,
            limits,
            clock,
            SendContext::new(clock, deadline_tick),
            process_ids,
            &json!({
                "jsonrpc": "2.0",
                "id": request_id.get(),
                "method": method,
                "params": params,
            }),
        )?;
        session.pending.insert(
            request_id,
            Pending {
                token: token.clone(),
                deadline_tick,
            },
        );
        Ok(token)
    }

    pub fn receive_response(
        &mut self,
        service_id: DaemonServiceId,
        generation: u64,
        frame: &[u8],
    ) -> Result<ResponseDisposition, SessionError> {
        self.receive_raw(service_id, generation, None, frame)
    }

    pub fn receive_captured_response(
        &mut self,
        service_id: DaemonServiceId,
        captured: &PendingToken,
        frame: &[u8],
    ) -> Result<ResponseDisposition, SessionError> {
        self.receive_raw(service_id, captured.generation, Some(captured), frame)
    }

    pub fn receive_notification(
        &mut self,
        service_id: DaemonServiceId,
        generation: u64,
        frame: &[u8],
    ) -> Result<NotificationDisposition, SessionError> {
        let codec_limits = self.limits.codec;
        let session = self.session_mut(service_id)?;
        if generation != session.generation {
            session.counters.discarded = session.counters.discarded.saturating_add(1);
            return Ok(NotificationDisposition::Discarded(
                DiscardReason::StaleGeneration,
            ));
        }
        let decoded = LspCodec::decode(frame, codec_limits)?;
        let value = decoded.value();
        if value.get("id").is_some()
            || value.get("method").and_then(Value::as_str)
                != Some("textDocument/publishDiagnostics")
        {
            return Err(SessionError::InvalidNotification);
        }
        let params = value
            .get("params")
            .and_then(Value::as_object)
            .ok_or(SessionError::InvalidNotification)?;
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or(SessionError::InvalidNotification)?;
        let version = params
            .get("version")
            .and_then(Value::as_i64)
            .ok_or(SessionError::InvalidNotification)
            .and_then(DocumentVersion::from_i64)?;
        self.validate_uri(uri)?;
        let session = self.session_mut(service_id)?;
        let document = session
            .documents
            .get(uri)
            .ok_or(SessionError::DocumentNotFound)?;
        if document.version != version {
            session.counters.discarded = session.counters.discarded.saturating_add(1);
            return Ok(NotificationDisposition::Discarded(
                DiscardReason::StaleDocumentVersion,
            ));
        }
        session.counters.accepted = session.counters.accepted.saturating_add(1);
        Ok(NotificationDisposition::Accepted(AcceptedNotification {
            generation,
            workspace_revision: session.workspace_revision,
            document_epoch: session.document_epoch,
            uri: uri.to_owned(),
            document_version: version,
            server: session.scope.server.clone(),
            position_encoding: session.scope.position_encoding,
            payload: value
                .get("params")
                .cloned()
                .ok_or(SessionError::InvalidNotification)?,
        }))
    }

    pub(crate) fn receive_current_notification(
        &mut self,
        service_id: DaemonServiceId,
        deadline_tick: u64,
    ) -> Result<ReceivedNotification, SessionError> {
        let context = SendContext::new(&self.clock, deadline_tick);
        if context.remaining().is_zero() {
            self.fence_receive_failure(service_id)?;
            return Err(TransportError::ReadDeadlineExceeded.into());
        }
        let codec_limits = self.limits.codec;
        let (process_id, generation, frame) = {
            let session = self.session_mut(service_id)?;
            require_running(session)?;
            let transport = session
                .transport
                .as_mut()
                .ok_or(SessionError::AdmissionClosed)?;
            let process_id = transport.claim().process_id;
            let generation = session.generation;
            match transport.receive_frame(codec_limits, context) {
                Ok(frame) => (process_id, generation, frame),
                Err(error) => {
                    self.fence_receive_failure(service_id)?;
                    return Err(error.into());
                }
            }
        };
        if self.clock.remaining_until(deadline_tick).is_zero() {
            self.fence_receive_failure(service_id)?;
            return Err(TransportError::ReadDeadlineExceeded.into());
        }
        if frame.len() > codec_limits.max_frame_bytes {
            self.fence_receive_failure(service_id)?;
            return Err(CodecError::FrameTooLarge.into());
        }
        let frame_bytes = frame.len();
        let disposition = match self.receive_notification(service_id, generation, &frame) {
            Ok(disposition) => disposition,
            Err(error) => {
                self.fence_receive_failure(service_id)?;
                return Err(error);
            }
        };
        Ok(ReceivedNotification {
            service_id,
            process_id,
            generation,
            frame_bytes,
            disposition,
        })
    }

    fn receive_raw(
        &mut self,
        service_id: DaemonServiceId,
        generation: u64,
        captured: Option<&PendingToken>,
        frame: &[u8],
    ) -> Result<ResponseDisposition, SessionError> {
        let session = self.session_mut(service_id)?;
        if generation != session.generation {
            return Ok(discard(session, DiscardReason::StaleGeneration));
        }
        let decoded = LspCodec::decode(frame, self.limits.codec)?;
        self.receive_decoded(service_id, generation, captured, decoded)
    }

    fn receive_decoded(
        &mut self,
        service_id: DaemonServiceId,
        generation: u64,
        captured: Option<&PendingToken>,
        frame: DecodedFrame,
    ) -> Result<ResponseDisposition, SessionError> {
        validate_response(&frame.0)?;
        let raw_id = frame
            .0
            .get("id")
            .and_then(Value::as_u64)
            .and_then(|id| u32::try_from(id).ok());
        let now = self.clock.now_tick();
        let limits = self.limits;
        let session = self.session_mut(service_id)?;
        if generation != session.generation {
            return Ok(discard(session, DiscardReason::StaleGeneration));
        }
        let Some(request_id) = raw_id.map(RequestId) else {
            return Ok(discard(session, DiscardReason::WrongRequestId));
        };
        if let Some(captured) = captured
            && captured.request_id != request_id
        {
            return Ok(discard(session, DiscardReason::WrongRequestId));
        }
        let Some(pending) = session.pending.get(&request_id) else {
            let reason = session
                .tombstones
                .get(&(generation, request_id))
                .copied()
                .map_or(DiscardReason::WrongRequestId, discard_for_termination);
            return Ok(discard(session, reason));
        };
        if pending.deadline_tick <= now {
            let pending = session
                .pending
                .remove(&request_id)
                .expect("pending was checked");
            push_tombstone(
                session,
                &pending.token,
                PendingTermination::DeadlineExceeded,
                limits.max_tombstones,
            );
            return Ok(discard(session, DiscardReason::DeadlineExceeded));
        }
        if captured.is_some_and(|captured| captured != &pending.token) {
            return Ok(discard(session, DiscardReason::TokenMismatch));
        }
        let reason = if pending.token.generation != session.generation {
            Some(DiscardReason::StaleGeneration)
        } else if pending.token.workspace_revision != session.workspace_revision {
            Some(DiscardReason::StaleWorkspaceRevision)
        } else if pending.token.document_epoch != session.document_epoch {
            Some(DiscardReason::StaleDocumentEpoch)
        } else {
            match session.documents.get(&pending.token.uri) {
                Some(document) if document.version == pending.token.document_version => None,
                _ => Some(DiscardReason::StaleDocumentVersion),
            }
        };
        if let Some(reason) = reason {
            let pending = session
                .pending
                .remove(&request_id)
                .expect("pending was checked");
            push_tombstone(
                session,
                &pending.token,
                termination_for_discard(reason),
                limits.max_tombstones,
            );
            return Ok(discard(session, reason));
        }
        let pending = session
            .pending
            .remove(&request_id)
            .expect("pending was checked");
        session.counters.accepted = session.counters.accepted.saturating_add(1);
        Ok(ResponseDisposition::Accepted(AcceptedResponse {
            token: pending.token,
            payload: frame.0,
        }))
    }

    pub fn cancel_request(
        &mut self,
        service_id: DaemonServiceId,
        request_id: RequestId,
    ) -> Result<(), SessionError> {
        let limits = self.limits;
        let context = self.lifecycle_send_context();
        let clock = &self.clock;
        let process_ids = &mut self.process_ids;
        let session = self
            .sessions
            .get_mut(&service_id)
            .ok_or(SessionError::SessionNotFound)?;
        let pending = session
            .pending
            .remove(&request_id)
            .ok_or(SessionError::RequestNotFound)?;
        push_tombstone(
            session,
            &pending.token,
            PendingTermination::Cancelled,
            limits.max_tombstones,
        );
        send_cancel(session, limits, clock, context, process_ids, request_id)
    }

    pub fn expire_deadlines(&mut self) -> Result<usize, SessionError> {
        let now = self.clock.now_tick();
        let context = self.lifecycle_send_context();
        let mut expired = 0;
        let mut first_error = None;
        let process_ids = &mut self.process_ids;
        for session in self.sessions.values_mut() {
            let request_ids = session
                .pending
                .iter()
                .filter_map(|(id, pending)| (pending.deadline_tick <= now).then_some(*id))
                .collect::<Vec<_>>();
            for request_id in request_ids {
                match expire_one(
                    session,
                    request_id,
                    self.limits,
                    &self.clock,
                    context,
                    process_ids,
                ) {
                    Ok(true) => expired += 1,
                    Ok(false) => {}
                    Err(error) => {
                        expired += 1;
                        first_error.get_or_insert(error);
                    }
                }
            }
        }
        first_error.map_or(Ok(expired), Err)
    }

    pub fn server_crashed(
        &mut self,
        service_id: DaemonServiceId,
    ) -> Result<Vec<(PendingToken, PendingTermination)>, SessionError> {
        let limits = self.limits;
        let context = self.lifecycle_send_context();
        let (service, scope, generation, terminated) = {
            let process_ids = &mut self.process_ids;
            let session = self
                .sessions
                .get_mut(&service_id)
                .ok_or(SessionError::SessionNotFound)?;
            if matches!(
                session.state,
                SessionState::ShuttingDown | SessionState::Closed
            ) {
                return Err(SessionError::AdmissionClosed);
            }
            let (terminated, close_result) = fence_session(
                session,
                limits.max_tombstones,
                process_ids,
                &self.clock,
                context,
            );
            close_result?;
            if session.counters.restarts >= limits.max_restarts {
                return Err(SessionError::RestartLimitExceeded);
            }
            session.counters.restarts += 1;
            session.state = SessionState::Restarting;
            let generation = session.generation;
            (
                session.service.clone(),
                session.scope.clone(),
                generation,
                terminated,
            )
        };

        let mut transport = match self.launch(&service, &scope, generation) {
            Ok(transport) => transport,
            Err(error) => {
                self.session_mut(service_id)?.state = SessionState::Faulted;
                return Err(error);
            }
        };
        if let Err(error) = send_initialize(
            limits,
            &self.clock,
            context,
            &mut transport,
            &service,
            &scope,
            generation,
        ) {
            reap_transport(&mut transport, &mut self.process_ids, &self.clock, context)?;
            self.session_mut(service_id)?.state = SessionState::Faulted;
            return Err(error);
        }
        let replay = {
            let clock = &self.clock;
            let session = self
                .sessions
                .get_mut(&service_id)
                .ok_or(SessionError::SessionNotFound)?;
            session.documents.iter().try_for_each(|(uri, document)| {
                send_value(
                    &mut transport,
                    limits,
                    clock,
                    context,
                    &did_open(uri, document.version, &document.text),
                )
            })
        };
        if let Err(error) = replay {
            reap_transport(&mut transport, &mut self.process_ids, &self.clock, context)?;
            self.session_mut(service_id)?.state = SessionState::Faulted;
            return Err(error);
        }
        let session = self.session_mut(service_id)?;
        session.transport = Some(transport);
        session.state = SessionState::Running;
        Ok(terminated)
    }

    pub fn close_session(&mut self, service_id: DaemonServiceId) -> Result<(), SessionError> {
        let deadline_tick = self.lifecycle_deadline_tick();
        self.close_session_until(service_id, deadline_tick)
    }

    pub(crate) fn close_session_until(
        &mut self,
        service_id: DaemonServiceId,
        deadline_tick: u64,
    ) -> Result<(), SessionError> {
        let limits = self.limits;
        let max_tombstones = self.limits.max_tombstones;
        let scope = self
            .sessions
            .get(&service_id)
            .ok_or(SessionError::SessionNotFound)?
            .scope
            .clone();
        let result = {
            let context = SendContext::new(&self.clock, deadline_tick);
            let clock = &self.clock;
            let process_ids = &mut self.process_ids;
            let session = self
                .sessions
                .get_mut(&service_id)
                .ok_or(SessionError::SessionNotFound)?;
            let mut notification_error = None;
            if let Some(transport) = session.transport.as_mut() {
                for uri in session.documents.keys() {
                    if let Err(error) =
                        send_value(transport, limits, clock, context, &did_close(uri))
                    {
                        notification_error.get_or_insert(error);
                        break;
                    }
                }
            }
            if notification_error.is_some() {
                session.generation = session.generation.saturating_add(1);
            }
            session.state = SessionState::ShuttingDown;
            terminate_all(session, PendingTermination::Shutdown, max_tombstones);
            match close_transport(session, process_ids, clock, context) {
                Ok(()) => (true, notification_error.map_or(Ok(()), Err)),
                Err(error) => (false, Err(error)),
            }
        };
        if result.0 {
            self.sessions.remove(&service_id);
            self.scopes.remove(&scope);
        }
        result.1
    }

    pub fn shutdown(&mut self) -> Result<(), SessionError> {
        self.admission_open = false;
        let ids = self.sessions.keys().copied().collect::<Vec<_>>();
        let mut closed = Vec::new();
        let mut first = None;
        let mut failed_sessions = 0;
        let context = SendContext::new(&self.clock, self.lifecycle_deadline_tick());
        let process_ids = &mut self.process_ids;
        for service_id in ids {
            let session = self
                .sessions
                .get_mut(&service_id)
                .expect("session ID came from the map");
            session.state = SessionState::ShuttingDown;
            terminate_all(
                session,
                PendingTermination::Shutdown,
                self.limits.max_tombstones,
            );
            match close_transport(session, process_ids, &self.clock, context) {
                Ok(()) => closed.push((service_id, session.scope.clone())),
                Err(SessionError::Transport(error)) => {
                    first.get_or_insert(error);
                    failed_sessions += 1;
                }
                Err(_) => unreachable!("transport close has one typed failure"),
            }
        }
        for (service_id, scope) in closed {
            self.sessions.remove(&service_id);
            self.scopes.remove(&scope);
        }
        match first {
            Some(first) => Err(SessionError::ShutdownFailed {
                failed_sessions,
                first,
            }),
            None => Ok(()),
        }
    }

    pub fn snapshot(&self, service_id: DaemonServiceId) -> Result<SessionSnapshot, SessionError> {
        let session = self
            .sessions
            .get(&service_id)
            .ok_or(SessionError::SessionNotFound)?;
        Ok(SessionSnapshot {
            service_id,
            process_id: session
                .transport
                .as_ref()
                .map(|transport| transport.claim().process_id),
            generation: session.generation,
            document_epoch: session.document_epoch,
            state: session.state,
            documents: session.documents.len(),
            pending_requests: session.pending.len(),
            tombstones: session.tombstones.len(),
            counters: session.counters,
        })
    }

    pub fn ownership_inventory(&self) -> Vec<OwnershipRecord> {
        let mut records = self
            .sessions
            .values()
            .map(|session| OwnershipRecord {
                service: session.service.clone(),
                scope: session.scope.clone(),
                process_claim: session.transport.as_ref().map(OwnedLspTransport::claim),
            })
            .collect::<Vec<_>>();
        records.sort_by_key(|record| record.service.id);
        records
    }

    pub fn usage(&self) -> ManagerUsage {
        ManagerUsage {
            sessions: self.sessions.len(),
            documents: self.sessions.values().map(|s| s.documents.len()).sum(),
            pending_requests: self.sessions.values().map(|s| s.pending.len()).sum(),
            live_transports: self
                .sessions
                .values()
                .filter(|session| session.transport.is_some())
                .count(),
        }
    }

    pub fn now_tick(&self) -> u64 {
        self.clock.now_tick()
    }

    pub(crate) fn remaining_until(&self, deadline_tick: u64) -> Duration {
        self.clock.remaining_until(deadline_tick)
    }

    pub fn retained_process_id_count(&self) -> usize {
        self.process_ids.retained_len()
    }

    pub fn launcher(&self) -> &L {
        &self.launcher
    }

    fn launch(
        &mut self,
        service: &DaemonService,
        scope: &SessionScope,
        generation: u64,
    ) -> Result<L::Transport, SessionError> {
        if self.process_ids.active.len() >= self.limits.max_sessions {
            return Err(SessionError::CapacityExceeded);
        }
        let ownership = ProcessOwnership::DaemonService(service.id);
        let mut transport = self.launcher.launch(LaunchRequest {
            service,
            ownership,
            scope,
            generation,
            execution_profile: &scope.execution_profile,
        })?;
        let context = SendContext::new(&self.clock, self.lifecycle_deadline_tick());
        if transport.claim().owner != ownership {
            close_owned_transport(&mut transport, &self.clock, context)?;
            return Err(SessionError::InvalidTransportOwner);
        }
        if !self.process_ids.admit(transport.claim().process_id) {
            close_owned_transport(&mut transport, &self.clock, context)?;
            return Err(SessionError::ProcessIdentityReused);
        }
        Ok(transport)
    }

    fn lifecycle_send_context(&self) -> SendContext {
        let deadline_tick = self.lifecycle_deadline_tick();
        SendContext::new(&self.clock, deadline_tick)
    }

    fn lifecycle_deadline_tick(&self) -> u64 {
        self.clock
            .now_tick()
            .saturating_add(self.limits.lifecycle_send_timeout_ticks)
    }

    fn session_mut(
        &mut self,
        service_id: DaemonServiceId,
    ) -> Result<&mut Session<L::Transport>, SessionError> {
        self.sessions
            .get_mut(&service_id)
            .ok_or(SessionError::SessionNotFound)
    }

    fn fence_receive_failure(&mut self, service_id: DaemonServiceId) -> Result<(), SessionError> {
        let context = SendContext::new(&self.clock, self.lifecycle_deadline_tick());
        if let Some(session) = self.sessions.get_mut(&service_id) {
            let (_, result) = fence_session(
                session,
                self.limits.max_tombstones,
                &mut self.process_ids,
                &self.clock,
                context,
            );
            result?;
        }
        Ok(())
    }

    fn require_admission(&self) -> Result<(), SessionError> {
        if self.admission_open {
            Ok(())
        } else {
            Err(SessionError::AdmissionClosed)
        }
    }

    fn validate_scope(
        &self,
        scope: &SessionScope,
        revision: RevisionId,
    ) -> Result<(), SessionError> {
        if matches!(scope.revision_policy, RevisionPolicy::Pinned(pinned) if pinned != revision) {
            return Err(SessionError::RevisionMismatch);
        }
        if matches!(scope.purpose, SessionPurpose::Shadow(_))
            && !matches!(scope.revision_policy, RevisionPolicy::Pinned(_))
        {
            return Err(SessionError::InvalidScope);
        }
        if !scope.execution_profile.resources().finite() {
            return Err(SessionError::InvalidScope);
        }
        Ok(())
    }

    fn validate_uri(&self, uri: &str) -> Result<(), SessionError> {
        if uri.is_empty()
            || uri.len() > self.limits.max_uri_bytes
            || !uri.contains(':')
            || uri.bytes().any(|byte| byte.is_ascii_control())
        {
            return Err(SessionError::InvalidScope);
        }
        Ok(())
    }
}

impl<L: OwnedLspLauncher, C: TickClock> Drop for LspSessionManager<L, C> {
    fn drop(&mut self) {
        let _ = self.shutdown();
        let context = SendContext::new(&self.clock, self.lifecycle_deadline_tick());
        let process_ids = &mut self.process_ids;
        for session in self.sessions.values_mut() {
            let _ = close_transport(session, process_ids, &self.clock, context);
        }
    }
}

fn require_running<T>(session: &Session<T>) -> Result<(), SessionError> {
    if session.state == SessionState::Running && session.transport.is_some() {
        Ok(())
    } else {
        Err(SessionError::AdmissionClosed)
    }
}

fn next_document_epoch<T>(session: &Session<T>) -> Result<u64, SessionError> {
    session
        .document_epoch
        .checked_add(1)
        .ok_or(SessionError::DocumentEpochOverflow)
}

fn send_initialize<T: OwnedLspTransport, C: TickClock>(
    limits: SessionLimits,
    clock: &C,
    context: SendContext,
    transport: &mut T,
    service: &DaemonService,
    scope: &SessionScope,
    generation: u64,
) -> Result<(), SessionError> {
    let purpose = match &scope.purpose {
        SessionPurpose::Live => json!({ "kind": "live" }),
        SessionPurpose::Shadow(run_digest) => {
            json!({ "kind": "shadow", "runDigest": run_digest.as_str() })
        }
    };
    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "processId": Value::Null,
            "clientInfo": { "name": "kit", "version": env!("CARGO_PKG_VERSION") },
            "rootUri": Value::Null,
            "capabilities": {
                "general": { "positionEncodings": [scope.position_encoding.lsp_name()] }
            },
            "initializationOptions": {
                "kitService": service.id.to_string(),
                "kitGeneration": generation,
                "canonicalRootIdentity": scope.canonical_root_identity.as_str(),
                "sessionPurpose": purpose,
                "protocolVersion": LSP_PROTOCOL_VERSION
            }
        }
    });
    let frame = LspCodec::encode(&initialize, limits.codec)?;
    let context = context.refreshed(clock);
    if context.remaining().is_zero() {
        return Err(TransportError::WriteDeadlineExceeded.into());
    }
    transport.initialize(&frame, limits.codec, context)?;
    if clock.remaining_until(context.deadline_tick()).is_zero() {
        return Err(TransportError::WriteDeadlineExceeded.into());
    }
    send_value(
        transport,
        limits,
        clock,
        context,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    )
}

fn send_value<T: OwnedLspTransport, C: TickClock>(
    transport: &mut T,
    limits: SessionLimits,
    clock: &C,
    context: SendContext,
    value: &Value,
) -> Result<(), SessionError> {
    let frame = LspCodec::encode(value, limits.codec)?;
    let context = context.refreshed(clock);
    if context.remaining().is_zero() {
        return Err(TransportError::WriteDeadlineExceeded.into());
    }
    transport.send_frame(&frame, context)?;
    if clock.remaining_until(context.deadline_tick()).is_zero() {
        return Err(TransportError::WriteDeadlineExceeded.into());
    }
    Ok(())
}

fn send_session<T: OwnedLspTransport, C: TickClock>(
    session: &mut Session<T>,
    limits: SessionLimits,
    clock: &C,
    context: SendContext,
    process_ids: &mut ProcessIds,
    value: &Value,
) -> Result<(), SessionError> {
    let result = session
        .transport
        .as_mut()
        .ok_or(SessionError::AdmissionClosed)
        .and_then(|transport| send_value(transport, limits, clock, context, value));
    if matches!(result, Err(SessionError::Transport(_))) {
        let reap_context = SendContext::new(
            clock,
            clock
                .now_tick()
                .saturating_add(limits.lifecycle_send_timeout_ticks),
        );
        let (_, close_result) = fence_session(
            session,
            limits.max_tombstones,
            process_ids,
            clock,
            reap_context,
        );
        close_result?;
    }
    result
}

fn send_cancel<T: OwnedLspTransport, C: TickClock>(
    session: &mut Session<T>,
    limits: SessionLimits,
    clock: &C,
    context: SendContext,
    process_ids: &mut ProcessIds,
    request_id: RequestId,
) -> Result<(), SessionError> {
    send_session(
        session,
        limits,
        clock,
        context,
        process_ids,
        &json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": { "id": request_id.get() },
        }),
    )
}

fn did_open(uri: &str, version: DocumentVersion, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": uri,
                "languageId": "",
                "version": version.get(),
                "text": text
            }
        }
    })
}

fn did_change(uri: &str, version: DocumentVersion, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didChange",
        "params": {
            "textDocument": { "uri": uri, "version": version.get() },
            "contentChanges": [{ "text": text }]
        }
    })
}

fn did_close(uri: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didClose",
        "params": { "textDocument": { "uri": uri } }
    })
}

fn discard<T>(session: &mut Session<T>, reason: DiscardReason) -> ResponseDisposition {
    session.counters.discarded = session.counters.discarded.saturating_add(1);
    ResponseDisposition::Discarded(reason)
}

fn discard_for_termination(termination: PendingTermination) -> DiscardReason {
    match termination {
        PendingTermination::ServerRestarted => DiscardReason::ServerRestarted,
        PendingTermination::StaleWorkspaceRevision => DiscardReason::StaleWorkspaceRevision,
        PendingTermination::StaleDocumentVersion => DiscardReason::StaleDocumentVersion,
        PendingTermination::StaleDocumentEpoch => DiscardReason::StaleDocumentEpoch,
        PendingTermination::Cancelled => DiscardReason::Cancelled,
        PendingTermination::DeadlineExceeded => DiscardReason::DeadlineExceeded,
        PendingTermination::Shutdown => DiscardReason::Shutdown,
    }
}

fn termination_for_discard(reason: DiscardReason) -> PendingTermination {
    match reason {
        DiscardReason::StaleGeneration | DiscardReason::ServerRestarted => {
            PendingTermination::ServerRestarted
        }
        DiscardReason::StaleWorkspaceRevision => PendingTermination::StaleWorkspaceRevision,
        DiscardReason::StaleDocumentVersion => PendingTermination::StaleDocumentVersion,
        DiscardReason::StaleDocumentEpoch => PendingTermination::StaleDocumentEpoch,
        DiscardReason::Cancelled => PendingTermination::Cancelled,
        DiscardReason::DeadlineExceeded => PendingTermination::DeadlineExceeded,
        DiscardReason::Shutdown => PendingTermination::Shutdown,
        DiscardReason::WrongRequestId | DiscardReason::TokenMismatch => {
            PendingTermination::StaleWorkspaceRevision
        }
    }
}

fn push_tombstone<T>(
    session: &mut Session<T>,
    token: &PendingToken,
    termination: PendingTermination,
    max_tombstones: usize,
) {
    let key = (token.generation, token.request_id);
    if session.tombstones.insert(key, termination).is_none() {
        session.tombstone_order.push_back(key);
    }
    while session.tombstone_order.len() > max_tombstones {
        if let Some(oldest) = session.tombstone_order.pop_front() {
            session.tombstones.remove(&oldest);
        }
    }
}

fn terminate_matching<T>(
    session: &mut Session<T>,
    predicate: impl Fn(&Pending) -> bool,
    termination: PendingTermination,
    max_tombstones: usize,
) {
    let request_ids = session
        .pending
        .iter()
        .filter_map(|(id, pending)| predicate(pending).then_some(*id))
        .collect::<Vec<_>>();
    for request_id in request_ids {
        if let Some(pending) = session.pending.remove(&request_id) {
            push_tombstone(session, &pending.token, termination, max_tombstones);
        }
    }
}

fn cancel_and_terminate_matching<T: OwnedLspTransport, C: TickClock>(
    session: &mut Session<T>,
    limits: SessionLimits,
    clock: &C,
    context: SendContext,
    process_ids: &mut ProcessIds,
    predicate: impl Fn(&Pending) -> bool,
    termination: PendingTermination,
) -> Result<(), SessionError> {
    let request_ids = session
        .pending
        .iter()
        .filter_map(|(id, pending)| predicate(pending).then_some(*id))
        .collect::<Vec<_>>();
    for request_id in request_ids {
        let Some(pending) = session.pending.remove(&request_id) else {
            continue;
        };
        push_tombstone(session, &pending.token, termination, limits.max_tombstones);
        send_cancel(session, limits, clock, context, process_ids, request_id)?;
    }
    Ok(())
}

fn terminate_all<T>(
    session: &mut Session<T>,
    termination: PendingTermination,
    max_tombstones: usize,
) {
    terminate_matching(session, |_| true, termination, max_tombstones);
}

fn expire_one<T: OwnedLspTransport, C: TickClock>(
    session: &mut Session<T>,
    request_id: RequestId,
    limits: SessionLimits,
    clock: &C,
    context: SendContext,
    process_ids: &mut ProcessIds,
) -> Result<bool, SessionError> {
    if let Some(pending) = session.pending.remove(&request_id) {
        push_tombstone(
            session,
            &pending.token,
            PendingTermination::DeadlineExceeded,
            limits.max_tombstones,
        );
        send_cancel(session, limits, clock, context, process_ids, request_id)?;
        return Ok(true);
    }
    Ok(false)
}

fn fence_session<T: OwnedLspTransport, C: TickClock>(
    session: &mut Session<T>,
    max_tombstones: usize,
    process_ids: &mut ProcessIds,
    clock: &C,
    context: SendContext,
) -> (
    Vec<(PendingToken, PendingTermination)>,
    Result<(), SessionError>,
) {
    session.state = SessionState::Faulted;
    session.generation = session.generation.saturating_add(1);
    let terminated = session
        .pending
        .drain()
        .map(|(_, pending)| (pending.token, PendingTermination::ServerRestarted))
        .collect::<Vec<_>>();
    for (token, termination) in &terminated {
        push_tombstone(session, token, *termination, max_tombstones);
    }
    let close_result = match session.transport.take() {
        Some(mut transport) => match reap_transport(&mut transport, process_ids, clock, context) {
            Ok(()) => Ok(()),
            Err(error) => {
                session.transport = Some(transport);
                Err(error.into())
            }
        },
        None => Ok(()),
    };
    (terminated, close_result)
}

fn close_transport<T: OwnedLspTransport, C: TickClock>(
    session: &mut Session<T>,
    process_ids: &mut ProcessIds,
    clock: &C,
    context: SendContext,
) -> Result<(), SessionError> {
    let Some(mut transport) = session.transport.take() else {
        session.state = SessionState::Closed;
        return Ok(());
    };
    match reap_transport(&mut transport, process_ids, clock, context) {
        Ok(()) => {
            session.state = SessionState::Closed;
            Ok(())
        }
        Err(error) => {
            session.transport = Some(transport);
            Err(error.into())
        }
    }
}

fn reap_transport<T: OwnedLspTransport, C: TickClock>(
    transport: &mut T,
    process_ids: &mut ProcessIds,
    clock: &C,
    context: SendContext,
) -> Result<(), TransportError> {
    let process_id = transport.claim().process_id;
    close_owned_transport(transport, clock, context)?;
    process_ids.reaped(process_id);
    Ok(())
}

fn close_owned_transport<T: OwnedLspTransport, C: TickClock>(
    transport: &mut T,
    clock: &C,
    context: SendContext,
) -> Result<(), TransportError> {
    let context = context.refreshed(clock);
    transport.close_and_reap(context)?;
    if clock.remaining_until(context.deadline_tick()).is_zero() {
        return Err(TransportError::CloseOrReapDeadlineExceeded);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde::ser::{SerializeSeq, Serializer};

    use super::{CodecError, CodecLimits, DocumentVersion, LspCodec};

    #[test]
    fn document_versions_fail_at_the_lsp_integer_ceiling() {
        assert!(DocumentVersion::from_i64(i64::from(i32::MAX) + 1).is_err());
        assert!(DocumentVersion::new(i32::MAX).next().is_err());
    }

    struct Endless;

    impl serde::Serialize for Endless {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut sequence = serializer.serialize_seq(None)?;
            loop {
                sequence.serialize_element(&0_u8)?;
            }
        }
    }

    #[test]
    fn capped_json_writer_stops_unbounded_serialization() {
        let mut writer = super::CappedWriter::new(64);
        assert!(serde_json::to_writer(&mut writer, &Endless).is_err());
        assert!(writer.exceeded);
        assert!(writer.bytes.len() <= 64);

        let limits = CodecLimits {
            max_header_bytes: 64,
            max_body_bytes: 32,
            max_frame_bytes: 96,
        };
        assert_eq!(
            LspCodec::encode(
                &serde_json::json!({"jsonrpc":"2.0","method":"x","params":"x".repeat(64)}),
                limits,
            ),
            Err(CodecError::BodyTooLarge)
        );
    }
}
