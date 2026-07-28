#[cfg(any(test, debug_assertions))]
use std::path::PathBuf;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    fs::File,
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

use crate::{
    domain::lifecycle::{AttemptOwnership, ProcessOwnership},
    executor::{
        backends::container::{self, ExecutionOutcome},
        cancel::{SqliteCancellationCoordinator, WorkspaceIdentity},
        overlay::ChangeKind,
        process::own::ProcessRegistryRegistration,
        profile::{ExecutionLabel, ExecutorProfile, SourceWriteMode},
    },
    workspace::{
        edit::{format::FormatterDescriptor, ir::RootRelativePath},
        revision::WorkspaceKernelMutationFence,
    },
};
#[cfg(any(test, debug_assertions))]
use sha2::{Digest as _, Sha256};

pub const FORMATTER_EXECUTOR_CONTRACT_VERSION: u16 = 1;
pub const FORMATTER_WRITE_SCOPE_VERSION: u16 = 1;
pub const EXECUTOR_ATTESTED_DIFF_VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatterBudgetCharge {
    Entries,
    Bytes,
    NameBytes,
    PathBytes,
    MetadataMemory,
}

pub(crate) trait FormatterBudget {
    fn charge_entry(
        &mut self,
        name_bytes: usize,
        path_bytes: usize,
        metadata_bytes: usize,
    ) -> Result<(), FormatterBudgetCharge>;
    fn charge_bytes(&mut self, bytes: u64) -> Result<(), FormatterBudgetCharge>;
    fn charge_metadata(&mut self, bytes: usize) -> Result<(), FormatterBudgetCharge>;
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FormatterWriteRule {
    path: RootRelativePath,
    base_digest: String,
    base_mode: u32,
    allowed_kinds: BTreeSet<ChangeKind>,
}

impl FormatterWriteRule {
    pub fn new(
        path: RootRelativePath,
        base_digest: String,
        base_mode: u32,
        allowed_kinds: BTreeSet<ChangeKind>,
    ) -> Result<Self, FormatterExecutorError> {
        if !valid_digest(&base_digest) || base_mode & !0o777 != 0 || allowed_kinds.is_empty() {
            return Err(FormatterExecutorError::Rejected);
        }
        Ok(Self {
            path,
            base_digest,
            base_mode,
            allowed_kinds,
        })
    }

    pub fn path(&self) -> &RootRelativePath {
        &self.path
    }

    pub fn base_digest(&self) -> &str {
        &self.base_digest
    }

    pub const fn base_mode(&self) -> u32 {
        self.base_mode
    }

    pub fn allowed_kinds(&self) -> &BTreeSet<ChangeKind> {
        &self.allowed_kinds
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatterWriteScope {
    version: u16,
    rules: Vec<FormatterWriteRule>,
    digest: String,
}

impl FormatterWriteScope {
    pub fn new(mut rules: Vec<FormatterWriteRule>) -> Result<Self, FormatterExecutorError> {
        rules.sort();
        if rules.is_empty() || rules.windows(2).any(|pair| pair[0].path == pair[1].path) {
            return Err(FormatterExecutorError::Rejected);
        }
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"kit-formatter-write-scope-v1");
        for rule in &rules {
            frame(&mut hasher, rule.path.as_str().as_bytes());
            frame(&mut hasher, rule.base_digest.as_bytes());
            hasher.update(&rule.base_mode.to_le_bytes());
            for kind in &rule.allowed_kinds {
                hasher.update(&[change_kind_byte(*kind)]);
            }
            hasher.update(&[0xff]);
        }
        Ok(Self {
            version: FORMATTER_WRITE_SCOPE_VERSION,
            rules,
            digest: format!("blake3:{}", hasher.finalize().to_hex()),
        })
    }

    pub const fn version(&self) -> u16 {
        self.version
    }

    pub fn rules(&self) -> &[FormatterWriteRule] {
        &self.rules
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    fn validate_base(&self, base: &BTreeMap<String, ArtifactState>) -> bool {
        self.rules.iter().all(|rule| {
            base.get(rule.path.as_str()).is_some_and(|state| {
                state.digest == rule.base_digest && state.mode == rule.base_mode
            })
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatterStatus {
    Success,
    Exit(i32),
    Timeout,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutorAttestedChange {
    path: RootRelativePath,
    kind: ChangeKind,
    base_digest: Option<String>,
    base_mode: Option<u32>,
    result_digest: Option<String>,
    result_mode: Option<u32>,
}

impl ExecutorAttestedChange {
    pub fn path(&self) -> &RootRelativePath {
        &self.path
    }

    pub const fn kind(&self) -> ChangeKind {
        self.kind
    }

    pub fn base_digest(&self) -> Option<&str> {
        self.base_digest.as_deref()
    }

    pub const fn base_mode(&self) -> Option<u32> {
        self.base_mode
    }

    pub fn result_digest(&self) -> Option<&str> {
        self.result_digest.as_deref()
    }

    pub const fn result_mode(&self) -> Option<u32> {
        self.result_mode
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutorAttestedDiff {
    version: u16,
    scope_digest: String,
    base_tree_digest: String,
    result_tree_digest: String,
    digest: String,
    changes: Vec<ExecutorAttestedChange>,
}

impl ExecutorAttestedDiff {
    pub const fn version(&self) -> u16 {
        self.version
    }

    pub fn scope_digest(&self) -> &str {
        &self.scope_digest
    }

    pub fn base_tree_digest(&self) -> &str {
        &self.base_tree_digest
    }

    pub fn result_tree_digest(&self) -> &str {
        &self.result_tree_digest
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn changes(&self) -> &[ExecutorAttestedChange] {
        &self.changes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatterProcessEvidence {
    boundary_id: String,
    invocation_digest: String,
    runtime_identity: String,
    helper_identity: String,
    bounded_capture_digest: String,
    resolved_image_digest: String,
    container_plan_digest: String,
    formatter_binary_digest: String,
    formatter_artifact_digest: String,
    formatter_config_digest: String,
    measurements_authoritative: bool,
    survivors: u32,
    boundary_absent: bool,
}

impl FormatterProcessEvidence {
    pub(crate) fn unavailable() -> Self {
        Self {
            boundary_id: "unavailable".to_owned(),
            invocation_digest: digest_bytes(b"unavailable-invocation"),
            runtime_identity: digest_bytes(b"unavailable-runtime"),
            helper_identity: digest_bytes(b"unavailable-helper"),
            bounded_capture_digest: digest_bytes(b"unavailable-capture"),
            resolved_image_digest: digest_bytes(b"unavailable-image"),
            container_plan_digest: digest_bytes(b"unavailable-plan"),
            formatter_binary_digest: digest_bytes(b"unavailable-binary"),
            formatter_artifact_digest: digest_bytes(b"unavailable-artifact"),
            formatter_config_digest: digest_bytes(b"unavailable-config"),
            measurements_authoritative: false,
            survivors: u32::MAX,
            boundary_absent: false,
        }
    }

    pub fn boundary_id(&self) -> &str {
        &self.boundary_id
    }

    pub fn invocation_digest(&self) -> &str {
        &self.invocation_digest
    }

    pub fn runtime_identity(&self) -> &str {
        &self.runtime_identity
    }

    pub fn helper_identity(&self) -> &str {
        &self.helper_identity
    }

    pub fn bounded_capture_digest(&self) -> &str {
        &self.bounded_capture_digest
    }

    pub fn resolved_image_digest(&self) -> &str {
        &self.resolved_image_digest
    }

    pub fn container_plan_digest(&self) -> &str {
        &self.container_plan_digest
    }

    pub fn formatter_binary_digest(&self) -> &str {
        &self.formatter_binary_digest
    }

    pub fn formatter_artifact_digest(&self) -> &str {
        &self.formatter_artifact_digest
    }

    pub fn formatter_config_digest(&self) -> &str {
        &self.formatter_config_digest
    }

    pub const fn measurements_authoritative(&self) -> bool {
        self.measurements_authoritative
    }

    pub const fn survivors(&self) -> u32 {
        self.survivors
    }

    pub const fn boundary_absent(&self) -> bool {
        self.boundary_absent
    }

    pub const fn quiescent(&self) -> bool {
        self.boundary_absent && self.survivors == 0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatterCompletion {
    status: FormatterStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    elapsed: Duration,
    attested_diff: ExecutorAttestedDiff,
    process: FormatterProcessEvidence,
    stdout_length: u64,
    stdout_digest: String,
    stderr_length: u64,
    stderr_digest: String,
    output_attestation: String,
}

impl FormatterCompletion {
    pub const fn status(&self) -> FormatterStatus {
        self.status
    }

    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    pub const fn stdout_length(&self) -> u64 {
        self.stdout_length
    }

    pub fn stdout_digest(&self) -> &str {
        &self.stdout_digest
    }

    pub const fn stderr_length(&self) -> u64 {
        self.stderr_length
    }

    pub fn stderr_digest(&self) -> &str {
        &self.stderr_digest
    }

    pub fn output_attestation(&self) -> &str {
        &self.output_attestation
    }

    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }

    pub fn overlay_digest(&self) -> &str {
        self.attested_diff.digest()
    }

    pub fn artifacts(&self) -> &[ExecutorAttestedChange] {
        self.attested_diff.changes()
    }

    pub const fn attested_diff(&self) -> &ExecutorAttestedDiff {
        &self.attested_diff
    }

    pub const fn process(&self) -> &FormatterProcessEvidence {
        &self.process
    }
}

#[derive(Debug)]
pub enum FormatterExecutorError {
    Unavailable,
    Rejected,
    Timeout,
    OutputLimit,
    Budget(FormatterBudgetCharge),
    NotQuiescent,
    Undeclared(RootRelativePath),
    UnsafeOverlay,
}

impl fmt::Display for FormatterExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "registered formatter executor is unavailable",
            Self::Rejected => "registered formatter executor rejected the request",
            Self::Timeout => "formatter execution exceeded its deadline",
            Self::OutputLimit => "formatter execution exceeded its output bound",
            Self::Budget(_) => "formatter execution exceeded its staging budget",
            Self::NotQuiescent => "formatter boundary did not prove zero-survivor quiescence",
            Self::Undeclared(_) => "formatter changed a path outside its write scope",
            Self::UnsafeOverlay => "formatter overlay attestation failed",
        })
    }
}

impl std::error::Error for FormatterExecutorError {}

enum Backend {
    Container {
        owner: ProcessOwnership,
        registry: Box<ProcessRegistryRegistration>,
        cancellation: Option<Box<(SqliteCancellationCoordinator, WorkspaceIdentity)>>,
    },
    #[cfg(any(test, debug_assertions))]
    Debug(DebugFormatterAction),
}

/// Sealed formatter execution service. Production construction and completion
/// attestation remain in the executor module; callers cannot inject a runner.
pub struct FormatterExecutor {
    backend: Backend,
}

impl FormatterExecutor {
    pub(crate) fn bind_attempt(&mut self, attempt: AttemptOwnership) {
        if let Backend::Container { owner, .. } = &mut self.backend {
            *owner = ProcessOwnership::Attempt(attempt);
        }
    }

    pub const fn production_available() -> bool {
        cfg!(target_os = "linux")
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
    pub(crate) fn debug(action: DebugFormatterAction) -> Self {
        Self {
            backend: Backend::Debug(action),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute(
        &mut self,
        descriptor: &FormatterDescriptor,
        profile: &ExecutorProfile,
        scope: &FormatterWriteScope,
        source_handle: &File,
        overlay_handle: &File,
        build_handle: &File,
        temp_handle: &File,
        source: &Path,
        overlay: &Path,
        build: &Path,
        temp: &Path,
        max_entries: usize,
        max_file_bytes: usize,
        max_total_bytes: u64,
        max_output_bytes: usize,
        deadline: Instant,
        mutation_fence: &mut WorkspaceKernelMutationFence,
        budget: &mut dyn FormatterBudget,
    ) -> Result<FormatterCompletion, FormatterExecutorError> {
        validate_request(descriptor, profile, scope, deadline)?;
        budget
            .charge_metadata(max_output_bytes)
            .map_err(FormatterExecutorError::Budget)?;
        let source_identity = pinned_directory_identity(source_handle, source)?;
        let overlay_identity = pinned_directory_identity(overlay_handle, overlay)?;
        let build_identity = pinned_directory_identity(build_handle, build)?;
        let temp_identity = pinned_directory_identity(temp_handle, temp)?;
        let source_before = scan_tree(
            source,
            max_entries,
            max_file_bytes,
            max_total_bytes,
            deadline,
            budget,
        )?;
        let overlay_before = scan_tree(
            overlay,
            max_entries,
            max_file_bytes,
            max_total_bytes,
            deadline,
            budget,
        )?;
        if source_before != overlay_before || !scope.validate_base(&source_before) {
            return Err(FormatterExecutorError::Rejected);
        }
        mutation_fence
            .reset_after_verified_read()
            .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
        let started = Instant::now();

        let (status, stdout, stderr, process, output_evidence) = match &mut self.backend {
            Backend::Container {
                owner,
                registry,
                cancellation,
            } => {
                if !Self::production_available() {
                    return Err(FormatterExecutorError::Unavailable);
                }
                let command = descriptor
                    .command()
                    .ok_or(FormatterExecutorError::Unavailable)?;
                let argv = std::iter::once(command.program().to_owned())
                    .chain(command.arguments().iter().cloned())
                    .collect::<Vec<_>>();
                let plan = container::prepare_mutation_overlay(
                    profile,
                    source,
                    overlay,
                    build,
                    temp,
                    &format!("formatter-{}", descriptor.id()),
                    command.image(),
                    argv,
                    container::FormatterExecutionRequest {
                        program: command.program(),
                        arguments: command.arguments(),
                        binary_digest: command.requested_binary_digest(),
                        config_digest: command.requested_config_digest(),
                    },
                )
                .map_err(|_| FormatterExecutorError::Unavailable)?;
                let report = if let Some(cancellation) = cancellation {
                    let (coordinator, workspace) = cancellation.as_ref();
                    plan.run_registered(
                        *owner,
                        coordinator,
                        workspace.clone(),
                        registry.as_ref().clone(),
                        true,
                    )
                } else {
                    plan.run_observed(*owner, registry.as_ref().clone())
                }
                .map_err(|error| match error {
                    container::ExecutionError::Bound(_) => FormatterExecutorError::Timeout,
                    container::ExecutionError::MonitorProtocol(_)
                    | container::ExecutionError::InvocationMismatch { .. } => {
                        FormatterExecutorError::Rejected
                    }
                    container::ExecutionError::NotQuiescent { .. }
                    | container::ExecutionError::OutcomeUnknown { .. } => {
                        FormatterExecutorError::NotQuiescent
                    }
                    _ => FormatterExecutorError::Unavailable,
                })?;
                let status = match report.outcome {
                    ExecutionOutcome::Success => FormatterStatus::Success,
                    ExecutionOutcome::Exit(code) => FormatterStatus::Exit(code),
                    ExecutionOutcome::Signal(signal) => FormatterStatus::Exit(128 + signal),
                };
                let output = report
                    .child_output
                    .ok_or(FormatterExecutorError::Rejected)?;
                let output_length = output
                    .stdout
                    .length
                    .checked_add(output.stderr.length)
                    .ok_or(FormatterExecutorError::OutputLimit)?;
                if output_length > max_output_bytes as u64 {
                    return Err(FormatterExecutorError::OutputLimit);
                }
                let evidence = report.evidence;
                let output_evidence = (
                    output.stdout.length,
                    output.stdout.digest.clone(),
                    output.stderr.length,
                    output.stderr.digest.clone(),
                    output.attestation.clone(),
                );
                let command = descriptor
                    .command()
                    .ok_or(FormatterExecutorError::Unavailable)?;
                if evidence.resolved_image_digest != requested_image_digest(command.image()) {
                    return Err(FormatterExecutorError::Rejected);
                }
                let measurements_authoritative = evidence.formatter_binary_digest.is_some()
                    && evidence.formatter_config_digest.is_some()
                    && evidence.formatter_artifact_digest.is_some();
                (
                    status,
                    output.stdout.bytes,
                    output.stderr.bytes,
                    FormatterProcessEvidence {
                        boundary_id: evidence.boundary_id,
                        invocation_digest: evidence.invocation_digest,
                        runtime_identity: evidence.runtime_identity,
                        helper_identity: evidence.helper_identity,
                        bounded_capture_digest: evidence.bounded_capture_digest,
                        resolved_image_digest: evidence.resolved_image_digest.clone(),
                        container_plan_digest: evidence.plan_digest,
                        formatter_binary_digest: evidence
                            .formatter_binary_digest
                            .unwrap_or_else(|| digest_bytes(b"unavailable-binary-measurement")),
                        formatter_artifact_digest: evidence
                            .formatter_artifact_digest
                            .unwrap_or_else(|| digest_bytes(b"unavailable-artifact-measurement")),
                        formatter_config_digest: evidence
                            .formatter_config_digest
                            .unwrap_or_else(|| digest_bytes(b"unavailable-config-measurement")),
                        measurements_authoritative,
                        survivors: 0,
                        boundary_absent: evidence.quiescent,
                    },
                    output_evidence,
                )
            }
            #[cfg(any(test, debug_assertions))]
            Backend::Debug(action) => {
                let (status, stdout, stderr, process) =
                    run_debug(action, overlay, max_output_bytes, budget)?;
                let output_evidence = (
                    stdout.len() as u64,
                    digest_bytes(&stdout),
                    stderr.len() as u64,
                    digest_bytes(&stderr),
                    bounded_stream_digest(&stdout, &stderr),
                );
                (status, stdout, stderr, process, output_evidence)
            }
        };

        if Instant::now() >= deadline {
            return Err(FormatterExecutorError::Timeout);
        }
        if let Some(command) = descriptor.command() {
            if !process.measurements_authoritative() {
                return Err(FormatterExecutorError::Rejected);
            }
            if process.resolved_image_digest() != requested_image_digest(command.image())
                || process.formatter_artifact_digest() != requested_image_digest(command.image())
                || process.formatter_binary_digest() != command.requested_binary_digest()
                || process.formatter_config_digest() != command.requested_config_digest()
            {
                return Err(FormatterExecutorError::Rejected);
            }
        }
        if !process.quiescent() {
            return Err(FormatterExecutorError::NotQuiescent);
        }
        if directory_identity(source)? != source_identity
            || directory_identity(overlay)? != overlay_identity
            || directory_identity(build)? != build_identity
            || directory_identity(temp)? != temp_identity
        {
            return Err(FormatterExecutorError::UnsafeOverlay);
        }
        let output = stdout
            .len()
            .checked_add(stderr.len())
            .ok_or(FormatterExecutorError::OutputLimit)?;
        if output > max_output_bytes {
            return Err(FormatterExecutorError::OutputLimit);
        }
        let source_after = scan_tree(
            source,
            max_entries,
            max_file_bytes,
            max_total_bytes,
            deadline,
            budget,
        )?;
        if source_after != source_before {
            return Err(FormatterExecutorError::UnsafeOverlay);
        }
        let overlay_after = scan_tree(
            overlay,
            max_entries,
            max_file_bytes,
            max_total_bytes,
            deadline,
            budget,
        )?;
        let attested_diff = attest_overlay(scope, &source_before, &overlay_after)?;
        revoke_writable_tree(overlay)?;
        watch_tree(overlay, overlay_handle, mutation_fence)?;
        let sealed = scan_tree(
            overlay,
            max_entries,
            max_file_bytes,
            max_total_bytes,
            deadline,
            budget,
        )?;
        if sealed.len() != overlay_after.len()
            || sealed.iter().any(|(path, state)| {
                overlay_after.get(path).is_none_or(|before| {
                    state.digest != before.digest || state.mode != sealed_file_mode(before.mode)
                })
            })
        {
            return Err(FormatterExecutorError::UnsafeOverlay);
        }
        mutation_fence
            .ensure_clean()
            .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
        Ok(FormatterCompletion {
            status,
            stdout,
            stderr,
            elapsed: started.elapsed(),
            attested_diff,
            process,
            stdout_length: output_evidence.0,
            stdout_digest: output_evidence.1,
            stderr_length: output_evidence.2,
            stderr_digest: output_evidence.3,
            output_attestation: output_evidence.4,
        })
    }
}

fn validate_request(
    descriptor: &FormatterDescriptor,
    profile: &ExecutorProfile,
    scope: &FormatterWriteScope,
    deadline: Instant,
) -> Result<(), FormatterExecutorError> {
    if Instant::now() >= deadline
        || profile.label() != ExecutionLabel::Restricted
        || profile.source_write() != SourceWriteMode::MutationOverlay
        || scope.version() != FORMATTER_WRITE_SCOPE_VERSION
        || scope.rules().len() != descriptor.files().len()
        || descriptor
            .files()
            .iter()
            .any(|path| !scope.rules().iter().any(|rule| rule.path() == path))
    {
        return Err(FormatterExecutorError::Rejected);
    }
    Ok(())
}

#[cfg(any(test, debug_assertions))]
#[derive(Debug)]
pub(crate) enum DebugFormatterAction {
    Pass,
    Rewrite(String, Vec<u8>),
    Delete(String),
    Chmod(String, u32),
    Symlink(String, String),
    Exit(i32),
    Timeout,
    Output(usize),
    SurvivingProcess,
    ProvenanceMismatch,
    MeasurementAbsent,
    Gate {
        entered: std::sync::mpsc::SyncSender<()>,
        release: std::sync::mpsc::Receiver<()>,
    },
}

#[cfg(any(test, debug_assertions))]
fn run_debug(
    action: &DebugFormatterAction,
    overlay: &Path,
    max_output_bytes: usize,
    budget: &mut dyn FormatterBudget,
) -> Result<(FormatterStatus, Vec<u8>, Vec<u8>, FormatterProcessEvidence), FormatterExecutorError> {
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let mut status = FormatterStatus::Success;
    let mut stdout = b"formatted".to_vec();
    let mut survivors = 0;
    let mut boundary_absent = true;
    match action {
        DebugFormatterAction::Pass => {}
        DebugFormatterAction::Rewrite(path, bytes) => {
            fs::write(debug_path(overlay, path)?, bytes)
                .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
        }
        DebugFormatterAction::Delete(path) => {
            fs::remove_file(debug_path(overlay, path)?)
                .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
        }
        DebugFormatterAction::Chmod(path, mode) => {
            #[cfg(unix)]
            fs::set_permissions(
                debug_path(overlay, path)?,
                fs::Permissions::from_mode(*mode),
            )
            .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            #[cfg(not(unix))]
            return Err(FormatterExecutorError::Unavailable);
        }
        DebugFormatterAction::Symlink(path, target) => {
            #[cfg(unix)]
            {
                let path = debug_path(overlay, path)?;
                fs::remove_file(&path).map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
                symlink(target, path).map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            }
            #[cfg(not(unix))]
            return Err(FormatterExecutorError::Unavailable);
        }
        DebugFormatterAction::Exit(code) => status = FormatterStatus::Exit(*code),
        DebugFormatterAction::Timeout => status = FormatterStatus::Timeout,
        DebugFormatterAction::Output(bytes) => {
            if *bytes > max_output_bytes {
                return Err(FormatterExecutorError::OutputLimit);
            }
            budget
                .charge_bytes(*bytes as u64)
                .map_err(FormatterExecutorError::Budget)?;
            stdout = vec![b'x'; *bytes];
        }
        DebugFormatterAction::SurvivingProcess => {
            survivors = 1;
            boundary_absent = false;
        }
        DebugFormatterAction::ProvenanceMismatch | DebugFormatterAction::MeasurementAbsent => {}
        DebugFormatterAction::Gate { entered, release } => {
            entered
                .send(())
                .map_err(|_| FormatterExecutorError::Unavailable)?;
            release
                .recv()
                .map_err(|_| FormatterExecutorError::Unavailable)?;
        }
    }
    let bounded_capture_digest = bounded_stream_digest(&stdout, &[]);
    Ok((
        status,
        stdout,
        Vec::new(),
        FormatterProcessEvidence {
            boundary_id: "trusted-test-boundary".to_owned(),
            invocation_digest: digest_bytes(b"trusted-test-invocation"),
            runtime_identity: digest_bytes(b"trusted-test-runtime"),
            helper_identity: digest_bytes(b"trusted-test-helper"),
            bounded_capture_digest,
            resolved_image_digest: sha256_bytes(b"trusted-test-image"),
            container_plan_digest: digest_bytes(b"trusted-test-plan"),
            formatter_binary_digest: digest_bytes(b"trusted-test-binary"),
            formatter_artifact_digest: sha256_bytes(b"trusted-test-image"),
            formatter_config_digest: if matches!(action, DebugFormatterAction::ProvenanceMismatch) {
                digest_bytes(b"mismatched-test-config")
            } else {
                digest_bytes(b"trusted-test-config")
            },
            measurements_authoritative: !matches!(action, DebugFormatterAction::MeasurementAbsent),
            survivors,
            boundary_absent,
        },
    ))
}

#[cfg(any(test, debug_assertions))]
fn debug_path(root: &Path, relative: &str) -> Result<PathBuf, FormatterExecutorError> {
    let path =
        RootRelativePath::parse(relative, 4096).map_err(|_| FormatterExecutorError::Rejected)?;
    Ok(root.join(path.as_str()))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ArtifactState {
    digest: String,
    mode: u32,
}

fn attest_overlay(
    scope: &FormatterWriteScope,
    source: &BTreeMap<String, ArtifactState>,
    overlay: &BTreeMap<String, ArtifactState>,
) -> Result<ExecutorAttestedDiff, FormatterExecutorError> {
    let paths = source
        .keys()
        .chain(overlay.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut changes = Vec::new();
    changes
        .try_reserve(paths.len())
        .map_err(|_| FormatterExecutorError::OutputLimit)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"kit-executor-attested-diff-v1");
    frame(&mut hasher, scope.digest().as_bytes());
    for path in paths {
        let before = source.get(&path);
        let after = overlay.get(&path);
        if before == after {
            continue;
        }
        let kind = match (before, after) {
            (None, Some(_)) => ChangeKind::Add,
            (Some(_), None) => ChangeKind::Delete,
            (Some(_), Some(_)) => ChangeKind::Modify,
            (None, None) => unreachable!(),
        };
        let parsed = RootRelativePath::parse(path.clone(), 4096)
            .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
        frame(&mut hasher, path.as_bytes());
        let Some(rule) = scope.rules().iter().find(|rule| rule.path.as_str() == path) else {
            return Err(FormatterExecutorError::Undeclared(parsed));
        };
        if !rule.allowed_kinds.contains(&kind) {
            return Err(FormatterExecutorError::UnsafeOverlay);
        }
        hasher.update(&[change_kind_byte(kind)]);
        frame(
            &mut hasher,
            before.map_or("", |state| state.digest.as_str()).as_bytes(),
        );
        hasher.update(&before.map_or(0, |state| state.mode).to_le_bytes());
        frame(
            &mut hasher,
            after.map_or("", |state| state.digest.as_str()).as_bytes(),
        );
        hasher.update(&after.map_or(0, |state| state.mode).to_le_bytes());
        changes.push(ExecutorAttestedChange {
            path: parsed,
            kind,
            base_digest: before.map(|state| state.digest.clone()),
            base_mode: before.map(|state| state.mode),
            result_digest: after.map(|state| state.digest.clone()),
            result_mode: after.map(|state| state.mode),
        });
    }
    let base_tree_digest = artifact_tree_digest(source);
    let result_tree_digest = artifact_tree_digest(overlay);
    frame(&mut hasher, base_tree_digest.as_bytes());
    frame(&mut hasher, result_tree_digest.as_bytes());
    Ok(ExecutorAttestedDiff {
        version: EXECUTOR_ATTESTED_DIFF_VERSION,
        scope_digest: scope.digest().to_owned(),
        base_tree_digest,
        result_tree_digest,
        digest: format!("blake3:{}", hasher.finalize().to_hex()),
        changes,
    })
}

fn scan_tree(
    root: &Path,
    max_entries: usize,
    max_file_bytes: usize,
    max_total_bytes: u64,
    deadline: Instant,
    budget: &mut dyn FormatterBudget,
) -> Result<BTreeMap<String, ArtifactState>, FormatterExecutorError> {
    let root_mount =
        file_mount_identity(&File::open(root).map_err(|_| FormatterExecutorError::UnsafeOverlay)?)?;
    let mut pending = vec![(root.to_owned(), String::new())];
    let mut files = BTreeMap::new();
    let mut entries = 0usize;
    let mut total = 0u64;
    while let Some((directory, prefix)) = pending.pop() {
        if Instant::now() >= deadline {
            return Err(FormatterExecutorError::Timeout);
        }
        let mut children = Vec::new();
        for child in fs::read_dir(directory).map_err(|_| FormatterExecutorError::UnsafeOverlay)? {
            entries = entries
                .checked_add(1)
                .ok_or(FormatterExecutorError::OutputLimit)?;
            if entries > max_entries {
                return Err(FormatterExecutorError::OutputLimit);
            }
            let child = child.map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            let name_bytes = child.file_name().as_encoded_bytes().len();
            budget
                .charge_entry(
                    name_bytes,
                    prefix.len().saturating_add(name_bytes).saturating_add(1),
                    std::mem::size_of::<fs::DirEntry>() + std::mem::size_of::<ArtifactState>(),
                )
                .map_err(FormatterExecutorError::Budget)?;
            children
                .try_reserve(1)
                .map_err(|_| FormatterExecutorError::OutputLimit)?;
            children.push(child);
        }
        children.sort_by_key(fs::DirEntry::file_name);
        for child in children {
            let name = child
                .file_name()
                .into_string()
                .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            RootRelativePath::parse(&relative, 4096)
                .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            let metadata = fs::symlink_metadata(child.path())
                .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            if metadata.file_type().is_symlink() {
                return Err(FormatterExecutorError::UnsafeOverlay);
            }
            if metadata.is_dir() {
                let directory =
                    File::open(child.path()).map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
                if file_mount_identity(&directory)? != root_mount {
                    return Err(FormatterExecutorError::UnsafeOverlay);
                }
                pending.push((child.path(), relative));
                continue;
            }
            if !metadata.is_file()
                || metadata_links(&metadata) != 1
                || metadata_mode(&metadata) & 0o7000 != 0
                || metadata.len() > max_file_bytes as u64
            {
                return Err(FormatterExecutorError::UnsafeOverlay);
            }
            total = total
                .checked_add(metadata.len())
                .ok_or(FormatterExecutorError::OutputLimit)?;
            if total > max_total_bytes {
                return Err(FormatterExecutorError::OutputLimit);
            }
            budget
                .charge_bytes(metadata.len())
                .map_err(FormatterExecutorError::Budget)?;
            let mut file =
                fs::File::open(child.path()).map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            if file_mount_identity(&file)? != root_mount {
                return Err(FormatterExecutorError::UnsafeOverlay);
            }
            let mut hasher = blake3::Hasher::new();
            let mut read = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                if Instant::now() >= deadline {
                    return Err(FormatterExecutorError::Timeout);
                }
                let count = file
                    .read(&mut buffer)
                    .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
                if count == 0 {
                    break;
                }
                read += count as u64;
                if read > metadata.len() {
                    return Err(FormatterExecutorError::UnsafeOverlay);
                }
                hasher.update(&buffer[..count]);
            }
            let after = file
                .metadata()
                .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            if read != metadata.len()
                || metadata_identity(&after) != metadata_identity(&metadata)
                || after.len() != metadata.len()
                || metadata_mode(&after) != metadata_mode(&metadata)
            {
                return Err(FormatterExecutorError::UnsafeOverlay);
            }
            files.insert(
                relative,
                ArtifactState {
                    digest: format!("blake3:{}", hasher.finalize().to_hex()),
                    mode: metadata_mode(&metadata) & 0o777,
                },
            );
        }
    }
    Ok(files)
}

fn artifact_tree_digest(entries: &BTreeMap<String, ArtifactState>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"kit-formatter-artifact-tree-v1");
    for (path, state) in entries {
        frame(&mut hasher, path.as_bytes());
        frame(&mut hasher, state.digest.as_bytes());
        hasher.update(&state.mode.to_le_bytes());
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn revoke_writable_tree(root: &Path) -> Result<(), FormatterExecutorError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let mut directories = vec![root.to_owned()];
        let mut ordered = Vec::new();
        while let Some(directory) = directories.pop() {
            let metadata = fs::symlink_metadata(&directory)
                .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(FormatterExecutorError::UnsafeOverlay);
            }
            ordered.push(directory.clone());
            for entry in
                fs::read_dir(&directory).map_err(|_| FormatterExecutorError::UnsafeOverlay)?
            {
                let path = entry
                    .map_err(|_| FormatterExecutorError::UnsafeOverlay)?
                    .path();
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
                if metadata.file_type().is_symlink() {
                    return Err(FormatterExecutorError::UnsafeOverlay);
                }
                if metadata.is_dir() {
                    directories.push(path);
                } else if metadata.is_file() && metadata_links(&metadata) == 1 {
                    fs::set_permissions(
                        path,
                        fs::Permissions::from_mode(sealed_file_mode(
                            metadata_mode(&metadata) & 0o777,
                        )),
                    )
                    .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
                } else {
                    return Err(FormatterExecutorError::UnsafeOverlay);
                }
            }
        }
        for directory in ordered.into_iter().rev() {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o500))
                .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(FormatterExecutorError::Unavailable)
    }
}

fn watch_tree(
    root: &Path,
    root_handle: &File,
    fence: &mut WorkspaceKernelMutationFence,
) -> Result<(), FormatterExecutorError> {
    let mut pending = vec![(
        root.to_owned(),
        root_handle
            .try_clone()
            .map_err(|_| FormatterExecutorError::UnsafeOverlay)?,
    )];
    while let Some((directory, handle)) = pending.pop() {
        fence
            .watch(&directory, &handle, true)
            .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
        for entry in fs::read_dir(&directory).map_err(|_| FormatterExecutorError::UnsafeOverlay)? {
            let entry = entry.map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            let path = entry.path();
            let metadata =
                fs::symlink_metadata(&path).map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            if metadata.file_type().is_symlink() {
                return Err(FormatterExecutorError::UnsafeOverlay);
            }
            let file = File::open(&path).map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            if metadata_identity(&metadata)
                != metadata_identity(
                    &file
                        .metadata()
                        .map_err(|_| FormatterExecutorError::UnsafeOverlay)?,
                )
            {
                return Err(FormatterExecutorError::UnsafeOverlay);
            }
            if metadata.is_dir() {
                pending.push((path, file));
            } else if metadata.is_file() && metadata_links(&metadata) == 1 {
                fence
                    .watch(&path, &file, false)
                    .map_err(|_| FormatterExecutorError::UnsafeOverlay)?;
            } else {
                return Err(FormatterExecutorError::UnsafeOverlay);
            }
        }
    }
    Ok(())
}

fn directory_identity(path: &Path) -> Result<(u64, u64), FormatterExecutorError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| FormatterExecutorError::Unavailable)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(FormatterExecutorError::Rejected);
    }
    Ok(metadata_identity(&metadata))
}

fn pinned_directory_identity(
    handle: &File,
    path: &Path,
) -> Result<(u64, u64), FormatterExecutorError> {
    let metadata = handle
        .metadata()
        .map_err(|_| FormatterExecutorError::Unavailable)?;
    if !metadata.is_dir() {
        return Err(FormatterExecutorError::Rejected);
    }
    let identity = metadata_identity(&metadata);
    if directory_identity(path)? != identity {
        return Err(FormatterExecutorError::Rejected);
    }
    Ok(identity)
}

#[cfg(unix)]
fn metadata_identity(metadata: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt as _;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn metadata_identity(_metadata: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

#[cfg(unix)]
fn metadata_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.mode()
}

#[cfg(not(unix))]
fn metadata_mode(_metadata: &fs::Metadata) -> u32 {
    0o600
}

#[cfg(unix)]
fn metadata_links(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    metadata.nlink()
}

#[cfg(target_os = "linux")]
fn file_mount_identity(file: &File) -> Result<[u8; 32], FormatterExecutorError> {
    use std::{mem::MaybeUninit, os::fd::AsRawFd as _};

    const STATX_MNT_ID: u32 = 0x1000;
    let mut state = MaybeUninit::<libc::statx>::zeroed();
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            STATX_MNT_ID,
            state.as_mut_ptr(),
        )
    } != 0
    {
        return Err(FormatterExecutorError::UnsafeOverlay);
    }
    let state = unsafe { state.assume_init() };
    if state.stx_mask & STATX_MNT_ID == 0 {
        return Err(FormatterExecutorError::UnsafeOverlay);
    }
    Ok(*blake3::hash(&state.stx_mnt_id.to_le_bytes()).as_bytes())
}

#[cfg(target_os = "macos")]
fn file_mount_identity(file: &File) -> Result<[u8; 32], FormatterExecutorError> {
    use std::{mem::MaybeUninit, os::fd::AsRawFd as _};

    let mut state = MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(file.as_raw_fd(), state.as_mut_ptr()) } != 0 {
        return Err(FormatterExecutorError::UnsafeOverlay);
    }
    let state = unsafe { state.assume_init() };
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&state.f_fsid as *const libc::fsid_t).cast::<u8>(),
            std::mem::size_of::<libc::fsid_t>(),
        )
    };
    Ok(*blake3::hash(bytes).as_bytes())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn file_mount_identity(_file: &File) -> Result<(), FormatterExecutorError> {
    Err(FormatterExecutorError::Unavailable)
}

#[cfg(not(unix))]
fn metadata_links(_metadata: &fs::Metadata) -> u64 {
    1
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

#[cfg(any(test, debug_assertions))]
fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn requested_image_digest(image: &str) -> String {
    let digest = image
        .strip_prefix("sha256:")
        .or_else(|| image.rsplit_once("@sha256:").map(|(_, digest)| digest))
        .unwrap_or("");
    format!("sha256:{digest}")
}

fn valid_digest(value: &str) -> bool {
    value.split_once(':').is_some_and(|(algorithm, hex)| {
        matches!(algorithm, "blake3" | "sha256")
            && hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

const fn change_kind_byte(kind: ChangeKind) -> u8 {
    match kind {
        ChangeKind::Add => 0,
        ChangeKind::Modify => 1,
        ChangeKind::Delete => 2,
    }
}

const fn sealed_file_mode(mode: u32) -> u32 {
    if mode & 0o111 == 0 { 0o400 } else { 0o500 }
}

#[cfg(any(test, debug_assertions))]
fn bounded_stream_digest(stdout: &[u8], stderr: &[u8]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"kit-formatter-bounded-capture-v1");
    frame(&mut hasher, stdout);
    frame(&mut hasher, stderr);
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn frame(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatter_write_scope_digest_vector() {
        let scope = FormatterWriteScope::new(vec![
            FormatterWriteRule::new(
                RootRelativePath::parse("src/main.rs", 4096).unwrap(),
                "blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
                0o644,
                BTreeSet::from([ChangeKind::Modify]),
            )
            .unwrap(),
        ])
        .unwrap();
        assert_eq!(
            scope.digest(),
            "blake3:cbf2ff97826913fde0b6ae8a7b97a759448962cc835cc99686c2509d23364240"
        );
    }
}
