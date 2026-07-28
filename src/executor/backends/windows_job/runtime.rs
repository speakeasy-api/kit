#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(windows)]
use std::path::Path;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, io,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Serialize;

use crate::executor::{
    process::tree::{
        BoundaryControl, BoundaryIdentity, BoundaryKind, Containment, Inspection, Ownership,
        decode_canonical_fields, encode_canonical_fields,
    },
    profile::{BackendPrimitive, ExecutionLabel, ExecutorProfile, Platform},
    secrets::{
        ExecutorSecretBroker, PreparedSecrets, WindowsSecretChannel, WindowsSecretChannelDescriptor,
    },
};

pub const HELPER_PROTOCOL: &str = "kit-windows-runtime-v1";
pub const HELPER_PATH: &str = r"C:\Program Files\Kit\kit-windows-runtime.exe";
const OPERATION_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeMode {
    Container,
    HyperV,
}

impl RuntimeMode {
    fn name(self) -> &'static str {
        match self {
            Self::Container => "container",
            Self::HyperV => "hyper_v",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "container" => Some(Self::Container),
            "hyper_v" => Some(Self::HyperV),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnavailableReason {
    UnsupportedHost,
    HelperMissing,
    HelperUntrusted,
    RuntimeProbeFailed,
    MalformedEvidence,
    IdentityMismatch,
    PrimitiveMissing,
    OutcomeUnknown,
    CredentialBrokerUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeUnavailable {
    pub reason: UnavailableReason,
    pub detail: String,
}

impl RuntimeUnavailable {
    fn new(reason: UnavailableReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for RuntimeUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Windows container/VM backend unavailable: {}",
            self.detail
        )
    }
}

impl std::error::Error for RuntimeUnavailable {}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
trait RuntimeTransport: Send + Sync {
    fn helper_identity(&self) -> &str;
    fn supports_secret_channel(&self) -> bool {
        false
    }
    fn invoke(
        &self,
        operation: &str,
        arguments: &[String],
        request: Option<&[u8]>,
        secret_channel: Option<&WindowsSecretChannel>,
        deadline: Instant,
    ) -> io::Result<String>;
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalPlan {
    Pipes,
    ConPty {
        source_process_id: u32,
        inherited_handles: Vec<u64>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchPlan {
    pub program: String,
    pub argv: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub current_directory: Option<String>,
    pub environment_policy: &'static str,
    pub mounts: Vec<LaunchMount>,
    pub image_digest: Option<String>,
    pub instance_id: Option<String>,
    pub rootfs_layer_id: Option<String>,
    pub writable_layer_id: Option<String>,
    pub terminal: TerminalPlan,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LaunchMount {
    pub role: String,
    pub source: String,
    pub target: String,
    pub access: String,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SpawnRequest<'a> {
    protocol: &'static str,
    operation: &'static str,
    mode: &'static str,
    canonical_profile: serde_json::Value,
    launch: &'a LaunchPlan,
    ownership: SpawnOwnership<'a>,
    nonce: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential_channel: Option<&'a WindowsSecretChannelDescriptor>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SpawnOwnership<'a> {
    daemon_service: &'a str,
    attempt: &'a str,
}

pub(crate) struct SpawnAttestation {
    runtime: Option<RuntimeBoundary>,
    recovery_identity: String,
    pub job_locator: String,
    pub job_token: String,
    pub job_start_identity: String,
    pub root_pid: u32,
    pub process_handle: u64,
    pub thread_handle: u64,
    pub job_handle: u64,
    pub stdout_handle: Option<u64>,
    pub stderr_handle: Option<u64>,
}

impl SpawnAttestation {
    pub(crate) fn runtime(&self) -> &RuntimeBoundary {
        self.runtime
            .as_ref()
            .expect("armed attestation has a runtime")
    }

    pub(crate) fn runtime_mut(&mut self) -> &mut RuntimeBoundary {
        self.runtime
            .as_mut()
            .expect("armed attestation has a runtime")
    }

    pub(crate) fn abort_and_verify(&mut self) -> io::Result<()> {
        let result = self.runtime_mut().abort_and_verify();
        // A failed proof is quarantined by immutable recovery identity; never retry stale state.
        self.runtime.take();
        result
    }

    pub(crate) fn recovery_identity(&self) -> String {
        self.recovery_identity.clone()
    }

    pub(crate) fn disarm(mut self) -> RuntimeBoundary {
        self.runtime
            .take()
            .expect("committed attestation has a runtime")
    }
}

impl Drop for SpawnAttestation {
    fn drop(&mut self) {
        if self.runtime.is_some() {
            let _ = self.abort_and_verify();
        }
    }
}

#[derive(Clone)]
pub struct RuntimeEvidence {
    mode: RuntimeMode,
    helper_identity: String,
    runtime_identity: String,
    capabilities: BTreeSet<BackendPrimitive>,
    transport: Arc<dyn RuntimeTransport>,
}

impl fmt::Debug for RuntimeEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeEvidence")
            .field("mode", &self.mode)
            .field("helper_identity", &self.helper_identity)
            .field("runtime_identity", &self.runtime_identity)
            .field("capabilities", &self.capabilities)
            .finish()
    }
}

impl RuntimeEvidence {
    pub const fn mode(&self) -> RuntimeMode {
        self.mode
    }

    pub fn capabilities(&self) -> &BTreeSet<BackendPrimitive> {
        &self.capabilities
    }
}

pub fn probe(profile: &ExecutorProfile) -> Result<RuntimeEvidence, RuntimeUnavailable> {
    probe_for_terminal(profile, false)
}

pub fn probe_with_secret_broker(
    profile: &ExecutorProfile,
    broker: &dyn ExecutorSecretBroker,
) -> Result<RuntimeEvidence, RuntimeUnavailable> {
    probe_for_terminal_with_secret_broker(profile, false, broker)
}

pub fn probe_for_terminal(
    profile: &ExecutorProfile,
    _require_conpty: bool,
) -> Result<RuntimeEvidence, RuntimeUnavailable> {
    if profile.platform() != Platform::Windows || !cfg!(windows) {
        return Err(RuntimeUnavailable::new(
            UnavailableReason::UnsupportedHost,
            "the trusted Windows helper requires a Windows host",
        ));
    }
    #[cfg(windows)]
    {
        let transport = Arc::new(InstalledTransport::load()?);
        probe_with_transport(profile, _require_conpty, transport, false)
    }
    #[cfg(not(windows))]
    unreachable!("guarded by target check")
}

pub(crate) fn probe_for_terminal_with_secret_broker(
    profile: &ExecutorProfile,
    _require_conpty: bool,
    _broker: &dyn ExecutorSecretBroker,
) -> Result<RuntimeEvidence, RuntimeUnavailable> {
    if profile.platform() != Platform::Windows || !cfg!(windows) {
        return Err(RuntimeUnavailable::new(
            UnavailableReason::UnsupportedHost,
            "the trusted Windows helper requires a Windows host",
        ));
    }
    #[cfg(windows)]
    {
        let transport = Arc::new(InstalledTransport::load()?);
        probe_with_transport(profile, _require_conpty, transport, true)
    }
    #[cfg(not(windows))]
    unreachable!("guarded by target check")
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn probe_with_transport(
    profile: &ExecutorProfile,
    require_conpty: bool,
    transport: Arc<dyn RuntimeTransport>,
    credential_broker_configured: bool,
) -> Result<RuntimeEvidence, RuntimeUnavailable> {
    if !profile.credentials().is_empty()
        && (!credential_broker_configured || !transport.supports_secret_channel())
    {
        return Err(RuntimeUnavailable::new(
            UnavailableReason::CredentialBrokerUnavailable,
            "credential-bearing Windows plans require an authorized dedicated helper channel",
        ));
    }
    let required_mode = if profile
        .requirements()
        .contains(&BackendPrimitive::VmTenantBoundary)
    {
        "vm_tenant"
    } else if profile
        .requirements()
        .contains(&BackendPrimitive::ContainerOrVmBoundary)
    {
        "container_or_vm"
    } else {
        return Err(RuntimeUnavailable::new(
            UnavailableReason::PrimitiveMissing,
            "profile does not require a Windows container or VM primitive",
        ));
    };
    let transcript = transport
        .invoke(
            "probe",
            &[
                format!("--required-boundary={required_mode}"),
                format!("--profile-digest={}", profile.digest()),
                format!("--require-conpty={require_conpty}"),
            ],
            None,
            None,
            Instant::now() + OPERATION_TIMEOUT,
        )
        .map_err(|error| {
            RuntimeUnavailable::new(UnavailableReason::RuntimeProbeFailed, error.to_string())
        })?;
    let fields = parse_fields(
        &transcript,
        &[
            "protocol",
            "helper_identity",
            "runtime_identity",
            "mode",
            "capabilities",
            "job_operations",
            "atomic_spawn",
            "conpty",
        ],
    )
    .map_err(|error| RuntimeUnavailable::new(UnavailableReason::MalformedEvidence, error))?;
    if fields["protocol"] != HELPER_PROTOCOL
        || fields["helper_identity"] != transport.helper_identity()
    {
        return Err(RuntimeUnavailable::new(
            UnavailableReason::IdentityMismatch,
            "probe evidence is not bound to the trusted installed helper",
        ));
    }
    if parse_bool(fields["job_operations"]) != Some(true)
        || parse_bool(fields["atomic_spawn"]) != Some(true)
        || (require_conpty && parse_bool(fields["conpty"]) != Some(true))
    {
        return Err(RuntimeUnavailable::new(
            UnavailableReason::PrimitiveMissing,
            "trusted helper did not prove Job operations, atomic spawn, or requested ConPTY",
        ));
    }
    let mode = RuntimeMode::parse(fields["mode"]).ok_or_else(|| {
        RuntimeUnavailable::new(UnavailableReason::MalformedEvidence, "invalid runtime mode")
    })?;
    if required_mode == "vm_tenant" && mode != RuntimeMode::HyperV {
        return Err(RuntimeUnavailable::new(
            UnavailableReason::PrimitiveMissing,
            "hostile Windows execution requires a Hyper-V VM tenant",
        ));
    }
    let mut capabilities = parse_capabilities(fields["capabilities"])?;
    if !credential_broker_configured || !transport.supports_secret_channel() {
        capabilities.retain(|capability| {
            !matches!(
                capability,
                BackendPrimitive::CredentialFileDescriptor
                    | BackendPrimitive::CredentialMemoryFile
                    | BackendPrimitive::CredentialScopedEnvironment
            )
        });
    }
    let mut composite = super::capabilities();
    composite.extend(capabilities.iter().copied());
    let missing = profile
        .requirements()
        .difference(&composite)
        .copied()
        .collect::<BTreeSet<_>>();
    if !missing.is_empty() {
        return Err(RuntimeUnavailable::new(
            UnavailableReason::PrimitiveMissing,
            format!("trusted helper is missing profile primitives: {missing:?}"),
        ));
    }
    Ok(RuntimeEvidence {
        mode,
        helper_identity: fields["helper_identity"].to_owned(),
        runtime_identity: fields["runtime_identity"].to_owned(),
        capabilities,
        transport,
    })
}

pub struct RuntimeBoundary {
    identity: BoundaryIdentity,
    mode: RuntimeMode,
    plan_digest: String,
    helper_identity: String,
    runtime_identity: String,
    isolation_identity: String,
    generation: String,
    root: Option<RootBinding>,
    transport: Arc<dyn RuntimeTransport>,
}

#[derive(Clone, Debug)]
struct RootBinding {
    pid: u32,
    creation_time: u64,
    job_locator: String,
    job_token: String,
    job_start_identity: String,
}

impl RuntimeBoundary {
    pub fn plan_digest(&self) -> &str {
        &self.plan_digest
    }

    pub fn helper_identity(&self) -> &str {
        &self.helper_identity
    }

    pub fn runtime_identity(&self) -> &str {
        &self.runtime_identity
    }

    pub(crate) fn spawn_suspended(
        profile: &ExecutorProfile,
        ownership: &Ownership,
        launch: &LaunchPlan,
        evidence: RuntimeEvidence,
        secrets: Option<&PreparedSecrets>,
        nonce: &str,
        deadline: Instant,
    ) -> Result<SpawnAttestation, RuntimeUnavailable> {
        let canonical_profile =
            serde_json::from_slice(profile.canonical_bytes()).map_err(|error| {
                RuntimeUnavailable::new(UnavailableReason::MalformedEvidence, error.to_string())
            })?;
        if profile.credentials().is_empty() != secrets.is_none()
            || (secrets.is_some() && !evidence.transport.supports_secret_channel())
        {
            return Err(RuntimeUnavailable::new(
                UnavailableReason::CredentialBrokerUnavailable,
                "credential-bearing Windows plans require an authorized dedicated helper channel",
            ));
        }
        let profile_digest = profile.digest().to_string();
        let credential_channel = secrets
            .map(|secrets| secrets.windows_channel_descriptor(&profile_digest, ownership, nonce));
        let request = SpawnRequest {
            protocol: HELPER_PROTOCOL,
            operation: "spawn_suspended",
            mode: evidence.mode.name(),
            canonical_profile,
            launch,
            ownership: SpawnOwnership {
                daemon_service: ownership.daemon_service(),
                attempt: ownership.attempt(),
            },
            nonce,
            credential_channel: credential_channel.as_ref(),
        };
        let request = serde_json::to_vec(&request).map_err(|error| {
            RuntimeUnavailable::new(UnavailableReason::MalformedEvidence, error.to_string())
        })?;
        if request.len() > REQUEST_LIMIT {
            return Err(RuntimeUnavailable::new(
                UnavailableReason::MalformedEvidence,
                "Windows helper request exceeds the 1 MiB protocol bound",
            ));
        }
        let plan_digest = format!("blake3:{}", blake3::hash(&request).to_hex());
        let secret_channel = secrets
            .zip(credential_channel.as_ref())
            .map(|(secrets, descriptor)| secrets.windows_channel(descriptor, &plan_digest))
            .transpose()
            .map_err(|error| {
                RuntimeUnavailable::new(
                    UnavailableReason::CredentialBrokerUnavailable,
                    error.to_string(),
                )
            })?;
        let transcript = evidence
            .transport
            .invoke(
                "spawn",
                &[format!("--request-bytes={}", request.len())],
                Some(&request),
                secret_channel.as_ref(),
                deadline,
            )
            .map_err(|error| {
                RuntimeUnavailable::new(UnavailableReason::RuntimeProbeFailed, error.to_string())
            })?;
        let fields = parse_spawn_attestation(&transcript).map_err(|error| {
            RuntimeUnavailable::new(UnavailableReason::MalformedEvidence, error)
        })?;
        if fields["protocol"] != HELPER_PROTOCOL
            || fields["helper_identity"] != evidence.helper_identity
            || fields["runtime_identity"] != evidence.runtime_identity
            || fields["mode"] != evidence.mode.name()
            || parse_bool(fields["boundary_created"]) != Some(true)
        {
            return Err(RuntimeUnavailable::new(
                UnavailableReason::IdentityMismatch,
                "spawn evidence is not bound to the exact suspended execution plan",
            ));
        }
        let root_pid = parse_number(fields["root_pid"], "root PID")?;
        let root_creation_time = parse_number(fields["root_creation_time"], "root creation time")?;
        let root = RootBinding {
            pid: root_pid,
            creation_time: root_creation_time,
            job_locator: fields["job_locator"].to_owned(),
            job_token: fields["job_token"].to_owned(),
            job_start_identity: fields["job_start_identity"].to_owned(),
        };
        let identity = BoundaryIdentity::new(
            BoundaryKind::WindowsContainerOrVm,
            fields["boundary"],
            fields["ownership_id"],
            encode_start_identity(
                evidence.mode,
                &plan_digest,
                &evidence.helper_identity,
                &evidence.runtime_identity,
                fields["isolation_identity"],
                fields["generation"],
                Some(&root),
            )?,
        )
        .map_err(|error| {
            RuntimeUnavailable::new(UnavailableReason::MalformedEvidence, error.to_string())
        })?;
        let runtime = Self {
            identity,
            mode: evidence.mode,
            plan_digest,
            helper_identity: evidence.helper_identity,
            runtime_identity: evidence.runtime_identity,
            isolation_identity: fields["isolation_identity"].to_owned(),
            generation: fields["generation"].to_owned(),
            root: Some(root),
            transport: evidence.transport,
        };
        let recovery_identity = format!(
            "{}:{}:{}",
            runtime.identity.locator(),
            runtime.identity.ownership_token(),
            runtime.identity.start_identity()
        );
        let mut attestation = SpawnAttestation {
            runtime: Some(runtime),
            recovery_identity,
            job_locator: fields["job_locator"].to_owned(),
            job_token: fields["job_token"].to_owned(),
            job_start_identity: fields["job_start_identity"].to_owned(),
            root_pid,
            /*
            process_handle: parse_handle(fields["process_handle"])?,
            thread_handle: parse_handle(fields["thread_handle"])?,
            job_handle: parse_handle(fields["job_handle"])?,
            stdout_handle: parse_optional_handle(fields["stdout_handle"])?,
            stderr_handle: parse_optional_handle(fields["stderr_handle"])?,
            */
            process_handle: 0,
            thread_handle: 0,
            job_handle: 0,
            stdout_handle: None,
            stderr_handle: None,
        };
        let validated = (|| {
            if fields["plan_digest"] != attestation.runtime().plan_digest
                || fields["nonce"] != nonce
                || parse_bool(fields["suspended"]) != Some(true)
                || parse_bool(fields["job_assigned"]) != Some(true)
                || parse_bool(fields["conpty_bound"])
                    != Some(matches!(launch.terminal, TerminalPlan::ConPty { .. }))
            {
                return Err(RuntimeUnavailable::new(
                    UnavailableReason::IdentityMismatch,
                    "spawn evidence is not bound to the exact suspended execution plan",
                ));
            }
            verify_job_identity(
                fields["job_start_identity"],
                root_pid,
                root_creation_time,
                profile,
            )?;
            if fields["job_token"].len() != 64
                || !fields["job_token"]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || fields["job_locator"] != format!("Local\\kit-job-{}", fields["job_token"])
            {
                return Err(RuntimeUnavailable::new(
                    UnavailableReason::IdentityMismatch,
                    "attested Windows Job name/token is not self-authenticating",
                ));
            }
            let handles = [
                fields["process_handle"],
                fields["thread_handle"],
                fields["job_handle"],
                fields["stdout_handle"],
                fields["stderr_handle"],
            ]
            .into_iter()
            .filter(|handle| *handle != "none")
            .collect::<BTreeSet<_>>();
            let expected_handles = 3
                + usize::from(fields["stdout_handle"] != "none")
                + usize::from(fields["stderr_handle"] != "none");
            if handles.len() != expected_handles {
                return Err(RuntimeUnavailable::new(
                    UnavailableReason::IdentityMismatch,
                    "trusted helper returned aliased duplicated handles",
                ));
            }
            attestation.process_handle = parse_handle(fields["process_handle"])?;
            attestation.thread_handle = parse_handle(fields["thread_handle"])?;
            attestation.job_handle = parse_handle(fields["job_handle"])?;
            attestation.stdout_handle = parse_optional_handle(fields["stdout_handle"])?;
            attestation.stderr_handle = parse_optional_handle(fields["stderr_handle"])?;
            Ok(())
        })();
        if let Err(error) = validated {
            return match attestation.abort_and_verify() {
                Ok(()) => Err(error),
                Err(cleanup) => Err(RuntimeUnavailable::new(
                    UnavailableReason::OutcomeUnknown,
                    format!(
                        "outcome_unknown: {error}; helper abort was not proven: {cleanup}; recovery_identity={}",
                        attestation.recovery_identity()
                    ),
                )),
            };
        }
        Ok(attestation)
    }

    pub fn recover(identity: &BoundaryIdentity) -> Result<Self, RuntimeUnavailable> {
        if identity.kind() != BoundaryKind::WindowsContainerOrVm {
            return Err(RuntimeUnavailable::new(
                UnavailableReason::MalformedEvidence,
                "recovery identity is not a Windows container/VM boundary",
            ));
        }
        #[cfg(windows)]
        let (parsed, transport): (ParsedStart, Arc<dyn RuntimeTransport>) = (
            parse_start_identity(identity.start_identity())?,
            Arc::new(InstalledTransport::load()?),
        );
        #[cfg(not(windows))]
        return Err(RuntimeUnavailable::new(
            UnavailableReason::UnsupportedHost,
            "the trusted Windows helper requires a Windows host",
        ));
        #[cfg(windows)]
        {
            if transport.helper_identity() != parsed.helper_identity {
                return Err(RuntimeUnavailable::new(
                    UnavailableReason::IdentityMismatch,
                    "installed helper changed since boundary creation",
                ));
            }
            let boundary = Self {
                identity: identity.clone(),
                mode: parsed.mode,
                plan_digest: parsed.plan_digest,
                helper_identity: parsed.helper_identity,
                runtime_identity: parsed.runtime_identity,
                isolation_identity: parsed.isolation_identity,
                generation: parsed.generation,
                root: Some(parsed.root),
                transport,
            };
            boundary
                .control("recover", Instant::now() + OPERATION_TIMEOUT)
                .map_err(|error| {
                    RuntimeUnavailable::new(
                        UnavailableReason::RuntimeProbeFailed,
                        error.to_string(),
                    )
                })?;
            Ok(boundary)
        }
    }

    fn arguments(&self) -> Vec<String> {
        let mut arguments = vec![
            format!("--mode={}", self.mode.name()),
            format!("--boundary={}", self.identity.locator()),
            format!("--ownership-id={}", self.identity.ownership_token()),
            format!("--plan-digest={}", self.plan_digest),
            format!("--helper-identity={}", self.helper_identity),
            format!("--runtime-identity={}", self.runtime_identity),
            format!(
                "--isolation-identity={}",
                encode_bytes(self.isolation_identity.as_bytes())
            ),
            format!("--generation={}", self.generation),
        ];
        if let Some(root) = &self.root {
            arguments.extend([
                format!("--root-pid={}", root.pid),
                format!("--root-creation-time={}", root.creation_time),
                format!(
                    "--job-locator={}",
                    encode_bytes(root.job_locator.as_bytes())
                ),
                format!("--job-token={}", encode_bytes(root.job_token.as_bytes())),
                format!(
                    "--job-start-identity={}",
                    encode_bytes(root.job_start_identity.as_bytes())
                ),
            ]);
        }
        arguments
    }

    fn control(&self, operation: &str, deadline: Instant) -> io::Result<ControlEvidence> {
        if self.root.is_none() {
            return Err(io::Error::other(
                "Windows runtime control requires an attested root binding",
            ));
        }
        let transcript =
            self.transport
                .invoke(operation, &self.arguments(), None, None, deadline)?;
        let fields = parse_fields(
            &transcript,
            &[
                "protocol",
                "helper_identity",
                "runtime_identity",
                "isolation_identity",
                "mode",
                "plan_digest",
                "boundary",
                "ownership_id",
                "generation",
                "root_pid",
                "root_creation_time",
                "job_locator",
                "job_token",
                "job_start_identity",
                "root_bound",
                "job_bound",
                "boundary_absent",
                "survivors",
                "direct_child_reaped",
            ],
        )
        .map_err(io::Error::other)?;
        self.verify_fields(&fields)?;
        Ok(ControlEvidence {
            boundary_absent: parse_bool(fields["boundary_absent"])
                .ok_or_else(|| io::Error::other("invalid Windows helper boolean"))?,
            survivors: fields["survivors"]
                .parse()
                .map_err(|_| io::Error::other("invalid Windows helper survivor count"))?,
            direct_child_reaped: parse_bool(fields["direct_child_reaped"])
                .ok_or_else(|| io::Error::other("invalid Windows helper boolean"))?,
        })
    }

    pub(crate) fn abort_evidence(&mut self) -> io::Result<RuntimeAbortEvidence> {
        let evidence = self.control("abort", Instant::now() + OPERATION_TIMEOUT)?;
        Ok(RuntimeAbortEvidence {
            boundary_absent: evidence.boundary_absent,
            survivors: evidence.survivors,
            direct_child_reaped: evidence.direct_child_reaped,
        })
    }

    fn abort_and_verify(&mut self) -> io::Result<()> {
        let evidence = self.abort_evidence()?;
        if evidence.boundary_absent && evidence.survivors == 0 && evidence.direct_child_reaped {
            Ok(())
        } else {
            Err(io::Error::other(
                "Windows helper abort did not prove an absent boundary and zero survivors",
            ))
        }
    }

    fn verify_fields(&self, fields: &BTreeMap<&str, &str>) -> io::Result<()> {
        let root = self.root.as_ref().ok_or_else(|| {
            io::Error::other("Windows helper control has no persisted root binding")
        })?;
        if fields["protocol"] == HELPER_PROTOCOL
            && fields["helper_identity"] == self.helper_identity
            && fields["runtime_identity"] == self.runtime_identity
            && fields["isolation_identity"] == self.isolation_identity
            && fields["mode"] == self.mode.name()
            && fields["plan_digest"] == self.plan_digest
            && fields["boundary"] == self.identity.locator()
            && fields["ownership_id"] == self.identity.ownership_token()
            && fields["generation"] == self.generation
            && fields["root_pid"] == root.pid.to_string()
            && fields["root_creation_time"] == root.creation_time.to_string()
            && fields["job_locator"] == root.job_locator
            && fields["job_token"] == root.job_token
            && fields["job_start_identity"] == root.job_start_identity
            && parse_bool(fields["root_bound"]) == Some(true)
            && parse_bool(fields["job_bound"]) == Some(true)
        {
            Ok(())
        } else {
            Err(io::Error::other("Windows helper control identity mismatch"))
        }
    }
}

impl BoundaryControl for RuntimeBoundary {
    fn identity(&self) -> &BoundaryIdentity {
        &self.identity
    }

    fn containment(&self) -> Containment {
        Containment::Complete
    }

    fn release(&mut self, deadline: Instant) -> io::Result<()> {
        self.control("resume", deadline).map(drop)
    }

    fn kill_boundary(&mut self, deadline: Instant) -> io::Result<()> {
        self.control("kill", deadline).map(drop)
    }

    fn wait_and_reap(&mut self, deadline: Instant) -> io::Result<()> {
        let evidence = self.control("reap", deadline)?;
        if evidence.direct_child_reaped {
            Ok(())
        } else {
            Err(io::Error::other(
                "Windows helper did not prove its supervisor reaped the root",
            ))
        }
    }

    fn inspect(&mut self, deadline: Instant) -> io::Result<Inspection> {
        let evidence = self.control("inspect", deadline)?;
        Ok(Inspection {
            identity: self.identity.clone(),
            survivors: Some(evidence.survivors),
            quiescent: evidence.boundary_absent && evidence.survivors == 0,
        })
    }
}

pub(crate) struct RuntimeAbortEvidence {
    pub(crate) boundary_absent: bool,
    pub(crate) survivors: u32,
    pub(crate) direct_child_reaped: bool,
}

struct ControlEvidence {
    boundary_absent: bool,
    survivors: u32,
    direct_child_reaped: bool,
}

#[derive(Debug)]
struct ParsedStart {
    mode: RuntimeMode,
    plan_digest: String,
    helper_identity: String,
    runtime_identity: String,
    isolation_identity: String,
    generation: String,
    root: RootBinding,
}

fn parse_start_identity(value: &str) -> Result<ParsedStart, RuntimeUnavailable> {
    let fields = decode_canonical_fields(value, "v4", 12).map_err(|_| {
        RuntimeUnavailable::new(
            UnavailableReason::MalformedEvidence,
            "persisted Windows runtime identity is corrupt or noncanonical",
        )
    })?;
    if fields[6] != "root" {
        return Err(RuntimeUnavailable::new(
            UnavailableReason::MalformedEvidence,
            "persisted Windows runtime identity is partial or unbound",
        ));
    }
    Ok(ParsedStart {
        mode: RuntimeMode::parse(fields[0]).ok_or_else(|| {
            RuntimeUnavailable::new(UnavailableReason::MalformedEvidence, "invalid runtime mode")
        })?,
        plan_digest: fields[1].to_owned(),
        helper_identity: fields[2].to_owned(),
        runtime_identity: fields[3].to_owned(),
        isolation_identity: fields[4].to_owned(),
        generation: fields[5].to_owned(),
        root: RootBinding {
            pid: fields[7].parse().map_err(|_| {
                RuntimeUnavailable::new(UnavailableReason::MalformedEvidence, "invalid root PID")
            })?,
            creation_time: fields[8].parse().map_err(|_| {
                RuntimeUnavailable::new(
                    UnavailableReason::MalformedEvidence,
                    "invalid root creation time",
                )
            })?,
            job_locator: fields[9].to_owned(),
            job_token: fields[10].to_owned(),
            job_start_identity: fields[11].to_owned(),
        },
    })
}

fn encode_start_identity(
    mode: RuntimeMode,
    plan: &str,
    helper: &str,
    runtime: &str,
    isolation: &str,
    generation: &str,
    root: Option<&RootBinding>,
) -> Result<String, RuntimeUnavailable> {
    let (marker, pid, creation, job, token, start) = root.map_or_else(
        || {
            (
                "unbound",
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            )
        },
        |root| {
            (
                "root",
                root.pid.to_string(),
                root.creation_time.to_string(),
                root.job_locator.clone(),
                root.job_token.clone(),
                root.job_start_identity.clone(),
            )
        },
    );
    let fields = [
        mode.name(),
        plan,
        helper,
        runtime,
        isolation,
        generation,
        marker,
        &pid,
        &creation,
        &job,
        &token,
        &start,
    ];
    if fields.iter().any(|field| {
        field.is_empty() || field.len() > 4 * 1024 || field.contains(['\0', '\n', '\r'])
    }) {
        return Err(RuntimeUnavailable::new(
            UnavailableReason::MalformedEvidence,
            "Windows runtime identity field exceeds the 4 KiB protocol bound",
        ));
    }
    Ok(encode_canonical_fields("v4", &fields))
}

fn parse_spawn_attestation(transcript: &str) -> Result<BTreeMap<&str, &str>, String> {
    parse_fields(
        transcript,
        &[
            "protocol",
            "helper_identity",
            "runtime_identity",
            "mode",
            "plan_digest",
            "boundary",
            "ownership_id",
            "generation",
            "nonce",
            "root_pid",
            "root_creation_time",
            "job_locator",
            "job_token",
            "job_start_identity",
            "isolation_identity",
            "suspended",
            "job_assigned",
            "boundary_created",
            "conpty_bound",
            "process_handle",
            "thread_handle",
            "job_handle",
            "stdout_handle",
            "stderr_handle",
        ],
    )
}

fn parse_number<T>(value: &str, name: &str) -> Result<T, RuntimeUnavailable>
where
    T: std::str::FromStr,
{
    value.parse().map_err(|_| {
        RuntimeUnavailable::new(
            UnavailableReason::MalformedEvidence,
            format!("invalid Windows helper {name}"),
        )
    })
}

fn parse_handle(value: &str) -> Result<u64, RuntimeUnavailable> {
    let handle = parse_number(value, "duplicated handle")?;
    if handle == 0 {
        Err(RuntimeUnavailable::new(
            UnavailableReason::MalformedEvidence,
            "Windows helper returned a null duplicated handle",
        ))
    } else {
        Ok(handle)
    }
}

fn parse_optional_handle(value: &str) -> Result<Option<u64>, RuntimeUnavailable> {
    if value == "none" {
        Ok(None)
    } else {
        parse_handle(value).map(Some)
    }
}

fn verify_job_identity(
    value: &str,
    root_pid: u32,
    creation_time: u64,
    profile: &ExecutorProfile,
) -> Result<(), RuntimeUnavailable> {
    let fields = value.split(':').collect::<Vec<_>>();
    let limits = profile.resources();
    let cpu_100ns = limits.cpu_millis.checked_mul(10_000).ok_or_else(|| {
        RuntimeUnavailable::new(UnavailableReason::MalformedEvidence, "CPU limit overflow")
    })?;
    if fields.len() != 6
        || fields[0] != "v1"
        || fields[1] != root_pid.to_string()
        || fields[2] != creation_time.to_string()
        || fields[3] != cpu_100ns.to_string()
        || fields[4] != limits.memory_bytes.to_string()
        || fields[5] != limits.pids.to_string()
    {
        return Err(RuntimeUnavailable::new(
            UnavailableReason::IdentityMismatch,
            "attested Windows Job root or limits do not match the execution plan",
        ));
    }
    Ok(())
}

fn parse_fields<'a>(
    transcript: &'a str,
    expected: &[&str],
) -> Result<BTreeMap<&'a str, &'a str>, String> {
    let mut fields = BTreeMap::new();
    for line in transcript.lines() {
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| "invalid Windows helper evidence line".to_owned())?;
        if !expected.contains(&name) || value.is_empty() || fields.insert(name, value).is_some() {
            return Err("unknown, empty, or duplicate Windows helper evidence field".to_owned());
        }
    }
    if expected.iter().any(|name| !fields.contains_key(name)) || fields.len() != expected.len() {
        return Err("missing Windows helper evidence field".to_owned());
    }
    Ok(fields)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn parse_capabilities(value: &str) -> Result<BTreeSet<BackendPrimitive>, RuntimeUnavailable> {
    value
        .split(',')
        .map(|name| {
            serde_json::from_str::<BackendPrimitive>(&format!("\"{name}\"")).map_err(|_| {
                RuntimeUnavailable::new(
                    UnavailableReason::MalformedEvidence,
                    format!("invalid Windows helper capability {name}"),
                )
            })
        })
        .collect()
}

fn parse_bool(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn encode_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn composite_capabilities(evidence: &RuntimeEvidence) -> BTreeSet<BackendPrimitive> {
    let mut capabilities = super::capabilities();
    capabilities.extend(evidence.capabilities.iter().copied());
    capabilities
}

pub fn supports_profile(profile: &ExecutorProfile, evidence: &RuntimeEvidence) -> bool {
    profile.platform() == Platform::Windows
        && matches!(
            profile.label(),
            ExecutionLabel::TrustedLocal | ExecutionLabel::Restricted | ExecutionLabel::Hostile
        )
        && profile
            .requirements()
            .is_subset(&composite_capabilities(evidence))
        && (!profile
            .requirements()
            .contains(&BackendPrimitive::VmTenantBoundary)
            || evidence.mode == RuntimeMode::HyperV)
}

#[cfg(windows)]
struct InstalledTransport {
    identity: String,
}

#[cfg(windows)]
impl InstalledTransport {
    fn load() -> Result<Self, RuntimeUnavailable> {
        use sha2::Digest as _;

        let path = Path::new(HELPER_PATH);
        let bytes = std::fs::read(path).map_err(|error| {
            RuntimeUnavailable::new(
                if error.kind() == io::ErrorKind::NotFound {
                    UnavailableReason::HelperMissing
                } else {
                    UnavailableReason::HelperUntrusted
                },
                format!("{HELPER_PATH}: {error}"),
            )
        })?;
        verify_authenticode(path).map_err(|error| {
            RuntimeUnavailable::new(UnavailableReason::HelperUntrusted, error.to_string())
        })?;
        Ok(Self {
            identity: format!("sha256:{:x}", sha2::Sha256::digest(bytes)),
        })
    }
}

#[cfg(windows)]
impl RuntimeTransport for InstalledTransport {
    fn helper_identity(&self) -> &str {
        &self.identity
    }

    fn supports_secret_channel(&self) -> bool {
        true
    }

    fn invoke(
        &self,
        operation: &str,
        arguments: &[String],
        request: Option<&[u8]>,
        secret_channel: Option<&WindowsSecretChannel>,
        deadline: Instant,
    ) -> io::Result<String> {
        let current = Self::load().map_err(io::Error::other)?;
        if current.identity != self.identity {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "trusted Windows helper identity changed",
            ));
        }
        let mut argv = vec![
            operation.to_owned(),
            format!("--protocol={HELPER_PROTOCOL}"),
        ];
        argv.extend_from_slice(arguments);
        let output = if let (Some(request), Some(secret_channel)) = (request, secret_channel) {
            crate::executor::backends::container::limits::bounded_output_with_secret_input(
                Path::new(HELPER_PATH),
                argv,
                request,
                secret_channel.bytes(),
                deadline,
                1024 * 1024,
            )?
        } else if let Some(request) = request {
            crate::executor::backends::container::limits::bounded_output_with_input(
                Path::new(HELPER_PATH),
                argv,
                request,
                deadline,
                1024 * 1024,
            )?
        } else {
            crate::executor::backends::container::limits::bounded_output(
                Path::new(HELPER_PATH),
                argv,
                deadline,
                1024 * 1024,
            )?
        };
        if !output.status.success() {
            return Err(io::Error::other(
                "trusted Windows helper rejected operation",
            ));
        }
        String::from_utf8(output.stdout)
            .map_err(|_| io::Error::other("trusted Windows helper returned non-UTF-8 evidence"))
    }
}

#[cfg(windows)]
fn verify_authenticode(path: &Path) -> io::Result<()> {
    use std::{mem, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::Security::WinTrust::{
        WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0, WINTRUST_FILE_INFO,
        WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT, WTD_REVOKE_WHOLECHAIN,
        WTD_STATEACTION_IGNORE, WTD_UI_NONE, WTD_UICONTEXT_EXECUTE, WinVerifyTrust,
    };

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: wide.as_ptr(),
        hFile: ptr::null_mut(),
        pgKnownSubject: ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: mem::size_of::<WINTRUST_DATA>() as u32,
        pPolicyCallbackData: ptr::null_mut(),
        pSIPClientData: ptr::null_mut(),
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_WHOLECHAIN,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
        dwStateAction: WTD_STATEACTION_IGNORE,
        hWVTStateData: ptr::null_mut(),
        pwszURLReference: ptr::null_mut(),
        dwProvFlags: WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT,
        dwUIContext: WTD_UICONTEXT_EXECUTE,
        pSignatureSettings: ptr::null_mut(),
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    // SAFETY: all WinTrust structures and the NUL-terminated path remain live for the call.
    let status = unsafe {
        WinVerifyTrust(
            ptr::null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("installed helper Authenticode verification failed: 0x{status:08x}"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::profile::{
        Architecture, CredentialHandle, CredentialInjection, CredentialInjectionMode, EgressGrant,
        EgressTransport, ProfileSpec, ResourceLimits, TrustTier,
    };
    use std::sync::Mutex;

    struct FakeTransport {
        identity: String,
        transcript: Mutex<String>,
        secret_channel: bool,
        calls: Mutex<usize>,
    }

    #[derive(Default)]
    struct ProtocolState {
        events: Vec<String>,
        request: Option<serde_json::Value>,
        control_arguments: Vec<String>,
        plan_digest: String,
        secret_channel: Option<Vec<u8>>,
    }

    struct ProtocolTransport {
        state: Mutex<ProtocolState>,
        mismatch_plan: bool,
        alias_handles: bool,
        abort_proof: bool,
    }

    impl RuntimeTransport for ProtocolTransport {
        fn helper_identity(&self) -> &str {
            "sha256:trusted"
        }

        fn supports_secret_channel(&self) -> bool {
            true
        }

        fn invoke(
            &self,
            operation: &str,
            arguments: &[String],
            request: Option<&[u8]>,
            secret_channel: Option<&WindowsSecretChannel>,
            _deadline: Instant,
        ) -> io::Result<String> {
            let mut state = self.state.lock().unwrap();
            state.events.push(operation.to_owned());
            if operation == "spawn" {
                let request = request.ok_or_else(|| io::Error::other("missing spawn request"))?;
                state.request = Some(serde_json::from_slice(request).unwrap());
                state.secret_channel = secret_channel.map(|channel| channel.bytes().to_vec());
                state.plan_digest = format!("blake3:{}", blake3::hash(request).to_hex());
                let plan = if self.mismatch_plan {
                    "blake3:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                } else {
                    &state.plan_digest
                };
                let thread_handle = if self.alias_handles { 10 } else { 11 };
                return Ok(format!(
                    "protocol={HELPER_PROTOCOL}\nhelper_identity=sha256:trusted\nruntime_identity=containerd:1\nmode=container\nplan_digest={plan}\nboundary=container-1\nownership_id=owner-1\ngeneration=generation-1\nnonce=nonce-1\nroot_pid=42\nroot_creation_time=99\njob_locator=Local\\kit-job-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\njob_token=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\njob_start_identity=v1:42:99:10000:1:1\nisolation_identity=container:instance-1\nsuspended=true\njob_assigned=true\nboundary_created=true\nconpty_bound=false\nprocess_handle=10\nthread_handle={thread_handle}\njob_handle=12\nstdout_handle=13\nstderr_handle=14\n"
                ));
            }
            state.control_arguments = arguments.to_vec();
            let aborted = operation == "abort" && self.abort_proof;
            Ok(format!(
                "protocol={HELPER_PROTOCOL}\nhelper_identity=sha256:trusted\nruntime_identity=containerd:1\nisolation_identity=container:instance-1\nmode=container\nplan_digest={}\nboundary=container-1\nownership_id=owner-1\ngeneration=generation-1\nroot_pid=42\nroot_creation_time=99\njob_locator=Local\\kit-job-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\njob_token=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\njob_start_identity=v1:42:99:10000:1:1\nroot_bound=true\njob_bound=true\nboundary_absent={aborted}\nsurvivors={}\ndirect_child_reaped={aborted}\n",
                state.plan_digest,
                u8::from(!aborted),
            ))
        }
    }

    impl RuntimeTransport for FakeTransport {
        fn helper_identity(&self) -> &str {
            &self.identity
        }

        fn supports_secret_channel(&self) -> bool {
            self.secret_channel
        }

        fn invoke(
            &self,
            _operation: &str,
            _arguments: &[String],
            _request: Option<&[u8]>,
            _secret_channel: Option<&WindowsSecretChannel>,
            _deadline: Instant,
        ) -> io::Result<String> {
            *self.calls.lock().unwrap() += 1;
            Ok(self.transcript.lock().unwrap().clone())
        }
    }

    fn profile(tier: TrustTier) -> ExecutorProfile {
        ExecutorProfile::new(ProfileSpec::isolated(
            tier,
            Platform::Windows,
            Architecture::X86_64,
            ResourceLimits::new(1, 1, 1, 1, 1, 1, 64, 1),
        ))
        .unwrap()
    }

    fn capabilities(profile: &ExecutorProfile) -> String {
        profile
            .requirements()
            .iter()
            .filter(|primitive| !super::super::capabilities().contains(primitive))
            .map(|primitive| {
                serde_json::to_string(primitive)
                    .unwrap()
                    .trim_matches('"')
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    #[test]
    fn trusted_probe_requires_matching_helper_identity_and_hyper_v_for_hostile() {
        let profile = profile(TrustTier::Hostile);
        let transport = Arc::new(FakeTransport {
            identity: "sha256:trusted".to_owned(),
            transcript: Mutex::new(format!(
                "protocol={HELPER_PROTOCOL}\nhelper_identity=sha256:trusted\nruntime_identity=hyperv:1\nmode=hyper_v\ncapabilities={}\njob_operations=true\natomic_spawn=true\nconpty=true\n",
                capabilities(&profile)
            )),
            secret_channel: false,
            calls: Mutex::new(0),
        });
        let evidence = probe_with_transport(&profile, true, transport.clone(), false).unwrap();
        assert!(supports_profile(&profile, &evidence));

        *transport.transcript.lock().unwrap() = format!(
            "protocol={HELPER_PROTOCOL}\nhelper_identity=sha256:other\nruntime_identity=hyperv:1\nmode=hyper_v\ncapabilities={}\njob_operations=true\natomic_spawn=true\nconpty=true\n",
            capabilities(&profile)
        );
        assert_eq!(
            probe_with_transport(&profile, true, transport, false)
                .unwrap_err()
                .reason,
            UnavailableReason::IdentityMismatch
        );
    }

    #[test]
    fn absent_installed_helper_is_typed_unavailable_off_windows() {
        if !cfg!(windows) {
            assert_eq!(
                probe(&profile(TrustTier::Restricted)).unwrap_err().reason,
                UnavailableReason::UnsupportedHost
            );
        }
    }

    fn protocol_evidence(transport: Arc<dyn RuntimeTransport>) -> RuntimeEvidence {
        RuntimeEvidence {
            mode: RuntimeMode::Container,
            helper_identity: "sha256:trusted".to_owned(),
            runtime_identity: "containerd:1".to_owned(),
            capabilities: BTreeSet::new(),
            transport,
        }
    }

    fn launch() -> LaunchPlan {
        LaunchPlan {
            program: r"C:\tool.exe".to_owned(),
            argv: vec!["--safe".to_owned()],
            environment: BTreeMap::from([("PATH".to_owned(), r"C:\Windows".to_owned())]),
            current_directory: Some(r"C:\workspace".to_owned()),
            environment_policy: "clear_and_set",
            mounts: vec![LaunchMount {
                role: "source".to_owned(),
                source: r"C:\workspace".to_owned(),
                target: r"C:\workspace".to_owned(),
                access: "read_only".to_owned(),
            }],
            image_digest: None,
            instance_id: None,
            rootfs_layer_id: None,
            writable_layer_id: None,
            terminal: TerminalPlan::Pipes,
        }
    }

    #[test]
    fn spawn_protocol_transmits_complete_canonical_profile_and_reauthenticates_root() {
        const CANARY: &[u8] = b"windows-helper-secret-canary";
        let mut spec = ProfileSpec::isolated(
            TrustTier::Restricted,
            Platform::Windows,
            Architecture::X86_64,
            ResourceLimits::new(1, 1, 1, 1, 1, 1, 64, 1),
        );
        spec.credentials.push(CredentialInjection {
            handle: CredentialHandle::new("provider_token").unwrap(),
            mode: CredentialInjectionMode::FileDescriptor,
        });
        spec.egress
            .insert(EgressGrant::new("example.com", 443, EgressTransport::Tcp).unwrap());
        let profile = ExecutorProfile::new(spec).unwrap();
        let transport = Arc::new(ProtocolTransport {
            state: Mutex::new(ProtocolState::default()),
            mismatch_plan: false,
            alias_handles: false,
            abort_proof: true,
        });
        let ownership = Ownership::new("daemon", "attempt:fence:7").unwrap();
        let secrets = PreparedSecrets::for_windows_channel_test(CANARY);
        let attestation = RuntimeBoundary::spawn_suspended(
            &profile,
            &ownership,
            &launch(),
            protocol_evidence(transport.clone()),
            Some(&secrets),
            "nonce-1",
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        let parsed =
            parse_start_identity(attestation.runtime().identity().start_identity()).unwrap();
        assert_eq!(parsed.root.pid, 42);
        assert_eq!(parsed.root.creation_time, 99);
        assert_eq!(
            parsed.root.job_locator,
            r"Local\kit-job-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(parsed.root.job_token, "a".repeat(64));
        assert_eq!(parsed.root.job_start_identity, "v1:42:99:10000:1:1");
        assert!(
            !attestation
                .runtime()
                .identity()
                .start_identity()
                .as_bytes()
                .windows(CANARY.len())
                .any(|value| value == CANARY)
        );
        assert!(!format!("{secrets:?}").contains(std::str::from_utf8(CANARY).unwrap()));

        let mut attestation = attestation;
        attestation
            .runtime_mut()
            .release(Instant::now() + Duration::from_secs(1))
            .unwrap();
        let _runtime = attestation.disarm();
        let state = transport.state.lock().unwrap();
        let request = state.request.as_ref().unwrap();
        assert_eq!(
            request["canonical_profile"],
            serde_json::from_slice::<serde_json::Value>(profile.canonical_bytes()).unwrap()
        );
        assert_eq!(request["ownership"]["attempt"], "attempt:fence:7");
        assert_eq!(request["launch"]["environment_policy"], "clear_and_set");
        assert_eq!(
            request["credential_channel"]["protocol"],
            "kit-windows-secret-channel-v1"
        );
        assert!(
            !serde_json::to_string(request)
                .unwrap()
                .contains("actual-secret-value")
        );
        assert_eq!(state.events, ["spawn", "resume"]);
        let request_json = serde_json::to_vec(request).unwrap();
        assert!(
            !request_json
                .windows(CANARY.len())
                .any(|value| value == CANARY)
        );
        assert!(
            state
                .secret_channel
                .as_ref()
                .unwrap()
                .windows(CANARY.len())
                .any(|value| value == CANARY)
        );
        for field in [
            "--root-pid=42",
            "--root-creation-time=99",
            "--job-start-identity=76313a34323a39393a31303030303a313a31",
        ] {
            assert!(
                state
                    .control_arguments
                    .iter()
                    .any(|argument| argument == field)
            );
        }
        assert!(
            state
                .control_arguments
                .iter()
                .any(|argument| argument == &format!("--job-token={}", "61".repeat(64)))
        );
    }

    #[test]
    fn spawn_protocol_rejects_attestation_plan_mismatch_before_resume() {
        let profile = profile(TrustTier::Restricted);
        let transport = Arc::new(ProtocolTransport {
            state: Mutex::new(ProtocolState::default()),
            mismatch_plan: true,
            alias_handles: false,
            abort_proof: true,
        });
        let error = RuntimeBoundary::spawn_suspended(
            &profile,
            &Ownership::new("daemon", "attempt:fence:8").unwrap(),
            &launch(),
            protocol_evidence(transport.clone()),
            None,
            "nonce-1",
            Instant::now() + Duration::from_secs(1),
        )
        .err()
        .expect("mismatched attestation must fail");
        assert_eq!(error.reason, UnavailableReason::IdentityMismatch);
        assert_eq!(transport.state.lock().unwrap().events, ["spawn", "abort"]);
        assert!(
            transport.state.lock().unwrap().request.as_ref().unwrap()["credential_channel"]
                .is_null()
        );
    }

    #[test]
    fn credential_profile_without_broker_channel_is_rejected_before_helper_spawn() {
        let mut spec = ProfileSpec::isolated(
            TrustTier::Restricted,
            Platform::Windows,
            Architecture::X86_64,
            ResourceLimits::new(1, 1, 1, 1, 1, 1, 64, 1),
        );
        spec.credentials.push(CredentialInjection {
            handle: CredentialHandle::new("provider_token").unwrap(),
            mode: CredentialInjectionMode::FileDescriptor,
        });
        let profile = ExecutorProfile::new(spec).unwrap();
        let transport = Arc::new(ProtocolTransport {
            state: Mutex::new(ProtocolState::default()),
            mismatch_plan: false,
            alias_handles: false,
            abort_proof: true,
        });
        let error = RuntimeBoundary::spawn_suspended(
            &profile,
            &Ownership::new("daemon", "attempt:fence:10").unwrap(),
            &launch(),
            protocol_evidence(transport.clone()),
            None,
            "nonce-1",
            Instant::now() + Duration::from_secs(1),
        )
        .err()
        .expect("missing credential channel must fail");
        assert_eq!(error.reason, UnavailableReason::CredentialBrokerUnavailable);
        assert!(transport.state.lock().unwrap().events.is_empty());
    }

    #[test]
    fn credential_probe_requires_broker_and_channel_before_advertising_capability() {
        let mut spec = ProfileSpec::isolated(
            TrustTier::Restricted,
            Platform::Windows,
            Architecture::X86_64,
            ResourceLimits::new(1, 1, 1, 1, 1, 1, 64, 1),
        );
        spec.credentials.push(CredentialInjection {
            handle: CredentialHandle::new("provider_token").unwrap(),
            mode: CredentialInjectionMode::FileDescriptor,
        });
        let profile = ExecutorProfile::new(spec).unwrap();
        let transport = Arc::new(FakeTransport {
            identity: "sha256:trusted".to_owned(),
            transcript: Mutex::new(format!(
                "protocol={HELPER_PROTOCOL}\nhelper_identity=sha256:trusted\nruntime_identity=containerd:1\nmode=container\ncapabilities={}\njob_operations=true\natomic_spawn=true\nconpty=true\n",
                capabilities(&profile)
            )),
            secret_channel: true,
            calls: Mutex::new(0),
        });

        assert_eq!(
            probe_with_transport(&profile, false, transport.clone(), false)
                .unwrap_err()
                .reason,
            UnavailableReason::CredentialBrokerUnavailable
        );
        assert_eq!(*transport.calls.lock().unwrap(), 0);

        let evidence = probe_with_transport(&profile, false, transport.clone(), true).unwrap();
        assert!(
            composite_capabilities(&evidence).contains(&BackendPrimitive::CredentialFileDescriptor)
        );
        assert_eq!(*transport.calls.lock().unwrap(), 1);
    }

    #[test]
    fn successful_abort_retains_identity_and_consumes_guard_once() {
        let profile = profile(TrustTier::Restricted);
        let transport = Arc::new(ProtocolTransport {
            state: Mutex::new(ProtocolState::default()),
            mismatch_plan: false,
            alias_handles: false,
            abort_proof: true,
        });
        let mut attestation = RuntimeBoundary::spawn_suspended(
            &profile,
            &Ownership::new("daemon", "attempt:fence:12").unwrap(),
            &launch(),
            protocol_evidence(transport.clone()),
            None,
            "nonce-1",
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();
        let recovery_identity = attestation.recovery_identity();

        attestation.abort_and_verify().unwrap();
        assert_eq!(attestation.recovery_identity(), recovery_identity);
        drop(attestation);
        assert_eq!(transport.state.lock().unwrap().events, ["spawn", "abort"]);
    }

    #[test]
    fn post_attestation_validation_failure_aborts_and_proves_zero_survivors() {
        let profile = profile(TrustTier::Restricted);
        let transport = Arc::new(ProtocolTransport {
            state: Mutex::new(ProtocolState::default()),
            mismatch_plan: false,
            alias_handles: true,
            abort_proof: true,
        });
        let error = RuntimeBoundary::spawn_suspended(
            &profile,
            &Ownership::new("daemon", "attempt:fence:9").unwrap(),
            &launch(),
            protocol_evidence(transport.clone()),
            None,
            "nonce-1",
            Instant::now() + Duration::from_secs(1),
        )
        .err()
        .expect("aliased handles must fail");
        assert_eq!(error.reason, UnavailableReason::IdentityMismatch);
        assert_eq!(transport.state.lock().unwrap().events, ["spawn", "abort"]);
    }

    #[test]
    fn armed_attestation_aborts_for_every_post_attestation_fault_class() {
        for stage in [
            "handle_wrapping",
            "alias_validation",
            "job_limits",
            "creation_time",
            "membership",
            "composite_construction",
            "persistence",
            "cancellation_registration",
            "registry",
            "resume",
        ] {
            let profile = profile(TrustTier::Restricted);
            let transport = Arc::new(ProtocolTransport {
                state: Mutex::new(ProtocolState::default()),
                mismatch_plan: false,
                alias_handles: false,
                abort_proof: true,
            });
            let attestation = RuntimeBoundary::spawn_suspended(
                &profile,
                &Ownership::new("daemon", format!("attempt:{stage}")).unwrap(),
                &launch(),
                protocol_evidence(transport.clone()),
                None,
                "nonce-1",
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
            drop(attestation);
            assert_eq!(
                transport.state.lock().unwrap().events,
                ["spawn", "abort"],
                "fault stage {stage} did not abort"
            );
        }
    }

    #[test]
    fn unproved_abort_is_outcome_unknown_and_retains_recovery_identity() {
        let profile = profile(TrustTier::Restricted);
        let transport = Arc::new(ProtocolTransport {
            state: Mutex::new(ProtocolState::default()),
            mismatch_plan: false,
            alias_handles: true,
            abort_proof: false,
        });
        let error = RuntimeBoundary::spawn_suspended(
            &profile,
            &Ownership::new("daemon", "attempt:fence:11").unwrap(),
            &launch(),
            protocol_evidence(transport.clone()),
            None,
            "nonce-1",
            Instant::now() + Duration::from_secs(1),
        )
        .err()
        .expect("unproved cleanup must fail");
        assert_eq!(error.reason, UnavailableReason::OutcomeUnknown);
        assert!(error.detail.contains("recovery_identity=container-1:"));
        assert_eq!(transport.state.lock().unwrap().events, ["spawn", "abort"]);
    }

    #[test]
    fn capability_names_without_operational_probe_evidence_are_rejected() {
        let profile = profile(TrustTier::Restricted);
        let transport = Arc::new(FakeTransport {
            identity: "sha256:trusted".to_owned(),
            transcript: Mutex::new(format!(
                "protocol={HELPER_PROTOCOL}\nhelper_identity=sha256:trusted\nruntime_identity=containerd:1\nmode=container\ncapabilities={}\njob_operations=false\natomic_spawn=false\nconpty=false\n",
                capabilities(&profile)
            )),
            secret_channel: false,
            calls: Mutex::new(0),
        });
        assert_eq!(
            probe_with_transport(&profile, false, transport, false)
                .unwrap_err()
                .reason,
            UnavailableReason::PrimitiveMissing
        );
    }

    #[test]
    fn requested_conpty_requires_operational_probe_evidence() {
        let profile = profile(TrustTier::Restricted);
        let transport = Arc::new(FakeTransport {
            identity: "sha256:trusted".to_owned(),
            transcript: Mutex::new(format!(
                "protocol={HELPER_PROTOCOL}\nhelper_identity=sha256:trusted\nruntime_identity=containerd:1\nmode=container\ncapabilities={}\njob_operations=true\natomic_spawn=true\nconpty=false\n",
                capabilities(&profile)
            )),
            secret_channel: false,
            calls: Mutex::new(0),
        });
        assert_eq!(
            probe_with_transport(&profile, true, transport, false)
                .unwrap_err()
                .reason,
            UnavailableReason::PrimitiveMissing
        );
    }

    #[test]
    fn runtime_identity_round_trips_worst_case_fields_and_rejects_limit_edges() {
        let root = RootBinding {
            pid: u32::MAX,
            creation_time: u64::MAX,
            job_locator: "j".repeat(1024),
            job_token: "a".repeat(64),
            job_start_identity: "s".repeat(1024),
        };
        let encoded = encode_start_identity(
            RuntimeMode::HyperV,
            &"p".repeat(1024),
            &"h".repeat(1024),
            &"r".repeat(1024),
            &"i".repeat(1024),
            &"g".repeat(512),
            Some(&root),
        )
        .unwrap();
        let parsed = parse_start_identity(&encoded).unwrap();
        assert_eq!(parsed.mode, RuntimeMode::HyperV);
        assert_eq!(parsed.plan_digest, "p".repeat(1024));
        assert_eq!(parsed.helper_identity, "h".repeat(1024));
        assert_eq!(parsed.runtime_identity, "r".repeat(1024));
        assert_eq!(parsed.isolation_identity, "i".repeat(1024));
        assert_eq!(parsed.generation, "g".repeat(512));
        assert_eq!(parsed.root.job_locator, root.job_locator);
        assert_eq!(parsed.root.job_token, root.job_token);
        assert_eq!(parsed.root.job_start_identity, root.job_start_identity);

        assert!(
            encode_start_identity(
                RuntimeMode::Container,
                "plan",
                &"h".repeat(4 * 1024 + 1),
                "runtime",
                "isolation",
                "generation",
                Some(&root),
            )
            .is_err()
        );
        let mut corrupted = encoded;
        let last = corrupted.len() - 1;
        let replacement = if corrupted.as_bytes()[last] == b'0' {
            "1"
        } else {
            "0"
        };
        corrupted.replace_range(last.., replacement);
        assert_eq!(
            parse_start_identity(&corrupted).unwrap_err().reason,
            UnavailableReason::MalformedEvidence
        );
    }
}
