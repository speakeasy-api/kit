use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    fmt,
    fs::File,
    mem::{size_of, size_of_val},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use crate::workspace::{
    acquire::{
        AcquisitionError, TrustedGitImplementationIdentity, TrustedGitRead, TrustedGitReadLimits,
        TrustedGitRepositoryPolicyMetrics, TrustedGitRepositoryPolicySession,
        begin_trusted_git_repository_policy_session, finish_trusted_git_repository_policy_session,
        trusted_git_implementation_identity, trusted_git_read, trusted_git_read_in_session,
        trusted_git_repository_policy_metrics,
    },
    edit::ir::RootRelativePath,
    index::meta::{ContentState, MetadataIndex},
    revision::{LimitKind, ManagedWorkspace, RevisionError, RevisionId},
};

use super::structure::{GraphError, GraphRange, StructureGraph};

/// `content_digest` covers canonical Git facts, digest-only blame evidence, request/options, and
/// the observed Git implementation identity. Workspace revision identity is snapshot-only.
pub const HISTORY_EXTRACTOR_POLICY: &str = "history-v3;commits=cat-file;trees=ls-tree-r-z;renames=unique-delete-add-same-blob-mode-exact-only-observed-partial;cochange=committed-non-root-non-merge-current-indexed-ranges;blame=line-porcelain-root-digest-only-requested-worktree;git-identity=executable-content+normalized-version";
pub const CHANGED_WITH_POLICY: &str = "non-root-non-merge-once-per-commit-v1";
const BTREE_ENTRY_WEIGHT: usize = size_of::<[usize; 8]>();
const HASH_ENTRY_WEIGHT: usize = 2 * size_of::<usize>();

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    const fn hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectId(String);

impl ObjectId {
    pub fn parse(value: &str, format: ObjectFormat) -> Result<Self, HistoryError> {
        if value.len() != format.hex_len()
            || !value
                .as_bytes()
                .iter()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(HistoryError::Malformed("invalid Git object ID"));
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) const fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitImplementationIdentity {
    executable: String,
    executable_digest: [u8; 32],
    version_digest: [u8; 32],
    digest: [u8; 32],
}

impl GitImplementationIdentity {
    fn from_trusted(identity: &TrustedGitImplementationIdentity) -> Self {
        Self::new(
            identity.executable.to_string_lossy().into_owned(),
            identity.executable_digest,
            identity.version_digest,
        )
    }

    fn fixture() -> Self {
        Self::new("fixture-git".to_owned(), [0x46; 32], [0x56; 32])
    }

    pub fn new(executable: String, executable_digest: [u8; 32], version_digest: [u8; 32]) -> Self {
        let mut hash = blake3::Hasher::new();
        hash.update(b"kit-git-implementation-v1\0");
        frame(&mut hash, executable.as_bytes());
        hash.update(&executable_digest);
        hash.update(&version_digest);
        Self {
            executable,
            executable_digest,
            version_digest,
            digest: *hash.finalize().as_bytes(),
        }
    }

    pub fn executable(&self) -> &str {
        &self.executable
    }
    pub const fn executable_digest(&self) -> [u8; 32] {
        self.executable_digest
    }
    pub const fn version_digest(&self) -> [u8; 32] {
        self.version_digest
    }
    pub const fn digest(&self) -> [u8; 32] {
        self.digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryRequest {
    scope: Vec<RootRelativePath>,
    blame_paths: Vec<RootRelativePath>,
    include_changed_with: bool,
}

impl HistoryRequest {
    pub fn new(
        mut scope: Vec<RootRelativePath>,
        mut blame_paths: Vec<RootRelativePath>,
        include_changed_with: bool,
    ) -> Self {
        scope.sort();
        scope.dedup();
        blame_paths.sort();
        blame_paths.dedup();
        Self {
            scope,
            blame_paths,
            include_changed_with,
        }
    }

    pub fn all(blame_paths: Vec<RootRelativePath>) -> Self {
        Self::new(Vec::new(), blame_paths, true)
    }

    pub fn blame_only(blame_paths: Vec<RootRelativePath>) -> Self {
        Self::new(Vec::new(), blame_paths, false)
    }

    pub fn scope(&self) -> &[RootRelativePath] {
        &self.scope
    }

    pub fn blame_paths(&self) -> &[RootRelativePath] {
        &self.blame_paths
    }

    pub const fn include_changed_with(&self) -> bool {
        self.include_changed_with
    }

    pub fn digest(&self) -> [u8; 32] {
        digest_request(self)
    }
}

impl Default for HistoryRequest {
    fn default() -> Self {
        Self::all(Vec::new())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryOptions {
    pub max_commands: usize,
    pub max_commits: usize,
    pub max_parents: usize,
    pub max_changes: usize,
    pub max_raw_changes: usize,
    pub max_paths: usize,
    pub max_renames: usize,
    pub max_blame_paths: usize,
    pub max_blame_lines: usize,
    pub max_output_bytes: usize,
    pub max_command_output_bytes: usize,
    pub max_error_output_bytes: usize,
    pub max_object_bytes: usize,
    pub max_provenance: usize,
    pub max_pairs: usize,
    pub max_nodes: usize,
    pub max_edges: usize,
    pub max_cache_entries: usize,
    pub max_cache_bytes: usize,
    pub max_staging_bytes: usize,
    pub max_work: usize,
    pub max_time: Duration,
}

impl Default for HistoryOptions {
    fn default() -> Self {
        Self {
            max_commands: 100_000,
            max_commits: 10_000,
            max_parents: 40_000,
            max_changes: 1_000_000,
            max_raw_changes: 2_000_000,
            max_paths: 250_000,
            max_renames: 250_000,
            max_blame_paths: 1_024,
            max_blame_lines: 2_000_000,
            max_output_bytes: 512 * 1024 * 1024,
            max_command_output_bytes: 64 * 1024 * 1024,
            max_error_output_bytes: 16 * 1024,
            max_object_bytes: 16 * 1024 * 1024,
            max_provenance: 1_000_000,
            max_pairs: 1_000_000,
            max_nodes: 250_000,
            max_edges: 1_000_000,
            max_cache_entries: 20_000,
            max_cache_bytes: 256 * 1024 * 1024,
            max_staging_bytes: 768 * 1024 * 1024,
            max_work: 100_000_000,
            max_time: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryBound {
    Commands,
    Commits,
    Parents,
    Changes,
    RawChanges,
    Paths,
    Renames,
    BlamePaths,
    BlameLines,
    OutputBytes,
    CommandOutputBytes,
    ObjectBytes,
    Provenance,
    Pairs,
    Nodes,
    Edges,
    CacheEntries,
    CacheBytes,
    StagingBytes,
    Work,
    Time,
}

#[derive(Debug)]
pub enum HistoryError {
    Revision(RevisionError),
    InvalidOptions(&'static str),
    InvalidIndex(&'static str),
    InvalidRequest(&'static str),
    SelectorNoMatch(RootRelativePath),
    Unavailable(&'static str),
    StaleRepositoryFence,
    Git {
        operation: &'static str,
        message: String,
    },
    Malformed(&'static str),
    MissingObject(ObjectId),
    RepositoryRootMismatch {
        expected: PathBuf,
        actual: PathBuf,
    },
    UnsafeGitPath(PathBuf),
    BoundExceeded(HistoryBound),
}

impl From<RevisionError> for HistoryError {
    fn from(value: RevisionError) -> Self {
        match value {
            RevisionError::LimitExceeded(LimitKind::Time) => {
                Self::BoundExceeded(HistoryBound::Time)
            }
            value => Self::Revision(value),
        }
    }
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Revision(error) => error.fmt(formatter),
            Self::InvalidOptions(reason) => write!(formatter, "invalid history options: {reason}"),
            Self::InvalidIndex(reason) => write!(formatter, "invalid metadata index: {reason}"),
            Self::InvalidRequest(reason) => write!(formatter, "invalid history request: {reason}"),
            Self::SelectorNoMatch(path) => {
                write!(
                    formatter,
                    "history path selector matched no tracked HEAD path: {path}"
                )
            }
            Self::Unavailable(reason) => write!(formatter, "history unavailable: {reason}"),
            Self::StaleRepositoryFence => {
                formatter.write_str("history graph repository fence is stale")
            }
            Self::Git { operation, message } => {
                write!(formatter, "Git {operation} failed: {message}")
            }
            Self::Malformed(reason) => write!(formatter, "malformed Git history: {reason}"),
            Self::MissingObject(oid) => write!(formatter, "Git object {oid} is missing"),
            Self::RepositoryRootMismatch { expected, actual } => write!(
                formatter,
                "history runner repository root {} does not match workspace root {}",
                actual.display(),
                expected.display()
            ),
            Self::UnsafeGitPath(path) => {
                write!(formatter, "unsafe Git metadata path: {}", path.display())
            }
            Self::BoundExceeded(bound) => write!(formatter, "history {bound:?} bound exceeded"),
        }
    }
}

impl std::error::Error for HistoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Revision(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum HistoryGitCommand {
    Head,
    ObjectFormat,
    ShallowBoundaries,
    Commit(ObjectId),
    Tree(ObjectId),
    Blob(ObjectId),
    Blame {
        head: ObjectId,
        path: RootRelativePath,
    },
}

#[derive(Clone, Copy, Debug)]
struct GitCommandLimits {
    timeout: Duration,
    stdout_bytes: usize,
    stderr_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitCommandOutput {
    stdout: Vec<u8>,
}

impl GitCommandOutput {
    pub fn new(stdout: Vec<u8>) -> Self {
        Self { stdout }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum GitCommandError {
    Unavailable(&'static str),
    TimedOut,
    OutputTooLarge,
    Failed(String),
}

trait HistoryCommandRunner {
    fn canonical_repository_root(&self) -> &Path;

    fn begin_refresh(&mut self, _deadline: Instant) -> Result<(), GitCommandError> {
        Ok(())
    }

    fn finish_refresh(&mut self, _deadline: Instant) -> Result<(), GitCommandError> {
        Ok(())
    }

    fn abort_refresh(&mut self) {}

    fn cochange_map_entry(&mut self) {}

    fn implementation_identity(&self) -> GitImplementationIdentity {
        GitImplementationIdentity::fixture()
    }

    fn policy_metrics(&self) -> TrustedGitRepositoryPolicyMetrics {
        TrustedGitRepositoryPolicyMetrics::default()
    }

    fn before_prune_cache(&mut self) -> Result<(), HistoryError> {
        Ok(())
    }

    fn staging_allocation(&mut self, _allocation: StagingAllocation, _required_peak: usize) {}

    fn run(
        &mut self,
        command: &HistoryGitCommand,
        limits: GitCommandLimits,
    ) -> Result<GitCommandOutput, GitCommandError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StagingAllocation {
    CacheEvictionCandidates,
    PruneSets,
    RenameKeys,
}

#[derive(Debug)]
struct TrustedGitRunner {
    repository: PathBuf,
    root: File,
    session: Option<TrustedGitRepositoryPolicySession>,
    implementation: Option<GitImplementationIdentity>,
    last_policy_metrics: TrustedGitRepositoryPolicyMetrics,
}

impl TrustedGitRunner {
    pub fn new(workspace: &ManagedWorkspace) -> Result<Self, HistoryError> {
        Ok(Self {
            repository: workspace.canonical_root().to_owned(),
            root: workspace.duplicate_root()?,
            session: None,
            implementation: None,
            last_policy_metrics: TrustedGitRepositoryPolicyMetrics::default(),
        })
    }
}

impl HistoryCommandRunner for TrustedGitRunner {
    fn canonical_repository_root(&self) -> &Path {
        &self.repository
    }

    fn begin_refresh(&mut self, deadline: Instant) -> Result<(), GitCommandError> {
        self.session = None;
        self.last_policy_metrics = TrustedGitRepositoryPolicyMetrics::default();
        let session =
            begin_trusted_git_repository_policy_session(&self.root, &self.repository, deadline)
                .map_err(trusted_git_error)?;
        self.implementation = Some(GitImplementationIdentity::from_trusted(
            trusted_git_implementation_identity(&session),
        ));
        self.session = Some(session);
        Ok(())
    }

    fn finish_refresh(&mut self, deadline: Instant) -> Result<(), GitCommandError> {
        let mut session = self
            .session
            .take()
            .ok_or_else(|| GitCommandError::Failed("Git policy session is absent".into()))?;
        let result = finish_trusted_git_repository_policy_session(&mut session, deadline)
            .map_err(trusted_git_error);
        self.last_policy_metrics = trusted_git_repository_policy_metrics(&session);
        result
    }

    fn abort_refresh(&mut self) {
        if let Some(session) = self.session.take() {
            self.last_policy_metrics = trusted_git_repository_policy_metrics(&session);
        }
    }

    fn implementation_identity(&self) -> GitImplementationIdentity {
        self.implementation
            .clone()
            .unwrap_or_else(GitImplementationIdentity::fixture)
    }

    fn policy_metrics(&self) -> TrustedGitRepositoryPolicyMetrics {
        self.session.as_ref().map_or(
            self.last_policy_metrics,
            trusted_git_repository_policy_metrics,
        )
    }

    fn run(
        &mut self,
        command: &HistoryGitCommand,
        limits: GitCommandLimits,
    ) -> Result<GitCommandOutput, GitCommandError> {
        let read = match command {
            HistoryGitCommand::Head => TrustedGitRead::Head,
            HistoryGitCommand::ObjectFormat => TrustedGitRead::ObjectFormat,
            HistoryGitCommand::ShallowBoundaries => TrustedGitRead::ShallowBoundaries,
            HistoryGitCommand::Commit(oid) => TrustedGitRead::Commit { oid: oid.as_str() },
            HistoryGitCommand::Tree(oid) => TrustedGitRead::Tree { oid: oid.as_str() },
            HistoryGitCommand::Blob(oid) => TrustedGitRead::Blob { oid: oid.as_str() },
            HistoryGitCommand::Blame { head, path } => TrustedGitRead::Blame {
                head: head.as_str(),
                path,
            },
        };
        let limits = TrustedGitReadLimits {
            timeout: limits.timeout,
            stdout_bytes: limits.stdout_bytes,
            stderr_bytes: limits.stderr_bytes,
        };
        if let Some(session) = &mut self.session {
            trusted_git_read_in_session(session, read, limits)
        } else {
            trusted_git_read(&self.root, &self.repository, read, limits)
        }
        .map(|output| GitCommandOutput::new(output.stdout))
        .map_err(trusted_git_error)
    }
}

fn trusted_git_error(error: AcquisitionError) -> GitCommandError {
    match error {
        AcquisitionError::Unavailable { capability } => GitCommandError::Unavailable(capability),
        AcquisitionError::CommandTimedOut(_) => GitCommandError::TimedOut,
        AcquisitionError::OutputTooLarge(_) => GitCommandError::OutputTooLarge,
        AcquisitionError::Git { message, .. } => GitCommandError::Failed(message),
        other => GitCommandError::Failed(other.to_string()),
    }
}

#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
/// Debug-only conformance contract; this namespace is absent from release builds.
pub mod test_support {
    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum GitCommand {
        Head,
        ObjectFormat,
        ShallowBoundaries,
        Commit(ObjectId),
        Tree(ObjectId),
        Blob(ObjectId),
        Blame {
            head: ObjectId,
            path: RootRelativePath,
        },
    }

    #[derive(Clone, Copy, Debug)]
    pub struct GitCommandLimits {
        pub timeout: Duration,
        pub stdout_bytes: usize,
        pub stderr_bytes: usize,
    }

    #[derive(Clone, Copy, Debug, Default)]
    pub struct RepositoryPolicyMetrics {
        logical_bytes: usize,
        peak_bytes: usize,
        consumed_work: usize,
        policy_scans: usize,
        fence_scans: usize,
        streamed_executable_bytes: usize,
        streamed_executable_chunks: usize,
        commands: usize,
        output_bytes: usize,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct GitCommandOutput {
        stdout: Vec<u8>,
    }

    impl GitCommandOutput {
        pub fn new(stdout: Vec<u8>) -> Self {
            Self { stdout }
        }

        pub fn stdout(&self) -> &[u8] {
            &self.stdout
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    pub enum GitCommandError {
        Unavailable(&'static str),
        TimedOut,
        OutputTooLarge,
        Failed(String),
    }

    pub trait HistoryCommandRunner {
        fn canonical_repository_root(&self) -> &Path;

        fn begin_refresh(&mut self, _deadline: Instant) -> Result<(), GitCommandError> {
            Ok(())
        }

        fn finish_refresh(&mut self, _deadline: Instant) -> Result<(), GitCommandError> {
            Ok(())
        }

        fn abort_refresh(&mut self) {}

        fn cochange_map_entry(&mut self) {}

        fn implementation_identity(&self) -> GitImplementationIdentity {
            GitImplementationIdentity::fixture()
        }

        fn policy_metrics(&self) -> RepositoryPolicyMetrics {
            RepositoryPolicyMetrics::default()
        }

        fn before_prune_cache(&mut self) -> Result<(), HistoryError> {
            Ok(())
        }

        fn staging_allocation(&mut self, _allocation: StagingAllocation, _required_peak: usize) {}

        fn run(
            &mut self,
            command: &GitCommand,
            limits: GitCommandLimits,
        ) -> Result<GitCommandOutput, GitCommandError>;
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum StagingAllocation {
        CacheEvictionCandidates,
        PruneSets,
        RenameKeys,
    }

    pub struct TrustedGitRunner(super::TrustedGitRunner);

    impl TrustedGitRunner {
        pub fn new(workspace: &ManagedWorkspace) -> Result<Self, HistoryError> {
            super::TrustedGitRunner::new(workspace).map(Self)
        }

        pub fn repository(&self) -> &Path {
            &self.0.repository
        }
    }

    impl HistoryCommandRunner for TrustedGitRunner {
        fn canonical_repository_root(&self) -> &Path {
            &self.0.repository
        }

        fn begin_refresh(&mut self, deadline: Instant) -> Result<(), GitCommandError> {
            super::HistoryCommandRunner::begin_refresh(&mut self.0, deadline).map_err(public_error)
        }

        fn finish_refresh(&mut self, deadline: Instant) -> Result<(), GitCommandError> {
            super::HistoryCommandRunner::finish_refresh(&mut self.0, deadline).map_err(public_error)
        }

        fn abort_refresh(&mut self) {
            super::HistoryCommandRunner::abort_refresh(&mut self.0);
        }

        fn cochange_map_entry(&mut self) {
            super::HistoryCommandRunner::cochange_map_entry(&mut self.0);
        }

        fn implementation_identity(&self) -> GitImplementationIdentity {
            super::HistoryCommandRunner::implementation_identity(&self.0)
        }

        fn policy_metrics(&self) -> RepositoryPolicyMetrics {
            let metrics = super::HistoryCommandRunner::policy_metrics(&self.0);
            RepositoryPolicyMetrics {
                logical_bytes: metrics.logical_bytes(),
                peak_bytes: metrics.peak_bytes(),
                consumed_work: metrics.consumed_work(),
                policy_scans: metrics.policy_scans(),
                fence_scans: metrics.fence_scans(),
                streamed_executable_bytes: metrics.streamed_executable_bytes(),
                streamed_executable_chunks: metrics.streamed_executable_chunks(),
                commands: metrics.commands(),
                output_bytes: metrics.output_bytes(),
            }
        }

        fn run(
            &mut self,
            command: &GitCommand,
            limits: GitCommandLimits,
        ) -> Result<GitCommandOutput, GitCommandError> {
            super::HistoryCommandRunner::run(
                &mut self.0,
                &internal_command(command),
                super::GitCommandLimits {
                    timeout: limits.timeout,
                    stdout_bytes: limits.stdout_bytes,
                    stderr_bytes: limits.stderr_bytes,
                },
            )
            .map(|output| GitCommandOutput::new(output.stdout))
            .map_err(public_error)
        }
    }

    pub fn refresh_with_runner<'a, R: HistoryCommandRunner>(
        provider: &'a mut HistoryGraphProvider,
        workspace: &ManagedWorkspace,
        index: &MetadataIndex,
        request: &HistoryRequest,
        options: &HistoryOptions,
        runner: &mut R,
    ) -> Result<&'a HistoryGraph, HistoryError> {
        provider.refresh_with(
            workspace,
            index,
            request,
            options,
            &mut RunnerAdapter { runner },
        )
    }

    pub fn validated_graph_with_runner<'a, R: HistoryCommandRunner>(
        provider: &'a HistoryGraphProvider,
        workspace: &ManagedWorkspace,
        runner: &mut R,
    ) -> Result<Option<&'a HistoryGraph>, HistoryError> {
        provider.validated_graph_with(workspace, &mut RunnerAdapter { runner })
    }

    pub const fn validation_scans(metrics: &RefreshMetrics) -> usize {
        metrics.validation_scans
    }

    pub const fn cochange_preflight_staging_bytes(metrics: &RefreshMetrics) -> usize {
        metrics.cochange_preflight_staging_bytes
    }

    struct RunnerAdapter<'a, R> {
        runner: &'a mut R,
    }

    impl<R: HistoryCommandRunner> super::HistoryCommandRunner for RunnerAdapter<'_, R> {
        fn canonical_repository_root(&self) -> &Path {
            self.runner.canonical_repository_root()
        }

        fn begin_refresh(&mut self, deadline: Instant) -> Result<(), super::GitCommandError> {
            self.runner.begin_refresh(deadline).map_err(internal_error)
        }

        fn finish_refresh(&mut self, deadline: Instant) -> Result<(), super::GitCommandError> {
            self.runner.finish_refresh(deadline).map_err(internal_error)
        }

        fn abort_refresh(&mut self) {
            self.runner.abort_refresh();
        }

        fn cochange_map_entry(&mut self) {
            self.runner.cochange_map_entry();
        }

        fn implementation_identity(&self) -> GitImplementationIdentity {
            self.runner.implementation_identity()
        }

        fn policy_metrics(&self) -> TrustedGitRepositoryPolicyMetrics {
            let metrics = self.runner.policy_metrics();
            TrustedGitRepositoryPolicyMetrics {
                logical_bytes: metrics.logical_bytes,
                peak_bytes: metrics.peak_bytes,
                consumed_work: metrics.consumed_work,
                policy_scans: metrics.policy_scans,
                fence_scans: metrics.fence_scans,
                streamed_executable_bytes: metrics.streamed_executable_bytes,
                streamed_executable_chunks: metrics.streamed_executable_chunks,
                commands: metrics.commands,
                output_bytes: metrics.output_bytes,
            }
        }

        fn before_prune_cache(&mut self) -> Result<(), HistoryError> {
            self.runner.before_prune_cache()
        }

        fn staging_allocation(
            &mut self,
            allocation: super::StagingAllocation,
            required_peak: usize,
        ) {
            self.runner.staging_allocation(
                match allocation {
                    super::StagingAllocation::CacheEvictionCandidates => {
                        StagingAllocation::CacheEvictionCandidates
                    }
                    super::StagingAllocation::PruneSets => StagingAllocation::PruneSets,
                    super::StagingAllocation::RenameKeys => StagingAllocation::RenameKeys,
                },
                required_peak,
            );
        }

        fn run(
            &mut self,
            command: &super::HistoryGitCommand,
            limits: super::GitCommandLimits,
        ) -> Result<super::GitCommandOutput, super::GitCommandError> {
            self.runner
                .run(
                    &public_command(command),
                    GitCommandLimits {
                        timeout: limits.timeout,
                        stdout_bytes: limits.stdout_bytes,
                        stderr_bytes: limits.stderr_bytes,
                    },
                )
                .map(|output| super::GitCommandOutput::new(output.stdout))
                .map_err(internal_error)
        }
    }

    fn public_command(command: &super::HistoryGitCommand) -> GitCommand {
        match command {
            super::HistoryGitCommand::Head => GitCommand::Head,
            super::HistoryGitCommand::ObjectFormat => GitCommand::ObjectFormat,
            super::HistoryGitCommand::ShallowBoundaries => GitCommand::ShallowBoundaries,
            super::HistoryGitCommand::Commit(oid) => GitCommand::Commit(oid.clone()),
            super::HistoryGitCommand::Tree(oid) => GitCommand::Tree(oid.clone()),
            super::HistoryGitCommand::Blob(oid) => GitCommand::Blob(oid.clone()),
            super::HistoryGitCommand::Blame { head, path } => GitCommand::Blame {
                head: head.clone(),
                path: path.clone(),
            },
        }
    }

    fn internal_command(command: &GitCommand) -> super::HistoryGitCommand {
        match command {
            GitCommand::Head => super::HistoryGitCommand::Head,
            GitCommand::ObjectFormat => super::HistoryGitCommand::ObjectFormat,
            GitCommand::ShallowBoundaries => super::HistoryGitCommand::ShallowBoundaries,
            GitCommand::Commit(oid) => super::HistoryGitCommand::Commit(oid.clone()),
            GitCommand::Tree(oid) => super::HistoryGitCommand::Tree(oid.clone()),
            GitCommand::Blob(oid) => super::HistoryGitCommand::Blob(oid.clone()),
            GitCommand::Blame { head, path } => super::HistoryGitCommand::Blame {
                head: head.clone(),
                path: path.clone(),
            },
        }
    }

    fn public_error(error: super::GitCommandError) -> GitCommandError {
        match error {
            super::GitCommandError::Unavailable(reason) => GitCommandError::Unavailable(reason),
            super::GitCommandError::TimedOut => GitCommandError::TimedOut,
            super::GitCommandError::OutputTooLarge => GitCommandError::OutputTooLarge,
            super::GitCommandError::Failed(message) => GitCommandError::Failed(message),
        }
    }

    fn internal_error(error: GitCommandError) -> super::GitCommandError {
        match error {
            GitCommandError::Unavailable(reason) => super::GitCommandError::Unavailable(reason),
            GitCommandError::TimedOut => super::GitCommandError::TimedOut,
            GitCommandError::OutputTooLarge => super::GitCommandError::OutputTooLarge,
            GitCommandError::Failed(message) => super::GitCommandError::Failed(message),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CoverageStatus {
    Complete,
    ObservedPartial,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CoverageArea {
    Commits,
    Renames,
    CoChange,
    Blame,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CoverageRecord {
    area: CoverageArea,
    status: CoverageStatus,
    detail: &'static str,
    omitted_count: usize,
}

impl CoverageRecord {
    pub const fn area(&self) -> CoverageArea {
        self.area
    }
    pub const fn status(&self) -> CoverageStatus {
        self.status
    }
    pub const fn detail(&self) -> &'static str {
        self.detail
    }
    pub const fn omitted_count(&self) -> usize {
        self.omitted_count
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryCommit {
    oid: ObjectId,
    tree: ObjectId,
    parents: Vec<ObjectId>,
}

impl HistoryCommit {
    pub fn oid(&self) -> &ObjectId {
        &self.oid
    }
    pub fn tree(&self) -> &ObjectId {
        &self.tree
    }
    pub fn parents(&self) -> &[ObjectId] {
        &self.parents
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ChangeKind {
    Added,
    Deleted,
    Modified,
    Renamed,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HistoryChange {
    commit: ObjectId,
    parent: Option<ObjectId>,
    path: RootRelativePath,
    current_path: Option<RootRelativePath>,
    kind: ChangeKind,
}

impl HistoryChange {
    pub fn commit(&self) -> &ObjectId {
        &self.commit
    }
    pub fn parent(&self) -> Option<&ObjectId> {
        self.parent.as_ref()
    }
    pub const fn path(&self) -> &RootRelativePath {
        &self.path
    }
    pub const fn current_path(&self) -> Option<&RootRelativePath> {
        self.current_path.as_ref()
    }
    pub const fn kind(&self) -> ChangeKind {
        self.kind
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ExactRename {
    commit: ObjectId,
    parent: ObjectId,
    from: RootRelativePath,
    to: RootRelativePath,
    current_path: Option<RootRelativePath>,
    blob: ObjectId,
    mode: String,
    confidence_millis: u16,
}

impl ExactRename {
    pub fn commit(&self) -> &ObjectId {
        &self.commit
    }
    pub fn parent(&self) -> &ObjectId {
        &self.parent
    }
    pub const fn from(&self) -> &RootRelativePath {
        &self.from
    }
    pub const fn to(&self) -> &RootRelativePath {
        &self.to
    }
    pub const fn current_path(&self) -> Option<&RootRelativePath> {
        self.current_path.as_ref()
    }
    pub fn blob(&self) -> &ObjectId {
        &self.blob
    }
    pub fn mode(&self) -> &str {
        &self.mode
    }
    pub const fn confidence_millis(&self) -> u16 {
        self.confidence_millis
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum BlameSource {
    Git,
    Worktree,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct BlameHunk {
    path: RootRelativePath,
    range: GraphRange,
    source: BlameSource,
    source_commit: Option<ObjectId>,
    source_path: RootRelativePath,
    source_blob: Option<ObjectId>,
    source_range: GraphRange,
    boundary: bool,
    confidence_millis: u16,
    revision: RevisionId,
    line_digest: [u8; 32],
    evidence_digest: [u8; 32],
}

impl BlameHunk {
    pub const fn path(&self) -> &RootRelativePath {
        &self.path
    }
    pub const fn range(&self) -> GraphRange {
        self.range
    }
    pub const fn source(&self) -> BlameSource {
        self.source
    }
    pub fn source_commit(&self) -> Option<&ObjectId> {
        self.source_commit.as_ref()
    }
    pub const fn source_path(&self) -> &RootRelativePath {
        &self.source_path
    }
    pub fn source_blob(&self) -> Option<&ObjectId> {
        self.source_blob.as_ref()
    }
    pub const fn source_range(&self) -> GraphRange {
        self.source_range
    }
    pub const fn source_start_line(&self) -> usize {
        self.source_range.start_line
    }
    pub const fn boundary(&self) -> bool {
        self.boundary
    }
    pub const fn confidence_millis(&self) -> u16 {
        self.confidence_millis
    }
    pub const fn revision(&self) -> RevisionId {
        self.revision
    }
    pub const fn line_digest(&self) -> [u8; 32] {
        self.line_digest
    }
    pub const fn evidence_digest(&self) -> [u8; 32] {
        self.evidence_digest
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedWithProvenance {
    head: ObjectId,
    scope_digest: [u8; 32],
    policy: &'static str,
    commits: Vec<ObjectId>,
    count: usize,
    left_count: usize,
    right_count: usize,
    shared_count: usize,
    evidence_digest: [u8; 32],
    revision: RevisionId,
    left_range: GraphRange,
    right_range: GraphRange,
    left_committed_blob: ObjectId,
    right_committed_blob: ObjectId,
    left_committed_range: GraphRange,
    right_committed_range: GraphRange,
}

impl ChangedWithProvenance {
    pub fn head(&self) -> &ObjectId {
        &self.head
    }
    pub const fn scope_digest(&self) -> [u8; 32] {
        self.scope_digest
    }
    pub fn policy(&self) -> &str {
        self.policy
    }
    pub fn commits(&self) -> &[ObjectId] {
        &self.commits
    }
    pub const fn count(&self) -> usize {
        self.count
    }
    pub const fn left_count(&self) -> usize {
        self.left_count
    }
    pub const fn right_count(&self) -> usize {
        self.right_count
    }
    pub const fn shared_count(&self) -> usize {
        self.shared_count
    }
    pub const fn evidence_digest(&self) -> [u8; 32] {
        self.evidence_digest
    }
    pub const fn revision(&self) -> RevisionId {
        self.revision
    }
    pub const fn left_range(&self) -> GraphRange {
        self.left_range
    }
    pub const fn right_range(&self) -> GraphRange {
        self.right_range
    }
    pub fn left_committed_blob(&self) -> &ObjectId {
        &self.left_committed_blob
    }
    pub fn right_committed_blob(&self) -> &ObjectId {
        &self.right_committed_blob
    }
    pub const fn left_committed_range(&self) -> GraphRange {
        self.left_committed_range
    }
    pub const fn right_committed_range(&self) -> GraphRange {
        self.right_committed_range
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedWithFact {
    left: RootRelativePath,
    right: RootRelativePath,
    count: usize,
    strength_millis: u16,
    extraction_confidence_millis: u16,
    provenance: ChangedWithProvenance,
}

impl ChangedWithFact {
    pub const fn left(&self) -> &RootRelativePath {
        &self.left
    }
    pub const fn right(&self) -> &RootRelativePath {
        &self.right
    }
    pub const fn count(&self) -> usize {
        self.count
    }
    pub const fn strength_millis(&self) -> u16 {
        self.strength_millis
    }
    pub const fn extraction_confidence_millis(&self) -> u16 {
        self.extraction_confidence_millis
    }
    pub const fn provenance(&self) -> &ChangedWithProvenance {
        &self.provenance
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryGraph {
    revision: RevisionId,
    workspace_digest: String,
    index_digest: [u8; 32],
    head: ObjectId,
    head_tree: ObjectId,
    object_format: ObjectFormat,
    shallow_digest: [u8; 32],
    scope_digest: [u8; 32],
    request_digest: [u8; 32],
    commits: Vec<HistoryCommit>,
    changes: Vec<HistoryChange>,
    renames: Vec<ExactRename>,
    blame_hunks: Arc<[BlameHunk]>,
    changed_with: Vec<ChangedWithFact>,
    coverage: Vec<CoverageRecord>,
    content_digest: [u8; 32],
    snapshot_digest: [u8; 32],
    extractor_digest: [u8; 32],
    git_implementation: GitImplementationIdentity,
    options_digest: [u8; 32],
    logical_bytes: usize,
}

impl HistoryGraph {
    pub const fn revision(&self) -> RevisionId {
        self.revision
    }
    pub fn workspace_digest(&self) -> &str {
        &self.workspace_digest
    }
    pub const fn index_digest(&self) -> [u8; 32] {
        self.index_digest
    }
    pub fn head(&self) -> &ObjectId {
        &self.head
    }
    pub fn head_tree(&self) -> &ObjectId {
        &self.head_tree
    }
    pub const fn object_format(&self) -> ObjectFormat {
        self.object_format
    }
    pub const fn shallow_digest(&self) -> [u8; 32] {
        self.shallow_digest
    }
    pub const fn scope_digest(&self) -> [u8; 32] {
        self.scope_digest
    }
    pub const fn request_digest(&self) -> [u8; 32] {
        self.request_digest
    }
    pub fn commits(&self) -> &[HistoryCommit] {
        &self.commits
    }
    pub fn changes(&self) -> &[HistoryChange] {
        &self.changes
    }
    pub fn renames(&self) -> &[ExactRename] {
        &self.renames
    }
    pub fn blame_hunks(&self) -> &[BlameHunk] {
        &self.blame_hunks
    }
    pub(super) fn blame_hunks_arc(&self) -> Arc<[BlameHunk]> {
        Arc::clone(&self.blame_hunks)
    }
    pub fn changed_with(&self) -> &[ChangedWithFact] {
        &self.changed_with
    }
    pub fn coverage(&self) -> &[CoverageRecord] {
        &self.coverage
    }
    pub const fn content_digest(&self) -> [u8; 32] {
        self.content_digest
    }
    pub const fn snapshot_digest(&self) -> [u8; 32] {
        self.snapshot_digest
    }
    pub const fn extractor_digest(&self) -> [u8; 32] {
        self.extractor_digest
    }
    pub const fn git_implementation(&self) -> &GitImplementationIdentity {
        &self.git_implementation
    }
    pub const fn options_digest(&self) -> [u8; 32] {
        self.options_digest
    }
    pub const fn logical_bytes(&self) -> usize {
        self.logical_bytes
    }

    pub fn enrich_structure(
        &self,
        structure: &StructureGraph,
        limits: super::structure::HistoryEnrichmentLimits,
    ) -> Result<StructureGraph, GraphError> {
        structure.with_history(self, limits)
    }
}

#[derive(Debug)]
pub struct ValidatedHistoryFence {
    revision: RevisionId,
    history_snapshot_digest: [u8; 32],
    history_content_digest: [u8; 32],
    request_digest: [u8; 32],
    head: ObjectId,
    object_format: ObjectFormat,
    shallow_digest: [u8; 32],
    git_implementation: GitImplementationIdentity,
    runner: RefCell<TrustedGitRunner>,
    validations: Cell<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryFenceMetrics {
    policy_scans: usize,
    fence_scans: usize,
    validations: usize,
    commands: usize,
    output_bytes: usize,
    streamed_executable_bytes: usize,
    streamed_executable_chunks: usize,
    work: usize,
    logical_memory_bytes: usize,
    peak_memory_bytes: usize,
}

impl HistoryFenceMetrics {
    pub const fn policy_scans(self) -> usize {
        self.policy_scans
    }
    pub const fn validations(self) -> usize {
        self.validations
    }
    pub const fn fence_scans(self) -> usize {
        self.fence_scans
    }
    pub const fn commands(self) -> usize {
        self.commands
    }
    pub const fn output_bytes(self) -> usize {
        self.output_bytes
    }
    pub const fn streamed_executable_bytes(self) -> usize {
        self.streamed_executable_bytes
    }
    pub const fn streamed_executable_chunks(self) -> usize {
        self.streamed_executable_chunks
    }
    pub const fn work(self) -> usize {
        self.work
    }
    pub const fn peak_memory_bytes(self) -> usize {
        self.peak_memory_bytes
    }
    pub const fn logical_memory_bytes(self) -> usize {
        self.logical_memory_bytes
    }
}

impl ValidatedHistoryFence {
    pub const fn revision(&self) -> RevisionId {
        self.revision
    }
    pub const fn history_snapshot_digest(&self) -> [u8; 32] {
        self.history_snapshot_digest
    }
    pub const fn history_content_digest(&self) -> [u8; 32] {
        self.history_content_digest
    }

    pub const fn request_digest(&self) -> [u8; 32] {
        self.request_digest
    }

    pub fn metrics(&self) -> HistoryFenceMetrics {
        let policy = self.runner.borrow().policy_metrics();
        HistoryFenceMetrics {
            policy_scans: policy.policy_scans(),
            fence_scans: policy.fence_scans(),
            validations: self.validations.get(),
            commands: policy.commands(),
            output_bytes: policy.output_bytes(),
            streamed_executable_bytes: policy.streamed_executable_bytes(),
            streamed_executable_chunks: policy.streamed_executable_chunks(),
            work: policy.consumed_work(),
            logical_memory_bytes: policy.logical_bytes(),
            peak_memory_bytes: policy.peak_bytes(),
        }
    }

    pub const fn conservative_metrics(validations: usize) -> HistoryFenceMetrics {
        let command_output = 2 * 1024 * 1024;
        HistoryFenceMetrics {
            policy_scans: 1,
            fence_scans: 1 + validations.saturating_mul(6),
            validations,
            commands: 1 + validations.saturating_mul(3),
            output_bytes: 4 * 1024 + validations.saturating_mul(3 * command_output),
            streamed_executable_bytes: 0,
            streamed_executable_chunks: 0,
            work: 1 + validations.saturating_mul(3 * command_output),
            logical_memory_bytes: size_of::<TrustedGitRepositoryPolicySession>(),
            peak_memory_bytes: size_of::<TrustedGitRepositoryPolicySession>() + 2 * command_output,
        }
    }

    fn new(workspace: &ManagedWorkspace, graph: &HistoryGraph) -> Result<Self, HistoryError> {
        workspace.validate_revision(graph.revision)?;
        let mut runner = TrustedGitRunner::new(workspace)?;
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(5))
            .unwrap_or_else(Instant::now);
        runner
            .begin_refresh(deadline)
            .map_err(repository_validation_error)?;
        if runner.implementation_identity() != graph.git_implementation {
            runner.abort_refresh();
            return Err(HistoryError::StaleRepositoryFence);
        }
        let fence = Self {
            revision: graph.revision,
            history_snapshot_digest: graph.snapshot_digest,
            history_content_digest: graph.content_digest,
            request_digest: graph.request_digest,
            head: graph.head.clone(),
            object_format: graph.object_format,
            shallow_digest: graph.shallow_digest,
            git_implementation: graph.git_implementation.clone(),
            runner: RefCell::new(runner),
            validations: Cell::new(0),
        };
        fence.validate_repository(workspace, deadline)?;
        Ok(fence)
    }

    pub(crate) fn validate_repository(
        &self,
        workspace: &ManagedWorkspace,
        deadline: Instant,
    ) -> Result<(), HistoryError> {
        workspace.validate_revision_until(self.revision, deadline)?;
        let mut runner = self.runner.borrow_mut();
        validate_runner_root(workspace, &*runner)?;
        if runner.implementation_identity() != self.git_implementation {
            return Err(HistoryError::StaleRepositoryFence);
        }
        let head = read_runner_head(&mut *runner, self.object_format, deadline)?;
        let format = read_runner_object_format(&mut *runner, deadline)?;
        let shallow_digest = read_runner_shallow_digest(&mut *runner, format, deadline)?;
        self.validations
            .set(self.validations.get().saturating_add(1));
        workspace.validate_revision_until(self.revision, deadline)?;
        if head != self.head
            || format != self.object_format
            || shallow_digest != self.shallow_digest
        {
            return Err(HistoryError::StaleRepositoryFence);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RefreshMetrics {
    commands: usize,
    output_bytes: usize,
    parsed_objects: usize,
    reused_objects: usize,
    consumed_work: usize,
    peak_staging_bytes: usize,
    validation_scans: usize,
    cochange_preflight_staging_bytes: usize,
}

impl RefreshMetrics {
    pub const fn commands(&self) -> usize {
        self.commands
    }
    pub const fn output_bytes(&self) -> usize {
        self.output_bytes
    }
    pub const fn parsed_objects(&self) -> usize {
        self.parsed_objects
    }
    pub const fn reused_objects(&self) -> usize {
        self.reused_objects
    }
    pub const fn consumed_work(&self) -> usize {
        self.consumed_work
    }
    pub const fn peak_staging_bytes(&self) -> usize {
        self.peak_staging_bytes
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheUsage {
    entries: usize,
    logical_bytes: usize,
}

impl CacheUsage {
    pub const fn entries(self) -> usize {
        self.entries
    }
    pub const fn logical_bytes(self) -> usize {
        self.logical_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TreeEntry {
    mode: String,
    blob: ObjectId,
}

type Tree = BTreeMap<RootRelativePath, TreeEntry>;

#[derive(Clone, Debug)]
struct BlobData {
    bytes: Vec<u8>,
    lines: Vec<(usize, usize)>,
}

#[derive(Clone, Debug)]
struct CacheEntry<T> {
    value: Arc<T>,
    used: u64,
    weight: usize,
}

#[derive(Clone, Debug, Default)]
pub struct HistoryGraphProvider {
    graph: Option<HistoryGraph>,
    tree_cache: BTreeMap<ObjectId, CacheEntry<Tree>>,
    blob_cache: BTreeMap<ObjectId, CacheEntry<BlobData>>,
    cache_clock: u64,
    cache_bytes: usize,
    metrics: RefreshMetrics,
}

impl HistoryGraphProvider {
    pub fn new() -> Self {
        Self::default()
    }
    pub const fn graph(&self) -> Option<&HistoryGraph> {
        self.graph.as_ref()
    }
    pub const fn metrics(&self) -> &RefreshMetrics {
        &self.metrics
    }

    pub fn validated_graph(
        &self,
        workspace: &ManagedWorkspace,
    ) -> Result<Option<&HistoryGraph>, HistoryError> {
        let mut runner = TrustedGitRunner::new(workspace)?;
        self.validated_graph_with(workspace, &mut runner)
    }

    pub fn validated_fence(
        &self,
        workspace: &ManagedWorkspace,
    ) -> Result<Option<ValidatedHistoryFence>, HistoryError> {
        self.graph
            .as_ref()
            .map(|graph| ValidatedHistoryFence::new(workspace, graph))
            .transpose()
    }

    pub fn cache_usage(&self) -> CacheUsage {
        CacheUsage {
            entries: self.tree_cache.len().saturating_add(self.blob_cache.len()),
            logical_bytes: self.cache_bytes,
        }
    }

    pub fn refresh(
        &mut self,
        workspace: &ManagedWorkspace,
        index: &MetadataIndex,
        request: &HistoryRequest,
        options: &HistoryOptions,
    ) -> Result<&HistoryGraph, HistoryError> {
        let mut runner = TrustedGitRunner::new(workspace)?;
        self.refresh_with(workspace, index, request, options, &mut runner)
    }

    pub fn refresh_fenced(
        &mut self,
        workspace: &ManagedWorkspace,
        index: &MetadataIndex,
        request: &HistoryRequest,
        options: &HistoryOptions,
    ) -> Result<(&HistoryGraph, ValidatedHistoryFence), HistoryError> {
        let graph = self.refresh(workspace, index, request, options)?;
        let fence = ValidatedHistoryFence::new(workspace, graph)?;
        Ok((graph, fence))
    }

    fn validated_graph_with<R: HistoryCommandRunner>(
        &self,
        workspace: &ManagedWorkspace,
        runner: &mut R,
    ) -> Result<Option<&HistoryGraph>, HistoryError> {
        if let Some(graph) = &self.graph {
            validate_runner_root(workspace, runner)?;
            workspace.validate_revision(graph.revision)?;
            let deadline = Instant::now()
                .checked_add(Duration::from_secs(5))
                .unwrap_or_else(Instant::now);
            runner
                .begin_refresh(deadline)
                .map_err(repository_validation_error)?;
            if runner.implementation_identity() != graph.git_implementation {
                runner.abort_refresh();
                return Err(HistoryError::StaleRepositoryFence);
            }
            let checked = (|| {
                let head = read_runner_head(runner, graph.object_format, deadline)?;
                let format = read_runner_object_format(runner, deadline)?;
                let shallow_digest = read_runner_shallow_digest(runner, format, deadline)?;
                Ok::<_, HistoryError>((head, format, shallow_digest))
            })();
            let (head, format, shallow_digest) = match checked {
                Ok(checked) => checked,
                Err(error) => {
                    runner.abort_refresh();
                    return Err(error);
                }
            };
            runner
                .finish_refresh(deadline)
                .map_err(repository_validation_error)?;
            workspace.validate_revision(graph.revision)?;
            if head != graph.head
                || format != graph.object_format
                || shallow_digest != graph.shallow_digest
            {
                return Err(HistoryError::StaleRepositoryFence);
            }
        }
        Ok(self.graph.as_ref())
    }

    fn refresh_with<R: HistoryCommandRunner>(
        &mut self,
        workspace: &ManagedWorkspace,
        index: &MetadataIndex,
        request: &HistoryRequest,
        options: &HistoryOptions,
        runner: &mut R,
    ) -> Result<&HistoryGraph, HistoryError> {
        validate_options(options)?;
        validate_runner_root(workspace, runner)?;
        let started = Instant::now();
        let deadline = started.checked_add(options.max_time).unwrap_or(started);
        let current = workspace.validate_revision_until(index.revision(), deadline)?;
        if current.epoch() != index.epoch() {
            return Err(HistoryError::InvalidIndex(
                "workspace epoch does not match index",
            ));
        }
        runner
            .begin_refresh(deadline)
            .map_err(repository_validation_error)?;
        let mut build = match Build::new(self, index, request, options, runner, deadline) {
            Ok(build) => build,
            Err(error) => {
                runner.abort_refresh();
                return Err(error);
            }
        };
        let output = match build.extract() {
            Ok(output) => output,
            Err(error) => {
                build.runner.abort_refresh();
                return Err(error);
            }
        };
        validate_runner_root(workspace, runner)?;
        workspace.validate_revision_until(index.revision(), deadline)?;
        check_deadline(deadline)?;
        self.graph = Some(output.graph);
        self.tree_cache = output.tree_cache;
        self.blob_cache = output.blob_cache;
        self.cache_clock = output.cache_clock;
        self.cache_bytes = output.cache_bytes;
        self.metrics = output.metrics;
        Ok(self.graph.as_ref().expect("published history graph"))
    }
}

fn validate_runner_root<R: HistoryCommandRunner>(
    workspace: &ManagedWorkspace,
    runner: &R,
) -> Result<(), HistoryError> {
    let expected = workspace.canonical_root();
    let actual = runner.canonical_repository_root();
    if actual == expected {
        Ok(())
    } else {
        Err(HistoryError::RepositoryRootMismatch {
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        })
    }
}

fn read_runner_object_format<R: HistoryCommandRunner>(
    runner: &mut R,
    deadline: Instant,
) -> Result<ObjectFormat, HistoryError> {
    match run_fenced(runner, HistoryGitCommand::ObjectFormat, deadline)?.as_slice() {
        b"sha1\n" | b"sha1\r\n" | b"sha1" => Ok(ObjectFormat::Sha1),
        b"sha256\n" | b"sha256\r\n" | b"sha256" => Ok(ObjectFormat::Sha256),
        _ => Err(HistoryError::Malformed("invalid Git object format")),
    }
}

fn read_runner_head<R: HistoryCommandRunner>(
    runner: &mut R,
    format: ObjectFormat,
    deadline: Instant,
) -> Result<ObjectId, HistoryError> {
    let output = run_fenced(runner, HistoryGitCommand::Head, deadline)?;
    let value = std::str::from_utf8(&output)
        .map_err(|_| HistoryError::Malformed("HEAD is not ASCII"))?
        .trim_end_matches(['\r', '\n']);
    ObjectId::parse(value, format)
}

fn read_runner_shallow_digest<R: HistoryCommandRunner>(
    runner: &mut R,
    format: ObjectFormat,
    deadline: Instant,
) -> Result<[u8; 32], HistoryError> {
    let output = run_fenced(runner, HistoryGitCommand::ShallowBoundaries, deadline)?;
    let mut boundaries = BTreeSet::new();
    for line in output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let value = std::str::from_utf8(line)
            .map_err(|_| HistoryError::Malformed("shallow boundary is not ASCII"))?;
        boundaries.insert(ObjectId::parse(value.trim_end_matches('\r'), format)?);
    }
    Ok(digest_shallow(&boundaries))
}

fn run_fenced<R: HistoryCommandRunner>(
    runner: &mut R,
    command: HistoryGitCommand,
    deadline: Instant,
) -> Result<Vec<u8>, HistoryError> {
    const BYTES: usize = 1024 * 1024;
    let timeout = Duration::from_secs(2).min(deadline.saturating_duration_since(Instant::now()));
    if timeout.is_zero() {
        return Err(HistoryError::BoundExceeded(HistoryBound::Time));
    }
    let started = Instant::now();
    let output = runner
        .run(
            &command,
            GitCommandLimits {
                timeout,
                stdout_bytes: BYTES,
                stderr_bytes: BYTES,
            },
        )
        .map_err(|error| {
            command_error(
                &command,
                error,
                HistoryBound::CommandOutputBytes,
                BYTES,
                BYTES,
            )
        })?;
    if started.elapsed() > timeout {
        return Err(HistoryError::BoundExceeded(HistoryBound::Time));
    }
    if output.stdout.len() > BYTES {
        return Err(HistoryError::BoundExceeded(
            HistoryBound::CommandOutputBytes,
        ));
    }
    Ok(output.stdout)
}

struct BuildOutput {
    graph: HistoryGraph,
    tree_cache: BTreeMap<ObjectId, CacheEntry<Tree>>,
    blob_cache: BTreeMap<ObjectId, CacheEntry<BlobData>>,
    cache_clock: u64,
    cache_bytes: usize,
    metrics: RefreshMetrics,
}

struct Build<'a, R> {
    index: &'a MetadataIndex,
    request: &'a HistoryRequest,
    options: &'a HistoryOptions,
    runner: &'a mut R,
    deadline: Instant,
    tree_cache: BTreeMap<ObjectId, CacheEntry<Tree>>,
    blob_cache: BTreeMap<ObjectId, CacheEntry<BlobData>>,
    cache_clock: u64,
    cache_bytes: usize,
    metrics: RefreshMetrics,
    retained: usize,
    git_implementation: GitImplementationIdentity,
    policy_consumed_work: usize,
}

impl<'a, R: HistoryCommandRunner> Build<'a, R> {
    fn new(
        provider: &HistoryGraphProvider,
        index: &'a MetadataIndex,
        request: &'a HistoryRequest,
        options: &'a HistoryOptions,
        runner: &'a mut R,
        deadline: Instant,
    ) -> Result<Self, HistoryError> {
        if request.blame_paths.len() > options.max_blame_paths {
            return Err(HistoryError::BoundExceeded(HistoryBound::BlamePaths));
        }
        if request
            .scope
            .len()
            .saturating_add(request.blame_paths.len())
            > options.max_paths
        {
            return Err(HistoryError::BoundExceeded(HistoryBound::Paths));
        }
        if provider
            .tree_cache
            .len()
            .saturating_add(provider.blob_cache.len())
            > options.max_cache_entries
        {
            return Err(HistoryError::BoundExceeded(HistoryBound::CacheEntries));
        }
        if provider.cache_bytes > options.max_cache_bytes {
            return Err(HistoryError::BoundExceeded(HistoryBound::CacheBytes));
        }
        let retained = provider
            .graph
            .as_ref()
            .map_or(0, HistoryGraph::logical_bytes)
            .saturating_add(provider.cache_bytes);
        let cloned_maps = cache_map_weight(&provider.tree_cache)
            .saturating_add(cache_map_weight(&provider.blob_cache));
        let policy = runner.policy_metrics();
        let base_retained = retained.saturating_add(cloned_maps);
        let retained = base_retained.saturating_add(policy.logical_bytes());
        if retained > options.max_staging_bytes {
            return Err(HistoryError::BoundExceeded(HistoryBound::StagingBytes));
        }
        let git_implementation = runner.implementation_identity();
        Ok(Self {
            index,
            request,
            options,
            runner,
            deadline,
            tree_cache: provider.tree_cache.clone(),
            blob_cache: provider.blob_cache.clone(),
            cache_clock: provider.cache_clock,
            cache_bytes: provider.cache_bytes,
            metrics: RefreshMetrics {
                commands: policy.commands().max(1),
                consumed_work: policy.consumed_work().max(1),
                peak_staging_bytes: base_retained
                    .saturating_add(policy.peak_bytes())
                    .max(retained),
                validation_scans: policy.policy_scans().max(1),
                ..RefreshMetrics::default()
            },
            retained,
            git_implementation,
            policy_consumed_work: policy.consumed_work(),
        })
    }

    fn extract(&mut self) -> Result<BuildOutput, HistoryError> {
        let format = self.object_format()?;
        let shallow_boundaries = self.shallow_boundaries(format)?;
        let shallow_digest = digest_shallow(&shallow_boundaries);
        let shallow = !shallow_boundaries.is_empty();
        let shallow_weight = shallow_boundaries
            .iter()
            .map(|oid| BTREE_ENTRY_WEIGHT + object_id_weight(oid))
            .sum();
        self.retain(shallow_weight)?;
        let head = self.resolve_head(format)?;
        let mut coverage = BTreeSet::new();
        let mut commits = self.walk_commits(&head, format, &shallow_boundaries, &mut coverage)?;
        drop(shallow_boundaries);
        self.release(shallow_weight);
        self.charge_sort(commits.len())?;
        commits.sort_by(|left, right| left.oid.cmp(&right.oid));
        let head_tree = commits
            .iter()
            .find(|commit| commit.oid == head)
            .ok_or(HistoryError::Malformed("HEAD commit was not retained"))?
            .tree
            .clone();
        let commit_map = commits
            .iter()
            .map(|commit| (commit.oid.clone(), commit))
            .collect::<BTreeMap<_, _>>();
        let commit_map_weight = commit_map
            .keys()
            .map(|oid| BTREE_ENTRY_WEIGHT + object_id_weight(oid) + size_of::<&HistoryCommit>())
            .sum();
        self.retain(commit_map_weight)?;
        for commit in &commits {
            self.step(1)?;
            self.tree(&commit.tree, format)?;
        }
        let head_tree_entries = self.tree(&head_tree, format)?;
        let scope = self.scope(&head_tree_entries, &mut coverage)?;
        let scope_weight = scope
            .iter()
            .map(|path| BTREE_ENTRY_WEIGHT + path_weight(path))
            .sum();
        self.retain(scope_weight)?;
        let scope_digest = digest_paths(scope.iter());
        let request_digest = digest_request(self.request);
        if scope.len() > self.options.max_nodes {
            return Err(HistoryError::BoundExceeded(HistoryBound::Nodes));
        }
        let mut edge_changes = BTreeMap::new();
        let mut rename_count = 0_usize;
        let mut change_count = 0_usize;
        let mut raw_change_count = 0_usize;
        for commit in &commits {
            self.step(1)?;
            if commit.parents.is_empty() {
                let after = self.tree(&commit.tree, format)?;
                let changes = self.compare_trees(
                    commit,
                    None,
                    &BTreeMap::new(),
                    &after,
                    format,
                    &mut coverage,
                    self.options.max_changes.saturating_sub(change_count),
                    self.options
                        .max_raw_changes
                        .saturating_sub(raw_change_count),
                    self.options.max_renames.saturating_sub(rename_count),
                )?;
                rename_count += changes.renames.len();
                change_count += changes.raw.len() + changes.renames.len();
                raw_change_count += changes.raw_records;
                edge_changes.insert((commit.oid.clone(), None), changes);
            } else {
                for parent in &commit.parents {
                    self.step(1)?;
                    let Some(parent_commit) = commit_map.get(parent) else {
                        continue;
                    };
                    let before = self.tree(&parent_commit.tree, format)?;
                    let after = self.tree(&commit.tree, format)?;
                    let changes = self.compare_trees(
                        commit,
                        Some(parent),
                        &before,
                        &after,
                        format,
                        &mut coverage,
                        self.options.max_changes.saturating_sub(change_count),
                        self.options
                            .max_raw_changes
                            .saturating_sub(raw_change_count),
                        self.options.max_renames.saturating_sub(rename_count),
                    )?;
                    rename_count += changes.renames.len();
                    change_count += changes.raw.len() + changes.renames.len();
                    raw_change_count += changes.raw_records;
                    edge_changes.insert((commit.oid.clone(), Some(parent.clone())), changes);
                }
            }
        }
        if rename_count > self.options.max_renames {
            return Err(HistoryError::BoundExceeded(HistoryBound::Renames));
        }
        let edge_values_weight = edge_changes
            .values()
            .map(|changes| changes.weight)
            .sum::<usize>();
        let edge_map_weight = edge_changes
            .keys()
            .map(|(commit, parent)| {
                BTREE_ENTRY_WEIGHT
                    + size_of::<(ObjectId, Option<ObjectId>)>()
                    + commit.as_str().len()
                    + parent.as_ref().map_or(0, |oid| oid.as_str().len())
                    + size_of::<EdgeChanges>()
            })
            .sum::<usize>();
        self.retain(edge_map_weight)?;
        let (mut changes, mut renames) = self.materialize_changes(
            &head,
            &head_tree_entries,
            &commit_map,
            &edge_changes,
            format,
        )?;
        drop(edge_changes);
        self.release(edge_values_weight.saturating_add(edge_map_weight));
        self.charge_sort(changes.len())?;
        changes.sort();
        changes.dedup();
        self.charge_sort(renames.len())?;
        renames.sort();
        renames.dedup();
        if renames.len() > self.options.max_provenance {
            return Err(HistoryError::BoundExceeded(HistoryBound::Provenance));
        }
        let mut changes_by_commit = BTreeMap::<ObjectId, BTreeSet<RootRelativePath>>::new();
        for change in &changes {
            self.step(1)?;
            if let Some(current) = &change.current_path {
                changes_by_commit
                    .entry(change.commit.clone())
                    .or_default()
                    .insert(current.clone());
            }
        }
        let changes_by_commit_weight = changes_by_commit
            .iter()
            .map(|(oid, paths)| {
                oid.as_str().len()
                    + size_of::<ObjectId>()
                    + BTREE_ENTRY_WEIGHT
                    + size_of::<BTreeSet<RootRelativePath>>()
                    + paths
                        .iter()
                        .map(|path| BTREE_ENTRY_WEIGHT + path_weight(path))
                        .sum::<usize>()
            })
            .sum();
        self.retain(changes_by_commit_weight)?;
        let changed_with = if self.request.include_changed_with {
            self.changed_with(
                &head,
                &head_tree_entries,
                &scope,
                &commits,
                &changes_by_commit,
                self.options.max_provenance - renames.len(),
            )?
        } else {
            Vec::new()
        };
        drop(changes_by_commit);
        self.release(changes_by_commit_weight);
        drop(scope);
        self.release(scope_weight);
        let blame_hunks = self.blame(
            &head,
            format,
            &commit_map,
            &head_tree_entries,
            &mut coverage,
            self.options
                .max_provenance
                .saturating_sub(changed_with.len().saturating_add(renames.len())),
        )?;
        drop(commit_map);
        self.release(commit_map_weight);
        if changed_with
            .len()
            .saturating_add(blame_hunks.len())
            .saturating_add(renames.len())
            > self.options.max_provenance
        {
            return Err(HistoryError::BoundExceeded(HistoryBound::Provenance));
        }
        if self.request.include_changed_with {
            coverage.insert(CoverageRecord {
                area: CoverageArea::CoChange,
                status: if shallow {
                    CoverageStatus::ObservedPartial
                } else {
                    CoverageStatus::Complete
                },
                detail: if shallow {
                    "co-change evidence is limited to observed shallow history"
                } else {
                    "committed co-change excludes root and merge commits; worktree blame is included only when requested"
                },
                omitted_count: 0,
            });
        } else {
            coverage.insert(CoverageRecord {
                area: CoverageArea::CoChange,
                status: CoverageStatus::Unavailable,
                detail: "co-change was not requested",
                omitted_count: 0,
            });
        }
        coverage.insert(CoverageRecord {
            area: CoverageArea::Commits,
            status: if shallow {
                CoverageStatus::ObservedPartial
            } else {
                CoverageStatus::Complete
            },
            detail: if shallow {
                "reachable history ends at a shallow boundary"
            } else {
                "reachable commit DAG extracted"
            },
            omitted_count: 0,
        });
        if !coverage
            .iter()
            .any(|item| item.area == CoverageArea::Renames)
        {
            coverage.insert(CoverageRecord {
                area: CoverageArea::Renames,
                status: CoverageStatus::ObservedPartial,
                detail: "only unique exact blob and mode renames are extracted",
                omitted_count: 0,
            });
        }
        if self.request.blame_paths.is_empty() {
            coverage.insert(CoverageRecord {
                area: CoverageArea::Blame,
                status: CoverageStatus::Unavailable,
                detail: "blame was not requested",
                omitted_count: 0,
            });
        }
        if shallow {
            let prior = std::mem::take(&mut coverage);
            for (area, partial_detail, unavailable_detail) in [
                (
                    CoverageArea::Commits,
                    "reachable history ends at an exact shallow boundary",
                    "commit history is unavailable",
                ),
                (
                    CoverageArea::Renames,
                    "rename evidence is limited to observed shallow history",
                    "rename evidence is unavailable",
                ),
                (
                    CoverageArea::CoChange,
                    "co-change evidence is limited to observed shallow history",
                    "co-change was not requested",
                ),
                (
                    CoverageArea::Blame,
                    "blame evidence is limited to observed shallow history",
                    "blame was not requested",
                ),
            ] {
                let omitted_count = prior
                    .iter()
                    .filter(|item| item.area == area)
                    .map(|item| item.omitted_count)
                    .sum();
                let unavailable = prior
                    .iter()
                    .any(|item| item.area == area && item.status == CoverageStatus::Unavailable);
                coverage.insert(CoverageRecord {
                    area,
                    status: if unavailable {
                        CoverageStatus::Unavailable
                    } else {
                        CoverageStatus::ObservedPartial
                    },
                    detail: if unavailable {
                        unavailable_detail
                    } else {
                        partial_detail
                    },
                    omitted_count,
                });
            }
        }
        let coverage = coverage.into_iter().collect::<Vec<_>>();
        let options_digest = digest_options(self.options);
        let mut extractor = blake3::Hasher::new();
        extractor.update(b"kit-history-extractor-v2\0");
        extractor.update(HISTORY_EXTRACTOR_POLICY.as_bytes());
        extractor.update(&self.git_implementation.digest());
        let extractor_digest = *extractor.finalize().as_bytes();
        let digest_work = graph_weight(
            &commits,
            &changes,
            &renames,
            &blame_hunks,
            &changed_with,
            &coverage,
        );
        self.step(digest_work)?;
        let content_digest = digest_content(
            &head,
            &head_tree,
            format,
            shallow_digest,
            scope_digest,
            request_digest,
            options_digest,
            &commits,
            &changes,
            &renames,
            &blame_hunks,
            &changed_with,
            &coverage,
            extractor_digest,
        );
        let snapshot_digest = digest_snapshot(
            self.index.revision(),
            &head,
            request_digest,
            options_digest,
            content_digest,
            &blame_hunks,
        );
        let blame_hunks: Arc<[BlameHunk]> = blame_hunks.into();
        let logical_bytes = graph_weight(
            &commits,
            &changes,
            &renames,
            &blame_hunks,
            &changed_with,
            &coverage,
        )
        .saturating_add(self.index.digest().to_string().len())
        .saturating_add(head.as_str().len())
        .saturating_add(head_tree.as_str().len())
        .saturating_add(self.git_implementation.executable.capacity());
        self.retain(
            size_of::<HistoryGraph>()
                .saturating_add(head.as_str().len())
                .saturating_add(head_tree.as_str().len())
                .saturating_add(size_of_val(coverage.as_slice())),
        )?;
        self.runner.before_prune_cache()?;
        self.prune_cache(&commits, format)?;
        let graph = HistoryGraph {
            revision: self.index.revision(),
            workspace_digest: self.index.digest().to_string(),
            index_digest: *self.index.index_digest(),
            head,
            head_tree,
            object_format: format,
            shallow_digest,
            scope_digest,
            request_digest,
            commits,
            changes,
            renames,
            blame_hunks,
            changed_with,
            coverage,
            content_digest,
            snapshot_digest,
            extractor_digest,
            git_implementation: self.git_implementation.clone(),
            options_digest,
            logical_bytes,
        };
        if self.resolve_head(format)? != graph.head
            || self.object_format()? != format
            || digest_shallow(&self.shallow_boundaries(format)?) != shallow_digest
        {
            return Err(HistoryError::Malformed(
                "Git HEAD, object format, or shallow boundary changed during extraction",
            ));
        }
        self.finish_repository_validation()?;
        Ok(BuildOutput {
            graph,
            tree_cache: std::mem::take(&mut self.tree_cache),
            blob_cache: std::mem::take(&mut self.blob_cache),
            cache_clock: self.cache_clock,
            cache_bytes: self.cache_bytes,
            metrics: self.metrics.clone(),
        })
    }

    fn object_format(&mut self) -> Result<ObjectFormat, HistoryError> {
        match self
            .text(
                HistoryGitCommand::ObjectFormat,
                HistoryBound::CommandOutputBytes,
            )?
            .as_str()
        {
            "sha1" => Ok(ObjectFormat::Sha1),
            "sha256" => Ok(ObjectFormat::Sha256),
            _ => Err(HistoryError::Unavailable(
                "Git SHA-1 or SHA-256 object format",
            )),
        }
    }

    fn resolve_head(&mut self, format: ObjectFormat) -> Result<ObjectId, HistoryError> {
        let value = self
            .text(HistoryGitCommand::Head, HistoryBound::CommandOutputBytes)
            .map_err(|error| match error {
                HistoryError::Git { .. } => {
                    HistoryError::Unavailable("repository has no valid HEAD commit")
                }
                other => other,
            })?;
        ObjectId::parse(&value, format)
    }

    fn shallow_boundaries(
        &mut self,
        format: ObjectFormat,
    ) -> Result<BTreeSet<ObjectId>, HistoryError> {
        let bytes = self.command(
            HistoryGitCommand::ShallowBoundaries,
            HistoryBound::CommandOutputBytes,
        )?;
        let mut boundaries = BTreeSet::new();
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            self.step(line.len().saturating_add(1))?;
            if boundaries.len() == self.options.max_commits {
                return Err(HistoryError::BoundExceeded(HistoryBound::Commits));
            }
            let value = std::str::from_utf8(line)
                .map_err(|_| HistoryError::Malformed("shallow boundary is not ASCII"))?;
            boundaries.insert(ObjectId::parse(value.trim_end_matches('\r'), format)?);
        }
        Ok(boundaries)
    }

    fn walk_commits(
        &mut self,
        head: &ObjectId,
        format: ObjectFormat,
        shallow_boundaries: &BTreeSet<ObjectId>,
        coverage: &mut BTreeSet<CoverageRecord>,
    ) -> Result<Vec<HistoryCommit>, HistoryError> {
        let mut queue = VecDeque::from([head.clone()]);
        let mut seen = BTreeSet::new();
        let mut commits = Vec::new();
        let mut parent_count = 0_usize;
        while let Some(oid) = queue.pop_front() {
            self.step(1)?;
            if !seen.insert(oid.clone()) {
                continue;
            }
            if seen.len() > self.options.max_commits {
                return Err(HistoryError::BoundExceeded(HistoryBound::Commits));
            }
            let bytes = match self.command(
                HistoryGitCommand::Commit(oid.clone()),
                HistoryBound::ObjectBytes,
            ) {
                Ok(bytes) => bytes,
                Err(HistoryError::Git { .. }) if oid == *head => {
                    return Err(HistoryError::MissingObject(oid));
                }
                Err(HistoryError::Git { .. }) => return Err(HistoryError::MissingObject(oid)),
                Err(error) => return Err(error),
            };
            let commit = parse_commit(
                oid,
                &bytes,
                format,
                self.options.max_parents.saturating_sub(parent_count),
                |staged| {
                    self.step(1)?;
                    self.stage(bytes.len().saturating_add(staged))
                },
            )?;
            parent_count = parent_count
                .checked_add(commit.parents.len())
                .filter(|count| *count <= self.options.max_parents)
                .ok_or(HistoryError::BoundExceeded(HistoryBound::Parents))?;
            if shallow_boundaries.contains(&commit.oid) {
                coverage.insert(CoverageRecord {
                    area: CoverageArea::Commits,
                    status: CoverageStatus::ObservedPartial,
                    detail: "reachable history ends at an exact shallow boundary",
                    omitted_count: 0,
                });
            } else {
                queue.extend(commit.parents.iter().cloned());
            }
            self.metrics.parsed_objects += 1;
            self.retain(commit_weight(&commit))?;
            commits.push(commit);
        }
        Ok(commits)
    }

    fn tree(&mut self, oid: &ObjectId, format: ObjectFormat) -> Result<Arc<Tree>, HistoryError> {
        self.cache_clock = self.cache_clock.wrapping_add(1);
        if let Some(entry) = self.tree_cache.get_mut(oid) {
            entry.used = self.cache_clock;
            let tree = Arc::clone(&entry.value);
            self.metrics.reused_objects += 1;
            self.step(1)?;
            return Ok(tree);
        }
        let bytes = self
            .command(
                HistoryGitCommand::Tree(oid.clone()),
                HistoryBound::CommandOutputBytes,
            )
            .map_err(|error| match error {
                HistoryError::Git { .. } => HistoryError::MissingObject(oid.clone()),
                other => other,
            })?;
        let output_bytes = bytes.len();
        let tree = parse_tree(&bytes, format, self.options.max_paths, |staged| {
            self.step(1)?;
            self.stage(output_bytes.saturating_add(staged))
        })?;
        let weight = tree_weight(oid, &tree);
        self.make_cache_room(weight)?;
        self.retain(weight)?;
        let tree = Arc::new(tree);
        self.cache_bytes += weight;
        self.tree_cache.insert(
            oid.clone(),
            CacheEntry {
                value: Arc::clone(&tree),
                used: self.cache_clock,
                weight,
            },
        );
        self.metrics.parsed_objects += 1;
        Ok(tree)
    }

    fn blob(&mut self, oid: &ObjectId) -> Result<Arc<BlobData>, HistoryError> {
        self.cache_clock = self.cache_clock.wrapping_add(1);
        if let Some(entry) = self.blob_cache.get_mut(oid) {
            entry.used = self.cache_clock;
            let blob = Arc::clone(&entry.value);
            self.metrics.reused_objects += 1;
            self.step(1)?;
            return Ok(blob);
        }
        let bytes = self
            .command(
                HistoryGitCommand::Blob(oid.clone()),
                HistoryBound::ObjectBytes,
            )
            .map_err(|error| match error {
                HistoryError::Git { .. } => HistoryError::MissingObject(oid.clone()),
                other => other,
            })?;
        let output_bytes = bytes.len();
        let lines = line_ranges(&bytes, self.options.max_blame_lines, |staged| {
            self.step(1)?;
            self.stage(output_bytes.saturating_add(staged))
        })?;
        let data = BlobData { bytes, lines };
        let weight = blob_weight(oid, &data);
        self.make_cache_room(weight)?;
        self.retain(weight)?;
        let data = Arc::new(data);
        self.cache_bytes += weight;
        self.blob_cache.insert(
            oid.clone(),
            CacheEntry {
                value: Arc::clone(&data),
                used: self.cache_clock,
                weight,
            },
        );
        self.metrics.parsed_objects += 1;
        Ok(data)
    }

    fn make_cache_room(&mut self, incoming: usize) -> Result<(), HistoryError> {
        if incoming > self.options.max_cache_bytes {
            return Err(HistoryError::BoundExceeded(HistoryBound::CacheBytes));
        }
        if self.tree_cache.len().saturating_add(self.blob_cache.len())
            < self.options.max_cache_entries
            && self.cache_bytes.saturating_add(incoming) <= self.options.max_cache_bytes
        {
            return Ok(());
        }
        let candidate_count = self
            .tree_cache
            .iter()
            .filter(|(_, entry)| Arc::strong_count(&entry.value) == 1)
            .count()
            .checked_add(
                self.blob_cache
                    .iter()
                    .filter(|(_, entry)| Arc::strong_count(&entry.value) == 1)
                    .count(),
            )
            .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
        let candidate_key_bytes = self
            .tree_cache
            .iter()
            .filter(|(_, entry)| Arc::strong_count(&entry.value) == 1)
            .map(|(oid, _)| oid.capacity())
            .chain(
                self.blob_cache
                    .iter()
                    .filter(|(_, entry)| Arc::strong_count(&entry.value) == 1)
                    .map(|(oid, _)| oid.capacity()),
            )
            .try_fold(0_usize, |bytes, capacity| bytes.checked_add(capacity))
            .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
        let candidate_bytes = candidate_count
            .checked_mul(size_of::<(u64, u8, ObjectId)>())
            .and_then(|bytes| bytes.checked_add(candidate_key_bytes))
            .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
        self.stage(candidate_bytes)?;
        if candidate_count != 0 {
            self.runner.staging_allocation(
                StagingAllocation::CacheEvictionCandidates,
                self.metrics.peak_staging_bytes,
            );
        }
        let mut candidates = Vec::with_capacity(candidate_count);
        candidates.extend(
            self.tree_cache
                .iter()
                .filter(|(_, entry)| Arc::strong_count(&entry.value) == 1)
                .map(|(oid, entry)| (entry.used, 0_u8, oid.clone()))
                .chain(
                    self.blob_cache
                        .iter()
                        .filter(|(_, entry)| Arc::strong_count(&entry.value) == 1)
                        .map(|(oid, entry)| (entry.used, 1_u8, oid.clone())),
                ),
        );
        let comparisons = std::cell::Cell::new(0_usize);
        candidates.sort_by(|left, right| {
            comparisons.set(comparisons.get().saturating_add(1));
            left.cmp(right)
        });
        self.step(comparisons.get())?;
        let mut candidates = candidates.into_iter();
        while self.tree_cache.len().saturating_add(self.blob_cache.len())
            >= self.options.max_cache_entries
            || self.cache_bytes.saturating_add(incoming) > self.options.max_cache_bytes
        {
            let Some((_, kind, oid)) = candidates.next() else {
                return Err(HistoryError::BoundExceeded(HistoryBound::CacheEntries));
            };
            let weight = if kind == 0 {
                self.tree_cache
                    .remove(&oid)
                    .expect("selected tree cache entry")
                    .weight
            } else {
                self.blob_cache
                    .remove(&oid)
                    .expect("selected blob cache entry")
                    .weight
            };
            self.cache_bytes = self.cache_bytes.saturating_sub(weight);
            self.step(1)?;
        }
        Ok(())
    }

    fn prune_cache(
        &mut self,
        commits: &[HistoryCommit],
        format: ObjectFormat,
    ) -> Result<(), HistoryError> {
        let mut prune_sets_upper = 0_usize;
        for commit in commits {
            self.step(1)?;
            prune_sets_upper = prune_sets_upper
                .checked_add(BTREE_ENTRY_WEIGHT + object_id_weight(&commit.tree))
                .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
            for entry in self.tree(&commit.tree, format)?.values() {
                self.step(1)?;
                prune_sets_upper = prune_sets_upper
                    .checked_add(BTREE_ENTRY_WEIGHT + object_id_weight(&entry.blob))
                    .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
            }
        }
        self.retain(prune_sets_upper)?;
        self.runner.staging_allocation(
            StagingAllocation::PruneSets,
            self.metrics.peak_staging_bytes,
        );
        let trees = commits
            .iter()
            .map(|commit| commit.tree.clone())
            .collect::<BTreeSet<_>>();
        let mut blobs = BTreeSet::new();
        for oid in &trees {
            for entry in self.tree(oid, format)?.values() {
                self.step(1)?;
                if blobs.len() == self.options.max_paths && !blobs.contains(&entry.blob) {
                    return Err(HistoryError::BoundExceeded(HistoryBound::Paths));
                }
                blobs.insert(entry.blob.clone());
            }
        }
        debug_assert!(
            trees
                .iter()
                .chain(blobs.iter())
                .map(|oid| BTREE_ENTRY_WEIGHT + object_id_weight(oid))
                .sum::<usize>()
                <= prune_sets_upper
        );
        self.tree_cache.retain(|oid, entry| {
            if trees.contains(oid) {
                true
            } else {
                self.cache_bytes = self.cache_bytes.saturating_sub(entry.weight);
                false
            }
        });
        self.blob_cache.retain(|oid, entry| {
            if blobs.contains(oid) {
                true
            } else {
                self.cache_bytes = self.cache_bytes.saturating_sub(entry.weight);
                false
            }
        });
        self.release(prune_sets_upper);
        Ok(())
    }

    fn scope(
        &mut self,
        head_tree: &Tree,
        coverage: &mut BTreeSet<CoverageRecord>,
    ) -> Result<BTreeSet<RootRelativePath>, HistoryError> {
        if !self.request.include_changed_with {
            if !self.request.scope.is_empty() {
                return Err(HistoryError::InvalidRequest(
                    "history scope requires changed-with extraction",
                ));
            }
            return Ok(BTreeSet::new());
        }
        let scope: BTreeSet<RootRelativePath> = if self.request.scope.is_empty() {
            let mut omitted = 0;
            let scope = self
                .index
                .entries()
                .iter()
                .filter(|entry| entry.kind == crate::workspace::revision::EntryKind::File)
                .filter_map(|entry| {
                    let path = entry
                        .path
                        .to_str()
                        .and_then(|path| RootRelativePath::parse(path, 256 * 1024).ok());
                    match path {
                        Some(path)
                            if head_tree.contains_key(&path)
                                && entry.content_state == ContentState::Text =>
                        {
                            Some(path)
                        }
                        Some(_) => {
                            omitted += 1;
                            None
                        }
                        None => None,
                    }
                })
                .collect();
            if omitted != 0 {
                coverage.insert(CoverageRecord {
                    area: CoverageArea::CoChange,
                    status: CoverageStatus::ObservedPartial,
                    detail: "untracked or non-text indexed files were omitted from default co-change scope",
                    omitted_count: omitted,
                });
            }
            scope
        } else {
            for path in &self.request.scope {
                if !head_tree.contains_key(path) {
                    return Err(HistoryError::SelectorNoMatch(path.clone()));
                }
            }
            let mut omitted = 0;
            let scope = self
                .request
                .scope
                .iter()
                .filter_map(|path| {
                    let available = self.index.entries().iter().any(|entry| {
                        entry.path == Path::new(path.as_str())
                            && entry.kind == crate::workspace::revision::EntryKind::File
                            && entry.content_state == ContentState::Text
                    });
                    if available {
                        Some(path.clone())
                    } else {
                        omitted += 1;
                        None
                    }
                })
                .collect();
            if omitted != 0 {
                coverage.insert(CoverageRecord {
                    area: CoverageArea::CoChange,
                    status: CoverageStatus::Unavailable,
                    detail: "explicit co-change path has no current indexed UTF-8 content",
                    omitted_count: omitted,
                });
            }
            scope
        };
        self.step(scope.len())?;
        Ok(scope)
    }

    #[allow(clippy::too_many_arguments)]
    fn compare_trees(
        &mut self,
        commit: &HistoryCommit,
        parent: Option<&ObjectId>,
        before: &Tree,
        after: &Tree,
        _format: ObjectFormat,
        coverage: &mut BTreeSet<CoverageRecord>,
        max_changes: usize,
        max_raw_changes: usize,
        max_renames: usize,
    ) -> Result<EdgeChanges, HistoryError> {
        self.step(before.len().saturating_add(after.len()))?;
        let mut preflight_candidates = 0_usize;
        let mut preflight_bytes = size_of::<EdgeChanges>();
        for (path, old) in before {
            match after.get(path) {
                None => {
                    preflight_candidates = preflight_candidates
                        .checked_add(1)
                        .ok_or(HistoryError::BoundExceeded(HistoryBound::RawChanges))?;
                    preflight_bytes = preflight_bytes
                        .checked_add(
                            BTREE_ENTRY_WEIGHT
                                + size_of::<(ObjectId, String)>()
                                + old.blob.as_str().len()
                                + old.mode.len()
                                + size_of::<Vec<RootRelativePath>>()
                                + path_weight(path),
                        )
                        .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
                }
                Some(new) if new != old => {
                    preflight_candidates = preflight_candidates
                        .checked_add(1)
                        .ok_or(HistoryError::BoundExceeded(HistoryBound::RawChanges))?;
                    preflight_bytes = preflight_bytes
                        .checked_add(size_of::<(RootRelativePath, ChangeKind)>() + path.capacity())
                        .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
                }
                Some(_) => {}
            }
        }
        for (path, new) in after {
            if !before.contains_key(path) {
                preflight_candidates = preflight_candidates
                    .checked_add(1)
                    .ok_or(HistoryError::BoundExceeded(HistoryBound::RawChanges))?;
                preflight_bytes = preflight_bytes
                    .checked_add(
                        BTREE_ENTRY_WEIGHT
                            + size_of::<(ObjectId, String)>()
                            + new.blob.as_str().len()
                            + new.mode.len()
                            + size_of::<Vec<RootRelativePath>>()
                            + path_weight(path),
                    )
                    .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
            }
        }
        if preflight_candidates > max_raw_changes {
            return Err(HistoryError::BoundExceeded(HistoryBound::RawChanges));
        }
        self.stage(preflight_bytes)?;
        let mut deleted = BTreeMap::<(ObjectId, String), Vec<RootRelativePath>>::new();
        let mut added = BTreeMap::<(ObjectId, String), Vec<RootRelativePath>>::new();
        let mut raw = Vec::new();
        let mut candidate_count = 0_usize;
        for (path, old) in before {
            self.step(1)?;
            match after.get(path) {
                None => {
                    candidate_count = candidate_count
                        .checked_add(1)
                        .filter(|count| *count <= max_raw_changes)
                        .ok_or(HistoryError::BoundExceeded(HistoryBound::RawChanges))?;
                    deleted
                        .entry((old.blob.clone(), old.mode.clone()))
                        .or_default()
                        .push(path.clone());
                }
                Some(new) if new != old => {
                    candidate_count = candidate_count
                        .checked_add(1)
                        .filter(|count| *count <= max_raw_changes)
                        .ok_or(HistoryError::BoundExceeded(HistoryBound::RawChanges))?;
                    raw.push((path.clone(), ChangeKind::Modified));
                }
                Some(_) => {}
            }
        }
        for (path, new) in after {
            self.step(1)?;
            if !before.contains_key(path) {
                candidate_count = candidate_count
                    .checked_add(1)
                    .filter(|count| *count <= max_raw_changes)
                    .ok_or(HistoryError::BoundExceeded(HistoryBound::RawChanges))?;
                added
                    .entry((new.blob.clone(), new.mode.clone()))
                    .or_default()
                    .push(path.clone());
            }
        }
        let mut renames = Vec::new();
        let rename_keys_upper = deleted
            .keys()
            .chain(added.keys())
            .try_fold(0_usize, |bytes, (oid, mode)| {
                bytes
                    .checked_add(BTREE_ENTRY_WEIGHT + size_of::<(ObjectId, String)>())
                    .and_then(|bytes| bytes.checked_add(oid.capacity()))
                    .and_then(|bytes| bytes.checked_add(mode.capacity()))
            })
            .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
        self.retain(rename_keys_upper)?;
        self.stage(preflight_bytes)?;
        if rename_keys_upper != 0 {
            self.runner.staging_allocation(
                StagingAllocation::RenameKeys,
                self.metrics.peak_staging_bytes,
            );
        }
        let keys = deleted
            .keys()
            .chain(added.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for key in keys {
            self.step(1)?;
            let removed = deleted.remove(&key).unwrap_or_default();
            let inserted = added.remove(&key).unwrap_or_default();
            if removed.len() == 1 && inserted.len() == 1 && parent.is_some() {
                if renames.len() == max_renames {
                    return Err(HistoryError::BoundExceeded(HistoryBound::Renames));
                }
                renames.push(RenameDraft {
                    from: removed[0].clone(),
                    to: inserted[0].clone(),
                    blob: key.0,
                    mode: key.1,
                });
            } else {
                if !removed.is_empty() && !inserted.is_empty() {
                    coverage.insert(CoverageRecord {
                        area: CoverageArea::Renames,
                        status: CoverageStatus::ObservedPartial,
                        detail: "ambiguous exact rename candidates were retained as add and delete",
                        omitted_count: 0,
                    });
                }
                raw.extend(removed.into_iter().map(|path| (path, ChangeKind::Deleted)));
                raw.extend(inserted.into_iter().map(|path| (path, ChangeKind::Added)));
            }
        }
        self.release(rename_keys_upper);
        self.stage(edge_change_staging_weight(
            &raw, &renames, &deleted, &added,
        )?)?;
        self.charge_sort(raw.len())?;
        raw.sort();
        self.charge_sort(renames.len())?;
        renames.sort_by(|left, right| (&left.from, &left.to).cmp(&(&right.from, &right.to)));
        let rename_forward = renames
            .iter()
            .map(|rename| (rename.from.clone(), rename.to.clone()))
            .collect::<BTreeMap<_, _>>();
        self.stage(
            rename_forward
                .iter()
                .map(|(from, to)| {
                    BTREE_ENTRY_WEIGHT
                        + size_of::<RootRelativePath>() * 2
                        + from.as_str().len()
                        + to.as_str().len()
                })
                .sum(),
        )?;
        let count = raw.len().saturating_add(renames.len());
        if count > max_changes {
            return Err(HistoryError::BoundExceeded(HistoryBound::Changes));
        }
        let mut changes = EdgeChanges {
            raw,
            renames,
            rename_forward,
            raw_records: candidate_count,
            weight: 0,
        };
        changes.weight = edge_changes_weight(&changes);
        self.retain(changes.weight)?;
        let _ = (commit, parent);
        Ok(changes)
    }

    fn materialize_changes(
        &mut self,
        head: &ObjectId,
        head_tree: &Tree,
        commits: &BTreeMap<ObjectId, &HistoryCommit>,
        edges: &BTreeMap<(ObjectId, Option<ObjectId>), EdgeChanges>,
        format: ObjectFormat,
    ) -> Result<(Vec<HistoryChange>, Vec<ExactRename>), HistoryError> {
        let mut children = BTreeMap::<ObjectId, Vec<ObjectId>>::new();
        for commit in commits.values() {
            self.step(1)?;
            for parent in &commit.parents {
                self.step(1)?;
                if commits.contains_key(parent) {
                    let entry = children.entry(parent.clone()).or_default();
                    if entry.len() == self.options.max_parents {
                        return Err(HistoryError::BoundExceeded(HistoryBound::Parents));
                    }
                    entry.push(commit.oid.clone());
                }
            }
        }
        for value in children.values_mut() {
            value.sort();
            value.dedup();
        }
        self.stage(map_paths_weight(&children))?;
        let output_count = edges.values().try_fold(0_usize, |count, edge| {
            count
                .checked_add(edge.raw.len())
                .and_then(|count| count.checked_add(edge.renames.len()))
                .ok_or(HistoryError::BoundExceeded(HistoryBound::Changes))
        })?;
        let rename_output_count = edges.values().try_fold(0_usize, |count, edge| {
            count
                .checked_add(edge.renames.len())
                .ok_or(HistoryError::BoundExceeded(HistoryBound::Renames))
        })?;
        let maximum_path_bytes = head_tree
            .keys()
            .map(RootRelativePath::capacity)
            .chain(edges.values().flat_map(|edge| {
                edge.raw.iter().map(|(path, _)| path.capacity()).chain(
                    edge.renames
                        .iter()
                        .flat_map(|rename| [rename.from.capacity(), rename.to.capacity()]),
                )
            }))
            .max()
            .unwrap_or(0);
        let maximum_oid_bytes = commits
            .keys()
            .map(ObjectId::capacity)
            .chain(
                edges
                    .values()
                    .flat_map(|edge| edge.renames.iter().map(|rename| rename.blob.capacity())),
            )
            .chain(std::iter::once(format.hex_len()))
            .max()
            .unwrap_or(format.hex_len());
        let maximum_mode_bytes = edges
            .values()
            .flat_map(|edge| edge.renames.iter().map(|rename| rename.mode.len()))
            .max()
            .unwrap_or(0);
        let output_reservation =
            edges
                .iter()
                .try_fold(0_usize, |bytes, ((_commit, parent), edge)| {
                    let bytes = edge.raw.iter().try_fold(bytes, |bytes, _| {
                        bytes
                            .checked_add(size_of::<HistoryChange>())
                            .and_then(|bytes| bytes.checked_add(maximum_oid_bytes))
                            .and_then(|bytes| {
                                bytes.checked_add(usize::from(parent.is_some()) * maximum_oid_bytes)
                            })
                            .and_then(|bytes| bytes.checked_add(2 * maximum_path_bytes))
                            .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))
                    })?;
                    edge.renames.iter().try_fold(bytes, |bytes, _| {
                        bytes
                            .checked_add(size_of::<HistoryChange>() + size_of::<ExactRename>())
                            .and_then(|bytes| bytes.checked_add(5 * maximum_oid_bytes))
                            .and_then(|bytes| bytes.checked_add(5 * maximum_path_bytes))
                            .and_then(|bytes| bytes.checked_add(maximum_mode_bytes))
                            .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))
                    })
                })?;
        self.retain(output_reservation)?;
        let mut changes = Vec::with_capacity(output_count);
        let mut renames = Vec::with_capacity(rename_output_count);
        for ((commit, parent), edge) in edges {
            self.step(1)?;
            for (path, kind) in &edge.raw {
                self.step(1)?;
                let identity_commit = if *kind == ChangeKind::Deleted {
                    parent.as_ref()
                } else {
                    Some(commit)
                };
                let current_path = match identity_commit {
                    Some(owner) => self.map_current_path(
                        owner, path, head, head_tree, commits, edges, &children, format,
                    )?,
                    None => None,
                };
                changes.push(HistoryChange {
                    commit: commit.clone(),
                    parent: parent.clone(),
                    path: path.clone(),
                    current_path,
                    kind: *kind,
                });
            }
            for rename in &edge.renames {
                self.step(1)?;
                let Some(parent) = parent else { continue };
                let current_path = self.map_current_path(
                    commit, &rename.to, head, head_tree, commits, edges, &children, format,
                )?;
                changes.push(HistoryChange {
                    commit: commit.clone(),
                    parent: Some(parent.clone()),
                    path: rename.to.clone(),
                    current_path: current_path.clone(),
                    kind: ChangeKind::Renamed,
                });
                renames.push(ExactRename {
                    commit: commit.clone(),
                    parent: parent.clone(),
                    from: rename.from.clone(),
                    to: rename.to.clone(),
                    current_path,
                    blob: rename.blob.clone(),
                    mode: rename.mode.clone(),
                    confidence_millis: 1_000,
                });
            }
        }
        if changes.len() > self.options.max_changes {
            return Err(HistoryError::BoundExceeded(HistoryBound::Changes));
        }
        if changes_weight(&changes).saturating_add(renames_weight(&renames)) > output_reservation {
            return Err(HistoryError::BoundExceeded(HistoryBound::StagingBytes));
        }
        Ok((changes, renames))
    }

    #[allow(clippy::too_many_arguments)]
    fn map_current_path(
        &mut self,
        start: &ObjectId,
        path: &RootRelativePath,
        head: &ObjectId,
        head_tree: &Tree,
        commits: &BTreeMap<ObjectId, &HistoryCommit>,
        edges: &BTreeMap<(ObjectId, Option<ObjectId>), EdgeChanges>,
        children: &BTreeMap<ObjectId, Vec<ObjectId>>,
        format: ObjectFormat,
    ) -> Result<Option<RootRelativePath>, HistoryError> {
        let mut queue = VecDeque::from([(start.clone(), path.clone())]);
        let mut seen = BTreeSet::new();
        let mut mapped = BTreeSet::new();
        while let Some((parent, parent_path)) = queue.pop_front() {
            self.step(1)?;
            let state = (parent.clone(), parent_path.clone());
            if seen.contains(&state) {
                continue;
            }
            self.stage(
                size_of::<ObjectId>()
                    + parent.as_str().len()
                    + size_of::<RootRelativePath>()
                    + parent_path.capacity()
                    + BTREE_ENTRY_WEIGHT,
            )?;
            seen.insert(state);
            if seen.len() > self.options.max_paths {
                return Err(HistoryError::BoundExceeded(HistoryBound::Paths));
            }
            if parent == *head {
                if head_tree.contains_key(&parent_path) {
                    mapped.insert(parent_path);
                    if mapped.len() > 1 {
                        return Ok(None);
                    }
                }
                continue;
            }
            for child in children.get(&parent).into_iter().flatten() {
                self.step(1)?;
                let edge = edges
                    .get(&(child.clone(), Some(parent.clone())))
                    .ok_or(HistoryError::Malformed("commit edge changes are missing"))?;
                let child_path = edge
                    .rename_forward
                    .get(&parent_path)
                    .cloned()
                    .unwrap_or_else(|| parent_path.clone());
                let child_commit = commits
                    .get(child)
                    .ok_or(HistoryError::Malformed("child commit is absent"))?;
                if self
                    .tree(&child_commit.tree, format)?
                    .contains_key(&child_path)
                {
                    queue.push_back((child.clone(), child_path));
                }
            }
        }
        Ok(mapped.into_iter().next())
    }

    fn changed_with(
        &mut self,
        head: &ObjectId,
        head_tree: &Tree,
        scope: &BTreeSet<RootRelativePath>,
        commits: &[HistoryCommit],
        changes: &BTreeMap<ObjectId, BTreeSet<RootRelativePath>>,
        max_provenance: usize,
    ) -> Result<Vec<ChangedWithFact>, HistoryError> {
        let mut pair_upper = 0_usize;
        let mut map_bytes_upper = 0_usize;
        let mut temporary_path_bytes = 0_usize;
        let mut population_work = 0_usize;
        let ranges_upper = scope.iter().try_fold(0_usize, |bytes, path| {
            bytes
                .checked_add(BTREE_ENTRY_WEIGHT + path_weight(path))
                .and_then(|bytes| bytes.checked_add(size_of::<CurrentAndCommittedRange>()))
                .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))
        })?;
        for commit in commits {
            if commit.parents.len() != 1 {
                continue;
            }
            let mut path_count = 0_usize;
            let mut path_bytes = 0_usize;
            for path in changes
                .get(&commit.oid)
                .into_iter()
                .flatten()
                .filter(|path| scope.contains(*path))
            {
                path_count = path_count
                    .checked_add(1)
                    .ok_or(HistoryError::BoundExceeded(HistoryBound::Paths))?;
                path_bytes = path_bytes
                    .checked_add(size_of::<RootRelativePath>())
                    .and_then(|bytes| bytes.checked_add(path.capacity()))
                    .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
            }
            self.step(
                path_count
                    .checked_add(1)
                    .ok_or(HistoryError::BoundExceeded(HistoryBound::Work))?,
            )?;
            let pairs = path_count
                .checked_mul(path_count.saturating_sub(1))
                .and_then(|count| count.checked_div(2))
                .ok_or(HistoryError::BoundExceeded(HistoryBound::Pairs))?;
            add_bounded(
                &mut pair_upper,
                pairs,
                self.options.max_pairs,
                HistoryBound::Pairs,
            )?;
            if pair_upper > self.options.max_edges {
                return Err(HistoryError::BoundExceeded(HistoryBound::Edges));
            }
            if pair_upper > max_provenance {
                return Err(HistoryError::BoundExceeded(HistoryBound::Provenance));
            }
            population_work = population_work
                .checked_add(pairs)
                .and_then(|work| work.checked_add(path_count))
                .and_then(|work| work.checked_add(1))
                .ok_or(HistoryError::BoundExceeded(HistoryBound::Work))?;
            let side_bytes = path_bytes
                .checked_add(
                    path_count
                        .checked_mul(BTREE_ENTRY_WEIGHT + size_of::<usize>())
                        .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?,
                )
                .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
            let pair_path_bytes = path_bytes
                .checked_mul(path_count.saturating_sub(1))
                .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
            let pair_bytes = pairs
                .checked_mul(
                    BTREE_ENTRY_WEIGHT
                        + 2 * size_of::<RootRelativePath>()
                        + BTREE_ENTRY_WEIGHT
                        + object_id_weight(&commit.oid),
                )
                .and_then(|bytes| bytes.checked_add(pair_path_bytes))
                .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
            map_bytes_upper = map_bytes_upper
                .checked_add(side_bytes)
                .and_then(|bytes| bytes.checked_add(pair_bytes))
                .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
            temporary_path_bytes = temporary_path_bytes.max(path_bytes);
        }
        map_bytes_upper = map_bytes_upper
            .checked_add(ranges_upper)
            .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
        if population_work
            > self
                .options
                .max_work
                .checked_sub(self.metrics.consumed_work)
                .ok_or(HistoryError::BoundExceeded(HistoryBound::Work))?
        {
            return Err(HistoryError::BoundExceeded(HistoryBound::Work));
        }
        self.metrics.cochange_preflight_staging_bytes = self
            .retained
            .checked_add(map_bytes_upper)
            .and_then(|bytes| bytes.checked_add(temporary_path_bytes))
            .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
        self.retain(map_bytes_upper)?;
        self.stage(temporary_path_bytes)?;

        let mut side_counts = BTreeMap::<RootRelativePath, usize>::new();
        let mut supports =
            BTreeMap::<(RootRelativePath, RootRelativePath), BTreeSet<ObjectId>>::new();
        let mut provenance_count = 0_usize;
        for commit in commits {
            if commit.parents.len() != 1 {
                continue;
            }
            let paths = changes
                .get(&commit.oid)
                .into_iter()
                .flatten()
                .filter(|path| scope.contains(*path))
                .cloned()
                .collect::<BTreeSet<_>>();
            self.step(
                paths
                    .len()
                    .checked_add(1)
                    .ok_or(HistoryError::BoundExceeded(HistoryBound::Work))?,
            )?;
            for path in &paths {
                if !side_counts.contains_key(path) {
                    self.runner.cochange_map_entry();
                }
                let count = side_counts.entry(path.clone()).or_default();
                *count = count
                    .checked_add(1)
                    .ok_or(HistoryError::BoundExceeded(HistoryBound::Work))?;
            }
            let paths = paths.into_iter().collect::<Vec<_>>();
            for left in 0..paths.len() {
                for right in left + 1..paths.len() {
                    let key = (paths[left].clone(), paths[right].clone());
                    if !supports.contains_key(&key) {
                        self.runner.cochange_map_entry();
                    }
                    let support = supports.entry(key).or_default();
                    if !support.contains(&commit.oid) {
                        support.insert(commit.oid.clone());
                        provenance_count = provenance_count
                            .checked_add(1)
                            .ok_or(HistoryError::BoundExceeded(HistoryBound::Provenance))?;
                    }
                    self.step(1)?;
                }
            }
        }
        let scope_digest = digest_paths(scope.iter());
        let ranges_weight = side_counts.keys().try_fold(0_usize, |bytes, path| {
            bytes
                .checked_add(BTREE_ENTRY_WEIGHT + path_weight(path))
                .and_then(|bytes| bytes.checked_add(size_of::<CurrentAndCommittedRange>()))
                .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))
        })?;
        debug_assert!(ranges_weight <= ranges_upper);
        let mut ranges = BTreeMap::new();
        for path in side_counts.keys() {
            let entry = self
                .index
                .entries()
                .iter()
                .find(|entry| entry.path == Path::new(path.as_str()))
                .ok_or(HistoryError::InvalidIndex(
                    "co-change path is absent from index",
                ))?;
            let current = entry.text().ok_or(HistoryError::InvalidIndex(
                "co-change path has no indexed UTF-8 content",
            ))?;
            let committed_entry = head_tree
                .get(path)
                .ok_or_else(|| HistoryError::SelectorNoMatch(path.clone()))?;
            let committed = self.blob(&committed_entry.blob)?;
            ranges.insert(
                path.clone(),
                CurrentAndCommittedRange {
                    current: whole_range(current.as_bytes()),
                    committed_blob: committed_entry.blob.clone(),
                    committed: whole_range(&committed.bytes),
                },
            );
        }
        let facts_weight =
            supports
                .iter()
                .try_fold(0_usize, |bytes, ((left, right), commits)| {
                    bytes
                        .checked_add(size_of::<ChangedWithFact>())
                        .and_then(|bytes| bytes.checked_add(left.as_str().len()))
                        .and_then(|bytes| bytes.checked_add(right.as_str().len()))
                        .and_then(|bytes| bytes.checked_add(head.as_str().len()))
                        .and_then(|bytes| {
                            bytes.checked_add("non-root-non-merge-once-per-commit-v1".len())
                        })
                        .and_then(|bytes| {
                            bytes.checked_add(commits.iter().map(object_id_weight).sum::<usize>())
                        })
                        .and_then(|bytes| bytes.checked_add(2 * head.as_str().len()))
                        .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))
                })?;
        self.retain(facts_weight)?;
        let mut facts = Vec::with_capacity(supports.len());
        for ((left, right), commits) in supports {
            let commits = commits.into_iter().collect::<Vec<_>>();
            let left_count = side_counts[&left];
            let right_count = side_counts[&right];
            let shared_count = commits.len();
            let strength_millis =
                changed_with_strength_millis(shared_count, left_count, right_count)?;
            let mut evidence = blake3::Hasher::new();
            evidence.update(b"kit-history-changed-with-evidence-v1\0");
            frame(&mut evidence, left.as_str().as_bytes());
            frame(&mut evidence, right.as_str().as_bytes());
            for commit in &commits {
                frame(&mut evidence, commit.as_str().as_bytes());
            }
            let provenance = ChangedWithProvenance {
                head: head.clone(),
                scope_digest,
                policy: CHANGED_WITH_POLICY,
                commits,
                count: shared_count,
                left_count,
                right_count,
                shared_count,
                evidence_digest: *evidence.finalize().as_bytes(),
                revision: self.index.revision(),
                left_range: ranges[&left].current,
                right_range: ranges[&right].current,
                left_committed_blob: ranges[&left].committed_blob.clone(),
                right_committed_blob: ranges[&right].committed_blob.clone(),
                left_committed_range: ranges[&left].committed,
                right_committed_range: ranges[&right].committed,
            };
            facts.push(ChangedWithFact {
                left,
                right,
                count: shared_count,
                strength_millis,
                extraction_confidence_millis: 1_000,
                provenance,
            });
        }
        if provenance_count > max_provenance {
            return Err(HistoryError::BoundExceeded(HistoryBound::Provenance));
        }
        if facts.len() > max_provenance {
            return Err(HistoryError::BoundExceeded(HistoryBound::Provenance));
        }
        debug_assert!(changed_with_weight(&facts) <= facts_weight);
        drop(ranges);
        self.release(map_bytes_upper);
        Ok(facts)
    }

    fn blame(
        &mut self,
        head: &ObjectId,
        format: ObjectFormat,
        commits: &BTreeMap<ObjectId, &HistoryCommit>,
        head_tree: &Tree,
        coverage: &mut BTreeSet<CoverageRecord>,
        max_hunks: usize,
    ) -> Result<Vec<BlameHunk>, HistoryError> {
        let mut hunks = Vec::new();
        let mut total_lines = 0_usize;
        for path in &self.request.blame_paths {
            let path_hunk_start = hunks.len();
            self.step(1)?;
            let entry = self
                .index
                .entries()
                .iter()
                .find(|entry| entry.path == Path::new(path.as_str()))
                .ok_or(HistoryError::InvalidRequest(
                    "blame path is absent from the metadata index",
                ))?;
            if entry.content_state != ContentState::Text {
                return Err(HistoryError::InvalidRequest(
                    "blame path has no indexed text",
                ));
            }
            let current = entry
                .text()
                .ok_or(HistoryError::InvalidIndex("indexed text is unavailable"))?
                .as_bytes();
            let committed = self.head_blob_bytes(head_tree, path)?;
            let output = self.command(
                HistoryGitCommand::Blame {
                    head: head.clone(),
                    path: path.clone(),
                },
                HistoryBound::CommandOutputBytes,
            )?;
            let remaining_lines = self.options.max_blame_lines.saturating_sub(total_lines);
            let output_bytes = output.len();
            self.retain(output_bytes)?;
            let mut temporary_weight = output_bytes;
            let lines = parse_blame(&output, format, path, remaining_lines, |staged| {
                self.step(1)?;
                self.stage(staged)
            })?;
            let lines_weight = lines
                .iter()
                .map(|line| {
                    size_of::<BlameLine>()
                        + line.commit.as_str().len()
                        + line.source_path.capacity()
                        + line.text.len()
                })
                .sum::<usize>();
            self.retain(lines_weight)?;
            temporary_weight = temporary_weight.saturating_add(lines_weight);
            total_lines = total_lines
                .checked_add(lines.len())
                .filter(|count| *count <= self.options.max_blame_lines)
                .ok_or(HistoryError::BoundExceeded(HistoryBound::BlameLines))?;
            if lines.len() != committed.lines.len() {
                return Err(HistoryError::Malformed(
                    "blame line count differs from HEAD blob",
                ));
            }
            let mut source_blobs = BTreeMap::<ObjectId, Arc<BlobData>>::new();
            let mut source_ids = BTreeMap::<ObjectId, BTreeMap<RootRelativePath, ObjectId>>::new();
            let map_weight = size_of_val(&source_blobs).saturating_add(size_of_val(&source_ids));
            self.retain(map_weight)?;
            temporary_weight = temporary_weight.saturating_add(map_weight);
            for (index, line) in lines.iter().enumerate() {
                self.step(line.text.len().saturating_add(1))?;
                if line.final_line != index + 1
                    || Some(line.text.as_slice()) != blob_line(&committed, index)
                {
                    return Err(HistoryError::Malformed(
                        "blame line bytes or range differ from HEAD blob",
                    ));
                }
                let commit = commits.get(&line.commit).ok_or(HistoryError::Malformed(
                    "blame source commit is outside retained history",
                ))?;
                let blob = if let Some(blob) = source_ids
                    .get(&line.commit)
                    .and_then(|paths| paths.get(&line.source_path))
                {
                    blob.clone()
                } else {
                    let tree = self.tree(&commit.tree, format)?;
                    let blob = tree
                        .get(&line.source_path)
                        .ok_or(HistoryError::Malformed(
                            "blame source path is absent from source tree",
                        ))?
                        .blob
                        .clone();
                    let source_id_weight = line.commit.as_str().len()
                        + line.source_path.capacity()
                        + blob.as_str().len()
                        + 2 * BTREE_ENTRY_WEIGHT
                        + 2 * size_of::<ObjectId>()
                        + size_of::<RootRelativePath>();
                    self.retain(source_id_weight)?;
                    temporary_weight = temporary_weight.saturating_add(source_id_weight);
                    source_ids
                        .entry(line.commit.clone())
                        .or_default()
                        .insert(line.source_path.clone(), blob.clone());
                    blob
                };
                if !source_blobs.contains_key(&blob) {
                    let source = self.blob(&blob)?;
                    let source_blob_weight = size_of::<ObjectId>()
                        + blob.as_str().len()
                        + size_of::<Arc<BlobData>>()
                        + BTREE_ENTRY_WEIGHT;
                    self.retain(source_blob_weight)?;
                    temporary_weight = temporary_weight.saturating_add(source_blob_weight);
                    source_blobs.insert(blob.clone(), source);
                }
                if line.original_line == 0
                    || blob_line(&source_blobs[&blob], line.original_line - 1)
                        != Some(line.text.as_slice())
                {
                    return Err(HistoryError::Malformed(
                        "blame source line does not match source blob",
                    ));
                }
            }
            let collapsed = collapse_blame(
                path,
                &lines,
                &committed.bytes,
                &source_ids,
                &source_blobs,
                self.index.revision(),
                max_hunks.saturating_sub(hunks.len()),
                |staged| {
                    self.step(1)?;
                    self.stage(staged)
                },
            )?;
            self.step(committed.bytes.len().saturating_add(lines.len()))?;
            let collapsed_weight = collapsed.iter().map(blame_hunk_weight).sum();
            self.retain(collapsed_weight)?;
            hunks.extend(collapsed);
            drop(source_blobs);
            drop(source_ids);
            drop(lines);
            drop(output);
            self.release(temporary_weight);
            if current != committed.bytes.as_slice() {
                hunks.truncate(path_hunk_start);
                self.release(collapsed_weight);
                total_lines = total_lines
                    .checked_add(line_count(current))
                    .filter(|count| *count <= self.options.max_blame_lines)
                    .ok_or(HistoryError::BoundExceeded(HistoryBound::BlameLines))?;
                if hunks.len() == max_hunks {
                    return Err(HistoryError::BoundExceeded(HistoryBound::Provenance));
                }
                let mut hunk = BlameHunk {
                    path: path.clone(),
                    range: whole_range(current),
                    source: BlameSource::Worktree,
                    source_commit: None,
                    source_path: path.clone(),
                    source_blob: None,
                    source_range: whole_range(current),
                    boundary: false,
                    confidence_millis: 1_000,
                    revision: self.index.revision(),
                    line_digest: *blake3::hash(current).as_bytes(),
                    evidence_digest: [0; 32],
                };
                hunk.evidence_digest = digest_blame_evidence(&hunk);
                self.retain(blame_hunk_weight(&hunk))?;
                hunks.push(hunk);
                coverage.insert(CoverageRecord {
                    area: CoverageArea::Blame,
                    status: CoverageStatus::ObservedPartial,
                    detail: "worktree overlay is explicit and included in content identity",
                    omitted_count: 0,
                });
            }
        }
        if !self.request.blame_paths.is_empty()
            && !coverage.iter().any(|item| item.area == CoverageArea::Blame)
        {
            coverage.insert(CoverageRecord {
                area: CoverageArea::Blame,
                status: CoverageStatus::Complete,
                detail: "requested HEAD blame paths verified",
                omitted_count: 0,
            });
        }
        hunks.sort();
        Ok(hunks)
    }

    fn head_blob_bytes(
        &mut self,
        tree: &Tree,
        path: &RootRelativePath,
    ) -> Result<Arc<BlobData>, HistoryError> {
        let blob = tree
            .get(path)
            .ok_or(HistoryError::InvalidRequest("path is not present at HEAD"))?
            .blob
            .clone();
        self.blob(&blob)
    }

    fn text(
        &mut self,
        command: HistoryGitCommand,
        bound: HistoryBound,
    ) -> Result<String, HistoryError> {
        let bytes = self.command(command, bound)?;
        let mut text = String::from_utf8(bytes)
            .map_err(|_| HistoryError::Malformed("Git output is not UTF-8"))?;
        text.truncate(text.trim_end_matches(['\r', '\n']).len());
        Ok(text)
    }

    fn command(
        &mut self,
        command: HistoryGitCommand,
        bound: HistoryBound,
    ) -> Result<Vec<u8>, HistoryError> {
        check_deadline(self.deadline)?;
        let remaining_output = self
            .options
            .max_output_bytes
            .saturating_sub(self.metrics.output_bytes);
        let stdout_bytes = self
            .options
            .max_command_output_bytes
            .min(if matches!(command, HistoryGitCommand::ShallowBoundaries) {
                usize::MAX
            } else {
                remaining_output
            })
            .min(if bound == HistoryBound::ObjectBytes {
                self.options.max_object_bytes
            } else {
                usize::MAX
            });
        let timeout = self.deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(HistoryError::BoundExceeded(HistoryBound::Time));
        }
        self.metrics.commands = self
            .metrics
            .commands
            .checked_add(1)
            .filter(|commands| *commands <= self.options.max_commands)
            .ok_or(HistoryError::BoundExceeded(HistoryBound::Commands))?;
        let policy_before = self.runner.policy_metrics().consumed_work();
        let started = Instant::now();
        let output = self
            .runner
            .run(
                &command,
                GitCommandLimits {
                    timeout,
                    stdout_bytes,
                    stderr_bytes: self.options.max_error_output_bytes,
                },
            )
            .map_err(|error| {
                command_error(&command, error, bound, stdout_bytes, remaining_output)
            })?;
        if self.runner.policy_metrics().consumed_work() == policy_before {
            self.step(1)?;
        } else {
            self.charge_policy()?;
        }
        if started.elapsed() > timeout {
            return Err(HistoryError::BoundExceeded(HistoryBound::Time));
        }
        if output.stdout.len() > stdout_bytes {
            return Err(HistoryError::BoundExceeded(
                if stdout_bytes == remaining_output {
                    HistoryBound::OutputBytes
                } else {
                    bound
                },
            ));
        }
        self.metrics.output_bytes = self
            .metrics
            .output_bytes
            .checked_add(output.stdout.len())
            .filter(|bytes| *bytes <= self.options.max_output_bytes)
            .ok_or(HistoryError::BoundExceeded(HistoryBound::OutputBytes))?;
        self.step(output.stdout.len())?;
        self.metrics.peak_staging_bytes = self
            .metrics
            .peak_staging_bytes
            .max(self.retained.saturating_add(output.stdout.len()));
        if self.metrics.peak_staging_bytes > self.options.max_staging_bytes {
            return Err(HistoryError::BoundExceeded(HistoryBound::StagingBytes));
        }
        Ok(output.stdout)
    }

    fn finish_repository_validation(&mut self) -> Result<(), HistoryError> {
        self.metrics.commands = self
            .metrics
            .commands
            .checked_add(1)
            .filter(|commands| *commands <= self.options.max_commands)
            .ok_or(HistoryError::BoundExceeded(HistoryBound::Commands))?;
        self.metrics.validation_scans = self
            .metrics
            .validation_scans
            .checked_add(1)
            .ok_or(HistoryError::BoundExceeded(HistoryBound::Commands))?;
        let policy_before = self.runner.policy_metrics().consumed_work();
        self.runner
            .finish_refresh(self.deadline)
            .map_err(repository_validation_error)?;
        if self.runner.policy_metrics().consumed_work() == policy_before {
            self.step(1)
        } else {
            self.charge_policy()
        }
    }

    fn charge_policy(&mut self) -> Result<(), HistoryError> {
        let policy = self.runner.policy_metrics();
        let added = policy
            .consumed_work()
            .checked_sub(self.policy_consumed_work)
            .ok_or(HistoryError::BoundExceeded(HistoryBound::Work))?;
        self.policy_consumed_work = policy.consumed_work();
        self.metrics.consumed_work = self
            .metrics
            .consumed_work
            .checked_add(added)
            .filter(|work| *work <= self.options.max_work)
            .ok_or(HistoryError::BoundExceeded(HistoryBound::Work))?;
        self.metrics.validation_scans = self.metrics.validation_scans.max(policy.policy_scans());
        self.metrics.peak_staging_bytes = self.metrics.peak_staging_bytes.max(
            self.retained
                .saturating_sub(policy.logical_bytes())
                .saturating_add(policy.peak_bytes()),
        );
        if self.metrics.peak_staging_bytes > self.options.max_staging_bytes {
            return Err(HistoryError::BoundExceeded(HistoryBound::StagingBytes));
        }
        Ok(())
    }

    fn step(&mut self, amount: usize) -> Result<(), HistoryError> {
        self.metrics.consumed_work = self
            .metrics
            .consumed_work
            .checked_add(amount)
            .filter(|work| *work <= self.options.max_work)
            .ok_or(HistoryError::BoundExceeded(HistoryBound::Work))?;
        check_deadline(self.deadline)
    }

    fn charge_sort(&mut self, len: usize) -> Result<(), HistoryError> {
        self.step(sort_comparison_bound(len)?)
    }

    fn retain(&mut self, amount: usize) -> Result<(), HistoryError> {
        self.retained = self
            .retained
            .checked_add(amount)
            .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
        self.metrics.peak_staging_bytes = self.metrics.peak_staging_bytes.max(self.retained);
        if self.metrics.peak_staging_bytes > self.options.max_staging_bytes {
            Err(HistoryError::BoundExceeded(HistoryBound::StagingBytes))
        } else {
            Ok(())
        }
    }

    fn stage(&mut self, amount: usize) -> Result<(), HistoryError> {
        self.metrics.peak_staging_bytes = self
            .metrics
            .peak_staging_bytes
            .max(self.retained.saturating_add(amount));
        if self.metrics.peak_staging_bytes > self.options.max_staging_bytes {
            Err(HistoryError::BoundExceeded(HistoryBound::StagingBytes))
        } else {
            check_deadline(self.deadline)
        }
    }

    fn release(&mut self, amount: usize) {
        self.retained = self.retained.saturating_sub(amount);
    }
}

#[derive(Clone, Debug)]
struct RenameDraft {
    from: RootRelativePath,
    to: RootRelativePath,
    blob: ObjectId,
    mode: String,
}

#[derive(Clone, Debug)]
struct CurrentAndCommittedRange {
    current: GraphRange,
    committed_blob: ObjectId,
    committed: GraphRange,
}

#[derive(Clone, Debug)]
struct EdgeChanges {
    raw: Vec<(RootRelativePath, ChangeKind)>,
    renames: Vec<RenameDraft>,
    rename_forward: BTreeMap<RootRelativePath, RootRelativePath>,
    raw_records: usize,
    weight: usize,
}

fn edge_changes_weight(changes: &EdgeChanges) -> usize {
    size_of::<EdgeChanges>()
        + changes
            .raw
            .iter()
            .map(|(path, _)| size_of::<RootRelativePath>() + path.capacity())
            .sum::<usize>()
        + changes
            .renames
            .iter()
            .map(|rename| {
                size_of::<RenameDraft>()
                    + rename.from.as_str().len()
                    + rename.to.as_str().len()
                    + rename.blob.as_str().len()
                    + rename.mode.len()
            })
            .sum::<usize>()
        + changes
            .rename_forward
            .iter()
            .map(|(from, to)| {
                BTREE_ENTRY_WEIGHT
                    + 2 * size_of::<RootRelativePath>()
                    + from.as_str().len()
                    + to.as_str().len()
            })
            .sum::<usize>()
}

#[derive(Clone, Debug)]
struct BlameLine {
    commit: ObjectId,
    original_line: usize,
    final_line: usize,
    source_path: RootRelativePath,
    text: Vec<u8>,
    boundary: bool,
}

fn add_bounded(
    total: &mut usize,
    amount: usize,
    maximum: usize,
    bound: HistoryBound,
) -> Result<(), HistoryError> {
    *total = total
        .checked_add(amount)
        .filter(|total| *total <= maximum)
        .ok_or(HistoryError::BoundExceeded(bound))?;
    Ok(())
}

fn validate_options(options: &HistoryOptions) -> Result<(), HistoryError> {
    let values = [
        options.max_commands,
        options.max_commits,
        options.max_parents,
        options.max_changes,
        options.max_raw_changes,
        options.max_paths,
        options.max_renames,
        options.max_blame_paths,
        options.max_blame_lines,
        options.max_output_bytes,
        options.max_command_output_bytes,
        options.max_error_output_bytes,
        options.max_object_bytes,
        options.max_provenance,
        options.max_pairs,
        options.max_nodes,
        options.max_edges,
        options.max_cache_entries,
        options.max_cache_bytes,
        options.max_staging_bytes,
        options.max_work,
    ];
    if values.contains(&0) || options.max_time.is_zero() {
        Err(HistoryError::InvalidOptions("all bounds must be nonzero"))
    } else {
        Ok(())
    }
}

fn command_operation(command: &HistoryGitCommand) -> &'static str {
    match command {
        HistoryGitCommand::Head => "resolve HEAD",
        HistoryGitCommand::ObjectFormat => "resolve object format",
        HistoryGitCommand::ShallowBoundaries => "read shallow boundaries",
        HistoryGitCommand::Commit(_) => "read commit object",
        HistoryGitCommand::Tree(_) => "read tree object",
        HistoryGitCommand::Blob(_) => "read blob object",
        HistoryGitCommand::Blame { .. } => "read blame porcelain",
    }
}

fn command_error(
    command: &HistoryGitCommand,
    error: GitCommandError,
    bound: HistoryBound,
    stdout_bytes: usize,
    remaining_output: usize,
) -> HistoryError {
    match error {
        GitCommandError::Unavailable(reason) => HistoryError::Unavailable(reason),
        GitCommandError::TimedOut => HistoryError::BoundExceeded(HistoryBound::Time),
        GitCommandError::OutputTooLarge if stdout_bytes == remaining_output => {
            HistoryError::BoundExceeded(HistoryBound::OutputBytes)
        }
        GitCommandError::OutputTooLarge => HistoryError::BoundExceeded(bound),
        GitCommandError::Failed(message) => HistoryError::Git {
            operation: command_operation(command),
            message,
        },
    }
}

fn repository_validation_error(error: GitCommandError) -> HistoryError {
    match error {
        GitCommandError::Unavailable(reason) => HistoryError::Unavailable(reason),
        GitCommandError::TimedOut => HistoryError::BoundExceeded(HistoryBound::Time),
        GitCommandError::OutputTooLarge => {
            HistoryError::Unavailable("bounded Git repository policy validation")
        }
        GitCommandError::Failed(message) => HistoryError::Git {
            operation: "validate Git repository policy",
            message,
        },
    }
}

fn parse_commit<F>(
    oid: ObjectId,
    bytes: &[u8],
    format: ObjectFormat,
    max_parents: usize,
    mut step: F,
) -> Result<HistoryCommit, HistoryError>
where
    F: FnMut(usize) -> Result<(), HistoryError>,
{
    if bytes.contains(&0) || !bytes.windows(2).any(|window| window == b"\n\n") {
        return Err(HistoryError::Malformed("commit headers are not terminated"));
    }
    let mut headers = bytes
        .split(|byte| *byte == b'\n')
        .take_while(|line| !line.is_empty());
    let tree_line = headers
        .next()
        .and_then(|line| line.strip_prefix(b"tree "))
        .ok_or(HistoryError::Malformed(
            "commit tree is not the first header",
        ))?;
    let tree = ObjectId::parse(
        std::str::from_utf8(tree_line)
            .map_err(|_| HistoryError::Malformed("commit tree is not ASCII"))?,
        format,
    )?;
    let mut parents = Vec::new();
    let mut unique_parents = HashSet::new();
    let mut saw_non_parent = false;
    let mut authors = 0;
    let mut committers = 0;
    let mut staged = commit_weight(&HistoryCommit {
        oid: oid.clone(),
        tree: tree.clone(),
        parents: Vec::new(),
    });
    for line in headers {
        step(staged)?;
        if let Some(value) = line.strip_prefix(b"parent ") {
            if saw_non_parent {
                return Err(HistoryError::Malformed(
                    "commit parent header is out of order",
                ));
            }
            if parents.len() == max_parents {
                return Err(HistoryError::BoundExceeded(HistoryBound::Parents));
            }
            staged = staged
                .checked_add(
                    2_usize.saturating_mul(size_of::<ObjectId>() + value.len()) + HASH_ENTRY_WEIGHT,
                )
                .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
            step(staged)?;
            let parent = ObjectId::parse(
                std::str::from_utf8(value)
                    .map_err(|_| HistoryError::Malformed("commit parent is not ASCII"))?,
                format,
            )?;
            if !unique_parents.insert(parent.clone()) {
                return Err(HistoryError::Malformed("commit repeats a parent"));
            }
            parents.push(parent);
        } else {
            saw_non_parent = true;
            authors += usize::from(line.starts_with(b"author "));
            committers += usize::from(line.starts_with(b"committer "));
            if line == b"tree" || line.starts_with(b"tree ") {
                return Err(HistoryError::Malformed("commit has multiple trees"));
            }
        }
    }
    if authors != 1 || committers != 1 {
        return Err(HistoryError::Malformed(
            "commit must have one author and one committer header",
        ));
    }
    Ok(HistoryCommit { oid, tree, parents })
}

fn parse_tree<F>(
    bytes: &[u8],
    format: ObjectFormat,
    max_paths: usize,
    mut reserve: F,
) -> Result<Tree, HistoryError>
where
    F: FnMut(usize) -> Result<(), HistoryError>,
{
    let mut tree = BTreeMap::new();
    let mut staged = size_of::<Tree>();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        if tree.len() == max_paths {
            return Err(HistoryError::BoundExceeded(HistoryBound::Paths));
        }
        staged = staged
            .checked_add(
                record.len()
                    + BTREE_ENTRY_WEIGHT
                    + size_of::<RootRelativePath>()
                    + size_of::<TreeEntry>(),
            )
            .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
        reserve(staged)?;
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or(HistoryError::Malformed("tree record has no path separator"))?;
        let metadata = std::str::from_utf8(&record[..tab])
            .map_err(|_| HistoryError::Malformed("tree metadata is not ASCII"))?;
        let mut fields = metadata.split(' ');
        let mode = fields
            .next()
            .ok_or(HistoryError::Malformed("tree mode is absent"))?;
        let kind = fields
            .next()
            .ok_or(HistoryError::Malformed("tree type is absent"))?;
        let oid = fields
            .next()
            .ok_or(HistoryError::Malformed("tree object ID is absent"))?;
        if fields.next().is_some()
            || kind != "blob"
            || !matches!(mode, "100644" | "100755" | "120000")
        {
            return Err(HistoryError::Malformed("unsupported tree entry"));
        }
        let path = std::str::from_utf8(&record[tab + 1..])
            .map_err(|_| HistoryError::Malformed("tree path is not UTF-8"))?;
        let path = RootRelativePath::parse(path.to_owned(), 256 * 1024)
            .map_err(|_| HistoryError::Malformed("tree path is unsafe"))?;
        if tree
            .insert(
                path,
                TreeEntry {
                    mode: mode.to_owned(),
                    blob: ObjectId::parse(oid, format)?,
                },
            )
            .is_some()
        {
            return Err(HistoryError::Malformed("tree repeats a path"));
        }
    }
    Ok(tree)
}

fn parse_blame<F>(
    bytes: &[u8],
    format: ObjectFormat,
    current_path: &RootRelativePath,
    max_lines: usize,
    mut step: F,
) -> Result<Vec<BlameLine>, HistoryError>
where
    F: FnMut(usize) -> Result<(), HistoryError>,
{
    let mut source = bytes.split(|byte| *byte == b'\n').peekable();
    let mut result = Vec::new();
    let mut staged = 0_usize;
    while let Some(header) = source.next() {
        step(staged)?;
        if header.is_empty() {
            continue;
        }
        if result.len() == max_lines {
            return Err(HistoryError::BoundExceeded(HistoryBound::BlameLines));
        }
        let header = std::str::from_utf8(header)
            .map_err(|_| HistoryError::Malformed("blame header is not ASCII"))?;
        let mut fields = header.split_whitespace();
        let commit_field = fields
            .next()
            .ok_or(HistoryError::Malformed("invalid blame header"))?;
        let mut boundary = commit_field.starts_with('^');
        let commit = ObjectId::parse(commit_field.trim_start_matches('^'), format)?;
        let original_line = fields
            .next()
            .ok_or(HistoryError::Malformed("invalid blame header"))?
            .parse()
            .map_err(|_| HistoryError::Malformed("invalid blame original line"))?;
        let final_line = fields
            .next()
            .ok_or(HistoryError::Malformed("invalid blame header"))?
            .parse()
            .map_err(|_| HistoryError::Malformed("invalid blame final line"))?;
        let mut source_path = None;
        let mut text = None;
        for line in source.by_ref() {
            step(staged)?;
            if let Some(value) = line.strip_prefix(b"filename ") {
                step(
                    staged
                        .saturating_add(size_of::<RootRelativePath>())
                        .saturating_add(value.len()),
                )?;
                let value = std::str::from_utf8(value)
                    .map_err(|_| HistoryError::Malformed("blame filename is not UTF-8"))?;
                source_path = Some(
                    RootRelativePath::parse(value.to_owned(), 256 * 1024)
                        .map_err(|_| HistoryError::Malformed("blame filename is unsafe"))?,
                );
            }
            boundary |= line == b"boundary";
            if let Some(value) = line.strip_prefix(b"\t") {
                step(staged.saturating_add(value.len()))?;
                text = Some(value.to_vec());
                break;
            }
        }
        let source_path = source_path.unwrap_or_else(|| current_path.clone());
        let text = text.ok_or(HistoryError::Malformed("blame line content is absent"))?;
        staged = staged
            .checked_add(
                size_of::<BlameLine>()
                    + commit.as_str().len()
                    + source_path.capacity()
                    + text.len(),
            )
            .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
        step(staged)?;
        result.push(BlameLine {
            commit,
            original_line,
            final_line,
            source_path,
            text,
            boundary,
        });
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn collapse_blame<F>(
    path: &RootRelativePath,
    lines: &[BlameLine],
    committed: &[u8],
    source_ids: &BTreeMap<ObjectId, BTreeMap<RootRelativePath, ObjectId>>,
    source_blobs: &BTreeMap<ObjectId, Arc<BlobData>>,
    revision: RevisionId,
    max_hunks: usize,
    mut reserve: F,
) -> Result<Vec<BlameHunk>, HistoryError>
where
    F: FnMut(usize) -> Result<(), HistoryError>,
{
    reserve(
        lines
            .len()
            .saturating_add(1)
            .saturating_mul(size_of::<usize>()),
    )?;
    let mut starts = Vec::with_capacity(lines.len() + 1);
    let mut cursor = 0;
    starts.push(cursor);
    for line in lines {
        reserve(starts.capacity().saturating_mul(size_of::<usize>()))?;
        cursor += line.text.len();
        if committed.get(cursor) == Some(&b'\n') {
            cursor += 1;
        }
        starts.push(cursor);
    }
    if cursor != committed.len() {
        return Err(HistoryError::Malformed(
            "blame byte ranges do not cover the HEAD blob",
        ));
    }
    let mut hunks = Vec::new();
    let mut start = 0;
    while start < lines.len() {
        reserve(
            starts
                .capacity()
                .saturating_mul(size_of::<usize>())
                .saturating_add(hunks.iter().map(blame_hunk_weight).sum::<usize>()),
        )?;
        if hunks.len() == max_hunks {
            return Err(HistoryError::BoundExceeded(HistoryBound::Provenance));
        }
        let mut end = start + 1;
        while end < lines.len()
            && lines[end].commit == lines[start].commit
            && lines[end].source_path == lines[start].source_path
            && lines[end].original_line == lines[start].original_line + end - start
            && lines[end].final_line == lines[start].final_line + end - start
        {
            reserve(
                starts
                    .capacity()
                    .saturating_mul(size_of::<usize>())
                    .saturating_add(hunks.iter().map(blame_hunk_weight).sum::<usize>()),
            )?;
            end += 1;
        }
        let blob = source_ids
            .get(&lines[start].commit)
            .and_then(|paths| paths.get(&lines[start].source_path))
            .ok_or(HistoryError::Malformed("blame source blob is absent"))?
            .clone();
        let source_blob = source_blobs.get(&blob).ok_or(HistoryError::Malformed(
            "blame source blob bytes are absent",
        ))?;
        let source_range = line_span(source_blob, lines[start].original_line, end - start)?;
        let mut hunk = BlameHunk {
            path: path.clone(),
            range: GraphRange {
                start_byte: starts[start],
                end_byte: starts[end],
                start_line: lines[start].final_line,
                end_line: lines[end - 1].final_line,
            },
            source: BlameSource::Git,
            source_commit: Some(lines[start].commit.clone()),
            source_path: lines[start].source_path.clone(),
            source_blob: Some(blob),
            source_range,
            boundary: lines[start..end].iter().any(|line| line.boundary),
            confidence_millis: 1_000,
            revision,
            line_digest: *blake3::hash(&committed[starts[start]..starts[end]]).as_bytes(),
            evidence_digest: [0; 32],
        };
        hunk.evidence_digest = digest_blame_evidence(&hunk);
        reserve(
            starts
                .capacity()
                .saturating_mul(size_of::<usize>())
                .saturating_add(hunks.iter().map(blame_hunk_weight).sum::<usize>())
                .saturating_add(blame_hunk_weight(&hunk)),
        )?;
        hunks.push(hunk);
        start = end;
    }
    Ok(hunks)
}

fn line_span(blob: &BlobData, start_line: usize, count: usize) -> Result<GraphRange, HistoryError> {
    let start = start_line
        .checked_sub(1)
        .ok_or(HistoryError::Malformed("blame source line is zero"))?;
    let end = start
        .checked_add(count)
        .ok_or(HistoryError::Malformed("blame source line range overflow"))?;
    let start_byte = blob
        .lines
        .get(start)
        .map(|range| range.0)
        .ok_or(HistoryError::Malformed(
            "blame source range starts outside blob",
        ))?;
    let end_byte = blob
        .lines
        .get(end)
        .map_or(blob.bytes.len(), |range| range.0);
    if end > blob.lines.len() {
        return Err(HistoryError::Malformed(
            "blame source range ends outside blob",
        ));
    }
    Ok(GraphRange {
        start_byte,
        end_byte,
        start_line,
        end_line: start_line + count - 1,
    })
}

fn digest_blame_evidence(hunk: &BlameHunk) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-history-blame-evidence-v1\0");
    frame(&mut hash, hunk.path.as_str().as_bytes());
    digest_range(&mut hash, hunk.range);
    integer(&mut hash, hunk.source as usize);
    frame(
        &mut hash,
        hunk.source_commit
            .as_ref()
            .map_or(b"".as_slice(), |oid| oid.as_str().as_bytes()),
    );
    frame(&mut hash, hunk.source_path.as_str().as_bytes());
    frame(
        &mut hash,
        hunk.source_blob
            .as_ref()
            .map_or(b"".as_slice(), |oid| oid.as_str().as_bytes()),
    );
    digest_range(&mut hash, hunk.source_range);
    integer(&mut hash, usize::from(hunk.boundary));
    integer(&mut hash, usize::from(hunk.confidence_millis));
    hash.update(&hunk.line_digest);
    *hash.finalize().as_bytes()
}

fn line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        0
    } else {
        bytes.iter().filter(|byte| **byte == b'\n').count() + usize::from(!bytes.ends_with(b"\n"))
    }
}

fn line_ranges<F>(
    bytes: &[u8],
    max_lines: usize,
    mut reserve: F,
) -> Result<Vec<(usize, usize)>, HistoryError>
where
    F: FnMut(usize) -> Result<(), HistoryError>,
{
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in bytes.iter().enumerate() {
        reserve(lines.len().saturating_mul(size_of::<(usize, usize)>()))?;
        if *byte == b'\n' {
            if lines.len() == max_lines {
                return Err(HistoryError::BoundExceeded(HistoryBound::BlameLines));
            }
            reserve(
                lines
                    .len()
                    .saturating_add(1)
                    .saturating_mul(size_of::<(usize, usize)>()),
            )?;
            lines.push((start, index));
            start = index + 1;
        }
    }
    if start < bytes.len() {
        if lines.len() == max_lines {
            return Err(HistoryError::BoundExceeded(HistoryBound::BlameLines));
        }
        reserve(
            lines
                .len()
                .saturating_add(1)
                .saturating_mul(size_of::<(usize, usize)>()),
        )?;
        lines.push((start, bytes.len()));
    }
    Ok(lines)
}

fn blob_line(blob: &BlobData, index: usize) -> Option<&[u8]> {
    let (start, end) = *blob.lines.get(index)?;
    Some(&blob.bytes[start..end])
}

fn whole_range(bytes: &[u8]) -> GraphRange {
    let lines = line_count(bytes);
    GraphRange {
        start_byte: 0,
        end_byte: bytes.len(),
        start_line: 1,
        end_line: lines.max(1),
    }
}

fn check_deadline(deadline: Instant) -> Result<(), HistoryError> {
    if Instant::now() >= deadline {
        Err(HistoryError::BoundExceeded(HistoryBound::Time))
    } else {
        Ok(())
    }
}

fn sort_comparison_bound(len: usize) -> Result<usize, HistoryError> {
    if len < 2 {
        return Ok(0);
    }
    let levels = usize::BITS as usize - (len - 1).leading_zeros() as usize;
    len.checked_mul(levels)
        .ok_or(HistoryError::BoundExceeded(HistoryBound::Work))
}

fn changed_with_strength_millis(
    shared: usize,
    left: usize,
    right: usize,
) -> Result<u16, HistoryError> {
    let union = left
        .checked_add(right)
        .and_then(|count| count.checked_sub(shared))
        .filter(|count| *count != 0 && shared <= left && shared <= right)
        .ok_or(HistoryError::Malformed(
            "incoherent co-change support counts",
        ))?;
    let rounded = (shared as u128)
        .checked_mul(1_000)
        .and_then(|value| value.checked_add((union / 2) as u128))
        .and_then(|value| value.checked_div(union as u128))
        .ok_or(HistoryError::Malformed("co-change strength overflowed"))?;
    u16::try_from(rounded).map_err(|_| HistoryError::Malformed("co-change strength overflowed"))
}

const fn object_id_weight(oid: &ObjectId) -> usize {
    size_of::<ObjectId>() + oid.0.capacity()
}

fn path_weight(path: &RootRelativePath) -> usize {
    size_of::<RootRelativePath>() + path.capacity()
}

fn changes_weight(changes: &[HistoryChange]) -> usize {
    changes
        .iter()
        .map(|item| {
            size_of::<HistoryChange>()
                + item.commit.capacity()
                + item.parent.as_ref().map_or(0, ObjectId::capacity)
                + item.path.capacity()
                + item
                    .current_path
                    .as_ref()
                    .map_or(0, RootRelativePath::capacity)
        })
        .sum()
}

fn renames_weight(renames: &[ExactRename]) -> usize {
    renames
        .iter()
        .map(|item| {
            size_of::<ExactRename>()
                + item.commit.capacity()
                + item.parent.capacity()
                + item.from.capacity()
                + item.to.capacity()
                + item
                    .current_path
                    .as_ref()
                    .map_or(0, RootRelativePath::capacity)
                + item.blob.capacity()
                + item.mode.len()
        })
        .sum()
}

fn changed_with_weight(facts: &[ChangedWithFact]) -> usize {
    facts
        .iter()
        .map(|item| {
            size_of::<ChangedWithFact>()
                + item.left.capacity()
                + item.right.capacity()
                + item.provenance.head.capacity()
                + item.provenance.policy.len()
                + item
                    .provenance
                    .commits
                    .iter()
                    .map(object_id_weight)
                    .sum::<usize>()
                + item.provenance.left_committed_blob.capacity()
                + item.provenance.right_committed_blob.capacity()
        })
        .sum()
}

fn edge_change_staging_weight(
    raw: &[(RootRelativePath, ChangeKind)],
    renames: &[RenameDraft],
    deleted: &BTreeMap<(ObjectId, String), Vec<RootRelativePath>>,
    added: &BTreeMap<(ObjectId, String), Vec<RootRelativePath>>,
) -> Result<usize, HistoryError> {
    let vectors = raw
        .iter()
        .map(|(path, _)| size_of::<(RootRelativePath, ChangeKind)>() + path.capacity())
        .sum::<usize>()
        .checked_add(
            renames
                .iter()
                .map(|rename| {
                    size_of::<RenameDraft>()
                        + rename.from.capacity()
                        + rename.to.capacity()
                        + rename.blob.as_str().len()
                        + rename.mode.len()
                })
                .sum(),
        )
        .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))?;
    deleted
        .iter()
        .chain(added)
        .try_fold(vectors, |bytes, ((oid, mode), paths)| {
            bytes
                .checked_add(
                    BTREE_ENTRY_WEIGHT
                        + size_of::<(ObjectId, String)>()
                        + oid.as_str().len()
                        + mode.len()
                        + size_of::<Vec<RootRelativePath>>(),
                )
                .and_then(|bytes| bytes.checked_add(paths.iter().map(path_weight).sum::<usize>()))
                .ok_or(HistoryError::BoundExceeded(HistoryBound::StagingBytes))
        })
}

fn commit_weight(commit: &HistoryCommit) -> usize {
    size_of::<HistoryCommit>()
        + commit.oid.as_str().len()
        + commit.tree.as_str().len()
        + commit
            .parents
            .iter()
            .map(|parent| size_of::<ObjectId>() + parent.as_str().len())
            .sum::<usize>()
}

fn tree_weight(oid: &ObjectId, tree: &Tree) -> usize {
    size_of::<Tree>()
        + size_of::<ObjectId>()
        + size_of::<CacheEntry<Tree>>()
        + BTREE_ENTRY_WEIGHT
        + oid.as_str().len()
        + tree
            .iter()
            .map(|(path, entry)| {
                size_of::<RootRelativePath>()
                    + path.capacity()
                    + size_of::<TreeEntry>()
                    + entry.mode.len()
                    + entry.blob.as_str().len()
                    + BTREE_ENTRY_WEIGHT
            })
            .sum::<usize>()
}

fn blob_weight(oid: &ObjectId, blob: &BlobData) -> usize {
    size_of::<BlobData>()
        + size_of::<ObjectId>()
        + size_of::<CacheEntry<BlobData>>()
        + BTREE_ENTRY_WEIGHT
        + oid.as_str().len()
        + blob.bytes.len()
        + blob.lines.len().saturating_mul(size_of::<(usize, usize)>())
}

fn blame_hunk_weight(hunk: &BlameHunk) -> usize {
    size_of::<BlameHunk>()
        + hunk.path.capacity()
        + hunk.source_path.capacity()
        + hunk
            .source_commit
            .as_ref()
            .map_or(0, |oid| oid.as_str().len())
        + hunk
            .source_blob
            .as_ref()
            .map_or(0, |oid| oid.as_str().len())
}

fn cache_map_weight<T>(cache: &BTreeMap<ObjectId, CacheEntry<T>>) -> usize {
    size_of::<BTreeMap<ObjectId, CacheEntry<T>>>()
        + cache
            .keys()
            .map(|oid| object_id_weight(oid) + size_of::<CacheEntry<T>>() + BTREE_ENTRY_WEIGHT)
            .sum::<usize>()
}

fn map_paths_weight(map: &BTreeMap<ObjectId, Vec<ObjectId>>) -> usize {
    size_of::<BTreeMap<ObjectId, Vec<ObjectId>>>()
        + map
            .iter()
            .map(|(key, values)| {
                key.as_str().len()
                    + size_of::<ObjectId>()
                    + values
                        .iter()
                        .map(|oid| size_of::<ObjectId>() + oid.as_str().len())
                        .sum::<usize>()
                    + BTREE_ENTRY_WEIGHT
            })
            .sum::<usize>()
}

fn graph_weight(
    commits: &[HistoryCommit],
    changes: &[HistoryChange],
    renames: &[ExactRename],
    blame: &[BlameHunk],
    pairs: &[ChangedWithFact],
    coverage: &[CoverageRecord],
) -> usize {
    size_of::<HistoryGraph>()
        + commits.iter().map(commit_weight).sum::<usize>()
        + changes_weight(changes)
        + renames_weight(renames)
        + blame.iter().map(blame_hunk_weight).sum::<usize>()
        + changed_with_weight(pairs)
        + size_of_val(coverage)
}

fn digest_options(options: &HistoryOptions) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-history-options-v1\0");
    for value in [
        options.max_commands,
        options.max_commits,
        options.max_parents,
        options.max_changes,
        options.max_raw_changes,
        options.max_paths,
        options.max_renames,
        options.max_blame_paths,
        options.max_blame_lines,
        options.max_output_bytes,
        options.max_command_output_bytes,
        options.max_error_output_bytes,
        options.max_object_bytes,
        options.max_provenance,
        options.max_pairs,
        options.max_nodes,
        options.max_edges,
        options.max_cache_entries,
        options.max_cache_bytes,
        options.max_staging_bytes,
        options.max_work,
    ] {
        integer(&mut hash, value);
    }
    hash.update(&options.max_time.as_nanos().to_le_bytes());
    *hash.finalize().as_bytes()
}

#[allow(clippy::too_many_arguments)]
fn digest_content(
    head: &ObjectId,
    tree: &ObjectId,
    format: ObjectFormat,
    shallow: [u8; 32],
    scope: [u8; 32],
    request: [u8; 32],
    options: [u8; 32],
    commits: &[HistoryCommit],
    changes: &[HistoryChange],
    renames: &[ExactRename],
    blame: &[BlameHunk],
    pairs: &[ChangedWithFact],
    coverage: &[CoverageRecord],
    extractor: [u8; 32],
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-history-content-v1\0");
    integer(&mut hash, format as usize);
    frame(&mut hash, head.as_str().as_bytes());
    frame(&mut hash, tree.as_str().as_bytes());
    hash.update(&shallow);
    hash.update(&scope);
    hash.update(&request);
    hash.update(&options);
    hash.update(&extractor);
    integer(&mut hash, commits.len());
    for commit in commits {
        frame(&mut hash, commit.oid.as_str().as_bytes());
        frame(&mut hash, commit.tree.as_str().as_bytes());
        integer(&mut hash, commit.parents.len());
        for parent in &commit.parents {
            frame(&mut hash, parent.as_str().as_bytes());
        }
    }
    integer(&mut hash, changes.len());
    for change in changes {
        frame(&mut hash, change.commit.as_str().as_bytes());
        frame(
            &mut hash,
            change
                .parent
                .as_ref()
                .map_or(b"".as_slice(), |oid| oid.as_str().as_bytes()),
        );
        frame(&mut hash, change.path.as_str().as_bytes());
        frame(
            &mut hash,
            change
                .current_path
                .as_ref()
                .map_or(b"".as_slice(), |path| path.as_str().as_bytes()),
        );
        integer(&mut hash, change.kind as usize);
    }
    integer(&mut hash, renames.len());
    for rename in renames {
        frame(&mut hash, rename.commit.as_str().as_bytes());
        frame(&mut hash, rename.parent.as_str().as_bytes());
        frame(&mut hash, rename.from.as_str().as_bytes());
        frame(&mut hash, rename.to.as_str().as_bytes());
        frame(
            &mut hash,
            rename
                .current_path
                .as_ref()
                .map_or(b"".as_slice(), |path| path.as_str().as_bytes()),
        );
        frame(&mut hash, rename.blob.as_str().as_bytes());
        frame(&mut hash, rename.mode.as_bytes());
        integer(&mut hash, usize::from(rename.confidence_millis));
    }
    integer(&mut hash, blame.len());
    for hunk in blame {
        frame(&mut hash, hunk.path.as_str().as_bytes());
        digest_range(&mut hash, hunk.range);
        integer(&mut hash, hunk.source as usize);
        frame(
            &mut hash,
            hunk.source_commit
                .as_ref()
                .map_or(b"".as_slice(), |oid| oid.as_str().as_bytes()),
        );
        frame(&mut hash, hunk.source_path.as_str().as_bytes());
        frame(
            &mut hash,
            hunk.source_blob
                .as_ref()
                .map_or(b"".as_slice(), |oid| oid.as_str().as_bytes()),
        );
        integer(&mut hash, hunk.source_range.start_line);
        digest_range(&mut hash, hunk.source_range);
        integer(&mut hash, usize::from(hunk.boundary));
        integer(&mut hash, usize::from(hunk.confidence_millis));
        hash.update(&hunk.line_digest);
        hash.update(&hunk.evidence_digest);
    }
    integer(&mut hash, pairs.len());
    for pair in pairs {
        frame(&mut hash, pair.left.as_str().as_bytes());
        frame(&mut hash, pair.right.as_str().as_bytes());
        integer(&mut hash, pair.count);
        integer(&mut hash, usize::from(pair.strength_millis));
        integer(&mut hash, usize::from(pair.extraction_confidence_millis));
        let provenance = &pair.provenance;
        frame(&mut hash, provenance.head.as_str().as_bytes());
        hash.update(&provenance.scope_digest);
        frame(&mut hash, provenance.policy.as_bytes());
        integer(&mut hash, provenance.commits.len());
        for commit in &provenance.commits {
            frame(&mut hash, commit.as_str().as_bytes());
        }
        integer(&mut hash, provenance.count);
        integer(&mut hash, provenance.left_count);
        integer(&mut hash, provenance.right_count);
        integer(&mut hash, provenance.shared_count);
        hash.update(&provenance.evidence_digest);
        digest_range(&mut hash, provenance.left_range);
        digest_range(&mut hash, provenance.right_range);
        frame(
            &mut hash,
            provenance.left_committed_blob.as_str().as_bytes(),
        );
        frame(
            &mut hash,
            provenance.right_committed_blob.as_str().as_bytes(),
        );
        digest_range(&mut hash, provenance.left_committed_range);
        digest_range(&mut hash, provenance.right_committed_range);
    }
    integer(&mut hash, coverage.len());
    for item in coverage {
        integer(&mut hash, item.area as usize);
        integer(&mut hash, item.status as usize);
        frame(&mut hash, item.detail.as_bytes());
        integer(&mut hash, item.omitted_count);
    }
    *hash.finalize().as_bytes()
}

fn digest_snapshot(
    revision: RevisionId,
    head: &ObjectId,
    request: [u8; 32],
    options: [u8; 32],
    content: [u8; 32],
    blame: &[BlameHunk],
) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-history-snapshot-v1\0");
    hash.update(revision.as_bytes());
    frame(&mut hash, head.as_str().as_bytes());
    hash.update(&request);
    hash.update(&options);
    hash.update(&content);
    for hunk in blame
        .iter()
        .filter(|hunk| hunk.source == BlameSource::Worktree)
    {
        frame(&mut hash, hunk.path.as_str().as_bytes());
        digest_range(&mut hash, hunk.range);
        hash.update(&hunk.line_digest);
    }
    *hash.finalize().as_bytes()
}

fn digest_paths<'a>(paths: impl Iterator<Item = &'a RootRelativePath>) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-history-scope-v1\0");
    for path in paths {
        frame(&mut hash, path.as_str().as_bytes());
    }
    *hash.finalize().as_bytes()
}

fn digest_shallow(boundaries: &BTreeSet<ObjectId>) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-history-shallow-v1\0");
    integer(&mut hash, boundaries.len());
    for boundary in boundaries {
        frame(&mut hash, boundary.as_str().as_bytes());
    }
    *hash.finalize().as_bytes()
}

fn digest_request(request: &HistoryRequest) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"kit-history-request-v1\0");
    integer(&mut hash, usize::from(request.include_changed_with));
    integer(&mut hash, request.scope.len());
    for path in &request.scope {
        frame(&mut hash, path.as_str().as_bytes());
    }
    integer(&mut hash, request.blame_paths.len());
    for path in &request.blame_paths {
        frame(&mut hash, path.as_str().as_bytes());
    }
    *hash.finalize().as_bytes()
}

fn digest_range(hash: &mut blake3::Hasher, range: GraphRange) {
    integer(hash, range.start_byte);
    integer(hash, range.end_byte);
    integer(hash, range.start_line);
    integer(hash, range.end_line);
}

fn integer(hash: &mut blake3::Hasher, value: usize) {
    hash.update(
        &u64::try_from(value)
            .expect("usize is no wider than the canonical u64 history integer")
            .to_le_bytes(),
    );
}

fn frame(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(
        &u64::try_from(bytes.len())
            .expect("slice length fits the canonical u64 history integer")
            .to_le_bytes(),
    );
    hash.update(bytes);
}
