use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(target_os = "linux")]
use kit::domain::{ids::DaemonServiceId, lifecycle::ProcessOwnership};
use kit::{
    executor::{
        backends::container::{
            ContainerError,
            limits::{EnforcementPrimitive, EvidenceError, NotAvailableReason, ProbeRecord},
            preview,
        },
        profile::{
            Architecture, EgressGrant, EgressTransport, ExecutorProfile, Platform, ProfileSpec,
            ResourceLimits, TrustTier,
        },
    },
    workspace::acquire::{
        AcquisitionMode, AcquisitionRequest, AcquisitionResult, OwnerId, WorkspaceId, WriterPolicy,
        acquire,
    },
};

const IMAGE: &str =
    "example.invalid/net@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const CAPABILITIES: &str = "rootless,seccomp,no_new_privileges,capability_drop,network_deny,proxy_only_network,dns_revalidation,connection_revalidation,peer_deny,gateway_deny,udp_deny,cpu_aggregate,memory,pids,file_size,quota_backed_binds,boundary_io,whole_boundary_kill,quiescence_inspect,pinned_mounts,secret_fd_injection,secret_memfd_injection,secret_scoped_env_injection";
static NEXT: AtomicU64 = AtomicU64::new(0);

fn transcript(capabilities: &str) -> String {
    format!(
        "protocol=kit-container-v1\nruntime=podman\nruntime_path=/usr/bin/podman\nruntime_identity=sha256:{a}\nruntime_version=sha256:{b}\nruntime_config=sha256:{c}\nhelper_identity=sha256:{d}\nseccomp=builtin\ncapabilities={capabilities}\nproxy_network=kit-proxy-only\nproxy_endpoint=http://proxy:8080\nproxy_lease={e}\n",
        a = "a".repeat(64),
        b = "b".repeat(64),
        c = "c".repeat(64),
        d = "d".repeat(64),
        e = "e".repeat(64),
    )
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
            "kit-container-net-{}-{}",
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
            WorkspaceId::new(format!("network-{}", NEXT.fetch_add(1, Ordering::Relaxed))).unwrap(),
            OwnerId::new("container-net").unwrap(),
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

fn profile(grants: &[(&str, u16, EgressTransport)]) -> ExecutorProfile {
    let mut spec = ProfileSpec::isolated(
        TrustTier::Restricted,
        Platform::Linux,
        if cfg!(target_arch = "aarch64") {
            Architecture::Aarch64
        } else {
            Architecture::X86_64
        },
        ResourceLimits::new(1_000, 64 << 20, 16, 1024, 2048, 4096, 8192, 1_000),
    );
    for (destination, port, transport) in grants {
        spec.egress
            .insert(EgressGrant::new(*destination, *port, *transport).unwrap());
    }
    ExecutorProfile::new(spec).unwrap()
}

#[test]
fn metadata_private_and_local_destinations_cannot_be_granted() {
    for destination in [
        "0.0.0.0",
        "10.0.0.1",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "192.168.0.1",
        "::",
        "::1",
        "fc00::1",
        "fe80::1",
        "localhost",
        "service.localhost",
        "metadata.google.internal",
        "metadata.aws.internal",
        "instance-data.ec2.internal",
    ] {
        assert!(
            EgressGrant::new(destination, 443, EgressTransport::Tcp).is_err(),
            "unsafe destination was grantable: {destination}"
        );
    }
}

#[test]
fn proxy_evidence_must_prove_direct_peer_gateway_udp_and_rebinding_denials() {
    for (name, primitive) in [
        ("proxy_only_network", EnforcementPrimitive::ProxyOnlyNetwork),
        ("dns_revalidation", EnforcementPrimitive::DnsRevalidation),
        (
            "connection_revalidation",
            EnforcementPrimitive::ConnectionRevalidation,
        ),
        ("peer_deny", EnforcementPrimitive::PeerDeny),
        ("gateway_deny", EnforcementPrimitive::GatewayDeny),
        ("udp_deny", EnforcementPrimitive::UdpDeny),
    ] {
        let incomplete = CAPABILITIES
            .split(',')
            .filter(|capability| *capability != name)
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            ProbeRecord::parse(&transcript(&incomplete)),
            Err(EvidenceError::MissingPrimitive(primitive)),
            "caller-controlled proxy variables substituted for {name} evidence"
        );
    }
}

#[test]
fn granted_network_uses_an_operational_proxy_only_contract_not_just_environment_hints() {
    let fixture = Fixture::new();
    let evidence = ProbeRecord::parse(&transcript(CAPABILITIES)).unwrap();
    let plan = preview(
        &evidence,
        &profile(&[("packages.example", 443, EgressTransport::Tcp)]),
        &fixture.workspace,
        &fixture.build,
        &fixture.temp,
        "proxy-contract",
        IMAGE,
        ["true"],
    )
    .unwrap();
    let args = plan
        .arguments()
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>();
    for required in [
        "--network-policy=proxy-only",
        "--deny-direct-egress",
        "--deny-peer",
        "--deny-gateway",
        "--deny-udp",
        "--revalidate-dns",
        "--revalidate-connections",
        "--grant=tcp:packages.example:443",
        "--network=kit-proxy-only",
        "--env=HTTP_PROXY=http://proxy:8080",
        "--env=NO_PROXY=",
    ] {
        assert!(
            args.iter().any(|argument| argument == required),
            "missing {required}"
        );
    }
    assert!(!args.iter().any(|argument| argument == "--network=none"));
}

#[test]
fn udp_grants_fail_typed_unavailable_instead_of_bypassing_the_http_proxy() {
    let fixture = Fixture::new();
    let evidence = ProbeRecord::parse(&transcript(CAPABILITIES)).unwrap();
    assert!(matches!(
        preview(
            &evidence,
            &profile(&[("dns.example", 53, EgressTransport::Udp)]),
            &fixture.workspace,
            &fixture.build,
            &fixture.temp,
            "udp-refused",
            IMAGE,
            ["true"],
        ),
        Err(ContainerError::NotAvailable(ref unavailable))
            if unavailable.reason == NotAvailableReason::EgressUnavailable
    ));
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires the trusted helper and a purpose-built immutable socket probe image"]
fn explicitly_requested_direct_socket_bypass_test_cannot_skip_missing_evidence() {
    use kit::executor::backends::container::prepare;
    use std::ffi::OsString;

    let image = std::env::var("KIT_CONTAINER_NET_PROBE_IMAGE")
        .expect("KIT_CONTAINER_NET_PROBE_IMAGE must be set when this ignored test is requested");
    let allowed_host = std::env::var("KIT_CONTAINER_NET_ALLOWED_HOST")
        .expect("KIT_CONTAINER_NET_ALLOWED_HOST must name the externally gated TCP endpoint");
    let allowed_port = std::env::var("KIT_CONTAINER_NET_ALLOWED_PORT")
        .expect("KIT_CONTAINER_NET_ALLOWED_PORT must name the externally gated TCP port")
        .parse::<u16>()
        .expect("KIT_CONTAINER_NET_ALLOWED_PORT must be a non-zero u16");
    assert_ne!(
        allowed_port, 0,
        "KIT_CONTAINER_NET_ALLOWED_PORT must be non-zero"
    );
    let fixture = Fixture::new();
    let plan = prepare(
        &profile(&[(&allowed_host, allowed_port, EgressTransport::Tcp)]),
        &fixture.workspace,
        &fixture.build,
        &fixture.temp,
        "proxy-only-actual",
        &image,
        [
            OsString::from("/kit-net-probe"),
            OsString::from("verify-proxy-only"),
            OsString::from(&allowed_host),
            OsString::from(allowed_port.to_string()),
            OsString::from("/build/net-probe-report"),
        ],
    )
    .expect("trusted proxy-only plan and helper evidence are mandatory");
    plan.run(ProcessOwnership::DaemonService(
        DaemonServiceId::generate().unwrap(),
    ))
    .expect("the complete proxy-only network probe must finish successfully");
    let expected = format!(
        "protocol=kit-net-probe-v1\nauthorized_proxy_tcp={allowed_host}:{allowed_port}:allowed\ndirect_metadata_tcp=denied\ndirect_arbitrary_tcp=denied\ndirect_peer_tcp=denied\ndirect_gateway_tcp=denied\ndirect_arbitrary_udp=denied\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.build.join("net-probe-report")).unwrap(),
        expected
    );
}
