use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

use kit::{
    domain::{ids::DaemonServiceId, lifecycle::ProcessOwnership},
    executor::{
        backends::container::{
            ContainerError, ExecutionError,
            limits::{NotAvailableReason, ProbeRecord},
            prepare, preview,
        },
        profile::{
            Architecture, BackendPrimitive, CredentialHandle, CredentialInjection,
            CredentialInjectionMode, ExecutorProfile, MountAccess, MountRole, Platform,
            ProfileSpec, RepositoryCodePolicy, ResourceLimits, SourceWriteMode, TrustTier,
        },
    },
    workspace::acquire::{
        AcquisitionMode, AcquisitionRequest, AcquisitionResult, OwnerId, WorkspaceId, WriterPolicy,
        acquire,
    },
};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);
const IMAGE: &str = "example.invalid/kit-test@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const CAPABILITIES: &str = "rootless,seccomp,no_new_privileges,capability_drop,network_deny,proxy_only_network,dns_revalidation,connection_revalidation,peer_deny,gateway_deny,udp_deny,cpu_aggregate,memory,pids,file_size,quota_backed_binds,boundary_io,whole_boundary_kill,quiescence_inspect,pinned_mounts,secret_fd_injection,secret_memfd_injection,secret_scoped_env_injection";

fn evidence() -> ProbeRecord {
    ProbeRecord::parse(&format!(
        "protocol=kit-container-v1\nruntime=podman\nruntime_path=/usr/bin/podman\nruntime_identity=sha256:{a}\nruntime_version=sha256:{b}\nruntime_config=sha256:{c}\nhelper_identity=sha256:{d}\nseccomp=/usr/share/containers/seccomp.json\ncapabilities={CAPABILITIES}\nproxy_network=kit-proxy-only\nproxy_endpoint=http://proxy:8080\nproxy_lease={e}\n",
        a = "a".repeat(64),
        b = "b".repeat(64),
        c = "c".repeat(64),
        d = "d".repeat(64),
        e = "e".repeat(64),
    ))
    .unwrap()
}

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(name: &str) -> Self {
        let base = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let path = base.join(format!(
            "kit-container-fs-{name}-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }

    fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.0.join(path)
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct WorkspaceFixture {
    _root: TestRoot,
    workspace: AcquisitionResult,
    build: PathBuf,
    temp: PathBuf,
}

impl WorkspaceFixture {
    fn independent(name: &str) -> Self {
        Self::new(name, AcquisitionMode::LocalClone, WriterPolicy::Restricted)
    }

    fn new(name: &str, mode: AcquisitionMode, writer_policy: WriterPolicy) -> Self {
        let root = TestRoot::new(name);
        let source = root.join("source");
        let managed = root.join("managed");
        for path in [&source, &managed] {
            fs::create_dir(path).unwrap();
        }
        git(&source, ["init", "--quiet"]);
        git(&source, ["config", "user.name", "Kit Test"]);
        git(&source, ["config", "user.email", "kit@example.invalid"]);
        fs::write(source.join("victim"), b"unchanged\n").unwrap();
        git(&source, ["add", "victim"]);
        git(&source, ["commit", "--quiet", "-m", "fixture"]);
        let workspace = acquire(AcquisitionRequest::new(
            &source,
            &managed,
            WorkspaceId::new(format!("workspace-{name}")).unwrap(),
            OwnerId::new("owner-container-fs").unwrap(),
            mode,
            writer_policy,
        ))
        .unwrap();
        let allocation = workspace.path.parent().unwrap();
        let build = allocation.join("build");
        let temp = allocation.join("temp");
        fs::create_dir(&build).unwrap();
        fs::create_dir(&temp).unwrap();
        Self {
            _root: root,
            workspace,
            build: fs::canonicalize(build).unwrap(),
            temp: fs::canonicalize(temp).unwrap(),
        }
    }
}

fn git<const N: usize>(directory: &Path, arguments: [&str; N]) {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn profile() -> ExecutorProfile {
    ExecutorProfile::new(profile_spec()).unwrap()
}

fn profile_spec() -> ProfileSpec {
    ProfileSpec::isolated(
        TrustTier::Restricted,
        Platform::Linux,
        if cfg!(target_arch = "aarch64") {
            Architecture::Aarch64
        } else {
            Architecture::X86_64
        },
        ResourceLimits::new(1_500, 64 << 20, 16, 1024, 2048, 4096, 8192, 250),
    )
}

fn preview_fixture(
    fixture: &WorkspaceFixture,
) -> Result<kit::executor::backends::container::ContainerPlan, ContainerError> {
    preview(
        &evidence(),
        &profile(),
        &fixture.workspace,
        &fixture.build,
        &fixture.temp,
        "integrated-plan",
        IMAGE,
        [
            OsString::from("/usr/bin/tool"),
            OsString::from("argument with spaces"),
        ],
    )
}

#[test]
fn one_non_runnable_preview_contains_the_entire_execution_contract() {
    let fixture = WorkspaceFixture::independent("argv");
    let plan = preview_fixture(&fixture).unwrap();
    assert_eq!(plan.program(), "/usr/libexec/kit-container-helper");
    assert!(!plan.is_runnable());
    let args = plan
        .arguments()
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    for required in [
        "--mount-lease=pinned",
        "--cpu-aggregate-ms=1500",
        "--memory-bytes=67108864",
        "--pids=16",
        "--file-bytes=1024",
        "--writable-bind-quota=2048",
        "--boundary-io-bytes=4096",
        "--output-bytes=8192",
        "--wall-time-ms=250",
        "--kill-whole-boundary",
        "--require-quiescence",
        "--persist-before-release",
        "--network-policy=deny",
        "--network=none",
        "--read-only",
        "--cap-drop=ALL",
        "--security-opt=no-new-privileges",
        "--security-opt=seccomp=/usr/share/containers/seccomp.json",
        "--pull=never",
    ] {
        assert!(
            args.iter().any(|argument| argument == required),
            "missing {required}"
        );
    }
    let mounts = args
        .iter()
        .filter(|argument| argument.starts_with("--mount="))
        .collect::<Vec<_>>();
    assert_eq!(mounts.len(), 3);
    assert!(mounts.iter().all(|mount| mount.contains("src=@kit-lease:")));
    assert!(
        mounts
            .iter()
            .any(|mount| mount.contains("dst=/workspace,readonly,"))
    );
    assert!(
        mounts
            .iter()
            .any(|mount| mount.contains("dst=/build,") && !mount.contains("readonly"))
    );
    assert!(
        mounts
            .iter()
            .any(|mount| mount.contains("dst=/tmp,") && !mount.contains("readonly"))
    );
    assert!(args.iter().any(|argument| argument == IMAGE));
    let boundary_record = args
        .iter()
        .find_map(|argument| argument.strip_prefix("--boundary-record="))
        .unwrap();
    assert_eq!(
        PathBuf::from(boundary_record).parent(),
        fixture.temp.parent()
    );
    assert!(!PathBuf::from(boundary_record).starts_with(&fixture.temp));
    assert!(
        !args
            .iter()
            .any(|argument| argument.starts_with("--result="))
    );
    assert!(matches!(
        plan.run(ProcessOwnership::DaemonService(
            DaemonServiceId::generate().unwrap()
        )),
        Err(ExecutionError::NotAvailable(ref unavailable))
            if unavailable.reason == NotAvailableReason::UntrustedTestEvidence
    ));
}

#[test]
fn duplicate_display_names_get_distinct_internal_boundary_ownership() {
    let fixture = WorkspaceFixture::independent("duplicate-boundary");
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| preview_fixture(&fixture).unwrap());
        let second = scope.spawn(|| preview_fixture(&fixture).unwrap());
        (first.join().unwrap(), second.join().unwrap())
    });
    let value = |plan: &kit::executor::backends::container::ContainerPlan, prefix: &str| {
        plan.arguments()
            .iter()
            .map(|argument| argument.to_string_lossy())
            .find_map(|argument| argument.strip_prefix(prefix).map(str::to_owned))
            .unwrap()
    };
    assert_ne!(
        value(&first, "--ownership-id="),
        value(&second, "--ownership-id=")
    );
    assert_ne!(value(&first, "--boundary="), value(&second, "--boundary="));
    assert_ne!(value(&first, "--name="), value(&second, "--name="));
    assert_ne!(
        value(&first, "--plan-digest="),
        value(&second, "--plan-digest=")
    );
    assert_ne!(
        value(&first, "--boundary-record="),
        value(&second, "--boundary-record=")
    );
}

#[test]
fn unsupported_profile_primitives_fail_closed_before_a_plan_exists() {
    let fixture = WorkspaceFixture::independent("unsupported-profile");
    let assert_missing = |spec| {
        let profile = ExecutorProfile::new(spec).unwrap();
        assert!(matches!(
            preview(
                &evidence(),
                &profile,
                &fixture.workspace,
                &fixture.build,
                &fixture.temp,
                "unsupported",
                IMAGE,
                ["true"],
            ),
            Err(ContainerError::NotAvailable(ref unavailable))
                if unavailable.reason == NotAvailableReason::PrimitiveMissing
        ));
    };

    let mut additional = profile_spec();
    additional
        .additional_requirements
        .insert(BackendPrimitive::TenantBoundary);
    assert_missing(additional);

    let mut credentials = profile_spec();
    credentials.credentials.push(CredentialInjection {
        handle: CredentialHandle::new("registry").unwrap(),
        mode: CredentialInjectionMode::MemoryFile,
    });
    let credential_profile = ExecutorProfile::new(credentials).unwrap();
    let credential_plan = preview(
        &evidence(),
        &credential_profile,
        &fixture.workspace,
        &fixture.build,
        &fixture.temp,
        "credential",
        IMAGE,
        ["true"],
    );
    if cfg!(target_os = "linux") {
        assert!(
            credential_plan
                .unwrap()
                .arguments()
                .iter()
                .any(|argument| { argument == "--secret-binding=memfd:100:/proc/self/fd/100" })
        );
    } else {
        assert!(matches!(credential_plan, Err(ContainerError::Secret(_))));
    }

    let mut source_write = profile_spec();
    source_write.source_write = SourceWriteMode::MutationOverlay;
    source_write
        .mounts
        .iter_mut()
        .find(|mount| mount.role == MountRole::Source)
        .unwrap()
        .access = MountAccess::CopyOnWrite;
    assert_missing(source_write);

    let mut repository = profile_spec();
    repository.repository.hooks = RepositoryCodePolicy::Sandboxed;
    assert_missing(repository);
}

#[test]
fn profile_mount_target_delimiters_are_rejected_before_argv_construction() {
    let fixture = WorkspaceFixture::independent("target-delimiter");
    let mut spec = profile_spec();
    spec.mounts
        .iter_mut()
        .find(|mount| mount.role == MountRole::Source)
        .unwrap()
        .target = PathBuf::from("/workspace,src=host");
    let profile = ExecutorProfile::new(spec).unwrap();
    assert!(matches!(
        preview(
            &evidence(),
            &profile,
            &fixture.workspace,
            &fixture.build,
            &fixture.temp,
            "delimiter",
            IMAGE,
            ["true"],
        ),
        Err(ContainerError::Mount(_))
    ));
}

#[test]
fn invalid_digest_shared_git_and_overlapping_writable_paths_are_refused() {
    let fixture = WorkspaceFixture::independent("invalid");
    assert!(matches!(
        preview(
            &evidence(),
            &profile(),
            &fixture.workspace,
            &fixture.build,
            &fixture.temp,
            "bad",
            "image:latest",
            ["true"]
        ),
        Err(ContainerError::InvalidImageDigest)
    ));
    assert!(matches!(
        preview(
            &evidence(),
            &profile(),
            &fixture.workspace,
            &fixture.build,
            &fixture.build,
            "bad",
            IMAGE,
            ["true"]
        ),
        Err(ContainerError::Mount(_))
    ));
    let shared = WorkspaceFixture::new(
        "shared",
        AcquisitionMode::DetachedWorktree,
        WriterPolicy::TrustedAllowSharedGitMetadata,
    );
    assert!(matches!(
        preview(
            &evidence(),
            &profile(),
            &shared.workspace,
            &shared.build,
            &shared.temp,
            "bad",
            IMAGE,
            ["true"]
        ),
        Err(ContainerError::Mount(_))
    ));
}

#[cfg(unix)]
#[test]
fn source_escape_corpus_is_refused_before_any_plan_exists() {
    use std::os::unix::{fs::symlink, net::UnixListener};

    let symlink_fixture = WorkspaceFixture::independent("symlink");
    symlink(
        "../../../host-secret",
        symlink_fixture.workspace.path.join("escape"),
    )
    .unwrap();
    assert!(matches!(
        preview_fixture(&symlink_fixture),
        Err(ContainerError::Mount(_))
    ));

    let hardlink_fixture = WorkspaceFixture::independent("hardlink");
    let outside = hardlink_fixture._root.join("outside");
    fs::write(&outside, b"host secret").unwrap();
    fs::hard_link(&outside, hardlink_fixture.workspace.path.join("hardlink")).unwrap();
    assert!(matches!(
        preview_fixture(&hardlink_fixture),
        Err(ContainerError::Mount(_))
    ));

    let socket_fixture = WorkspaceFixture::independent("socket");
    let short_socket = PathBuf::from(format!(
        "/tmp/kcf-{}-{}",
        std::process::id(),
        NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
    ));
    let _socket = UnixListener::bind(&short_socket).unwrap();
    fs::rename(&short_socket, socket_fixture.workspace.path.join("socket")).unwrap();
    assert!(matches!(
        preview_fixture(&socket_fixture),
        Err(ContainerError::Mount(_))
    ));

    let delimiter_fixture = WorkspaceFixture::independent("delimiter");
    let comma = delimiter_fixture
        .workspace
        .path
        .parent()
        .unwrap()
        .join("build,source=/");
    fs::create_dir(&comma).unwrap();
    assert!(matches!(
        preview(
            &evidence(),
            &profile(),
            &delimiter_fixture.workspace,
            &fs::canonicalize(comma).unwrap(),
            &delimiter_fixture.temp,
            "bad",
            IMAGE,
            ["true"],
        ),
        Err(ContainerError::Mount(_))
    ));
}

#[cfg(not(target_os = "linux"))]
#[test]
fn this_host_returns_a_typed_not_available_record() {
    let fixture = WorkspaceFixture::independent("host-unavailable");
    assert!(matches!(
        prepare(
            &profile(),
            &fixture.workspace,
            &fixture.build,
            &fixture.temp,
            "host-unavailable",
            IMAGE,
            ["true"],
        ),
        Err(ContainerError::NotAvailable(ref unavailable))
            if unavailable.reason == NotAvailableReason::UnsupportedHost && !unavailable.detail.is_empty()
    ));
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the installed trusted helper and a preloaded immutable image"]
fn explicitly_requested_runtime_escape_test_requires_real_evidence() {
    let image = std::env::var("KIT_TEST_CONTAINER_IMAGE")
        .expect("KIT_TEST_CONTAINER_IMAGE must be set when this ignored test is requested");
    let fixture = WorkspaceFixture::independent("runtime");
    let helper = fixture.build.join("container-fs-helper");
    fs::copy(std::env::current_exe().unwrap(), &helper).unwrap();
    let plan = prepare(
        &profile(),
        &fixture.workspace,
        &fixture.build,
        &fixture.temp,
        "runtime-escape",
        &image,
        [
            OsString::from("/build/container-fs-helper"),
            OsString::from("--ignored"),
            OsString::from("container_runtime_escape_helper"),
            OsString::from("--nocapture"),
        ],
    )
    .expect("trusted helper evidence is mandatory when this test is requested");
    plan.run(ProcessOwnership::DaemonService(
        DaemonServiceId::generate().unwrap(),
    ))
    .expect("trusted runtime escape corpus");
    assert_eq!(
        fs::read(fixture.workspace.path.join("victim")).unwrap(),
        b"unchanged\n"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn container_runtime_escape_helper() {
    use std::{ffi::CString, os::unix::net::UnixStream, ptr};

    for result in [
        fs::write("/workspace/new", b"escape"),
        fs::write("/workspace/victim", b"escape"),
        fs::remove_file("/workspace/victim"),
        fs::create_dir("/workspace/new-directory"),
    ] {
        assert_eq!(result.unwrap_err().raw_os_error(), Some(30));
    }
    std::os::unix::fs::symlink("/workspace", "/build/source-link").unwrap();
    assert_eq!(
        fs::write("/build/source-link/write", b"escape")
            .unwrap_err()
            .raw_os_error(),
        Some(30)
    );
    assert!(fs::hard_link("/workspace/victim", "/build/hardlink").is_err());
    for socket in [
        "/run/docker.sock",
        "/var/run/docker.sock",
        "/run/podman/podman.sock",
    ] {
        assert!(
            UnixStream::connect(socket).is_err(),
            "connected to {socket}"
        );
    }
    for device in ["/dev/kvm", "/dev/mem", "/dev/sda"] {
        assert!(
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(device)
                .is_err()
        );
    }
    assert!(std::env::var_os("SSH_AUTH_SOCK").is_none());
    fs::create_dir("/build/mount-target").unwrap();
    let source = CString::new("/workspace").unwrap();
    let target = CString::new("/build/mount-target").unwrap();
    unsafe extern "C" {
        fn mount(
            source: *const std::ffi::c_char,
            target: *const std::ffi::c_char,
            filesystem_type: *const std::ffi::c_char,
            flags: usize,
            data: *const std::ffi::c_void,
        ) -> std::ffi::c_int;
    }
    assert_eq!(
        unsafe {
            mount(
                source.as_ptr(),
                target.as_ptr(),
                ptr::null(),
                4096,
                ptr::null(),
            )
        },
        -1
    );
}
