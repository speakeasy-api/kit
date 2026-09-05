//! Black-box fault injection around the production disk backend.
use super::*;
use std::fs as native;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Point {
    Open,
    Write,
    Sync,
    Rename,
    Mkdir,
    DirectorySync,
    Chmod,
    Remove,
    ProbeCollision,
    ProbeSwap,
    Read,
    Seek,
    Metadata,
    AfterWrite,
    AfterRename,
    LeaseCheck,
    FileDrop,
}
#[derive(Default)]
struct Faults(Mutex<Option<(Point, i32)>>, AtomicUsize, AtomicUsize);
impl Faults {
    fn arm_panic(&self, point: Point) {
        self.2.store(point as usize + 1, Ordering::SeqCst);
    }
    fn panic_at(&self, point: Point) {
        if self
            .2
            .compare_exchange(point as usize + 1, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            panic!("backend panic at {point:?}");
        }
    }
    fn arm(&self, point: Point, code: i32) {
        *self.0.lock().unwrap() = Some((point, code));
    }
    fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }
    fn check(&self, point: Point) -> io::Result<()> {
        match *self.0.lock().unwrap() {
            Some((p, code)) if p == point => Err(io::Error::from_raw_os_error(code)),
            _ => Ok(()),
        }
    }
}
struct Injected {
    disk: DiskBackend,
    faults: Arc<Faults>,
}
struct InjectedFile {
    disk: Box<dyn BackendFile>,
    faults: Arc<Faults>,
    wrote_prefix: bool,
}
impl Read for InjectedFile {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        let n = self.disk.read(b)?;
        self.faults.panic_at(Point::Read);
        self.faults.check(Point::Read)?;
        Ok(n)
    }
}
impl Seek for InjectedFile {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        let position = self.disk.seek(p)?;
        self.faults.panic_at(Point::Seek);
        self.faults.check(Point::Seek)?;
        Ok(position)
    }
}
impl Write for InjectedFile {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.faults.check(Point::Write).is_err() && !b.is_empty() {
            if self.wrote_prefix {
                self.faults.check(Point::Write)?;
            }
            self.wrote_prefix = true;
            return self.disk.write(&b[..b.len().min(3)]);
        }
        let n = self.disk.write(b)?;
        self.faults.panic_at(Point::AfterWrite);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.disk.flush()
    }
}
impl BackendFile for InjectedFile {
    fn identity(&self) -> io::Result<Option<FileIdentity>> {
        self.disk.identity()
    }
    fn metadata(&self) -> io::Result<native::Metadata> {
        let meta = self.disk.metadata()?;
        self.faults.panic_at(Point::Metadata);
        Ok(meta)
    }
    fn set_len(&self, n: u64) -> io::Result<()> {
        self.disk.set_len(n)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.faults.check(Point::Sync)?;
        self.disk.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.faults.check(Point::Sync)?;
        self.disk.sync_all()
    }
    fn set_permissions(&self, p: Permissions) -> io::Result<()> {
        self.disk.set_permissions(p)
    }
}
impl Drop for InjectedFile {
    fn drop(&mut self) {
        self.faults.panic_at(Point::FileDrop);
    }
}
struct InjectedLease {
    disk: Box<dyn BackendLease>,
    faults: Arc<Faults>,
}
impl BackendLease for InjectedLease {
    fn check(&self) -> io::Result<()> {
        self.disk.check()?;
        self.faults.panic_at(Point::LeaseCheck);
        Ok(())
    }
}
impl Backend for Injected {
    fn identity(&self, path: &Path, follow: bool) -> io::Result<Option<FileIdentity>> {
        self.disk.identity(path, follow)
    }
    fn open(&self, p: &Path, o: &DiskOpenOptions) -> io::Result<Box<dyn BackendFile>> {
        if o.write || o.append {
            self.faults.check(Point::Open)?;
        }
        let probe = p
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with(".kit-mode-"));
        if probe {
            self.faults.1.fetch_add(1, Ordering::Relaxed);
            self.faults.check(Point::ProbeCollision)?;
        }
        let disk = self.disk.open(p, o)?;
        if probe && self.faults.check(Point::ProbeSwap).is_err() {
            native::rename(p, p.with_extension("held")).unwrap();
            native::write(p, b"replacement").unwrap();
        }
        Ok(Box::new(InjectedFile {
            disk,
            faults: self.faults.clone(),
            wrote_prefix: false,
        }))
    }
    fn metadata(&self, p: &Path, follow: bool) -> io::Result<native::Metadata> {
        self.disk.metadata(p, follow)
    }
    fn read_dir(&self, p: &Path) -> io::Result<Vec<DiskEntry>> {
        self.disk.read_dir(p)
    }
    fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
        self.disk.read_link(p)
    }
    fn canonicalize(&self, p: &Path) -> io::Result<PathBuf> {
        self.disk.canonicalize(p)
    }
    fn create_dir(&self, p: &Path, private: bool) -> io::Result<()> {
        self.faults.check(Point::Mkdir)?;
        self.disk.create_dir(p, private)
    }
    fn remove_file(&self, p: &Path) -> io::Result<()> {
        self.faults.check(Point::Remove)?;
        self.disk.remove_file(p)
    }
    fn remove_dir(&self, p: &Path) -> io::Result<()> {
        self.disk.remove_dir(p)
    }
    fn rename(&self, a: &Path, b: &Path) -> io::Result<()> {
        self.faults.check(Point::Rename)?;
        self.disk.rename(a, b)?;
        self.faults.panic_at(Point::AfterRename);
        Ok(())
    }
    fn set_permissions(&self, p: &Path, mode: Permissions) -> io::Result<()> {
        self.faults.check(Point::Chmod)?;
        self.disk.set_permissions(p, mode)
    }
    fn sync_directory(&self, p: &Path) -> io::Result<()> {
        self.faults.check(Point::DirectorySync)?;
        self.disk.sync_directory(p)
    }
    fn acquire_lease(&self, r: &LeaseRequest) -> io::Result<Box<dyn BackendLease>> {
        Ok(Box::new(InjectedLease {
            disk: self.disk.acquire_lease(r)?,
            faults: self.faults.clone(),
        }))
    }
    fn open_beneath(&self, root: &Path, p: &Path) -> io::Result<Box<dyn BackendFile>> {
        self.disk.open_beneath(root, p)
    }
}
struct Fixture {
    root: PathBuf,
    fs: Fs,
    faults: Arc<Faults>,
}
impl Fixture {
    fn new() -> Self {
        Self::budget(1024 * 1024, 4096)
    }
    fn budget(bytes: usize, ops: usize) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "kit-resilient-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        native::create_dir(&root).unwrap();
        let faults = Arc::new(Faults::default());
        let fs = Fs::with_budget(
            Arc::new(Injected {
                disk: DiskBackend,
                faults: faults.clone(),
            }),
            bytes,
            ops,
        );
        Self { root, fs, faults }
    }
    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
    fn settle(&self) {
        self.faults.clear();
        self.fs.require_disk(&self.root).unwrap();
        let report = self.fs.recover();
        assert_eq!(report.remaining_operations, 0);
        assert!(report.blocked.is_none());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = native::remove_dir_all(&self.root);
    }
}
fn names(fs: &Fs, p: &Path) -> Vec<OsString> {
    let mut names: Vec<_> = fs
        .read_dir(p)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    names.sort();
    names
}

#[test]
#[cfg(unix)]
fn capacity_at_each_commit_stage_is_accepted_but_not_durable() {
    for code in [libc::ENOSPC, libc::EDQUOT] {
        for point in [
            Point::Open,
            Point::Write,
            Point::Sync,
            Point::Rename,
            Point::DirectorySync,
        ] {
            let t = Fixture::new();
            let p = t.path("value");
            native::write(&p, b"old disk content").unwrap();
            t.faults.arm(point, code);
            t.fs.replace(&p, b"complete replacement bytes").unwrap();
            assert_eq!(
                t.fs.read(&p).unwrap(),
                b"complete replacement bytes",
                "{point:?}"
            );
            for _ in 0..3 {
                let r = t.fs.recover();
                assert!(r.remaining_operations > 0, "{point:?}");
                assert_eq!(r.blocked.unwrap().raw_os_error(), Some(code));
                assert_eq!(
                    t.fs.require_disk(&p).unwrap_err().raw_os_error(),
                    Some(code)
                );
            }
            t.settle();
            assert_eq!(native::read(&p).unwrap(), b"complete replacement bytes");
            let disk_names: Vec<_> = native::read_dir(&t.root)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(
                disk_names,
                vec![OsString::from("value")],
                "temporary file leaked at {point:?}"
            );
        }
    }
}

#[test]
#[cfg(unix)]
fn handles_close_reopen_append_seek_clone_and_truncate_during_outage() {
    let t = Fixture::new();
    let p = t.path("log");
    t.faults.arm(Point::Open, libc::ENOSPC);
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open_in(&t.fs, &p)
        .unwrap();
    f.write_all(b"abcdef").unwrap();
    f.sync_data().unwrap();
    f.seek(SeekFrom::Start(2)).unwrap();
    let mut clone = f.try_clone().unwrap();
    clone.write_all(b"XY").unwrap();
    assert_eq!(f.stream_position().unwrap(), 4);
    drop(clone);
    drop(f);
    assert_eq!(t.fs.read(&p).unwrap(), b"abXYef");
    let mut append = OpenOptions::new().append(true).open_in(&t.fs, &p).unwrap();
    append.seek(SeekFrom::Start(0)).unwrap();
    append.write_all(b"!").unwrap();
    append.sync_all().unwrap();
    drop(append);
    assert_eq!(t.fs.read(&p).unwrap(), b"abXYef!");
    let f = OpenOptions::new().write(true).open_in(&t.fs, &p).unwrap();
    f.set_len(4).unwrap();
    f.set_len(6).unwrap();
    drop(f);
    assert_eq!(t.fs.read(&p).unwrap(), b"abXY\0\0");
    t.settle();
    assert_eq!(native::read(&p).unwrap(), b"abXY\0\0");
    let mut f = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open_in(&t.fs, &p)
        .unwrap();
    f.write_all(b"new").unwrap();
    drop(f);
    t.settle();
    assert_eq!(native::read(&p).unwrap(), b"new");
}

#[test]
#[cfg(unix)]
fn overlay_directory_tree_rename_delete_and_recreate() {
    let t = Fixture::new();
    for code in [libc::ENOSPC, libc::EDQUOT] {
        t.faults.arm(Point::Mkdir, code);
        let a = t.path("a");
        let b = t.path("b");
        t.fs.create_dir_all(a.join("nested")).unwrap();
        t.fs.write(a.join("nested/one"), b"one").unwrap();
        t.fs.write(a.join("two"), b"two").unwrap();
        assert!(!a.exists());
        assert!(t.fs.metadata(&a).unwrap().is_dir());
        assert_eq!(
            names(&t.fs, &a),
            vec![OsString::from("nested"), OsString::from("two")]
        );
        t.fs.rename(&a, &b).unwrap();
        assert!(!t.fs.try_exists(&a).unwrap());
        assert_eq!(t.fs.read(b.join("nested/one")).unwrap(), b"one");
        t.fs.remove_file(b.join("two")).unwrap();
        t.fs.write(b.join("two"), b"reborn").unwrap();
        t.fs.remove_dir_all(b.join("nested")).unwrap();
        t.fs.create_dir(b.join("nested")).unwrap();
        t.fs.write(b.join("nested/new"), b"new").unwrap();
        t.settle();
        assert!(!a.exists());
        assert_eq!(native::read(b.join("two")).unwrap(), b"reborn");
        assert!(!b.join("nested/one").exists());
        assert_eq!(native::read(b.join("nested/new")).unwrap(), b"new");
        t.fs.remove_dir_all(&b).unwrap();
        t.settle();
        assert!(!b.exists());
    }
}

#[test]
#[cfg(unix)]
fn noncapacity_errors_are_not_reported_as_success() {
    for code in [libc::EACCES, libc::EIO, libc::EROFS] {
        for point in [
            Point::Open,
            Point::Write,
            Point::Sync,
            Point::Rename,
            Point::DirectorySync,
            Point::Mkdir,
        ] {
            let t = Fixture::new();
            t.faults.arm(point, code);
            let result = if point == Point::Mkdir {
                t.fs.create_dir(t.path("dir"))
            } else {
                t.fs.replace(t.path("file"), b"long enough for partial write")
            };
            if point == Point::DirectorySync {
                assert!(result.is_ok());
                assert!(t.fs.recover().blocked.is_some());
            } else {
                assert_eq!(result.unwrap_err().raw_os_error(), Some(code), "{point:?}");
            }
        }
    }
}

#[test]
#[cfg(unix)]
fn private_modes_survive_fallback_and_recovery() {
    use std::os::unix::fs::PermissionsExt;
    let t = Fixture::new();
    t.faults.arm(Point::Mkdir, libc::EDQUOT);
    let dir = t.path("private");
    let p = dir.join("secret");
    t.fs.create_private_dir_all(&dir).unwrap();
    t.fs.replace_private(&p, b"secret").unwrap();
    assert_eq!(
        t.fs.metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        t.fs.metadata(&p).unwrap().permissions().mode() & 0o777,
        0o600
    );
    t.settle();
    assert_eq!(
        native::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        native::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o600
    );
    native::set_permissions(&p, Permissions::from_mode(0o640)).unwrap();
    let fs = Fs::new(Arc::new(DiskBackend));
    fs.replace(&p, b"updated").unwrap();
    fs.require_disk(&p).unwrap();
    assert_eq!(
        native::metadata(&p).unwrap().permissions().mode() & 0o777,
        0o640
    );
}

#[test]
#[cfg(unix)]
fn budgets_reject_without_losing_previously_accepted_data() {
    for (bytes, ops) in [(16, 100), (1024, 1)] {
        let t = Fixture::budget(bytes, ops);
        t.faults.arm(Point::Open, libc::ENOSPC);
        let p = t.path("accepted");
        t.fs.replace(&p, b"kept").unwrap();
        let rejected = t.path("rejected");
        let payload = vec![b'x'; if bytes == 16 { 32 } else { 4 }];
        assert_eq!(
            t.fs.replace(&rejected, &payload).unwrap_err().kind(),
            io::ErrorKind::OutOfMemory
        );
        assert_eq!(t.fs.read(&p).unwrap(), b"kept");
        assert!(!t.fs.try_exists(&rejected).unwrap());
        t.settle();
        assert_eq!(native::read(&p).unwrap(), b"kept");
        assert!(!rejected.exists());
    }
}

#[test]
fn open_modes_reject_invalid_access_and_exclusive_recreation() {
    let t = Fixture::new();
    let p = t.path("file");
    assert!(OpenOptions::new().open_in(&t.fs, &p).is_err());
    assert!(
        OpenOptions::new()
            .read(true)
            .create(true)
            .open_in(&t.fs, &p)
            .is_err()
    );
    t.fs.write(&p, b"contents").unwrap();
    assert_eq!(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open_in(&t.fs, &p)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::AlreadyExists
    );
    let mut read = t.fs.open(&p).unwrap();
    assert!(read.write_all(b"bad").is_err());
    assert!(read.set_len(0).is_err());
    let mut write = OpenOptions::new().write(true).open_in(&t.fs, &p).unwrap();
    assert!(write.read(&mut [0]).is_err());
    assert_eq!(t.fs.read(&p).unwrap(), b"contents");
}

#[test]
#[cfg(unix)]
fn pending_work_retains_native_lease_until_recovery() {
    let t = Fixture::new();
    let lock_path = t.path("owner.lock");
    let lease =
        t.fs.acquire_lease(&lock_path, &t.root, LeaseMode::CreateNew)
            .unwrap();
    let guarded = t.fs.guarded(&lease).unwrap();
    assert!(Fs::new(Arc::new(DiskBackend)).guarded(&lease).is_err());
    t.faults.arm(Point::Open, libc::ENOSPC);
    guarded
        .replace(t.path("value"), b"owned pending bytes")
        .unwrap();
    drop(guarded);
    drop(lease);
    assert!(lock_path.exists());
    let competitor = Fs::new(Arc::new(DiskBackend));
    assert!(
        competitor
            .acquire_lease(&lock_path, &t.root, LeaseMode::ExistingOrNew)
            .is_err()
    );
    t.settle();
    assert_eq!(
        native::read(t.path("value")).unwrap(),
        b"owned pending bytes"
    );
    assert!(!lock_path.exists());
    let next = competitor
        .acquire_lease(&lock_path, &t.root, LeaseMode::CreateNew)
        .unwrap();
    drop(next);
    assert!(!lock_path.exists());
}

#[test]
#[cfg(unix)]
fn changed_lease_identity_blocks_replay_and_does_not_unlink_replacement() {
    let t = Fixture::new();
    let lock_path = t.path("owner.lock");
    let lease =
        t.fs.acquire_lease(&lock_path, &t.root, LeaseMode::CreateNew)
            .unwrap();
    let guarded = t.fs.guarded(&lease).unwrap();
    t.faults.arm(Point::Open, libc::ENOSPC);
    guarded
        .replace(t.path("value"), b"must not replay")
        .unwrap();
    native::rename(&lock_path, t.path("old.lock")).unwrap();
    native::write(&lock_path, b"different owner").unwrap();
    t.faults.clear();
    let r = t.fs.recover();
    assert!(r.remaining_operations > 0);
    assert!(r.blocked.is_some());
    assert!(t.fs.require_disk(t.path("value")).is_err());
    assert!(!t.path("value").exists());
    drop(guarded);
    drop(lease);
    assert_eq!(native::read(&lock_path).unwrap(), b"different owner");
}

#[test]
#[cfg(unix)]
fn open_handle_tracks_rename_but_not_deleted_path_recreation() {
    let t = Fixture::new();
    let a = t.path("a");
    let b = t.path("b");
    t.fs.write(&a, b"original").unwrap();
    let mut held = OpenOptions::new()
        .read(true)
        .write(true)
        .open_in(&t.fs, &a)
        .unwrap();
    t.faults.arm(Point::Rename, libc::ENOSPC);
    t.fs.rename(&a, &b).unwrap();
    held.seek(SeekFrom::Start(0)).unwrap();
    held.write_all(b"renamed!").unwrap();
    assert_eq!(t.fs.read(&b).unwrap(), b"renamed!");
    t.fs.remove_file(&b).unwrap();
    t.fs.write(&b, b"replacement").unwrap();
    held.seek(SeekFrom::Start(0)).unwrap();
    let mut old = Vec::new();
    held.read_to_end(&mut old).unwrap();
    assert_eq!(old, b"renamed!");
    held.seek(SeekFrom::Start(0)).unwrap();
    held.write_all(b"detached").unwrap();
    assert_eq!(t.fs.read(&b).unwrap(), b"replacement");
    drop(held);
    t.settle();
    assert!(!a.exists());
    assert_eq!(native::read(&b).unwrap(), b"replacement");
}

#[test]
#[cfg(unix)]
fn dropping_service_releases_pending_lease_without_claiming_durability() {
    let t = Fixture::new();
    let fs = Fs::new(Arc::new(Injected {
        disk: DiskBackend,
        faults: t.faults.clone(),
    }));
    let lock_path = t.path("owner.lock");
    let lease = fs
        .acquire_lease(&lock_path, &t.root, LeaseMode::CreateNew)
        .unwrap();
    let guarded = fs.guarded(&lease).unwrap();
    t.faults.arm(Point::Open, libc::EDQUOT);
    guarded.replace(t.path("pending"), b"memory only").unwrap();
    drop(lease);
    drop(guarded);
    assert!(lock_path.exists());
    drop(fs);
    assert!(!lock_path.exists());
    assert!(!t.path("pending").exists());
}

#[test]
#[cfg(unix)]
fn secure_beneath_checks_disk_links_even_for_memory_entries() {
    use std::os::unix::fs::symlink;
    let t = Fixture::new();
    native::create_dir(t.path("dir")).unwrap();
    native::write(t.path("dir/disk"), b"disk").unwrap();
    let mut f = t.fs.open_beneath(&t.root, "dir/disk").unwrap();
    let mut data = String::new();
    f.read_to_string(&mut data).unwrap();
    assert_eq!(data, "disk");
    t.faults.arm(Point::Open, libc::ENOSPC);
    t.fs.write(t.path("dir/memory"), b"memory").unwrap();
    assert!(t.fs.open_beneath(&t.root, "dir/memory").is_ok());
    native::rename(t.path("dir"), t.path("moved")).unwrap();
    symlink(t.path("moved"), t.path("dir")).unwrap();
    assert!(t.fs.open_beneath(&t.root, "dir/memory").is_err());
    assert!(t.fs.open_beneath(&t.root, "dir/disk").is_err());
    assert!(t.fs.open_beneath(&t.root, "../outside").is_err());
}

#[test]
#[cfg(unix)]
fn directory_sync_failure_keeps_namespace_and_retry_stages() {
    let t = Fixture::new();
    t.faults.arm(Point::DirectorySync, libc::ENOSPC);
    t.fs.create_dir(t.path("dir")).unwrap();
    assert!(t.fs.metadata(t.path("dir")).unwrap().is_dir());
    assert!(t.fs.recover().blocked.is_some());
    t.settle();
    t.faults.arm(Point::DirectorySync, libc::ENOSPC);
    t.fs.remove_dir(t.path("dir")).unwrap();
    assert!(!t.fs.try_exists(t.path("dir")).unwrap());
    assert!(t.fs.recover().blocked.is_some());
    t.settle();
    assert!(!t.path("dir").exists());
}

#[test]
#[cfg(unix)]
fn rejected_capacity_payload_does_not_publish_and_orphans_remain_budgeted() {
    let t = Fixture::new();
    let fs = Fs::with_budget(
        Arc::new(Injected {
            disk: DiskBackend,
            faults: t.faults.clone(),
        }),
        32,
        16,
    );
    fs.write(t.path("old"), b"12345678").unwrap();
    let held = fs.open(t.path("old")).unwrap();
    fs.remove_file(t.path("old")).unwrap();
    fs.recover();
    assert_eq!(fs.status().retained_bytes, 0); // healthy detached descriptor is streaming
    t.faults.arm(Point::Open, libc::ENOSPC);
    assert_eq!(
        fs.write(t.path("large"), [0; 20]).unwrap_err().kind(),
        io::ErrorKind::OutOfMemory
    );
    assert!(!fs.try_exists(t.path("large")).unwrap());
    assert!(fs.status().exhausted);
    drop(held);
    fs.recover();
    assert_eq!(fs.status().retained_bytes, 0);
}

#[test]
#[cfg(unix)]
fn noncapacity_partial_image_is_abandoned_not_replayed() {
    let t = Fixture::new();
    t.fs.write(t.path("value"), b"old").unwrap();
    t.faults.arm(Point::Write, libc::EIO);
    assert_eq!(
        t.fs.write(t.path("value"), b"not accepted")
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EIO)
    );
    assert_eq!(t.fs.read(t.path("value")).unwrap(), b"old");
    assert_eq!(t.fs.status().pending_operations, 0);
    t.settle();
    assert_eq!(native::read(t.path("value")).unwrap(), b"old");
    assert_eq!(names(&t.fs, &t.root), vec![OsString::from("value")]);
}

#[test]
#[cfg(unix)]
fn bounded_recovery_does_not_discard_later_work() {
    let t = Fixture::new();
    t.faults.arm(Point::Open, libc::ENOSPC);
    for index in 0..80 {
        t.fs.write(t.path(&format!("item-{index}")), [index as u8])
            .unwrap();
    }
    assert_eq!(t.fs.status().pending_operations, 80);
    t.faults.clear();
    let first = t.fs.recover();
    assert_eq!(first.completed_operations, 64);
    assert_eq!(first.remaining_operations, 16);
    assert!(first.blocked.is_none());
    let second = t.fs.recover();
    assert_eq!(second.completed_operations, 16);
    assert_eq!(second.remaining_operations, 0);
    assert_eq!(t.fs.recover().completed_operations, 0);
    for index in 0..80 {
        assert_eq!(
            native::read(t.path(&format!("item-{index}"))).unwrap(),
            [index as u8]
        );
    }
}

#[test]
#[cfg(unix)]
fn temporary_images_are_not_namespace_entries() {
    let t = Fixture::new();
    t.faults.arm(Point::Write, libc::ENOSPC);
    t.fs.write(t.path("visible"), b"intended complete contents")
        .unwrap();
    assert_eq!(names(&t.fs, &t.root), vec![OsString::from("visible")]);
    assert_eq!(
        t.fs.read(t.path("visible")).unwrap(),
        b"intended complete contents"
    );
    t.settle();
}

#[test]
fn healthy_large_files_and_directory_renames_do_not_use_fallback_budget() {
    let t = Fixture::new();
    native::create_dir(t.path("large-dir")).unwrap();
    let path = t.path("large-dir/file");
    let native_file = native::File::create(&path).unwrap();
    native_file.set_len(4 * 1024 * 1024).unwrap();
    drop(native_file);
    let fs = Fs::with_budget(Arc::new(DiskBackend), 16, 1);
    let mut old = fs.open(&path).unwrap();
    old.seek(SeekFrom::End(-1)).unwrap();
    assert_eq!(old.read(&mut [1]).unwrap(), 1);
    let secure = fs.open_beneath(t.path("large-dir"), "file").unwrap();
    assert_eq!(secure.metadata().unwrap().len(), 4 * 1024 * 1024);
    fs.rename(t.path("large-dir"), t.path("moved")).unwrap();
    let mut writer = OpenOptions::new()
        .append(true)
        .open_in(&fs, t.path("moved/file"))
        .unwrap();
    writer.write_all(b"tail").unwrap();
    assert_eq!(writer.metadata().unwrap().len(), 4 * 1024 * 1024 + 4);
    assert_eq!(fs.status().retained_bytes, 0);
    assert!(!fs.status().exhausted);
    fs.write(t.path("new-large"), [7u8; 8192]).unwrap();
    assert_eq!(fs.status().retained_bytes, 0);
}

#[test]
fn fresh_reads_do_not_use_clean_live_handle_cache() {
    let t = Fixture::new();
    t.fs.write(t.path("value"), b"old").unwrap();
    let mut held = t.fs.open(t.path("value")).unwrap();
    native::write(t.path("replacement"), b"new and longer").unwrap();
    native::rename(t.path("replacement"), t.path("value")).unwrap();
    assert_eq!(t.fs.read(t.path("value")).unwrap(), b"new and longer");
    let mut old = String::new();
    held.read_to_string(&mut old).unwrap();
    assert_eq!(old, "old");
    assert_eq!(t.fs.metadata(t.path("value")).unwrap().len(), 14);
}

#[test]
#[cfg(unix)]
fn dirty_lease_handoff_reopens_but_rejects_live_callers() {
    let t = Fixture::new();
    let lock_path = t.path("session.lock");
    let lease =
        t.fs.acquire_lease(&lock_path, &t.root, LeaseMode::CreateNew)
            .unwrap();
    let guarded = t.fs.guarded(&lease).unwrap();
    let mut writer = OpenOptions::new()
        .append(true)
        .create(true)
        .private(true)
        .open_in(&guarded, t.path("session"))
        .unwrap();
    writer.write_all(b"first\n").unwrap();
    t.faults.arm(Point::Open, libc::ENOSPC);
    writer.write_all(b"second\n").unwrap();
    assert_eq!(
        t.fs.acquire_lease(&lock_path, &t.root, LeaseMode::ExistingOrNew)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    drop(lease);
    drop(guarded);
    assert_eq!(
        t.fs.acquire_lease(&lock_path, &t.root, LeaseMode::ExistingOrNew)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    drop(writer);
    let resumed =
        t.fs.acquire_lease(&lock_path, &t.root, LeaseMode::ExistingOrNew)
            .unwrap();
    let guarded = t.fs.guarded(&resumed).unwrap();
    assert_eq!(guarded.read(t.path("session")).unwrap(), b"first\nsecond\n");
    let mut writer = OpenOptions::new()
        .append(true)
        .open_in(&guarded, t.path("session"))
        .unwrap();
    writer.write_all(b"third\n").unwrap();
    drop(writer);
    drop(guarded);
    drop(resumed);
    t.settle();
    assert_eq!(
        native::read(t.path("session")).unwrap(),
        b"first\nsecond\nthird\n"
    );
    assert!(!lock_path.exists());
}

#[test]
#[cfg(unix)]
fn large_native_baseline_accepts_small_bounded_delta() {
    let t = Fixture::new();
    let path = t.path("large");
    let file = native::File::create(&path).unwrap();
    file.set_len(1024 * 1024).unwrap();
    drop(file);
    let fs = Fs::with_budget(
        Arc::new(Injected {
            disk: DiskBackend,
            faults: t.faults.clone(),
        }),
        64,
        16,
    );
    let mut writer = OpenOptions::new().append(true).open_in(&fs, &path).unwrap();
    t.faults.arm(Point::Write, libc::ENOSPC);
    writer.write_all(b"delta").unwrap();
    drop(writer);
    assert!(fs.status().retained_bytes <= 64);
    let mut read = fs.open(&path).unwrap();
    read.seek(SeekFrom::End(-5)).unwrap();
    let mut tail = [0; 5];
    read.read_exact(&mut tail).unwrap();
    assert_eq!(&tail, b"delta");
    drop(read);
    t.faults.clear();
    fs.require_disk(&path).unwrap();
    assert_eq!(native::metadata(&path).unwrap().len(), 1024 * 1024 + 5);
}

#[test]
#[cfg(unix)]
fn rejected_delta_does_not_replay_or_advance_cursor() {
    let t = Fixture::new();
    t.fs.write(t.path("value"), b"old").unwrap();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open_in(&t.fs, t.path("value"))
        .unwrap();
    t.faults.arm(Point::Write, libc::EIO);
    assert!(file.write_all(b"rejected delta").is_err());
    assert_eq!(file.stream_position().unwrap(), 0);
    assert_eq!(t.fs.status().pending_operations, 0);
    assert_eq!(t.fs.read(t.path("value")).unwrap(), b"old");
    t.settle();
    assert_eq!(native::read(t.path("value")).unwrap(), b"old");
}

#[test]
#[cfg(unix)]
fn recovery_does_not_truncate_a_replaced_or_hardlinked_temp() {
    use std::os::unix::fs::PermissionsExt;
    for hardlink in [false, true] {
        let t = Fixture::new();
        t.faults.arm(Point::Write, libc::ENOSPC);
        t.fs.write(t.path("value"), b"accepted data longer than prefix")
            .unwrap();
        let temp = native::read_dir(&t.root)
            .unwrap()
            .map(Result::unwrap)
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".kit-resilient-")
            })
            .unwrap()
            .path();
        if hardlink {
            native::hard_link(&temp, t.path("linked")).unwrap();
        } else {
            native::remove_file(&temp).unwrap();
            native::write(&temp, b"other owner").unwrap();
            native::set_permissions(&temp, Permissions::from_mode(0o600)).unwrap();
        }
        let before = native::read(&temp).unwrap();
        t.faults.clear();
        assert_eq!(
            t.fs.recover().blocked.unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(native::read(&temp).unwrap(), before);
        assert_eq!(
            t.fs.read(t.path("value")).unwrap(),
            b"accepted data longer than prefix"
        );
    }
}

#[test]
#[cfg(unix)]
fn renamed_directory_redirects_follow_partially_completed_recovery() {
    let t = Fixture::new();
    native::create_dir(t.path("a")).unwrap();
    native::write(t.path("a/child"), b"unchanged baseline").unwrap();
    t.faults.arm(Point::Rename, libc::ENOSPC);
    t.fs.rename(t.path("a"), t.path("b")).unwrap();
    t.fs.rename(t.path("b"), t.path("c")).unwrap();
    assert_eq!(t.fs.read(t.path("c/child")).unwrap(), b"unchanged baseline");
    t.faults.arm(Point::DirectorySync, libc::ENOSPC);
    assert!(t.fs.recover().blocked.is_some());
    assert_eq!(t.fs.read(t.path("c/child")).unwrap(), b"unchanged baseline");
    t.settle();
    assert_eq!(
        native::read(t.path("c/child")).unwrap(),
        b"unchanged baseline"
    );
}

#[test]
#[cfg(unix)]
fn public_directory_entries_preserve_symlink_alias_spelling() {
    use std::os::unix::fs::symlink;
    let t = Fixture::new();
    native::create_dir(t.path("actual")).unwrap();
    native::write(t.path("actual/disk"), b"disk").unwrap();
    symlink(t.path("actual"), t.path("alias")).unwrap();
    // Use an alias in an ancestor, as /var -> /private/var on macOS does.
    native::create_dir(t.path("actual/listed")).unwrap();
    native::write(t.path("actual/listed/disk"), b"disk").unwrap();
    let requested = t.path("alias/listed");
    let entry = t.fs.read_dir(&requested).unwrap().next().unwrap().unwrap();
    assert_eq!(entry.path(), requested.join("disk"));
    assert_eq!(
        entry.path().strip_prefix(&requested).unwrap(),
        Path::new("disk")
    );
    assert!(entry.metadata().unwrap().is_file());
    t.faults.arm(Point::Open, libc::ENOSPC);
    t.fs.write(requested.join("memory"), b"memory").unwrap();
    for entry in t.fs.read_dir(&requested).unwrap() {
        assert!(entry.unwrap().path().starts_with(&requested));
    }
    t.settle();
}

#[test]
#[cfg(unix)]
fn clean_lost_lease_can_be_reacquired_while_old_observer_stays_fenced() {
    let t = Fixture::new();
    let path = t.path("lease.lock");
    let old =
        t.fs.acquire_lease(&path, &t.root, LeaseMode::CreateNew)
            .unwrap();
    let guarded = t.fs.guarded(&old).unwrap();
    guarded.write(t.path("value"), b"durable").unwrap();
    native::remove_file(&path).unwrap();
    assert_eq!(old.check().unwrap_err().kind(), io::ErrorKind::NotFound);
    assert_eq!(old.check().unwrap_err().kind(), io::ErrorKind::NotFound);
    let new =
        t.fs.acquire_lease(&path, &t.root, LeaseMode::CreateNew)
            .unwrap();
    new.check().unwrap();
    assert_eq!(
        old.check().unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(guarded.write(t.path("value"), b"forbidden").is_err());
    assert_eq!(t.fs.read(t.path("value")).unwrap(), b"durable");
    drop(old);
    drop(guarded);
    new.check().unwrap();
}

#[test]
#[cfg(unix)]
fn dirty_lost_lease_cannot_acquire_new_authority_for_old_pending_bytes() {
    let t = Fixture::new();
    let path = t.path("lease.lock");
    let lease =
        t.fs.acquire_lease(&path, &t.root, LeaseMode::CreateNew)
            .unwrap();
    let guarded = t.fs.guarded(&lease).unwrap();
    t.faults.arm(Point::Open, libc::ENOSPC);
    guarded
        .write(t.path("value"), b"must remain pending")
        .unwrap();
    drop(guarded);
    drop(lease);
    native::remove_file(&path).unwrap();
    t.faults.clear();
    assert!(
        t.fs.acquire_lease(&path, &t.root, LeaseMode::CreateNew)
            .is_err()
    );
    assert!(!path.exists());
    assert!(!t.path("value").exists());
    assert_eq!(t.fs.status().pending_operations, 1);
    assert_eq!(t.fs.read(t.path("value")).unwrap(), b"must remain pending");
}

#[test]
#[cfg(unix)]
fn private_directory_creation_tightens_only_existing_target() {
    use std::os::unix::fs::PermissionsExt;
    let t = Fixture::new();
    native::create_dir(t.path("parent")).unwrap();
    native::create_dir(t.path("parent/target")).unwrap();
    native::set_permissions(t.path("parent"), Permissions::from_mode(0o755)).unwrap();
    native::set_permissions(t.path("parent/target"), Permissions::from_mode(0o755)).unwrap();
    t.fs.create_private_dir_all(t.path("parent/target"))
        .unwrap();
    assert_eq!(
        native::metadata(t.path("parent"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        native::metadata(t.path("parent/target"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn metadata_identity_is_physical_or_stable_overlay_not_file_contents() {
    let t = Fixture::new();
    t.fs.write(t.path("one"), b"same").unwrap();
    t.fs.write(t.path("two"), b"same").unwrap();
    let one = t.fs.metadata(t.path("one")).unwrap();
    let one_file = t.fs.open(t.path("one")).unwrap().metadata().unwrap();
    let two = t.fs.metadata(t.path("two")).unwrap();
    assert!(one.same_identity(&one_file));
    assert!(!one.same_identity(&two));
    t.faults.arm(Point::Open, if cfg!(unix) { 28 } else { 112 });
    t.fs.write(t.path("pending"), b"memory").unwrap();
    let first = t.fs.metadata(t.path("pending")).unwrap();
    let second = t.fs.metadata(t.path("pending")).unwrap();
    assert!(first.same_identity(&second));
}

#[test]
fn allocator_failure_invokes_exit_hook_before_returning_an_error() {
    const CHILD: &str = "KIT_FS_ALLOCATOR_FAILURE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        set_allocation_failure_handler(|| std::process::exit(73)).unwrap();
        let _ = allocation_oom();
        panic!("allocator failure returned past the exit hook");
    }
    let module = module_path!().split_once("::").unwrap().1;
    let name = format!("{module}::allocator_failure_invokes_exit_hook_before_returning_an_error");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(73), "{output:?}");
}

#[test]
fn independent_append_handles_share_service_mutations() {
    let t = Fixture::new();
    native::write(t.path("value"), b"start").unwrap();
    let mut a = OpenOptions::new()
        .append(true)
        .open_in(&t.fs, t.path("value"))
        .unwrap();
    let mut b = OpenOptions::new()
        .append(true)
        .open_in(&t.fs, t.path("value"))
        .unwrap();
    a.write_all(b"A").unwrap();
    b.write_all(b"B").unwrap();
    assert_eq!(native::read(t.path("value")).unwrap(), b"startAB");
    t.fs.write(t.path("value"), b"ordinary").unwrap();
    b.write_all(b"B").unwrap();
    assert_eq!(native::read(t.path("value")).unwrap(), b"ordinaryB");
    let _truncated = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open_in(&t.fs, t.path("value"))
        .unwrap();
    a.write_all(b"after truncate").unwrap();
    assert_eq!(native::read(t.path("value")).unwrap(), b"after truncate");
    t.fs.replace(t.path("value"), b"replacement").unwrap();
    b.write_all(b"old inode").unwrap();
    assert_eq!(native::read(t.path("value")).unwrap(), b"replacement");
}

#[test]
fn handle_chmod_obeys_operation_budget() {
    let t = Fixture::budget(1024, 1);
    native::write(t.path("value"), b"start").unwrap();
    let mut file = OpenOptions::new()
        .append(true)
        .open_in(&t.fs, t.path("value"))
        .unwrap();
    let before = file.metadata().unwrap().permissions();
    t.faults.arm(Point::Open, libc::ENOSPC);
    file.write_all(b"pending").unwrap();
    let mut changed = before.clone();
    changed.set_readonly(true);
    for _ in 0..3 {
        assert_eq!(
            file.set_permissions(changed.clone()).unwrap_err().kind(),
            io::ErrorKind::OutOfMemory
        );
        assert_eq!(t.fs.status().pending_operations, 1);
        assert_eq!(file.metadata().unwrap().permissions(), before);
    }
    t.settle();
}

#[test]
fn retained_lease_handoff_with_pending_parent_sync() {
    let t = Fixture::new();
    let path = t.path("session.lock");
    let lease =
        t.fs.acquire_lease(&path, &t.root, LeaseMode::CreateNew)
            .unwrap();
    let guarded = t.fs.guarded(&lease).unwrap();
    t.faults.arm(Point::DirectorySync, libc::ENOSPC);
    guarded.write(t.path("value"), b"published").unwrap();
    guarded.sync_directory(&t.root).unwrap();
    assert!(t.fs.status().pending_operations > 0);
    assert_eq!(
        t.fs.acquire_lease(&path, &t.root, LeaseMode::ExistingOrNew)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    drop(guarded);
    drop(lease);
    let resumed =
        t.fs.acquire_lease(&path, &t.root, LeaseMode::ExistingOrNew)
            .unwrap();
    resumed.check().unwrap();
    t.settle();
}

#[cfg(unix)]
#[test]
fn ordinary_creation_honors_restrictive_umask() {
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = "KIT_FS_UMASK_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Only the isolated child changes umask, before running its sole test.
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        let test_name = std::thread::current().name().unwrap().to_owned();
        command.args(["--exact", &test_name, "--nocapture"]);
        command.env(CHILD, "1");
        unsafe {
            command.pre_exec(|| {
                libc::umask(0o077);
                Ok(())
            });
        }
        assert!(command.status().unwrap().success());
        return;
    }
    let t = Fixture::new();
    t.fs.write(t.path("write"), b"secret").unwrap();
    let _file = OpenOptions::new()
        .write(true)
        .create(true)
        .open_in(&t.fs, t.path("open"))
        .unwrap();
    for name in ["write", "open"] {
        assert_eq!(
            native::metadata(t.path(name)).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    native::set_permissions(t.path("write"), Permissions::from_mode(0o640)).unwrap();
    t.fs.write(t.path("write"), b"preserved").unwrap();
    assert_eq!(
        native::metadata(t.path("write"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    t.faults.arm(Point::Open, libc::ENOSPC);
    t.fs.write(t.path("fallback"), b"secret").unwrap();
    t.settle();
    assert_eq!(
        native::metadata(t.path("fallback"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn stale_handle_chmod_does_not_change_replacement() {
    use std::os::unix::fs::PermissionsExt;
    for external in [false, true] {
        let t = Fixture::new();
        let path = t.path("value");
        native::write(&path, b"old").unwrap();
        let file = OpenOptions::new().read(true).open_in(&t.fs, &path).unwrap();
        if external {
            native::write(t.path("new"), b"replacement").unwrap();
            native::rename(t.path("new"), &path).unwrap();
        } else {
            t.fs.replace(&path, b"replacement").unwrap();
        }
        native::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        file.set_permissions(Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            native::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[cfg(unix)]
#[test]
fn blocked_replacement_detaches_stale_handle_mutations() {
    use std::os::unix::fs::PermissionsExt;
    for replacement in [
        "replace",
        "private",
        "unlink",
        "rename",
        "replace-rename",
        "rename-replace",
    ] {
        for mutation in ["chmod", "write", "set_len"] {
            for dirty in [false, true] {
                let t = Fixture::new();
                let path = t.path("value");
                let moved = t.path("moved");
                native::write(&path, b"baseline").unwrap();
                native::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
                native::write(t.path("source"), b"replacement").unwrap();
                native::set_permissions(t.path("source"), Permissions::from_mode(0o640)).unwrap();
                let mut held = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open_in(&t.fs, &path)
                    .unwrap();
                t.faults.arm(Point::Rename, libc::ENOSPC);
                if dirty {
                    // Also cover a displaced object with an existing queued image.
                    t.fs.write(&path, b"baseline").unwrap();
                }
                let named = match replacement {
                    "replace" => {
                        t.fs.replace(&path, b"replacement").unwrap();
                        &path
                    }
                    "private" => {
                        t.fs.replace_private(&path, b"replacement").unwrap();
                        &path
                    }
                    "unlink" => {
                        t.faults.arm(Point::Remove, libc::ENOSPC);
                        t.fs.remove_file(&path).unwrap();
                        t.fs.write(&path, b"replacement").unwrap();
                        t.fs.set_permissions(&path, Permissions::from_mode(0o640))
                            .unwrap();
                        &path
                    }
                    "rename" => {
                        t.fs.rename(t.path("source"), &path).unwrap();
                        &path
                    }
                    "replace-rename" => {
                        t.fs.replace(&path, b"replacement").unwrap();
                        t.fs.rename(&path, &moved).unwrap();
                        &moved
                    }
                    "rename-replace" => {
                        t.fs.rename(&path, &moved).unwrap();
                        t.fs.replace(&moved, b"replacement").unwrap();
                        &moved
                    }
                    _ => unreachable!(),
                };
                let mode = if replacement == "private" {
                    0o600
                } else {
                    0o640
                };
                let pending = t.fs.status().pending_operations;
                assert!(pending > 0);
                match mutation {
                    "chmod" => held.set_permissions(Permissions::from_mode(0o400)).unwrap(),
                    "write" => held.write_all(b"stale").unwrap(),
                    "set_len" => held.set_len(3).unwrap(),
                    _ => unreachable!(),
                }
                assert_eq!(t.fs.status().pending_operations, pending);
                assert_eq!(native::read(&path).unwrap(), b"baseline");
                assert_eq!(
                    native::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o640
                );
                assert_eq!(t.fs.read(named).unwrap(), b"replacement");
                assert_eq!(
                    t.fs.metadata(named).unwrap().permissions().mode() & 0o777,
                    mode
                );
                held.rewind().unwrap();
                let mut old = Vec::new();
                held.read_to_end(&mut old).unwrap();
                assert_eq!(
                    old,
                    match mutation {
                        "write" => b"staleine".as_slice(),
                        "set_len" => b"bas".as_slice(),
                        _ => b"baseline".as_slice(),
                    }
                );
                assert_eq!(
                    held.metadata().unwrap().permissions().mode() & 0o777,
                    if mutation == "chmod" { 0o400 } else { 0o640 }
                );
                t.settle();
                assert_eq!(native::read(named).unwrap(), b"replacement");
                assert_eq!(
                    native::metadata(named).unwrap().permissions().mode() & 0o777,
                    mode
                );
                assert_eq!(t.fs.read(named).unwrap(), b"replacement");
                if named == &moved {
                    assert!(!t.fs.try_exists(&path).unwrap());
                    assert!(!path.exists());
                }
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn blocked_ordinary_write_and_truncate_keep_shared_handle_identity() {
    use std::os::unix::fs::PermissionsExt;
    let t = Fixture::new();
    let path = t.path("value");
    native::write(&path, b"baseline").unwrap();
    native::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
    let mut held = OpenOptions::new()
        .read(true)
        .write(true)
        .open_in(&t.fs, &path)
        .unwrap();
    t.faults.arm(Point::Rename, libc::ENOSPC);
    t.fs.write(&path, b"ordinary").unwrap();
    let mut bytes = Vec::new();
    held.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"ordinary");
    let truncated = t.fs.create(&path).unwrap();
    assert_eq!(held.metadata().unwrap().len(), 0);
    held.rewind().unwrap();
    held.write_all(b"shared").unwrap();
    assert_eq!(truncated.metadata().unwrap().len(), 6);
    held.set_permissions(Permissions::from_mode(0o600)).unwrap();
    assert_eq!(t.fs.read(&path).unwrap(), b"shared");
    assert_eq!(
        t.fs.metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    t.settle();
    assert_eq!(native::read(&path).unwrap(), b"shared");
    assert_eq!(
        native::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[cfg(unix)]
#[test]
fn pending_handle_metadata_and_zero_patch_reads_are_logical() {
    use std::os::unix::fs::PermissionsExt;
    let t = Fixture::new();
    let path = t.path("value");
    native::write(&path, b"baseline").unwrap();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open_in(&t.fs, &path)
        .unwrap();
    t.faults.arm(Point::Chmod, libc::ENOSPC);
    file.set_permissions(Permissions::from_mode(0o600)).unwrap();
    assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(
        t.fs.metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    t.settle();
    t.faults.arm(Point::Open, libc::ENOSPC);
    file.set_len(0).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 0);
    assert_eq!(file.seek(SeekFrom::End(0)).unwrap(), 0);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert!(bytes.is_empty());
    file.set_len(5).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 5);
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, [0; 5]);
    t.settle();
    assert_eq!(native::read(&path).unwrap(), [0; 5]);
}

#[cfg(unix)]
#[test]
fn permission_probe_retries_collisions_and_preserves_replacements() {
    let t = Fixture::new();
    t.faults.arm(Point::ProbeCollision, libc::EEXIST);
    assert_eq!(
        t.fs.write(t.path("value"), b"data").unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(t.fs.status().pending_operations, 0);
    assert_eq!(t.faults.1.load(Ordering::Relaxed), 8);
    t.faults.arm(Point::ProbeSwap, libc::EIO);
    assert_eq!(
        t.fs.write(t.path("value"), b"data").unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    let replacement = native::read_dir(&t.root)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "tmp"))
        .unwrap();
    assert_eq!(native::read(replacement).unwrap(), b"replacement");
}

#[cfg(unix)]
#[test]
fn capacity_during_permission_probe_cleanup_does_not_reject_write() {
    let t = Fixture::new();
    t.faults.arm(Point::Remove, libc::ENOSPC);
    t.fs.write(t.path("value"), b"data").unwrap();
    assert_eq!(native::read(t.path("value")).unwrap(), b"data");
    t.settle();
}

fn assert_poison(e: io::Error) {
    assert_eq!(e.kind(), io::ErrorKind::Other);
    assert!(e.get_ref().unwrap().is::<PoisonedState>());
}

fn assert_service_isolated(t: &Fixture) {
    let report = t.fs.recover();
    assert_eq!(report.completed_operations, 0);
    assert_eq!(report.remaining_operations, usize::MAX);
    assert_poison(report.blocked.unwrap());
    let status = t.fs.status();
    assert!(status.exhausted);
    assert_eq!(status.pending_operations, usize::MAX);
    assert_eq!(status.retained_bytes, usize::MAX);
    assert_poison(t.fs.require_disk(&t.root).unwrap_err());
    assert_poison(t.fs.write(t.path("must-not-appear"), b"no").unwrap_err());
    assert!(!t.path("must-not-appear").exists());
    assert!(t.fs.service.state.is_poisoned());
    // Poison is scoped to this service, not a process-wide recovery default.
    let other = Fixture::new();
    other.fs.write(other.path("healthy"), b"yes").unwrap();
    other.settle();
}

#[test]
fn backend_panic_after_disk_effect_is_not_replayed_or_hidden() {
    for point in [Point::AfterWrite, Point::AfterRename, Point::FileDrop] {
        let t = Fixture::new();
        let path = t.path("value");
        native::write(&path, b"old").unwrap();
        let held = t.fs.open(&path).unwrap();
        t.faults.arm_panic(point);
        let fs = t.fs.clone();
        let target = path.clone();
        assert!(
            std::thread::spawn(move || fs.write(target, b"replacement"))
                .join()
                .is_err()
        );
        let expected = if point == Point::AfterRename {
            b"replacement".as_slice()
        } else {
            b"old".as_slice()
        };
        assert_eq!(native::read(&path).unwrap(), expected);
        let before: Vec<_> = native::read_dir(&t.root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_service_isolated(&t);
        assert_poison(held.metadata().unwrap_err());
        assert_eq!(native::read(&path).unwrap(), expected);
        let after: Vec<_> = native::read_dir(&t.root)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(
            before, after,
            "recovery must not clean or replay unknown disk state"
        );
    }
}

#[test]
fn handle_backend_panics_fence_clones_and_service() {
    for point in [Point::Read, Point::Seek, Point::Metadata] {
        let t = Fixture::new();
        let path = t.path("value");
        native::write(&path, b"contents").unwrap();
        let mut held = t.fs.open(&path).unwrap();
        let mut worker = held.try_clone().unwrap();
        t.faults.arm_panic(point);
        assert!(
            std::thread::spawn(move || {
                if point == Point::Metadata {
                    worker.seek(SeekFrom::End(0)).unwrap();
                } else {
                    worker.read_exact(&mut [0; 2]).unwrap();
                }
            })
            .join()
            .is_err()
        );
        assert!(held.cursor.is_poisoned());
        {
            let object = held.object.lock();
            if point == Point::Metadata {
                assert!(object.is_err());
                drop(object);
                assert_poison(lock(&held.object).err().unwrap());
            } else {
                let object = object.unwrap();
                let Source::Native(native) = &object.image.source else {
                    panic!("expected native source")
                };
                assert!(native.is_poisoned());
                assert_poison(lock(native).err().unwrap());
            }
        }
        assert_poison(lock(&held.cursor).err().unwrap());
        assert_poison(held.read(&mut [0; 2]).unwrap_err());
        assert_poison(held.seek(SeekFrom::Start(0)).unwrap_err());
        assert_poison(held.sync_all().unwrap_err());
        assert_service_isolated(&t);
        assert_eq!(native::read(&path).unwrap(), b"contents");
    }
}

#[test]
fn lease_callback_panic_permanently_fences_authority() {
    let t = Fixture::new();
    let lease =
        t.fs.acquire_lease(t.path("lock"), &t.root, LeaseMode::CreateNew)
            .unwrap();
    let guarded = t.fs.guarded(&lease).unwrap();
    let authority = lease.inner.clone();
    t.faults.arm_panic(Point::LeaseCheck);
    assert!(
        std::thread::spawn(move || authority.check())
            .join()
            .is_err()
    );
    assert_poison(lease.check().unwrap_err());
    assert_poison(guarded.write(t.path("value"), b"no").unwrap_err());
    assert!(!t.path("value").exists());
    assert!(lease.inner.fenced.is_poisoned());
    assert!(!t.fs.service.state.is_poisoned());
    // A rejected authority check must not corrupt service accounting.
    assert_eq!(t.fs.status().pending_operations, 0);
}

#[test]
#[cfg(unix)]
fn failed_native_read_and_seek_do_not_commit_logical_cursor() {
    for point in [Point::Read, Point::Seek] {
        let t = Fixture::new();
        let path = t.path("value");
        native::write(&path, b"abcdef").unwrap();
        let mut file = t.fs.open(&path).unwrap();
        file.seek(SeekFrom::Start(2)).unwrap();
        t.faults.arm(point, libc::EIO);
        assert_eq!(
            file.read(&mut [0; 2]).unwrap_err().raw_os_error(),
            Some(libc::EIO)
        );
        assert!(!t.fs.service.state.is_poisoned());
        t.faults.clear();
        assert_eq!(file.stream_position().unwrap(), 2);
        let mut clone = file.try_clone().unwrap();
        let mut data = [0; 2];
        clone.read_exact(&mut data).unwrap();
        assert_eq!(&data, b"cd");
        assert_eq!(file.stream_position().unwrap(), 4);
        t.settle();
    }
}

#[test]
#[cfg(unix)]
fn panic_during_pending_publication_blocks_further_recovery() {
    let t = Fixture::new();
    let path = t.path("value");
    native::write(&path, b"old").unwrap();
    t.faults.arm(Point::Open, libc::ENOSPC);
    t.fs.write(&path, b"queued").unwrap();
    assert_eq!(t.fs.status().pending_operations, 1);
    assert_eq!(native::read(&path).unwrap(), b"old");
    t.faults.clear();
    t.faults.arm_panic(Point::AfterRename);
    let fs = t.fs.clone();
    assert!(std::thread::spawn(move || fs.recover()).join().is_err());
    // Native publication happened, but the stage and completion count were not
    // committed. Replaying this action or claiming durability would be unsound.
    assert_eq!(native::read(&path).unwrap(), b"queued");
    assert_service_isolated(&t);
    assert_eq!(native::read(&path).unwrap(), b"queued");
}
