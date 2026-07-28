use std::{
    collections::BTreeSet,
    ffi::OsStr,
    fmt,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::executor::process::tree::BoundaryIdentity;

#[cfg(target_os = "linux")]
use serde_json::Value;
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
#[cfg(windows)]
use std::process::Child;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation},
    Security::SECURITY_ATTRIBUTES,
    System::Pipes::CreatePipe,
};

#[cfg(windows)]
trait BoundedRead: Read + AsRawHandle + Send + 'static {}
#[cfg(windows)]
impl<T: Read + AsRawHandle + Send + 'static> BoundedRead for T {}

#[cfg(not(windows))]
trait BoundedRead: Read + Send + 'static {}
#[cfg(not(windows))]
impl<T: Read + Send + 'static> BoundedRead for T {}

pub(crate) const HELPER_PROTOCOL: &str = "kit-container-v1";
pub(crate) const HELPER_PATH: &str = "/usr/libexec/kit-container-helper";
#[cfg(target_os = "linux")]
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const PROBE_OUTPUT_LIMIT: usize = 1024 * 1024;

pub const fn helper_path() -> &'static str {
    HELPER_PATH
}

pub const fn helper_protocol() -> &'static str {
    HELPER_PROTOCOL
}

#[derive(Clone, Debug)]
pub(crate) struct ControlIdentity {
    boundary: String,
    ownership_id: String,
    plan_digest: String,
    runtime_identity: String,
    helper_identity: String,
    invocation_digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ControlEvidence {
    pub(crate) boundary_absent: bool,
    pub(crate) survivors: u32,
    pub(crate) direct_child_reaped: bool,
}

impl ControlIdentity {
    pub(crate) fn from_boundary(identity: &BoundaryIdentity) -> io::Result<Self> {
        let mut start = identity.start_identity().split('|');
        let (
            Some(plan_digest),
            Some(runtime_identity),
            Some(helper_identity),
            Some(invocation_digest),
            None,
        ) = (
            start.next(),
            start.next(),
            start.next(),
            start.next(),
            start.next(),
        )
        else {
            return Err(io::Error::other(
                "invalid persisted container helper identity",
            ));
        };
        Ok(Self {
            boundary: identity.locator().to_owned(),
            ownership_id: identity.ownership_token().to_owned(),
            plan_digest: plan_digest.to_owned(),
            runtime_identity: runtime_identity.to_owned(),
            helper_identity: helper_identity.to_owned(),
            invocation_digest: invocation_digest.to_owned(),
        })
    }

    pub(crate) fn arguments(&self, operation: &str) -> Vec<String> {
        vec![
            operation.to_owned(),
            format!("--protocol={HELPER_PROTOCOL}"),
            format!("--boundary={}", self.boundary),
            format!("--ownership-id={}", self.ownership_id),
            format!("--plan-digest={}", self.plan_digest),
            format!("--runtime-identity={}", self.runtime_identity),
            format!("--helper-identity={}", self.helper_identity),
            format!("--invocation-digest={}", self.invocation_digest),
        ]
    }

    pub(crate) fn parse_evidence(&self, transcript: &str) -> io::Result<ControlEvidence> {
        let mut fields = std::collections::BTreeMap::new();
        for line in transcript.lines() {
            let (name, value) = line
                .split_once('=')
                .ok_or_else(|| io::Error::other("invalid container helper control evidence"))?;
            if !matches!(
                name,
                "protocol"
                    | "ownership_id"
                    | "plan_digest"
                    | "runtime_identity"
                    | "helper_identity"
                    | "invocation_digest"
                    | "boundary_absent"
                    | "survivors"
                    | "direct_child_reaped"
                    | "supervisor_state"
            ) || fields.insert(name, value).is_some()
            {
                return Err(io::Error::other(
                    "invalid container helper control evidence",
                ));
            }
        }
        let field = |name| {
            fields
                .get(name)
                .copied()
                .ok_or_else(|| io::Error::other("missing container helper control evidence"))
        };
        if field("protocol")? != HELPER_PROTOCOL
            || field("ownership_id")? != self.ownership_id
            || field("plan_digest")? != self.plan_digest
            || field("runtime_identity")? != self.runtime_identity
            || field("helper_identity")? != self.helper_identity
            || field("invocation_digest")? != self.invocation_digest
        {
            return Err(io::Error::other(
                "container helper control identity mismatch",
            ));
        }
        let boundary_absent = parse_bool(field("boundary_absent")?)?;
        let survivors = field("survivors")?
            .parse()
            .map_err(|_| io::Error::other("invalid container helper survivor count"))?;
        let direct_child_reaped = fields
            .get("direct_child_reaped")
            .map(|value| parse_bool(value))
            .transpose()?
            .unwrap_or(false)
            || fields.get("supervisor_state").copied() == Some("adopted_gone");
        Ok(ControlEvidence {
            boundary_absent,
            survivors,
            direct_child_reaped,
        })
    }
}

fn parse_bool(value: &str) -> io::Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(io::Error::other(
            "invalid container helper boolean evidence",
        )),
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ContainerRuntime {
    Podman,
    Docker,
}

impl ContainerRuntime {
    pub const fn executable(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Docker => "docker",
        }
    }

    #[cfg(target_os = "linux")]
    const fn path(self) -> &'static str {
        match self {
            Self::Podman => "/usr/bin/podman",
            Self::Docker => "/usr/bin/docker",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "podman" => Some(Self::Podman),
            "docker" => Some(Self::Docker),
            _ => None,
        }
    }
}

impl fmt::Display for ContainerRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.executable())
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EnforcementPrimitive {
    Rootless,
    Seccomp,
    NoNewPrivileges,
    CapabilityDrop,
    NetworkDeny,
    ProxyOnlyNetwork,
    DnsRevalidation,
    ConnectionRevalidation,
    PeerDeny,
    GatewayDeny,
    UdpDeny,
    CpuAggregate,
    Memory,
    Pids,
    FileSize,
    QuotaBackedBinds,
    BoundaryIo,
    WholeBoundaryKill,
    QuiescenceInspect,
    PinnedMounts,
    SecretFileDescriptor,
    SecretMemoryFile,
    SecretScopedEnvironment,
}

impl EnforcementPrimitive {
    const ALL: [Self; 23] = [
        Self::Rootless,
        Self::Seccomp,
        Self::NoNewPrivileges,
        Self::CapabilityDrop,
        Self::NetworkDeny,
        Self::ProxyOnlyNetwork,
        Self::DnsRevalidation,
        Self::ConnectionRevalidation,
        Self::PeerDeny,
        Self::GatewayDeny,
        Self::UdpDeny,
        Self::CpuAggregate,
        Self::Memory,
        Self::Pids,
        Self::FileSize,
        Self::QuotaBackedBinds,
        Self::BoundaryIo,
        Self::WholeBoundaryKill,
        Self::QuiescenceInspect,
        Self::PinnedMounts,
        Self::SecretFileDescriptor,
        Self::SecretMemoryFile,
        Self::SecretScopedEnvironment,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Rootless => "rootless",
            Self::Seccomp => "seccomp",
            Self::NoNewPrivileges => "no_new_privileges",
            Self::CapabilityDrop => "capability_drop",
            Self::NetworkDeny => "network_deny",
            Self::ProxyOnlyNetwork => "proxy_only_network",
            Self::DnsRevalidation => "dns_revalidation",
            Self::ConnectionRevalidation => "connection_revalidation",
            Self::PeerDeny => "peer_deny",
            Self::GatewayDeny => "gateway_deny",
            Self::UdpDeny => "udp_deny",
            Self::CpuAggregate => "cpu_aggregate",
            Self::Memory => "memory",
            Self::Pids => "pids",
            Self::FileSize => "file_size",
            Self::QuotaBackedBinds => "quota_backed_binds",
            Self::BoundaryIo => "boundary_io",
            Self::WholeBoundaryKill => "whole_boundary_kill",
            Self::QuiescenceInspect => "quiescence_inspect",
            Self::PinnedMounts => "pinned_mounts",
            Self::SecretFileDescriptor => "secret_fd_injection",
            Self::SecretMemoryFile => "secret_memfd_injection",
            Self::SecretScopedEnvironment => "secret_scoped_env_injection",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|primitive| primitive.name() == value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotAvailableReason {
    UnsupportedHost,
    HelperMissing,
    HelperUntrusted,
    RuntimeMissing,
    RuntimeProbeFailed,
    MalformedEvidence,
    IdentityMismatch,
    PrimitiveMissing,
    EgressUnavailable,
    UntrustedTestEvidence,
    MountLeaseUnavailable,
    CredentialBrokerUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotAvailable {
    pub reason: NotAvailableReason,
    pub detail: String,
}

impl NotAvailable {
    pub(crate) fn new(reason: NotAvailableReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for NotAvailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "container backend not available: {}",
            self.detail
        )
    }
}

impl std::error::Error for NotAvailable {}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CapabilityRecord {
    runtime: ContainerRuntime,
    runtime_path: PathBuf,
    runtime_identity: String,
    runtime_version: String,
    runtime_config: String,
    helper_identity: String,
    seccomp: String,
    capabilities: BTreeSet<EnforcementPrimitive>,
    proxy_network: String,
    proxy_endpoint: String,
    proxy_lease: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbeRecord(CapabilityRecord);

impl ProbeRecord {
    /// Parses helper output for deterministic protocol/argv tests. A record parsed here is
    /// deliberately not trusted and can never be executed.
    pub fn parse(transcript: &str) -> Result<Self, EvidenceError> {
        parse_record(transcript).map(Self)
    }

    pub const fn runtime(&self) -> ContainerRuntime {
        self.0.runtime
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeEvidence(CapabilityRecord);

impl RuntimeEvidence {
    pub(crate) const fn runtime(&self) -> ContainerRuntime {
        self.0.runtime
    }

    pub(crate) fn runtime_path(&self) -> &Path {
        &self.0.runtime_path
    }

    pub(crate) fn runtime_identity(&self) -> &str {
        &self.0.runtime_identity
    }

    pub(crate) fn runtime_version(&self) -> &str {
        &self.0.runtime_version
    }

    pub(crate) fn runtime_config(&self) -> &str {
        &self.0.runtime_config
    }

    pub(crate) fn helper_identity(&self) -> &str {
        &self.0.helper_identity
    }

    pub(crate) fn seccomp(&self) -> &str {
        &self.0.seccomp
    }

    pub(crate) fn proxy_network(&self) -> &str {
        &self.0.proxy_network
    }

    pub(crate) fn proxy_endpoint(&self) -> &str {
        &self.0.proxy_endpoint
    }

    pub(crate) fn proxy_lease(&self) -> &str {
        &self.0.proxy_lease
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvidenceError {
    Missing(&'static str),
    Duplicate(String),
    UnknownField(String),
    Invalid(&'static str),
    MissingPrimitive(EnforcementPrimitive),
}

impl fmt::Display for EvidenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing evidence field {field}"),
            Self::Duplicate(field) => write!(formatter, "duplicate evidence field {field}"),
            Self::UnknownField(field) => write!(formatter, "unknown evidence field {field}"),
            Self::Invalid(field) => write!(formatter, "invalid evidence field {field}"),
            Self::MissingPrimitive(primitive) => {
                write!(formatter, "missing enforcement primitive {primitive:?}")
            }
        }
    }
}

impl std::error::Error for EvidenceError {}

pub(crate) fn probe_backend() -> Result<RuntimeEvidence, NotAvailable> {
    if !cfg!(target_os = "linux") {
        return Err(NotAvailable::new(
            NotAvailableReason::UnsupportedHost,
            "the trusted container helper requires Linux",
        ));
    }
    probe_linux()
}

#[cfg(target_os = "linux")]
fn probe_linux() -> Result<RuntimeEvidence, NotAvailable> {
    let helper = Path::new(HELPER_PATH);
    trusted_root_owned_file(helper).map_err(|error| {
        NotAvailable::new(
            if error.kind() == io::ErrorKind::NotFound {
                NotAvailableReason::HelperMissing
            } else {
                NotAvailableReason::HelperUntrusted
            },
            format!("{HELPER_PATH}: {error}"),
        )
    })?;
    let helper_identity = file_identity(helper).map_err(|error| {
        NotAvailable::new(NotAvailableReason::HelperUntrusted, error.to_string())
    })?;

    let mut failures = Vec::new();
    for runtime in [ContainerRuntime::Podman, ContainerRuntime::Docker] {
        match probe_runtime(helper, &helper_identity, runtime) {
            Ok(evidence) => return Ok(RuntimeEvidence(evidence)),
            Err(error) => failures.push((runtime, error)),
        }
    }
    let reason = if failures
        .iter()
        .all(|(_, error)| error.reason == NotAvailableReason::RuntimeMissing)
    {
        NotAvailableReason::RuntimeMissing
    } else {
        NotAvailableReason::RuntimeProbeFailed
    };
    Err(NotAvailable::new(
        reason,
        failures
            .iter()
            .map(|(runtime, error)| format!("{runtime} [{:?}]: {}", error.reason, error.detail))
            .collect::<Vec<_>>()
            .join("; "),
    ))
}

#[cfg(not(target_os = "linux"))]
fn probe_linux() -> Result<RuntimeEvidence, NotAvailable> {
    unreachable!("guarded by target check")
}

#[cfg(target_os = "linux")]
fn trusted_root_owned_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "must be a root-owned regular file not writable by group or other",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn probe_runtime(
    helper: &Path,
    helper_identity: &str,
    runtime: ContainerRuntime,
) -> Result<CapabilityRecord, NotAvailable> {
    let runtime_path = Path::new(runtime.path());
    trusted_root_owned_file(runtime_path).map_err(|error| {
        NotAvailable::new(
            if error.kind() == io::ErrorKind::NotFound {
                NotAvailableReason::RuntimeMissing
            } else {
                NotAvailableReason::RuntimeProbeFailed
            },
            format!("{}: {error}", runtime_path.display()),
        )
    })?;
    let runtime_identity = file_identity(runtime_path).map_err(|error| {
        NotAvailable::new(NotAvailableReason::RuntimeProbeFailed, error.to_string())
    })?;
    let info_args: &[&str] = match runtime {
        ContainerRuntime::Podman => &["info", "--format=json"],
        ContainerRuntime::Docker => &["info", "--format={{json .}}"],
    };
    let version_args: &[&str] = match runtime {
        ContainerRuntime::Podman => &["version", "--format=json"],
        ContainerRuntime::Docker => &["version", "--format={{json .}}"],
    };
    let info = checked_output(runtime_path, info_args)?;
    validate_rootless_seccomp(runtime, &info)?;
    let version = checked_output(runtime_path, version_args)?;
    let runtime_config = bytes_identity(&info);
    let runtime_version = bytes_identity(&version);

    let output = bounded_output(
        helper,
        [
            "probe",
            "--protocol",
            HELPER_PROTOCOL,
            "--runtime",
            runtime_path.to_str().expect("fixed runtime path is UTF-8"),
            "--runtime-identity",
            &runtime_identity,
            "--runtime-version",
            &runtime_version,
            "--runtime-config",
            &runtime_config,
        ],
        Instant::now() + PROBE_TIMEOUT,
        PROBE_OUTPUT_LIMIT,
    )
    .map_err(|error| {
        NotAvailable::new(NotAvailableReason::RuntimeProbeFailed, error.to_string())
    })?;
    if !output.status.success() {
        return Err(NotAvailable::new(
            NotAvailableReason::RuntimeProbeFailed,
            bounded_diagnostic(&output.stderr),
        ));
    }
    let transcript = String::from_utf8(output.stdout).map_err(|_| {
        NotAvailable::new(
            NotAvailableReason::MalformedEvidence,
            "helper evidence is not UTF-8",
        )
    })?;
    let record = parse_record(&transcript).map_err(|error| {
        let reason = if matches!(error, EvidenceError::MissingPrimitive(_)) {
            NotAvailableReason::PrimitiveMissing
        } else {
            NotAvailableReason::MalformedEvidence
        };
        NotAvailable::new(reason, error.to_string())
    })?;
    if record.runtime != runtime
        || record.runtime_path != runtime_path
        || record.runtime_identity != runtime_identity
        || record.runtime_version != runtime_version
        || record.runtime_config != runtime_config
        || record.helper_identity != helper_identity
    {
        return Err(NotAvailable::new(
            NotAvailableReason::IdentityMismatch,
            "helper evidence is not bound to the probed helper/runtime/version/config",
        ));
    }
    Ok(record)
}

#[cfg(target_os = "linux")]
fn checked_output(program: &Path, args: &[&str]) -> Result<Vec<u8>, NotAvailable> {
    let output = bounded_output(
        program,
        args,
        Instant::now() + PROBE_TIMEOUT,
        PROBE_OUTPUT_LIMIT,
    )
    .map_err(|error| {
        NotAvailable::new(NotAvailableReason::RuntimeProbeFailed, error.to_string())
    })?;
    if !output.status.success() {
        return Err(NotAvailable::new(
            NotAvailableReason::RuntimeProbeFailed,
            bounded_diagnostic(&output.stderr),
        ));
    }
    Ok(output.stdout)
}

pub(crate) struct BoundedOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) stderr: Vec<u8>,
}

pub(crate) fn bounded_output<I, S>(
    program: &Path,
    args: I,
    deadline: Instant,
    limit: usize,
) -> io::Result<BoundedOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "command deadline elapsed before spawn",
        ));
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_group(&mut command);
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("cannot capture command stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("cannot capture command stderr"))?;
    if let Err(error) = set_nonblocking(&stdout).and_then(|()| set_nonblocking(&stderr)) {
        terminate_and_reap(&mut child);
        return Err(error);
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    let mut stdout = match BoundedReader::start(stdout, limit, deadline, cancelled.clone()) {
        Ok(stdout) => stdout,
        Err(error) => {
            terminate_and_reap(&mut child);
            return Err(error);
        }
    };
    let mut stderr = match BoundedReader::start(stderr, limit, deadline, cancelled.clone()) {
        Ok(stderr) => stderr,
        Err(error) => {
            terminate_and_reap(&mut child);
            cancelled.store(true, Ordering::Release);
            let _ = stdout.join();
            return Err(error);
        }
    };
    let mut status = None;
    let mut stdout_bytes = None;
    let mut stderr_bytes = None;
    let outcome = loop {
        if stdout_bytes.is_none() {
            match stdout.poll() {
                Ok(result) => stdout_bytes = result,
                Err(error) => break Err(error),
            }
        }
        if stderr_bytes.is_none() {
            match stderr.poll() {
                Ok(result) => stderr_bytes = result,
                Err(error) => break Err(error),
            }
        }
        if let Some(Err(error)) = stdout_bytes.as_mut() {
            break Err(io::Error::new(error.kind(), error.to_string()));
        }
        if let Some(Err(error)) = stderr_bytes.as_mut() {
            break Err(io::Error::new(error.kind(), error.to_string()));
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(error) => break Err(error),
            }
        }
        if status.is_some() && stdout_bytes.is_some() && stderr_bytes.is_some() {
            break Ok(());
        }
        if Instant::now() >= deadline {
            break Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
        }
        thread::sleep(Duration::from_millis(2));
    };

    if outcome.is_err() {
        terminate_and_reap(&mut child);
    }
    cancelled.store(true, Ordering::Release);
    let stdout_join = stdout.join();
    let stderr_join = stderr.join();
    outcome?;
    stdout_join??;
    stderr_join??;
    Ok(BoundedOutput {
        status: status.expect("successful bounded output has an exit status"),
        stdout: stdout_bytes
            .expect("successful bounded output has stdout")
            .expect("reader errors are returned above"),
        stderr: stderr_bytes
            .expect("successful bounded output has stderr")
            .expect("reader errors are returned above"),
    })
}

#[cfg(windows)]
pub(crate) fn bounded_output_with_input<I, S>(
    program: &Path,
    args: I,
    input: &[u8],
    deadline: Instant,
    limit: usize,
) -> io::Result<BoundedOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    bounded_output_with_input_inner(program, args, input, 1024 * 1024, None, deadline, limit)
}

pub(crate) fn bounded_output_with_bounded_input<I, S>(
    program: &Path,
    args: I,
    input: &[u8],
    input_limit: usize,
    memory_limit: usize,
    deadline: Instant,
    limit: usize,
) -> io::Result<BoundedOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    bounded_output_with_input_inner(
        program,
        args,
        input,
        input_limit,
        Some(memory_limit),
        deadline,
        limit,
    )
}

fn bounded_output_with_input_inner<I, S>(
    program: &Path,
    args: I,
    input: &[u8],
    input_limit: usize,
    memory_limit: Option<usize>,
    deadline: Instant,
    limit: usize,
) -> io::Result<BoundedOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    if input.len() > input_limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "command input exceeded its bound",
        ));
    }
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "command deadline elapsed before spawn",
        ));
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if memory_limit.is_some() {
        command.env_clear();
        #[cfg(unix)]
        command.current_dir("/");
    }
    configure_process_group(&mut command);
    configure_memory_limit(&mut command, memory_limit);
    let mut child = command.spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("cannot bind command stdin"))?;
    let input = input.to_vec();
    let writer = match thread::Builder::new()
        .name("bounded-command-input".to_owned())
        .spawn(move || stdin.write_all(&input))
    {
        Ok(writer) => writer,
        Err(error) => {
            terminate_and_reap(&mut child);
            return Err(error);
        }
    };
    let output = collect_bounded_child(&mut child, deadline, limit);
    if output.is_err() {
        terminate_and_reap(&mut child);
    }
    writer
        .join()
        .map_err(|_| io::Error::other("bounded input writer panicked"))??;
    output
}

#[cfg(target_os = "linux")]
fn configure_memory_limit(command: &mut Command, memory_limit: Option<usize>) {
    use std::os::unix::process::CommandExt as _;

    let Some(memory_limit) = memory_limit else {
        return;
    };
    // SAFETY: setrlimit is async-signal-safe and the closure uses only copied scalars.
    unsafe {
        command.pre_exec(move || {
            let limit = libc::rlimit {
                rlim_cur: memory_limit as libc::rlim_t,
                rlim_max: memory_limit as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_DATA, &limit) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
}

#[cfg(target_os = "macos")]
fn configure_memory_limit(command: &mut Command, memory_limit: Option<usize>) {
    use std::os::unix::process::CommandExt as _;

    let Some(memory_limit) = memory_limit else {
        return;
    };
    let limit = {
        let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_taskinfo>();
        // SAFETY: info points to size writable bytes for the current process query.
        let read = unsafe {
            libc::proc_pidinfo(
                libc::getpid(),
                libc::PROC_PIDTASKINFO,
                0,
                info.as_mut_ptr().cast(),
                size as i32,
            )
        };
        if read != size as i32 {
            None
        } else {
            // SAFETY: proc_pidinfo reported that it initialized the complete structure.
            let virtual_size = unsafe { info.assume_init() }.pti_virtual_size;
            virtual_size.checked_add(memory_limit as u64)
        }
    };
    // Darwin includes its large shared VM reservation in both limits, so cap growth above the
    // measured inherited baseline rather than installing an invalid sub-baseline limit.
    // SAFETY: setrlimit is async-signal-safe and the closure uses only copied scalars.
    unsafe {
        command.pre_exec(move || {
            let limit = limit.ok_or_else(|| io::Error::from_raw_os_error(libc::EOVERFLOW))?;
            let limit = libc::rlimit {
                rlim_cur: limit as libc::rlim_t,
                rlim_max: limit as libc::rlim_t,
            };
            for resource in [libc::RLIMIT_AS, libc::RLIMIT_DATA] {
                if libc::setrlimit(resource, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn configure_memory_limit(_command: &mut Command, _memory_limit: Option<usize>) {}

#[cfg(windows)]
pub(crate) fn bounded_output_with_secret_input<I, S>(
    program: &Path,
    args: I,
    input: &[u8],
    secret: &[u8],
    deadline: Instant,
    limit: usize,
) -> io::Result<BoundedOutput>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    if input.len() > 1024 * 1024 || secret.len() > 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "helper input exceeds the 1 MiB protocol bound",
        ));
    }
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "command deadline elapsed before spawn",
        ));
    }
    let mut read = std::ptr::null_mut();
    let mut write = std::ptr::null_mut();
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    // SAFETY: both output pointers and the initialized attributes remain valid for the call.
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe returned two owned handles.
    let read = unsafe { OwnedHandle::from_raw_handle(read.cast()) };
    // SAFETY: CreatePipe returned two owned handles.
    let write = unsafe { OwnedHandle::from_raw_handle(write.cast()) };
    // The daemon-side writer must never be inherited by the helper.
    if unsafe { SetHandleInformation(write.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut command = Command::new(program);
    command
        .args(args)
        .arg(format!(
            "--secret-channel-handle={}",
            read.as_raw_handle() as usize
        ))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    // Inheritance is needed only atomically at spawn; retire the parent copy immediately.
    if unsafe { SetHandleInformation(read.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) } == 0 {
        terminate_and_reap(&mut child);
        return Err(io::Error::last_os_error());
    }
    drop(read);
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("cannot bind command stdin"))?;
    let mut channel = std::fs::File::from(write);
    if let Err(error) = stdin
        .write_all(input)
        .and_then(|()| channel.write_all(secret))
    {
        terminate_and_reap(&mut child);
        return Err(error);
    }
    drop(stdin);
    drop(channel);
    collect_bounded_child(&mut child, deadline, limit)
}

fn collect_bounded_child(
    child: &mut std::process::Child,
    deadline: Instant,
    limit: usize,
) -> io::Result<BoundedOutput> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("cannot capture command stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("cannot capture command stderr"))?;
    if let Err(error) = set_nonblocking(&stdout).and_then(|()| set_nonblocking(&stderr)) {
        terminate_and_reap(child);
        return Err(error);
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    let mut stdout = BoundedReader::start(stdout, limit, deadline, cancelled.clone())?;
    let mut stderr = BoundedReader::start(stderr, limit, deadline, cancelled.clone())?;
    let mut status = None;
    let mut stdout_bytes = None;
    let mut stderr_bytes = None;
    let outcome = loop {
        if stdout_bytes.is_none() {
            match stdout.poll() {
                Ok(result) => stdout_bytes = result,
                Err(error) => break Err(error),
            }
        }
        if stderr_bytes.is_none() {
            match stderr.poll() {
                Ok(result) => stderr_bytes = result,
                Err(error) => break Err(error),
            }
        }
        if let Some(Err(error)) = stdout_bytes.as_mut() {
            break Err(io::Error::new(error.kind(), error.to_string()));
        }
        if let Some(Err(error)) = stderr_bytes.as_mut() {
            break Err(io::Error::new(error.kind(), error.to_string()));
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(result) => status = result,
                Err(error) => break Err(error),
            }
        }
        if status.is_some() && stdout_bytes.is_some() && stderr_bytes.is_some() {
            break Ok(());
        }
        if Instant::now() >= deadline {
            break Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
        }
        thread::sleep(Duration::from_millis(2));
    };
    if outcome.is_err() {
        terminate_and_reap(child);
    }
    cancelled.store(true, Ordering::Release);
    let stdout_join = stdout.join();
    let stderr_join = stderr.join();
    outcome?;
    stdout_join??;
    stderr_join??;
    Ok(BoundedOutput {
        status: status.expect("completed child has an exit status"),
        stdout: stdout_bytes
            .expect("completed child has stdout")
            .expect("reader errors are returned above"),
        stderr: stderr_bytes
            .expect("completed child has stderr")
            .expect("reader errors are returned above"),
    })
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: setpgid is async-signal-safe and uses only constant arguments before exec.
    unsafe {
        command.pre_exec(|| {
            if setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn kill_process_group(pid: u32) -> io::Result<()> {
    const SIGKILL: i32 = 9;
    // SAFETY: the child was placed in a process group whose ID is its PID before exec.
    if unsafe { kill(-(pid as i32), SIGKILL) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process groups are unavailable",
    ))
}

#[cfg(unix)]
unsafe extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, signal: i32) -> i32;
}

struct BoundedReader {
    handle: Option<JoinHandle<io::Result<Vec<u8>>>>,
}

impl BoundedReader {
    fn start(
        reader: impl BoundedRead,
        limit: usize,
        deadline: Instant,
        cancelled: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        Ok(Self {
            handle: Some(
                thread::Builder::new()
                    .name("bounded-command-output".to_owned())
                    .spawn(move || read_bounded(reader, limit, deadline, &cancelled))?,
            ),
        })
    }

    fn poll(&mut self) -> io::Result<Option<io::Result<Vec<u8>>>> {
        if self.handle.as_ref().is_none_or(JoinHandle::is_finished) {
            Ok(Some(self.join()?))
        } else {
            Ok(None)
        }
    }

    fn join(&mut self) -> io::Result<io::Result<Vec<u8>>> {
        self.handle
            .take()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| io::Error::other("bounded output reader panicked"))
            })
            .unwrap_or(Ok(Ok(Vec::new())))
    }
}

fn read_bounded(
    mut reader: impl BoundedRead,
    limit: usize,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "command output capture cancelled",
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "command output pipe timed out",
            ));
        }
        match read_bounded_chunk(&mut reader, &mut chunk) {
            Ok(None) => thread::sleep(Duration::from_millis(2)),
            Ok(Some(0)) => return Ok(bytes),
            Ok(Some(read)) if read > limit.saturating_sub(bytes.len()) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "command output exceeded its bound",
                ));
            }
            Ok(Some(read)) => bytes.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(not(windows))]
fn read_bounded_chunk(
    reader: &mut impl BoundedRead,
    chunk: &mut [u8],
) -> io::Result<Option<usize>> {
    reader.read(chunk).map(Some)
}

#[cfg(windows)]
fn read_bounded_chunk(
    reader: &mut impl BoundedRead,
    chunk: &mut [u8],
) -> io::Result<Option<usize>> {
    use windows_sys::Win32::{
        Foundation::{ERROR_BROKEN_PIPE, HANDLE},
        System::Pipes::PeekNamedPipe,
    };

    let mut available = 0;
    // SAFETY: the child pipe handle is live and `available` is writable.
    if unsafe {
        PeekNamedPipe(
            reader.as_raw_handle() as HANDLE,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    } == 0
    {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
            Ok(Some(0))
        } else {
            Err(error)
        };
    }
    if available == 0 {
        return Ok(None);
    }
    let available = usize::try_from(available)
        .unwrap_or(usize::MAX)
        .min(chunk.len());
    reader.read(&mut chunk[..available]).map(Some)
}

fn terminate_and_reap(child: &mut std::process::Child) {
    let _ = kill_process_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn set_nonblocking<T: std::os::fd::AsRawFd>(pipe: &T) -> io::Result<()> {
    const F_GETFL: i32 = 3;
    const F_SETFL: i32 = 4;
    #[cfg(target_os = "linux")]
    const O_NONBLOCK: i32 = 0x800;
    #[cfg(target_os = "macos")]
    const O_NONBLOCK: i32 = 0x4;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    const O_NONBLOCK: i32 = 0x4;
    let descriptor = pipe.as_raw_fd();
    let flags = unsafe { fcntl(descriptor, F_GETFL) };
    if flags < 0 || unsafe { fcntl(descriptor, F_SETFL, flags | O_NONBLOCK) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn set_nonblocking<T>(_pipe: &T) -> io::Result<()> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn set_nonblocking<T>(_pipe: &T) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "bounded output requires nonblocking pipes",
    ))
}

#[cfg(unix)]
unsafe extern "C" {
    fn fcntl(descriptor: i32, command: i32, ...) -> i32;
}

#[cfg(target_os = "linux")]
fn validate_rootless_seccomp(runtime: ContainerRuntime, bytes: &[u8]) -> Result<(), NotAvailable> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        NotAvailable::new(NotAvailableReason::RuntimeProbeFailed, error.to_string())
    })?;
    let proven = match runtime {
        ContainerRuntime::Podman => value.pointer("/host/security").is_some_and(|security| {
            security.get("rootless").and_then(Value::as_bool) == Some(true)
                && security.get("seccompEnabled").and_then(Value::as_bool) == Some(true)
        }),
        ContainerRuntime::Docker => value
            .get("SecurityOptions")
            .and_then(Value::as_array)
            .is_some_and(|options| {
                let options = options.iter().filter_map(Value::as_str).collect::<Vec<_>>();
                options.contains(&"name=rootless")
                    && options.iter().any(|option| {
                        option.starts_with("name=seccomp,profile=")
                            && !option.ends_with("unconfined")
                    })
            }),
    };
    if proven {
        Ok(())
    } else {
        Err(NotAvailable::new(
            NotAvailableReason::RuntimeProbeFailed,
            "runtime did not directly report rootless seccomp enforcement",
        ))
    }
}

fn parse_record(transcript: &str) -> Result<CapabilityRecord, EvidenceError> {
    let mut fields = std::collections::BTreeMap::new();
    for line in transcript.lines() {
        let (name, value) = line.split_once('=').ok_or(EvidenceError::Invalid("line"))?;
        if !matches!(
            name,
            "protocol"
                | "runtime"
                | "runtime_path"
                | "runtime_identity"
                | "runtime_version"
                | "runtime_config"
                | "helper_identity"
                | "seccomp"
                | "capabilities"
                | "proxy_network"
                | "proxy_endpoint"
                | "proxy_lease"
        ) {
            return Err(EvidenceError::UnknownField(name.to_owned()));
        }
        if fields.insert(name, value).is_some() {
            return Err(EvidenceError::Duplicate(name.to_owned()));
        }
    }
    let field = |name: &'static str| {
        fields
            .get(name)
            .copied()
            .ok_or(EvidenceError::Missing(name))
    };
    if field("protocol")? != HELPER_PROTOCOL {
        return Err(EvidenceError::Invalid("protocol"));
    }
    let runtime =
        ContainerRuntime::parse(field("runtime")?).ok_or(EvidenceError::Invalid("runtime"))?;
    let runtime_path = PathBuf::from(field("runtime_path")?);
    if !runtime_path.is_absolute() {
        return Err(EvidenceError::Invalid("runtime_path"));
    }
    let runtime_identity = digest(field("runtime_identity")?, "runtime_identity")?;
    let runtime_version = digest(field("runtime_version")?, "runtime_version")?;
    let runtime_config = digest(field("runtime_config")?, "runtime_config")?;
    let helper_identity = digest(field("helper_identity")?, "helper_identity")?;
    let seccomp = field("seccomp")?.to_owned();
    if seccomp != "builtin" && (!Path::new(&seccomp).is_absolute() || seccomp.contains(',')) {
        return Err(EvidenceError::Invalid("seccomp"));
    }
    let mut capabilities = BTreeSet::new();
    for value in field("capabilities")?.split(',') {
        let primitive =
            EnforcementPrimitive::parse(value).ok_or(EvidenceError::Invalid("capabilities"))?;
        if !capabilities.insert(primitive) {
            return Err(EvidenceError::Invalid("capabilities"));
        }
    }
    for primitive in EnforcementPrimitive::ALL {
        if !capabilities.contains(&primitive) {
            return Err(EvidenceError::MissingPrimitive(primitive));
        }
    }
    let proxy_network = field("proxy_network")?.to_owned();
    let proxy_endpoint = field("proxy_endpoint")?.to_owned();
    let proxy_lease = field("proxy_lease")?.to_owned();
    if !safe_name(&proxy_network)
        || !safe_proxy_endpoint(&proxy_endpoint)
        || !is_hex(&proxy_lease, 64)
    {
        return Err(EvidenceError::Invalid("proxy"));
    }
    Ok(CapabilityRecord {
        runtime,
        runtime_path,
        runtime_identity,
        runtime_version,
        runtime_config,
        helper_identity,
        seccomp,
        capabilities,
        proxy_network,
        proxy_endpoint,
        proxy_lease,
    })
}

fn digest(value: &str, field: &'static str) -> Result<String, EvidenceError> {
    value
        .strip_prefix("sha256:")
        .filter(|value| is_hex(value, 64))
        .map(|_| value.to_owned())
        .ok_or(EvidenceError::Invalid(field))
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn safe_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn safe_proxy_endpoint(endpoint: &str) -> bool {
    let Some(authority) = endpoint.strip_prefix("http://") else {
        return false;
    };
    let Some((host, port)) = authority.rsplit_once(':') else {
        return false;
    };
    safe_name(host) && port.parse::<u16>().is_ok_and(|port| port != 0)
}

#[cfg(target_os = "linux")]
fn file_identity(path: &Path) -> io::Result<String> {
    fs::read(path).map(|bytes| bytes_identity(&bytes))
}

#[cfg(target_os = "linux")]
fn bytes_identity(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(target_os = "linux")]
fn bounded_diagnostic(bytes: &[u8]) -> String {
    let value = String::from_utf8_lossy(bytes);
    let value = value.trim();
    if value.is_empty() {
        "probe exited unsuccessfully".to_owned()
    } else {
        value.chars().take(1024).collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ResourceIdentity {
    Cpu,
    Memory,
    Pids,
    File,
    Disk,
    Io,
    Output,
    WallTime,
}

impl ResourceIdentity {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Pids => "pids",
            Self::File => "file",
            Self::Disk => "disk",
            Self::Io => "io",
            Self::Output => "output",
            Self::WallTime => "wall_time",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        [
            Self::Cpu,
            Self::Memory,
            Self::Pids,
            Self::File,
            Self::Disk,
            Self::Io,
            Self::Output,
            Self::WallTime,
        ]
        .into_iter()
        .find(|resource| resource.name() == value)
    }
}

impl fmt::Display for ResourceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContainerResourceLimits {
    pub cpu_millis: u64,
    pub memory_bytes: u64,
    pub pids: u32,
    pub file_bytes: u64,
    pub disk_bytes: u64,
    pub io_bytes: u64,
    pub output_bytes: u64,
    pub wall_time_millis: u64,
}

impl ContainerResourceLimits {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        cpu_millis: u64,
        memory_bytes: u64,
        pids: u32,
        file_bytes: u64,
        disk_bytes: u64,
        io_bytes: u64,
        output_bytes: u64,
        wall_time_millis: u64,
    ) -> Self {
        Self {
            cpu_millis,
            memory_bytes,
            pids,
            file_bytes,
            disk_bytes,
            io_bytes,
            output_bytes,
            wall_time_millis,
        }
    }

    pub(crate) fn validate(self) -> Result<(), ResourceIdentity> {
        [
            (ResourceIdentity::Cpu, self.cpu_millis),
            (ResourceIdentity::Memory, self.memory_bytes),
            (ResourceIdentity::Pids, u64::from(self.pids)),
            (ResourceIdentity::File, self.file_bytes),
            (ResourceIdentity::Disk, self.disk_bytes),
            (ResourceIdentity::Io, self.io_bytes),
            (ResourceIdentity::Output, self.output_bytes),
            (ResourceIdentity::WallTime, self.wall_time_millis),
        ]
        .into_iter()
        .find_map(|(resource, value)| (value == 0).then_some(resource))
        .map_or(Ok(()), Err)
    }

    pub const fn bound(self, resource: ResourceIdentity) -> u64 {
        match resource {
            ResourceIdentity::Cpu => self.cpu_millis,
            ResourceIdentity::Memory => self.memory_bytes,
            ResourceIdentity::Pids => self.pids as u64,
            ResourceIdentity::File => self.file_bytes,
            ResourceIdentity::Disk => self.disk_bytes,
            ResourceIdentity::Io => self.io_bytes,
            ResourceIdentity::Output => self.output_bytes,
            ResourceIdentity::WallTime => self.wall_time_millis,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundError {
    pub resource: ResourceIdentity,
    pub limit: u64,
    pub observed: Option<u64>,
    pub monitor_evidence: String,
}

impl BoundError {
    pub fn new(
        resource: ResourceIdentity,
        limit: u64,
        observed: Option<u64>,
        monitor_evidence: impl Into<String>,
    ) -> Self {
        Self {
            resource,
            limit,
            observed,
            monitor_evidence: monitor_evidence.into(),
        }
    }
}

impl fmt::Display for BoundError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "container {} bound exceeded (limit={}, evidence={}",
            self.resource, self.limit, self.monitor_evidence
        )?;
        if let Some(observed) = self.observed {
            write!(formatter, ", observed={observed}")?;
        }
        formatter.write_str(")")
    }
}

impl std::error::Error for BoundError {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::executor::process::tree::{BoundaryIdentity, BoundaryKind};

    #[test]
    fn descendant_held_output_pipe_is_killed_and_joined_by_deadline() {
        let started = Instant::now();
        let error = match bounded_output(
            Path::new("/bin/sh"),
            ["-c", "sleep 30 &"],
            Instant::now() + Duration::from_millis(100),
            4096,
        ) {
            Ok(_) => panic!("descendant-held pipe unexpectedly closed"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn control_protocol_is_shared_and_reap_requires_bound_helper_evidence() {
        assert_eq!(helper_path(), "/usr/libexec/kit-container-helper");
        assert_eq!(helper_protocol(), "kit-container-v1");
        let identity = BoundaryIdentity::new(
            BoundaryKind::Container,
            "boundary-1",
            "owner-1",
            format!(
                "plan|sha256:{}|sha256:{}|invocation",
                "a".repeat(64),
                "b".repeat(64)
            ),
        )
        .unwrap();
        let control = ControlIdentity::from_boundary(&identity).unwrap();
        assert_eq!(control.arguments("reap")[0], "reap");
        let transcript = format!(
            "protocol={HELPER_PROTOCOL}\nownership_id=owner-1\nplan_digest=plan\nruntime_identity=sha256:{runtime}\nhelper_identity=sha256:{helper}\ninvocation_digest=invocation\nboundary_absent=true\nsurvivors=0\n",
            runtime = "a".repeat(64),
            helper = "b".repeat(64),
        );
        assert!(
            !control
                .parse_evidence(&transcript)
                .unwrap()
                .direct_child_reaped
        );
        assert!(
            control
                .parse_evidence(&(transcript + "direct_child_reaped=true\n"))
                .unwrap()
                .direct_child_reaped
        );
    }
}

pub(crate) fn record_for_preview(record: &ProbeRecord) -> RuntimeEvidence {
    RuntimeEvidence(record.0.clone())
}
