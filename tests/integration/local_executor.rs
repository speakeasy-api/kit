#![cfg(unix)]

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use kit::{
    domain::{ids::DaemonServiceId, lifecycle::ProcessOwnership},
    executor::{
        backends::local_os::{LocalCommand, LocalOsBackend, SandboxPaths},
        profile::{
            Architecture, CompatibilityOptIn, ExecutionLabel, ExecutorProfile, Platform,
            ProfileSpec, ResourceLimits, TrustTier,
        },
    },
};

#[cfg(target_os = "linux")]
use kit::{
    domain::{
        ids::{AttemptId, PrincipalId},
        lifecycle::{AttemptOwnership, FencingToken},
    },
    executor::process::own::ProcessState,
    telemetry::redact::{CaptureBoundary, CaptureRedactor},
};

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    build: PathBuf,
    temp: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root =
            env::temp_dir().join(format!("kit-local-executor-{name}-{}", std::process::id()));
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
        10_000,
        128 << 20,
        32,
        16 << 20,
        128 << 20,
        128 << 20,
        1 << 20,
        5_000,
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

fn owner() -> ProcessOwnership {
    ProcessOwnership::DaemonService(DaemonServiceId::generate().unwrap())
}

#[cfg(target_os = "linux")]
fn attempt_owner() -> ProcessOwnership {
    ProcessOwnership::Attempt(AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        PrincipalId::parse("principal_00000000000000000000000001").unwrap(),
        FencingToken::new(1),
    ))
}

#[cfg(target_os = "linux")]
fn output_bytes(stream: &kit::executor::process::own::CapturedStream) -> Vec<u8> {
    stream
        .sanitize(&CaptureRedactor::new(&[]), CaptureBoundary::Log)
        .bytes()
        .unwrap()
        .to_vec()
}

#[test]
fn trusted_local_is_unavailable_without_safe_mount_and_process_custody() {
    let fixture = Fixture::new("trusted");
    let paths = fixture.paths();
    let profile = ExecutorProfile::new(ProfileSpec::isolated(
        TrustTier::TrustedLocal,
        platform(),
        architecture(),
        limits(),
    ))
    .unwrap();
    assert!(
        matches!(
            LocalOsBackend::probe(&profile, &paths).status,
            kit::executor::profile::ProbeStatus::Unavailable { .. }
        ),
        "trusted-local must not claim runnable local isolation"
    );
}

#[test]
fn explicit_compatibility_is_synchronous_unpublished_and_cleans_the_process_group() {
    let fixture = Fixture::new("compatibility");
    let paths = fixture.paths();
    let profile = ExecutorProfile::new(ProfileSpec::host_compatibility(
        platform(),
        architecture(),
        limits(),
        CompatibilityOptIn::trusted_local("integration test explicitly needs host tools").unwrap(),
    ))
    .unwrap();
    if cfg!(target_os = "macos") {
        let error = LocalOsBackend::select(&profile, &paths).unwrap_err();
        assert!(
            error
                .detail
                .contains("complete reconstructible process boundary")
        );
        assert!(matches!(
            LocalOsBackend::probe(&profile, &paths).status,
            kit::executor::profile::ProbeStatus::Unavailable { .. }
        ));
    } else {
        let Ok(backend) = LocalOsBackend::select(&profile, &paths) else {
            return;
        };
        assert_eq!(backend.label(), ExecutionLabel::HostCompatibility);
        assert!(!backend.is_isolation());

        #[cfg(target_os = "macos")]
        {
            let escaped = fixture.temp.join("setsid-survivor");
            let script = format!(
                "python3 -c 'import os,time; os.setsid(); time.sleep(.2); open({}, \"w\").close()' &",
                shell_path(&escaped)
            );
            let prepared = backend
                .prepare(
                    &profile,
                    &paths,
                    LocalCommand::new("/bin/sh", &fixture.source)
                        .arg("-c")
                        .arg(script),
                )
                .unwrap();
            let error = match prepared.run_compatibility_sync(owner()) {
                Ok(_) => panic!("macOS process group entered reassignable execution"),
                Err(error) => error,
            };
            assert!(error.to_string().contains("complete process boundary"));
            std::thread::sleep(std::time::Duration::from_millis(300));
            assert!(!escaped.exists(), "refused setsid payload was launched");
        }

        #[cfg(target_os = "linux")]
        {
            let environment = backend
                .prepare(
                    &profile,
                    &paths,
                    LocalCommand::new("/usr/bin/env", &fixture.source)
                        .env("KIT_ALLOWED", "visible"),
                )
                .unwrap()
                .run_compatibility_sync(owner())
                .unwrap();
            assert!(matches!(
                environment.state,
                ProcessState::Exited { success: true, .. }
            ));
            let output = String::from_utf8(output_bytes(&environment.output.stdout)).unwrap();
            assert!(output.lines().any(|line| line == "KIT_ALLOWED=visible"));
            for line in output.lines() {
                let key = line.split_once('=').unwrap().0;
                assert!(matches!(
                    key,
                    "HOME"
                        | "TMPDIR"
                        | "PATH"
                        | "LANG"
                        | "LC_ALL"
                        | "GIT_CONFIG_NOSYSTEM"
                        | "GIT_CONFIG_GLOBAL"
                        | "GIT_CONFIG_COUNT"
                        | "GIT_CONFIG_KEY_0"
                        | "GIT_CONFIG_VALUE_0"
                        | "KIT_ALLOWED"
                ));
            }

            let pid_file = fixture.temp.join("child.pid");
            let script = format!(
                "/usr/bin/setsid sleep 30 & printf %s $! > {}; exit 0",
                shell_path(&pid_file)
            );
            let grouped = backend
                .prepare(
                    &profile,
                    &paths,
                    LocalCommand::new("/bin/sh", &fixture.source)
                        .arg("-c")
                        .arg(script),
                )
                .unwrap()
                .run_compatibility_sync(owner())
                .unwrap();
            assert!(matches!(
                grouped.state,
                ProcessState::Exited { success: true, .. }
            ));
            let child: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
            assert_eq!(
                unsafe { kill(child, 0) },
                -1,
                "background process survived group cleanup"
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn local_compatibility_rejects_attempt_ownership_without_a_false_cancel_path() {
    let fixture = Fixture::new("attempt-owner");
    let paths = fixture.paths();
    let profile = ExecutorProfile::new(ProfileSpec::host_compatibility(
        platform(),
        architecture(),
        limits(),
        CompatibilityOptIn::trusted_local("attempt ownership rejection test").unwrap(),
    ))
    .unwrap();
    let Ok(backend) = LocalOsBackend::select(&profile, &paths) else {
        return;
    };
    let prepared = backend
        .prepare(
            &profile,
            &paths,
            LocalCommand::new("/usr/bin/true", &fixture.source),
        )
        .unwrap();

    let error = prepared
        .run_compatibility_sync(attempt_owner())
        .unwrap_err();
    assert!(error.to_string().contains("cancellation coordination"));
    assert!(!fs::read_dir(&fixture.root).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".kit-local-boundary-")
    }));
}

#[cfg(target_os = "linux")]
#[test]
fn raced_executable_returns_error_without_started_publication() {
    use std::os::unix::fs::PermissionsExt as _;

    let fixture = Fixture::new("raced-executable");
    let paths = fixture.paths();
    let program = fixture.source.join("program");
    fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let profile = ExecutorProfile::new(ProfileSpec::host_compatibility(
        platform(),
        architecture(),
        limits(),
        CompatibilityOptIn::trusted_local("executable race test").unwrap(),
    ))
    .unwrap();
    let Ok(backend) = LocalOsBackend::select(&profile, &paths) else {
        return;
    };
    let prepared = backend
        .prepare(
            &profile,
            &paths,
            LocalCommand::new(&program, &fixture.source),
        )
        .unwrap();
    fs::remove_file(program).unwrap();

    assert!(prepared.run_compatibility_sync(owner()).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_mount_replacement_between_prepare_and_launch_fails_closed() {
    let fixture = Fixture::new("replacement");
    let paths = fixture.paths();
    let profile = ExecutorProfile::new(ProfileSpec::host_compatibility(
        platform(),
        architecture(),
        limits(),
        CompatibilityOptIn::trusted_local("adversarial replacement test").unwrap(),
    ))
    .unwrap();
    let Ok(backend) = LocalOsBackend::select(&profile, &paths) else {
        return;
    };
    let prepared = backend
        .prepare(
            &profile,
            &paths,
            LocalCommand::new("/usr/bin/true", &fixture.source),
        )
        .unwrap();
    fs::rename(&fixture.source, fixture.root.join("old-source")).unwrap();
    fs::create_dir(&fixture.source).unwrap();

    let error = match prepared.run_compatibility_sync(owner()) {
        Ok(_) => panic!("replaced mount identity was launched"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("identity was pinned"));
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}
