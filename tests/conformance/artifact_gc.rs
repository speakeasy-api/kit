use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use kit::domain::ids::{PrincipalId, ProjectId};
use kit::store::artifacts::{
    self, ArtifactClass, ArtifactMetadata, ArtifactRetention, ArtifactStore, Reachability,
};

struct StateRoot(PathBuf);

impl StateRoot {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "kit-artifact-gc-{name}-{}-{}",
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

fn metadata(retention: ArtifactRetention) -> ArtifactMetadata {
    ArtifactMetadata::new(
        "text/plain",
        ArtifactClass::Log,
        "principal_test",
        "project_test",
        retention,
        0,
    )
    .unwrap()
}

#[test]
fn gc_deletes_exactly_the_unreachable_fixture_set() {
    let root = StateRoot::new("exact");
    let store = ArtifactStore::open(&root.0).unwrap();
    let retained = store
        .put(
            b"event-retained",
            metadata(ArtifactRetention::UntilUnixMicros(0)),
        )
        .unwrap();
    let held = store
        .put(
            b"legal-hold",
            metadata(ArtifactRetention::UntilUnixMicros(0)),
        )
        .unwrap();
    let shared = store
        .put(
            b"shared-reference",
            metadata(ArtifactRetention::UntilUnixMicros(0)),
        )
        .unwrap();
    let backed_up = store
        .put(
            b"backup-inventory",
            metadata(ArtifactRetention::UntilUnixMicros(0)),
        )
        .unwrap();
    let policy_retained = store
        .put(b"policy-retained", metadata(ArtifactRetention::Forever))
        .unwrap();
    let unreachable_a = store
        .put(
            b"unreachable-a",
            metadata(ArtifactRetention::UntilUnixMicros(0)),
        )
        .unwrap();
    let unreachable_b = store
        .put(
            b"unreachable-b",
            metadata(ArtifactRetention::UntilUnixMicros(0)),
        )
        .unwrap();

    let reachability = Reachability {
        now_unix_micros: 100,
        orphan_grace_micros: 0,
        retained: BTreeSet::from([retained.digest()]),
        legal_holds: BTreeSet::from([held.digest()]),
        shared_references: BTreeSet::from([shared.digest()]),
        backup_inventory: BTreeSet::from([backed_up.digest()]),
    };
    let report = store.collect_garbage(&reachability).unwrap();
    assert_eq!(
        report.deleted_artifacts,
        BTreeSet::from([unreachable_a.digest(), unreachable_b.digest()])
    );
    assert_eq!(report.deleted_artifacts.len(), 2);
    for artifact in [retained, held, shared, backed_up, policy_retained] {
        assert!(!store.open_bytes(artifact.digest()).unwrap().is_empty());
    }
}

#[cfg(unix)]
#[test]
fn gc_never_follows_attacker_links_or_accepts_traversal_ids() {
    use std::os::unix::fs::symlink;

    let root = StateRoot::new("links");
    let store = ArtifactStore::open(&root.0).unwrap();
    let outside = root.0.with_extension("outside");
    std::fs::write(&outside, b"must survive").unwrap();
    symlink(&outside, root.0.join("staging").join("attacker.tmp")).unwrap();
    symlink(&outside, root.0.join("objects").join("attacker-link")).unwrap();
    let protected = store
        .put(
            b"protected object",
            metadata(ArtifactRetention::UntilUnixMicros(0)),
        )
        .unwrap();
    let hex = protected.digest().to_string();
    let hex = hex.strip_prefix("blake3:").unwrap();
    let manifest_path = root
        .0
        .join("manifests")
        .join(&hex[..2])
        .join(format!("{}.manifest", &hex[2..]));
    std::fs::remove_file(&manifest_path).unwrap();
    symlink(&outside, manifest_path).unwrap();

    let report = store
        .collect_garbage(&Reachability {
            now_unix_micros: i64::MAX,
            orphan_grace_micros: 0,
            ..Reachability::default()
        })
        .unwrap();
    assert_eq!(std::fs::read(&outside).unwrap(), b"must survive");
    assert_eq!(report.deleted_staged_files, 0);
    assert_eq!(report.deleted_artifacts.len(), 0);
    assert_eq!(report.skipped_unsafe_entries, 4);
    assert!(artifacts::ArtifactDigest::parse("blake3:../../outside").is_err());
    std::fs::remove_file(outside).unwrap();
}

#[test]
fn open_rehashes_bytes_and_manifest_is_canonical() {
    let root = StateRoot::new("open");
    let store = ArtifactStore::open(&root.0).unwrap();
    let artifact = store
        .put(
            b"canonical",
            ArtifactMetadata::new(
                "text/plain",
                ArtifactClass::Report,
                "principal_a",
                "project_b",
                ArtifactRetention::UntilUnixMicros(42),
                7,
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(
        artifact.manifest().canonical_bytes(),
        b"kit-artifact-manifest-v1\nsize=9\nmedia=text/plain\nclass=report\nprincipal=principal_a\nproject=project_b\nretention=until:42\nstored_at=7\n"
    );
    assert_eq!(store.open_bytes(artifact.digest()).unwrap(), b"canonical");
}

#[test]
fn persistent_leases_and_owner_references_survive_expired_retention() {
    let root = StateRoot::new("lease");
    let store = ArtifactStore::open(&root.0).unwrap();
    let artifact = store
        .put(b"leased", metadata(ArtifactRetention::UntilUnixMicros(0)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let lease = store
        .acquire_lease_before(artifact.digest(), "transaction:test", deadline)
        .unwrap();
    let expired = Reachability {
        now_unix_micros: i64::MAX,
        orphan_grace_micros: 0,
        ..Reachability::default()
    };
    assert!(
        !store
            .collect_garbage(&expired)
            .unwrap()
            .deleted_artifacts
            .contains(&artifact.digest())
    );

    store
        .transfer_lease_to_reference_before(&lease, "workspace-revision:test", deadline)
        .unwrap();
    assert!(
        !store
            .collect_garbage(&expired)
            .unwrap()
            .deleted_artifacts
            .contains(&artifact.digest())
    );
    store
        .release_reference_before(artifact.digest(), "workspace-revision:test", deadline)
        .unwrap();
    assert!(
        store
            .collect_garbage(&expired)
            .unwrap()
            .deleted_artifacts
            .contains(&artifact.digest())
    );
}

#[test]
fn identical_content_has_independent_opaque_ownership_and_retention_records() {
    let root = StateRoot::new("independent-records");
    let store = ArtifactStore::open(&root.0).unwrap();
    let bytes = b"shared bytes";
    let principal_a = PrincipalId::generate().unwrap();
    let project_a = ProjectId::generate().unwrap();
    let principal_b = PrincipalId::generate().unwrap();
    let project_b = ProjectId::generate().unwrap();
    let (authenticated_a, _, _) =
        kit::test_support::trusted_verification_context(principal_a, project_a);
    let (authenticated_b, _, _) =
        kit::test_support::trusted_verification_context(principal_b, project_b);
    let first = store
        .put(
            bytes,
            ArtifactMetadata::new(
                "text/plain",
                ArtifactClass::Log,
                principal_a.to_string(),
                project_a.to_string(),
                ArtifactRetention::UntilUnixMicros(100),
                10,
            )
            .unwrap(),
        )
        .unwrap();
    let second = store
        .put(
            bytes,
            ArtifactMetadata::new(
                "application/octet-stream",
                ArtifactClass::Report,
                principal_b.to_string(),
                project_b.to_string(),
                ArtifactRetention::UntilUnixMicros(200),
                20,
            )
            .unwrap(),
        )
        .unwrap();

    assert_eq!(first.digest(), second.digest());
    assert_ne!(first.reference(), second.reference());
    assert_ne!(first.reference().to_string(), first.digest().to_string());
    let first = store
        .resolve_reference(&authenticated_a, first.reference())
        .unwrap();
    let second = store
        .resolve_reference(&authenticated_b, second.reference())
        .unwrap();
    assert!(
        store
            .resolve_reference(&authenticated_b, first.reference())
            .is_err()
    );
    assert_eq!(first.manifest().principal, principal_a.to_string());
    assert_eq!(first.manifest().project, project_a.to_string());
    assert_eq!(first.manifest().class, ArtifactClass::Log);
    assert_eq!(first.manifest().stored_at_unix_micros, 10);
    assert_eq!(second.manifest().principal, principal_b.to_string());
    assert_eq!(second.manifest().project, project_b.to_string());
    assert_eq!(second.manifest().class, ArtifactClass::Report);
    assert_eq!(second.manifest().stored_at_unix_micros, 20);

    let digest = first.digest();
    let retained = store
        .collect_garbage(&Reachability {
            now_unix_micros: 150,
            orphan_grace_micros: 0,
            ..Reachability::default()
        })
        .unwrap();
    assert!(!retained.deleted_artifacts.contains(&digest));
    assert_eq!(store.open_bytes(digest).unwrap(), bytes);
    let deleted = store
        .collect_garbage(&Reachability {
            now_unix_micros: i64::MAX,
            orphan_grace_micros: 0,
            ..Reachability::default()
        })
        .unwrap();
    assert!(deleted.deleted_artifacts.contains(&digest));
}

#[test]
fn empty_streams_publish_distinct_references_to_one_content_object() {
    let root = StateRoot::new("empty-streams");
    let store = ArtifactStore::open(&root.0).unwrap();
    let stdout = store
        .put(b"", metadata(ArtifactRetention::Forever))
        .unwrap();
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let (authenticated, _, _) = kit::test_support::trusted_verification_context(principal, project);
    let stderr = store
        .put(
            b"",
            ArtifactMetadata::new(
                "application/octet-stream",
                ArtifactClass::Log,
                principal.to_string(),
                project.to_string(),
                ArtifactRetention::UntilUnixMicros(42),
                7,
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(stdout.digest(), stderr.digest());
    assert_ne!(stdout.reference(), stderr.reference());
    assert!(store.open_bytes(stdout.digest()).unwrap().is_empty());
    assert_eq!(
        store
            .resolve_reference(&authenticated, stderr.reference())
            .unwrap()
            .manifest()
            .principal,
        principal.to_string()
    );
}
