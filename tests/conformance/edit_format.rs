#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Barrier, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use kit::{
    domain::events::ContentDigest,
    test_support::{
        SyntaxTestAction, syntax_executor, syntax_executor_gate_second,
        syntax_executor_with_capture,
    },
    workspace::{
        edit::{
            format::{
                NATIVE_JSON_VERSION, NATIVE_TEXT_VERSION, RUST_GRAMMAR_VERSION, SyntaxRequirement,
            },
            ir::{
                ByteRange, EditIr, EditLimits, EditOperation, ExecutableMode, RevisionToken,
                RootRelativePath, TextContent,
            },
            stage::{StageError, StageLimit, StageLimits, stage},
            validate::validate,
        },
        revision::ManagedWorkspace,
    },
};

struct Fixture {
    root: PathBuf,
    workspace_path: PathBuf,
    workspace: ManagedWorkspace,
}

impl Fixture {
    fn new(files: &[(&str, &[u8], u32)]) -> Self {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-edit-format-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let workspace_path = root.join("workspace");
        fs::create_dir_all(&workspace_path).unwrap();
        for (path, content, mode) in files {
            let path = workspace_path.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(*mode)).unwrap();
        }
        let workspace = ManagedWorkspace::open(&workspace_path).unwrap();
        Self {
            root,
            workspace_path,
            workspace,
        }
    }

    fn revision(&self) -> RevisionToken {
        RevisionToken::parse(self.workspace.current_revision().unwrap().id().to_string()).unwrap()
    }

    fn plan(
        &self,
        operations: Vec<EditOperation>,
    ) -> kit::workspace::edit::validate::ValidatedPlan<'_> {
        let ir = EditIr::new(self.revision(), operations, EditLimits::default()).unwrap();
        validate(&self.workspace, &ir, EditLimits::default()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn path(value: &str) -> RootRelativePath {
    RootRelativePath::parse(value, EditLimits::default().max_path_bytes).unwrap()
}

fn text(bytes: &[u8]) -> TextContent {
    TextContent::from_bytes(bytes).unwrap()
}

fn digest(bytes: &[u8]) -> ContentDigest {
    ContentDigest::parse(&format!("blake3:{}", blake3::hash(bytes).to_hex())).unwrap()
}

fn replace_operation(name: &str, before: &[u8], after: &[u8]) -> EditOperation {
    EditOperation::ReplaceRange {
        path: path(name),
        base_digest: digest(before),
        range: ByteRange::new(0, before.len()).unwrap(),
        expected: text(before),
        replacement: text(after),
        executable: ExecutableMode::Preserve,
    }
}

fn single_replace<'a>(
    fixture: &'a Fixture,
    after: &[u8],
) -> kit::workspace::edit::validate::ValidatedPlan<'a> {
    fixture.plan(vec![replace_operation("changed.json", b"{}\n", after)])
}

#[test]
fn stage_applies_all_operations_preserves_tree_modes_and_newlines_without_workspace_visibility() {
    let fixture = Fixture::new(&[
        ("src/delete.txt", b"delete\n", 0o640),
        ("src/move.txt", b"#!/bin/sh\r\n", 0o755),
        ("src/replace.txt", b"old\r\n", 0o640),
        ("unrelated/nested.bin", b"\0\xff", 0o600),
    ]);
    let operations = vec![
        EditOperation::AddFile {
            path: path("src/added.txt"),
            content: text(b"new\r\n"),
            executable: true,
        },
        EditOperation::DeleteFile {
            path: path("src/delete.txt"),
            base_digest: digest(b"delete\n"),
        },
        EditOperation::MoveFile {
            from: path("src/move.txt"),
            to: path("src/moved.txt"),
            base_digest: digest(b"#!/bin/sh\r\n"),
        },
        replace_operation("src/replace.txt", b"old\r\n", b"new\r\n"),
    ];
    let staged = stage(
        fixture.plan(operations.clone()),
        StageLimits::default(),
        &[],
        &mut [],
    )
    .unwrap();

    assert_eq!(staged.changes().len(), 5);
    assert_eq!(
        staged.read_file(&path("src/added.txt"), 1024).unwrap(),
        b"new\r\n"
    );
    assert_eq!(
        staged.read_file(&path("src/moved.txt"), 1024).unwrap(),
        b"#!/bin/sh\r\n"
    );
    assert_eq!(
        staged.read_file(&path("src/replace.txt"), 1024).unwrap(),
        b"new\r\n"
    );
    assert_eq!(
        staged
            .read_file(&path("unrelated/nested.bin"), 1024)
            .unwrap(),
        b"\0\xff"
    );
    assert!(staged.read_file(&path("src/delete.txt"), 1024).is_err());
    let modes = staged
        .changes()
        .iter()
        .map(|change| (change.path().as_str(), change.after_mode()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(modes["src/added.txt"], Some(0o755));
    assert_eq!(modes["src/moved.txt"], Some(0o755));
    assert_eq!(modes["src/replace.txt"], Some(0o640));

    for _ in 0..100 {
        assert_eq!(
            fs::read(fixture.workspace_path.join("src/replace.txt")).unwrap(),
            b"old\r\n"
        );
        assert!(!fixture.workspace_path.join("src/added.txt").exists());
        assert!(!fixture.workspace_path.join("src/moved.txt").exists());
        assert!(fixture.workspace_path.join("src/move.txt").exists());
    }
    let first_digest = staged.digest().to_owned();
    drop(staged);
    let second = stage(
        fixture.plan(operations),
        StageLimits::default(),
        &[],
        &mut [],
    )
    .unwrap();
    assert_eq!(second.digest(), first_digest);
}

#[test]
fn same_process_manager_open_does_not_recover_a_live_stage() {
    let fixture = Fixture::new(&[("changed.json", b"{}\n", 0o644)]);
    let staged = stage(
        single_replace(&fixture, b"{\"staged\":true}\n"),
        StageLimits::default(),
        &[],
        &mut [],
    )
    .unwrap();
    thread::scope(|scope| {
        let workspace_path = fixture.workspace_path.clone();
        let start = Arc::new(Barrier::new(2));
        let opener_start = Arc::clone(&start);
        let opener = scope.spawn(move || {
            opener_start.wait();
            ManagedWorkspace::open(workspace_path).unwrap()
        });
        start.wait();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            staged.read_file(&path("changed.json"), 1024).unwrap(),
            b"{\"staged\":true}\n"
        );
        staged.close().unwrap();
        drop(opener.join().unwrap());
    });
}

#[test]
fn cross_process_owner_takeover_recovers_abandoned_stage() {
    let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "kit-edit-takeover-{}",
        blake3::hash(format!("{}-{:#?}", std::process::id(), Instant::now()).as_bytes()).to_hex()
    ));
    let workspace_path = root.join("workspace");
    fs::create_dir_all(&workspace_path).unwrap();
    fs::write(workspace_path.join("changed.json"), b"{}\n").unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "edit_format::stage_recovery_subprocess_worker",
            "--nocapture",
        ])
        .env("KIT_STAGE_RECOVERY_WORKSPACE", &workspace_path)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(has_stage_allocation(&root));
    let manager = ManagedWorkspace::open(&workspace_path).unwrap();
    assert!(!has_stage_allocation(&root));
    drop(manager);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn stage_recovery_subprocess_worker() {
    let Ok(workspace_path) = std::env::var("KIT_STAGE_RECOVERY_WORKSPACE") else {
        return;
    };
    let workspace = ManagedWorkspace::open(workspace_path).unwrap();
    let revision =
        RevisionToken::parse(workspace.current_revision().unwrap().id().to_string()).unwrap();
    let ir = EditIr::new(
        revision,
        vec![replace_operation(
            "changed.json",
            b"{}\n",
            b"{\"abandoned\":true}\n",
        )],
        EditLimits::default(),
    )
    .unwrap();
    let staged = stage(
        validate(&workspace, &ir, EditLimits::default()).unwrap(),
        StageLimits::default(),
        &[],
        &mut [],
    )
    .unwrap();
    std::mem::forget(staged);
}

fn has_stage_allocation(root: &Path) -> bool {
    fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .any(|state| {
            state.file_name().to_string_lossy().ends_with(".staging")
                && fs::read_dir(state.path())
                    .unwrap()
                    .filter_map(Result::ok)
                    .any(|entry| {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        stage_allocation_name(&name)
                    })
        })
}

#[test]
fn syntax_is_versioned_sealed_stage_only_and_required_unavailable_is_typed() {
    let fixture = Fixture::new(&[("changed.json", b"{}\n", 0o644)]);
    let json =
        SyntaxRequirement::new(path("changed.json"), "json", NATIVE_JSON_VERSION, true).unwrap();
    stage(
        single_replace(&fixture, b"{\"ok\":true}\n"),
        StageLimits::default(),
        std::slice::from_ref(&json),
        &mut [],
    )
    .unwrap();
    assert!(matches!(
        stage(
            single_replace(&fixture, b"{broken}\n"),
            StageLimits::default(),
            &[json],
            &mut [],
        ),
        Err(StageError::SyntaxFailed(path)) if path.as_str() == "changed.json"
    ));

    let rust_fixture = Fixture::new(&[("changed.rs", b"fn old() {}\n", 0o644)]);
    let rust_plan = |after: &[u8]| {
        rust_fixture.plan(vec![replace_operation(
            "changed.rs",
            b"fn old() {}\n",
            after,
        )])
    };
    let requirement =
        SyntaxRequirement::new(path("changed.rs"), "rust", RUST_GRAMMAR_VERSION, true).unwrap();
    stage(
        rust_plan(b"fn staged() {}\n"),
        StageLimits::default(),
        std::slice::from_ref(&requirement),
        &mut [],
    )
    .unwrap();
    assert!(matches!(
        stage(
            rust_plan(b"fn staged(\n"),
            StageLimits::default(),
            std::slice::from_ref(&requirement),
            &mut [],
        ),
        Err(StageError::SyntaxFailed(_))
    ));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut executor =
        syntax_executor_with_capture("rust", RUST_GRAMMAR_VERSION, Arc::clone(&seen));
    let mut executors = [&mut executor];
    stage(
        rust_plan(b"fn staged() {}\n"),
        StageLimits::default(),
        &[requirement],
        &mut executors,
    )
    .unwrap();
    assert_eq!(&*seen.lock().unwrap(), b"fn staged() {}\n");
    assert_eq!(
        fs::read(fixture.workspace_path.join("changed.json")).unwrap(),
        b"{}\n"
    );
}

#[test]
fn stuck_syntax_executor_times_out() {
    let fixture = Fixture::new(&[("changed.rs", b"fn old() {}\n", 0o644)]);
    let requirement =
        SyntaxRequirement::new(path("changed.rs"), "rust", RUST_GRAMMAR_VERSION, true).unwrap();
    let mut executor = syntax_executor("rust", RUST_GRAMMAR_VERSION, SyntaxTestAction::Stuck);
    let result = stage(
        fixture.plan(vec![replace_operation(
            "changed.rs",
            b"fn old() {}\n",
            b"fn staged() {}\n",
        )]),
        StageLimits {
            max_time: Duration::from_millis(500),
            ..StageLimits::default()
        },
        &[requirement],
        &mut [&mut executor],
    );
    assert!(
        matches!(&result, Err(StageError::SyntaxTimeout(path)) if path.as_str() == "changed.rs"),
        "unexpected stuck syntax result: {:?}",
        result.as_ref().err()
    );
}

#[test]
fn stage_rejects_a_final_snapshot_mutation() {
    for _ in 0..4 {
        assert_final_snapshot_mutation_rejected();
    }
}

#[test]
#[ignore = "exact opt-in filesystem stress; run serially with --ignored --exact --test-threads=1"]
fn final_mutation_is_rejected_500_iterations_parallel() {
    let next = Arc::new(AtomicUsize::new(0));
    thread::scope(|scope| {
        for _ in 0..8 {
            let next = Arc::clone(&next);
            scope.spawn(move || {
                loop {
                    let iteration = next.fetch_add(1, Ordering::Relaxed);
                    if iteration >= 500 {
                        break;
                    }
                    assert_final_snapshot_mutation_rejected();
                }
            });
        }
    });
}

fn assert_final_snapshot_mutation_rejected() {
    let fixture = Fixture::new(&[("changed.rs", b"fn old() {}\n", 0o644)]);
    let requirement =
        SyntaxRequirement::new(path("changed.rs"), "rust", RUST_GRAMMAR_VERSION, true).unwrap();
    let (mut executor, entered, release) =
        syntax_executor_gate_second("rust", RUST_GRAMMAR_VERSION);
    thread::scope(|scope| {
        let root = fixture.root.clone();
        let mutator = scope.spawn(move || {
            entered.recv().unwrap();
            let file = frozen_stage_file(&root, "changed.rs").expect("final staged file");
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
            fs::write(&file, b"fn raced() {}\n").unwrap();
            fs::write(&file, b"fn staged() {}\n").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o400)).unwrap();
            release.send(()).unwrap();
        });
        let plan = fixture.plan(vec![replace_operation(
            "changed.rs",
            b"fn old() {}\n",
            b"fn staged() {}\n",
        )]);
        let limits = StageLimits {
            max_time: Duration::from_secs(2),
            ..StageLimits::default()
        };
        let result = stage(plan, limits, &[requirement], &mut [&mut executor]);
        mutator.join().unwrap();
        assert!(matches!(result, Err(StageError::StageChanged)));
    });
}

fn frozen_stage_file(root: &Path, relative: &str) -> Option<PathBuf> {
    for state in fs::read_dir(root).ok()? {
        let state = state.ok()?;
        if !state.file_name().to_string_lossy().ends_with(".staging") {
            continue;
        }
        for allocation in fs::read_dir(state.path()).ok()? {
            let allocation = allocation.ok()?;
            let name = allocation.file_name();
            if !stage_allocation_name(&name.to_string_lossy()) {
                continue;
            }
            let file = allocation.path().join("final").join(relative);
            if fs::metadata(&file).ok()?.permissions().mode() & 0o777 == 0o400 {
                return Some(file);
            }
        }
    }
    None
}

fn stage_allocation_name(name: &str) -> bool {
    name.strip_prefix(".kit-stage-drop-")
        .or_else(|| name.strip_prefix(".kit-stage-"))
        .is_some_and(|suffix| {
            suffix.len() == 32
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

#[test]
fn unknown_extensions_cannot_claim_native_text_or_an_unrelated_grammar() {
    let fixture = Fixture::new(&[("changed.unknown", b"old\n", 0o644)]);
    for (language, version) in [
        ("text", NATIVE_TEXT_VERSION),
        ("rust", RUST_GRAMMAR_VERSION),
    ] {
        let requirement =
            SyntaxRequirement::new(path("changed.unknown"), language, version, true).unwrap();
        assert!(matches!(
            stage(
                fixture.plan(vec![replace_operation(
                    "changed.unknown",
                    b"old\n",
                    b"new\n",
                )]),
                StageLimits::default(),
                &[requirement],
                &mut [],
            ),
            Err(StageError::SyntaxUnavailable(path)) if path.as_str() == "changed.unknown"
        ));
    }
}

#[test]
fn state_digest_is_deterministic_and_evidence_digest_is_separate() {
    let fixture = Fixture::new(&[("changed.json", b"{}\n", 0o644)]);
    let first = stage(
        single_replace(&fixture, b"{\"staged\":true}\n"),
        StageLimits::default(),
        &[],
        &mut [],
    )
    .unwrap();
    let state = first.state_digest().to_owned();
    let evidence = first.evidence_digest().to_owned();
    assert_eq!(first.digest(), state);
    assert_ne!(state, evidence);
    first.close().unwrap();
    let second = stage(
        single_replace(&fixture, b"{\"staged\":true}\n"),
        StageLimits::default(),
        &[],
        &mut [],
    )
    .unwrap();
    assert_eq!(second.state_digest(), state);
    assert_eq!(second.evidence_digest(), evidence);
}

#[test]
fn stage_rejects_setid_and_user_xattr_metadata() {
    let setid = Fixture::new(&[("changed.json", b"{}\n", 0o644)]);
    fs::set_permissions(
        setid.workspace_path.join("changed.json"),
        fs::Permissions::from_mode(0o4644),
    )
    .unwrap();
    assert!(matches!(
        stage(
            single_replace(&setid, b"{\"changed\":true}\n"),
            StageLimits::default(),
            &[],
            &mut [],
        ),
        Err(StageError::UnsafeSource)
    ));

    let directory = Fixture::new(&[("dir/changed.json", b"{}\n", 0o644)]);
    set_user_xattr(&directory.workspace_path.join("dir"));
    let directory_result = stage(
        directory.plan(vec![replace_operation(
            "dir/changed.json",
            b"{}\n",
            b"{\"changed\":true}\n",
        )]),
        StageLimits::default(),
        &[],
        &mut [],
    );
    assert!(
        matches!(&directory_result, Err(StageError::UnsafeSource)),
        "unexpected directory metadata result: {}",
        directory_result.err().unwrap()
    );

    let xattr = Fixture::new(&[("changed.json", b"{}\n", 0o644)]);
    set_user_xattr(&xattr.workspace_path.join("changed.json"));
    assert!(matches!(
        stage(
            single_replace(&xattr, b"{\"changed\":true}\n"),
            StageLimits::default(),
            &[],
            &mut [],
        ),
        Err(StageError::UnsafeSource)
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn stage_rejects_directory_acls() {
    let fixture = Fixture::new(&[("dir/changed.json", b"{}\n", 0o644)]);
    let user = std::env::var("USER").unwrap();
    assert!(
        std::process::Command::new("chmod")
            .arg("+a")
            .arg(format!("user:{user} allow read"))
            .arg(fixture.workspace_path.join("dir"))
            .status()
            .unwrap()
            .success()
    );
    assert!(matches!(
        stage(
            fixture.plan(vec![replace_operation(
                "dir/changed.json",
                b"{}\n",
                b"{\"changed\":true}\n",
            )]),
            StageLimits::default(),
            &[],
            &mut [],
        ),
        Err(StageError::UnsafeSource)
    ));
}

#[cfg(target_os = "macos")]
fn set_user_xattr(path: &Path) {
    assert!(
        std::process::Command::new("xattr")
            .args(["-w", "com.kit.review", "1"])
            .arg(path)
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(target_os = "linux")]
fn set_user_xattr(path: &Path) {
    use std::{ffi::CString, os::unix::ffi::OsStrExt as _};

    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let value = b"1";
    assert_eq!(
        unsafe {
            libc::setxattr(
                path.as_ptr(),
                c"user.kit.review".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        },
        0
    );
}

#[test]
fn staging_bounds_fail_typed_and_failed_stages_leave_no_allocations() {
    let fixture = Fixture::new(&[
        ("changed.json", b"{}\n", 0o644),
        ("unrelated.txt", b"same\n", 0o640),
    ]);
    for (limits, expected) in [
        (
            StageLimits {
                max_entries: 1,
                ..StageLimits::default()
            },
            StageLimit::Entries,
        ),
        (
            StageLimits {
                max_total_bytes: 2,
                ..StageLimits::default()
            },
            StageLimit::TotalBytes,
        ),
        (
            StageLimits {
                max_file_bytes: 2,
                ..StageLimits::default()
            },
            StageLimit::FileBytes,
        ),
        (
            StageLimits {
                max_time: Duration::from_nanos(1),
                ..StageLimits::default()
            },
            StageLimit::Time,
        ),
        (
            StageLimits {
                max_name_bytes: 1,
                ..StageLimits::default()
            },
            StageLimit::NameBytes,
        ),
        (
            StageLimits {
                max_path_bytes: 1,
                ..StageLimits::default()
            },
            StageLimit::PathBytes,
        ),
        (
            StageLimits {
                max_metadata_bytes: 1,
                ..StageLimits::default()
            },
            StageLimit::MetadataMemory,
        ),
    ] {
        let result = stage(single_replace(&fixture, b"x\n"), limits, &[], &mut []);
        let actual = result.as_ref().err().map(ToString::to_string);
        assert!(
            matches!(result, Err(StageError::LimitExceeded(actual)) if actual == expected),
            "unexpected staging limit result for {expected:?}: {actual:?}"
        );
    }
}

#[test]
fn stage_contract_is_documented_as_opaque_and_non_host() {
    let documentation = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/operations/edit-staging.md"),
    )
    .unwrap();
    for text in [
        "cannot be serialized or forged",
        "daemon host",
        "mode `0700`",
    ] {
        assert!(
            documentation.contains(text),
            "missing contract text: {text}"
        );
    }
}
