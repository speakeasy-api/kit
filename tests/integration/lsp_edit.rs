//! End-to-end conformance for the production stdio LSP launcher: a real child
//! process (the kit binary in `--kit-lsp-conformance-worker` mode) speaking
//! Content-Length framing, driven through `ShadowLspRunner::run_staged`
//! against a genuinely staged edit.

#![cfg(unix)]

use std::{fs, path::PathBuf, time::Duration};

use kit::{
    domain::{
        events::ContentDigest,
        ids::{PrincipalId, ProjectId, WorkspaceId},
    },
    executor::profile::{
        Architecture, ExecutorProfile, Platform, ProfileSpec, ResourceLimits, TrustTier,
    },
    verify::lsp::{
        facts::FactLimits,
        launcher::{NativeLspServerConfig, StdioLspLauncher},
        session::{
            ExecutionProfileIdentity, PositionEncoding, ServerIdentity, SessionLimits,
        },
        shadow::{
            ShadowAdapterCapabilities, ShadowAdapterDecision, ShadowAdapterRegistry,
            ShadowAdapterRequest, ShadowDiagnosticScope, ShadowError, ShadowLimits,
            ShadowLspRunner, ShadowOutcome,
        },
    },
    workspace::{
        edit::{
            ir::{
                ByteRange, EditIr, EditLimits, EditOperation, ExecutableMode, RevisionToken,
                RootRelativePath, TextContent,
            },
            stage::{StageLimits, StagedEdit, stage},
            validate::validate_authorized,
        },
        revision::ManagedWorkspace,
    },
};
use url::Url;

const BASELINE: &[u8] = b"deterministic staged baseline\n";
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

struct Fixture {
    parent: PathBuf,
    root: PathBuf,
    workspace: ManagedWorkspace,
    principal: PrincipalId,
    project: ProjectId,
}

impl Fixture {
    fn new() -> Self {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let parent = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("kit-lsp-launcher-{}", u64::from_le_bytes(nonce)));
        let root = parent.join("workspace");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("probe.txt"), BASELINE).unwrap();
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

    fn stage_replace(&self, after: &[u8]) -> StagedEdit<'_> {
        let revision = self.workspace.current_revision().unwrap().id();
        let path = RootRelativePath::parse("probe.txt", EditLimits::default().max_path_bytes)
            .unwrap();
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.parent);
    }
}

fn content_digest(bytes: &[u8]) -> ContentDigest {
    ContentDigest::parse(&format!("blake3:{}", blake3::hash(bytes).to_hex())).unwrap()
}

fn digest(byte: u8) -> ContentDigest {
    content_digest(&[byte])
}

fn profile() -> ExecutionProfileIdentity {
    let platform = if cfg!(target_os = "macos") {
        Platform::MacOs
    } else {
        Platform::Linux
    };
    let architecture = if cfg!(target_arch = "aarch64") {
        Architecture::Aarch64
    } else {
        Architecture::X86_64
    };
    ExecutionProfileIdentity::from_profile(
        &ExecutorProfile::new(ProfileSpec::isolated(
            TrustTier::TrustedLocal,
            platform,
            architecture,
            ResourceLimits::new(1, 1, 1, 1, 1, 1, 1, 1),
        ))
        .unwrap(),
    )
}

fn supported(fixture: &Fixture) -> ShadowAdapterDecision {
    let request = ShadowAdapterRequest::new(
        fixture.principal,
        fixture.project,
        WorkspaceId::generate().unwrap(),
        ServerIdentity {
            server_artifact: digest(1),
            configuration: digest(2),
        },
        "kit-lsp-conformance-worker-1",
        PositionEncoding::Utf16,
        ShadowAdapterCapabilities::new(true, true, true),
        digest(4),
        profile(),
    )
    .unwrap();
    ShadowAdapterRegistry::from_trusted_request(&request)
        .unwrap()
        .resolve(request)
}

fn worker_config(mode: &str) -> NativeLspServerConfig {
    NativeLspServerConfig::new(
        env!("CARGO_BIN_EXE_kit").to_owned(),
        vec!["--kit-lsp-conformance-worker".to_owned(), mode.to_owned()],
        vec!["rust".to_owned()],
        5_000,
        100,
    )
    .unwrap()
}

fn runner(fixture: &Fixture, mode: &str) -> ShadowLspRunner<StdioLspLauncher> {
    let session_limits = SessionLimits::default();
    let launcher = StdioLspLauncher::new(
        &worker_config(mode),
        fixture.root.clone(),
        session_limits.codec,
    );
    ShadowLspRunner::new(
        launcher,
        session_limits,
        EditLimits::default(),
        FactLimits::default(),
        ShadowLimits::default(),
    )
    .unwrap()
}

#[test]
fn stdio_launcher_collects_staged_diagnostics_and_filters_server_noise() {
    let fixture = Fixture::new();
    let staged = fixture.stage_replace(b"staged replacement with a defect\n");
    let mut runner = runner(&fixture, "diagnose");
    let deadline = runner.deadline_after(TEST_TIMEOUT).unwrap();
    let outcome = runner
        .run_staged(
            &staged,
            &fixture.root,
            &fixture.url(),
            ShadowDiagnosticScope::Document,
            supported(&fixture),
            deadline,
        )
        .unwrap();
    let ShadowOutcome::Completed(report) = outcome else {
        panic!("expected completed shadow run, got {outcome:?}");
    };
    assert_eq!(report.accepted_notification_count(), 1);
    let diagnostics = report.diagnostics();
    assert_eq!(diagnostics.len(), 1);
    let diagnostic = &diagnostics[0];
    assert_eq!(diagnostic.path().as_path().as_str(), "probe.txt");
    assert_eq!(diagnostic.severity(), Some(1));
    assert_eq!(diagnostic.source(), Some("kit-fake-lsp"));
    assert_eq!(diagnostic.message(), "kit fake lsp diagnostic");
}

#[test]
fn stdio_launcher_expires_a_hanging_server_within_the_deadline() {
    let fixture = Fixture::new();
    let staged = fixture.stage_replace(b"staged replacement that hangs\n");
    let mut runner = runner(&fixture, "hang");
    let deadline = runner.deadline_after(Duration::from_millis(1_200)).unwrap();
    let error = runner
        .run_staged(
            &staged,
            &fixture.root,
            &fixture.url(),
            ShadowDiagnosticScope::Document,
            supported(&fixture),
            deadline,
        )
        .unwrap_err();
    assert_eq!(error, ShadowError::DeadlineExceeded);
}

#[test]
fn stdio_launcher_reports_a_crashed_server_without_hanging() {
    let fixture = Fixture::new();
    let staged = fixture.stage_replace(b"staged replacement that crashes\n");
    let mut runner = runner(&fixture, "crash");
    let deadline = runner.deadline_after(TEST_TIMEOUT).unwrap();
    let result = runner.run_staged(
        &staged,
        &fixture.root,
        &fixture.url(),
        ShadowDiagnosticScope::Document,
        supported(&fixture),
        deadline,
    );
    assert!(result.is_err(), "crashed server must fail the shadow run");
}
