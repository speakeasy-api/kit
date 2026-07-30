use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::workspace::edit::ir::RootRelativePath;

const MARKER_NAME: &str = ".kit-workspace";
const MARKER_VERSION: &str = "kit-workspace-v1";
const MAX_COMMAND_OUTPUT: usize = 64 * 1024 * 1024;
const MAX_ERROR_OUTPUT: usize = 16 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_SNAPSHOT_ENTRIES: usize = 1_000_000;
const MAX_SNAPSHOT_NAME_BYTES: usize = 256 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_GIT_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_GIT_PACK_ENTRIES: usize = 1_000_000;
const MAX_GIT_EXECUTABLE_BYTES: u64 = 128 * 1024 * 1024;
const GIT_EXECUTABLE_HASH_BUFFER_BYTES: usize = 64 * 1024;
const BTREE_ENTRY_WEIGHT: usize = std::mem::size_of::<[usize; 8]>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AcquisitionMode {
    DetachedWorktree,
    LocalClone,
    CopyOnWriteSnapshot,
}

impl AcquisitionMode {
    fn marker_name(self) -> &'static str {
        match self {
            Self::DetachedWorktree => "worktree",
            Self::LocalClone => "clone",
            Self::CopyOnWriteSnapshot => "cow-fallback",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WriterPolicy {
    TrustedAllowSharedGitMetadata,
    Restricted,
    Hostile,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitMetadata {
    SharedWithSource,
    Independent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirtyContent {
    SourceClean,
    Included,
    NotIncluded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotMaterialization {
    DetachedWorktree,
    IndependentClone,
    FullCopyFallback,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    pub fn new(value: impl Into<String>) -> Result<Self, AcquisitionError> {
        let value = value.into();
        validate_id("workspace", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnerId(String);

impl OwnerId {
    pub fn new(value: impl Into<String>) -> Result<Self, AcquisitionError> {
        let value = value.into();
        validate_id("owner", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcquisitionId(String);

impl AcquisitionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateHash(String);

impl StateHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for StateHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRevision {
    pub number: u64,
    pub hash: StateHash,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcquisitionRequest {
    pub source: PathBuf,
    pub managed_root: PathBuf,
    pub workspace_id: WorkspaceId,
    pub owner_id: OwnerId,
    pub mode: AcquisitionMode,
    pub writer_policy: WriterPolicy,
}

impl AcquisitionRequest {
    pub fn new(
        source: impl Into<PathBuf>,
        managed_root: impl Into<PathBuf>,
        workspace_id: WorkspaceId,
        owner_id: OwnerId,
        mode: AcquisitionMode,
        writer_policy: WriterPolicy,
    ) -> Self {
        Self {
            source: source.into(),
            managed_root: managed_root.into(),
            workspace_id,
            owner_id,
            mode,
            writer_policy,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcquisitionResult {
    pub canonical_source: PathBuf,
    pub managed_root: PathBuf,
    pub path: PathBuf,
    pub base_commit: String,
    pub initial_dirty_state: StateHash,
    pub workspace_revision: WorkspaceRevision,
    pub mode: AcquisitionMode,
    pub materialization: SnapshotMaterialization,
    pub git_metadata: GitMetadata,
    pub dirty_content: DirtyContent,
    pub acquisition_id: AcquisitionId,
    pub owner_id: OwnerId,
    allocation_path: PathBuf,
    allocation_identity: Option<FilesystemIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReservationRequest {
    pub managed_root: PathBuf,
    pub workspace_id: WorkspaceId,
    pub owner_id: OwnerId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReservedTarget {
    pub path: PathBuf,
    pub acquisition_id: AcquisitionId,
    pub owner_id: OwnerId,
    managed_root: PathBuf,
    allocation_identity: Option<FilesystemIdentity>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CleanupOutcome {
    Removed,
    AlreadyAbsent,
}

#[derive(Debug)]
pub enum AcquisitionError {
    InvalidId {
        kind: &'static str,
    },
    UnsafePath {
        kind: &'static str,
        path: PathBuf,
    },
    SymlinkPath {
        kind: &'static str,
        path: PathBuf,
    },
    NotDirectory {
        kind: &'static str,
        path: PathBuf,
    },
    OverlappingRoots,
    NotRepositoryRoot(PathBuf),
    MissingHead,
    SharedMetadataForbidden(WriterPolicy),
    AtomicReservationExhausted,
    SourceChangedDuringAcquisition,
    SnapshotMismatch,
    MarkerMissing,
    MarkerMismatch,
    FilesystemIdentityChanged,
    UnsupportedIndexState {
        path: PathBuf,
        reason: &'static str,
    },
    UnsupportedGitlink(PathBuf),
    UnsupportedSourceEntry(PathBuf),
    OutputTooLarge(&'static str),
    CommandTimedOut(&'static str),
    SnapshotLimitExceeded,
    HardlinkedSourceEntry(PathBuf),
    SourceFilesystemBoundary(PathBuf),
    Unavailable {
        capability: &'static str,
    },
    Git {
        operation: &'static str,
        message: String,
    },
    Io {
        operation: &'static str,
        source: io::Error,
    },
    CleanupAfterAcquisition {
        acquisition: Box<AcquisitionError>,
        cleanup: Box<AcquisitionError>,
    },
}

impl fmt::Display for AcquisitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId { kind } => write!(formatter, "invalid {kind} id"),
            Self::UnsafePath { kind, path } => {
                write!(formatter, "unsafe {kind} path {}", path.display())
            }
            Self::SymlinkPath { kind, path } => {
                write!(formatter, "{kind} path contains symlink {}", path.display())
            }
            Self::NotDirectory { kind, path } => {
                write!(
                    formatter,
                    "{kind} path is not a directory: {}",
                    path.display()
                )
            }
            Self::OverlappingRoots => {
                formatter.write_str("source and managed root must not overlap")
            }
            Self::NotRepositoryRoot(path) => {
                write!(
                    formatter,
                    "source is not a Git worktree root: {}",
                    path.display()
                )
            }
            Self::MissingHead => formatter.write_str("source repository has no base commit"),
            Self::SharedMetadataForbidden(policy) => write!(
                formatter,
                "detached worktrees share writable Git metadata and are forbidden by {policy:?} policy"
            ),
            Self::AtomicReservationExhausted => {
                formatter.write_str("could not reserve a unique workspace target")
            }
            Self::SourceChangedDuringAcquisition => {
                formatter.write_str("source changed while the workspace was acquired")
            }
            Self::SnapshotMismatch => {
                formatter.write_str("copy snapshot does not match the source dirty state")
            }
            Self::MarkerMissing => formatter.write_str("managed workspace marker is missing"),
            Self::MarkerMismatch => formatter.write_str("managed workspace marker does not match"),
            Self::FilesystemIdentityChanged => {
                formatter.write_str("managed workspace filesystem identity changed")
            }
            Self::UnsupportedIndexState { path, reason } => write!(
                formatter,
                "unsupported Git index state ({reason}) for {}",
                path.display()
            ),
            Self::UnsupportedGitlink(path) => write!(
                formatter,
                "Git submodule entries are unsupported: {}",
                path.display()
            ),
            Self::UnsupportedSourceEntry(path) => write!(
                formatter,
                "source contains an unsupported filesystem entry: {}",
                path.display()
            ),
            Self::OutputTooLarge(operation) => {
                write!(formatter, "{operation} produced too much output")
            }
            Self::CommandTimedOut(operation) => write!(formatter, "{operation} timed out"),
            Self::SnapshotLimitExceeded => {
                formatter.write_str("source snapshot exceeded its limit")
            }
            Self::HardlinkedSourceEntry(path) => write!(
                formatter,
                "source contains a hardlinked entry: {}",
                path.display()
            ),
            Self::SourceFilesystemBoundary(path) => write!(
                formatter,
                "source entry crosses a filesystem boundary: {}",
                path.display()
            ),
            Self::Unavailable { capability } => {
                write!(
                    formatter,
                    "required security capability is unavailable: {capability}"
                )
            }
            Self::Git { operation, message } => {
                write!(formatter, "Git {operation} failed: {message}")
            }
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::CleanupAfterAcquisition {
                acquisition,
                cleanup,
            } => write!(
                formatter,
                "{acquisition}; workspace cleanup also failed: {cleanup}"
            ),
        }
    }
}

impl std::error::Error for AcquisitionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub fn reserve_target(request: ReservationRequest) -> Result<ReservedTarget, AcquisitionError> {
    let managed_root = canonical_directory(&request.managed_root, "managed root")?;
    let reserved = reserve_directory(
        &managed_root,
        &request.workspace_id,
        &request.owner_id,
        "reservation",
    )?;
    Ok(ReservedTarget {
        path: reserved.path,
        acquisition_id: reserved.id,
        owner_id: request.owner_id,
        managed_root,
        allocation_identity: reserved.identity,
    })
}

pub fn release_reserved_target(
    reserved: &ReservedTarget,
) -> Result<CleanupOutcome, AcquisitionError> {
    remove_marked_directory(
        &reserved.managed_root,
        &reserved.path,
        &reserved.acquisition_id,
        &reserved.owner_id,
        "reservation",
        reserved.allocation_identity.as_ref(),
    )
}

pub fn acquire(request: AcquisitionRequest) -> Result<AcquisitionResult, AcquisitionError> {
    if request.mode == AcquisitionMode::DetachedWorktree
        && request.writer_policy != WriterPolicy::TrustedAllowSharedGitMetadata
    {
        return Err(AcquisitionError::SharedMetadataForbidden(
            request.writer_policy,
        ));
    }

    let source = canonical_directory(&request.source, "source")?;
    let managed_root = canonical_directory(&request.managed_root, "managed root")?;
    if source.starts_with(&managed_root) || managed_root.starts_with(&source) {
        return Err(AcquisitionError::OverlappingRoots);
    }
    if request.writer_policy != WriterPolicy::TrustedAllowSharedGitMetadata {
        return acquire_untrusted(request, source, managed_root);
    }
    ensure_repository_root(&source)?;
    reject_split_index(&source)?;
    let base_commit = git_text(
        &source,
        "resolve HEAD",
        ["rev-parse", "--verify", "HEAD^{commit}"],
    )
    .map_err(|error| match error {
        AcquisitionError::Git { .. } => AcquisitionError::MissingHead,
        other => other,
    })?;
    let source_state = git_status(&source)?;

    let reserved = reserve_directory(
        &managed_root,
        &request.workspace_id,
        &request.owner_id,
        request.mode.marker_name(),
    )?;
    let workspace_path = reserved.path.join("repo");
    let mut worktree_registered = false;

    let materialized = (|| {
        let materialization = match request.mode {
            AcquisitionMode::DetachedWorktree => {
                git_success(
                    &source,
                    "add detached worktree",
                    [
                        OsStr::new("worktree"),
                        OsStr::new("add"),
                        OsStr::new("--detach"),
                        OsStr::new("--no-checkout"),
                        workspace_path.as_os_str(),
                        OsStr::new(&base_commit),
                    ],
                )?;
                worktree_registered = true;
                checkout_base(&workspace_path, &base_commit)?;
                SnapshotMaterialization::DetachedWorktree
            }
            AcquisitionMode::LocalClone => {
                clone_without_checkout(&source, &workspace_path)?;
                checkout_base(&workspace_path, &base_commit)?;
                SnapshotMaterialization::IndependentClone
            }
            AcquisitionMode::CopyOnWriteSnapshot => {
                clone_without_checkout(&source, &workspace_path)?;
                copy_worktree(&source, &workspace_path)?;
                copy_index(&source, &workspace_path)?;
                SnapshotMaterialization::FullCopyFallback
            }
        };

        let current_base = git_text(
            &source,
            "recheck HEAD",
            ["rev-parse", "--verify", "HEAD^{commit}"],
        )?;
        let current_source_state = git_status(&source)?;
        if current_base != base_commit || current_source_state.hash != source_state.hash {
            return Err(AcquisitionError::SourceChangedDuringAcquisition);
        }

        let workspace_state = git_status(&workspace_path)?;
        if request.mode == AcquisitionMode::CopyOnWriteSnapshot
            && workspace_state.hash != source_state.hash
        {
            return Err(AcquisitionError::SnapshotMismatch);
        }
        let git_metadata = verify_git_metadata(
            &source,
            &workspace_path,
            request.mode == AcquisitionMode::DetachedWorktree,
        )?;
        let dirty_content = if !source_state.dirty {
            DirtyContent::SourceClean
        } else if request.mode == AcquisitionMode::CopyOnWriteSnapshot {
            DirtyContent::Included
        } else {
            DirtyContent::NotIncluded
        };
        let workspace_revision = WorkspaceRevision {
            number: 0,
            hash: revision_hash(&base_commit, &workspace_state.hash),
        };
        Ok((
            materialization,
            git_metadata,
            dirty_content,
            workspace_revision,
        ))
    })();

    let (materialization, git_metadata, dirty_content, workspace_revision) = match materialized {
        Ok(value) => value,
        Err(error) => {
            if let Err(cleanup) = cleanup_failed_acquisition(
                &managed_root,
                &reserved.path,
                &reserved.id,
                &request.owner_id,
                request.mode.marker_name(),
                reserved.identity.as_ref(),
                worktree_registered.then_some((&source, &workspace_path)),
            ) {
                return Err(AcquisitionError::CleanupAfterAcquisition {
                    acquisition: Box::new(error),
                    cleanup: Box::new(cleanup),
                });
            }
            return Err(error);
        }
    };

    Ok(AcquisitionResult {
        canonical_source: source,
        managed_root,
        path: workspace_path,
        base_commit,
        initial_dirty_state: source_state.hash,
        workspace_revision,
        mode: request.mode,
        materialization,
        git_metadata,
        dirty_content,
        acquisition_id: reserved.id,
        owner_id: request.owner_id,
        allocation_path: reserved.path,
        allocation_identity: reserved.identity,
    })
}

fn acquire_untrusted(
    request: AcquisitionRequest,
    source: PathBuf,
    managed_root: PathBuf,
) -> Result<AcquisitionResult, AcquisitionError> {
    ensure_plain_repository_root(&source)?;
    let reserved = reserve_directory(
        &managed_root,
        &request.workspace_id,
        &request.owner_id,
        request.mode.marker_name(),
    )?;
    let workspace_path = reserved.path.join("repo");

    let materialized = (|| {
        snapshot_repository(&source, &workspace_path)?;
        sanitize_snapshot_git_metadata(&workspace_path)?;
        reject_split_index(&workspace_path)?;
        let base_commit = git_text(
            &workspace_path,
            "resolve HEAD",
            ["rev-parse", "--verify", "HEAD^{commit}"],
        )
        .map_err(|error| match error {
            AcquisitionError::Git { .. } => AcquisitionError::MissingHead,
            other => other,
        })?;
        let source_state = git_status(&workspace_path)?;
        validate_snapshot_entries(&workspace_path)?;

        let materialization = match request.mode {
            AcquisitionMode::LocalClone => {
                clear_snapshot_worktree(&workspace_path)?;
                reset_snapshot_to_base(&workspace_path, &base_commit)?;
                SnapshotMaterialization::FullCopyFallback
            }
            AcquisitionMode::CopyOnWriteSnapshot => {
                remove_ignored_snapshot_entries(&workspace_path)?;
                SnapshotMaterialization::FullCopyFallback
            }
            AcquisitionMode::DetachedWorktree => {
                unreachable!("shared worktree policy checked above")
            }
        };
        let workspace_state = git_status(&workspace_path)?;
        if request.mode == AcquisitionMode::CopyOnWriteSnapshot
            && workspace_state.hash != source_state.hash
        {
            return Err(AcquisitionError::SnapshotMismatch);
        }
        let git_dir = fs::canonicalize(workspace_path.join(".git"))
            .map_err(|source| io_error("canonicalize snapshot Git directory", source))?;
        if !git_dir.starts_with(&workspace_path) {
            return Err(AcquisitionError::SnapshotMismatch);
        }
        let dirty_content = if !source_state.dirty {
            DirtyContent::SourceClean
        } else if request.mode == AcquisitionMode::CopyOnWriteSnapshot {
            DirtyContent::Included
        } else {
            DirtyContent::NotIncluded
        };
        let workspace_revision = WorkspaceRevision {
            number: 0,
            hash: revision_hash(&base_commit, &workspace_state.hash),
        };
        Ok((
            base_commit,
            source_state,
            materialization,
            dirty_content,
            workspace_revision,
        ))
    })();

    let (base_commit, source_state, materialization, dirty_content, workspace_revision) =
        match materialized {
            Ok(value) => value,
            Err(error) => {
                if let Err(cleanup) = cleanup_failed_acquisition(
                    &managed_root,
                    &reserved.path,
                    &reserved.id,
                    &request.owner_id,
                    request.mode.marker_name(),
                    reserved.identity.as_ref(),
                    None,
                ) {
                    return Err(AcquisitionError::CleanupAfterAcquisition {
                        acquisition: Box::new(error),
                        cleanup: Box::new(cleanup),
                    });
                }
                return Err(error);
            }
        };

    Ok(AcquisitionResult {
        canonical_source: source,
        managed_root,
        path: workspace_path,
        base_commit,
        initial_dirty_state: source_state.hash,
        workspace_revision,
        mode: request.mode,
        materialization,
        git_metadata: GitMetadata::Independent,
        dirty_content,
        acquisition_id: reserved.id,
        owner_id: request.owner_id,
        allocation_path: reserved.path,
        allocation_identity: reserved.identity,
    })
}

pub fn cleanup(workspace: &AcquisitionResult) -> Result<CleanupOutcome, AcquisitionError> {
    validate_cleanup_boundary(
        &workspace.managed_root,
        &workspace.allocation_path,
        &workspace.path,
    )?;
    cleanup_failed_acquisition(
        &workspace.managed_root,
        &workspace.allocation_path,
        &workspace.acquisition_id,
        &workspace.owner_id,
        workspace.mode.marker_name(),
        workspace.allocation_identity.as_ref(),
        (workspace.mode == AcquisitionMode::DetachedWorktree)
            .then_some((&workspace.canonical_source, &workspace.path)),
    )
}

struct ReservedDirectory {
    path: PathBuf,
    id: AcquisitionId,
    identity: Option<FilesystemIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FilesystemIdentity {
    first: u64,
    second: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TrustedGitRepositoryIdentity {
    root: FilesystemIdentity,
    git_dir: FilesystemIdentity,
    common_dir: FilesystemIdentity,
    #[cfg(unix)]
    policy_metadata: BTreeMap<PathBuf, UnixMetadata>,
    #[cfg(unix)]
    fence_metadata: BTreeMap<PathBuf, UnixMetadata>,
}

#[derive(Debug)]
pub(crate) struct TrustedGitRepositoryPolicySession {
    root: File,
    repository: PathBuf,
    identity: TrustedGitRepositoryIdentity,
    implementation: TrustedGitImplementationIdentity,
    metrics: TrustedGitRepositoryPolicyMetrics,
}

impl TrustedGitRepositoryPolicySession {
    pub(crate) const fn logical_bytes(&self) -> usize {
        self.metrics.logical_bytes()
    }
    pub(crate) const fn peak_bytes(&self) -> usize {
        self.metrics.peak_bytes()
    }
    pub(crate) const fn consumed_work(&self) -> usize {
        self.metrics.consumed_work()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TrustedGitRepositoryPolicyMetrics {
    pub(crate) logical_bytes: usize,
    pub(crate) peak_bytes: usize,
    pub(crate) consumed_work: usize,
    pub(crate) policy_scans: usize,
    pub(crate) fence_scans: usize,
    pub(crate) streamed_executable_bytes: usize,
    pub(crate) streamed_executable_chunks: usize,
    pub(crate) commands: usize,
    pub(crate) output_bytes: usize,
}

impl TrustedGitRepositoryPolicyMetrics {
    pub(crate) const fn logical_bytes(self) -> usize {
        self.logical_bytes
    }
    pub(crate) const fn peak_bytes(self) -> usize {
        self.peak_bytes
    }
    pub(crate) const fn consumed_work(self) -> usize {
        self.consumed_work
    }
    pub(crate) const fn policy_scans(self) -> usize {
        self.policy_scans
    }
    pub(crate) const fn fence_scans(self) -> usize {
        self.fence_scans
    }
    pub(crate) const fn streamed_executable_bytes(self) -> usize {
        self.streamed_executable_bytes
    }
    pub(crate) const fn streamed_executable_chunks(self) -> usize {
        self.streamed_executable_chunks
    }
    pub(crate) const fn commands(self) -> usize {
        self.commands
    }
    pub(crate) const fn output_bytes(self) -> usize {
        self.output_bytes
    }
}

#[derive(Debug)]
struct TrustedGitImplementationCapture {
    identity: TrustedGitImplementationIdentity,
    streamed_bytes: usize,
    streamed_chunks: usize,
    output_bytes: usize,
    peak_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrustedGitImplementationIdentity {
    pub(crate) executable: PathBuf,
    pub(crate) executable_digest: [u8; 32],
    pub(crate) version_digest: [u8; 32],
}

struct GitState {
    hash: StateHash,
    dirty: bool,
}

fn validate_id(kind: &'static str, value: &str) -> Result<(), AcquisitionError> {
    if value.is_empty()
        || value.len() > 128
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(AcquisitionError::InvalidId { kind });
    }
    Ok(())
}

fn canonical_directory(path: &Path, kind: &'static str) -> Result<PathBuf, AcquisitionError> {
    validate_lexical_path(path, kind)?;
    reject_path_symlinks(path, kind)?;
    let canonical =
        fs::canonicalize(path).map_err(|source| io_error("canonicalize path", source))?;
    let metadata = fs::metadata(&canonical).map_err(|source| io_error("inspect path", source))?;
    if !metadata.is_dir() {
        return Err(AcquisitionError::NotDirectory {
            kind,
            path: path.to_path_buf(),
        });
    }
    Ok(canonical)
}

fn validate_lexical_path(path: &Path, kind: &'static str) -> Result<(), AcquisitionError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(AcquisitionError::UnsafePath {
            kind,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn reject_path_symlinks(path: &Path, kind: &'static str) -> Result<(), AcquisitionError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        let metadata = fs::symlink_metadata(&current)
            .map_err(|source| io_error("inspect path component", source))?;
        if metadata.file_type().is_symlink() {
            return Err(AcquisitionError::SymlinkPath {
                kind,
                path: current,
            });
        }
    }
    Ok(())
}

fn ensure_repository_root(source: &Path) -> Result<(), AcquisitionError> {
    let root = git_text(
        source,
        "resolve repository root",
        ["rev-parse", "--show-toplevel"],
    )?;
    let root =
        fs::canonicalize(root).map_err(|source| io_error("canonicalize Git root", source))?;
    if root != source {
        return Err(AcquisitionError::NotRepositoryRoot(source.to_path_buf()));
    }
    Ok(())
}

fn ensure_plain_repository_root(source: &Path) -> Result<(), AcquisitionError> {
    let git_dir = source.join(".git");
    let metadata = fs::symlink_metadata(&git_dir)
        .map_err(|_| AcquisitionError::NotRepositoryRoot(source.to_path_buf()))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(AcquisitionError::NotRepositoryRoot(source.to_path_buf()));
    }
    let head = git_dir.join("HEAD");
    let metadata = fs::symlink_metadata(&head)
        .map_err(|_| AcquisitionError::NotRepositoryRoot(source.to_path_buf()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 4096 {
        return Err(AcquisitionError::NotRepositoryRoot(source.to_path_buf()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn snapshot_repository(_source: &Path, _target: &Path) -> Result<(), AcquisitionError> {
    Err(AcquisitionError::Unavailable {
        capability: "physical no-follow workspace snapshots",
    })
}

#[cfg(unix)]
fn snapshot_repository(source: &Path, target: &Path) -> Result<(), AcquisitionError> {
    let source_dir = open_absolute_directory(source, "source")?;
    let source_root = unix_metadata(&source_dir)
        .map_err(|error| snapshot_error("inspect snapshot source root", error))?;
    let source_filesystem = SnapshotFilesystem {
        device: source_root.dev,
        mount: snapshot_mount_identity(&source_dir)?,
    };
    let target_parent = target
        .parent()
        .ok_or_else(|| AcquisitionError::UnsafePath {
            kind: "snapshot target",
            path: target.to_path_buf(),
        })?;
    let target_name = target
        .file_name()
        .ok_or_else(|| AcquisitionError::UnsafePath {
            kind: "snapshot target",
            path: target.to_path_buf(),
        })?;
    let target_parent = open_absolute_directory(target_parent, "snapshot target parent")?;
    mkdir_at(&target_parent, target_name, 0o700)
        .map_err(|error| snapshot_error("create repository snapshot", error))?;
    let target_dir = open_directory_at(&target_parent, target_name)
        .map_err(|error| snapshot_error("open repository snapshot", error))?;
    let target_root = unix_metadata(&target_dir)
        .map_err(|error| snapshot_error("inspect snapshot target root", error))?;
    let target_filesystem = SnapshotFilesystem {
        device: target_root.dev,
        mount: snapshot_mount_identity(&target_dir)?,
    };

    let mut limits = SnapshotLimits::default();
    copy_directory_at(
        &source_dir,
        &target_dir,
        source,
        Path::new(""),
        &source_filesystem,
        &target_filesystem,
        &mut limits,
    )?;

    let source_hash = hash_directory_at(&source_dir, source, &source_filesystem)?;
    let target_hash = hash_directory_at(&target_dir, target, &target_filesystem)?;
    if source_hash != target_hash {
        return Err(AcquisitionError::SourceChangedDuringAcquisition);
    }
    let source_after = unix_metadata(&source_dir)
        .map_err(|error| snapshot_error("reinspect snapshot source root", error))?;
    if !source_root.same_directory(&source_after) {
        return Err(AcquisitionError::SourceChangedDuringAcquisition);
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UnixMetadata {
    dev: u64,
    ino: u64,
    nlink: u64,
    mode: libc::mode_t,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(unix)]
#[derive(Clone, Debug, Eq, PartialEq)]
enum SnapshotMountIdentity {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Linux(u64),
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    Apple(Vec<u8>),
}

#[cfg(unix)]
struct SnapshotFilesystem {
    device: u64,
    mount: SnapshotMountIdentity,
}

#[cfg(unix)]
impl UnixMetadata {
    fn kind(self) -> libc::mode_t {
        self.mode & libc::S_IFMT
    }

    fn same_object(self, other: &Self) -> bool {
        self.dev == other.dev && self.ino == other.ino && self.kind() == other.kind()
    }

    fn same_file(self, other: &Self) -> bool {
        self.same_object(other)
            && self.nlink == other.nlink
            && self.size == other.size
            && self.modified_seconds == other.modified_seconds
            && self.modified_nanoseconds == other.modified_nanoseconds
            && self.mode & 0o777 == other.mode & 0o777
    }

    fn same_directory(self, other: &Self) -> bool {
        self.same_object(other)
            && self.modified_seconds == other.modified_seconds
            && self.modified_nanoseconds == other.modified_nanoseconds
    }
}

#[cfg(unix)]
#[derive(Default)]
struct SnapshotLimits {
    entries: usize,
    bytes: u64,
}

#[cfg(unix)]
fn copy_directory_at(
    source_dir: &File,
    target_dir: &File,
    source_root: &Path,
    relative: &Path,
    source_filesystem: &SnapshotFilesystem,
    target_filesystem: &SnapshotFilesystem,
    limits: &mut SnapshotLimits,
) -> Result<(), AcquisitionError> {
    use std::os::unix::fs::PermissionsExt;

    let directory_before = unix_metadata(source_dir)
        .map_err(|error| snapshot_error("inspect snapshot directory", error))?;
    for name in directory_entries(source_dir)? {
        limits.entries = limits
            .entries
            .checked_add(1)
            .ok_or(AcquisitionError::SnapshotLimitExceeded)?;
        if limits.entries > MAX_SNAPSHOT_ENTRIES {
            return Err(AcquisitionError::SnapshotLimitExceeded);
        }
        let child_relative = relative.join(&name);
        let display_path = source_root.join(&child_relative);
        let before = metadata_at(source_dir, &name)
            .map_err(|error| snapshot_error("inspect repository snapshot entry", error))?;
        if before.dev != source_filesystem.device {
            return Err(AcquisitionError::SourceFilesystemBoundary(display_path));
        }
        match before.kind() {
            libc::S_IFDIR => {
                let child_source = open_directory_at(source_dir, &name)
                    .map_err(|error| snapshot_error("open repository snapshot directory", error))?;
                let opened = unix_metadata(&child_source)
                    .map_err(|error| snapshot_error("inspect open snapshot directory", error))?;
                if !before.same_object(&opened) || opened.dev != source_filesystem.device {
                    return Err(AcquisitionError::SourceChangedDuringAcquisition);
                }
                if snapshot_mount_identity(&child_source)? != source_filesystem.mount {
                    return Err(AcquisitionError::SourceFilesystemBoundary(display_path));
                }
                mkdir_at(target_dir, &name, 0o700).map_err(|error| {
                    snapshot_error("create repository snapshot directory", error)
                })?;
                let child_target = open_directory_at(target_dir, &name)
                    .map_err(|error| snapshot_error("open snapshot target directory", error))?;
                let target_metadata = unix_metadata(&child_target)
                    .map_err(|error| snapshot_error("inspect snapshot target directory", error))?;
                if target_metadata.dev != target_filesystem.device {
                    return Err(AcquisitionError::SourceFilesystemBoundary(target_dir_path(
                        &child_relative,
                    )));
                }
                if snapshot_mount_identity(&child_target)? != target_filesystem.mount {
                    return Err(AcquisitionError::SourceFilesystemBoundary(target_dir_path(
                        &child_relative,
                    )));
                }
                copy_directory_at(
                    &child_source,
                    &child_target,
                    source_root,
                    &child_relative,
                    source_filesystem,
                    target_filesystem,
                    limits,
                )?;
            }
            libc::S_IFREG => {
                if before.nlink > 1 {
                    return Err(AcquisitionError::HardlinkedSourceEntry(display_path));
                }
                let remaining = MAX_SNAPSHOT_BYTES.saturating_sub(limits.bytes);
                if before.size > remaining {
                    return Err(AcquisitionError::SnapshotLimitExceeded);
                }
                let mut input = open_file_at(source_dir, &name, libc::O_RDONLY | libc::O_NONBLOCK)
                    .map_err(|error| snapshot_error("open repository snapshot file", error))?;
                let opened = unix_metadata(&input)
                    .map_err(|error| snapshot_error("inspect open snapshot file", error))?;
                if opened.kind() != libc::S_IFREG || !before.same_file(&opened) {
                    return Err(AcquisitionError::SourceChangedDuringAcquisition);
                }
                if snapshot_mount_identity(&input)? != source_filesystem.mount {
                    return Err(AcquisitionError::SourceFilesystemBoundary(display_path));
                }
                let mut output = create_file_at(target_dir, &name, 0o600)
                    .map_err(|error| snapshot_error("create repository snapshot file", error))?;
                let copied = io::copy(
                    &mut Read::by_ref(&mut input).take(remaining.saturating_add(1)),
                    &mut output,
                )
                .map_err(|error| snapshot_error("copy repository snapshot file", error))?;
                if copied > remaining {
                    return Err(AcquisitionError::SnapshotLimitExceeded);
                }
                limits.bytes += copied;
                output
                    .set_permissions(fs::Permissions::from_mode(permission_mode(
                        before.mode & 0o777,
                    )))
                    .map_err(|error| snapshot_error("preserve snapshot permissions", error))?;
                let after = unix_metadata(&input)
                    .map_err(|error| snapshot_error("reinspect open snapshot file", error))?;
                if !before.same_file(&after) {
                    return Err(AcquisitionError::SourceChangedDuringAcquisition);
                }
            }
            libc::S_IFLNK => {
                return Err(AcquisitionError::SymlinkPath {
                    kind: "snapshot source",
                    path: display_path,
                });
            }
            _ => return Err(AcquisitionError::UnsupportedSourceEntry(display_path)),
        }
    }
    let directory_after = unix_metadata(source_dir)
        .map_err(|error| snapshot_error("reinspect snapshot directory", error))?;
    if !directory_before.same_directory(&directory_after) {
        return Err(AcquisitionError::SourceChangedDuringAcquisition);
    }
    Ok(())
}

#[cfg(unix)]
fn hash_directory_at(
    root: &File,
    display_root: &Path,
    filesystem: &SnapshotFilesystem,
) -> Result<blake3::Hash, AcquisitionError> {
    let mut hasher = blake3::Hasher::new();
    let mut limits = SnapshotLimits::default();
    hash_directory_contents(
        root,
        display_root,
        Path::new(""),
        filesystem,
        &mut limits,
        &mut hasher,
    )?;
    Ok(hasher.finalize())
}

#[cfg(unix)]
fn hash_directory_contents(
    directory: &File,
    display_root: &Path,
    relative: &Path,
    filesystem: &SnapshotFilesystem,
    limits: &mut SnapshotLimits,
    hasher: &mut blake3::Hasher,
) -> Result<(), AcquisitionError> {
    let directory_before = unix_metadata(directory)
        .map_err(|error| snapshot_error("inspect hashed snapshot directory", error))?;
    for name in directory_entries(directory)? {
        limits.entries += 1;
        if limits.entries > MAX_SNAPSHOT_ENTRIES {
            return Err(AcquisitionError::SnapshotLimitExceeded);
        }
        let child_relative = relative.join(&name);
        let display_path = display_root.join(&child_relative);
        let before = metadata_at(directory, &name)
            .map_err(|error| snapshot_error("inspect hashed snapshot entry", error))?;
        if before.dev != filesystem.device {
            return Err(AcquisitionError::SourceFilesystemBoundary(display_path));
        }
        hasher.update(os_str_bytes(child_relative.as_os_str()).as_ref());
        match before.kind() {
            libc::S_IFDIR => {
                hasher.update(b"\0directory\0");
                let child = open_directory_at(directory, &name)
                    .map_err(|error| snapshot_error("open hashed snapshot directory", error))?;
                let opened = unix_metadata(&child)
                    .map_err(|error| snapshot_error("inspect hashed snapshot directory", error))?;
                if !before.same_object(&opened) {
                    return Err(AcquisitionError::SourceChangedDuringAcquisition);
                }
                if snapshot_mount_identity(&child)? != filesystem.mount {
                    return Err(AcquisitionError::SourceFilesystemBoundary(display_path));
                }
                hash_directory_contents(
                    &child,
                    display_root,
                    &child_relative,
                    filesystem,
                    limits,
                    hasher,
                )?;
            }
            libc::S_IFREG => {
                if before.nlink > 1 {
                    return Err(AcquisitionError::HardlinkedSourceEntry(display_path));
                }
                hasher.update(if before.mode & 0o111 != 0 {
                    b"\0executable\0"
                } else {
                    b"\0file\0"
                });
                let remaining = MAX_SNAPSHOT_BYTES.saturating_sub(limits.bytes);
                let mut file = open_file_at(directory, &name, libc::O_RDONLY | libc::O_NONBLOCK)
                    .map_err(|error| snapshot_error("open hashed snapshot file", error))?;
                let opened = unix_metadata(&file)
                    .map_err(|error| snapshot_error("inspect hashed snapshot file", error))?;
                if !before.same_file(&opened) {
                    return Err(AcquisitionError::SourceChangedDuringAcquisition);
                }
                if snapshot_mount_identity(&file)? != filesystem.mount {
                    return Err(AcquisitionError::SourceFilesystemBoundary(display_path));
                }
                let copied = io::copy(
                    &mut Read::by_ref(&mut file).take(remaining.saturating_add(1)),
                    hasher,
                )
                .map_err(|error| snapshot_error("hash repository snapshot file", error))?;
                if copied > remaining {
                    return Err(AcquisitionError::SnapshotLimitExceeded);
                }
                limits.bytes += copied;
                let after = unix_metadata(&file)
                    .map_err(|error| snapshot_error("reinspect hashed snapshot file", error))?;
                if !before.same_file(&after) {
                    return Err(AcquisitionError::SourceChangedDuringAcquisition);
                }
            }
            libc::S_IFLNK => {
                return Err(AcquisitionError::SymlinkPath {
                    kind: "snapshot source",
                    path: display_path,
                });
            }
            _ => return Err(AcquisitionError::UnsupportedSourceEntry(display_path)),
        }
    }
    let directory_after = unix_metadata(directory)
        .map_err(|error| snapshot_error("reinspect hashed snapshot directory", error))?;
    if !directory_before.same_directory(&directory_after) {
        return Err(AcquisitionError::SourceChangedDuringAcquisition);
    }
    Ok(())
}

#[cfg(unix)]
fn target_dir_path(relative: &Path) -> PathBuf {
    Path::new("<snapshot-target>").join(relative)
}

#[cfg(unix)]
fn open_absolute_directory(path: &Path, kind: &'static str) -> Result<File, AcquisitionError> {
    use std::os::fd::FromRawFd;

    validate_lexical_path(path, kind)?;
    // SAFETY: open receives a valid static C string.
    let descriptor = unsafe {
        libc::open(
            c"/".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(io_error("open filesystem root", io::Error::last_os_error()));
    }
    // SAFETY: descriptor is newly opened and uniquely owned.
    let mut directory = unsafe { File::from_raw_fd(descriptor) };
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_directory_at(&directory, name).map_err(|error| {
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::ELOOP) | Some(libc::ENOTDIR)
                    ) {
                        AcquisitionError::SymlinkPath {
                            kind,
                            path: path.to_path_buf(),
                        }
                    } else {
                        io_error("open path component", error)
                    }
                })?;
            }
            _ => {
                return Err(AcquisitionError::UnsafePath {
                    kind,
                    path: path.to_path_buf(),
                });
            }
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_directory_at(directory: &File, name: &OsStr) -> io::Result<File> {
    open_file_at(directory, name, libc::O_RDONLY | libc::O_DIRECTORY)
}

#[cfg(unix)]
fn open_file_at(directory: &File, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
    use std::{
        os::fd::{AsRawFd, FromRawFd},
        os::unix::ffi::OsStrExt,
    };

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: openat receives valid descriptors and the successful descriptor is owned by File.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: descriptor is newly opened and uniquely owned.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

#[cfg(unix)]
fn open_relative_file(root: &File, path: &Path) -> io::Result<File> {
    let mut directory = root.try_clone()?;
    let mut components = path.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path is not root relative",
            ));
        };
        if components.peek().is_some() {
            directory = open_directory_at(&directory, name)?;
        } else {
            return open_file_at(&directory, name, libc::O_RDONLY | libc::O_NONBLOCK);
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidInput, "path is empty"))
}

#[cfg(not(unix))]
fn open_relative_file(_root: &File, _path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor-relative files are unavailable",
    ))
}

#[cfg(unix)]
fn create_file_at(directory: &File, name: &OsStr, mode: libc::mode_t) -> io::Result<File> {
    use std::{
        os::fd::{AsRawFd, FromRawFd},
        os::unix::ffi::OsStrExt,
    };

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: openat receives valid arguments and the successful descriptor is owned by File.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            libc::c_uint::from(mode),
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: descriptor is newly opened and uniquely owned.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

#[cfg(unix)]
fn mkdir_at(directory: &File, name: &OsStr, mode: libc::mode_t) -> io::Result<()> {
    use std::{os::fd::AsRawFd, os::unix::ffi::OsStrExt};

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: mkdirat receives a valid directory descriptor and C string.
    if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), mode) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn directory_entries(directory: &File) -> Result<Vec<std::ffi::OsString>, AcquisitionError> {
    directory_entries_bounded(
        directory,
        MAX_SNAPSHOT_ENTRIES,
        MAX_SNAPSHOT_NAME_BYTES,
        None,
    )
}

#[cfg(unix)]
fn directory_entries_bounded(
    directory: &File,
    max_entries: usize,
    max_name_bytes: usize,
    deadline: Option<Instant>,
) -> Result<Vec<std::ffi::OsString>, AcquisitionError> {
    use std::{
        os::fd::{FromRawFd, IntoRawFd},
        os::unix::ffi::{OsStrExt, OsStringExt},
    };

    let iterator = open_directory_at(directory, OsStr::new("."))
        .map_err(|error| snapshot_error("open repository snapshot directory", error))?;
    let descriptor = iterator.into_raw_fd();
    // SAFETY: descriptor is uniquely transferred to DIR and closed by closedir below.
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        // SAFETY: fdopendir did not take ownership after failure.
        drop(unsafe { File::from_raw_fd(descriptor) });
        return Err(snapshot_error(
            "enumerate repository snapshot directory",
            io::Error::last_os_error(),
        ));
    }
    let mut entries = Vec::new();
    let mut name_bytes = 0_usize;
    loop {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            // SAFETY: stream was returned by fdopendir and has not been closed.
            let _ = unsafe { libc::closedir(stream) };
            return Err(AcquisitionError::CommandTimedOut(
                "validate Git object packs",
            ));
        }
        clear_errno();
        // SAFETY: stream remains valid until closed below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            if let Some(error) = current_errno() {
                // SAFETY: stream was returned by fdopendir and has not been closed.
                let _ = unsafe { libc::closedir(stream) };
                return Err(snapshot_error("enumerate repository directory", error));
            }
            break;
        }
        // SAFETY: d_name is NUL-terminated for a valid dirent returned by readdir.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            if entries.len() == max_entries {
                // SAFETY: stream was returned by fdopendir and has not been closed.
                let _ = unsafe { libc::closedir(stream) };
                return Err(AcquisitionError::SnapshotLimitExceeded);
            }
            let Some(next_name_bytes) = name_bytes
                .checked_add(name.len())
                .filter(|bytes| *bytes <= max_name_bytes)
            else {
                // SAFETY: stream was returned by fdopendir and has not been closed.
                let _ = unsafe { libc::closedir(stream) };
                return Err(AcquisitionError::SnapshotLimitExceeded);
            };
            name_bytes = next_name_bytes;
            entries.push(std::ffi::OsString::from_vec(name.to_vec()));
        }
    }
    // SAFETY: stream was returned by fdopendir and has not been closed.
    let close_result = unsafe { libc::closedir(stream) };
    if close_result != 0 {
        return Err(snapshot_error(
            "close repository snapshot directory",
            io::Error::last_os_error(),
        ));
    }
    entries.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(entries)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn clear_errno() {
    // SAFETY: __error returns this thread's errno storage.
    unsafe { *libc::__error() = 0 };
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn current_errno() -> Option<io::Error> {
    // SAFETY: __error returns this thread's errno storage.
    let errno = unsafe { *libc::__error() };
    (errno != 0).then(|| io::Error::from_raw_os_error(errno))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn clear_errno() {
    // SAFETY: __errno_location returns this thread's errno storage.
    unsafe { *libc::__errno_location() = 0 };
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn current_errno() -> Option<io::Error> {
    // SAFETY: __errno_location returns this thread's errno storage.
    let errno = unsafe { *libc::__errno_location() };
    (errno != 0).then(|| io::Error::from_raw_os_error(errno))
}

#[cfg(all(
    unix,
    not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "linux",
        target_os = "android"
    ))
))]
fn clear_errno() {}

#[cfg(all(
    unix,
    not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "linux",
        target_os = "android"
    ))
))]
fn current_errno() -> Option<io::Error> {
    None
}

#[cfg(unix)]
fn metadata_at(directory: &File, name: &OsStr) -> io::Result<UnixMetadata> {
    use std::{mem::MaybeUninit, os::fd::AsRawFd, os::unix::ffi::OsStrExt};

    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata points to writable storage and all other arguments are valid.
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized metadata.
    Ok(unix_metadata_from_stat(unsafe { metadata.assume_init() }))
}

#[cfg(unix)]
fn unix_metadata(file: &File) -> io::Result<UnixMetadata> {
    use std::{mem::MaybeUninit, os::fd::AsRawFd};

    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata points to writable storage and file owns a valid descriptor.
    if unsafe { libc::fstat(file.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstat initialized metadata.
    Ok(unix_metadata_from_stat(unsafe { metadata.assume_init() }))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn snapshot_mount_identity(file: &File) -> Result<SnapshotMountIdentity, AcquisitionError> {
    use std::{mem::MaybeUninit, os::fd::AsRawFd};

    const STATX_MNT_ID: u32 = 0x1000;
    #[repr(C)]
    struct StatxTimestamp {
        _seconds: i64,
        _nanoseconds: u32,
        _reserved: i32,
    }
    #[repr(C)]
    struct Statx {
        mask: u32,
        _block_size: u32,
        _attributes: u64,
        _link_count: u32,
        _uid: u32,
        _gid: u32,
        _mode: u16,
        _reserved: u16,
        _inode: u64,
        _size: u64,
        _blocks: u64,
        _attributes_mask: u64,
        _access_time: StatxTimestamp,
        _birth_time: StatxTimestamp,
        _change_time: StatxTimestamp,
        _modification_time: StatxTimestamp,
        _device_major: u32,
        _device_minor: u32,
        _source_device_major: u32,
        _source_device_minor: u32,
        mount_id: u64,
        _direct_io_memory_alignment: u32,
        _direct_io_offset_alignment: u32,
        _spare: [u64; 12],
    }

    let mut statx = MaybeUninit::<Statx>::zeroed();
    // SAFETY: statx receives a valid descriptor, an empty C path with AT_EMPTY_PATH,
    // and writable storage matching the kernel statx layout.
    let result = unsafe {
        libc::syscall(
            libc::SYS_statx,
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            STATX_MNT_ID,
            statx.as_mut_ptr(),
        )
    };
    if result != 0 {
        return Err(AcquisitionError::Unavailable {
            capability: "descriptor-relative snapshot mount identity",
        });
    }
    // SAFETY: successful statx initialized the output structure.
    let statx = unsafe { statx.assume_init() };
    if statx.mask & STATX_MNT_ID == 0 {
        return Err(AcquisitionError::Unavailable {
            capability: "descriptor-relative snapshot mount identity",
        });
    }
    Ok(SnapshotMountIdentity::Linux(statx.mount_id))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn snapshot_mount_identity(file: &File) -> Result<SnapshotMountIdentity, AcquisitionError> {
    use std::{mem::MaybeUninit, os::fd::AsRawFd};

    let mut statfs = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs receives a live descriptor and writable statfs storage.
    if unsafe { libc::fstatfs(file.as_raw_fd(), statfs.as_mut_ptr()) } != 0 {
        return Err(AcquisitionError::Unavailable {
            capability: "descriptor-relative snapshot mount identity",
        });
    }
    // SAFETY: successful fstatfs initialized the output structure.
    let statfs = unsafe { statfs.assume_init() };
    let mount_point = statfs
        .f_mntonname
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    Ok(SnapshotMountIdentity::Apple(mount_point))
}

#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))
))]
fn snapshot_mount_identity(_file: &File) -> Result<SnapshotMountIdentity, AcquisitionError> {
    Err(AcquisitionError::Unavailable {
        capability: "descriptor-relative snapshot mount identity",
    })
}

#[cfg(all(unix, any(target_os = "macos", target_os = "ios")))]
fn unix_metadata_from_stat(metadata: libc::stat) -> UnixMetadata {
    UnixMetadata {
        dev: unsigned_metadata_number(metadata.st_dev),
        ino: metadata.st_ino,
        nlink: unsigned_metadata_number(metadata.st_nlink),
        mode: metadata.st_mode,
        size: metadata.st_size as u64,
        modified_seconds: metadata.st_mtime,
        modified_nanoseconds: metadata.st_mtime_nsec,
        changed_seconds: metadata.st_ctime,
        changed_nanoseconds: metadata.st_ctime_nsec,
    }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
fn unix_metadata_from_stat(metadata: libc::stat) -> UnixMetadata {
    UnixMetadata {
        dev: unsigned_metadata_number(metadata.st_dev),
        ino: metadata.st_ino,
        nlink: unsigned_metadata_number(metadata.st_nlink),
        mode: metadata.st_mode,
        size: metadata.st_size as u64,
        modified_seconds: metadata.st_mtime,
        modified_nanoseconds: metadata.st_mtime_nsec,
        changed_seconds: metadata.st_ctime,
        changed_nanoseconds: metadata.st_ctime_nsec,
    }
}

#[cfg(unix)]
fn unsigned_metadata_number<T: TryInto<u64>>(value: T) -> u64
where
    T::Error: fmt::Debug,
{
    value.try_into().expect("filesystem device number fits u64")
}

#[cfg(unix)]
fn permission_mode<T: Into<u32>>(value: T) -> u32 {
    value.into()
}

#[cfg(unix)]
fn snapshot_error(operation: &'static str, source: io::Error) -> AcquisitionError {
    if matches!(
        source.raw_os_error(),
        Some(libc::ENOENT)
            | Some(libc::ENOTDIR)
            | Some(libc::ELOOP)
            | Some(libc::ESTALE)
            | Some(libc::EINVAL)
    ) {
        AcquisitionError::SourceChangedDuringAcquisition
    } else {
        io_error(operation, source)
    }
}

fn sanitize_snapshot_git_metadata(repository: &Path) -> Result<(), AcquisitionError> {
    let git_dir = repository.join(".git");
    for relative in [
        "config",
        "config.worktree",
        "commondir",
        "hooks",
        "objects/info/alternates",
    ] {
        remove_snapshot_entry(&git_dir.join(relative))?;
    }
    Ok(())
}

fn remove_snapshot_entry(path: &Path) -> Result<(), AcquisitionError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error("inspect snapshot Git metadata", source)),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .map_err(|source| io_error("sanitize snapshot Git metadata", source))
}

fn clear_snapshot_worktree(repository: &Path) -> Result<(), AcquisitionError> {
    for entry in
        fs::read_dir(repository).map_err(|source| io_error("read snapshot worktree", source))?
    {
        let entry = entry.map_err(|source| io_error("read snapshot worktree entry", source))?;
        if entry.file_name() == OsStr::new(".git") {
            continue;
        }
        remove_snapshot_entry(&entry.path())?;
    }
    Ok(())
}

fn reset_snapshot_to_base(repository: &Path, base_commit: &str) -> Result<(), AcquisitionError> {
    git_success(
        repository,
        "reset snapshot to base",
        [
            OsStr::new("reset"),
            OsStr::new("--hard"),
            OsStr::new(base_commit),
        ],
    )
}

fn validate_snapshot_entries(repository: &Path) -> Result<(), AcquisitionError> {
    let listed = git_output(
        repository,
        "validate snapshot paths",
        [
            OsStr::new("ls-files"),
            OsStr::new("--cached"),
            OsStr::new("--others"),
            OsStr::new("--exclude-standard"),
            OsStr::new("-z"),
        ],
    )?;
    for raw_path in nul_paths(&listed.stdout) {
        let relative = validated_git_path(&raw_path, "validate snapshot path")?;
        let path = repository.join(relative);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(AcquisitionError::UnsupportedSourceEntry(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error("inspect snapshot path", source)),
        }
    }
    Ok(())
}

fn remove_ignored_snapshot_entries(repository: &Path) -> Result<(), AcquisitionError> {
    let ignored = git_output(
        repository,
        "list ignored snapshot paths",
        [
            OsStr::new("ls-files"),
            OsStr::new("--others"),
            OsStr::new("--ignored"),
            OsStr::new("--exclude-standard"),
            OsStr::new("--directory"),
            OsStr::new("-z"),
        ],
    )?;
    for raw_path in nul_paths(&ignored.stdout) {
        let relative = validated_git_path(&raw_path, "remove ignored snapshot path")?;
        remove_snapshot_entry(&repository.join(relative))?;
    }
    Ok(())
}

fn reject_split_index(source: &Path) -> Result<(), AcquisitionError> {
    let shared_index = git_text(
        source,
        "resolve shared index",
        ["rev-parse", "--shared-index-path"],
    )?;
    if shared_index.is_empty() {
        return Ok(());
    }
    let path = PathBuf::from(shared_index);
    Err(AcquisitionError::UnsupportedIndexState {
        path: if path.is_absolute() {
            path
        } else {
            source.join(path)
        },
        reason: "split index",
    })
}

fn reserve_directory(
    root: &Path,
    workspace: &WorkspaceId,
    owner: &OwnerId,
    marker_mode: &str,
) -> Result<ReservedDirectory, AcquisitionError> {
    for _ in 0..128 {
        let id = new_acquisition_id()?;
        let path = root.join(format!(
            "{}-{}-{}",
            workspace.as_str(),
            owner.as_str(),
            id.as_str()
        ));
        let mut directory = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        match directory.create(&path) {
            Ok(()) => {
                let marker = marker_bytes(&id, owner, marker_mode);
                let marker_path = path.join(MARKER_NAME);
                let mut options = OpenOptions::new();
                options.create_new(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut marker_file = match options.open(&marker_path) {
                    Ok(file) => file,
                    Err(source) => {
                        if let Err(cleanup) = fs::remove_dir(&path) {
                            return Err(AcquisitionError::CleanupAfterAcquisition {
                                acquisition: Box::new(io_error("create workspace marker", source)),
                                cleanup: Box::new(io_error(
                                    "clean failed workspace reservation",
                                    cleanup,
                                )),
                            });
                        }
                        return Err(io_error("create workspace marker", source));
                    }
                };
                if let Err(source) = marker_file.write_all(&marker) {
                    drop(marker_file);
                    let _ = fs::remove_file(&marker_path);
                    fs::remove_dir(&path).map_err(|cleanup| {
                        io_error("clean failed workspace reservation", cleanup)
                    })?;
                    return Err(io_error("create workspace marker", source));
                }
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|source| io_error("inspect workspace reservation", source))?;
                return Ok(ReservedDirectory {
                    path,
                    id,
                    identity: filesystem_identity(&metadata),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(io_error("reserve workspace target", source)),
        }
    }
    Err(AcquisitionError::AtomicReservationExhausted)
}

fn new_acquisition_id() -> Result<AcquisitionId, AcquisitionError> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random).map_err(|error| AcquisitionError::Io {
        operation: "generate acquisition identity",
        source: io::Error::other(error.to_string()),
    })?;
    Ok(AcquisitionId(hex(&random)))
}

fn marker_bytes(id: &AcquisitionId, owner: &OwnerId, mode: &str) -> Vec<u8> {
    format!(
        "{MARKER_VERSION}\n{}\n{}\n{mode}\n",
        id.as_str(),
        owner.as_str()
    )
    .into_bytes()
}

fn verify_marker(
    allocation: &Path,
    id: &AcquisitionId,
    owner: &OwnerId,
    mode: &str,
) -> Result<(), AcquisitionError> {
    let marker = allocation.join(MARKER_NAME);
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(AcquisitionError::MarkerMissing);
        }
        Err(source) => return Err(io_error("inspect workspace marker", source)),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 1024 {
        return Err(AcquisitionError::MarkerMismatch);
    }
    let mut file =
        open_read_no_follow(&marker).map_err(|source| io_error("open workspace marker", source))?;
    let opened = file
        .metadata()
        .map_err(|source| io_error("inspect open workspace marker", source))?;
    if !same_filesystem_object(&metadata, &opened) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    let mut actual = Vec::new();
    file.read_to_end(&mut actual)
        .map_err(|source| io_error("read workspace marker", source))?;
    let current = fs::symlink_metadata(&marker)
        .map_err(|source| io_error("reinspect workspace marker", source))?;
    if !same_filesystem_object(&metadata, &current) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    if actual != marker_bytes(id, owner, mode) {
        return Err(AcquisitionError::MarkerMismatch);
    }
    Ok(())
}

fn validate_cleanup_boundary(
    root: &Path,
    allocation: &Path,
    workspace: &Path,
) -> Result<(), AcquisitionError> {
    let root = canonical_directory(root, "managed root")?;
    if allocation.parent() != Some(root.as_path()) || workspace != allocation.join("repo") {
        return Err(AcquisitionError::UnsafePath {
            kind: "cleanup target",
            path: workspace.to_path_buf(),
        });
    }
    if allocation.exists() {
        let metadata = fs::symlink_metadata(allocation)
            .map_err(|source| io_error("inspect cleanup target", source))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(AcquisitionError::SymlinkPath {
                kind: "cleanup target",
                path: allocation.to_path_buf(),
            });
        }
        let canonical = fs::canonicalize(allocation)
            .map_err(|source| io_error("canonicalize cleanup target", source))?;
        if canonical.parent() != Some(root.as_path()) {
            return Err(AcquisitionError::UnsafePath {
                kind: "cleanup target",
                path: workspace.to_path_buf(),
            });
        }
        if workspace.exists() {
            let metadata = fs::symlink_metadata(workspace)
                .map_err(|source| io_error("inspect workspace path", source))?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(AcquisitionError::SymlinkPath {
                    kind: "workspace",
                    path: workspace.to_path_buf(),
                });
            }
        }
    }
    Ok(())
}

fn remove_marked_directory(
    root: &Path,
    allocation: &Path,
    id: &AcquisitionId,
    owner: &OwnerId,
    mode: &str,
    expected_identity: Option<&FilesystemIdentity>,
) -> Result<CleanupOutcome, AcquisitionError> {
    let Some(quarantine) =
        quarantine_marked_directory(root, allocation, id, owner, mode, expected_identity)?
    else {
        return Ok(CleanupOutcome::AlreadyAbsent);
    };
    remove_quarantined_directory(&quarantine, "remove quarantined target")?;
    Ok(CleanupOutcome::Removed)
}

fn cleanup_failed_acquisition(
    root: &Path,
    allocation: &Path,
    id: &AcquisitionId,
    owner: &OwnerId,
    mode: &str,
    expected_identity: Option<&FilesystemIdentity>,
    worktree: Option<(&Path, &Path)>,
) -> Result<CleanupOutcome, AcquisitionError> {
    let Some(quarantine) =
        quarantine_marked_directory(root, allocation, id, owner, mode, expected_identity)?
    else {
        return Ok(CleanupOutcome::AlreadyAbsent);
    };
    if let Some((source, target)) = worktree
        && let Err(error) = remove_worktree(source, target)
    {
        fs::rename(&quarantine, allocation)
            .map_err(|source| io_error("restore workspace after cleanup failure", source))?;
        return Err(error);
    }
    remove_quarantined_directory(&quarantine, "remove quarantined workspace")?;
    Ok(CleanupOutcome::Removed)
}

fn remove_quarantined_directory(
    quarantine: &Path,
    operation: &'static str,
) -> Result<(), AcquisitionError> {
    fs::remove_dir_all(quarantine).map_err(|source| io_error(operation, source))
}

fn quarantine_marked_directory(
    root: &Path,
    allocation: &Path,
    id: &AcquisitionId,
    owner: &OwnerId,
    mode: &str,
    expected_identity: Option<&FilesystemIdentity>,
) -> Result<Option<PathBuf>, AcquisitionError> {
    let root = canonical_directory(root, "managed root")?;
    if allocation.parent() != Some(root.as_path()) {
        return Err(AcquisitionError::UnsafePath {
            kind: "cleanup target",
            path: allocation.to_path_buf(),
        });
    }
    let quarantine = root.join(format!(".kit-quarantine-{}", id.as_str()));
    let before = match fs::symlink_metadata(allocation) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let quarantined = match fs::symlink_metadata(&quarantine) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(source) => return Err(io_error("inspect quarantined workspace", source)),
            };
            if !quarantined.is_dir() || quarantined.file_type().is_symlink() {
                return Err(AcquisitionError::SymlinkPath {
                    kind: "quarantined workspace",
                    path: quarantine,
                });
            }
            if expected_identity.is_none()
                || filesystem_identity(&quarantined).as_ref() != expected_identity
            {
                return Err(AcquisitionError::FilesystemIdentityChanged);
            }
            let canonical = fs::canonicalize(&quarantine)
                .map_err(|source| io_error("canonicalize quarantined workspace", source))?;
            if canonical.parent() != Some(root.as_path()) {
                return Err(AcquisitionError::UnsafePath {
                    kind: "quarantined workspace",
                    path: quarantine,
                });
            }
            return Ok(Some(quarantine));
        }
        Err(source) => return Err(io_error("inspect cleanup target", source)),
    };
    if !before.is_dir() || before.file_type().is_symlink() {
        return Err(AcquisitionError::SymlinkPath {
            kind: "cleanup target",
            path: allocation.to_path_buf(),
        });
    }
    if expected_identity.is_some() && filesystem_identity(&before).as_ref() != expected_identity {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    let canonical = fs::canonicalize(allocation)
        .map_err(|source| io_error("canonicalize cleanup target", source))?;
    if canonical.parent() != Some(root.as_path()) {
        return Err(AcquisitionError::UnsafePath {
            kind: "cleanup target",
            path: allocation.to_path_buf(),
        });
    }
    verify_marker(allocation, id, owner, mode)?;

    match fs::symlink_metadata(&quarantine) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => return Err(AcquisitionError::FilesystemIdentityChanged),
        Err(source) => return Err(io_error("inspect quarantine path", source)),
    }
    fs::rename(allocation, &quarantine)
        .map_err(|source| io_error("quarantine managed workspace", source))?;
    let after = fs::symlink_metadata(&quarantine)
        .map_err(|source| io_error("inspect quarantined workspace", source))?;
    if !after.is_dir() || after.file_type().is_symlink() || !same_filesystem_object(&before, &after)
    {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    verify_marker(&quarantine, id, owner, mode)?;
    Ok(Some(quarantine))
}

fn clone_without_checkout(source: &Path, target: &Path) -> Result<(), AcquisitionError> {
    git_success(
        source,
        "clone repository",
        [
            OsStr::new("clone"),
            OsStr::new("--no-local"),
            OsStr::new("--no-hardlinks"),
            OsStr::new("--no-checkout"),
            OsStr::new("--no-recurse-submodules"),
            OsStr::new("--"),
            source.as_os_str(),
            target.as_os_str(),
        ],
    )
}

fn checkout_base(target: &Path, base_commit: &str) -> Result<(), AcquisitionError> {
    git_success(
        target,
        "checkout base commit",
        [
            OsStr::new("checkout"),
            OsStr::new("--detach"),
            OsStr::new(base_commit),
            OsStr::new("--"),
        ],
    )
}

fn remove_worktree(source: &Path, target: &Path) -> Result<(), AcquisitionError> {
    let result = git_success(
        source,
        "remove managed worktree",
        [
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--force"),
            target.as_os_str(),
        ],
    );
    if result.is_ok() {
        return Ok(());
    }
    let listed = git_output(
        source,
        "list managed worktrees",
        [
            OsStr::new("worktree"),
            OsStr::new("list"),
            OsStr::new("--porcelain"),
            OsStr::new("-z"),
        ],
    )?;
    if listed.stdout.len() > MAX_COMMAND_OUTPUT {
        return Err(AcquisitionError::OutputTooLarge("Git worktree list"));
    }
    let target = os_str_bytes(target.as_os_str());
    if listed.stdout.split(|byte| *byte == 0).any(|line| {
        line.strip_prefix(b"worktree ")
            .is_some_and(|path| path == target.as_ref())
    }) {
        result
    } else {
        Ok(())
    }
}

fn copy_worktree(source: &Path, target: &Path) -> Result<(), AcquisitionError> {
    let listed = git_output(
        source,
        "list snapshot paths",
        [
            OsStr::new("ls-files"),
            OsStr::new("--cached"),
            OsStr::new("--others"),
            OsStr::new("--exclude-standard"),
            OsStr::new("-z"),
        ],
    )?;
    if listed.stdout.len() > MAX_COMMAND_OUTPUT {
        return Err(AcquisitionError::OutputTooLarge("Git snapshot paths"));
    }
    for raw_path in nul_paths(&listed.stdout).collect::<BTreeSet<_>>() {
        let relative = validated_git_path(&raw_path, "copy snapshot")?;
        let source_parents = snapshot_source_parents(source, &relative)?;
        let source_path = source.join(&relative);
        let metadata = match fs::symlink_metadata(&source_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => return Err(io_error("inspect snapshot source", source)),
        };
        let target_path = target.join(&relative);
        create_snapshot_parents(target, &relative)?;
        if metadata.is_file() {
            copy_regular_file(&source_path, &target_path, &metadata)?;
        } else if metadata.file_type().is_symlink() {
            copy_symlink(&source_path, &target_path, &metadata)?;
        } else {
            return Err(AcquisitionError::UnsupportedSourceEntry(source_path));
        }
        verify_snapshot_source_parents(&source_parents)?;
    }
    Ok(())
}

fn snapshot_source_parents(
    source: &Path,
    relative: &Path,
) -> Result<Vec<(PathBuf, Option<FilesystemIdentity>)>, AcquisitionError> {
    let mut path = source.to_path_buf();
    let source_metadata = fs::symlink_metadata(source)
        .map_err(|source| io_error("inspect snapshot source root", source))?;
    if !source_metadata.is_dir() || source_metadata.file_type().is_symlink() {
        return Err(AcquisitionError::SymlinkPath {
            kind: "snapshot source root",
            path,
        });
    }
    let mut parents = vec![(path.clone(), filesystem_identity(&source_metadata))];
    for component in relative.parent().into_iter().flat_map(Path::components) {
        let Component::Normal(name) = component else {
            return Err(AcquisitionError::UnsafePath {
                kind: "snapshot source parent",
                path: relative.to_path_buf(),
            });
        };
        path.push(name);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|source| io_error("inspect snapshot source parent", source))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(AcquisitionError::SymlinkPath {
                kind: "snapshot source parent",
                path,
            });
        }
        parents.push((path.clone(), filesystem_identity(&metadata)));
    }
    Ok(parents)
}

fn verify_snapshot_source_parents(
    parents: &[(PathBuf, Option<FilesystemIdentity>)],
) -> Result<(), AcquisitionError> {
    for (path, expected) in parents {
        let metadata = fs::symlink_metadata(path)
            .map_err(|source| io_error("reinspect snapshot source parent", source))?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || (expected.is_some() && filesystem_identity(&metadata).as_ref() != expected.as_ref())
        {
            return Err(AcquisitionError::FilesystemIdentityChanged);
        }
    }
    Ok(())
}

fn create_snapshot_parents(target: &Path, relative: &Path) -> Result<(), AcquisitionError> {
    let mut parent = target.to_path_buf();
    for component in relative.parent().into_iter().flat_map(Path::components) {
        let Component::Normal(name) = component else {
            return Err(AcquisitionError::UnsafePath {
                kind: "snapshot entry",
                path: relative.to_path_buf(),
            });
        };
        parent.push(name);
        match fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(AcquisitionError::SymlinkPath {
                    kind: "snapshot parent",
                    path: parent,
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&parent)
                .map_err(|source| io_error("create snapshot directory", source))?,
            Err(source) => return Err(io_error("inspect snapshot parent", source)),
        }
    }
    Ok(())
}

fn copy_regular_file(
    source: &Path,
    target: &Path,
    before: &fs::Metadata,
) -> Result<(), AcquisitionError> {
    let mut input = open_read_no_follow(source)
        .map_err(|source| io_error("open snapshot source file", source))?;
    let opened = input
        .metadata()
        .map_err(|source| io_error("inspect open snapshot source", source))?;
    if !opened.is_file() || !same_hashed_file_metadata(before, &opened) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    let mut output = options
        .open(target)
        .map_err(|source| io_error("create snapshot file", source))?;
    io::copy(&mut input, &mut output).map_err(|source| io_error("copy snapshot file", source))?;
    let after = fs::symlink_metadata(source)
        .map_err(|source| io_error("reinspect snapshot source", source))?;
    if !same_hashed_file_metadata(before, &after) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    fs::set_permissions(target, before.permissions())
        .map_err(|source| io_error("preserve snapshot permissions", source))?;
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(
    source: &Path,
    target: &Path,
    before: &fs::Metadata,
) -> Result<(), AcquisitionError> {
    let link = fs::read_link(source).map_err(|source| io_error("read source symlink", source))?;
    let after = fs::symlink_metadata(source)
        .map_err(|source| io_error("reinspect source symlink", source))?;
    if !same_filesystem_object(before, &after) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    std::os::unix::fs::symlink(link, target)
        .map_err(|source| io_error("copy source symlink", source))
}

#[cfg(windows)]
fn copy_symlink(
    source: &Path,
    target: &Path,
    before: &fs::Metadata,
) -> Result<(), AcquisitionError> {
    use std::os::windows::fs::FileTypeExt;

    let link = fs::read_link(source).map_err(|source| io_error("read source symlink", source))?;
    let after = fs::symlink_metadata(source)
        .map_err(|source| io_error("reinspect source symlink", source))?;
    if !same_filesystem_object(before, &after) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    if before.file_type().is_symlink_dir() {
        std::os::windows::fs::symlink_dir(link, target)
    } else {
        std::os::windows::fs::symlink_file(link, target)
    }
    .map_err(|source| io_error("copy source symlink", source))
}

fn copy_index(source: &Path, target: &Path) -> Result<(), AcquisitionError> {
    ensure_index_objects(source, target)?;
    let source_index = git_path(source, "index")?;
    let target_index = git_path(target, "index")?;
    let before = fs::symlink_metadata(&source_index)
        .map_err(|source| io_error("inspect source Git index", source))?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(AcquisitionError::UnsupportedSourceEntry(source_index));
    }
    let mut input = open_read_no_follow(&source_index)
        .map_err(|source| io_error("open source Git index", source))?;
    let opened = input
        .metadata()
        .map_err(|source| io_error("inspect open Git index", source))?;
    if !same_filesystem_object(&before, &opened) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    if fs::symlink_metadata(&target_index).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(AcquisitionError::SymlinkPath {
            kind: "snapshot index",
            path: target_index,
        });
    }
    let mut output = open_write_no_follow(&target_index)
        .map_err(|source| io_error("open snapshot Git index", source))?;
    io::copy(&mut input, &mut output).map_err(|source| io_error("copy Git index", source))?;
    let after = fs::symlink_metadata(&source_index)
        .map_err(|source| io_error("reinspect source Git index", source))?;
    if !same_filesystem_object(&before, &after) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    Ok(())
}

fn ensure_index_objects(source: &Path, target: &Path) -> Result<(), AcquisitionError> {
    let index = git_output(
        source,
        "list index objects",
        [
            OsStr::new("ls-files"),
            OsStr::new("--stage"),
            OsStr::new("-z"),
        ],
    )?;
    if index.stdout.len() > MAX_COMMAND_OUTPUT {
        return Err(AcquisitionError::OutputTooLarge("Git index"));
    }
    validate_staged_index(&index.stdout)?;
    let mut objects = BTreeSet::new();
    for entry in index.stdout.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let metadata = entry.split(|byte| *byte == b'\t').next().unwrap_or(entry);
        let Some(object) = metadata.split(|byte| *byte == b' ').nth(1) else {
            return Err(AcquisitionError::Git {
                operation: "list index objects",
                message: "Git returned a malformed index entry".into(),
            });
        };
        if !matches!(object.len(), 40 | 64) || !object.iter().all(u8::is_ascii_hexdigit) {
            return Err(AcquisitionError::Git {
                operation: "list index objects",
                message: "Git returned an invalid object identity".into(),
            });
        }
        objects.insert(object.to_vec());
    }

    let mut query = Vec::with_capacity(objects.len() * 65);
    for object in &objects {
        query.extend_from_slice(object);
        query.push(b'\n');
    }
    let checked = git_output_with_input(
        target,
        "check snapshot objects",
        [OsStr::new("cat-file"), OsStr::new("--batch-check")],
        query,
    )?;
    if checked.stdout.len() > MAX_COMMAND_OUTPUT {
        return Err(AcquisitionError::OutputTooLarge("Git object check"));
    }
    let missing = checked
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| line.ends_with(b" missing"))
        .filter_map(|line| line.split(|byte| *byte == b' ').next())
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    for object in missing {
        let object_text = std::str::from_utf8(&object).map_err(|_| AcquisitionError::Git {
            operation: "copy snapshot object",
            message: "Git returned a non-UTF-8 object identity".into(),
        })?;
        let blob = git_output(
            source,
            "read snapshot object",
            [
                OsStr::new("cat-file"),
                OsStr::new("blob"),
                OsStr::new(object_text),
            ],
        )?;
        if blob.stdout.len() > MAX_COMMAND_OUTPUT {
            return Err(AcquisitionError::OutputTooLarge("Git snapshot object"));
        }
        let written = git_output_with_input(
            target,
            "write snapshot object",
            [
                OsStr::new("hash-object"),
                OsStr::new("-w"),
                OsStr::new("--stdin"),
            ],
            blob.stdout,
        )?;
        if written.stdout.strip_suffix(b"\n") != Some(object.as_slice()) {
            return Err(AcquisitionError::SnapshotMismatch);
        }
    }
    Ok(())
}

fn git_path(repository: &Path, name: &str) -> Result<PathBuf, AcquisitionError> {
    let path = git_text(
        repository,
        "resolve Git path",
        ["rev-parse", "--git-path", name],
    )?;
    let path = PathBuf::from(path);
    Ok(if path.is_absolute() {
        path
    } else {
        repository.join(path)
    })
}

fn verify_git_metadata(
    source: &Path,
    workspace: &Path,
    shared_expected: bool,
) -> Result<GitMetadata, AcquisitionError> {
    let source_common = canonical_git_common_dir(source)?;
    let workspace_common = canonical_git_common_dir(workspace)?;
    if shared_expected {
        if source_common != workspace_common {
            return Err(AcquisitionError::SnapshotMismatch);
        }
        Ok(GitMetadata::SharedWithSource)
    } else {
        let canonical_workspace = fs::canonicalize(workspace)
            .map_err(|source| io_error("canonicalize workspace", source))?;
        if workspace_common == source_common || !workspace_common.starts_with(&canonical_workspace)
        {
            return Err(AcquisitionError::SnapshotMismatch);
        }
        Ok(GitMetadata::Independent)
    }
}

fn canonical_git_common_dir(repository: &Path) -> Result<PathBuf, AcquisitionError> {
    let value = git_text(
        repository,
        "resolve Git common directory",
        ["rev-parse", "--git-common-dir"],
    )?;
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        repository.join(path)
    };
    fs::canonicalize(path).map_err(|source| io_error("canonicalize Git common directory", source))
}

fn git_status(repository: &Path) -> Result<GitState, AcquisitionError> {
    let status = git_output(
        repository,
        "read status",
        [
            OsStr::new("status"),
            OsStr::new("--porcelain=v2"),
            OsStr::new("-z"),
            OsStr::new("--untracked-files=all"),
            OsStr::new("--ignore-submodules=none"),
        ],
    )?;
    let index = git_output(
        repository,
        "read index state",
        [
            OsStr::new("ls-files"),
            OsStr::new("--stage"),
            OsStr::new("-z"),
        ],
    )?;
    let changed = git_output(
        repository,
        "list changed files",
        [
            OsStr::new("diff"),
            OsStr::new("--name-only"),
            OsStr::new("-z"),
            OsStr::new("--no-ext-diff"),
            OsStr::new("--ignore-submodules=none"),
            OsStr::new("--"),
        ],
    )?;
    let untracked = git_output(
        repository,
        "list untracked files",
        [
            OsStr::new("ls-files"),
            OsStr::new("--others"),
            OsStr::new("--exclude-standard"),
            OsStr::new("-z"),
        ],
    )?;
    let index_flags = git_output(
        repository,
        "read index flags",
        [OsStr::new("ls-files"), OsStr::new("-v"), OsStr::new("-z")],
    )?;
    if [
        &status.stdout,
        &index.stdout,
        &changed.stdout,
        &untracked.stdout,
        &index_flags.stdout,
    ]
    .into_iter()
    .any(|value| value.len() > MAX_COMMAND_OUTPUT)
    {
        return Err(AcquisitionError::OutputTooLarge("Git status"));
    }
    validate_staged_index(&index.stdout)?;
    validate_index_flags(&index_flags.stdout)?;
    let dirty =
        !status.stdout.is_empty() || index_executable_state_changed(repository, &index.stdout)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"kit-git-dirty-state-v3\0status\0");
    hasher.update(&status.stdout);
    hasher.update(b"\0index\0");
    hasher.update(&index.stdout);
    hasher.update(b"\0index-flags\0");
    hasher.update(&index_flags.stdout);
    let paths = nul_paths(&changed.stdout)
        .chain(nul_paths(&untracked.stdout))
        .chain(index_paths(&index.stdout))
        .collect::<BTreeSet<_>>();
    for path in paths {
        hash_worktree_entry(repository, &path, &mut hasher)?;
    }
    Ok(GitState {
        hash: StateHash(format!("blake3:{}", hasher.finalize().to_hex())),
        dirty,
    })
}

fn index_paths(value: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
    value.split(|byte| *byte == 0).filter_map(|entry| {
        entry
            .iter()
            .position(|byte| *byte == b'\t')
            .map(|separator| entry[separator + 1..].to_vec())
    })
}

fn index_executable_state_changed(
    repository: &Path,
    value: &[u8],
) -> Result<bool, AcquisitionError> {
    for entry in value
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let Some(separator) = entry.iter().position(|byte| *byte == b'\t') else {
            return Err(malformed_index("missing path separator"));
        };
        let (metadata, raw_path) = (&entry[..separator], &entry[separator + 1..]);
        let Some(mode) = metadata.split(|byte| *byte == b' ').next() else {
            return Err(malformed_index("missing entry mode"));
        };
        if !matches!(mode, b"100644" | b"100755") {
            continue;
        }
        let path = repository.join(validated_git_path(raw_path, "read executable state")?);
        match fs::symlink_metadata(&path) {
            Ok(worktree) if worktree.is_file() => {
                if is_executable(&worktree) != (mode == b"100755") {
                    return Ok(true);
                }
            }
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
            Err(source) => return Err(io_error("inspect executable state", source)),
        }
    }
    Ok(false)
}

fn revision_hash(base_commit: &str, state: &StateHash) -> StateHash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"kit-workspace-revision-v1\0");
    hasher.update(base_commit.as_bytes());
    hasher.update(b"\0");
    hasher.update(state.as_str().as_bytes());
    StateHash(format!("blake3:{}", hasher.finalize().to_hex()))
}

fn nul_paths(value: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
    value
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
}

fn validate_staged_index(value: &[u8]) -> Result<(), AcquisitionError> {
    for entry in value
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let Some(separator) = entry.iter().position(|byte| *byte == b'\t') else {
            return Err(malformed_index("missing path separator"));
        };
        let (metadata, raw_path) = (&entry[..separator], &entry[separator + 1..]);
        let mut fields = metadata.split(|byte| *byte == b' ');
        let mode = fields.next().unwrap_or_default();
        let object = fields.next().unwrap_or_default();
        let stage = fields.next().unwrap_or_default();
        if fields.next().is_some() || mode.is_empty() || object.is_empty() || stage.is_empty() {
            return Err(malformed_index("invalid index metadata"));
        }
        let path = validated_git_path(raw_path, "validate index")?;
        if mode == b"160000" {
            return Err(AcquisitionError::UnsupportedGitlink(path));
        }
        if !matches!(mode, b"100644" | b"100755" | b"120000") {
            return Err(AcquisitionError::UnsupportedIndexState {
                path,
                reason: "unsupported entry mode",
            });
        }
        if stage != b"0" {
            return Err(AcquisitionError::UnsupportedIndexState {
                path,
                reason: "unresolved merge entry",
            });
        }
        if object.iter().all(|byte| *byte == b'0') {
            return Err(AcquisitionError::UnsupportedIndexState {
                path,
                reason: "intent-to-add entry",
            });
        }
    }
    Ok(())
}

fn validate_index_flags(value: &[u8]) -> Result<(), AcquisitionError> {
    for entry in value
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        if entry.len() < 3 || entry[1] != b' ' {
            return Err(malformed_index("invalid index flags"));
        }
        let path = validated_git_path(&entry[2..], "validate index flags")?;
        if entry[0] == b'S' || entry[0] == b's' {
            return Err(AcquisitionError::UnsupportedIndexState {
                path,
                reason: "skip-worktree entry",
            });
        }
        if entry[0].is_ascii_lowercase() {
            return Err(AcquisitionError::UnsupportedIndexState {
                path,
                reason: "assume-unchanged entry",
            });
        }
    }
    Ok(())
}

fn malformed_index(message: &str) -> AcquisitionError {
    AcquisitionError::Git {
        operation: "read index state",
        message: message.to_owned(),
    }
}

fn validated_git_path(
    raw_path: &[u8],
    operation: &'static str,
) -> Result<PathBuf, AcquisitionError> {
    let relative = path_from_git(raw_path)?;
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || relative
            .components()
            .next()
            .is_some_and(|component| component.as_os_str() == OsStr::new(".git"))
    {
        return Err(AcquisitionError::Git {
            operation,
            message: "Git returned an unsafe worktree path".into(),
        });
    }
    Ok(relative)
}

fn hash_worktree_entry(
    repository: &Path,
    raw_path: &[u8],
    hasher: &mut blake3::Hasher,
) -> Result<(), AcquisitionError> {
    let relative = validated_git_path(raw_path, "hash dirty state")?;
    hasher.update(b"\0path\0");
    hasher.update(raw_path);
    let path = repository.join(relative);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            hasher.update(b"\0missing");
            return Ok(());
        }
        Err(source) => return Err(io_error("inspect dirty worktree entry", source)),
    };
    if metadata.is_file() {
        hasher.update(if is_executable(&metadata) {
            b"\0file\0executable\0"
        } else {
            b"\0file\0regular\0"
        });
        let mut file =
            open_read_no_follow(&path).map_err(|source| io_error("open dirty file", source))?;
        let opened = file
            .metadata()
            .map_err(|source| io_error("inspect open dirty file", source))?;
        if !opened.is_file() || !same_hashed_file_metadata(&metadata, &opened) {
            return Err(AcquisitionError::FilesystemIdentityChanged);
        }
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|source| io_error("read dirty file", source))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let after = fs::symlink_metadata(&path)
            .map_err(|source| io_error("reinspect dirty file", source))?;
        if !same_hashed_file_metadata(&metadata, &after) {
            return Err(AcquisitionError::FilesystemIdentityChanged);
        }
    } else if metadata.file_type().is_symlink() {
        hasher.update(b"\0symlink\0");
        let target =
            fs::read_link(&path).map_err(|source| io_error("read dirty symlink", source))?;
        let after = fs::symlink_metadata(&path)
            .map_err(|source| io_error("reinspect dirty symlink", source))?;
        if !same_filesystem_object(&metadata, &after) {
            return Err(AcquisitionError::FilesystemIdentityChanged);
        }
        hasher.update(os_str_bytes(target.as_os_str()).as_ref());
    } else if metadata.is_dir() {
        hasher.update(b"\0directory");
    } else {
        return Err(AcquisitionError::UnsupportedSourceEntry(path));
    }
    Ok(())
}

#[cfg(unix)]
fn path_from_git(value: &[u8]) -> Result<PathBuf, AcquisitionError> {
    use std::os::unix::ffi::OsStrExt;
    Ok(PathBuf::from(OsStr::from_bytes(value)))
}

#[cfg(windows)]
fn path_from_git(value: &[u8]) -> Result<PathBuf, AcquisitionError> {
    String::from_utf8(value.to_vec())
        .map(PathBuf::from)
        .map_err(|_| AcquisitionError::Git {
            operation: "hash dirty state",
            message: "Git returned a non-UTF-8 Windows path".into(),
        })
}

#[cfg(unix)]
fn os_str_bytes(value: &OsStr) -> std::borrow::Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    std::borrow::Cow::Borrowed(value.as_bytes())
}

#[cfg(windows)]
fn os_str_bytes(value: &OsStr) -> std::borrow::Cow<'_, [u8]> {
    std::borrow::Cow::Owned(value.to_string_lossy().into_owned().into_bytes())
}

fn open_read_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    options.open(path)
}

fn open_write_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).write(true).truncate(true);
    configure_no_follow(&mut options);
    options.open(path)
}

fn configure_no_follow(options: &mut OpenOptions) {
    #[cfg(all(unix, any(target_os = "linux", target_os = "android")))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x20000);
    }
    #[cfg(all(
        unix,
        any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "netbsd",
            target_os = "dragonfly"
        )
    ))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x100);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
}

#[cfg(unix)]
fn filesystem_identity(metadata: &fs::Metadata) -> Option<FilesystemIdentity> {
    use std::os::unix::fs::MetadataExt;

    Some(FilesystemIdentity {
        first: metadata.dev(),
        second: metadata.ino(),
    })
}

#[cfg(windows)]
fn filesystem_identity(metadata: &fs::Metadata) -> Option<FilesystemIdentity> {
    use std::os::windows::fs::MetadataExt;

    Some(FilesystemIdentity {
        first: u64::from(metadata.volume_serial_number()?),
        second: metadata.file_index()?,
    })
}

#[cfg(not(any(unix, windows)))]
fn filesystem_identity(_metadata: &fs::Metadata) -> Option<FilesystemIdentity> {
    None
}

fn same_filesystem_object(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    match (filesystem_identity(left), filesystem_identity(right)) {
        (Some(left), Some(right)) => left == right,
        _ => {
            left.is_file() == right.is_file()
                && left.is_dir() == right.is_dir()
                && left.file_type().is_symlink() == right.file_type().is_symlink()
        }
    }
}

fn same_hashed_file_metadata(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_filesystem_object(left, right)
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && is_executable(left) == is_executable(right)
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    false
}

fn git_text<const N: usize>(
    repository: &Path,
    operation: &'static str,
    arguments: [&str; N],
) -> Result<String, AcquisitionError> {
    let output = git_output(repository, operation, arguments.map(OsStr::new))?;
    if output.stdout.len() > MAX_ERROR_OUTPUT {
        return Err(AcquisitionError::OutputTooLarge(operation));
    }
    let text = String::from_utf8(output.stdout).map_err(|_| AcquisitionError::Git {
        operation,
        message: "output was not UTF-8".into(),
    })?;
    Ok(text.trim_end_matches(['\r', '\n']).to_owned())
}

fn git_success<I, S>(
    repository: &Path,
    operation: &'static str,
    arguments: I,
) -> Result<(), AcquisitionError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_output(repository, operation, arguments).map(|_| ())
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum TrustedGitRead<'a> {
    Head,
    ObjectFormat,
    ShallowBoundaries,
    Commit {
        oid: &'a str,
    },
    Tree {
        oid: &'a str,
    },
    Blob {
        oid: &'a str,
    },
    Blame {
        head: &'a str,
        path: &'a RootRelativePath,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TrustedGitReadLimits {
    pub(crate) timeout: Duration,
    pub(crate) stdout_bytes: usize,
    pub(crate) stderr_bytes: usize,
}

#[cfg(unix)]
fn validate_trusted_git_repository(
    root: &File,
    repository: &Path,
    deadline: Instant,
) -> Result<TrustedGitRepositoryIdentity, AcquisitionError> {
    ensure_deadline(deadline, "validate Git repository")?;
    verify_open_directory(root, repository, "Git repository root")?;
    let dot_git = repository.join(".git");
    let metadata = fs::symlink_metadata(&dot_git)
        .map_err(|source| io_error("inspect Git metadata", source))?;
    let (git_dir, linked) = if metadata.is_dir() && !metadata.file_type().is_symlink() {
        (dot_git.clone(), false)
    } else if metadata.is_file() && !metadata.file_type().is_symlink() {
        let value = read_bounded_regular_file_before(
            &dot_git,
            MAX_GIT_METADATA_BYTES,
            "read Git metadata pointer",
            deadline,
        )?;
        let value = std::str::from_utf8(&value).map_err(|_| AcquisitionError::Unavailable {
            capability: "UTF-8 local Git metadata pointer",
        })?;
        let relative =
            value
                .trim()
                .strip_prefix("gitdir: ")
                .ok_or(AcquisitionError::Unavailable {
                    capability: "regular local Git metadata",
                })?;
        let canonical = fs::canonicalize(repository.join(relative))
            .map_err(|source| io_error("canonicalize Git metadata", source))?;
        let directory = open_absolute_directory(&canonical, "Git metadata")?;
        verify_open_directory(&directory, &canonical, "Git metadata")?;
        (canonical, true)
    } else {
        return Err(AcquisitionError::Unavailable {
            capability: "regular local Git metadata",
        });
    };
    let commondir = git_dir.join("commondir");
    let common_dir = match fs::symlink_metadata(&commondir) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(AcquisitionError::Unavailable {
                    capability: "regular local Git common directory pointer",
                });
            }
            let value = read_bounded_regular_file_before(
                &commondir,
                MAX_GIT_METADATA_BYTES,
                "read Git common directory",
                deadline,
            )?;
            let value = std::str::from_utf8(&value).map_err(|_| AcquisitionError::Unavailable {
                capability: "UTF-8 local Git common directory pointer",
            })?;
            let canonical = fs::canonicalize(git_dir.join(value.trim()))
                .map_err(|source| io_error("canonicalize Git common directory", source))?;
            let directory = open_absolute_directory(&canonical, "Git common directory")?;
            verify_open_directory(&directory, &canonical, "Git common directory")?;
            canonical
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && !linked => git_dir.clone(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(AcquisitionError::Unavailable {
                capability: "linked worktree common directory evidence",
            });
        }
        Err(source) => return Err(io_error("inspect Git common directory", source)),
    };
    let common = open_absolute_directory(&common_dir, "Git common directory")?;
    verify_open_directory(&common, &common_dir, "Git common directory")?;
    let mut policy_metadata = BTreeMap::new();
    if linked {
        record_policy_metadata(
            &mut policy_metadata,
            dot_git.clone(),
            unix_metadata_path(&dot_git)
                .map_err(|source| io_error("inspect Git metadata pointer", source))?,
        )?;
    }
    if common_dir != git_dir {
        record_policy_metadata(
            &mut policy_metadata,
            commondir.clone(),
            unix_metadata_path(&commondir)
                .map_err(|source| io_error("inspect Git common directory pointer", source))?,
        )?;
    }
    record_policy_metadata(
        &mut policy_metadata,
        git_dir.clone(),
        unix_metadata_path(&git_dir)
            .map_err(|source| io_error("inspect Git metadata directory", source))?,
    )?;
    record_policy_metadata(
        &mut policy_metadata,
        common_dir.clone(),
        unix_metadata_path(&common_dir)
            .map_err(|source| io_error("inspect Git common directory", source))?,
    )?;
    for path in [common_dir.join("config"), git_dir.join("config.worktree")] {
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                validate_git_config(&path, deadline)?;
                record_policy_metadata(
                    &mut policy_metadata,
                    path.clone(),
                    unix_metadata_path(&path)
                        .map_err(|source| io_error("inspect local Git configuration", source))?,
                )?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_error("inspect local Git configuration", source)),
        }
    }
    validate_git_objects(&common, &common_dir, deadline, &mut policy_metadata)?;
    ensure_deadline(deadline, "validate Git repository")?;
    verify_open_directory(root, repository, "Git repository root")?;
    let root_identity = root
        .metadata()
        .map_err(|source| io_error("inspect Git repository root", source))?;
    let git_identity = fs::symlink_metadata(&git_dir)
        .map_err(|source| io_error("inspect Git metadata directory", source))?;
    let common_identity = fs::symlink_metadata(&common_dir)
        .map_err(|source| io_error("inspect Git common directory", source))?;
    let fence_metadata = policy_metadata
        .iter()
        .filter(|(path, metadata)| {
            metadata.kind() == libc::S_IFDIR
                || matches!(
                    path.file_name().and_then(OsStr::to_str),
                    Some(".git" | "commondir" | "config" | "config.worktree")
                )
        })
        .map(|(path, metadata)| (path.clone(), *metadata))
        .collect();
    let identity = TrustedGitRepositoryIdentity {
        root: filesystem_identity(&root_identity).ok_or(AcquisitionError::Unavailable {
            capability: "Git repository filesystem identity",
        })?,
        git_dir: filesystem_identity(&git_identity).ok_or(AcquisitionError::Unavailable {
            capability: "Git metadata filesystem identity",
        })?,
        common_dir: filesystem_identity(&common_identity).ok_or(AcquisitionError::Unavailable {
            capability: "Git common directory filesystem identity",
        })?,
        policy_metadata,
        fence_metadata,
    };
    fence_trusted_git_repository(root, repository, &identity, deadline)?;
    Ok(identity)
}

#[cfg(not(unix))]
fn validate_trusted_git_repository(
    _root: &File,
    _repository: &Path,
    _deadline: Instant,
) -> Result<TrustedGitRepositoryIdentity, AcquisitionError> {
    Err(AcquisitionError::Unavailable {
        capability: "descriptor-relative trusted Git repository validation",
    })
}

fn validate_git_config(path: &Path, deadline: Instant) -> Result<(), AcquisitionError> {
    let bytes = read_bounded_regular_file_before(
        path,
        MAX_GIT_METADATA_BYTES,
        "read local Git configuration",
        deadline,
    )?;
    let text = std::str::from_utf8(&bytes).map_err(|_| AcquisitionError::Unavailable {
        capability: "UTF-8 local Git configuration",
    })?;
    for line in git_config_lines(text, deadline)? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            let Some(value) = line
                .strip_prefix('[')
                .and_then(|line| line.strip_suffix(']'))
            else {
                return Err(AcquisitionError::Unavailable {
                    capability: "parseable local Git configuration",
                });
            };
            let section = value
                .split(|character: char| character.is_whitespace() || character == '.')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if matches!(section.as_str(), "include" | "includeif") {
                return Err(AcquisitionError::Unavailable {
                    capability: "include-free local Git configuration",
                });
            }
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .map_or((line, None), |(key, value)| (key, Some(value.trim())));
        let key = key.trim().to_ascii_lowercase();
        if key.is_empty()
            || key.chars().any(char::is_whitespace)
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
        {
            return Err(AcquisitionError::Unavailable {
                capability: "parseable local Git configuration",
            });
        }
        let promisor = key == "promisor" || key.ends_with(".promisor");
        if key == "include.path"
            || (key.starts_with("includeif.") && key.ends_with(".path"))
            || (promisor && !value.is_some_and(git_config_false))
            || key == "partialclone"
            || key.ends_with(".partialclone")
            || key == "alternaterefscommand"
            || key.ends_with(".alternaterefscommand")
        {
            return Err(AcquisitionError::Unavailable {
                capability: "local non-promisor Git object storage",
            });
        }
    }
    Ok(())
}

fn git_config_false(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "false" | "no" | "off" | "0"
    )
}

fn git_config_lines(text: &str, deadline: Instant) -> Result<Vec<String>, AcquisitionError> {
    let malformed = || AcquisitionError::Unavailable {
        capability: "parseable local Git configuration",
    };
    let mut chars = text.chars().peekable();
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut quoted = false;
    let mut comment = false;
    while let Some(character) = chars.next() {
        ensure_deadline(deadline, "validate local Git configuration")?;
        if comment {
            if character == '\n' {
                lines.push(std::mem::take(&mut line));
                comment = false;
            }
            continue;
        }
        match character {
            '\n' => {
                if quoted {
                    return Err(malformed());
                }
                lines.push(std::mem::take(&mut line));
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '#' | ';' if !quoted => comment = true,
            '"' => quoted = !quoted,
            '\\' => match chars.next().ok_or_else(&malformed)? {
                '\n' => {}
                '\r' if chars.next() == Some('\n') => {}
                'n' => line.push('\n'),
                't' => line.push('\t'),
                'b' => line.push('\u{0008}'),
                '\\' => line.push('\\'),
                '"' => line.push('"'),
                _ => return Err(malformed()),
            },
            _ => line.push(character),
        }
    }
    if quoted {
        return Err(malformed());
    }
    if !line.is_empty() {
        lines.push(line);
    }
    Ok(lines)
}

#[cfg(unix)]
#[derive(Default)]
struct GitObjectLimits {
    entries: usize,
    name_bytes: usize,
}

#[cfg(unix)]
fn validate_git_objects(
    common: &File,
    common_path: &Path,
    deadline: Instant,
    policy_metadata: &mut BTreeMap<PathBuf, UnixMetadata>,
) -> Result<(), AcquisitionError> {
    use std::os::unix::ffi::OsStrExt;

    let before = metadata_at(common, OsStr::new("objects"))
        .map_err(|source| io_error("inspect Git objects directory", source))?;
    if before.kind() != libc::S_IFDIR {
        return Err(AcquisitionError::Unavailable {
            capability: "regular local Git objects directory",
        });
    }
    let objects = open_directory_at(common, OsStr::new("objects"))
        .map_err(|source| io_error("open Git objects directory", source))?;
    let opened = unix_metadata(&objects)
        .map_err(|source| io_error("inspect open Git objects directory", source))?;
    if !before.same_object(&opened) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    record_policy_metadata(policy_metadata, common_path.join("objects"), opened)?;
    let mount = snapshot_mount_identity(&objects)?;
    if snapshot_mount_identity(common)? != mount {
        return Err(AcquisitionError::SourceFilesystemBoundary(PathBuf::from(
            "objects",
        )));
    }
    let mut limits = GitObjectLimits::default();
    for name in git_object_entries(&objects, &mut limits, deadline)? {
        ensure_deadline(deadline, "validate Git objects")?;
        let bytes = name.as_bytes();
        if bytes == b"pack" {
            validate_git_object_directory(&objects, &name, &mount, deadline, |pack| {
                validate_git_metadata_tree(
                    pack,
                    &common_path.join("objects/pack"),
                    &mount,
                    &mut limits,
                    deadline,
                    0,
                    true,
                    policy_metadata,
                )
            })?;
        } else if bytes == b"info" {
            validate_git_object_directory(&objects, &name, &mount, deadline, |info| {
                validate_git_metadata_tree(
                    info,
                    &common_path.join("objects/info"),
                    &mount,
                    &mut limits,
                    deadline,
                    0,
                    false,
                    policy_metadata,
                )
            })?;
        } else if bytes.len() == 2 && bytes.iter().all(u8::is_ascii_hexdigit) {
            validate_git_object_directory(&objects, &name, &mount, deadline, |fanout| {
                let fanout_path = common_path.join("objects").join(&name);
                record_policy_metadata(
                    policy_metadata,
                    fanout_path.clone(),
                    unix_metadata(fanout)
                        .map_err(|source| io_error("inspect loose Git object directory", source))?,
                )?;
                for name in git_object_entries(fanout, &mut limits, deadline)? {
                    ensure_deadline(deadline, "validate loose Git objects")?;
                    let before = metadata_at(fanout, &name)
                        .map_err(|source| io_error("inspect loose Git object", source))?;
                    if before.kind() != libc::S_IFREG {
                        return Err(AcquisitionError::Unavailable {
                            capability: "regular local loose Git objects",
                        });
                    }
                    let file = open_file_at(fanout, &name, libc::O_RDONLY | libc::O_NONBLOCK)
                        .map_err(|source| io_error("open loose Git object", source))?;
                    let opened = unix_metadata(&file)
                        .map_err(|source| io_error("inspect open loose Git object", source))?;
                    if !before.same_object(&opened) {
                        return Err(AcquisitionError::FilesystemIdentityChanged);
                    }
                    if snapshot_mount_identity(&file)? != mount {
                        return Err(AcquisitionError::SourceFilesystemBoundary(
                            Path::new("objects").join(&name),
                        ));
                    }
                    record_policy_metadata(policy_metadata, fanout_path.join(&name), opened)?;
                }
                Ok(())
            })?;
        } else {
            return Err(AcquisitionError::Unavailable {
                capability: "regular local Git object storage",
            });
        }
    }
    Ok(())
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn validate_git_metadata_tree(
    directory: &File,
    directory_path: &Path,
    mount: &SnapshotMountIdentity,
    limits: &mut GitObjectLimits,
    deadline: Instant,
    depth: usize,
    reject_promisor: bool,
    policy_metadata: &mut BTreeMap<PathBuf, UnixMetadata>,
) -> Result<(), AcquisitionError> {
    if depth > 8 {
        return Err(AcquisitionError::SnapshotLimitExceeded);
    }
    record_policy_metadata(
        policy_metadata,
        directory_path.to_owned(),
        unix_metadata(directory)
            .map_err(|source| io_error("inspect Git object metadata directory", source))?,
    )?;
    for name in git_object_entries(directory, limits, deadline)? {
        ensure_deadline(deadline, "validate Git object metadata")?;
        if name == OsStr::new("alternates") {
            return Err(AcquisitionError::Unavailable {
                capability: "local non-promisor Git object storage",
            });
        }
        if reject_promisor && Path::new(&name).extension() == Some(OsStr::new("promisor")) {
            return Err(AcquisitionError::Unavailable {
                capability: "local non-promisor Git object storage",
            });
        }
        let path = directory_path.join(&name);
        let before = metadata_at(directory, &name)
            .map_err(|source| io_error("inspect Git object metadata entry", source))?;
        match before.kind() {
            libc::S_IFDIR => {
                let child = open_directory_at(directory, &name)
                    .map_err(|source| io_error("open Git object metadata directory", source))?;
                let opened = unix_metadata(&child).map_err(|source| {
                    io_error("inspect open Git object metadata directory", source)
                })?;
                if !before.same_object(&opened) {
                    return Err(AcquisitionError::FilesystemIdentityChanged);
                }
                if snapshot_mount_identity(&child)? != *mount {
                    return Err(AcquisitionError::SourceFilesystemBoundary(path));
                }
                validate_git_metadata_tree(
                    &child,
                    &path,
                    mount,
                    limits,
                    deadline,
                    depth + 1,
                    reject_promisor,
                    policy_metadata,
                )?;
            }
            libc::S_IFREG => {
                let mut file = open_file_at(directory, &name, libc::O_RDONLY | libc::O_NONBLOCK)
                    .map_err(|source| io_error("open Git object metadata file", source))?;
                let opened = unix_metadata(&file)
                    .map_err(|source| io_error("inspect open Git object metadata file", source))?;
                if !before.same_object(&opened) {
                    return Err(AcquisitionError::FilesystemIdentityChanged);
                }
                if snapshot_mount_identity(&file)? != *mount {
                    return Err(AcquisitionError::SourceFilesystemBoundary(path));
                }
                let mut probe = [0_u8; 1];
                file.read(&mut probe)
                    .map_err(|source| io_error("read Git object metadata file", source))?;
                ensure_deadline(deadline, "validate Git object metadata")?;
                record_policy_metadata(policy_metadata, path, opened)?;
            }
            _ => {
                return Err(AcquisitionError::Unavailable {
                    capability: "regular local Git object metadata entries",
                });
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_git_object_directory(
    parent: &File,
    name: &OsStr,
    mount: &SnapshotMountIdentity,
    deadline: Instant,
    validate: impl FnOnce(&File) -> Result<(), AcquisitionError>,
) -> Result<(), AcquisitionError> {
    ensure_deadline(deadline, "validate Git objects")?;
    let before = metadata_at(parent, name)
        .map_err(|source| io_error("inspect Git object directory", source))?;
    if before.kind() != libc::S_IFDIR {
        return Err(AcquisitionError::Unavailable {
            capability: "regular local Git object directories",
        });
    }
    let directory = open_directory_at(parent, name)
        .map_err(|source| io_error("open Git object directory", source))?;
    let opened = unix_metadata(&directory)
        .map_err(|source| io_error("inspect open Git object directory", source))?;
    if !before.same_object(&opened) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    if snapshot_mount_identity(&directory)? != *mount {
        return Err(AcquisitionError::SourceFilesystemBoundary(
            Path::new("objects").join(name),
        ));
    }
    validate(&directory)
}

#[cfg(unix)]
fn git_object_entries(
    directory: &File,
    limits: &mut GitObjectLimits,
    deadline: Instant,
) -> Result<Vec<OsString>, AcquisitionError> {
    use std::os::unix::ffi::OsStrExt;

    let entries = directory_entries_bounded(
        directory,
        MAX_GIT_PACK_ENTRIES.saturating_sub(limits.entries),
        (MAX_GIT_METADATA_BYTES as usize).saturating_sub(limits.name_bytes),
        Some(deadline),
    )?;
    limits.entries = limits
        .entries
        .checked_add(entries.len())
        .ok_or(AcquisitionError::SnapshotLimitExceeded)?;
    limits.name_bytes = limits
        .name_bytes
        .checked_add(
            entries
                .iter()
                .map(|name| name.as_bytes().len())
                .sum::<usize>(),
        )
        .ok_or(AcquisitionError::SnapshotLimitExceeded)?;
    Ok(entries)
}

#[cfg(unix)]
fn record_policy_metadata(
    records: &mut BTreeMap<PathBuf, UnixMetadata>,
    path: PathBuf,
    metadata: UnixMetadata,
) -> Result<(), AcquisitionError> {
    if let Some(recorded) = records.get(&path) {
        if *recorded != metadata {
            return Err(AcquisitionError::FilesystemIdentityChanged);
        }
    } else {
        records.insert(path, metadata);
    }
    Ok(())
}

#[cfg(unix)]
fn unix_metadata_path(path: &Path) -> io::Result<UnixMetadata> {
    use std::{mem::MaybeUninit, os::unix::ffi::OsStrExt};

    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: lstat receives a valid C path and writable metadata storage.
    if unsafe { libc::lstat(path.as_ptr(), metadata.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful lstat initialized metadata.
    Ok(unix_metadata_from_stat(unsafe { metadata.assume_init() }))
}

fn ensure_deadline(deadline: Instant, operation: &'static str) -> Result<(), AcquisitionError> {
    if Instant::now() >= deadline {
        Err(AcquisitionError::CommandTimedOut(operation))
    } else {
        Ok(())
    }
}

fn read_bounded_regular_file_before(
    path: &Path,
    max_bytes: u64,
    operation: &'static str,
    deadline: Instant,
) -> Result<Vec<u8>, AcquisitionError> {
    ensure_deadline(deadline, operation)?;
    let before = fs::symlink_metadata(path).map_err(|source| io_error(operation, source))?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(AcquisitionError::Unavailable {
            capability: "regular bounded Git metadata file",
        });
    }
    if before.len() > max_bytes {
        return Err(AcquisitionError::OutputTooLarge(operation));
    }
    let mut file = open_read_no_follow(path).map_err(|source| io_error(operation, source))?;
    let opened = file
        .metadata()
        .map_err(|source| io_error(operation, source))?;
    if !opened.is_file() || opened.len() > max_bytes || !same_filesystem_object(&before, &opened) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    let capacity =
        usize::try_from(opened.len()).map_err(|_| AcquisitionError::OutputTooLarge(operation))?;
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| io_error(operation, source))?;
    if bytes.len() as u64 > max_bytes {
        return Err(AcquisitionError::OutputTooLarge(operation));
    }
    let after = file
        .metadata()
        .map_err(|source| io_error(operation, source))?;
    if !same_hashed_file_metadata(&opened, &after) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    ensure_deadline(deadline, operation)?;
    Ok(bytes)
}

fn read_bounded_open_file_before(
    mut file: File,
    max_bytes: u64,
    operation: &'static str,
    deadline: Instant,
) -> Result<Vec<u8>, AcquisitionError> {
    ensure_deadline(deadline, operation)?;
    let before = file
        .metadata()
        .map_err(|source| io_error(operation, source))?;
    if !before.is_file() || before.len() > max_bytes {
        return Err(AcquisitionError::OutputTooLarge(operation));
    }
    let capacity =
        usize::try_from(before.len()).map_err(|_| AcquisitionError::OutputTooLarge(operation))?;
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| io_error(operation, source))?;
    if bytes.len() as u64 > max_bytes {
        return Err(AcquisitionError::OutputTooLarge(operation));
    }
    let after = file
        .metadata()
        .map_err(|source| io_error(operation, source))?;
    if !same_hashed_file_metadata(&before, &after) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    ensure_deadline(deadline, operation)?;
    Ok(bytes)
}

fn verify_open_directory(
    directory: &File,
    path: &Path,
    operation: &'static str,
) -> Result<(), AcquisitionError> {
    let opened = directory
        .metadata()
        .map_err(|source| io_error(operation, source))?;
    let current = fs::symlink_metadata(path).map_err(|source| io_error(operation, source))?;
    if !opened.is_dir()
        || !current.is_dir()
        || current.file_type().is_symlink()
        || !same_filesystem_object(&opened, &current)
    {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    Ok(())
}

#[cfg(unix)]
fn fence_trusted_git_repository(
    root: &File,
    repository: &Path,
    identity: &TrustedGitRepositoryIdentity,
    deadline: Instant,
) -> Result<(), AcquisitionError> {
    ensure_deadline(deadline, "fence Git repository policy")?;
    verify_open_directory(root, repository, "Git repository root")?;
    let current_root = root
        .metadata()
        .map_err(|source| io_error("inspect Git repository root", source))?;
    if filesystem_identity(&current_root).as_ref() != Some(&identity.root) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    for (path, expected) in &identity.fence_metadata {
        ensure_deadline(deadline, "fence Git repository policy")?;
        let current = unix_metadata_path(path)
            .map_err(|source| io_error("inspect Git repository policy metadata", source))?;
        if current != *expected {
            return Err(AcquisitionError::FilesystemIdentityChanged);
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn fence_trusted_git_repository(
    _root: &File,
    _repository: &Path,
    _identity: &TrustedGitRepositoryIdentity,
    _deadline: Instant,
) -> Result<(), AcquisitionError> {
    Err(AcquisitionError::Unavailable {
        capability: "descriptor-relative trusted Git repository validation",
    })
}

pub(crate) fn begin_trusted_git_repository_policy_session(
    root: &File,
    repository: &Path,
    deadline: Instant,
) -> Result<TrustedGitRepositoryPolicySession, AcquisitionError> {
    let identity = validate_trusted_git_repository(root, repository, deadline)?;
    let repository = repository.to_owned();
    let implementation = capture_trusted_git_implementation(deadline)?;
    let logical_bytes =
        trusted_git_policy_session_logical_bytes(&repository, &identity, &implementation.identity);
    let validation_transient_bytes = policy_validation_transient_bytes(&identity);
    let scan_work = policy_scan_work(&identity);
    let consumed_work = scan_work
        .checked_add(implementation.streamed_bytes)
        .and_then(|work| work.checked_add(implementation.streamed_chunks))
        .and_then(|work| work.checked_add(1))
        .ok_or(AcquisitionError::SnapshotLimitExceeded)?;
    Ok(TrustedGitRepositoryPolicySession {
        root: root
            .try_clone()
            .map_err(|source| io_error("duplicate trusted Git repository root", source))?,
        repository,
        identity,
        implementation: implementation.identity,
        metrics: TrustedGitRepositoryPolicyMetrics {
            logical_bytes,
            peak_bytes: logical_bytes
                .checked_add(implementation.peak_bytes.max(validation_transient_bytes))
                .ok_or(AcquisitionError::SnapshotLimitExceeded)?,
            consumed_work,
            policy_scans: 1,
            fence_scans: 1,
            streamed_executable_bytes: implementation.streamed_bytes,
            streamed_executable_chunks: implementation.streamed_chunks,
            commands: 1,
            output_bytes: implementation.output_bytes,
        },
    })
}

pub(crate) const fn trusted_git_repository_policy_metrics(
    session: &TrustedGitRepositoryPolicySession,
) -> TrustedGitRepositoryPolicyMetrics {
    TrustedGitRepositoryPolicyMetrics {
        logical_bytes: session.logical_bytes(),
        peak_bytes: session.peak_bytes(),
        consumed_work: session.consumed_work(),
        ..session.metrics
    }
}

pub(crate) fn trusted_git_implementation_identity(
    session: &TrustedGitRepositoryPolicySession,
) -> &TrustedGitImplementationIdentity {
    &session.implementation
}

fn capture_trusted_git_implementation(
    deadline: Instant,
) -> Result<TrustedGitImplementationCapture, AcquisitionError> {
    let executable = trusted_git_executable()?;
    let (executable_digest, streamed_bytes, streamed_chunks) = hash_bounded_regular_file_before(
        &executable,
        MAX_GIT_EXECUTABLE_BYTES,
        "digest Git executable",
        deadline,
    )?;
    let mut command = Command::new(&executable);
    command.env_clear().env("LC_ALL", "C").arg("--version");
    let output = bounded_command_output_before(
        command,
        "identify Git version",
        None,
        deadline,
        4 * 1024,
        MAX_ERROR_OUTPUT,
    )?;
    let version = std::str::from_utf8(&output.stdout).map_err(|_| AcquisitionError::Git {
        operation: "identify Git version",
        message: "output was not UTF-8".into(),
    })?;
    let normalized = version.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Err(AcquisitionError::Git {
            operation: "identify Git version",
            message: "output was empty".into(),
        });
    }
    let output_bytes = output.stdout.len().saturating_add(output.stderr.len());
    let peak_bytes = GIT_EXECUTABLE_HASH_BUFFER_BYTES.max(
        output
            .stdout
            .capacity()
            .saturating_add(output.stderr.capacity()),
    );
    Ok(TrustedGitImplementationCapture {
        identity: TrustedGitImplementationIdentity {
            executable,
            executable_digest,
            version_digest: *blake3::hash(normalized.as_bytes()).as_bytes(),
        },
        streamed_bytes,
        streamed_chunks,
        output_bytes,
        peak_bytes,
    })
}

fn hash_bounded_regular_file_before(
    path: &Path,
    max_bytes: u64,
    operation: &'static str,
    deadline: Instant,
) -> Result<([u8; 32], usize, usize), AcquisitionError> {
    ensure_deadline(deadline, operation)?;
    let path_metadata = fs::symlink_metadata(path).map_err(|source| io_error(operation, source))?;
    if !path_metadata.is_file() || path_metadata.file_type().is_symlink() {
        return Err(AcquisitionError::Unavailable {
            capability: "regular bounded Git executable",
        });
    }
    let mut file = open_read_no_follow(path).map_err(|source| io_error(operation, source))?;
    let before = file
        .metadata()
        .map_err(|source| io_error(operation, source))?;
    if !before.is_file()
        || before.len() > max_bytes
        || !same_filesystem_object(&path_metadata, &before)
    {
        return Err(AcquisitionError::OutputTooLarge(operation));
    }
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0_u8; GIT_EXECUTABLE_HASH_BUFFER_BYTES];
    let mut total = 0_u64;
    let mut chunks = 0_usize;
    loop {
        ensure_deadline(deadline, operation)?;
        let read = file
            .read(&mut buffer)
            .map_err(|source| io_error(operation, source))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .filter(|total| *total <= max_bytes)
            .ok_or(AcquisitionError::OutputTooLarge(operation))?;
        chunks = chunks
            .checked_add(1)
            .ok_or(AcquisitionError::SnapshotLimitExceeded)?;
        hash.update(&buffer[..read]);
    }
    let after = file
        .metadata()
        .map_err(|source| io_error(operation, source))?;
    if total != before.len() || !same_hashed_file_metadata(&before, &after) {
        return Err(AcquisitionError::FilesystemIdentityChanged);
    }
    ensure_deadline(deadline, operation)?;
    Ok((
        *hash.finalize().as_bytes(),
        usize::try_from(total).map_err(|_| AcquisitionError::OutputTooLarge(operation))?,
        chunks,
    ))
}

pub(crate) fn finish_trusted_git_repository_policy_session(
    session: &mut TrustedGitRepositoryPolicySession,
    deadline: Instant,
) -> Result<(), AcquisitionError> {
    let final_identity =
        validate_trusted_git_repository(&session.root, &session.repository, deadline)?;
    let final_implementation = capture_trusted_git_implementation(deadline)?;
    let final_logical = trusted_git_policy_session_logical_bytes(
        &session.repository,
        &final_identity,
        &final_implementation.identity,
    );
    session.metrics.peak_bytes = session.metrics.peak_bytes.max(
        session
            .metrics
            .logical_bytes
            .saturating_add(final_logical)
            .saturating_add(
                final_implementation
                    .peak_bytes
                    .max(policy_validation_transient_bytes(&final_identity)),
            ),
    );
    session.metrics.policy_scans = session.metrics.policy_scans.saturating_add(1);
    session.metrics.fence_scans = session.metrics.fence_scans.saturating_add(1);
    session.metrics.streamed_executable_bytes = session
        .metrics
        .streamed_executable_bytes
        .saturating_add(final_implementation.streamed_bytes);
    session.metrics.streamed_executable_chunks = session
        .metrics
        .streamed_executable_chunks
        .saturating_add(final_implementation.streamed_chunks);
    session.metrics.commands = session.metrics.commands.saturating_add(1);
    session.metrics.output_bytes = session
        .metrics
        .output_bytes
        .saturating_add(final_implementation.output_bytes);
    session.metrics.consumed_work = session
        .metrics
        .consumed_work
        .saturating_add(policy_scan_work(&final_identity))
        .saturating_add(final_implementation.streamed_bytes)
        .saturating_add(final_implementation.streamed_chunks)
        .saturating_add(1);
    if final_identity == session.identity && final_implementation.identity == session.implementation
    {
        Ok(())
    } else {
        Err(AcquisitionError::FilesystemIdentityChanged)
    }
}

pub(crate) fn trusted_git_read(
    root: &File,
    repository: &Path,
    read: TrustedGitRead<'_>,
    limits: TrustedGitReadLimits,
) -> Result<CapturedOutput, AcquisitionError> {
    let deadline = Instant::now()
        .checked_add(limits.timeout)
        .ok_or(AcquisitionError::CommandTimedOut("validate Git repository"))?;
    let mut session = begin_trusted_git_repository_policy_session(root, repository, deadline)?;
    let output = trusted_git_read_before(&mut session, read, limits, deadline)?;
    finish_trusted_git_repository_policy_session(&mut session, deadline)?;
    Ok(output)
}

pub(crate) fn trusted_git_read_in_session(
    session: &mut TrustedGitRepositoryPolicySession,
    read: TrustedGitRead<'_>,
    limits: TrustedGitReadLimits,
) -> Result<CapturedOutput, AcquisitionError> {
    let deadline =
        Instant::now()
            .checked_add(limits.timeout)
            .ok_or(AcquisitionError::CommandTimedOut(
                "fence Git repository policy",
            ))?;
    trusted_git_read_before(session, read, limits, deadline)
}

fn trusted_git_read_before(
    session: &mut TrustedGitRepositoryPolicySession,
    read: TrustedGitRead<'_>,
    limits: TrustedGitReadLimits,
    deadline: Instant,
) -> Result<CapturedOutput, AcquisitionError> {
    fence_trusted_git_repository(
        &session.root,
        &session.repository,
        &session.identity,
        deadline,
    )?;
    let (operation, arguments) = trusted_git_read_arguments(read)?;
    let owned_root = session
        .root
        .try_clone()
        .map_err(|source| io_error("duplicate trusted Git repository root", source))?;
    let mut command = git_command_at(&session.repository, true, Some(owned_root))?;
    command.args(arguments);
    let output = bounded_command_output_before(
        command,
        operation,
        None,
        deadline,
        limits.stdout_bytes,
        limits.stderr_bytes,
    )?;
    if output.stdout.len() > limits.stdout_bytes || output.stderr.len() > limits.stderr_bytes {
        return Err(AcquisitionError::OutputTooLarge(operation));
    }
    let output_bytes = output.stdout.len().saturating_add(output.stderr.len());
    session.metrics.commands = session.metrics.commands.saturating_add(1);
    session.metrics.output_bytes = session.metrics.output_bytes.saturating_add(output_bytes);
    session.metrics.consumed_work = session
        .metrics
        .consumed_work
        .saturating_add(session.identity.fence_metadata.len().saturating_mul(2))
        .saturating_add(output_bytes)
        .saturating_add(1);
    session.metrics.fence_scans = session.metrics.fence_scans.saturating_add(2);
    session.metrics.peak_bytes = session.metrics.peak_bytes.max(
        session
            .metrics
            .logical_bytes
            .saturating_add(output.stdout.capacity())
            .saturating_add(output.stderr.capacity()),
    );
    let output = if matches!(read, TrustedGitRead::ShallowBoundaries) {
        shallow_boundaries(&session.root, output, limits, deadline)?
    } else {
        output
    };
    fence_trusted_git_repository(
        &session.root,
        &session.repository,
        &session.identity,
        deadline,
    )?;
    Ok(output)
}

#[cfg(unix)]
fn trusted_git_policy_session_logical_bytes(
    repository: &PathBuf,
    identity: &TrustedGitRepositoryIdentity,
    implementation: &TrustedGitImplementationIdentity,
) -> usize {
    std::mem::size_of::<TrustedGitRepositoryPolicySession>()
        .saturating_add(repository.capacity())
        .saturating_add(implementation.executable.capacity())
        .saturating_add(policy_map_logical_bytes(&identity.policy_metadata))
        .saturating_add(policy_map_logical_bytes(&identity.fence_metadata))
}

#[cfg(unix)]
fn policy_map_logical_bytes(records: &BTreeMap<PathBuf, UnixMetadata>) -> usize {
    records
        .keys()
        .map(|path| {
            BTREE_ENTRY_WEIGHT
                .saturating_add(std::mem::size_of::<PathBuf>())
                .saturating_add(std::mem::size_of::<UnixMetadata>())
                .saturating_add(path.capacity())
        })
        .sum()
}

#[cfg(not(unix))]
fn trusted_git_policy_session_logical_bytes(
    repository: &PathBuf,
    _identity: &TrustedGitRepositoryIdentity,
    implementation: &TrustedGitImplementationIdentity,
) -> usize {
    std::mem::size_of::<TrustedGitRepositoryPolicySession>()
        .saturating_add(repository.as_os_str().as_encoded_bytes().len())
        .saturating_add(implementation.executable.capacity())
}

#[cfg(unix)]
fn policy_scan_work(identity: &TrustedGitRepositoryIdentity) -> usize {
    identity
        .policy_metadata
        .len()
        .saturating_add(identity.fence_metadata.len())
}

#[cfg(unix)]
fn policy_validation_transient_bytes(identity: &TrustedGitRepositoryIdentity) -> usize {
    identity
        .policy_metadata
        .iter()
        .filter(|(path, metadata)| {
            metadata.kind() == libc::S_IFREG
                && matches!(
                    path.file_name().and_then(OsStr::to_str),
                    Some(".git" | "commondir" | "config" | "config.worktree")
                )
        })
        .filter_map(|(_, metadata)| usize::try_from(metadata.size).ok())
        .max()
        .unwrap_or(0)
        .saturating_mul(2)
}

#[cfg(not(unix))]
fn policy_scan_work(_identity: &TrustedGitRepositoryIdentity) -> usize {
    0
}

#[cfg(not(unix))]
fn policy_validation_transient_bytes(_identity: &TrustedGitRepositoryIdentity) -> usize {
    0
}

fn trusted_git_read_arguments(
    read: TrustedGitRead<'_>,
) -> Result<(&'static str, Vec<OsString>), AcquisitionError> {
    let (operation, fixed): (&'static str, &[&str]) = match read {
        TrustedGitRead::Head => ("resolve HEAD", &["rev-parse", "--verify", "HEAD^{commit}"]),
        TrustedGitRead::ObjectFormat => (
            "resolve object format",
            &["rev-parse", "--show-object-format"],
        ),
        TrustedGitRead::ShallowBoundaries => (
            "read shallow boundaries",
            &[
                "rev-parse",
                "--is-shallow-repository",
                "--git-path",
                "shallow",
            ],
        ),
        TrustedGitRead::Commit { .. } => ("read commit object", &["cat-file", "commit"]),
        TrustedGitRead::Tree { .. } => {
            ("read tree object", &["ls-tree", "-r", "-z", "--full-tree"])
        }
        TrustedGitRead::Blob { .. } => ("read blob object", &["cat-file", "blob"]),
        TrustedGitRead::Blame { .. } => (
            "read blame porcelain",
            &["blame", "--line-porcelain", "--root", "--no-textconv"],
        ),
    };
    let mut arguments = fixed.iter().map(OsString::from).collect::<Vec<_>>();
    match read {
        TrustedGitRead::Commit { oid }
        | TrustedGitRead::Tree { oid }
        | TrustedGitRead::Blob { oid } => {
            validate_git_oid(oid)?;
            arguments.push(oid.into());
        }
        TrustedGitRead::Blame { head, path } => {
            validate_git_oid(head)?;
            validate_git_path(path)?;
            arguments.push(head.into());
            arguments.push("--".into());
            arguments.push(path.as_str().into());
        }
        TrustedGitRead::Head | TrustedGitRead::ObjectFormat | TrustedGitRead::ShallowBoundaries => {
        }
    }
    Ok((operation, arguments))
}

fn validate_git_oid(value: &str) -> Result<(), AcquisitionError> {
    if matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(AcquisitionError::Git {
            operation: "validate read-only Git command",
            message: "invalid object ID".into(),
        })
    }
}

fn validate_git_path(path: &RootRelativePath) -> Result<(), AcquisitionError> {
    RootRelativePath::parse(path.as_str(), 256 * 1024)
        .map(|_| ())
        .map_err(|_| AcquisitionError::Git {
            operation: "validate read-only Git command",
            message: "invalid root-relative path".into(),
        })
}

fn shallow_boundaries(
    root: &File,
    output: CapturedOutput,
    limits: TrustedGitReadLimits,
    deadline: Instant,
) -> Result<CapturedOutput, AcquisitionError> {
    let text = std::str::from_utf8(&output.stdout).map_err(|_| AcquisitionError::Git {
        operation: "read shallow boundaries",
        message: "output was not UTF-8".into(),
    })?;
    let mut lines = text.lines();
    match lines.next() {
        Some("false") => {
            if lines.next().is_none() || lines.next().is_some() {
                return Err(AcquisitionError::Git {
                    operation: "read shallow boundaries",
                    message: "invalid shallow repository response".into(),
                });
            }
            Ok(CapturedOutput {
                stdout: Vec::new(),
                ..output
            })
        }
        Some("true") => {
            let path = lines.next().ok_or(AcquisitionError::Git {
                operation: "read shallow boundaries",
                message: "shallow path was absent".into(),
            })?;
            if lines.next().is_some() {
                return Err(AcquisitionError::CommandTimedOut("read shallow boundaries"));
            }
            ensure_deadline(deadline, "read shallow boundaries")?;
            let path = Path::new(path);
            let stdout = if path.is_absolute() {
                read_bounded_regular_file_before(
                    path,
                    (limits.stdout_bytes as u64).min(MAX_GIT_METADATA_BYTES),
                    "read shallow boundaries",
                    deadline,
                )?
            } else {
                let file = open_relative_file(root, path)
                    .map_err(|source| io_error("open shallow boundaries", source))?;
                read_bounded_open_file_before(
                    file,
                    (limits.stdout_bytes as u64).min(MAX_GIT_METADATA_BYTES),
                    "read shallow boundaries",
                    deadline,
                )?
            };
            if stdout.len() > limits.stdout_bytes {
                return Err(AcquisitionError::OutputTooLarge("read shallow boundaries"));
            }
            Ok(CapturedOutput { stdout, ..output })
        }
        _ => Err(AcquisitionError::Git {
            operation: "read shallow boundaries",
            message: "invalid shallow repository response".into(),
        }),
    }
}

fn git_output<I, S>(
    repository: &Path,
    operation: &'static str,
    arguments: I,
) -> Result<CapturedOutput, AcquisitionError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = git_command(repository, false)?;
    command.args(arguments);
    bounded_command_output(
        command,
        operation,
        None,
        GIT_TIMEOUT,
        MAX_COMMAND_OUTPUT,
        MAX_ERROR_OUTPUT,
    )
}

fn git_output_with_input<I, S>(
    repository: &Path,
    operation: &'static str,
    arguments: I,
    input: Vec<u8>,
) -> Result<CapturedOutput, AcquisitionError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = git_command(repository, false)?;
    command.args(arguments);
    bounded_command_output(
        command,
        operation,
        Some(input),
        GIT_TIMEOUT,
        MAX_COMMAND_OUTPUT,
        MAX_ERROR_OUTPUT,
    )
}

fn git_command(repository: &Path, offline: bool) -> Result<Command, AcquisitionError> {
    git_command_at(repository, offline, None)
}

fn git_command_at(
    repository: &Path,
    offline: bool,
    root: Option<File>,
) -> Result<Command, AcquisitionError> {
    let executable = trusted_git_executable()?;
    #[cfg(target_os = "macos")]
    let mut command = {
        let sandbox = verified_system_executable(Path::new("/usr/bin/sandbox-exec"))?;
        let mut command = Command::new(sandbox);
        command
            .args([
                "-p",
                "(version 1) (allow default) (deny process-fork)",
                "--",
            ])
            .arg(&executable);
        command
    };
    #[cfg(not(target_os = "macos"))]
    let mut command = Command::new(&executable);
    command
        .env_clear()
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("PAGER", "cat")
        .env("LC_ALL", "C")
        .arg("--no-replace-objects")
        .arg("-c")
        .arg(format!("core.hooksPath={}", null_device()))
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("core.quotePath=false")
        .arg("-c")
        .arg("core.untrackedCache=false")
        .arg("-c")
        .arg("submodule.recurse=false");
    if root.is_none() {
        command.current_dir(repository);
    }
    configure_working_directory(&mut command, root);
    if offline {
        command
            .env("GIT_NO_LAZY_FETCH", "1")
            .arg("-c")
            .arg("protocol.allow=never")
            .arg("-c")
            .arg("protocol.file.allow=never")
            .arg("-c")
            .arg("credential.helper=")
            .arg("-c")
            .arg("core.askPass=");
    }
    command.env(
        "PATH",
        executable
            .parent()
            .expect("verified Git executable has a parent"),
    );
    #[cfg(windows)]
    for name in ["SystemRoot", "WINDIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    Ok(command)
}

#[cfg(unix)]
fn configure_working_directory(command: &mut Command, root: Option<File>) {
    use std::{os::fd::AsRawFd, os::unix::process::CommandExt};

    let Some(root) = root else { return };
    let descriptor = root.as_raw_fd();
    // SAFETY: fchdir is async-signal-safe; the captured File keeps the descriptor live through
    // spawn and remains close-on-exec, so the descriptor cannot leak into Git.
    unsafe {
        command.pre_exec(move || {
            let _keep_alive = &root;
            if libc::fchdir(descriptor) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_working_directory(_command: &mut Command, _root: Option<File>) {}

#[cfg(unix)]
fn trusted_git_executable() -> Result<PathBuf, AcquisitionError> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const CANDIDATES: &[&str] = &["/usr/bin/git", "/bin/git"];
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    const CANDIDATES: &[&str] = &["/usr/bin/git"];
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    const CANDIDATES: &[&str] = &["/usr/bin/git", "/usr/local/bin/git"];

    CANDIDATES
        .iter()
        .find_map(|candidate| verified_system_executable(Path::new(candidate)).ok())
        .ok_or(AcquisitionError::Unavailable {
            capability: "an administrator-controlled non-user-writable system Git executable",
        })
}

#[cfg(not(unix))]
fn trusted_git_executable() -> Result<PathBuf, AcquisitionError> {
    Err(AcquisitionError::Unavailable {
        capability: "an administrator-controlled non-user-writable system Git executable",
    })
}

#[cfg(unix)]
fn verified_system_executable(path: &Path) -> Result<PathBuf, AcquisitionError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    if !path.is_absolute() {
        return Err(AcquisitionError::Unavailable {
            capability: "an absolute system executable path",
        });
    }
    let mut current = PathBuf::from("/");
    let root = fs::symlink_metadata(&current).map_err(|_| AcquisitionError::Unavailable {
        capability: "an administrator-controlled non-user-writable system Git executable",
    })?;
    if root.uid() != 0 || root.permissions().mode() & 0o022 != 0 || !root.is_dir() {
        return Err(AcquisitionError::Unavailable {
            capability: "an administrator-controlled non-user-writable system Git executable",
        });
    }
    for component in path.components().skip(1) {
        let Component::Normal(name) = component else {
            return Err(AcquisitionError::Unavailable {
                capability: "an administrator-controlled non-user-writable executable path",
            });
        };
        current.push(name);
        let metadata =
            fs::symlink_metadata(&current).map_err(|_| AcquisitionError::Unavailable {
                capability: "an administrator-controlled non-user-writable system Git executable",
            })?;
        if metadata.uid() != 0
            || metadata.permissions().mode() & 0o022 != 0
            || metadata.file_type().is_symlink()
        {
            return Err(AcquisitionError::Unavailable {
                capability: "an administrator-controlled non-user-writable system Git executable",
            });
        }
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| AcquisitionError::Unavailable {
        capability: "an administrator-controlled non-user-writable system Git executable",
    })?;
    if !metadata.is_file()
        || metadata.file_type().is_block_device()
        || metadata.file_type().is_char_device()
        || metadata.permissions().mode() & 0o111 == 0
    {
        return Err(AcquisitionError::Unavailable {
            capability: "an administrator-controlled non-user-writable system Git executable",
        });
    }
    Ok(path.to_path_buf())
}

#[derive(Debug)]
pub(crate) struct CapturedOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

fn bounded_command_output(
    command: Command,
    operation: &'static str,
    input: Option<Vec<u8>>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<CapturedOutput, AcquisitionError> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(AcquisitionError::CommandTimedOut(operation))?;
    bounded_command_output_before(
        command,
        operation,
        input,
        deadline,
        stdout_limit,
        stderr_limit,
    )
}

fn bounded_command_output_before(
    mut command: Command,
    operation: &'static str,
    input: Option<Vec<u8>>,
    deadline: Instant,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<CapturedOutput, AcquisitionError> {
    ensure_deadline(deadline, operation)?;
    ensure_bounded_git_runner()?;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_boundary(&mut command);
    let mut child = command
        .spawn()
        .map_err(|source| io_error("execute Git", source))?;
    let boundary = child.id();
    let stdout = child.stdout.take().ok_or_else(|| AcquisitionError::Git {
        operation,
        message: "Git stdout was unavailable".into(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| AcquisitionError::Git {
        operation,
        message: "Git stderr was unavailable".into(),
    })?;
    let exceeded = Arc::new(AtomicBool::new(false));
    let (finished_tx, finished_rx) = mpsc::channel();
    let stdout_reader = capture_stream(
        stdout,
        stdout_limit,
        Arc::clone(&exceeded),
        finished_tx.clone(),
    );
    let stderr_reader = capture_stream(
        stderr,
        stderr_limit,
        Arc::clone(&exceeded),
        finished_tx.clone(),
    );
    let writer = input.map(|input| {
        let mut stdin = child.stdin.take().expect("piped stdin was requested");
        thread::spawn(move || {
            let result = stdin.write_all(&input);
            let _ = finished_tx.send(());
            result
        })
    });
    let mut finished = 0;
    let expected = 2 + usize::from(writer.is_some());
    let mut status = None;
    while finished < expected || status.is_none() {
        if exceeded.load(Ordering::Acquire) {
            kill_and_reap_boundary(&mut child, boundary)?;
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            if let Some(writer) = writer {
                let _ = writer.join();
            }
            return Err(AcquisitionError::OutputTooLarge(operation));
        }
        if Instant::now() >= deadline {
            kill_and_reap_boundary(&mut child, boundary)?;
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            if let Some(writer) = writer {
                let _ = writer.join();
            }
            return Err(AcquisitionError::CommandTimedOut(operation));
        }
        if status.is_none() && leader_exited_without_reaping(&child)? {
            status = Some(terminate_exited_boundary(&mut child, boundary)?);
        }
        match finished_rx.recv_timeout(PROCESS_POLL_INTERVAL) {
            Ok(()) => finished += 1,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                thread::sleep(PROCESS_POLL_INTERVAL);
            }
        }
    }
    if exceeded.load(Ordering::Acquire) {
        kill_and_reap_boundary(&mut child, boundary)?;
        let _ = stdout_reader.join();
        let _ = stderr_reader.join();
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        return Err(AcquisitionError::OutputTooLarge(operation));
    }
    let status = status.expect("command loop waits for child exit");
    let stdout = stdout_reader.join().map_err(|_| AcquisitionError::Git {
        operation,
        message: "Git stdout reader failed".into(),
    })??;
    let stderr = stderr_reader.join().map_err(|_| AcquisitionError::Git {
        operation,
        message: "Git stderr reader failed".into(),
    })??;
    if let Some(writer) = writer {
        writer
            .join()
            .map_err(|_| AcquisitionError::Git {
                operation,
                message: "Git input writer failed".into(),
            })?
            .map_err(|source| io_error("write Git input", source))?;
    }
    finish_git_output(
        operation,
        CapturedOutput {
            status,
            stdout,
            stderr,
        },
    )
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn ensure_bounded_git_runner() -> Result<(), AcquisitionError> {
    if native_audit_arch().is_some() {
        Ok(())
    } else {
        Err(AcquisitionError::Unavailable {
            capability: "complete Git process containment",
        })
    }
}

#[cfg(target_os = "macos")]
fn ensure_bounded_git_runner() -> Result<(), AcquisitionError> {
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn ensure_bounded_git_runner() -> Result<(), AcquisitionError> {
    Err(AcquisitionError::Unavailable {
        capability: "complete Git process containment",
    })
}

fn capture_stream<R: Read + Send + 'static>(
    mut stream: R,
    limit: usize,
    exceeded: Arc<AtomicBool>,
    finished: mpsc::Sender<()>,
) -> thread::JoinHandle<Result<Vec<u8>, AcquisitionError>> {
    thread::spawn(move || {
        let result = (|| {
            let mut size = 0_usize;
            let mut output = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = stream
                    .read(&mut buffer)
                    .map_err(|source| io_error("read Git output", source))?;
                if read == 0 {
                    break;
                }
                let remaining = limit.saturating_sub(size);
                output.extend_from_slice(&buffer[..read.min(remaining)]);
                size += read;
                if read > remaining {
                    exceeded.store(true, Ordering::Release);
                    break;
                }
            }
            Ok(output)
        })();
        let _ = finished.send(());
        result
    })
}

#[cfg(unix)]
fn configure_process_boundary(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: setpgid is async-signal-safe and uses no Rust-managed state.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(any(target_os = "linux", target_os = "android"))]
            install_process_topology_filter()?;
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn configure_process_boundary(_command: &mut Command) {}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn install_process_topology_filter() -> io::Result<()> {
    let audit_arch = native_audit_arch()
        .ok_or_else(|| io::Error::other("unsupported Linux seccomp audit architecture"))?;
    let filters = process_topology_filters(audit_arch);
    let program = libc::sock_fprog {
        len: filters.len() as u16,
        filter: filters.as_ptr().cast_mut(),
    };
    // SAFETY: prctl is called with documented scalar values and a live filter program.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
        || unsafe {
            libc::prctl(
                libc::PR_SET_SECCOMP,
                libc::SECCOMP_MODE_FILTER,
                &program as *const libc::sock_fprog,
            )
        } != 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PROCESS_TOPOLOGY_FILTER_LEN: usize = 15;

#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    not(all(target_os = "linux", target_arch = "x86_64"))
))]
const PROCESS_TOPOLOGY_FILTER_LEN: usize = 13;

#[cfg(any(target_os = "linux", target_os = "android"))]
fn process_topology_filters(audit_arch: u32) -> [libc::sock_filter; PROCESS_TOPOLOGY_FILTER_LEN] {
    const LOAD_SYSCALL: u16 = 0x20;
    const JUMP_EQUAL: u16 = 0x15;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    const JUMP_BITS_SET: u16 = 0x45;
    const RETURN: u16 = 0x06;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;
    const ALLOW: u32 = 0x7fff_0000;
    const DENY: u32 = 0x0005_0000 | libc::EPERM as u32;
    const KILL_PROCESS: u32 = 0x8000_0000;

    [
        libc::sock_filter {
            code: LOAD_SYSCALL,
            jt: 0,
            jf: 0,
            k: 4,
        },
        libc::sock_filter {
            code: JUMP_EQUAL,
            jt: 1,
            jf: 0,
            k: audit_arch,
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: KILL_PROCESS,
        },
        libc::sock_filter {
            code: LOAD_SYSCALL,
            jt: 0,
            jf: 0,
            k: 0,
        },
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        libc::sock_filter {
            code: JUMP_BITS_SET,
            jt: 0,
            jf: 1,
            k: X32_SYSCALL_BIT,
        },
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: DENY,
        },
        libc::sock_filter {
            code: JUMP_EQUAL,
            jt: 0,
            jf: 1,
            k: libc::SYS_setsid as u32,
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: DENY,
        },
        libc::sock_filter {
            code: JUMP_EQUAL,
            jt: 0,
            jf: 1,
            k: libc::SYS_setpgid as u32,
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: DENY,
        },
        libc::sock_filter {
            code: JUMP_EQUAL,
            jt: 0,
            jf: 1,
            k: libc::SYS_unshare as u32,
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: DENY,
        },
        libc::sock_filter {
            code: JUMP_EQUAL,
            jt: 0,
            jf: 1,
            k: libc::SYS_setns as u32,
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: DENY,
        },
        libc::sock_filter {
            code: RETURN,
            jt: 0,
            jf: 0,
            k: ALLOW,
        },
    ]
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const fn native_audit_arch() -> Option<u32> {
    #[cfg(target_arch = "x86_64")]
    return Some(0xc000_003e);
    #[cfg(target_arch = "x86")]
    return Some(0x4000_0003);
    #[cfg(target_arch = "aarch64")]
    return Some(0xc000_00b7);
    #[cfg(target_arch = "arm")]
    return Some(0x4000_0028);
    #[cfg(target_arch = "riscv64")]
    return Some(0xc000_00f3);
    #[cfg(target_arch = "s390x")]
    return Some(0x8000_0016);
    #[cfg(all(target_arch = "powerpc64", target_endian = "big"))]
    return Some(0x8000_0015);
    #[cfg(all(target_arch = "powerpc64", target_endian = "little"))]
    return Some(0xc000_0015);
    #[cfg(target_arch = "loongarch64")]
    return Some(0xc000_0102);
    #[allow(unreachable_code)]
    None
}

#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn leader_exited_without_reaping(child: &std::process::Child) -> Result<bool, AcquisitionError> {
    use std::mem::MaybeUninit;

    let mut information = MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: waitid receives the live child PID and writable siginfo storage. WNOWAIT
    // leaves an exited leader unreaped so its PID still reserves the process-group ID.
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(io_error("inspect Git leader", io::Error::last_os_error()));
    }
    // SAFETY: successful waitid initialized siginfo; si_pid is zero when WNOHANG found no event.
    Ok(unsafe { information.assume_init().si_pid() } != 0)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn leader_exited_without_reaping(_child: &std::process::Child) -> Result<bool, AcquisitionError> {
    Err(AcquisitionError::Unavailable {
        capability: "non-reaping Git process inspection",
    })
}

fn kill_and_reap_boundary(
    child: &mut std::process::Child,
    boundary: u32,
) -> Result<(), AcquisitionError> {
    let mut kill_error = None;
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(-(boundary as i32), libc::SIGKILL) };
        let error = io::Error::last_os_error();
        if result != 0 && error.raw_os_error() != Some(libc::ESRCH) {
            #[cfg(target_os = "macos")]
            if error.raw_os_error() != Some(libc::EPERM) {
                kill_error = Some(error);
            }
            #[cfg(not(target_os = "macos"))]
            {
                kill_error = Some(error);
            }
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = child.kill() {
        kill_error = Some(error);
    }
    child
        .wait()
        .map_err(|source| io_error("reap Git process boundary", source))?;
    verify_boundary_empty(boundary)?;
    if let Some(error) = kill_error {
        Err(io_error("kill Git process boundary", error))
    } else {
        Ok(())
    }
}

fn terminate_exited_boundary(
    child: &mut std::process::Child,
    boundary: u32,
) -> Result<ExitStatus, AcquisitionError> {
    #[cfg(unix)]
    {
        terminate_exited_boundary_with(
            || {
                // SAFETY: the unreaped leader still reserves this process-group identity.
                let result = unsafe { libc::kill(-(boundary as i32), libc::SIGKILL) };
                let error = io::Error::last_os_error();
                if result == 0
                    || error.raw_os_error() == Some(libc::ESRCH)
                    || cfg!(target_os = "macos") && error.raw_os_error() == Some(libc::EPERM)
                {
                    Ok(())
                } else {
                    Err(io_error("terminate exited Git process boundary", error))
                }
            },
            || {
                child
                    .wait()
                    .map_err(|source| io_error("reap exited Git leader", source))
            },
            || verify_boundary_empty(boundary),
        )
    }
    #[cfg(not(unix))]
    {
        let _ = boundary;
        Err(AcquisitionError::Unavailable {
            capability: "complete Git process containment",
        })
    }
}

fn terminate_exited_boundary_with<T, E>(
    mut terminate: impl FnMut() -> Result<(), E>,
    mut reap: impl FnMut() -> Result<T, E>,
    mut inspect: impl FnMut() -> Result<(), E>,
) -> Result<T, E> {
    terminate()?;
    let status = reap()?;
    inspect()?;
    Ok(status)
}

fn verify_boundary_empty(boundary: u32) -> Result<(), AcquisitionError> {
    #[cfg(unix)]
    {
        for _ in 0..100 {
            // SAFETY: signal zero only probes the command's dedicated process group.
            if unsafe { libc::kill(-(boundary as i32), 0) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(());
                }
                #[cfg(target_os = "macos")]
                if error.raw_os_error() == Some(libc::EPERM) {
                    // Darwin reports EPERM while a killed process group is being dismantled.
                    thread::sleep(PROCESS_POLL_INTERVAL);
                    continue;
                }
                return Err(io_error("verify Git process boundary", error));
            }
            thread::sleep(PROCESS_POLL_INTERVAL);
        }
        Err(AcquisitionError::Unavailable {
            capability: "reaping all Git process descendants",
        })
    }
    #[cfg(not(unix))]
    {
        let _ = boundary;
        Err(AcquisitionError::Unavailable {
            capability: "complete Git process containment",
        })
    }
}

fn finish_git_output(
    operation: &'static str,
    output: CapturedOutput,
) -> Result<CapturedOutput, AcquisitionError> {
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(AcquisitionError::Git { operation, message });
    }
    Ok(output)
}

#[cfg(windows)]
fn null_device() -> &'static str {
    "NUL"
}

#[cfg(not(windows))]
fn null_device() -> &'static str {
    "/dev/null"
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(DIGITS[usize::from(byte >> 4)]));
        value.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    value
}

fn io_error(operation: &'static str, source: io::Error) -> AcquisitionError {
    AcquisitionError::Io { operation, source }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temporary_path(prefix: &str) -> PathBuf {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).unwrap();
        std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("{prefix}-{}", hex(&random)))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn evaluate_seccomp_filter(
        filters: &[libc::sock_filter],
        audit_arch: u32,
        syscall: u32,
    ) -> u32 {
        let mut accumulator = 0;
        let mut instruction = 0;
        loop {
            let filter = &filters[instruction];
            match filter.code {
                0x20 => {
                    accumulator = match filter.k {
                        0 => syscall,
                        4 => audit_arch,
                        offset => panic!("unexpected seccomp_data offset {offset}"),
                    };
                    instruction += 1;
                }
                0x15 => {
                    let offset = if accumulator == filter.k {
                        filter.jt
                    } else {
                        filter.jf
                    };
                    instruction += 1 + usize::from(offset);
                }
                0x45 => {
                    let offset = if accumulator & filter.k != 0 {
                        filter.jt
                    } else {
                        filter.jf
                    };
                    instruction += 1 + usize::from(offset);
                }
                0x06 => return filter.k,
                code => panic!("unexpected BPF instruction {code:#x}"),
            }
        }
    }

    #[test]
    fn hashed_file_metadata_detects_executable_mode_races() {
        let path = temporary_path("kit-mode-race");
        fs::write(&path, "same bytes\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let before = fs::symlink_metadata(&path).unwrap();

        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let after = fs::symlink_metadata(&path).unwrap();

        assert!(!same_hashed_file_metadata(&before, &after));
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn policy_accounting_bounds_large_metadata_maps_and_streamed_executables() {
        let executable = temporary_path("kit-large-executable");
        let bytes = vec![0x5a; GIT_EXECUTABLE_HASH_BUFFER_BYTES * 3 + 17];
        fs::write(&executable, &bytes).unwrap();
        let (_, streamed, chunks) = hash_bounded_regular_file_before(
            &executable,
            bytes.len() as u64,
            "test executable digest",
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(streamed, bytes.len());
        assert_eq!(chunks, 4);

        let metadata = UnixMetadata {
            dev: 1,
            ino: 1,
            nlink: 1,
            mode: libc::S_IFREG,
            size: 0,
            modified_seconds: 0,
            modified_nanoseconds: 0,
            changed_seconds: 0,
            changed_nanoseconds: 0,
        };
        let policy_metadata = (0..4_096)
            .map(|index| {
                (
                    PathBuf::from(format!("/repo/.git/objects/{index:04x}")),
                    metadata,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let fence_metadata = policy_metadata
            .iter()
            .take(128)
            .map(|(path, metadata)| (path.clone(), *metadata))
            .collect();
        let identity = TrustedGitRepositoryIdentity {
            root: FilesystemIdentity {
                first: 1,
                second: 1,
            },
            git_dir: FilesystemIdentity {
                first: 1,
                second: 2,
            },
            common_dir: FilesystemIdentity {
                first: 1,
                second: 2,
            },
            policy_metadata,
            fence_metadata,
        };
        let implementation = TrustedGitImplementationIdentity {
            executable: executable.clone(),
            executable_digest: [1; 32],
            version_digest: [2; 32],
        };
        let logical = trusted_git_policy_session_logical_bytes(
            &PathBuf::from("/repo"),
            &identity,
            &implementation,
        );
        assert!(
            logical
                > 4_096 * (std::mem::size_of::<UnixMetadata>() + std::mem::size_of::<PathBuf>())
        );
        assert!(policy_scan_work(&identity) >= 4_096 + 128);
        fs::remove_file(executable).unwrap();
    }

    #[test]
    fn git_executable_ignores_hostile_path() {
        if std::env::var_os("KIT_HOSTILE_PATH_CHILD").is_some() {
            let executable = trusted_git_executable().unwrap();
            assert!(executable.is_absolute());
            assert!(!executable.starts_with(std::env::var_os("PATH").unwrap()));
            return;
        }
        let directory = temporary_path("kit-hostile-path");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("git"), "#!/bin/sh\nexit 99\n").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace::acquire::tests::git_executable_ignores_hostile_path",
            ])
            .env("KIT_HOSTILE_PATH_CHILD", "1")
            .env("PATH", &directory)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    #[test]
    fn device_identity_rejects_mounted_snapshot_content() {
        let directory = temporary_path("kit-volume-crossing");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("file"), "bytes").unwrap();
        let root = open_absolute_directory(&directory, "test root").unwrap();
        let device = unix_metadata(&root).unwrap().dev;
        let filesystem = SnapshotFilesystem {
            device: device.wrapping_add(1),
            mount: snapshot_mount_identity(&root).unwrap(),
        };
        let result = hash_directory_at(&root, &directory, &filesystem);
        assert!(matches!(
            result,
            Err(AcquisitionError::SourceFilesystemBoundary(_))
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(all(
        unix,
        not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))
    ))]
    #[test]
    fn hostile_snapshot_mount_detection_is_unavailable_without_native_identity() {
        let directory = temporary_path("kit-mount-identity");
        fs::create_dir(&directory).unwrap();
        let root = open_absolute_directory(&directory, "test root").unwrap();
        assert!(matches!(
            snapshot_mount_identity(&root),
            Err(AcquisitionError::Unavailable {
                capability: "descriptor-relative snapshot mount identity"
            })
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    #[test]
    fn bounded_git_runner_is_unavailable_without_complete_containment() {
        assert!(matches!(
            ensure_bounded_git_runner(),
            Err(AcquisitionError::Unavailable {
                capability: "complete Git process containment"
            })
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn seccomp_filter_kills_a_mismatched_audit_arch_before_syscall_checks() {
        let arch = native_audit_arch().unwrap();
        let filters = process_topology_filters(arch);
        assert_eq!(
            evaluate_seccomp_filter(&filters, arch ^ 1, libc::SYS_getpid as u32),
            0x8000_0000
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn seccomp_filter_enforces_native_allowed_and_denied_matrix() {
        const ALLOW: u32 = 0x7fff_0000;
        const DENY: u32 = 0x0005_0000 | libc::EPERM as u32;

        let arch = native_audit_arch().unwrap();
        let filters = process_topology_filters(arch);
        for syscall in [
            libc::SYS_setsid,
            libc::SYS_setpgid,
            libc::SYS_unshare,
            libc::SYS_setns,
        ] {
            assert_eq!(
                evaluate_seccomp_filter(&filters, arch, syscall as u32),
                DENY,
                "native syscall {syscall} was allowed"
            );
        }
        for syscall in [libc::SYS_read, libc::SYS_getpid] {
            assert_eq!(
                evaluate_seccomp_filter(&filters, arch, syscall as u32),
                ALLOW,
                "native syscall {syscall} was denied"
            );
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn seccomp_filter_denies_x32_process_topology_syscalls() {
        const X32_SYSCALL_BIT: u32 = 0x4000_0000;
        const DENY: u32 = 0x0005_0000 | libc::EPERM as u32;

        let arch = native_audit_arch().unwrap();
        let filters = process_topology_filters(arch);
        for syscall in [
            libc::SYS_setsid,
            libc::SYS_setpgid,
            libc::SYS_unshare,
            libc::SYS_setns,
        ] {
            assert_eq!(
                evaluate_seccomp_filter(&filters, arch, syscall as u32 | X32_SYSCALL_BIT),
                DENY,
                "x32 syscall {syscall} was allowed"
            );
        }
    }

    #[test]
    fn exited_boundary_is_terminated_before_leader_reap_allows_pgid_reuse() {
        use std::cell::Cell;

        let leader_alive = Cell::new(true);
        let terminated = Cell::new(false);
        let status = terminate_exited_boundary_with(
            || {
                assert!(leader_alive.get(), "signaled a potentially reused PGID");
                terminated.set(true);
                Ok::<_, ()>(())
            },
            || {
                assert!(terminated.get());
                leader_alive.set(false);
                Ok::<_, ()>(7)
            },
            || {
                assert!(!leader_alive.get());
                Ok::<_, ()>(())
            },
        )
        .unwrap();
        assert_eq!(status, 7);
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn bounded_capture_kills_output_flood_boundary() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "while :; do printf 0123456789abcdef; done"]);
        let result = bounded_command_output(
            command,
            "output flood",
            None,
            Duration::from_secs(2),
            1024,
            1024,
        );
        assert!(
            matches!(
                &result,
                Err(AcquisitionError::OutputTooLarge("output flood"))
            ),
            "{result:?}"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn bounded_capture_times_out_and_kills_descendants() {
        let pid_file = temporary_path("kit-git-child");
        let script = format!(
            "sh -c 'trap \"\" TERM; while :; do sleep 1; done' & echo $! > '{}'; wait",
            pid_file.display()
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script]);
        let result = bounded_command_output(
            command,
            "hung command",
            None,
            Duration::from_millis(100),
            1024,
            1024,
        );
        assert!(
            matches!(
                &result,
                Err(AcquisitionError::CommandTimedOut("hung command"))
            ),
            "{result:?}"
        );
        let pid = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                fs::remove_file(pid_file).unwrap();
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("descendant {pid} survived command timeout");
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn bounded_capture_kills_background_descendants_after_success() {
        let pid_file = temporary_path("kit-git-success-child");
        let script = format!("sleep 30 & echo $! > '{}'; exit 0", pid_file.display());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script]);
        let output = bounded_command_output(
            command,
            "successful command",
            None,
            Duration::from_secs(2),
            1024,
            1024,
        )
        .unwrap();
        assert!(output.status.success());
        let pid = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        fs::remove_file(pid_file).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn process_boundary_denies_setsid_escape() {
        let pid_file = temporary_path("kit-git-setsid-child");
        let script = format!(
            "setsid /bin/sh -c 'sleep 30' >/dev/null 2>&1 & echo $! > '{}'; exit 0",
            pid_file.display()
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script]);
        bounded_command_output(
            command,
            "setsid escape",
            None,
            Duration::from_secs(2),
            1024,
            1024,
        )
        .unwrap();
        let pid = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        fs::remove_file(pid_file).unwrap();
    }
}
