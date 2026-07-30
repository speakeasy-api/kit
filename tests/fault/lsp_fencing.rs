use std::{
    collections::{HashSet, VecDeque},
    io::{BufReader, Cursor},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use kit::{
    domain::{
        events::ContentDigest,
        ids::{PrincipalId, ProcessId, ProjectId, WorkspaceId},
        lifecycle::{ProcessClaim, ProcessOwnership},
    },
    executor::profile::{
        Architecture, ExecutorProfile, Platform, ProfileSpec, ResourceLimits, TrustTier,
    },
    verify::lsp::session::{
        CodecError, CodecLimits, DiscardReason, DocumentVersion, ExecutionProfileIdentity,
        LaunchRequest, LspCodec, LspSessionManager, NotificationDisposition, OwnedLspLauncher,
        OwnedLspTransport, PendingTermination, PositionEncoding, ResponseDisposition,
        RevisionPolicy, SendContext, ServerIdentity, SessionError, SessionLimits, SessionPurpose,
        SessionScope, SessionState, TickClock, TransportError,
    },
    workspace::revision::RevisionId,
};
use serde_json::{Value, json};

const MAX_RETAINED_FRAMES: usize = 128;
const URI: &str = "file:///workspace/main.test";

#[derive(Default)]
struct FakeState {
    launches: usize,
    live_transports: usize,
    frames: VecDeque<(u64, Value)>,
    fail_launches: usize,
    fail_close: HashSet<ProcessId>,
    process_ids: VecDeque<ProcessId>,
    fail_writes: usize,
    fail_initializes: usize,
    initialize_entries: usize,
    send_frame_entries: usize,
    complete_at_deadline_writes: usize,
    advance_after_writes: u64,
    send_remaining: VecDeque<Duration>,
    next_received_frames: VecDeque<Result<Vec<u8>, TransportError>>,
    receive_frame_entries: usize,
    advance_after_close: u64,
    fail_methods: HashSet<String>,
    clock: Option<ManualClock>,
}

impl FakeState {
    fn record(&mut self, generation: u64, value: Value) {
        if self.frames.len() == MAX_RETAINED_FRAMES {
            self.frames.pop_front();
        }
        self.frames.push_back((generation, value));
    }

    fn method_count(&self, method: &str) -> usize {
        self.frames
            .iter()
            .filter(|(_, value)| value.get("method").and_then(Value::as_str) == Some(method))
            .count()
    }

    fn record_context(&mut self, context: SendContext) {
        if self.send_remaining.len() == MAX_RETAINED_FRAMES {
            self.send_remaining.pop_front();
        }
        self.send_remaining.push_back(context.remaining());
    }
}

#[derive(Clone, Default)]
struct FakeLauncher(Arc<Mutex<FakeState>>);

struct FakeTransport {
    claim: ProcessClaim,
    generation: u64,
    state: Arc<Mutex<FakeState>>,
    received_frames: VecDeque<Result<Vec<u8>, TransportError>>,
    live: bool,
}

impl OwnedLspLauncher for FakeLauncher {
    type Transport = FakeTransport;

    fn launch(&mut self, request: LaunchRequest<'_>) -> Result<Self::Transport, TransportError> {
        assert_eq!(
            request.ownership,
            ProcessOwnership::DaemonService(request.service.id)
        );
        assert_eq!(request.execution_profile, &request.scope.execution_profile);
        assert!(request.execution_profile.resources().finite());
        let mut state = self.0.lock().unwrap();
        if state.fail_launches > 0 {
            state.fail_launches -= 1;
            return Err(TransportError::LaunchFailed);
        }
        let process_id = state.process_ids.pop_front().map_or_else(
            || ProcessId::generate().map_err(|_| TransportError::LaunchFailed),
            Ok,
        )?;
        let claim = ProcessClaim::new(process_id, request.ownership);
        let received_frames = std::mem::take(&mut state.next_received_frames);
        state.launches += 1;
        state.live_transports += 1;
        drop(state);
        Ok(FakeTransport {
            claim,
            generation: request.generation,
            state: self.0.clone(),
            received_frames,
            live: true,
        })
    }
}

impl OwnedLspTransport for FakeTransport {
    fn claim(&self) -> ProcessClaim {
        self.claim
    }

    fn initialize(
        &mut self,
        request_frame: &[u8],
        codec_limits: CodecLimits,
        context: SendContext,
    ) -> Result<(), TransportError> {
        let mut state = self.state.lock().unwrap();
        state.initialize_entries += 1;
        if context.remaining().is_zero() {
            return Err(TransportError::WriteDeadlineExceeded);
        }
        if state.fail_initializes > 0 {
            state.fail_initializes -= 1;
            return Err(TransportError::WriteFailed);
        }
        let value = LspCodec::decode(request_frame, codec_limits)
            .map_err(|_| TransportError::WriteFailed)?;
        if value.value().get("method").and_then(Value::as_str) != Some("initialize") {
            return Err(TransportError::WriteFailed);
        }
        state.record_context(context);
        state.record(self.generation, value.value().clone());
        Ok(())
    }

    fn send_frame(&mut self, frame: &[u8], context: SendContext) -> Result<(), TransportError> {
        let mut state = self.state.lock().unwrap();
        state.send_frame_entries += 1;
        let value = LspCodec::decode(frame, SessionLimits::default().codec)
            .map_err(|_| TransportError::WriteFailed)?;
        if context.remaining().is_zero() {
            return Err(TransportError::WriteDeadlineExceeded);
        }
        if state.fail_writes > 0 {
            state.fail_writes -= 1;
            return Err(TransportError::WriteFailed);
        }
        if value
            .value()
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|method| state.fail_methods.remove(method))
        {
            return Err(TransportError::WriteFailed);
        }
        if state.complete_at_deadline_writes > 0 {
            state.complete_at_deadline_writes -= 1;
            state.clock.as_ref().unwrap().set(context.deadline_tick());
            return Ok(());
        }
        state.record_context(context);
        state.record(self.generation, value.value().clone());
        if state.advance_after_writes > 0 {
            let clock = state.clock.as_ref().unwrap();
            clock.set(clock.now_tick().saturating_add(state.advance_after_writes));
        }
        Ok(())
    }

    fn receive_frame(
        &mut self,
        limits: CodecLimits,
        context: SendContext,
    ) -> Result<Vec<u8>, TransportError> {
        if context.remaining().is_zero() {
            return Err(TransportError::ReadDeadlineExceeded);
        }
        self.state.lock().unwrap().receive_frame_entries += 1;
        let frame = self
            .received_frames
            .pop_front()
            .unwrap_or(Err(TransportError::ReadFailed))?;
        if frame.len() > limits.max_frame_bytes {
            return Err(TransportError::ReadFailed);
        }
        Ok(frame)
    }

    fn close_and_reap(&mut self, context: SendContext) -> Result<(), TransportError> {
        let mut state = self.state.lock().unwrap();
        if context.remaining().is_zero() {
            return Err(TransportError::CloseOrReapDeadlineExceeded);
        }
        if state.fail_close.remove(&self.claim.process_id) {
            return Err(TransportError::CloseOrReapFailed);
        }
        if self.live {
            self.live = false;
            state.live_transports -= 1;
        }
        if state.advance_after_close > 0 {
            let clock = state.clock.as_ref().unwrap();
            clock.set(clock.now_tick().saturating_add(state.advance_after_close));
        }
        Ok(())
    }
}

impl Drop for FakeTransport {
    fn drop(&mut self) {
        if self.live {
            self.live = false;
            self.state.lock().unwrap().live_transports -= 1;
        }
    }
}

#[derive(Clone, Default)]
struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    fn set(&self, tick: u64) {
        self.0.store(tick, Ordering::SeqCst);
    }
}

impl TickClock for ManualClock {
    fn now_tick(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    fn remaining_until(&self, deadline_tick: u64) -> Duration {
        Duration::from_millis(deadline_tick.saturating_sub(self.now_tick()))
    }
}

#[derive(Clone, Default)]
struct ExpiringClock {
    tick: Arc<AtomicU64>,
    expiring: Arc<AtomicBool>,
}

impl ExpiringClock {
    fn expire(&self) {
        self.expiring.store(true, Ordering::SeqCst);
    }
}

impl TickClock for ExpiringClock {
    fn now_tick(&self) -> u64 {
        self.tick.load(Ordering::SeqCst)
    }

    fn remaining_until(&self, deadline_tick: u64) -> Duration {
        if self.expiring.load(Ordering::SeqCst) {
            self.tick.store(deadline_tick, Ordering::SeqCst);
            Duration::ZERO
        } else {
            Duration::from_millis(deadline_tick.saturating_sub(self.now_tick()))
        }
    }
}

type Manager = LspSessionManager<FakeLauncher, ManualClock>;

fn revision(byte: u8) -> RevisionId {
    RevisionId::parse(&format!(
        "r:{}",
        format_args!("{byte:02x}").to_string().repeat(32)
    ))
    .expect("test revision")
}

fn digest(byte: u8) -> ContentDigest {
    ContentDigest::parse(&format!(
        "blake3:{}",
        format_args!("{byte:02x}").to_string().repeat(32)
    ))
    .unwrap()
}

fn process_id(value: &str) -> ProcessId {
    ProcessId::parse(value).unwrap()
}

fn resources(memory_bytes: u64) -> ResourceLimits {
    ResourceLimits::new(
        3_600_000,
        memory_bytes,
        256,
        64 * 1024 * 1024,
        4 * 1024 * 1024 * 1024,
        16 * 1024 * 1024 * 1024,
        64 * 1024 * 1024,
        3_600_000,
    )
}

fn profile(memory_bytes: u64) -> ExecutionProfileIdentity {
    let profile = ExecutorProfile::new(ProfileSpec::isolated(
        TrustTier::TrustedLocal,
        Platform::MacOs,
        Architecture::Aarch64,
        resources(memory_bytes),
    ))
    .unwrap();
    ExecutionProfileIdentity::from_profile(&profile)
}

fn scope(revision_policy: RevisionPolicy) -> SessionScope {
    SessionScope {
        principal_id: PrincipalId::generate().unwrap(),
        project_id: ProjectId::generate().unwrap(),
        workspace_id: WorkspaceId::generate().unwrap(),
        canonical_root_identity: digest(1),
        purpose: SessionPurpose::Live,
        revision_policy,
        server: ServerIdentity {
            server_artifact: digest(2),
            configuration: digest(3),
        },
        position_encoding: PositionEncoding::Utf16,
        execution_profile: profile(2 * 1024 * 1024 * 1024),
    }
}

fn manager_with_limits(limits: SessionLimits) -> (Manager, Arc<Mutex<FakeState>>, ManualClock) {
    let launcher = FakeLauncher::default();
    let state = launcher.0.clone();
    let clock = ManualClock::default();
    state.lock().unwrap().clock = Some(clock.clone());
    (
        LspSessionManager::with_clock(launcher, limits, clock.clone()).unwrap(),
        state,
        clock,
    )
}

fn manager() -> (Manager, Arc<Mutex<FakeState>>, ManualClock) {
    manager_with_limits(SessionLimits::default())
}

fn response(id: u32) -> Vec<u8> {
    LspCodec::encode(
        &json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}),
        SessionLimits::default().codec,
    )
    .unwrap()
}

fn request_frame(id: u32) -> Vec<u8> {
    LspCodec::encode(
        &json!({"jsonrpc": "2.0", "id": id, "method": "server/request", "params": {}}),
        SessionLimits::default().codec,
    )
    .unwrap()
}

fn open_document(
    manager: &mut Manager,
    session_scope: SessionScope,
    revision: RevisionId,
) -> kit::domain::ids::DaemonServiceId {
    let service = manager.open(session_scope, revision).unwrap();
    manager
        .open_document(
            service,
            URI.to_owned(),
            DocumentVersion::new(1),
            "one".to_owned(),
        )
        .unwrap();
    service
}

fn request(
    manager: &mut Manager,
    service: kit::domain::ids::DaemonServiceId,
    revision: RevisionId,
    deadline: u64,
) -> kit::verify::lsp::session::PendingToken {
    manager
        .request(
            service,
            revision,
            URI,
            "textDocument/hover",
            json!({}),
            deadline,
        )
        .unwrap()
}

#[test]
fn stale_document_versions_are_discarded_100_of_100_then_current_is_accepted() {
    let (mut manager, _, _) = manager();
    let workspace_revision = revision(1);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    for version in 0..100 {
        let token = request(&mut manager, service, workspace_revision, 1_000);
        manager
            .update_document(service, URI, format!("v{version}"))
            .unwrap();
        assert_eq!(
            manager
                .receive_captured_response(service, &token, &response(token.request_id.get()))
                .unwrap(),
            ResponseDisposition::Discarded(DiscardReason::StaleDocumentEpoch)
        );
    }
    let current = request(&mut manager, service, workspace_revision, 1_000);
    assert!(matches!(
        manager
            .receive_captured_response(service, &current, &response(current.request_id.get()))
            .unwrap(),
        ResponseDisposition::Accepted(_)
    ));
    let snapshot = manager.snapshot(service).unwrap();
    assert_eq!(snapshot.counters.discarded, 100);
    assert_eq!(snapshot.counters.accepted, 1);
    manager.shutdown().unwrap();
}

#[test]
fn semantic_request_position_is_validated_and_fenced_in_the_pending_token() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(36);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    let token = manager
        .request(
            service,
            workspace_revision,
            URI,
            "textDocument/definition",
            json!({
                "textDocument": {"uri": URI},
                "position": {"line": 0, "character": 1}
            }),
            100,
        )
        .unwrap();
    let position = token.request_position.unwrap();
    assert_eq!((position.line(), position.character()), (0, 1));

    let sent = state
        .lock()
        .unwrap()
        .method_count("textDocument/definition");
    for params in [
        json!({}),
        json!({
            "textDocument": {"uri": "file:///workspace/other.test"},
            "position": {"line": 0, "character": 1}
        }),
        json!({
            "textDocument": {"uri": URI},
            "position": {"line": -1, "character": 1}
        }),
        json!({
            "textDocument": {"uri": URI},
            "position": {"line": 0, "character": 4}
        }),
    ] {
        assert_eq!(
            manager.request(
                service,
                workspace_revision,
                URI,
                "textDocument/definition",
                params,
                100,
            ),
            Err(SessionError::InvalidRequestPosition)
        );
    }
    assert_eq!(
        state
            .lock()
            .unwrap()
            .method_count("textDocument/definition"),
        sent
    );
    manager.shutdown().unwrap();
}

#[test]
fn old_generation_and_wrong_id_cannot_complete_a_current_request() {
    let (mut manager, _, _) = manager();
    let workspace_revision = revision(2);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    let old_generation = manager.snapshot(service).unwrap().generation;
    manager.server_crashed(service).unwrap();
    let current = request(&mut manager, service, workspace_revision, 100);
    assert_eq!(
        manager
            .receive_response(service, old_generation, b"malformed stale bytes")
            .unwrap(),
        ResponseDisposition::Discarded(DiscardReason::StaleGeneration)
    );
    assert_eq!(
        manager
            .receive_response(
                service,
                current.generation,
                &response(current.request_id.get() + 1),
            )
            .unwrap(),
        ResponseDisposition::Discarded(DiscardReason::WrongRequestId)
    );
    assert_eq!(manager.snapshot(service).unwrap().pending_requests, 1);
    assert!(matches!(
        manager
            .receive_captured_response(service, &current, &response(current.request_id.get()))
            .unwrap(),
        ResponseDisposition::Accepted(_)
    ));
    manager.shutdown().unwrap();
}

#[test]
fn crash_retains_owner_replays_documents_and_accepts_fresh_work() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(3);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    let pending = request(&mut manager, service, workspace_revision, 100);
    let before_inventory = manager.ownership_inventory();
    let before = manager.snapshot(service).unwrap();
    let terminated = manager.server_crashed(service).unwrap();
    let after = manager.snapshot(service).unwrap();
    let after_inventory = manager.ownership_inventory();
    assert_eq!(before_inventory[0].service, after_inventory[0].service);
    assert_eq!(before_inventory[0].scope, after_inventory[0].scope);
    assert_eq!(
        terminated,
        vec![(pending, PendingTermination::ServerRestarted)]
    );
    assert!(after.generation > before.generation);
    assert_ne!(after.process_id, before.process_id);
    assert!(
        state
            .lock()
            .unwrap()
            .frames
            .iter()
            .any(|(generation, value)| {
                *generation == after.generation
                    && value.get("method").and_then(Value::as_str) == Some("textDocument/didOpen")
                    && value
                        .pointer("/params/textDocument/text")
                        .and_then(Value::as_str)
                        == Some("one")
            })
    );
    let fresh = request(&mut manager, service, workspace_revision, 100);
    assert!(matches!(
        manager
            .receive_captured_response(service, &fresh, &response(fresh.request_id.get()))
            .unwrap(),
        ResponseDisposition::Accepted(_)
    ));
    manager.shutdown().unwrap();
}

#[test]
fn strict_codec_rejects_malformed_duplicate_oversized_and_invalid_json() {
    let limits = SessionLimits::default().codec;
    assert_eq!(
        LspCodec::decode(b"X: 1\r\n\r\n{}", limits),
        Err(CodecError::InvalidHeader)
    );
    assert_eq!(
        LspCodec::decode(b"Content-Length: no\r\n\r\n{}", limits),
        Err(CodecError::InvalidContentLength)
    );
    assert_eq!(
        LspCodec::decode(b"Content-Length: 2\r\nContent-Length: 3\r\n\r\n{}", limits),
        Err(CodecError::DuplicateContentLength)
    );
    let oversized_header = vec![b'x'; limits.max_header_bytes + 1];
    assert_eq!(
        LspCodec::decode(&oversized_header, limits),
        Err(CodecError::HeaderTooLarge)
    );
    let oversized_body = format!("Content-Length: {}\r\n\r\n", limits.max_body_bytes + 1);
    assert_eq!(
        LspCodec::decode(oversized_body.as_bytes(), limits),
        Err(CodecError::BodyTooLarge)
    );
    assert_eq!(
        LspCodec::decode(b"Content-Length: 1\r\n\r\n{", limits),
        Err(CodecError::MalformedJson)
    );
    let body = br#"{"jsonrpc":"2.0","result":1}"#;
    let mut invalid = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    invalid.extend_from_slice(body);
    assert_eq!(
        LspCodec::decode(&invalid, limits),
        Err(CodecError::InvalidEnvelope)
    );
    let out_of_range = br#"{"jsonrpc":"2.0","id":2147483648,"result":null}"#;
    let mut invalid = format!("Content-Length: {}\r\n\r\n", out_of_range.len()).into_bytes();
    invalid.extend_from_slice(out_of_range);
    assert_eq!(
        LspCodec::decode(&invalid, limits),
        Err(CodecError::InvalidEnvelope)
    );
}

#[test]
fn lsp_json_preflight_caps_body_nesting_and_collection_items_before_serde() {
    fn frame(body: &[u8]) -> Vec<u8> {
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(body);
        frame
    }

    let nested = format!(
        "{{\"jsonrpc\":\"2.0\",\"method\":\"x\",\"params\":{}{}}}",
        "[".repeat(129),
        "]".repeat(129)
    );
    assert_eq!(
        LspCodec::decode(&frame(nested.as_bytes()), SessionLimits::default().codec),
        Err(CodecError::BodyTooLarge)
    );

    let items = format!(
        "{{\"jsonrpc\":\"2.0\",\"method\":\"x\",\"params\":[{}]}}",
        vec!["0"; 100_001].join(",")
    );
    assert!(items.len() < 4 * 1024 * 1024);
    assert_eq!(
        LspCodec::decode(&frame(items.as_bytes()), SessionLimits::default().codec),
        Err(CodecError::BodyTooLarge)
    );

    let oversized_uri = format!(
        "{{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{{\"uri\":\"file:///{}\",\"version\":1,\"diagnostics\":[]}}}}",
        "x".repeat(16 * 1024)
    );
    assert_eq!(
        LspCodec::decode(
            &frame(oversized_uri.as_bytes()),
            SessionLimits::default().codec
        ),
        Err(CodecError::JsonTokenTooLarge)
    );

    let permissive = CodecLimits {
        max_header_bytes: 8 * 1024,
        max_body_bytes: 64 * 1024 * 1024,
        max_frame_bytes: 64 * 1024 * 1024 + 8 * 1024,
    };
    let oversized = format!("Content-Length: {}\r\n\r\n", 5 * 1024 * 1024);
    assert_eq!(
        LspCodec::decode(oversized.as_bytes(), permissive),
        Err(CodecError::BodyTooLarge)
    );
}

#[test]
fn lsp_json_preflight_rejects_escaped_keys_and_oversized_fact_tokens() {
    fn decode(body: &str) -> Result<kit::verify::lsp::session::DecodedFrame, CodecError> {
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(body.as_bytes());
        LspCodec::decode(&frame, SessionLimits::default().codec)
    }

    assert_eq!(
        decode(r#"{"jsonrpc":"2.0","method":"x","params":{"\u0075ri":"file:///safe"}}"#),
        Err(CodecError::EscapedObjectKey)
    );

    let oversized_key = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"changes":{{"{}":[]}}}}}}"#,
        "u".repeat(16 * 1024 + 1)
    );
    assert_eq!(decode(&oversized_key), Err(CodecError::JsonTokenTooLarge));

    let oversized_annotation = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{{"changeAnnotations":{{"{}":{{"label":"safe"}}}}}}}}"#,
        "a".repeat(1_025)
    );
    assert_eq!(
        decode(&oversized_annotation),
        Err(CodecError::JsonTokenTooLarge)
    );

    for (field, size) in [
        ("message", 64 * 1024 + 1),
        ("source", 1_025),
        ("code", 1_025),
    ] {
        let body = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{{"uri":"file:///safe","version":1,"diagnostics":[{{"range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":0}}}},"{field}":"{}","message":"ok"}}]}}}}"#,
            "x".repeat(size)
        );
        assert_eq!(
            decode(&body),
            Err(CodecError::JsonTokenTooLarge),
            "preflight accepted oversized {field}"
        );
    }
}

#[test]
fn open_document_accounting_rejects_amplification_without_mutation() {
    let limits = SessionLimits {
        max_document_bytes: 64,
        max_total_document_bytes: 256,
        ..SessionLimits::default()
    };
    let (mut manager, state, _) = manager_with_limits(limits);
    let workspace_revision = revision(32);
    let service = manager
        .open(scope(RevisionPolicy::ManagedLive), workspace_revision)
        .unwrap();
    manager
        .open_document(
            service,
            URI.to_owned(),
            DocumentVersion::new(1),
            "one".to_owned(),
        )
        .unwrap();
    let before = manager.snapshot(service).unwrap();
    let did_open = state.lock().unwrap().method_count("textDocument/didOpen");

    assert_eq!(
        manager.open_document(
            service,
            "file:///workspace/tiny.test".to_owned(),
            DocumentVersion::new(1),
            "x".to_owned(),
        ),
        Err(SessionError::DocumentCapacityExceeded)
    );
    assert_eq!(manager.snapshot(service).unwrap(), before);
    assert_eq!(
        state.lock().unwrap().method_count("textDocument/didOpen"),
        did_open
    );

    let did_change = state.lock().unwrap().method_count("textDocument/didChange");
    assert_eq!(
        manager.update_document(service, URI, "\n".repeat(32)),
        Err(SessionError::DocumentCapacityExceeded)
    );
    assert_eq!(manager.snapshot(service).unwrap(), before);
    assert_eq!(
        state.lock().unwrap().method_count("textDocument/didChange"),
        did_change
    );
    manager.shutdown().unwrap();
}

#[test]
fn cancellation_and_deadline_fence_late_responses_without_poisoning_session() {
    let (mut manager, state, clock) = manager();
    let workspace_revision = revision(4);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    let cancelled = request(&mut manager, service, workspace_revision, 10);
    manager
        .cancel_request(service, cancelled.request_id)
        .unwrap();
    assert_eq!(
        manager
            .receive_captured_response(service, &cancelled, &response(cancelled.request_id.get()))
            .unwrap(),
        ResponseDisposition::Discarded(DiscardReason::Cancelled)
    );
    let expired = request(&mut manager, service, workspace_revision, 20);
    clock.set(20);
    assert_eq!(
        manager
            .receive_captured_response(service, &expired, &response(expired.request_id.get()))
            .unwrap(),
        ResponseDisposition::Discarded(DiscardReason::DeadlineExceeded)
    );
    assert_eq!(manager.expire_deadlines(), Ok(0));
    assert_eq!(state.lock().unwrap().method_count("$/cancelRequest"), 1);
    let sent_requests = state.lock().unwrap().method_count("textDocument/hover");
    assert_eq!(
        manager.request(
            service,
            workspace_revision,
            URI,
            "textDocument/hover",
            json!({}),
            20,
        ),
        Err(SessionError::DeadlineExceeded)
    );
    assert_eq!(
        state.lock().unwrap().method_count("textDocument/hover"),
        sent_requests
    );
    let later = request(&mut manager, service, workspace_revision, 30);
    assert!(matches!(
        manager
            .receive_captured_response(service, &later, &response(later.request_id.get()))
            .unwrap(),
        ResponseDisposition::Accepted(_)
    ));
    manager.shutdown().unwrap();
}

#[test]
fn shutdown_closes_admission_and_reaps_every_fake_transport() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(5);
    let original_scope = scope(RevisionPolicy::ManagedLive);
    let service = open_document(&mut manager, original_scope.clone(), workspace_revision);
    request(&mut manager, service, workspace_revision, 10);
    manager.shutdown().unwrap();
    assert_eq!(manager.usage(), Default::default());
    assert_eq!(state.lock().unwrap().live_transports, 0);
    assert_eq!(
        manager.open(original_scope, workspace_revision),
        Err(SessionError::AdmissionClosed)
    );
}

#[test]
fn pool_reuses_exact_scope_and_isolates_every_scope_dimension() {
    let (mut manager, _, _) = manager();
    let workspace_revision = revision(6);
    let base = scope(RevisionPolicy::ManagedLive);
    let first = manager.open(base.clone(), workspace_revision).unwrap();
    assert_eq!(
        manager.open(base.clone(), workspace_revision).unwrap(),
        first
    );

    let mut variants = Vec::new();
    let mut changed = base.clone();
    changed.principal_id = PrincipalId::generate().unwrap();
    variants.push(changed);
    let mut changed = base.clone();
    changed.project_id = ProjectId::generate().unwrap();
    variants.push(changed);
    let mut changed = base.clone();
    changed.workspace_id = WorkspaceId::generate().unwrap();
    variants.push(changed);
    let mut changed = base.clone();
    changed.canonical_root_identity = digest(4);
    variants.push(changed);
    let mut changed = base.clone();
    changed.revision_policy = RevisionPolicy::Pinned(workspace_revision);
    variants.push(changed);
    let mut changed = base.clone();
    changed.server.server_artifact = digest(5);
    variants.push(changed);
    let mut changed = base.clone();
    changed.server.configuration = digest(6);
    variants.push(changed);
    let mut changed = base.clone();
    changed.position_encoding = PositionEncoding::Utf8;
    variants.push(changed);
    let mut changed = base;
    changed.execution_profile = profile(1024 * 1024 * 1024);
    variants.push(changed);

    for variant in variants {
        assert_ne!(manager.open(variant, workspace_revision).unwrap(), first);
    }
    assert_eq!(manager.usage().sessions, 10);
    manager.shutdown().unwrap();
}

#[test]
fn server_request_with_colliding_id_cannot_consume_pending_client_request() {
    let (mut manager, _, _) = manager();
    let workspace_revision = revision(7);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    let pending = request(&mut manager, service, workspace_revision, 100);
    assert_eq!(
        manager.receive_response(
            service,
            pending.generation,
            &request_frame(pending.request_id.get()),
        ),
        Err(SessionError::Codec(CodecError::InvalidEnvelope))
    );
    assert_eq!(manager.snapshot(service).unwrap().pending_requests, 1);
    manager.shutdown().unwrap();
}

#[test]
fn shutdown_records_first_failure_but_closes_other_sessions() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(8);
    let first = manager
        .open(scope(RevisionPolicy::ManagedLive), workspace_revision)
        .unwrap();
    let second = manager
        .open(scope(RevisionPolicy::ManagedLive), workspace_revision)
        .unwrap();
    let failed_process = manager.snapshot(first).unwrap().process_id.unwrap();
    state.lock().unwrap().fail_close.insert(failed_process);
    assert_eq!(
        manager.shutdown(),
        Err(SessionError::ShutdownFailed {
            failed_sessions: 1,
            first: TransportError::CloseOrReapFailed,
        })
    );
    assert!(manager.snapshot(first).is_ok());
    assert_eq!(manager.snapshot(second), Err(SessionError::SessionNotFound));
    assert_eq!(state.lock().unwrap().live_transports, 1);
    manager.shutdown().unwrap();
    assert_eq!(state.lock().unwrap().live_transports, 0);
}

#[test]
fn restart_launch_failure_leaves_faulted_session_retryable_without_reaping_old_twice() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(9);
    let session_scope = scope(RevisionPolicy::ManagedLive);
    let service = open_document(&mut manager, session_scope.clone(), workspace_revision);
    let owner = manager.ownership_inventory()[0].service.clone();
    state.lock().unwrap().fail_launches = 1;
    assert_eq!(
        manager.server_crashed(service),
        Err(SessionError::Transport(TransportError::LaunchFailed))
    );
    let faulted = manager.snapshot(service).unwrap();
    assert_eq!(faulted.process_id, None);
    assert_eq!(faulted.counters.restarts, 1);
    assert_eq!(state.lock().unwrap().live_transports, 0);
    assert_eq!(
        manager.open(session_scope, workspace_revision).unwrap(),
        service
    );
    let recovered = manager.snapshot(service).unwrap();
    assert!(recovered.process_id.is_some());
    assert_eq!(recovered.counters.restarts, 2);
    assert_eq!(manager.ownership_inventory()[0].service, owner);
    manager.shutdown().unwrap();
}

#[test]
fn managed_live_exact_scope_advances_revision_and_pinned_scope_rejects_it() {
    let (mut manager, _, _) = manager();
    let old_revision = revision(10);
    let new_revision = revision(11);
    let live_scope = scope(RevisionPolicy::ManagedLive);
    let live = open_document(&mut manager, live_scope.clone(), old_revision);
    let stale = request(&mut manager, live, old_revision, 100);
    assert_eq!(manager.open(live_scope, new_revision).unwrap(), live);
    assert_eq!(manager.snapshot(live).unwrap().pending_requests, 0);
    let current = request(&mut manager, live, new_revision, 100);
    assert_eq!(
        manager
            .receive_captured_response(live, &stale, &response(stale.request_id.get()))
            .unwrap(),
        ResponseDisposition::Discarded(DiscardReason::StaleWorkspaceRevision)
    );
    assert!(matches!(
        manager
            .receive_captured_response(live, &current, &response(current.request_id.get()))
            .unwrap(),
        ResponseDisposition::Accepted(_)
    ));

    let pinned_scope = scope(RevisionPolicy::Pinned(old_revision));
    let pinned = manager.open(pinned_scope, old_revision).unwrap();
    assert_eq!(
        manager.set_workspace_revision(pinned, new_revision),
        Err(SessionError::RevisionMismatch)
    );
    manager.shutdown().unwrap();
}

#[test]
fn updating_document_drains_withheld_pending_and_immediately_frees_capacity() {
    let limits = SessionLimits {
        max_pending_requests: 1,
        ..SessionLimits::default()
    };
    let (mut manager, _, _) = manager_with_limits(limits);
    let workspace_revision = revision(12);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    let stale = request(&mut manager, service, workspace_revision, 100);
    manager
        .update_document(service, URI, "updated".to_owned())
        .unwrap();
    let current = request(&mut manager, service, workspace_revision, 100);
    assert_eq!(manager.snapshot(service).unwrap().pending_requests, 1);
    assert_eq!(
        manager
            .receive_captured_response(service, &stale, &response(stale.request_id.get()))
            .unwrap(),
        ResponseDisposition::Discarded(DiscardReason::StaleDocumentEpoch)
    );
    assert!(matches!(
        manager
            .receive_captured_response(service, &current, &response(current.request_id.get()))
            .unwrap(),
        ResponseDisposition::Accepted(_)
    ));
    manager.shutdown().unwrap();
}

#[test]
fn streaming_codec_rejects_limits_before_body_read_or_large_allocation() {
    let limits = CodecLimits {
        max_header_bytes: 64,
        max_body_bytes: 32,
        max_frame_bytes: 96,
    };
    let oversized = format!("Content-Length: {}\r\n\r\n", usize::MAX);
    let mut reader = BufReader::new(Cursor::new(oversized.as_bytes()));
    assert_eq!(
        LspCodec::decode_from(&mut reader, limits),
        Err(CodecError::BodyTooLarge)
    );

    let header = vec![b'x'; 1_024];
    let mut reader = BufReader::new(Cursor::new(header));
    assert_eq!(
        LspCodec::decode_from(&mut reader, limits),
        Err(CodecError::HeaderTooLarge)
    );

    let encoded = LspCodec::encode(
        &json!({"jsonrpc":"2.0","method":"x","params":"x".repeat(64)}),
        limits,
    );
    assert_eq!(encoded, Err(CodecError::BodyTooLarge));
}

#[test]
fn close_document_and_idle_session_eviction_release_hard_bounds() {
    let limits = SessionLimits {
        max_sessions: 1,
        ..SessionLimits::default()
    };
    let (mut manager, state, _) = manager_with_limits(limits);
    let workspace_revision = revision(13);
    let first_scope = scope(RevisionPolicy::ManagedLive);
    let first = open_document(&mut manager, first_scope, workspace_revision);
    assert_eq!(
        manager.open(scope(RevisionPolicy::ManagedLive), workspace_revision),
        Err(SessionError::CapacityExceeded)
    );
    manager.close_document(first, URI).unwrap();
    assert_eq!(
        state.lock().unwrap().method_count("textDocument/didClose"),
        1
    );
    let second = manager
        .open(scope(RevisionPolicy::ManagedLive), workspace_revision)
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(manager.usage().sessions, 1);
    manager
        .open_document(
            second,
            URI.to_owned(),
            DocumentVersion::new(1),
            "two".to_owned(),
        )
        .unwrap();
    manager.close_session(second).unwrap();
    assert_eq!(manager.usage(), Default::default());
    assert_eq!(state.lock().unwrap().live_transports, 0);
    assert_eq!(
        state.lock().unwrap().method_count("textDocument/didClose"),
        2
    );
}

#[test]
fn tombstones_have_bounded_fifo_eviction_and_exact_generation_lookup() {
    let limits = SessionLimits {
        max_tombstones: 2,
        ..SessionLimits::default()
    };
    let (mut manager, _, _) = manager_with_limits(limits);
    let workspace_revision = revision(14);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    let first = request(&mut manager, service, workspace_revision, 100);
    manager.cancel_request(service, first.request_id).unwrap();
    let second = request(&mut manager, service, workspace_revision, 100);
    manager.cancel_request(service, second.request_id).unwrap();
    let third = request(&mut manager, service, workspace_revision, 100);
    manager.cancel_request(service, third.request_id).unwrap();
    assert_eq!(manager.snapshot(service).unwrap().tombstones, 2);
    assert_eq!(
        manager
            .receive_response(service, first.generation, &response(first.request_id.get()))
            .unwrap(),
        ResponseDisposition::Discarded(DiscardReason::WrongRequestId)
    );
    assert_eq!(
        manager
            .receive_response(service, third.generation, &response(third.request_id.get()))
            .unwrap(),
        ResponseDisposition::Discarded(DiscardReason::Cancelled)
    );
    manager.shutdown().unwrap();
}

#[test]
fn fake_transport_history_is_bounded_and_consumable() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(15);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    for version in 0..MAX_RETAINED_FRAMES * 2 {
        manager
            .update_document(service, URI, format!("version {version}"))
            .unwrap();
    }
    assert_eq!(state.lock().unwrap().frames.len(), MAX_RETAINED_FRAMES);
    manager.shutdown().unwrap();
}

#[test]
fn failed_request_write_fences_generation_and_all_existing_pending() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(16);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    let existing = request(&mut manager, service, workspace_revision, 100);
    let generation = existing.generation;
    state.lock().unwrap().fail_writes = 1;

    assert_eq!(
        manager.request(
            service,
            workspace_revision,
            URI,
            "textDocument/hover",
            json!({}),
            100,
        ),
        Err(SessionError::Transport(TransportError::WriteFailed))
    );
    let snapshot = manager.snapshot(service).unwrap();
    assert_eq!(snapshot.state, SessionState::Faulted);
    assert!(snapshot.generation > generation);
    assert_eq!(snapshot.pending_requests, 0);
    assert_eq!(snapshot.process_id, None);
    assert_eq!(state.lock().unwrap().live_transports, 0);
    assert_eq!(
        manager
            .receive_captured_response(service, &existing, &response(existing.request_id.get()))
            .unwrap(),
        ResponseDisposition::Discarded(DiscardReason::StaleGeneration)
    );
}

#[test]
fn write_completing_at_deadline_is_authoritatively_fenced() {
    let (mut manager, state, clock) = manager();
    let workspace_revision = revision(17);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    state.lock().unwrap().complete_at_deadline_writes = 1;

    assert_eq!(
        manager.request(
            service,
            workspace_revision,
            URI,
            "textDocument/hover",
            json!({}),
            manager.now_tick() + 10,
        ),
        Err(SessionError::Transport(
            TransportError::WriteDeadlineExceeded
        ))
    );
    assert_eq!(clock.now_tick(), 10);
    let snapshot = manager.snapshot(service).unwrap();
    assert_eq!(snapshot.state, SessionState::Faulted);
    assert_eq!(snapshot.pending_requests, 0);
    assert_eq!(snapshot.process_id, None);
}

#[test]
fn send_context_budget_shrinks_and_zero_budget_never_reaches_transport() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(25);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    manager
        .open_document(
            service,
            "file:///workspace/other.test".to_owned(),
            DocumentVersion::new(1),
            "two".to_owned(),
        )
        .unwrap();
    {
        let mut state = state.lock().unwrap();
        state.send_remaining.clear();
        state.advance_after_writes = 10;
    }
    manager.close_session(service).unwrap();
    assert_eq!(
        state
            .lock()
            .unwrap()
            .send_remaining
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        [Duration::from_millis(5_000), Duration::from_millis(4_990)]
    );

    let launcher = FakeLauncher::default();
    let state = launcher.0.clone();
    let clock = ExpiringClock::default();
    clock.expire();
    let mut manager =
        LspSessionManager::with_clock(launcher, SessionLimits::default(), clock).unwrap();
    assert_eq!(
        manager.open(scope(RevisionPolicy::ManagedLive), revision(26)),
        Err(SessionError::Transport(
            TransportError::CloseOrReapDeadlineExceeded
        ))
    );
    let state = state.lock().unwrap();
    assert!(state.frames.is_empty());
    assert!(state.send_remaining.is_empty());
    assert_eq!(state.initialize_entries, 0);
    assert_eq!(state.send_frame_entries, 0);
    assert_eq!(state.live_transports, 0);

    let launcher = FakeLauncher::default();
    let state = launcher.0.clone();
    let clock = ExpiringClock::default();
    let mut manager =
        LspSessionManager::with_clock(launcher, SessionLimits::default(), clock.clone()).unwrap();
    let service = manager
        .open(scope(RevisionPolicy::ManagedLive), revision(29))
        .unwrap();
    let send_frame_entries = state.lock().unwrap().send_frame_entries;
    assert_eq!(send_frame_entries, 1);
    clock.expire();
    assert_eq!(
        manager.open_document(
            service,
            URI.to_owned(),
            DocumentVersion::new(1),
            "one".to_owned(),
        ),
        Err(SessionError::Transport(
            TransportError::CloseOrReapDeadlineExceeded
        ))
    );
    let state = state.lock().unwrap();
    assert_eq!(state.initialize_entries, 1);
    assert_eq!(state.send_frame_entries, send_frame_entries);
}

#[test]
fn manager_rejects_reap_that_returns_at_the_deadline() {
    let (mut manager, state, _) = manager();
    let service = manager
        .open(scope(RevisionPolicy::ManagedLive), revision(35))
        .unwrap();
    state.lock().unwrap().advance_after_close = 5_000;
    assert_eq!(
        manager.close_session(service),
        Err(SessionError::Transport(
            TransportError::CloseOrReapDeadlineExceeded
        ))
    );
    assert!(manager.snapshot(service).is_ok());
}

#[test]
fn exhausted_restart_budget_still_terminally_fences_dead_generation() {
    let limits = SessionLimits {
        max_restarts: 1,
        ..SessionLimits::default()
    };
    let (mut manager, state, _) = manager_with_limits(limits);
    let workspace_revision = revision(18);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    manager.server_crashed(service).unwrap();
    let late = request(&mut manager, service, workspace_revision, 100);

    assert_eq!(
        manager.server_crashed(service),
        Err(SessionError::RestartLimitExceeded)
    );
    let snapshot = manager.snapshot(service).unwrap();
    assert_eq!(snapshot.state, SessionState::Faulted);
    assert_eq!(snapshot.process_id, None);
    assert_eq!(snapshot.pending_requests, 0);
    assert_eq!(state.lock().unwrap().live_transports, 0);
    assert_eq!(
        manager
            .receive_captured_response(service, &late, &response(late.request_id.get()))
            .unwrap(),
        ResponseDisposition::Discarded(DiscardReason::StaleGeneration)
    );
}

#[test]
fn process_ids_reject_concurrent_and_recent_generation_reuse() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(19);
    let a = process_id("process_00000000000000000000000001");
    let b = process_id("process_00000000000000000000000002");
    state.lock().unwrap().process_ids.extend([a, a]);
    let first = manager
        .open(scope(RevisionPolicy::ManagedLive), workspace_revision)
        .unwrap();
    assert_eq!(
        manager.open(scope(RevisionPolicy::ManagedLive), workspace_revision),
        Err(SessionError::ProcessIdentityReused)
    );
    assert_eq!(manager.snapshot(first).unwrap().process_id, Some(a));

    state.lock().unwrap().process_ids.extend([b, a]);
    manager.server_crashed(first).unwrap();
    assert_eq!(manager.snapshot(first).unwrap().process_id, Some(b));
    assert_eq!(
        manager.server_crashed(first),
        Err(SessionError::ProcessIdentityReused)
    );
    let snapshot = manager.snapshot(first).unwrap();
    assert_eq!(snapshot.state, SessionState::Faulted);
    assert_eq!(snapshot.process_id, None);
    assert_eq!(state.lock().unwrap().live_transports, 0);
}

#[test]
fn process_id_history_is_bounded_and_rejects_recent_a_b_a_replay() {
    let invalid = SessionLimits {
        max_recent_reaped_process_ids: 0,
        ..SessionLimits::default()
    };
    assert!(matches!(
        LspSessionManager::with_clock(FakeLauncher::default(), invalid, ManualClock::default()),
        Err(SessionError::InvalidLimits)
    ));

    let limits = SessionLimits {
        max_sessions: 1,
        max_recent_reaped_process_ids: 2,
        ..SessionLimits::default()
    };
    let (mut manager, state, _) = manager_with_limits(limits);
    let workspace_revision = revision(27);
    let a = process_id("process_00000000000000000000000003");
    let b = process_id("process_00000000000000000000000004");
    state.lock().unwrap().process_ids.extend([a, b, a]);

    for _ in 0..2 {
        let service = manager
            .open(scope(RevisionPolicy::ManagedLive), workspace_revision)
            .unwrap();
        assert!(manager.retained_process_id_count() <= 3);
        manager.close_session(service).unwrap();
        assert!(manager.retained_process_id_count() <= 2);
    }
    assert_eq!(
        manager.open(scope(RevisionPolicy::ManagedLive), workspace_revision),
        Err(SessionError::ProcessIdentityReused)
    );

    for _ in 0..100 {
        let service = manager
            .open(scope(RevisionPolicy::ManagedLive), workspace_revision)
            .unwrap();
        assert!(manager.retained_process_id_count() <= 3);
        manager.close_session(service).unwrap();
        assert!(manager.retained_process_id_count() <= 2);
    }
}

#[test]
fn failed_initialization_and_reap_keep_process_identity_accounting_safe() {
    let limits = SessionLimits {
        max_sessions: 1,
        max_recent_reaped_process_ids: 2,
        ..SessionLimits::default()
    };
    let workspace_revision = revision(28);
    let a = process_id("process_00000000000000000000000005");
    let b = process_id("process_00000000000000000000000006");

    let (mut manager, state, _) = manager_with_limits(limits);
    {
        let mut state = state.lock().unwrap();
        state.process_ids.extend([a, a]);
        state.fail_initializes = 1;
    }
    assert_eq!(
        manager.open(scope(RevisionPolicy::ManagedLive), workspace_revision),
        Err(SessionError::Transport(TransportError::WriteFailed))
    );
    assert_eq!(manager.retained_process_id_count(), 1);
    assert_eq!(
        manager.open(scope(RevisionPolicy::ManagedLive), workspace_revision),
        Err(SessionError::ProcessIdentityReused)
    );

    let (mut manager, state, _) = manager_with_limits(limits);
    {
        let mut state = state.lock().unwrap();
        state.process_ids.extend([b, a]);
        state.fail_initializes = 1;
        state.fail_close.insert(b);
    }
    assert_eq!(
        manager.open(scope(RevisionPolicy::ManagedLive), workspace_revision),
        Err(SessionError::Transport(TransportError::CloseOrReapFailed))
    );
    assert_eq!(manager.retained_process_id_count(), 1);
    assert_eq!(
        manager.open(scope(RevisionPolicy::ManagedLive), workspace_revision),
        Err(SessionError::CapacityExceeded)
    );
    assert_eq!(state.lock().unwrap().launches, 1);
}

#[test]
fn close_session_releases_memory_after_did_close_write_failure() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(20);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    state
        .lock()
        .unwrap()
        .fail_methods
        .insert("textDocument/didClose".to_owned());

    assert_eq!(
        manager.close_session(service),
        Err(SessionError::Transport(TransportError::WriteFailed))
    );
    assert_eq!(
        manager.snapshot(service),
        Err(SessionError::SessionNotFound)
    );
    assert_eq!(manager.usage(), Default::default());
    assert_eq!(state.lock().unwrap().live_transports, 0);
}

#[test]
fn cancellation_write_failure_is_returned_after_expiration_and_fences_session() {
    let (mut manager, state, clock) = manager();
    let workspace_revision = revision(21);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    request(&mut manager, service, workspace_revision, 10);
    state
        .lock()
        .unwrap()
        .fail_methods
        .insert("$/cancelRequest".to_owned());
    clock.set(10);

    assert_eq!(
        manager.expire_deadlines(),
        Err(SessionError::Transport(TransportError::WriteFailed))
    );
    let snapshot = manager.snapshot(service).unwrap();
    assert_eq!(snapshot.state, SessionState::Faulted);
    assert_eq!(snapshot.pending_requests, 0);
    assert_eq!(snapshot.process_id, None);
}

#[test]
fn document_revision_and_close_invalidation_cancel_each_terminated_request() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(22);
    let next_revision = revision(23);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    request(&mut manager, service, workspace_revision, 100);
    request(&mut manager, service, workspace_revision, 100);

    manager
        .update_document(service, URI, "updated".to_owned())
        .unwrap();
    assert_eq!(state.lock().unwrap().method_count("$/cancelRequest"), 2);
    request(&mut manager, service, workspace_revision, 100);
    manager
        .set_workspace_revision(service, next_revision)
        .unwrap();
    assert_eq!(state.lock().unwrap().method_count("$/cancelRequest"), 3);
    request(&mut manager, service, next_revision, 100);
    manager.close_document(service, URI).unwrap();
    assert_eq!(state.lock().unwrap().method_count("$/cancelRequest"), 4);
    assert_eq!(manager.snapshot(service).unwrap().pending_requests, 0);
    manager.shutdown().unwrap();
}

#[test]
fn invalidation_cancel_failure_fences_remaining_pending_requests() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(24);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    request(&mut manager, service, workspace_revision, 100);
    request(&mut manager, service, workspace_revision, 100);
    state
        .lock()
        .unwrap()
        .fail_methods
        .insert("$/cancelRequest".to_owned());

    assert_eq!(
        manager.update_document(service, URI, "updated".to_owned()),
        Err(SessionError::Transport(TransportError::WriteFailed))
    );
    let snapshot = manager.snapshot(service).unwrap();
    assert_eq!(snapshot.state, SessionState::Faulted);
    assert_eq!(snapshot.pending_requests, 0);
    assert_eq!(snapshot.process_id, None);
    assert_eq!(state.lock().unwrap().live_transports, 0);
}

#[test]
fn every_document_mutation_advances_epoch_and_fences_cross_document_requests() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(30);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    let pending = request(&mut manager, service, workspace_revision, 100);
    assert_eq!(pending.method.as_str(), "textDocument/hover");
    let before = pending.document_epoch;
    let other = "file:///workspace/other.test";
    manager
        .open_document(
            service,
            other.to_owned(),
            DocumentVersion::new(1),
            "other".to_owned(),
        )
        .unwrap();
    assert!(manager.snapshot(service).unwrap().document_epoch > before);
    assert_eq!(manager.snapshot(service).unwrap().pending_requests, 0);
    assert_eq!(state.lock().unwrap().method_count("$/cancelRequest"), 1);
    assert_eq!(
        manager
            .receive_captured_response(service, &pending, &response(pending.request_id.get()))
            .unwrap(),
        ResponseDisposition::Discarded(DiscardReason::StaleDocumentEpoch)
    );

    let current = request(&mut manager, service, workspace_revision, 100);
    manager
        .update_document(service, other, "changed".to_owned())
        .unwrap();
    assert_eq!(
        manager
            .receive_captured_response(service, &current, &response(current.request_id.get()))
            .unwrap(),
        ResponseDisposition::Discarded(DiscardReason::StaleDocumentEpoch)
    );
    manager.shutdown().unwrap();
}

#[test]
fn delayed_generation_one_diagnostics_are_rejected_when_version_coincides() {
    let (mut manager, _, _) = manager();
    let workspace_revision = revision(31);
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    let generation_one = manager.snapshot(service).unwrap().generation;
    let frame = LspCodec::encode(
        &json!({
            "jsonrpc":"2.0",
            "method":"textDocument/publishDiagnostics",
            "params":{"uri":URI,"version":1,"diagnostics":[]}
        }),
        SessionLimits::default().codec,
    )
    .unwrap();
    manager.server_crashed(service).unwrap();
    let generation_two = manager.snapshot(service).unwrap().generation;
    assert!(generation_two > generation_one);
    assert_eq!(
        manager
            .receive_notification(service, generation_one, &frame)
            .unwrap(),
        NotificationDisposition::Discarded(DiscardReason::StaleGeneration)
    );
    assert!(matches!(
        manager
            .receive_notification(service, generation_two, &frame)
            .unwrap(),
        NotificationDisposition::Accepted(_)
    ));

    let missing_version = LspCodec::encode(
        &json!({
            "jsonrpc":"2.0",
            "method":"textDocument/publishDiagnostics",
            "params":{"uri":URI,"diagnostics":[]}
        }),
        SessionLimits::default().codec,
    )
    .unwrap();
    assert_eq!(
        manager.receive_notification(service, generation_two, &missing_version),
        Err(SessionError::InvalidNotification)
    );
    manager.shutdown().unwrap();
}

#[test]
fn transport_owned_receive_drops_unread_old_generation_frames() {
    let (mut manager, state, _) = manager();
    let workspace_revision = revision(33);
    let notification = |version| {
        LspCodec::encode(
            &json!({
                "jsonrpc":"2.0",
                "method":"textDocument/publishDiagnostics",
                "params":{"uri":URI,"version":version,"diagnostics":[]}
            }),
            SessionLimits::default().codec,
        )
        .unwrap()
    };
    state
        .lock()
        .unwrap()
        .next_received_frames
        .push_back(Ok(notification(99)));
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        workspace_revision,
    );
    state
        .lock()
        .unwrap()
        .next_received_frames
        .push_back(Ok(notification(1)));
    manager.server_crashed(service).unwrap();

    let received =
        kit::test_support::receive_current_notification(&mut manager, service, 100).unwrap();
    assert!(matches!(received, NotificationDisposition::Accepted(_)));
    assert_eq!(state.lock().unwrap().receive_frame_entries, 1);
    manager.shutdown().unwrap();
}

#[test]
fn expired_receive_deadline_never_enters_transport_and_reaps_it() {
    let (mut manager, state, clock) = manager();
    let service = open_document(
        &mut manager,
        scope(RevisionPolicy::ManagedLive),
        revision(34),
    );
    clock.set(10);
    assert_eq!(
        kit::test_support::receive_current_notification(&mut manager, service, 10),
        Err(SessionError::Transport(
            TransportError::ReadDeadlineExceeded
        ))
    );
    let state = state.lock().unwrap();
    assert_eq!(state.receive_frame_entries, 0);
    assert_eq!(state.live_transports, 0);
}
