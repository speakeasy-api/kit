#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use kit::{
    domain::{
        events::ContentDigest,
        ids::{DaemonServiceId, PrincipalId, ProcessId, ProjectId, WorkspaceId},
        lifecycle::{ProcessClaim, ProcessOwnership},
    },
    executor::profile::{
        Architecture, ExecutorProfile, Platform, ProfileSpec, ResourceLimits, TrustTier,
    },
    verify::lsp::{
        facts::FactLimits,
        session::{
            CodecLimits, DocumentVersion, ExecutionProfileIdentity, LaunchRequest, LspCodec,
            LspSessionManager, OwnedLspLauncher, OwnedLspTransport, PositionEncoding,
            RevisionPolicy, SendContext, ServerIdentity, SessionError, SessionLimits,
            SessionPurpose, SessionScope, TransportError,
        },
        shadow::{
            ShadowAdapterCapabilities, ShadowAdapterDecision, ShadowAdapterRegistry,
            ShadowAdapterRequest, ShadowDiagnosticScope, ShadowError, ShadowFallbackAction,
            ShadowFallbackReason, ShadowLimits, ShadowLspRunner, ShadowOutcome,
        },
    },
    workspace::{
        edit::{
            ir::{
                ByteRange, EditIr, EditLimits, EditOperation, ExecutableMode, RevisionToken,
                RootRelativePath, TextContent,
            },
            stage::{StageError, StageLimit, StageLimits, StagedEdit, stage},
            validate::validate_authorized,
        },
        revision::ManagedWorkspace,
    },
};
use serde_json::{Value, json};
use url::Url;

const PROBES: usize = 1_000;
const TEST_TIMEOUT: Duration = Duration::from_secs(10);
const CANARY_PREFIX: &[u8] = b"KIT_SHADOW_CANARY_";
const BASELINE: &[u8] = b"KIT_LIVE_BASELINE_SENTINEL\n";
const BASELINE_SENTINEL: &[u8] = b"KIT_LIVE_BASELINE_SENTINEL";

struct Fixture {
    parent: PathBuf,
    root: PathBuf,
    workspace: ManagedWorkspace,
    principal: PrincipalId,
    project: ProjectId,
}

impl Fixture {
    fn new(bytes: &[u8]) -> Self {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let parent = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("kit-shadow-leak-{}", u64::from_le_bytes(nonce)));
        let root = parent.join("workspace");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("probe.txt"), bytes).unwrap();
        let root = root.canonicalize().unwrap();
        let workspace = ManagedWorkspace::open(&root).unwrap();
        Self {
            parent,
            root,
            workspace,
            principal: PrincipalId::generate().unwrap(),
            project: ProjectId::generate().unwrap(),
        }
    }

    fn path(&self) -> RootRelativePath {
        RootRelativePath::parse("probe.txt", EditLimits::default().max_path_bytes).unwrap()
    }

    fn stage_replace<'a>(&'a self, after: &[u8]) -> StagedEdit<'a> {
        let revision = self.workspace.current_revision().unwrap().id();
        let path = self.path();
        let ir = EditIr::new(
            RevisionToken::parse(revision.to_string()).unwrap(),
            vec![EditOperation::ReplaceRange {
                path,
                base_digest: content_digest(BASELINE),
                range: ByteRange::new(0, BASELINE.len()).unwrap(),
                expected: TextContent::from_bytes(BASELINE).unwrap(),
                replacement: TextContent::from_bytes(after).unwrap(),
                executable: ExecutableMode::Preserve,
            }],
            EditLimits::default(),
        )
        .unwrap();
        self.stage(ir)
    }

    fn stage_delete(&self) -> StagedEdit<'_> {
        let revision = self.workspace.current_revision().unwrap().id();
        let ir = EditIr::new(
            RevisionToken::parse(revision.to_string()).unwrap(),
            vec![EditOperation::DeleteFile {
                path: self.path(),
                base_digest: content_digest(BASELINE),
            }],
            EditLimits::default(),
        )
        .unwrap();
        self.stage(ir)
    }

    fn stage(&self, ir: EditIr) -> StagedEdit<'_> {
        let plan = validate_authorized(
            &self.workspace,
            &ir,
            EditLimits::default(),
            kit::test_support::trusted_edit_authority(self.principal, self.project),
        )
        .unwrap();
        stage(plan, StageLimits::default(), &[], &mut []).unwrap()
    }

    fn url(&self) -> Url {
        Url::from_directory_path(&self.root).unwrap()
    }

    fn uri(&self) -> String {
        Url::from_file_path(self.root.join("probe.txt"))
            .unwrap()
            .to_string()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.parent);
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct AuditSnapshot {
    frames: u64,
    bytes: u64,
    canaries: [u64; 16],
    baseline_hits: u64,
    launches: u64,
    close_attempts: u64,
    closes: u64,
    services: Vec<DaemonServiceId>,
    processes: Vec<ProcessId>,
    roots: Vec<ContentDigest>,
    generations: Vec<u64>,
    request_ids: Vec<i64>,
    documents: Vec<(String, i64)>,
    receive_remaining: Vec<Duration>,
    reap_remaining: Vec<Duration>,
}

impl AuditSnapshot {
    fn canary_count(&self) -> usize {
        self.canaries
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    fn observe(&mut self, frame: &[u8]) {
        self.frames += 1;
        self.bytes += frame.len() as u64;
        self.baseline_hits += frame
            .windows(BASELINE_SENTINEL.len())
            .filter(|window| *window == BASELINE_SENTINEL)
            .count() as u64;
        for offset in 0..frame.len().saturating_sub(CANARY_PREFIX.len() + 3) {
            if frame[offset..].starts_with(CANARY_PREFIX) {
                let digits = &frame[offset + CANARY_PREFIX.len()..offset + CANARY_PREFIX.len() + 4];
                if digits.iter().all(u8::is_ascii_digit) {
                    let index = digits.iter().fold(0_usize, |value, digit| {
                        value * 10 + usize::from(digit - b'0')
                    });
                    if index < PROBES {
                        self.canaries[index / 64] |= 1_u64 << (index % 64);
                    }
                }
            }
        }
        let Ok(decoded) = LspCodec::decode(frame, SessionLimits::default().codec) else {
            return;
        };
        let value = decoded.value();
        if let Some(id) = value.get("id").and_then(Value::as_i64) {
            self.request_ids.push(id);
        }
        if value.get("method").and_then(Value::as_str) == Some("textDocument/didOpen") {
            let document = &value["params"]["textDocument"];
            if let (Some(uri), Some(version)) = (
                document.get("uri").and_then(Value::as_str),
                document.get("version").and_then(Value::as_i64),
            ) {
                self.documents.push((uri.to_owned(), version));
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuditPartition {
    Live,
    Shadow,
}

impl AuditPartition {
    fn for_purpose(purpose: &SessionPurpose) -> Self {
        match purpose {
            SessionPurpose::Live => Self::Live,
            SessionPurpose::Shadow(_) => Self::Shadow,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum AuditMutation {
    #[default]
    None,
    RouteOneShadowCanaryToLive,
    DropShadowCanaryTraffic,
}

#[derive(Default)]
struct AuditState {
    live: AuditSnapshot,
    shadow: AuditSnapshot,
    mutation: AuditMutation,
    fail_close: bool,
    fail_close_deadline: bool,
    receive_steps: VecDeque<SourceStep>,
    receive_calls: Option<Arc<Mutex<usize>>>,
}

#[derive(Clone, Default)]
struct AuditLauncher(Arc<Mutex<AuditState>>);

impl AuditLauncher {
    fn with_mutation(mutation: AuditMutation) -> Self {
        let launcher = Self::default();
        launcher.0.lock().unwrap().mutation = mutation;
        launcher
    }

    fn snapshot(&self, partition: AuditPartition) -> AuditSnapshot {
        let state = self.0.lock().unwrap();
        match partition {
            AuditPartition::Live => state.live.clone(),
            AuditPartition::Shadow => state.shadow.clone(),
        }
    }

    fn fail_close(&self) {
        self.0.lock().unwrap().fail_close = true;
    }

    fn fail_close_deadline(&self) {
        self.0.lock().unwrap().fail_close_deadline = true;
    }

    fn observe(&self, partition: AuditPartition, frame: &[u8]) {
        let mut state = self.0.lock().unwrap();
        let contains_canary = frame
            .windows(CANARY_PREFIX.len())
            .any(|window| window == CANARY_PREFIX);
        let partition = match (partition, state.mutation, contains_canary) {
            (AuditPartition::Shadow, AuditMutation::RouteOneShadowCanaryToLive, true) => {
                state.mutation = AuditMutation::None;
                AuditPartition::Live
            }
            (AuditPartition::Shadow, AuditMutation::DropShadowCanaryTraffic, true) => return,
            _ => partition,
        };
        match partition {
            AuditPartition::Live => state.live.observe(frame),
            AuditPartition::Shadow => state.shadow.observe(frame),
        }
    }
}

struct AuditTransport {
    claim: ProcessClaim,
    launcher: AuditLauncher,
    partition: AuditPartition,
    receive_steps: VecDeque<SourceStep>,
    receive_calls: Option<Arc<Mutex<usize>>>,
}

impl OwnedLspLauncher for AuditLauncher {
    type Transport = AuditTransport;

    fn launch(&mut self, request: LaunchRequest<'_>) -> Result<Self::Transport, TransportError> {
        let mut state = self.0.lock().unwrap();
        let partition = AuditPartition::for_purpose(&request.scope.purpose);
        let process_id = ProcessId::generate().unwrap();
        let snapshot = match partition {
            AuditPartition::Live => &mut state.live,
            AuditPartition::Shadow => &mut state.shadow,
        };
        snapshot.launches += 1;
        snapshot.services.push(request.service.id);
        snapshot.processes.push(process_id);
        snapshot
            .roots
            .push(request.scope.canonical_root_identity.clone());
        snapshot.generations.push(request.generation);
        let (receive_steps, receive_calls) = if partition == AuditPartition::Shadow {
            (
                std::mem::take(&mut state.receive_steps),
                state.receive_calls.take(),
            )
        } else {
            (VecDeque::new(), None)
        };
        drop(state);
        Ok(AuditTransport {
            claim: ProcessClaim::new(
                process_id,
                ProcessOwnership::DaemonService(request.service.id),
            ),
            launcher: self.clone(),
            partition,
            receive_steps,
            receive_calls,
        })
    }
}

impl OwnedLspTransport for AuditTransport {
    fn claim(&self) -> ProcessClaim {
        self.claim
    }

    fn initialize(
        &mut self,
        frame: &[u8],
        _: CodecLimits,
        _: SendContext,
    ) -> Result<(), TransportError> {
        self.launcher.observe(self.partition, frame);
        Ok(())
    }

    fn send_frame(&mut self, frame: &[u8], _: SendContext) -> Result<(), TransportError> {
        self.launcher.observe(self.partition, frame);
        Ok(())
    }

    fn receive_frame(
        &mut self,
        limits: CodecLimits,
        context: SendContext,
    ) -> Result<Vec<u8>, TransportError> {
        {
            let mut state = self.launcher.0.lock().unwrap();
            let snapshot = match self.partition {
                AuditPartition::Live => &mut state.live,
                AuditPartition::Shadow => &mut state.shadow,
            };
            snapshot.receive_remaining.push(context.remaining());
        }
        if context.remaining().is_zero() {
            return Err(TransportError::ReadDeadlineExceeded);
        }
        if let Some(calls) = &self.receive_calls {
            *calls.lock().unwrap() += 1;
        }
        let frame = match self
            .receive_steps
            .pop_front()
            .unwrap_or(SourceStep::Error(TransportError::ReadFailed))
        {
            SourceStep::Match(frame) if frame.len() <= limits.max_frame_bytes => Ok(frame),
            SourceStep::Match(_) => Err(TransportError::ReadFailed),
            SourceStep::Error(error) => Err(error),
        }?;
        self.launcher.observe(self.partition, &frame);
        Ok(frame)
    }

    fn close_and_reap(&mut self, context: SendContext) -> Result<(), TransportError> {
        let mut state = self.launcher.0.lock().unwrap();
        let snapshot = match self.partition {
            AuditPartition::Live => &mut state.live,
            AuditPartition::Shadow => &mut state.shadow,
        };
        snapshot.close_attempts += 1;
        snapshot.reap_remaining.push(context.remaining());
        if context.remaining().is_zero() {
            return Err(TransportError::CloseOrReapDeadlineExceeded);
        }
        if std::mem::take(&mut state.fail_close_deadline) {
            return Err(TransportError::CloseOrReapDeadlineExceeded);
        }
        if std::mem::take(&mut state.fail_close) {
            return Err(TransportError::CloseOrReapFailed);
        }
        match self.partition {
            AuditPartition::Live => state.live.closes += 1,
            AuditPartition::Shadow => state.shadow.closes += 1,
        }
        Ok(())
    }
}

enum SourceStep {
    Match(Vec<u8>),
    Error(TransportError),
}

struct Source {
    steps: VecDeque<SourceStep>,
    calls: Arc<Mutex<usize>>,
}

impl Source {
    fn new(steps: impl IntoIterator<Item = SourceStep>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            calls: Arc::new(Mutex::new(0)),
        }
    }
}

fn content_digest(bytes: &[u8]) -> ContentDigest {
    ContentDigest::parse(&format!("blake3:{}", blake3::hash(bytes).to_hex())).unwrap()
}

fn digest(byte: u8) -> ContentDigest {
    ContentDigest::parse(&format!("blake3:{}", format!("{byte:02x}").repeat(32))).unwrap()
}

fn profile() -> ExecutionProfileIdentity {
    #[cfg(target_os = "linux")]
    let platform = Platform::Linux;
    #[cfg(target_os = "macos")]
    let platform = Platform::MacOs;
    ExecutionProfileIdentity::from_profile(
        &ExecutorProfile::new(ProfileSpec::isolated(
            TrustTier::TrustedLocal,
            platform,
            Architecture::Aarch64,
            ResourceLimits::new(
                60_000,
                1 << 30,
                64,
                64 << 20,
                1 << 30,
                1 << 30,
                64 << 20,
                60_000,
            ),
        ))
        .unwrap(),
    )
}

fn server() -> ServerIdentity {
    ServerIdentity {
        server_artifact: digest(1),
        configuration: digest(2),
    }
}

fn adapter_request() -> ShadowAdapterRequest {
    adapter_request_for(
        PrincipalId::generate().unwrap(),
        ProjectId::generate().unwrap(),
        WorkspaceId::generate().unwrap(),
        server(),
        profile(),
    )
}

fn adapter_request_for(
    principal: PrincipalId,
    project: ProjectId,
    workspace: WorkspaceId,
    server: ServerIdentity,
    profile: ExecutionProfileIdentity,
) -> ShadowAdapterRequest {
    ShadowAdapterRequest::new(
        principal,
        project,
        workspace,
        server,
        "fixture-lsp-1.0.0",
        PositionEncoding::Utf16,
        ShadowAdapterCapabilities::new(true, true, true),
        digest(4),
        profile,
    )
    .unwrap()
}

fn supported() -> ShadowAdapterDecision {
    let request = adapter_request();
    kit::test_support::shadow_adapter_registry_fixture(&request, true)
        .unwrap()
        .resolve(request)
}

fn supported_for(request: ShadowAdapterRequest) -> ShadowAdapterDecision {
    kit::test_support::shadow_adapter_registry_fixture(&request, true)
        .unwrap()
        .resolve(request)
}

fn notification(uri: &str, version: i32, messages: impl IntoIterator<Item = String>) -> Vec<u8> {
    let diagnostics = messages
        .into_iter()
        .enumerate()
        .map(|(line, message)| {
            json!({
                "range": {
                    "start": {"line": line, "character": 0},
                    "end": {"line": line, "character": message.len()}
                },
                "severity": 2,
                "source": "shadow-audit",
                "message": message
            })
        })
        .collect::<Vec<Value>>();
    LspCodec::encode(
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {"uri": uri, "version": version, "diagnostics": diagnostics}
        }),
        SessionLimits::default().codec,
    )
    .unwrap()
}

fn empty_notification(uri: &str, version: i32) -> Vec<u8> {
    notification(uri, version, [])
}

fn runner(launcher: AuditLauncher, source: Source) -> ShadowLspRunner<AuditLauncher> {
    runner_with_limits(launcher, source, SessionLimits::default())
}

fn runner_with_limits(
    launcher: AuditLauncher,
    source: Source,
    session_limits: SessionLimits,
) -> ShadowLspRunner<AuditLauncher> {
    {
        let mut state = launcher.0.lock().unwrap();
        state.receive_steps = source.steps;
        state.receive_calls = Some(source.calls);
    }
    ShadowLspRunner::new(
        launcher,
        session_limits,
        EditLimits::default(),
        FactLimits::default(),
        ShadowLimits::default(),
    )
    .unwrap()
}

fn probes() -> (String, Vec<String>) {
    let canaries = (0..PROBES)
        .map(|index| format!("KIT_SHADOW_CANARY_{index:04}"))
        .collect::<Vec<_>>();
    let mut text = canaries.join("\n");
    text.push('\n');
    (text, canaries)
}

fn isolation_oracle(
    shadow: &AuditSnapshot,
    live_before: &AuditSnapshot,
    live_after: &AuditSnapshot,
) -> bool {
    shadow.canary_count() == PROBES
        && live_after.frames == live_before.frames
        && live_after.bytes == live_before.bytes
        && live_after.canaries == live_before.canaries
}

fn deadline(runner: &ShadowLspRunner<AuditLauncher>) -> u64 {
    runner.deadline_after(TEST_TIMEOUT).unwrap()
}

pub(crate) fn assert_staged_lsp_isolation() {
    assert!(run_isolation_probe(AuditMutation::None, true));
    assert!(!run_isolation_probe(
        AuditMutation::RouteOneShadowCanaryToLive,
        false
    ));
    assert!(!run_isolation_probe(
        AuditMutation::DropShadowCanaryTraffic,
        false
    ));
}

fn run_isolation_probe(mutation: AuditMutation, assert_evidence: bool) -> bool {
    let (staged_text, canaries) = probes();
    let fixture = Fixture::new(BASELINE);
    let staged = fixture.stage_replace(staged_text.as_bytes());
    let staged_read = staged
        .read_file(&fixture.path(), staged_text.len())
        .unwrap();
    let mut staged_audit = AuditSnapshot::default();
    staged_audit.observe(&staged_read);
    assert_eq!(staged_audit.canary_count(), PROBES);

    let workspace = WorkspaceId::generate().unwrap();
    let server = server();
    let profile = profile();
    let live_root = digest(3);
    let launcher = AuditLauncher::with_mutation(mutation);
    let mut live = LspSessionManager::new(launcher.clone(), SessionLimits::default()).unwrap();
    let live_service = live
        .open(
            SessionScope {
                principal_id: fixture.principal,
                project_id: fixture.project,
                workspace_id: workspace,
                canonical_root_identity: live_root.clone(),
                purpose: SessionPurpose::Live,
                revision_policy: RevisionPolicy::ManagedLive,
                server: server.clone(),
                position_encoding: PositionEncoding::Utf16,
                execution_profile: profile.clone(),
            },
            staged.revision(),
        )
        .unwrap();
    live.open_document(
        live_service,
        fixture.uri(),
        DocumentVersion::new(1),
        String::from_utf8(BASELINE.to_vec()).unwrap(),
    )
    .unwrap();
    let live_session = live.snapshot(live_service).unwrap();
    let live_before = launcher.snapshot(AuditPartition::Live);
    assert!(live_before.baseline_hits > 0);
    assert_eq!(live_before.canary_count(), 0);

    let source = Source::new([SourceStep::Match(notification(
        &fixture.uri(),
        1,
        canaries.clone(),
    ))]);
    let mut shadow = runner(launcher.clone(), source);
    let deadline = deadline(&shadow);
    let ShadowOutcome::Completed(report) = shadow
        .run_staged(
            &staged,
            &fixture.root,
            &fixture.url(),
            ShadowDiagnosticScope::Document,
            supported_for(adapter_request_for(
                fixture.principal,
                fixture.project,
                workspace,
                server,
                profile,
            )),
            deadline,
        )
        .unwrap()
    else {
        panic!("supported real stage did not complete")
    };
    let shadow_after = launcher.snapshot(AuditPartition::Shadow);
    let live_after = launcher.snapshot(AuditPartition::Live);
    let isolated = isolation_oracle(&shadow_after, &live_before, &live_after);
    if assert_evidence {
        assert_eq!(shadow_after.canary_count(), PROBES);
        assert_eq!(report.diagnostics().len(), PROBES);
        assert_eq!(report.staged_digest().as_str(), staged.state_digest());
        let mut terminal = AuditSnapshot::default();
        for diagnostic in report.diagnostics() {
            terminal.observe(diagnostic.message().as_bytes());
        }
        assert_eq!(terminal.canary_count(), PROBES);
        assert_eq!(live_before.services, [live_service]);
        assert_eq!(shadow_after.services, [report.service_id()]);
        assert_ne!(live_service, report.service_id());
        assert_eq!(live_before.processes, [live_session.process_id.unwrap()]);
        assert_eq!(shadow_after.processes, [report.process_id()]);
        assert_ne!(live_session.process_id.unwrap(), report.process_id());
        assert_eq!(
            live_before.roots.as_slice(),
            std::slice::from_ref(&live_root)
        );
        assert_eq!(
            shadow_after.roots,
            [report.canonical_root_identity().clone()]
        );
        assert_ne!(live_root, *report.canonical_root_identity());
        assert_eq!(live_before.generations, [1]);
        assert_eq!(shadow_after.generations, [1]);
        assert_eq!(live_session.generation, report.generation());
        assert!(live_before.request_ids.contains(&0));
        assert!(shadow_after.request_ids.contains(&0));
        let document = (fixture.uri(), 1);
        assert!(live_before.documents.contains(&document));
        assert!(shadow_after.documents.contains(&document));
        assert!(
            shadow_after
                .receive_remaining
                .iter()
                .all(|remaining| !remaining.is_zero() && *remaining <= TEST_TIMEOUT)
        );
        assert!(!shadow_after.receive_remaining.is_empty());
        assert!(
            shadow_after
                .reap_remaining
                .iter()
                .all(|remaining| !remaining.is_zero() && *remaining <= TEST_TIMEOUT)
        );
        assert!(!shadow_after.reap_remaining.is_empty());
        assert!(isolated);
    }
    live.shutdown().unwrap();
    isolated
}

#[test]
fn unresolved_pins_workspace_and_deleted_are_zero_launch_fallbacks() {
    let fixture = Fixture::new(BASELINE);
    let staged = fixture.stage_replace(b"replacement\n");
    let launcher = AuditLauncher::default();
    let mut runner = runner(launcher.clone(), Source::new([]));
    let finite_deadline = deadline(&runner);
    for (scope, decision, reason, deadline) in [
        (
            ShadowDiagnosticScope::Document,
            ShadowAdapterRegistry::compiled().resolve(adapter_request()),
            ShadowFallbackReason::PinsUnavailable,
            0,
        ),
        (
            ShadowDiagnosticScope::Workspace,
            supported(),
            ShadowFallbackReason::NoIsolatedWorkspace,
            finite_deadline,
        ),
    ] {
        let ShadowOutcome::Fallback(record) = runner
            .run_staged(
                &staged,
                &fixture.root,
                &fixture.url(),
                scope,
                decision,
                deadline,
            )
            .unwrap()
        else {
            panic!("expected fallback")
        };
        assert_eq!(record.reason(), reason);
        assert_eq!(
            record.action(),
            ShadowFallbackAction::CompilerChecksThenLiveDiagnosticsAfterCommit
        );
        assert_eq!(record.base_revision(), staged.revision());
        assert_eq!(record.staged_digest().as_str(), staged.state_digest());
        assert_eq!(record.affected_paths(), ["probe.txt"]);
        assert_eq!(record.affected_count(), 1);
        assert_eq!(record.server(), &server());
    }
    drop(staged);
    let deleted_fixture = Fixture::new(BASELINE);
    let deleted = deleted_fixture.stage_delete();
    let finite_deadline = deadline(&runner);
    let ShadowOutcome::Fallback(record) = runner
        .run_staged(
            &deleted,
            &deleted_fixture.root,
            &deleted_fixture.url(),
            ShadowDiagnosticScope::Document,
            supported(),
            finite_deadline,
        )
        .unwrap()
    else {
        panic!("expected deleted fallback")
    };
    assert_eq!(record.reason(), ShadowFallbackReason::DeletedEffect);
    assert_eq!(record.server(), &server());
    assert_eq!(launcher.snapshot(AuditPartition::Shadow).launches, 0);
}

#[test]
fn stale_generation_and_version_notifications_cannot_complete() {
    let fixture = Fixture::new(BASELINE);
    let staged = fixture.stage_replace(b"replacement\n");
    let launcher = AuditLauncher::default();
    let mut wrong_generation = runner(
        launcher.clone(),
        Source::new([SourceStep::Error(TransportError::ReadFailed)]),
    );
    let finite_deadline = deadline(&wrong_generation);
    assert_eq!(
        wrong_generation.run_staged(
            &staged,
            &fixture.root,
            &fixture.url(),
            ShadowDiagnosticScope::Document,
            supported(),
            finite_deadline,
        ),
        Err(ShadowError::SourceUnavailable)
    );
    let mut stale_version = runner(
        launcher,
        Source::new([
            SourceStep::Match(empty_notification(&fixture.uri(), 0)),
            SourceStep::Error(TransportError::ReadFailed),
        ]),
    );
    let finite_deadline = deadline(&stale_version);
    assert_eq!(
        stale_version.run_staged(
            &staged,
            &fixture.root,
            &fixture.url(),
            ShadowDiagnosticScope::Document,
            supported(),
            finite_deadline,
        ),
        Err(ShadowError::SourceUnavailable)
    );
}

#[test]
fn source_timeout_and_cleanup_failure_attempt_transport_close() {
    let fixture = Fixture::new(BASELINE);
    let staged = fixture.stage_replace(b"replacement\n");
    let timeout_launcher = AuditLauncher::default();
    let mut timeout = runner(
        timeout_launcher.clone(),
        Source::new([SourceStep::Error(TransportError::ReadDeadlineExceeded)]),
    );
    let finite_deadline = deadline(&timeout);
    assert_eq!(
        timeout.run_staged(
            &staged,
            &fixture.root,
            &fixture.url(),
            ShadowDiagnosticScope::Document,
            supported(),
            finite_deadline,
        ),
        Err(ShadowError::DeadlineExceeded)
    );
    assert_eq!(
        timeout_launcher
            .snapshot(AuditPartition::Shadow)
            .close_attempts,
        1
    );

    let failing_launcher = AuditLauncher::default();
    failing_launcher.fail_close();
    let mut failing = runner(
        failing_launcher.clone(),
        Source::new([SourceStep::Match(empty_notification(&fixture.uri(), 1))]),
    );
    let finite_deadline = deadline(&failing);
    assert!(matches!(
        failing.run_staged(
            &staged,
            &fixture.root,
            &fixture.url(),
            ShadowDiagnosticScope::Document,
            supported(),
            finite_deadline,
        ),
        Err(ShadowError::CleanupFailed(_))
    ));
    assert_eq!(
        failing_launcher
            .snapshot(AuditPartition::Shadow)
            .close_attempts,
        1
    );

    let deadline_launcher = AuditLauncher::default();
    deadline_launcher.fail_close_deadline();
    let mut deadline = runner(
        deadline_launcher.clone(),
        Source::new([SourceStep::Match(empty_notification(&fixture.uri(), 1))]),
    );
    let finite_deadline = self::deadline(&deadline);
    assert_eq!(
        deadline.run_staged(
            &staged,
            &fixture.root,
            &fixture.url(),
            ShadowDiagnosticScope::Document,
            supported(),
            finite_deadline,
        ),
        Err(ShadowError::CleanupFailed(SessionError::Transport(
            TransportError::CloseOrReapDeadlineExceeded
        )))
    );
    assert_eq!(
        deadline_launcher
            .snapshot(AuditPartition::Shadow)
            .close_attempts,
        1
    );
}

#[test]
fn nonce_isolates_runs_and_empty_diagnostics_require_evidence_and_cleanup() {
    let fixture = Fixture::new(BASELINE);
    let staged = fixture.stage_replace(b"replacement\n");
    let launcher = AuditLauncher::default();
    let mut reports = Vec::new();
    for _ in 0..2 {
        let source = Source::new([SourceStep::Match(empty_notification(&fixture.uri(), 1))]);
        let calls = source.calls.clone();
        let mut shadow_runner = runner(launcher.clone(), source);
        let finite_deadline = deadline(&shadow_runner);
        let ShadowOutcome::Completed(report) = shadow_runner
            .run_staged(
                &staged,
                &fixture.root,
                &fixture.url(),
                ShadowDiagnosticScope::Document,
                supported(),
                finite_deadline,
            )
            .unwrap()
        else {
            panic!("expected completion")
        };
        assert!(report.diagnostics().is_empty());
        assert_eq!(report.accepted_notification_count(), 1);
        assert_eq!(*calls.lock().unwrap(), 1);
        reports.push(report);
    }
    assert_ne!(reports[0].run_identity(), reports[1].run_identity());
    assert_eq!(
        reports[0].canonical_root_identity(),
        reports[1].canonical_root_identity()
    );
    assert_ne!(reports[0].service_id(), reports[1].service_id());
    assert_eq!(launcher.snapshot(AuditPartition::Shadow).closes, 2);

    let mut no_evidence = runner(AuditLauncher::default(), Source::new([]));
    let finite_deadline = deadline(&no_evidence);
    assert_eq!(
        no_evidence.run_staged(
            &staged,
            &fixture.root,
            &fixture.url(),
            ShadowDiagnosticScope::Document,
            supported(),
            finite_deadline,
        ),
        Err(ShadowError::SourceUnavailable)
    );
}

#[test]
fn expired_deadline_prevents_stage_read_launch_and_receive() {
    let fixture = Fixture::new(BASELINE);
    let staged = fixture.stage_replace(b"replacement\n");
    let launcher = AuditLauncher::default();
    let source = Source::new([SourceStep::Match(empty_notification(&fixture.uri(), 1))]);
    let calls = source.calls.clone();
    let mut shadow = runner(launcher.clone(), source);
    assert_eq!(
        shadow.run_staged(
            &staged,
            &fixture.root,
            &fixture.url(),
            ShadowDiagnosticScope::Document,
            supported(),
            0,
        ),
        Err(ShadowError::DeadlineExceeded)
    );
    assert_eq!(launcher.snapshot(AuditPartition::Shadow).launches, 0);
    assert_eq!(*calls.lock().unwrap(), 0);
}

#[test]
fn staged_read_honors_expired_instant() {
    let fixture = Fixture::new(BASELINE);
    let staged = fixture.stage_replace(b"replacement\n");
    assert!(matches!(
        staged.read_file_before(&fixture.path(), 1024, Instant::now()),
        Err(StageError::LimitExceeded(StageLimit::Time))
    ));
}

#[test]
fn near_limit_staged_text_is_rejected_before_retained_clones_or_launch() {
    let fixture = Fixture::new(BASELINE);
    let staged = fixture.stage_replace(&vec![b'x'; 4_095]);
    let launcher = AuditLauncher::default();
    let limits = SessionLimits {
        max_document_bytes: 4_096,
        max_total_document_bytes: 4_096,
        ..SessionLimits::default()
    };
    let mut shadow = runner_with_limits(launcher.clone(), Source::new([]), limits);
    let finite_deadline = deadline(&shadow);
    assert_eq!(
        shadow.run_staged(
            &staged,
            &fixture.root,
            &fixture.url(),
            ShadowDiagnosticScope::Document,
            supported(),
            finite_deadline,
        ),
        Err(ShadowError::StageReadFailed)
    );
    assert_eq!(launcher.snapshot(AuditPartition::Shadow).launches, 0);
}
