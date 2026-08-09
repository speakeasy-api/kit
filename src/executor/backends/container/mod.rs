pub mod limits;
pub mod mount;

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use limits::{
    BoundError, BoundedOutput, ContainerResourceLimits, ContainerRuntime, ControlIdentity,
    HELPER_PATH, HELPER_PROTOCOL, NotAvailable, NotAvailableReason, ProbeRecord, ResourceIdentity,
    RuntimeEvidence, bounded_output, probe_backend, record_for_preview,
};
use mount::{MountError, ValidatedMounts};
use sha2::{Digest, Sha256};

use crate::{
    domain::{ids::CommandId, lifecycle::ProcessOwnership},
    executor::{
        cancel::{CancellationIntent, SqliteCancellationCoordinator, WorkspaceIdentity},
        process::{
            own::{
                CapturedStream, PreparedCommandToken, ProcessRegistryRegistration, ProcessState,
                spawn_owned, spawn_owned_with_broker,
            },
            tree::{
                BoundaryControl as TreeBoundaryControl, BoundaryIdentity, BoundaryKind,
                Containment, Inspection, PersistedBoundary,
            },
        },
        profile::{
            Architecture, BackendPrimitive, EgressTransport, ExecutionLabel, ExecutorProfile,
            Platform, RepositoryExecutionPolicy, ResourceLimits, SourceWriteMode,
        },
        secrets::{ExecutorSecretBroker, PreparationError, SecretAuthorization, SecretSpawnPlan},
    },
    workspace::acquire::AcquisitionResult,
};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const MONITOR_RECORD_LIMIT: usize = 16 * 1024;
const TEST_RECORD_NONCE: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

#[derive(Debug)]
pub enum ContainerError {
    NotAvailable(NotAvailable),
    Mount(MountError),
    InvalidImageDigest,
    InvalidBoundaryName,
    EmptyCommand,
    InvalidEnvironment,
    UnboundedResource(ResourceIdentity),
    RandomnessUnavailable,
    Secret(PreparationError),
}

impl fmt::Display for ContainerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAvailable(error) => error.fmt(formatter),
            Self::Mount(error) => error.fmt(formatter),
            Self::InvalidImageDigest => {
                formatter.write_str("container image is not pinned by sha256 digest")
            }
            Self::InvalidBoundaryName => formatter.write_str("invalid container boundary name"),
            Self::EmptyCommand => formatter.write_str("container command is empty"),
            Self::InvalidEnvironment => formatter.write_str("container environment is invalid"),
            Self::UnboundedResource(resource) => {
                write!(
                    formatter,
                    "container resource {resource} must have a finite non-zero bound"
                )
            }
            Self::RandomnessUnavailable => {
                formatter.write_str("cryptographic randomness is unavailable")
            }
            Self::Secret(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ContainerError {}

impl From<NotAvailable> for ContainerError {
    fn from(error: NotAvailable) -> Self {
        Self::NotAvailable(error)
    }
}

impl From<MountError> for ContainerError {
    fn from(error: MountError) -> Self {
        Self::Mount(error)
    }
}

#[derive(Debug)]
pub struct ContainerPlan {
    profile: ExecutorProfile,
    program: PathBuf,
    arguments: Vec<OsString>,
    boundary_name: String,
    ownership_id: String,
    plan_digest: String,
    image_digest: String,
    limits: ContainerResourceLimits,
    mounts: ValidatedMounts,
    evidence: RuntimeEvidence,
    authority: PlanAuthority,
    boundary_record: PathBuf,
    secrets: Option<SecretSpawnPlan>,
    trial_pins: Option<OwnedTrialExecutionPins>,
    trusted_input: Option<(PathBuf, String)>,
    formatter_output_required: bool,
}

pub(crate) struct FormatterExecutionRequest<'a> {
    pub(crate) program: &'a str,
    pub(crate) arguments: &'a [String],
    pub(crate) binary_digest: &'a str,
    pub(crate) config_digest: &'a str,
}

pub(crate) type CheckExecutionRequest<'a> = FormatterExecutionRequest<'a>;

#[derive(Clone, Debug)]
struct OwnedTrialExecutionPins {
    instance_id: String,
    rootfs_lease_id: String,
    writable_lease_id: String,
}

#[derive(Clone, Copy, Debug)]
pub struct TrialExecutionPins<'a> {
    pub instance_id: &'a str,
    pub rootfs_lease_id: &'a str,
    pub writable_lease_id: &'a str,
    pub trusted_read_only_input: Option<&'a Path>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlanAuthority {
    TrustedProbe,
    Preview,
}

impl ContainerPlan {
    pub fn program(&self) -> &OsStr {
        self.program.as_os_str()
    }

    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    pub const fn limits(&self) -> ContainerResourceLimits {
        self.limits
    }

    pub const fn is_runnable(&self) -> bool {
        matches!(self.authority, PlanAuthority::TrustedProbe)
    }

    pub fn invocation_digest_for_test(&self, nonce: &str) -> String {
        invocation_digest(&self.plan_digest, nonce)
    }

    pub fn run(self, owner: ProcessOwnership) -> Result<ExecutionReport, ExecutionError> {
        if matches!(owner, ProcessOwnership::Attempt(_)) {
            return Err(ExecutionError::RegistrationRequired);
        }
        if !self.is_runnable() {
            return self.run_inner(owner, None, None, None);
        }
        Err(ExecutionError::RegistrationRequired)
    }

    pub fn run_observed(
        self,
        owner: ProcessOwnership,
        registration: ProcessRegistryRegistration,
    ) -> Result<ExecutionReport, ExecutionError> {
        if self.secrets.is_some() {
            return Err(ExecutionError::NotAvailable(NotAvailable::new(
                NotAvailableReason::CredentialBrokerUnavailable,
                "credential-bearing plans require an executor secret broker",
            )));
        }
        self.run_inner(owner, None, None, Some(registration))
    }

    pub fn run_registered(
        self,
        owner: ProcessOwnership,
        coordinator: &SqliteCancellationCoordinator,
        workspace: WorkspaceIdentity,
        registry: ProcessRegistryRegistration,
        more_boundaries: bool,
    ) -> Result<ExecutionReport, ExecutionError> {
        if self.secrets.is_some() {
            return Err(ExecutionError::NotAvailable(NotAvailable::new(
                NotAvailableReason::CredentialBrokerUnavailable,
                "credential-bearing plans require an executor secret broker",
            )));
        }
        self.run_inner(
            owner,
            None,
            Some((coordinator, workspace, more_boundaries)),
            Some(registry),
        )
    }

    pub fn run_with_secret_broker(
        self,
        owner: ProcessOwnership,
        authorization: SecretAuthorization,
        broker: &dyn ExecutorSecretBroker,
        registry: ProcessRegistryRegistration,
    ) -> Result<ExecutionReport, ExecutionError> {
        if matches!(owner, ProcessOwnership::Attempt(_)) {
            return Err(ExecutionError::RegistrationRequired);
        }
        self.run_inner(owner, Some((authorization, broker)), None, Some(registry))
    }

    pub fn run_registered_with_secret_broker(
        self,
        owner: ProcessOwnership,
        authorization: SecretAuthorization,
        broker: &dyn ExecutorSecretBroker,
        coordinator: &SqliteCancellationCoordinator,
        workspace: WorkspaceIdentity,
        registry: ProcessRegistryRegistration,
    ) -> Result<ExecutionReport, ExecutionError> {
        self.run_inner(
            owner,
            Some((authorization, broker)),
            Some((coordinator, workspace, false)),
            Some(registry),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run_registered_sequence_with_secret_broker(
        self,
        owner: ProcessOwnership,
        authorization: SecretAuthorization,
        broker: &dyn ExecutorSecretBroker,
        coordinator: &SqliteCancellationCoordinator,
        workspace: WorkspaceIdentity,
        registry: ProcessRegistryRegistration,
        more_boundaries: bool,
    ) -> Result<ExecutionReport, ExecutionError> {
        self.run_inner(
            owner,
            Some((authorization, broker)),
            Some((coordinator, workspace, more_boundaries)),
            Some(registry),
        )
    }

    fn run_inner(
        mut self,
        owner: ProcessOwnership,
        secret_broker: Option<(SecretAuthorization, &dyn ExecutorSecretBroker)>,
        registration: Option<(&SqliteCancellationCoordinator, WorkspaceIdentity, bool)>,
        observer: Option<ProcessRegistryRegistration>,
    ) -> Result<ExecutionReport, ExecutionError> {
        if observer.as_ref().is_some_and(|registration| {
            registration.terminal_transport() == crate::executor::terminal::TerminalTransport::Pty
        }) {
            return Err(ExecutionError::Spawn(io::Error::new(
                io::ErrorKind::Unsupported,
                "container PTY binding is unavailable; the helper cannot bind the contained child",
            )));
        }
        if !self.is_runnable() {
            return Err(ExecutionError::NotAvailable(NotAvailable::new(
                NotAvailableReason::UntrustedTestEvidence,
                "parsed test evidence cannot authorize execution",
            )));
        }
        self.mounts.revalidate().map_err(ExecutionError::Mount)?;
        if let Some((path, expected)) = &self.trusted_input
            && trusted_directory_identity(path).as_deref() != Some(expected)
        {
            return Err(ExecutionError::InvocationMismatch {
                detail: "trusted input mount identity changed before launch".to_owned(),
            });
        }
        let nonce = random_hex().map_err(|_| ExecutionError::RandomnessUnavailable)?;
        let invocation = invocation_digest(&self.plan_digest, &nonce);
        if matches!(owner, ProcessOwnership::Attempt(_)) && registration.is_none() {
            return Err(ExecutionError::RegistrationRequired);
        }

        let mut arguments = self.arguments.clone();
        let runtime_marker = arguments
            .iter()
            .position(|argument| argument == "--runtime-argv")
            .expect("container plans include a runtime argv marker");
        arguments.splice(
            runtime_marker..runtime_marker,
            [
                OsString::from("--record-channel=stdout"),
                OsString::from(format!("--record-limit={MONITOR_RECORD_LIMIT}")),
                OsString::from(format!("--record-nonce={nonce}")),
            ],
        );
        let started = Instant::now();
        let deadline = started
            .checked_add(Duration::from_millis(self.limits.wall_time_millis))
            .ok_or_else(|| {
                ExecutionError::Spawn(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "wall-time limit overflows clock",
                ))
            })?;
        let limits = ResourceLimits::new(
            self.limits.cpu_millis,
            self.limits.memory_bytes,
            self.limits.pids,
            self.limits.file_bytes,
            self.limits.disk_bytes,
            self.limits.io_bytes,
            self.limits.output_bytes,
            self.limits.wall_time_millis,
        );
        let mut command = Command::new(&self.program);
        command.args(&arguments);
        let request_id = registration
            .as_ref()
            .map(|_| CommandId::generate())
            .transpose()
            .map_err(|error| ExecutionError::Spawn(io::Error::other(error.to_string())))?;
        let control = ContainerBoundaryControl::new(&self, &invocation)?;
        let control_attestations = control.attestations.clone();
        let boundary_record = self.boundary_record.clone();
        let durable_registration = registration.as_ref().map(|(coordinator, workspace, more)| {
            ((*coordinator).clone(), workspace.clone(), *more)
        });
        let mut prepared = PreparedCommandToken::issue_observed_registered(
            command,
            owner,
            control,
            move |record: &PersistedBoundary| persist_boundary(&boundary_record, record),
            move |claim, boundary| {
                let Some((coordinator, workspace, _)) = &durable_registration else {
                    return Ok(());
                };
                let ProcessOwnership::Attempt(attempt) = owner else {
                    return Err(io::Error::other(
                        "registered container execution must be attempt-owned",
                    ));
                };
                let intent = CancellationIntent::new(
                    request_id.expect("registered execution has a request ID"),
                    attempt,
                    *claim,
                    boundary.clone(),
                    workspace.clone(),
                    CONTROL_TIMEOUT,
                )
                .map_err(|error| io::Error::other(error.to_string()))?;
                coordinator
                    .register_claim(&intent)
                    .map_err(|error| io::Error::other(error.to_string()))
            },
            observer,
            deadline,
            limits,
        )
        .map_err(ExecutionError::Spawn)?
        .bind_profile(self.profile.clone());
        if let Some(secrets) = self.secrets.take() {
            let (authorization, _) = secret_broker.expect("credential broker checked by caller");
            prepared = prepared.bind_secrets(secrets.authorize(authorization));
        }
        let mut process = match secret_broker {
            Some((_, broker)) => spawn_owned_with_broker(prepared, limits, broker),
            None => spawn_owned(prepared, limits),
        }
        .map_err(ExecutionError::Spawn)?;
        let output = match process.wait() {
            Ok(output) => output,
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                let mut confirmation = None;
                if let (
                    ProcessOwnership::Attempt(attempt),
                    Some((coordinator, _, more_boundaries)),
                    Some(request_id),
                ) = (owner, &registration, request_id)
                {
                    match coordinator.confirm_quiescence(attempt, request_id, *more_boundaries) {
                        Ok(value) => confirmation = Some(value),
                        Err(error) => {
                            return Err(ExecutionError::Launched {
                                source: Box::new(ExecutionError::OutcomeUnknown {
                                    detail: format!(
                                        "durable timeout quiescence confirmation failed: {error}"
                                    ),
                                }),
                                evidence: Box::new(self.failure_evidence(
                                    &invocation,
                                    owner,
                                    process.record().process_id(),
                                    false,
                                    true,
                                    (None, control_attestation_digest(&control_attestations)),
                                )),
                            });
                        }
                    }
                }
                return Err(ExecutionError::Launched {
                    source: Box::new(ExecutionError::Bound(BoundError::new(
                        ResourceIdentity::WallTime,
                        self.limits.wall_time_millis,
                        Some(started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64),
                        "owned-process-monotonic-clock",
                    ))),
                    evidence: Box::new(self.failure_evidence(
                        &invocation,
                        owner,
                        process.record().process_id(),
                        true,
                        true,
                        (
                            confirmation,
                            control_attestation_digest(&control_attestations),
                        ),
                    )),
                });
            }
            Err(error) => {
                return Err(ExecutionError::Launched {
                    source: Box::new(ExecutionError::OutcomeUnknown {
                        detail: format!("owned helper completion failed: {error}"),
                    }),
                    evidence: Box::new(self.failure_evidence(
                        &invocation,
                        owner,
                        process.record().process_id(),
                        false,
                        true,
                        (None, control_attestation_digest(&control_attestations)),
                    )),
                });
            }
        };
        let capture_digest = bounded_capture_digest(&output);
        let failure_evidence = self.failure_evidence(
            &invocation,
            owner,
            process.record().process_id(),
            true,
            false,
            (None, None),
        );
        (|| {
            if let (
                ProcessOwnership::Attempt(attempt),
                Some((coordinator, _, more_boundaries)),
                Some(request_id),
            ) = (owner, registration, request_id)
            {
                coordinator
                    .confirm_quiescence(attempt, request_id, more_boundaries)
                    .map_err(|error| ExecutionError::OutcomeUnknown {
                        detail: format!("durable quiescence confirmation failed: {error}"),
                    })?;
            }
            let termination = match process.record().state() {
                ProcessState::Exited { code, signal, .. } => ProcessTermination { code, signal },
                ProcessState::Started => {
                    return Err(ExecutionError::OutcomeUnknown {
                        detail: "owned helper returned output without an exit record".to_owned(),
                    });
                }
            };
            let monitor = TrustedMonitorChannel::new(&output.stdout);
            let transcript = monitor.transcript()?;
            let formatter_channel = TrustedFormatterOutputChannel::new(&output.stderr);
            let formatter_output = if self.formatter_output_required {
                Some(formatter_channel.transcript()?)
            } else {
                None
            };
            let mut report = self.classify_exit(
                termination,
                &nonce,
                Some(transcript),
                formatter_output,
                Some(capture_digest),
            )?;
            report.evidence.ownership_id = ownership_digest(owner);
            report.evidence.process_id = process.record().process_id().to_string();
            Ok(report)
        })()
        .map_err(|source| ExecutionError::Launched {
            source: Box::new(source),
            evidence: Box::new(failure_evidence),
        })
    }

    fn failure_evidence(
        &self,
        invocation_digest: &str,
        owner: ProcessOwnership,
        process_id: crate::domain::ids::ProcessId,
        quiescent: bool,
        kill_attempted: bool,
        cancellation: (
            Option<crate::executor::cancel::DurableCancellationConfirmation>,
            Option<String>,
        ),
    ) -> ExecutionEvidence {
        let (confirmation, helper_attestation_digest) = cancellation;
        let (source, request_id, fence, phase, commit_digest) = confirmation.map_or_else(
            || {
                (
                    helper_attestation_digest
                        .as_ref()
                        .map(|_| "helper_control_attestation".to_owned()),
                    None,
                    None,
                    helper_attestation_digest
                        .as_ref()
                        .map(|_| "quiescent".to_owned()),
                    helper_attestation_digest,
                )
            },
            |confirmation| {
                (
                    Some("coordinator_commit".to_owned()),
                    Some(confirmation.request_id),
                    Some(confirmation.fence),
                    Some(confirmation.phase),
                    Some(confirmation.commit_digest),
                )
            },
        );
        ExecutionEvidence {
            resolved_image_digest: self.image_digest.clone(),
            boundary_id: self.boundary_name.clone(),
            instance_id: "none".to_owned(),
            rootfs_lease_id: "none".to_owned(),
            writable_lease_id: "none".to_owned(),
            plan_digest: self.plan_digest.clone(),
            invocation_digest: invocation_digest.to_owned(),
            ownership_id: ownership_digest(owner),
            process_id: process_id.to_string(),
            runtime_identity: self.evidence.runtime_identity().to_owned(),
            helper_identity: self.evidence.helper_identity().to_owned(),
            bounded_capture_digest: bounded_capture_digest_empty(),
            formatter_binary_digest: None,
            formatter_config_digest: None,
            formatter_artifact_digest: None,
            survivors: if quiescent { 0 } else { u32::MAX },
            boundary_absent: quiescent,
            kill_attempted,
            reaped: quiescent,
            inspected: quiescent,
            quiescent,
            cancellation_source: source,
            cancellation_request_id: request_id,
            cancellation_fence: fence,
            cancellation_phase: phase,
            cancellation_commit_digest: commit_digest,
        }
    }

    /// Exercises monitor parsing/classification without making a preview plan runnable.
    pub fn classify_exit_for_test(
        &self,
        status_success: bool,
        transcript: Option<&str>,
    ) -> Result<ExecutionReport, ExecutionError> {
        self.classify_exit(
            ProcessTermination {
                code: Some(if status_success { 0 } else { 1 }),
                signal: None,
            },
            TEST_RECORD_NONCE,
            transcript,
            None,
            None,
        )
    }

    pub fn classify_completion_for_test(
        &self,
        code: Option<i32>,
        signal: Option<i32>,
        nonce: &str,
        transcript: Option<&str>,
    ) -> Result<ExecutionReport, ExecutionError> {
        self.classify_exit(
            ProcessTermination { code, signal },
            nonce,
            transcript,
            None,
            None,
        )
    }

    pub fn classify_formatter_completion_for_test(
        &self,
        code: Option<i32>,
        signal: Option<i32>,
        nonce: &str,
        monitor: Option<&str>,
        formatter_output: Option<&str>,
    ) -> Result<ExecutionReport, ExecutionError> {
        self.classify_exit(
            ProcessTermination { code, signal },
            nonce,
            monitor,
            formatter_output,
            None,
        )
    }

    fn classify_exit(
        &self,
        termination: ProcessTermination,
        nonce: &str,
        transcript: Option<&str>,
        formatter_output: Option<&str>,
        capture_digest: Option<String>,
    ) -> Result<ExecutionReport, ExecutionError> {
        let transcript = transcript.ok_or_else(|| ExecutionError::OutcomeUnknown {
            detail: "helper exited without a monitor record".to_owned(),
        })?;
        let record = ExecutionRecord::parse(transcript)?;
        if record.nonce != nonce
            || record.ownership_id != self.ownership_id
            || record.plan_digest != self.plan_digest
            || record.resolved_image_digest != self.image_digest
            || record.boundary_id != self.boundary_name
            || record.invocation_digest != invocation_digest(&self.plan_digest, nonce)
            || record.runtime_identity != self.evidence.runtime_identity()
            || record.helper_identity != self.evidence.helper_identity()
        {
            return Err(ExecutionError::InvocationMismatch {
                detail: "monitor record is not bound to this exact invocation".to_owned(),
            });
        }
        if let Some(expected) = &self.trial_pins
            && (record.instance_id != expected.instance_id
                || record.rootfs_lease_id != expected.rootfs_lease_id
                || record.writable_lease_id != expected.writable_lease_id)
        {
            return Err(ExecutionError::InvocationMismatch {
                detail: "monitor storage evidence does not match the VM contract".to_owned(),
            });
        }
        if !record.boundary_absent || record.survivors != 0 {
            return Err(ExecutionError::NotQuiescent {
                survivors: record.survivors,
                detail: "monitor did not prove the boundary absent".to_owned(),
            });
        }
        let outcome = match record.outcome {
            RecordedOutcome::Success if termination.code == Some(0) => ExecutionOutcome::Success,
            RecordedOutcome::Exit(code) if termination.code == Some(code) && code != 0 => {
                ExecutionOutcome::Exit(code)
            }
            RecordedOutcome::Signal(signal) if termination.signal == Some(signal) => {
                ExecutionOutcome::Signal(signal)
            }
            RecordedOutcome::Bound(resource)
                if !record.monitor_evidence.is_empty() && record.monitor_evidence != "none" =>
            {
                return Err(ExecutionError::Bound(BoundError::new(
                    resource,
                    self.limits.bound(resource),
                    record.observed,
                    record.monitor_evidence,
                )));
            }
            _ => {
                return Err(ExecutionError::OutcomeUnknown {
                    detail: "runtime exit and trusted monitor outcome disagree".to_owned(),
                });
            }
        };
        let child_output = match (self.formatter_output_required, formatter_output) {
            (true, Some(output)) => Some(FormatterOutputRecord::parse(
                output,
                nonce,
                &self.plan_digest,
                &record.invocation_digest,
            )?),
            (true, None) => {
                return Err(ExecutionError::MonitorProtocol(
                    "formatter helper omitted its output report".to_owned(),
                ));
            }
            (false, _) => None,
        };
        Ok(ExecutionReport {
            outcome,
            child_output,
            evidence: ExecutionEvidence {
                resolved_image_digest: record.resolved_image_digest,
                boundary_id: record.boundary_id,
                instance_id: record.instance_id,
                rootfs_lease_id: record.rootfs_lease_id,
                writable_lease_id: record.writable_lease_id,
                plan_digest: record.plan_digest,
                invocation_digest: record.invocation_digest,
                ownership_id: "none".to_owned(),
                process_id: "none".to_owned(),
                runtime_identity: record.runtime_identity,
                helper_identity: record.helper_identity,
                bounded_capture_digest: capture_digest.unwrap_or_else(bounded_capture_digest_empty),
                formatter_binary_digest: record.formatter_binary_digest,
                formatter_config_digest: record.formatter_config_digest,
                formatter_artifact_digest: record.formatter_artifact_digest,
                survivors: record.survivors,
                boundary_absent: record.boundary_absent,
                kill_attempted: false,
                reaped: true,
                inspected: true,
                quiescent: true,
                cancellation_source: None,
                cancellation_request_id: None,
                cancellation_fence: None,
                cancellation_phase: None,
                cancellation_commit_digest: None,
            },
        })
    }
}

/// The helper monitor pipe is protocol evidence, never user process output.
struct TrustedMonitorChannel<'a>(&'a CapturedStream);

impl<'a> TrustedMonitorChannel<'a> {
    const fn new(stream: &'a CapturedStream) -> Self {
        Self(stream)
    }

    fn transcript(&self) -> Result<&str, ExecutionError> {
        std::str::from_utf8(self.0.raw_bytes())
            .map_err(|_| ExecutionError::MonitorProtocol("monitor record is not UTF-8".to_owned()))
    }
}

impl fmt::Debug for TrustedMonitorChannel<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TrustedMonitorChannel")
            .field("bytes", &self.0.original_bytes())
            .finish()
    }
}

/// Formatter child output is a separate trusted helper report, never monitor evidence.
struct TrustedFormatterOutputChannel<'a>(&'a CapturedStream);

impl<'a> TrustedFormatterOutputChannel<'a> {
    const fn new(stream: &'a CapturedStream) -> Self {
        Self(stream)
    }

    fn transcript(&self) -> Result<&str, ExecutionError> {
        std::str::from_utf8(self.0.raw_bytes()).map_err(|_| {
            ExecutionError::MonitorProtocol("formatter output report is not UTF-8".to_owned())
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionOutcome {
    Success,
    Exit(i32),
    Signal(i32),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionEvidence {
    pub resolved_image_digest: String,
    pub boundary_id: String,
    pub instance_id: String,
    pub rootfs_lease_id: String,
    pub writable_lease_id: String,
    pub plan_digest: String,
    pub invocation_digest: String,
    pub ownership_id: String,
    pub process_id: String,
    pub runtime_identity: String,
    pub helper_identity: String,
    pub bounded_capture_digest: String,
    pub formatter_binary_digest: Option<String>,
    pub formatter_config_digest: Option<String>,
    pub formatter_artifact_digest: Option<String>,
    pub survivors: u32,
    pub boundary_absent: bool,
    pub kill_attempted: bool,
    pub reaped: bool,
    pub inspected: bool,
    pub quiescent: bool,
    pub cancellation_source: Option<String>,
    pub cancellation_request_id: Option<String>,
    pub cancellation_fence: Option<u64>,
    pub cancellation_phase: Option<String>,
    pub cancellation_commit_digest: Option<String>,
}

fn ownership_digest(owner: ProcessOwnership) -> String {
    blake3::hash(&serde_json::to_vec(&owner).expect("process ownership serialization cannot fail"))
        .to_hex()
        .to_string()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionStreamReport {
    pub length: u64,
    pub digest: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionChildOutput {
    pub stdout: ExecutionStreamReport,
    pub stderr: ExecutionStreamReport,
    pub attestation: String,
}

fn bounded_capture_digest(output: &crate::executor::process::own::ProcessOutput) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"kit-container-bounded-capture-v1");
    for stream in [&output.stdout, &output.stderr] {
        hasher.update(&stream.original_bytes().to_le_bytes());
        hasher.update(&stream.truncated_bytes().to_le_bytes());
        hasher.update(&(stream.raw_bytes().len() as u64).to_le_bytes());
        hasher.update(stream.raw_bytes());
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn bounded_capture_digest_empty() -> String {
    let output = b"kit-container-bounded-capture-v1";
    let mut hasher = blake3::Hasher::new();
    hasher.update(output);
    for _ in 0..2 {
        hasher.update(&0u64.to_le_bytes());
        hasher.update(&0u64.to_le_bytes());
        hasher.update(&0u64.to_le_bytes());
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionReport {
    pub outcome: ExecutionOutcome,
    pub child_output: Option<ExecutionChildOutput>,
    pub evidence: ExecutionEvidence,
}

#[derive(Clone, Copy)]
struct ProcessTermination {
    code: Option<i32>,
    signal: Option<i32>,
}

#[derive(Debug)]
pub enum ExecutionError {
    NotAvailable(NotAvailable),
    Mount(MountError),
    Spawn(io::Error),
    Wait(io::Error),
    RandomnessUnavailable,
    RegistrationRequired,
    MonitorProtocol(String),
    InvocationMismatch {
        detail: String,
    },
    Bound(BoundError),
    OutcomeUnknown {
        detail: String,
    },
    NotQuiescent {
        survivors: u32,
        detail: String,
    },
    Launched {
        source: Box<ExecutionError>,
        evidence: Box<ExecutionEvidence>,
    },
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAvailable(error) => error.fmt(formatter),
            Self::Mount(error) => error.fmt(formatter),
            Self::Spawn(error) => write!(
                formatter,
                "failed to start trusted container helper: {error}"
            ),
            Self::Wait(error) => write!(
                formatter,
                "failed to wait for trusted container helper: {error}"
            ),
            Self::RandomnessUnavailable => {
                formatter.write_str("cryptographic randomness is unavailable for execution")
            }
            Self::RegistrationRequired => formatter
                .write_str("attempt-owned container execution requires durable registration"),
            Self::MonitorProtocol(error) => write!(formatter, "invalid monitor evidence: {error}"),
            Self::InvocationMismatch { detail } => {
                write!(formatter, "monitor invocation mismatch: {detail}")
            }
            Self::Bound(error) => error.fmt(formatter),
            Self::OutcomeUnknown { detail } => {
                write!(formatter, "container outcome unknown: {detail}")
            }
            Self::NotQuiescent { survivors, detail } => {
                write!(
                    formatter,
                    "container boundary not quiescent ({survivors} survivors): {detail}"
                )
            }
            Self::Launched { source, .. } => source.fmt(formatter),
        }
    }
}

impl std::error::Error for ExecutionError {}

#[allow(clippy::too_many_arguments)]
pub fn prepare<I, S>(
    profile: &ExecutorProfile,
    workspace: &AcquisitionResult,
    build: &Path,
    temp: &Path,
    boundary_name: &str,
    image: &str,
    command: I,
) -> Result<ContainerPlan, ContainerError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    validate_supported_profile(profile)?;
    let evidence = probe_backend()?;
    build_plan(
        evidence,
        PlanAuthority::TrustedProbe,
        profile,
        Some(workspace),
        None,
        None,
        build,
        temp,
        boundary_name,
        image,
        command,
        None,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_trial<I, S>(
    profile: &ExecutorProfile,
    workspace: &AcquisitionResult,
    build: &Path,
    temp: &Path,
    boundary_name: &str,
    image: &str,
    command: I,
    pins: TrialExecutionPins<'_>,
) -> Result<ContainerPlan, ContainerError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    validate_supported_profile(profile)?;
    let evidence = probe_backend()?;
    build_plan(
        evidence,
        PlanAuthority::TrustedProbe,
        profile,
        Some(workspace),
        None,
        None,
        build,
        temp,
        boundary_name,
        image,
        command,
        None,
        Some(pins),
        None,
    )
}

/// Builds the exact argv shape from an untrusted transcript for deterministic tests. The returned
/// plan reports `is_runnable() == false`, and `run()` always returns typed `NotAvailable`.
#[allow(clippy::too_many_arguments)]
pub fn preview<I, S>(
    evidence: &ProbeRecord,
    profile: &ExecutorProfile,
    workspace: &AcquisitionResult,
    build: &Path,
    temp: &Path,
    boundary_name: &str,
    image: &str,
    command: I,
) -> Result<ContainerPlan, ContainerError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    build_plan(
        record_for_preview(evidence),
        PlanAuthority::Preview,
        profile,
        Some(workspace),
        None,
        None,
        build,
        temp,
        boundary_name,
        image,
        command,
        None,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_plan<I, S>(
    evidence: RuntimeEvidence,
    authority: PlanAuthority,
    profile: &ExecutorProfile,
    workspace: Option<&AcquisitionResult>,
    overlay_mounts: Option<(&Path, &Path)>,
    immutable_source: Option<&Path>,
    build: &Path,
    temp: &Path,
    boundary_name: &str,
    image: &str,
    command: I,
    environment: Option<&BTreeMap<String, String>>,
    trial_pins: Option<TrialExecutionPins<'_>>,
    formatter: Option<FormatterExecutionRequest<'_>>,
) -> Result<ContainerPlan, ContainerError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    validate_supported_profile_mode(profile, overlay_mounts.is_some())?;
    let secrets = workspace
        .map(|workspace| {
            SecretSpawnPlan::for_container(
                profile,
                workspace,
                "container-helper-run",
                PathBuf::from(HELPER_PATH),
            )
        })
        .transpose()
        .map_err(ContainerError::Secret)?
        .flatten();
    if !safe_name(boundary_name) {
        return Err(ContainerError::InvalidBoundaryName);
    }
    validate_image(image)?;
    let image_digest = format!("sha256:{}", pinned_image_digest(image));
    let formatter_output_required = formatter.is_some();
    let mut child = command.into_iter().map(Into::into).collect::<Vec<_>>();
    if child.is_empty() || child[0].is_empty() {
        return Err(ContainerError::EmptyCommand);
    }
    if environment.is_some_and(|environment| {
        environment.iter().any(|(key, value)| {
            key.is_empty()
                || key.contains(['=', '\0', '\n', '\r'])
                || value.contains(['\0', '\n', '\r'])
        })
    }) {
        return Err(ContainerError::InvalidEnvironment);
    }
    let mounts = match (workspace, overlay_mounts, immutable_source) {
        (Some(workspace), None, None) => ValidatedMounts::acquire(profile, workspace, build, temp)?,
        (None, Some((source, overlay)), None) => {
            ValidatedMounts::acquire_overlay(profile, source, overlay, build, temp)?
        }
        (None, None, Some(source)) => {
            ValidatedMounts::acquire_immutable(profile, source, build, temp)?
        }
        _ => {
            return Err(ContainerError::Mount(MountError::InvalidProfile(
                "ambiguous mount request",
            )));
        }
    };
    let resources = profile.resources();
    let limits = ContainerResourceLimits::new(
        resources.cpu_millis,
        resources.memory_bytes,
        resources.pids,
        resources.file_bytes,
        resources.disk_bytes,
        resources.io_bytes,
        resources.output_bytes,
        resources.wall_time_millis,
    );
    limits
        .validate()
        .map_err(ContainerError::UnboundedResource)?;
    let ownership_id = random_hex().map_err(|_| ContainerError::RandomnessUnavailable)?;
    let boundary_name = internal_boundary_name(boundary_name, &ownership_id);

    let mut arguments = vec![
        OsString::from("run"),
        OsString::from(format!("--protocol={HELPER_PROTOCOL}")),
        option_with_path("--runtime=", evidence.runtime_path()),
        OsString::from(format!(
            "--runtime-identity={}",
            evidence.runtime_identity()
        )),
        OsString::from(format!("--runtime-version={}", evidence.runtime_version())),
        OsString::from(format!("--runtime-config={}", evidence.runtime_config())),
        OsString::from(format!("--helper-identity={}", evidence.helper_identity())),
        OsString::from(format!("--boundary={boundary_name}")),
        OsString::from(format!("--ownership-id={ownership_id}")),
        OsString::from(format!("--profile-digest={}", profile.digest())),
        option_with_path("--source=", &mounts.source),
        OsString::from(format!("--source-identity={}", mounts.source_identity())),
        option_with_path("--build=", &mounts.build),
        OsString::from(format!("--build-identity={}", mounts.build_identity())),
        option_with_path("--temp=", &mounts.temp),
        OsString::from(format!("--temp-identity={}", mounts.temp_identity())),
        OsString::from("--mount-lease=pinned"),
        OsString::from(format!("--cpu-aggregate-ms={}", limits.cpu_millis)),
        OsString::from(format!("--memory-bytes={}", limits.memory_bytes)),
        OsString::from(format!("--pids={}", limits.pids)),
        OsString::from(format!("--file-bytes={}", limits.file_bytes)),
        OsString::from(format!("--writable-bind-quota={}", limits.disk_bytes)),
        OsString::from(format!("--boundary-io-bytes={}", limits.io_bytes)),
        OsString::from(format!("--output-bytes={}", limits.output_bytes)),
        OsString::from(format!("--wall-time-ms={}", limits.wall_time_millis)),
        OsString::from("--kill-whole-boundary"),
        OsString::from("--require-quiescence"),
    ];
    if let (Some(overlay), Some(identity)) = (&mounts.overlay, mounts.overlay_identity()) {
        arguments.extend([
            option_with_path("--overlay=", overlay),
            OsString::from(format!("--overlay-identity={identity}")),
            OsString::from("--overlay-separate-writable-layer"),
        ]);
    }
    let (trial_pins, trusted_input) = if let Some(pins) = trial_pins {
        if !safe_name(pins.instance_id)
            || !safe_name(pins.rootfs_lease_id)
            || !safe_name(pins.writable_lease_id)
        {
            return Err(ContainerError::InvalidBoundaryName);
        }
        arguments.extend([
            OsString::from(format!("--expected-instance-id={}", pins.instance_id)),
            OsString::from(format!(
                "--expected-rootfs-lease-id={}",
                pins.rootfs_lease_id
            )),
            OsString::from(format!(
                "--expected-writable-lease-id={}",
                pins.writable_lease_id
            )),
        ]);
        let trusted_input = pins
            .trusted_read_only_input
            .map(|path| validate_trusted_input(path, &mounts))
            .transpose()?;
        if let Some((path, identity)) = &trusted_input {
            arguments.extend([
                option_with_path("--trusted-input=", path),
                OsString::from(format!("--trusted-input-identity={identity}")),
            ]);
        }
        (
            Some(OwnedTrialExecutionPins {
                instance_id: pins.instance_id.to_owned(),
                rootfs_lease_id: pins.rootfs_lease_id.to_owned(),
                writable_lease_id: pins.writable_lease_id.to_owned(),
            }),
            trusted_input,
        )
    } else {
        (None, None)
    };
    if let Some(secrets) = &secrets {
        arguments.extend(secrets.helper_arguments().into_iter().map(OsString::from));
    }
    if let Some(formatter) = formatter {
        append_formatter_arguments(
            &mut arguments,
            &image_digest,
            (limits.output_bytes / 8).max(1),
            formatter,
        );
    }
    if profile.egress().is_empty() {
        arguments.push(OsString::from("--network-policy=deny"));
    } else {
        arguments.extend([
            OsString::from("--network-policy=proxy-only"),
            OsString::from(format!("--proxy-lease={}", evidence.proxy_lease())),
            OsString::from("--deny-direct-egress"),
            OsString::from("--deny-peer"),
            OsString::from("--deny-gateway"),
            OsString::from("--deny-udp"),
            OsString::from("--revalidate-dns"),
            OsString::from("--revalidate-connections"),
        ]);
        for grant in profile.egress() {
            arguments.push(OsString::from(format!(
                "--grant=tcp:{}:{}",
                grant.destination(),
                grant.port()
            )));
        }
    }
    arguments.push(OsString::from("--runtime-argv"));
    arguments.extend([
        OsString::from("run"),
        OsString::from(format!("--name={boundary_name}")),
        OsString::from("--rm"),
        OsString::from("--pull=never"),
        OsString::from("--read-only"),
        OsString::from("--ipc=private"),
        OsString::from("--pid=private"),
        OsString::from("--cap-drop=ALL"),
        OsString::from("--security-opt=no-new-privileges"),
        OsString::from(format!("--security-opt=seccomp={}", evidence.seccomp())),
    ]);
    match evidence.runtime() {
        ContainerRuntime::Podman => arguments.push(OsString::from("--userns=keep-id")),
        ContainerRuntime::Docker => arguments.push(OsString::from("--user=0:0")),
    }
    if let Some(environment) = environment {
        arguments.extend(
            environment
                .iter()
                .map(|(key, value)| OsString::from(format!("--env={key}={value}"))),
        );
    }
    if profile.egress().is_empty() {
        arguments.push(OsString::from("--network=none"));
    } else {
        arguments.extend([
            OsString::from(format!("--network={}", evidence.proxy_network())),
            OsString::from(format!("--env=HTTP_PROXY={}", evidence.proxy_endpoint())),
            OsString::from(format!("--env=HTTPS_PROXY={}", evidence.proxy_endpoint())),
            OsString::from(format!("--env=ALL_PROXY={}", evidence.proxy_endpoint())),
            OsString::from("--env=NO_PROXY="),
        ]);
    }
    if let Some(base_target) = &mounts.base_source_target {
        arguments.extend([
            bind_mount("source", base_target, true),
            bind_mount("overlay", &mounts.source_target, false),
        ]);
    } else {
        arguments.push(bind_mount("source", &mounts.source_target, true));
    }
    arguments.extend([
        bind_mount("build", &mounts.build_target, false),
        bind_mount("temp", &mounts.temp_target, false),
        option_with_path("--workdir=", &mounts.source_target),
        OsString::from("--env=HOME=/tmp"),
        OsString::from("--env=TMPDIR=/tmp"),
        OsString::from("--memory"),
        OsString::from(limits.memory_bytes.to_string()),
        OsString::from("--memory-swap"),
        OsString::from(limits.memory_bytes.to_string()),
        OsString::from("--pids-limit"),
        OsString::from(limits.pids.to_string()),
        OsString::from("--ulimit"),
        OsString::from(format!("fsize={0}:{0}", limits.file_bytes)),
        OsString::from("--stop-timeout=0"),
        OsString::from(image),
    ]);
    if trusted_input.is_some() {
        let runtime_marker = arguments
            .iter()
            .position(|argument| argument == "--runtime-argv")
            .expect("container plans include a runtime argv marker");
        let runtime_mount = bind_mount("trusted-input", Path::new("/kit-trusted-input"), true);
        arguments.insert(runtime_marker + 1, runtime_mount);
    }
    arguments.append(&mut child);

    let boundary_record = mounts
        .temp
        .parent()
        .expect("validated mounts have an allocation parent")
        .join(format!(".kit-boundary-{ownership_id}"));
    let runtime_marker = arguments
        .iter()
        .position(|argument| argument == "--runtime-argv")
        .expect("container plans include a runtime argv marker");
    arguments.splice(
        runtime_marker..runtime_marker,
        [
            OsString::from("--persist-before-release"),
            option_with_path("--boundary-record=", &boundary_record),
        ],
    );
    let plan_digest = canonical_plan_digest(&PathBuf::from(HELPER_PATH), profile, &arguments);
    let runtime_marker = arguments
        .iter()
        .position(|argument| argument == "--runtime-argv")
        .expect("container plans include a runtime argv marker");
    arguments.insert(
        runtime_marker,
        OsString::from(format!("--plan-digest={plan_digest}")),
    );
    Ok(ContainerPlan {
        profile: profile.clone(),
        program: PathBuf::from(HELPER_PATH),
        arguments,
        boundary_name,
        ownership_id,
        plan_digest,
        image_digest,
        limits,
        mounts,
        evidence,
        authority,
        boundary_record,
        secrets,
        trial_pins,
        trusted_input,
        formatter_output_required,
    })
}

fn append_formatter_arguments(
    arguments: &mut Vec<OsString>,
    image_digest: &str,
    output_bytes: u64,
    formatter: FormatterExecutionRequest<'_>,
) {
    arguments.extend([
        OsString::from(format!("--formatter-program={}", formatter.program)),
        OsString::from(format!(
            "--formatter-requested-binary-digest={}",
            formatter.binary_digest
        )),
        OsString::from(format!(
            "--formatter-requested-config-digest={}",
            formatter.config_digest
        )),
        OsString::from(format!(
            "--formatter-requested-artifact-digest={image_digest}"
        )),
        OsString::from(format!("--formatter-output-complete-bytes={output_bytes}")),
        OsString::from("--formatter-output-channel=stderr"),
        OsString::from("--formatter-output-redact-before-report"),
    ]);
    arguments.extend(
        formatter
            .arguments
            .iter()
            .map(|argument| OsString::from(format!("--formatter-argument={argument}"))),
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_captured<I, S>(
    profile: &ExecutorProfile,
    workspace: &AcquisitionResult,
    build: &Path,
    temp: &Path,
    boundary_name: &str,
    image: &str,
    command: I,
    environment: &BTreeMap<String, String>,
    capture: CheckExecutionRequest<'_>,
) -> Result<ContainerPlan, ContainerError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    validate_supported_profile(profile)?;
    let evidence = probe_backend()?;
    build_plan(
        evidence,
        PlanAuthority::TrustedProbe,
        profile,
        Some(workspace),
        None,
        None,
        build,
        temp,
        boundary_name,
        image,
        command,
        Some(environment),
        None,
        Some(capture),
    )
}

fn validate_trusted_input(
    path: &Path,
    mounts: &ValidatedMounts,
) -> Result<(PathBuf, String), ContainerError> {
    let canonical = fs::canonicalize(path)
        .map_err(|error| ContainerError::Mount(MountError::IdentityUnavailable(error)))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| ContainerError::Mount(MountError::IdentityUnavailable(error)))?;
    if canonical != path
        || !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || canonical.starts_with(&mounts.source)
        || canonical.starts_with(&mounts.build)
        || canonical.starts_with(&mounts.temp)
        || mounts.source.starts_with(&canonical)
        || mounts.build.starts_with(&canonical)
        || mounts.temp.starts_with(&canonical)
        || canonical.to_string_lossy().contains(',')
    {
        return Err(ContainerError::Mount(MountError::InvalidPath {
            kind: "trusted input",
            path: path.to_owned(),
            reason: "path must be a distinct canonical symlink-free directory",
        }));
    }
    let identity = trusted_directory_identity(&canonical).ok_or_else(|| {
        ContainerError::Mount(MountError::IdentityUnavailable(io::Error::new(
            io::ErrorKind::Unsupported,
            "trusted input mount identity unavailable",
        )))
    })?;
    Ok((canonical, identity))
}

#[cfg(unix)]
fn trusted_directory_identity(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path).ok()?;
    (metadata.is_dir() && !metadata.file_type().is_symlink()).then(|| {
        format!(
            "{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.ctime(),
            metadata.ctime_nsec()
        )
    })
}

#[cfg(not(unix))]
fn trusted_directory_identity(_path: &Path) -> Option<String> {
    None
}

fn validate_supported_profile(profile: &ExecutorProfile) -> Result<(), ContainerError> {
    validate_supported_profile_mode(profile, false)
}

fn validate_supported_profile_mode(
    profile: &ExecutorProfile,
    mutation_overlay: bool,
) -> Result<(), ContainerError> {
    if profile.label() != ExecutionLabel::Restricted || profile.platform() != Platform::Linux {
        return Err(ContainerError::NotAvailable(NotAvailable::new(
            NotAvailableReason::PrimitiveMissing,
            "container backend requires a restricted Linux profile",
        )));
    }
    let expected_architecture = if cfg!(target_arch = "aarch64") {
        Architecture::Aarch64
    } else if cfg!(target_arch = "x86_64") {
        Architecture::X86_64
    } else {
        return Err(ContainerError::NotAvailable(NotAvailable::new(
            NotAvailableReason::UnsupportedHost,
            "container backend supports only x86_64 and aarch64 hosts",
        )));
    };
    if profile.architecture() != expected_architecture {
        return Err(ContainerError::NotAvailable(NotAvailable::new(
            NotAvailableReason::PrimitiveMissing,
            "profile architecture does not match the container host",
        )));
    }
    if (mutation_overlay && profile.source_write() != SourceWriteMode::MutationOverlay)
        || (!mutation_overlay && profile.source_write() != SourceWriteMode::ReadOnly)
    {
        return Err(ContainerError::NotAvailable(NotAvailable::new(
            NotAvailableReason::PrimitiveMissing,
            "container backend does not provide source mutation overlays",
        )));
    }
    if profile.repository() != RepositoryExecutionPolicy::DISABLED {
        return Err(ContainerError::NotAvailable(NotAvailable::new(
            NotAvailableReason::PrimitiveMissing,
            "container backend does not sandbox repository hooks or submodules",
        )));
    }
    if profile
        .egress()
        .iter()
        .any(|grant| grant.transport() != EgressTransport::Tcp)
    {
        return Err(ContainerError::NotAvailable(NotAvailable::new(
            NotAvailableReason::EgressUnavailable,
            "the verified HTTP proxy can authorize only TCP grants",
        )));
    }
    if let Some(primitive) = profile
        .requirements()
        .iter()
        .find(|primitive| !supported_primitive(**primitive, mutation_overlay))
    {
        return Err(ContainerError::NotAvailable(NotAvailable::new(
            NotAvailableReason::PrimitiveMissing,
            format!("container backend does not enforce required primitive {primitive:?}"),
        )));
    }
    Ok(())
}

fn supported_primitive(primitive: BackendPrimitive, mutation_overlay: bool) -> bool {
    use BackendPrimitive as P;

    matches!(
        primitive,
        P::FilesystemBoundary
            | P::ProcessBoundary
            | P::PrivilegeBoundary
            | P::UserNamespace
            | P::RootlessBoundary
            | P::ContainerOrVmBoundary
            | P::SyscallPolicy
            | P::ScrubbedEnvironment
            | P::WholeProcessTreeControl
            | P::ReadOnlyMount
            | P::WritableMount
            | P::ReadOnlySource
            | P::CredentialFileDescriptor
            | P::CredentialMemoryFile
            | P::CredentialScopedEnvironment
            | P::NetworkDeny
            | P::DestinationEgress
            | P::RebindingSafeEgress
            | P::CpuLimit
            | P::MemoryLimit
            | P::PidLimit
            | P::FileSizeLimit
            | P::DiskLimit
            | P::IoLimit
            | P::OutputLimit
            | P::WallTimeLimit
            | P::RepositoryCodeDisabled
    ) || (mutation_overlay && matches!(primitive, P::CopyOnWriteMount | P::SourceMutationOverlay))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BoundaryState {
    pub boundary_absent: bool,
    pub survivors: u32,
}

pub trait BoundaryControl {
    fn kill_boundary(&mut self, deadline: Instant) -> io::Result<bool>;
    fn kill_cli(&mut self, deadline: Instant) -> io::Result<()>;
    fn inspect(&mut self, deadline: Instant) -> io::Result<BoundaryState>;
}

pub fn terminate_after_violation(
    control: &mut impl BoundaryControl,
    pending: BoundError,
) -> Result<BoundError, ExecutionError> {
    let killed = control
        .kill_boundary(Instant::now() + CONTROL_TIMEOUT)
        .unwrap_or(false);
    let cli_result = control.kill_cli(Instant::now() + CONTROL_TIMEOUT);
    let state = control.inspect(Instant::now() + CONTROL_TIMEOUT);
    if !killed {
        return match state {
            Ok(state) if state.boundary_absent && state.survivors == 0 => {
                Err(ExecutionError::OutcomeUnknown {
                    detail: "whole-boundary kill could not be proven even though inspection is now clear"
                        .to_owned(),
                })
            }
            Ok(state) => Err(ExecutionError::NotQuiescent {
                survivors: state.survivors,
                detail: "whole-boundary kill failed".to_owned(),
            }),
            Err(error) => Err(ExecutionError::NotQuiescent {
                survivors: u32::MAX,
                detail: format!("whole-boundary kill and post-kill inspection failed: {error}"),
            }),
        };
    }
    cli_result.map_err(|error| ExecutionError::OutcomeUnknown {
        detail: format!("boundary was killed but the runtime CLI could not be reaped: {error}"),
    })?;
    let state = state.map_err(|error| ExecutionError::NotQuiescent {
        survivors: u32::MAX,
        detail: format!("post-kill inspection failed: {error}"),
    })?;
    if !state.boundary_absent || state.survivors != 0 {
        return Err(ExecutionError::NotQuiescent {
            survivors: state.survivors,
            detail: "post-kill inspection did not prove an absent boundary".to_owned(),
        });
    }
    Ok(pending)
}

struct ContainerBoundaryControl {
    identity: BoundaryIdentity,
    boundary_record: PathBuf,
    attestations: Arc<Mutex<Vec<String>>>,
}

impl ContainerBoundaryControl {
    fn new(plan: &ContainerPlan, invocation_digest: &str) -> Result<Self, ExecutionError> {
        let identity = BoundaryIdentity::new(
            BoundaryKind::Container,
            plan.boundary_name.clone(),
            plan.ownership_id.clone(),
            format!(
                "{}|{}|{}|{invocation_digest}",
                plan.plan_digest,
                plan.evidence.runtime_identity(),
                plan.evidence.helper_identity()
            ),
        )
        .map_err(|error| ExecutionError::InvocationMismatch {
            detail: error.to_string(),
        })?;
        Ok(Self {
            identity,
            boundary_record: plan.boundary_record.clone(),
            attestations: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn helper(&self, operation: &str, deadline: Instant) -> io::Result<BoundedOutput> {
        let arguments = ControlIdentity::from_boundary(&self.identity)?.arguments(operation);
        bounded_output(Path::new(HELPER_PATH), arguments, deadline, 4096)
    }

    fn evidence(&self, operation: &str, deadline: Instant) -> io::Result<limits::ControlEvidence> {
        let output = self.helper(operation, deadline)?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "trusted helper rejected boundary {operation}"
            )));
        }
        let transcript = String::from_utf8(output.stdout)
            .map_err(|_| io::Error::other("trusted helper returned non-UTF-8 control evidence"))?;
        let evidence =
            ControlIdentity::from_boundary(&self.identity)?.parse_evidence(&transcript)?;
        self.attestations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(format!(
                "{operation}:{}",
                blake3::hash(transcript.as_bytes()).to_hex()
            ));
        Ok(evidence)
    }
}

fn control_attestation_digest(attestations: &Arc<Mutex<Vec<String>>>) -> Option<String> {
    let attestations = attestations
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    (!attestations.is_empty()).then(|| {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"kit-helper-control-attestations-v1");
        for attestation in attestations.iter() {
            hasher.update(&(attestation.len() as u64).to_le_bytes());
            hasher.update(attestation.as_bytes());
        }
        format!("blake3:{}", hasher.finalize().to_hex())
    })
}

impl TreeBoundaryControl for ContainerBoundaryControl {
    fn identity(&self) -> &BoundaryIdentity {
        &self.identity
    }

    fn containment(&self) -> Containment {
        Containment::Complete
    }

    fn release(&mut self, _deadline: Instant) -> io::Result<()> {
        let persisted = fs::read_to_string(&self.boundary_record)?;
        let persisted = PersistedBoundary::decode(&persisted)
            .map_err(|error| io::Error::other(error.to_string()))?;
        if persisted.identity != self.identity {
            return Err(io::Error::other("durable boundary identity mismatch"));
        }
        Ok(())
    }

    fn kill_boundary(&mut self, deadline: Instant) -> io::Result<()> {
        self.evidence("kill", deadline).map(drop)
    }

    fn wait_and_reap(&mut self, deadline: Instant) -> io::Result<()> {
        let evidence = self.evidence("reap", deadline)?;
        if evidence.direct_child_reaped {
            Ok(())
        } else {
            Err(io::Error::other(
                "trusted helper did not prove execution supervisor reaped or adopted and gone",
            ))
        }
    }

    fn inspect(&mut self, deadline: Instant) -> io::Result<Inspection> {
        let evidence = self.evidence("inspect", deadline)?;
        if evidence.boundary_absent && evidence.survivors == 0 {
            remove_boundary_record(&self.boundary_record)?;
        }
        Ok(Inspection {
            identity: self.identity.clone(),
            survivors: Some(evidence.survivors),
            quiescent: evidence.boundary_absent && evidence.survivors == 0,
        })
    }
}

#[derive(Debug)]
struct ExecutionRecord {
    nonce: String,
    ownership_id: String,
    plan_digest: String,
    runtime_identity: String,
    helper_identity: String,
    resolved_image_digest: String,
    boundary_id: String,
    instance_id: String,
    rootfs_lease_id: String,
    writable_lease_id: String,
    invocation_digest: String,
    outcome: RecordedOutcome,
    observed: Option<u64>,
    monitor_evidence: String,
    boundary_absent: bool,
    survivors: u32,
    formatter_binary_digest: Option<String>,
    formatter_config_digest: Option<String>,
    formatter_artifact_digest: Option<String>,
}

#[derive(Clone, Copy, Debug)]
enum RecordedOutcome {
    Success,
    Exit(i32),
    Signal(i32),
    Bound(ResourceIdentity),
}

impl ExecutionRecord {
    fn parse(value: &str) -> Result<Self, ExecutionError> {
        let mut fields = std::collections::BTreeMap::new();
        for line in value.lines() {
            let (name, value) = line.split_once('=').ok_or_else(|| {
                ExecutionError::MonitorProtocol("record line has no separator".to_owned())
            })?;
            if !matches!(
                name,
                "protocol"
                    | "nonce"
                    | "ownership_id"
                    | "plan_digest"
                    | "runtime_identity"
                    | "helper_identity"
                    | "resolved_image_digest"
                    | "boundary_id"
                    | "instance_id"
                    | "rootfs_lease_id"
                    | "writable_lease_id"
                    | "invocation_digest"
                    | "outcome"
                    | "observed"
                    | "monitor_evidence"
                    | "boundary_absent"
                    | "survivors"
                    | "formatter_binary_digest"
                    | "formatter_config_digest"
                    | "formatter_artifact_digest"
            ) || fields.insert(name, value).is_some()
            {
                return Err(ExecutionError::MonitorProtocol(format!(
                    "unknown or duplicate record field {name}"
                )));
            }
        }
        let field = |name| {
            fields.get(name).copied().ok_or_else(|| {
                ExecutionError::MonitorProtocol(format!("missing record field {name}"))
            })
        };
        if field("protocol")? != HELPER_PROTOCOL {
            return Err(ExecutionError::MonitorProtocol(
                "protocol mismatch".to_owned(),
            ));
        }
        let nonce = field("nonce")?;
        let ownership_id = field("ownership_id")?;
        let plan_digest = field("plan_digest")?;
        if !is_lower_hex(nonce, 64)
            || !is_lower_hex(ownership_id, 64)
            || !valid_sha256(plan_digest)
            || !valid_sha256(field("runtime_identity")?)
            || !valid_sha256(field("helper_identity")?)
            || !valid_sha256(field("resolved_image_digest")?)
            || !valid_sha256(field("invocation_digest")?)
            || !safe_name(field("boundary_id")?)
            || !safe_name(field("instance_id")?)
            || !safe_name(field("rootfs_lease_id")?)
            || !safe_name(field("writable_lease_id")?)
        {
            return Err(ExecutionError::MonitorProtocol(
                "invalid invocation identity".to_owned(),
            ));
        }
        let outcome_value = field("outcome")?;
        let outcome = if outcome_value == "success" {
            RecordedOutcome::Success
        } else if let Some(code) = outcome_value.strip_prefix("exit:") {
            let code = code
                .parse()
                .map_err(|_| ExecutionError::MonitorProtocol("invalid exit code".to_owned()))?;
            if code == 0 {
                return Err(ExecutionError::MonitorProtocol(
                    "zero exit must use success outcome".to_owned(),
                ));
            }
            RecordedOutcome::Exit(code)
        } else if let Some(signal) = outcome_value.strip_prefix("signal:") {
            let signal = signal
                .parse()
                .map_err(|_| ExecutionError::MonitorProtocol("invalid signal".to_owned()))?;
            if signal <= 0 {
                return Err(ExecutionError::MonitorProtocol("invalid signal".to_owned()));
            }
            RecordedOutcome::Signal(signal)
        } else if let Some(resource) = outcome_value
            .strip_prefix("bound:")
            .and_then(ResourceIdentity::parse)
        {
            RecordedOutcome::Bound(resource)
        } else {
            return Err(ExecutionError::MonitorProtocol(
                "unknown outcome".to_owned(),
            ));
        };
        let observed = match field("observed")? {
            "none" => None,
            value => Some(value.parse().map_err(|_| {
                ExecutionError::MonitorProtocol("invalid observed value".to_owned())
            })?),
        };
        let boundary_absent = match field("boundary_absent")? {
            "true" => true,
            "false" => false,
            _ => {
                return Err(ExecutionError::MonitorProtocol(
                    "invalid boundary state".to_owned(),
                ));
            }
        };
        let survivors = field("survivors")?
            .parse()
            .map_err(|_| ExecutionError::MonitorProtocol("invalid survivor count".to_owned()))?;
        let formatter_binary_digest = fields.get("formatter_binary_digest").copied();
        let formatter_config_digest = fields.get("formatter_config_digest").copied();
        let formatter_artifact_digest = fields.get("formatter_artifact_digest").copied();
        if [
            formatter_binary_digest,
            formatter_config_digest,
            formatter_artifact_digest,
        ]
        .iter()
        .any(Option::is_some)
            && (!matches!(
                (
                    formatter_binary_digest,
                    formatter_config_digest,
                    formatter_artifact_digest
                ),
                (Some(_), Some(_), Some(_))
            ) || !valid_measurement_digest(formatter_binary_digest.unwrap())
                || !valid_measurement_digest(formatter_config_digest.unwrap())
                || !valid_sha256(formatter_artifact_digest.unwrap()))
        {
            return Err(ExecutionError::MonitorProtocol(
                "invalid formatter measurements".to_owned(),
            ));
        }
        Ok(Self {
            nonce: nonce.to_owned(),
            ownership_id: ownership_id.to_owned(),
            plan_digest: plan_digest.to_owned(),
            runtime_identity: field("runtime_identity")?.to_owned(),
            helper_identity: field("helper_identity")?.to_owned(),
            resolved_image_digest: field("resolved_image_digest")?.to_owned(),
            boundary_id: field("boundary_id")?.to_owned(),
            instance_id: field("instance_id")?.to_owned(),
            rootfs_lease_id: field("rootfs_lease_id")?.to_owned(),
            writable_lease_id: field("writable_lease_id")?.to_owned(),
            invocation_digest: field("invocation_digest")?.to_owned(),
            outcome,
            observed,
            monitor_evidence: field("monitor_evidence")?.to_owned(),
            boundary_absent,
            survivors,
            formatter_binary_digest: formatter_binary_digest.map(str::to_owned),
            formatter_config_digest: formatter_config_digest.map(str::to_owned),
            formatter_artifact_digest: formatter_artifact_digest.map(str::to_owned),
        })
    }
}

pub(crate) struct FormatterOutputRecord;

impl FormatterOutputRecord {
    pub(crate) fn parse(
        value: &str,
        expected_nonce: &str,
        expected_plan: &str,
        expected_invocation: &str,
    ) -> Result<ExecutionChildOutput, ExecutionError> {
        let mut fields = std::collections::BTreeMap::new();
        for line in value.lines() {
            let (name, value) = line.split_once('=').ok_or_else(|| {
                ExecutionError::MonitorProtocol(
                    "formatter output report line has no separator".to_owned(),
                )
            })?;
            if !matches!(
                name,
                "protocol"
                    | "nonce"
                    | "plan_digest"
                    | "invocation_digest"
                    | "stdout_length"
                    | "stdout_digest"
                    | "stdout_hex"
                    | "stderr_length"
                    | "stderr_digest"
                    | "stderr_hex"
                    | "redacted"
                    | "attestation"
            ) || fields.insert(name, value).is_some()
            {
                return Err(ExecutionError::MonitorProtocol(format!(
                    "unknown or duplicate formatter output field {name}"
                )));
            }
        }
        let field = |name| {
            fields.get(name).copied().ok_or_else(|| {
                ExecutionError::MonitorProtocol(format!("missing formatter output field {name}"))
            })
        };
        if field("protocol")? != "kit-check-output-v2"
            || field("nonce")? != expected_nonce
            || field("plan_digest")? != expected_plan
            || field("invocation_digest")? != expected_invocation
            || field("redacted")? != "true"
        {
            return Err(ExecutionError::InvocationMismatch {
                detail: "formatter output report identity mismatch".to_owned(),
            });
        }
        let stdout = parse_execution_stream(
            field("stdout_length")?,
            field("stdout_digest")?,
            field("stdout_hex")?,
        )?;
        let stderr = parse_execution_stream(
            field("stderr_length")?,
            field("stderr_digest")?,
            field("stderr_hex")?,
        )?;
        let attestation = field("attestation")?;
        if attestation
            != formatter_output_attestation(
                expected_nonce,
                expected_plan,
                expected_invocation,
                &stdout,
                &stderr,
            )
        {
            return Err(ExecutionError::InvocationMismatch {
                detail: "formatter output is not bound to this exact invocation".to_owned(),
            });
        }
        Ok(ExecutionChildOutput {
            stdout,
            stderr,
            attestation: attestation.to_owned(),
        })
    }
}

#[cfg(any(test, debug_assertions))]
#[cfg(test)]
pub(crate) fn parse_check_output_for_test(
    stdout: &[u8],
    stderr: &[u8],
) -> Result<ExecutionChildOutput, ExecutionError> {
    let nonce = "1".repeat(64);
    let plan = format!("sha256:{}", "2".repeat(64));
    let invocation = format!("sha256:{}", "3".repeat(64));
    let stdout = ExecutionStreamReport {
        length: stdout.len() as u64,
        digest: format!("blake3:{}", blake3::hash(stdout).to_hex()),
        bytes: stdout.to_vec(),
    };
    let stderr = ExecutionStreamReport {
        length: stderr.len() as u64,
        digest: format!("blake3:{}", blake3::hash(stderr).to_hex()),
        bytes: stderr.to_vec(),
    };
    let attestation = formatter_output_attestation(&nonce, &plan, &invocation, &stdout, &stderr);
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let record = format!(
        "protocol=kit-check-output-v2\nnonce={nonce}\nplan_digest={plan}\ninvocation_digest={invocation}\nstdout_length={}\nstdout_digest={}\nstdout_hex={}\nstderr_length={}\nstderr_digest={}\nstderr_hex={}\nredacted=true\nattestation={attestation}\n",
        stdout.length,
        stdout.digest,
        hex(&stdout.bytes),
        stderr.length,
        stderr.digest,
        hex(&stderr.bytes),
    );
    FormatterOutputRecord::parse(&record, &nonce, &plan, &invocation)
}

fn parse_execution_stream(
    length: &str,
    digest: &str,
    bytes: &str,
) -> Result<ExecutionStreamReport, ExecutionError> {
    let length = length.parse::<u64>().map_err(|_| {
        ExecutionError::MonitorProtocol("invalid formatter output length".to_owned())
    })?;
    if !valid_measurement_digest(digest) {
        return Err(ExecutionError::MonitorProtocol(
            "invalid formatter output digest".to_owned(),
        ));
    }
    let bytes = decode_hex(bytes).ok_or_else(|| {
        ExecutionError::MonitorProtocol("invalid complete helper output".to_owned())
    })?;
    if bytes.len() as u64 != length || !digest_matches_bytes(digest, &bytes) {
        return Err(ExecutionError::MonitorProtocol(
            "helper output length or digest does not match complete bytes".to_owned(),
        ));
    }
    Ok(ExecutionStreamReport {
        length,
        digest: digest.to_owned(),
        bytes,
    })
}

fn formatter_output_attestation(
    nonce: &str,
    plan_digest: &str,
    invocation_digest: &str,
    stdout: &ExecutionStreamReport,
    stderr: &ExecutionStreamReport,
) -> String {
    let mut digest = Sha256::new();
    digest_field(&mut digest, b"domain", b"kit-check-output-v2");
    digest_field(&mut digest, b"nonce", nonce.as_bytes());
    digest_field(&mut digest, b"plan_digest", plan_digest.as_bytes());
    digest_field(
        &mut digest,
        b"invocation_digest",
        invocation_digest.as_bytes(),
    );
    for (name, stream) in [
        (b"stdout".as_slice(), stdout),
        (b"stderr".as_slice(), stderr),
    ] {
        digest_field(&mut digest, name, &stream.length.to_be_bytes());
        digest_field(&mut digest, name, stream.digest.as_bytes());
        digest_field(&mut digest, name, &stream.bytes);
    }
    digest_field(&mut digest, b"redacted", b"true");
    format!("sha256:{:x}", digest.finalize())
}

fn digest_matches_bytes(digest: &str, bytes: &[u8]) -> bool {
    match digest.split_once(':') {
        Some(("blake3", value)) => blake3::hash(bytes).to_hex().as_str() == value,
        Some(("sha256", value)) => format!("{:x}", Sha256::digest(bytes)) == value,
        _ => false,
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) || !is_lower_hex(value, value.len()) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect()
}

fn random_hex() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn persist_boundary(path: &Path, record: &PersistedBoundary) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let result = (|| {
        file.write_all(record.encode().as_bytes())?;
        file.sync_all()?;
        fs::File::open(
            path.parent()
                .ok_or_else(|| io::Error::other("boundary record has no parent"))?,
        )?
        .sync_all()
    })();
    if let Err(error) = result {
        drop(file);
        remove_boundary_record(path).map_err(|cleanup| {
            io::Error::other(format!(
                "boundary persistence failed ({error}); partial record cleanup failed: {cleanup}"
            ))
        })?;
        return Err(error);
    }
    Ok(())
}

fn remove_boundary_record(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    }
    fs::File::open(
        path.parent()
            .ok_or_else(|| io::Error::other("boundary record has no parent"))?,
    )?
    .sync_all()
}

fn internal_boundary_name(display: &str, ownership_id: &str) -> String {
    const PREFIX_LIMIT: usize = 48;
    format!(
        "{}-{ownership_id}",
        &display[..display.len().min(PREFIX_LIMIT)]
    )
}

fn canonical_plan_digest(
    program: &Path,
    profile: &ExecutorProfile,
    arguments: &[OsString],
) -> String {
    let mut digest = Sha256::new();
    digest_field(&mut digest, b"protocol", HELPER_PROTOCOL.as_bytes());
    digest_field(
        &mut digest,
        b"program",
        program.as_os_str().as_encoded_bytes(),
    );
    digest_field(
        &mut digest,
        b"profile_digest",
        profile.digest().to_string().as_bytes(),
    );
    for argument in arguments {
        digest_field(&mut digest, b"argv", argument.as_encoded_bytes());
    }
    format!("sha256:{:x}", digest.finalize())
}

fn digest_field(digest: &mut Sha256, name: &[u8], value: &[u8]) {
    digest.update((name.len() as u64).to_be_bytes());
    digest.update(name);
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn invocation_digest(plan_digest: &str, nonce: &str) -> String {
    let mut digest = Sha256::new();
    digest_field(&mut digest, b"domain", b"kit-container-invocation-v1");
    digest_field(&mut digest, b"plan_digest", plan_digest.as_bytes());
    digest_field(&mut digest, b"nonce", nonce.as_bytes());
    format!("sha256:{:x}", digest.finalize())
}

fn valid_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|value| is_lower_hex(value, 64))
}

fn valid_measurement_digest(value: &str) -> bool {
    value.split_once(':').is_some_and(|(algorithm, value)| {
        matches!(algorithm, "sha256" | "blake3") && is_lower_hex(value, 64)
    })
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn bind_mount(lease: &str, target: &Path, read_only: bool) -> OsString {
    let mut value = OsString::from("--mount=type=bind,src=@kit-lease:");
    value.push(lease);
    value.push(",dst=");
    value.push(target);
    if read_only {
        value.push(",readonly");
    }
    value.push(",bind-propagation=rprivate");
    value
}

fn option_with_path(prefix: &str, value: &Path) -> OsString {
    let mut option = OsString::from(prefix);
    option.push(value);
    option
}

fn safe_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn validate_image(image: &str) -> Result<(), ContainerError> {
    let digest = pinned_image_digest(image);
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ContainerError::InvalidImageDigest);
    }
    Ok(())
}

fn pinned_image_digest(image: &str) -> &str {
    if let Some(digest) = image.strip_prefix("sha256:") {
        digest
    } else if let Some((name, digest)) = image.rsplit_once("@sha256:")
        && !name.is_empty()
        && !name.starts_with('-')
        && !name.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        digest
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::process::tree::Ownership;

    #[test]
    fn formatter_v1_request_transmits_command_and_digest_constraints() {
        let command = vec!["--config=/workspace/rustfmt.toml".to_owned()];
        let mut arguments = Vec::new();
        append_formatter_arguments(
            &mut arguments,
            &format!("sha256:{}", "a".repeat(64)),
            2048,
            FormatterExecutionRequest {
                program: "/usr/bin/rustfmt",
                arguments: &command,
                binary_digest: &format!("sha256:{}", "b".repeat(64)),
                config_digest: &format!("blake3:{}", "c".repeat(64)),
            },
        );
        let arguments = arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            vec![
                "--formatter-program=/usr/bin/rustfmt".to_owned(),
                format!(
                    "--formatter-requested-binary-digest=sha256:{}",
                    "b".repeat(64)
                ),
                format!(
                    "--formatter-requested-config-digest=blake3:{}",
                    "c".repeat(64)
                ),
                format!(
                    "--formatter-requested-artifact-digest=sha256:{}",
                    "a".repeat(64)
                ),
                "--formatter-output-complete-bytes=2048".to_owned(),
                "--formatter-output-channel=stderr".to_owned(),
                "--formatter-output-redact-before-report".to_owned(),
                "--formatter-argument=--config=/workspace/rustfmt.toml".to_owned(),
            ]
        );
    }

    #[test]
    fn create_new_collision_never_removes_the_existing_invocation_record() {
        let path = std::env::temp_dir().join(format!(
            "kit-boundary-collision-{}-{}",
            std::process::id(),
            random_hex().unwrap()
        ));
        fs::write(&path, b"first invocation").unwrap();
        let record = PersistedBoundary {
            ownership: Ownership::new("service", "attempt").unwrap(),
            identity: BoundaryIdentity::new(
                BoundaryKind::Container,
                "boundary",
                "a".repeat(64),
                "start",
            )
            .unwrap(),
        };
        assert_eq!(
            persist_boundary(&path, &record).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&path).unwrap(), b"first invocation");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn formatter_output_report_is_bounded_and_invocation_attested() {
        let nonce = "1".repeat(64);
        let plan = format!("sha256:{}", "2".repeat(64));
        let invocation = format!("sha256:{}", "3".repeat(64));
        let stdout = ExecutionStreamReport {
            length: 6,
            digest: format!("blake3:{}", blake3::hash(b"failed").to_hex()),
            bytes: b"failed".to_vec(),
        };
        let stderr = ExecutionStreamReport {
            length: 5,
            digest: format!("sha256:{:x}", Sha256::digest(b"error")),
            bytes: b"error".to_vec(),
        };
        let attestation =
            formatter_output_attestation(&nonce, &plan, &invocation, &stdout, &stderr);
        let stdout_bytes = stdout
            .bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let stderr_bytes = stderr
            .bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let record = format!(
            "protocol=kit-check-output-v2\nnonce={nonce}\nplan_digest={plan}\ninvocation_digest={invocation}\nstdout_length={}\nstdout_digest={}\nstdout_hex={}\nstderr_length={}\nstderr_digest={}\nstderr_hex={}\nredacted=true\nattestation={attestation}\n",
            stdout.length, stdout.digest, stdout_bytes, stderr.length, stderr.digest, stderr_bytes,
        );
        let parsed = FormatterOutputRecord::parse(&record, &nonce, &plan, &invocation).unwrap();
        assert_eq!(parsed.stdout.bytes, b"failed");
        assert_eq!(parsed.stderr.bytes, b"error");
        assert!(matches!(
            FormatterOutputRecord::parse(
                &record.replace("redacted=true", "redacted=false"),
                &nonce,
                &plan,
                &invocation,
            ),
            Err(ExecutionError::InvocationMismatch { .. })
        ));
    }
}
