use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use kit::workspace::acquire::{
    AcquisitionError, AcquisitionMode, AcquisitionRequest, CleanupOutcome, DirtyContent,
    GitMetadata, OwnerId, ReservationRequest, SnapshotMaterialization, WorkspaceId, WriterPolicy,
    acquire, cleanup, release_reserved_target, reserve_target,
};

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    managed: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0_u8; 16];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("kit-workspace-test-{}", hex(&random)));
        let source = root.join("source");
        let managed = root.join("managed");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&source).unwrap();
        fs::create_dir(&managed).unwrap();
        git(&source, &["init", "--quiet"]);
        fs::write(source.join("tracked.txt"), "tracked base\n").unwrap();
        fs::write(source.join("staged.txt"), "staged base\n").unwrap();
        git(&source, &["add", "--", "tracked.txt", "staged.txt"]);
        git(
            &source,
            &[
                "-c",
                "user.name=Kit Test",
                "-c",
                "user.email=kit@example.invalid",
                "commit",
                "--quiet",
                "--no-gpg-sign",
                "-m",
                "base",
            ],
        );
        Self {
            root,
            source,
            managed,
        }
    }

    fn dirty(&self) {
        fs::write(self.source.join("tracked.txt"), "unstaged change\n").unwrap();
        fs::write(self.source.join("staged.txt"), "staged change\n").unwrap();
        git(&self.source, &["add", "--", "staged.txt"]);
        fs::write(self.source.join("untracked.txt"), "untracked content\n").unwrap();
    }

    fn request(&self, mode: AcquisitionMode, policy: WriterPolicy) -> AcquisitionRequest {
        AcquisitionRequest::new(
            &self.source,
            &self.managed,
            WorkspaceId::new("workspace-1").unwrap(),
            OwnerId::new("writer-1").unwrap(),
            mode,
            policy,
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn all_modes_preserve_dirty_source_and_report_snapshot_truthfully() {
    for (mode, policy, expected_dirty, expected_metadata, materialization) in [
        (
            AcquisitionMode::DetachedWorktree,
            WriterPolicy::TrustedAllowSharedGitMetadata,
            DirtyContent::NotIncluded,
            GitMetadata::SharedWithSource,
            SnapshotMaterialization::DetachedWorktree,
        ),
        (
            AcquisitionMode::LocalClone,
            WriterPolicy::Restricted,
            DirtyContent::NotIncluded,
            GitMetadata::Independent,
            SnapshotMaterialization::FullCopyFallback,
        ),
        (
            AcquisitionMode::CopyOnWriteSnapshot,
            WriterPolicy::Hostile,
            DirtyContent::Included,
            GitMetadata::Independent,
            SnapshotMaterialization::FullCopyFallback,
        ),
    ] {
        let fixture = Fixture::new();
        fixture.dirty();
        let status_before = status(&fixture.source);
        let bytes_before = source_bytes(&fixture.source);

        let workspace = acquire(fixture.request(mode, policy)).unwrap();
        assert_eq!(workspace.dirty_content, expected_dirty);
        assert_eq!(workspace.git_metadata, expected_metadata);
        assert_eq!(workspace.materialization, materialization);
        assert_eq!(
            workspace.canonical_source,
            fixture.source.canonicalize().unwrap()
        );
        assert_eq!(workspace.owner_id.as_str(), "writer-1");
        assert_eq!(workspace.acquisition_id.as_str().len(), 32);
        assert_eq!(workspace.base_commit.len(), 40);
        assert!(
            workspace
                .initial_dirty_state
                .as_str()
                .starts_with("blake3:")
        );
        assert!(
            workspace
                .workspace_revision
                .hash
                .as_str()
                .starts_with("blake3:")
        );
        assert_eq!(workspace.workspace_revision.number, 0);
        assert_eq!(status(&fixture.source), status_before);
        assert_eq!(source_bytes(&fixture.source), bytes_before);

        match mode {
            AcquisitionMode::CopyOnWriteSnapshot => {
                assert_eq!(
                    fs::read(workspace.path.join("tracked.txt")).unwrap(),
                    b"unstaged change\n"
                );
                assert_eq!(
                    fs::read(workspace.path.join("staged.txt")).unwrap(),
                    b"staged change\n"
                );
                assert_eq!(
                    fs::read(workspace.path.join("untracked.txt")).unwrap(),
                    b"untracked content\n"
                );
                assert_eq!(status(&workspace.path), status_before);
            }
            AcquisitionMode::DetachedWorktree | AcquisitionMode::LocalClone => {
                assert_eq!(
                    fs::read(workspace.path.join("tracked.txt")).unwrap(),
                    b"tracked base\n"
                );
                assert!(!workspace.path.join("untracked.txt").exists());
                assert!(status(&workspace.path).is_empty());
            }
        }

        assert_eq!(cleanup(&workspace).unwrap(), CleanupOutcome::Removed);
        assert_eq!(cleanup(&workspace).unwrap(), CleanupOutcome::AlreadyAbsent);
        assert_eq!(status(&fixture.source), status_before);
        assert_eq!(source_bytes(&fixture.source), bytes_before);
    }
}

#[test]
fn restricted_and_hostile_writers_fail_closed_for_shared_worktrees() {
    let fixture = Fixture::new();
    for policy in [WriterPolicy::Restricted, WriterPolicy::Hostile] {
        assert!(matches!(
            acquire(fixture.request(AcquisitionMode::DetachedWorktree, policy)),
            Err(AcquisitionError::SharedMetadataForbidden(rejected)) if rejected == policy
        ));
    }
    assert!(fs::read_dir(&fixture.managed).unwrap().next().is_none());
}

#[test]
fn dirty_state_hash_binds_content_not_only_status_shape() {
    let fixture = Fixture::new();
    fs::write(fixture.source.join("tracked.txt"), "first bytes\n").unwrap();
    let first =
        acquire(fixture.request(AcquisitionMode::LocalClone, WriterPolicy::Restricted)).unwrap();
    let first_hash = first.initial_dirty_state.clone();
    cleanup(&first).unwrap();

    fs::write(fixture.source.join("tracked.txt"), "second bytes\n").unwrap();
    let second =
        acquire(fixture.request(AcquisitionMode::LocalClone, WriterPolicy::Restricted)).unwrap();
    assert_ne!(first_hash, second.initial_dirty_state);
    cleanup(&second).unwrap();
}

#[cfg(unix)]
#[test]
fn tracked_executable_changes_dirty_and_revision_hashes() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let first =
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
            .unwrap();
    let initial_hash = first.initial_dirty_state.clone();
    let revision_hash = first.workspace_revision.hash.clone();
    cleanup(&first).unwrap();

    fs::set_permissions(
        fixture.source.join("tracked.txt"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let second =
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
            .unwrap();
    assert_ne!(second.initial_dirty_state, initial_hash);
    assert_ne!(second.workspace_revision.hash, revision_hash);
    cleanup(&second).unwrap();
}

#[cfg(unix)]
#[test]
fn tracked_executable_is_hashed_when_core_filemode_is_false() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    git(&fixture.source, &["config", "core.filemode", "false"]);
    let first =
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
            .unwrap();
    let initial_hash = first.initial_dirty_state.clone();
    cleanup(&first).unwrap();

    fs::set_permissions(
        fixture.source.join("tracked.txt"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(status(&fixture.source).is_empty());
    let second =
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
            .unwrap();
    assert_ne!(second.initial_dirty_state, initial_hash);
    assert_eq!(second.dirty_content, DirtyContent::Included);
    cleanup(&second).unwrap();
}

#[cfg(unix)]
#[test]
fn untracked_executable_changes_dirty_and_revision_hashes() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let path = fixture.source.join("untracked.txt");
    fs::write(&path, "same bytes\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let first =
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
            .unwrap();
    let initial_hash = first.initial_dirty_state.clone();
    let revision_hash = first.workspace_revision.hash.clone();
    cleanup(&first).unwrap();

    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let second =
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
            .unwrap();
    assert_ne!(second.initial_dirty_state, initial_hash);
    assert_ne!(second.workspace_revision.hash, revision_hash);
    cleanup(&second).unwrap();
}

#[cfg(unix)]
#[test]
fn irrelevant_untracked_permissions_do_not_change_hashes() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let path = fixture.source.join("untracked.txt");
    fs::write(&path, "same bytes\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let first =
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
            .unwrap();
    let initial_hash = first.initial_dirty_state.clone();
    let revision_hash = first.workspace_revision.hash.clone();
    cleanup(&first).unwrap();

    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let second =
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
            .unwrap();
    assert_eq!(second.initial_dirty_state, initial_hash);
    assert_eq!(second.workspace_revision.hash, revision_hash);
    cleanup(&second).unwrap();
}

#[cfg(windows)]
#[test]
fn windows_read_only_state_does_not_invent_executable_metadata() {
    let fixture = Fixture::new();
    let path = fixture.source.join("untracked.txt");
    fs::write(&path, "same bytes\n").unwrap();
    let first =
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
            .unwrap();
    let initial_hash = first.initial_dirty_state.clone();
    let revision_hash = first.workspace_revision.hash.clone();
    cleanup(&first).unwrap();

    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions).unwrap();
    let second =
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
            .unwrap();
    assert_eq!(second.initial_dirty_state, initial_hash);
    assert_eq!(second.workspace_revision.hash, revision_hash);
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(false);
    fs::set_permissions(&path, permissions).unwrap();
    let copied = second.path.join("untracked.txt");
    let mut permissions = fs::metadata(&copied).unwrap().permissions();
    permissions.set_readonly(false);
    fs::set_permissions(copied, permissions).unwrap();
    cleanup(&second).unwrap();
}

#[test]
fn cow_snapshot_excludes_ignored_files() {
    let fixture = Fixture::new();
    fs::write(fixture.source.join(".gitignore"), "ignored.txt\n").unwrap();
    fs::write(fixture.source.join("ignored.txt"), "must not be copied\n").unwrap();

    let workspace =
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
            .unwrap();
    assert!(!workspace.path.join("ignored.txt").exists());
    cleanup(&workspace).unwrap();
}

#[cfg(unix)]
#[test]
fn untrusted_snapshot_rejects_symlinks_without_copying_their_targets() {
    let fixture = Fixture::new();
    let external = fixture.root.join("external-secret");
    fs::write(&external, "external bytes\n").unwrap();
    std::os::unix::fs::symlink(&external, fixture.source.join("external-link")).unwrap();

    for (mode, policy) in [
        (AcquisitionMode::LocalClone, WriterPolicy::Restricted),
        (AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile),
    ] {
        assert!(matches!(
            acquire(fixture.request(mode, policy)),
            Err(AcquisitionError::SymlinkPath { .. })
        ));
    }
    assert_eq!(fs::read(&external).unwrap(), b"external bytes\n");
    assert!(fs::read_dir(&fixture.managed).unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn untrusted_snapshot_rejects_git_objects_and_indexed_symlink_ancestors() {
    let objects = Fixture::new();
    let external_objects = objects.root.join("external-objects");
    fs::rename(objects.source.join(".git/objects"), &external_objects).unwrap();
    fs::create_dir_all(external_objects.join("info")).unwrap();
    let alternates = external_objects.join("info/alternates");
    fs::write(&alternates, "must remain\n").unwrap();
    std::os::unix::fs::symlink(&external_objects, objects.source.join(".git/objects")).unwrap();
    assert!(matches!(
        acquire(objects.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile)),
        Err(AcquisitionError::SymlinkPath { .. })
    ));
    assert_eq!(fs::read(&alternates).unwrap(), b"must remain\n");
    assert!(fs::read_dir(&objects.managed).unwrap().next().is_none());

    let indexed = Fixture::new();
    fs::create_dir(indexed.source.join("indexed")).unwrap();
    fs::write(indexed.source.join("indexed/file"), "indexed bytes\n").unwrap();
    git(&indexed.source, &["add", "indexed/file"]);
    git(
        &indexed.source,
        &[
            "-c",
            "user.name=Kit Test",
            "-c",
            "user.email=kit@example.invalid",
            "commit",
            "--quiet",
            "--no-gpg-sign",
            "-m",
            "indexed directory",
        ],
    );
    fs::remove_dir_all(indexed.source.join("indexed")).unwrap();
    let external = indexed.root.join("external-indexed");
    fs::create_dir(&external).unwrap();
    fs::write(external.join("file"), "outside bytes\n").unwrap();
    std::os::unix::fs::symlink(&external, indexed.source.join("indexed")).unwrap();
    assert!(matches!(
        acquire(indexed.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile)),
        Err(AcquisitionError::SymlinkPath { .. })
    ));
    assert!(fs::read_dir(&indexed.managed).unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn hostile_snapshot_rejects_hardlinks_and_fifos() {
    let hardlink = Fixture::new();
    fs::hard_link(
        hardlink.source.join("tracked.txt"),
        hardlink.source.join("second-name"),
    )
    .unwrap();
    assert!(matches!(
        acquire(hardlink.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile)),
        Err(AcquisitionError::HardlinkedSourceEntry(_))
    ));
    assert!(fs::read_dir(&hardlink.managed).unwrap().next().is_none());

    let fifo = Fixture::new();
    let path = fifo.source.join("named-pipe");
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: path is a valid NUL-terminated pathname in the owned fixture.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert!(matches!(
        acquire(fifo.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile)),
        Err(AcquisitionError::UnsupportedSourceEntry(_))
    ));
    assert!(fs::read_dir(&fifo.managed).unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn concurrent_source_replacement_never_copies_outside_bytes() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let fixture = Fixture::new();
    let source_entry = fixture.source.join("racy-entry");
    let external = fixture.root.join("external-secret");
    fs::write(&source_entry, "inside bytes\n").unwrap();
    fs::write(&external, "outside bytes must never be copied\n").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let writer_stop = Arc::clone(&stop);
    let writer_source = source_entry.clone();
    let writer_external = external.clone();
    let writer = std::thread::spawn(move || {
        let mut iteration = 0_u64;
        while !writer_stop.load(Ordering::Acquire) {
            let replacement = writer_source.with_extension(format!("replacement-{iteration}"));
            if iteration.is_multiple_of(2) {
                fs::write(&replacement, "inside bytes\n").unwrap();
            } else {
                std::os::unix::fs::symlink(&writer_external, &replacement).unwrap();
            }
            fs::rename(replacement, &writer_source).unwrap();
            iteration += 1;
        }
    });

    for _ in 0..10 {
        match acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
        {
            Ok(workspace) => {
                let copied = workspace.path.join("racy-entry");
                let metadata = fs::symlink_metadata(&copied).unwrap();
                assert!(metadata.is_file());
                assert_ne!(fs::read(&copied).unwrap(), fs::read(&external).unwrap());
                cleanup(&workspace).unwrap();
            }
            Err(AcquisitionError::SourceChangedDuringAcquisition)
            | Err(AcquisitionError::SymlinkPath { .. }) => {}
            Err(error) => panic!("replacement race returned an untyped error: {error:?}"),
        }
    }
    stop.store(true, Ordering::Release);
    writer.join().unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn hostile_snapshot_rejects_same_device_bind_mount_when_mounting_is_available() {
    use std::{ffi::CString, os::unix::fs::MetadataExt};

    struct Unmount(PathBuf);
    impl Drop for Unmount {
        fn drop(&mut self) {
            let path = CString::new(self.0.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: path names the bind mount owned by this test.
            unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) };
        }
    }

    let fixture = Fixture::new();
    let external = fixture.root.join("external-mount");
    let mounted = fixture.source.join("mounted");
    fs::create_dir(&external).unwrap();
    fs::create_dir(&mounted).unwrap();
    fs::write(external.join("outside"), "outside bytes\n").unwrap();
    assert_eq!(
        fs::metadata(&external).unwrap().dev(),
        fs::metadata(&mounted).unwrap().dev()
    );
    let source = CString::new(external.as_os_str().as_encoded_bytes()).unwrap();
    let target = CString::new(mounted.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: both C strings name test-owned directories and bind mount takes no data pointer.
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
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile)),
        Err(AcquisitionError::SourceFilesystemBoundary(_))
    ));
    assert!(fs::read_dir(&fixture.managed).unwrap().next().is_none());
}

#[test]
fn unsupported_index_flags_and_gitlinks_are_typed_rejections() {
    let skip = Fixture::new();
    git(
        &skip.source,
        &["update-index", "--skip-worktree", "tracked.txt"],
    );
    assert!(matches!(
        acquire(skip.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile)),
        Err(AcquisitionError::UnsupportedIndexState {
            reason: "skip-worktree entry",
            ..
        })
    ));
    assert!(fs::read_dir(&skip.managed).unwrap().next().is_none());

    let assume = Fixture::new();
    git(
        &assume.source,
        &["update-index", "--assume-unchanged", "tracked.txt"],
    );
    assert!(matches!(
        acquire(assume.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile)),
        Err(AcquisitionError::UnsupportedIndexState {
            reason: "assume-unchanged entry",
            ..
        })
    ));

    let gitlink = Fixture::new();
    let head = String::from_utf8(git(&gitlink.source, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();
    git(
        &gitlink.source,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            &head,
            "nested",
        ],
    );
    assert!(matches!(
        acquire(gitlink.request(
            AcquisitionMode::CopyOnWriteSnapshot,
            WriterPolicy::Hostile
        )),
        Err(AcquisitionError::UnsupportedGitlink(path)) if path == Path::new("nested")
    ));
    assert!(fs::read_dir(&gitlink.managed).unwrap().next().is_none());
}

#[test]
fn split_index_is_rejected_before_allocation() {
    let fixture = Fixture::new();
    git(&fixture.source, &["update-index", "--split-index"]);

    assert!(matches!(
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile)),
        Err(AcquisitionError::UnsupportedIndexState {
            reason: "split index",
            ..
        })
    ));
    assert!(fs::read_dir(&fixture.managed).unwrap().next().is_none());
}

#[test]
fn acquisition_does_not_run_checkout_hooks_or_recursive_submodules() {
    let fixture = Fixture::new();
    let sentinel = fixture.root.join("hook-ran");
    let hook = fixture.source.join(".git/hooks/post-checkout");
    fs::write(
        &hook,
        format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
    )
    .unwrap();
    make_executable(&hook);
    fs::write(
        fixture.source.join(".gitmodules"),
        "[submodule \"missing\"]\n\tpath = nested\n\turl = /missing\n",
    )
    .unwrap();
    git(&fixture.source, &["add", ".gitmodules"]);
    git(
        &fixture.source,
        &[
            "-c",
            "user.name=Kit Test",
            "-c",
            "user.email=kit@example.invalid",
            "commit",
            "--quiet",
            "--no-gpg-sign",
            "-m",
            "submodule declaration",
        ],
    );

    for (mode, policy) in [
        (
            AcquisitionMode::DetachedWorktree,
            WriterPolicy::TrustedAllowSharedGitMetadata,
        ),
        (AcquisitionMode::LocalClone, WriterPolicy::Restricted),
        (AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile),
    ] {
        let workspace = acquire(fixture.request(mode, policy)).unwrap();
        assert!(!sentinel.exists());
        assert!(!workspace.path.join("nested").exists());
        cleanup(&workspace).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn untrusted_acquisition_never_loads_source_git_commands() {
    for (mode, policy) in [
        (AcquisitionMode::LocalClone, WriterPolicy::Restricted),
        (AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile),
    ] {
        let fixture = Fixture::new();
        let canary = fixture.root.join("git-canary");
        let sentinel = fixture.root.join("git-command-ran");
        fs::write(
            &canary,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", sentinel.display()),
        )
        .unwrap();
        make_executable(&canary);
        let included = fixture.root.join("included-git-config");
        fs::write(
            &included,
            format!("[core]\n\tfsmonitor = {}\n", canary.display()),
        )
        .unwrap();
        let config = fixture.source.join(".git/config");
        let mut contents = fs::read_to_string(&config).unwrap();
        contents.push_str(&format!(
            "\n[include]\n\tpath = {}\n[filter \"hostile\"]\n\tprocess = {}\n\tclean = {}\n\tsmudge = {}\n[diff \"hostile\"]\n\ttextconv = {}\n[credential]\n\thelper = !{}\n[remote \"origin\"]\n\tpromisor = true\n",
            included.display(),
            canary.display(),
            canary.display(),
            canary.display(),
            canary.display(),
            canary.display(),
        ));
        fs::write(config, contents).unwrap();
        fs::write(
            fixture.source.join(".gitattributes"),
            "tracked.txt filter=hostile diff=hostile\n",
        )
        .unwrap();
        fs::write(fixture.source.join("tracked.txt"), "hostile change\n").unwrap();
        let hook = fixture.source.join(".git/hooks/post-checkout");
        fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\n", sentinel.display()),
        )
        .unwrap();
        make_executable(&hook);

        let workspace = acquire(fixture.request(mode, policy)).unwrap();
        assert!(
            !sentinel.exists(),
            "source Git command executed for {policy:?}"
        );
        assert!(!workspace.path.join(".git/config").exists());
        assert!(!workspace.path.join(".git/hooks").exists());
        cleanup(&workspace).unwrap();
        assert!(!sentinel.exists(), "cleanup executed a source Git command");
    }
}

#[cfg(unix)]
#[test]
fn untrusted_acquisition_does_not_spawn_setsid_background_helpers() {
    let fixture = Fixture::new();
    let sentinel = fixture.root.join("background-helper-ran");
    let helper = fixture.root.join("background-helper");
    fs::write(
        &helper,
        format!(
            "#!/bin/sh\nsetsid /bin/sh -c 'sleep 0.1; touch {}' >/dev/null 2>&1 &\n",
            sentinel.display()
        ),
    )
    .unwrap();
    make_executable(&helper);
    fs::write(
        fixture.source.join(".git/config"),
        format!("[core]\n\tfsmonitor = {}\n", helper.display()),
    )
    .unwrap();

    let workspace =
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile))
            .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(250));
    assert!(!sentinel.exists());
    cleanup(&workspace).unwrap();
}

#[test]
fn one_thousand_atomic_reservations_have_no_path_collisions() {
    let fixture = Fixture::new();
    let mut paths = HashSet::new();
    let mut reservations = Vec::new();
    for _ in 0..1000 {
        let reserved = reserve_target(ReservationRequest {
            managed_root: fixture.managed.clone(),
            workspace_id: WorkspaceId::new("same-workspace").unwrap(),
            owner_id: OwnerId::new("same-writer").unwrap(),
        })
        .unwrap();
        assert!(reserved.path.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&reserved.path).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(reserved.path.join(".kit-workspace"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        assert!(paths.insert(reserved.path.clone()), "reservation collision");
        reservations.push(reserved);
    }
    assert_eq!(paths.len(), 1000);
    for reserved in &reservations {
        assert_eq!(
            release_reserved_target(reserved).unwrap(),
            CleanupOutcome::Removed
        );
        assert_eq!(
            release_reserved_target(reserved).unwrap(),
            CleanupOutcome::AlreadyAbsent
        );
    }
}

#[test]
fn cleanup_refuses_boundary_changes_and_marker_mismatch() {
    let fixture = Fixture::new();
    let mut workspace =
        acquire(fixture.request(AcquisitionMode::LocalClone, WriterPolicy::Restricted)).unwrap();
    let real_path = workspace.path.clone();
    workspace.path = fixture.root.join("outside");
    assert!(matches!(
        cleanup(&workspace),
        Err(AcquisitionError::UnsafePath { .. })
    ));
    workspace.path = real_path;
    let marker = workspace.path.parent().unwrap().join(".kit-workspace");
    fs::write(&marker, "wrong identity\n").unwrap();
    assert!(matches!(
        cleanup(&workspace),
        Err(AcquisitionError::MarkerMismatch)
    ));
    assert!(workspace.path.exists());

    let mut reserved = reserve_target(ReservationRequest {
        managed_root: fixture.managed.clone(),
        workspace_id: WorkspaceId::new("reserved").unwrap(),
        owner_id: OwnerId::new("owner").unwrap(),
    })
    .unwrap();
    let actual = reserved.path.clone();
    reserved.path = fixture.root.join("outside-reservation");
    assert!(matches!(
        release_reserved_target(&reserved),
        Err(AcquisitionError::UnsafePath { .. })
    ));
    assert!(actual.exists());
}

#[cfg(unix)]
#[test]
fn cleanup_rejects_symlink_markers_and_swapped_allocations_without_removing_them() {
    let fixture = Fixture::new();
    let reserved = reserve_target(ReservationRequest {
        managed_root: fixture.managed.clone(),
        workspace_id: WorkspaceId::new("reserved").unwrap(),
        owner_id: OwnerId::new("owner").unwrap(),
    })
    .unwrap();
    let marker = reserved.path.join(".kit-workspace");
    let marker_bytes = fs::read(&marker).unwrap();
    let external_marker = fixture.root.join("external-marker");
    fs::write(&external_marker, marker_bytes).unwrap();
    fs::remove_file(&marker).unwrap();
    std::os::unix::fs::symlink(&external_marker, &marker).unwrap();
    assert!(matches!(
        release_reserved_target(&reserved),
        Err(AcquisitionError::MarkerMismatch)
    ));
    assert!(reserved.path.exists());

    let workspace =
        acquire(fixture.request(AcquisitionMode::LocalClone, WriterPolicy::Restricted)).unwrap();
    let allocation = workspace.path.parent().unwrap().to_path_buf();
    let original = fixture.root.join("original-allocation");
    let marker_bytes = fs::read(allocation.join(".kit-workspace")).unwrap();
    fs::rename(&allocation, &original).unwrap();
    fs::create_dir(&allocation).unwrap();
    fs::create_dir(allocation.join("repo")).unwrap();
    fs::write(allocation.join(".kit-workspace"), marker_bytes).unwrap();
    fs::write(allocation.join("victim"), "do not remove\n").unwrap();

    assert!(matches!(
        cleanup(&workspace),
        Err(AcquisitionError::FilesystemIdentityChanged)
    ));
    assert!(allocation.join("victim").exists());
}

#[test]
fn failed_acquisition_cleans_its_reserved_allocation() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.source.join("tracked.txt")).unwrap();
    fs::create_dir(fixture.source.join("tracked.txt")).unwrap();

    assert!(matches!(
        acquire(fixture.request(AcquisitionMode::CopyOnWriteSnapshot, WriterPolicy::Hostile)),
        Err(AcquisitionError::UnsupportedSourceEntry(_))
    ));
    assert!(fs::read_dir(&fixture.managed).unwrap().next().is_none());
}

#[test]
#[cfg(unix)]
fn cleanup_resumes_owned_quarantine_after_removal_failure() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let workspace = acquire(fixture.request(
        AcquisitionMode::DetachedWorktree,
        WriterPolicy::TrustedAllowSharedGitMetadata,
    ))
    .unwrap();
    let workspace_text = workspace.path.to_string_lossy().into_owned();
    let allocation = workspace.path.parent().unwrap().to_path_buf();
    let quarantine = fixture.managed.join(format!(
        ".kit-quarantine-{}",
        workspace.acquisition_id.as_str()
    ));
    let blocked = workspace.path.join("blocked");
    fs::create_dir(&blocked).unwrap();
    fs::write(blocked.join("file"), "blocked").unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0)).unwrap();

    assert!(matches!(
        cleanup(&workspace),
        Err(AcquisitionError::Io { .. })
    ));
    assert!(!allocation.exists());
    assert!(quarantine.exists());
    fs::set_permissions(
        quarantine.join("repo/blocked"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();

    assert_eq!(cleanup(&workspace).unwrap(), CleanupOutcome::Removed);
    assert!(!quarantine.exists());
    assert!(fs::read_dir(&fixture.managed).unwrap().next().is_none());
    let listed =
        String::from_utf8(git(&fixture.source, &["worktree", "list", "--porcelain"])).unwrap();
    assert!(!listed.contains(&workspace_text));
}

#[test]
fn cleanup_removes_worktree_metadata_when_worktree_directory_is_gone() {
    let fixture = Fixture::new();
    let workspace = acquire(fixture.request(
        AcquisitionMode::DetachedWorktree,
        WriterPolicy::TrustedAllowSharedGitMetadata,
    ))
    .unwrap();
    let workspace_text = workspace.path.to_string_lossy().into_owned();
    fs::remove_dir_all(&workspace.path).unwrap();
    assert_eq!(cleanup(&workspace).unwrap(), CleanupOutcome::Removed);
    let listed =
        String::from_utf8(git(&fixture.source, &["worktree", "list", "--porcelain"])).unwrap();
    assert!(!listed.contains(&workspace_text));
}

#[test]
fn unsafe_ids_paths_and_symlink_roots_are_rejected() {
    for id in ["", ".", "../escape", "has/slash", "has space", "é"] {
        assert!(WorkspaceId::new(id).is_err(), "accepted id {id:?}");
        assert!(OwnerId::new(id).is_err(), "accepted id {id:?}");
    }
    let fixture = Fixture::new();
    let relative = AcquisitionRequest::new(
        "relative/source",
        &fixture.managed,
        WorkspaceId::new("workspace").unwrap(),
        OwnerId::new("owner").unwrap(),
        AcquisitionMode::LocalClone,
        WriterPolicy::Restricted,
    );
    assert!(matches!(
        acquire(relative),
        Err(AcquisitionError::UnsafePath { .. })
    ));

    #[cfg(unix)]
    {
        let source_link = fixture.root.join("source-link");
        std::os::unix::fs::symlink(&fixture.source, &source_link).unwrap();
        let request = AcquisitionRequest::new(
            source_link,
            &fixture.managed,
            WorkspaceId::new("workspace").unwrap(),
            OwnerId::new("owner").unwrap(),
            AcquisitionMode::LocalClone,
            WriterPolicy::Restricted,
        );
        assert!(matches!(
            acquire(request),
            Err(AcquisitionError::SymlinkPath { .. })
        ));
    }
}

fn git(directory: &Path, arguments: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn status(directory: &Path) -> Vec<u8> {
    git(
        directory,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )
}

fn source_bytes(source: &Path) -> Vec<(String, Vec<u8>)> {
    ["tracked.txt", "staged.txt", "untracked.txt"]
        .into_iter()
        .map(|name| (name.to_owned(), fs::read(source.join(name)).unwrap()))
        .collect()
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[cfg(windows)]
fn make_executable(_path: &Path) {}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
