#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{
    fs,
    io::{Seek as _, SeekFrom, Write as _},
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    os::unix::process::ExitStatusExt as _,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use kit::{
    api::auth::contract::AuthenticatedPrincipal,
    domain::{
        events::ContentDigest,
        ids::{PrincipalId, ProjectId},
    },
    store::artifacts::{ArtifactDigest, ArtifactRetention, ArtifactStore, Reachability},
    workspace::{
        edit::{
            ir::{
                ByteRange, EditIr, EditLimits, EditOperation, ExecutableMode, RevisionToken,
                RootRelativePath, TextContent,
            },
            recovery::{MaterializeOptions, RecoveryError, RecoveryPoint, materialize_with_hook},
            stage::{StageLimits, stage},
            validate::{validate, validate_authorized},
        },
        revision::{ManagedWorkspace, RevisionOptions},
    },
};

struct Fixture {
    root: PathBuf,
    workspace_path: PathBuf,
    workspace: ManagedWorkspace,
    artifacts: ArtifactStore,
    principal: PrincipalId,
    project: ProjectId,
    authenticated: AuthenticatedPrincipal,
}

impl Fixture {
    fn new() -> Self {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-edit-recovery-{}",
            nonce
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        initialize_root(&root);
        Self::open_existing(root)
    }

    fn open_existing(root: PathBuf) -> Self {
        let workspace_path = root.join("workspace");
        let artifacts = ArtifactStore::open(root.join("artifacts")).unwrap();
        let workspace = ManagedWorkspace::open(&workspace_path).unwrap();
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let (authenticated, _grants, _config) =
            kit::test_support::trusted_verification_context(principal, project);
        Self {
            root,
            workspace_path,
            workspace,
            artifacts,
            principal,
            project,
            authenticated,
        }
    }

    fn fresh_root() -> PathBuf {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        std::env::temp_dir().canonicalize().unwrap().join(format!(
            "kit-edit-recovery-{}",
            nonce
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ))
    }

    fn options(&self) -> MaterializeOptions {
        MaterializeOptions::new(ArtifactRetention::Forever)
    }

    fn assert_before(&self) {
        assert_before(&self.workspace_path);
    }

    fn assert_after(&self) {
        assert_after(&self.workspace_path);
    }

    fn stage(&self) -> kit::workspace::edit::stage::StagedEdit<'_> {
        let revision = self.workspace.current_revision().unwrap().id();
        let ir = edit_ir(revision);
        let plan = validate_authorized(
            &self.workspace,
            &ir,
            EditLimits::default(),
            kit::test_support::trusted_edit_authority(self.principal, self.project),
        )
        .unwrap();
        stage(plan, StageLimits::default(), &[], &mut []).unwrap()
    }
}

#[test]
fn cancellation_before_materialization_leaves_base_content_and_revision() {
    let fixture = Fixture::new();
    let revision = fixture.workspace.current_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(true));
    let error = fixture
        .stage()
        .materialize(
            &fixture.artifacts,
            fixture.options().with_cancellation(cancellation),
        )
        .unwrap_err();
    assert!(matches!(error, RecoveryError::Cancelled));
    fixture.assert_before();
    assert_eq!(fixture.workspace.current_revision().unwrap(), revision);
}

#[test]
fn cancellation_after_recovery_prepare_rolls_back_materialization() {
    let fixture = Fixture::new();
    let revision = fixture.workspace.current_revision().unwrap();
    let cancellation = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&cancellation);
    let error = materialize_with_hook(
        fixture.stage(),
        &fixture.artifacts,
        fixture
            .options()
            .with_cancellation(Arc::clone(&cancellation)),
        &mut move |point, _| {
            if point == RecoveryPoint::AfterPreparedManifestSync {
                observed.store(true, Ordering::Release);
            }
            false
        },
    )
    .unwrap_err();
    assert!(matches!(error, RecoveryError::Cancelled));
    fixture.assert_before();
    assert_eq!(fixture.workspace.current_revision().unwrap(), revision);
}

#[test]
fn cancellation_after_revision_commit_reports_committed_race() {
    let fixture = Fixture::new();
    let cancellation = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&cancellation);
    let result = materialize_with_hook(
        fixture.stage(),
        &fixture.artifacts,
        fixture
            .options()
            .with_cancellation(Arc::clone(&cancellation)),
        &mut move |point, _| {
            if point == RecoveryPoint::AfterRevisionCommit {
                observed.store(true, Ordering::Release);
            }
            false
        },
    )
    .unwrap();
    assert!(result.committed_with_cancel_race());
    fixture.assert_after();
}

fn initialize_root(root: &Path) {
    let workspace_path = root.join("workspace");
    fs::create_dir_all(workspace_path.join("empty")).unwrap();
    fs::write(workspace_path.join("replace.txt"), b"old\r\n").unwrap();
    fs::write(workspace_path.join("delete.bin"), b"\0old").unwrap();
    fs::write(workspace_path.join("move.txt"), b"move\n").unwrap();
    fs::set_permissions(
        workspace_path.join("move.txt"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
}

fn copy_directory(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    fs::set_permissions(destination, fs::metadata(source).unwrap().permissions()).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let source = entry.path();
        let destination = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory(&source, &destination);
        } else {
            fs::copy(&source, &destination).unwrap();
            fs::set_permissions(&destination, fs::metadata(&source).unwrap().permissions())
                .unwrap();
        }
    }
}

fn edit_ir(revision: kit::workspace::revision::RevisionId) -> EditIr {
    EditIr::new(
        RevisionToken::parse(revision.to_string()).unwrap(),
        vec![
            EditOperation::ReplaceRange {
                path: path("replace.txt"),
                base_digest: digest(b"old\r\n"),
                range: ByteRange::new(0, 5).unwrap(),
                expected: text(b"old\r\n"),
                replacement: text(b"new\r\n"),
                executable: ExecutableMode::Executable,
            },
            EditOperation::DeleteFile {
                path: path("delete.bin"),
                base_digest: digest(b"\0old"),
            },
            EditOperation::MoveFile {
                from: path("move.txt"),
                to: path("moved.txt"),
                base_digest: digest(b"move\n"),
            },
            EditOperation::AddFile {
                path: path("added.txt"),
                content: text(b"added\n"),
                executable: false,
            },
        ],
        EditLimits::default(),
    )
    .unwrap()
}

fn assert_before(workspace_path: &Path) {
    assert_eq!(
        fs::read(workspace_path.join("replace.txt")).unwrap(),
        b"old\r\n"
    );
    assert_eq!(
        fs::read(workspace_path.join("delete.bin")).unwrap(),
        b"\0old"
    );
    assert_eq!(
        fs::read(workspace_path.join("move.txt")).unwrap(),
        b"move\n"
    );
    assert!(!workspace_path.join("moved.txt").exists());
    assert!(!workspace_path.join("added.txt").exists());
    assert!(workspace_path.join("empty").is_dir());
}

fn assert_after(workspace_path: &Path) {
    assert_eq!(
        fs::read(workspace_path.join("replace.txt")).unwrap(),
        b"new\r\n"
    );
    assert!(
        fs::metadata(workspace_path.join("replace.txt"))
            .unwrap()
            .mode()
            & 0o111
            != 0
    );
    assert!(!workspace_path.join("delete.bin").exists());
    assert!(!workspace_path.join("move.txt").exists());
    assert_eq!(
        fs::read(workspace_path.join("moved.txt")).unwrap(),
        b"move\n"
    );
    assert_eq!(
        fs::read(workspace_path.join("added.txt")).unwrap(),
        b"added\n"
    );
    assert!(workspace_path.join("empty").is_dir());
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn path(value: &str) -> RootRelativePath {
    RootRelativePath::parse(value, EditLimits::default().max_path_bytes).unwrap()
}

fn text(value: &[u8]) -> TextContent {
    TextContent::from_bytes(value).unwrap()
}

fn digest(value: &[u8]) -> ContentDigest {
    ContentDigest::parse(&format!("blake3:{}", blake3::hash(value).to_hex())).unwrap()
}

#[test]
fn materialization_commits_exact_tree_and_authenticated_actual_diff() {
    let fixture = Fixture::new();
    let staged = fixture.stage();
    let result = staged
        .materialize(&fixture.artifacts, fixture.options())
        .unwrap();
    fixture.assert_after();
    assert_eq!(
        fixture.workspace.current_revision().unwrap().id(),
        result.revision().id()
    );
    let diff = fixture
        .artifacts
        .open_bytes(result.diff_artifact_digest())
        .unwrap();
    assert_eq!(
        blake3::hash(&diff).as_bytes(),
        &result.diff_artifact_digest().as_bytes()
    );
    let diff = String::from_utf8(diff).unwrap();
    assert!(diff.contains(&format!(
        "principal={}\nproject={}",
        fixture.principal, fixture.project
    )));
    assert!(diff.contains("rename from move.txt\nrename to moved.txt"));
    assert!(diff.contains("Binary files a/delete.bin and /dev/null differ"));
    assert!(diff.contains("old mode 000644\nnew mode 000755"));
    assert!(diff.contains("-old\r\n+new\r\n"));
}

#[test]
fn rollback_and_rollforward_preserve_the_exact_stage_binding() {
    for (point, committed) in [
        (RecoveryPoint::AfterPreparedManifestSync, false),
        (RecoveryPoint::AfterRevisionCommit, true),
    ] {
        let fixture = Fixture::new();
        let staged = fixture.stage();
        let expected_stage_digest = staged.state_digest().to_owned();
        let expected_plan_digest = staged.plan_digest().to_owned();
        let result = materialize_with_hook(
            staged,
            &fixture.artifacts,
            fixture.options(),
            &mut |visited, _| visited == point,
        );
        assert!(matches!(result, Err(RecoveryError::InjectedCrash { .. })));
        let manifest: serde_json::Value = serde_json::from_slice(
            &fs::read(recovery_state_root(&fixture.root).join(".kit-edit-recovery.manifest"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            manifest["version"],
            serde_json::json!(kit::workspace::edit::recovery::RECOVERY_MANIFEST_VERSION)
        );
        assert_eq!(manifest["stage_digest"], expected_stage_digest.as_str());
        assert_eq!(manifest["plan_digest"], expected_plan_digest.as_str());
        assert!(manifest.get("verification").is_none());
        fixture.workspace.current_revision().unwrap();
        if committed {
            fixture.assert_after();
        } else {
            fixture.assert_before();
        }
    }
}

#[test]
fn startup_recovery_rejects_a_substituted_stage_digest() {
    let fixture = Fixture::new();
    let result = materialize_with_hook(
        fixture.stage(),
        &fixture.artifacts,
        fixture.options(),
        &mut |visited, _| visited == RecoveryPoint::AfterPreparedManifestSync,
    );
    assert!(matches!(result, Err(RecoveryError::InjectedCrash { .. })));

    let manifest_path = recovery_state_root(&fixture.root).join(".kit-edit-recovery.manifest");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["stage_digest"] = serde_json::Value::String("tampered".to_owned());
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    assert!(fixture.workspace.current_revision().is_err());
}

#[test]
fn every_recovery_crash_point_rolls_back_before_and_forward_after_revision_commit() {
    for point in RecoveryPoint::ALL {
        let actions: &[usize] = match point {
            RecoveryPoint::AfterUndoImageSync
            | RecoveryPoint::BeforeAction
            | RecoveryPoint::AfterActionSync
            | RecoveryPoint::DuringCleanup => &[0, 1, 2, 3, 4],
            RecoveryPoint::AfterDestinationTempSync => &[0, 3, 4],
            RecoveryPoint::AfterSourceQuarantineSync => &[1, 2, 4],
            _ => &[0],
        };
        for target_action in actions {
            let fixture = Fixture::new();
            let mut fired = false;
            let result = materialize_with_hook(
                fixture.stage(),
                &fixture.artifacts,
                fixture.options(),
                &mut |visited, action| {
                    if !fired && visited == point && action == *target_action {
                        fired = true;
                        true
                    } else {
                        false
                    }
                },
            );
            assert!(
                fired,
                "fault point was not reached: {point:?}/{target_action}"
            );
            assert!(
                matches!(
                    &result,
                    Err(RecoveryError::InjectedCrash { point: actual, action })
                        if *actual == point && action == target_action
                ) || point == RecoveryPoint::DuringCleanup
                    && matches!(&result, Err(RecoveryError::CommittedCleanup { .. }))
            );

            fixture.workspace.current_revision().unwrap();
            if matches!(
                point,
                RecoveryPoint::AfterRevisionCommit
                    | RecoveryPoint::AfterCommittedManifestSync
                    | RecoveryPoint::AfterCleanupManifestSync
                    | RecoveryPoint::DuringCleanup
            ) {
                fixture.assert_after();
            } else {
                fixture.assert_before();
            }
            assert!(fs::read_dir(&fixture.workspace_path).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".kit-edit-")
            }));
        }
    }
}

#[test]
fn external_edit_after_staging_is_never_overwritten() {
    let fixture = Fixture::new();
    let staged = fixture.stage();
    fs::write(fixture.workspace_path.join("replace.txt"), b"external\r\n").unwrap();
    let result = staged.materialize(&fixture.artifacts, fixture.options());
    assert!(
        matches!(
            result,
            Err(RecoveryError::Revision(_) | RecoveryError::StageChanged)
        ),
        "unexpected external-edit result: {result:?}"
    );
    assert_eq!(
        fs::read(fixture.workspace_path.join("replace.txt")).unwrap(),
        b"external\r\n"
    );
    assert_eq!(
        fs::read(fixture.workspace_path.join("delete.bin")).unwrap(),
        b"\0old"
    );
    assert!(fixture.workspace.current_revision().is_ok());
}

#[test]
fn authority_budget_preview_and_minimum_retention_are_enforced() {
    let unauthenticated = Fixture::new();
    let ir = edit_ir(unauthenticated.workspace.current_revision().unwrap().id());
    let plan = validate(&unauthenticated.workspace, &ir, EditLimits::default()).unwrap();
    let staged = stage(plan, StageLimits::default(), &[], &mut []).unwrap();
    assert!(
        staged
            .materialize(&unauthenticated.artifacts, unauthenticated.options())
            .is_err()
    );
    unauthenticated.assert_before();

    let bounded = Fixture::new();
    let mut options = bounded.options();
    options.max_diff_bytes = 16;
    assert!(
        bounded
            .stage()
            .materialize(&bounded.artifacts, options)
            .is_err()
    );
    bounded.workspace.current_revision().unwrap();
    bounded.assert_before();

    let retained = Fixture::new();
    let mut options = MaterializeOptions::new(ArtifactRetention::UntilUnixMicros(0));
    options.max_preview_bytes = 1;
    let result = retained
        .stage()
        .materialize(&retained.artifacts, options)
        .unwrap();
    assert!(result.diff_preview().len() <= 1);
    let artifact = retained
        .artifacts
        .resolve_reference(&retained.authenticated, result.diff_artifact_reference())
        .unwrap();
    assert!(matches!(
        artifact.manifest().retention,
        ArtifactRetention::UntilUnixMicros(expiry)
            if expiry > kit::store::artifacts::now_unix_micros().unwrap()
    ));
}

#[test]
fn pending_recovery_lease_survives_retention_and_rollback_releases_it() {
    let fixture = Fixture::new();
    let result = materialize_with_hook(
        fixture.stage(),
        &fixture.artifacts,
        MaterializeOptions::new(ArtifactRetention::UntilUnixMicros(0)),
        &mut |point, _| point == RecoveryPoint::AfterStagedManifestSync,
    );
    assert!(matches!(
        result,
        Err(RecoveryError::InjectedCrash {
            point: RecoveryPoint::AfterStagedManifestSync,
            ..
        })
    ));
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(recovery_state_root(&fixture.root).join(".kit-edit-recovery.manifest")).unwrap(),
    )
    .unwrap();
    let digest = ArtifactDigest::parse(manifest["diff_artifact"].as_str().unwrap()).unwrap();
    let expired = Reachability {
        now_unix_micros: i64::MAX,
        orphan_grace_micros: 0,
        ..Reachability::default()
    };
    let pending_gc = fixture.artifacts.collect_garbage(&expired).unwrap();
    assert!(!pending_gc.deleted_artifacts.contains(&digest));

    fixture.workspace.current_revision().unwrap();
    fixture.assert_before();
    let completed_gc = fixture.artifacts.collect_garbage(&expired).unwrap();
    assert!(completed_gc.deleted_artifacts.contains(&digest));
}

#[test]
fn corrupted_durable_manifest_quarantines_workspace_reads() {
    let fixture = Fixture::new();
    let result = materialize_with_hook(
        fixture.stage(),
        &fixture.artifacts,
        fixture.options(),
        &mut |point, _| point == RecoveryPoint::AfterPreparedManifestSync,
    );
    assert!(matches!(
        result,
        Err(RecoveryError::InjectedCrash {
            point: RecoveryPoint::AfterPreparedManifestSync,
            ..
        })
    ));
    let manifest = recovery_state_root(&fixture.root).join(".kit-edit-recovery.manifest");
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(manifest)
        .unwrap();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(b"X").unwrap();
    file.sync_all().unwrap();
    assert!(fixture.workspace.current_revision().is_err());
    fixture.assert_before();
}

#[test]
fn recovery_binds_a_custom_metadata_store_to_the_exact_workspace_root() {
    let root = Fixture::fresh_root();
    initialize_root(&root);
    let workspace_path = root.join("workspace");
    let artifact_path = root.join("artifacts");
    let metadata_parent = root.join("custom-metadata");
    fs::create_dir(&metadata_parent).unwrap();
    let metadata_path = metadata_parent.join("revision.state");
    let options = RevisionOptions {
        metadata_path: Some(metadata_path.clone()),
        ..RevisionOptions::default()
    };
    let artifacts = ArtifactStore::open(&artifact_path).unwrap();
    let workspace = ManagedWorkspace::open_with_options(&workspace_path, options.clone()).unwrap();
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let ir = edit_ir(workspace.current_revision().unwrap().id());
    let plan = validate_authorized(
        &workspace,
        &ir,
        EditLimits::default(),
        kit::test_support::trusted_edit_authority(principal, project),
    )
    .unwrap();
    let staged = stage(plan, StageLimits::default(), &[], &mut []).unwrap();
    let result = materialize_with_hook(
        staged,
        &artifacts,
        MaterializeOptions::new(ArtifactRetention::Forever),
        &mut |point, _| point == RecoveryPoint::AfterPreparedManifestSync,
    );
    assert!(matches!(
        result,
        Err(RecoveryError::InjectedCrash {
            point: RecoveryPoint::AfterPreparedManifestSync,
            ..
        })
    ));
    drop(workspace);
    drop(artifacts);

    let copied_root = Fixture::fresh_root();
    fs::create_dir(&copied_root).unwrap();
    let copied_workspace = copied_root.join("workspace");
    copy_directory(&workspace_path, &copied_workspace);
    let copied_metadata_parent = copied_root.join("metadata");
    fs::create_dir(&copied_metadata_parent).unwrap();
    let copied_metadata = copied_metadata_parent.join("revision.state");
    fs::copy(&metadata_path, &copied_metadata).unwrap();
    fs::copy(
        metadata_path.with_extension("state.lock"),
        copied_metadata.with_extension("state.lock"),
    )
    .unwrap();
    copy_directory(
        &metadata_path.with_extension("state.staging"),
        &copied_metadata.with_extension("state.staging"),
    );
    let copied_options = RevisionOptions {
        metadata_path: Some(copied_metadata),
        ..RevisionOptions::default()
    };
    assert!(ManagedWorkspace::open_with_options(&copied_workspace, copied_options).is_err());
    assert_before(&copied_workspace);
    fs::remove_dir_all(copied_root).unwrap();

    let recovered = ManagedWorkspace::open_with_options(&workspace_path, options).unwrap();
    recovered.current_revision().unwrap();
    assert_before(&workspace_path);
    drop(recovered);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn concurrent_transactions_serialize_under_the_retained_guard() {
    let fixture = Fixture::new();
    let first = fixture.stage();
    let finished = AtomicBool::new(false);
    thread::scope(|scope| {
        let second = scope.spawn(|| {
            fixture.stage().close().unwrap();
            finished.store(true, Ordering::Release);
        });
        thread::sleep(Duration::from_millis(100));
        assert!(!finished.load(Ordering::Acquire));
        first.close().unwrap();
        second.join().unwrap();
    });
    assert!(finished.load(Ordering::Acquire));
}

#[test]
fn subprocess_crash_child() {
    let Ok(root) = std::env::var("KIT_EDIT_CRASH_ROOT") else {
        return;
    };
    let target = std::env::var("KIT_EDIT_CRASH_POINT").unwrap();
    let target_action: usize = std::env::var("KIT_EDIT_CRASH_ACTION")
        .unwrap()
        .parse()
        .unwrap();
    let kill = std::env::var_os("KIT_EDIT_CRASH_KILL").is_some();
    let fixture = Fixture::open_existing(PathBuf::from(root));
    let options = if std::env::var_os("KIT_EDIT_CRASH_EXPIRED").is_some() {
        MaterializeOptions::new(ArtifactRetention::UntilUnixMicros(0))
    } else {
        fixture.options()
    };
    let _ = materialize_with_hook(
        fixture.stage(),
        &fixture.artifacts,
        options,
        &mut |point, action| {
            if format!("{point:?}") == target && action == target_action {
                if kill {
                    unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                } else {
                    unsafe { libc::_exit(86) };
                }
            }
            false
        },
    );
    panic!("crash point {target} was not reached");
}

#[test]
fn subprocess_recovery_child() {
    let Ok(root) = std::env::var("KIT_EDIT_RECOVER_ROOT") else {
        return;
    };
    ManagedWorkspace::open(PathBuf::from(root).join("workspace"))
        .unwrap()
        .current_revision()
        .unwrap();
}

fn crash_materialization(root: &Path, point: RecoveryPoint) {
    let status = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("edit_recovery::subprocess_crash_child")
        .arg("--test-threads=1")
        .env("KIT_EDIT_CRASH_ROOT", root)
        .env("KIT_EDIT_CRASH_POINT", format!("{point:?}"))
        .env("KIT_EDIT_CRASH_ACTION", "0")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(86));
}

fn recover_through_repeated_crashes(root: &Path, point: RecoveryPoint) {
    for _ in 0..64 {
        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("edit_recovery::subprocess_recovery_child")
            .arg("--test-threads=1")
            .env("KIT_EDIT_RECOVER_ROOT", root)
            .env("KIT_EDIT_CRASH_POINT", format!("{point:?}"))
            .env("KIT_EDIT_CRASH_ACTION", usize::MAX.to_string())
            .status()
            .unwrap();
        if status.success() {
            return;
        }
        assert_eq!(
            status.code(),
            Some(86),
            "recovery failed at {point:?} for {}",
            root.display()
        );
    }
    panic!("recovery did not finish after repeated crashes at {point:?}");
}

fn assert_no_recovery_residue(root: &Path) {
    assert!(
        fs::read_dir(recovery_state_root(&root.to_path_buf()))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".kit-edit-"))
    );
}

#[test]
fn directory_quarantine_crash_matrix_resumes_premarker_and_committed_cleanup() {
    let points = [
        RecoveryPoint::QuarantineMkdir,
        RecoveryPoint::QuarantineExchange,
        RecoveryPoint::QuarantineSourceSentinelRetire,
        RecoveryPoint::QuarantineItemUnlink,
        RecoveryPoint::QuarantineDirectoryRemove,
        RecoveryPoint::QuarantineParentSync,
    ];
    for point in points {
        let premarker = Fixture::fresh_root();
        initialize_root(&premarker);
        crash_materialization(&premarker, RecoveryPoint::TransactionMarkerWrite);
        recover_through_repeated_crashes(&premarker, point);
        assert_before(&premarker.join("workspace"));
        assert_no_recovery_residue(&premarker);
        fs::remove_dir_all(premarker).unwrap();

        let committed = Fixture::fresh_root();
        initialize_root(&committed);
        crash_materialization(&committed, RecoveryPoint::AfterCleanupManifestSync);
        recover_through_repeated_crashes(&committed, point);
        assert_after(&committed.join("workspace"));
        assert_no_recovery_residue(&committed);
        fs::remove_dir_all(committed).unwrap();
    }
}

#[test]
fn unexpected_directory_quarantine_fails_closed_without_mutation() {
    let root = Fixture::fresh_root();
    initialize_root(&root);
    crash_materialization(&root, RecoveryPoint::TransactionMarkerWrite);
    let state_root = recovery_state_root(&root);
    let ledger: serde_json::Value =
        serde_json::from_slice(&fs::read(state_root.join(".kit-edit-recovery.ledger")).unwrap())
            .unwrap();
    let source = state_root.join(ledger["transaction_name"].as_str().unwrap());
    let quarantine = state_root.join(
        ledger["cleanup_intents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|intent| intent["key"] == "transaction")
            .unwrap()["quarantine"]
            .as_str()
            .unwrap(),
    );
    fs::create_dir(&quarantine).unwrap();
    fs::write(quarantine.join("unexpected"), b"leave me").unwrap();
    let source_inode = fs::metadata(&source).unwrap().ino();

    assert!(ManagedWorkspace::open(root.join("workspace")).is_err());
    assert_eq!(fs::metadata(&source).unwrap().ino(), source_inode);
    assert_eq!(
        fs::read(quarantine.join("unexpected")).unwrap(),
        b"leave me"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn crash_after_lease_creation_recovers_and_gc_removes_every_artifact_owner() {
    let root = Fixture::fresh_root();
    initialize_root(&root);
    let status = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("edit_recovery::subprocess_crash_child")
        .arg("--test-threads=1")
        .env("KIT_EDIT_CRASH_ROOT", &root)
        .env("KIT_EDIT_CRASH_POINT", "ArtifactLease")
        .env("KIT_EDIT_CRASH_ACTION", "0")
        .env("KIT_EDIT_CRASH_EXPIRED", "1")
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(86));

    let ledger: serde_json::Value = serde_json::from_slice(
        &fs::read(recovery_state_root(&root).join(".kit-edit-recovery.ledger")).unwrap(),
    )
    .unwrap();
    let digest = ArtifactDigest::parse(ledger["diff_artifact"].as_str().unwrap()).unwrap();
    let expired = Reachability {
        now_unix_micros: i64::MAX,
        orphan_grace_micros: 0,
        ..Reachability::default()
    };
    let artifacts = ArtifactStore::open(root.join("artifacts")).unwrap();
    assert!(
        !artifacts
            .collect_garbage(&expired)
            .unwrap()
            .deleted_artifacts
            .contains(&digest)
    );

    ManagedWorkspace::open(root.join("workspace"))
        .unwrap()
        .current_revision()
        .unwrap();
    assert!(
        artifacts
            .collect_garbage(&expired)
            .unwrap()
            .deleted_artifacts
            .contains(&digest)
    );
    assert!(matches!(
        artifacts.open_verified(digest),
        Err(kit::store::artifacts::ArtifactError::Missing(_))
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn real_process_death_matrix_recovers_exact_tree() {
    for (index, point) in RecoveryPoint::CRASH_MATRIX.into_iter().enumerate() {
        let action = match point {
            RecoveryPoint::AfterSourceQuarantineSync
            | RecoveryPoint::QuarantineMkdir
            | RecoveryPoint::QuarantineSentinelCreate
            | RecoveryPoint::QuarantineSentinelSync
            | RecoveryPoint::QuarantineExchange
            | RecoveryPoint::QuarantinePostVerify
            | RecoveryPoint::QuarantineSourceSentinelRetire
            | RecoveryPoint::QuarantineItemUnlink
            | RecoveryPoint::QuarantineDirectoryRemove
            | RecoveryPoint::QuarantineParentSync => 1,
            _ => 0,
        };
        let root = Fixture::fresh_root();
        initialize_root(&root);
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("edit_recovery::subprocess_crash_child")
            .arg("--test-threads=1")
            .env("KIT_EDIT_CRASH_ROOT", &root)
            .env("KIT_EDIT_CRASH_POINT", format!("{point:?}"))
            .env("KIT_EDIT_CRASH_ACTION", action.to_string());
        if index % 2 == 1 {
            command.env("KIT_EDIT_CRASH_KILL", "1");
        }
        let status = command.status().unwrap();
        if index % 2 == 1 {
            assert_eq!(status.into_raw(), libc::SIGKILL);
        } else {
            assert_eq!(status.code(), Some(86));
        }

        let fixture = Fixture::open_existing(root);
        fixture.workspace.current_revision().unwrap();
        if matches!(
            point,
            RecoveryPoint::RevisionStateRename
                | RecoveryPoint::RevisionStateDirectorySync
                | RecoveryPoint::RevisionGuardWrite
                | RecoveryPoint::RevisionGuardSync
                | RecoveryPoint::AfterRevisionCommit
                | RecoveryPoint::AfterCommittedManifestSync
                | RecoveryPoint::AfterCleanupManifestSync
                | RecoveryPoint::CleanupDirectory
                | RecoveryPoint::CleanupManifestRemove
                | RecoveryPoint::CleanupManifestDirectorySync
                | RecoveryPoint::CleanupLedgerRemove
                | RecoveryPoint::CleanupLedgerDirectorySync
                | RecoveryPoint::DuringCleanup
        ) {
            fixture.assert_after();
        } else {
            fixture.assert_before();
        }
        assert!(fs::read_dir(&fixture.workspace_path).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".kit-edit-")
        }));
        assert!(
            fs::read_dir(recovery_state_root(&fixture.root))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".kit-edit-")),
            "recovery residue remained after {point:?}"
        );
    }
}

fn recovery_state_root(root: &PathBuf) -> PathBuf {
    fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.starts_with(".kit-revision-") && name.ends_with(".state.staging")
            })
        })
        .unwrap()
}
