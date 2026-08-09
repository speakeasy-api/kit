#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use kit::workspace::revision::{
    EntryKind, FileReadRange, LimitKind, ManagedWorkspace, RevisionError, RevisionOptions,
    SnapshotEntry,
};

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-revision-test-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let workspace = root.join("workspace");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&workspace).unwrap();
        Self { root, workspace }
    }

    fn options(&self) -> RevisionOptions {
        RevisionOptions {
            max_entries: 10_000,
            max_name_bytes: 1024 * 1024,
            max_bytes: 64 * 1024 * 1024,
            max_memory_bytes: 128 * 1024 * 1024,
            max_depth: 64,
            max_scan_time: Duration::from_secs(5),
            max_scan_attempts: 2,
            watcher_interval: Duration::from_millis(5),
            reconciliation_interval: Duration::from_millis(30),
            metadata_path: Some(self.root.join("revision.state")),
        }
    }

    fn open(&self) -> ManagedWorkspace {
        ManagedWorkspace::open_with_options(&self.workspace, self.options()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn content_revision_tracks_bytes_paths_types_modes_and_empty_directories() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("file"), b"one").unwrap();
    let workspace = fixture.open();
    let first = workspace.current_revision().unwrap();

    fs::write(fixture.workspace.join("file"), b"two").unwrap();
    let second = workspace.reconcile().unwrap();
    assert_ne!(second.id(), first.id());
    assert_ne!(second.digest(), first.digest());

    fs::set_permissions(
        fixture.workspace.join("file"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let executable = workspace.reconcile().unwrap();
    assert_ne!(executable.id(), second.id());
    assert_ne!(executable.digest(), second.digest());

    fs::create_dir(fixture.workspace.join("empty")).unwrap();
    let directory = workspace.reconcile().unwrap();
    assert_ne!(directory.id(), executable.id());
    let snapshot = workspace.snapshot(directory.id()).unwrap();
    assert!(
        snapshot.entries().iter().any(|entry| {
            entry.path == Path::new("empty") && entry.kind == EntryKind::Directory
        })
    );

    fs::rename(
        fixture.workspace.join("file"),
        fixture.workspace.join("renamed"),
    )
    .unwrap();
    let renamed = workspace.reconcile().unwrap();
    assert_ne!(renamed.digest(), directory.digest());
    fs::remove_file(fixture.workspace.join("renamed")).unwrap();
    fs::remove_dir(fixture.workspace.join("empty")).unwrap();
    let deleted = workspace.reconcile().unwrap();
    assert_ne!(deleted.id(), renamed.id());
}

#[test]
fn no_op_reconciliation_keeps_revision_and_digest_stable() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("file"), b"same").unwrap();
    let workspace = fixture.open();
    let first = workspace.current_revision().unwrap();
    workspace.mark_dirty();
    let second = workspace.reconcile().unwrap();
    assert_eq!(second, first);
    assert!(!workspace.is_dirty());
}

#[test]
fn expected_revision_is_checked_after_foreground_reconciliation() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("file"), b"old").unwrap();
    let workspace = fixture.open();
    let old = workspace.current_revision().unwrap();
    fs::write(fixture.workspace.join("file"), b"new").unwrap();

    let error = workspace.read_file(old.id(), "file").unwrap_err();
    let RevisionError::StaleRevision { expected, current } = error else {
        panic!("external edit returned an untyped result: {error:?}");
    };
    assert_eq!(expected, old.id());
    assert_ne!(current.id(), old.id());
    assert_eq!(workspace.read_file(current.id(), "file").unwrap(), b"new");
}

#[test]
fn watcher_marks_dirty_and_periodic_reconciliation_advances_revision() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("file"), b"old").unwrap();
    let workspace = fixture.open();
    let old = workspace.current_revision().unwrap();
    fs::write(fixture.workspace.join("file"), b"new bytes").unwrap();

    wait_until(Duration::from_secs(2), || workspace.is_dirty());
    wait_until(Duration::from_secs(2), || {
        workspace.current_revision().unwrap().id() != old.id()
    });
    assert_eq!(
        workspace
            .read_file(workspace.current_revision().unwrap().id(), "file")
            .unwrap(),
        b"new bytes"
    );
}

#[test]
fn injected_watcher_loss_never_returns_silent_stale_content() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("file"), b"old").unwrap();
    let workspace = fixture.open();
    let old = workspace.current_revision().unwrap();
    fs::write(fixture.workspace.join("file"), b"new").unwrap();
    workspace.inject_watcher_loss();

    assert!(matches!(
        workspace.read_file(old.id(), "file"),
        Err(RevisionError::StaleRevision { .. })
    ));
    let fresh = workspace.current_revision().unwrap();
    assert_eq!(workspace.read_file(fresh.id(), "file").unwrap(), b"new");
}

#[test]
fn watcher_loss_maps_failed_bounded_reconciliation_to_unavailable() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("file"), b"ok").unwrap();
    let mut options = fixture.options();
    options.max_bytes = 4;
    let workspace = ManagedWorkspace::open_with_options(&fixture.workspace, options).unwrap();
    let revision = workspace.current_revision().unwrap();
    fs::write(fixture.workspace.join("file"), b"too large").unwrap();
    workspace.inject_watcher_loss();
    assert!(matches!(
        workspace.read_file(revision.id(), "file"),
        Err(RevisionError::Unavailable { .. })
    ));
}

#[test]
fn full_manager_restart_rotates_identity_even_without_an_edit() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("file"), b"one").unwrap();
    let mut previous = {
        let workspace = fixture.open();
        workspace.current_revision().unwrap()
    };
    for iteration in 0..8 {
        let current = {
            let workspace = fixture.open();
            workspace.current_revision().unwrap()
        };
        assert_ne!(current.id(), previous.id(), "iteration {iteration}");
        assert_eq!(current.digest(), previous.digest(), "iteration {iteration}");
        previous = current;
    }

    fs::write(fixture.workspace.join("file"), b"changed while stopped").unwrap();
    let changed = fixture.open().current_revision().unwrap();
    assert_ne!(changed.id(), previous.id());
    assert_ne!(changed.digest(), previous.digest());
}

#[test]
#[ignore = "exact opt-in workspace-owner stress; run serially with --ignored --exact --test-threads=1"]
fn full_manager_restart_rotates_identity_500_iterations_parallel() {
    let next = Arc::new(AtomicUsize::new(0));
    thread::scope(|scope| {
        for _ in 0..8 {
            let next = Arc::clone(&next);
            scope.spawn(move || {
                let fixture = Fixture::new();
                fs::write(fixture.workspace.join("file"), b"same").unwrap();
                let mut previous = {
                    let workspace = fixture.open();
                    workspace.current_revision().unwrap()
                };
                loop {
                    let iteration = next.fetch_add(1, Ordering::Relaxed);
                    if iteration >= 500 {
                        break;
                    }
                    let current = {
                        let workspace = fixture.open();
                        workspace.current_revision().unwrap()
                    };
                    assert_ne!(current.epoch(), previous.epoch(), "iteration {iteration}");
                    assert_eq!(current.digest(), previous.digest(), "iteration {iteration}");
                    previous = current;
                }
            });
        }
    });
}

#[test]
fn concurrent_managers_publish_one_revision_for_one_digest() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("file"), b"old").unwrap();
    let first = Arc::new(fixture.open());
    let second = Arc::new(fixture.open());
    fs::write(fixture.workspace.join("file"), b"new").unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = [first, second]
        .into_iter()
        .map(|workspace| {
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                workspace.reconcile().unwrap()
            })
        })
        .collect();
    barrier.wait();
    let revisions: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(revisions[0], revisions[1]);
}

#[test]
fn concurrent_foreign_processes_fail_busy_while_one_process_owns_the_workspace() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("file"), b"same").unwrap();
    let owner = fixture.open();
    let token = owner.current_revision().unwrap().id();
    let executable = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for index in 0..8 {
        let output = fixture.root.join(format!("worker-{index}"));
        let child = Command::new(&executable)
            .args([
                "--exact",
                "ws_revision::revision_subprocess_worker",
                "--nocapture",
            ])
            .env("KIT_REVISION_WORKER_ROOT", &fixture.workspace)
            .env("KIT_REVISION_WORKER_OUTPUT", output)
            .spawn()
            .unwrap();
        children.push(child);
    }
    for mut child in children {
        assert!(child.wait().unwrap().success());
    }
    let results: Vec<_> = (0..8)
        .map(|index| fs::read_to_string(fixture.root.join(format!("worker-{index}"))).unwrap())
        .collect();
    assert!(results.iter().all(|result| result == "busy"));
    assert_eq!(owner.current_revision().unwrap().id(), token);
}

#[test]
fn revision_subprocess_worker() {
    let Ok(root) = std::env::var("KIT_REVISION_WORKER_ROOT") else {
        return;
    };
    let output = std::env::var("KIT_REVISION_WORKER_OUTPUT").unwrap();
    let result = match ManagedWorkspace::open(root) {
        Ok(workspace) => workspace.current_revision().unwrap().id().to_string(),
        Err(RevisionError::Unavailable {
            reason: "workspace manager is owned by another process",
        }) => "busy".to_owned(),
        Err(error) => panic!("unexpected worker result: {error:?}"),
    };
    fs::write(output, result).unwrap();
}

#[test]
fn missing_corrupt_or_rolled_back_metadata_rotates_the_epoch() {
    let fixture = Fixture::new();
    let metadata = fixture.root.join("revision.state");
    let lock = fixture.root.join("revision.state.lock");
    fs::write(fixture.workspace.join("file"), b"one").unwrap();
    let mut options = fixture.options();
    options.metadata_path = Some(metadata.clone());
    let workspace =
        ManagedWorkspace::open_with_options(&fixture.workspace, options.clone()).unwrap();
    let old = workspace.current_revision().unwrap();
    let old_state = fs::read(&metadata).unwrap();
    let old_guard = fs::read(&lock).unwrap();

    fs::write(fixture.workspace.join("file"), b"two").unwrap();
    let advanced = workspace.current_revision().unwrap();
    assert_ne!(advanced.id(), old.id());
    fs::write(fixture.workspace.join("file"), b"one").unwrap();
    fs::write(&metadata, &old_state).unwrap();
    fs::write(&lock, &old_guard).unwrap();
    let after_rollback = workspace.current_revision().unwrap();
    assert_ne!(after_rollback.id(), old.id());

    fs::write(&metadata, b"corrupt").unwrap();
    let after_corruption = workspace.current_revision().unwrap();
    assert_ne!(after_corruption.id(), after_rollback.id());
    fs::remove_file(&metadata).unwrap();
    let after_missing = workspace.current_revision().unwrap();
    assert_ne!(after_missing.id(), after_corruption.id());
}

#[test]
fn full_restart_rejects_replayed_state_and_guard_tokens() {
    let fixture = Fixture::new();
    let metadata = fixture.root.join("revision.state");
    let guard = fixture.root.join("revision.state.lock");
    fs::write(fixture.workspace.join("file"), b"same").unwrap();
    let mut options = fixture.options();
    options.metadata_path = Some(metadata.clone());

    let old = {
        let workspace =
            ManagedWorkspace::open_with_options(&fixture.workspace, options.clone()).unwrap();
        workspace.current_revision().unwrap()
    };
    let old_state = fs::read(&metadata).unwrap();
    let old_guard = fs::read(&guard).unwrap();
    let replacement = {
        let workspace =
            ManagedWorkspace::open_with_options(&fixture.workspace, options.clone()).unwrap();
        workspace.current_revision().unwrap()
    };
    assert_ne!(replacement.id(), old.id());

    fs::write(&metadata, old_state).unwrap();
    fs::write(&guard, old_guard).unwrap();
    let replayed = ManagedWorkspace::open_with_options(&fixture.workspace, options)
        .unwrap()
        .current_revision()
        .unwrap();
    assert_ne!(replayed.id(), old.id());
    assert_ne!(replayed.id(), replacement.id());
    assert_eq!(replayed.digest(), old.digest());
}

#[test]
fn metadata_aliases_are_rejected_and_parent_replacement_cannot_redirect_updates() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("file"), b"one").unwrap();

    let mut inside = fixture.options();
    inside.metadata_path = Some(fixture.workspace.join("revision.state"));
    assert!(matches!(
        ManagedWorkspace::open_with_options(&fixture.workspace, inside),
        Err(RevisionError::UnsafePath(_))
    ));

    let target = fixture.workspace.join("target");
    fs::write(&target, b"workspace bytes").unwrap();
    let alias = fixture.root.join("alias.state");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let mut aliased = fixture.options();
    aliased.metadata_path = Some(alias);
    assert!(matches!(
        ManagedWorkspace::open_with_options(&fixture.workspace, aliased),
        Err(RevisionError::UnsafePath(_))
    ));

    let parent = fixture.root.join("metadata");
    let retained = fixture.root.join("metadata-retained");
    fs::create_dir(&parent).unwrap();
    let state = parent.join("revision.state");
    let mut options = fixture.options();
    options.metadata_path = Some(state);
    let workspace = ManagedWorkspace::open_with_options(&fixture.workspace, options).unwrap();
    fs::rename(&parent, &retained).unwrap();
    std::os::unix::fs::symlink(&fixture.workspace, &parent).unwrap();
    fs::write(fixture.workspace.join("file"), b"two").unwrap();
    workspace.reconcile().unwrap();
    assert!(retained.join("revision.state").is_file());
    assert!(!fixture.workspace.join("revision.state").exists());
}

#[test]
fn symlinks_hardlinks_and_special_entries_fail_closed() {
    // Symlinks are inert first-class entries: the scan binds their literal
    // target bytes into the revision without following them, so a retarget
    // changes the revision like any content change.
    let symlink = Fixture::new();
    let external = symlink.root.join("external");
    fs::write(&external, b"outside").unwrap();
    std::os::unix::fs::symlink(&external, symlink.workspace.join("link")).unwrap();
    let workspace = ManagedWorkspace::open(&symlink.workspace).unwrap();
    let first = workspace.current_revision().unwrap().id().to_string();
    fs::remove_file(symlink.workspace.join("link")).unwrap();
    std::os::unix::fs::symlink("elsewhere", symlink.workspace.join("link")).unwrap();
    workspace.reconcile().unwrap();
    let second = workspace.current_revision().unwrap().id().to_string();
    assert_ne!(first, second);
    drop(workspace);

    let hardlink = Fixture::new();
    fs::write(hardlink.workspace.join("first"), b"bytes").unwrap();
    fs::hard_link(
        hardlink.workspace.join("first"),
        hardlink.workspace.join("second"),
    )
    .unwrap();
    assert!(matches!(
        ManagedWorkspace::open(&hardlink.workspace),
        Err(RevisionError::Hardlink(_))
    ));

    let fifo = Fixture::new();
    let path =
        std::ffi::CString::new(fifo.workspace.join("pipe").as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: path names a test-owned, absent entry.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert!(matches!(
        ManagedWorkspace::open(&fifo.workspace),
        Err(RevisionError::UnsupportedEntry(_))
    ));
}

#[test]
fn scan_entry_byte_and_time_bounds_are_typed() {
    let entries = Fixture::new();
    fs::write(entries.workspace.join("a"), b"a").unwrap();
    fs::write(entries.workspace.join("b"), b"b").unwrap();
    let mut options = entries.options();
    options.max_entries = 1;
    assert!(matches!(
        ManagedWorkspace::open_with_options(&entries.workspace, options),
        Err(RevisionError::LimitExceeded(LimitKind::Entries))
    ));

    let bytes = Fixture::new();
    fs::write(bytes.workspace.join("file"), b"five!").unwrap();
    let mut options = bytes.options();
    options.max_bytes = 4;
    assert!(matches!(
        ManagedWorkspace::open_with_options(&bytes.workspace, options),
        Err(RevisionError::LimitExceeded(LimitKind::Bytes))
    ));

    let time = Fixture::new();
    let mut options = time.options();
    options.max_scan_time = Duration::from_nanos(1);
    assert!(matches!(
        ManagedWorkspace::open_with_options(&time.workspace, options),
        Err(RevisionError::LimitExceeded(LimitKind::Time))
    ));
}

#[test]
fn name_depth_and_memory_bounds_apply_during_enumeration() {
    let names = Fixture::new();
    fs::write(names.workspace.join("long-name"), b"x").unwrap();
    let mut options = names.options();
    options.max_name_bytes = 4;
    assert!(matches!(
        ManagedWorkspace::open_with_options(&names.workspace, options),
        Err(RevisionError::LimitExceeded(LimitKind::NameBytes))
    ));

    let depth = Fixture::new();
    fs::create_dir(depth.workspace.join("one")).unwrap();
    fs::create_dir(depth.workspace.join("one/two")).unwrap();
    let mut options = depth.options();
    options.max_depth = 1;
    assert!(matches!(
        ManagedWorkspace::open_with_options(&depth.workspace, options),
        Err(RevisionError::LimitExceeded(LimitKind::Depth))
    ));

    let memory = Fixture::new();
    fs::write(memory.workspace.join("file"), b"x").unwrap();
    let mut options = memory.options();
    options.max_memory_bytes = 64;
    assert!(matches!(
        ManagedWorkspace::open_with_options(&memory.workspace, options),
        Err(RevisionError::LimitExceeded(LimitKind::Memory))
    ));
}

#[test]
fn operation_memory_limit_matches_the_documented_logical_budget() {
    let fixture = Fixture::new();
    let name = "file";
    let content_len = 3 * 64 * 1024 + 17;
    fs::write(fixture.workspace.join(name), vec![b'x'; content_len]).unwrap();

    let scan_pass = logical_vector_growth::<std::ffi::OsString>()
        + logical_allocation(name.len())
        + logical_allocation(name.len() + 1)
        + logical_allocation(
            fixture.workspace.as_os_str().as_encoded_bytes().len() + name.len() + 1,
        );
    let fence = if cfg!(target_os = "macos") {
        logical_vector_growth::<fs::File>()
    } else {
        0
    };
    let revision_budget = fence + 2 * scan_pass;
    let read_budget =
        revision_budget + logical_allocation(name.len()) + logical_allocation(content_len);
    let snapshot_budget = revision_budget
        + logical_allocation(content_len)
        + logical_vector_growth::<SnapshotEntry>();
    assert!(revision_budget < content_len as u64);

    let mut exact = fixture.options();
    exact.max_memory_bytes = revision_budget;
    let workspace = ManagedWorkspace::open_with_options(&fixture.workspace, exact).unwrap();
    workspace.current_revision().unwrap();
    drop(workspace);

    let mut short = fixture.options();
    short.max_memory_bytes = revision_budget - 1;
    assert!(matches!(
        ManagedWorkspace::open_with_options(&fixture.workspace, short),
        Err(RevisionError::LimitExceeded(LimitKind::Memory))
    ));

    let mut exact = fixture.options();
    exact.max_memory_bytes = read_budget;
    let workspace = ManagedWorkspace::open_with_options(&fixture.workspace, exact).unwrap();
    let revision = workspace.current_revision().unwrap();
    assert_eq!(
        workspace.read_file(revision.id(), name).unwrap().len(),
        content_len
    );
    drop(workspace);

    let mut short = fixture.options();
    short.max_memory_bytes = read_budget - 1;
    let workspace = ManagedWorkspace::open_with_options(&fixture.workspace, short).unwrap();
    let revision = workspace.current_revision().unwrap();
    assert!(matches!(
        workspace.read_file(revision.id(), name),
        Err(RevisionError::LimitExceeded(LimitKind::Memory))
    ));
    drop(workspace);

    let mut exact = fixture.options();
    exact.max_memory_bytes = snapshot_budget;
    let workspace = ManagedWorkspace::open_with_options(&fixture.workspace, exact).unwrap();
    let revision = workspace.current_revision().unwrap();
    assert_eq!(
        workspace.snapshot(revision.id()).unwrap().entries().len(),
        1
    );
    drop(workspace);

    let mut short = fixture.options();
    short.max_memory_bytes = snapshot_budget - 1;
    let workspace = ManagedWorkspace::open_with_options(&fixture.workspace, short).unwrap();
    let revision = workspace.current_revision().unwrap();
    assert!(matches!(
        workspace.snapshot(revision.id()),
        Err(RevisionError::LimitExceeded(LimitKind::Memory))
    ));
}

#[test]
fn revision_and_focused_read_do_not_retain_unselected_file_bytes() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("large"), vec![b'x'; 1024 * 1024]).unwrap();
    fs::write(fixture.workspace.join("selected"), b"ok").unwrap();
    let mut options = fixture.options();
    options.max_memory_bytes = 64 * 1024;
    let workspace = ManagedWorkspace::open_with_options(&fixture.workspace, options).unwrap();
    let revision = workspace.current_revision().unwrap();

    assert_eq!(
        workspace.read_file(revision.id(), "selected").unwrap(),
        b"ok"
    );
    assert!(matches!(
        workspace.read_file(revision.id(), "large"),
        Err(RevisionError::LimitExceeded(LimitKind::Memory))
    ));
    assert!(matches!(
        workspace.snapshot(revision.id()),
        Err(RevisionError::LimitExceeded(LimitKind::Memory))
    ));
}

#[test]
fn bounded_byte_and_line_reads_do_not_allocate_the_full_file_or_newline_offsets() {
    let fixture = Fixture::new();
    let bytes = 1024 * 1024;
    fs::write(fixture.workspace.join("huge"), vec![b'x'; bytes]).unwrap();
    let lines = 256 * 1024;
    fs::write(fixture.workspace.join("newlines"), vec![b'\n'; lines]).unwrap();
    let mut options = fixture.options();
    options.max_memory_bytes = 512 * 1024;
    let workspace = ManagedWorkspace::open_with_options(&fixture.workspace, options).unwrap();
    let revision = workspace.current_revision().unwrap().id();
    let deadline = Instant::now() + Duration::from_secs(5);

    let one = workspace
        .read_file_range_before(
            revision,
            "huge",
            FileReadRange::Bytes {
                start: bytes - 1,
                end: bytes,
            },
            1,
            deadline,
        )
        .unwrap();
    assert_eq!(one.bytes, b"x");
    assert_eq!(one.file_bytes, bytes);

    let last = workspace
        .read_file_range_before(
            revision,
            "newlines",
            FileReadRange::Lines {
                start: lines,
                end: lines,
            },
            1,
            deadline,
        )
        .unwrap();
    assert_eq!(last.bytes, b"\n");
    assert_eq!(last.byte_start, lines - 1);
    assert_eq!(last.byte_end, lines);
}

fn logical_allocation(payload: usize) -> u64 {
    if payload == 0 {
        return 0;
    }
    let payload = payload as u64;
    let quantum = if payload <= 4096 { 64 } else { 4096 };
    payload.div_ceil(quantum) * quantum + 64
}

fn logical_vector_growth<T>() -> u64 {
    let item_size = std::mem::size_of::<T>().max(1);
    logical_allocation((4096 / item_size).max(1) * item_size)
}

#[test]
fn current_revision_is_an_authoritative_bounded_reconciliation() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("file"), b"old").unwrap();
    let workspace = fixture.open();
    let old = workspace.current_revision().unwrap();
    fs::write(fixture.workspace.join("file"), b"new").unwrap();
    let current = workspace.current_revision().unwrap();
    assert_ne!(current.id(), old.id());
    assert_eq!(workspace.read_file(current.id(), "file").unwrap(), b"new");
}

#[test]
fn dropping_watcher_wakes_a_long_poll_interval() {
    let fixture = Fixture::new();
    let mut options = fixture.options();
    options.watcher_interval = Duration::from_secs(60 * 60);
    options.reconciliation_interval = Duration::from_secs(60 * 60);
    let workspace = ManagedWorkspace::open_with_options(&fixture.workspace, options).unwrap();
    let started = Instant::now();
    drop(workspace);
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn concurrent_readers_observe_one_revision_snapshot() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace.join("file"), vec![7_u8; 1024]).unwrap();
    let workspace = Arc::new(fixture.open());
    let revision = workspace.current_revision().unwrap().id();
    let readers: Vec<_> = (0..16)
        .map(|_| {
            let workspace = Arc::clone(&workspace);
            thread::spawn(move || workspace.read_file(revision, "file").unwrap())
        })
        .collect();
    for reader in readers {
        assert_eq!(reader.join().unwrap(), vec![7_u8; 1024]);
    }
}

#[test]
fn active_writer_produces_only_consistent_content_or_typed_race() {
    let fixture = Fixture::new();
    const FILE_BYTES: usize = 512 * 1024;
    let path = fixture.workspace.join("file");
    fs::write(&path, vec![b'a'; FILE_BYTES]).unwrap();
    let mut options = fixture.options();
    options.max_scan_attempts = 2;
    let workspace = ManagedWorkspace::open_with_options(&fixture.workspace, options).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writer_stop = Arc::clone(&stop);
    let writer = thread::spawn(move || {
        let mut byte = b'b';
        while !writer_stop.load(Ordering::Acquire) {
            fs::write(&path, vec![byte; FILE_BYTES]).unwrap();
            byte = if byte == b'a' { b'b' } else { b'a' };
        }
    });

    let mut saw_race = false;
    for _ in 0..20 {
        let expected = match workspace.current_revision() {
            Ok(revision) => revision.id(),
            Err(RevisionError::ScanRace { attempts: 2 }) => {
                saw_race = true;
                continue;
            }
            Err(error) => panic!("writer race returned an untyped error: {error:?}"),
        };
        match workspace.read_file(expected, "file") {
            Ok(bytes) => assert!(bytes.is_empty() || bytes.iter().all(|byte| *byte == bytes[0])),
            Err(RevisionError::ScanRace { attempts: 2 }) => saw_race = true,
            Err(RevisionError::StaleRevision { .. }) => {}
            Err(error) => panic!("writer race returned an untyped error: {error:?}"),
        }
    }
    stop.store(true, Ordering::Release);
    writer.join().unwrap();
    assert!(
        saw_race,
        "continuous writer never exercised typed scan retry"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn same_device_bind_mount_is_rejected_when_mounting_is_available() {
    use std::{ffi::CString, os::unix::fs::MetadataExt};

    struct Unmount(PathBuf);
    impl Drop for Unmount {
        fn drop(&mut self) {
            let path = CString::new(self.0.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: path names the test-owned bind mount.
            unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) };
        }
    }

    let fixture = Fixture::new();
    let external = fixture.root.join("external");
    let mounted = fixture.workspace.join("mounted");
    fs::create_dir(&external).unwrap();
    fs::create_dir(&mounted).unwrap();
    fs::write(external.join("outside"), b"outside").unwrap();
    assert_eq!(
        fs::metadata(&external).unwrap().dev(),
        fs::metadata(&mounted).unwrap().dev()
    );
    let source = CString::new(external.as_os_str().as_encoded_bytes()).unwrap();
    let target = CString::new(mounted.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: source and target are test-owned directories.
    if unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    } != 0
    {
        return;
    }
    let _unmount = Unmount(mounted);
    assert!(matches!(
        ManagedWorkspace::open(&fixture.workspace),
        Err(RevisionError::MountBoundary(_))
    ));
}

fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(predicate(), "condition was not met within {timeout:?}");
}
