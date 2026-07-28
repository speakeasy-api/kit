use std::{fmt, time::Duration};

use super::{format::SyntaxRequirement, ir::RootRelativePath, validate::ValidationError};
use crate::executor::formatter::{FormatterProcessEvidence, FormatterStatus};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::recover_allocations;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use unix::{StagedEdit, stage};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod unavailable;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub use unavailable::{StagedEdit, stage};

pub fn stage_traced<'workspace>(
    plan: super::validate::ValidatedPlan<'workspace>,
    limits: StageLimits,
    syntax: SyntaxRequirements<'_>,
    syntax_executors: &mut [&mut crate::executor::syntax::SyntaxExecutor],
    formatter: Option<(
        &super::format::FormatterDescriptor,
        &mut crate::executor::formatter::FormatterExecutor,
    )>,
    trace: &mut impl super::EditTrace,
) -> Result<StagedEdit<'workspace>, StageError> {
    let staged = stage(plan, limits, syntax, syntax_executors, formatter)?;
    trace.emit(super::EditTraceId::Stage);
    Ok(staged)
}

impl<'workspace> StagedEdit<'workspace> {
    pub fn verify_traced(
        self,
        request: crate::verify::profiles::VerificationRequest<'_>,
        trace: &mut impl super::EditTrace,
    ) -> Result<VerificationOutcome<'workspace>, crate::verify::profiles::VerificationError> {
        let outcome = self.verify(request)?;
        trace.emit(super::EditTraceId::Verify);
        Ok(outcome)
    }
}

pub const STAGE_FORMAT_VERSION: u16 = 1;

pub enum VerificationOutcome<'workspace> {
    Commit(VerifiedStagedEdit<'workspace>),
    Abort(AbortedStagedEdit<'workspace>),
}

impl<'workspace> VerificationOutcome<'workspace> {
    pub fn verification(&self) -> &crate::verify::profiles::VerificationResult {
        match self {
            Self::Commit(outcome) => outcome.verification(),
            Self::Abort(outcome) => outcome.verification(),
        }
    }

    pub fn operation_context(&self) -> &super::validate::EditOperationContext {
        self.staged().operation_context()
    }

    pub fn base_revision(&self) -> &str {
        self.operation_context().base_revision()
    }

    pub fn staged_state_digest(&self) -> &str {
        self.staged().state_digest()
    }

    pub fn verification_provenance(&self) -> &str {
        self.verification().provenance()
    }

    pub(crate) fn staged(&self) -> &StagedEdit<'workspace> {
        match self {
            Self::Commit(outcome) => outcome.staged(),
            Self::Abort(outcome) => outcome.staged(),
        }
    }
}

pub struct VerifiedStagedEdit<'workspace> {
    pub(crate) staged: StagedEdit<'workspace>,
    verification: crate::verify::profiles::VerificationResult,
    receipt: crate::verify::profiles::VerificationReceipt,
}

pub struct AbortedStagedEdit<'workspace> {
    staged: StagedEdit<'workspace>,
    verification: crate::verify::profiles::VerificationResult,
    receipt: crate::verify::profiles::VerificationReceipt,
}

impl<'workspace> AbortedStagedEdit<'workspace> {
    pub fn verification(&self) -> &crate::verify::profiles::VerificationResult {
        &self.verification
    }

    pub fn verification_receipt(&self) -> &crate::verify::profiles::VerificationReceipt {
        &self.receipt
    }

    pub fn operation_context(&self) -> &super::validate::EditOperationContext {
        self.staged.operation_context()
    }

    pub fn staged_state_digest(&self) -> &str {
        self.staged.state_digest()
    }

    pub fn close(mut self) -> Result<(), StageError> {
        self.staged.cleanup()
    }

    pub(crate) fn staged(&self) -> &StagedEdit<'workspace> {
        &self.staged
    }
}

impl<'workspace> VerifiedStagedEdit<'workspace> {
    pub fn verification(&self) -> &crate::verify::profiles::VerificationResult {
        &self.verification
    }

    pub fn verification_receipt(&self) -> &crate::verify::profiles::VerificationReceipt {
        &self.receipt
    }

    pub fn operation_context(&self) -> &super::validate::EditOperationContext {
        self.staged.operation_context()
    }

    pub fn staged_state_digest(&self) -> &str {
        self.staged.state_digest()
    }

    pub(crate) fn staged(&self) -> &StagedEdit<'workspace> {
        &self.staged
    }

    pub fn materialize(
        self,
        artifacts: &crate::store::artifacts::ArtifactStore,
        options: crate::workspace::edit::recovery::MaterializeOptions,
    ) -> Result<
        crate::workspace::edit::recovery::MaterializedEdit,
        crate::workspace::edit::recovery::RecoveryError,
    > {
        crate::workspace::edit::recovery::materialize(self, artifacts, options)
    }

    pub fn materialize_traced(
        self,
        artifacts: &crate::store::artifacts::ArtifactStore,
        options: crate::workspace::edit::recovery::MaterializeOptions,
        trace: &mut impl super::EditTrace,
    ) -> Result<
        crate::workspace::edit::recovery::MaterializedEdit,
        crate::workspace::edit::recovery::RecoveryError,
    > {
        let edit = self.materialize(artifacts, options)?;
        trace.emit(super::EditTraceId::Recovery);
        Ok(edit)
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        StagedEdit<'workspace>,
        crate::verify::profiles::VerificationReceipt,
    ) {
        (self.staged, self.receipt)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StagedOperation {
    Add(RootRelativePath),
    Delete(RootRelativePath),
    Move {
        from: RootRelativePath,
        to: RootRelativePath,
    },
    Replace(RootRelativePath),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StageLimits {
    pub max_entries: usize,
    pub max_total_bytes: u64,
    pub max_file_bytes: usize,
    pub max_formatter_output_bytes: usize,
    pub max_name_bytes: usize,
    pub max_path_bytes: usize,
    pub max_metadata_bytes: usize,
    pub max_time: Duration,
}

impl Default for StageLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_total_bytes: 1024 * 1024 * 1024,
            max_file_bytes: 64 * 1024 * 1024,
            max_formatter_output_bytes: 1024 * 1024,
            max_name_bytes: 64 * 1024 * 1024,
            max_path_bytes: 256 * 1024 * 1024,
            max_metadata_bytes: 256 * 1024 * 1024,
            max_time: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageLimit {
    Entries,
    TotalBytes,
    FileBytes,
    FormatterOutput,
    NameBytes,
    PathBytes,
    MetadataMemory,
    Time,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StageChange {
    path: RootRelativePath,
    before_hash: Option<String>,
    after_hash: Option<String>,
    before_mode: Option<u32>,
    after_mode: Option<u32>,
}

impl StageChange {
    pub fn path(&self) -> &RootRelativePath {
        &self.path
    }

    pub fn before_hash(&self) -> Option<&str> {
        self.before_hash.as_deref()
    }

    pub fn after_hash(&self) -> Option<&str> {
        self.after_hash.as_deref()
    }

    pub const fn before_mode(&self) -> Option<u32> {
        self.before_mode
    }

    pub const fn after_mode(&self) -> Option<u32> {
        self.after_mode
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FormatterCapture {
    id: String,
    version: String,
    status: FormatterStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_length: u64,
    stdout_digest: String,
    stderr_length: u64,
    stderr_digest: String,
    output_attestation: String,
    elapsed: Duration,
    overlay_digest: String,
    process: FormatterProcessEvidence,
    verified_binary_digest: String,
    verified_config_digest: String,
    profile_digest: String,
    write_scope_digest: String,
}

impl FormatterCapture {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn version(&self) -> &str {
        &self.version
    }

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
        &self.overlay_digest
    }

    pub const fn process(&self) -> &FormatterProcessEvidence {
        &self.process
    }

    pub fn verified_binary_digest(&self) -> &str {
        &self.verified_binary_digest
    }

    pub fn verified_config_digest(&self) -> &str {
        &self.verified_config_digest
    }

    pub fn profile_digest(&self) -> &str {
        &self.profile_digest
    }

    pub fn write_scope_digest(&self) -> &str {
        &self.write_scope_digest
    }
}

#[derive(Debug)]
pub enum StageError {
    Validation(ValidationError),
    LimitExceeded(StageLimit),
    UnsafeSource,
    StageChanged,
    PlanMismatch,
    SyntaxFailed(RootRelativePath),
    SyntaxTimeout(RootRelativePath),
    SyntaxUnavailable(RootRelativePath),
    FormatterUnavailable,
    FormatterRejected,
    FormatterFailed(Box<FormatterCapture>),
    FormatterTimeout(Box<FormatterCapture>),
    FormatterNotQuiescent,
    FormatterUndeclaredChange(RootRelativePath),
    FormatterUnsafeChange,
    CleanupFailed,
    Unavailable,
}

impl fmt::Display for StageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(error) => error.fmt(formatter),
            Self::LimitExceeded(limit) => write!(formatter, "staging exceeded {limit:?} limit"),
            Self::UnsafeSource => formatter.write_str("workspace contains an unsafe stage source"),
            Self::StageChanged => formatter.write_str("staged content changed unexpectedly"),
            Self::PlanMismatch => {
                formatter.write_str("staged result does not match validated plan")
            }
            Self::SyntaxFailed(path) => write!(formatter, "staged syntax failed at {path}"),
            Self::SyntaxTimeout(path) => write!(formatter, "staged syntax timed out at {path}"),
            Self::SyntaxUnavailable(path) => {
                write!(
                    formatter,
                    "required staged syntax adapter unavailable at {path}"
                )
            }
            Self::FormatterUnavailable => {
                formatter.write_str("isolated formatter runner is unavailable")
            }
            Self::FormatterRejected => formatter.write_str("isolated formatter request rejected"),
            Self::FormatterFailed(_) => formatter.write_str("staged formatter failed"),
            Self::FormatterTimeout(_) => formatter.write_str("staged formatter timed out"),
            Self::FormatterNotQuiescent => {
                formatter.write_str("formatter process tree is not quiescent")
            }
            Self::FormatterUndeclaredChange(path) => {
                write!(formatter, "formatter changed undeclared path {path}")
            }
            Self::FormatterUnsafeChange => {
                formatter.write_str("formatter created an unsafe staged entry")
            }
            Self::CleanupFailed => formatter.write_str("private stage cleanup failed"),
            Self::Unavailable => formatter.write_str("safe edit staging is unavailable"),
        }
    }
}

impl std::error::Error for StageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Validation(error) => Some(error),
            _ => None,
        }
    }
}

pub type SyntaxRequirements<'a> = &'a [SyntaxRequirement];

#[allow(clippy::too_many_arguments)]
pub(crate) fn capture(
    id: &str,
    version: &str,
    status: FormatterStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_length: u64,
    stdout_digest: String,
    stderr_length: u64,
    stderr_digest: String,
    output_attestation: String,
    elapsed: Duration,
    overlay_digest: String,
    process: FormatterProcessEvidence,
    verified_binary_digest: String,
    verified_config_digest: String,
    profile_digest: String,
    write_scope_digest: String,
) -> FormatterCapture {
    FormatterCapture {
        id: id.to_owned(),
        version: version.to_owned(),
        status,
        stdout,
        stderr,
        stdout_length,
        stdout_digest,
        stderr_length,
        stderr_digest,
        output_attestation,
        elapsed,
        overlay_digest,
        process,
        verified_binary_digest,
        verified_config_digest,
        profile_digest,
        write_scope_digest,
    }
}

pub(crate) fn change(
    path: RootRelativePath,
    before_hash: Option<String>,
    after_hash: Option<String>,
    before_mode: Option<u32>,
    after_mode: Option<u32>,
) -> StageChange {
    StageChange {
        path,
        before_hash,
        after_hash,
        before_mode,
        after_mode,
    }
}
