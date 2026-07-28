use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{CStr, CString},
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd, IntoRawFd},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{
    store::artifacts::{
        self, ArtifactClass, ArtifactLease, ArtifactMetadata, ArtifactRetention, ArtifactStore,
        VerifiedArtifact,
    },
    workspace::{
        edit::{
            ir::RootRelativePath,
            stage::{StageChange, StagedOperation, VerifiedStagedEdit},
        },
        revision::{RevisionError, RevisionId},
    },
};

use super::{
    MaterializeOptions, MaterializedEdit, RECOVERY_MANIFEST_VERSION, RecoveryError, RecoveryHook,
    RecoveryPoint, RecoveryPosition, arm_system_crash, result, system_crash,
};

const MANIFEST_NAME: &CStr = c".kit-edit-recovery.manifest";
const LEDGER_NAME: &CStr = c".kit-edit-recovery.ledger";
const MANIFEST_TEMP_NAME: &CStr = c".kit-edit-recovery.manifest.tmp";
const LEDGER_TEMP_NAME: &CStr = c".kit-edit-recovery.ledger.tmp";
const TX_MARKER_NAME: &CStr = c"transaction.marker";
const TX_MARKER_TEMP_NAME: &CStr = c"transaction.marker.tmp";
const MAX_PARTIAL_MARKER_BYTES: u64 = 128;
const MIN_DIFF_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_RECOVERY_STATE_BYTES: usize = 64 * 1024 * 1024;
const MAX_STARTUP_RECOVERY_TIME: Duration = Duration::from_secs(30);
const DIRECTORY_CLEANUP_ACTION: usize = usize::MAX;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ManifestState {
    Staged,
    Prepared,
    Materialized,
    Committed,
    RolledBack,
    Cleanup,
    Ambiguous,
    Quarantined,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u16,
    transaction: String,
    nonce: String,
    plan_digest: String,
    stage_digest: String,
    expected_base_revision: String,
    expected_base_epoch: String,
    expected_base_digest: String,
    expected_final_revision: String,
    expected_final_epoch: String,
    expected_final_digest: String,
    principal: String,
    project: String,
    verification: crate::verify::profiles::VerificationReceipt,
    diff_reference: String,
    diff_artifact: String,
    diff_bytes: u64,
    diff_media_type: String,
    diff_class: String,
    diff_retention: String,
    diff_stored_at_unix_micros: i64,
    diff_image: ImageRef,
    artifact_store_path: String,
    artifact_store: ObjectIdentity,
    diff_lease: String,
    verification_leases: Vec<RecoveryArtifactLease>,
    diff_owner_referenced: bool,
    state: ManifestState,
    workspace: ObjectIdentity,
    metadata_store: ObjectIdentity,
    transaction_directory: DirectoryIdentity,
    ordered_operations: Vec<OperationRecord>,
    actions: Vec<Action>,
    cleanup_remaining: Vec<String>,
    cleanup_intents: Vec<CleanupIntent>,
    max_manifest_bytes: usize,
    max_actions: usize,
    max_path_bytes: usize,
    max_image_bytes: u64,
    max_diff_bytes: u64,
    max_total_bytes: u64,
    max_time_millis: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u16,
    transaction: String,
    nonce: String,
    transaction_name: String,
    transaction_directory: Option<DirectoryIdentity>,
    workspace: ObjectIdentity,
    metadata_store: ObjectIdentity,
    final_state: bool,
    artifact_store_path: String,
    artifact_store: ObjectIdentity,
    diff_reference: Option<String>,
    diff_artifact: Option<String>,
    diff_lease: Option<String>,
    verification_leases: Vec<RecoveryArtifactLease>,
    cleanup_intents: Vec<CleanupIntent>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RecoveryArtifactLease {
    digest: String,
    lease: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CleanupIntent {
    key: String,
    quarantine: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ObjectIdentity {
    device: u64,
    inode: u64,
    mount: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct OperationRecord {
    kind: String,
    path: String,
    destination: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Action {
    kind: String,
    path: String,
    move_peer: Option<String>,
    before: Option<StoredFile>,
    after: Option<StoredFile>,
    new_temp: String,
    undo_temp: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredFile {
    digest: String,
    mode: u32,
    size: u64,
    identity: Option<FileIdentity>,
    image: ImageRef,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ImageRef {
    name: String,
    identity: FileIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DirectoryIdentity {
    name: String,
    device: u64,
    inode: u64,
    marker: FileIdentity,
}

#[derive(Clone, Copy)]
struct Stat {
    identity: FileIdentity,
    mode: u32,
    links: u64,
    size: u64,
    uid: u32,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct MountIdentity([u8; 32]);

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum TestRaceWindow {
    MoveSource,
    RemoveSource,
    ReplaceDestination,
}

#[cfg(test)]
struct TestRaceHook {
    window: TestRaceWindow,
    entered: std::sync::Arc<std::sync::Barrier>,
    release: std::sync::Arc<std::sync::Barrier>,
}

#[cfg(test)]
static TEST_RACE_HOOK: std::sync::Mutex<Option<TestRaceHook>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn pause_test_race(window: TestRaceWindow) {
    let hook = TEST_RACE_HOOK
        .lock()
        .unwrap()
        .as_ref()
        .filter(|hook| hook.window == window)
        .map(|hook| (hook.entered.clone(), hook.release.clone()));
    if let Some((entered, release)) = hook {
        entered.wait();
        release.wait();
    }
}

pub fn materialize(
    staged: VerifiedStagedEdit<'_>,
    artifacts: &ArtifactStore,
    options: MaterializeOptions,
) -> Result<MaterializedEdit, RecoveryError> {
    let mut hook = |_: RecoveryPoint, _: usize| false;
    materialize_with_hook(staged, artifacts, options, &mut hook)
}

pub fn materialize_with_hook(
    staged: VerifiedStagedEdit<'_>,
    artifacts: &ArtifactStore,
    options: MaterializeOptions,
    hook: RecoveryHook<'_>,
) -> Result<MaterializedEdit, RecoveryError> {
    let (mut staged, verification) = staged.into_parts();
    if options.max_preview_bytes == 0
        || options.max_actions == 0
        || options.max_path_bytes == 0
        || options.max_image_bytes == 0
        || options.max_manifest_bytes == 0
        || options.max_diff_bytes == 0
        || options.max_total_bytes == 0
        || options.max_time.is_zero()
    {
        return Err(RecoveryError::InvalidOptions);
    }
    if cancelled(&options) {
        return Err(RecoveryError::Cancelled);
    }
    let deadline = Instant::now()
        .checked_add(options.max_time)
        .ok_or(RecoveryError::InvalidOptions)?;
    let _crash_arm = arm_system_crash();
    let authority = staged.authority().ok_or(RecoveryError::InvalidOptions)?;
    let principal = authority.principal().to_string();
    let project = authority.project().to_string();
    verification
        .validate_artifacts(artifacts, &principal, &project)
        .map_err(|_| RecoveryError::CorruptManifest)?;
    let verification_digests = verification_artifact_digests(artifacts, &verification)?;
    let stored_at = artifacts::now_unix_micros()?;
    let retention = minimum_retention(options.retention, stored_at)?;
    let artifact_metadata = ArtifactMetadata::new(
        "text/x-diff; charset=utf-8",
        ArtifactClass::Diff,
        principal.clone(),
        project.clone(),
        retention,
        stored_at,
    )?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce).map_err(|_| RecoveryError::Unavailable)?;
    let nonce = hex(&nonce);
    let transaction = format!("edit:{nonce}");
    let transaction_name = format!(".kit-edit-recovery-{nonce}");
    let plan_digest = staged.plan_digest().to_owned();
    let stage_digest = staged.state_digest().to_owned();
    let base_revision = staged.revision();
    let base_epoch = staged.base_epoch();
    let base_digest = staged.base_workspace_digest().to_owned();
    let final_digest = staged.workspace_digest().to_owned();
    let mut budget = MaterializationBudget::new(&options);
    budget.actions(staged.changes().len())?;
    for change in staged.changes() {
        budget.path(change.path().as_str().len())?;
    }
    let changes = staged.changes().to_vec();
    let operations = staged.operations().to_vec();
    let final_root = staged.final_root().try_clone().map_err(RecoveryError::Io)?;
    let prepared = staged.guard_mut().prepare_commit(&final_digest, deadline)?;
    if cancelled(&options) {
        return Err(RecoveryError::Cancelled);
    }
    let expected_revision = prepared.revision().id();
    let expected_epoch = prepared.revision().epoch();
    let (workspace, state_root) = staged.guard_mut().recovery_roots()?;
    let artifact_store_path = std::fs::canonicalize(artifacts.root())
        .map_err(RecoveryError::Io)?
        .to_str()
        .ok_or(RecoveryError::InvalidOptions)?
        .to_owned();
    let artifact_store_file = File::open(&artifact_store_path)?;
    let artifact_store_identity = object_identity(&artifact_store_file)?;

    ensure_no_recovery(&state_root)?;
    let workspace_identity = object_identity(&workspace)?;
    let metadata_store_identity = object_identity(&state_root)?;
    let mut ledger = Ledger {
        version: RECOVERY_MANIFEST_VERSION,
        transaction: transaction.clone(),
        nonce: nonce.clone(),
        transaction_name,
        transaction_directory: None,
        workspace: workspace_identity.clone(),
        metadata_store: metadata_store_identity,
        final_state: false,
        artifact_store_path: artifact_store_path.clone(),
        artifact_store: artifact_store_identity.clone(),
        diff_reference: None,
        diff_artifact: None,
        diff_lease: None,
        verification_leases: Vec::new(),
        cleanup_intents: ["transaction", "manifest", "ledger"]
            .into_iter()
            .map(|key| cleanup_intent(&nonce, key, &object_identity_digest(&workspace_identity)))
            .collect(),
    };
    create_ledger(&state_root, &ledger, options.max_manifest_bytes)?;
    let transaction_directory = create_transaction_directory(&state_root, &nonce)?;
    ledger.transaction_directory = Some(transaction_directory.clone());
    replace_ledger(&state_root, &ledger)?;
    system_crash(RecoveryPoint::TransactionBind, 0);
    let transaction_root = open_named_directory(&state_root, &transaction_directory.name)?;
    ledger.verification_leases = verification_digests
        .iter()
        .map(|digest| RecoveryArtifactLease {
            digest: digest.to_string(),
            lease: verification_lease_id(&ledger.nonce, &ledger.workspace, *digest),
        })
        .collect();
    replace_ledger(&state_root, &ledger)?;
    for lease in &ledger.verification_leases {
        artifacts.acquire_lease_with_id_before(
            artifacts::ArtifactDigest::parse(&lease.digest)?,
            &lease.lease,
            &transaction,
            deadline,
        )?;
    }
    let prepared_data = (|| {
        let move_peers = move_peers(&operations);
        let actions = build_actions(
            &workspace,
            &final_root,
            &transaction_root,
            &nonce,
            &changes,
            &move_peers,
            deadline,
            hook,
            &mut budget,
        )?;
        transaction_root.sync_all()?;
        state_root.sync_all()?;
        let diff_name = c"actual.diff";
        let diff_file = create_file(&transaction_root, diff_name, 0o600)?;
        let mut diff =
            DiffWriter::new(diff_file, options.max_diff_bytes, options.max_preview_bytes);
        actual_diff(
            &mut diff,
            &transaction,
            expected_revision,
            &principal,
            &project,
            &plan_digest,
            &stage_digest,
            &operations,
            &actions,
            &transaction_root,
            deadline,
        )?;
        budget.diff(diff.bytes)?;
        diff.file.sync_all()?;
        transaction_root.sync_all()?;
        let diff_identity = stat_file(&diff.file)?.identity;
        let mut diff_reader = open_named_file(&transaction_root, "actual.diff")?;
        require_identity("actual.diff", stat_file(&diff_reader)?, diff_identity)?;
        let artifact = artifacts.stage_reader_before(
            &mut diff_reader,
            diff.bytes,
            artifact_metadata,
            deadline,
        )?;
        inject(hook, RecoveryPoint::AfterDiffArtifactSync, 0)?;
        Ok::<_, RecoveryError>((actions, diff, diff_identity, artifact))
    })();
    let (mut actions, diff, diff_identity, artifact) = match prepared_data {
        Ok(prepared) => prepared,
        Err(error) => {
            remove_private_transaction(&state_root, &transaction_directory, deadline)?;
            release_verification_leases(
                artifacts,
                &ledger.verification_leases,
                &ledger.transaction,
                deadline,
            )?;
            remove_ledger(&state_root, deadline)?;
            return Err(error);
        }
    };

    let diff_digest = artifact.digest();
    let diff_reference = artifact.reference();
    let pending = artifact.promote_pending_before(deadline)?;
    system_crash(RecoveryPoint::ArtifactPromote, 0);
    let lease_id = transaction_lease_id(&ledger);
    ledger.diff_reference = Some(diff_reference.to_string());
    ledger.diff_artifact = Some(diff_digest.to_string());
    ledger.diff_lease = Some(lease_id.clone());
    replace_ledger(&state_root, &ledger)?;
    let lease = artifacts.acquire_lease_with_id_before_with_hook(
        diff_digest,
        &lease_id,
        &transaction,
        deadline,
        |point| {
            let point = match point {
                artifacts::CrashPoint::AfterLeaseTempCreated => {
                    RecoveryPoint::ArtifactLeaseTempCreate
                }
                artifacts::CrashPoint::AfterLeasePartialWrite => {
                    RecoveryPoint::ArtifactLeasePartialWrite
                }
                artifacts::CrashPoint::AfterLeaseFileSynced => RecoveryPoint::ArtifactLeaseFileSync,
                artifacts::CrashPoint::AfterLeaseRenamed => RecoveryPoint::ArtifactLeaseRename,
                artifacts::CrashPoint::AfterLeaseDirectorySynced => {
                    RecoveryPoint::ArtifactLeaseDirectorySync
                }
                _ => return false,
            };
            system_crash(point, 0);
            false
        },
    )?;
    system_crash(RecoveryPoint::ArtifactLease, 0);
    let mut manifest = Manifest {
        version: RECOVERY_MANIFEST_VERSION,
        transaction: transaction.clone(),
        nonce,
        plan_digest,
        stage_digest,
        expected_base_revision: base_revision.to_string(),
        expected_base_epoch: base_epoch.to_string(),
        expected_base_digest: base_digest,
        expected_final_revision: expected_revision.to_string(),
        expected_final_epoch: expected_epoch.to_string(),
        expected_final_digest: final_digest,
        principal,
        project,
        verification: verification.clone(),
        diff_reference: diff_reference.to_string(),
        diff_artifact: diff_digest.to_string(),
        diff_bytes: diff.bytes,
        diff_media_type: "text/x-diff; charset=utf-8".to_owned(),
        diff_class: "diff".to_owned(),
        diff_retention: retention_string(retention),
        diff_stored_at_unix_micros: stored_at,
        diff_image: ImageRef {
            name: "actual.diff".to_owned(),
            identity: diff_identity,
        },
        artifact_store_path,
        artifact_store: artifact_store_identity,
        diff_lease: lease.id().to_owned(),
        verification_leases: ledger.verification_leases.clone(),
        diff_owner_referenced: false,
        state: ManifestState::Staged,
        workspace: ledger.workspace.clone(),
        metadata_store: ledger.metadata_store.clone(),
        transaction_directory,
        ordered_operations: operation_records(&operations),
        actions: actions.clone(),
        cleanup_remaining: Vec::new(),
        cleanup_intents: Vec::new(),
        max_manifest_bytes: options.max_manifest_bytes,
        max_actions: options.max_actions,
        max_path_bytes: options.max_path_bytes,
        max_image_bytes: options.max_image_bytes,
        max_diff_bytes: options.max_diff_bytes,
        max_total_bytes: options.max_total_bytes,
        max_time_millis: options
            .max_time
            .as_millis()
            .try_into()
            .map_err(|_| RecoveryError::InvalidOptions)?,
    };
    initialize_cleanup(&mut manifest);
    budget.manifest(
        serde_json::to_vec(&manifest)
            .map_err(|_| RecoveryError::CorruptManifest)?
            .len(),
    )?;
    create_manifest(&state_root, &manifest)?;
    let artifact = pending.commit_before(deadline)?;
    verify_diff_artifact(&artifact, &manifest)?;
    inject(hook, RecoveryPoint::AfterStagedManifestSync, 0)?;

    let prepare_result =
        prepare_destination_temps(&workspace, &transaction_root, &mut actions, deadline, hook);
    if let Err(error) = prepare_result {
        if matches!(error, RecoveryError::InjectedCrash { .. }) {
            return Err(error);
        }
        if matches!(error, RecoveryError::Conflict(_)) {
            manifest.state = ManifestState::Ambiguous;
            replace_manifest(&state_root, &manifest)?;
            return Err(error);
        }
        recover_and_cleanup(
            &workspace,
            &state_root,
            artifacts,
            &mut manifest,
            false,
            deadline,
        )?;
        return Err(error);
    }
    manifest.actions.clone_from(&actions);
    manifest.state = ManifestState::Prepared;
    replace_manifest(&state_root, &manifest)?;
    inject(hook, RecoveryPoint::AfterPreparedManifestSync, 0)?;
    if cancelled(&options) {
        recover_and_cleanup(
            &workspace,
            &state_root,
            artifacts,
            &mut manifest,
            false,
            deadline,
        )?;
        return Err(RecoveryError::Cancelled);
    }

    if let Err(error) = apply_forward(&workspace, &manifest, Some(hook), deadline) {
        if matches!(error, RecoveryError::InjectedCrash { .. }) {
            return Err(error);
        }
        if matches!(error, RecoveryError::Conflict(_)) {
            manifest.state = ManifestState::Quarantined;
            replace_manifest(&state_root, &manifest)?;
            return Err(error);
        }
        recover_and_cleanup(
            &workspace,
            &state_root,
            artifacts,
            &mut manifest,
            false,
            deadline,
        )?;
        return Err(error);
    }
    if let Err(error) = retire_undo_temps(&workspace, &state_root, &mut manifest, deadline) {
        if matches!(error, RecoveryError::Conflict(_)) {
            manifest.state = ManifestState::Quarantined;
            replace_manifest(&state_root, &manifest)?;
            return Err(error);
        }
        recover_and_cleanup(
            &workspace,
            &state_root,
            artifacts,
            &mut manifest,
            false,
            deadline,
        )?;
        return Err(error);
    }
    manifest.state = ManifestState::Materialized;
    replace_manifest(&state_root, &manifest)?;
    inject(hook, RecoveryPoint::AfterMaterializedManifestSync, 0)?;
    inject(hook, RecoveryPoint::BeforeRevisionCommit, 0)?;
    if cancelled(&options) {
        recover_and_cleanup(
            &workspace,
            &state_root,
            artifacts,
            &mut manifest,
            false,
            deadline,
        )?;
        return Err(RecoveryError::Cancelled);
    }

    let revision = match staged.guard_mut().commit_prepared(&prepared, deadline) {
        Ok(revision) => revision,
        Err(error) => {
            recover_and_cleanup(
                &workspace,
                &state_root,
                artifacts,
                &mut manifest,
                false,
                deadline,
            )?;
            return Err(error.into());
        }
    };
    inject(hook, RecoveryPoint::AfterRevisionCommit, 0)?;
    manifest.state = ManifestState::Committed;
    replace_manifest(&state_root, &manifest)?;
    inject(hook, RecoveryPoint::AfterCommittedManifestSync, 0)?;
    retain_committed_diff(
        artifacts,
        &state_root,
        &mut manifest,
        revision.id(),
        deadline,
    )?;

    let preview = diff.preview(artifact.digest());
    let mut result = result(
        transaction,
        revision,
        artifact.reference(),
        artifact.digest(),
        preview,
        verification,
    );
    if cancelled(&options) {
        result.mark_cancel_race();
    }
    manifest.state = ManifestState::Cleanup;
    replace_manifest(&state_root, &manifest)?;
    inject(hook, RecoveryPoint::AfterCleanupManifestSync, 0)?;
    if let Err(error) = cleanup(
        &workspace,
        &state_root,
        artifacts,
        &mut manifest,
        true,
        Some(hook),
        deadline,
    ) {
        if matches!(error, RecoveryError::Conflict(_)) {
            manifest.state = ManifestState::Quarantined;
            replace_manifest(&state_root, &manifest)?;
        }
        return Err(RecoveryError::CommittedCleanup {
            result: Box::new(result),
            source: into_io(error),
        });
    }
    if let Err(error) = staged.cleanup() {
        return Err(RecoveryError::CommittedCleanup {
            result: Box::new(result),
            source: io::Error::other(error.to_string()),
        });
    }
    Ok(result)
}

fn cancelled(options: &MaterializeOptions) -> bool {
    options
        .cancellation
        .as_ref()
        .is_some_and(|signal| signal.load(std::sync::atomic::Ordering::Acquire))
}

pub(crate) fn recover_pending(
    workspace: &File,
    state_root: &File,
    mut resolve_artifacts: impl FnMut(
        &std::path::Path,
    ) -> Result<ArtifactStore, artifacts::ArtifactError>,
    mut position: impl FnMut(
        &str,
        &str,
        &str,
        &str,
        &str,
        &str,
    ) -> Result<RecoveryPosition, RevisionError>,
) -> Result<(), RecoveryError> {
    let _crash_arm = arm_system_crash();
    let started = Instant::now();
    let mut deadline = started
        .checked_add(MAX_STARTUP_RECOVERY_TIME)
        .ok_or(RecoveryError::CorruptManifest)?;
    check_deadline(deadline)?;
    cleanup_atomic_temps(state_root, deadline)?;
    let ledger = read_ledger(state_root)?;
    let ledger = match ledger {
        Some(ledger) => ledger,
        None if read_manifest(state_root)?.is_some() => return Err(RecoveryError::CorruptManifest),
        None => return Ok(()),
    };
    validate_ledger(&ledger, workspace, state_root)?;
    if ledger.final_state {
        remove_partial_transaction(state_root, &ledger, deadline)?;
        remove_manifest_if_present(state_root, deadline)?;
        return remove_ledger(state_root, deadline);
    }
    let manifest = read_manifest(state_root)?;
    let Some(mut manifest) = manifest else {
        remove_partial_transaction(state_root, &ledger, deadline)?;
        release_ledger_lease(&ledger, &mut resolve_artifacts, deadline)?;
        return remove_ledger(state_root, deadline);
    };
    let stored_duration = Duration::from_millis(manifest.max_time_millis);
    deadline = started
        .checked_add(stored_duration.min(MAX_STARTUP_RECOVERY_TIME))
        .ok_or(RecoveryError::CorruptManifest)?;
    check_deadline(deadline)?;
    if manifest.transaction != ledger.transaction
        || manifest.nonce != ledger.nonce
        || manifest.transaction_directory.name != ledger.transaction_name
        || ledger.transaction_directory.as_ref() != Some(&manifest.transaction_directory)
        || manifest.workspace != ledger.workspace
        || manifest.metadata_store != ledger.metadata_store
        || manifest.artifact_store_path != ledger.artifact_store_path
        || manifest.artifact_store != ledger.artifact_store
        || ledger.diff_reference.as_deref() != Some(manifest.diff_reference.as_str())
        || ledger.diff_artifact.as_deref() != Some(manifest.diff_artifact.as_str())
        || ledger.diff_lease.as_deref() != Some(manifest.diff_lease.as_str())
        || ledger.verification_leases != manifest.verification_leases
    {
        return Err(RecoveryError::CorruptManifest);
    }
    validate_manifest(&manifest, state_root, deadline)?;
    if matches!(
        manifest.state,
        ManifestState::Ambiguous | ManifestState::Quarantined
    ) {
        return Err(RecoveryError::Conflict(
            "recovery transaction requires manual quarantine review".to_owned(),
        ));
    }
    let artifacts = resolve_recovery_artifacts(
        &manifest.artifact_store_path,
        &manifest.artifact_store,
        &mut resolve_artifacts,
    )?;
    manifest
        .verification
        .validate_artifacts(&artifacts, &manifest.principal, &manifest.project)
        .map_err(|_| RecoveryError::CorruptManifest)?;
    validate_verification_leases(&artifacts, &manifest)?;
    let digest = artifacts::ArtifactDigest::parse(&manifest.diff_artifact)?;
    let owner = diff_reference_owner(&manifest)?;
    if manifest.diff_owner_referenced {
        if !artifacts.reference_exists(digest, &owner)? {
            return Err(RecoveryError::CorruptManifest);
        }
    } else if artifacts.reference_exists(digest, &owner)? {
        manifest.diff_owner_referenced = true;
        replace_manifest(state_root, &manifest)?;
    } else {
        artifacts.open_lease(digest, &manifest.diff_lease, &manifest.transaction)?;
    }
    let boundary = position(
        &manifest.expected_base_revision,
        &manifest.expected_base_epoch,
        &manifest.expected_base_digest,
        &manifest.expected_final_revision,
        &manifest.expected_final_epoch,
        &manifest.expected_final_digest,
    )?;
    if boundary == RecoveryPosition::Other {
        return Err(RecoveryError::Conflict(
            "workspace revision is neither transaction base nor successor".to_owned(),
        ));
    }
    let reference = artifacts::ArtifactReference::parse(&manifest.diff_reference)?;
    if let Some(artifact) = artifacts.open_reference_optional(reference)? {
        verify_diff_artifact(&artifact, &manifest)?;
    } else if boundary == RecoveryPosition::Base {
        artifacts.verify_content(digest, manifest.diff_bytes)?;
    } else {
        return Err(RecoveryError::CorruptManifest);
    }
    if boundary == RecoveryPosition::Successor {
        if !matches!(
            manifest.state,
            ManifestState::Materialized | ManifestState::Committed | ManifestState::Cleanup
        ) {
            return Err(RecoveryError::CorruptManifest);
        }
        if manifest.state != ManifestState::Cleanup
            && let Err(error) = apply_forward(workspace, &manifest, None, deadline)
        {
            persist_recovery_conflict(state_root, &mut manifest, &error, true)?;
            return Err(error);
        }
        if manifest.state != ManifestState::Cleanup {
            manifest.state = ManifestState::Committed;
            append_manifest(state_root, &manifest)?;
            manifest.state = ManifestState::Cleanup;
            append_manifest(state_root, &manifest)?;
        }
        let revision = RevisionId::parse(&manifest.expected_final_revision)
            .ok_or(RecoveryError::CorruptManifest)?;
        retain_committed_diff(&artifacts, state_root, &mut manifest, revision, deadline)?;
        let result = cleanup(
            workspace,
            state_root,
            &artifacts,
            &mut manifest,
            true,
            None,
            deadline,
        );
        if let Err(error) = &result {
            persist_recovery_conflict(state_root, &mut manifest, error, true)?;
        }
        result
    } else {
        if matches!(
            manifest.state,
            ManifestState::Committed | ManifestState::Cleanup
        ) {
            return Err(RecoveryError::CorruptManifest);
        }
        if let Err(error) = apply_rollback(workspace, state_root, &manifest, deadline) {
            persist_recovery_conflict(state_root, &mut manifest, &error, false)?;
            return Err(error);
        }
        manifest.state = ManifestState::RolledBack;
        append_manifest(state_root, &manifest)?;
        let result = cleanup(
            workspace,
            state_root,
            &artifacts,
            &mut manifest,
            false,
            None,
            deadline,
        );
        if let Err(error) = &result {
            persist_recovery_conflict(state_root, &mut manifest, error, false)?;
        }
        result
    }
}

fn recover_and_cleanup(
    workspace: &File,
    state_root: &File,
    artifacts: &ArtifactStore,
    manifest: &mut Manifest,
    committed: bool,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    if committed {
        if let Err(error) = apply_forward(workspace, manifest, None, deadline) {
            persist_recovery_conflict(state_root, manifest, &error, true)?;
            return Err(error);
        }
        manifest.state = ManifestState::Cleanup;
    } else {
        if let Err(error) = apply_rollback(workspace, state_root, manifest, deadline) {
            persist_recovery_conflict(state_root, manifest, &error, false)?;
            return Err(error);
        }
        manifest.state = ManifestState::RolledBack;
    }
    append_manifest(state_root, manifest)?;
    let result = cleanup(
        workspace, state_root, artifacts, manifest, committed, None, deadline,
    );
    if let Err(error) = &result {
        persist_recovery_conflict(state_root, manifest, error, committed)?;
    }
    result
}

fn persist_recovery_conflict(
    state_root: &File,
    manifest: &mut Manifest,
    error: &RecoveryError,
    quarantined: bool,
) -> Result<(), RecoveryError> {
    if matches!(error, RecoveryError::Conflict(_)) {
        manifest.state = if quarantined {
            ManifestState::Quarantined
        } else {
            ManifestState::Ambiguous
        };
        replace_manifest(state_root, manifest)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_actions(
    workspace: &File,
    final_root: &File,
    transaction_root: &File,
    nonce: &str,
    changes: &[StageChange],
    move_peers: &BTreeMap<String, String>,
    deadline: Instant,
    hook: RecoveryHook<'_>,
    budget: &mut MaterializationBudget,
) -> Result<Vec<Action>, RecoveryError> {
    let mut actions = Vec::with_capacity(changes.len());
    for (index, change) in changes.iter().enumerate() {
        check_deadline(deadline)?;
        let path = change.path().as_str().to_owned();
        let before = match (change.before_hash(), change.before_mode()) {
            (Some(digest), Some(mode)) => Some(store_image(
                workspace,
                transaction_root,
                change.path(),
                digest,
                mode,
                &format!("before-{index}"),
                true,
                deadline,
                budget,
            )?),
            (None, None) => None,
            _ => return Err(RecoveryError::StageChanged),
        };
        let after = match (change.after_hash(), change.after_mode()) {
            (Some(digest), Some(mode)) => Some(store_image(
                final_root,
                transaction_root,
                change.path(),
                digest,
                mode,
                &format!("after-{index}"),
                false,
                deadline,
                budget,
            )?),
            (None, None) => None,
            _ => return Err(RecoveryError::StageChanged),
        };
        inject(hook, RecoveryPoint::AfterUndoImageSync, index)?;
        let kind = match (&before, &after, move_peers.get(&path)) {
            (None, Some(_), Some(_)) => "move_to",
            (Some(_), None, Some(_)) => "move_from",
            (None, Some(_), None) => "add",
            (Some(_), None, None) => "delete",
            (Some(_), Some(_), _) => "replace",
            _ => return Err(RecoveryError::StageChanged),
        };
        actions.push(Action {
            kind: kind.to_owned(),
            path,
            move_peer: move_peers.get(change.path().as_str()).cloned(),
            before,
            after,
            new_temp: format!(".kit-edit-{nonce}-{index}.new"),
            undo_temp: format!(".kit-edit-{nonce}-{index}.undo"),
        });
    }
    Ok(actions)
}

#[allow(clippy::too_many_arguments)]
fn store_image(
    source_root: &File,
    transaction_root: &File,
    path: &RootRelativePath,
    digest: &str,
    mode: u32,
    image_name: &str,
    retain_source_identity: bool,
    deadline: Instant,
    budget: &mut MaterializationBudget,
) -> Result<StoredFile, RecoveryError> {
    let mut source = open_relative(source_root, path.as_str(), libc::O_RDONLY)?;
    let source_stat = stat_file(&source)?;
    validate_regular(path.as_str(), source_stat)?;
    if mount_identity(&source)? != mount_identity(source_root)? {
        return Err(RecoveryError::UnsafeEntry(path.as_str().to_owned()));
    }
    if source_stat.mode & 0o777 != if retain_source_identity { mode } else { 0o400 } {
        return Err(RecoveryError::StageChanged);
    }
    budget.image(source_stat.size)?;
    let name = CString::new(image_name).map_err(|_| RecoveryError::CorruptManifest)?;
    let mut image = create_file(transaction_root, &name, 0o600)?;
    let mut hasher = blake3::Hasher::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_deadline(deadline)?;
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(count as u64)
            .ok_or(RecoveryError::StageChanged)?;
        if copied > source_stat.size {
            return Err(RecoveryError::StageChanged);
        }
        image.write_all(&buffer[..count])?;
        system_crash(RecoveryPoint::ImageWrite, 0);
        hasher.update(&buffer[..count]);
    }
    if copied != source_stat.size
        || format!("blake3:{}", hasher.finalize().to_hex()) != digest
        || stat_file(&source)?.identity != source_stat.identity
    {
        return Err(RecoveryError::StageChanged);
    }
    set_mode(&image, mode)?;
    image.sync_all()?;
    system_crash(RecoveryPoint::ImageFileSync, 0);
    let image_stat = stat_file(&image)?;
    validate_regular(image_name, image_stat)?;
    Ok(StoredFile {
        digest: digest.to_owned(),
        mode,
        size: source_stat.size,
        identity: retain_source_identity.then_some(source_stat.identity),
        image: ImageRef {
            name: image_name.to_owned(),
            identity: image_stat.identity,
        },
    })
}

fn prepare_destination_temps(
    workspace: &File,
    transaction_root: &File,
    actions: &mut [Action],
    deadline: Instant,
    hook: RecoveryHook<'_>,
) -> Result<(), RecoveryError> {
    for (index, action) in actions.iter_mut().enumerate() {
        let Some(after) = &mut action.after else {
            continue;
        };
        check_deadline(deadline)?;
        let (parent, _) = open_parent(workspace, &action.path)?;
        ensure_absent(&parent, &action.new_temp)?;
        ensure_absent(&parent, &action.undo_temp)?;
        let mut source = open_image(transaction_root, after)?;
        let name =
            CString::new(action.new_temp.as_str()).map_err(|_| RecoveryError::CorruptManifest)?;
        let mut temp = create_file(&parent, &name, 0o600)?;
        copy_verified(&mut source, &mut temp, after, deadline)?;
        set_mode(&temp, after.mode)?;
        temp.sync_all()?;
        parent.sync_all()?;
        let stat = stat_file(&temp)?;
        validate_regular(&action.new_temp, stat)?;
        after.identity = Some(stat.identity);
        inject(hook, RecoveryPoint::AfterDestinationTempSync, index)?;
    }
    Ok(())
}

fn apply_forward(
    workspace: &File,
    manifest: &Manifest,
    mut hook: Option<RecoveryHook<'_>>,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    for (index, action) in manifest.actions.clone().iter().enumerate() {
        check_deadline(deadline)?;
        if let Some(hook) = hook.as_deref_mut() {
            inject(hook, RecoveryPoint::BeforeAction, index)?;
        }
        let (parent, leaf) = open_parent(workspace, &action.path)?;
        if let Some(before) = &action.before {
            let before_identity = before.identity.ok_or(RecoveryError::CorruptManifest)?;
            let undo = CString::new(action.undo_temp.as_str())
                .map_err(|_| RecoveryError::CorruptManifest)?;
            normalize_workspace_exchange(&parent, &leaf, before_identity, &undo)?;
            let current = stat_at_optional(&parent, &leaf)?;
            match (current, stat_at_optional(&parent, &undo)?) {
                (Some(current), None) if current.identity == before_identity => {
                    verify_path_file(&parent, &leaf, before, deadline)?;
                    rename_noreplace_identity(
                        &parent,
                        &leaf,
                        &parent,
                        &undo,
                        before_identity,
                        Some(before),
                        deadline,
                    )?;
                    parent.sync_all()?;
                    if let Some(hook) = hook.as_deref_mut() {
                        inject(hook, RecoveryPoint::AfterSourceQuarantineSync, index)?;
                    }
                }
                (Some(current), Some(undo_state))
                    if action.after.as_ref().and_then(|file| file.identity)
                        == Some(current.identity)
                        && undo_state.identity == before_identity => {}
                (Some(current), None)
                    if matches!(
                        manifest.state,
                        ManifestState::Materialized
                            | ManifestState::Committed
                            | ManifestState::Cleanup
                    ) && action.after.as_ref().and_then(|file| file.identity)
                        == Some(current.identity) => {}
                (None, Some(undo_state)) if undo_state.identity == before_identity => {}
                (None, None)
                    if action.after.is_none()
                        && matches!(
                            manifest.state,
                            ManifestState::Materialized
                                | ManifestState::Committed
                                | ManifestState::Cleanup
                        ) => {}
                _ => return Err(RecoveryError::Conflict(action.path.clone())),
            }
        } else if let Some(current) = stat_at_optional(&parent, &leaf)?
            && action.after.as_ref().and_then(|file| file.identity) != Some(current.identity)
        {
            return Err(RecoveryError::Conflict(action.path.clone()));
        }

        if let Some(after) = &action.after {
            let identity = after.identity.ok_or(RecoveryError::CorruptManifest)?;
            let temp = CString::new(action.new_temp.as_str())
                .map_err(|_| RecoveryError::CorruptManifest)?;
            normalize_workspace_exchange(&parent, &temp, identity, &leaf)?;
            match stat_at_optional(&parent, &leaf)? {
                Some(current) if current.identity == identity => {
                    verify_path_file(&parent, &leaf, after, deadline)?
                }
                None => {
                    require_identity(&action.new_temp, stat_at(&parent, &temp)?, identity)?;
                    verify_path_file(&parent, &temp, after, deadline)?;
                    rename_noreplace_identity(
                        &parent,
                        &temp,
                        &parent,
                        &leaf,
                        identity,
                        Some(after),
                        deadline,
                    )?;
                    parent.sync_all()?;
                }
                _ => return Err(RecoveryError::Conflict(action.path.clone())),
            }
        } else if stat_at_optional(&parent, &leaf)?.is_some() {
            return Err(RecoveryError::Conflict(action.path.clone()));
        }
        if let Some(hook) = hook.as_deref_mut() {
            inject(hook, RecoveryPoint::AfterActionSync, index)?;
        }
    }
    Ok(())
}

fn apply_rollback(
    workspace: &File,
    state_root: &File,
    manifest: &Manifest,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    let transaction_root = open_transaction_directory(state_root, &manifest.transaction_directory)?;
    for (index, action) in manifest.actions.iter().enumerate().rev() {
        check_deadline(deadline)?;
        let (parent, leaf) = open_parent(workspace, &action.path)?;
        if let Some(after) = &action.after {
            if let Some(identity) = after.identity {
                let temp = CString::new(action.new_temp.as_str())
                    .map_err(|_| RecoveryError::CorruptManifest)?;
                normalize_workspace_exchange(&parent, &leaf, identity, &temp)?;
                match stat_at_optional(&parent, &leaf)? {
                    Some(current) if current.identity == identity => {
                        verify_path_file(&parent, &leaf, after, deadline)?;
                        rename_noreplace_identity(
                            &parent,
                            &leaf,
                            &parent,
                            &temp,
                            identity,
                            Some(after),
                            deadline,
                        )?;
                        parent.sync_all()?;
                    }
                    Some(current)
                        if action.before.as_ref().and_then(|file| file.identity)
                            == Some(current.identity) => {}
                    None => {}
                    Some(_) => return Err(RecoveryError::Conflict(action.path.clone())),
                }
                remove_if_identity(
                    &parent,
                    &temp,
                    cleanup_quarantine(&manifest.cleanup_intents, &format!("new:{index}"))?,
                    index,
                    identity,
                    after,
                    deadline,
                )?;
            } else {
                remove_unbound_temp(
                    &parent,
                    action,
                    cleanup_quarantine(&manifest.cleanup_intents, &format!("new:{index}"))?,
                    index,
                    after,
                    deadline,
                )?;
            }
        }
        if let Some(before) = &action.before {
            let identity = before.identity.ok_or(RecoveryError::CorruptManifest)?;
            let undo = CString::new(action.undo_temp.as_str())
                .map_err(|_| RecoveryError::CorruptManifest)?;
            normalize_workspace_exchange(&parent, &undo, identity, &leaf)?;
            match stat_at_optional(&parent, &leaf)? {
                Some(current) if current.identity == identity => {
                    verify_path_file(&parent, &leaf, before, deadline)?
                }
                None => {
                    if let Some(undo_state) = stat_at_optional(&parent, &undo)? {
                        require_identity(&action.undo_temp, undo_state, identity)?;
                        verify_path_file(&parent, &undo, before, deadline)?;
                        rename_noreplace_identity(
                            &parent,
                            &undo,
                            &parent,
                            &leaf,
                            identity,
                            Some(before),
                            deadline,
                        )?;
                    } else {
                        restore_image(&transaction_root, &parent, &leaf, action, before, deadline)?;
                    }
                    parent.sync_all()?;
                }
                _ => return Err(RecoveryError::Conflict(action.path.clone())),
            }
        } else if stat_at_optional(&parent, &leaf)?.is_some() {
            return Err(RecoveryError::Conflict(action.path.clone()));
        }
    }
    Ok(())
}

fn restore_image(
    transaction_root: &File,
    parent: &File,
    leaf: &CStr,
    action: &Action,
    before: &StoredFile,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    let mut source = open_image(transaction_root, before)?;
    let temp_name =
        CString::new(action.new_temp.as_str()).map_err(|_| RecoveryError::CorruptManifest)?;
    ensure_absent(parent, &action.new_temp)?;
    let mut temp = create_file(parent, &temp_name, 0o600)?;
    copy_verified(&mut source, &mut temp, before, deadline)?;
    set_mode(&temp, before.mode)?;
    temp.sync_all()?;
    parent.sync_all()?;
    let identity = stat_file(&temp)?.identity;
    rename_noreplace_identity(
        parent,
        &temp_name,
        parent,
        leaf,
        identity,
        Some(before),
        deadline,
    )?;
    parent.sync_all()?;
    Ok(())
}

fn retire_undo_temps(
    workspace: &File,
    state_root: &File,
    manifest: &mut Manifest,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    for (index, action) in manifest.actions.clone().iter().enumerate() {
        let Some(before) = &action.before else {
            continue;
        };
        let (parent, _) = open_parent(workspace, &action.path)?;
        let undo =
            CString::new(action.undo_temp.as_str()).map_err(|_| RecoveryError::CorruptManifest)?;
        cleanup_file(
            &parent,
            &undo,
            before.identity.ok_or(RecoveryError::CorruptManifest)?,
            before,
            &format!("undo:{index}"),
            state_root,
            manifest,
            deadline,
        )?;
    }
    Ok(())
}

fn complete_cleanup_key(
    state_root: &File,
    manifest: &mut Manifest,
    key: &str,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    check_deadline(deadline)?;
    if let Some(index) = manifest
        .cleanup_remaining
        .iter()
        .position(|remaining| remaining == key)
    {
        manifest.cleanup_remaining.remove(index);
        replace_manifest(state_root, manifest)?;
        system_crash(RecoveryPoint::CleanupProgress, 0);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cleanup_file(
    parent: &File,
    name: &CStr,
    identity: FileIdentity,
    stored: &StoredFile,
    key: &str,
    state_root: &File,
    manifest: &mut Manifest,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    check_deadline(deadline)?;
    let trash = CString::new(format!(
        ".kit-edit-clean-{}-{}",
        manifest.nonce,
        &blake3::hash(key.as_bytes()).to_hex()[..16]
    ))
    .map_err(|_| RecoveryError::CorruptManifest)?;
    let remaining = manifest.cleanup_remaining.iter().any(|item| item == key);
    let source = stat_at_optional(parent, name)?;
    let trashed = stat_at_optional(parent, &trash)?;
    if remaining {
        match (source, trashed) {
            (Some(source), None) => {
                require_identity(name.to_string_lossy().as_ref(), source, identity)?;
                verify_path_file(parent, name, stored, deadline)?;
                rename_noreplace_identity(
                    parent,
                    name,
                    parent,
                    &trash,
                    identity,
                    Some(stored),
                    deadline,
                )?;
                parent.sync_all()?;
                require_identity(key, stat_at(parent, &trash)?, identity)?;
            }
            (None, Some(trashed)) => require_identity(key, trashed, identity)?,
            (None, None)
                if manifest.state == ManifestState::RolledBack
                    || key.starts_with("new:")
                        && matches!(
                            manifest.state,
                            ManifestState::Materialized
                                | ManifestState::Committed
                                | ManifestState::Cleanup
                        ) => {}
            _ => return Err(RecoveryError::Conflict(key.to_owned())),
        }
        complete_cleanup_key(state_root, manifest, key, deadline)?;
    } else if source.is_some() {
        return Err(RecoveryError::Conflict(key.to_owned()));
    }
    if stat_at_optional(parent, &trash)?.is_some()
        || stat_at_optional(
            parent,
            &CString::new(cleanup_quarantine(&manifest.cleanup_intents, key)?)
                .map_err(|_| RecoveryError::CorruptManifest)?,
        )?
        .is_some()
    {
        let quarantine = cleanup_quarantine(&manifest.cleanup_intents, key)?;
        quarantine_remove_file(
            parent,
            &trash,
            quarantine,
            cleanup_action(key),
            Some(identity),
            Some(stored),
            deadline,
        )?;
        system_crash(RecoveryPoint::CleanupItem, 0);
    }
    Ok(())
}

fn cleanup_marker(
    transaction_root: &File,
    state_root: &File,
    manifest: &mut Manifest,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    check_deadline(deadline)?;
    let key = "marker";
    let trash = c"transaction.marker.trash";
    let remaining = manifest.cleanup_remaining.iter().any(|item| item == key);
    let source = stat_at_optional(transaction_root, TX_MARKER_NAME)?;
    let trashed = stat_at_optional(transaction_root, trash)?;
    if remaining {
        match (source, trashed) {
            (Some(source), None) => {
                require_identity(key, source, manifest.transaction_directory.marker)?;
                rename_noreplace_identity(
                    transaction_root,
                    TX_MARKER_NAME,
                    transaction_root,
                    trash,
                    manifest.transaction_directory.marker,
                    None,
                    deadline,
                )?;
                transaction_root.sync_all()?;
            }
            (None, Some(trashed)) => {
                require_identity(key, trashed, manifest.transaction_directory.marker)?
            }
            _ => return Err(RecoveryError::Conflict(key.to_owned())),
        }
        complete_cleanup_key(state_root, manifest, key, deadline)?;
    }
    let quarantine = cleanup_quarantine(&manifest.cleanup_intents, key)?;
    if stat_at_optional(transaction_root, trash)?.is_some()
        || stat_at_optional(
            transaction_root,
            &CString::new(quarantine).map_err(|_| RecoveryError::CorruptManifest)?,
        )?
        .is_some()
    {
        quarantine_remove_file(
            transaction_root,
            trash,
            quarantine,
            0,
            Some(manifest.transaction_directory.marker),
            None,
            deadline,
        )?;
    }
    Ok(())
}

fn cleanup_transaction_directory(
    state_root: &File,
    manifest: &mut Manifest,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    check_deadline(deadline)?;
    let quarantine = cleanup_quarantine(&manifest.cleanup_intents, "transaction")?.to_owned();
    let expected = manifest.transaction_directory.clone();
    let authority = DirectoryAuthority::Bound {
        expected: &expected,
        require_marker: false,
    };
    let remaining = manifest
        .cleanup_remaining
        .iter()
        .any(|item| item == "transaction");
    if remaining {
        inspect_directory_quarantine(state_root, &expected.name, &quarantine, authority, deadline)?;
        complete_cleanup_key(state_root, manifest, "transaction", deadline)?;
    }
    remove_quarantined_directory(state_root, &expected.name, &quarantine, authority, deadline)?;
    system_crash(RecoveryPoint::CleanupDirectory, 0);
    Ok(())
}

fn require_directory_identity(
    stat: Stat,
    expected: &DirectoryIdentity,
) -> Result<(), RecoveryError> {
    if stat.mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32
        && stat.identity.device == expected.device
        && stat.identity.inode == expected.inode
    {
        Ok(())
    } else {
        Err(RecoveryError::CorruptManifest)
    }
}

fn remove_manifest_if_present(state_root: &File, deadline: Instant) -> Result<(), RecoveryError> {
    let ledger = read_ledger(state_root)?.ok_or(RecoveryError::CorruptManifest)?;
    match quarantine_remove_file(
        state_root,
        MANIFEST_NAME,
        cleanup_quarantine(&ledger.cleanup_intents, "manifest")?,
        0,
        None,
        None,
        deadline,
    ) {
        Ok(()) => {
            system_crash(RecoveryPoint::CleanupManifestRemove, 0);
            system_crash(RecoveryPoint::CleanupManifestDirectorySync, 0);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn cleanup(
    workspace: &File,
    state_root: &File,
    artifacts: &ArtifactStore,
    manifest: &mut Manifest,
    committed: bool,
    mut hook: Option<RecoveryHook<'_>>,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    for (index, action) in manifest.actions.clone().iter().enumerate() {
        check_deadline(deadline)?;
        let (parent, _) = open_parent(workspace, &action.path)?;
        if let Some(before) = &action.before {
            let undo = CString::new(action.undo_temp.as_str())
                .map_err(|_| RecoveryError::CorruptManifest)?;
            cleanup_file(
                &parent,
                &undo,
                before.identity.ok_or(RecoveryError::CorruptManifest)?,
                before,
                &format!("undo:{index}"),
                state_root,
                manifest,
                deadline,
            )?;
        }
        if let Some(after) = &action.after
            && let Some(identity) = after.identity
        {
            let temp = CString::new(action.new_temp.as_str())
                .map_err(|_| RecoveryError::CorruptManifest)?;
            cleanup_file(
                &parent,
                &temp,
                identity,
                after,
                &format!("new:{index}"),
                state_root,
                manifest,
                deadline,
            )?;
        } else if let Some(after) = &action.after {
            remove_unbound_temp(
                &parent,
                action,
                cleanup_quarantine(&manifest.cleanup_intents, &format!("new:{index}"))?,
                index,
                after,
                deadline,
            )?;
            complete_cleanup_key(state_root, manifest, &format!("new:{index}"), deadline)?;
        }
        parent.sync_all()?;
        if let Some(hook) = hook.as_deref_mut() {
            inject(hook, RecoveryPoint::DuringCleanup, index)?;
        }
    }

    if manifest
        .cleanup_remaining
        .iter()
        .any(|item| item == "transaction")
    {
        let marker_remaining = manifest
            .cleanup_remaining
            .iter()
            .any(|item| item == "marker");
        let transaction_root = open_cleanup_transaction_directory(
            state_root,
            &manifest.transaction_directory,
            marker_remaining,
        )?;
        for action in &manifest.actions.clone() {
            for stored in [action.before.as_ref(), action.after.as_ref()]
                .into_iter()
                .flatten()
            {
                let name = CString::new(stored.image.name.as_str())
                    .map_err(|_| RecoveryError::CorruptManifest)?;
                cleanup_file(
                    &transaction_root,
                    &name,
                    stored.image.identity,
                    stored,
                    &format!("image:{}", stored.image.name),
                    state_root,
                    manifest,
                    deadline,
                )?;
            }
        }
        let diff = StoredFile {
            digest: manifest.diff_artifact.clone(),
            mode: 0o600,
            size: manifest.diff_bytes,
            identity: None,
            image: manifest.diff_image.clone(),
        };
        let diff_name =
            CString::new(diff.image.name.as_str()).map_err(|_| RecoveryError::CorruptManifest)?;
        cleanup_file(
            &transaction_root,
            &diff_name,
            diff.image.identity,
            &diff,
            "diff",
            state_root,
            manifest,
            deadline,
        )?;
        cleanup_marker(&transaction_root, state_root, manifest, deadline)?;
        transaction_root.sync_all()?;
    }
    cleanup_transaction_directory(state_root, manifest, deadline)?;
    if !manifest.cleanup_remaining.is_empty() {
        return Err(RecoveryError::CorruptManifest);
    }
    if !committed {
        release_manifest_lease(artifacts, manifest, deadline)?;
    }
    release_verification_leases(
        artifacts,
        &manifest.verification_leases,
        &manifest.transaction,
        deadline,
    )?;
    let mut ledger = read_ledger(state_root)?.ok_or(RecoveryError::CorruptManifest)?;
    ledger.final_state = true;
    replace_ledger(state_root, &ledger)?;
    remove_manifest_if_present(state_root, deadline)?;
    remove_ledger(state_root, deadline)?;
    Ok(())
}

struct MaterializationBudget {
    actions: usize,
    paths: usize,
    images: u64,
    manifest: usize,
    diff: u64,
    total: u64,
    max_actions: usize,
    max_paths: usize,
    max_images: u64,
    max_manifest: usize,
    max_diff: u64,
    max_total: u64,
}

impl MaterializationBudget {
    fn new(options: &MaterializeOptions) -> Self {
        Self {
            actions: 0,
            paths: 0,
            images: 0,
            manifest: 0,
            diff: 0,
            total: 0,
            max_actions: options.max_actions,
            max_paths: options.max_path_bytes,
            max_images: options.max_image_bytes,
            max_manifest: options.max_manifest_bytes,
            max_diff: options.max_diff_bytes,
            max_total: options.max_total_bytes,
        }
    }

    fn total(&mut self, bytes: u64) -> Result<(), RecoveryError> {
        self.total = self
            .total
            .checked_add(bytes)
            .ok_or(RecoveryError::InvalidOptions)?;
        if self.total > self.max_total {
            Err(RecoveryError::InvalidOptions)
        } else {
            Ok(())
        }
    }

    fn actions(&mut self, count: usize) -> Result<(), RecoveryError> {
        self.actions = self
            .actions
            .checked_add(count)
            .ok_or(RecoveryError::InvalidOptions)?;
        if self.actions > self.max_actions {
            return Err(RecoveryError::InvalidOptions);
        }
        self.total(count as u64)
    }

    fn path(&mut self, bytes: usize) -> Result<(), RecoveryError> {
        self.paths = self
            .paths
            .checked_add(bytes)
            .ok_or(RecoveryError::InvalidOptions)?;
        if self.paths > self.max_paths {
            return Err(RecoveryError::InvalidOptions);
        }
        self.total(bytes as u64)
    }

    fn image(&mut self, bytes: u64) -> Result<(), RecoveryError> {
        self.images = self
            .images
            .checked_add(bytes)
            .ok_or(RecoveryError::InvalidOptions)?;
        if self.images > self.max_images {
            return Err(RecoveryError::InvalidOptions);
        }
        self.total(bytes)
    }

    fn manifest(&mut self, bytes: usize) -> Result<(), RecoveryError> {
        self.manifest = bytes;
        if bytes > self.max_manifest {
            return Err(RecoveryError::InvalidOptions);
        }
        self.total(bytes as u64)
    }

    fn diff(&mut self, bytes: u64) -> Result<(), RecoveryError> {
        self.diff = bytes;
        if bytes > self.max_diff {
            return Err(RecoveryError::InvalidOptions);
        }
        self.total(bytes)
    }
}

struct DiffWriter {
    file: File,
    bytes: u64,
    max_bytes: u64,
    preview: Vec<u8>,
    max_preview_bytes: usize,
}

impl DiffWriter {
    fn new(file: File, max_bytes: u64, max_preview_bytes: usize) -> Self {
        Self {
            file,
            bytes: 0,
            max_bytes,
            preview: Vec::with_capacity(max_preview_bytes.min(64 * 1024)),
            max_preview_bytes,
        }
    }

    fn preview(&self, digest: crate::store::artifacts::ArtifactDigest) -> Vec<u8> {
        if self.bytes <= self.max_preview_bytes as u64 {
            return self.preview.clone();
        }
        let mut omitted = self.bytes;
        let mut marker = format!(
            "\n[diff preview truncated; full artifact={digest}; omitted_bytes={omitted}]\n"
        );
        for _ in 0..3 {
            omitted = self
                .bytes
                .saturating_sub(self.max_preview_bytes.saturating_sub(marker.len()) as u64);
            marker = format!(
                "\n[diff preview truncated; full artifact={digest}; omitted_bytes={omitted}]\n"
            );
        }
        if marker.len() >= self.max_preview_bytes {
            let short = b"\n[diff preview truncated]\n";
            return short[..self.max_preview_bytes.min(short.len())].to_vec();
        }
        let keep = self.max_preview_bytes - marker.len();
        let mut preview = self.preview[..keep.min(self.preview.len())].to_vec();
        preview.extend_from_slice(marker.as_bytes());
        preview
    }
}

impl Write for DiffWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("diff size overflow"))?;
        if next > self.max_bytes {
            return Err(io::Error::other("diff exceeds materialization bound"));
        }
        self.file.write_all(bytes)?;
        let remaining = self.max_preview_bytes.saturating_sub(self.preview.len());
        self.preview
            .extend_from_slice(&bytes[..remaining.min(bytes.len())]);
        self.bytes = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[allow(clippy::too_many_arguments)]
fn actual_diff<W: Write>(
    output: &mut W,
    transaction: &str,
    revision: RevisionId,
    principal: &str,
    project: &str,
    plan_digest: &str,
    stage_digest: &str,
    operations: &[StagedOperation],
    actions: &[Action],
    transaction_root: &File,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    let by_path: BTreeMap<&str, &Action> = actions
        .iter()
        .map(|action| (action.path.as_str(), action))
        .collect();
    write!(
        output,
        "{}",
        format_args!(
            "kit-actual-diff-v1\ntransaction={transaction}\nrevision={revision}\nprincipal={principal}\nproject={project}\nplan={plan_digest}\nstage={stage_digest}\n\n"
        )
    )?;
    let mut emitted = BTreeSet::new();
    for operation in operations {
        check_deadline(deadline)?;
        match operation {
            StagedOperation::Move { from, to } => {
                let before = by_path
                    .get(from.as_str())
                    .and_then(|action| action.before.as_ref())
                    .ok_or(RecoveryError::StageChanged)?;
                let after = by_path
                    .get(to.as_str())
                    .and_then(|action| action.after.as_ref())
                    .ok_or(RecoveryError::StageChanged)?;
                append_file_diff(
                    output,
                    from.as_str(),
                    to.as_str(),
                    Some(before),
                    Some(after),
                    transaction_root,
                    true,
                    deadline,
                )?;
                emitted.insert(from.as_str());
                emitted.insert(to.as_str());
            }
            StagedOperation::Add(path)
            | StagedOperation::Delete(path)
            | StagedOperation::Replace(path) => {
                if emitted.insert(path.as_str()) {
                    let action = by_path
                        .get(path.as_str())
                        .ok_or(RecoveryError::StageChanged)?;
                    append_file_diff(
                        output,
                        path.as_str(),
                        path.as_str(),
                        action.before.as_ref(),
                        action.after.as_ref(),
                        transaction_root,
                        false,
                        deadline,
                    )?;
                }
            }
        }
    }
    for action in actions {
        check_deadline(deadline)?;
        if emitted.insert(&action.path) {
            append_file_diff(
                output,
                &action.path,
                &action.path,
                action.before.as_ref(),
                action.after.as_ref(),
                transaction_root,
                false,
                deadline,
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn append_file_diff<W: Write>(
    output: &mut W,
    from: &str,
    to: &str,
    before: Option<&StoredFile>,
    after: Option<&StoredFile>,
    root: &File,
    moved: bool,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    writeln!(output, "diff --git a/{from} b/{to}")?;
    let exact_move = moved
        && matches!((before, after), (Some(before), Some(after)) if before.digest == after.digest && before.mode == after.mode);
    if exact_move {
        writeln!(output, "similarity index 100%")?;
    }
    if moved {
        writeln!(output, "rename from {from}")?;
        writeln!(output, "rename to {to}")?;
    }
    match (before, after) {
        (None, Some(after)) => writeln!(output, "new file mode {:06o}", after.mode)?,
        (Some(before), None) => writeln!(output, "deleted file mode {:06o}", before.mode)?,
        (Some(before), Some(after)) if before.mode != after.mode => {
            writeln!(output, "old mode {:06o}", before.mode)?;
            writeln!(output, "new mode {:06o}", after.mode)?;
        }
        _ => {}
    }
    if exact_move {
        return Ok(());
    }
    let old_binary = before
        .map(|file| inspect_image_before(root, file, deadline))
        .transpose()?;
    let new_binary = after
        .map(|file| inspect_image_before(root, file, deadline))
        .transpose()?;
    if old_binary == Some(true) || new_binary == Some(true) {
        writeln!(
            output,
            "Binary files {} and {} differ",
            if before.is_some() {
                format!("a/{from}")
            } else {
                "/dev/null".to_owned()
            },
            if after.is_some() {
                format!("b/{to}")
            } else {
                "/dev/null".to_owned()
            }
        )?;
        return Ok(());
    }
    writeln!(
        output,
        "--- {}",
        if before.is_some() {
            format!("a/{from}")
        } else {
            "/dev/null".to_owned()
        }
    )?;
    writeln!(
        output,
        "+++ {}",
        if after.is_some() {
            format!("b/{to}")
        } else {
            "/dev/null".to_owned()
        }
    )?;
    let old_count = before
        .map(|file| image_line_count(root, file, deadline))
        .transpose()?
        .unwrap_or(0);
    let new_count = after
        .map(|file| image_line_count(root, file, deadline))
        .transpose()?
        .unwrap_or(0);
    writeln!(
        output,
        "@@ -{} +{} @@",
        hunk_range(old_count),
        hunk_range(new_count)
    )?;
    if let Some(file) = before {
        append_image_lines(output, b'-', root, file, deadline)?;
    }
    if let Some(file) = after {
        append_image_lines(output, b'+', root, file, deadline)?;
    }
    Ok(())
}

fn append_image_lines<W: Write>(
    output: &mut W,
    marker: u8,
    root: &File,
    stored: &StoredFile,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    let mut file = open_image(root, stored)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut line_start = true;
    loop {
        check_deadline(deadline)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let mut start = 0;
        for (index, byte) in buffer[..count].iter().enumerate() {
            if line_start {
                output.write_all(&[marker])?;
                line_start = false;
            }
            if *byte == b'\n' {
                output.write_all(&buffer[start..=index])?;
                start = index + 1;
                line_start = true;
            }
        }
        if start < count {
            output.write_all(&buffer[start..count])?;
        }
    }
    if !line_start {
        output.write_all(b"\n\\ No newline at end of file\n")?;
    }
    Ok(())
}

fn image_line_count(
    root: &File,
    stored: &StoredFile,
    deadline: Instant,
) -> Result<usize, RecoveryError> {
    let mut file = open_image(root, stored)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut count = 0_usize;
    let mut last = None;
    loop {
        check_deadline(deadline)?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count = count
            .checked_add(buffer[..read].iter().filter(|byte| **byte == b'\n').count())
            .ok_or(RecoveryError::InvalidOptions)?;
        last = Some(buffer[read - 1]);
    }
    Ok(count + usize::from(last.is_some_and(|byte| byte != b'\n')))
}

fn hunk_range(lines: usize) -> String {
    match lines {
        0 => "0,0".to_owned(),
        1 => "1".to_owned(),
        count => format!("1,{count}"),
    }
}

fn inspect_image_before(
    root: &File,
    stored: &StoredFile,
    deadline: Instant,
) -> Result<bool, RecoveryError> {
    let mut file = open_image(root, stored)?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut tail = Vec::new();
    let mut binary = false;
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0_u64;
    loop {
        check_deadline(deadline)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .ok_or(RecoveryError::CorruptManifest)?;
        hasher.update(&buffer[..count]);
        binary |= buffer[..count].contains(&0);
        if !binary {
            tail.extend_from_slice(&buffer[..count]);
            match std::str::from_utf8(&tail) {
                Ok(_) => tail.clear(),
                Err(error) if error.error_len().is_some() => binary = true,
                Err(error) => {
                    let valid = error.valid_up_to();
                    tail.drain(..valid);
                    if tail.len() > 3 {
                        binary = true;
                    }
                }
            }
        }
    }
    if bytes != stored.size || format!("blake3:{}", hasher.finalize().to_hex()) != stored.digest {
        return Err(RecoveryError::CorruptManifest);
    }
    Ok(binary || !tail.is_empty())
}

fn operation_records(operations: &[StagedOperation]) -> Vec<OperationRecord> {
    operations
        .iter()
        .map(|operation| match operation {
            StagedOperation::Add(path) => OperationRecord {
                kind: "add".to_owned(),
                path: path.as_str().to_owned(),
                destination: None,
            },
            StagedOperation::Delete(path) => OperationRecord {
                kind: "delete".to_owned(),
                path: path.as_str().to_owned(),
                destination: None,
            },
            StagedOperation::Move { from, to } => OperationRecord {
                kind: "move".to_owned(),
                path: from.as_str().to_owned(),
                destination: Some(to.as_str().to_owned()),
            },
            StagedOperation::Replace(path) => OperationRecord {
                kind: "replace".to_owned(),
                path: path.as_str().to_owned(),
                destination: None,
            },
        })
        .collect()
}

fn minimum_retention(
    requested: ArtifactRetention,
    now: i64,
) -> Result<ArtifactRetention, RecoveryError> {
    let minimum = now
        .checked_add(
            i64::try_from(MIN_DIFF_RETENTION.as_micros())
                .map_err(|_| RecoveryError::InvalidOptions)?,
        )
        .ok_or(RecoveryError::InvalidOptions)?;
    Ok(match requested {
        ArtifactRetention::Forever => ArtifactRetention::Forever,
        ArtifactRetention::UntilUnixMicros(expiry) => {
            ArtifactRetention::UntilUnixMicros(expiry.max(minimum))
        }
    })
}

fn retention_string(retention: ArtifactRetention) -> String {
    match retention {
        ArtifactRetention::Forever => "forever".to_owned(),
        ArtifactRetention::UntilUnixMicros(expiry) => format!("until:{expiry}"),
    }
}

fn verify_diff_artifact(
    artifact: &VerifiedArtifact,
    manifest: &Manifest,
) -> Result<(), RecoveryError> {
    let metadata = artifact.manifest();
    if artifact.digest().to_string() != manifest.diff_artifact
        || artifact.reference().to_string() != manifest.diff_reference
        || metadata.size != manifest.diff_bytes
        || metadata.media_type != manifest.diff_media_type
        || metadata.class != ArtifactClass::Diff
        || manifest.diff_class != "diff"
        || metadata.principal != manifest.principal
        || metadata.project != manifest.project
        || retention_string(metadata.retention) != manifest.diff_retention
        || metadata.stored_at_unix_micros != manifest.diff_stored_at_unix_micros
    {
        return Err(RecoveryError::CorruptManifest);
    }
    Ok(())
}

fn resolve_recovery_artifacts(
    path: &str,
    expected: &ObjectIdentity,
    resolve: &mut impl FnMut(&std::path::Path) -> Result<ArtifactStore, artifacts::ArtifactError>,
) -> Result<ArtifactStore, RecoveryError> {
    let path = std::path::Path::new(path);
    if !path.is_absolute() {
        return Err(RecoveryError::CorruptManifest);
    }
    let root = File::open(path)?;
    require_object_identity(&root, expected)?;
    let store = resolve(path)?;
    let root = File::open(store.root())?;
    require_object_identity(&root, expected)?;
    Ok(store)
}

fn release_ledger_lease(
    ledger: &Ledger,
    resolve: &mut impl FnMut(&std::path::Path) -> Result<ArtifactStore, artifacts::ArtifactError>,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    if ledger.diff_artifact.is_none() && ledger.verification_leases.is_empty() {
        return Ok(());
    }
    let store =
        resolve_recovery_artifacts(&ledger.artifact_store_path, &ledger.artifact_store, resolve)?;
    release_verification_leases(
        &store,
        &ledger.verification_leases,
        &ledger.transaction,
        deadline,
    )?;
    if let (Some(digest), Some(lease)) = (&ledger.diff_artifact, &ledger.diff_lease) {
        let digest = artifacts::ArtifactDigest::parse(digest)?;
        store.release_lease_with_id_before(digest, lease, &ledger.transaction, deadline)?;
    }
    Ok(())
}

fn transaction_lease_id(ledger: &Ledger) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"kit-edit-artifact-lease-v1\0");
    hasher.update(ledger.nonce.as_bytes());
    hasher.update(&ledger.workspace.device.to_le_bytes());
    hasher.update(&ledger.workspace.inode.to_le_bytes());
    hasher.update(ledger.workspace.mount.as_bytes());
    hasher.finalize().to_hex()[..32].to_owned()
}

fn verification_lease_id(
    nonce: &str,
    workspace: &ObjectIdentity,
    digest: artifacts::ArtifactDigest,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"kit-edit-verification-artifact-lease-v1\0");
    hasher.update(nonce.as_bytes());
    hasher.update(&workspace.device.to_le_bytes());
    hasher.update(&workspace.inode.to_le_bytes());
    hasher.update(workspace.mount.as_bytes());
    hasher.update(&digest.as_bytes());
    hasher.finalize().to_hex()[..32].to_owned()
}

fn verification_artifact_digests(
    artifacts: &ArtifactStore,
    receipt: &crate::verify::profiles::VerificationReceipt,
) -> Result<Vec<artifacts::ArtifactDigest>, RecoveryError> {
    let mut references = vec![receipt.result_artifact.reference.as_str()];
    references.extend(
        receipt
            .stdout_artifacts
            .iter()
            .chain(&receipt.stderr_artifacts)
            .map(|artifact| artifact.reference.as_str()),
    );
    references.extend(receipt.process_artifacts.iter().filter_map(|artifact| {
        if let crate::verify::profiles::VerificationProcessReference::Report { reference, .. } =
            artifact
        {
            Some(reference.as_str())
        } else {
            None
        }
    }));
    let mut digests = BTreeSet::new();
    for reference in references {
        let reference = artifacts::ArtifactReference::parse(reference)?;
        digests.insert(artifacts.open_reference(reference)?.digest());
    }
    Ok(digests.into_iter().collect())
}

fn release_verification_leases(
    artifacts: &ArtifactStore,
    leases: &[RecoveryArtifactLease],
    transaction: &str,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    for lease in leases {
        artifacts.release_lease_with_id_before(
            artifacts::ArtifactDigest::parse(&lease.digest)?,
            &lease.lease,
            transaction,
            deadline,
        )?;
    }
    Ok(())
}

fn validate_verification_leases(
    artifacts: &ArtifactStore,
    manifest: &Manifest,
) -> Result<(), RecoveryError> {
    let expected = verification_artifact_digests(artifacts, &manifest.verification)?;
    if manifest.verification_leases.len() != expected.len() {
        return Err(RecoveryError::CorruptManifest);
    }
    for (lease, expected_digest) in manifest.verification_leases.iter().zip(expected) {
        let digest = artifacts::ArtifactDigest::parse(&lease.digest)?;
        if digest != expected_digest
            || lease.lease != verification_lease_id(&manifest.nonce, &manifest.workspace, digest)
        {
            return Err(RecoveryError::CorruptManifest);
        }
        artifacts.open_lease(digest, &lease.lease, &manifest.transaction)?;
    }
    Ok(())
}

fn manifest_lease(
    artifacts: &ArtifactStore,
    manifest: &Manifest,
) -> Result<ArtifactLease, RecoveryError> {
    let digest = artifacts::ArtifactDigest::parse(&manifest.diff_artifact)?;
    Ok(artifacts.open_lease(digest, &manifest.diff_lease, &manifest.transaction)?)
}

fn diff_reference_owner(manifest: &Manifest) -> Result<String, RecoveryError> {
    if RevisionId::parse(&manifest.expected_final_revision).is_none() {
        return Err(RecoveryError::CorruptManifest);
    }
    Ok(format!(
        "workspace-revision:{}:{}",
        manifest.expected_final_revision, manifest.transaction
    ))
}

fn retain_committed_diff(
    artifacts: &ArtifactStore,
    state_root: &File,
    manifest: &mut Manifest,
    revision: RevisionId,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    if revision.to_string() != manifest.expected_final_revision {
        return Err(RecoveryError::CorruptManifest);
    }
    let owner = diff_reference_owner(manifest)?;
    let digest = artifacts::ArtifactDigest::parse(&manifest.diff_artifact)?;
    if manifest.diff_owner_referenced {
        return if artifacts.reference_exists(digest, &owner)? {
            Ok(())
        } else {
            Err(RecoveryError::CorruptManifest)
        };
    }
    let lease = manifest_lease(artifacts, manifest)?;
    artifacts.transfer_lease_to_reference_before(&lease, &owner, deadline)?;
    manifest.diff_owner_referenced = true;
    replace_manifest(state_root, manifest)
}

fn release_manifest_lease(
    artifacts: &ArtifactStore,
    manifest: &Manifest,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    if !manifest.diff_owner_referenced {
        let lease = manifest_lease(artifacts, manifest)?;
        artifacts.release_lease_before(&lease, deadline)?;
    }
    Ok(())
}

fn initialize_cleanup(manifest: &mut Manifest) {
    for (index, action) in manifest.actions.iter().enumerate() {
        if let Some(before) = &action.before {
            let key = format!("undo:{index}");
            manifest.cleanup_remaining.push(key.clone());
            manifest.cleanup_intents.push(cleanup_intent(
                &manifest.nonce,
                &key,
                &file_identity_bytes(before.identity.unwrap_or(before.image.identity)),
            ));
        }
        if let Some(after) = &action.after {
            let key = format!("new:{index}");
            manifest.cleanup_remaining.push(key.clone());
            manifest.cleanup_intents.push(cleanup_intent(
                &manifest.nonce,
                &key,
                &file_identity_bytes(after.identity.unwrap_or(after.image.identity)),
            ));
        }
        for stored in [action.before.as_ref(), action.after.as_ref()]
            .into_iter()
            .flatten()
        {
            let key = format!("image:{}", stored.image.name);
            manifest.cleanup_remaining.push(key.clone());
            manifest.cleanup_intents.push(cleanup_intent(
                &manifest.nonce,
                &key,
                &file_identity_bytes(stored.image.identity),
            ));
        }
    }
    for (key, identity) in [
        ("diff", manifest.diff_image.identity),
        ("marker", manifest.transaction_directory.marker),
    ] {
        manifest.cleanup_remaining.push(key.to_owned());
        manifest.cleanup_intents.push(cleanup_intent(
            &manifest.nonce,
            key,
            &file_identity_bytes(identity),
        ));
    }
    manifest.cleanup_remaining.push("transaction".to_owned());
    manifest.cleanup_intents.push(cleanup_intent(
        &manifest.nonce,
        "transaction",
        &directory_identity_digest(&manifest.transaction_directory),
    ));
}

fn cleanup_intent(nonce: &str, key: &str, identity: &[u8]) -> CleanupIntent {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"kit-edit-cleanup-v1\0");
    hasher.update(nonce.as_bytes());
    hasher.update(&[0]);
    hasher.update(key.as_bytes());
    hasher.update(&[0]);
    hasher.update(identity);
    CleanupIntent {
        key: key.to_owned(),
        quarantine: format!(
            ".kit-edit-quarantine-{nonce}-{}",
            &hasher.finalize().to_hex()[..24]
        ),
    }
}

fn cleanup_quarantine<'a>(
    intents: &'a [CleanupIntent],
    key: &str,
) -> Result<&'a str, RecoveryError> {
    intents
        .iter()
        .find(|intent| intent.key == key)
        .map(|intent| intent.quarantine.as_str())
        .ok_or(RecoveryError::CorruptManifest)
}

fn file_identity_bytes(identity: FileIdentity) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&identity.device.to_le_bytes());
    bytes[8..].copy_from_slice(&identity.inode.to_le_bytes());
    bytes
}

fn object_identity_digest(identity: &ObjectIdentity) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&identity.device.to_le_bytes());
    hasher.update(&identity.inode.to_le_bytes());
    hasher.update(identity.mount.as_bytes());
    *hasher.finalize().as_bytes()
}

fn directory_identity_digest(identity: &DirectoryIdentity) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(identity.name.as_bytes());
    hasher.update(&identity.device.to_le_bytes());
    hasher.update(&identity.inode.to_le_bytes());
    hasher.update(&file_identity_bytes(identity.marker));
    *hasher.finalize().as_bytes()
}

fn object_identity(file: &File) -> Result<ObjectIdentity, RecoveryError> {
    let stat = stat_file(file)?;
    Ok(ObjectIdentity {
        device: stat.identity.device,
        inode: stat.identity.inode,
        mount: hex(&mount_identity(file)?.0),
    })
}

fn require_object_identity(file: &File, expected: &ObjectIdentity) -> Result<(), RecoveryError> {
    if object_identity(file)? == *expected {
        Ok(())
    } else {
        Err(RecoveryError::CorruptManifest)
    }
}

fn move_peers(operations: &[StagedOperation]) -> BTreeMap<String, String> {
    let mut peers = BTreeMap::new();
    for operation in operations {
        if let StagedOperation::Move { from, to } = operation {
            peers.insert(from.as_str().to_owned(), to.as_str().to_owned());
            peers.insert(to.as_str().to_owned(), from.as_str().to_owned());
        }
    }
    peers
}

fn create_transaction_directory(
    state_root: &File,
    nonce: &str,
) -> Result<DirectoryIdentity, RecoveryError> {
    let name = format!(".kit-edit-recovery-{nonce}");
    let cname = CString::new(name.as_str()).map_err(|_| RecoveryError::CorruptManifest)?;
    mkdir(state_root, &cname, 0o700)?;
    system_crash(RecoveryPoint::TransactionMkdir, 0);
    let root = open_named_directory(state_root, &name)?;
    let stat = stat_file(&root)?;
    if stat.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32
        || stat.mode & 0o777 != 0o700
        || stat.links < 2
    {
        return Err(RecoveryError::UnsafeEntry(name));
    }
    let mut marker = create_file(&root, TX_MARKER_TEMP_NAME, 0o600)?;
    marker.write_all(format!("kit-edit-transaction-v1\nnonce={nonce}\n").as_bytes())?;
    system_crash(RecoveryPoint::TransactionMarkerWrite, 0);
    marker.sync_all()?;
    system_crash(RecoveryPoint::TransactionMarkerSync, 0);
    let marker_identity = stat_file(&marker)?.identity;
    rename_noreplace(&root, TX_MARKER_TEMP_NAME, &root, TX_MARKER_NAME)?;
    root.sync_all()?;
    state_root.sync_all()?;
    Ok(DirectoryIdentity {
        name,
        device: stat.identity.device,
        inode: stat.identity.inode,
        marker: marker_identity,
    })
}

fn create_manifest(state_root: &File, manifest: &Manifest) -> Result<(), RecoveryError> {
    atomic_json(
        state_root,
        MANIFEST_NAME,
        manifest,
        manifest.max_manifest_bytes,
        true,
    )
}

fn replace_manifest(state_root: &File, manifest: &Manifest) -> Result<(), RecoveryError> {
    atomic_json(
        state_root,
        MANIFEST_NAME,
        manifest,
        manifest.max_manifest_bytes,
        false,
    )
}

fn append_manifest(state_root: &File, manifest: &Manifest) -> Result<(), RecoveryError> {
    replace_manifest(state_root, manifest)
}

fn create_ledger(state_root: &File, ledger: &Ledger, limit: usize) -> Result<(), RecoveryError> {
    atomic_json(state_root, LEDGER_NAME, ledger, limit, true)
}

fn replace_ledger(state_root: &File, ledger: &Ledger) -> Result<(), RecoveryError> {
    atomic_json(
        state_root,
        LEDGER_NAME,
        ledger,
        MAX_RECOVERY_STATE_BYTES,
        false,
    )
}

fn atomic_json<T: Serialize>(
    parent: &File,
    target: &CStr,
    value: &T,
    limit: usize,
    create_only: bool,
) -> Result<(), RecoveryError> {
    if limit == 0 || limit > MAX_RECOVERY_STATE_BYTES {
        return Err(RecoveryError::InvalidOptions);
    }
    let bytes = serde_json::to_vec(value).map_err(|_| RecoveryError::CorruptManifest)?;
    if bytes.len() > limit {
        return Err(RecoveryError::CorruptManifest);
    }
    let temp_name = if target == MANIFEST_NAME {
        MANIFEST_TEMP_NAME
    } else if target == LEDGER_NAME {
        LEDGER_TEMP_NAME
    } else {
        return Err(RecoveryError::CorruptManifest);
    };
    let mut temp = create_file(parent, temp_name, 0o600)?;
    temp.write_all(&bytes)?;
    system_crash(
        if target == MANIFEST_NAME {
            RecoveryPoint::ManifestTempWrite
        } else {
            RecoveryPoint::LedgerTempWrite
        },
        0,
    );
    temp.sync_all()?;
    system_crash(
        if target == MANIFEST_NAME {
            RecoveryPoint::ManifestFileSync
        } else {
            RecoveryPoint::LedgerFileSync
        },
        0,
    );
    if create_only {
        rename_noreplace(parent, temp_name, parent, target)?;
    } else {
        publish_replace(parent, temp_name, target)?;
    }
    system_crash(
        if target == MANIFEST_NAME {
            RecoveryPoint::ManifestRename
        } else {
            RecoveryPoint::LedgerRename
        },
        0,
    );
    parent.sync_all()?;
    system_crash(
        if target == MANIFEST_NAME {
            RecoveryPoint::ManifestDirectorySync
        } else {
            RecoveryPoint::LedgerDirectorySync
        },
        0,
    );
    Ok(())
}

fn read_manifest(state_root: &File) -> Result<Option<Manifest>, RecoveryError> {
    let mut file = match open_component(state_root, MANIFEST_NAME, libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let stat = stat_file(&file)?;
    validate_regular(MANIFEST_NAME.to_string_lossy().as_ref(), stat)?;
    if stat.size == 0 || stat.size > MAX_RECOVERY_STATE_BYTES as u64 {
        return Err(RecoveryError::CorruptManifest);
    }
    let mut bytes = Vec::with_capacity(stat.size as usize);
    file.read_to_end(&mut bytes)?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|_| RecoveryError::CorruptManifest)?;
    if bytes.len() > manifest.max_manifest_bytes
        || manifest.max_manifest_bytes > MAX_RECOVERY_STATE_BYTES
    {
        return Err(RecoveryError::CorruptManifest);
    }
    Ok(Some(manifest))
}

fn read_ledger(state_root: &File) -> Result<Option<Ledger>, RecoveryError> {
    let mut file = match open_component(state_root, LEDGER_NAME, libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let stat = stat_file(&file)?;
    validate_regular(LEDGER_NAME.to_string_lossy().as_ref(), stat)?;
    if stat.size == 0 || stat.size > MAX_RECOVERY_STATE_BYTES as u64 {
        return Err(RecoveryError::CorruptManifest);
    }
    let mut bytes = Vec::with_capacity(stat.size as usize);
    file.read_to_end(&mut bytes)?;
    let ledger = serde_json::from_slice(&bytes).map_err(|_| RecoveryError::CorruptManifest)?;
    Ok(Some(ledger))
}

fn validate_manifest(
    manifest: &Manifest,
    state_root: &File,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    check_deadline(deadline)?;
    if manifest.version != RECOVERY_MANIFEST_VERSION
        || manifest.transaction != format!("edit:{}", manifest.nonce)
        || manifest.nonce.len() != 32
        || manifest.actions.is_empty()
        || manifest.max_manifest_bytes == 0
        || manifest.max_manifest_bytes > MAX_RECOVERY_STATE_BYTES
        || manifest.max_actions == 0
        || manifest.actions.len() > manifest.max_actions
        || manifest.max_path_bytes == 0
        || manifest.max_image_bytes == 0
        || manifest.max_diff_bytes == 0
        || manifest.max_total_bytes == 0
        || manifest.max_time_millis == 0
        || manifest.max_time_millis > MAX_STARTUP_RECOVERY_TIME.as_millis() as u64
        || manifest.diff_class != "diff"
        || manifest.diff_media_type != "text/x-diff; charset=utf-8"
        || manifest.diff_bytes == 0
    {
        return Err(RecoveryError::CorruptManifest);
    }
    RevisionId::parse(&manifest.expected_base_revision).ok_or(RecoveryError::CorruptManifest)?;
    RevisionId::parse(&manifest.expected_final_revision).ok_or(RecoveryError::CorruptManifest)?;
    if !manifest.expected_base_epoch.starts_with("e:")
        || manifest.expected_base_epoch.len() != 34
        || !manifest.expected_final_epoch.starts_with("e:")
        || manifest.expected_final_epoch.len() != 34
    {
        return Err(RecoveryError::CorruptManifest);
    }
    if !valid_digest(&manifest.plan_digest)
        || !valid_digest(&manifest.stage_digest)
        || !valid_digest(&manifest.expected_final_digest)
        || !valid_digest(&manifest.diff_artifact)
        || artifacts::ArtifactReference::parse(&manifest.diff_reference).is_err()
        || !valid_verification_leases(
            &manifest.verification_leases,
            &manifest.nonce,
            &manifest.workspace,
        )
    {
        return Err(RecoveryError::CorruptManifest);
    }
    crate::domain::ids::PrincipalId::parse(&manifest.principal)
        .map_err(|_| RecoveryError::CorruptManifest)?;
    crate::domain::ids::ProjectId::parse(&manifest.project)
        .map_err(|_| RecoveryError::CorruptManifest)?;
    let minimum_expiry = manifest
        .diff_stored_at_unix_micros
        .checked_add(
            i64::try_from(MIN_DIFF_RETENTION.as_micros())
                .map_err(|_| RecoveryError::CorruptManifest)?,
        )
        .ok_or(RecoveryError::CorruptManifest)?;
    if manifest.diff_retention != "forever"
        && manifest
            .diff_retention
            .strip_prefix("until:")
            .and_then(|expiry| expiry.parse::<i64>().ok())
            .is_none_or(|expiry| expiry < minimum_expiry)
    {
        return Err(RecoveryError::CorruptManifest);
    }
    let mut paths = BTreeSet::new();
    let mut cleanup_keys = BTreeSet::new();
    let mut path_bytes = 0_usize;
    let mut image_bytes = 0_u64;
    for (index, action) in manifest.actions.iter().enumerate() {
        RootRelativePath::parse(&action.path, usize::MAX)
            .map_err(|_| RecoveryError::CorruptManifest)?;
        if !paths.insert(&action.path)
            || action.new_temp.contains('/')
            || action.undo_temp.contains('/')
            || !action
                .new_temp
                .starts_with(&format!(".kit-edit-{}-", manifest.nonce))
            || !action
                .undo_temp
                .starts_with(&format!(".kit-edit-{}-", manifest.nonce))
        {
            return Err(RecoveryError::CorruptManifest);
        }
        path_bytes = path_bytes
            .checked_add(action.path.len())
            .ok_or(RecoveryError::CorruptManifest)?;
        for file in [action.before.as_ref(), action.after.as_ref()]
            .into_iter()
            .flatten()
        {
            if !valid_digest(&file.digest) || file.mode & !0o777 != 0 {
                return Err(RecoveryError::CorruptManifest);
            }
            image_bytes = image_bytes
                .checked_add(file.size)
                .ok_or(RecoveryError::CorruptManifest)?;
            cleanup_keys.insert(format!("image:{}", file.image.name));
        }
        if action.before.is_some() {
            cleanup_keys.insert(format!("undo:{index}"));
        }
        if action.after.is_some() {
            cleanup_keys.insert(format!("new:{index}"));
        }
    }
    cleanup_keys.extend([
        "diff".to_owned(),
        "marker".to_owned(),
        "transaction".to_owned(),
    ]);
    if path_bytes > manifest.max_path_bytes
        || image_bytes > manifest.max_image_bytes
        || manifest.diff_bytes > manifest.max_diff_bytes
        || image_bytes
            .checked_add(manifest.diff_bytes)
            .is_none_or(|total| total > manifest.max_total_bytes)
        || !valid_hex_string(&manifest.diff_lease, 32)
        || !valid_cleanup_intents(&manifest.cleanup_intents, &manifest.nonce, &cleanup_keys)
    {
        return Err(RecoveryError::CorruptManifest);
    }
    require_object_identity(state_root, &manifest.metadata_store)?;
    if manifest
        .cleanup_remaining
        .iter()
        .any(|item| item == "transaction")
    {
        let marker_remaining = manifest
            .cleanup_remaining
            .iter()
            .any(|item| item == "marker");
        let transaction = open_cleanup_transaction_directory(
            state_root,
            &manifest.transaction_directory,
            marker_remaining,
        )?;
        if manifest.cleanup_remaining.iter().any(|item| item == "diff") {
            let diff = StoredFile {
                digest: manifest.diff_artifact.clone(),
                mode: 0o600,
                size: manifest.diff_bytes,
                identity: None,
                image: manifest.diff_image.clone(),
            };
            inspect_image_before(&transaction, &diff, deadline)?;
        }
    }
    Ok(())
}

fn validate_ledger(
    ledger: &Ledger,
    workspace: &File,
    state_root: &File,
) -> Result<(), RecoveryError> {
    if ledger.version != RECOVERY_MANIFEST_VERSION
        || ledger.transaction != format!("edit:{}", ledger.nonce)
        || ledger.nonce.len() != 32
        || ledger.transaction_name != format!(".kit-edit-recovery-{}", ledger.nonce)
        || ledger
            .transaction_directory
            .as_ref()
            .is_some_and(|directory| directory.name != ledger.transaction_name)
        || !std::path::Path::new(&ledger.artifact_store_path).is_absolute()
        || ledger.diff_reference.is_some() != ledger.diff_artifact.is_some()
        || ledger.diff_artifact.is_some() != ledger.diff_lease.is_some()
        || ledger
            .diff_reference
            .as_deref()
            .is_some_and(|reference| artifacts::ArtifactReference::parse(reference).is_err())
        || ledger
            .diff_artifact
            .as_deref()
            .is_some_and(|digest| !valid_digest(digest))
        || ledger
            .diff_lease
            .as_deref()
            .is_some_and(|lease| !valid_hex_string(lease, 32))
        || ledger
            .diff_lease
            .as_deref()
            .is_some_and(|lease| lease != transaction_lease_id(ledger))
        || !valid_verification_leases(
            &ledger.verification_leases,
            &ledger.nonce,
            &ledger.workspace,
        )
        || !valid_cleanup_intents(
            &ledger.cleanup_intents,
            &ledger.nonce,
            &BTreeSet::from([
                "transaction".to_owned(),
                "manifest".to_owned(),
                "ledger".to_owned(),
            ]),
        )
    {
        return Err(RecoveryError::CorruptManifest);
    }
    require_object_identity(workspace, &ledger.workspace)?;
    require_object_identity(state_root, &ledger.metadata_store)
}

fn remove_partial_transaction(
    state_root: &File,
    ledger: &Ledger,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    check_deadline(deadline)?;
    let authority = if let Some(expected) = &ledger.transaction_directory {
        DirectoryAuthority::Bound {
            expected,
            require_marker: true,
        }
    } else {
        DirectoryAuthority::PreMarker {
            nonce: &ledger.nonce,
        }
    };
    remove_quarantined_directory(
        state_root,
        &ledger.transaction_name,
        cleanup_quarantine(&ledger.cleanup_intents, "transaction")?,
        authority,
        deadline,
    )
}

fn validate_pre_marker_directory(
    root: &File,
    nonce: &str,
    deadline: Instant,
) -> Result<Option<FileIdentity>, RecoveryError> {
    let names = directory_entries(root, deadline)?;
    match names.as_slice() {
        [] => Ok(None),
        [name] if name.as_c_str() == TX_MARKER_TEMP_NAME => {
            let mut partial = open_component(root, TX_MARKER_TEMP_NAME, libc::O_RDONLY)?;
            let stat = stat_file(&partial)?;
            validate_regular(TX_MARKER_TEMP_NAME.to_string_lossy().as_ref(), stat)?;
            if stat.uid != unsafe { libc::geteuid() }
                || stat.mode & 0o7777 != 0o600
                || stat.size > MAX_PARTIAL_MARKER_BYTES
            {
                return Err(RecoveryError::CorruptManifest);
            }
            let mut bytes = Vec::new();
            Read::by_ref(&mut partial)
                .take(MAX_PARTIAL_MARKER_BYTES + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 != stat.size || stat_file(&partial)?.identity != stat.identity {
                return Err(RecoveryError::CorruptManifest);
            }
            Ok(None)
        }
        [name] if name.as_c_str() == TX_MARKER_NAME => {
            let marker = stat_at(root, TX_MARKER_NAME)?;
            validate_transaction_marker(root, marker.identity, nonce)?;
            Ok(Some(marker.identity))
        }
        _ => Err(RecoveryError::CorruptManifest),
    }
}

fn directory_entries(root: &File, deadline: Instant) -> Result<Vec<CString>, RecoveryError> {
    let duplicate = open_component(root, c".", libc::O_RDONLY | libc::O_DIRECTORY)?.into_raw_fd();
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(io::Error::last_os_error().into());
    }
    let mut names: Vec<CString> = Vec::new();
    loop {
        check_deadline(deadline)?;
        clear_errno();
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            unsafe { libc::closedir(stream) };
            if error.raw_os_error().unwrap_or(0) != 0 {
                return Err(error.into());
            }
            names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
            return Ok(names);
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if !matches!(name.to_bytes(), b"." | b"..") {
            names.push(CString::new(name.to_bytes()).map_err(|_| RecoveryError::CorruptManifest)?);
        }
    }
}

fn directory_empty(root: &File, deadline: Instant) -> Result<bool, RecoveryError> {
    let duplicate = open_component(root, c".", libc::O_RDONLY | libc::O_DIRECTORY)?.into_raw_fd();
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(io::Error::last_os_error().into());
    }
    loop {
        check_deadline(deadline)?;
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            unsafe { libc::closedir(stream) };
            return Ok(true);
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if !matches!(name.to_bytes(), b"." | b"..") {
            unsafe { libc::closedir(stream) };
            return Ok(false);
        }
    }
}

fn valid_digest(value: &str) -> bool {
    value
        .strip_prefix("blake3:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn valid_hex_string(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_verification_leases(
    leases: &[RecoveryArtifactLease],
    nonce: &str,
    workspace: &ObjectIdentity,
) -> bool {
    let mut previous = None;
    for lease in leases {
        let Ok(digest) = artifacts::ArtifactDigest::parse(&lease.digest) else {
            return false;
        };
        if previous.is_some_and(|previous| previous >= digest)
            || lease.lease != verification_lease_id(nonce, workspace, digest)
        {
            return false;
        }
        previous = Some(digest);
    }
    true
}

fn valid_cleanup_intents(
    intents: &[CleanupIntent],
    nonce: &str,
    expected: &BTreeSet<String>,
) -> bool {
    let prefix = format!(".kit-edit-quarantine-{nonce}-");
    intents.len() == expected.len()
        && intents.iter().all(|intent| {
            expected.contains(&intent.key)
                && intent.quarantine.starts_with(&prefix)
                && intent.quarantine.len() == prefix.len() + 24
                && intent.quarantine[prefix.len()..]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        && intents
            .iter()
            .map(|intent| intent.key.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            == intents.len()
}

fn ensure_no_recovery(state_root: &File) -> Result<(), RecoveryError> {
    for name in [LEDGER_NAME, MANIFEST_NAME] {
        match stat_at(state_root, name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(RecoveryError::Conflict(
                    "pending recovery transaction".to_owned(),
                ));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn remove_ledger(state_root: &File, deadline: Instant) -> Result<(), RecoveryError> {
    let expected = stat_at(state_root, LEDGER_NAME)?.identity;
    remove_open_identity(state_root, LEDGER_NAME, expected)?;
    system_crash(RecoveryPoint::CleanupLedgerRemove, 0);
    state_root.sync_all()?;
    system_crash(RecoveryPoint::CleanupLedgerDirectorySync, 0);
    check_deadline(deadline)
}

fn cleanup_atomic_temps(state_root: &File, deadline: Instant) -> Result<(), RecoveryError> {
    check_deadline(deadline)?;
    for name in [MANIFEST_TEMP_NAME, LEDGER_TEMP_NAME] {
        match stat_at_optional(state_root, name)? {
            None => {}
            Some(stat) => {
                validate_regular(name.to_string_lossy().as_ref(), stat)?;
                remove_open_identity(state_root, name, stat.identity)?;
                state_root.sync_all()?;
            }
        }
    }
    Ok(())
}

fn open_transaction_directory(
    state_root: &File,
    expected: &DirectoryIdentity,
) -> Result<File, RecoveryError> {
    let root = open_named_directory(state_root, &expected.name)?;
    let stat = stat_file(&root)?;
    if stat.identity.device != expected.device
        || stat.identity.inode != expected.inode
        || stat.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32
        || stat.mode & 0o777 != 0o700
    {
        return Err(RecoveryError::CorruptManifest);
    }
    let nonce = expected
        .name
        .strip_prefix(".kit-edit-recovery-")
        .ok_or(RecoveryError::CorruptManifest)?;
    validate_transaction_marker(&root, expected.marker, nonce)?;
    Ok(root)
}

fn open_cleanup_transaction_directory(
    state_root: &File,
    expected: &DirectoryIdentity,
    require_marker: bool,
) -> Result<File, RecoveryError> {
    let root = open_named_directory(state_root, &expected.name)?;
    let stat = stat_file(&root)?;
    require_directory_identity(stat, expected)?;
    if stat.uid != unsafe { libc::geteuid() }
        || stat.mode & 0o7777 != 0o700
        || mount_identity(&root)? != mount_identity(state_root)?
    {
        return Err(RecoveryError::CorruptManifest);
    }
    match stat_at_optional(&root, TX_MARKER_NAME)? {
        Some(_) => validate_transaction_marker(
            &root,
            expected.marker,
            expected
                .name
                .strip_prefix(".kit-edit-recovery-")
                .ok_or(RecoveryError::CorruptManifest)?,
        )?,
        None if require_marker => return Err(RecoveryError::CorruptManifest),
        None => {}
    }
    Ok(root)
}

fn validate_transaction_marker(
    root: &File,
    identity: FileIdentity,
    nonce: &str,
) -> Result<(), RecoveryError> {
    let marker = open_component(root, TX_MARKER_NAME, libc::O_RDONLY)?;
    let stat = stat_file(&marker)?;
    require_identity("transaction.marker", stat, identity)?;
    if stat.uid != unsafe { libc::geteuid() } || stat.mode & 0o7777 != 0o600 {
        return Err(RecoveryError::CorruptManifest);
    }
    let expected = format!("kit-edit-transaction-v1\nnonce={nonce}\n");
    let mut bytes = Vec::new();
    marker.take(128).read_to_end(&mut bytes)?;
    if bytes != expected.as_bytes() {
        return Err(RecoveryError::CorruptManifest);
    }
    Ok(())
}

fn remove_private_transaction(
    state_root: &File,
    expected: &DirectoryIdentity,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    let ledger = read_ledger(state_root)?.ok_or(RecoveryError::CorruptManifest)?;
    remove_quarantined_directory(
        state_root,
        &expected.name,
        cleanup_quarantine(&ledger.cleanup_intents, "transaction")?,
        DirectoryAuthority::Bound {
            expected,
            require_marker: true,
        },
        deadline,
    )
}

#[derive(Clone, Copy)]
enum DirectoryAuthority<'a> {
    Bound {
        expected: &'a DirectoryIdentity,
        require_marker: bool,
    },
    PreMarker {
        nonce: &'a str,
    },
}

enum DirectoryCandidate {
    Missing,
    Present { root: File, stat: Stat, empty: bool },
    Unexpected,
}

enum DirectoryQuarantineState {
    Done,
    CreateSentinel(DirectoryCandidate),
    Exchange(DirectoryCandidate, DirectoryCandidate),
    RetireSourceSentinel(DirectoryCandidate, DirectoryCandidate),
    Cleanup(DirectoryCandidate),
    RemoveSourceSentinel(DirectoryCandidate),
    RemoveQuarantineSentinel(DirectoryCandidate),
}

fn remove_quarantined_directory(
    parent: &File,
    source_name: &str,
    quarantine: &str,
    authority: DirectoryAuthority<'_>,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    let source = CString::new(source_name).map_err(|_| RecoveryError::CorruptManifest)?;
    let quarantine_name = CString::new(quarantine).map_err(|_| RecoveryError::CorruptManifest)?;
    loop {
        check_deadline(deadline)?;
        match inspect_directory_quarantine(parent, source_name, quarantine, authority, deadline)? {
            DirectoryQuarantineState::Done => return Ok(()),
            DirectoryQuarantineState::CreateSentinel(original) => {
                verify_original_directory(&original, authority, true, deadline)?;
                mkdir(parent, &quarantine_name, 0o700)?;
                parent.sync_all()?;
                system_crash(RecoveryPoint::QuarantineMkdir, DIRECTORY_CLEANUP_ACTION);
                system_crash(
                    RecoveryPoint::QuarantineParentSync,
                    DIRECTORY_CLEANUP_ACTION,
                );
            }
            DirectoryQuarantineState::Exchange(original, sentinel) => {
                verify_original_directory(&original, authority, true, deadline)?;
                verify_directory_path(parent, &quarantine_name, &sentinel, deadline)?;
                rename_exchange(parent, &source, parent, &quarantine_name)?;
                system_crash(RecoveryPoint::QuarantineExchange, DIRECTORY_CLEANUP_ACTION);
            }
            DirectoryQuarantineState::RetireSourceSentinel(sentinel, original) => {
                verify_directory_path(parent, &source, &sentinel, deadline)?;
                verify_original_directory(&original, authority, true, deadline)?;
                rmdir(parent, &source)?;
                parent.sync_all()?;
                system_crash(
                    RecoveryPoint::QuarantineSourceSentinelRetire,
                    DIRECTORY_CLEANUP_ACTION,
                );
                system_crash(
                    RecoveryPoint::QuarantineParentSync,
                    DIRECTORY_CLEANUP_ACTION,
                );
            }
            DirectoryQuarantineState::Cleanup(original) => {
                let root = verify_original_directory(&original, authority, false, deadline)?;
                validate_controlled_tree(root, deadline)?;
                remove_controlled_tree(root, deadline)?;
                verify_directory_path(parent, &quarantine_name, &original, deadline)?;
                rmdir(parent, &quarantine_name)?;
                system_crash(
                    RecoveryPoint::QuarantineDirectoryRemove,
                    DIRECTORY_CLEANUP_ACTION,
                );
                parent.sync_all()?;
                system_crash(
                    RecoveryPoint::QuarantineParentSync,
                    DIRECTORY_CLEANUP_ACTION,
                );
            }
            DirectoryQuarantineState::RemoveSourceSentinel(sentinel) => {
                verify_directory_path(parent, &source, &sentinel, deadline)?;
                rmdir(parent, &source)?;
                parent.sync_all()?;
                system_crash(
                    RecoveryPoint::QuarantineSourceSentinelRetire,
                    DIRECTORY_CLEANUP_ACTION,
                );
                system_crash(
                    RecoveryPoint::QuarantineParentSync,
                    DIRECTORY_CLEANUP_ACTION,
                );
            }
            DirectoryQuarantineState::RemoveQuarantineSentinel(sentinel) => {
                verify_directory_path(parent, &quarantine_name, &sentinel, deadline)?;
                rmdir(parent, &quarantine_name)?;
                system_crash(
                    RecoveryPoint::QuarantineDirectoryRemove,
                    DIRECTORY_CLEANUP_ACTION,
                );
                parent.sync_all()?;
                system_crash(
                    RecoveryPoint::QuarantineParentSync,
                    DIRECTORY_CLEANUP_ACTION,
                );
            }
        }
    }
}

fn inspect_directory_quarantine(
    parent: &File,
    source_name: &str,
    quarantine_name: &str,
    authority: DirectoryAuthority<'_>,
    deadline: Instant,
) -> Result<DirectoryQuarantineState, RecoveryError> {
    let source = CString::new(source_name).map_err(|_| RecoveryError::CorruptManifest)?;
    let quarantine = CString::new(quarantine_name).map_err(|_| RecoveryError::CorruptManifest)?;
    let source = inspect_directory_candidate(parent, &source, deadline)?;
    let quarantine = inspect_directory_candidate(parent, &quarantine, deadline)?;
    if matches!(source, DirectoryCandidate::Unexpected)
        || matches!(quarantine, DirectoryCandidate::Unexpected)
    {
        return Err(RecoveryError::Conflict(source_name.to_owned()));
    }

    let state = match authority {
        DirectoryAuthority::Bound { expected, .. } => {
            let source_original = candidate_has_identity(&source, expected);
            let quarantine_original = candidate_has_identity(&quarantine, expected);
            let source_sentinel = candidate_is_sentinel(&source) && !source_original;
            let quarantine_sentinel = candidate_is_sentinel(&quarantine) && !quarantine_original;
            match (
                candidate_missing(&source),
                source_original,
                source_sentinel,
                candidate_missing(&quarantine),
                quarantine_original,
                quarantine_sentinel,
            ) {
                (true, false, false, true, false, false) => DirectoryQuarantineState::Done,
                (false, true, false, true, false, false) => {
                    DirectoryQuarantineState::CreateSentinel(source)
                }
                (false, true, false, false, false, true) => {
                    DirectoryQuarantineState::Exchange(source, quarantine)
                }
                (false, false, true, false, true, false) => {
                    DirectoryQuarantineState::RetireSourceSentinel(source, quarantine)
                }
                (true, false, false, false, true, false) => {
                    DirectoryQuarantineState::Cleanup(quarantine)
                }
                (false, false, true, true, false, false) => {
                    DirectoryQuarantineState::RemoveSourceSentinel(source)
                }
                (true, false, false, false, false, true) => {
                    DirectoryQuarantineState::RemoveQuarantineSentinel(quarantine)
                }
                _ => return Err(RecoveryError::Conflict(source_name.to_owned())),
            }
        }
        DirectoryAuthority::PreMarker { nonce } => match (&source, &quarantine) {
            (DirectoryCandidate::Missing, DirectoryCandidate::Missing) => {
                DirectoryQuarantineState::Done
            }
            (DirectoryCandidate::Present { root, .. }, DirectoryCandidate::Missing)
                if validate_pre_marker_directory(root, nonce, deadline).is_ok() =>
            {
                DirectoryQuarantineState::CreateSentinel(source)
            }
            (DirectoryCandidate::Missing, DirectoryCandidate::Present { root, .. })
                if validate_pre_marker_directory(root, nonce, deadline).is_ok() =>
            {
                DirectoryQuarantineState::Cleanup(quarantine)
            }
            (
                DirectoryCandidate::Present {
                    empty: source_empty,
                    ..
                },
                DirectoryCandidate::Present {
                    root: quarantine_root,
                    ..
                },
            ) if *source_empty
                && validate_pre_marker_directory(quarantine_root, nonce, deadline).is_ok() =>
            {
                DirectoryQuarantineState::RetireSourceSentinel(source, quarantine)
            }
            (
                DirectoryCandidate::Present { root, .. },
                DirectoryCandidate::Present { empty: true, .. },
            ) if !candidate_is_sentinel(&source)
                && validate_pre_marker_directory(root, nonce, deadline).is_ok() =>
            {
                DirectoryQuarantineState::Exchange(source, quarantine)
            }
            _ => return Err(RecoveryError::Conflict(source_name.to_owned())),
        },
    };
    Ok(state)
}

fn inspect_directory_candidate(
    parent: &File,
    name: &CStr,
    deadline: Instant,
) -> Result<DirectoryCandidate, RecoveryError> {
    let stat = match stat_at(parent, name) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DirectoryCandidate::Missing);
        }
        Err(error) => return Err(error.into()),
    };
    if stat.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32
        || stat.uid != unsafe { libc::geteuid() }
        || stat.mode & 0o7777 != 0o700
        || stat.links < 2
    {
        return Ok(DirectoryCandidate::Unexpected);
    }
    let root = match open_component(parent, name, libc::O_RDONLY | libc::O_DIRECTORY) {
        Ok(root) => root,
        Err(_) => return Ok(DirectoryCandidate::Unexpected),
    };
    if require_directory_stat(stat_file(&root)?, stat).is_err()
        || mount_identity(&root)? != mount_identity(parent)?
    {
        return Ok(DirectoryCandidate::Unexpected);
    }
    let empty = directory_empty(&root, deadline)?;
    Ok(DirectoryCandidate::Present { root, stat, empty })
}

fn candidate_missing(candidate: &DirectoryCandidate) -> bool {
    matches!(candidate, DirectoryCandidate::Missing)
}

fn candidate_is_sentinel(candidate: &DirectoryCandidate) -> bool {
    matches!(candidate, DirectoryCandidate::Present { empty: true, .. })
}

fn candidate_has_identity(candidate: &DirectoryCandidate, expected: &DirectoryIdentity) -> bool {
    matches!(candidate, DirectoryCandidate::Present { stat, .. }
        if stat.identity.device == expected.device && stat.identity.inode == expected.inode)
}

fn verify_original_directory<'a>(
    candidate: &'a DirectoryCandidate,
    authority: DirectoryAuthority<'_>,
    before_cleanup: bool,
    deadline: Instant,
) -> Result<&'a File, RecoveryError> {
    let DirectoryCandidate::Present { root, stat, .. } = candidate else {
        return Err(RecoveryError::CorruptManifest);
    };
    match authority {
        DirectoryAuthority::Bound {
            expected,
            require_marker,
        } => {
            require_directory_identity(*stat, expected)?;
            let marker_required = require_marker && before_cleanup;
            match stat_at_optional(root, TX_MARKER_NAME)? {
                Some(_) => validate_transaction_marker(
                    root,
                    expected.marker,
                    expected
                        .name
                        .strip_prefix(".kit-edit-recovery-")
                        .ok_or(RecoveryError::CorruptManifest)?,
                )
                .map_err(|_| RecoveryError::Conflict(expected.name.clone()))?,
                None if marker_required => {
                    return Err(RecoveryError::Conflict(expected.name.clone()));
                }
                None => {}
            }
        }
        DirectoryAuthority::PreMarker { nonce } => {
            validate_pre_marker_directory(root, nonce, deadline)
                .map_err(|_| RecoveryError::Conflict(nonce.to_owned()))?;
        }
    }
    Ok(root)
}

fn verify_directory_path(
    parent: &File,
    name: &CStr,
    candidate: &DirectoryCandidate,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    let DirectoryCandidate::Present { root, stat, empty } = candidate else {
        return Err(RecoveryError::CorruptManifest);
    };
    require_directory_stat(stat_file(root)?, *stat)?;
    require_directory_stat(stat_at(parent, name)?, *stat)?;
    if *empty && !directory_empty(root, deadline)? {
        return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
    }
    Ok(())
}

fn validate_controlled_tree(root: &File, deadline: Instant) -> Result<(), RecoveryError> {
    for name in directory_entries(root, deadline)? {
        let stat = stat_at(root, &name)?;
        match stat.mode & libc::S_IFMT as u32 {
            kind if kind == libc::S_IFREG as u32 => {
                let child = open_component(root, &name, libc::O_RDONLY)?;
                if stat.links != 1
                    || stat.uid != unsafe { libc::geteuid() }
                    || stat_file(&child)?.identity != stat.identity
                    || mount_identity(&child)? != mount_identity(root)?
                {
                    return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
                }
            }
            kind if kind == libc::S_IFDIR as u32 => {
                let child = open_component(root, &name, libc::O_RDONLY | libc::O_DIRECTORY)?;
                if stat.uid != unsafe { libc::geteuid() }
                    || stat.mode & 0o7777 != 0o700
                    || stat_file(&child)?.identity != stat.identity
                    || mount_identity(&child)? != mount_identity(root)?
                {
                    return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
                }
                validate_controlled_tree(&child, deadline)?;
            }
            _ => return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned())),
        }
    }
    Ok(())
}

fn remove_controlled_tree(root: &File, deadline: Instant) -> Result<(), RecoveryError> {
    for name in directory_entries(root, deadline)? {
        check_deadline(deadline)?;
        let stat = stat_at(root, &name)?;
        let flags = if stat.mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32 {
            let child = open_component(root, &name, libc::O_RDONLY | libc::O_DIRECTORY)?;
            if stat_file(&child)?.identity != stat.identity {
                return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
            }
            remove_controlled_tree(&child, deadline)?;
            libc::AT_REMOVEDIR
        } else {
            let child = open_component(root, &name, libc::O_RDONLY)?;
            if stat.mode & libc::S_IFMT as u32 != libc::S_IFREG as u32
                || stat.links != 1
                || stat_file(&child)?.identity != stat.identity
            {
                return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
            }
            0
        };
        if stat_at(root, &name)?.identity != stat.identity {
            return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
        }
        if unsafe { libc::unlinkat(root.as_raw_fd(), name.as_ptr(), flags) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        root.sync_all()?;
        system_crash(
            RecoveryPoint::QuarantineItemUnlink,
            DIRECTORY_CLEANUP_ACTION,
        );
    }
    Ok(())
}

fn require_directory_stat(actual: Stat, expected: Stat) -> Result<(), RecoveryError> {
    if actual.mode & libc::S_IFMT as u32 == libc::S_IFDIR as u32
        && actual.identity == expected.identity
        && actual.uid == expected.uid
        && actual.mode & 0o7777 == expected.mode & 0o7777
    {
        Ok(())
    } else {
        Err(RecoveryError::CorruptManifest)
    }
}

fn clear_errno() {
    #[cfg(target_os = "macos")]
    unsafe {
        *libc::__error() = 0;
    }
    #[cfg(target_os = "linux")]
    unsafe {
        *libc::__errno_location() = 0;
    }
}

fn open_image(root: &File, stored: &StoredFile) -> Result<File, RecoveryError> {
    let file = open_named_file(root, &stored.image.name)?;
    let stat = stat_file(&file)?;
    require_identity(&stored.image.name, stat, stored.image.identity)?;
    if stat.size != stored.size || stat.mode & 0o777 != stored.mode {
        return Err(RecoveryError::CorruptManifest);
    }
    Ok(file)
}

fn copy_verified(
    source: &mut File,
    destination: &mut File,
    stored: &StoredFile,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut hasher = blake3::Hasher::new();
    let mut copied = 0_u64;
    loop {
        check_deadline(deadline)?;
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(count as u64)
            .ok_or(RecoveryError::CorruptManifest)?;
        if copied > stored.size {
            return Err(RecoveryError::CorruptManifest);
        }
        destination.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
    }
    if copied != stored.size || format!("blake3:{}", hasher.finalize().to_hex()) != stored.digest {
        return Err(RecoveryError::CorruptManifest);
    }
    Ok(())
}

fn verify_path_file(
    parent: &File,
    name: &CStr,
    stored: &StoredFile,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    check_deadline(deadline)?;
    let mut file = open_component(parent, name, libc::O_RDONLY)?;
    let stat = stat_file(&file)?;
    validate_regular(name.to_string_lossy().as_ref(), stat)?;
    if stat.size != stored.size || stat.mode & 0o777 != stored.mode {
        return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
    }
    let mut hasher = blake3::Hasher::new();
    let mut remaining = stored.size;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining != 0 {
        check_deadline(deadline)?;
        let chunk = remaining.min(buffer.len() as u64) as usize;
        let count = file.read(&mut buffer[..chunk])?;
        if count == 0 {
            return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
        }
        remaining -= count as u64;
        hasher.update(&buffer[..count]);
    }
    if file.read(&mut buffer[..1])? != 0
        || format!("blake3:{}", hasher.finalize().to_hex()) != stored.digest
    {
        return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
    }
    Ok(())
}

fn verify_open_file(
    file: &mut File,
    name: &CStr,
    stored: &StoredFile,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    check_deadline(deadline)?;
    file.seek(SeekFrom::Start(0))?;
    let stat = stat_file(file)?;
    validate_regular(name.to_string_lossy().as_ref(), stat)?;
    if stat.size != stored.size || stat.mode & 0o777 != stored.mode {
        return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
    }
    let mut hasher = blake3::Hasher::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check_deadline(deadline)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .ok_or(RecoveryError::CorruptManifest)?;
        if bytes > stored.size {
            return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
        }
        hasher.update(&buffer[..count]);
    }
    if bytes != stored.size || format!("blake3:{}", hasher.finalize().to_hex()) != stored.digest {
        return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
    }
    Ok(())
}

fn quarantine_remove_file(
    parent: &File,
    name: &CStr,
    quarantine_name: &str,
    action: usize,
    mut expected: Option<FileIdentity>,
    stored: Option<&StoredFile>,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    check_deadline(deadline)?;
    let quarantine = CString::new(quarantine_name).map_err(|_| RecoveryError::CorruptManifest)?;
    let sentinel_bytes = format!("kit-edit-cleanup-sentinel-v1\nintent={quarantine_name}\n");
    let source_before = classify_cleanup_file(parent, name, expected, sentinel_bytes.as_bytes())?;
    if expected.is_none()
        && let CleanupFile::Object(stat) = source_before
    {
        expected = Some(stat.identity);
    }
    if matches!(source_before, CleanupFile::Missing)
        && stat_at_optional(parent, &quarantine)?.is_none()
    {
        return Ok(());
    }
    if stat_at_optional(parent, &quarantine)?.is_none() {
        mkdir(parent, &quarantine, 0o700)?;
        system_crash(RecoveryPoint::QuarantineMkdir, action);
        parent.sync_all()?;
        system_crash(RecoveryPoint::QuarantineParentSync, action);
    }
    let root = open_component(parent, &quarantine, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let root_stat = stat_file(&root)?;
    if root_stat.uid != unsafe { libc::geteuid() }
        || root_stat.mode & 0o7777 != 0o700
        || mount_identity(&root)? != mount_identity(parent)?
    {
        return Err(RecoveryError::CorruptManifest);
    }
    let item = c"item";
    loop {
        check_deadline(deadline)?;
        let source = classify_cleanup_file(parent, name, expected, sentinel_bytes.as_bytes())?;
        let quarantined = classify_cleanup_file(&root, item, expected, sentinel_bytes.as_bytes())?;
        match (source, quarantined) {
            (CleanupFile::Object(source_stat), CleanupFile::Missing) => {
                let mut sentinel = create_file(&root, item, 0o600)?;
                system_crash(RecoveryPoint::QuarantineSentinelCreate, action);
                sentinel.write_all(sentinel_bytes.as_bytes())?;
                sentinel.sync_all()?;
                root.sync_all()?;
                system_crash(RecoveryPoint::QuarantineSentinelSync, action);
                require_identity(
                    name.to_string_lossy().as_ref(),
                    source_stat,
                    source_stat.identity,
                )?;
            }
            (CleanupFile::Object(source_stat), CleanupFile::Sentinel) => {
                let mut source = open_component(parent, name, libc::O_RDONLY)?;
                require_identity(
                    name.to_string_lossy().as_ref(),
                    source_stat,
                    source_stat.identity,
                )?;
                if let Some(stored) = stored {
                    verify_open_file(&mut source, name, stored, deadline)?;
                }
                #[cfg(test)]
                pause_test_race(TestRaceWindow::RemoveSource);
                rename_exchange(parent, name, &root, item)?;
                system_crash(RecoveryPoint::QuarantineExchange, action);
                if stat_file(&source)?.identity != source_stat.identity
                    || stat_at(&root, item)?.identity != source_stat.identity
                    || !matches!(
                        classify_cleanup_file(parent, name, expected, sentinel_bytes.as_bytes())?,
                        CleanupFile::Sentinel
                    )
                {
                    return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
                }
                system_crash(RecoveryPoint::QuarantinePostVerify, action);
                root.sync_all()?;
                parent.sync_all()?;
                system_crash(RecoveryPoint::QuarantineParentSync, action);
            }
            (CleanupFile::Sentinel, CleanupFile::Object(item_stat)) => {
                if let Some(stored) = stored {
                    let mut item_file = open_component(&root, item, libc::O_RDONLY)?;
                    verify_open_file(&mut item_file, item, stored, deadline)?;
                }
                if expected.is_some_and(|identity| identity != item_stat.identity) {
                    return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
                }
                unlink(parent, name)?;
                system_crash(RecoveryPoint::QuarantineSourceSentinelRetire, action);
                parent.sync_all()?;
                system_crash(RecoveryPoint::QuarantineParentSync, action);
            }
            (CleanupFile::Missing, CleanupFile::Object(item_stat)) => {
                if expected.is_some_and(|identity| identity != item_stat.identity) {
                    return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
                }
                if let Some(stored) = stored {
                    let mut item_file = open_component(&root, item, libc::O_RDONLY)?;
                    verify_open_file(&mut item_file, item, stored, deadline)?;
                }
                unlink(&root, item)?;
                system_crash(RecoveryPoint::QuarantineItemUnlink, action);
                root.sync_all()?;
                system_crash(RecoveryPoint::QuarantineParentSync, action);
            }
            (CleanupFile::Missing, CleanupFile::Sentinel) => {
                unlink(&root, item)?;
                system_crash(RecoveryPoint::QuarantineItemUnlink, action);
                root.sync_all()?;
                system_crash(RecoveryPoint::QuarantineParentSync, action);
            }
            (CleanupFile::Sentinel, CleanupFile::Missing) => {
                unlink(parent, name)?;
                system_crash(RecoveryPoint::QuarantineSourceSentinelRetire, action);
                parent.sync_all()?;
                system_crash(RecoveryPoint::QuarantineParentSync, action);
            }
            (CleanupFile::Sentinel, CleanupFile::Sentinel) => {
                unlink(parent, name)?;
                system_crash(RecoveryPoint::QuarantineSourceSentinelRetire, action);
                unlink(&root, item)?;
                system_crash(RecoveryPoint::QuarantineItemUnlink, action);
                root.sync_all()?;
                parent.sync_all()?;
                system_crash(RecoveryPoint::QuarantineParentSync, action);
            }
            (CleanupFile::Object(_), CleanupFile::Other)
            | (CleanupFile::Missing, CleanupFile::Other) => {
                unlink(&root, item)?;
                root.sync_all()?;
                system_crash(RecoveryPoint::QuarantineParentSync, action);
            }
            (CleanupFile::Object(_), CleanupFile::Object(_))
            | (CleanupFile::Other, _)
            | (CleanupFile::Sentinel, CleanupFile::Other) => {
                return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
            }
            (CleanupFile::Missing, CleanupFile::Missing) => break,
        }
    }
    require_directory_stat(stat_at(parent, &quarantine)?, root_stat)?;
    rmdir(parent, &quarantine)?;
    system_crash(RecoveryPoint::QuarantineDirectoryRemove, action);
    parent.sync_all()?;
    system_crash(RecoveryPoint::QuarantineParentSync, action);
    Ok(())
}

#[derive(Clone, Copy)]
enum CleanupFile {
    Missing,
    Object(Stat),
    Sentinel,
    Other,
}

fn classify_cleanup_file(
    parent: &File,
    name: &CStr,
    expected: Option<FileIdentity>,
    sentinel: &[u8],
) -> Result<CleanupFile, RecoveryError> {
    let mut file = match open_component(parent, name, libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(CleanupFile::Missing),
        Err(error) => return Err(error.into()),
    };
    let stat = stat_file(&file)?;
    if validate_regular(name.to_string_lossy().as_ref(), stat).is_err() {
        return Ok(CleanupFile::Other);
    }
    if stat.size == sentinel.len() as u64 {
        let mut bytes = Vec::with_capacity(sentinel.len());
        file.read_to_end(&mut bytes)?;
        if bytes == sentinel {
            return Ok(CleanupFile::Sentinel);
        }
    }
    if expected.is_none_or(|identity| identity == stat.identity) {
        Ok(CleanupFile::Object(stat))
    } else {
        Ok(CleanupFile::Other)
    }
}

fn cleanup_action(key: &str) -> usize {
    key.rsplit_once(':')
        .and_then(|(_, index)| index.parse().ok())
        .unwrap_or(0)
}

fn remove_if_identity(
    parent: &File,
    name: &CStr,
    quarantine: &str,
    action: usize,
    identity: FileIdentity,
    stored: &StoredFile,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    quarantine_remove_file(
        parent,
        name,
        quarantine,
        action,
        Some(identity),
        Some(stored),
        deadline,
    )
}

fn remove_unbound_temp(
    parent: &File,
    action: &Action,
    quarantine: &str,
    action_index: usize,
    stored: &StoredFile,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    let name =
        CString::new(action.new_temp.as_str()).map_err(|_| RecoveryError::CorruptManifest)?;
    quarantine_remove_file(
        parent,
        &name,
        quarantine,
        action_index,
        None,
        Some(stored),
        deadline,
    )
}

fn open_parent(root: &File, path: &str) -> Result<(File, CString), RecoveryError> {
    RootRelativePath::parse(path, usize::MAX).map_err(|_| RecoveryError::CorruptManifest)?;
    let root_mount = mount_identity(root)?;
    let mut components = path.split('/').peekable();
    let mut directory = root.try_clone()?;
    while let Some(component) = components.next() {
        let name = CString::new(component).map_err(|_| RecoveryError::CorruptManifest)?;
        if components.peek().is_none() {
            return Ok((directory, name));
        }
        directory = open_component(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY)?;
        let stat = stat_file(&directory)?;
        if stat.mode & libc::S_IFMT as u32 != libc::S_IFDIR as u32
            || mount_identity(&directory)? != root_mount
        {
            return Err(RecoveryError::UnsafeEntry(path.to_owned()));
        }
    }
    Err(RecoveryError::CorruptManifest)
}

fn open_relative(root: &File, path: &str, flags: libc::c_int) -> Result<File, RecoveryError> {
    let (parent, leaf) = open_parent(root, path)?;
    Ok(open_component(&parent, &leaf, flags)?)
}

fn open_named_directory(parent: &File, name: &str) -> Result<File, RecoveryError> {
    let name = CString::new(name).map_err(|_| RecoveryError::CorruptManifest)?;
    Ok(open_component(
        parent,
        &name,
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?)
}

fn open_named_file(parent: &File, name: &str) -> Result<File, RecoveryError> {
    let name = CString::new(name).map_err(|_| RecoveryError::CorruptManifest)?;
    Ok(open_component(parent, &name, libc::O_RDONLY)?)
}

fn open_component(parent: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

fn create_file(parent: &File, name: &CStr, mode: u32) -> io::Result<File> {
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

fn mkdir(parent: &File, name: &CStr, mode: u32) -> io::Result<()> {
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn unlink(parent: &File, name: &CStr) -> io::Result<()> {
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn rmdir(parent: &File, name: &CStr) -> io::Result<()> {
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn stat_file(file: &File) -> io::Result<Stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(stat_from(unsafe { stat.assume_init() }))
}

fn stat_at(parent: &File, name: &CStr) -> io::Result<Stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(stat_from(unsafe { stat.assume_init() }))
}

fn stat_at_optional(parent: &File, name: &CStr) -> Result<Option<Stat>, RecoveryError> {
    match stat_at(parent, name) {
        Ok(stat) => Ok(Some(stat)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn stat_from(stat: libc::stat) -> Stat {
    Stat {
        identity: FileIdentity {
            device: stat.st_dev as u64,
            inode: stat.st_ino,
        },
        mode: stat.st_mode as u32,
        links: stat.st_nlink as u64,
        size: stat.st_size.max(0) as u64,
        uid: stat.st_uid,
    }
}

fn validate_regular(path: &str, stat: Stat) -> Result<(), RecoveryError> {
    if stat.mode & libc::S_IFMT as u32 != libc::S_IFREG as u32 || stat.links != 1 {
        Err(RecoveryError::UnsafeEntry(path.to_owned()))
    } else {
        Ok(())
    }
}

fn require_identity(path: &str, stat: Stat, expected: FileIdentity) -> Result<(), RecoveryError> {
    validate_regular(path, stat)?;
    if stat.identity == expected {
        Ok(())
    } else {
        Err(RecoveryError::Conflict(path.to_owned()))
    }
}

fn ensure_absent(parent: &File, name: &str) -> Result<(), RecoveryError> {
    let name = CString::new(name).map_err(|_| RecoveryError::CorruptManifest)?;
    match stat_at(parent, &name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(RecoveryError::Conflict(name.to_string_lossy().into_owned())),
        Err(error) => Err(error.into()),
    }
}

fn set_mode(file: &File, mode: u32) -> io::Result<()> {
    if mode & !0o777 != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid mode"));
    }
    if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn mount_identity(file: &File) -> io::Result<MountIdentity> {
    const STATX_MNT_ID: u32 = 0x1000;
    let mut statx = std::mem::MaybeUninit::<libc::statx>::zeroed();
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            STATX_MNT_ID,
            statx.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let statx = unsafe { statx.assume_init() };
    if statx.stx_mask & STATX_MNT_ID == 0 {
        return Err(io::Error::other("mount identity unavailable"));
    }
    Ok(MountIdentity(
        *blake3::hash(&statx.stx_mnt_id.to_le_bytes()).as_bytes(),
    ))
}

#[cfg(target_os = "macos")]
fn mount_identity(file: &File) -> io::Result<MountIdentity> {
    let mut metadata = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(file.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let metadata = unsafe { metadata.assume_init() };
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&metadata.f_fsid as *const libc::fsid_t).cast::<u8>(),
            std::mem::size_of::<libc::fsid_t>(),
        )
    };
    Ok(MountIdentity(*blake3::hash(bytes).as_bytes()))
}

fn rename_noreplace(
    from_parent: &File,
    from: &CStr,
    to_parent: &File,
    to: &CStr,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from_parent.as_raw_fd(),
            from.as_ptr(),
            to_parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        ) as libc::c_int
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            from_parent.as_raw_fd(),
            from.as_ptr(),
            to_parent.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn rename_noreplace_identity(
    from_parent: &File,
    from: &CStr,
    to_parent: &File,
    to: &CStr,
    expected: FileIdentity,
    stored: Option<&StoredFile>,
    deadline: Instant,
) -> Result<(), RecoveryError> {
    let mut source = open_component(from_parent, from, libc::O_RDONLY)?;
    if stat_file(&source)?.identity != expected {
        return Err(RecoveryError::Conflict(from.to_string_lossy().into_owned()));
    }
    #[cfg(test)]
    pause_test_race(TestRaceWindow::MoveSource);
    system_crash(RecoveryPoint::BeforeWorkspaceExchange, 0);
    let sentinel = match create_file(to_parent, to, 0o600) {
        Ok(sentinel) => sentinel,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(RecoveryError::Conflict(to.to_string_lossy().into_owned()));
        }
        Err(error) => return Err(error.into()),
    };
    system_crash(RecoveryPoint::QuarantineSentinelCreate, 0);
    sentinel.sync_all()?;
    to_parent.sync_all()?;
    system_crash(RecoveryPoint::QuarantineSentinelSync, 0);
    let sentinel_identity = stat_file(&sentinel)?.identity;
    if let Err(error) = rename_exchange(from_parent, from, to_parent, to) {
        let _ = remove_open_identity(to_parent, to, sentinel_identity);
        return Err(error.into());
    }
    system_crash(RecoveryPoint::QuarantineExchange, 0);
    let source_fd_identity = stat_file(&source)?.identity;
    let sentinel_fd_identity = stat_file(&sentinel)?.identity;
    let moved = stat_at_optional(to_parent, to)?;
    let exchanged_sentinel = stat_at_optional(from_parent, from)?;
    if source_fd_identity != expected
        || sentinel_fd_identity != sentinel_identity
        || moved.is_none_or(|state| state.identity != expected)
        || exchanged_sentinel.is_none_or(|state| state.identity != sentinel_identity)
    {
        rollback_failed_exchange(from_parent, from, to_parent, to, sentinel_identity)?;
        return Err(RecoveryError::Conflict(from.to_string_lossy().into_owned()));
    }
    system_crash(RecoveryPoint::QuarantinePostVerify, 0);
    if let Some(stored) = stored
        && let Err(error) = verify_open_file(&mut source, from, stored, deadline)
    {
        rollback_failed_exchange(from_parent, from, to_parent, to, sentinel_identity)?;
        return Err(error);
    }
    remove_open_identity(from_parent, from, sentinel_identity)?;
    system_crash(RecoveryPoint::QuarantineSourceSentinelRetire, 0);
    from_parent.sync_all()?;
    system_crash(RecoveryPoint::QuarantineParentSync, 0);
    if from_parent.as_raw_fd() != to_parent.as_raw_fd() {
        to_parent.sync_all()?;
        system_crash(RecoveryPoint::QuarantineParentSync, 0);
    }
    system_crash(RecoveryPoint::AfterWorkspaceExchange, 0);
    Ok(())
}

fn rollback_failed_exchange(
    from_parent: &File,
    from: &CStr,
    to_parent: &File,
    to: &CStr,
    sentinel_identity: FileIdentity,
) -> Result<(), RecoveryError> {
    rename_exchange(from_parent, from, to_parent, to)?;
    if stat_at(to_parent, to)?.identity != sentinel_identity {
        return Err(RecoveryError::Conflict(to.to_string_lossy().into_owned()));
    }
    remove_open_identity(to_parent, to, sentinel_identity)?;
    from_parent.sync_all()?;
    if from_parent.as_raw_fd() != to_parent.as_raw_fd() {
        to_parent.sync_all()?;
    }
    Ok(())
}

fn remove_open_identity(
    parent: &File,
    name: &CStr,
    expected: FileIdentity,
) -> Result<(), RecoveryError> {
    let opened = open_component(parent, name, libc::O_RDONLY)?;
    if stat_file(&opened)?.identity != expected || stat_at(parent, name)?.identity != expected {
        return Err(RecoveryError::Conflict(name.to_string_lossy().into_owned()));
    }
    unlink(parent, name)?;
    Ok(())
}

fn normalize_workspace_exchange(
    parent: &File,
    source: &CStr,
    expected: FileIdentity,
    destination: &CStr,
) -> Result<(), RecoveryError> {
    let source_state = stat_at_optional(parent, source)?;
    let destination_state = stat_at_optional(parent, destination)?;
    match (source_state, destination_state) {
        (Some(source_state), Some(destination_state))
            if source_state.identity == expected && is_workspace_sentinel(destination_state) =>
        {
            remove_open_identity(parent, destination, destination_state.identity)?;
            parent.sync_all()?;
            system_crash(RecoveryPoint::QuarantineParentSync, 0);
        }
        (Some(source_state), Some(destination_state))
            if is_workspace_sentinel(source_state) && destination_state.identity == expected =>
        {
            remove_open_identity(parent, source, source_state.identity)?;
            parent.sync_all()?;
            system_crash(RecoveryPoint::QuarantineParentSync, 0);
        }
        _ => {}
    }
    Ok(())
}

fn is_workspace_sentinel(stat: Stat) -> bool {
    stat.mode & libc::S_IFMT as u32 == libc::S_IFREG as u32
        && stat.mode & 0o7777 == 0o600
        && stat.links == 1
        && stat.size == 0
        && stat.uid == unsafe { libc::geteuid() }
}

fn rename_exchange(
    left_parent: &File,
    left: &CStr,
    right_parent: &File,
    right: &CStr,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            left_parent.as_raw_fd(),
            left.as_ptr(),
            right_parent.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        ) as libc::c_int
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            left_parent.as_raw_fd(),
            left.as_ptr(),
            right_parent.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_SWAP,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn publish_replace(parent: &File, source: &CStr, destination: &CStr) -> Result<(), RecoveryError> {
    let source_file = open_component(parent, source, libc::O_RDONLY)?;
    let source_identity = stat_file(&source_file)?.identity;
    let destination_file = match open_component(parent, destination, libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return rename_noreplace(parent, source, parent, destination).map_err(Into::into);
        }
        Err(error) => return Err(error.into()),
    };
    let destination_identity = stat_file(&destination_file)?.identity;
    #[cfg(test)]
    pause_test_race(TestRaceWindow::ReplaceDestination);
    rename_exchange(parent, source, parent, destination)?;
    if stat_file(&source_file)?.identity != source_identity
        || stat_at(parent, destination)?.identity != source_identity
        || stat_file(&destination_file)?.identity != destination_identity
        || stat_at(parent, source)?.identity != destination_identity
    {
        return Err(RecoveryError::Conflict(
            destination.to_string_lossy().into_owned(),
        ));
    }
    remove_open_identity(parent, source, destination_identity)?;
    parent.sync_all()?;
    Ok(())
}

fn check_deadline(deadline: Instant) -> Result<(), RecoveryError> {
    if Instant::now() >= deadline {
        Err(RevisionError::LimitExceeded(crate::workspace::revision::LimitKind::Time).into())
    } else {
        Ok(())
    }
}

fn inject(
    hook: RecoveryHook<'_>,
    point: RecoveryPoint,
    action: usize,
) -> Result<(), RecoveryError> {
    if hook(point, action) {
        Err(RecoveryError::InjectedCrash { point, action })
    } else {
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn into_io(error: RecoveryError) -> io::Error {
    match error {
        RecoveryError::Io(error) => error,
        other => io::Error::other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    static TEST_RACE_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn race_root(label: &str) -> std::path::PathBuf {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let path = std::env::temp_dir().join(format!("kit-{label}-{}", hex(&nonce)));
        std::fs::create_dir(&path).unwrap();
        path
    }

    fn arm_race(
        window: TestRaceWindow,
    ) -> (
        std::sync::Arc<std::sync::Barrier>,
        std::sync::Arc<std::sync::Barrier>,
    ) {
        let entered = std::sync::Arc::new(std::sync::Barrier::new(2));
        let release = std::sync::Arc::new(std::sync::Barrier::new(2));
        *TEST_RACE_HOOK.lock().unwrap() = Some(TestRaceHook {
            window,
            entered: entered.clone(),
            release: release.clone(),
        });
        (entered, release)
    }

    fn stored_file(root: &File, name: &str, bytes: &[u8]) -> StoredFile {
        let file = open_named_file(root, name).unwrap();
        let identity = stat_file(&file).unwrap().identity;
        StoredFile {
            digest: format!("blake3:{}", blake3::hash(bytes).to_hex()),
            mode: 0o644,
            size: bytes.len() as u64,
            identity: Some(identity),
            image: ImageRef {
                name: name.to_owned(),
                identity,
            },
        }
    }

    #[test]
    fn source_swap_is_rolled_back_without_publishing_or_deleting_the_racer() {
        let _serial = TEST_RACE_SERIAL.lock().unwrap();
        let path = race_root("move-source-race");
        std::fs::write(path.join("source"), b"expected").unwrap();
        std::fs::write(path.join("racer"), b"racer").unwrap();
        let root = File::open(&path).unwrap();
        let stored = stored_file(&root, "source", b"expected");
        let expected = stored.identity.unwrap();
        let (entered, release) = arm_race(TestRaceWindow::MoveSource);
        let racer_path = path.clone();
        let racer = std::thread::spawn(move || {
            entered.wait();
            std::fs::rename(racer_path.join("source"), racer_path.join("saved")).unwrap();
            std::fs::rename(racer_path.join("racer"), racer_path.join("source")).unwrap();
            release.wait();
        });
        let result = rename_noreplace_identity(
            &root,
            c"source",
            &root,
            c"destination",
            expected,
            Some(&stored),
            Instant::now() + Duration::from_secs(5),
        );
        racer.join().unwrap();
        *TEST_RACE_HOOK.lock().unwrap() = None;
        assert!(matches!(result, Err(RecoveryError::Conflict(_))));
        assert_eq!(std::fs::read(path.join("source")).unwrap(), b"racer");
        assert_eq!(std::fs::read(path.join("saved")).unwrap(), b"expected");
        assert!(!path.join("destination").exists());
        drop(root);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn cleanup_swap_quarantines_and_never_unlinks_the_racer() {
        let _serial = TEST_RACE_SERIAL.lock().unwrap();
        let path = race_root("remove-source-race");
        std::fs::write(path.join("source"), b"expected").unwrap();
        std::fs::write(path.join("racer"), b"racer").unwrap();
        let root = File::open(&path).unwrap();
        let stored = stored_file(&root, "source", b"expected");
        let expected = stored.identity.unwrap();
        let (entered, release) = arm_race(TestRaceWindow::RemoveSource);
        let racer_path = path.clone();
        let racer = std::thread::spawn(move || {
            entered.wait();
            std::fs::rename(racer_path.join("source"), racer_path.join("saved")).unwrap();
            std::fs::rename(racer_path.join("racer"), racer_path.join("source")).unwrap();
            release.wait();
        });
        let result = quarantine_remove_file(
            &root,
            c"source",
            ".kit-edit-quarantine-test",
            0,
            Some(expected),
            Some(&stored),
            Instant::now() + Duration::from_secs(5),
        );
        racer.join().unwrap();
        *TEST_RACE_HOOK.lock().unwrap() = None;
        assert!(matches!(result, Err(RecoveryError::Conflict(_))));
        assert_eq!(std::fs::read(path.join("saved")).unwrap(), b"expected");
        let quarantine = std::fs::read_dir(&path)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".kit-edit-quarantine-")
            })
            .unwrap()
            .path();
        assert_eq!(std::fs::read(quarantine.join("item")).unwrap(), b"racer");
        drop(root);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn replacement_swap_reports_conflict_and_preserves_the_racer() {
        let _serial = TEST_RACE_SERIAL.lock().unwrap();
        let path = race_root("replace-destination-race");
        std::fs::write(path.join("source"), b"new").unwrap();
        std::fs::write(path.join("destination"), b"old").unwrap();
        std::fs::write(path.join("racer"), b"racer").unwrap();
        let root = File::open(&path).unwrap();
        let (entered, release) = arm_race(TestRaceWindow::ReplaceDestination);
        let racer_path = path.clone();
        let racer = std::thread::spawn(move || {
            entered.wait();
            std::fs::rename(racer_path.join("destination"), racer_path.join("saved")).unwrap();
            std::fs::rename(racer_path.join("racer"), racer_path.join("destination")).unwrap();
            release.wait();
        });
        let result = publish_replace(&root, c"source", c"destination");
        racer.join().unwrap();
        *TEST_RACE_HOOK.lock().unwrap() = None;
        assert!(matches!(result, Err(RecoveryError::Conflict(_))));
        assert_eq!(std::fs::read(path.join("source")).unwrap(), b"racer");
        assert_eq!(std::fs::read(path.join("destination")).unwrap(), b"new");
        assert_eq!(std::fs::read(path.join("saved")).unwrap(), b"old");
        drop(root);
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn changed_move_golden_never_claims_full_similarity() {
        let mut nonce = [0_u8; 8];
        getrandom::fill(&mut nonce).unwrap();
        let path = std::env::temp_dir().join(format!("kit-move-diff-{}", hex(&nonce)));
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("before"), b"old\n").unwrap();
        std::fs::write(path.join("after"), b"new\n").unwrap();
        std::fs::set_permissions(path.join("before"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        std::fs::set_permissions(path.join("after"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let root = File::open(&path).unwrap();
        let stored = |name: &str, bytes: &[u8], mode| {
            let file = open_named_file(&root, name).unwrap();
            StoredFile {
                digest: format!("blake3:{}", blake3::hash(bytes).to_hex()),
                mode,
                size: bytes.len() as u64,
                identity: None,
                image: ImageRef {
                    name: name.to_owned(),
                    identity: stat_file(&file).unwrap().identity,
                },
            }
        };
        let before = stored("before", b"old\n", 0o644);
        let after = stored("after", b"new\n", 0o755);
        let mut output = Vec::new();
        append_file_diff(
            &mut output,
            "old.txt",
            "new.txt",
            Some(&before),
            Some(&after),
            &root,
            true,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "diff --git a/old.txt b/new.txt\nrename from old.txt\nrename to new.txt\nold mode 000644\nnew mode 000755\n--- a/old.txt\n+++ b/new.txt\n@@ -1 +1 @@\n-old\n+new\n"
        );
        drop(root);
        std::fs::remove_dir_all(path).unwrap();
    }
}
