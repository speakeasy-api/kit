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
}
#[derive(Default)]
struct Faults(Mutex<Option<(Point, i32)>>);
impl Faults {
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
        self.disk.read(b)
    }
}
impl Seek for InjectedFile {
    fn seek(&mut self, p: SeekFrom) -> io::Result<u64> {
        self.disk.seek(p)
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
        self.disk.write(b)
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
        self.disk.metadata()
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
impl Backend for Injected {
    fn identity(&self, path: &Path, follow: bool) -> io::Result<Option<FileIdentity>> {
        self.disk.identity(path, follow)
    }
    fn open(&self, p: &Path, o: &DiskOpenOptions) -> io::Result<Box<dyn BackendFile>> {
        if o.write || o.append {
            self.faults.check(Point::Open)?;
        }
        Ok(Box::new(InjectedFile {
            disk: self.disk.open(p, o)?,
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
        self.disk.remove_file(p)
    }
    fn remove_dir(&self, p: &Path) -> io::Result<()> {
        self.disk.remove_dir(p)
    }
    fn rename(&self, a: &Path, b: &Path) -> io::Result<()> {
        self.faults.check(Point::Rename)?;
        self.disk.rename(a, b)
    }
    fn set_permissions(&self, p: &Path, mode: Permissions) -> io::Result<()> {
        self.disk.set_permissions(p, mode)
    }
    fn sync_directory(&self, p: &Path) -> io::Result<()> {
        self.faults.check(Point::DirectorySync)?;
        self.disk.sync_directory(p)
    }
    fn acquire_lease(&self, r: &LeaseRequest) -> io::Result<Box<dyn BackendLease>> {
        self.disk.acquire_lease(r)
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
