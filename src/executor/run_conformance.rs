//! Deterministic conformance runner for the `kit_run` test path.
//!
//! Production `kit_run` executes through the sealed container backend
//! (`container::prepare_captured`). Tests substitute this runner to exercise
//! the run dispatch path without a container runtime: it validates the
//! request against the immutable source tree, replays scripted completions,
//! and persists the same artifact/evidence shape the production path emits.

use std::{
    collections::VecDeque,
    fmt, fs,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};

use crate::{
    domain::secret::SecretLease,
    executor::{backends::container, profile::ResourceLimits},
    store::artifacts::{ArtifactClass, ArtifactMetadata, ArtifactRetention, ArtifactStore},
    telemetry::redact::{CaptureBoundary, CaptureRedactor},
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunCommand {
    id: String,
    program: String,
    arguments: Vec<String>,
    image: String,
    tool_digest: String,
    config_digest: String,
    resources: ResourceLimits,
}

impl RunCommand {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        program: impl Into<String>,
        arguments: Vec<String>,
        image: impl Into<String>,
        tool_digest: impl Into<String>,
        config_digest: impl Into<String>,
        resources: ResourceLimits,
    ) -> Result<Self, RunConformanceError> {
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

    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("run command serialization cannot fail")
    }

    fn validate(&self) -> Result<(), RunConformanceError> {
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
            return Err(RunConformanceError::Rejected);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunStatus {
    Pass,
    Exit(i32),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RunExecutionRoute {
    ConformanceFake,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunArtifactRef {
    reference: String,
}

impl RunArtifactRef {
    pub fn reference(&self) -> &str {
        &self.reference
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunCancellationEvidence {
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
pub struct RunProcessEvidence {
    pub route: RunExecutionRoute,
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
    pub cancellation: Option<RunCancellationEvidence>,
    pub kill_attempted: bool,
    pub reaped: bool,
    pub inspected: bool,
    pub survivors: u32,
    pub boundary_absent: bool,
    pub quiescent: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunCompletion {
    pub status: RunStatus,
    pub stdout_artifact: RunArtifactRef,
    pub stderr_artifact: RunArtifactRef,
    pub process: RunProcessEvidence,
    pub process_artifact: RunArtifactRef,
}

pub struct RunExecutionRequest<'a> {
    pub command: &'a RunCommand,
    pub immutable_source: &'a Path,
    pub source_digest: &'a str,
    pub artifacts: &'a ArtifactStore,
    pub principal: &'a str,
    pub project: &'a str,
    pub retention: ArtifactRetention,
    pub stored_at_unix_micros: i64,
    pub secrets: &'a [SecretLease],
}

pub struct RunConformanceRunner {
    completions: VecDeque<ConformanceRun>,
}

impl RunConformanceRunner {
    pub fn conformance(completions: impl IntoIterator<Item = ConformanceRun>) -> Self {
        Self {
            completions: completions.into_iter().collect(),
        }
    }

    pub(crate) fn execute(
        &mut self,
        request: RunExecutionRequest<'_>,
    ) -> Result<RunCompletion, RunConformanceError> {
        request.command.validate()?;
        let source_before = tree_seal(request.immutable_source)?;
        if !valid_digest(request.source_digest) || source_before.digest != request.source_digest {
            return Err(RunConformanceError::StaleTree);
        }
        let completion = self
            .completions
            .pop_front()
            .ok_or(RunConformanceError::Unavailable)?;
        let raw = match completion.complete(request.command) {
            Ok(raw) => raw,
            Err(RunConformanceError::Cancelled { process }) => {
                let _ = persist_process(&request, &process);
                return Err(RunConformanceError::Cancelled { process });
            }
            Err(error) => return Err(error),
        };
        if tree_seal(request.immutable_source)? != source_before {
            return Err(RunConformanceError::StaleTree);
        }
        if !raw.process.quiescent {
            return Err(RunConformanceError::NotQuiescent);
        }
        if raw.stdout_length != raw.stdout.len() as u64
            || raw.stderr_length != raw.stderr.len() as u64
            || raw.stdout_digest != digest_bytes(&raw.stdout)
            || raw.stderr_digest != digest_bytes(&raw.stderr)
        {
            return Err(RunConformanceError::Protocol);
        }
        let redactor = CaptureRedactor::new(request.secrets);
        let stdout = sanitize(&redactor, raw.stdout)?;
        let stderr = sanitize(&redactor, raw.stderr)?;
        let stdout_artifact = persist_stream(&request, &stdout)?;
        let stderr_artifact = persist_stream(&request, &stderr)?;
        let process = raw.process;
        if !valid_process_evidence(&process) {
            return Err(RunConformanceError::NotQuiescent);
        }
        let process_artifact = persist_process(&request, &process)?;
        Ok(RunCompletion {
            status: raw.status,
            stdout_artifact,
            stderr_artifact,
            process,
            process_artifact,
        })
    }
}

struct RawCompletion {
    status: RunStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_length: u64,
    stdout_digest: String,
    stderr_length: u64,
    stderr_digest: String,
    process: RunProcessEvidence,
}

#[derive(Clone, Debug)]
pub enum ConformanceRun {
    Complete {
        status: RunStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Unavailable,
    CancelWhenSignalled {
        entered: std::sync::mpsc::SyncSender<()>,
        cancellation: Arc<AtomicBool>,
    },
}

impl ConformanceRun {
    pub fn pass(stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> Self {
        Self::Complete {
            status: RunStatus::Pass,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    pub fn exit(code: i32, stdout: impl Into<Vec<u8>>, stderr: impl Into<Vec<u8>>) -> Self {
        Self::Complete {
            status: RunStatus::Exit(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    fn complete(self, command: &RunCommand) -> Result<RawCompletion, RunConformanceError> {
        let (status, stdout, stderr) = match self {
            Self::Complete {
                status,
                stdout,
                stderr,
            } => (status, stdout, stderr),
            Self::Unavailable => return Err(RunConformanceError::Unavailable),
            Self::CancelWhenSignalled {
                entered,
                cancellation,
            } => {
                entered
                    .send(())
                    .map_err(|_| RunConformanceError::Unavailable)?;
                while !cancellation.load(Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                return Err(RunConformanceError::Cancelled {
                    process: Box::new(cancelled_process(command)),
                });
            }
        };
        let output = container::parse_check_output_for_test(&stdout, &stderr)
            .map_err(|_| RunConformanceError::Protocol)?;
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

fn cancelled_process(command: &RunCommand) -> RunProcessEvidence {
    let mut process = conformance_process(command, true);
    process.kill_attempted = true;
    let record = conformance_cancellation(&process);
    process.cancellation_record = Some(record.commit_digest.clone());
    process.cancellation = Some(record);
    process
}

fn conformance_process(command: &RunCommand, quiescent: bool) -> RunProcessEvidence {
    let binding = digest_bytes(&command.canonical_bytes());
    RunProcessEvidence {
        route: RunExecutionRoute::ConformanceFake,
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

fn conformance_cancellation(process: &RunProcessEvidence) -> RunCancellationEvidence {
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
    RunCancellationEvidence {
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

#[derive(Clone, Debug)]
pub enum RunConformanceError {
    Unavailable,
    Rejected,
    StaleTree,
    Cancelled { process: Box<RunProcessEvidence> },
    NotQuiescent,
    Protocol,
    Io(String),
}

impl fmt::Display for RunConformanceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "conformance run executor is unavailable",
            Self::Rejected => "conformance run executor rejected the request",
            Self::StaleTree => "immutable run tree binding is stale",
            Self::Cancelled { .. } => "run was durably cancelled",
            Self::NotQuiescent => "run boundary did not prove quiescence",
            Self::Protocol => "run helper evidence did not match the request",
            Self::Io(_) => "run tree could not be inspected",
        })
    }
}

impl std::error::Error for RunConformanceError {}

fn sanitize(
    redactor: &CaptureRedactor<'_>,
    bytes: Vec<u8>,
) -> Result<Vec<u8>, RunConformanceError> {
    let capture = redactor.sanitize(CaptureBoundary::Artifact, &bytes);
    capture
        .bytes()
        .map(Vec::from)
        .map_err(|_| RunConformanceError::Protocol)
}

fn artifact_metadata(
    request: &RunExecutionRequest<'_>,
    media_type: &str,
    class: ArtifactClass,
) -> Result<ArtifactMetadata, RunConformanceError> {
    ArtifactMetadata::new(
        media_type,
        class,
        request.principal,
        request.project,
        request.retention,
        request.stored_at_unix_micros,
    )
    .map_err(|error| RunConformanceError::Io(error.to_string()))
}

fn persist_stream(
    request: &RunExecutionRequest<'_>,
    bytes: &[u8],
) -> Result<RunArtifactRef, RunConformanceError> {
    let artifact = request
        .artifacts
        .put(
            bytes,
            artifact_metadata(request, "application/octet-stream", ArtifactClass::Log)?,
        )
        .map_err(|error| RunConformanceError::Io(error.to_string()))?;
    let verified = request
        .artifacts
        .open_reference(artifact.reference())
        .map_err(|error| RunConformanceError::Io(error.to_string()))?;
    if verified.manifest().size != bytes.len() as u64
        || verified.digest().to_string() != digest_bytes(bytes)
    {
        return Err(RunConformanceError::Protocol);
    }
    Ok(RunArtifactRef {
        reference: verified.reference().to_string(),
    })
}

fn persist_process(
    request: &RunExecutionRequest<'_>,
    process: &RunProcessEvidence,
) -> Result<RunArtifactRef, RunConformanceError> {
    let bytes = serde_json::to_vec(process).map_err(|_| RunConformanceError::Protocol)?;
    let artifact = request
        .artifacts
        .put(
            &bytes,
            artifact_metadata(request, "application/json", ArtifactClass::Report)?,
        )
        .map_err(|error| RunConformanceError::Io(error.to_string()))?;
    Ok(RunArtifactRef {
        reference: artifact.reference().to_string(),
    })
}

fn valid_process_evidence(process: &RunProcessEvidence) -> bool {
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
        && process.survivors == 0
        && process.boundary_absent
        && process.reaped
        && process.inspected
        && process.quiescent
}

pub fn immutable_tree_digest(root: &Path) -> Result<String, RunConformanceError> {
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

fn tree_seal(root: &Path) -> Result<TreeSeal, RunConformanceError> {
    let root = fs::canonicalize(root).map_err(io_error)?;
    if !root.is_dir() {
        return Err(RunConformanceError::Rejected);
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
                .map_err(|error| RunConformanceError::Io(error.to_string()))?
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
                return Err(RunConformanceError::Rejected);
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

fn io_error(error: std::io::Error) -> RunConformanceError {
    RunConformanceError::Io(error.to_string())
}
