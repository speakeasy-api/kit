use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use kit::store::artifacts::{
    self, ArtifactClass, ArtifactError, ArtifactMetadata, ArtifactRetention, ArtifactStore,
    CrashPoint, Reachability, ReferenceError,
};

struct StateRoot(PathBuf);

impl StateRoot {
    fn new(point: CrashPoint) -> Self {
        let root = std::env::temp_dir().join(format!(
            "kit-artifact-crash-{point:?}-{}-{}",
            std::process::id(),
            artifacts::now_unix_micros().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
}

impl Drop for StateRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn metadata() -> ArtifactMetadata {
    ArtifactMetadata::new(
        "application/octet-stream",
        ArtifactClass::Report,
        "principal_test",
        "project_test",
        ArtifactRetention::UntilUnixMicros(0),
        0,
    )
    .unwrap()
}

#[test]
fn every_artifact_state_has_zero_committed_references_to_missing_bytes() {
    for point in CrashPoint::ALL {
        let root = StateRoot::new(point);
        let store = ArtifactStore::open(&root.0).unwrap();
        let bytes = format!("artifact-at-{point:?}").into_bytes();
        let mut committed = BTreeSet::new();

        let artifact = store.put_with_hook(&bytes, metadata(), |visited| visited == point);
        if matches!(
            point,
            CrashPoint::BeforeEventReference | CrashPoint::AfterEventReference
        ) {
            let artifact = artifact.unwrap();
            let result: Result<(), ReferenceError<()>> = store.commit_reference_with_hook(
                &artifact,
                |id| {
                    committed.insert(id);
                    Ok(())
                },
                |visited| visited == point,
            );
            assert!(matches!(
                result,
                Err(ReferenceError::Artifact(ArtifactError::InjectedCrash(actual))) if actual == point
            ));
        } else {
            assert!(matches!(
                artifact,
                Err(ArtifactError::InjectedCrash(actual)) if actual == point
            ));
        }

        let recovered = ArtifactStore::open(&root.0).unwrap();
        for id in &committed {
            assert_eq!(recovered.open_bytes(*id).unwrap(), bytes);
        }
        if committed.is_empty() {
            let report = recovered
                .collect_garbage(&Reachability {
                    now_unix_micros: i64::MAX,
                    orphan_grace_micros: 0,
                    ..Reachability::default()
                })
                .unwrap();
            assert!(report.deleted_staged_files + report.deleted_artifacts.len() > 0);
        }
    }
}

#[test]
fn corruption_after_upload_is_refused_before_reference() {
    let root = StateRoot::new(CrashPoint::BeforeEventReference);
    let store = ArtifactStore::open(&root.0).unwrap();
    let artifact = store.put(b"trusted bytes", metadata()).unwrap();
    let hex = artifact.digest().to_string();
    let hex = hex.strip_prefix("blake3:").unwrap();
    let path = root
        .0
        .join("objects")
        .join(&hex[..2])
        .join(format!("{}.blob", &hex[2..]));
    std::fs::write(path, b"tampered bytes").unwrap();
    let mut committed = false;
    let result: Result<(), ReferenceError<()>> = store.commit_reference(&artifact, |_| {
        committed = true;
        Ok(())
    });
    assert!(matches!(
        result,
        Err(ReferenceError::Artifact(ArtifactError::DigestMismatch(_)))
    ));
    assert!(!committed);
}

#[test]
fn every_lease_publication_crash_recovers_by_exact_ledger_id_and_gcs() {
    const ID: &str = "0123456789abcdef0123456789abcdef";
    const OWNER: &str = "edit:lease-crash";
    for point in CrashPoint::LEASE_PUBLICATION {
        let root = StateRoot::new(point);
        let store = ArtifactStore::open(&root.0).unwrap();
        let artifact = store.put(b"leased crash artifact", metadata()).unwrap();
        let result = store.acquire_lease_with_id_before_with_hook(
            artifact.digest(),
            ID,
            OWNER,
            Instant::now() + Duration::from_secs(5),
            |visited| visited == point,
        );
        assert!(matches!(
            result,
            Err(ArtifactError::InjectedCrash(actual)) if actual == point
        ));

        let recovered = ArtifactStore::open(&root.0).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let lease = recovered
            .acquire_lease_with_id_before(artifact.digest(), ID, OWNER, deadline)
            .unwrap();
        recovered.open_lease(artifact.digest(), ID, OWNER).unwrap();
        recovered.release_lease_before(&lease, deadline).unwrap();
        assert!(
            recovered
                .collect_garbage(&Reachability {
                    now_unix_micros: i64::MAX,
                    orphan_grace_micros: 0,
                    ..Reachability::default()
                })
                .unwrap()
                .deleted_artifacts
                .contains(&artifact.digest())
        );
    }
}

#[test]
fn authorized_ledger_id_repairs_a_legacy_partial_final_lease() {
    const ID: &str = "fedcba9876543210fedcba9876543210";
    const OWNER: &str = "edit:legacy-partial";
    let root = StateRoot::new(CrashPoint::AfterLeasePartialWrite);
    let store = ArtifactStore::open(&root.0).unwrap();
    let artifact = store.put(b"legacy partial", metadata()).unwrap();
    let hex = artifact.digest().to_string();
    let hex = hex.strip_prefix("blake3:").unwrap();
    let shard = root.0.join("leases").join(&hex[..2]);
    std::fs::create_dir_all(&shard).unwrap();
    let final_path = shard.join(format!("{}.{}.lease", &hex[2..], ID));
    std::fs::write(&final_path, b"kit-artifact-lease-v1\ndigest=").unwrap();
    std::fs::File::open(&final_path)
        .unwrap()
        .sync_all()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    store
        .acquire_lease_with_id_before(artifact.digest(), ID, OWNER, deadline)
        .unwrap();
    store.open_lease(artifact.digest(), ID, OWNER).unwrap();
    store
        .release_lease_with_id_before(artifact.digest(), ID, OWNER, deadline)
        .unwrap();
}
