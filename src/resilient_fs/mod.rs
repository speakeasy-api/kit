//! Process-owned, bounded write-back filesystem.
//!
//! Successful writes and syncs mean *accepted*, not durable: ENOSPC/EDQUOT
//! obligations remain in memory until recovery succeeds. `require_disk` is the
//! explicit durability barrier. No memory state survives process termination.
//! Native locks and secure descriptor traversal never pretend disk success.
mod backend;
pub use backend::*;
pub use std::fs::Permissions;
use std::{
    collections::VecDeque,
    ffi::OsString,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::SystemTime,
};
use zeroize::Zeroizing;

fn error(kind: io::ErrorKind, msg: &str) -> io::Error {
    io::Error::new(kind, msg)
}
fn capacity(e: &io::Error) -> bool {
    if matches!(
        e.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        matches!(e.raw_os_error(), Some(libc::ENOSPC) | Some(libc::EDQUOT))
    }
    #[cfg(not(unix))]
    {
        matches!(e.raw_os_error(), Some(112) | Some(39) | Some(1816))
    }
}
// Actual allocator failures signal process-wide pressure, unlike an individual
// service's configurable budget. The application decides cancellation policy.
static ALLOCATION_EXHAUSTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static ALLOCATION_FAILURE_HANDLER: OnceLock<fn() -> !> = OnceLock::new();

/// Install a process-level emergency exit before callers can allocate an error
/// wrapper. Configured-budget exhaustion still uses ordinary cancellation.
pub fn set_allocation_failure_handler(handler: fn() -> !) -> Result<(), fn() -> !> {
    ALLOCATION_FAILURE_HANDLER.set(handler)
}

fn allocation_oom() -> io::Error {
    ALLOCATION_EXHAUSTED.store(true, std::sync::atomic::Ordering::Release);
    if let Some(handler) = ALLOCATION_FAILURE_HANDLER.get() {
        handler();
    }
    oom()
}
fn oom() -> io::Error {
    io::ErrorKind::OutOfMemory.into()
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
fn bytes(data: &[u8]) -> io::Result<Zeroizing<Vec<u8>>> {
    let mut v = Vec::new();
    v.try_reserve_exact(data.len())
        .map_err(|_| allocation_oom())?;
    v.extend_from_slice(data);
    Ok(Zeroizing::new(v))
}
#[cfg(unix)]
fn permissions(private: bool, dir: bool) -> Permissions {
    use std::os::unix::fs::PermissionsExt;
    Permissions::from_mode(if private {
        if dir { 0o700 } else { 0o600 }
    } else if dir {
        0o755
    } else {
        0o644
    })
}
#[derive(Clone, Copy, Debug)]
pub struct FileType {
    file: bool,
    dir: bool,
    symlink: bool,
}
impl FileType {
    pub fn is_file(&self) -> bool {
        self.file
    }
    pub fn is_dir(&self) -> bool {
        self.dir
    }
    pub fn is_symlink(&self) -> bool {
        self.symlink
    }
}
fn same_disk_identity(a: Option<FileIdentity>, b: Option<FileIdentity>) -> bool {
    a.is_some() && a == b
}
fn next_identity() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}
#[derive(Clone, Debug)]
pub struct Metadata {
    identity: u64,
    disk_identity: Option<FileIdentity>,
    disk: Option<std::fs::Metadata>,
    kind: FileType,
    len: u64,
    permissions: Permissions,
    modified: SystemTime,
}
impl Metadata {
    fn disk(m: std::fs::Metadata, disk_identity: Option<FileIdentity>) -> Self {
        Self {
            identity: 0,
            disk_identity,
            kind: FileType {
                file: m.is_file(),
                dir: m.is_dir(),
                symlink: m.file_type().is_symlink(),
            },
            len: m.len(),
            permissions: m.permissions(),
            modified: m.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            disk: Some(m),
        }
    }
    pub fn is_file(&self) -> bool {
        self.kind.file
    }
    pub fn is_dir(&self) -> bool {
        self.kind.dir
    }
    pub fn file_type(&self) -> FileType {
        self.kind
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn permissions(&self) -> Permissions {
        self.permissions.clone()
    }
    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(self.modified)
    }
    pub fn same_identity(&self, other: &Metadata) -> bool {
        if self.identity != 0 && other.identity != 0 {
            return self.identity == other.identity;
        }
        same_disk_identity(self.disk_identity, other.disk_identity)
    }
    pub fn disk_metadata(&self) -> Option<&std::fs::Metadata> {
        self.disk.as_ref()
    }
}
type Payload = Arc<Zeroizing<Vec<u8>>>;
type Native = Arc<Mutex<Box<dyn BackendFile>>>;
#[derive(Clone)]
enum Source {
    Memory(Payload),
    Native(Native),
}
#[derive(Clone)]
struct Patch {
    offset: u64,
    data: Payload,
    len: u64,
}
#[derive(Clone)]
struct Image {
    source: Source,
    base_len: u64,
    len: u64,
    patches: Arc<Vec<Patch>>,
}
impl Image {
    fn memory(data: Payload) -> Self {
        let len = data.len() as u64;
        Self {
            source: Source::Memory(data),
            base_len: len,
            len,
            patches: Arc::new(Vec::new()),
        }
    }
    fn native(file: Box<dyn BackendFile>) -> io::Result<Self> {
        let len = file.metadata()?.len();
        Ok(Self {
            source: Source::Native(Arc::new(Mutex::new(file))),
            base_len: len,
            len,
            patches: Arc::new(Vec::new()),
        })
    }
    fn payload_bytes(&self) -> usize {
        let base = match &self.source {
            Source::Memory(d) => d.len(),
            Source::Native(_) => 0,
        };
        self.patches
            .iter()
            .fold(base, |n, p| n.saturating_add(p.data.len()))
    }
    fn patched(&self, offset: u64, data: &[u8], len: u64) -> io::Result<Self> {
        let mut patches = Vec::new();
        patches
            .try_reserve_exact(self.patches.len() + 1)
            .map_err(|_| allocation_oom())?;
        patches.extend(self.patches.iter().cloned());
        patches.push(Patch {
            offset,
            data: Arc::new(bytes(data)?),
            len,
        });
        Ok(Self {
            source: self.source.clone(),
            base_len: self.base_len,
            len,
            patches: Arc::new(patches),
        })
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let n = usize::try_from(self.len.saturating_sub(offset).min(buf.len() as u64)).unwrap();
        let buf = &mut buf[..n];
        buf.fill(0);
        let base_n = usize::try_from(self.base_len.saturating_sub(offset).min(n as u64)).unwrap();
        match &self.source {
            Source::Memory(data) => {
                if base_n > 0 {
                    buf[..base_n].copy_from_slice(&data[offset as usize..offset as usize + base_n]);
                }
            }
            Source::Native(file) => {
                if base_n > 0 {
                    let mut file = lock(file);
                    file.seek(SeekFrom::Start(offset))?;
                    file.read_exact(&mut buf[..base_n])?;
                }
            }
        }
        for patch in self.patches.iter() {
            if patch.len < offset + n as u64 {
                let start = patch.len.saturating_sub(offset).min(n as u64) as usize;
                buf[start..].fill(0);
            }
            let lo = offset.max(patch.offset);
            let hi = (offset + n as u64)
                .min(patch.offset + patch.data.len() as u64)
                .min(patch.len);
            if hi > lo {
                buf[(lo - offset) as usize..(hi - offset) as usize].copy_from_slice(
                    &patch.data[(lo - patch.offset) as usize..(hi - patch.offset) as usize],
                );
            }
        }
        Ok(n)
    }
    fn write_to(&self, file: &mut dyn BackendFile) -> io::Result<()> {
        let mut buf = Zeroizing::new([0u8; 64 * 1024]);
        let mut offset = 0;
        while offset < self.len {
            let n = self.read_at(offset, &mut *buf)?;
            file.write_all(&buf[..n])?;
            offset += n as u64;
        }
        Ok(())
    }
}
struct Object {
    image: Image,
    meta: Metadata,
    path: Option<PathBuf>,
    dirty: bool,
}
impl Object {
    fn memory(data: Payload, meta: Metadata) -> Self {
        Self {
            image: Image::memory(data),
            meta,
            path: None,
            dirty: true,
        }
    }
    fn native(file: Box<dyn BackendFile>, path: PathBuf) -> io::Result<Self> {
        let meta = Metadata::disk(file.metadata()?, file.identity()?);
        Ok(Self {
            image: Image::native(file)?,
            meta,
            path: Some(path),
            dirty: false,
        })
    }
}
type Obj = Arc<Mutex<Object>>;
struct Entry {
    path: PathBuf,
    object: Option<Obj>,
}
#[derive(Clone)]
pub struct Fs {
    service: Arc<Service>,
    lease: Option<Arc<LeaseCaller>>,
}
struct Service {
    backend: Arc<dyn Backend>,
    state: Mutex<State>,
    max_bytes: usize,
    max_operations: usize,
}
struct State {
    entries: Vec<Entry>,
    redirects: Vec<(PathBuf, PathBuf)>,
    objects: Vec<std::sync::Weak<Mutex<Object>>>,
    pending: VecDeque<Pending>,
    next: u64,
    exhausted: bool,
    leases: Vec<(
        PathBuf,
        std::sync::Weak<LeaseInner>,
        std::sync::Weak<LeaseCaller>,
    )>,
}
#[derive(Debug)]
pub struct Status {
    pub pending_operations: usize,
    pub retained_bytes: usize,
    pub exhausted: bool,
}
#[derive(Debug)]
pub struct RecoveryReport {
    pub completed_operations: usize,
    pub remaining_operations: usize,
    pub blocked: Option<io::Error>,
}
pub struct Lease {
    inner: Arc<LeaseCaller>,
}
struct LeaseCaller {
    authority: Arc<LeaseInner>,
}
impl std::ops::Deref for LeaseCaller {
    type Target = LeaseInner;
    fn deref(&self) -> &LeaseInner {
        &self.authority
    }
}
struct LeaseInner {
    fenced: Mutex<Option<io::ErrorKind>>,
    native: Box<dyn BackendLease>,
    scope: PathBuf,
    service: std::sync::Weak<Service>,
}
impl LeaseInner {
    fn check(&self) -> io::Result<()> {
        let mut fenced = lock(&self.fenced);
        if let Some(kind) = *fenced {
            if kind == io::ErrorKind::NotFound {
                // Keep Missing while the name is absent, but report a replaced
                // owner as PermissionDenied. A restored old inode stays fenced.
                match self.native.check() {
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(e),
                    _ => {
                        *fenced = Some(io::ErrorKind::PermissionDenied);
                        return Err(io::ErrorKind::PermissionDenied.into());
                    }
                }
            }
            return Err(kind.into());
        }
        let result = self.native.check();
        if let Err(e) = &result {
            *fenced = Some(e.kind());
        }
        result
    }
}
impl Lease {
    pub fn check(&self) -> io::Result<()> {
        self.inner.check()
    }
}
enum Action {
    Put {
        path: PathBuf,
        temp: PathBuf,
        temp_file: Option<Native>,
        parent_identity: Option<FileIdentity>,
        image: Image,
        permissions: Permissions,
        stage: u8,
    },
    Mkdir {
        path: PathBuf,
        private: bool,
        stage: u8,
    },
    Unlink {
        path: PathBuf,
        dir: bool,
        stage: u8,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        stage: u8,
    },
    Chmod {
        path: PathBuf,
        permissions: Permissions,
    },
    Sync {
        path: PathBuf,
    },
}
struct Pending {
    action: Action,
    lease: Option<Arc<LeaseInner>>,
}
impl Action {
    fn bytes(&self) -> usize {
        match self {
            Self::Put { image, .. } => image.payload_bytes(),
            _ => 0,
        }
    }
    fn touches(&self, p: &Path) -> bool {
        match self {
            Self::Rename { from, to, .. } => {
                from.starts_with(p) || p.starts_with(from) || to.starts_with(p) || p.starts_with(to)
            }
            Self::Put { path, .. }
            | Self::Mkdir { path, .. }
            | Self::Unlink { path, .. }
            | Self::Chmod { path, .. }
            | Self::Sync { path } => path.starts_with(p) || p.starts_with(path),
        }
    }
}
static GLOBAL: OnceLock<Fs> = OnceLock::new();
pub fn initialize_global(fs: Fs) -> Result<(), Fs> {
    GLOBAL.set(fs)
}
pub fn global() -> &'static Fs {
    GLOBAL.get_or_init(|| Fs::new(Arc::new(DiskBackend)))
}
impl Fs {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self::with_budget(backend, 64 * 1024 * 1024, 4096)
    }
    pub fn with_budget(backend: Arc<dyn Backend>, max_bytes: usize, max_operations: usize) -> Self {
        Self {
            service: Arc::new(Service {
                backend,
                state: Mutex::new(State {
                    entries: Vec::new(),
                    redirects: Vec::new(),
                    objects: Vec::new(),
                    pending: VecDeque::new(),
                    next: 0,
                    exhausted: false,
                    leases: Vec::new(),
                }),
                max_bytes,
                max_operations,
            }),
            lease: None,
        }
    }
    pub fn guarded(&self, lease: &Lease) -> io::Result<Self> {
        lease.check()?;
        if !lease.inner.service.ptr_eq(&Arc::downgrade(&self.service)) {
            return Err(error(
                io::ErrorKind::PermissionDenied,
                "lease belongs to another filesystem",
            ));
        }
        Ok(Self {
            service: self.service.clone(),
            lease: Some(lease.inner.clone()),
        })
    }
    pub fn acquire_lease<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        path: P,
        scope: Q,
        mode: LeaseMode,
    ) -> io::Result<Lease> {
        let cleanup = matches!(mode, LeaseMode::CreateNew);
        self.acquire_lease_with_cleanup(path, scope, mode, cleanup)
    }
    pub fn acquire_lease_with_cleanup<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        path: P,
        scope: Q,
        mode: LeaseMode,
        remove_on_drop: bool,
    ) -> io::Result<Lease> {
        let path = self.norm(path.as_ref())?;
        let scope = self.norm(scope.as_ref())?;
        let mut state = lock(&self.service.state);
        state
            .leases
            .retain(|(_, authority, _)| authority.strong_count() > 0);
        if let Some(index) = state.leases.iter().position(|(p, _, _)| *p == path)
            && let Some(authority) = state.leases[index].1.upgrade()
        {
            let valid = authority.check();
            let dirty = state
                .pending
                .iter()
                .any(|p| p.lease.as_ref().is_some_and(|l| Arc::ptr_eq(l, &authority)));
            if valid.is_err() && !dirty {
                // A lost *clean* lease does not reserve a namespace forever.
                // The old observer remains fenced; reacquisition is real native IO.
                state.leases.remove(index);
            } else {
                valid?;
                if state.leases[index].2.upgrade().is_some() {
                    return Err(error(
                        io::ErrorKind::WouldBlock,
                        "lease has a live observer",
                    ));
                }
                if authority.scope != scope {
                    return Err(error(
                        io::ErrorKind::PermissionDenied,
                        "retained lease scope mismatch",
                    ));
                }
                let owner = Arc::new(LeaseCaller { authority });
                state.leases[index].2 = Arc::downgrade(&owner);
                return Ok(Lease { inner: owner });
            }
        }
        // Retained authority precedes recovery: parent sync touches the lock.
        let report = self.recover_locked(&mut state);
        self.rebase(&mut state);
        Self::prune(&mut state);
        if state.pending.iter().any(|p| p.action.touches(&path)) {
            return Err(report.blocked.unwrap_or_else(|| {
                error(
                    io::ErrorKind::WouldBlock,
                    "bounded recovery has pending work",
                )
            }));
        }
        state.leases.try_reserve(1).map_err(|_| allocation_oom())?;
        let native = self.service.backend.acquire_lease(&LeaseRequest {
            path: path.clone(),
            scope: scope.clone(),
            mode,
            remove_on_drop,
        })?;
        let authority = Arc::new(LeaseInner {
            fenced: Mutex::new(None),
            native,
            scope,
            service: Arc::downgrade(&self.service),
        });
        let caller = Arc::new(LeaseCaller {
            authority: authority.clone(),
        });
        state
            .leases
            .push((path, Arc::downgrade(&authority), Arc::downgrade(&caller)));
        Ok(Lease { inner: caller })
    }
    fn norm(&self, path: &Path) -> io::Result<PathBuf> {
        // Canonicalize a real ancestor, never collapse `..` through a symlink.
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        if absolute
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return self.service.backend.canonicalize(&absolute);
        }
        let mut ancestor = absolute.parent().unwrap_or(&absolute);
        let mut suffix = Vec::new();
        if let Some(name) = absolute.file_name() {
            suffix.push(name.to_os_string());
        }
        loop {
            match self.service.backend.canonicalize(ancestor) {
                Ok(mut base) => {
                    for part in suffix.iter().rev() {
                        base.push(part);
                    }
                    return Ok(base);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if let Some(name) = ancestor.file_name() {
                        suffix.push(name.to_os_string());
                    }
                    ancestor = ancestor.parent().ok_or(e)?;
                }
                Err(e) => return Err(e),
            }
        }
    }
    fn authority(&self, path: &Path) -> io::Result<()> {
        if let Some(l) = &self.lease {
            l.check()?;
            if !path.starts_with(&l.scope) {
                return Err(error(
                    io::ErrorKind::PermissionDenied,
                    "mutation outside lease scope",
                ));
            }
        }
        Ok(())
    }
    fn secure_path(&self, s: &State, path: &Path, final_link: bool) -> io::Result<()> {
        let mut cur = PathBuf::new();
        for c in path.components() {
            cur.push(c.as_os_str());
            if final_link && cur == path {
                break;
            }
            if let Some(e) = s.entries.iter().find(|e| e.path == cur) {
                if let Some(o) = &e.object {
                    if lock(o).meta.kind.symlink {
                        return Err(error(
                            io::ErrorKind::PermissionDenied,
                            "symlink in managed path",
                        ));
                    }
                    continue;
                } else {
                    continue;
                }
            }
            match self
                .service
                .backend
                .metadata(&Self::disk_path(s, &cur), false)
            {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(error(
                        io::ErrorKind::PermissionDenied,
                        "symlink in managed path",
                    ));
                }
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }
    fn retained(s: &State) -> usize {
        let objects = s
            .objects
            .iter()
            .filter_map(|o| o.upgrade())
            .map(|o| lock(&o).image.payload_bytes())
            .sum::<usize>();
        objects.saturating_add(s.pending.iter().map(|p| p.action.bytes()).sum::<usize>())
    }
    pub fn status(&self) -> Status {
        let s = lock(&self.service.state);
        Status {
            pending_operations: s.pending.len(),
            retained_bytes: Self::retained(&s),
            exhausted: s.exhausted
                || ALLOCATION_EXHAUSTED.load(std::sync::atomic::Ordering::Acquire),
        }
    }
    fn reserve(&self, s: &mut State, additional: usize, entries: usize) -> io::Result<()> {
        if s.pending.len() >= self.service.max_operations
            || Self::retained(s).saturating_add(additional) > self.service.max_bytes
            || s.entries.len().saturating_add(entries)
                > self.service.max_operations.saturating_mul(4)
        {
            s.exhausted = true;
            return Err(oom());
        }
        s.objects.retain(|w| w.strong_count() > 0);
        if s.pending.try_reserve(1).is_err()
            || s.entries.try_reserve(entries).is_err()
            || s.objects.try_reserve(entries).is_err()
        {
            s.exhausted = true;
            return Err(allocation_oom());
        }
        Ok(())
    }
    fn disk_path(s: &State, path: &Path) -> PathBuf {
        let mut mapped = if let Some((to, from)) = s
            .redirects
            .iter()
            .filter(|(to, _)| path.starts_with(to))
            .max_by_key(|(to, _)| to.components().count())
        {
            if path == to {
                from.clone()
            } else {
                from.join(path.strip_prefix(to).unwrap())
            }
        } else {
            path.to_path_buf()
        };
        for p in &s.pending {
            if let Action::Rename { from, to, stage: 1 } = &p.action
                && mapped.starts_with(from)
            {
                mapped = if mapped == *from {
                    to.clone()
                } else {
                    to.join(mapped.strip_prefix(from).unwrap())
                };
            }
        }
        mapped
    }
    fn prepare(s: &mut State, entries: usize) -> io::Result<()> {
        s.entries
            .try_reserve(entries)
            .map_err(|_| allocation_oom())?;
        s.objects
            .try_reserve(entries)
            .map_err(|_| allocation_oom())?;
        s.redirects
            .try_reserve(entries)
            .map_err(|_| allocation_oom())?;
        s.pending.try_reserve(1).map_err(|_| allocation_oom())?;
        Ok(())
    }
    fn lookup(&self, s: &State, path: &Path) -> io::Result<Metadata> {
        if let Some(e) = s
            .entries
            .iter()
            .rev()
            .find(|e| e.path == path && s.pending.iter().any(|p| p.action.touches(path)))
        {
            return e
                .object
                .as_ref()
                .map(|o| lock(o).meta.clone())
                .ok_or_else(|| error(io::ErrorKind::NotFound, "removed path"));
        }
        if s.entries.iter().any(|e| {
            e.object.is_none()
                && path.starts_with(&e.path)
                && s.pending.iter().any(|p| p.action.touches(&e.path))
        }) {
            return Err(error(io::ErrorKind::NotFound, "removed ancestor"));
        }
        let path = Self::disk_path(s, path);
        Ok(Metadata::disk(
            self.service.backend.metadata(&path, false)?,
            self.service.backend.identity(&path, false)?,
        ))
    }
    fn object(&self, s: &mut State, path: &Path) -> io::Result<Obj> {
        if let Some(e) = s.entries.iter().find(|e| e.path == path) {
            return e
                .object
                .clone()
                .ok_or_else(|| error(io::ErrorKind::NotFound, "removed path"));
        }
        let meta = self.lookup(s, path)?;
        if !meta.is_file() {
            return Err(error(io::ErrorKind::InvalidInput, "not a regular file"));
        }
        let native = self.service.backend.open(
            &Self::disk_path(s, path),
            &DiskOpenOptions {
                read: true,
                ..Default::default()
            },
        )?;
        let opened = Object::native(native, path.to_path_buf())?;
        // Share logical identity only while the named inode still matches.
        // Explicit and external replacements must remain distinct.
        for object in s.objects.iter().filter_map(|w| w.upgrade()) {
            let held = lock(&object);
            if held.path.as_deref() == Some(path)
                && same_disk_identity(held.meta.disk_identity, opened.meta.disk_identity)
            {
                drop(held);
                return Ok(object);
            }
        }
        let object = Arc::new(Mutex::new(opened));
        s.objects.try_reserve(1).map_err(|_| allocation_oom())?;
        s.objects.push(Arc::downgrade(&object));
        Ok(object)
    }
    fn live_object(&self, s: &State, path: &Path) -> io::Result<Option<Obj>> {
        if let Some(entry) = s.entries.iter().find(|e| e.path == path) {
            return Ok(entry.object.clone());
        }
        let identity = match self.service.backend.identity(path, false) {
            Ok(identity) => identity,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(s.objects.iter().filter_map(|w| w.upgrade()).find(|o| {
            let o = lock(o);
            o.path.as_deref() == Some(path) && same_disk_identity(o.meta.disk_identity, identity)
        }))
    }
    fn entry(s: &mut State, path: PathBuf, object: Option<Obj>) {
        if let Some(o) = &object {
            lock(o).path = Some(path.clone());
        }
        if let Some(o) = &object
            && !s.objects.iter().any(|w| w.ptr_eq(&Arc::downgrade(o)))
        {
            s.objects.push(Arc::downgrade(o));
        }
        if let Some(e) = s.entries.iter_mut().find(|e| e.path == path) {
            e.object = object;
        } else {
            s.entries.push(Entry { path, object });
        }
    }
    fn new_permissions(
        &self,
        s: &State,
        path: &Path,
        private: bool,
        dir: bool,
    ) -> io::Result<Permissions> {
        #[cfg(unix)]
        {
            if private || dir {
                return Ok(permissions(private, dir));
            }
            // Let the kernel apply umask without changing process-global state.
            // Probe an empty exclusive file; never broaden its mode.
            let mut parent = path
                .parent()
                .ok_or_else(|| error(io::ErrorKind::InvalidInput, "no parent"))?;
            while s.entries.iter().any(|e| {
                e.path == parent && e.object.as_ref().is_some_and(|o| lock(o).meta.is_dir())
            }) && matches!(self.service.backend.metadata(parent, false),
                Err(e) if e.kind() == io::ErrorKind::NotFound)
            {
                parent = parent
                    .parent()
                    .ok_or_else(|| error(io::ErrorKind::InvalidInput, "no disk ancestor"))?;
            }
            for _ in 0..8 {
                let mut random = [0u8; 16];
                getrandom::fill(&mut random).map_err(io::Error::other)?;
                let probe = parent.join(format!(
                    ".kit-mode-{:032x}.tmp",
                    u128::from_ne_bytes(random)
                ));
                match self.service.backend.open(
                    &probe,
                    &DiskOpenOptions {
                        write: true,
                        create_new: true,
                        ..Default::default()
                    },
                ) {
                    Ok(file) => {
                        let result = file.metadata().map(|m| m.permissions());
                        // Never unlink another actor's replacement, including a
                        // symlink. Keep the descriptor alive through cleanup.
                        if !same_disk_identity(
                            file.identity()?,
                            self.service.backend.identity(&probe, false)?,
                        ) {
                            return Err(error(
                                io::ErrorKind::PermissionDenied,
                                "permission probe identity changed",
                            ));
                        }
                        match self.service.backend.remove_file(&probe) {
                            Ok(()) => {}
                            // An empty probe may remain, but no user bytes are
                            // exposed and capacity failure must permit fallback.
                            Err(e) if capacity(&e) => {}
                            Err(e) => return Err(e),
                        }
                        return result;
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    // Only capacity failure allows fallback; use a restrictive mode.
                    Err(e) if capacity(&e) => return Ok(permissions(true, false)),
                    Err(e) => return Err(e),
                }
            }
            Err(error(
                io::ErrorKind::AlreadyExists,
                "permission probe collisions",
            ))
        }
        #[cfg(not(unix))]
        {
            let _ = (private, dir);
            let parent = path
                .parent()
                .ok_or_else(|| error(io::ErrorKind::InvalidInput, "no parent"))?;
            let mut p = self.lookup(s, parent)?.permissions();
            // This branch is non-Unix: clear the Windows readonly attribute,
            // never broaden Unix mode bits. ACL policy remains in DiskBackend.
            #[allow(clippy::permissions_set_readonly_false)]
            p.set_readonly(false);
            Ok(p)
        }
    }
    fn parent(&self, s: &State, path: &Path) -> io::Result<()> {
        let p = path
            .parent()
            .ok_or_else(|| error(io::ErrorKind::InvalidInput, "no parent"))?;
        let parent = self.lookup(s, p)?;
        if parent.permissions().readonly() {
            return Err(error(
                io::ErrorKind::PermissionDenied,
                "parent directory is read-only",
            ));
        }
        if !parent.is_dir() {
            return Err(error(
                io::ErrorKind::NotADirectory,
                "parent is not a directory",
            ));
        }
        Ok(())
    }
    fn preflight(&self, s: &State, path: &Path) -> io::Result<()> {
        self.authority(path)?;
        self.secure_path(s, path, false)?;
        self.parent(s, path)?;
        let existing = match self.lookup(s, path) {
            Ok(m) => Some(m),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        if let Some(m) = existing {
            if !m.is_file() {
                return Err(error(io::ErrorKind::InvalidInput, "not a regular file"));
            }
            #[cfg(unix)]
            if let Some(d) = m.disk_metadata() {
                use std::os::unix::fs::MetadataExt;
                if d.nlink() > 1 {
                    return Err(error(
                        io::ErrorKind::Unsupported,
                        "hard-linked mutation unsupported",
                    ));
                }
            }
            if m.permissions().readonly() {
                return Err(error(io::ErrorKind::PermissionDenied, "read-only file"));
            }
            match self.service.backend.open(
                path,
                &DiskOpenOptions {
                    write: true,
                    ..Default::default()
                },
            ) {
                Ok(_) => {}
                Err(e) if capacity(&e) => {}
                Err(e)
                    if e.kind() == io::ErrorKind::NotFound
                        && s.entries
                            .iter()
                            .any(|e| e.path == path && e.object.is_some()) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn put_action(s: &mut State, path: &Path, image: Image, permissions: Permissions) -> Action {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        s.next = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        static START: OnceLock<u128> = OnceLock::new();
        let start = START.get_or_init(|| {
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        });
        let temp = path.with_file_name(format!(
            ".kit-resilient-{}-{}-{}.tmp",
            std::process::id(),
            start,
            s.next
        ));
        Action::Put {
            path: path.to_path_buf(),
            temp,
            temp_file: None,
            parent_identity: None,
            image,
            permissions,
            stage: 0,
        }
    }
    // Healthy IO never consumes the configured fallback budget. Only a
    // complete, unpublished obligation enters the bounded write-back queue.
    fn submit(&self, s: &mut State, mut action: Action) -> (bool, io::Result<()>) {
        if s.pending.try_reserve(1).is_err() {
            return (false, Err(allocation_oom()));
        }
        if !s.pending.is_empty() {
            if let Err(e) = self.reserve(s, action.bytes().saturating_mul(2), 1) {
                return (false, Err(e));
            }
            return self.enqueue(s, action);
        }
        let result = self
            .lease
            .as_ref()
            .map_or(Ok(()), |l| l.check())
            .and_then(|_| self.replay(&mut action));
        match result {
            Ok(()) => (true, Ok(())),
            Err(e) => {
                let published = matches!(
                    action,
                    Action::Put { stage: 3, .. }
                        | Action::Rename { stage: 1, .. }
                        | Action::Mkdir { stage: 1, .. }
                        | Action::Unlink { stage: 1, .. }
                );
                if published && let Action::Put { image, .. } = &mut action {
                    *image = Image::memory(Arc::new(Zeroizing::new(Vec::new())));
                }
                if !capacity(&e) && !published {
                    self.abandon(&action);
                    return (false, Err(e));
                }
                if !published && let Err(e) = self.reserve(s, action.bytes().saturating_mul(2), 1) {
                    self.abandon(&action);
                    return (false, Err(e));
                }
                s.pending.push_back(Pending {
                    action,
                    lease: self.lease.as_ref().map(|c| c.authority.clone()),
                });
                (true, Ok(()))
            }
        }
    }
    fn abandon(&self, action: &Action) {
        if let Action::Put {
            temp,
            temp_file: Some(file),
            stage: 1 | 2,
            ..
        } = action
            && let Ok(held) = lock(file).identity()
            && let Ok(named) = self.service.backend.identity(temp, false)
            && same_disk_identity(held, named)
        {
            let _ = self.service.backend.remove_file(temp);
        }
    }
    fn rebase(&self, s: &mut State) {
        for object in s.objects.iter().filter_map(|w| w.upgrade()) {
            let mut object = lock(&object);
            let Some(path) = object.path.as_ref() else {
                continue;
            };
            if !object.dirty || !object.meta.is_file() {
                continue;
            }
            if s.pending.iter().any(|p| {
                p.action.touches(path)
                    && !matches!(p.action, Action::Put { stage: 3, .. } | Action::Sync { .. })
            }) {
                continue;
            }
            let published = s.pending.iter().rev().find_map(|p| match &p.action {
                Action::Put {
                    path: p,
                    temp_file: Some(file),
                    stage: 3,
                    ..
                } if p == path => Some(file.clone()),
                _ => None,
            });
            if let Some(file) = published {
                let snapshot = {
                    let held = lock(&file);
                    held.metadata()
                        .and_then(|meta| Ok((meta, held.identity()?)))
                };
                if let Ok((meta, disk_identity)) = snapshot {
                    let len = meta.len();
                    object.image = Image {
                        source: Source::Native(file),
                        base_len: len,
                        len,
                        patches: Arc::new(Vec::new()),
                    };
                    let identity = object.meta.identity;
                    object.meta = Metadata::disk(meta, disk_identity);
                    object.meta.identity = identity;
                    object.dirty = false;
                }
            } else if let Ok(native) = self.service.backend.open(
                path,
                &DiskOpenOptions {
                    read: true,
                    ..Default::default()
                },
            ) && let Ok(meta) = native.metadata()
                && let Ok(disk_identity) = native.identity()
                && let Ok(image) = Image::native(native)
            {
                object.image = image;
                let identity = object.meta.identity;
                object.meta = Metadata::disk(meta, disk_identity);
                object.meta.identity = identity;
                object.dirty = false;
            }
        }
    }
    fn enqueue(&self, s: &mut State, action: Action) -> (bool, io::Result<()>) {
        s.pending.push_back(Pending {
            action,
            lease: self.lease.as_ref().map(|c| c.authority.clone()),
        });
        let r = self.recover_locked(s);
        match r.blocked {
            Some(e) if !capacity(&e) => {
                // An unpublished temporary image can be abandoned safely. An
                // already published rename retains its directory-sync obligation.
                let published = s.pending.len() == 1
                    && s.pending.back().is_some_and(|p| {
                        matches!(
                            p.action,
                            Action::Put { stage: 3, .. }
                                | Action::Rename { stage: 1, .. }
                                | Action::Mkdir { stage: 1, .. }
                                | Action::Unlink { stage: 1, .. }
                        )
                    });
                if !published && let Some(p) = s.pending.pop_back() {
                    self.abandon(&p.action);
                }
                if published {
                    (true, Ok(()))
                } else {
                    (false, Err(e))
                }
            }
            _ => (true, Ok(())),
        }
    }
    pub fn recover(&self) -> RecoveryReport {
        let mut s = lock(&self.service.state);
        let report = self.recover_locked(&mut s);
        self.rebase(&mut s);
        Self::prune(&mut s);
        report
    }
    fn recover_locked(&self, s: &mut State) -> RecoveryReport {
        let mut completed = 0;
        let mut blocked = None;
        for _ in 0..64 {
            let Some(p) = s.pending.front_mut() else {
                break;
            };
            let result = p
                .lease
                .as_ref()
                .map_or(Ok(()), |l| l.check())
                .and_then(|_| self.replay(&mut p.action));
            match result {
                Ok(()) => {
                    if let Some(Pending {
                        action: Action::Rename { from, to, .. },
                        ..
                    }) = s.pending.pop_front()
                    {
                        s.redirects.retain(|(path, _)| *path != to);
                        for (_, source) in &mut s.redirects {
                            if source.starts_with(&from) {
                                *source = if *source == from {
                                    to.clone()
                                } else {
                                    to.join(source.strip_prefix(&from).unwrap())
                                };
                            }
                        }
                    }
                    completed += 1;
                }
                Err(e) => {
                    blocked = Some(e);
                    break;
                }
            }
        }
        RecoveryReport {
            completed_operations: completed,
            remaining_operations: s.pending.len(),
            blocked,
        }
    }
    fn replay(&self, a: &mut Action) -> io::Result<()> {
        let b = &self.service.backend;
        match a {
            Action::Put {
                path,
                temp,
                temp_file,
                parent_identity,
                image,
                permissions,
                stage,
            } => {
                if *stage == 0 {
                    let parent = b.metadata(path.parent().unwrap(), false)?;
                    if !parent.is_dir() {
                        return Err(error(
                            io::ErrorKind::NotADirectory,
                            "replacement parent changed",
                        ));
                    }
                    *parent_identity = b.identity(path.parent().unwrap(), false)?;
                    if parent_identity.is_none() {
                        return Err(error(
                            io::ErrorKind::PermissionDenied,
                            "replacement parent identity unavailable",
                        ));
                    }
                    let file = b.open(
                        temp,
                        &DiskOpenOptions {
                            read: true,
                            write: true,
                            create_new: true,
                            private: true,
                            ..Default::default()
                        },
                    )?;
                    *temp_file = Some(Arc::new(Mutex::new(file)));
                    *stage = 1;
                }
                if *stage < 3 {
                    let file = temp_file.as_ref().ok_or_else(|| {
                        error(io::ErrorKind::InvalidData, "missing temporary descriptor")
                    })?;
                    let mut file = lock(file);
                    #[cfg(unix)]
                    let named = b.metadata(temp, false)?;
                    if !same_disk_identity(file.identity()?, b.identity(temp, false)?)
                        || !same_disk_identity(
                            *parent_identity,
                            b.identity(path.parent().unwrap(), false)?,
                        )
                    {
                        return Err(error(
                            io::ErrorKind::PermissionDenied,
                            "temporary file or parent identity changed",
                        ));
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        if named.nlink() != 1 {
                            return Err(error(
                                io::ErrorKind::PermissionDenied,
                                "temporary file is hard-linked",
                            ));
                        }
                    }
                    if *stage == 1 {
                        file.set_len(0)?;
                        file.seek(SeekFrom::Start(0))?;
                        file.set_permissions(permissions.clone())?;
                        image.write_to(&mut **file)?;
                        file.sync_all()?;
                        *stage = 2;
                    }
                    if *stage == 2 {
                        b.rename(temp, path)?;
                        *stage = 3;
                    }
                }
                b.sync_directory(path.parent().unwrap())
            }
            Action::Mkdir {
                path,
                private,
                stage,
            } => {
                if *stage == 0 {
                    b.create_dir(path, *private)?;
                    *stage = 1;
                }
                b.sync_directory(path.parent().unwrap())
            }
            Action::Unlink { path, dir, stage } => {
                if *stage == 0 {
                    match if *dir {
                        b.remove_dir(path)
                    } else {
                        b.remove_file(path)
                    } {
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        r => r?,
                    }
                    *stage = 1;
                }
                b.sync_directory(path.parent().unwrap())
            }
            Action::Rename { from, to, stage } => {
                if *stage == 0 {
                    b.rename(from, to)?;
                    *stage = 1;
                }
                b.sync_directory(to.parent().unwrap())?;
                if from.parent() != to.parent() {
                    b.sync_directory(from.parent().unwrap())?;
                }
                Ok(())
            }
            Action::Chmod { path, permissions } => b.set_permissions(path, permissions.clone()),
            Action::Sync { path } => b.sync_directory(path),
        }
    }
    pub fn require_disk<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let path = self.norm(path.as_ref())?;
        let mut s = lock(&self.service.state);
        let report = self.recover_locked(&mut s);
        self.rebase(&mut s);
        Self::prune(&mut s);
        if s.pending.iter().any(|p| p.action.touches(&path)) {
            return Err(report.blocked.unwrap_or_else(|| {
                error(
                    io::ErrorKind::WouldBlock,
                    "bounded recovery has pending work",
                )
            }));
        }
        Ok(())
    }
    pub fn metadata<P: AsRef<Path>>(&self, path: P) -> io::Result<Metadata> {
        let path = self.norm(path.as_ref())?;
        let s = lock(&self.service.state);
        self.secure_path(&s, &path, false)?;
        self.lookup(&s, &path)
    }
    pub fn symlink_metadata<P: AsRef<Path>>(&self, path: P) -> io::Result<Metadata> {
        let path = self.norm(path.as_ref())?;
        let s = lock(&self.service.state);
        self.secure_path(&s, &path, true)?;
        self.lookup(&s, &path)
    }
    pub fn try_exists<P: AsRef<Path>>(&self, path: P) -> io::Result<bool> {
        match self.metadata(path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }
    pub fn read<P: AsRef<Path>>(&self, path: P) -> io::Result<Vec<u8>> {
        let file = self.open(path)?;
        let image = lock(&file.object).image.clone();
        let len = usize::try_from(image.len).map_err(|_| allocation_oom())?;
        let mut data = Zeroizing::new(Vec::new());
        data.try_reserve_exact(len).map_err(|_| allocation_oom())?;
        data.resize(len, 0);
        image.read_at(0, &mut data)?;
        Ok(std::mem::take(&mut *data))
    }
    pub fn read_to_string<P: AsRef<Path>>(&self, path: P) -> io::Result<String> {
        String::from_utf8(self.read(path)?)
            .map_err(|e| error(io::ErrorKind::InvalidData, &e.to_string()))
    }
    pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(&self, path: P, contents: C) -> io::Result<()> {
        self.replace_impl(path.as_ref(), contents.as_ref(), false, false)
    }
    pub fn replace<P: AsRef<Path>>(&self, path: P, contents: &[u8]) -> io::Result<()> {
        self.replace_impl(path.as_ref(), contents, false, true)
    }
    pub fn replace_private<P: AsRef<Path>>(&self, path: P, contents: &[u8]) -> io::Result<()> {
        self.replace_impl(path.as_ref(), contents, true, true)
    }
    fn replace_impl(
        &self,
        path: &Path,
        contents: &[u8],
        private: bool,
        new_object: bool,
    ) -> io::Result<()> {
        let path = self.norm(path)?;
        let mut s = lock(&self.service.state);
        self.recover_before(&mut s)?;
        self.preflight(&s, &path)?;
        s.entries.try_reserve(1).map_err(|_| allocation_oom())?;
        s.objects.try_reserve(1).map_err(|_| allocation_oom())?;
        let perms = if private {
            self.new_permissions(&s, &path, true, false)?
        } else {
            self.lookup(&s, &path)
                .map(|m| m.permissions())
                .map_or_else(
                    |e| {
                        if e.kind() == io::ErrorKind::NotFound {
                            self.new_permissions(&s, &path, false, false)
                        } else {
                            Err(e)
                        }
                    },
                    Ok,
                )?
        };
        let data = Arc::new(bytes(contents)?);
        let object = if !new_object {
            self.live_object(&s, &path)?
        } else {
            None
        };
        let meta = Metadata {
            identity: next_identity(),
            disk_identity: None,
            disk: None,
            kind: FileType {
                file: true,
                dir: false,
                symlink: false,
            },
            len: data.len() as u64,
            permissions: perms.clone(),
            modified: SystemTime::now(),
        };
        let a = Self::put_action(&mut s, &path, Image::memory(data.clone()), perms);
        let (accepted, result) = self.submit(&mut s, a);
        if !accepted {
            return result;
        }
        let object = if let Some(o) = object {
            *lock(&o) = Object::memory(data.clone(), meta);
            o
        } else {
            Arc::new(Mutex::new(Object::memory(data.clone(), meta)))
        };
        Self::entry(&mut s, path.clone(), Some(object));
        self.rebase(&mut s);
        result
    }
    fn prune(s: &mut State) {
        s.entries
            .retain(|entry| s.pending.iter().any(|p| p.action.touches(&entry.path)));
        s.objects.retain(|w| w.strong_count() > 0);
        s.redirects
            .retain(|(path, _)| s.pending.iter().any(|p| p.action.touches(path)));
    }
    fn recover_before(&self, s: &mut State) -> io::Result<()> {
        let report = self.recover_locked(s);
        self.rebase(s);
        Self::prune(s);
        match report.blocked {
            Some(e) if !capacity(&e) => Err(e),
            _ => Ok(()),
        }
    }
    pub fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        OpenOptions::new().read(true).open_in(self, path)
    }
    pub fn create<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open_in(self, path)
    }
    pub fn read_link<P: AsRef<Path>>(&self, path: P) -> io::Result<PathBuf> {
        let p = self.norm(path.as_ref())?;
        let s = lock(&self.service.state);
        self.secure_path(&s, &p, true)?;
        if !self.lookup(&s, &p)?.file_type().is_symlink() {
            return Err(error(io::ErrorKind::InvalidInput, "not a symlink"));
        }
        self.service.backend.read_link(&p)
    }
    pub fn canonicalize<P: AsRef<Path>>(&self, path: P) -> io::Result<PathBuf> {
        let p = self.norm(path.as_ref())?;
        match self.service.backend.canonicalize(&p) {
            Ok(p) => Ok(p),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.metadata(&p)?;
                Ok(p)
            }
            Err(e) => Err(e),
        }
    }
}
#[derive(Clone)]
pub struct DirEntry {
    fs: Fs,
    path: PathBuf,
    key: PathBuf,
}
impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }
    pub fn file_name(&self) -> OsString {
        self.path.file_name().unwrap_or_default().to_os_string()
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        self.fs.symlink_metadata(&self.key)
    }
    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.metadata()?.file_type())
    }
}
pub struct ReadDir {
    entries: std::vec::IntoIter<io::Result<DirEntry>>,
}
impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;
    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next()
    }
}
impl Fs {
    fn list(&self, s: &State, path: &Path) -> io::Result<Vec<PathBuf>> {
        if !self.lookup(s, path)?.is_dir() {
            return Err(error(io::ErrorKind::NotADirectory, "not a directory"));
        }
        let mut paths = Vec::new();
        match self.service.backend.read_dir(&Self::disk_path(s, path)) {
            Ok(entries) => {
                paths
                    .try_reserve(entries.len())
                    .map_err(|_| allocation_oom())?;
                for e in entries {
                    let e = DiskEntry {
                        path: path.join(e.file_name),
                        file_name: OsString::new(),
                    };
                    if s.pending
                        .iter()
                        .any(|p| matches!(&p.action,Action::Put{temp,..} if *temp==e.path))
                    {
                        continue;
                    }
                    match self.lookup(s, &e.path) {
                        Ok(_) => paths.push(e.path),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    && s.entries
                        .iter()
                        .any(|e| e.path == path && e.object.is_some()) => {}
            Err(e) => return Err(e),
        }
        for e in &s.entries {
            if e.path.parent() == Some(path)
                && e.object.is_some()
                && s.pending.iter().any(|p| p.action.touches(&e.path))
                && !paths.contains(&e.path)
            {
                paths.try_reserve(1).map_err(|_| allocation_oom())?;
                paths.push(e.path.clone());
            }
        }
        paths.sort();
        Ok(paths)
    }
    pub fn read_dir<P: AsRef<Path>>(&self, path: P) -> io::Result<ReadDir> {
        let p = self.norm(path.as_ref())?;
        let s = lock(&self.service.state);
        self.secure_path(&s, &p, false)?;
        let paths = self.list(&s, &p)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(paths.len())
            .map_err(|_| allocation_oom())?;
        for key in paths {
            let display = path.as_ref().join(key.file_name().unwrap_or_default());
            entries.push(Ok(DirEntry {
                fs: self.clone(),
                path: display,
                key,
            }));
        }
        Ok(ReadDir {
            entries: entries.into_iter(),
        })
    }
    pub fn create_dir<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        self.mkdir(path.as_ref(), false)
    }
    fn mkdir(&self, path: &Path, private: bool) -> io::Result<()> {
        let p = self.norm(path)?;
        let mut s = lock(&self.service.state);
        self.recover_before(&mut s)?;
        self.authority(&p)?;
        self.secure_path(&s, &p, false)?;
        self.parent(&s, &p)?;
        match self.lookup(&s, &p) {
            Ok(_) => return Err(error(io::ErrorKind::AlreadyExists, "path exists")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Self::prepare(&mut s, 1)?;
        let meta = Metadata {
            identity: next_identity(),
            disk_identity: None,
            disk: None,
            kind: FileType {
                file: false,
                dir: true,
                symlink: false,
            },
            len: 0,
            permissions: self.new_permissions(&s, &p, private, true)?,
            modified: SystemTime::now(),
        };
        let (accepted, result) = self.submit(
            &mut s,
            Action::Mkdir {
                path: p.clone(),
                private,
                stage: 0,
            },
        );
        if !accepted {
            return result;
        }
        Self::entry(
            &mut s,
            p.clone(),
            Some(Arc::new(Mutex::new(Object::memory(
                Arc::new(Zeroizing::new(Vec::new())),
                meta,
            )))),
        );
        result
    }
    pub fn create_dir_all<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        self.mkdir_all(path.as_ref(), false)
    }
    pub fn create_private_dir_all<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        self.mkdir_all(path.as_ref(), true)
    }
    fn mkdir_all(&self, path: &Path, private: bool) -> io::Result<()> {
        let p = self.norm(path)?;
        let mut cur = PathBuf::new();
        for c in p.components() {
            cur.push(c.as_os_str());
            match self.metadata(&cur) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => {
                    return Err(error(
                        io::ErrorKind::NotADirectory,
                        "ancestor is not a directory",
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => match self.mkdir(&cur, private) {
                    Err(e)
                        if e.kind() == io::ErrorKind::AlreadyExists
                            && self.metadata(&cur)?.is_dir() => {}
                    r => r?,
                },
                Err(e) => return Err(e),
            }
        }
        #[cfg(unix)]
        if private {
            self.set_permissions(&p, permissions(true, true))?;
        }
        Ok(())
    }
    pub fn remove_file<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        self.unlink(path.as_ref(), false)
    }
    pub fn remove_dir<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        self.unlink(path.as_ref(), true)
    }
    fn unlink(&self, path: &Path, dir: bool) -> io::Result<()> {
        let p = self.norm(path)?;
        let mut s = lock(&self.service.state);
        self.recover_before(&mut s)?;
        self.authority(&p)?;
        self.secure_path(&s, &p, false)?;
        let m = self.lookup(&s, &p)?;
        if m.is_dir() != dir {
            return Err(error(io::ErrorKind::InvalidInput, "incorrect removal type"));
        }
        if dir && !self.list(&s, &p)?.is_empty() {
            return Err(error(
                io::ErrorKind::DirectoryNotEmpty,
                "directory not empty",
            ));
        }
        Self::prepare(&mut s, 1)?;
        let (accepted, result) = self.submit(
            &mut s,
            Action::Unlink {
                path: p.clone(),
                dir,
                stage: 0,
            },
        );
        if accepted {
            for object in s.objects.iter().filter_map(|w| w.upgrade()) {
                let mut object = lock(&object);
                if object.path.as_ref() == Some(&p) {
                    object.path = None;
                }
            }
            Self::entry(&mut s, p, None);
        }
        result
    }
    pub fn remove_dir_all<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let p = self.norm(path.as_ref())?;
        for e in self.read_dir(&p)? {
            let e = e?;
            if e.file_type()?.is_dir() {
                self.remove_dir_all(e.path())?;
            } else {
                self.remove_file(e.path())?;
            }
        }
        self.remove_dir(p)
    }
    pub fn rename<P: AsRef<Path>, Q: AsRef<Path>>(&self, from: P, to: Q) -> io::Result<()> {
        let from = self.norm(from.as_ref())?;
        let to = self.norm(to.as_ref())?;
        let mut s = lock(&self.service.state);
        self.recover_before(&mut s)?;
        self.authority(&from)?;
        self.authority(&to)?;
        self.secure_path(&s, &from, false)?;
        self.secure_path(&s, &to, false)?;
        let meta = self.lookup(&s, &from)?;
        if from == to {
            return Ok(());
        }
        if to.starts_with(&from) {
            return Err(error(
                io::ErrorKind::InvalidInput,
                "rename into own subtree",
            ));
        }
        self.parent(&s, &to)?;
        match self.lookup(&s, &to) {
            Ok(dest) => {
                if dest.is_dir() != meta.is_dir() {
                    return Err(error(io::ErrorKind::InvalidInput, "rename type mismatch"));
                }
                if dest.is_dir() && !self.list(&s, &to)?.is_empty() {
                    return Err(error(
                        io::ErrorKind::DirectoryNotEmpty,
                        "destination not empty",
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let count = s
            .entries
            .iter()
            .filter(|e| e.path.starts_with(&from))
            .count();
        Self::prepare(&mut s, count + 2)?;
        let source = Self::disk_path(&s, &from);
        // Capture only a descriptor and metadata, never directory descendants or
        // file payloads. A redirect merges unchanged descendants during fallback.
        let root = if meta.is_file() {
            self.object(&mut s, &from)?
        } else {
            Arc::new(Mutex::new(Object::memory(
                Arc::new(Zeroizing::new(Vec::new())),
                meta,
            )))
        };
        let mut moved = Vec::new();
        moved
            .try_reserve_exact(count + 1)
            .map_err(|_| allocation_oom())?;
        moved.push((to.clone(), Some(root.clone())));
        for e in &s.entries {
            if e.path.starts_with(&from) && e.path != from {
                moved.push((
                    to.join(e.path.strip_prefix(&from).unwrap()),
                    e.object.clone(),
                ));
            }
        }
        let (accepted, result) = self.submit(
            &mut s,
            Action::Rename {
                from: from.clone(),
                to: to.clone(),
                stage: 0,
            },
        );
        if accepted {
            for object in s.objects.iter().filter_map(|w| w.upgrade()) {
                let mut object = lock(&object);
                if let Some(path) = &object.path {
                    if path.starts_with(&from) {
                        object.path = Some(if *path == from {
                            to.clone()
                        } else {
                            to.join(path.strip_prefix(&from).unwrap())
                        });
                    } else if path.starts_with(&to) {
                        object.path = None;
                    }
                }
            }
            for e in &mut s.entries {
                if e.path.starts_with(&from) {
                    e.object = None;
                }
            }
            Self::entry(&mut s, from.clone(), None);
            for (path, object) in moved {
                Self::entry(&mut s, path, object);
            }
            s.redirects
                .retain(|(path, _)| !path.starts_with(&from) && !path.starts_with(&to));
            if s.pending.iter().any(|p| p.action.touches(&to)) {
                s.redirects.push((to, source));
            }
        }
        result
    }
    pub fn copy<P: AsRef<Path>, Q: AsRef<Path>>(&self, from: P, to: Q) -> io::Result<u64> {
        let data = Zeroizing::new(self.read(&from)?);
        let p = self.metadata(&from)?.permissions();
        self.write(&to, &*data)?;
        self.set_permissions(to, p)?;
        Ok(data.len() as u64)
    }
    pub fn set_permissions<P: AsRef<Path>>(
        &self,
        path: P,
        permissions: Permissions,
    ) -> io::Result<()> {
        let p = self.norm(path.as_ref())?;
        let mut s = lock(&self.service.state);
        self.recover_before(&mut s)?;
        self.authority(&p)?;
        self.secure_path(&s, &p, false)?;
        self.capture_shallow(&mut s, &p)?;
        Self::prepare(&mut s, 0)?;
        let (accepted, result) = self.submit(
            &mut s,
            Action::Chmod {
                path: p.clone(),
                permissions: permissions.clone(),
            },
        );
        if !accepted {
            return result;
        }
        if let Some(o) = s
            .entries
            .iter()
            .find(|e| e.path == p)
            .and_then(|e| e.object.as_ref())
        {
            let mut o = lock(o);
            o.meta.permissions = permissions.clone();
            o.meta.disk = None;
        }
        result
    }
    fn capture_shallow(&self, s: &mut State, p: &Path) -> io::Result<()> {
        let meta = self.lookup(s, p)?;
        if meta.is_file() {
            self.object(s, p)?;
        } else if meta.is_dir() && !s.entries.iter().any(|e| e.path == p) {
            Self::prepare(s, 1)?;
            Self::entry(
                s,
                p.to_path_buf(),
                Some(Arc::new(Mutex::new(Object::memory(
                    Arc::new(Zeroizing::new(Vec::new())),
                    meta,
                )))),
            );
        }
        Ok(())
    }
    pub fn sync_directory<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let p = self.norm(path.as_ref())?;
        let mut s = lock(&self.service.state);
        self.recover_before(&mut s)?;
        self.authority(&p)?;
        self.secure_path(&s, &p, false)?;
        if !self.lookup(&s, &p)?.is_dir() {
            return Err(error(io::ErrorKind::NotADirectory, "not a directory"));
        }
        Self::prepare(&mut s, 0)?;
        self.submit(&mut s, Action::Sync { path: p }).1
    }
    pub fn open_beneath<P: AsRef<Path>, Q: AsRef<Path>>(
        &self,
        root: P,
        relative: Q,
    ) -> io::Result<File> {
        if relative.as_ref().as_os_str().is_empty()
            || relative
                .as_ref()
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(error(
                io::ErrorKind::PermissionDenied,
                "relative path must contain normal components only",
            ));
        }
        let root = self.norm(root.as_ref())?;
        let p = root.join(relative);
        let mut s = lock(&self.service.state);
        let _ = self.recover_locked(&mut s);
        self.rebase(&mut s);
        Self::prune(&mut s);
        self.secure_path(&s, &p, false)?;
        if s.entries.iter().any(|e| e.path == p) {
            let object = self.object(&mut s, &p)?;
            return Ok(File {
                fs: self.clone(),
                object,
                cursor: Arc::new(Mutex::new(0)),
                read: true,
                write: false,
                append: false,
            });
        }
        let native = self
            .service
            .backend
            .open_beneath(&Self::disk_path(&s, &root), p.strip_prefix(&root).unwrap())?;
        if !native.metadata()?.is_file() {
            return Err(error(io::ErrorKind::InvalidInput, "not a regular file"));
        }
        let object = Arc::new(Mutex::new(Object::native(native, p)?));
        s.objects.try_reserve(1).map_err(|_| allocation_oom())?;
        s.objects.push(Arc::downgrade(&object));

        Ok(File {
            fs: self.clone(),
            object,
            cursor: Arc::new(Mutex::new(0)),
            read: true,
            write: false,
            append: false,
        })
    }
}
#[derive(Clone, Default, Debug)]
pub struct OpenOptions {
    options: DiskOpenOptions,
}
impl OpenOptions {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn read(&mut self, v: bool) -> &mut Self {
        self.options.read = v;
        self
    }
    pub fn write(&mut self, v: bool) -> &mut Self {
        self.options.write = v;
        self
    }
    pub fn append(&mut self, v: bool) -> &mut Self {
        self.options.append = v;
        self
    }
    pub fn truncate(&mut self, v: bool) -> &mut Self {
        self.options.truncate = v;
        self
    }
    pub fn create(&mut self, v: bool) -> &mut Self {
        self.options.create = v;
        self
    }
    pub fn create_new(&mut self, v: bool) -> &mut Self {
        self.options.create_new = v;
        self
    }
    pub fn private(&mut self, v: bool) -> &mut Self {
        self.options.private = v;
        self
    }
    pub fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        self.open_in(global(), path)
    }
    pub fn open_in<P: AsRef<Path>>(&self, fs: &Fs, path: P) -> io::Result<File> {
        let p = fs.norm(path.as_ref())?;
        let o = &self.options;
        let writable = o.write || o.append;
        if (o.create_new || o.create || o.truncate || !o.read) && !writable
            || o.truncate && o.append && !o.create_new
        {
            return Err(error(io::ErrorKind::InvalidInput, "invalid open options"));
        }
        let mut s = lock(&fs.service.state);
        if writable {
            fs.recover_before(&mut s)?;
        } else {
            let _ = fs.recover_locked(&mut s);
            fs.rebase(&mut s);
            Fs::prune(&mut s);
        }
        fs.secure_path(&s, &p, false)?;
        let exists = match fs.lookup(&s, &p) {
            Ok(m) => {
                if !m.is_file() {
                    return Err(error(io::ErrorKind::InvalidInput, "not a regular file"));
                }
                true
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        if exists && o.create_new {
            return Err(error(io::ErrorKind::AlreadyExists, "path exists"));
        }
        if !exists && !o.create && !o.create_new {
            return Err(error(io::ErrorKind::NotFound, "path does not exist"));
        }
        if writable {
            fs.preflight(&s, &p)?;
        }
        if !exists || o.truncate {
            s.entries.try_reserve(1).map_err(|_| allocation_oom())?;
            s.objects.try_reserve(1).map_err(|_| allocation_oom())?;
            let perms = if o.private {
                fs.new_permissions(&s, &p, true, false)?
            } else {
                fs.lookup(&s, &p).map(|m| m.permissions()).map_or_else(
                    |e| {
                        if e.kind() == io::ErrorKind::NotFound {
                            fs.new_permissions(&s, &p, false, false)
                        } else {
                            Err(e)
                        }
                    },
                    Ok,
                )?
            };
            let data = Arc::new(Zeroizing::new(Vec::new()));
            let meta = Metadata {
                identity: next_identity(),
                disk_identity: None,
                disk: None,
                kind: FileType {
                    file: true,
                    dir: false,
                    symlink: false,
                },
                len: 0,
                permissions: perms.clone(),
                modified: SystemTime::now(),
            };
            let obj = fs.live_object(&s, &p)?;
            let a = Fs::put_action(&mut s, &p, Image::memory(data.clone()), perms);
            let (accepted, result) = fs.submit(&mut s, a);
            if !accepted {
                result?;
                unreachable!();
            }
            let object = if let Some(obj) = obj {
                *lock(&obj) = Object::memory(data.clone(), meta);
                obj
            } else {
                Arc::new(Mutex::new(Object::memory(data.clone(), meta)))
            };
            Fs::entry(&mut s, p.clone(), Some(object));
            result?;
        }
        fs.rebase(&mut s);
        let object = fs.object(&mut s, &p)?;
        Ok(File {
            fs: fs.clone(),
            object,
            cursor: Arc::new(Mutex::new(0)),
            read: o.read,
            write: writable,
            append: o.append,
        })
    }
}
pub struct File {
    fs: Fs,
    object: Obj,
    cursor: Arc<Mutex<u64>>,
    read: bool,
    write: bool,
    append: bool,
}
impl File {
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        global().open(path)
    }
    pub fn create<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        global().create(path)
    }
    pub fn open_in<P: AsRef<Path>>(fs: &Fs, path: P) -> io::Result<Self> {
        fs.open(path)
    }
    pub fn create_in<P: AsRef<Path>>(fs: &Fs, path: P) -> io::Result<Self> {
        fs.create(path)
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            fs: self.fs.clone(),
            object: self.object.clone(),
            cursor: self.cursor.clone(),
            read: self.read,
            write: self.write,
            append: self.append,
        })
    }
    pub fn metadata(&self) -> io::Result<Metadata> {
        let object = lock(&self.object);
        if !object.dirty
            && let Source::Native(file) = &object.image.source
        {
            let file = lock(file);
            let mut meta = Metadata::disk(file.metadata()?, file.identity()?);
            meta.identity = object.meta.identity;
            Ok(meta)
        } else {
            Ok(object.meta.clone())
        }
    }
    fn mutate(&self, offset: Option<u64>, data: &[u8], size: Option<u64>) -> io::Result<usize> {
        if !self.write {
            return Err(error(
                io::ErrorKind::PermissionDenied,
                "handle is not writable",
            ));
        }
        if data.is_empty() && size.is_none() {
            return Ok(0);
        }
        let mut s = lock(&self.fs.service.state);
        self.fs.recover_before(&mut s)?;
        let mut cursor = lock(&self.cursor);
        let mut object = lock(&self.object);
        if !object.dirty
            && let Source::Native(native) = &object.image.source
        {
            let (meta, disk_identity) = {
                let native = lock(native);
                (native.metadata()?, native.identity()?)
            };
            if disk_identity.is_none() {
                return Err(error(
                    io::ErrorKind::PermissionDenied,
                    "native file identity unavailable",
                ));
            }
            if let Some(path) = &object.path
                && !s.pending.iter().any(|p| p.action.touches(path))
            {
                match self.fs.service.backend.identity(path, false) {
                    Ok(named) if same_disk_identity(disk_identity, named) => {}
                    Ok(None) => {
                        return Err(error(
                            io::ErrorKind::PermissionDenied,
                            "native identity unavailable",
                        ));
                    }
                    Ok(_) => object.path = None,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => object.path = None,
                    Err(e) => return Err(e),
                }
            }
            object.image.base_len = meta.len();
            object.image.len = meta.len();
            let identity = object.meta.identity;
            object.meta = Metadata::disk(meta, disk_identity);
            object.meta.identity = identity;
        }
        let path = object.path.clone();
        let start = if self.append && size.is_none() {
            object.image.len
        } else {
            offset.unwrap_or(*cursor)
        };
        let end = start
            .checked_add(data.len() as u64)
            .ok_or_else(allocation_oom)?;
        let len = size.unwrap_or(end.max(object.image.len));
        let image = object.image.patched(start, data, len)?;
        let perms = object.meta.permissions();
        drop(object);
        if let Some(path) = &path {
            self.fs.preflight(&s, path)?;
        } else if let Some(lease) = &self.fs.lease {
            lease.check()?;
        }
        let (accepted, result) = if let Some(path) = &path {
            s.entries.try_reserve(1).map_err(|_| allocation_oom())?;
            s.objects.try_reserve(1).map_err(|_| allocation_oom())?;
            let action = Fs::put_action(&mut s, path, image.clone(), perms);
            self.fs.submit(&mut s, action)
        } else {
            self.fs.reserve(&mut s, image.payload_bytes(), 0)?;
            (true, Ok(()))
        };
        if !accepted {
            result?;
            unreachable!();
        }
        {
            let mut object = lock(&self.object);
            object.image = image;
            object.meta.len = len;
            object.meta.modified = SystemTime::now();
            object.meta.disk = None;
            if object.meta.identity == 0 {
                object.meta.identity = next_identity();
            }
            object.dirty = true;
        }
        if let Some(path) = path {
            Fs::entry(&mut s, path, Some(self.object.clone()));
        }
        if size.is_none() {
            *cursor = end;
        }
        self.fs.rebase(&mut s);
        result?;
        Ok(data.len())
    }
    pub fn set_len(&self, size: u64) -> io::Result<()> {
        self.mutate(None, &[], Some(size)).map(|_| ())
    }
    pub fn sync_data(&self) -> io::Result<()> {
        let r = self.fs.recover();
        match r.blocked {
            Some(e) if !capacity(&e) => Err(e),
            _ => Ok(()),
        }
    }
    pub fn sync_all(&self) -> io::Result<()> {
        self.sync_data()
    }
    pub fn set_permissions(&self, p: Permissions) -> io::Result<()> {
        let mut s = lock(&self.fs.service.state);
        self.fs.recover_before(&mut s)?;
        let path = {
            let mut object = lock(&self.object);
            if let Some(path) = &object.path
                && !s.pending.iter().any(|p| p.action.touches(path))
                && let Source::Native(native) = &object.image.source
            {
                let held = lock(native).identity()?;
                match self.fs.service.backend.identity(path, false) {
                    Ok(named) if same_disk_identity(held, named) => {}
                    Ok(None) => {
                        return Err(error(
                            io::ErrorKind::PermissionDenied,
                            "native identity unavailable",
                        ));
                    }
                    Ok(_) => object.path = None,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => object.path = None,
                    Err(e) => return Err(e),
                }
            }
            object.path.clone()
        };
        let (accepted, result) = if let Some(path) = &path {
            self.fs.authority(path)?;
            self.fs.secure_path(&s, path, false)?;
            Fs::prepare(&mut s, 1)?;
            self.fs.submit(
                &mut s,
                Action::Chmod {
                    path: path.clone(),
                    permissions: p.clone(),
                },
            )
        } else {
            if let Some(lease) = &self.fs.lease {
                lease.check()?;
            }
            (true, Ok(()))
        };
        if accepted {
            let mut object = lock(&self.object);
            object.meta.permissions = p;
            object.meta.disk = None;
            object.dirty = true;
            drop(object);
            if let Some(path) = path {
                Fs::entry(&mut s, path, Some(self.object.clone()));
            }
            self.fs.rebase(&mut s);
        }
        result
    }
}
impl Read for File {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.read {
            return Err(error(
                io::ErrorKind::PermissionDenied,
                "handle is not readable",
            ));
        }
        let mut cursor = lock(&self.cursor);
        let (image, dirty) = {
            let object = lock(&self.object);
            (object.image.clone(), object.dirty)
        };
        let n = if !dirty && let Source::Native(file) = &image.source {
            let mut file = lock(file);
            file.seek(SeekFrom::Start(*cursor))?;
            file.read(buf)?
        } else {
            image.read_at(*cursor, buf)?
        };
        *cursor += n as u64;
        Ok(n)
    }
}
impl Write for File {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.mutate(None, data, None)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.sync_data()
    }
}
impl Seek for File {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let mut cursor = lock(&self.cursor);
        let next = match from {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => *cursor as i128 + n as i128,
            SeekFrom::End(n) => self.metadata()?.len() as i128 + n as i128,
        };
        if !(0..=u64::MAX as i128).contains(&next) {
            return Err(error(io::ErrorKind::InvalidInput, "invalid seek"));
        }
        *cursor = next as u64;
        Ok(*cursor)
    }
}
macro_rules! forward {($($name:ident -> $out:ty;)+)=>{$(pub fn $name<P:AsRef<Path>>(path:P)->io::Result<$out>{global().$name(path)})+};}
forward! {read->Vec<u8>;read_to_string->String;create_dir->();create_dir_all->();create_private_dir_all->();remove_file->();remove_dir->();remove_dir_all->();metadata->Metadata;symlink_metadata->Metadata;read_dir->ReadDir;read_link->PathBuf;canonicalize->PathBuf;try_exists->bool;sync_directory->();require_disk->();}
pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
    global().write(path, contents)
}
pub fn replace<P: AsRef<Path>>(path: P, contents: &[u8]) -> io::Result<()> {
    global().replace(path, contents)
}
pub fn replace_private<P: AsRef<Path>>(path: P, contents: &[u8]) -> io::Result<()> {
    global().replace_private(path, contents)
}
pub fn set_permissions<P: AsRef<Path>>(path: P, p: Permissions) -> io::Result<()> {
    global().set_permissions(path, p)
}
pub fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()> {
    global().rename(from, to)
}
pub fn copy<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<u64> {
    global().copy(from, to)
}
pub fn open_beneath<P: AsRef<Path>, Q: AsRef<Path>>(root: P, relative: Q) -> io::Result<File> {
    global().open_beneath(root, relative)
}

#[cfg(test)]
mod tests;

#[derive(Default, Debug)]
pub struct DirBuilder {
    mode: Option<u32>,
}
impl DirBuilder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = Some(mode);
        self
    }
    pub fn create<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        match self.mode {
            Some(0o700) => global().mkdir(path.as_ref(), true),
            None => global().create_dir(path),
            Some(_) => Err(error(
                io::ErrorKind::Unsupported,
                "only private directory mode 0700 is supported",
            )),
        }
    }
}
impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("File")
            .field("read", &self.read)
            .field("write", &self.write)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for Fs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fs").finish_non_exhaustive()
    }
}
