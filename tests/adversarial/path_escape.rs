#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    ffi::CString,
    fs,
    os::unix::{ffi::OsStrExt, net::UnixListener},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use kit::workspace::{
    edit::ir::{EditLimits, FilesystemIdentityPolicy},
    path_auth::{Authority, EntryType, PathAuthError, PathAuthLimit, PathAuthorizer},
    revision::{ManagedWorkspace, RevisionError},
};

struct Fixture {
    root: PathBuf,
    workspace_path: PathBuf,
    workspace: ManagedWorkspace,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kpa-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let workspace_path = root.join("workspace");
        fs::create_dir_all(&workspace_path).unwrap();
        let workspace = ManagedWorkspace::open(&workspace_path).unwrap();
        Self {
            root,
            workspace_path,
            workspace,
        }
    }

    fn transaction<T>(
        &self,
        limits: EditLimits,
        operation: impl FnOnce(&mut PathAuthorizer<'_, '_>) -> T,
    ) -> T {
        let revision = self.workspace.current_revision().unwrap();
        let mut guard = self.workspace.mutation_guard(revision.id()).unwrap();
        let mut authorizer =
            PathAuthorizer::new(&mut guard, revision.id(), revision.epoch(), limits).unwrap();
        operation(&mut authorizer)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn lexical_escapes_private_paths_and_alias_components_are_denied() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace_path.join("CaseName"), b"case").unwrap();
    fs::write(fixture.workspace_path.join("e\u{301}.txt"), b"unicode").unwrap();
    fixture.transaction(EditLimits::default(), |authorizer| {
        for path in [
            "",
            ".",
            "..",
            "../outside",
            "a/../../outside",
            "/etc/passwd",
            "a//b",
            "a/./b",
            "C:/Windows/system.ini",
            r"C:\Windows\system.ini",
        ] {
            assert!(
                matches!(
                    authorizer.authorize_read(path),
                    Err(PathAuthError::InvalidPath(_))
                ),
                "accepted lexical escape {path:?}"
            );
        }
        for path in [
            ".git/config",
            ".GIT/config",
            ".kit/index",
            ".KiT/index",
            ".kit.lock",
            ".kit-revision-state",
            ".KiT-cache/state",
            ".KIT-cache/state",
        ] {
            for existing in [true, false] {
                let result = if existing {
                    authorizer.authorize_read(path).map(drop)
                } else {
                    authorizer.authorize_create(path).map(drop)
                };
                assert!(
                    matches!(result, Err(PathAuthError::PrivatePath(_))),
                    "accepted private path alias {path:?}"
                );
            }
        }
        assert!(matches!(
            authorizer.authorize_read("casename"),
            Err(PathAuthError::Alias(_))
        ));
        assert!(matches!(
            authorizer.authorize_read("é.txt"),
            Err(PathAuthError::Alias(_))
        ));
        assert!(matches!(
            authorizer.authorize_read("e\u{301}.txt"),
            Err(PathAuthError::Alias(_))
        ));
    });
}

#[test]
fn case_sensitive_policy_fails_closed_without_a_safe_probe() {
    let fixture = Fixture::new();
    let revision = fixture.workspace.current_revision().unwrap();
    let mut guard = fixture.workspace.mutation_guard(revision.id()).unwrap();
    assert!(matches!(
        PathAuthorizer::new(
            &mut guard,
            revision.id(),
            revision.epoch(),
            EditLimits {
                identity_policy: FilesystemIdentityPolicy::CaseSensitive,
                ..EditLimits::default()
            },
        ),
        Err(PathAuthError::Unavailable { .. })
    ));
}

#[test]
fn symlinks_at_every_position_are_denied_without_outside_access() {
    for path in ["first/secret", "real/second/secret", "real/final"] {
        let fixture = Fixture::new();
        let outside = fixture.root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"outside").unwrap();
        fs::create_dir_all(fixture.workspace_path.join("real")).unwrap();
        match path {
            "first/secret" => {
                std::os::unix::fs::symlink(&outside, fixture.workspace_path.join("first")).unwrap();
            }
            "real/second/secret" => {
                std::os::unix::fs::symlink(&outside, fixture.workspace_path.join("real/second"))
                    .unwrap();
            }
            _ => {
                std::os::unix::fs::symlink(
                    outside.join("secret"),
                    fixture.workspace_path.join("real/final"),
                )
                .unwrap();
            }
        }
        assert!(matches!(
            fixture.workspace.current_revision(),
            Err(RevisionError::Symlink(_))
        ));
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"outside");
    }
}

#[test]
fn capabilities_are_typed_descriptor_authorizations_without_publication() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace_path.join("file"), b"old").unwrap();
    fixture.transaction(EditLimits::default(), |authorizer| {
        let read = authorizer.authorize_read("file").unwrap();
        assert_eq!(read.binding().authority(), Authority::ExistingRead);
        assert_eq!(
            authorizer
                .read(read, 1024, 1024, Instant::now() + Duration::from_secs(1))
                .unwrap(),
            b"old"
        );

        let replace = authorizer.authorize_replace("file").unwrap();
        assert_eq!(replace.binding().authority(), Authority::ReplaceSource);
        let delete = authorizer.authorize_delete("file").unwrap();
        assert_eq!(delete.binding().authority(), Authority::DeleteSource);
        let create = authorizer.authorize_create("created").unwrap();
        assert_eq!(create.binding().authority(), Authority::CreateParent);
        assert_eq!(
            create.binding().object_identity().unwrap().entry_type(),
            EntryType::Directory
        );
        let (source, destination) = authorizer.authorize_move("file", "moved").unwrap();
        assert_eq!(source.binding().authority(), Authority::MoveSource);
        assert_eq!(
            destination.binding().authority(),
            Authority::MoveDestination
        );
    });
    assert_eq!(
        fs::read(fixture.workspace_path.join("file")).unwrap(),
        b"old"
    );
    assert!(!fixture.workspace_path.join("created").exists());
    assert!(!fixture.workspace_path.join("moved").exists());
}

#[test]
fn synchronized_component_and_final_swaps_use_the_original_handle_or_reject() {
    for swapped in ["a", "a/b", "a/b/file"] {
        let fixture = Fixture::new();
        let path = fixture.workspace_path.join("a/b/file");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"original").unwrap();
        fixture.transaction(EditLimits::default(), |authorizer| {
            let capability = authorizer.authorize_read("a/b/file").unwrap();
            let original_inode = capability.binding().object_identity().unwrap().inode();
            let named = fixture.workspace_path.join(swapped);
            let detached = fixture.root.join("detached");
            fs::rename(&named, &detached).unwrap();
            recreate_same_tree(&fixture.workspace_path, swapped);
            assert_eq!(
                inode_of_detached(&detached, swapped),
                original_inode,
                "capability did not retain the object opened before swapping {swapped}"
            );
            match authorizer.read(
                capability,
                1024,
                1024,
                Instant::now() + Duration::from_secs(1),
            ) {
                Ok(bytes) => assert_eq!(bytes, b"original"),
                Err(error) => assert!(is_stale_or_changed(&error), "unexpected error: {error}"),
            }
        });
    }
}

#[test]
fn transient_component_and_leaf_aba_during_resolution_is_rejected() {
    for swapped in ["a", "a/b", "a/b/file"] {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.workspace_path.join("a/b")).unwrap();
        fs::write(fixture.workspace_path.join("a/b/file"), b"inside").unwrap();
        let outside = fixture.root.join("outside");
        fs::create_dir_all(outside.join("b")).unwrap();
        fs::write(outside.join("file"), b"outside").unwrap();
        fs::write(outside.join("b/file"), b"outside").unwrap();

        fixture.transaction(EditLimits::default(), |authorizer| {
            let mut swapped_once = false;
            let result = kit::test_support::authorize_path_read_with_hook(
                authorizer,
                "a/b/file",
                |stage, walked| {
                    let trigger = match swapped {
                        "a" => stage == "parent-watched" && walked.as_os_str().is_empty(),
                        "a/b" => stage == "parent-watched" && walked == Path::new("a"),
                        "a/b/file" => stage == "leaf-opened",
                        _ => unreachable!(),
                    };
                    if !trigger || swapped_once {
                        return;
                    }
                    swapped_once = true;
                    let named = fixture.workspace_path.join(swapped);
                    let detached = fixture.root.join("aba-detached");
                    let target = match swapped {
                        "a" => &outside,
                        "a/b" => &outside,
                        "a/b/file" => &outside.join("file"),
                        _ => unreachable!(),
                    };
                    fs::rename(&named, &detached).unwrap();
                    std::os::unix::fs::symlink(target, &named).unwrap();
                    fs::remove_file(&named).unwrap();
                    fs::rename(&detached, &named).unwrap();
                },
            );
            assert!(swapped_once, "ABA hook did not run for {swapped}");
            assert!(
                matches!(
                    result,
                    Err(PathAuthError::Revision(RevisionError::ScanRace { .. }))
                        | Err(PathAuthError::ObjectChanged(_))
                ),
                "transient {swapped} swap issued read authority"
            );
        });
        assert_eq!(
            fs::read(fixture.workspace_path.join("a/b/file")).unwrap(),
            b"inside"
        );
    }
}

#[test]
fn destination_parent_swap_never_turns_authorization_into_publication() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.workspace_path.join("parent")).unwrap();
    fixture.transaction(EditLimits::default(), |authorizer| {
        let capability = authorizer.authorize_create("parent/new").unwrap();
        let original_parent = capability.binding().object_identity().unwrap().inode();
        let detached = fixture.root.join("detached-parent");
        fs::rename(fixture.workspace_path.join("parent"), &detached).unwrap();
        fs::create_dir(fixture.workspace_path.join("parent")).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(&detached).unwrap().ino(), original_parent);
        assert!(!detached.join("new").exists());
        assert!(!fixture.workspace_path.join("parent/new").exists());
    });
}

fn recreate_same_tree(root: &Path, swapped: &str) {
    match swapped {
        "a" => {
            fs::create_dir_all(root.join("a/b")).unwrap();
            fs::write(root.join("a/b/file"), b"original").unwrap();
        }
        "a/b" => {
            fs::create_dir_all(root.join("a/b")).unwrap();
            fs::write(root.join("a/b/file"), b"original").unwrap();
        }
        "a/b/file" => fs::write(root.join("a/b/file"), b"original").unwrap(),
        _ => unreachable!(),
    }
}

fn inode_of_detached(detached: &Path, swapped: &str) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let original = match swapped {
        "a" => detached.join("b/file"),
        "a/b" => detached.join("file"),
        "a/b/file" => detached.to_owned(),
        _ => unreachable!(),
    };
    fs::metadata(original).unwrap().ino()
}

fn is_stale_or_changed(error: &PathAuthError) -> bool {
    matches!(
        error,
        PathAuthError::ObjectChanged(_)
            | PathAuthError::Revision(RevisionError::StaleRevision { .. })
            | PathAuthError::Revision(RevisionError::ScanRace { .. })
    )
}

#[test]
fn authorizer_and_read_capability_reject_after_revision_changes() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace_path.join("file"), b"old").unwrap();
    fixture.transaction(EditLimits::default(), |authorizer| {
        let capability = authorizer.authorize_read("file").unwrap();
        fs::write(fixture.workspace_path.join("file"), b"changed").unwrap();
        assert!(matches!(
            authorizer.authorize_read("file"),
            Err(PathAuthError::Revision(RevisionError::StaleRevision { .. }))
        ));
        assert!(matches!(
            authorizer.read(
                capability,
                1024,
                1024,
                Instant::now() + Duration::from_secs(1)
            ),
            Err(PathAuthError::Revision(RevisionError::StaleRevision { .. }))
                | Err(PathAuthError::Revision(RevisionError::ScanRace { .. }))
        ));
    });
}

#[test]
fn authorizer_creation_reconciles_mutation_after_guard_acquisition() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace_path.join("file"), b"old").unwrap();
    let revision = fixture.workspace.current_revision().unwrap();
    let mut guard = fixture.workspace.mutation_guard(revision.id()).unwrap();
    fs::write(fixture.workspace_path.join("file"), b"changed").unwrap();
    assert!(matches!(
        PathAuthorizer::new(
            &mut guard,
            revision.id(),
            revision.epoch(),
            EditLimits::default(),
        ),
        Err(PathAuthError::Revision(RevisionError::StaleRevision { .. }))
    ));
}

#[test]
fn stale_guard_epoch_root_and_cross_root_substitution_are_denied() {
    let first = Fixture::new();
    fs::write(first.workspace_path.join("file"), b"first").unwrap();
    let stale = first.workspace.current_revision().unwrap();
    fs::write(first.workspace_path.join("file"), b"changed").unwrap();
    assert!(matches!(
        first.workspace.mutation_guard(stale.id()),
        Err(RevisionError::StaleRevision { .. })
    ));

    let current = first.workspace.current_revision().unwrap();
    let epoch_source = Fixture::new();
    let other_epoch = epoch_source.workspace.current_revision().unwrap().epoch();
    let mut guard = first.workspace.mutation_guard(current.id()).unwrap();
    assert!(matches!(
        PathAuthorizer::new(&mut guard, current.id(), other_epoch, EditLimits::default()),
        Err(PathAuthError::StaleEpoch { .. })
    ));
    drop(guard);

    let second = Fixture::new();
    fs::write(second.workspace_path.join("file"), b"second").unwrap();
    let first_revision = first.workspace.current_revision().unwrap();
    let second_revision = second.workspace.current_revision().unwrap();
    let mut first_guard = first.workspace.mutation_guard(first_revision.id()).unwrap();
    let mut second_guard = second
        .workspace
        .mutation_guard(second_revision.id())
        .unwrap();
    let mut first_auth = PathAuthorizer::new(
        &mut first_guard,
        first_revision.id(),
        first_revision.epoch(),
        EditLimits::default(),
    )
    .unwrap();
    let mut second_auth = PathAuthorizer::new(
        &mut second_guard,
        second_revision.id(),
        second_revision.epoch(),
        EditLimits::default(),
    )
    .unwrap();
    let foreign = second_auth.authorize_read("file").unwrap();
    assert!(matches!(
        first_auth.read(foreign, 1024, 1024, Instant::now() + Duration::from_secs(1)),
        Err(PathAuthError::CrossGuard)
    ));

    drop(first_auth);
    drop(second_auth);
    drop(first_guard);
    drop(second_guard);
    let root_revision = first.workspace.current_revision().unwrap();
    let retained = first.root.join("retained-workspace");
    fs::rename(&first.workspace_path, &retained).unwrap();
    fs::create_dir(&first.workspace_path).unwrap();
    let result = first.workspace.mutation_guard(root_revision.id());
    assert!(result.is_err());
}

#[test]
fn hardlinks_and_special_files_are_rejected_after_guard_acquisition() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace_path.join("file"), b"bytes").unwrap();
    fixture.transaction(EditLimits::default(), |authorizer| {
        fs::hard_link(
            fixture.workspace_path.join("file"),
            fixture.workspace_path.join("alias"),
        )
        .unwrap();
        assert!(matches!(
            authorizer.authorize_replace("file"),
            Err(PathAuthError::Revision(RevisionError::Hardlink(_)))
        ));
    });

    let fixture = Fixture::new();
    fixture.transaction(EditLimits::default(), |authorizer| {
        let fifo = fixture.workspace_path.join("fifo");
        let fifo_name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            authorizer.authorize_read("fifo"),
            Err(PathAuthError::Revision(RevisionError::UnsupportedEntry(_)))
        ));
    });

    let fixture = Fixture::new();
    fixture.transaction(EditLimits::default(), |authorizer| {
        let socket = fixture.workspace_path.join("socket");
        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(matches!(
            authorizer.authorize_delete("socket"),
            Err(PathAuthError::Revision(RevisionError::UnsupportedEntry(_)))
        ));
    });
}

#[test]
fn hardlinked_file_at_current_revision_is_rejected_as_hardlink() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace_path.join("file"), b"bytes").unwrap();
    fixture.transaction(EditLimits::default(), |authorizer| {
        fs::hard_link(
            fixture.workspace_path.join("file"),
            fixture.root.join("outside-alias"),
        )
        .unwrap();
        assert!(matches!(
            authorizer.authorize_replace("file"),
            Err(PathAuthError::Revision(RevisionError::Hardlink(_)))
        ));
    });
}

#[test]
fn directory_and_file_work_are_bounded_by_edit_limits() {
    let fixture = Fixture::new();
    for index in 0..1024 {
        fs::write(
            fixture.workspace_path.join(format!("entry-{index:02}")),
            b"x",
        )
        .unwrap();
    }
    fixture.transaction(
        EditLimits {
            max_authorization_entries: 8,
            ..EditLimits::default()
        },
        |authorizer| {
            assert!(matches!(
                authorizer.authorize_create("absent"),
                Err(PathAuthError::LimitExceeded(PathAuthLimit::Entries))
            ));
        },
    );

    fixture.transaction(
        EditLimits {
            max_authorization_name_bytes: 8,
            ..EditLimits::default()
        },
        |authorizer| {
            assert!(matches!(
                authorizer.authorize_create("absent"),
                Err(PathAuthError::LimitExceeded(PathAuthLimit::NameBytes))
            ));
        },
    );

    fixture.transaction(
        EditLimits {
            max_authorization_memory_bytes: 4,
            ..EditLimits::default()
        },
        |authorizer| {
            assert!(matches!(
                authorizer.authorize_create("absent"),
                Err(PathAuthError::LimitExceeded(PathAuthLimit::Memory))
            ));
        },
    );

    let fixture = Fixture::new();
    fs::write(
        fixture.workspace_path.join("large"),
        vec![b'x'; 1024 * 1024],
    )
    .unwrap();
    fixture.transaction(
        EditLimits {
            max_content_bytes: 64,
            ..EditLimits::default()
        },
        |authorizer| {
            let capability = authorizer.authorize_read("large").unwrap();
            assert!(matches!(
                authorizer.read(
                    capability,
                    usize::MAX,
                    usize::MAX,
                    Instant::now() + Duration::from_secs(1)
                ),
                Err(PathAuthError::LimitExceeded(PathAuthLimit::Content))
            ));
        },
    );
}

#[test]
fn authorized_reads_precharge_explicit_byte_memory_and_deadline_bounds() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace_path.join("file"), b"bytes").unwrap();
    fixture.transaction(EditLimits::default(), |authorizer| {
        let read = authorizer.authorize_read("file").unwrap();
        assert!(matches!(
            authorizer.read(read, 4, usize::MAX, Instant::now() + Duration::from_secs(1)),
            Err(PathAuthError::LimitExceeded(PathAuthLimit::ReadBytes))
        ));

        let read = authorizer.authorize_read("file").unwrap();
        assert!(matches!(
            authorizer.read(read, usize::MAX, 4, Instant::now() + Duration::from_secs(1)),
            Err(PathAuthError::LimitExceeded(PathAuthLimit::Memory))
        ));

        let read = authorizer.authorize_read("file").unwrap();
        assert!(matches!(
            authorizer.read(read, usize::MAX, usize::MAX, Instant::now()),
            Err(PathAuthError::LimitExceeded(PathAuthLimit::Time))
                | Err(PathAuthError::Revision(RevisionError::LimitExceeded(_)))
        ));
    });
}

#[test]
fn deadline_is_cooperative_and_checked_before_filesystem_work() {
    let fixture = Fixture::new();
    fs::write(fixture.workspace_path.join("file"), b"bytes").unwrap();
    let revision = fixture.workspace.current_revision().unwrap();
    let mut guard = fixture.workspace.mutation_guard(revision.id()).unwrap();
    let result = PathAuthorizer::new(
        &mut guard,
        revision.id(),
        revision.epoch(),
        EditLimits {
            max_authorization_time: Duration::from_nanos(1),
            ..EditLimits::default()
        },
    );
    assert!(matches!(
        result,
        Err(PathAuthError::Revision(RevisionError::LimitExceeded(_)))
            | Err(PathAuthError::LimitExceeded(PathAuthLimit::Time))
    ));

    let contract = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/operations/path-authorization.md"),
    )
    .unwrap();
    assert!(contract.contains("cooperative deadline checked between filesystem syscalls"));
    assert!(contract.contains("does not interrupt a blocked syscall"));
}

#[test]
fn raw_component_opens_are_isolated_and_enforce_required_flags() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/workspace/path_auth");
    let sys_path = root.join("unix/sys.rs");
    for path in [
        root.join("mod.rs"),
        root.join("unix.rs"),
        root.join("unavailable.rs"),
    ] {
        let source = fs::read_to_string(&path).unwrap();
        assert!(
            !source.contains("libc::openat"),
            "raw openat in {}",
            path.display()
        );
        assert!(
            !source.contains("libc::SYS_openat2") && !source.contains("libc::syscall"),
            "raw openat2 in {}",
            path.display()
        );
    }

    let compact: String = fs::read_to_string(sys_path)
        .unwrap()
        .split_whitespace()
        .collect();
    for required in [
        "letflags=requested_flags|libc::O_NOFOLLOW|libc::O_CLOEXEC;",
        "libc::openat(directory.as_raw_fd(),name.as_ptr(),flags)",
        "how.flags=flagsasu64;",
        "how.resolve=libc::RESOLVE_BENEATH|libc::RESOLVE_NO_SYMLINKS;",
    ] {
        assert!(
            compact.contains(required),
            "missing syscall invariant {required}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn mount_crossing_is_denied_when_bind_mounting_is_available() {
    struct Unmount(PathBuf);
    impl Drop for Unmount {
        fn drop(&mut self) {
            let path = CString::new(self.0.as_os_str().as_bytes()).unwrap();
            unsafe { libc::umount2(path.as_ptr(), libc::MNT_DETACH) };
        }
    }

    let fixture = Fixture::new();
    let outside = fixture.root.join("mount-source");
    let mounted = fixture.workspace_path.join("mounted");
    fs::create_dir(&outside).unwrap();
    fs::create_dir(&mounted).unwrap();
    fs::write(outside.join("file"), b"outside").unwrap();
    fixture.transaction(EditLimits::default(), |authorizer| {
        let source = CString::new(outside.as_os_str().as_bytes()).unwrap();
        let target = CString::new(mounted.as_os_str().as_bytes()).unwrap();
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
            authorizer.authorize_read("mounted/file"),
            Err(PathAuthError::Revision(RevisionError::MountBoundary(_)))
                | Err(PathAuthError::MountBoundary(_))
        ));
    });
}
