use std::{fmt, time::Duration};

use super::{format::SyntaxRequirement, ir::RootRelativePath, validate::ValidationError};

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
    trace: &mut impl super::EditTrace,
) -> Result<StagedEdit<'workspace>, StageError> {
    let staged = stage(plan, limits, syntax, syntax_executors)?;
    trace.emit(super::EditTraceId::Stage);
    Ok(staged)
}

pub const STAGE_FORMAT_VERSION: u16 = 2;

impl<'workspace> StagedEdit<'workspace> {
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
    pub max_syntax_output_bytes: usize,
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
            max_syntax_output_bytes: 1024 * 1024,
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
    SyntaxOutput,
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
