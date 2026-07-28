use std::{
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
};

#[cfg(any(test, debug_assertions))]
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    domain::{
        lifecycle::{AttemptOwnership, ProcessOwnership},
        secret::SecretLease,
    },
    executor::{
        backends::container::{self, ExecutionOutcome},
        cancel::{SqliteCancellationCoordinator, WorkspaceIdentity},
        process::own::ProcessRegistryRegistration,
        profile::{
            Architecture, ExecutorProfile, Platform, ProfileSpec, RepositoryExecutionPolicy,
            ResourceLimits, TrustTier,
        },
    },
    store::artifacts::{ArtifactClass, ArtifactMetadata, ArtifactRetention, ArtifactStore},
    telemetry::redact::{CaptureBoundary, CaptureRedactor},
};

use crate::executor::backends::container::limits::ProbeRecord;

pub const CHECK_EXECUTOR_CONTRACT_VERSION: u16 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CheckCommand {
    id: String,
    program: String,
    arguments: Vec<String>,
    image: String,
    tool_digest: String,
    config_digest: String,
    resources: ResourceLimits,
}

impl CheckCommand {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        program: impl Into<String>,
        arguments: Vec<String>,
        image: impl Into<String>,
        tool_digest: impl Into<String>,
        config_digest: impl Into<String>,
        resources: ResourceLimits,
    ) -> Result<Self, CheckExecutorError> {
        let command = Self {
            id: id.into(),
            program: program.into(),
            arguments,
            image: image.into(),
            tool_digest: tool_digest.into(),
            config_digest: config_digest.into(),
            resources,
        };
        command.validate()?;
        Ok(command)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    pub fn tool_digest(&self) -> &str {
        &self.tool_digest
    }

    pub fn config_digest(&self) -> &str {
        &self.config_digest
    }

    pub const fn resources(&self) -> ResourceLimits {
        self.resources
    }

    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("check command serialization cannot fail")
    }

    fn validate(&self) -> Result<(), CheckExecutorError> {
        if !safe_id(&self.id)
            || self.program.is_empty()
            || self.program.as_bytes().contains(&0)
            || self
                .arguments
                .iter()
                .any(|argument| argument.as_bytes().contains(&0))
            || !valid_image(&self.image)
            || !valid_digest(&self.tool_digest)
            || !valid_digest(&self.config_digest)
            || !self.resources.finite()
        {
            return Err(CheckExecutorError::Rejected);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckStatus {
    Pass,
    Exit(i32),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CheckExecutionRoute {
    SealedContainerHelper,
    #[cfg(any(test, debug_assertions))]
    ConformanceFake,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckArtifactRef {
    reference: String,
    digest: String,
    length: u64,
}

impl CheckArtifactRef {
    pub fn reference(&self) -> &str {
        &self.reference
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub const fn length(&self) -> u64 {
        self.length
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckCancellationEvidence {
    pub invocation_digest: String,
    pub process_id: String,
    pub boundary_id: String,
    pub ownership: String,
    pub source: String,
    pub request_id: Option<String>,
    pub fence: Option<u64>,
    pub phase: String,
    pub commit_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckProcessEvidence {
    pub route: CheckExecutionRoute,
    pub boundary_id: String,
    pub plan_digest: String,
    pub invocation_digest: String,
    pub runtime_identity: String,
    pub helper_identity: String,
    pub image_digest: String,
    pub tool_digest: String,
    pub config_digest: String,
    pub durable_record: String,
    pub cancellation_record: Option<String>,
    pub cancellation: Option<CheckCancellationEvidence>,
    pub kill_attempted: bool,
    pub reaped: bool,
    pub inspected: bool,
    pub survivors: u32,
    pub boundary_absent: bool,
    pub quiescent: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckCompletion {
    pub contract_version: u16,
    pub check_id: String,
    pub status: CheckStatus,
    pub stdout_artifact: CheckArtifactRef,
    pub stderr_artifact: CheckArtifactRef,
    pub stdout_preview: Vec<u8>,
    pub stderr_preview: Vec<u8>,
    pub process: CheckProcessEvidence,
    pub process_artifact: CheckArtifactRef,
}

pub struct CheckExecutionRequest<'a> {
    pub command: &'a CheckCommand,
    pub immutable_source: &'a Path,
    pub source_digest: &'a str,
    pub build: &'a Path,
    pub temp: &'a Path,
    pub max_preview_bytes: usize,
    pub artifacts: &'a ArtifactStore,
    pub principal: &'a str,
    pub project: &'a str,
    pub retention: ArtifactRetention,
    pub stored_at_unix_micros: i64,
    pub secrets: &'a [SecretLease],
    pub more_boundaries: bool,
}

enum Backend {
    Container {
        owner: ProcessOwnership,
        registry: Box<ProcessRegistryRegistration>,
        cancellation: Option<Box<(SqliteCancellationCoordinator, WorkspaceIdentity)>>,
    },
    #[cfg(any(test, debug_assertions))]
    Conformance(VecDeque<ConformanceCheck>),
}

pub struct CheckRunner {
    backend: Backend,
}

impl CheckRunner {
    pub(crate) fn bind_attempt(&mut self, attempt: AttemptOwnership) {
        if let Backend::Container { owner, .. } = &mut self.backend {
            *owner = ProcessOwnership::Attempt(attempt);
        }
    }

    pub fn registered_container(
        owner: ProcessOwnership,
        registry: ProcessRegistryRegistration,
    ) -> Self {
        Self {
            backend: Backend::Container {
                owner,
                registry: Box::new(registry),
                cancellation: None,
            },
        }
    }

    pub fn registered_attempt_container(
        owner: AttemptOwnership,
        coordinator: SqliteCancellationCoordinator,
        workspace: WorkspaceIdentity,
        registry: ProcessRegistryRegistration,
    ) -> Self {
        Self {
            backend: Backend::Container {
                owner: ProcessOwnership::Attempt(owner),
                registry: Box::new(registry),
                cancellation: Some(Box::new((coordinator, workspace))),
            },
        }
    }

    #[cfg(any(test, debug_assertions))]
    pub fn conformance(completions: impl IntoIterator<Item = ConformanceCheck>) -> Self {
        Self {
            backend: Backend::Conformance(completions.into_iter().collect()),
        }
    }

    pub(crate) fn execute(
        &mut self,
        request: CheckExecutionRequest<'_>,
    ) -> Result<CheckCompletion, CheckExecutionFailure> {
        let invocation = invocation_identity(request.command);
        let mut execute = || -> Result<RawCompletion, CheckExecutionFailure> {
            request.command.validate().map_err(not_started)?;
            let source_before = tree_seal(request.immutable_source).map_err(not_started)?;
            if request.max_preview_bytes == 0
                || !valid_digest(request.source_digest)
                || source_before.digest != request.source_digest
            {
                return Err(not_started(CheckExecutorError::StaleTree));
            }
            let command = request.command;
            let raw = match &mut self.backend {
                Backend::Container {
                    owner,
                    registry,
                    cancellation,
                } => {
                    let profile = check_profile(command.resources()).map_err(not_started)?;
                    let argv = std::iter::once(command.program.clone())
                        .chain(command.arguments.iter().cloned())
                        .collect::<Vec<_>>();
                    let plan = container::prepare_check(
                        &profile,
                        request.immutable_source,
                        request.build,
                        request.temp,
                        &format!("check-{}", command.id),
                        &command.image,
                        argv,
                        container::CheckExecutionRequest {
                            program: &command.program,
                            arguments: &command.arguments,
                            binary_digest: &command.tool_digest,
                            config_digest: &command.config_digest,
                        },
                    )
                    .map_err(|_| not_started(CheckExecutorError::Unavailable))?;
                    let report = if let Some(cancellation) = cancellation {
                        let (coordinator, workspace) = cancellation.as_ref();
                        plan.run_registered(
                            *owner,
                            coordinator,
                            workspace.clone(),
                            registry.as_ref().clone(),
                            request.more_boundaries,
                        )
                    } else {
                        plan.run_observed(*owner, registry.as_ref().clone())
                    }
                    .map_err(|error| map_execution_failure(error, command))?;
                    let evidence = process_evidence(command, &report.evidence);
                    let output = report.child_output.ok_or_else(|| {
                        launched_failure(CheckExecutorError::Protocol, Some(evidence.clone()))
                    })?;
                    if !evidence.quiescent
                        || evidence.image_digest != requested_image_digest(&command.image)
                        || report.evidence.formatter_artifact_digest.as_deref()
                            != Some(requested_image_digest(&command.image).as_str())
                        || report.evidence.formatter_binary_digest.as_deref()
                            != Some(&command.tool_digest)
                        || report.evidence.formatter_config_digest.as_deref()
                            != Some(&command.config_digest)
                    {
                        return Err(launched_failure(
                            CheckExecutorError::Protocol,
                            Some(evidence),
                        ));
                    }
                    RawCompletion {
                        status: match report.outcome {
                            ExecutionOutcome::Success => CheckStatus::Pass,
                            ExecutionOutcome::Exit(code) => CheckStatus::Exit(code),
                            ExecutionOutcome::Signal(signal) => CheckStatus::Exit(128 + signal),
                        },
                        stdout: output.stdout.bytes,
                        stderr: output.stderr.bytes,
                        stdout_length: output.stdout.length,
                        stdout_digest: output.stdout.digest,
                        stderr_length: output.stderr.length,
                        stderr_digest: output.stderr.digest,
                        process: evidence,
                    }
                }
                #[cfg(any(test, debug_assertions))]
                Backend::Conformance(completions) => {
                    let completion = completions
                        .pop_front()
                        .ok_or_else(|| not_started(CheckExecutorError::Unavailable))?;
                    completion.complete(command)?
                }
            };
            if tree_seal(request.immutable_source).map_err(not_started)? != source_before {
                return Err(launched_failure(
                    CheckExecutorError::StaleTree,
                    Some(raw.process),
                ));
            }
            if !raw.process.quiescent {
                return Err(launched_failure(
                    CheckExecutorError::NotQuiescent,
                    Some(raw.process),
                ));
            }
            Ok(raw)
        };
        let raw = match execute() {
            Ok(raw) => raw,
            Err(failure) => {
                return Err(persist_failure(&request, failure));
            }
        };
        if raw.stdout_length != raw.stdout.len() as u64
            || raw.stderr_length != raw.stderr.len() as u64
            || raw.stdout_digest != digest_bytes(&raw.stdout)
            || raw.stderr_digest != digest_bytes(&raw.stderr)
        {
            return Err(persist_failure(
                &request,
                launched_failure(CheckExecutorError::Protocol, Some(raw.process)),
            ));
        }
        let redactor = CaptureRedactor::new(request.secrets);
        let stdout = sanitize(&redactor, raw.stdout).map_err(|kind| {
            persist_failure(&request, launched_failure(kind, Some(raw.process.clone())))
        })?;
        let stderr = sanitize(&redactor, raw.stderr).map_err(|kind| {
            persist_failure(&request, launched_failure(kind, Some(raw.process.clone())))
        })?;
        let stdout_artifact = persist_stream(&request, &stdout).map_err(|kind| {
            persist_failure(&request, launched_failure(kind, Some(raw.process.clone())))
        })?;
        let stderr_artifact = persist_stream(&request, &stderr).map_err(|kind| {
            persist_failure(&request, launched_failure(kind, Some(raw.process.clone())))
        })?;
        let mut process = raw.process;
        process.invocation_digest = invocation;
        if !valid_process_evidence(&process) {
            return Err(persist_failure(
                &request,
                launched_failure(CheckExecutorError::NotQuiescent, Some(process)),
            ));
        }
        let process_artifact = persist_process(&request, &process).map_err(|kind| {
            persist_failure(&request, launched_failure(kind, Some(process.clone())))
        })?;
        Ok(CheckCompletion {
            contract_version: CHECK_EXECUTOR_CONTRACT_VERSION,
            check_id: request.command.id.clone(),
            status: raw.status,
            stdout_artifact,
            stderr_artifact,
            stdout_preview: bounded(stdout, request.max_preview_bytes),
            stderr_preview: bounded(stderr, request.max_preview_bytes),
            process,
            process_artifact,
        })
    }
}

struct RawCompletion {
    status: CheckStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_length: u64,
    stdout_digest: String,
    stderr_length: u64,
    stderr_digest: String,
    process: CheckProcessEvidence,
}

#[cfg(any(test, debug_assertions))]
#[derive(Clone, Debug)]
pub enum ConformanceCheck {
    Complete {
        status: CheckStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Unavailable,
    Timeout,
    Cancelled,
    NotQuiescent,
    ProtocolMismatch,
    MissingEvidence,
    CancelWhenSignalled {
        entered: std::sync::mpsc::SyncSender<()>,
        cancellation: Arc<AtomicBool>,
    },
}

#[cfg(any(test, debug_assertions))]
impl ConformanceCheck {
    pub fn pass(stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> Self {
        Self::Complete {
            status: CheckStatus::Pass,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    pub fn exit(code: i32, stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> Self {
        Self::Complete {
            status: CheckStatus::Exit(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    fn complete(self, command: &CheckCommand) -> Result<RawCompletion, CheckExecutionFailure> {
        let failure = |kind, quiescent, cancellation| {
            let mut process = conformance_process(command, quiescent);
            if cancellation {
                process.kill_attempted = true;
                let record = conformance_cancellation(&process);
                process.cancellation_record = Some(record.commit_digest.clone());
                process.cancellation = Some(record);
            }
            launched_failure(kind, Some(process))
        };
        let (status, stdout, stderr) = match self {
            Self::Complete {
                status,
                stdout,
                stderr,
            } => (status, stdout, stderr),
            Self::Unavailable => return Err(not_started(CheckExecutorError::Unavailable)),
            Self::Timeout => return Err(failure(CheckExecutorError::Timeout, true, true)),
            Self::Cancelled => return Err(failure(CheckExecutorError::Cancelled, true, true)),
            Self::NotQuiescent => {
                return Err(failure(CheckExecutorError::NotQuiescent, false, false));
            }
            Self::ProtocolMismatch => {
                return Err(failure(CheckExecutorError::Protocol, true, false));
            }
            Self::MissingEvidence => {
                return Err(launched_failure(CheckExecutorError::Protocol, None));
            }
            Self::CancelWhenSignalled {
                entered,
                cancellation,
            } => {
                entered
                    .send(())
                    .map_err(|_| not_started(CheckExecutorError::Unavailable))?;
                while !cancellation.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                return Err(failure(CheckExecutorError::Cancelled, true, true));
            }
        };
        let output = container::parse_check_output_for_test(&stdout, &stderr)
            .map_err(|_| failure(CheckExecutorError::Protocol, true, false))?;
        let stdout = output.stdout.bytes;
        let stderr = output.stderr.bytes;
        let stdout_digest = digest_bytes(&stdout);
        let stderr_digest = digest_bytes(&stderr);
        Ok(RawCompletion {
            status,
            stdout_length: stdout.len() as u64,
            stderr_length: stderr.len() as u64,
            stdout,
            stderr,
            stdout_digest,
            stderr_digest,
            process: conformance_process(command, true),
        })
    }
}

#[cfg(any(test, debug_assertions))]
fn conformance_process(command: &CheckCommand, quiescent: bool) -> CheckProcessEvidence {
    let binding = digest_bytes(&command.canonical_bytes());
    CheckProcessEvidence {
        route: CheckExecutionRoute::ConformanceFake,
        boundary_id: format!("fake-{}", command.id),
        plan_digest: binding.clone(),
        invocation_digest: digest_bytes(format!("invoke:{binding}").as_bytes()),
        runtime_identity: digest_bytes(b"conformance-runtime"),
        helper_identity: digest_bytes(b"conformance-helper-v1"),
        image_digest: requested_image_digest(&command.image),
        tool_digest: command.tool_digest.clone(),
        config_digest: command.config_digest.clone(),
        durable_record: digest_bytes(format!("durable:{binding}").as_bytes()),
        cancellation_record: None,
        cancellation: None,
        kill_attempted: !quiescent,
        reaped: quiescent,
        inspected: true,
        survivors: if quiescent { 0 } else { 1 },
        boundary_absent: quiescent,
        quiescent,
    }
}

#[cfg(any(test, debug_assertions))]
fn conformance_cancellation(process: &CheckProcessEvidence) -> CheckCancellationEvidence {
    let invocation_digest = process.invocation_digest.clone();
    let process_id = format!("fake-process-{}", process.boundary_id);
    let boundary_id = process.boundary_id.clone();
    let ownership = "conformance-daemon".to_owned();
    let source = "helper_control_attestation".to_owned();
    let phase = "quiescent".to_owned();
    let commit_digest = digest_bytes(
        format!("{invocation_digest}\0{process_id}\0{boundary_id}\0{ownership}\0{source}\0{phase}")
            .as_bytes(),
    );
    CheckCancellationEvidence {
        invocation_digest,
        process_id,
        boundary_id,
        ownership,
        source,
        request_id: None,
        fence: None,
        phase,
        commit_digest,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckExecutorError {
    Unavailable,
    Rejected,
    StaleTree,
    Timeout,
    Cancelled,
    NotQuiescent,
    OutputLimit,
    Protocol,
    Io(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CheckExecutionFailure {
    pub(crate) state: CheckFailureState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CheckFailureState {
    NotStarted(CheckExecutorError),
    LaunchedFailure {
        kind: CheckExecutorError,
        process: Option<Box<CheckProcessEvidence>>,
        process_artifact: Option<CheckArtifactRef>,
    },
}

impl fmt::Display for CheckExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "sealed check executor is unavailable",
            Self::Rejected => "sealed check executor rejected the request",
            Self::StaleTree => "immutable check tree binding is stale",
            Self::Timeout => "check exceeded its wall-time bound",
            Self::Cancelled => "check was durably cancelled",
            Self::NotQuiescent => "check boundary did not prove quiescence",
            Self::OutputLimit => "check exceeded its output bound",
            Self::Protocol => "check helper evidence did not match the request",
            Self::Io(_) => "check tree could not be inspected",
        })
    }
}

impl std::error::Error for CheckExecutorError {}

fn invocation_identity(command: &CheckCommand) -> String {
    let mut nonce = [0_u8; 32];
    if getrandom::fill(&mut nonce).is_err() {
        return digest_bytes(&command.canonical_bytes());
    }
    let mut bytes = command.canonical_bytes();
    bytes.extend_from_slice(&nonce);
    digest_bytes(&bytes)
}

fn sanitize(redactor: &CaptureRedactor<'_>, bytes: Vec<u8>) -> Result<Vec<u8>, CheckExecutorError> {
    let capture = redactor.sanitize(CaptureBoundary::Artifact, &bytes);
    capture
        .bytes()
        .map(Vec::from)
        .map_err(|_| CheckExecutorError::Protocol)
}

fn artifact_metadata(
    request: &CheckExecutionRequest<'_>,
    media_type: &str,
    class: ArtifactClass,
) -> Result<ArtifactMetadata, CheckExecutorError> {
    ArtifactMetadata::new(
        media_type,
        class,
        request.principal,
        request.project,
        request.retention,
        request.stored_at_unix_micros,
    )
    .map_err(|error| CheckExecutorError::Io(error.to_string()))
}

fn persist_stream(
    request: &CheckExecutionRequest<'_>,
    bytes: &[u8],
) -> Result<CheckArtifactRef, CheckExecutorError> {
    let artifact = request
        .artifacts
        .put(
            bytes,
            artifact_metadata(request, "application/octet-stream", ArtifactClass::Log)?,
        )
        .map_err(|error| CheckExecutorError::Io(error.to_string()))?;
    let verified = request
        .artifacts
        .open_reference(artifact.reference())
        .map_err(|error| CheckExecutorError::Io(error.to_string()))?;
    if verified.manifest().size != bytes.len() as u64
        || verified.digest().to_string() != digest_bytes(bytes)
    {
        return Err(CheckExecutorError::Protocol);
    }
    Ok(CheckArtifactRef {
        reference: verified.reference().to_string(),
        digest: verified.digest().to_string(),
        length: verified.manifest().size,
    })
}

fn persist_process(
    request: &CheckExecutionRequest<'_>,
    process: &CheckProcessEvidence,
) -> Result<CheckArtifactRef, CheckExecutorError> {
    let bytes = serde_json::to_vec(process).map_err(|_| CheckExecutorError::Protocol)?;
    let artifact = request
        .artifacts
        .put(
            &bytes,
            artifact_metadata(request, "application/json", ArtifactClass::Report)?,
        )
        .map_err(|error| CheckExecutorError::Io(error.to_string()))?;
    Ok(CheckArtifactRef {
        reference: artifact.reference().to_string(),
        digest: artifact.digest().to_string(),
        length: artifact.manifest().size,
    })
}

fn persist_failure(
    request: &CheckExecutionRequest<'_>,
    mut failure: CheckExecutionFailure,
) -> CheckExecutionFailure {
    if let CheckFailureState::LaunchedFailure {
        process,
        process_artifact,
        ..
    } = &mut failure.state
        && let Some(process) = process.as_deref_mut()
        && process_evidence_shape_valid(process)
    {
        *process_artifact = persist_process(request, process).ok();
    }
    failure
}

fn not_started(kind: CheckExecutorError) -> CheckExecutionFailure {
    CheckExecutionFailure {
        state: CheckFailureState::NotStarted(kind),
    }
}

fn launched_failure(
    kind: CheckExecutorError,
    process: Option<CheckProcessEvidence>,
) -> CheckExecutionFailure {
    CheckExecutionFailure {
        state: CheckFailureState::LaunchedFailure {
            kind,
            process: process.map(Box::new),
            process_artifact: None,
        },
    }
}

fn valid_process_evidence(process: &CheckProcessEvidence) -> bool {
    process_evidence_shape_valid(process)
        && process.survivors == 0
        && process.boundary_absent
        && process.reaped
        && process.inspected
        && process.quiescent
}

fn process_evidence_shape_valid(process: &CheckProcessEvidence) -> bool {
    safe_id(&process.boundary_id)
        && [
            &process.plan_digest,
            &process.invocation_digest,
            &process.runtime_identity,
            &process.helper_identity,
            &process.image_digest,
            &process.tool_digest,
            &process.config_digest,
            &process.durable_record,
        ]
        .into_iter()
        .all(|value| valid_digest(value))
        && process
            .cancellation_record
            .as_deref()
            .is_none_or(valid_digest)
        && process.cancellation.as_ref().is_none_or(|cancellation| {
            cancellation.invocation_digest == process.invocation_digest
                && cancellation.boundary_id == process.boundary_id
                && safe_id(&cancellation.process_id)
                && safe_id(&cancellation.ownership)
                && matches!(
                    cancellation.source.as_str(),
                    "coordinator_commit" | "helper_control_attestation"
                )
                && cancellation.phase == "quiescent"
                && valid_digest(&cancellation.commit_digest)
                && process.cancellation_record.as_deref()
                    == Some(cancellation.commit_digest.as_str())
                && match cancellation.source.as_str() {
                    "coordinator_commit" => {
                        cancellation.request_id.as_deref().is_some_and(safe_id)
                            && cancellation.fence.is_some_and(|fence| fence != 0)
                    }
                    _ => cancellation.request_id.is_none() && cancellation.fence.is_none(),
                }
        })
}

pub(crate) fn validate_process_evidence_report(bytes: &[u8]) -> bool {
    serde_json::from_slice::<CheckProcessEvidence>(bytes)
        .is_ok_and(|process| process_evidence_shape_valid(&process))
}

pub fn immutable_tree_digest(root: &Path) -> Result<String, CheckExecutorError> {
    Ok(tree_seal(root)?.digest)
}

#[derive(Eq, PartialEq)]
struct TreeSeal {
    digest: String,
    entries: Vec<EntrySeal>,
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct EntrySeal {
    path: PathBuf,
    directory: bool,
    size: u64,
    mode: u32,
    device: u64,
    inode: u64,
    changed_seconds: i64,
    changed_nanos: i64,
}

fn tree_seal(root: &Path) -> Result<TreeSeal, CheckExecutorError> {
    let root = fs::canonicalize(root).map_err(io_error)?;
    if !root.is_dir() {
        return Err(CheckExecutorError::Rejected);
    }
    let mut pending = vec![root.clone()];
    let mut entries = Vec::new();
    let mut seals = vec![entry_seal(
        PathBuf::new(),
        &fs::metadata(&root).map_err(io_error)?,
    )];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
            let relative = path
                .strip_prefix(&root)
                .map_err(|error| CheckExecutorError::Io(error.to_string()))?
                .to_path_buf();
            if metadata.is_dir() {
                pending.push(path);
                seals.push(entry_seal(relative.clone(), &metadata));
                entries.push((relative, None));
            } else if metadata.is_file() {
                let mut file = fs::File::open(&path).map_err(io_error)?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).map_err(io_error)?;
                seals.push(entry_seal(relative.clone(), &metadata));
                entries.push((relative, Some(bytes)));
            } else {
                return Err(CheckExecutorError::Rejected);
            }
        }
    }
    seals.sort();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = blake3::Hasher::new();
    digest.update(b"kit-immutable-check-tree-v1");
    for (path, bytes) in entries {
        frame(&mut digest, path.as_os_str().as_encoded_bytes());
        match bytes {
            Some(bytes) => {
                digest.update(&[1]);
                frame(&mut digest, &bytes);
            }
            None => {
                digest.update(&[0]);
            }
        }
    }
    Ok(TreeSeal {
        digest: format!("blake3:{}", digest.finalize().to_hex()),
        entries: seals,
    })
}

#[cfg(unix)]
fn entry_seal(path: PathBuf, metadata: &fs::Metadata) -> EntrySeal {
    use std::os::unix::fs::MetadataExt;

    EntrySeal {
        path,
        directory: metadata.is_dir(),
        size: metadata.size(),
        mode: metadata.mode(),
        device: metadata.dev(),
        inode: metadata.ino(),
        changed_seconds: metadata.ctime(),
        changed_nanos: metadata.ctime_nsec(),
    }
}

#[cfg(not(unix))]
fn entry_seal(path: PathBuf, metadata: &fs::Metadata) -> EntrySeal {
    let changed = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
    EntrySeal {
        path,
        directory: metadata.is_dir(),
        size: metadata.len(),
        mode: u32::from(metadata.permissions().readonly()),
        device: 0,
        inode: 0,
        changed_seconds: changed.map_or(0, |value| value.as_secs().min(i64::MAX as u64) as i64),
        changed_nanos: changed.map_or(0, |value| i64::from(value.subsec_nanos())),
    }
}

pub fn production_protocol_preview(
    evidence: &ProbeRecord,
    command: &CheckCommand,
    immutable_source: &Path,
    build: &Path,
    temp: &Path,
) -> Result<container::ContainerPlan, CheckExecutorError> {
    command.validate()?;
    let profile = check_profile(command.resources())?;
    let argv = std::iter::once(command.program.clone())
        .chain(command.arguments.iter().cloned())
        .collect::<Vec<_>>();
    container::preview_check(
        evidence,
        &profile,
        immutable_source,
        build,
        temp,
        &format!("check-{}", command.id),
        &command.image,
        argv,
        container::CheckExecutionRequest {
            program: &command.program,
            arguments: &command.arguments,
            binary_digest: &command.tool_digest,
            config_digest: &command.config_digest,
        },
    )
    .map_err(|error| CheckExecutorError::Io(error.to_string()))
}

fn check_profile(resources: ResourceLimits) -> Result<ExecutorProfile, CheckExecutorError> {
    let architecture = if cfg!(target_arch = "aarch64") {
        Architecture::Aarch64
    } else {
        Architecture::X86_64
    };
    let mut spec = ProfileSpec::isolated(
        TrustTier::Restricted,
        Platform::Linux,
        architecture,
        resources,
    );
    spec.repository = RepositoryExecutionPolicy::DISABLED;
    ExecutorProfile::new(spec).map_err(|_| CheckExecutorError::Rejected)
}

fn map_execution_failure(
    error: container::ExecutionError,
    command: &CheckCommand,
) -> CheckExecutionFailure {
    match error {
        container::ExecutionError::Launched { source, evidence } => launched_failure(
            map_execution_error(*source),
            Some(process_evidence(command, &evidence)),
        ),
        error => not_started(map_execution_error(error)),
    }
}

fn process_evidence(
    command: &CheckCommand,
    evidence: &container::ExecutionEvidence,
) -> CheckProcessEvidence {
    let cancellation = if evidence.kill_attempted
        && evidence.quiescent
        && let (Some(source), Some(commit_digest)) = (
            &evidence.cancellation_source,
            &evidence.cancellation_commit_digest,
        ) {
        let phase = evidence
            .cancellation_phase
            .clone()
            .unwrap_or_else(|| "quiescent".to_owned());
        Some(CheckCancellationEvidence {
            invocation_digest: evidence.invocation_digest.clone(),
            process_id: evidence.process_id.clone(),
            boundary_id: evidence.boundary_id.clone(),
            ownership: evidence.ownership_id.clone(),
            source: source.clone(),
            request_id: evidence.cancellation_request_id.clone(),
            fence: evidence.cancellation_fence,
            phase,
            commit_digest: commit_digest.clone(),
        })
    } else {
        None
    };
    CheckProcessEvidence {
        route: CheckExecutionRoute::SealedContainerHelper,
        durable_record: evidence.plan_digest.clone(),
        cancellation_record: cancellation
            .as_ref()
            .map(|record| record.commit_digest.clone()),
        cancellation,
        boundary_id: evidence.boundary_id.clone(),
        plan_digest: evidence.plan_digest.clone(),
        invocation_digest: evidence.invocation_digest.clone(),
        runtime_identity: evidence.runtime_identity.clone(),
        helper_identity: evidence.helper_identity.clone(),
        image_digest: evidence.resolved_image_digest.clone(),
        tool_digest: command.tool_digest.clone(),
        config_digest: command.config_digest.clone(),
        kill_attempted: evidence.kill_attempted,
        reaped: evidence.reaped,
        inspected: evidence.inspected,
        survivors: evidence.survivors,
        boundary_absent: evidence.boundary_absent,
        quiescent: evidence.quiescent,
    }
}

fn map_execution_error(error: container::ExecutionError) -> CheckExecutorError {
    match error {
        container::ExecutionError::Bound(_) => CheckExecutorError::Timeout,
        container::ExecutionError::NotQuiescent { .. }
        | container::ExecutionError::OutcomeUnknown { .. } => CheckExecutorError::NotQuiescent,
        container::ExecutionError::MonitorProtocol(_)
        | container::ExecutionError::InvocationMismatch { .. } => CheckExecutorError::Protocol,
        container::ExecutionError::Launched { source, .. } => map_execution_error(*source),
        _ => CheckExecutorError::Unavailable,
    }
}

fn bounded(mut value: Vec<u8>, limit: usize) -> Vec<u8> {
    value.truncate(limit);
    value
}

fn requested_image_digest(image: &str) -> String {
    let digest = image
        .strip_prefix("sha256:")
        .or_else(|| image.rsplit_once("@sha256:").map(|(_, digest)| digest))
        .expect("validated image has a digest");
    format!("sha256:{digest}")
}

fn valid_image(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .or_else(|| value.rsplit_once("@sha256:").map(|(_, digest)| digest))
        .is_some_and(lower_hex)
}

fn valid_digest(value: &str) -> bool {
    value.split_once(':').is_some_and(|(algorithm, digest)| {
        matches!(algorithm, "blake3" | "sha256") && lower_hex(digest)
    })
}

fn lower_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn safe_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

fn frame(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn io_error(error: std::io::Error) -> CheckExecutorError {
    CheckExecutorError::Io(error.to_string())
}
