use std::{
    fmt,
    fs::File,
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const UNAVAILABLE: &str = "managed workspace revisions require Linux or macOS";

#[derive(Clone, Debug)]
pub struct RevisionOptions {
    pub max_entries: usize,
    pub max_name_bytes: usize,
    pub max_bytes: u64,
    pub max_memory_bytes: u64,
    pub max_depth: usize,
    pub max_scan_time: Duration,
    pub max_scan_attempts: usize,
    pub watcher_interval: Duration,
    pub reconciliation_interval: Duration,
    pub metadata_path: Option<PathBuf>,
}

impl Default for RevisionOptions {
    fn default() -> Self {
        Self {
            max_entries: 1_000_000,
            max_name_bytes: 256 * 1024 * 1024,
            max_bytes: 1024 * 1024 * 1024,
            max_memory_bytes: 1536 * 1024 * 1024,
            max_depth: 256,
            max_scan_time: Duration::from_secs(10),
            max_scan_attempts: 3,
            watcher_interval: Duration::from_millis(25),
            reconciliation_interval: Duration::from_secs(5),
            metadata_path: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RevisionId([u8; 32]);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EpochId([u8; 16]);

impl fmt::Display for RevisionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unavailable")
    }
}

impl RevisionId {
    pub fn parse(_value: &str) -> Option<Self> {
        None
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl EpochId {
    pub(crate) fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl serde::Serialize for RevisionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

impl fmt::Display for EpochId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("unavailable")
    }
}

impl serde::Serialize for EpochId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_str(self)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ContentDigest(String);

impl ContentDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Revision {
    id: RevisionId,
    digest: ContentDigest,
}

impl Revision {
    pub fn id(&self) -> RevisionId {
        self.id
    }

    pub fn digest(&self) -> &ContentDigest {
        &self.digest
    }

    pub fn epoch(&self) -> EpochId {
        EpochId([0; 16])
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    Directory,
    File,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotEntry {
    pub path: PathBuf,
    pub kind: EntryKind,
    pub executable: bool,
    pub size: u64,
    pub has_nul: bool,
    pub valid_utf8: bool,
    pub content_complete: bool,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileReadRange {
    Full,
    Bytes { start: usize, end: usize },
    Lines { start: usize, end: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedFileRead {
    pub bytes: Vec<u8>,
    pub file_bytes: usize,
    pub byte_start: usize,
    pub byte_end: usize,
    pub line_start: Option<usize>,
    pub line_end: Option<usize>,
    pub has_nul: bool,
    pub valid_utf8: bool,
    pub has_lf: bool,
    pub has_crlf: bool,
    pub final_newline: bool,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    revision: Revision,
    entries: Vec<SnapshotEntry>,
}

impl Snapshot {
    pub fn revision(&self) -> &Revision {
        &self.revision
    }

    pub fn entries(&self) -> &[SnapshotEntry] {
        &self.entries
    }

    pub fn file(&self, path: &Path) -> Option<&[u8]> {
        self.entries
            .iter()
            .find(|entry| entry.path == path)
            .and_then(|entry| (entry.kind == EntryKind::File).then_some(entry.bytes.as_slice()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitKind {
    Entries,
    NameBytes,
    Bytes,
    Memory,
    Depth,
    Time,
}

#[derive(Debug)]
pub enum RevisionError {
    UnsafePath(PathBuf),
    Symlink(PathBuf),
    NotDirectory(PathBuf),
    UnsupportedEntry(PathBuf),
    Hardlink(PathBuf),
    MountBoundary(PathBuf),
    LimitExceeded(LimitKind),
    ScanRace {
        attempts: usize,
    },
    StaleRevision {
        expected: RevisionId,
        current: Box<Revision>,
    },
    InvalidRange(&'static str),
    NotFound(PathBuf),
    CorruptMetadata,
    Unavailable {
        reason: &'static str,
    },
    Io {
        operation: &'static str,
        source: io::Error,
    },
}

impl fmt::Display for RevisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { reason } => {
                write!(formatter, "workspace revision unavailable: {reason}")
            }
            other => write!(
                formatter,
                "workspace revision unavailable on this platform: {other:?}"
            ),
        }
    }
}

impl std::error::Error for RevisionError {}

#[derive(Clone)]
pub struct ManagedWorkspace;
pub struct WorkspaceMutationGuard<'a> {
    _private: std::marker::PhantomData<&'a ()>,
}
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct MutationGuardNonce([u8; 16]);
pub struct WorkspaceStableReadGuard<'a> {
    _private: std::marker::PhantomData<&'a ()>,
}

impl ManagedWorkspace {
    pub fn open(_root: impl AsRef<Path>) -> Result<Self, RevisionError> {
        Err(unavailable())
    }

    pub fn open_with_options(
        _root: impl AsRef<Path>,
        _options: RevisionOptions,
    ) -> Result<Self, RevisionError> {
        Err(unavailable())
    }

    pub fn current_revision(&self) -> Result<Revision, RevisionError> {
        Err(unavailable())
    }

    pub(crate) fn canonical_root(&self) -> &Path {
        Path::new("")
    }

    pub(crate) fn duplicate_root(&self) -> Result<File, RevisionError> {
        Err(unavailable())
    }

    pub fn is_dirty(&self) -> bool {
        true
    }

    pub fn mark_dirty(&self) {}

    pub fn inject_watcher_loss(&self) {}

    pub fn reconcile(&self) -> Result<Revision, RevisionError> {
        Err(unavailable())
    }

    pub fn snapshot(&self, _expected: RevisionId) -> Result<Snapshot, RevisionError> {
        Err(unavailable())
    }

    pub fn metadata_snapshot_before(
        &self,
        _expected: RevisionId,
        _max_file_bytes: usize,
        _max_special_file_bytes: usize,
        _max_content_bytes: u64,
        _deadline: Instant,
    ) -> Result<Snapshot, RevisionError> {
        Err(unavailable())
    }

    pub fn read_file(
        &self,
        _expected: RevisionId,
        _path: impl AsRef<Path>,
    ) -> Result<Vec<u8>, RevisionError> {
        Err(unavailable())
    }

    pub fn read_file_before(
        &self,
        _expected: RevisionId,
        _path: impl AsRef<Path>,
        _deadline: Instant,
    ) -> Result<Vec<u8>, RevisionError> {
        Err(unavailable())
    }

    pub fn read_file_range_before(
        &self,
        _expected: RevisionId,
        _path: impl AsRef<Path>,
        _range: FileReadRange,
        _max_bytes: usize,
        _deadline: Instant,
    ) -> Result<BoundedFileRead, RevisionError> {
        Err(unavailable())
    }

    pub fn validate_revision(&self, _expected: RevisionId) -> Result<Revision, RevisionError> {
        Err(unavailable())
    }

    pub fn validate_revision_until(
        &self,
        _expected: RevisionId,
        _deadline: Instant,
    ) -> Result<Revision, RevisionError> {
        Err(unavailable())
    }

    pub fn mutation_guard(
        &self,
        _expected: RevisionId,
    ) -> Result<WorkspaceMutationGuard<'_>, RevisionError> {
        Err(unavailable())
    }

    pub fn mutation_guard_before(
        &self,
        _expected: RevisionId,
        _deadline: Instant,
    ) -> Result<WorkspaceMutationGuard<'_>, RevisionError> {
        Err(unavailable())
    }

    pub fn stable_read_guard_before(
        &self,
        _expected: RevisionId,
        _deadline: Instant,
    ) -> Result<WorkspaceStableReadGuard<'_>, RevisionError> {
        Err(unavailable())
    }
}

impl WorkspaceMutationGuard<'_> {
    pub fn revision(&self) -> &Revision {
        unreachable!()
    }

    pub fn validate_revision_until(
        &mut self,
        _expected: RevisionId,
        _deadline: Instant,
    ) -> Result<Revision, RevisionError> {
        Err(unavailable())
    }

    pub(crate) fn validate_held_revision_until(
        &mut self,
        _expected: RevisionId,
        _deadline: Instant,
    ) -> Result<Revision, RevisionError> {
        Err(unavailable())
    }
}

impl WorkspaceStableReadGuard<'_> {
    pub fn read_file_range_before(
        &mut self,
        _path: impl AsRef<Path>,
        _range: FileReadRange,
        _max_bytes: usize,
        _deadline: Instant,
    ) -> Result<BoundedFileRead, RevisionError> {
        Err(unavailable())
    }

    pub fn validate_before(&mut self, _deadline: Instant) -> Result<Revision, RevisionError> {
        Err(unavailable())
    }
}

fn unavailable() -> RevisionError {
    RevisionError::Unavailable {
        reason: UNAVAILABLE,
    }
}
