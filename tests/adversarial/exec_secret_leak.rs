use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use kit::{
    api::auth::{
        contract::{Authenticator, GrantSnapshot},
        local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
    },
    domain::{
        config::Grant,
        ids::{AttemptId, DaemonServiceId, PrincipalId, ProcessId, ProjectId},
        lifecycle::{AttemptOwnership, FencingToken, ProcessClaim, ProcessOwnership},
        secret::SecretLease,
    },
    executor::{
        backends::container::{
            ExecutionError,
            limits::{NotAvailableReason, ProbeRecord},
            preview,
        },
        profile::{
            Architecture, CredentialHandle, CredentialInjection, CredentialInjectionMode,
            ExecutorProfile, Platform, ProfileSpec, ResourceLimits, TrustTier,
        },
        terminal::{
            FakePtyDriver, OutputRetention, TerminalAllocation, TerminalManager, TerminalRequest,
            TerminalSize, TerminalSnapshot,
        },
    },
    telemetry::redact::{CaptureBoundary, CapturePersistencePolicy, CaptureRedactor},
    workspace::acquire::{
        AcquisitionMode, AcquisitionRequest, OwnerId, WorkspaceId, WriterPolicy, acquire, cleanup,
    },
};

const CANARY: &str = "kit-exec-secret-canary+/=42";
const IMAGE: &str = "example.invalid/secret@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const CAPABILITIES: &str = "rootless,seccomp,no_new_privileges,capability_drop,network_deny,proxy_only_network,dns_revalidation,connection_revalidation,peer_deny,gateway_deny,udp_deny,cpu_aggregate,memory,pids,file_size,quota_backed_binds,boundary_io,whole_boundary_kill,quiescence_inspect,pinned_mounts,secret_fd_injection,secret_memfd_injection,secret_scoped_env_injection";

#[test]
pub(crate) fn secret_absent_workspace_metadata() {
    let fixture = Fixture::new();
    fs::write(fixture.source.join("untracked-secret"), CANARY).unwrap();
    let workspace = acquire(fixture.request(
        AcquisitionMode::CopyOnWriteSnapshot,
        WriterPolicy::Hostile,
        "workspace-metadata",
    ))
    .unwrap();

    assert_eq!(format!("{workspace:?}").matches(CANARY).count(), 0);
    let marker = fs::read(workspace.path.parent().unwrap().join(".kit-workspace")).unwrap();
    assert_eq!(String::from_utf8_lossy(&marker).matches(CANARY).count(), 0);
    cleanup(&workspace).unwrap();
}

#[test]
pub(crate) fn secret_argv_policy() {
    let fixture = Fixture::new();
    let workspace = acquire(fixture.request(
        AcquisitionMode::LocalClone,
        WriterPolicy::Restricted,
        "argv-policy",
    ))
    .unwrap();
    let allocation = workspace.path.parent().unwrap();
    let build = allocation.join("build");
    let temp = allocation.join("temp");
    fs::create_dir(&build).unwrap();
    fs::create_dir(&temp).unwrap();
    let plan = preview(
        &evidence(),
        &credential_profile(),
        &workspace,
        &build,
        &temp,
        "credential-plan",
        IMAGE,
        ["/usr/bin/true"],
    );
    if !cfg!(target_os = "linux") {
        assert!(matches!(
            plan,
            Err(kit::executor::backends::container::ContainerError::Secret(
                kit::executor::secrets::PreparationError::UnsupportedPlatform
            ))
        ));
        cleanup(&workspace).unwrap();
        return;
    }
    let plan = plan.unwrap();
    let argv = plan
        .arguments()
        .iter()
        .map(|argument| argument.to_string_lossy())
        .collect::<Vec<_>>();

    assert_eq!(
        argv.iter()
            .filter(|argument| argument.contains(CANARY))
            .count(),
        0
    );
    for binding in [
        "--secret-binding=fd:100",
        "--secret-binding=memfd:101:/proc/self/fd/101",
        "--secret-binding=env:KIT_SCOPED_SECRET:102",
    ] {
        assert!(argv.iter().any(|argument| argument == binding));
    }
    cleanup(&workspace).unwrap();
}

pub(crate) fn secret_absent_terminal_history() {
    const RAW: &str = "split-secret";
    const PERCENT: &str = "%73%70%6C%69%74%2D%73%65%63%72%65%74";
    const BASE64: &str = "c3BsaXQtc2VjcmV0";
    let leases = [SecretLease::new(RAW)];
    let redactor = CaptureRedactor::new(&leases);
    let mut capture = redactor.start(CaptureBoundary::TerminalMetadata);
    for chunk in [
        b"split-".as_slice(),
        b"secret %73%70%6C%69%74%2D%73",
        b"%65%63%72%65%74 c3BsaXQ",
        b"tc2VjcmV0",
    ] {
        capture.push(chunk).unwrap();
    }
    capture.finish().unwrap();

    let principal_id = PrincipalId::generate().unwrap();
    let project_id = ProjectId::generate().unwrap();
    let principal = LocalPeerAuthenticator::new(BTreeMap::from([(
        1_000,
        GrantSnapshot::new(principal_id, project_id, [Grant::ProcessSpawn]),
    )]))
    .authenticate(&LocalPeerObservation::from_transport(1_000, 7, 1_000))
    .unwrap();
    let manager = TerminalManager::new(
        project_id,
        FakePtyDriver::default(),
        (|_: &TerminalSnapshot| Ok(())) as fn(&TerminalSnapshot) -> std::io::Result<()>,
    );
    let claim = ProcessClaim::new(
        ProcessId::generate().unwrap(),
        ProcessOwnership::Attempt(AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal_id,
            FencingToken::new(1),
        )),
    );
    let TerminalAllocation::Pty { control, .. } = manager
        .allocate(
            TerminalRequest::pty(CapturePersistencePolicy::no_secrets()),
            &principal,
            claim,
            "secret-boundary",
            TerminalSize::new(80, 24).unwrap(),
            OutputRetention::new(4096, 1_000),
        )
        .unwrap()
    else {
        unreachable!()
    };
    manager.append_output(&control, &capture, 1).unwrap();
    let persisted = serde_json::to_string(&manager.snapshot(&control).unwrap()).unwrap();
    for secret in [RAW, PERCENT, BASE64] {
        assert!(!persisted.contains(secret));
    }
    assert!(!format!("{capture:?}").contains(RAW));
}

#[cfg(unix)]
#[test]
fn hooks_and_submodules_are_disabled_in_every_workspace_acquisition_mode() {
    for (index, (mode, policy)) in [
        (
            AcquisitionMode::DetachedWorktree,
            WriterPolicy::TrustedAllowSharedGitMetadata,
        ),
        (AcquisitionMode::LocalClone, WriterPolicy::Restricted),
        (AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile),
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = Fixture::new();
        let sentinel = fixture.root.join("hook-ran");
        let hook = fixture.source.join(".git/hooks/post-checkout");
        fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
        )
        .unwrap();
        let mut permissions = fs::metadata(&hook).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&hook, permissions).unwrap();

        let workspace = acquire(fixture.request(mode, policy, &format!("policy-{index}"))).unwrap();
        assert!(!sentinel.exists(), "checkout hook ran for {mode:?}");
        if mode != AcquisitionMode::DetachedWorktree {
            assert!(!workspace.path.join(".git/hooks/post-checkout").exists());
        }
        let allocation = workspace.path.parent().unwrap();
        let build = allocation.join("build");
        let temp = allocation.join("temp");
        fs::create_dir(&build).unwrap();
        fs::create_dir(&temp).unwrap();
        match preview(
            &evidence(),
            &restricted_profile(),
            &workspace,
            &build,
            &temp,
            "hook-attempt",
            IMAGE,
            ["/usr/bin/git", "checkout", "HEAD", "--", "."],
        ) {
            Ok(plan) => assert!(matches!(
                plan.run(ProcessOwnership::DaemonService(
                    DaemonServiceId::generate().unwrap()
                )),
                Err(ExecutionError::NotAvailable(ref unavailable))
                    if unavailable.reason == NotAvailableReason::UntrustedTestEvidence
            )),
            Err(error) => assert_eq!(
                mode,
                AcquisitionMode::DetachedWorktree,
                "independent workspace policy failed unexpectedly: {error}"
            ),
        }
        assert!(
            !sentinel.exists(),
            "selected policy allowed hook for {mode:?}"
        );
        cleanup(&workspace).unwrap();
    }
}

fn evidence() -> ProbeRecord {
    ProbeRecord::parse(&format!(
        "protocol=kit-container-v1\nruntime=podman\nruntime_path=/usr/bin/podman\nruntime_identity=sha256:{a}\nruntime_version=sha256:{b}\nruntime_config=sha256:{c}\nhelper_identity=sha256:{d}\nseccomp=builtin\ncapabilities={CAPABILITIES}\nproxy_network=kit-proxy-only\nproxy_endpoint=http://proxy:8080\nproxy_lease={e}\n",
        a = "a".repeat(64),
        b = "b".repeat(64),
        c = "c".repeat(64),
        d = "d".repeat(64),
        e = "e".repeat(64),
    ))
    .unwrap()
}

fn credential_profile() -> ExecutorProfile {
    let mut spec = restricted_spec();
    spec.credentials = vec![
        CredentialInjection {
            handle: CredentialHandle::new("fd-secret").unwrap(),
            mode: CredentialInjectionMode::FileDescriptor,
        },
        CredentialInjection {
            handle: CredentialHandle::new("memfd-secret").unwrap(),
            mode: CredentialInjectionMode::MemoryFile,
        },
        CredentialInjection {
            handle: CredentialHandle::new("env-secret").unwrap(),
            mode: CredentialInjectionMode::ScopedEnvironment {
                variable: "KIT_SCOPED_SECRET".to_owned(),
            },
        },
    ];
    ExecutorProfile::new(spec).unwrap()
}

fn restricted_profile() -> ExecutorProfile {
    ExecutorProfile::new(restricted_spec()).unwrap()
}

fn restricted_spec() -> ProfileSpec {
    ProfileSpec::isolated(
        TrustTier::Restricted,
        Platform::Linux,
        host_architecture(),
        ResourceLimits::new(1, 1, 1, 1, 1, 1, 128, 1_000),
    )
}

fn host_architecture() -> Architecture {
    if cfg!(target_arch = "aarch64") {
        Architecture::Aarch64
    } else {
        Architecture::X86_64
    }
}

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    managed: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("kit-exec-secret-{}", hex(&random)));
        let source = root.join("source");
        let managed = root.join("managed");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&source).unwrap();
        fs::create_dir(&managed).unwrap();
        git(&source, &["init", "--quiet"]);
        fs::write(source.join("tracked.txt"), "base\n").unwrap();
        git(&source, &["add", "tracked.txt"]);
        git(
            &source,
            &[
                "-c",
                "user.name=Kit Test",
                "-c",
                "user.email=kit@example.invalid",
                "commit",
                "--quiet",
                "--no-gpg-sign",
                "-m",
                "base",
            ],
        );
        Self {
            root,
            source,
            managed,
        }
    }

    fn request(
        &self,
        mode: AcquisitionMode,
        writer_policy: WriterPolicy,
        id: &str,
    ) -> AcquisitionRequest {
        AcquisitionRequest::new(
            &self.source,
            &self.managed,
            WorkspaceId::new(id).unwrap(),
            OwnerId::new(id).unwrap(),
            mode,
            writer_policy,
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git(directory: &Path, arguments: &[&str]) {
    let output = Command::new("git")
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
