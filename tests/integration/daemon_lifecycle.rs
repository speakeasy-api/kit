use std::{
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, Mac};
#[cfg(unix)]
use std::{
    io::{ErrorKind, Read},
    process::{Child, Command as ProcessCommand, Stdio},
    thread,
    time::Instant,
};

#[cfg(unix)]
struct ChildGuard(Child);

#[cfg(unix)]
impl ChildGuard {
    fn kill9(&mut self) {
        assert!(
            ProcessCommand::new("kill")
                .args(["-KILL", &self.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        self.0.wait().unwrap();
    }

    fn assert_alive(&mut self) {
        assert!(
            self.0.try_wait().unwrap().is_none(),
            "daemon exited at barrier"
        );
    }
}

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

use kit::{
    agent::{
        driver::restart::EffectCorrelation,
        executor::{FakeResponse, FakeScenario},
        extensions::{
            ContentDigest, ExtensionConfigLayer, ExtensionIdentity, ExtensionPoint,
            ExtensionReference, ExtensionVersion,
        },
        providers::{
            adapter::StreamCommitFactory, persistence::SqliteStreamCommitFactory,
            streaming::StreamLimits,
        },
    },
    api::service::{Query, QueryProjection, ServiceStore},
    cli::core::read_discovery,
    domain::ids::{ArtifactId, PrincipalId, ProjectId, RunId, ThreadId},
    runtime::{
        daemon::{DISCOVERY_FILE, Daemon, DaemonConfig, DaemonError, DaemonSignal},
        lease::StateRootLockError,
    },
    store::artifacts::{
        ArtifactClass, ArtifactMetadata, ArtifactRetention, ArtifactStore, now_unix_micros,
    },
    store::backup::BackupConfig,
    test_support,
};

async fn start_daemon(config: DaemonConfig) -> Result<Daemon, DaemonError> {
    Daemon::start(config, DaemonSignal::install().unwrap()).await
}

fn setup_error(result: Result<Daemon, DaemonError>) -> String {
    match result {
        Err(DaemonError::Setup(error)) => error,
        Err(error) => panic!("expected daemon setup error, got {error}"),
        Ok(daemon) => {
            drop(daemon);
            panic!("invalid provider configuration started the daemon")
        }
    }
}
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use kit::{
    api::http::exec::AllocateTerminalBody,
    cli::{
        core::HttpClient,
        exec::{ExecRequest, execute as execute_exec},
    },
    domain::{
        ids::{AttemptId, ProcessId, TerminalId},
        lifecycle::{AttemptOwnership, FencingToken, ProcessClaim, ProcessOwnership},
    },
    executor::{
        process::{
            own::{ProcessRegistrationContext, ProcessTerminalConfig},
            tree::{BoundaryIdentity, BoundaryKind, Ownership, PersistedBoundary},
        },
        terminal::{OutputRetention, TerminalRequest, TerminalSize},
    },
    store::sqlite::idempotency::IdempotencyKey,
    telemetry::redact::{CaptureBoundary, CapturePersistencePolicy, CaptureRedactor},
};

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let sequence = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kit-daemon-lifecycle-{}-{sequence}",
            std::process::id()
        ));
        Self(path)
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

async fn request(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: &str,
) -> (u16, String) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let mut wire = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        wire.push_str(name);
        wire.push_str(": ");
        wire.push_str(value);
        wire.push_str("\r\n");
    }
    wire.push_str("\r\n");
    wire.push_str(body);
    stream.write_all(wire.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    (status, body.to_owned())
}

async fn wait_ready(address: std::net::SocketAddr) {
    for _ in 0..100 {
        if let Ok(mut stream) = TcpStream::connect(address).await {
            let request = format!(
                "GET /health/ready HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
            );
            if stream.write_all(request.as_bytes()).await.is_ok() {
                let mut response = Vec::new();
                if stream.read_to_end(&mut response).await.is_ok()
                    && response.starts_with(b"HTTP/1.1 200")
                {
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon did not become ready");
}

async fn wait_run_state(
    address: std::net::SocketAddr,
    credential: &str,
    origin: &str,
    run_id: RunId,
    expected: &str,
) -> serde_json::Value {
    let mut last = serde_json::Value::Null;
    for attempt in 0..3_000 {
        let headers = authenticated_headers(
            credential,
            origin,
            &format!("run-state-{run_id}-{expected}-{attempt}"),
        );
        let (status, body) =
            request(address, "GET", &format!("/v1/runs/{run_id}"), &headers, "").await;
        assert_eq!(status, 200, "{body}");
        last = serde_json::from_str(&body).unwrap();
        if last["state"] == expected {
            return last;
        }
        if matches!(last["state"].as_str(), Some("failed" | "cancelled")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run did not reach {expected}: {last}");
}

async fn wait_repository_result(
    address: std::net::SocketAddr,
    credential: &str,
    origin: &str,
    result_id: &str,
    nonce: &str,
) -> serde_json::Value {
    let mut last = serde_json::Value::Null;
    for attempt in 0..3_000 {
        let headers = authenticated_headers(
            credential,
            origin,
            &format!("{nonce}-{result_id}-{attempt}"),
        );
        let (status, body) = request(
            address,
            "GET",
            &format!("/v1/repository-results/{result_id}"),
            &headers,
            "",
        )
        .await;
        assert_eq!(status, 200, "{body}");
        last = serde_json::from_str(&body).unwrap();
        if matches!(
            last["status"].as_str(),
            Some("completed" | "failed" | "cancelled" | "outcome_unknown" | "denied")
        ) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("repository operation did not terminate: {last}");
}

async fn start_scenario_run(
    root: &TestRoot,
    scenario: FakeScenario,
) -> (Daemon, kit::cli::core::DaemonDiscovery, ThreadId, RunId) {
    let project = root.0.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("README.md"),
        "deterministic native workspace\n",
    )
    .unwrap();
    init_git(&project);
    let daemon = start_daemon(
        DaemonConfig::new(&root.0)
            .with_development_provider(
                kit::domain::config::Provider::OpenAi,
                FakeResponse::completed("daemon scenario complete"),
                scenario,
            )
            .with_project_root(project),
    )
    .await
    .unwrap();
    let address = daemon.endpoint();
    let project = daemon.project_id();
    wait_ready(address).await;
    let discovery = read_discovery(&root.0).unwrap();
    let command_headers = |nonce: &str, key: &str| {
        let mut headers = authenticated_headers(&discovery.credential, &discovery.endpoint, nonce);
        headers.push(("Content-Type", "application/json".to_owned()));
        headers.push(("Idempotency-Key", key.to_owned()));
        headers
    };
    let (status, body) = request(
        address,
        "POST",
        "/v1/projects",
        &command_headers("scenario-project", "scenario-project"),
        &format!(r#"{{"id":"{project}"}}"#),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let thread = ThreadId::generate().unwrap();
    let (status, body) = request(
        address,
        "POST",
        &format!("/v1/projects/{project}/threads"),
        &command_headers("scenario-thread", "scenario-thread"),
        &format!(r#"{{"id":"{thread}"}}"#),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let run = RunId::generate().unwrap();
    let (status, body) = request(
        address,
        "POST",
        &format!("/v1/threads/{thread}/runs"),
        &command_headers("scenario-run", "scenario-run"),
        &format!(r#"{{"id":"{run}","message":"exercise daemon scenario"}}"#),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    (daemon, discovery, thread, run)
}

fn development_config(root: impl Into<std::path::PathBuf>) -> DaemonConfig {
    let root = root.into();
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("README.md"),
        "deterministic native workspace\n",
    )
    .unwrap();
    init_git(&project);
    DaemonConfig::new(root)
        .with_development_provider(
            kit::domain::config::Provider::OpenAi,
            FakeResponse::completed("daemon test provider"),
            FakeScenario::Complete,
        )
        .with_project_root(project)
}

fn init_git(root: &std::path::Path) {
    if root.join(".git").exists() {
        return;
    }
    for arguments in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Kit Test",
            "-c",
            "user.email=kit@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(arguments)
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }
}

#[tokio::test]
async fn daemon_start_only_validates_configured_mcp_servers() {
    let root = TestRoot::new();
    let mut config = development_config(&root.0);
    config.mcp_servers = vec![
        serde_json::from_value(serde_json::json!({
            "id": "offline-docs",
            "transport": {
                "kind": "http",
                "endpoint": "https://127.0.0.1:9/mcp"
            },
            "owner": {
                "principal_id": PrincipalId::generate().unwrap(),
                "project_id": ProjectId::generate().unwrap()
            },
            "source": "mcp.docs",
            "trust_domain": "loopback.invalid",
            "namespace": "docs",
            "version": "1",
            "credential_handle": "env:KIT_TEST_MCP_TOKEN",
            "credential_scope": {"kind": "project"},
            "egress": {"scheme": "https", "host": "127.0.0.1", "port": 9},
            "descriptors": [{
                "kind": "tool",
                "remote": "search",
                "descriptor_digest": format!("sha256:{}", "00".repeat(32)),
                "effect": "network_egress",
                "retry_safety": "idempotent",
                "required_grants": ["network_egress"],
                "auth_scopes": ["docs.read"],
                "availability": "available"
            }]
        }))
        .unwrap(),
    ];

    let daemon = tokio::time::timeout(Duration::from_secs(5), start_daemon(config))
        .await
        .expect("daemon startup attempted MCP I/O")
        .unwrap();
    daemon.shutdown().await.unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_repository_http_structural_preview_applies_the_exact_diff_once() {
    use std::collections::{BTreeMap, BTreeSet};

    use kit::{
        executor::{check::ConformanceCheck, profile::ResourceLimits},
        verify::{
            feedback::{DiagnosticAdapter, FeedbackLimits},
            profiles::{CheckClass, CheckRequirement, DeclaredCheck, VerificationRegistry},
        },
    };

    let root = TestRoot::new();
    let project_root = root.0.join("project");
    fs::create_dir_all(project_root.join("src")).unwrap();
    fs::write(
        project_root.join("src/lib.rs"),
        "fn f() { let value = Some(1); }\n",
    )
    .unwrap();
    fs::write(
        project_root.join("src/base.rs"),
        "pub const BASE_ONLY_SENTINEL: u8 = 1;\n",
    )
    .unwrap();
    init_git(&project_root);
    let check = kit::executor::check::CheckCommand::new(
        "diagnostics",
        "cargo",
        vec!["check".to_owned()],
        format!("example.invalid/check@sha256:{}", "a".repeat(64)),
        format!("sha256:{}", "b".repeat(64)),
        format!("sha256:{}", "c".repeat(64)),
        ResourceLimits::new(1_000, 1024 * 1024, 8, 1024, 1024, 1024, 1024, 1_000),
    )
    .unwrap();
    let registry = VerificationRegistry::new(vec![
        DeclaredCheck::new(
            CheckClass::Diagnostics,
            check,
            CheckRequirement::Required,
            BTreeSet::new(),
            false,
        )
        .unwrap(),
    ])
    .unwrap();
    let config = development_config(&root.0)
        .with_project_root(&project_root)
        .with_verification_registry(registry)
        .with_native_feedback(
            BTreeMap::from([(
                "diagnostics".to_owned(),
                DiagnosticAdapter::NormalizedJsonLinesV1,
            )]),
            FeedbackLimits::default(),
        )
        .with_native_check_completions([
            ConformanceCheck::pass(b"", b""),
            ConformanceCheck::pass(b"", b""),
        ]);
    let daemon = start_daemon(config).await.unwrap();
    wait_ready(daemon.endpoint()).await;
    let discovery = read_discovery(&root.0).unwrap();
    let project = daemon.project_id();

    let headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "repo-lifecycle-revision",
    );
    let (status, body) = request(
        daemon.endpoint(),
        "GET",
        &format!("/v1/projects/{project}/repository/revision"),
        &headers,
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let revision = serde_json::from_str::<serde_json::Value>(&body).unwrap()["revision"]
        .as_str()
        .unwrap()
        .to_owned();

    let mut headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "repo-lifecycle-search",
    );
    headers.push(("Content-Type", "application/json".to_owned()));
    let search_input = serde_json::json!({
        "expected_revision": revision,
        "text": "Some($A)",
        "mode": "structural",
        "rewrite": "Ok($A)",
        "path_prefixes": [],
        "languages": ["rust"]
    });
    let (status, body) = request(
        daemon.endpoint(),
        "POST",
        &format!("/v1/projects/{project}/repository/search"),
        &headers,
        &search_input.to_string(),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let queued: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(queued["operation"], "repo.search");
    assert_eq!(queued["status"], "queued");
    let search = wait_repository_result(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        queued["id"].as_str().unwrap(),
        "repo-lifecycle-search-result",
    )
    .await;
    assert_eq!(search["status"], "completed", "{search}");
    let preview = &search["output"]["data"];
    let rewrite = preview["rewrite"].as_object().unwrap();
    let public_preview = preview.to_string();
    for hidden in ["\"operations\":", "\"replacement\":", "\"expected\":"] {
        assert!(
            !public_preview.contains(hidden),
            "public rewrite exposed {hidden}"
        );
    }
    assert!(!public_preview.contains("BASE_ONLY_SENTINEL"));
    let preview_diff = rewrite["change_diff"].as_str().unwrap().to_owned();
    let token = rewrite["apply"]["preview_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let edit_input = serde_json::json!({"preview_token": token});

    let mut headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "repo-lifecycle-edit",
    );
    headers.push(("Content-Type", "application/json".to_owned()));
    headers.push(("Idempotency-Key", "repo-lifecycle-edit".to_owned()));
    let (status, body) = request(
        daemon.endpoint(),
        "POST",
        &format!("/v1/projects/{project}/repository/edit"),
        &headers,
        &edit_input.to_string(),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let edit: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(edit["status"], "waiting_approval");
    let edit_id = edit["id"].as_str().unwrap();
    let mut headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "repo-lifecycle-edit-approval",
    );
    headers.push(("Content-Type", "application/json".to_owned()));
    headers.push(("Idempotency-Key", "repo-lifecycle-edit-approval".to_owned()));
    let (status, body) = request(
        daemon.endpoint(),
        "POST",
        &format!("/v1/repository-results/{edit_id}/approval"),
        &headers,
        r#"{"decision":"approved"}"#,
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let applied = wait_repository_result(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        edit_id,
        "repo-lifecycle-edit-result",
    )
    .await;
    assert_eq!(applied["status"], "completed", "{applied}");
    let applied_diff = applied["output"]["data"]["change_diff"].as_str().unwrap();
    assert_eq!(preview_diff.as_bytes(), applied_diff.as_bytes());
    assert_eq!(
        fs::read_to_string(project_root.join("src/lib.rs")).unwrap(),
        "fn f() { let value = Ok(1); }\n"
    );

    let mut headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "repo-lifecycle-replay",
    );
    headers.push(("Content-Type", "application/json".to_owned()));
    headers.push(("Idempotency-Key", "repo-lifecycle-replay".to_owned()));
    let (status, body) = request(
        daemon.endpoint(),
        "POST",
        &format!("/v1/projects/{project}/repository/edit"),
        &headers,
        &edit_input.to_string(),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let replay: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(replay["status"], "waiting_approval");
    let replay_id = replay["id"].as_str().unwrap();
    let mut headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "repo-lifecycle-replay-approval",
    );
    headers.push(("Content-Type", "application/json".to_owned()));
    headers.push((
        "Idempotency-Key",
        "repo-lifecycle-replay-approval".to_owned(),
    ));
    let (status, body) = request(
        daemon.endpoint(),
        "POST",
        &format!("/v1/repository-results/{replay_id}/approval"),
        &headers,
        r#"{"decision":"approved"}"#,
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let replay = wait_repository_result(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        replay_id,
        "repo-lifecycle-replay-result",
    )
    .await;
    assert_eq!(replay["status"], "failed", "{replay}");
    assert_eq!(replay["error"]["code"], "structural_preview_invalid");

    daemon.shutdown().await.unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn authenticated_daemon_http_drives_native_pty_and_reads_retained_output_after_restart() {
    let root = TestRoot::new();
    let daemon = start_daemon(development_config(&root.0)).await.unwrap();
    wait_ready(daemon.endpoint()).await;
    let project_id = daemon.project_id();
    let principal_id = daemon.principal_id();
    let process_id = ProcessId::generate().unwrap();
    let claim = ProcessClaim::new(
        process_id,
        ProcessOwnership::Attempt(AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal_id,
            FencingToken::new(1),
        )),
    );
    let boundary = PersistedBoundary {
        ownership: Ownership::new("daemon-http-pty", process_id.to_string()).unwrap(),
        identity: BoundaryIdentity::new(
            BoundaryKind::Container,
            process_id.to_string(),
            "daemon-http-pty-owner",
            "daemon-http-pty-runtime",
        )
        .unwrap(),
    };
    let context = ProcessRegistrationContext {
        project_id,
        principal_id,
    };
    let registration = daemon
        .executor_runtime_services()
        .process_registration(context);
    let terminal = ProcessTerminalConfig {
        request: TerminalRequest::pty(CapturePersistencePolicy::no_secrets()),
        size: TerminalSize::new(80, 24).unwrap(),
        retention: OutputRetention::new(16 * 1024, 60_000),
    };
    registration
        .registry
        .prepared(context, claim, &boundary, terminal)
        .unwrap();

    let mut command = ProcessCommand::new("/bin/sh");
    command.arg("-c").arg(
        "IFS= read -r line; size=$(stty size); printf 'received:%s size:%s\\n' \"$line\" \"$size\"",
    );
    let mut pty_output = registration
        .registry
        .bind_terminal(context, process_id, &mut command)
        .unwrap();
    let mut child = command.spawn().unwrap();

    let discovery = read_discovery(&root.0).unwrap();
    let (terminal_id, viewer) = tokio::task::spawn_blocking(move || {
        let mut client = HttpClient::connect(&discovery, Duration::from_secs(5)).unwrap();
        let allocation = execute_exec(
            &mut client,
            ExecRequest::allocate_terminal(
                process_id,
                AllocateTerminalBody {
                    columns: 80,
                    rows: 24,
                    max_output_bytes: 16 * 1024,
                    max_output_age_millis: 60_000,
                },
                IdempotencyKey::parse("daemon-http-pty-allocate").unwrap(),
            ),
        )
        .unwrap();
        let terminal_id =
            TerminalId::parse(allocation["resource"]["terminal_id"].as_str().unwrap()).unwrap();
        let viewer = execute_exec(
            &mut client,
            ExecRequest::attach_viewer(
                terminal_id,
                IdempotencyKey::parse("daemon-http-pty-viewer").unwrap(),
            ),
        )
        .unwrap()["resource"]["attachment_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let writer = execute_exec(
            &mut client,
            ExecRequest::claim_writer(
                terminal_id,
                60_000,
                IdempotencyKey::parse("daemon-http-pty-writer").unwrap(),
            ),
        )
        .unwrap()["resource"]["attachment_id"]
            .as_str()
            .unwrap()
            .to_owned();
        execute_exec(
            &mut client,
            ExecRequest::resize(
                &writer,
                100,
                40,
                IdempotencyKey::parse("daemon-http-pty-resize").unwrap(),
            ),
        )
        .unwrap();
        execute_exec(
            &mut client,
            ExecRequest::write_input(
                &writer,
                b"hello daemon\r",
                IdempotencyKey::parse("daemon-http-pty-input").unwrap(),
            ),
        )
        .unwrap();
        (terminal_id, viewer)
    })
    .await
    .unwrap();
    let child_deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    let mut buffer = [0_u8; 1024];
    let status = loop {
        match pty_output.read(&mut buffer) {
            Ok(0) => {}
            Ok(length) => output.extend_from_slice(&buffer[..length]),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
            Err(error) => panic!("read native PTY output: {error}"),
        }
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= child_deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("PTY child timed out: {}", String::from_utf8_lossy(&output));
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success());

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match pty_output.read(&mut buffer) {
            Ok(0) => break,
            Ok(length) => output.extend_from_slice(&buffer[..length]),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "PTY output timed out");
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) => panic!("read native PTY output: {error}"),
        }
        if String::from_utf8_lossy(&output).contains("received:hello daemon size:40 100") {
            break;
        }
    }
    assert!(
        String::from_utf8_lossy(&output).contains("received:hello daemon size:40 100"),
        "{}",
        String::from_utf8_lossy(&output)
    );
    let capture = CaptureRedactor::new(&[]).sanitize(CaptureBoundary::TerminalMetadata, &output);
    registration
        .registry
        .append_terminal_output(context, process_id, &capture)
        .unwrap();
    registration
        .registry
        .close_terminal(context, process_id)
        .unwrap();

    let discovery = read_discovery(&root.0).unwrap();
    let page = tokio::task::spawn_blocking(move || {
        let mut client = HttpClient::connect(&discovery, Duration::from_secs(5)).unwrap();
        execute_exec(
            &mut client,
            ExecRequest::read_output(&viewer, "output_0000000000000001"),
        )
        .unwrap()
    })
    .await
    .unwrap();
    assert_eq!(page["gap"], false);
    assert!(!page["chunks"].as_array().unwrap().is_empty());
    daemon.shutdown().await.unwrap();

    let restarted = start_daemon(development_config(&root.0)).await.unwrap();
    wait_ready(restarted.endpoint()).await;
    let discovery = read_discovery(&root.0).unwrap();
    let retained = tokio::task::spawn_blocking(move || {
        let mut client = HttpClient::connect(&discovery, Duration::from_secs(5)).unwrap();
        let replacement = execute_exec(
            &mut client,
            ExecRequest::attach_viewer(
                terminal_id,
                IdempotencyKey::parse("daemon-http-pty-replacement").unwrap(),
            ),
        )
        .unwrap()["resource"]["attachment_id"]
            .as_str()
            .unwrap()
            .to_owned();
        execute_exec(
            &mut client,
            ExecRequest::read_output(&replacement, "output_0000000000000001"),
        )
        .unwrap()
    })
    .await
    .unwrap();
    let retained = retained["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|chunk| chunk["bytes"].as_array().unwrap())
        .map(|byte| byte.as_u64().unwrap() as u8)
        .collect::<Vec<_>>();
    assert!(String::from_utf8_lossy(&retained).contains("received:hello daemon size:40 100"));
    restarted.shutdown().await.unwrap();
}

#[cfg(unix)]
fn spawn_scenario_process(root: &std::path::Path, scenario: &str) -> ChildGuard {
    prepare_subprocess_project(root);
    let _ = fs::remove_file(root.join(DISCOVERY_FILE));
    let mut command = ProcessCommand::new(env!("CARGO_BIN_EXE_kit"));
    command
        .args(["daemon", "--state-root"])
        .arg(root)
        .env("KIT_PROVIDER", "deterministic-test")
        .env("KIT_FAKE_PROVIDER", "openai")
        .env("KIT_FAKE_SCENARIO", scenario)
        .env("KIT_PROJECT_ROOT", root.join("unconfigured-project"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if scenario == "tool" {
        command.env("KIT_FAKE_DELAY_MS", "500");
    } else if scenario == "tool-barrier" {
        command
            .env("KIT_FAKE_BARRIER_ROOT", root.join("fake-provider-barrier"))
            .env("KIT_FAKE_BARRIER_AT", "after_tool_outcome");
    }
    let child = command.spawn().unwrap();
    ChildGuard(child)
}

#[cfg(unix)]
fn spawn_barrier_process(
    root: &std::path::Path,
    barrier: &std::path::Path,
    checkpoint: &str,
) -> ChildGuard {
    prepare_subprocess_project(root);
    let _ = fs::remove_file(root.join(DISCOVERY_FILE));
    ChildGuard(
        ProcessCommand::new(env!("CARGO_BIN_EXE_kit"))
            .args(["daemon", "--state-root"])
            .arg(root)
            .env("KIT_PROVIDER", "deterministic-test")
            .env("KIT_FAKE_PROVIDER", "openai")
            .env("KIT_FAKE_SCENARIO", "barrier")
            .env("KIT_PROJECT_ROOT", root.join("unconfigured-project"))
            .env("KIT_FAKE_BARRIER_ROOT", barrier)
            .env("KIT_FAKE_BARRIER_AT", checkpoint)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[cfg(unix)]
fn prepare_subprocess_project(root: &std::path::Path) {
    let project = root.join("unconfigured-project");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("README.md"),
        "deterministic native workspace\n",
    )
    .unwrap();
    init_git(&project);
}

#[cfg(unix)]
fn sql_count(database: &std::path::Path, sql: &str) -> i64 {
    let connection = rusqlite::Connection::open(database).unwrap();
    connection.busy_timeout(Duration::from_secs(1)).unwrap();
    connection.query_row(sql, [], |row| row.get(0)).unwrap()
}

#[cfg(unix)]
fn event_count(database: &std::path::Path, event_type: &str) -> i64 {
    rusqlite::Connection::open(database)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM events WHERE event_type = ?1",
            [event_type],
            |row| row.get(0),
        )
        .unwrap()
}

#[cfg(unix)]
fn observation_count(barrier: &std::path::Path, kind: &str) -> usize {
    fs::read_dir(barrier.join(kind)).map_or(0, |entries| entries.count())
}

#[cfg(unix)]
fn first_observation(barrier: &std::path::Path, kind: &str) -> Vec<u8> {
    let path = fs::read_dir(barrier.join(kind))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::read(path).unwrap()
}

#[cfg(unix)]
async fn wait_barrier(barrier: &std::path::Path, process: &mut ChildGuard, checkpoint: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        process.assert_alive();
        if fs::read_to_string(barrier.join("reached")).is_ok_and(|value| value == checkpoint) {
            return;
        }
        assert!(Instant::now() < deadline, "barrier {checkpoint} timed out");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[cfg(unix)]
fn assert_stale_stream_fence(database: &std::path::Path, correlation: EffectCorrelation) {
    let mut request = agentkit_loop::TurnRequest {
        session_id: agentkit_core::SessionId::new("stale"),
        turn_id: agentkit_core::TurnId::new("stale"),
        transcript: Vec::new(),
        available_tools: Vec::new(),
        cache: None,
        structured_output: None,
        generation: Default::default(),
        metadata: agentkit_core::MetadataMap::new(),
    };
    request.metadata.insert(
        kit::agent::driver::restart::EFFECT_CORRELATION_METADATA.into(),
        serde_json::to_value(correlation).unwrap(),
    );
    let factory = SqliteStreamCommitFactory::open(database, StreamLimits::default()).unwrap();
    let mut commit = factory.for_request(&request).unwrap();
    let error = commit
        .commit_chunk(
            1,
            &agentkit_loop::ModelTurnEvent::Usage(agentkit_core::Usage::new(
                agentkit_core::TokenUsage::new(0, 0),
            )),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("stale provider stream fence"),
        "{error}"
    );
}

#[cfg(unix)]
async fn start_subprocess_run(
    root: &std::path::Path,
    process: &mut ChildGuard,
) -> (
    kit::cli::core::DaemonDiscovery,
    ProjectId,
    PrincipalId,
    RunId,
) {
    wait_for_subprocess_discovery(root, &mut process.0);
    let discovery = read_discovery(root).unwrap();
    let address = discovery
        .endpoint
        .strip_prefix("http://")
        .unwrap()
        .parse()
        .unwrap();
    wait_ready(address).await;
    let identity: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("daemon-identity.json")).unwrap()).unwrap();
    let project: ProjectId = identity["project_id"].as_str().unwrap().parse().unwrap();
    let principal: PrincipalId = identity["principal_id"].as_str().unwrap().parse().unwrap();
    let command_headers = |nonce: &str, key: &str| {
        let mut headers = authenticated_headers(&discovery.credential, &discovery.endpoint, nonce);
        headers.push(("Content-Type", "application/json".to_owned()));
        headers.push(("Idempotency-Key", key.to_owned()));
        headers
    };
    let (status, body) = request(
        address,
        "POST",
        "/v1/projects",
        &command_headers("kill-project", "kill-project"),
        &format!(r#"{{"id":"{project}"}}"#),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let thread = ThreadId::generate().unwrap();
    let (status, body) = request(
        address,
        "POST",
        &format!("/v1/projects/{project}/threads"),
        &command_headers("kill-thread", "kill-thread"),
        &format!(r#"{{"id":"{thread}"}}"#),
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let run = RunId::generate().unwrap();
    let (status, body) = request(
        address,
        "POST",
        &format!("/v1/threads/{thread}/runs"),
        &command_headers("kill-run", "kill-run"),
        &format!(r#"{{"id":"{run}","message":"kill matrix"}}"#),
    )
    .await;
    assert_eq!(status, 202, "{body}");
    (discovery, project, principal, run)
}

fn authenticated_headers(
    credential: &str,
    origin: &str,
    nonce: &str,
) -> Vec<(&'static str, String)> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let key = Sha256::digest(credential.as_bytes());
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).unwrap();
    mac.update(b"KIT-LOOPBACK-REQUEST-V1\0");
    mac.update(&timestamp.to_be_bytes());
    mac.update(nonce.as_bytes());
    let signature = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    vec![
        ("Authorization", format!("Bearer {credential}")),
        ("Origin", origin.to_owned()),
        ("X-Kit-Nonce", nonce.to_owned()),
        ("X-Kit-Timestamp", timestamp.to_string()),
        ("X-Kit-Signature", signature),
    ]
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn daemon_serves_shuts_down_and_restarts_from_the_same_state_root() {
    let root = TestRoot::new();
    let daemon = start_daemon(development_config(&root.0)).await.unwrap();
    let address = daemon.endpoint();
    let project = daemon.project_id();
    wait_ready(address).await;
    assert!(daemon.health().is_live());
    assert!(daemon.health().is_ready());

    let discovery = read_discovery(&root.0).unwrap();
    #[cfg(unix)]
    {
        assert_eq!(mode(&root.0), 0o700);
        assert_eq!(mode(&root.0.join(DISCOVERY_FILE)), 0o600);
    }
    assert_eq!(discovery.endpoint, format!("http://{address}"));

    let (status, _) = request(address, "GET", &format!("/v1/projects/{project}"), &[], "").await;
    assert_eq!(status, 401);

    let headers = authenticated_headers(&discovery.credential, &discovery.endpoint, "create");
    let mut create_headers = headers;
    create_headers.push(("Content-Type", "application/json".to_owned()));
    create_headers.push(("Idempotency-Key", "daemon-create-project".to_owned()));
    let (status, _) = request(
        address,
        "POST",
        "/v1/projects",
        &create_headers,
        &format!(r#"{{"id":"{project}"}}"#),
    )
    .await;
    assert_eq!(status, 201);

    let thread = ThreadId::generate().unwrap();
    let mut headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "create-deletion-thread",
    );
    headers.push(("Content-Type", "application/json".to_owned()));
    headers.push(("Idempotency-Key", "create-deletion-thread".to_owned()));
    let (status, _) = request(
        address,
        "POST",
        &format!("/v1/projects/{project}/threads"),
        &headers,
        &format!(r#"{{"id":"{thread}"}}"#),
    )
    .await;
    assert_eq!(status, 201);
    let mut headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "request-durable-deletion",
    );
    headers.push(("Content-Type", "application/json".to_owned()));
    headers.push(("Idempotency-Key", "request-durable-deletion".to_owned()));
    let (status, body) = request(
        address,
        "POST",
        &format!("/v1/threads/{thread}/deletion"),
        &headers,
        "{}",
    )
    .await;
    assert_eq!(status, 202, "{body}");
    let deletion_body = serde_json::from_str::<serde_json::Value>(&body).unwrap();
    let deletion_job = deletion_body["id"]
        .as_str()
        .unwrap_or_else(|| panic!("deletion response lacked job id: {deletion_body}"))
        .to_owned();

    assert!(matches!(
        start_daemon(development_config(&root.0)).await,
        Err(DaemonError::StateRootLock(
            StateRootLockError::AlreadyLocked { .. }
        ))
    ));

    let shutdown = daemon.shutdown_handle();
    shutdown.request();
    assert!(!daemon.health().is_ready());
    daemon.shutdown().await.unwrap();
    assert!(!root.0.join(DISCOVERY_FILE).exists());

    let restarted = start_daemon(development_config(&root.0)).await.unwrap();
    assert_eq!(restarted.project_id(), project);
    let address = restarted.endpoint();
    wait_ready(address).await;
    let discovery = read_discovery(&root.0).unwrap();
    let headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "read-after-restart",
    );
    let (status, body) = request(
        address,
        "GET",
        &format!("/v1/projects/{project}"),
        &headers,
        "",
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains(&project.to_string()));
    let headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "deletion-after-restart",
    );
    let (status, body) = request(
        address,
        "GET",
        &format!("/v1/deletion-jobs/{deletion_job}"),
        &headers,
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"],
        deletion_job
    );
    restarted.shutdown().await.unwrap();
    assert!(!root.0.join(DISCOVERY_FILE).exists());
}

#[tokio::test]
async fn daemon_provider_selection_fails_closed() {
    let missing = TestRoot::new();
    let mut config = development_config(&missing.0);
    config.model_adapter = None;
    assert!(
        setup_error(start_daemon(config).await).contains("model adapter unavailable"),
        "unset provider selection did not fail closed"
    );

    let newer = TestRoot::new();
    let mut config = development_config(&newer.0);
    let mut layer = ExtensionConfigLayer::empty();
    layer.schema_version += 1;
    config.model_adapter.as_mut().unwrap().extensions.run = Some(layer);
    assert!(
        setup_error(start_daemon(config).await).contains("unsupported Run extension configuration"),
        "newer model adapter configuration did not fail closed"
    );

    let untrusted = TestRoot::new();
    let mut config = development_config(&untrusted.0);
    let mut layer = ExtensionConfigLayer::empty();
    layer.select(
        ExtensionPoint::ModelAdapter,
        ExtensionReference::new(
            ExtensionIdentity::parse("third_party.provider").unwrap(),
            ExtensionVersion::parse("1.0.0").unwrap(),
        ),
    );
    config.model_adapter.as_mut().unwrap().extensions.run = Some(layer);
    assert!(
        setup_error(start_daemon(config).await).contains("unknown extension selection"),
        "untrusted provider selection did not fail closed"
    );

    for implementation in [false, true] {
        let drift = TestRoot::new();
        let mut config = development_config(&drift.0);
        let provider = config.model_adapter.as_mut().unwrap();
        if implementation {
            provider.implementation_digest = ContentDigest::sha256(b"drifted implementation");
        } else {
            provider.schema_digest = ContentDigest::sha256(b"drifted schema");
        }
        let error = setup_error(start_daemon(config).await);
        assert!(
            error.contains(if implementation {
                "implementation drifted"
            } else {
                "schema drifted"
            }),
            "provider digest drift did not fail closed: {error}"
        );
    }
}

#[tokio::test]
async fn daemon_rejects_missing_artifact_references_and_accepts_published_bytes() {
    let root = TestRoot::new();
    let daemon = start_daemon(development_config(&root.0)).await.unwrap();
    let address = daemon.endpoint();
    let principal = daemon.principal_id();
    let project = daemon.project_id();
    wait_ready(address).await;
    let discovery = read_discovery(&root.0).unwrap();

    let command_headers = |nonce: &str, key: &str| {
        let mut headers = authenticated_headers(&discovery.credential, &discovery.endpoint, nonce);
        headers.push(("Content-Type", "application/json".to_owned()));
        headers.push(("Idempotency-Key", key.to_owned()));
        headers
    };
    let (status, _) = request(
        address,
        "POST",
        "/v1/projects",
        &command_headers("artifact-project", "artifact-project"),
        &format!(r#"{{"id":"{project}"}}"#),
    )
    .await;
    assert_eq!(status, 201);

    let thread = ThreadId::generate().unwrap();
    let (status, _) = request(
        address,
        "POST",
        &format!("/v1/projects/{project}/threads"),
        &command_headers("artifact-thread", "artifact-thread"),
        &format!(r#"{{"id":"{thread}"}}"#),
    )
    .await;
    assert_eq!(status, 201);

    let missing = format!("blake3:{}", "0".repeat(64));
    let missing_record = ArtifactId::generate().unwrap();
    let (status, body) = request(
        address,
        "POST",
        &format!("/v1/projects/{project}/artifacts"),
        &command_headers("artifact-missing-metadata", "artifact-missing-metadata"),
        &format!(
            r#"{{"id":"{missing_record}","reference":"{missing}","media_type":"text/plain","size":0}}"#
        ),
    )
    .await;
    assert_eq!(status, 400, "{body}");

    let missing_run = RunId::generate().unwrap();
    let (status, _) = request(
        address,
        "POST",
        &format!("/v1/threads/{thread}/runs"),
        &command_headers("artifact-missing-run", "artifact-missing-run"),
        &format!(r#"{{"id":"{missing_run}","input":"{missing}"}}"#),
    )
    .await;
    assert_eq!(status, 400);

    let artifact_store = ArtifactStore::open(root.0.join("artifacts")).unwrap();
    let artifact = artifact_store
        .put(
            b"verified daemon input",
            ArtifactMetadata::new(
                "text/plain",
                ArtifactClass::File,
                principal.to_string(),
                project.to_string(),
                ArtifactRetention::Forever,
                now_unix_micros().unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let artifact_record = ArtifactId::generate().unwrap();
    let (status, _) = request(
        address,
        "POST",
        &format!("/v1/projects/{project}/artifacts"),
        &command_headers("artifact-valid-metadata", "artifact-valid-metadata"),
        &format!(
            r#"{{"id":"{artifact_record}","reference":"{}","media_type":"text/plain","size":21}}"#,
            artifact.digest()
        ),
    )
    .await;
    assert_eq!(status, 201);

    let mismatched_record = ArtifactId::generate().unwrap();
    let (status, _) = request(
        address,
        "POST",
        &format!("/v1/projects/{project}/artifacts"),
        &command_headers(
            "artifact-mismatched-metadata",
            "artifact-mismatched-metadata",
        ),
        &format!(
            r#"{{"id":"{mismatched_record}","reference":"{}","media_type":"text/plain","size":20}}"#,
            artifact.digest()
        ),
    )
    .await;
    assert_eq!(status, 400);

    let foreign = artifact_store
        .put(
            b"foreign daemon input",
            ArtifactMetadata::new(
                "text/plain",
                ArtifactClass::File,
                kit::domain::ids::PrincipalId::generate()
                    .unwrap()
                    .to_string(),
                project.to_string(),
                ArtifactRetention::Forever,
                now_unix_micros().unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let foreign_run = RunId::generate().unwrap();
    let (status, _) = request(
        address,
        "POST",
        &format!("/v1/threads/{thread}/runs"),
        &command_headers("artifact-foreign-run", "artifact-foreign-run"),
        &format!(r#"{{"id":"{foreign_run}","input":"{}"}}"#, foreign.digest()),
    )
    .await;
    assert_eq!(status, 400);

    let valid_run = RunId::generate().unwrap();
    let (status, _) = request(
        address,
        "POST",
        &format!("/v1/threads/{thread}/runs"),
        &command_headers("artifact-valid-run", "artifact-valid-run"),
        &format!(r#"{{"id":"{valid_run}","input":"{}"}}"#, artifact.digest()),
    )
    .await;
    assert_eq!(status, 202);

    let headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "artifact-valid-read",
    );
    let (status, body) = request(
        address,
        "GET",
        &format!("/v1/runs/{valid_run}"),
        &headers,
        "",
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains(&artifact.digest().to_string()));

    daemon.shutdown().await.unwrap();
    let events = kit::test_support::open_service_store(root.0.join("state.sqlite3"))
        .unwrap()
        .append_store()
        .events()
        .unwrap();
    assert_eq!(events.len(), 4);
    let expected = serde_json::to_vec(&[artifact.digest().to_string()]).unwrap();
    assert_eq!(events[2].event.artifacts, expected);
    assert_eq!(events[3].event.artifacts, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn artifact_published_after_daemon_start_is_immediately_restorable() {
    let root = TestRoot::new();
    let state = root.0.join("state");
    let backups = root.0.join("backups");
    let mut config = development_config(&state);
    config.backup_destination = backups.clone();
    let daemon = start_daemon(config.clone()).await.unwrap();
    let address = daemon.endpoint();
    let principal = daemon.principal_id();
    let project = daemon.project_id();
    wait_ready(address).await;
    let discovery = read_discovery(&state).unwrap();
    let command_headers = |nonce: &str, key: &str| {
        let mut headers = authenticated_headers(&discovery.credential, &discovery.endpoint, nonce);
        headers.push(("Content-Type", "application/json".to_owned()));
        headers.push(("Idempotency-Key", key.to_owned()));
        headers
    };
    let (status, _) = request(
        address,
        "POST",
        "/v1/projects",
        &command_headers("backup-project", "backup-project"),
        &format!(r#"{{"id":"{project}"}}"#),
    )
    .await;
    assert_eq!(status, 201);

    let artifact_store = ArtifactStore::open(state.join("artifacts")).unwrap();
    let artifact = artifact_store
        .put(
            b"published after daemon startup",
            ArtifactMetadata::new(
                "text/plain",
                ArtifactClass::Report,
                principal.to_string(),
                project.to_string(),
                ArtifactRetention::Forever,
                now_unix_micros().unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let artifact_record = ArtifactId::generate().unwrap();
    let (status, body) = request(
        address,
        "POST",
        &format!("/v1/projects/{project}/artifacts"),
        &command_headers("backup-artifact", "backup-artifact"),
        &format!(
            r#"{{"id":"{artifact_record}","reference":"{}","media_type":"text/plain","size":30}}"#,
            artifact.digest()
        ),
    )
    .await;
    assert_eq!(status, 201, "{body}");

    let generation = daemon.trigger_backup().unwrap();
    let manager = test_support::open_backup_manager(BackupConfig {
        state_root: state.clone(),
        database_path: state.join("state.sqlite3"),
        artifact_root: state.join("artifacts"),
        destination: backups,
        retain_generations: config.backup_retain_generations,
        backup_expires_at_unix_micros: i64::MAX,
        build_version: env!("CARGO_PKG_VERSION").to_owned(),
    })
    .unwrap();
    let restored = root.0.join("restored");
    let report = manager
        .restore(
            &generation.name,
            &restored,
            now_unix_micros().unwrap(),
            env!("CARGO_PKG_VERSION"),
        )
        .unwrap();
    assert_eq!(report.artifact_count, 1);
    assert_eq!(
        ArtifactStore::open(restored.join("artifacts"))
            .unwrap()
            .open_bytes(artifact.digest())
            .unwrap(),
        b"published after daemon startup"
    );

    let racing_artifact = artifact_store
        .put(
            b"racing artifact",
            ArtifactMetadata::new(
                "text/plain",
                ArtifactClass::Report,
                principal.to_string(),
                project.to_string(),
                ArtifactRetention::Forever,
                now_unix_micros().unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let racing_record = ArtifactId::generate().unwrap();
    let racing_path = format!("/v1/projects/{project}/artifacts");
    let racing_headers = command_headers("racing-artifact", "racing-artifact");
    let racing_body = format!(
        r#"{{"id":"{racing_record}","reference":"{}","media_type":"text/plain","size":15}}"#,
        racing_artifact.digest()
    );
    let publish = request(address, "POST", &racing_path, &racing_headers, &racing_body);
    let backup = async { tokio::task::block_in_place(|| daemon.trigger_backup()) };
    let ((status, body), racing_generation) = tokio::join!(publish, backup);
    assert_eq!(status, 201, "{body}");
    let racing_generation = racing_generation.unwrap();
    let racing_restored = root.0.join("racing-restored");
    manager
        .restore(
            &racing_generation.name,
            &racing_restored,
            now_unix_micros().unwrap(),
            env!("CARGO_PKG_VERSION"),
        )
        .unwrap();
    let included = ArtifactStore::open(racing_restored.join("artifacts"))
        .unwrap()
        .open_bytes(racing_artifact.digest())
        .is_ok();
    assert!(
        included,
        "opaque artifact references are backed up independently"
    );

    daemon.shutdown().await.unwrap();
    let restarted = start_daemon(config).await.unwrap();
    let restarted_generation = restarted.trigger_backup().unwrap();
    let restarted_restore = root.0.join("restarted-restored");
    manager
        .restore(
            &restarted_generation.name,
            &restarted_restore,
            now_unix_micros().unwrap(),
            env!("CARGO_PKG_VERSION"),
        )
        .unwrap();
    assert_eq!(
        ArtifactStore::open(restarted_restore.join("artifacts"))
            .unwrap()
            .open_bytes(racing_artifact.digest())
            .unwrap(),
        b"racing artifact"
    );
    restarted.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_daemon_reaches_tool_input_approval_and_auth_boundaries() {
    let root = TestRoot::new();
    let (daemon, discovery, _, run) = start_scenario_run(&root, FakeScenario::Tool).await;
    wait_run_state(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        run,
        "completed",
    )
    .await;
    daemon.shutdown().await.unwrap();
    let events = test_support::open_service_store(root.0.join("state.sqlite3"))
        .unwrap()
        .append_store()
        .events()
        .unwrap();
    let journal = serde_json::to_string(
        &events
            .iter()
            .map(|event| String::from_utf8_lossy(&event.event.payload))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(journal.contains("kit_discover"));
    assert!(journal.contains("after_tool_outcome"));

    let root = TestRoot::new();
    let (daemon, discovery, _, run) = start_scenario_run(&root, FakeScenario::Input).await;
    let waiting = wait_run_state(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        run,
        "waiting_for_input",
    )
    .await;
    let artifact = ArtifactStore::open(root.0.join("artifacts"))
        .unwrap()
        .put(
            b"daemon resumed input",
            ArtifactMetadata::new(
                "text/plain; charset=utf-8",
                ArtifactClass::File,
                daemon.principal_id().to_string(),
                daemon.project_id().to_string(),
                ArtifactRetention::Forever,
                now_unix_micros().unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let mut headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "scenario-input-resolution",
    );
    headers.push(("Content-Type", "application/json".to_owned()));
    headers.push(("Idempotency-Key", "scenario-input-resolution".to_owned()));
    let (status, body) = request(
        daemon.endpoint(),
        "POST",
        &format!("/v1/runs/{run}/input"),
        &headers,
        &format!(
            r#"{{"input":"{}","expected_version":{}}}"#,
            artifact.digest(),
            waiting["version"]
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    wait_run_state(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        run,
        "completed",
    )
    .await;
    daemon.shutdown().await.unwrap();

    let root = TestRoot::new();
    let (daemon, discovery, _, run) = start_scenario_run(&root, FakeScenario::Approval).await;
    wait_run_state(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        run,
        "waiting_for_approval",
    )
    .await;
    let headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "scenario-approval-list",
    );
    let (status, body) = request(
        daemon.endpoint(),
        "GET",
        &format!("/v1/projects/{}/approvals", daemon.project_id()),
        &headers,
        "",
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let approval: serde_json::Value = serde_json::from_str(&body).unwrap();
    let approval = &approval["items"][0];
    let mut headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "scenario-approval-resolution",
    );
    headers.push(("Content-Type", "application/json".to_owned()));
    headers.push(("Idempotency-Key", "scenario-approval-resolution".to_owned()));
    let (status, body) = request(
        daemon.endpoint(),
        "POST",
        &format!("/v1/approvals/{}/resolve", approval["id"].as_str().unwrap()),
        &headers,
        &format!(
            r#"{{"decision":"approved","expected_version":{}}}"#,
            approval["version"]
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    wait_run_state(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        run,
        "completed",
    )
    .await;
    daemon.shutdown().await.unwrap();

    let root = TestRoot::new();
    let (daemon, discovery, _, run) = start_scenario_run(
        &root,
        FakeScenario::Auth {
            scope: "provider.read".to_owned(),
        },
    )
    .await;
    let waiting = wait_run_state(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        run,
        "waiting_for_auth",
    )
    .await;
    let mut headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "scenario-auth-resolution",
    );
    headers.push(("Content-Type", "application/json".to_owned()));
    headers.push(("Idempotency-Key", "scenario-auth-resolution".to_owned()));
    let (status, body) = request(
        daemon.endpoint(),
        "POST",
        &format!("/v1/runs/{run}/auth/resolve"),
        &headers,
        &format!(
            r#"{{"granted":true,"expected_version":{}}}"#,
            waiting["version"]
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    wait_run_state(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        run,
        "completed",
    )
    .await;
    daemon.shutdown().await.unwrap();
}

/// Rewrites every persisted capability-extension registry snapshot so the
/// built-in native provider carries a stale implementation digest, simulating
/// a state root produced by a differently built kit binary. Returns the
/// (genuine, stale) digest pair.
fn tamper_native_extension_digest(database: &std::path::Path) -> (String, String) {
    let genuine = kit::capabilities::extensions::built_in_contracts()
        .into_iter()
        .find(|contract| {
            contract.kind() == kit::capabilities::extensions::ExtensionKind::NativeProvider
        })
        .unwrap()
        .implementation_digest()
        .to_string();
    let stale = format!("sha256:{}", "ab".repeat(32));
    let connection = rusqlite::Connection::open(database).unwrap();
    connection.busy_timeout(Duration::from_secs(1)).unwrap();
    let snapshots = {
        let mut statement = connection
            .prepare("SELECT principal_id, project_id, snapshot FROM capability_extension_registry")
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let mut tampered_any = false;
    for (principal, project, snapshot) in snapshots {
        let text = String::from_utf8(snapshot).unwrap();
        if !text.contains(&genuine) {
            continue;
        }
        tampered_any = true;
        connection
            .execute(
                "UPDATE capability_extension_registry SET snapshot=?1
                 WHERE principal_id=?2 AND project_id=?3",
                rusqlite::params![text.replace(&genuine, &stale).into_bytes(), principal, project],
            )
            .unwrap();
    }
    assert!(tampered_any, "boot registered the built-in native contract");
    (genuine, stale)
}

fn stored_extension_snapshots(database: &std::path::Path) -> String {
    let connection = rusqlite::Connection::open(database).unwrap();
    connection.busy_timeout(Duration::from_secs(1)).unwrap();
    let mut statement = connection
        .prepare("SELECT snapshot FROM capability_extension_registry")
        .unwrap();
    let rows = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows.into_iter()
        .map(|snapshot| String::from_utf8(snapshot).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_boots_cleanly_after_built_in_extension_digest_change() {
    let root = TestRoot::new();
    let daemon = start_daemon(development_config(&root.0)).await.unwrap();
    wait_ready(daemon.endpoint()).await;
    daemon.shutdown().await.unwrap();

    let database = root.0.join("state.sqlite3");
    let (genuine, stale) = tamper_native_extension_digest(&database);
    assert!(stored_extension_snapshots(&database).contains(&stale));

    // A binary whose built-in digests differ from the stored contract must
    // supersede the entry and boot cleanly instead of exiting with
    // "extension contract conflicts with ...".
    let restarted = start_daemon(development_config(&root.0)).await.unwrap();
    wait_ready(restarted.endpoint()).await;
    restarted.shutdown().await.unwrap();

    let snapshots = stored_extension_snapshots(&database);
    assert!(snapshots.contains(&genuine));
    assert!(!snapshots.contains(&stale));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_run_resumes_after_built_in_extension_digest_change() {
    let root = TestRoot::new();
    let (daemon, discovery, _, run) = start_scenario_run(&root, FakeScenario::Input).await;
    wait_run_state(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        run,
        "waiting_for_input",
    )
    .await;
    daemon.shutdown().await.unwrap();

    // The run is parked on a durable input wait; simulate a kit upgrade that
    // changes the built-in native provider digest before the daemon restarts.
    let database = root.0.join("state.sqlite3");
    let (_, stale) = tamper_native_extension_digest(&database);

    let project = root.0.join("project");
    let daemon = start_daemon(
        DaemonConfig::new(&root.0)
            .with_development_provider(
                kit::domain::config::Provider::OpenAi,
                FakeResponse::completed("daemon scenario complete"),
                FakeScenario::Input,
            )
            .with_project_root(project),
    )
    .await
    .unwrap();
    wait_ready(daemon.endpoint()).await;
    let discovery = read_discovery(&root.0).unwrap();
    let waiting = wait_run_state(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        run,
        "waiting_for_input",
    )
    .await;
    let artifact = ArtifactStore::open(root.0.join("artifacts"))
        .unwrap()
        .put(
            b"resumed after digest change",
            ArtifactMetadata::new(
                "text/plain; charset=utf-8",
                ArtifactClass::File,
                daemon.principal_id().to_string(),
                daemon.project_id().to_string(),
                ArtifactRetention::Forever,
                now_unix_micros().unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let mut headers = authenticated_headers(
        &discovery.credential,
        &discovery.endpoint,
        "digest-change-input-resolution",
    );
    headers.push(("Content-Type", "application/json".to_owned()));
    headers.push(("Idempotency-Key", "digest-change-input-resolution".to_owned()));
    let (status, body) = request(
        daemon.endpoint(),
        "POST",
        &format!("/v1/runs/{run}/input"),
        &headers,
        &format!(
            r#"{{"input":"{}","expected_version":{}}}"#,
            artifact.digest(),
            waiting["version"]
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    // The resumed attempt re-mints its tool bindings from the live catalog
    // against the superseded (current-build) contract and completes.
    wait_run_state(
        daemon.endpoint(),
        &discovery.credential,
        &discovery.endpoint,
        run,
        "completed",
    )
    .await;
    daemon.shutdown().await.unwrap();
    assert!(!stored_extension_snapshots(&database).contains(&stale));
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigkill_model_provider_commit_windows_recover_exactly_once() {
    for checkpoint in [
        "before_provider_dispatch",
        "after_provider_dispatch",
        "after_first_stream_chunk",
        "after_stream_outcome",
        "after_model_outcome",
        "after_journal_boundary",
    ] {
        let root = TestRoot::new();
        let barrier = root.0.join("fake-provider-barrier");
        fs::create_dir_all(&barrier).unwrap();
        let database = root.0.join("state.sqlite3");
        let mut process = spawn_barrier_process(&root.0, &barrier, checkpoint);
        let (_, _, _, run) = start_subprocess_run(&root.0, &mut process).await;
        wait_barrier(&barrier, &mut process, checkpoint).await;

        assert_eq!(event_count(&database, "model_call.intent"), 1);
        assert_eq!(event_count(&database, "model_call.dispatched"), 1);
        assert_eq!(
            event_count(&database, "run.output"),
            i64::from(checkpoint == "after_journal_boundary")
        );
        assert_eq!(observation_count(&barrier, "request"), 1);
        let chunks = sql_count(&database, "SELECT count(*) FROM provider_stream_chunks");
        let stream_outcomes = sql_count(
            &database,
            "SELECT count(*) FROM provider_streams WHERE outcome IS NOT NULL",
        );
        let model_outcomes = event_count(&database, "model_call.outcome");
        let turn_ends = sql_count(
            &database,
            "SELECT count(*) FROM events WHERE event_type = 'agent.effect_journal' AND instr(CAST(payload AS TEXT), '\"boundary\":\"turn_end\"') > 0",
        );
        let expected = match checkpoint {
            "before_provider_dispatch" => (0, 0, 0, 0, 0, 0),
            "after_provider_dispatch" => (1, 0, 0, 0, 0, 0),
            "after_first_stream_chunk" => (1, 0, 1, 0, 0, 0),
            "after_stream_outcome" => (1, 1, 4, 1, 0, 0),
            "after_model_outcome" => (1, 1, 4, 1, 1, 0),
            "after_journal_boundary" => (1, 1, 4, 1, 1, 1),
            _ => unreachable!(),
        };
        assert_eq!(
            (
                observation_count(&barrier, "dispatch"),
                observation_count(&barrier, "result"),
                chunks,
                stream_outcomes,
                model_outcomes,
                turn_ends,
            ),
            expected,
            "unexpected durable prefix at {checkpoint}"
        );
        process.assert_alive();
        process.kill9();

        fs::write(barrier.join("release"), checkpoint).unwrap();
        let mut restarted = spawn_barrier_process(&root.0, &barrier, checkpoint);
        wait_for_subprocess_discovery(&root.0, &mut restarted.0);
        let discovery = read_discovery(&root.0).unwrap();
        let address = discovery
            .endpoint
            .strip_prefix("http://")
            .unwrap()
            .parse()
            .unwrap();
        wait_ready(address).await;
        let completed = matches!(checkpoint, "after_model_outcome" | "after_journal_boundary");
        wait_run_state(
            address,
            &discovery.credential,
            &discovery.endpoint,
            run,
            if completed { "completed" } else { "failed" },
        )
        .await;
        restarted.kill9();

        let (dispatches, results, chunks, stream_outcomes, progress) = match checkpoint {
            "before_provider_dispatch" => (0, 0, 0, 0, 0),
            "after_provider_dispatch" => (1, 0, 0, 0, 0),
            "after_first_stream_chunk" => (1, 0, 1, 0, 0),
            "after_stream_outcome" => (1, 1, 4, 1, 4),
            "after_model_outcome" | "after_journal_boundary" => (1, 1, 4, 1, 4),
            _ => unreachable!(),
        };
        assert_eq!(event_count(&database, "model_call.intent"), 1);
        assert_eq!(event_count(&database, "model_call.dispatched"), 1);
        assert_eq!(
            event_count(&database, "model_call.outcome"),
            i64::from(completed)
        );
        assert_eq!(event_count(&database, "run.output"), i64::from(completed));
        assert_eq!(event_count(&database, "run.progress"), progress);
        assert_eq!(observation_count(&barrier, "dispatch"), dispatches);
        assert_eq!(observation_count(&barrier, "result"), results);
        assert_eq!(
            sql_count(
                &database,
                "SELECT count(*) FROM provider_stream_chunks WHERE sequence BETWEEN 1 AND 4"
            ),
            chunks
        );
        assert_eq!(
            sql_count(
                &database,
                "SELECT count(DISTINCT sequence) FROM provider_stream_chunks",
            ),
            chunks
        );
        assert_eq!(
            sql_count(
                &database,
                "SELECT count(*) FROM provider_streams WHERE outcome IS NOT NULL",
            ),
            stream_outcomes
        );
        assert_eq!(
            sql_count(
                &database,
                "SELECT count(*) FROM events WHERE event_type = 'model_call.outcome' AND instr(CAST(payload AS TEXT), '\"status\":\"succeeded\"') > 0",
            ),
            i64::from(completed)
        );
        assert_eq!(
            sql_count(
                &database,
                "SELECT count(*) FROM scheduler_reservations WHERE kind = 'model'",
            ),
            1
        );
        assert_eq!(
            sql_count(
                &database,
                "SELECT count(*) FROM scheduler_reservations WHERE kind = 'model' AND state IN ('debited', 'reconciled')",
            ),
            1
        );

        let mut store = test_support::open_service_store(&database).unwrap();
        let QueryProjection::RunTranscript(transcript) =
            store.query(&Query::RunTranscript { run_id: run }).unwrap()
        else {
            panic!("unexpected transcript projection")
        };
        let expected_kinds = if completed {
            vec![
                "model_part_started",
                "model_text_delta",
                "model_part_committed",
                "model_usage",
                "assistant_final",
            ]
        } else if progress == 4 {
            vec![
                "model_part_started",
                "model_text_delta",
                "model_part_committed",
                "model_usage",
            ]
        } else {
            Vec::new()
        };
        assert_eq!(
            transcript
                .items
                .iter()
                .map(|item| item.sequence)
                .collect::<Vec<_>>(),
            (1..=expected_kinds.len() as u64).collect::<Vec<_>>()
        );
        assert_eq!(
            transcript
                .items
                .iter()
                .map(|item| item.kind.as_str())
                .collect::<Vec<_>>(),
            expected_kinds
        );
        let transcript_json = serde_json::to_string(&transcript).unwrap();
        assert_eq!(
            transcript_json.matches("hello from kit").count(),
            if completed {
                3
            } else if progress == 4 {
                2
            } else {
                0
            }
        );

        let correlation: Option<EffectCorrelation> =
            serde_json::from_slice(&first_observation(&barrier, "request")).unwrap();
        assert_stale_stream_fence(&database, correlation.unwrap());
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigkill_waiting_matrix_resumes_without_duplicate_effects_or_cost() {
    let mut boundaries = std::collections::BTreeSet::new();
    for (scenario, waiting_state, expected_models) in [
        ("input", "waiting_for_input", 2),
        ("approval", "waiting_for_approval", 2),
        ("auth", "waiting_for_auth", 1),
    ] {
        let root = TestRoot::new();
        let mut process = spawn_scenario_process(&root.0, scenario);
        let (discovery, project, principal, run) =
            start_subprocess_run(&root.0, &mut process).await;
        let address = discovery
            .endpoint
            .strip_prefix("http://")
            .unwrap()
            .parse()
            .unwrap();
        wait_run_state(
            address,
            &discovery.credential,
            &discovery.endpoint,
            run,
            waiting_state,
        )
        .await;
        process.kill9();

        let mut restarted = spawn_scenario_process(&root.0, scenario);
        wait_for_subprocess_discovery(&root.0, &mut restarted.0);
        let discovery = read_discovery(&root.0).unwrap();
        let address = discovery
            .endpoint
            .strip_prefix("http://")
            .unwrap()
            .parse()
            .unwrap();
        wait_ready(address).await;
        let waiting = wait_run_state(
            address,
            &discovery.credential,
            &discovery.endpoint,
            run,
            waiting_state,
        )
        .await;

        let mut headers = authenticated_headers(
            &discovery.credential,
            &discovery.endpoint,
            &format!("kill-resolve-{scenario}"),
        );
        headers.push(("Content-Type", "application/json".to_owned()));
        headers.push(("Idempotency-Key", format!("kill-resolve-{scenario}")));
        let (path, body) = match scenario {
            "input" => {
                let artifact = ArtifactStore::open(root.0.join("artifacts"))
                    .unwrap()
                    .put(
                        b"sigkill resumed input",
                        ArtifactMetadata::new(
                            "text/plain; charset=utf-8",
                            ArtifactClass::File,
                            principal.to_string(),
                            project.to_string(),
                            ArtifactRetention::Forever,
                            now_unix_micros().unwrap(),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                (
                    format!("/v1/runs/{run}/input"),
                    format!(
                        r#"{{"input":"{}","expected_version":{}}}"#,
                        artifact.digest(),
                        waiting["version"]
                    ),
                )
            }
            "approval" => {
                let list_headers = authenticated_headers(
                    &discovery.credential,
                    &discovery.endpoint,
                    "kill-approval-list",
                );
                let (status, body) = request(
                    address,
                    "GET",
                    &format!("/v1/projects/{project}/approvals"),
                    &list_headers,
                    "",
                )
                .await;
                assert_eq!(status, 200, "{body}");
                let approvals: serde_json::Value = serde_json::from_str(&body).unwrap();
                let approval = &approvals["items"][0];
                (
                    format!("/v1/approvals/{}/resolve", approval["id"].as_str().unwrap()),
                    format!(
                        r#"{{"decision":"approved","expected_version":{}}}"#,
                        approval["version"]
                    ),
                )
            }
            "auth" => (
                format!("/v1/runs/{run}/auth/resolve"),
                format!(
                    r#"{{"granted":true,"expected_version":{}}}"#,
                    waiting["version"]
                ),
            ),
            _ => unreachable!(),
        };
        let (status, response) = request(address, "POST", &path, &headers, &body).await;
        assert_eq!(status, 200, "{response}");
        wait_run_state(
            address,
            &discovery.credential,
            &discovery.endpoint,
            run,
            "completed",
        )
        .await;
        restarted.kill9();

        let events = test_support::open_service_store(root.0.join("state.sqlite3"))
            .unwrap()
            .append_store()
            .events()
            .unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event.event_type.as_str() == "model_call.intent")
                .count(),
            expected_models
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event.event_type.as_str() == "model_call.outcome")
                .count(),
            expected_models
        );
        let journal = events
            .iter()
            .filter(|event| event.event.event_type.as_str() == "agent.effect_journal")
            .map(|event| String::from_utf8_lossy(&event.event.payload).into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            journal
                .iter()
                .filter(|payload| payload.contains("waiting_resolved"))
                .count(),
            1
        );
        for boundary in [
            "before_model_dispatch",
            "after_model_outcome",
            "after_tool_outcome",
            "turn_end",
        ] {
            if journal.iter().any(|payload| payload.contains(boundary)) {
                boundaries.insert(boundary);
            }
        }
    }

    let root = TestRoot::new();
    let barrier = root.0.join("fake-provider-barrier");
    let mut process = spawn_scenario_process(&root.0, "tool-barrier");
    let (_, _, _, run) = start_subprocess_run(&root.0, &mut process).await;
    wait_barrier(&barrier, &mut process, "after_tool_outcome").await;
    process.kill9();
    fs::write(barrier.join("release"), "after_tool_outcome").unwrap();
    let mut restarted = spawn_scenario_process(&root.0, "tool-barrier");
    wait_for_subprocess_discovery(&root.0, &mut restarted.0);
    let discovery = read_discovery(&root.0).unwrap();
    let address = discovery
        .endpoint
        .strip_prefix("http://")
        .unwrap()
        .parse()
        .unwrap();
    wait_ready(address).await;
    wait_run_state(
        address,
        &discovery.credential,
        &discovery.endpoint,
        run,
        "completed",
    )
    .await;
    restarted.kill9();
    let events = test_support::open_service_store(root.0.join("state.sqlite3"))
        .unwrap()
        .append_store()
        .events()
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event.event_type.as_str() == "capability.invocation_intent")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event.event_type.as_str() == "capability.invocation_outcome")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event.event_type.as_str() == "model_call.intent")
            .count(),
        2
    );
    boundaries.insert("after_tool_outcome");
    assert_eq!(
        boundaries,
        std::collections::BTreeSet::from([
            "after_model_outcome",
            "after_tool_outcome",
            "before_model_dispatch",
            "turn_end",
        ])
    );
}

#[cfg(unix)]
#[test]
fn one_hundred_earliest_discovery_signals_shutdown_gracefully() {
    let root = TestRoot::new();
    for iteration in 0..100 {
        let signal = if iteration % 2 == 0 { "-TERM" } else { "-INT" };
        let mut daemon = ProcessCommand::new(env!("CARGO_BIN_EXE_kit"))
            .args(["daemon", "--state-root"])
            .arg(&root.0)
            .env("KIT_PROVIDER", "deterministic-test")
            .env("KIT_FAKE_PROVIDER", "openai")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for_subprocess_discovery(&root.0, &mut daemon);
        assert!(
            ProcessCommand::new("kill")
                .args([signal, &daemon.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = daemon.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = daemon.kill();
                let _ = daemon.wait();
                panic!("daemon did not stop after {signal}");
            }
            thread::sleep(Duration::from_millis(20));
        };
        assert!(
            status.success(),
            "daemon exited unsuccessfully after {signal}: {status}"
        );
        assert!(!root.0.join(DISCOVERY_FILE).exists());

        let database = root.0.join("state.sqlite3");
        drop(kit::test_support::open_service_store(&database).unwrap());
        let mut lease = kit::test_support::open_lease_runtime(&root.0).unwrap();
        assert_eq!(lease.leases().reconcile_startup().unwrap(), Vec::new());
        assert_eq!(lease.shutdown().unwrap(), Vec::new());
        let scheduler = kit::runtime::scheduler::DurableScheduler::open(&database).unwrap();
        assert_eq!(scheduler.reconcile_startup().unwrap(), Default::default());
        scheduler.shutdown().unwrap();
    }
}

#[cfg(unix)]
fn wait_for_subprocess_discovery(root: &std::path::Path, daemon: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if read_discovery(root).is_ok() {
            return;
        }
        if let Some(status) = daemon.try_wait().unwrap() {
            panic!("daemon exited before readiness: {status}");
        }
        if Instant::now() >= deadline {
            let _ = daemon.kill();
            let _ = daemon.wait();
            panic!("daemon readiness timed out");
        }
        thread::sleep(Duration::from_millis(1));
    }
}
