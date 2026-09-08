//! Faults at the existing filesystem backend boundary, isolated per process.
use super::*;
use crate::resilient_fs::{
    Backend, BackendFile, BackendLease, DiskBackend, DiskEntry, DiskOpenOptions, FileIdentity, Fs,
    LeaseRequest,
};
use std::{
    io::{self, Read, Seek, SeekFrom, Write},
    sync::Arc,
};

const MANIFEST_ENV: &str = "KIT_MANAGED_FILES_FAULT_TEST_MANIFEST";

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum Mode {
    Write,
    Sync,
    NoSpace,
}
impl Mode {
    fn expected_error(self) -> String {
        match self {
            Self::Write => "injected managed object write failure".into(),
            Self::Sync => "injected managed object sync failure".into(),
            Self::NoSpace => io::Error::from_raw_os_error(libc::ENOSPC).to_string(),
        }
    }
}

struct FaultBackend {
    mode: Mode,
    object_directory: PathBuf,
}
struct FaultFile {
    disk: Box<dyn BackendFile>,
    mode: Mode,
}

// No mutable fault switches or counters: every process owns one fixed policy.
impl Read for FaultFile {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.disk.read(bytes)
    }
}
impl Seek for FaultFile {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.disk.seek(from)
    }
}
impl Write for FaultFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self.mode {
            Mode::Write => Err(io::Error::other(self.mode.expected_error())),
            Mode::NoSpace => Err(io::Error::from_raw_os_error(libc::ENOSPC)),
            Mode::Sync => self.disk.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.disk.flush()
    }
}
impl FaultFile {
    fn check_sync(&self) -> io::Result<()> {
        // Permit empty-object creation; fail durability only after native data
        // exists. Directory syncs and all non-object files remain real.
        if matches!(self.mode, Mode::Sync) && self.disk.metadata()?.len() > 0 {
            return Err(io::Error::other(self.mode.expected_error()));
        }
        Ok(())
    }
}
impl BackendFile for FaultFile {
    fn metadata(&self) -> io::Result<disk::Metadata> {
        self.disk.metadata()
    }
    fn identity(&self) -> io::Result<Option<FileIdentity>> {
        self.disk.identity()
    }
    fn set_len(&self, size: u64) -> io::Result<()> {
        self.disk.set_len(size)
    }
    fn sync_data(&self) -> io::Result<()> {
        self.check_sync()?;
        self.disk.sync_data()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.check_sync()?;
        self.disk.sync_all()
    }
    fn set_permissions(&self, p: disk::Permissions) -> io::Result<()> {
        self.disk.set_permissions(p)
    }
}
impl FaultBackend {
    fn wrap(&self, path: &Path, disk: Box<dyn BackendFile>) -> Box<dyn BackendFile> {
        // Fs persists objects through sibling atomic-replacement files, not by
        // writing the final file_ basename. Scope faults to the store so private
        // import staging is covered without faulting directory work.
        if path.starts_with(&self.object_directory) {
            Box::new(FaultFile {
                disk,
                mode: self.mode,
            })
        } else {
            disk
        }
    }
}
impl Backend for FaultBackend {
    fn open(&self, path: &Path, options: &DiskOpenOptions) -> io::Result<Box<dyn BackendFile>> {
        Ok(self.wrap(path, DiskBackend.open(path, options)?))
    }
    fn metadata(&self, path: &Path, follow: bool) -> io::Result<disk::Metadata> {
        DiskBackend.metadata(path, follow)
    }
    fn identity(&self, path: &Path, follow: bool) -> io::Result<Option<FileIdentity>> {
        DiskBackend.identity(path, follow)
    }
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DiskEntry>> {
        DiskBackend.read_dir(path)
    }
    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        DiskBackend.read_link(path)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        DiskBackend.canonicalize(path)
    }
    fn create_dir(&self, path: &Path, private: bool) -> io::Result<()> {
        DiskBackend.create_dir(path, private)
    }
    fn remove_file(&self, path: &Path) -> io::Result<()> {
        DiskBackend.remove_file(path)
    }
    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        DiskBackend.remove_dir(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        DiskBackend.rename(from, to)
    }
    fn set_permissions(&self, path: &Path, p: disk::Permissions) -> io::Result<()> {
        DiskBackend.set_permissions(path, p)
    }
    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        DiskBackend.sync_directory(path)
    }
    fn acquire_lease(&self, request: &LeaseRequest) -> io::Result<Box<dyn BackendLease>> {
        DiskBackend.acquire_lease(request)
    }
    fn open_beneath(&self, root: &Path, relative: &Path) -> io::Result<Box<dyn BackendFile>> {
        Ok(self.wrap(
            &root.join(relative),
            DiskBackend.open_beneath(root, relative)?,
        ))
    }
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    base: PathBuf,
    source: PathBuf,
    mode: Mode,
    parent_pid: u32,
}

#[test]
fn write_sync_and_pending_recovery_fail_without_publishing_a_descriptor() {
    for mode in [Mode::Write, Mode::Sync, Mode::NoSpace] {
        let f = Fixture::new();
        let source = f.source("fault.png", ImageFormat::Png, 3, 2);
        let manifest = Manifest {
            base: f.store.base.clone(),
            source,
            mode,
            parent_pid: std::process::id(),
        };
        let path = f.dir.path().join("fault-manifest.json");
        disk::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "managed_files::tests::faults::fault_child",
                "--ignored",
                "--nocapture",
            ])
            .env(MANIFEST_ENV, &path)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{mode:?} child failed: {}\n{stdout}\n{stderr}",
            output.status
        );
        assert!(
            stdout.contains("managed_files::tests::faults::fault_child ... ok"),
            "{stdout}"
        );
        assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
        let error: String =
            serde_json::from_slice(&disk::read(path.with_extension("result.json")).unwrap())
                .unwrap();
        assert_eq!(error, mode.expected_error());
        // With the faulting process gone, future real imports and inheritance
        // work without investigating or deleting anything from source authority.
        let reference = f.import("recovered.png");
        f.store
            .prepare_inheritance("session", "recovered-fork")
            .unwrap()
            .unwrap()
            .commit();
        assert_eq!(
            f.store.resolve("recovered-fork", &reference).unwrap(),
            f.store.resolve("session", &reference).unwrap()
        );
    }
}

#[test]
#[ignore = "invoked by the parent in an isolated process with one immutable fault mode"]
fn fault_child() {
    let path = PathBuf::from(std::env::var_os(MANIFEST_ENV).expect("fault manifest path"));
    let manifest: Manifest = serde_json::from_slice(&disk::read(&path).unwrap()).unwrap();
    assert_ne!(std::process::id(), manifest.parent_pid);
    let store = FileStore {
        base: manifest.base,
    };
    assert!(
        fs::initialize_global(Fs::new(Arc::new(FaultBackend {
            mode: manifest.mode,
            object_directory: store.base.clone(),
        })))
        .is_ok(),
        "filesystem must be initialized only in this child"
    );
    // Every injected failure remains an error, including ENOSPC memory fallback.
    // Failed envelopes must never enter the enumerable inherited authority set.
    let error = store.import("session", &manifest.source, None).unwrap_err();
    assert_eq!(error, manifest.mode.expected_error());
    let directory = store.session_directory("session");
    assert_eq!(disk::read_dir(&directory).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
    match manifest.mode {
        Mode::Write | Mode::Sync => {
            assert!(disk::read_dir(&store.base).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".import-")
            }));
            store
                .prepare_inheritance("session", "fork-after-failure")
                .unwrap()
                .unwrap()
                .commit();
        }
        Mode::NoSpace => {
            // Recovery can also block best-effort cleanup, but only private
            // staging can retain the memory-backed envelope, never authority.
            let object = disk::read_dir(&store.base)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".import-")
                })
                .unwrap();
            // Cleanup hides the volatile object immediately, but its queued
            // unlink cannot reach disk while the earlier write still has ENOSPC.
            assert_eq!(
                fs::read(&object).unwrap_err().kind(),
                io::ErrorKind::NotFound
            );
            assert!(disk::read(&object).unwrap().is_empty());
            assert!(fs::global().status().pending_operations > 0);
            assert_eq!(
                fs::require_disk(&object).unwrap_err().raw_os_error(),
                Some(libc::ENOSPC)
            );
        }
    }
    disk::write(
        path.with_extension("result.json"),
        serde_json::to_vec(&error).unwrap(),
    )
    .unwrap();
}

#[test]
fn existing_grant_succeeds_when_new_object_writes_have_no_space() {
    let f = Fixture::new();
    let reference = f.import("retry.png");
    f.store
        .grant_to("session", &reference, &f.store, "parent", None)
        .unwrap();
    let manifest = f.dir.path().join("grant-retry.json");
    disk::write(
        &manifest,
        serde_json::to_vec(&(f.store.base.clone(), reference)).unwrap(),
    )
    .unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "managed_files::tests::faults::grant_retry_child",
            "--ignored",
            "--nocapture",
        ])
        .env(MANIFEST_ENV, &manifest)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("1 passed; 0 failed"), "{stdout}");
}

#[test]
#[ignore = "invoked by parent with immutable ENOSPC backend"]
fn grant_retry_child() {
    let path = PathBuf::from(std::env::var_os(MANIFEST_ENV).unwrap());
    let (base, reference): (PathBuf, FileReference) =
        serde_json::from_slice(&disk::read(path).unwrap()).unwrap();
    let store = FileStore { base };
    assert!(
        fs::initialize_global(Fs::new(Arc::new(FaultBackend {
            mode: Mode::NoSpace,
            object_directory: store.base.clone(),
        })))
        .is_ok()
    );
    let granted = store
        .grant_to("session", &reference, &store, "parent", None)
        .unwrap();
    assert_eq!(granted, reference);
    assert_eq!(
        store.resolve("parent", &reference).unwrap(),
        store.resolve("session", &reference).unwrap()
    );
    let controller = agentkit_core::CancellationController::new();
    let cancellation = controller.handle().checkpoint();
    controller.interrupt();
    assert!(
        store
            .grant_to("session", &reference, &store, "parent", Some(&cancellation))
            .unwrap_err()
            .contains("cancelled")
    );
    // A genuinely new grant hits real filesystem write-back/durability failure.
    assert!(
        store
            .grant_to("session", &reference, &store, "new", None)
            .is_err()
    );
    assert!(store.resolve("new", &reference).is_err());
    // Existing corrupt bytes must fail, even though retry is allocation-free.
    disk::write(
        store.session_directory("parent").join(&reference.id),
        b"corrupt",
    )
    .unwrap();
    assert!(
        store
            .grant_to("session", &reference, &store, "parent", None)
            .is_err()
    );
}
