#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use kit::{
    domain::events::ContentDigest,
    workspace::{
        edit::{
            ir::{
                ByteRange, EditIr, EditLimits, EditOperation, ExecutableMode,
                FilesystemIdentityPolicy, Newline, RevisionToken, RootRelativePath, TextContent,
            },
            normalize::{ModelEditFormat, NormalizationContext, normalize},
            validate::{PlannedEffect, UnsafePathKind, ValidationError, ValidationLimit, validate},
        },
        revision::ManagedWorkspace,
    },
};

struct Fixture {
    root: PathBuf,
    workspace_path: PathBuf,
    workspace: Arc<ManagedWorkspace>,
}

impl Fixture {
    fn new(files: &[(&str, &[u8])]) -> Self {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-edit-validate-{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let workspace_path = root.join("workspace");
        fs::create_dir_all(&workspace_path).unwrap();
        for (path, content) in files {
            let path = workspace_path.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
        let workspace = Arc::new(ManagedWorkspace::open(&workspace_path).unwrap());
        Self {
            root,
            workspace_path,
            workspace,
        }
    }

    fn revision(&self) -> RevisionToken {
        RevisionToken::parse(self.workspace.current_revision().unwrap().id().to_string()).unwrap()
    }

    fn ir(&self, operations: Vec<EditOperation>) -> EditIr {
        EditIr::new(self.revision(), operations, EditLimits::default()).unwrap()
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

fn replace(path_value: &str, current: &[u8], expected: &[u8]) -> EditOperation {
    EditOperation::ReplaceRange {
        path: path(path_value),
        base_digest: digest(current),
        range: ByteRange::new(0, expected.len()).unwrap(),
        expected: text(expected),
        replacement: text(b"changed"),
        executable: ExecutableMode::Preserve,
    }
}

#[test]
fn all_operations_validate_as_one_deterministic_non_mutating_plan() {
    let fixture = Fixture::new(&[
        ("delete", b"delete me"),
        ("move", b"move me\n"),
        ("replace", b"one two\n"),
    ]);
    let revision = fixture.revision();
    let operations = vec![
        EditOperation::AddFile {
            path: path("added"),
            content: text(b"new\n"),
            executable: false,
        },
        EditOperation::DeleteFile {
            path: path("delete"),
            base_digest: digest(b"delete me"),
        },
        EditOperation::MoveFile {
            from: path("move"),
            to: path("moved"),
            base_digest: digest(b"move me\n"),
        },
        EditOperation::ReplaceRange {
            path: path("replace"),
            base_digest: digest(b"one two\n"),
            range: ByteRange::new(0, 3).unwrap(),
            expected: text(b"one"),
            replacement: text(b"ONE"),
            executable: ExecutableMode::Preserve,
        },
        EditOperation::ReplaceRange {
            path: path("replace"),
            base_digest: digest(b"one two\n"),
            range: ByteRange::new(4, 7).unwrap(),
            expected: text(b"two"),
            replacement: text(b"TWO"),
            executable: ExecutableMode::Preserve,
        },
    ];
    let ir = EditIr::new(revision, operations, EditLimits::default()).unwrap();
    let first = validate(&fixture.workspace, &ir, EditLimits::default()).unwrap();
    let first_digest = first.digest().to_owned();
    assert_eq!(first.effects().len(), 4);
    assert_eq!(first.changed_files().len(), 5);
    assert!(matches!(
        &first.effects()[3],
        PlannedEffect::Replace { ranges, after, .. }
            if ranges.len() == 2 && after.content() == b"ONE TWO\n"
    ));
    assert_eq!(
        fs::read(fixture.workspace_path.join("delete")).unwrap(),
        b"delete me"
    );
    assert_eq!(
        fs::read(fixture.workspace_path.join("move")).unwrap(),
        b"move me\n"
    );
    assert_eq!(
        fs::read(fixture.workspace_path.join("replace")).unwrap(),
        b"one two\n"
    );
    assert!(!fixture.workspace_path.join("added").exists());
    assert!(!fixture.workspace_path.join("moved").exists());
    drop(first);

    let second = validate(&fixture.workspace, &ir, EditLimits::default()).unwrap();
    assert_eq!(second.digest(), first_digest);
}

#[test]
fn eight_edge_classes_have_safe_typed_outcomes() {
    let stale = Fixture::new(&[("file", b"old")]);
    let stale_ir = stale.ir(vec![EditOperation::AddFile {
        path: path("new"),
        content: text(b"x"),
        executable: false,
    }]);
    fs::write(stale.workspace_path.join("file"), b"new").unwrap();
    assert!(matches!(
        validate(&stale.workspace, &stale_ir, EditLimits::default()),
        Err(ValidationError::StaleRevision)
    ));

    let duplicate = Fixture::new(&[("file", b"same\nsame\n")]);
    let duplicate_ir = duplicate.ir(vec![replace("file", b"same\nsame\n", b"same\n")]);
    assert!(matches!(
        validate(&duplicate.workspace, &duplicate_ir, EditLimits::default()),
        Err(ValidationError::AmbiguousAnchor(_))
    ));

    let unicode = Fixture::new(&[("file", "éx".as_bytes())]);
    let unicode_ir = unicode.ir(vec![EditOperation::ReplaceRange {
        path: path("file"),
        base_digest: digest("éx".as_bytes()),
        range: ByteRange::new(1, 2).unwrap(),
        expected: text(b"x"),
        replacement: text(b"y"),
        executable: ExecutableMode::Preserve,
    }]);
    assert!(matches!(
        validate(&unicode.workspace, &unicode_ir, EditLimits::default()),
        Err(ValidationError::InvalidUnicode(_))
    ));

    let crlf = Fixture::new(&[("file", b"old\r\n")]);
    let crlf_ir = crlf.ir(vec![replace("file", b"old\r\n", b"old")]);
    assert!(matches!(
        validate(&crlf.workspace, &crlf_ir, EditLimits::default()),
        Err(ValidationError::NewlineMismatch(_))
    ));

    let no_final = Fixture::new(&[("file", b"tail")]);
    let no_final_ir = no_final.ir(vec![EditOperation::ReplaceRange {
        path: path("file"),
        base_digest: digest(b"tail"),
        range: ByteRange::new(0, 4).unwrap(),
        expected: TextContent::new("tai".to_owned(), Newline::Lf, true).unwrap(),
        replacement: text(b"next"),
        executable: ExecutableMode::Preserve,
    }]);
    assert!(matches!(
        validate(&no_final.workspace, &no_final_ir, EditLimits::default()),
        Err(ValidationError::FinalNewlineMismatch(_))
    ));

    let binary = Fixture::new(&[("file", b"a\0b")]);
    let binary_ir = binary.ir(vec![replace("file", b"a\0b", b"a")]);
    assert!(matches!(
        validate(&binary.workspace, &binary_ir, EditLimits::default()),
        Err(ValidationError::BinaryFile(_))
    ));

    let alias = Fixture::new(&[("File", b"x")]);
    let alias_ir = alias.ir(vec![EditOperation::DeleteFile {
        path: path("file"),
        base_digest: digest(b"x"),
    }]);
    assert!(matches!(
        validate(&alias.workspace, &alias_ir, EditLimits::default()),
        Err(ValidationError::UnsafePath(UnsafePathKind::Alias))
    ));

    let external = Fixture::new(&[("file", b"old")]);
    let external_ir = external.ir(vec![replace("file", b"old", b"old")]);
    let mut changed = false;
    let result = kit::test_support::validate_edit_with_hook(
        &external.workspace,
        &external_ir,
        EditLimits::default(),
        |stage, _| {
            if stage == "after-read" && !changed {
                changed = true;
                fs::write(external.workspace_path.join("file"), b"external").unwrap();
            }
        },
    );
    assert!(matches!(
        result,
        Err(ValidationError::StaleRevision | ValidationError::ExternalEdit)
    ));
}

#[test]
fn edit_rejects_fuzzy_duplicate() {
    let fixture = Fixture::new(&[("file", b"same\nsame\n")]);
    let ir = fixture.ir(vec![replace("file", b"same\nsame\n", b"same\n")]);
    assert!(matches!(
        validate(&fixture.workspace, &ir, EditLimits::default()),
        Err(ValidationError::AmbiguousAnchor(_))
    ));
    assert_eq!(
        fs::read(fixture.workspace_path.join("file")).unwrap(),
        b"same\nsame\n"
    );
}

#[test]
fn symlink_special_and_hardlink_sources_fail_as_the_documented_unsafe_outcome() {
    let symlink = Fixture::new(&[("file", b"x")]);
    let ir = symlink.ir(vec![EditOperation::AddFile {
        path: path("new"),
        content: text(b"x"),
        executable: false,
    }]);
    std::os::unix::fs::symlink("file", symlink.workspace_path.join("link")).unwrap();
    assert!(matches!(
        validate(&symlink.workspace, &ir, EditLimits::default()),
        Err(ValidationError::UnsafePath(UnsafePathKind::Symlink))
    ));

    let hardlink = Fixture::new(&[("file", b"x")]);
    let ir = hardlink.ir(vec![EditOperation::AddFile {
        path: path("new"),
        content: text(b"x"),
        executable: false,
    }]);
    fs::hard_link(
        hardlink.workspace_path.join("file"),
        hardlink.workspace_path.join("link"),
    )
    .unwrap();
    assert!(matches!(
        validate(&hardlink.workspace, &ir, EditLimits::default()),
        Err(ValidationError::UnsafePath(UnsafePathKind::Hardlink))
    ));

    let special = Fixture::new(&[]);
    let ir = special.ir(vec![EditOperation::AddFile {
        path: path("new"),
        content: text(b"x"),
        executable: false,
    }]);
    let fifo = special.workspace_path.join("fifo");
    let fifo = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    assert!(matches!(
        validate(&special.workspace, &ir, EditLimits::default()),
        Err(ValidationError::UnsafePath(UnsafePathKind::Special))
    ));
}

#[test]
fn external_edits_before_and_after_each_authorized_read_are_never_silent() {
    for stage_to_change in ["before-read", "after-read", "validation-complete"] {
        let fixture = Fixture::new(&[("file", b"old")]);
        let ir = fixture.ir(vec![replace("file", b"old", b"old")]);
        let mut changed = false;
        let result = kit::test_support::validate_edit_with_hook(
            &fixture.workspace,
            &ir,
            EditLimits::default(),
            |stage, _| {
                if stage == stage_to_change && !changed {
                    changed = true;
                    fs::write(fixture.workspace_path.join("file"), b"external").unwrap();
                }
            },
        );
        assert!(
            matches!(
                result,
                Err(ValidationError::StaleRevision | ValidationError::ExternalEdit)
            ),
            "external edit at {stage_to_change} was not typed"
        );
        assert_eq!(
            fs::read(fixture.workspace_path.join("file")).unwrap(),
            b"external"
        );
    }
}

#[test]
fn stale_revision_is_checked_after_waiting_for_the_exclusive_guard() {
    let fixture = Fixture::new(&[("file", b"old")]);
    let ir = fixture.ir(vec![EditOperation::AddFile {
        path: path("new"),
        content: text(b"x"),
        executable: false,
    }]);
    let workspace = Arc::clone(&fixture.workspace);
    let revision = workspace.current_revision().unwrap().id();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let holder = {
        let workspace = Arc::clone(&workspace);
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        thread::spawn(move || {
            let _guard = workspace.mutation_guard(revision).unwrap();
            entered.wait();
            release.wait();
        })
    };
    entered.wait();
    fs::write(
        fixture.workspace_path.join("file"),
        b"changed while waiting",
    )
    .unwrap();
    let validation_started = Arc::new(Barrier::new(2));
    let validator = {
        let workspace = Arc::clone(&workspace);
        let validation_started = Arc::clone(&validation_started);
        thread::spawn(move || {
            validation_started.wait();
            validate(&workspace, &ir, EditLimits::default()).map(drop)
        })
    };
    validation_started.wait();
    thread::sleep(Duration::from_millis(20));
    release.wait();
    holder.join().unwrap();
    assert!(matches!(
        validator.join().unwrap(),
        Err(ValidationError::StaleRevision)
    ));
}

#[test]
fn validation_read_memory_and_time_bounds_are_typed() {
    let fixture = Fixture::new(&[("file", b"bounded")]);
    let ir = fixture.ir(vec![EditOperation::DeleteFile {
        path: path("file"),
        base_digest: digest(b"bounded"),
    }]);
    let read = EditLimits {
        max_validation_read_bytes: 3,
        ..EditLimits::default()
    };
    assert!(matches!(
        validate(&fixture.workspace, &ir, read),
        Err(ValidationError::LimitExceeded(ValidationLimit::ReadBytes))
    ));
    let memory = EditLimits {
        max_validation_memory_bytes: 3,
        ..EditLimits::default()
    };
    assert!(matches!(
        validate(&fixture.workspace, &ir, memory),
        Err(ValidationError::LimitExceeded(ValidationLimit::Memory))
    ));
    let time = EditLimits {
        max_validation_time: Duration::from_nanos(1),
        ..EditLimits::default()
    };
    assert!(matches!(
        validate(&fixture.workspace, &ir, time),
        Err(ValidationError::LimitExceeded(ValidationLimit::Time))
    ));
}

#[test]
fn active_ir_policy_and_limits_are_rejected_before_filesystem_authority() {
    let fixture = Fixture::new(&[]);
    let case_limits = EditLimits {
        identity_policy: FilesystemIdentityPolicy::CaseSensitive,
        ..EditLimits::default()
    };
    let ir = EditIr::new(
        fixture.revision(),
        vec![
            EditOperation::AddFile {
                path: path("Name"),
                content: text(b"x"),
                executable: false,
            },
            EditOperation::AddFile {
                path: path("name"),
                content: text(b"x"),
                executable: false,
            },
        ],
        case_limits,
    )
    .unwrap();
    let mut acquired = false;
    let result = kit::test_support::validate_edit_with_hook(
        &fixture.workspace,
        &ir,
        EditLimits::default(),
        |stage, _| acquired |= stage == "guard-acquired",
    );
    assert!(matches!(
        result,
        Err(ValidationError::IdentityPolicyMismatch)
    ));
    assert!(!acquired);

    acquired = false;
    let result = kit::test_support::validate_edit_with_hook(
        &fixture.workspace,
        &ir,
        case_limits,
        |stage, _| acquired |= stage == "guard-acquired",
    );
    assert!(matches!(
        result,
        Err(ValidationError::UnsafePath(UnsafePathKind::Alias))
    ));
    assert!(!acquired);

    let ir = fixture.ir(vec![EditOperation::AddFile {
        path: path("new"),
        content: text(b"x"),
        executable: false,
    }]);
    for (limits, expected) in [
        (
            EditLimits {
                max_operations: 0,
                ..EditLimits::default()
            },
            ValidationLimit::Operations,
        ),
        (
            EditLimits {
                max_path_bytes: 2,
                ..EditLimits::default()
            },
            ValidationLimit::Path,
        ),
        (
            EditLimits {
                max_content_bytes: 0,
                ..EditLimits::default()
            },
            ValidationLimit::Content,
        ),
    ] {
        acquired = false;
        let result = kit::test_support::validate_edit_with_hook(
            &fixture.workspace,
            &ir,
            limits,
            |stage, _| acquired |= stage == "guard-acquired",
        );
        assert!(
            matches!(result, Err(ValidationError::LimitExceeded(actual)) if actual == expected)
        );
        assert!(!acquired);
    }
}

#[test]
fn zero_length_eof_insertions_validate_directly_and_from_unified_diff() {
    let fixture = Fixture::new(&[("file", b"tail\n")]);
    let direct = fixture.ir(vec![EditOperation::ReplaceRange {
        path: path("file"),
        base_digest: digest(b"tail\n"),
        range: ByteRange::new(5, 5).unwrap(),
        expected: TextContent::empty(Newline::Lf),
        replacement: text(b"added\n"),
        executable: ExecutableMode::Preserve,
    }]);
    assert!(matches!(
        &validate(&fixture.workspace, &direct, EditLimits::default())
            .unwrap()
            .effects()[0],
        PlannedEffect::Replace { after, .. } if after.content() == b"tail\nadded\n"
    ));

    let mut context = NormalizationContext::new(fixture.revision(), EditLimits::default());
    context.insert_file("file", b"tail\n", false).unwrap();
    let diff = normalize(
        ModelEditFormat::UnifiedDiff,
        b"--- a/file\n+++ b/file\n@@ -1,0 +2 @@\n+added\n",
        &context,
    )
    .unwrap();
    assert!(matches!(
        diff.operations()[0].operation(),
        EditOperation::ReplaceRange { range, expected, .. }
            if range.start == 5 && range.end == 5 && expected.render().is_empty()
    ));
    assert!(validate(&fixture.workspace, &diff, EditLimits::default()).is_ok());
}

fn same_inode_aba(path: &Path, detached: &Path) {
    let bytes = fs::read(path).unwrap();
    let permissions = fs::metadata(path).unwrap().permissions();
    fs::rename(path, detached).unwrap();
    fs::write(path, &bytes).unwrap();
    fs::set_permissions(path, permissions).unwrap();
    fs::remove_file(path).unwrap();
    fs::rename(detached, path).unwrap();
}

#[test]
fn same_content_inode_aba_after_read_and_finalize_is_rejected() {
    for stage_to_change in ["after-read", "finalized"] {
        let fixture = Fixture::new(&[("file", b"old")]);
        let ir = fixture.ir(vec![replace("file", b"old", b"old")]);
        let mut changed = false;
        let result = kit::test_support::validate_edit_with_hook(
            &fixture.workspace,
            &ir,
            EditLimits::default(),
            |stage, _| {
                if stage == stage_to_change && !changed {
                    changed = true;
                    same_inode_aba(
                        &fixture.workspace_path.join("file"),
                        &fixture.root.join("detached"),
                    );
                }
            },
        );
        assert!(
            matches!(
                result,
                Err(ValidationError::ExternalEdit | ValidationError::StaleRevision)
            ),
            "same-content ABA at {stage_to_change} was accepted"
        );
    }
}

#[test]
fn live_plan_revalidates_exact_descriptor_identity_and_rejects_aba() {
    let fixture = Fixture::new(&[("file", b"old")]);
    let ir = fixture.ir(vec![replace("file", b"old", b"old")]);
    let mut plan = validate(&fixture.workspace, &ir, EditLimits::default()).unwrap();
    let before_identity = match &plan.effects()[0] {
        PlannedEffect::Replace { before, .. } => before.identity(),
        _ => unreachable!(),
    };
    assert_eq!(
        kit::test_support::validated_edit_source_identities(&plan),
        [before_identity]
    );
    same_inode_aba(
        &fixture.workspace_path.join("file"),
        &fixture.root.join("detached"),
    );
    assert!(matches!(
        plan.revalidate_before(Instant::now() + Duration::from_secs(1)),
        Err(ValidationError::ExternalEdit | ValidationError::StaleRevision)
    ));
}

#[test]
fn validation_contract_documents_the_eight_outcomes_and_opaque_plan_boundary() {
    let contract = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/operations/edit-validation.md"),
    )
    .unwrap();
    for outcome in [
        "AmbiguousAnchor",
        "StaleRevision",
        "ExternalEdit",
        "InvalidUnicode",
        "NewlineMismatch",
        "FinalNewlineMismatch",
        "BinaryFile",
        "UnsafePath",
    ] {
        assert!(
            contract.contains(outcome),
            "missing documented outcome {outcome}"
        );
    }
    assert!(contract.contains("cannot be serialized or forged"));
    assert!(contract.contains("same guard nonce and revision"));
}
