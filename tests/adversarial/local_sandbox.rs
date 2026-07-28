use std::{
    env, fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use kit::executor::profile::{
    Architecture, BackendPrimitive, CompatibilityOptIn, CredentialHandle, CredentialInjection,
    CredentialInjectionMode, ExecutionLabel, ExecutorProfile, MountRole, Platform, ProfileSpec,
    ResourceLimits, TrustTier,
};

use kit::executor::backends::local_os::{LocalOsBackend, NotAvailableReason, SandboxPaths};

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    build: PathBuf,
    temp: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = env::temp_dir().join(format!(
            "kit-local-sandbox-{}-{}",
            std::process::id(),
            FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let source = root.join("source");
        let build = root.join("build");
        let temp = root.join("temp");
        for path in [&source, &build, &temp] {
            fs::create_dir_all(path).unwrap();
        }
        Self {
            root,
            source,
            build,
            temp,
        }
    }

    fn paths(&self) -> SandboxPaths {
        SandboxPaths::new(&self.source, &self.build, &self.temp).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn limits() -> ResourceLimits {
    ResourceLimits::new(
        60_000,
        512 * 1024 * 1024,
        64,
        32 * 1024 * 1024,
        512 * 1024 * 1024,
        512 * 1024 * 1024,
        32 * 1024 * 1024,
        60_000,
    )
}

fn platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else {
        Platform::Windows
    }
}

fn architecture() -> Architecture {
    if cfg!(target_arch = "aarch64") {
        Architecture::Aarch64
    } else {
        Architecture::X86_64
    }
}

fn profile(label: ExecutionLabel) -> ExecutorProfile {
    let spec = match label {
        ExecutionLabel::HostCompatibility => ProfileSpec::host_compatibility(
            platform(),
            architecture(),
            limits(),
            CompatibilityOptIn::trusted_local("explicit test opt-in").unwrap(),
        ),
        _ => ProfileSpec::isolated(
            TrustTier::TrustedLocal,
            platform(),
            architecture(),
            limits(),
        ),
    };
    ExecutorProfile::new(spec).unwrap()
}

#[test]
fn compatibility_has_zero_implicit_selection_paths_and_truthful_label() {
    let fixture = Fixture::new();
    let paths = fixture.paths();
    let trusted = profile(ExecutionLabel::TrustedLocal);
    let _trusted_error = LocalOsBackend::select(&trusted, &paths).unwrap_err();
    #[cfg(target_os = "linux")]
    assert_eq!(
        _trusted_error.reason,
        NotAvailableReason::MountCustodyUnavailable
    );
    assert!(matches!(
        LocalOsBackend::probe(&trusted, &paths).status,
        kit::executor::profile::ProbeStatus::Unavailable { .. }
    ));

    let compatibility_profile = profile(ExecutionLabel::HostCompatibility);
    match LocalOsBackend::select(&compatibility_profile, &paths) {
        Ok(backend) => {
            assert!(!backend.is_isolation());
            assert_eq!(backend.label(), ExecutionLabel::HostCompatibility);
            assert!(backend.description().contains("not isolation"));
        }
        Err(error) => {
            assert_eq!(error.reason, NotAvailableReason::PrimitiveMissing);
            assert!(matches!(
                LocalOsBackend::probe(&compatibility_profile, &paths).status,
                kit::executor::profile::ProbeStatus::Unavailable { .. }
            ));
        }
    }
}

#[test]
fn finite_limits_and_additional_requirements_are_typed_as_unavailable() {
    let fixture = Fixture::new();
    let mut spec = ProfileSpec::host_compatibility(
        platform(),
        architecture(),
        limits(),
        CompatibilityOptIn::trusted_local("explicit test opt-in").unwrap(),
    );
    spec.additional_requirements.extend([
        BackendPrimitive::CpuLimit,
        BackendPrimitive::MemoryLimit,
        BackendPrimitive::PidLimit,
        BackendPrimitive::FileSizeLimit,
        BackendPrimitive::DiskLimit,
        BackendPrimitive::IoLimit,
        BackendPrimitive::SyscallPolicy,
    ]);
    let profile = ExecutorProfile::new(spec).unwrap();
    let error = LocalOsBackend::select(&profile, &fixture.paths()).unwrap_err();
    for primitive in [
        BackendPrimitive::CpuLimit,
        BackendPrimitive::MemoryLimit,
        BackendPrimitive::PidLimit,
        BackendPrimitive::FileSizeLimit,
        BackendPrimitive::DiskLimit,
        BackendPrimitive::IoLimit,
        BackendPrimitive::SyscallPolicy,
    ] {
        assert!(error.missing_primitives.contains(&primitive));
    }
}

#[test]
fn restricted_hostile_and_mislabeled_isolation_never_use_local_compatibility() {
    let fixture = Fixture::new();
    for tier in [TrustTier::Restricted, TrustTier::Hostile] {
        let profile = ExecutorProfile::new(ProfileSpec::isolated(
            tier,
            platform(),
            architecture(),
            limits(),
        ))
        .unwrap();
        let error = LocalOsBackend::select(&profile, &fixture.paths()).unwrap_err();
        assert_eq!(error.reason, NotAvailableReason::UnsupportedLabel);
    }

    let mut full = ProfileSpec::isolated(
        TrustTier::TrustedLocal,
        platform(),
        architecture(),
        limits(),
    );
    full.additional_requirements.extend([
        BackendPrimitive::WholeProcessTreeControl,
        BackendPrimitive::CpuLimit,
    ]);
    let error =
        LocalOsBackend::select(&ExecutorProfile::new(full).unwrap(), &fixture.paths()).unwrap_err();
    assert_eq!(error.reason, NotAvailableReason::PrimitiveMissing);
    #[cfg(target_os = "macos")]
    assert!(
        error
            .missing_primitives
            .contains(&BackendPrimitive::WholeProcessTreeControl)
    );
    assert!(
        error
            .missing_primitives
            .contains(&BackendPrimitive::CpuLimit)
    );

    let compatibility = profile(ExecutionLabel::HostCompatibility);
    let backend = match LocalOsBackend::select(&compatibility, &fixture.paths()) {
        Ok(backend) => backend,
        Err(_) => return,
    };
    let trusted = profile(ExecutionLabel::TrustedLocal);
    let request = kit::executor::backends::local_os::LocalCommand::new(
        "/usr/bin/true",
        fixture.source.clone(),
    );
    assert!(
        backend
            .prepare(&trusted, &fixture.paths(), request)
            .is_err()
    );
}

#[test]
fn structural_policy_exposes_arguments_but_not_execution() {
    let fixture = Fixture::new();
    let paths = fixture.paths();
    let policy = LocalOsBackend::structural_policy_for_test(&paths).unwrap();
    assert!(policy.program().is_absolute());
    assert!(!policy.args().is_empty());

    #[cfg(target_os = "linux")]
    {
        let args = policy.args();
        assert!(args.windows(2).any(|args| args == ["--tmpfs", "/home"]));
        assert!(args.windows(2).any(|args| args == ["--tmpfs", "/run"]));
        for mount in args.windows(3).filter(|args| {
            args[0] == "--ro-bind" || args[0] == "--ro-bind-try" || args[0] == "--bind"
        }) {
            let source = mount[1].to_string_lossy();
            assert!(
                !["/", "/etc", "/opt", "/srv", "/home", "/run", "/var/run"]
                    .contains(&source.as_ref()),
                "bubblewrap exposed forbidden host path {source}"
            );
            assert!(
                !source.contains(".sock"),
                "bubblewrap mounted a host socket"
            );
        }
        let script = args
            .iter()
            .find(|arg| arg.to_string_lossy().contains("setsid"))
            .expect("behavioral process-tree probe");
        assert!(script.to_string_lossy().contains("/proc/net/dev"));
        assert!(script.to_string_lossy().contains("/workspace"));
    }

    #[cfg(target_os = "macos")]
    {
        let seatbelt = policy
            .args()
            .iter()
            .filter_map(|arg| arg.to_str())
            .find(|arg| arg.contains("deny network"))
            .expect("Seatbelt network deny policy");
        assert!(seatbelt.contains("file-write"));
        let script = policy
            .args()
            .iter()
            .filter_map(|arg| arg.to_str())
            .find(|arg| arg.contains("socket.socket"))
            .expect("behavioral socket probe");
        assert!(script.contains("KIT_PROBE_SOURCE"));
    }
}

#[test]
fn local_secret_custody_is_explicitly_unavailable() {
    let fixture = Fixture::new();
    let mut spec = ProfileSpec::host_compatibility(
        platform(),
        architecture(),
        limits(),
        CompatibilityOptIn::trusted_local("explicit test opt-in").unwrap(),
    );
    spec.credentials.push(CredentialInjection {
        handle: CredentialHandle::new("local-secret").unwrap(),
        mode: CredentialInjectionMode::ScopedEnvironment {
            variable: "KIT_SECRET".to_owned(),
        },
    });
    let error =
        LocalOsBackend::select(&ExecutorProfile::new(spec).unwrap(), &fixture.paths()).unwrap_err();
    assert_eq!(
        error.reason,
        NotAvailableReason::CredentialCustodyUnavailable
    );
}

#[test]
fn local_backend_rejects_noncanonical_mount_targets() {
    let fixture = Fixture::new();
    let mut spec = ProfileSpec::isolated(
        TrustTier::TrustedLocal,
        platform(),
        architecture(),
        limits(),
    );
    spec.mounts
        .iter_mut()
        .find(|mount| mount.role == MountRole::Source)
        .unwrap()
        .target = "/repo".into();
    let error =
        LocalOsBackend::select(&ExecutorProfile::new(spec).unwrap(), &fixture.paths()).unwrap_err();
    assert_eq!(error.reason, NotAvailableReason::InvalidPaths);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_source_rejects_cross_boundary_hard_links() {
    let fixture = Fixture::new();
    let source_file = fixture.source.join("linked");
    fs::write(&source_file, "source").unwrap();
    fs::hard_link(&source_file, fixture.root.join("outside-link")).unwrap();
    let error = SandboxPaths::new(&fixture.source, &fixture.build, &fixture.temp).unwrap_err();
    assert_eq!(error.reason, NotAvailableReason::InvalidPaths);
    assert!(error.detail.contains("hard link"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_source_rejects_unix_sockets_and_escaping_links() {
    use std::{os::unix::fs::symlink, os::unix::net::UnixListener};

    let socket_fixture = Fixture::new();
    let _listener = UnixListener::bind(socket_fixture.source.join("agent.sock")).unwrap();
    let error = SandboxPaths::new(
        &socket_fixture.source,
        &socket_fixture.build,
        &socket_fixture.temp,
    )
    .unwrap_err();
    assert!(error.detail.contains("Unix socket"));

    let link_fixture = Fixture::new();
    let outside = link_fixture.root.join("outside");
    fs::write(&outside, "secret").unwrap();
    symlink(&outside, link_fixture.source.join("escape")).unwrap();
    let error = SandboxPaths::new(
        &link_fixture.source,
        &link_fixture.build,
        &link_fixture.temp,
    )
    .unwrap_err();
    assert!(error.detail.contains("escapes"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_mount_identity_replacement_fails_closed() {
    let fixture = Fixture::new();
    let paths = fixture.paths();
    fs::rename(&fixture.source, fixture.root.join("old-source")).unwrap();
    fs::create_dir(&fixture.source).unwrap();

    let error =
        LocalOsBackend::select(&profile(ExecutionLabel::HostCompatibility), &paths).unwrap_err();
    assert_eq!(error.reason, NotAvailableReason::InvalidPaths);
    assert!(error.detail.contains("identity was pinned"));
}
