use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt,
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock, TryLockError, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const STATE_MAGIC: &str = "kit-workspace-revision-v3";
const DIGEST_MAGIC: &[u8] = b"kit-workspace-content-v1\0";
const REVISION_MAGIC: &[u8] = b"kit-workspace-revision-id-v1\0";

// The memory limit is a logical allocation budget, not an allocator measurement. Every
// requested heap growth is charged a 64-byte header plus payload rounded to 64-byte slabs
// below one page and 4 KiB pages above it. Vectors start with a fixed 4 KiB slab and double
// their logical capacity; every replacement buffer is charged in full before growth.
const MEMORY_HEADER_BYTES: u64 = 64;
const MEMORY_SLAB_BYTES: u64 = 64;
const MEMORY_PAGE_BYTES: u64 = 4096;
const VECTOR_GROWTH_BYTES: usize = MEMORY_PAGE_BYTES as usize;
const OWNER_LOCK_RETRY: Duration = Duration::from_millis(100);

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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RevisionId([u8; 32]);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct EpochId([u8; 16]);

impl fmt::Display for RevisionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("r:")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl RevisionId {
    pub fn parse(value: &str) -> Option<Self> {
        let hex = value.strip_prefix("r:")?;
        if hex.len() != 64 {
            return None;
        }
        let mut bytes = [0_u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(Self(bytes))
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

impl<'de> serde::Deserialize<'de> for RevisionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(&<String as serde::Deserialize>::deserialize(deserializer)?)
            .ok_or_else(|| serde::de::Error::custom("invalid workspace revision"))
    }
}

impl fmt::Display for EpochId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("e:")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
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

impl<'de> serde::Deserialize<'de> for EpochId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = <String as serde::Deserialize>::deserialize(deserializer)?;
        let hex = value
            .strip_prefix("e:")
            .filter(|hex| hex.len() == 32)
            .ok_or_else(|| serde::de::Error::custom("invalid workspace epoch"))?;
        let mut bytes = [0_u8; 16];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
                .map_err(serde::de::Error::custom)?;
        }
        Ok(Self(bytes))
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ContentDigest([u8; 71]);

impl ContentDigest {
    pub fn as_str(&self) -> &str {
        // ContentDigest is only constructed from the ASCII prefix and lowercase hex.
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }

    fn from_hash(hash: blake3::Hash) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut digest = [0_u8; 71];
        digest[..7].copy_from_slice(b"blake3:");
        for (index, byte) in hash.as_bytes().iter().copied().enumerate() {
            digest[7 + index * 2] = HEX[(byte >> 4) as usize];
            digest[8 + index * 2] = HEX[(byte & 0x0f) as usize];
        }
        Self(digest)
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        if value.len() != 71
            || !value.starts_with("blake3:")
            || !value.as_bytes()[7..].iter().all(u8::is_ascii_hexdigit)
        {
            return None;
        }
        let mut digest = [0_u8; 71];
        digest.copy_from_slice(value.as_bytes());
        Some(Self(digest))
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Revision {
    id: RevisionId,
    digest: ContentDigest,
    epoch: Epoch,
    sequence: u64,
    predecessor: Option<Epoch>,
    owner_boot_nonce: OwnerNonce,
    process_start: ProcessStartIdentity,
}

impl Revision {
    pub fn id(&self) -> RevisionId {
        self.id
    }

    pub fn digest(&self) -> &ContentDigest {
        &self.digest
    }

    pub fn epoch(&self) -> EpochId {
        EpochId(self.epoch.0)
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
            Self::UnsafePath(path) => {
                write!(formatter, "unsafe workspace path: {}", path.display())
            }
            Self::Symlink(path) => write!(
                formatter,
                "workspace contains a symlink: {}",
                path.display()
            ),
            Self::NotDirectory(path) => {
                write!(
                    formatter,
                    "workspace root is not a directory: {}",
                    path.display()
                )
            }
            Self::UnsupportedEntry(path) => {
                write!(
                    formatter,
                    "workspace contains a special entry: {}",
                    path.display()
                )
            }
            Self::Hardlink(path) => {
                write!(
                    formatter,
                    "workspace contains a hardlinked file: {}",
                    path.display()
                )
            }
            Self::MountBoundary(path) => {
                write!(
                    formatter,
                    "workspace crosses a mount boundary: {}",
                    path.display()
                )
            }
            Self::LimitExceeded(kind) => {
                write!(formatter, "workspace scan exceeded {kind:?} limit")
            }
            Self::ScanRace { attempts } => {
                write!(
                    formatter,
                    "workspace changed during {attempts} scan attempt(s)"
                )
            }
            Self::StaleRevision { expected, current } => write!(
                formatter,
                "stale workspace revision {expected}; current revision is {}",
                current.id
            ),
            Self::InvalidRange(reason) => write!(formatter, "invalid workspace range: {reason}"),
            Self::NotFound(path) => {
                write!(formatter, "workspace file not found: {}", path.display())
            }
            Self::CorruptMetadata => formatter.write_str("workspace revision metadata is corrupt"),
            Self::Unavailable { reason } => {
                write!(formatter, "workspace revision unavailable: {reason}")
            }
            Self::Io { operation, source } => write!(formatter, "{operation}: {source}"),
        }
    }
}

impl std::error::Error for RevisionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub struct ManagedWorkspace {
    inner: Arc<Inner>,
}

pub struct WorkspaceMutationGuard<'a> {
    stable: WorkspaceStableReadGuard<'a>,
    nonce: MutationGuardNonce,
}

pub(crate) struct PreparedWorkspaceCommit {
    revision: Revision,
    base: RevisionId,
    guard: MutationGuardNonce,
}

impl PreparedWorkspaceCommit {
    pub(crate) fn revision(&self) -> &Revision {
        &self.revision
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct MutationGuardNonce([u8; 16]);

pub(crate) struct WorkspaceKernelMutationFence {
    fence: MutationFence,
    memory: MemoryBudget,
    watched: Vec<OwnerKey>,
    watched_capacity: usize,
    generation: u64,
}

pub struct WorkspaceStableReadGuard<'a> {
    inner: &'a Inner,
    revision: MutexGuard<'a, Revision>,
    _operation: MetadataLock<'a>,
    expected: RevisionId,
}

struct Inner {
    root: File,
    root_path: PathBuf,
    metadata: MetadataStore,
    root_identity: RootIdentity,
    options: RevisionOptions,
    state: Mutex<Revision>,
    dirty: AtomicBool,
    hint_generation: AtomicU64,
    watcher_lost: AtomicBool,
    watcher_control: Arc<WatcherControl>,
    watcher: Mutex<Option<JoinHandle<()>>>,
}

struct WorkspaceOwner {
    _ownership: File,
    guard: File,
    guard_key: OwnerKey,
    operation: Mutex<()>,
    session: Mutex<OwnerSession>,
    live_handles: AtomicUsize,
    initialized: AtomicBool,
}

struct OwnerSession {
    boot_nonce: OwnerNonce,
    process_start: ProcessStartIdentity,
}

struct OwnerLease {
    owner: Arc<WorkspaceOwner>,
    handles: AtomicUsize,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct OwnerKey {
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OwnerNonce([u8; 16]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessStartIdentity([u8; 16]);

static OWNERS: OnceLock<Mutex<HashMap<OwnerKey, Weak<WorkspaceOwner>>>> = OnceLock::new();

struct WatcherControl {
    stopped: AtomicBool,
    wait: Mutex<()>,
    wake: Condvar,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Epoch([u8; 16]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RootIdentity {
    device: u64,
    inode: u64,
}

impl ManagedWorkspace {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, RevisionError> {
        Self::open_with_options(root, RevisionOptions::default())
    }

    pub fn open_with_options(
        root: impl AsRef<Path>,
        options: RevisionOptions,
    ) -> Result<Self, RevisionError> {
        validate_options(&options)?;
        let requested_root = root.as_ref();
        let root_file = open_absolute_directory(requested_root)?;
        let root_path = fs::canonicalize(requested_root)
            .map_err(|source| io_error("canonicalize workspace root", source))?;
        let root_metadata = unix_metadata(&root_file)
            .map_err(|source| io_error("inspect workspace root", source))?;
        if root_metadata.kind() != libc::S_IFDIR {
            return Err(RevisionError::NotDirectory(root_path));
        }
        let root_identity = RootIdentity {
            device: root_metadata.device,
            inode: root_metadata.inode,
        };
        let metadata = MetadataStore::open(&root_file, &root_path, root_identity, &options)?;
        let revision = {
            let operation_deadline = deadline(options.max_scan_time);
            let _lock = metadata.lock(operation_deadline, None)?;
            let takeover = !metadata.owner.initialized.load(Ordering::Acquire);
            recover_pending(&root_file, &metadata)?;
            if takeover {
                crate::workspace::edit::stage::recover_allocations(
                    &metadata.stage_root,
                    &metadata.stage_root_path,
                )
                .map_err(|_| RevisionError::Unavailable {
                    reason: "private stage cleanup recovery failed",
                })?;
            }
            let scanned = scan_stable(
                &root_file,
                &root_path,
                &options,
                operation_deadline,
                None,
                Capture::None,
            )?;
            if let Some(path) = scanned.hardlink {
                return Err(RevisionError::Hardlink(path));
            }
            let previous = metadata.load_consistent()?;
            let revision = next_revision(
                previous.as_ref(),
                None,
                scanned.digest,
                root_identity,
                &metadata.owner,
                takeover,
            )?;
            if takeover
                || previous
                    .as_ref()
                    .is_none_or(|stored| !stored.matches(&revision, root_identity))
            {
                metadata.persist(&revision, root_identity)?;
            }
            metadata.owner.initialized.store(true, Ordering::Release);
            revision
        };

        let watcher_control = Arc::new(WatcherControl {
            stopped: AtomicBool::new(false),
            wait: Mutex::new(()),
            wake: Condvar::new(),
        });
        let inner = Arc::new(Inner {
            root: root_file,
            root_path,
            metadata,
            root_identity,
            options,
            state: Mutex::new(revision),
            dirty: AtomicBool::new(false),
            hint_generation: AtomicU64::new(0),
            watcher_lost: AtomicBool::new(false),
            watcher_control,
            watcher: Mutex::new(None),
        });
        spawn_watcher(&inner);
        Ok(Self { inner })
    }

    pub fn current_revision(&self) -> Result<Revision, RevisionError> {
        self.bounded_scan(Capture::None).map(|scan| scan.revision)
    }

    pub(crate) fn canonical_root(&self) -> &Path {
        &self.inner.root_path
    }

    pub(crate) fn duplicate_root(&self) -> Result<File, RevisionError> {
        self.inner
            .root
            .try_clone()
            .map_err(|source| io_error("duplicate workspace root handle", source))
    }

    pub fn is_dirty(&self) -> bool {
        self.inner.dirty.load(Ordering::Acquire)
    }

    pub fn mark_dirty(&self) {
        mark_dirty(&self.inner);
    }

    pub fn inject_watcher_loss(&self) {
        self.inner.watcher_lost.store(true, Ordering::Release);
        mark_dirty(&self.inner);
    }

    pub fn reconcile(&self) -> Result<Revision, RevisionError> {
        reconcile_inner(&self.inner, Capture::None, None).map(|scan| scan.revision)
    }

    pub fn snapshot(&self, expected: RevisionId) -> Result<Snapshot, RevisionError> {
        let scan = self.bounded_scan(Capture::Snapshot {
            max_file_bytes: usize::MAX,
            max_special_file_bytes: usize::MAX,
            max_content_bytes: u64::MAX,
        })?;
        require_revision(expected, &scan.revision)?;
        Ok(Snapshot {
            revision: scan.revision,
            entries: scan.entries,
        })
    }

    pub fn metadata_snapshot_before(
        &self,
        expected: RevisionId,
        max_file_bytes: usize,
        max_special_file_bytes: usize,
        max_content_bytes: u64,
        deadline: Instant,
    ) -> Result<Snapshot, RevisionError> {
        let scan = self.bounded_scan_until(
            Capture::Snapshot {
                max_file_bytes,
                max_special_file_bytes,
                max_content_bytes,
            },
            Some(deadline),
        )?;
        require_revision(expected, &scan.revision)?;
        Ok(Snapshot {
            revision: scan.revision,
            entries: scan.entries,
        })
    }

    pub fn read_file(
        &self,
        expected: RevisionId,
        path: impl AsRef<Path>,
    ) -> Result<Vec<u8>, RevisionError> {
        self.read_file_until(expected, path, None)
    }

    pub fn read_file_before(
        &self,
        expected: RevisionId,
        path: impl AsRef<Path>,
        deadline: Instant,
    ) -> Result<Vec<u8>, RevisionError> {
        self.read_file_until(expected, path, Some(deadline))
    }

    fn read_file_until(
        &self,
        expected: RevisionId,
        path: impl AsRef<Path>,
        deadline: Option<Instant>,
    ) -> Result<Vec<u8>, RevisionError> {
        let path = path.as_ref();
        validate_relative_path(path)?;
        let scan = self.bounded_scan_until(
            Capture::File {
                path,
                range: FileReadRange::Full,
                max_bytes: usize::MAX,
            },
            deadline,
        )?;
        require_revision(expected, &scan.revision)?;
        scan.file.map(|file| file.bytes).ok_or_else(|| {
            RevisionError::NotFound(
                scan.requested_path
                    .expect("file scans retain their precharged request path"),
            )
        })
    }

    pub fn read_file_range_before(
        &self,
        expected: RevisionId,
        path: impl AsRef<Path>,
        range: FileReadRange,
        max_bytes: usize,
        deadline: Instant,
    ) -> Result<BoundedFileRead, RevisionError> {
        let path = path.as_ref();
        validate_relative_path(path)?;
        if max_bytes == 0 {
            return Err(RevisionError::LimitExceeded(LimitKind::Memory));
        }
        match range {
            FileReadRange::Bytes { start, end } if start >= end => {
                return Err(RevisionError::InvalidRange(
                    "byte end must be greater than start",
                ));
            }
            FileReadRange::Lines { start, end } if start == 0 || start > end => {
                return Err(RevisionError::InvalidRange(
                    "lines are one-based and ordered",
                ));
            }
            _ => {}
        }
        let scan = self.bounded_scan_until(
            Capture::File {
                path,
                range,
                max_bytes,
            },
            Some(deadline),
        )?;
        require_revision(expected, &scan.revision)?;
        scan.file.ok_or_else(|| {
            RevisionError::NotFound(
                scan.requested_path
                    .expect("file scans retain their precharged request path"),
            )
        })
    }

    pub fn validate_revision(&self, expected: RevisionId) -> Result<Revision, RevisionError> {
        let current = self.current_revision()?;
        require_revision(expected, &current)?;
        Ok(current)
    }

    pub fn validate_revision_until(
        &self,
        expected: RevisionId,
        deadline: Instant,
    ) -> Result<Revision, RevisionError> {
        if Instant::now() >= deadline {
            return Err(RevisionError::LimitExceeded(LimitKind::Time));
        }
        let scan = self.bounded_scan_until(Capture::None, Some(deadline))?;
        require_revision(expected, &scan.revision)?;
        Ok(scan.revision)
    }

    pub fn mutation_guard(
        &self,
        expected: RevisionId,
    ) -> Result<WorkspaceMutationGuard<'_>, RevisionError> {
        self.mutation_guard_until(expected, None)
    }

    pub fn mutation_guard_before(
        &self,
        expected: RevisionId,
        request_deadline: Instant,
    ) -> Result<WorkspaceMutationGuard<'_>, RevisionError> {
        self.mutation_guard_until(expected, Some(request_deadline))
    }

    fn mutation_guard_until(
        &self,
        expected: RevisionId,
        request_deadline: Option<Instant>,
    ) -> Result<WorkspaceMutationGuard<'_>, RevisionError> {
        let operation_deadline = request_deadline.map_or_else(
            || deadline(self.inner.options.max_scan_time),
            |request| deadline(self.inner.options.max_scan_time).min(request),
        );
        let mut revision = if let Some(request_deadline) = request_deadline {
            loop {
                match self.inner.state.try_lock() {
                    Ok(revision) => break revision,
                    Err(TryLockError::Poisoned(error)) => break error.into_inner(),
                    Err(TryLockError::WouldBlock) if Instant::now() >= request_deadline => {
                        return Err(RevisionError::LimitExceeded(LimitKind::Time));
                    }
                    Err(TryLockError::WouldBlock) => thread::yield_now(),
                }
            }
        } else {
            self.inner.state.lock().unwrap()
        };
        let operation = self.inner.metadata.lock(
            operation_deadline,
            Some(&self.inner.watcher_control.stopped),
        )?;
        let scan = reconcile_locked(
            &self.inner,
            &mut revision,
            Capture::None,
            operation_deadline,
            request_deadline,
        )?;
        require_revision(expected, &scan.revision)?;
        Ok(WorkspaceMutationGuard {
            stable: WorkspaceStableReadGuard {
                inner: &self.inner,
                revision,
                _operation: operation,
                expected,
            },
            nonce: MutationGuardNonce(random_nonce()?),
        })
    }

    pub fn stable_read_guard_before(
        &self,
        expected: RevisionId,
        request_deadline: Instant,
    ) -> Result<WorkspaceStableReadGuard<'_>, RevisionError> {
        let operation_deadline = deadline(self.inner.options.max_scan_time).min(request_deadline);
        let mut revision = self.inner.state.lock().unwrap();
        let operation = self.inner.metadata.lock(
            operation_deadline,
            Some(&self.inner.watcher_control.stopped),
        )?;
        let scan = reconcile_locked(
            &self.inner,
            &mut revision,
            Capture::None,
            operation_deadline,
            Some(request_deadline),
        )?;
        require_revision(expected, &scan.revision)?;
        Ok(WorkspaceStableReadGuard {
            inner: &self.inner,
            revision,
            _operation: operation,
            expected,
        })
    }

    fn bounded_scan(&self, capture: Capture<'_>) -> Result<Reconciled, RevisionError> {
        self.bounded_scan_until(capture, None)
    }

    fn bounded_scan_until(
        &self,
        capture: Capture<'_>,
        deadline: Option<Instant>,
    ) -> Result<Reconciled, RevisionError> {
        match reconcile_inner(&self.inner, capture, deadline) {
            Ok(scan) => {
                self.inner.watcher_lost.store(false, Ordering::Release);
                Ok(scan)
            }
            Err(error) if self.inner.watcher_lost.swap(false, Ordering::AcqRel) => {
                Err(RevisionError::Unavailable {
                    reason: watcher_loss_reason(&error),
                })
            }
            Err(error) => Err(error),
        }
    }
}

impl Clone for ManagedWorkspace {
    fn clone(&self) -> Self {
        self.inner.metadata.owner.retain();
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl WorkspaceMutationGuard<'_> {
    pub fn revision(&self) -> &Revision {
        &self.stable.revision
    }

    pub fn validate_revision_until(
        &mut self,
        expected: RevisionId,
        request_deadline: Instant,
    ) -> Result<Revision, RevisionError> {
        if expected != self.stable.expected {
            return Err(RevisionError::StaleRevision {
                expected,
                current: Box::new(self.stable.revision.clone()),
            });
        }
        self.stable.validate_before(request_deadline)
    }

    pub(crate) fn validate_held_revision_until(
        &mut self,
        expected: RevisionId,
        request_deadline: Instant,
    ) -> Result<Revision, RevisionError> {
        self.validate_revision_until(expected, request_deadline)
    }

    pub(crate) fn path_authorization_root(&self) -> Result<(File, PathBuf), RevisionError> {
        self.stable.path_authorization_root()
    }

    pub(crate) fn stage_allocation_root(&self) -> Result<(File, PathBuf), RevisionError> {
        self.stable.stage_allocation_root()
    }

    pub(crate) fn path_authorization_nonce(&self) -> MutationGuardNonce {
        self.nonce
    }

    pub(crate) fn stage_binding(&self, plan_digest: &str) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"kit-edit-stage-binding-v1\0");
        hasher.update(&self.nonce.0);
        hasher.update(plan_digest.as_bytes());
        *hasher.finalize().as_bytes()
    }

    pub(crate) fn path_authorization_fence(
        &self,
        max_memory_bytes: usize,
    ) -> Result<WorkspaceKernelMutationFence, RevisionError> {
        self.stable.path_authorization_fence(max_memory_bytes)
    }

    pub(crate) fn recovery_roots(&self) -> Result<(File, File), RevisionError> {
        Ok((
            self.stable
                .inner
                .root
                .try_clone()
                .map_err(|source| io_error("clone workspace recovery root", source))?,
            self.stable
                .inner
                .metadata
                .stage_root
                .try_clone()
                .map_err(|source| io_error("clone edit recovery state root", source))?,
        ))
    }

    pub(crate) fn prepare_commit(
        &mut self,
        final_digest: &str,
        request_deadline: Instant,
    ) -> Result<PreparedWorkspaceCommit, RevisionError> {
        self.validate_held_revision_until(self.stable.expected, request_deadline)?;
        let digest = ContentDigest::parse(final_digest).ok_or(RevisionError::CorruptMetadata)?;
        let stored = self.stable.inner.metadata.load_consistent()?;
        let revision = next_revision(
            stored.as_ref(),
            Some(&self.stable.revision),
            digest,
            self.stable.inner.root_identity,
            &self.stable.inner.metadata.owner,
            false,
        )?;
        Ok(PreparedWorkspaceCommit {
            revision,
            base: self.stable.expected,
            guard: self.nonce,
        })
    }

    pub(crate) fn commit_prepared(
        &mut self,
        prepared: &PreparedWorkspaceCommit,
        request_deadline: Instant,
    ) -> Result<Revision, RevisionError> {
        if prepared.guard != self.nonce || prepared.base != self.stable.expected {
            return Err(RevisionError::Unavailable {
                reason: "workspace commit fence is stale",
            });
        }
        let operation_deadline =
            deadline(self.stable.inner.options.max_scan_time).min(request_deadline);
        let scanned = scan_stable(
            &self.stable.inner.root,
            &self.stable.inner.root_path,
            &self.stable.inner.options,
            operation_deadline,
            Some(&self.stable.inner.watcher_control.stopped),
            Capture::None,
        )?;
        if let Some(path) = scanned.hardlink {
            return Err(RevisionError::Hardlink(path));
        }
        if scanned.digest != prepared.revision.digest {
            return Err(RevisionError::ScanRace { attempts: 1 });
        }
        let stored = self.stable.inner.metadata.load_consistent()?;
        let revision = next_revision(
            stored.as_ref(),
            Some(&self.stable.revision),
            scanned.digest,
            self.stable.inner.root_identity,
            &self.stable.inner.metadata.owner,
            false,
        )?;
        if revision != prepared.revision {
            return Err(RevisionError::Unavailable {
                reason: "prepared workspace revision changed",
            });
        }
        self.stable
            .inner
            .metadata
            .persist(&revision, self.stable.inner.root_identity)?;
        self.stable.revision.clone_from(&revision);
        self.stable.inner.dirty.store(false, Ordering::Release);
        Ok(revision)
    }
}

impl WorkspaceKernelMutationFence {
    pub(crate) fn watch(
        &mut self,
        path: &Path,
        file: &File,
        directory: bool,
    ) -> Result<(), RevisionError> {
        let metadata = unix_metadata(file)
            .map_err(|source| io_error("inspect kernel mutation watch", source))?;
        let key = OwnerKey {
            device: metadata.device,
            inode: metadata.inode,
        };
        if self.watched.contains(&key) {
            return Ok(());
        }
        reserve_vec_slot(
            &mut self.watched,
            &mut self.watched_capacity,
            &mut self.memory,
        )?;
        self.fence.watch(path, file, directory, &mut self.memory)?;
        self.watched.push(key);
        Ok(())
    }

    pub(crate) fn ensure_clean(&mut self) -> Result<(), RevisionError> {
        let events = self.fence.drain()?;
        self.generation =
            self.generation
                .checked_add(events as u64)
                .ok_or(RevisionError::Unavailable {
                    reason: "kernel workspace mutation generation exhausted",
                })?;
        if events == 0 {
            Ok(())
        } else {
            Err(RevisionError::ScanRace { attempts: 1 })
        }
    }

    pub(crate) fn reset_after_verified_read(&mut self) -> Result<(), RevisionError> {
        match self.ensure_clean() {
            Ok(()) | Err(RevisionError::ScanRace { .. }) => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

impl WorkspaceStableReadGuard<'_> {
    pub(crate) fn path_authorization_root(&self) -> Result<(File, PathBuf), RevisionError> {
        Ok((
            self.inner
                .root
                .try_clone()
                .map_err(|source| io_error("clone workspace root handle", source))?,
            self.inner.root_path.clone(),
        ))
    }

    pub(crate) fn stage_allocation_root(&self) -> Result<(File, PathBuf), RevisionError> {
        Ok((
            self.inner
                .metadata
                .stage_root
                .try_clone()
                .map_err(|source| io_error("clone stage state root handle", source))?,
            self.inner.metadata.stage_root_path.clone(),
        ))
    }

    pub(crate) fn path_authorization_fence(
        &self,
        max_memory_bytes: usize,
    ) -> Result<WorkspaceKernelMutationFence, RevisionError> {
        let mut memory = MemoryBudget::new(max_memory_bytes as u64);
        let fence = MutationFence::new(&mut memory)?;
        Ok(WorkspaceKernelMutationFence {
            fence,
            memory,
            watched: Vec::new(),
            watched_capacity: 0,
            generation: 0,
        })
    }

    pub fn read_file_range_before(
        &mut self,
        path: impl AsRef<Path>,
        range: FileReadRange,
        max_bytes: usize,
        request_deadline: Instant,
    ) -> Result<BoundedFileRead, RevisionError> {
        let path = path.as_ref();
        validate_relative_path(path)?;
        if max_bytes == 0 {
            return Err(RevisionError::LimitExceeded(LimitKind::Memory));
        }
        match range {
            FileReadRange::Bytes { start, end } if start >= end => {
                return Err(RevisionError::InvalidRange(
                    "byte end must be greater than start",
                ));
            }
            FileReadRange::Lines { start, end } if start == 0 || start > end => {
                return Err(RevisionError::InvalidRange(
                    "lines are one-based and ordered",
                ));
            }
            _ => {}
        }
        let operation_deadline = deadline(self.inner.options.max_scan_time).min(request_deadline);
        let scan = reconcile_locked(
            self.inner,
            &mut self.revision,
            Capture::File {
                path,
                range,
                max_bytes,
            },
            operation_deadline,
            Some(request_deadline),
        )?;
        require_revision(self.expected, &scan.revision)?;
        scan.file.ok_or_else(|| {
            RevisionError::NotFound(
                scan.requested_path
                    .expect("file scans retain their precharged request path"),
            )
        })
    }

    pub fn validate_before(
        &mut self,
        request_deadline: Instant,
    ) -> Result<Revision, RevisionError> {
        let operation_deadline = deadline(self.inner.options.max_scan_time).min(request_deadline);
        let scan = reconcile_locked(
            self.inner,
            &mut self.revision,
            Capture::None,
            operation_deadline,
            Some(request_deadline),
        )?;
        require_revision(self.expected, &scan.revision)?;
        Ok(scan.revision)
    }
}

impl Drop for ManagedWorkspace {
    fn drop(&mut self) {
        let last = self.inner.metadata.owner.release_handle();
        if last {
            let _wait = self.inner.watcher_control.wait.lock().unwrap();
            self.inner
                .watcher_control
                .stopped
                .store(true, Ordering::Release);
            self.inner.watcher_control.wake.notify_all();
        }
        self.inner.metadata.owner.release_owner();
        if last && let Some(watcher) = self.inner.watcher.lock().unwrap().take() {
            let _ = watcher.join();
        }
    }
}

fn validate_options(options: &RevisionOptions) -> Result<(), RevisionError> {
    if options.max_entries == 0
        || options.max_name_bytes == 0
        || options.max_bytes == 0
        || options.max_memory_bytes == 0
        || options.max_depth == 0
        || options.max_scan_time.is_zero()
        || options.max_scan_attempts < 2
        || options.watcher_interval.is_zero()
        || options.reconciliation_interval.is_zero()
    {
        return Err(RevisionError::Unavailable {
            reason: "revision limits and intervals must be nonzero",
        });
    }
    Ok(())
}

fn require_revision(expected: RevisionId, current: &Revision) -> Result<(), RevisionError> {
    if expected == current.id {
        Ok(())
    } else {
        Err(RevisionError::StaleRevision {
            expected,
            current: Box::new(current.clone()),
        })
    }
}

fn watcher_loss_reason(error: &RevisionError) -> &'static str {
    match error {
        RevisionError::LimitExceeded(_) => "watcher loss reconciliation exceeded its bound",
        RevisionError::ScanRace { .. } => "workspace kept changing after watcher loss",
        _ => "watcher loss reconciliation failed",
    }
}

fn mark_dirty(inner: &Inner) {
    inner.hint_generation.fetch_add(1, Ordering::AcqRel);
    inner.dirty.store(true, Ordering::Release);
}

fn reconcile_inner(
    inner: &Inner,
    capture: Capture<'_>,
    request_deadline: Option<Instant>,
) -> Result<Reconciled, RevisionError> {
    let mut revision = if let Some(deadline) = request_deadline {
        loop {
            match inner.state.try_lock() {
                Ok(revision) => break revision,
                Err(TryLockError::Poisoned(error)) => break error.into_inner(),
                Err(TryLockError::WouldBlock) if Instant::now() >= deadline => {
                    return Err(RevisionError::LimitExceeded(LimitKind::Time));
                }
                Err(TryLockError::WouldBlock) => thread::yield_now(),
            }
        }
    } else {
        inner.state.lock().unwrap()
    };
    let operation_deadline = request_deadline.map_or_else(
        || deadline(inner.options.max_scan_time),
        |request| deadline(inner.options.max_scan_time).min(request),
    );
    let _lock = inner
        .metadata
        .lock(operation_deadline, Some(&inner.watcher_control.stopped))?;
    reconcile_locked(
        inner,
        &mut revision,
        capture,
        operation_deadline,
        request_deadline,
    )
}

fn reconcile_locked(
    inner: &Inner,
    revision: &mut Revision,
    capture: Capture<'_>,
    operation_deadline: Instant,
    request_deadline: Option<Instant>,
) -> Result<Reconciled, RevisionError> {
    recover_pending(&inner.root, &inner.metadata)?;
    let generation = inner.hint_generation.load(Ordering::Acquire);
    let scanned = scan_stable(
        &inner.root,
        &inner.root_path,
        &inner.options,
        operation_deadline,
        Some(&inner.watcher_control.stopped),
        capture,
    )?;
    check_request_deadline(request_deadline)?;
    let stored = inner.metadata.load_consistent()?;
    let next = next_revision(
        stored.as_ref(),
        Some(revision),
        scanned.digest,
        inner.root_identity,
        &inner.metadata.owner,
        false,
    )?;
    if let Some(path) = scanned.hardlink {
        return Err(RevisionError::Hardlink(path));
    }
    if stored
        .as_ref()
        .is_none_or(|stored| !stored.matches(&next, inner.root_identity))
    {
        inner.metadata.persist(&next, inner.root_identity)?;
    }
    check_request_deadline(request_deadline)?;
    *revision = next;
    if inner.hint_generation.load(Ordering::Acquire) == generation {
        inner.dirty.store(false, Ordering::Release);
    }
    Ok(Reconciled {
        revision: (*revision).clone(),
        entries: scanned.entries,
        file: scanned.file,
        requested_path: scanned.requested_path,
    })
}

fn recover_pending(root: &File, metadata: &MetadataStore) -> Result<(), RevisionError> {
    crate::workspace::edit::recovery::recover_pending(
        root,
        &metadata.stage_root,
        |path| crate::store::artifacts::ArtifactStore::open(path),
        |base_revision,
         base_epoch,
         base_digest,
         successor_revision,
         successor_epoch,
         successor_digest| {
            metadata.recovery_position(
                root,
                &metadata.stage_root,
                base_revision,
                base_epoch,
                base_digest,
                successor_revision,
                successor_epoch,
                successor_digest,
            )
        },
    )
    .map_err(|_| RevisionError::Unavailable {
        reason: "workspace edit recovery failed closed",
    })
}

fn check_request_deadline(deadline: Option<Instant>) -> Result<(), RevisionError> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        Err(RevisionError::LimitExceeded(LimitKind::Time))
    } else {
        Ok(())
    }
}

fn spawn_watcher(inner: &Arc<Inner>) {
    let weak = Arc::downgrade(inner);
    let control = Arc::clone(&inner.watcher_control);
    let interval = inner.options.watcher_interval;
    let reconciliation_interval = inner.options.reconciliation_interval;
    let signature = hint_signature(&inner.root, &inner.options, &control.stopped).ok();
    let watcher = thread::spawn(move || {
        watcher_loop(weak, control, interval, reconciliation_interval, signature);
    });
    *inner.watcher.lock().unwrap() = Some(watcher);
}

fn watcher_loop(
    weak: std::sync::Weak<Inner>,
    control: Arc<WatcherControl>,
    interval: Duration,
    reconciliation_interval: Duration,
    mut signature: Option<blake3::Hash>,
) {
    let mut reconciled_at = Instant::now();
    loop {
        let wait = control.wait.lock().unwrap();
        if control.stopped.load(Ordering::Acquire) {
            return;
        }
        let _ = control.wake.wait_timeout(wait, interval).unwrap();
        if control.stopped.load(Ordering::Acquire) {
            return;
        }
        let Some(inner) = weak.upgrade() else {
            return;
        };
        match hint_signature(&inner.root, &inner.options, &control.stopped) {
            Ok(next) if signature == Some(next) => {}
            Ok(next) => {
                signature = Some(next);
                mark_dirty(&inner);
            }
            Err(_) => mark_dirty(&inner),
        }
        if !control.stopped.load(Ordering::Acquire)
            && reconciled_at.elapsed() >= reconciliation_interval
        {
            if reconcile_inner(&inner, Capture::None, None).is_err() {
                inner.dirty.store(true, Ordering::Release);
            }
            reconciled_at = Instant::now();
        }
    }
}

fn hint_signature(
    root: &File,
    options: &RevisionOptions,
    stopped: &AtomicBool,
) -> Result<blake3::Hash, RevisionError> {
    let mut hasher = blake3::Hasher::new();
    let mut limits = ScanLimits::new(deadline(options.max_scan_time), Some(stopped));
    let mut memory = MemoryBudget::new(options.max_memory_bytes);
    hint_directory(root, 0, options, &mut limits, &mut memory, &mut hasher)?;
    Ok(hasher.finalize())
}

fn hint_directory(
    directory: &File,
    depth: usize,
    options: &RevisionOptions,
    limits: &mut ScanLimits<'_>,
    memory: &mut MemoryBudget,
    hasher: &mut blake3::Hasher,
) -> Result<(), RevisionError> {
    for name in directory_entries(directory, options, limits, memory)? {
        check_time(limits)?;
        let metadata = metadata_at(directory, &name)
            .map_err(|source| io_error("inspect workspace hint entry", source))?;
        hash_frame(hasher, os_slice(&name));
        hasher.update(&metadata.mode.to_le_bytes());
        hasher.update(&metadata.size.to_le_bytes());
        hasher.update(&metadata.modified_seconds.to_le_bytes());
        hasher.update(&metadata.modified_nanoseconds.to_le_bytes());
        hasher.update(&metadata.changed_seconds.to_le_bytes());
        hasher.update(&metadata.changed_nanoseconds.to_le_bytes());
        if metadata.kind() == libc::S_IFDIR {
            if depth >= options.max_depth {
                return Err(RevisionError::LimitExceeded(LimitKind::Depth));
            }
            let child = open_directory_at(directory, &name)
                .map_err(|source| io_error("open workspace hint directory", source))?;
            hint_directory(&child, depth + 1, options, limits, memory, hasher)?;
        }
    }
    Ok(())
}

struct Scanned {
    digest: ContentDigest,
    hardlink: Option<PathBuf>,
    entries: Vec<SnapshotEntry>,
    file: Option<BoundedFileRead>,
    requested_path: Option<PathBuf>,
    scanned_entries: usize,
    scanned_bytes: u64,
}

struct Reconciled {
    revision: Revision,
    entries: Vec<SnapshotEntry>,
    file: Option<BoundedFileRead>,
    requested_path: Option<PathBuf>,
}

#[derive(Clone, Copy)]
enum Capture<'a> {
    None,
    Snapshot {
        max_file_bytes: usize,
        max_special_file_bytes: usize,
        max_content_bytes: u64,
    },
    File {
        path: &'a Path,
        range: FileReadRange,
        max_bytes: usize,
    },
}

struct ScanLimits<'a> {
    entries: usize,
    name_bytes: usize,
    bytes: u64,
    deadline: Instant,
    stopped: Option<&'a AtomicBool>,
}

impl<'a> ScanLimits<'a> {
    fn new(deadline: Instant, stopped: Option<&'a AtomicBool>) -> Self {
        Self {
            entries: 0,
            name_bytes: 0,
            bytes: 0,
            deadline,
            stopped,
        }
    }
}

struct MemoryBudget {
    used: u64,
    max: u64,
}

impl MemoryBudget {
    fn new(max: u64) -> Self {
        Self { used: 0, max }
    }

    fn reserve(&mut self, bytes: u64) -> Result<(), RevisionError> {
        self.used = self
            .used
            .checked_add(bytes)
            .ok_or(RevisionError::LimitExceeded(LimitKind::Memory))?;
        if self.used > self.max {
            Err(RevisionError::LimitExceeded(LimitKind::Memory))
        } else {
            Ok(())
        }
    }

    fn reserve_allocation(&mut self, payload: usize) -> Result<(), RevisionError> {
        self.reserve(allocation_charge(payload)?)
    }
}

fn allocation_charge(payload: usize) -> Result<u64, RevisionError> {
    if payload == 0 {
        return Ok(0);
    }
    let payload =
        u64::try_from(payload).map_err(|_| RevisionError::LimitExceeded(LimitKind::Memory))?;
    let quantum = if payload <= MEMORY_PAGE_BYTES {
        MEMORY_SLAB_BYTES
    } else {
        MEMORY_PAGE_BYTES
    };
    payload
        .checked_add(quantum - 1)
        .and_then(|value| value.checked_div(quantum))
        .and_then(|value| value.checked_mul(quantum))
        .and_then(|value| value.checked_add(MEMORY_HEADER_BYTES))
        .ok_or(RevisionError::LimitExceeded(LimitKind::Memory))
}

fn scan_stable(
    root: &File,
    display_root: &Path,
    options: &RevisionOptions,
    deadline: Instant,
    stopped: Option<&AtomicBool>,
    capture: Capture<'_>,
) -> Result<Scanned, RevisionError> {
    for attempt_number in 1..=options.max_scan_attempts {
        let mut memory = MemoryBudget::new(options.max_memory_bytes);
        let attempt = (|| {
            let mut scanned_bytes = 0_u64;
            let mut fence = MutationFence::new(&mut memory)?;
            let first_started = Instant::now();
            let first = scan_once(
                root,
                display_root,
                options,
                deadline,
                stopped,
                Capture::None,
                &mut fence,
                &mut memory,
                &mut scanned_bytes,
            )?;
            profile_scan(attempt_number, 1, &first, first_started.elapsed());
            fence.ensure_clean()?;
            let second_started = Instant::now();
            let second = scan_once(
                root,
                display_root,
                options,
                deadline,
                stopped,
                capture,
                &mut fence,
                &mut memory,
                &mut scanned_bytes,
            )?;
            profile_scan(attempt_number, 2, &second, second_started.elapsed());
            fence.ensure_clean()?;
            if first.digest != second.digest {
                return Err(RevisionError::ScanRace { attempts: 1 });
            }
            Ok(second)
        })();
        match attempt {
            Ok(scanned) => return Ok(scanned),
            Err(RevisionError::ScanRace { .. }) if Instant::now() < deadline => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(RevisionError::LimitExceeded(LimitKind::Time));
        }
    }
    Err(RevisionError::ScanRace {
        attempts: options.max_scan_attempts,
    })
}

#[allow(clippy::too_many_arguments)]
fn scan_once(
    root: &File,
    display_root: &Path,
    options: &RevisionOptions,
    deadline: Instant,
    stopped: Option<&AtomicBool>,
    capture: Capture<'_>,
    fence: &mut MutationFence,
    memory: &mut MemoryBudget,
    scanned_bytes: &mut u64,
) -> Result<Scanned, RevisionError> {
    let bytes_before = *scanned_bytes;
    let root_before =
        unix_metadata(root).map_err(|source| io_error("inspect workspace root", source))?;
    let path_root = open_absolute_directory(display_root)?;
    let path_before = unix_metadata(&path_root)
        .map_err(|source| io_error("inspect workspace root path", source))?;
    if !root_before.same_object(&path_before) {
        return Err(RevisionError::ScanRace { attempts: 1 });
    }
    let filesystem = Filesystem {
        device: root_before.device,
        mount: mount_identity(root)?,
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(DIGEST_MAGIC);
    let mut entries = Vec::new();
    let mut entries_capacity = 0;
    let mut hardlink = None;
    let mut captured_file = None;
    let mut captured_bytes = 0_u64;
    let requested_path = match capture {
        Capture::File { path, .. } => Some(clone_path(path, memory)?),
        _ => None,
    };
    let mut limits = ScanLimits::new(deadline, stopped);
    limits.bytes = *scanned_bytes;
    scan_directory(
        root,
        display_root,
        Path::new(""),
        0,
        &filesystem,
        options,
        &mut limits,
        memory,
        fence,
        capture,
        &mut hasher,
        &mut entries,
        &mut entries_capacity,
        &mut hardlink,
        &mut captured_file,
        &mut captured_bytes,
    )?;
    *scanned_bytes = limits.bytes;
    let root_after =
        unix_metadata(root).map_err(|source| io_error("reinspect workspace root", source))?;
    if !root_before.same_directory(&root_after) {
        return Err(RevisionError::ScanRace { attempts: 1 });
    }
    let path_root = open_absolute_directory(display_root)?;
    let path_after = unix_metadata(&path_root)
        .map_err(|source| io_error("reinspect workspace root path", source))?;
    if !root_before.same_object(&path_after) {
        return Err(RevisionError::ScanRace { attempts: 1 });
    }
    Ok(Scanned {
        digest: ContentDigest::from_hash(hasher.finalize()),
        hardlink,
        entries,
        file: captured_file,
        requested_path,
        scanned_entries: limits.entries,
        scanned_bytes: limits.bytes - bytes_before,
    })
}

fn profile_scan(attempt: usize, pass: usize, scanned: &Scanned, elapsed: Duration) {
    if std::env::var("KIT_WORKSPACE_SCAN_PROFILE").as_deref() == Ok("1") {
        eprintln!(
            "kit_workspace_scan attempt={attempt} pass={pass} entries={} bytes={} elapsed_ms={}",
            scanned.scanned_entries,
            scanned.scanned_bytes,
            elapsed.as_millis()
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_directory(
    directory: &File,
    display_root: &Path,
    relative: &Path,
    depth: usize,
    filesystem: &Filesystem,
    options: &RevisionOptions,
    limits: &mut ScanLimits<'_>,
    memory: &mut MemoryBudget,
    fence: &mut MutationFence,
    capture: Capture<'_>,
    hasher: &mut blake3::Hasher,
    entries: &mut Vec<SnapshotEntry>,
    entries_capacity: &mut usize,
    hardlink: &mut Option<PathBuf>,
    captured_file: &mut Option<BoundedFileRead>,
    captured_bytes: &mut u64,
) -> Result<(), RevisionError> {
    check_time(limits)?;
    let directory_before = unix_metadata(directory)
        .map_err(|source| io_error("inspect workspace directory", source))?;
    if relative.as_os_str().is_empty() {
        fence.watch(display_root, directory, true, memory)?;
    } else {
        let display_path = join_path(display_root, relative, memory)?;
        fence.watch(&display_path, directory, true, memory)?;
    }
    let watched = unix_metadata(directory)
        .map_err(|source| io_error("reinspect watched workspace directory", source))?;
    if !directory_before.same_object(&watched) {
        return Err(RevisionError::ScanRace { attempts: 1 });
    }
    for name in directory_entries(directory, options, limits, memory)? {
        check_time(limits)?;
        let child_relative = join_path(relative, Path::new(&name), memory)?;
        let display_path = join_path(display_root, &child_relative, memory)?;
        let before = match metadata_at(directory, &name) {
            Ok(metadata) => metadata,
            Err(source) => return Err(map_entry_error(display_path, source)),
        };
        if before.device != filesystem.device {
            return Err(RevisionError::MountBoundary(display_path));
        }
        match before.kind() {
            libc::S_IFDIR => {
                if depth >= options.max_depth {
                    return Err(RevisionError::LimitExceeded(LimitKind::Depth));
                }
                hash_entry_header(hasher, b'd', &child_relative, false, 0);
                if matches!(capture, Capture::Snapshot { .. }) {
                    let entry_path = clone_path(&child_relative, memory)?;
                    push_entry(
                        entries,
                        SnapshotEntry {
                            path: entry_path,
                            kind: EntryKind::Directory,
                            executable: false,
                            size: 0,
                            has_nul: false,
                            valid_utf8: true,
                            content_complete: true,
                            bytes: Vec::new(),
                        },
                        entries_capacity,
                        memory,
                    )?;
                }
                let child = match open_directory_at(directory, &name) {
                    Ok(child) => child,
                    Err(source) => return Err(map_entry_error(display_path, source)),
                };
                let opened = unix_metadata(&child)
                    .map_err(|source| io_error("inspect opened workspace directory", source))?;
                if !before.same_object(&opened) {
                    return Err(RevisionError::ScanRace { attempts: 1 });
                }
                if mount_identity(&child)? != filesystem.mount {
                    return Err(RevisionError::MountBoundary(display_path));
                }
                scan_directory(
                    &child,
                    display_root,
                    &child_relative,
                    depth + 1,
                    filesystem,
                    options,
                    limits,
                    memory,
                    fence,
                    capture,
                    hasher,
                    entries,
                    entries_capacity,
                    hardlink,
                    captured_file,
                    captured_bytes,
                )?;
            }
            libc::S_IFREG => {
                if before.links > 1 && hardlink.is_none() {
                    *hardlink = Some(clone_path(&display_path, memory)?);
                }
                let executable = before.mode & 0o111 != 0;
                let mut file =
                    match open_file_at(directory, &name, libc::O_RDONLY | libc::O_NONBLOCK) {
                        Ok(file) => file,
                        Err(source) => return Err(map_entry_error(display_path, source)),
                    };
                let opened = unix_metadata(&file)
                    .map_err(|source| io_error("inspect opened workspace file", source))?;
                if !before.same_file(&opened) {
                    return Err(RevisionError::ScanRace { attempts: 1 });
                }
                if mount_identity(&file)? != filesystem.mount {
                    return Err(RevisionError::MountBoundary(display_path));
                }
                fence.watch(&display_path, &file, false, memory)?;
                let watched = metadata_at(directory, &name).map_err(|source| {
                    if matches!(
                        source.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) {
                        RevisionError::ScanRace { attempts: 1 }
                    } else {
                        io_error("reinspect watched workspace file path", source)
                    }
                })?;
                if !opened.same_object(&watched) {
                    return Err(RevisionError::ScanRace { attempts: 1 });
                }
                let remaining = options.max_bytes.saturating_sub(limits.bytes);
                if before.size > remaining || before.size > usize::MAX as u64 {
                    return Err(RevisionError::LimitExceeded(LimitKind::Bytes));
                }
                hash_entry_header(hasher, b'f', &child_relative, executable, before.size);
                let selected = match capture {
                    Capture::File {
                        path,
                        range,
                        max_bytes,
                    } if path == child_relative => Some((range, max_bytes)),
                    _ => None,
                };
                let retain_snapshot = match capture {
                    Capture::Snapshot {
                        max_file_bytes,
                        max_special_file_bytes,
                        max_content_bytes,
                    } => {
                        let limit = if name == ".gitignore" {
                            max_special_file_bytes
                        } else {
                            max_file_bytes
                        };
                        before.size <= limit as u64
                            && captured_bytes
                                .checked_add(before.size)
                                .is_some_and(|total| total <= max_content_bytes)
                    }
                    _ => false,
                };
                let mut snapshot_bytes = if retain_snapshot {
                    *captured_bytes += before.size;
                    Some(precharged_bytes(before.size as usize, memory)?)
                } else {
                    None
                };
                let mut bounded = selected
                    .map(|(range, max_bytes)| {
                        FileCapture::new(range, max_bytes, before.size as usize, memory)
                    })
                    .transpose()?;
                let mut facts = FileFacts::default();
                let mut buffer = vec![0_u8; 64 * 1024];
                let mut read_size = 0_u64;
                loop {
                    check_time(limits)?;
                    let count = file
                        .read(&mut buffer)
                        .map_err(|source| io_error("read workspace file", source))?;
                    if count == 0 {
                        break;
                    }
                    let next = limits
                        .bytes
                        .checked_add(count as u64)
                        .ok_or(RevisionError::LimitExceeded(LimitKind::Bytes))?;
                    if next > options.max_bytes {
                        return Err(RevisionError::LimitExceeded(LimitKind::Bytes));
                    }
                    limits.bytes = next;
                    let next_read_size = read_size
                        .checked_add(count as u64)
                        .ok_or(RevisionError::LimitExceeded(LimitKind::Bytes))?;
                    if next_read_size > before.size {
                        return Err(RevisionError::ScanRace { attempts: 1 });
                    }
                    read_size = next_read_size;
                    hasher.update(&buffer[..count]);
                    facts.update(&buffer[..count]);
                    if let Some(bytes) = &mut snapshot_bytes {
                        bytes.extend_from_slice(&buffer[..count]);
                    }
                    if let Some(capture) = &mut bounded {
                        capture.update(&buffer[..count], (read_size as usize) - count, memory)?;
                    }
                }
                facts.finish();
                let after = unix_metadata(&file)
                    .map_err(|source| io_error("reinspect workspace file", source))?;
                if !before.same_file(&after)
                    || read_size != before.size
                    || snapshot_bytes
                        .as_ref()
                        .is_some_and(|bytes| bytes.len() as u64 != before.size)
                {
                    return Err(RevisionError::ScanRace { attempts: 1 });
                }
                if matches!(capture, Capture::Snapshot { .. }) {
                    push_entry(
                        entries,
                        SnapshotEntry {
                            path: child_relative,
                            kind: EntryKind::File,
                            executable,
                            size: before.size,
                            has_nul: facts.has_nul,
                            valid_utf8: facts.utf8.valid,
                            content_complete: snapshot_bytes.is_some(),
                            bytes: snapshot_bytes.unwrap_or_default(),
                        },
                        entries_capacity,
                        memory,
                    )?;
                } else if let Some(bounded) = bounded {
                    *captured_file = Some(bounded.finish(facts)?);
                }
            }
            libc::S_IFLNK => return Err(RevisionError::Symlink(display_path)),
            _ => return Err(RevisionError::UnsupportedEntry(display_path)),
        }
    }
    let directory_after = unix_metadata(directory)
        .map_err(|source| io_error("reinspect workspace directory", source))?;
    if !directory_before.same_directory(&directory_after) {
        return Err(RevisionError::ScanRace { attempts: 1 });
    }
    Ok(())
}

#[derive(Default)]
struct FileFacts {
    has_nul: bool,
    has_lf: bool,
    has_crlf: bool,
    final_newline: bool,
    newline_count: usize,
    previous: Option<u8>,
    utf8: Utf8Validator,
}

impl FileFacts {
    fn update(&mut self, bytes: &[u8]) {
        self.has_nul |= bytes.contains(&0);
        self.utf8.update(bytes);
        for byte in bytes {
            if *byte == b'\n' {
                self.newline_count += 1;
                if self.previous == Some(b'\r') {
                    self.has_crlf = true;
                } else {
                    self.has_lf = true;
                }
            }
            self.previous = Some(*byte);
        }
        self.final_newline = bytes.last().copied() == Some(b'\n');
    }

    fn finish(&mut self) {
        self.utf8.finish();
    }

    fn line_count(&self, file_bytes: usize) -> usize {
        if file_bytes == 0 {
            1
        } else {
            self.newline_count + usize::from(!self.final_newline)
        }
    }
}

#[derive(Default)]
struct Utf8Validator {
    pending: [u8; 4],
    pending_len: usize,
    valid: bool,
    initialized: bool,
}

impl Utf8Validator {
    fn update(&mut self, mut bytes: &[u8]) {
        if !self.initialized {
            self.initialized = true;
            self.valid = true;
        }
        if !self.valid {
            return;
        }
        if self.pending_len != 0 {
            while !bytes.is_empty() && self.pending_len < self.pending.len() {
                self.pending[self.pending_len] = bytes[0];
                self.pending_len += 1;
                bytes = &bytes[1..];
                match std::str::from_utf8(&self.pending[..self.pending_len]) {
                    Ok(_) => {
                        self.pending_len = 0;
                        break;
                    }
                    Err(error) if error.error_len().is_some() => {
                        self.valid = false;
                        return;
                    }
                    Err(_) => {}
                }
            }
            if self.pending_len != 0 {
                if self.pending_len == self.pending.len() {
                    self.valid = false;
                }
                return;
            }
        }
        if let Err(error) = std::str::from_utf8(bytes) {
            if error.error_len().is_some() {
                self.valid = false;
            } else {
                let trailing = &bytes[error.valid_up_to()..];
                self.pending[..trailing.len()].copy_from_slice(trailing);
                self.pending_len = trailing.len();
            }
        }
    }

    fn finish(&mut self) {
        if !self.initialized {
            self.valid = true;
        }
        if self.pending_len != 0 {
            self.valid = false;
        }
    }
}

struct FileCapture {
    range: FileReadRange,
    max_bytes: usize,
    file_bytes: usize,
    bytes: Vec<u8>,
    logical_capacity: usize,
    byte_start: Option<usize>,
    byte_end: Option<usize>,
    line_start: Option<usize>,
    line_end: Option<usize>,
    current_line: usize,
}

impl FileCapture {
    fn new(
        range: FileReadRange,
        max_bytes: usize,
        file_bytes: usize,
        memory: &mut MemoryBudget,
    ) -> Result<Self, RevisionError> {
        let requested = match range {
            FileReadRange::Full => file_bytes,
            FileReadRange::Bytes { start, end } => {
                if end > file_bytes {
                    return Err(RevisionError::InvalidRange("byte range exceeds the file"));
                }
                end - start
            }
            FileReadRange::Lines { .. } => file_bytes.min(max_bytes).min(4096),
        };
        let logical_capacity = requested.min(max_bytes);
        Ok(Self {
            range,
            max_bytes,
            file_bytes,
            bytes: precharged_bytes(logical_capacity, memory)?,
            logical_capacity,
            byte_start: matches!(range, FileReadRange::Full).then_some(0),
            byte_end: matches!(range, FileReadRange::Full).then_some(file_bytes),
            line_start: None,
            line_end: None,
            current_line: 1,
        })
    }

    fn update(
        &mut self,
        chunk: &[u8],
        offset: usize,
        memory: &mut MemoryBudget,
    ) -> Result<(), RevisionError> {
        match self.range {
            FileReadRange::Full => self.copy_bounded(chunk, memory)?,
            FileReadRange::Bytes { start, end } => {
                let chunk_end = offset + chunk.len();
                let from = start.saturating_sub(offset).min(chunk.len());
                let to = end.min(chunk_end).saturating_sub(offset).min(chunk.len());
                if from < to {
                    self.copy_bounded(&chunk[from..to], memory)?;
                }
                self.record_byte_lines(chunk, offset, start, end);
            }
            FileReadRange::Lines { start, end } => {
                for (within, byte) in chunk.iter().copied().enumerate() {
                    let position = offset + within;
                    if self.current_line == start && self.byte_start.is_none() {
                        self.byte_start = Some(position);
                    }
                    if (start..=end).contains(&self.current_line) {
                        self.copy_bounded(std::slice::from_ref(&byte), memory)?;
                    }
                    if byte == b'\n' {
                        if self.current_line == end {
                            self.byte_end = Some(position + 1);
                        }
                        self.current_line += 1;
                    }
                }
            }
        }
        Ok(())
    }

    fn record_byte_lines(&mut self, chunk: &[u8], offset: usize, start: usize, end: usize) {
        let chunk_end = offset + chunk.len();
        if self.line_start.is_none() && (offset..chunk_end).contains(&start) {
            self.line_start = Some(
                self.current_line
                    + chunk[..start - offset]
                        .iter()
                        .filter(|byte| **byte == b'\n')
                        .count(),
            );
        }
        let last = end - 1;
        if self.line_end.is_none() && (offset..chunk_end).contains(&last) {
            self.line_end = Some(
                self.current_line
                    + chunk[..last - offset]
                        .iter()
                        .filter(|byte| **byte == b'\n')
                        .count(),
            );
        }
        self.current_line += chunk.iter().filter(|byte| **byte == b'\n').count();
    }

    fn copy_bounded(
        &mut self,
        bytes: &[u8],
        memory: &mut MemoryBudget,
    ) -> Result<(), RevisionError> {
        let remaining = self.max_bytes.saturating_sub(self.bytes.len());
        let bytes = &bytes[..bytes.len().min(remaining)];
        let needed = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or(RevisionError::LimitExceeded(LimitKind::Memory))?;
        if needed > self.logical_capacity {
            let next = self
                .logical_capacity
                .max(1)
                .saturating_mul(2)
                .max(needed)
                .min(self.max_bytes);
            memory.reserve_allocation(next)?;
            self.bytes
                .try_reserve_exact(next - self.bytes.len())
                .map_err(|_| RevisionError::LimitExceeded(LimitKind::Memory))?;
            self.logical_capacity = next;
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn finish(mut self, facts: FileFacts) -> Result<BoundedFileRead, RevisionError> {
        match self.range {
            FileReadRange::Full => {}
            FileReadRange::Bytes { end, .. } => {
                self.byte_end = Some(end);
            }
            FileReadRange::Lines { start, end } => {
                let line_count = facts.line_count(self.file_bytes);
                if start > line_count || end > line_count {
                    return Err(RevisionError::InvalidRange("line range exceeds the file"));
                }
                if self.file_bytes == 0 {
                    self.byte_start = Some(0);
                }
                self.byte_end.get_or_insert(self.file_bytes);
                self.line_start = Some(start);
                self.line_end = Some(end);
            }
        }
        if matches!(self.range, FileReadRange::Full) && self.file_bytes != 0 {
            self.line_start = Some(1);
            self.line_end = Some(facts.line_count(self.file_bytes));
        }
        let byte_start = self.byte_start.unwrap_or(match self.range {
            FileReadRange::Bytes { start, .. } => start,
            _ => 0,
        });
        let byte_end = self.byte_end.unwrap_or(byte_start);
        Ok(BoundedFileRead {
            truncated: self.bytes.len() < byte_end.saturating_sub(byte_start),
            bytes: self.bytes,
            file_bytes: self.file_bytes,
            byte_start,
            byte_end,
            line_start: self.line_start,
            line_end: self.line_end,
            has_nul: facts.has_nul,
            valid_utf8: facts.utf8.valid,
            has_lf: facts.has_lf,
            has_crlf: facts.has_crlf,
            final_newline: facts.final_newline,
        })
    }
}

fn hash_entry_header(
    hasher: &mut blake3::Hasher,
    kind: u8,
    path: &Path,
    executable: bool,
    content_len: u64,
) {
    hasher.update(&[kind]);
    hash_frame(hasher, os_slice(path.as_os_str()));
    hasher.update(&[u8::from(executable)]);
    hasher.update(&content_len.to_le_bytes());
}

fn hash_frame(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn check_time(limits: &ScanLimits<'_>) -> Result<(), RevisionError> {
    if limits
        .stopped
        .is_some_and(|stopped| stopped.load(Ordering::Acquire))
    {
        Err(RevisionError::Unavailable {
            reason: "workspace watcher stopped",
        })
    } else if Instant::now() >= limits.deadline {
        Err(RevisionError::LimitExceeded(LimitKind::Time))
    } else {
        Ok(())
    }
}

fn precharged_bytes(length: usize, memory: &mut MemoryBudget) -> Result<Vec<u8>, RevisionError> {
    let mut bytes = Vec::new();
    if length != 0 {
        memory.reserve_allocation(length)?;
        bytes
            .try_reserve_exact(length)
            .map_err(|_| RevisionError::LimitExceeded(LimitKind::Memory))?;
    }
    Ok(bytes)
}

fn push_entry(
    entries: &mut Vec<SnapshotEntry>,
    entry: SnapshotEntry,
    logical_capacity: &mut usize,
    memory: &mut MemoryBudget,
) -> Result<(), RevisionError> {
    reserve_vec_slot(entries, logical_capacity, memory)?;
    entries.push(entry);
    Ok(())
}

fn reserve_vec_slot<T>(
    values: &mut Vec<T>,
    logical_capacity: &mut usize,
    memory: &mut MemoryBudget,
) -> Result<(), RevisionError> {
    if values.len() < *logical_capacity {
        return Ok(());
    }
    let item_size = std::mem::size_of::<T>().max(1);
    let next_capacity = if *logical_capacity == 0 {
        (VECTOR_GROWTH_BYTES / item_size).max(1)
    } else {
        logical_capacity
            .checked_mul(2)
            .ok_or(RevisionError::LimitExceeded(LimitKind::Memory))?
    };
    let additional = next_capacity - *logical_capacity;
    let payload = next_capacity
        .checked_mul(item_size)
        .ok_or(RevisionError::LimitExceeded(LimitKind::Memory))?;
    memory.reserve_allocation(payload)?;
    values
        .try_reserve_exact(additional)
        .map_err(|_| RevisionError::LimitExceeded(LimitKind::Memory))?;
    *logical_capacity = next_capacity;
    Ok(())
}

fn join_path(
    base: &Path,
    child: &Path,
    memory: &mut MemoryBudget,
) -> Result<PathBuf, RevisionError> {
    let length = os_len(base.as_os_str())
        .checked_add(os_len(child.as_os_str()))
        .and_then(|length| length.checked_add(1))
        .ok_or(RevisionError::LimitExceeded(LimitKind::Memory))?;
    memory.reserve_allocation(length)?;
    let mut path = PathBuf::new();
    path.try_reserve_exact(length)
        .map_err(|_| RevisionError::LimitExceeded(LimitKind::Memory))?;
    path.push(base);
    path.push(child);
    Ok(path)
}

fn clone_path(path: &Path, memory: &mut MemoryBudget) -> Result<PathBuf, RevisionError> {
    let length = os_len(path.as_os_str());
    memory.reserve_allocation(length)?;
    let mut clone = PathBuf::new();
    clone
        .try_reserve_exact(length)
        .map_err(|_| RevisionError::LimitExceeded(LimitKind::Memory))?;
    clone.push(path);
    Ok(clone)
}

fn deadline(duration: Duration) -> Instant {
    Instant::now()
        .checked_add(duration)
        .unwrap_or_else(Instant::now)
}

#[cfg(target_os = "linux")]
struct MutationFence {
    events: File,
}

#[cfg(target_os = "linux")]
impl MutationFence {
    fn new(_memory: &mut MemoryBudget) -> Result<Self, RevisionError> {
        use std::os::fd::FromRawFd;

        // SAFETY: inotify_init1 has no pointer arguments and File owns success.
        let descriptor = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if descriptor < 0 {
            return Err(RevisionError::Unavailable {
                reason: "kernel workspace mutation fence",
            });
        }
        Ok(Self {
            // SAFETY: descriptor was newly created and is uniquely owned.
            events: unsafe { File::from_raw_fd(descriptor) },
        })
    }

    fn watch(
        &mut self,
        path: &Path,
        _file: &File,
        directory: bool,
        _memory: &mut MemoryBudget,
    ) -> Result<(), RevisionError> {
        use std::os::fd::AsRawFd;

        let path = CName::new(path.as_os_str()).map_err(|_| RevisionError::Unavailable {
            reason: "kernel mutation watch path is too long",
        })?;
        let mut mask = libc::IN_MODIFY
            | libc::IN_ATTRIB
            | libc::IN_CLOSE_WRITE
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_DELETE_SELF
            | libc::IN_MOVE_SELF
            | libc::IN_UNMOUNT
            | libc::IN_DONT_FOLLOW
            | libc::IN_EXCL_UNLINK;
        if directory {
            mask |= libc::IN_ONLYDIR;
        }
        // SAFETY: descriptor and path are live and mask contains inotify flags.
        if unsafe { libc::inotify_add_watch(self.events.as_raw_fd(), path.as_ptr(), mask) } < 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::ENOENT) | Some(libc::ENOTDIR) => {
                    Err(RevisionError::ScanRace { attempts: 1 })
                }
                _ => Err(RevisionError::Unavailable {
                    reason: "kernel mutation watch cannot cover workspace tree",
                }),
            };
        }
        Ok(())
    }

    fn drain(&mut self) -> Result<usize, RevisionError> {
        let mut buffer = [0_u8; 16 * 1024];
        let mut events = 0_usize;
        loop {
            match self.events.read(&mut buffer) {
                Ok(0) => return Ok(events),
                Ok(count) => {
                    let mut offset = 0;
                    while offset + std::mem::size_of::<libc::inotify_event>() <= count {
                        // SAFETY: bounds above cover the fixed event header; unaligned events are valid.
                        let event = unsafe {
                            std::ptr::read_unaligned(
                                buffer.as_ptr().add(offset).cast::<libc::inotify_event>(),
                            )
                        };
                        if event.mask & (libc::IN_Q_OVERFLOW | libc::IN_IGNORED | libc::IN_UNMOUNT)
                            != 0
                        {
                            return Err(RevisionError::Unavailable {
                                reason: "kernel workspace mutation fence was lost",
                            });
                        }
                        events = events.saturating_add(1);
                        offset = offset
                            .saturating_add(std::mem::size_of::<libc::inotify_event>())
                            .saturating_add(event.len as usize);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(events),
                Err(_) => {
                    return Err(RevisionError::Unavailable {
                        reason: "kernel workspace mutation fence read failed",
                    });
                }
            }
        }
    }

    fn ensure_clean(&mut self) -> Result<(), RevisionError> {
        if self.drain()? == 0 {
            Ok(())
        } else {
            Err(RevisionError::ScanRace { attempts: 1 })
        }
    }
}

#[cfg(target_os = "macos")]
struct MutationFence {
    events: File,
    watched: Vec<File>,
    watched_capacity: usize,
}

#[cfg(target_os = "macos")]
impl MutationFence {
    fn new(_memory: &mut MemoryBudget) -> Result<Self, RevisionError> {
        use std::os::fd::FromRawFd;

        // SAFETY: kqueue has no pointer arguments and File owns success.
        let descriptor = unsafe { libc::kqueue() };
        if descriptor < 0 {
            return Err(RevisionError::Unavailable {
                reason: "kernel workspace mutation fence",
            });
        }
        Ok(Self {
            // SAFETY: descriptor was newly created and is uniquely owned.
            events: unsafe { File::from_raw_fd(descriptor) },
            watched: Vec::new(),
            watched_capacity: 0,
        })
    }

    fn watch(
        &mut self,
        _path: &Path,
        file: &File,
        _directory: bool,
        memory: &mut MemoryBudget,
    ) -> Result<(), RevisionError> {
        use std::os::fd::AsRawFd;

        reserve_vec_slot(&mut self.watched, &mut self.watched_capacity, memory)?;
        let watched = file.try_clone().map_err(|_| RevisionError::Unavailable {
            reason: "kernel mutation watch cannot cover workspace tree",
        })?;
        let change = libc::kevent {
            ident: watched.as_raw_fd() as usize,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
            fflags: libc::NOTE_WRITE
                | libc::NOTE_DELETE
                | libc::NOTE_EXTEND
                | libc::NOTE_ATTRIB
                | libc::NOTE_LINK
                | libc::NOTE_RENAME
                | libc::NOTE_REVOKE,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: kqueue descriptor and change list are live; no event output is requested.
        if unsafe {
            libc::kevent(
                self.events.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(RevisionError::Unavailable {
                reason: "kernel mutation watch cannot cover workspace tree",
            });
        }
        self.watched.push(watched);
        Ok(())
    }

    fn drain(&mut self) -> Result<usize, RevisionError> {
        use std::os::fd::AsRawFd;

        let mut events = [unsafe { std::mem::zeroed::<libc::kevent>() }; 64];
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: all pointers describe initialized, correctly sized storage.
        let mut total = 0_usize;
        loop {
            let count = unsafe {
                libc::kevent(
                    self.events.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    events.as_mut_ptr(),
                    events.len() as i32,
                    &timeout,
                )
            };
            if count < 0 {
                return Err(RevisionError::Unavailable {
                    reason: "kernel workspace mutation fence read failed",
                });
            }
            if events[..count as usize].iter().any(|event| {
                event.flags & (libc::EV_ERROR | libc::EV_EOF) != 0
                    || event.fflags & libc::NOTE_REVOKE != 0
            }) {
                return Err(RevisionError::Unavailable {
                    reason: "kernel workspace mutation fence was lost",
                });
            }
            if count == 0 {
                return Ok(total);
            }
            total = total.saturating_add(count as usize);
        }
    }

    fn ensure_clean(&mut self) -> Result<(), RevisionError> {
        if self.drain()? == 0 {
            Ok(())
        } else {
            Err(RevisionError::ScanRace { attempts: 1 })
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MountIdentity([u8; 32]);

struct Filesystem {
    device: u64,
    mount: MountIdentity,
}

#[derive(Clone, Copy)]
struct UnixMetadata {
    device: u64,
    inode: u64,
    links: u64,
    mode: libc::mode_t,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl UnixMetadata {
    fn kind(self) -> libc::mode_t {
        self.mode & libc::S_IFMT
    }

    fn same_object(self, other: &Self) -> bool {
        self.device == other.device && self.inode == other.inode && self.kind() == other.kind()
    }

    fn same_file(self, other: &Self) -> bool {
        self.same_object(other)
            && self.links == other.links
            && self.size == other.size
            && self.modified_seconds == other.modified_seconds
            && self.modified_nanoseconds == other.modified_nanoseconds
            && self.changed_seconds == other.changed_seconds
            && self.changed_nanoseconds == other.changed_nanoseconds
            && self.mode & 0o777 == other.mode & 0o777
    }

    fn same_directory(self, other: &Self) -> bool {
        self.same_object(other)
            && self.modified_seconds == other.modified_seconds
            && self.modified_nanoseconds == other.modified_nanoseconds
            && self.changed_seconds == other.changed_seconds
            && self.changed_nanoseconds == other.changed_nanoseconds
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredMetadata {
    epoch: Epoch,
    predecessor: Option<Epoch>,
    sequence: u64,
    digest: ContentDigest,
    root: RootIdentity,
    owner_boot_nonce: OwnerNonce,
    process_start: ProcessStartIdentity,
}

struct MetadataStore {
    parent: File,
    state_name: OsString,
    stage_root: File,
    stage_root_path: PathBuf,
    owner: OwnerLease,
}

struct MetadataLock<'a> {
    _operation: MutexGuard<'a, ()>,
    owner: Arc<WorkspaceOwner>,
}

fn acquire_owner(
    ownership: File,
    guard: File,
    root: RootIdentity,
    guard_metadata: UnixMetadata,
) -> Result<OwnerLease, RevisionError> {
    use std::os::fd::AsRawFd;

    let key = OwnerKey {
        device: root.device,
        inode: root.inode,
    };
    let guard_key = OwnerKey {
        device: guard_metadata.device,
        inode: guard_metadata.inode,
    };
    let mut owners = OWNERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(owner) = owners.get(&key).and_then(Weak::upgrade) {
        if owner.guard_key != guard_key {
            return Err(RevisionError::Unavailable {
                reason: "workspace manager metadata location differs from its owner",
            });
        }
        owner.retain_manager()?;
        return Ok(OwnerLease::new(owner));
    }
    let retry_deadline = Instant::now() + OWNER_LOCK_RETRY;
    loop {
        // SAFETY: ownership is a live, uniquely owned workspace-root descriptor.
        if unsafe { libc::flock(ownership.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if ![libc::EWOULDBLOCK, libc::EAGAIN].contains(&error.raw_os_error().unwrap_or_default()) {
            return Err(io_error("claim workspace manager ownership", error));
        }
        if Instant::now() >= retry_deadline {
            return Err(RevisionError::Unavailable {
                reason: "workspace manager is owned by another process",
            });
        }
        thread::sleep(Duration::from_millis(1));
    }

    let owner = Arc::new(WorkspaceOwner {
        _ownership: ownership,
        guard,
        guard_key,
        operation: Mutex::new(()),
        session: Mutex::new(OwnerSession {
            boot_nonce: OwnerNonce([0; 16]),
            process_start: ProcessStartIdentity([0; 16]),
        }),
        live_handles: AtomicUsize::new(0),
        initialized: AtomicBool::new(false),
    });
    owner.retain_manager()?;
    owners.insert(key, Arc::downgrade(&owner));
    Ok(OwnerLease::new(owner))
}

impl WorkspaceOwner {
    fn retain_manager(&self) -> Result<(), RevisionError> {
        let _operation = self.operation.lock().unwrap();
        self.retain_locked()
    }

    fn retain_locked(&self) -> Result<(), RevisionError> {
        let previous = self
            .live_handles
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .map_err(|_| RevisionError::Unavailable {
                reason: "workspace manager count exhausted",
            })?;
        if previous == 0 {
            let identity = (random_nonce(), random_nonce());
            match identity {
                (Ok(boot_nonce), Ok(process_start)) => {
                    let mut session = self.session.lock().unwrap();
                    session.boot_nonce = OwnerNonce(boot_nonce);
                    session.process_start = ProcessStartIdentity(process_start);
                    self.initialized.store(false, Ordering::Release);
                }
                (Err(error), _) | (_, Err(error)) => {
                    self.release_manager();
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn release_manager(&self) {
        self.live_handles
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_sub(1)
            })
            .expect("workspace owner handle count is balanced");
    }

    fn identity(&self) -> (OwnerNonce, ProcessStartIdentity) {
        let session = self.session.lock().unwrap();
        (session.boot_nonce, session.process_start)
    }
}

impl OwnerLease {
    fn new(owner: Arc<WorkspaceOwner>) -> Self {
        Self {
            owner,
            handles: AtomicUsize::new(1),
        }
    }

    fn retain(&self) {
        self.owner
            .retain_manager()
            .expect("a live workspace manager cannot restart its owner session");
        self.handles.fetch_add(1, Ordering::Relaxed);
    }

    fn release_handle(&self) -> bool {
        self.handles.fetch_sub(1, Ordering::AcqRel) == 1
    }

    fn release_owner(&self) {
        self.owner.release_manager();
    }
}

impl std::ops::Deref for OwnerLease {
    type Target = WorkspaceOwner;

    fn deref(&self) -> &Self::Target {
        &self.owner
    }
}

impl Drop for OwnerLease {
    fn drop(&mut self) {
        for _ in 0..*self.handles.get_mut() {
            self.owner.release_manager();
        }
    }
}

impl Drop for MetadataLock<'_> {
    fn drop(&mut self) {
        self.owner.release_manager();
    }
}

fn random_nonce() -> Result<[u8; 16], RevisionError> {
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| RevisionError::Unavailable {
        reason: "secure randomness failed",
    })?;
    Ok(nonce)
}

impl MetadataStore {
    fn open(
        root_file: &File,
        root: &Path,
        root_identity: RootIdentity,
        options: &RevisionOptions,
    ) -> Result<Self, RevisionError> {
        let path = if let Some(path) = &options.metadata_path {
            path.clone()
        } else {
            let parent = root
                .parent()
                .ok_or_else(|| RevisionError::UnsafePath(root.to_path_buf()))?;
            parent.join(format!(
                ".kit-revision-{}-{}.state",
                root_identity.device, root_identity.inode
            ))
        };
        if !path.is_absolute()
            || path.starts_with(root)
            || path
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(RevisionError::UnsafePath(path));
        }
        let parent_path = path
            .parent()
            .ok_or_else(|| RevisionError::UnsafePath(path.clone()))?;
        if parent_path.starts_with(root) {
            return Err(RevisionError::UnsafePath(path));
        }
        let state_name = path
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| RevisionError::UnsafePath(path.clone()))?
            .to_owned();
        let parent = open_absolute_directory(parent_path)?;
        let parent_metadata = unix_metadata(&parent)
            .map_err(|source| io_error("inspect revision metadata parent", source))?;
        if parent_metadata.device == root_identity.device
            && parent_metadata.inode == root_identity.inode
        {
            return Err(RevisionError::UnsafePath(path));
        }
        let mut lock_name = state_name.clone();
        lock_name.push(".lock");
        let lock = open_lock_file_at(&parent, &lock_name, 0o600)
            .map_err(|source| map_metadata_path_error(path.clone(), source))?;
        let lock_metadata = unix_metadata(&lock)
            .map_err(|source| io_error("inspect revision metadata lock", source))?;
        if lock_metadata.kind() != libc::S_IFREG || lock_metadata.links != 1 {
            return Err(RevisionError::UnsafePath(path));
        }
        let ownership = root_file
            .try_clone()
            .map_err(|source| io_error("clone workspace ownership handle", source))?;
        let owner = acquire_owner(ownership, lock, root_identity, lock_metadata)?;
        let mut stage_name = state_name.clone();
        stage_name.push(".staging");
        let stage_cname = CName::new(&stage_name)
            .map_err(|source| map_metadata_path_error(path.clone(), source))?;
        use std::os::fd::AsRawFd as _;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), stage_cname.as_ptr(), 0o700) } != 0 {
            let source = io::Error::last_os_error();
            if source.kind() != io::ErrorKind::AlreadyExists {
                return Err(map_metadata_path_error(path.clone(), source));
            }
        }
        let stage_root = open_directory_at(&parent, &stage_name)
            .map_err(|source| map_metadata_path_error(path.clone(), source))?;
        let stage_metadata = unix_metadata(&stage_root)
            .map_err(|source| io_error("inspect private stage state root", source))?;
        if stage_metadata.kind() != libc::S_IFDIR
            || stage_metadata.links < 2
            || stage_metadata.mode & 0o777 != 0o700
        {
            return Err(RevisionError::UnsafePath(parent_path.join(&stage_name)));
        }
        let stage_root_path = fs::canonicalize(parent_path.join(&stage_name))
            .map_err(|source| io_error("canonicalize private stage state root", source))?;
        Ok(Self {
            parent,
            state_name,
            stage_root,
            stage_root_path,
            owner,
        })
    }

    fn lock(
        &self,
        deadline: Instant,
        stopped: Option<&AtomicBool>,
    ) -> Result<MetadataLock<'_>, RevisionError> {
        loop {
            if stopped.is_some_and(|stopped| stopped.load(Ordering::Acquire)) {
                return Err(RevisionError::Unavailable {
                    reason: "workspace watcher stopped",
                });
            }
            match self.owner.operation.try_lock() {
                Ok(operation) => {
                    self.owner.retain_locked()?;
                    return Ok(MetadataLock {
                        _operation: operation,
                        owner: Arc::clone(&self.owner.owner),
                    });
                }
                Err(TryLockError::Poisoned(error)) => {
                    let operation = error.into_inner();
                    self.owner.retain_locked()?;
                    return Ok(MetadataLock {
                        _operation: operation,
                        owner: Arc::clone(&self.owner.owner),
                    });
                }
                Err(TryLockError::WouldBlock) if Instant::now() >= deadline => {
                    return Err(RevisionError::LimitExceeded(LimitKind::Time));
                }
                Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(2)),
            }
        }
    }

    fn load_consistent(&self) -> Result<Option<StoredMetadata>, RevisionError> {
        let state = match open_file_at(&self.parent, &self.state_name, libc::O_RDONLY) {
            Ok(file) => parse_metadata_file(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => {
                return Err(map_metadata_path_error(
                    PathBuf::from(&self.state_name),
                    source,
                ));
            }
        };
        let guard = parse_metadata_file(
            self.owner
                .guard
                .try_clone()
                .map_err(|source| io_error("clone revision metadata lock", source))?,
        );
        match (state, guard) {
            (Ok(Some(state)), Ok(Some(guard))) if state == guard => Ok(Some(state)),
            (Err(RevisionError::Io { operation, source }), _)
            | (_, Err(RevisionError::Io { operation, source })) => {
                Err(RevisionError::Io { operation, source })
            }
            _ => Ok(None),
        }
    }

    fn persist(&self, revision: &Revision, root: RootIdentity) -> Result<(), RevisionError> {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).map_err(|_| RevisionError::Unavailable {
            reason: "secure randomness failed",
        })?;
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let temporary = OsString::from(format!(".kit-revision-{suffix}.tmp"));
        let result = (|| {
            let mut file = create_new_file_at(&self.parent, &temporary, 0o600)
                .map_err(|source| io_error("create revision metadata", source))?;
            write_metadata(&mut file, revision, root)?;
            crate::workspace::edit::recovery::system_crash(
                crate::workspace::edit::recovery::RecoveryPoint::RevisionStateWrite,
                0,
            );
            file.sync_all()
                .map_err(|source| io_error("sync revision metadata", source))?;
            crate::workspace::edit::recovery::system_crash(
                crate::workspace::edit::recovery::RecoveryPoint::RevisionStateFileSync,
                0,
            );
            rename_at(&self.parent, &temporary, &self.state_name)
                .map_err(|source| io_error("publish revision metadata", source))?;
            crate::workspace::edit::recovery::system_crash(
                crate::workspace::edit::recovery::RecoveryPoint::RevisionStateRename,
                0,
            );
            self.parent
                .sync_all()
                .map_err(|source| io_error("sync revision metadata directory", source))?;
            crate::workspace::edit::recovery::system_crash(
                crate::workspace::edit::recovery::RecoveryPoint::RevisionStateDirectorySync,
                0,
            );

            self.owner
                .guard
                .set_len(0)
                .map_err(|source| io_error("truncate revision metadata guard", source))?;
            let mut guard = &self.owner.guard;
            guard
                .seek(SeekFrom::Start(0))
                .map_err(|source| io_error("seek revision metadata guard", source))?;
            write_metadata(&mut guard, revision, root)?;
            crate::workspace::edit::recovery::system_crash(
                crate::workspace::edit::recovery::RecoveryPoint::RevisionGuardWrite,
                0,
            );
            self.owner
                .guard
                .sync_all()
                .map_err(|source| io_error("sync revision metadata guard", source))?;
            crate::workspace::edit::recovery::system_crash(
                crate::workspace::edit::recovery::RecoveryPoint::RevisionGuardSync,
                0,
            );
            Ok(())
        })();
        if result.is_err() {
            let _ = unlink_at(&self.parent, &temporary);
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn recovery_position(
        &self,
        root: &File,
        metadata_store: &File,
        base_revision: &str,
        base_epoch: &str,
        base_digest: &str,
        successor_revision: &str,
        successor_epoch: &str,
        successor_digest: &str,
    ) -> Result<crate::workspace::edit::recovery::RecoveryPosition, RevisionError> {
        use crate::workspace::edit::recovery::RecoveryPosition;
        let current_root = unix_metadata(root)
            .map_err(|source| io_error("inspect recovery workspace root", source))?;
        let current_store = unix_metadata(metadata_store)
            .map_err(|source| io_error("inspect recovery metadata store", source))?;
        let expected_store = unix_metadata(&self.stage_root)
            .map_err(|source| io_error("inspect configured recovery metadata store", source))?;
        if current_store.device != expected_store.device
            || current_store.inode != expected_store.inode
        {
            return Ok(RecoveryPosition::Other);
        }
        let state = match open_file_at(&self.parent, &self.state_name, libc::O_RDONLY) {
            Ok(file) => parse_metadata_file(file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(io_error("open recovery revision metadata", source)),
        };
        let guard = parse_metadata_file(
            self.owner
                .guard
                .try_clone()
                .map_err(|source| io_error("clone recovery revision guard", source))?,
        )?;
        let classify = |stored: Option<&StoredMetadata>| {
            let Some(stored) = stored else {
                return RecoveryPosition::Other;
            };
            if stored.root.device != current_root.device || stored.root.inode != current_root.inode
            {
                return RecoveryPosition::Other;
            }
            let revision = revision_id(stored.epoch, stored.sequence, &stored.digest).to_string();
            let epoch = EpochId(stored.epoch.0).to_string();
            if revision == base_revision
                && epoch == base_epoch
                && stored.digest.as_str() == base_digest
            {
                RecoveryPosition::Base
            } else if revision == successor_revision
                && epoch == successor_epoch
                && stored.digest.as_str() == successor_digest
            {
                RecoveryPosition::Successor
            } else {
                RecoveryPosition::Other
            }
        };
        let state = classify(state.as_ref());
        let guard = classify(guard.as_ref());
        Ok(match (state, guard) {
            (RecoveryPosition::Base, RecoveryPosition::Base) => RecoveryPosition::Base,
            (RecoveryPosition::Successor, RecoveryPosition::Successor)
            | (RecoveryPosition::Successor, RecoveryPosition::Base)
            | (RecoveryPosition::Base, RecoveryPosition::Successor) => RecoveryPosition::Successor,
            _ => RecoveryPosition::Other,
        })
    }
}

impl StoredMetadata {
    fn matches(&self, revision: &Revision, root: RootIdentity) -> bool {
        self.epoch == revision.epoch
            && self.predecessor == revision.predecessor
            && self.sequence == revision.sequence
            && self.digest == revision.digest
            && self.root == root
            && self.owner_boot_nonce == revision.owner_boot_nonce
            && self.process_start == revision.process_start
    }
}

fn parse_metadata_file(mut file: File) -> Result<Option<StoredMetadata>, RevisionError> {
    let metadata =
        unix_metadata(&file).map_err(|source| io_error("inspect revision metadata", source))?;
    if metadata.kind() != libc::S_IFREG || metadata.links != 1 || metadata.size > 4096 {
        return Err(RevisionError::CorruptMetadata);
    }
    if metadata.size == 0 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|source| io_error("seek revision metadata", source))?;
    let mut bytes = [0_u8; 4096];
    let length = metadata.size as usize;
    file.read_exact(&mut bytes[..length])
        .map_err(|source| io_error("read revision metadata", source))?;
    let text = std::str::from_utf8(&bytes[..length]).map_err(|_| RevisionError::CorruptMetadata)?;
    let mut lines = text.lines();
    if lines.next() != Some(STATE_MAGIC) {
        return Err(RevisionError::CorruptMetadata);
    }
    let epoch = parse_epoch(lines.next(), "epoch=")?;
    let predecessor = match lines
        .next()
        .and_then(|line| line.strip_prefix("predecessor="))
        .ok_or(RevisionError::CorruptMetadata)?
    {
        "none" => None,
        value => Some(parse_epoch_value(value)?),
    };
    let sequence = lines
        .next()
        .and_then(|line| line.strip_prefix("sequence="))
        .and_then(|value| value.parse().ok())
        .ok_or(RevisionError::CorruptMetadata)?;
    let digest = lines
        .next()
        .and_then(|line| line.strip_prefix("digest="))
        .and_then(ContentDigest::parse)
        .ok_or(RevisionError::CorruptMetadata)?;
    let device = parse_metadata_number(lines.next(), "device=")?;
    let inode = parse_metadata_number(lines.next(), "inode=")?;
    let owner_boot_nonce = parse_nonce(lines.next(), "owner_boot_nonce=").map(OwnerNonce)?;
    let process_start = parse_nonce(lines.next(), "process_start=").map(ProcessStartIdentity)?;
    if lines.next().is_some() {
        return Err(RevisionError::CorruptMetadata);
    }
    Ok(Some(StoredMetadata {
        epoch,
        predecessor,
        sequence,
        digest,
        root: RootIdentity { device, inode },
        owner_boot_nonce,
        process_start,
    }))
}

fn parse_epoch(line: Option<&str>, prefix: &str) -> Result<Epoch, RevisionError> {
    let value = line
        .and_then(|line| line.strip_prefix(prefix))
        .ok_or(RevisionError::CorruptMetadata)?;
    parse_epoch_value(value)
}

fn parse_epoch_value(value: &str) -> Result<Epoch, RevisionError> {
    parse_nonce_value(value).map(Epoch)
}

fn parse_nonce(line: Option<&str>, prefix: &str) -> Result<[u8; 16], RevisionError> {
    let value = line
        .and_then(|line| line.strip_prefix(prefix))
        .ok_or(RevisionError::CorruptMetadata)?;
    parse_nonce_value(value)
}

fn parse_nonce_value(value: &str) -> Result<[u8; 16], RevisionError> {
    if value.len() != 32 {
        return Err(RevisionError::CorruptMetadata);
    }
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
            .map_err(|_| RevisionError::CorruptMetadata)?;
    }
    Ok(bytes)
}

fn parse_metadata_number(line: Option<&str>, prefix: &str) -> Result<u64, RevisionError> {
    line.and_then(|line| line.strip_prefix(prefix))
        .and_then(|value| value.parse().ok())
        .ok_or(RevisionError::CorruptMetadata)
}

fn next_revision(
    stored: Option<&StoredMetadata>,
    current: Option<&Revision>,
    digest: ContentDigest,
    root: RootIdentity,
    owner: &WorkspaceOwner,
    force_epoch_rotation: bool,
) -> Result<Revision, RevisionError> {
    let (boot_nonce, process_start) = owner.identity();
    let usable = stored.filter(|stored| {
        !force_epoch_rotation
            && stored.owner_boot_nonce == boot_nonce
            && stored.process_start == process_start
            && stored.root == root
            && current.is_none_or(|current| {
                (stored.epoch == current.epoch
                    && (stored.sequence > current.sequence
                        || (stored.sequence == current.sequence
                            && stored.digest == current.digest)))
                    || stored.predecessor == Some(current.epoch)
            })
    });
    let (epoch, predecessor, sequence) = match usable {
        Some(stored) if stored.digest == digest => {
            (stored.epoch, stored.predecessor, stored.sequence)
        }
        Some(stored) => (
            stored.epoch,
            stored.predecessor,
            stored
                .sequence
                .checked_add(1)
                .ok_or(RevisionError::Unavailable {
                    reason: "revision sequence exhausted",
                })?,
        ),
        None => (
            random_epoch()?,
            current
                .map(|current| current.epoch)
                .or_else(|| stored.map(|stored| stored.epoch)),
            1,
        ),
    };
    let id = revision_id(epoch, sequence, &digest);
    Ok(Revision {
        id,
        digest,
        epoch,
        sequence,
        predecessor,
        owner_boot_nonce: boot_nonce,
        process_start,
    })
}

fn random_epoch() -> Result<Epoch, RevisionError> {
    let mut epoch = [0_u8; 16];
    getrandom::fill(&mut epoch).map_err(|_| RevisionError::Unavailable {
        reason: "secure randomness failed",
    })?;
    Ok(Epoch(epoch))
}

fn revision_id(epoch: Epoch, sequence: u64, digest: &ContentDigest) -> RevisionId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(REVISION_MAGIC);
    hasher.update(&epoch.0);
    hasher.update(&sequence.to_le_bytes());
    hash_frame(&mut hasher, digest.as_str().as_bytes());
    RevisionId(*hasher.finalize().as_bytes())
}

fn hex_epoch(epoch: Epoch) -> String {
    hex_nonce(epoch.0)
}

fn hex_nonce(nonce: [u8; 16]) -> String {
    nonce.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_metadata(
    file: &mut impl Write,
    revision: &Revision,
    root: RootIdentity,
) -> Result<(), RevisionError> {
    let epoch = hex_epoch(revision.epoch);
    let predecessor = revision
        .predecessor
        .map(hex_epoch)
        .unwrap_or_else(|| "none".to_owned());
    let owner_boot_nonce = hex_nonce(revision.owner_boot_nonce.0);
    let process_start = hex_nonce(revision.process_start.0);
    write!(
        file,
        "{STATE_MAGIC}\nepoch={epoch}\npredecessor={predecessor}\nsequence={}\ndigest={}\ndevice={}\ninode={}\nowner_boot_nonce={owner_boot_nonce}\nprocess_start={process_start}\n",
        revision.sequence, revision.digest, root.device, root.inode,
    )
    .map_err(|source| io_error("write revision metadata", source))
}

fn validate_relative_path(path: &Path) -> Result<(), RevisionError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(RevisionError::UnsafePath(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(unix)]
fn open_absolute_directory(path: &Path) -> Result<File, RevisionError> {
    use std::os::fd::FromRawFd;

    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(RevisionError::UnsafePath(path.to_path_buf()));
    }
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
                directory = open_directory_at(&directory, name).map_err(|source| {
                    if matches!(
                        source.raw_os_error(),
                        Some(libc::ELOOP) | Some(libc::ENOTDIR)
                    ) {
                        RevisionError::Symlink(path.to_path_buf())
                    } else {
                        io_error("open workspace path component", source)
                    }
                })?;
            }
            _ => return Err(RevisionError::UnsafePath(path.to_path_buf())),
        }
    }
    Ok(directory)
}

#[cfg(not(unix))]
fn open_absolute_directory(_path: &Path) -> Result<File, RevisionError> {
    Err(RevisionError::Unavailable {
        reason: "descriptor-relative no-follow traversal is unavailable",
    })
}

const C_NAME_BYTES: usize = 4096;

struct CName {
    bytes: [u8; C_NAME_BYTES],
}

impl CName {
    fn new(name: &OsStr) -> io::Result<Self> {
        let name = os_slice(name);
        if name.len() >= C_NAME_BYTES || name.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path component is too long or contains NUL",
            ));
        }
        let mut bytes = [0_u8; C_NAME_BYTES];
        bytes[..name.len()].copy_from_slice(name);
        Ok(Self { bytes })
    }

    fn as_ptr(&self) -> *const libc::c_char {
        self.bytes.as_ptr().cast()
    }
}

#[cfg(unix)]
fn open_directory_at(directory: &File, name: &OsStr) -> io::Result<File> {
    open_file_at(directory, name, libc::O_RDONLY | libc::O_DIRECTORY)
}

#[cfg(unix)]
fn open_file_at(directory: &File, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let name = CName::new(name)?;
    // SAFETY: openat receives a live descriptor and valid C string; File owns success.
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

fn open_lock_file_at(directory: &File, name: &OsStr, mode: libc::mode_t) -> io::Result<File> {
    loop {
        match open_created_file_at(
            directory,
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            mode,
        ) {
            Ok(file) => return Ok(file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                match open_file_at(directory, name, libc::O_RDWR) {
                    Ok(file) => return Ok(file),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn create_new_file_at(directory: &File, name: &OsStr, mode: libc::mode_t) -> io::Result<File> {
    open_created_file_at(
        directory,
        name,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
        mode,
    )
}

fn open_created_file_at(
    directory: &File,
    name: &OsStr,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let name = CName::new(name)?;
    // SAFETY: openat receives a live descriptor, valid C string, flags, and mode.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: descriptor is newly opened and uniquely owned.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

fn rename_at(directory: &File, from: &OsStr, to: &OsStr) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let from = CName::new(from)?;
    let to = CName::new(to)?;
    // SAFETY: names are valid and both operations are relative to the live parent descriptor.
    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            from.as_ptr(),
            directory.as_raw_fd(),
            to.as_ptr(),
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn unlink_at(directory: &File, name: &OsStr) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    let name = CName::new(name)?;
    // SAFETY: name is valid and relative to the live parent descriptor.
    if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn directory_entries(
    directory: &File,
    options: &RevisionOptions,
    limits: &mut ScanLimits<'_>,
    memory: &mut MemoryBudget,
) -> Result<Vec<OsString>, RevisionError> {
    use std::{
        os::fd::{FromRawFd, IntoRawFd},
        os::unix::ffi::{OsStrExt, OsStringExt},
    };

    let iterator = open_directory_at(directory, OsStr::new("."))
        .map_err(|source| io_error("open workspace directory iterator", source))?;
    let descriptor = iterator.into_raw_fd();
    // SAFETY: descriptor ownership transfers to DIR and closedir below.
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        // SAFETY: fdopendir retained no ownership on failure.
        drop(unsafe { File::from_raw_fd(descriptor) });
        return Err(io_error(
            "enumerate workspace directory",
            io::Error::last_os_error(),
        ));
    }
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            // SAFETY: Stream uniquely owns the live DIR pointer.
            unsafe { libc::closedir(self.0) };
        }
    }
    let stream = Stream(stream);
    let mut entries = Vec::new();
    let mut entries_capacity = 0;
    loop {
        clear_errno();
        // SAFETY: stream remains live until closed below.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(0) {
                return Err(io_error("enumerate workspace directory", error));
            }
            break;
        }
        // SAFETY: d_name is NUL-terminated in a valid dirent.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            check_time(limits)?;
            limits.entries = limits
                .entries
                .checked_add(1)
                .ok_or(RevisionError::LimitExceeded(LimitKind::Entries))?;
            if limits.entries > options.max_entries {
                return Err(RevisionError::LimitExceeded(LimitKind::Entries));
            }
            limits.name_bytes = limits
                .name_bytes
                .checked_add(name.len())
                .ok_or(RevisionError::LimitExceeded(LimitKind::NameBytes))?;
            if limits.name_bytes > options.max_name_bytes {
                return Err(RevisionError::LimitExceeded(LimitKind::NameBytes));
            }
            reserve_vec_slot(&mut entries, &mut entries_capacity, memory)?;
            memory.reserve_allocation(name.len())?;
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(name.len())
                .map_err(|_| RevisionError::LimitExceeded(LimitKind::Memory))?;
            owned.extend_from_slice(name);
            entries.push(OsString::from_vec(owned));
        }
    }
    entries.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(entries)
}

fn clear_errno() {
    #[cfg(target_os = "macos")]
    // SAFETY: __error returns this thread's errno location.
    unsafe {
        *libc::__error() = 0;
    }
    #[cfg(target_os = "linux")]
    // SAFETY: __errno_location returns this thread's errno location.
    unsafe {
        *libc::__errno_location() = 0;
    }
}

#[cfg(unix)]
fn metadata_at(directory: &File, name: &OsStr) -> io::Result<UnixMetadata> {
    use std::{mem::MaybeUninit, os::fd::AsRawFd};

    let name = CName::new(name)?;
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: output storage and all arguments are valid.
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
    // SAFETY: file owns a live descriptor and output storage is writable.
    if unsafe { libc::fstat(file.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstat initialized metadata.
    Ok(unix_metadata_from_stat(unsafe { metadata.assume_init() }))
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn unix_metadata_from_stat(metadata: libc::stat) -> UnixMetadata {
    UnixMetadata {
        device: metadata.st_dev as u64,
        inode: metadata.st_ino,
        links: metadata.st_nlink as u64,
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
        device: metadata.st_dev as u64,
        inode: metadata.st_ino as u64,
        links: metadata.st_nlink as u64,
        mode: metadata.st_mode,
        size: metadata.st_size as u64,
        modified_seconds: metadata.st_mtime,
        modified_nanoseconds: metadata.st_mtime_nsec,
        changed_seconds: metadata.st_ctime,
        changed_nanoseconds: metadata.st_ctime_nsec,
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn mount_identity(file: &File) -> Result<MountIdentity, RevisionError> {
    use std::{mem::MaybeUninit, os::fd::AsRawFd};

    let mut metadata = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs receives a live descriptor and writable output storage.
    if unsafe { libc::fstatfs(file.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(RevisionError::Unavailable {
            reason: "descriptor-relative mount identity",
        });
    }
    // SAFETY: successful fstatfs initialized metadata.
    let metadata = unsafe { metadata.assume_init() };
    let length = metadata
        .f_mntonname
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(metadata.f_mntonname.len());
    let bytes =
        unsafe { std::slice::from_raw_parts(metadata.f_mntonname.as_ptr().cast::<u8>(), length) };
    Ok(MountIdentity(*blake3::hash(bytes).as_bytes()))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn mount_identity(file: &File) -> Result<MountIdentity, RevisionError> {
    use std::{mem::MaybeUninit, os::fd::AsRawFd};

    const STATX_MNT_ID: u32 = 0x1000;
    let mut statx = MaybeUninit::<libc::statx>::zeroed();
    // SAFETY: statx receives a live descriptor, valid empty C path, and output storage.
    let result = unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            STATX_MNT_ID,
            statx.as_mut_ptr(),
        )
    };
    if result != 0 {
        return Err(RevisionError::Unavailable {
            reason: "descriptor-relative mount identity",
        });
    }
    // SAFETY: successful statx initialized metadata.
    let statx = unsafe { statx.assume_init() };
    if statx.stx_mask & STATX_MNT_ID == 0 {
        return Err(RevisionError::Unavailable {
            reason: "descriptor-relative mount identity",
        });
    }
    Ok(MountIdentity(
        *blake3::hash(&statx.stx_mnt_id.to_le_bytes()).as_bytes(),
    ))
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
fn mount_identity(_file: &File) -> Result<MountIdentity, RevisionError> {
    Err(RevisionError::Unavailable {
        reason: "descriptor-relative mount identity",
    })
}

#[cfg(not(unix))]
fn unix_metadata(_file: &File) -> io::Result<UnixMetadata> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor metadata unavailable",
    ))
}

fn map_entry_error(path: PathBuf, source: io::Error) -> RevisionError {
    if matches!(source.raw_os_error(), Some(libc::ELOOP)) {
        RevisionError::Symlink(path)
    } else if matches!(
        source.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) {
        RevisionError::ScanRace { attempts: 1 }
    } else {
        io_error("inspect workspace entry", source)
    }
}

fn map_metadata_path_error(path: PathBuf, source: io::Error) -> RevisionError {
    if matches!(
        source.raw_os_error(),
        Some(libc::ELOOP) | Some(libc::ENOTDIR)
    ) {
        RevisionError::UnsafePath(path)
    } else {
        io_error("open revision metadata", source)
    }
}

fn os_len(value: &OsStr) -> usize {
    os_slice(value).len()
}

fn os_slice(value: &OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes()
}

fn io_error(operation: &'static str, source: io::Error) -> RevisionError {
    RevisionError::Io { operation, source }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_growth_uses_logical_capacity_even_with_allocator_spare_capacity() {
        let item_size = std::mem::size_of::<u64>();
        let first_capacity = VECTOR_GROWTH_BYTES / item_size;
        let first_charge = allocation_charge(first_capacity * item_size).unwrap();
        let second_charge = allocation_charge(first_capacity * 2 * item_size).unwrap();
        let mut values = Vec::with_capacity(first_capacity * 2);
        let mut logical_capacity = 0;
        let mut memory = MemoryBudget::new(first_charge + second_charge - 1);
        for value in 0..first_capacity as u64 {
            reserve_vec_slot(&mut values, &mut logical_capacity, &mut memory).unwrap();
            values.push(value);
        }
        assert!(matches!(
            reserve_vec_slot(&mut values, &mut logical_capacity, &mut memory),
            Err(RevisionError::LimitExceeded(LimitKind::Memory))
        ));
    }

    #[test]
    fn equal_multi_file_scans_are_rejected_when_kernel_events_show_a_torn_cycle() {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).unwrap();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("kit-revision-fence-{}", hex_nonce(nonce)));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a"), b"new").unwrap();
        fs::write(root.join("b"), b"old").unwrap();
        let root_file = open_absolute_directory(&root).unwrap();
        let options = RevisionOptions {
            max_entries: 16,
            max_name_bytes: 1024,
            max_bytes: 1024,
            max_memory_bytes: 1024 * 1024,
            max_depth: 8,
            max_scan_time: Duration::from_secs(2),
            max_scan_attempts: 2,
            watcher_interval: Duration::from_secs(1),
            reconciliation_interval: Duration::from_secs(1),
            metadata_path: None,
        };
        let scan_deadline = deadline(options.max_scan_time);
        let mut memory = MemoryBudget::new(options.max_memory_bytes);
        let mut fence = MutationFence::new(&mut memory).unwrap();
        let mut scanned_bytes = 0;
        let first = scan_once(
            &root_file,
            &root,
            &options,
            scan_deadline,
            None,
            Capture::None,
            &mut fence,
            &mut memory,
            &mut scanned_bytes,
        )
        .unwrap();
        fence.ensure_clean().unwrap();

        fs::write(root.join("a"), b"old").unwrap();
        fs::write(root.join("b"), b"new").unwrap();
        fs::write(root.join("a"), b"new").unwrap();
        fs::write(root.join("b"), b"old").unwrap();
        let second = scan_once(
            &root_file,
            &root,
            &options,
            scan_deadline,
            None,
            Capture::Snapshot {
                max_file_bytes: usize::MAX,
                max_special_file_bytes: usize::MAX,
                max_content_bytes: u64::MAX,
            },
            &mut fence,
            &mut memory,
            &mut scanned_bytes,
        )
        .unwrap();
        assert_eq!(first.digest, second.digest);
        assert!(matches!(
            fence.ensure_clean(),
            Err(RevisionError::ScanRace { .. })
        ));

        drop(fence);
        drop(root_file);
        fs::remove_dir_all(root).unwrap();
    }
}
