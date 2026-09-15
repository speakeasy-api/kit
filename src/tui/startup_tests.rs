//! Exercise cancellation through actual signals, recovery, and runtime teardown
//! in a separate process. The filesystem boundary models a permanently stuck read.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]

use crate::resilient_fs::{
    self as fs, Backend, BackendFile, BackendLease, DiskBackend, DiskEntry, DiskOpenOptions,
    LeaseRequest,
};
use std::{
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

struct StalledDisk {
    entered: PathBuf,
    canonicalize: bool,
}
impl Backend for StalledDisk {
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DiskEntry>> {
        if self.canonicalize {
            return DiskBackend.read_dir(path);
        }
        std::fs::write(&self.entered, b"reading")?;
        loop {
            std::thread::park();
        }
    }
    fn open(&self, path: &Path, options: &DiskOpenOptions) -> io::Result<Box<dyn BackendFile>> {
        DiskBackend.open(path, options)
    }
    fn metadata(&self, path: &Path, follow: bool) -> io::Result<std::fs::Metadata> {
        DiskBackend.metadata(path, follow)
    }
    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        DiskBackend.read_link(path)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        if self.canonicalize {
            std::fs::write(&self.entered, b"canonicalizing")?;
            loop {
                std::thread::park();
            }
        }
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
    fn set_permissions(&self, path: &Path, permissions: std::fs::Permissions) -> io::Result<()> {
        DiskBackend.set_permissions(path, permissions)
    }
    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        DiskBackend.sync_directory(path)
    }
    fn acquire_lease(&self, request: &LeaseRequest) -> io::Result<Box<dyn BackendLease>> {
        DiskBackend.acquire_lease(request)
    }
    fn open_beneath(&self, root: &Path, relative: &Path) -> io::Result<Box<dyn BackendFile>> {
        DiskBackend.open_beneath(root, relative)
    }
}

struct Reap(Child);
impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn cancelled_catalog_does_not_hold_runtime_teardown() {
    const CHILD: &str = "KIT_CATALOG_TEARDOWN_CHILD";
    if let Some(directory) = std::env::var_os(CHILD) {
        let directory = PathBuf::from(directory);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            fs::start_recovery_worker();
            let backend = Arc::new(StalledDisk {
                entered: directory.join("entered"),
                canonicalize: std::env::var_os("KIT_STALL_CANONICALIZE").is_some(),
            });
            let mut stop = super::Stop::new().unwrap();
            assert!(
                super::scan_catalog(directory.clone(), backend, &mut stop)
                    .await
                    .unwrap()
                    .is_none()
            );
            // Keep precisely the storage finish sequence used by CLI main.
            fs::finish_best_effort_recovery(fs::best_effort_global());
            fs::finish_recovery(fs::global()).unwrap();
            std::fs::write(directory.join("recovered"), b"finished").unwrap();
        });
        drop(runtime);
        return;
    }
    for (signal, canonicalize) in [false, true]
        .into_iter()
        .flat_map(|canonicalize| ["-INT", "-TERM", "-HUP"].map(|signal| (signal, canonicalize)))
    {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join(".kit/sessions")).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.env_remove("KIT_STALL_CANONICALIZE");
        if canonicalize {
            command.env("KIT_STALL_CANONICALIZE", "1");
        }
        let mut child = Reap(
            command
                .args([
                    "--exact",
                    "tui::startup::tests::cancelled_catalog_does_not_hold_runtime_teardown",
                    "--nocapture",
                ])
                .env(CHILD, directory.path())
                .env("HOME", directory.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !directory.path().join("entered").exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited before scanning"
            );
            assert!(
                Instant::now() < deadline,
                "child did not enter catalog read"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            Command::new("kill")
                .args([signal, &child.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "cancelled scan held recovery/runtime teardown"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "child failed: {status}");
        assert_eq!(
            std::fs::read(directory.path().join("recovered")).unwrap(),
            b"finished"
        );
    }
}

#[tokio::test]
async fn catalog_returns_canonical_workspace_root() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(&workspace, &alias).unwrap();
    let backend = Arc::new(DiskBackend);
    let mut stop = super::Stop::new().unwrap();
    let (root, entries) = super::scan_catalog(alias, backend, &mut stop)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(root, workspace.canonicalize().unwrap());
    assert!(entries.is_empty());
}

#[tokio::test]
async fn catalog_invalid_root_preserves_diagnostic() {
    let directory = tempfile::tempdir().unwrap();
    for root in [directory.path().join("missing"), PathBuf::new()] {
        let expected = format!("{}: {}", root.display(), root.canonicalize().unwrap_err());
        let backend = Arc::new(DiskBackend);
        let mut stop = super::Stop::new().unwrap();
        let error = super::scan_catalog(root, backend, &mut stop)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
}
