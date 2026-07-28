use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

#[cfg(target_os = "linux")]
use kit::domain::{ids::DaemonServiceId, lifecycle::ProcessOwnership};
use kit::{
    executor::{
        backends::container::{
            BoundaryControl, BoundaryState, ExecutionError,
            limits::{
                BoundError, EnforcementPrimitive, EvidenceError, ProbeRecord, ResourceIdentity,
            },
            preview, terminate_after_violation,
        },
        profile::{
            Architecture, ExecutorProfile, Platform, ProfileSpec, ResourceLimits, TrustTier,
        },
    },
    workspace::acquire::{
        AcquisitionMode, AcquisitionRequest, AcquisitionResult, OwnerId, WorkspaceId, WriterPolicy,
        acquire,
    },
};

const IMAGE: &str = "example.invalid/limits@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const CAPABILITIES: &str = "rootless,seccomp,no_new_privileges,capability_drop,network_deny,proxy_only_network,dns_revalidation,connection_revalidation,peer_deny,gateway_deny,udp_deny,cpu_aggregate,memory,pids,file_size,quota_backed_binds,boundary_io,whole_boundary_kill,quiescence_inspect,pinned_mounts,secret_fd_injection,secret_memfd_injection,secret_scoped_env_injection";
static NEXT: AtomicU64 = AtomicU64::new(0);

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

struct Fixture {
    root: PathBuf,
    workspace: AcquisitionResult,
    build: PathBuf,
    temp: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = PathBuf::from("/tmp").join(format!(
            "kit-container-limits-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let source = root.join("source");
        let managed = root.join("managed");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&managed).unwrap();
        git(&source, ["init", "--quiet"]);
        git(&source, ["config", "user.name", "Kit Test"]);
        git(&source, ["config", "user.email", "kit@example.invalid"]);
        fs::write(source.join("file"), b"fixture").unwrap();
        git(&source, ["add", "file"]);
        git(&source, ["commit", "--quiet", "-m", "fixture"]);
        let workspace = acquire(AcquisitionRequest::new(
            &source,
            &managed,
            WorkspaceId::new(format!("limits-{}", NEXT.fetch_add(1, Ordering::Relaxed))).unwrap(),
            OwnerId::new("container-limits").unwrap(),
            AcquisitionMode::LocalClone,
            WriterPolicy::Restricted,
        ))
        .unwrap();
        let build = workspace.path.parent().unwrap().join("build");
        let temp = workspace.path.parent().unwrap().join("temp");
        fs::create_dir(&build).unwrap();
        fs::create_dir(&temp).unwrap();
        Self {
            root,
            workspace,
            build,
            temp,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn git<const N: usize>(directory: &Path, arguments: [&str; N]) {
    assert!(
        Command::new("git")
            .args(arguments)
            .current_dir(directory)
            .status()
            .unwrap()
            .success()
    );
}

fn profile() -> ExecutorProfile {
    ExecutorProfile::new(ProfileSpec::isolated(
        TrustTier::Restricted,
        Platform::Linux,
        if cfg!(target_arch = "aarch64") {
            Architecture::Aarch64
        } else {
            Architecture::X86_64
        },
        ResourceLimits::new(1_500, 64 << 20, 16, 1024, 2048, 4096, 8192, 250),
    ))
    .unwrap()
}

fn plan(fixture: &Fixture) -> kit::executor::backends::container::ContainerPlan {
    preview(
        &evidence(),
        &profile(),
        &fixture.workspace,
        &fixture.build,
        &fixture.temp,
        "limit-probe",
        IMAGE,
        ["/kit-limit-probe"],
    )
    .unwrap()
}

fn plan_argument(plan: &kit::executor::backends::container::ContainerPlan, prefix: &str) -> String {
    plan.arguments()
        .iter()
        .map(|argument| argument.to_string_lossy())
        .find_map(|argument| argument.strip_prefix(prefix).map(str::to_owned))
        .unwrap()
}

fn monitor_record(
    plan: &kit::executor::backends::container::ContainerPlan,
    nonce: &str,
    outcome: &str,
    observed: &str,
    monitor_evidence: &str,
) -> String {
    format!(
        "protocol=kit-container-v1\nnonce={nonce}\nownership_id={ownership}\nplan_digest={plan_digest}\nruntime_identity=sha256:{runtime}\nhelper_identity=sha256:{helper}\nresolved_image_digest=sha256:{image}\nboundary_id={boundary}\ninstance_id=instance-1\nrootfs_lease_id=rootfs-1\nwritable_lease_id=writable-1\ninvocation_digest={invocation}\noutcome={outcome}\nobserved={observed}\nmonitor_evidence={monitor_evidence}\nboundary_absent=true\nsurvivors=0\n",
        ownership = plan_argument(plan, "--ownership-id="),
        plan_digest = plan_argument(plan, "--plan-digest="),
        runtime = "a".repeat(64),
        helper = "d".repeat(64),
        image = "0123456789abcdef".repeat(4),
        boundary = plan_argument(plan, "--boundary="),
        invocation = plan.invocation_digest_for_test(nonce),
    )
}

#[test]
fn aggregate_cpu_quota_backed_bind_disk_and_boundary_io_are_in_the_integrated_argv() {
    let fixture = Fixture::new();
    let plan = plan(&fixture);
    let args = plan
        .arguments()
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>();
    for required in [
        "--cpu-aggregate-ms=1500",
        "--writable-bind-quota=2048",
        "--boundary-io-bytes=4096",
        "--memory-bytes=67108864",
        "--pids=16",
        "--file-bytes=1024",
        "--wall-time-ms=250",
    ] {
        assert!(
            args.iter().any(|argument| argument == required),
            "missing {required}"
        );
    }
    assert!(!args.iter().any(|argument| argument.starts_with("cpu=")));
    assert!(!args.iter().any(|argument| argument == "--storage-opt"));
}

#[test]
fn aggregate_quota_io_kill_and_mount_lease_evidence_cannot_be_caller_asserted() {
    for (name, primitive) in [
        ("cpu_aggregate", EnforcementPrimitive::CpuAggregate),
        ("quota_backed_binds", EnforcementPrimitive::QuotaBackedBinds),
        ("boundary_io", EnforcementPrimitive::BoundaryIo),
        (
            "whole_boundary_kill",
            EnforcementPrimitive::WholeBoundaryKill,
        ),
        (
            "quiescence_inspect",
            EnforcementPrimitive::QuiescenceInspect,
        ),
        ("pinned_mounts", EnforcementPrimitive::PinnedMounts),
        (
            "secret_fd_injection",
            EnforcementPrimitive::SecretFileDescriptor,
        ),
        (
            "secret_memfd_injection",
            EnforcementPrimitive::SecretMemoryFile,
        ),
        (
            "secret_scoped_env_injection",
            EnforcementPrimitive::SecretScopedEnvironment,
        ),
    ] {
        let incomplete = CAPABILITIES
            .split(',')
            .filter(|capability| *capability != name)
            .collect::<Vec<_>>()
            .join(",");
        let transcript = format!(
            "protocol=kit-container-v1\nruntime=podman\nruntime_path=/usr/bin/podman\nruntime_identity=sha256:{a}\nruntime_version=sha256:{b}\nruntime_config=sha256:{c}\nhelper_identity=sha256:{d}\nseccomp=builtin\ncapabilities={incomplete}\nproxy_network=kit-proxy-only\nproxy_endpoint=http://proxy:8080\nproxy_lease={e}\n",
            a = "a".repeat(64),
            b = "b".repeat(64),
            c = "c".repeat(64),
            d = "d".repeat(64),
            e = "e".repeat(64),
        );
        assert_eq!(
            ProbeRecord::parse(&transcript),
            Err(EvidenceError::MissingPrimitive(primitive))
        );
    }
}

#[test]
fn unknown_or_unattested_runtime_exit_is_never_typed_success() {
    let fixture = Fixture::new();
    let plan = plan(&fixture);
    assert!(matches!(
        plan.classify_exit_for_test(true, None),
        Err(ExecutionError::OutcomeUnknown { .. })
    ));
    let unknown = monitor_record(&plan, &"f".repeat(64), "mystery", "none", "none");
    assert!(matches!(
        plan.classify_exit_for_test(false, Some(&unknown)),
        Err(ExecutionError::MonitorProtocol(_))
    ));
}

#[test]
fn concrete_identity_bound_monitor_record_maps_to_typed_bound_error() {
    let fixture = Fixture::new();
    let plan = plan(&fixture);
    let record = monitor_record(
        &plan,
        &"f".repeat(64),
        "bound:cpu",
        "1501",
        "cgroup.cpu.stat:usage_usec=1501000",
    );
    assert!(matches!(
        plan.classify_exit_for_test(false, Some(&record)),
        Err(ExecutionError::Bound(ref error))
            if error.resource == ResourceIdentity::Cpu
                && error.observed == Some(1501)
                && !error.monitor_evidence.is_empty()
    ));
}

#[test]
fn attested_nonzero_and_signal_completions_are_not_outcome_unknown() {
    let fixture = Fixture::new();
    let plan = plan(&fixture);
    let nonce = "1".repeat(64);
    let exited = monitor_record(&plan, &nonce, "exit:17", "none", "runtime-wait-status");
    assert!(matches!(
        plan.classify_completion_for_test(Some(17), None, &nonce, Some(&exited)),
        Ok(kit::executor::backends::container::ExecutionReport {
            outcome: kit::executor::backends::container::ExecutionOutcome::Exit(17),
            ..
        })
    ));
    let signaled = monitor_record(&plan, &nonce, "signal:9", "none", "runtime-wait-status");
    assert!(matches!(
        plan.classify_completion_for_test(None, Some(9), &nonce, Some(&signaled)),
        Ok(kit::executor::backends::container::ExecutionReport {
            outcome: kit::executor::backends::container::ExecutionOutcome::Signal(9),
            ..
        })
    ));
}

#[test]
fn replayed_or_mismatched_monitor_record_is_typed_rejected() {
    let fixture = Fixture::new();
    let plan = plan(&fixture);
    let expected_nonce = "1".repeat(64);
    let replay = monitor_record(
        &plan,
        &"2".repeat(64),
        "success",
        "none",
        "runtime-wait-status",
    );
    assert!(matches!(
        plan.classify_completion_for_test(Some(0), None, &expected_nonce, Some(&replay)),
        Err(ExecutionError::InvocationMismatch { .. })
    ));
}

#[test]
fn helper_report_surfaces_and_binds_runtime_storage_evidence() {
    let fixture = Fixture::new();
    let plan = plan(&fixture);
    let nonce = "3".repeat(64);
    let record = monitor_record(&plan, &nonce, "success", "none", "runtime-wait-status");
    let report = plan
        .classify_completion_for_test(Some(0), None, &nonce, Some(&record))
        .unwrap();
    assert_eq!(
        report.evidence.resolved_image_digest,
        format!("sha256:{}", "0123456789abcdef".repeat(4))
    );
    assert_eq!(report.evidence.instance_id, "instance-1");
    assert_eq!(report.evidence.rootfs_lease_id, "rootfs-1");
    assert_eq!(report.evidence.writable_lease_id, "writable-1");
    assert!(report.evidence.quiescent);

    let mismatched = record.replace(
        &format!(
            "resolved_image_digest=sha256:{}",
            "0123456789abcdef".repeat(4)
        ),
        &format!("resolved_image_digest=sha256:{}", "f".repeat(64)),
    );
    assert!(matches!(
        plan.classify_completion_for_test(Some(0), None, &nonce, Some(&mismatched)),
        Err(ExecutionError::InvocationMismatch { .. })
    ));
}

#[test]
fn formatter_measurements_are_all_or_nothing_helper_evidence() {
    let fixture = Fixture::new();
    let plan = plan(&fixture);
    let nonce = "4".repeat(64);
    let mut record = monitor_record(&plan, &nonce, "success", "none", "runtime-wait-status");
    record.push_str(&format!(
        "formatter_binary_digest=blake3:{binary}\nformatter_config_digest=sha256:{config}\nformatter_artifact_digest=sha256:{artifact}\n",
        binary = "a".repeat(64),
        config = "b".repeat(64),
        artifact = "c".repeat(64),
    ));
    let report = plan
        .classify_completion_for_test(Some(0), None, &nonce, Some(&record))
        .unwrap();
    assert_eq!(
        report.evidence.formatter_binary_digest.as_deref(),
        Some(format!("blake3:{}", "a".repeat(64)).as_str())
    );

    let partial = record
        .lines()
        .filter(|line| !line.starts_with("formatter_config_digest="))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    assert!(matches!(
        plan.classify_completion_for_test(Some(0), None, &nonce, Some(&partial)),
        Err(ExecutionError::MonitorProtocol(_))
    ));
}

struct FakeControl {
    kill_result: io::Result<bool>,
    state: io::Result<BoundaryState>,
    cli_killed: bool,
    inspected: bool,
}

impl BoundaryControl for FakeControl {
    fn kill_boundary(&mut self, _deadline: Instant) -> io::Result<bool> {
        self.kill_result
            .as_ref()
            .map(|value| *value)
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))
    }

    fn kill_cli(&mut self, _deadline: Instant) -> io::Result<()> {
        self.cli_killed = true;
        Ok(())
    }

    fn inspect(&mut self, _deadline: Instant) -> io::Result<BoundaryState> {
        self.inspected = true;
        self.state
            .as_ref()
            .map(|value| *value)
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))
    }
}

fn pending() -> BoundError {
    BoundError::new(
        ResourceIdentity::WallTime,
        250,
        Some(251),
        "parent-monotonic-clock",
    )
}

#[test]
fn kill_start_failure_still_kills_cli_and_can_only_return_unknown_or_not_quiescent() {
    let mut control = FakeControl {
        kill_result: Err(io::Error::other("cannot start kill")),
        state: Ok(BoundaryState {
            boundary_absent: true,
            survivors: 0,
        }),
        cli_killed: false,
        inspected: false,
    };
    assert!(matches!(
        terminate_after_violation(&mut control, pending()),
        Err(ExecutionError::OutcomeUnknown { .. })
    ));
    assert!(control.cli_killed);
    assert!(control.inspected);

    let mut survivors = FakeControl {
        kill_result: Ok(false),
        state: Ok(BoundaryState {
            boundary_absent: false,
            survivors: 3,
        }),
        cli_killed: false,
        inspected: false,
    };
    assert!(matches!(
        terminate_after_violation(&mut survivors, pending()),
        Err(ExecutionError::NotQuiescent { survivors: 3, .. })
    ));
    assert!(survivors.cli_killed && survivors.inspected);
}

#[test]
fn timed_out_boundary_control_is_unknown_after_cli_kill_and_inspection() {
    let mut control = FakeControl {
        kill_result: Err(io::Error::new(io::ErrorKind::TimedOut, "wedged helper")),
        state: Ok(BoundaryState {
            boundary_absent: true,
            survivors: 0,
        }),
        cli_killed: false,
        inspected: false,
    };
    assert!(matches!(
        terminate_after_violation(&mut control, pending()),
        Err(ExecutionError::OutcomeUnknown { .. })
    ));
    assert!(control.cli_killed && control.inspected);
}

#[test]
fn successful_kill_is_not_a_bound_result_until_inspect_proves_zero_survivors() {
    let mut control = FakeControl {
        kill_result: Ok(true),
        state: Ok(BoundaryState {
            boundary_absent: false,
            survivors: 1,
        }),
        cli_killed: false,
        inspected: false,
    };
    assert!(matches!(
        terminate_after_violation(&mut control, pending()),
        Err(ExecutionError::NotQuiescent { survivors: 1, .. })
    ));
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the trusted helper and immutable limit probe image"]
fn explicitly_requested_actual_bounds_require_all_external_evidence() {
    use kit::executor::backends::container::prepare;

    let image = std::env::var("KIT_CONTAINER_LIMIT_PROBE_IMAGE")
        .expect("KIT_CONTAINER_LIMIT_PROBE_IMAGE must be set when this ignored test is requested");
    let fixture = Fixture::new();
    for resource in [
        ResourceIdentity::Cpu,
        ResourceIdentity::Memory,
        ResourceIdentity::Pids,
        ResourceIdentity::File,
        ResourceIdentity::Disk,
        ResourceIdentity::Io,
        ResourceIdentity::WallTime,
    ] {
        let plan = prepare(
            &profile(),
            &fixture.workspace,
            &fixture.build,
            &fixture.temp,
            &format!("limit-{}", resource.name()),
            &image,
            ["/kit-limit-probe", "violate", resource.name()],
        )
        .expect("trusted aggregate/quota/I/O/kill evidence is mandatory");
        assert!(matches!(
            plan.run(ProcessOwnership::DaemonService(
                DaemonServiceId::generate().unwrap()
            )),
            Err(ExecutionError::Bound(ref error)) if error.resource == resource
        ));
    }
}
