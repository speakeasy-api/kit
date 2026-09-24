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
    rename_stall: Option<String>,
}
impl StalledDisk {
    fn stall_rename(&self, operation: &str) -> io::Result<()> {
        if self.rename_stall.as_deref() == Some(operation) {
            std::fs::write(&self.entered, operation)?;
            let release = self.entered.with_file_name("release");
            while !release.exists() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    }
}

impl Drop for StalledDisk {
    fn drop(&mut self) {
        if self.rename_stall.is_some() {
            // The isolated worker has relinquished its last backend reference.
            std::fs::write(self.entered.with_file_name("backend-dropped"), b"done").unwrap();
        }
    }
}

impl Backend for StalledDisk {
    fn read_dir(&self, path: &Path) -> io::Result<Vec<DiskEntry>> {
        if self.canonicalize || self.rename_stall.is_some() {
            return DiskBackend.read_dir(path);
        }
        std::fs::write(&self.entered, b"reading")?;
        loop {
            std::thread::park();
        }
    }
    fn open(&self, path: &Path, options: &DiskOpenOptions) -> io::Result<Box<dyn BackendFile>> {
        if options.write || options.append {
            self.stall_rename("write-open")?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            self.stall_rename("authority")?;
        }
        DiskBackend.open(path, options)
    }
    fn metadata(&self, path: &Path, follow: bool) -> io::Result<std::fs::Metadata> {
        if path.to_string_lossy().ends_with(".metadata.json") {
            self.stall_rename("metadata")?;
        }
        DiskBackend.metadata(path, follow)
    }
    fn read_link(&self, path: &Path) -> io::Result<PathBuf> {
        DiskBackend.read_link(path)
    }
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.stall_rename("canonicalize")?;
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
        self.stall_rename("commit")?;
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
        if relative
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            self.stall_rename("authority")?;
        }
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
                rename_stall: None,
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
    // Exercise every signal on one stall, and each other stall with SIGINT.
    for (signal, canonicalize) in [
        ("-INT", false),
        ("-TERM", false),
        ("-HUP", false),
        ("-INT", true),
    ] {
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

// Each subprocess owns HOME and global recovery state. Never mutate HOME in the
// test runner: other session tests can be creating transcripts concurrently.
fn rename_fixture(directory: &Path) -> PathBuf {
    let root = directory.join("workspace");
    std::fs::create_dir(&root).unwrap();
    drop(
        crate::session::open(
            &root,
            "rename-target",
            false,
            false,
            vec![agentkit_core::Item::text(
                agentkit_core::ItemKind::System,
                "startup rename fixture",
            )],
        )
        .unwrap(),
    );
    crate::session::set_display_name(&root, "rename-target", Some("original")).unwrap();
    assert_rename_title(&root, "original");
    root
}

fn assert_rename_title(root: &Path, expected: &str) {
    // A new namespace observes disk rather than a successful in-memory overlay.
    let filesystem = fs::Fs::new(Arc::new(DiskBackend));
    let entries = crate::session::catalog_with(&filesystem, root).unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.id == "rename-target")
        .unwrap();
    assert_eq!(entry.title.as_deref(), Some(expected));
}

fn wait_for_marker(path: &Path, child: &mut Reap) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "child exited before {}",
            path.display()
        );
        assert!(
            Instant::now() < deadline,
            "child did not reach {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_rename_child(child: &mut Reap) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success(), "rename child failed: {status}");
            return;
        }
        assert!(
            Instant::now() < deadline,
            "rename held recovery/runtime teardown"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn rename_child(test: &str, directory: &Path, operation: &str) -> Reap {
    Reap(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env("KIT_RENAME_TEARDOWN_CHILD", directory)
            .env("KIT_RENAME_STALL", operation)
            .env("HOME", directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn cancelled_rename_does_not_hold_recovery_or_write_after_shutdown() {
    if let Some(directory) = std::env::var_os("KIT_RENAME_TEARDOWN_CHILD") {
        let directory = PathBuf::from(directory);
        let root = rename_fixture(&directory);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            fs::start_recovery_worker();
            let mut stop = super::Stop::new().unwrap();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let filesystem = fs::Fs::new(Arc::new(StalledDisk {
                entered: directory.join("entered"),
                canonicalize: false,
                rename_stall: Some(std::env::var("KIT_RENAME_STALL").unwrap()),
            }));
            super::start_rename_preparation(
                root.clone(),
                "rename-target".into(),
                Some("changed".into()),
                filesystem.clone(),
                tx,
            )
            .unwrap();
            assert!(
                stop.until(rx.recv()).await.is_none(),
                "preparation completed before cancellation"
            );
            drop(rx);
            // Recovery must finish while preparation is still inside the backend.
            fs::finish_best_effort_recovery(&filesystem);
            fs::finish_best_effort_recovery(fs::best_effort_global());
            fs::finish_recovery(fs::global()).unwrap();
        });
        drop(runtime);
        std::fs::write(directory.join("recovered"), b"finished").unwrap();
        // Runtime teardown also cancels the forwarding task. Let the detached
        // worker finish, so absence of a late write is not a timing assertion.
        std::fs::write(directory.join("release"), b"go").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !directory.join("backend-dropped").exists() {
            assert!(
                Instant::now() < deadline,
                "released preparation did not finish"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_rename_title(&root, "original");
        return;
    }
    for operation in ["canonicalize", "authority", "metadata", "write-open"] {
        let directory = tempfile::tempdir().unwrap();
        let mut child = rename_child(
            "tui::startup::tests::cancelled_rename_does_not_hold_recovery_or_write_after_shutdown",
            directory.path(),
            operation,
        );
        wait_for_marker(&directory.path().join("entered"), &mut child);
        assert!(
            Command::new("kill")
                .args(["-INT", &child.0.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
        wait_for_rename_child(&mut child);
        assert_eq!(
            std::fs::read(directory.path().join("recovered")).unwrap(),
            b"finished"
        );
    }
}

#[test]
fn admitted_rename_is_drained_before_recovery() {
    if let Some(directory) = std::env::var_os("KIT_RENAME_TEARDOWN_CHILD") {
        let directory = PathBuf::from(directory);
        let root = rename_fixture(&directory);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            // Observe successful preparation and completion in this same child,
            // then exercise shutdown with the picker's receiver gone.
            let filesystem = fs::Fs::new(Arc::new(DiskBackend));
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            super::start_rename_preparation(
                root.clone(),
                "rename-target".into(),
                Some("  changed  ".into()),
                filesystem.clone(),
                tx,
            )
            .unwrap();
            let super::PickerUpdate::RenamePrepared {
                session_id,
                display_name,
                result,
            } = rx.recv().await.unwrap()
            else {
                panic!("expected prepared rename");
            };
            assert_eq!(session_id, "rename-target");
            assert_eq!(display_name.as_deref(), Some("changed"));
            let prepared = result.unwrap();
            assert_rename_title(&root, "original");
            let (updates, mut completed) = tokio::sync::mpsc::unbounded_channel();
            let mut commits = super::RenameCommits::default();
            commits.admit(prepared, session_id, display_name, updates);
            commits.drain().await.unwrap();
            let super::PickerUpdate::Session(super::Update::SessionRenamed { result, .. }) =
                completed.recv().await.unwrap()
            else {
                panic!("expected completed rename");
            };
            assert_eq!(result.unwrap().as_deref(), Some("changed"));
            fs::finish_best_effort_recovery(&filesystem);
            assert_rename_title(&root, "changed");
            let mut stop = super::Stop::new().unwrap();
            let filesystem = fs::Fs::new(Arc::new(StalledDisk {
                entered: directory.join("entered"),
                canonicalize: false,
                rename_stall: Some("commit".into()),
            }));
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            super::start_rename_preparation(
                root.clone(),
                "rename-target".into(),
                Some("drained".into()),
                filesystem.clone(),
                tx.clone(),
            )
            .unwrap();
            let super::PickerUpdate::RenamePrepared {
                session_id,
                display_name,
                result,
            } = rx.recv().await.unwrap()
            else {
                panic!("expected prepared rename");
            };
            let mut commits = super::RenameCommits::default();
            commits.admit(result.unwrap(), session_id, display_name, tx);
            // The parent sends a real signal only after the admitted write stalls.
            assert!(stop.until(std::future::pending::<()>()).await.is_none());
            drop(rx);
            let mut drain = std::pin::pin!(commits.drain());
            assert!(
                futures_util::poll!(drain.as_mut()).is_pending(),
                "admitted write escaped its owner"
            );
            std::fs::write(directory.join("draining"), b"waiting").unwrap();
            drain.await.unwrap();
            fs::finish_best_effort_recovery(&filesystem);
            fs::finish_best_effort_recovery(fs::best_effort_global());
            fs::finish_recovery(fs::global()).unwrap();
            assert_rename_title(&root, "drained");
        });
        drop(runtime);
        std::fs::write(directory.join("recovered"), b"finished").unwrap();
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut child = rename_child(
        "tui::startup::tests::admitted_rename_is_drained_before_recovery",
        directory.path(),
        "commit",
    );
    wait_for_marker(&directory.path().join("entered"), &mut child);
    assert!(
        Command::new("kill")
            .args(["-INT", &child.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    wait_for_marker(&directory.path().join("draining"), &mut child);
    assert!(!directory.path().join("recovered").exists());
    std::fs::write(directory.path().join("release"), b"go").unwrap();
    wait_for_rename_child(&mut child);
    assert_eq!(
        std::fs::read(directory.path().join("recovered")).unwrap(),
        b"finished"
    );
}
