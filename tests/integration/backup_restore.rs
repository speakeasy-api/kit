use std::path::{Path, PathBuf};

use kit::domain::commands::ExpectedVersion;
use kit::domain::events::{EntityId, EventType, SchemaVersion, TraceId, UtcDateTime};
use kit::domain::ids::{ArtifactId, CommandId, EventId, PrincipalId, ProjectId, RunId};
use kit::store::artifacts::{ArtifactClass, ArtifactMetadata, ArtifactRetention, ArtifactStore};
use kit::store::backup::{BackupConfig, BackupError, BackupManager};
use kit::store::sqlite::append::StoredEvent;
use kit::store::sqlite::append::{AppendCommand, ExpectedStreamVersion, NewEvent};
use kit::store::sqlite::idempotency::{CanonicalRequestDigest, IdempotencyKey, IdempotencyScope};
use kit::store::sqlite::projection::ProjectionError;
use kit::test_support;

struct Fixture {
    root: PathBuf,
    state: PathBuf,
    database: PathBuf,
    artifacts: PathBuf,
    backups: PathBuf,
    artifact_id: kit::store::artifacts::ArtifactId,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "kit-backup-{name}-{}-{}",
            std::process::id(),
            EventId::generate().unwrap()
        ));
        let state = root.join("state");
        let artifacts = state.join("artifacts");
        let backups = root.join("backups");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::create_dir(&backups).unwrap();
        let database = state.join("store.sqlite3");
        let artifact_store = ArtifactStore::open(&artifacts).unwrap();
        let artifact = artifact_store
            .put(
                b"shared content awaiting physical deletion",
                ArtifactMetadata::new(
                    "text/plain",
                    ArtifactClass::Report,
                    "principal_backup",
                    "project_backup",
                    ArtifactRetention::UntilUnixMicros(150),
                    10,
                )
                .unwrap(),
            )
            .unwrap();
        let stream = RunId::generate().unwrap();
        let principal = PrincipalId::generate().unwrap();
        let mut store = test_support::open_sqlite_store(&database).unwrap();
        test_support::append(
            &mut store,
            AppendCommand {
                idempotency_scope: IdempotencyScope::new(
                    principal,
                    "backup.append",
                    EntityId::Run(stream),
                )
                .unwrap(),
                idempotency_key: IdempotencyKey::parse("backup-fixture").unwrap(),
                request_digest: CanonicalRequestDigest::new([7; 32]),
                claim: None,
                driver_claim: None,
                allow_quiescent_driver_claim: false,
                expected_versions: vec![ExpectedStreamVersion {
                    stream: EntityId::Run(stream),
                    version: ExpectedVersion::new(0),
                }],
                events: vec![NewEvent {
                    id: EventId::generate().unwrap(),
                    stream: EntityId::Run(stream),
                    event_type: EventType::parse("backup.created").unwrap(),
                    schema_version: SchemaVersion::V1,
                    occurred_at: UtcDateTime::parse("2026-07-22T00:00:00Z").unwrap(),
                    causation_id: CommandId::generate().unwrap(),
                    correlation_id: EntityId::Run(stream),
                    attempt_id: None,
                    trace_id: TraceId::parse("backup-0000000000000001").unwrap(),
                    payload: b"durable event".to_vec(),
                    artifacts: format!("[\"{}\"]", artifact.id()).into_bytes(),
                }],
                response: b"committed".to_vec(),
            },
        )
        .unwrap();
        drop(store);
        let mut projections = test_support::open_projection_store(&database).unwrap();
        test_support::rebuild_projection(&mut projections, "events", b"projection-v1", reduce)
            .unwrap();
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "INSERT INTO deletion_objects
                 (object_key, object_kind, principal_id, project_id, stored_at_unix_micros,
                  archived, artifact_digest, policy_json)
                 VALUES ('artifact:backup-fixture', 'artifact', 'principal_backup',
                         'project_backup', 10, 0, NULL, '{}');
                 INSERT INTO deletion_legal_holds
                 (hold_id, scope_kind, scope_id, placed_at_unix_micros)
                 VALUES ('1', 'object', 'artifact:backup-fixture', 10);
                 INSERT INTO deletion_artifact_references
                 (reference_id, artifact_id, principal_id, project_id, expires_at_unix_micros)
                 VALUES ('1', 'backup-fixture', 'principal_backup', 'project_backup', 150);
                 INSERT INTO deletion_backup_generations
                 (generation_id, created_at_unix_micros, expires_at_unix_micros)
                 VALUES ('1', 10, 150);
                 INSERT INTO deletion_backup_contents (generation_id, object_key)
                 VALUES ('1', 'artifact:backup-fixture');
                 INSERT INTO deletion_jobs
                 (job_id, principal_id, project_id, object_key, idempotency_key,
                  resource_version, policy_snapshot_json, policy_json,
                  earliest_physical_unix_micros, state, version, fence, blockers_json,
                  requested_at_unix_micros)
                 VALUES ('deletion_00000000000000000000000000000001', 'principal_backup',
                         'project_backup', 'artifact:backup-fixture', 'backup-delete', 1,
                         '{}', '{}', 150, 'blocked', 1, 1,
                         '[\"legal_hold\",\"active_reference\",\"backup_generation\"]', 100);
                 INSERT INTO deletion_job_audit (job_id, sequence, state, at_unix_micros)
                 VALUES ('deletion_00000000000000000000000000000001', 1, 'blocked', 100);",
            )
            .unwrap();
        connection
            .execute(
                "UPDATE deletion_objects SET artifact_digest = ?1, policy_json = ?2
                 WHERE object_key = 'artifact:backup-fixture'",
                rusqlite::params![
                    artifact.id().to_string(),
                    br#"{"event":"forever","transcript":"forever","terminal":"forever","artifact":"forever","experiment":"forever","backup":"forever"}"#.as_slice(),
                ],
            )
            .unwrap();
        Self {
            root,
            state,
            database,
            artifacts,
            backups,
            artifact_id: artifact.id(),
        }
    }

    fn manager(&self, retain: usize, expiry: i64) -> BackupManager {
        test_support::open_backup_manager(BackupConfig {
            state_root: self.state.clone(),
            database_path: self.database.clone(),
            artifact_root: self.artifacts.clone(),
            destination: self.backups.clone(),
            retain_generations: retain,
            backup_expires_at_unix_micros: expiry,
            build_version: env!("CARGO_PKG_VERSION").to_owned(),
        })
        .unwrap()
    }

    fn create_backup(
        &self,
        manager: &mut BackupManager,
        now_unix_micros: i64,
    ) -> kit::store::backup::BackupGeneration {
        let mut inventory = test_support::open_service_store(&self.database).unwrap();
        test_support::create_backup(manager, now_unix_micros, &mut inventory).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn reduce(bytes: &mut Vec<u8>, event: &StoredEvent) -> Result<(), ProjectionError> {
    bytes.extend_from_slice(&event.canonical_bytes());
    Ok(())
}

fn artifact_file(generation: &Path, id: kit::store::artifacts::ArtifactId) -> PathBuf {
    generation.join("artifacts").join(format!(
        "{}.blob",
        id.to_string().strip_prefix("blake3:").unwrap()
    ))
}

fn generation_manifest(generation: &kit::store::backup::BackupGeneration) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(generation.path.join("manifest.json")).unwrap()).unwrap()
}

#[test]
fn ten_fresh_restores_match_database_projection_and_artifacts() {
    let fixture = Fixture::new("ten-restores");
    let mut manager = fixture.manager(3, 1_000);
    let generation = fixture.create_backup(&mut manager, 100);
    for index in 0..10 {
        let restored = fixture.root.join(format!("restored-{index}"));
        let report = manager
            .restore(&generation.name, &restored, 999, env!("CARGO_PKG_VERSION"))
            .unwrap();
        assert_eq!(report.commit_watermark, 1);
        assert_eq!(report.artifact_count, 1);
        let restored_database = rusqlite::Connection::open(restored.join("store.sqlite3")).unwrap();
        for table in [
            "deletion_jobs",
            "deletion_legal_holds",
            "deletion_artifact_references",
            "deletion_backup_generations",
            "deletion_backup_contents",
        ] {
            let count: i64 = restored_database
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 1, "restored {table}");
        }
        assert_eq!(
            ArtifactStore::open(restored.join("artifacts"))
                .unwrap()
                .open_bytes(fixture.artifact_id)
                .unwrap(),
            b"shared content awaiting physical deletion"
        );
    }
    let health = manager.health(125);
    assert_eq!(health.age_micros, Some(25));
    assert_eq!(health.last_success.unwrap().generation, generation.name);
}

#[test]
fn corrupt_database_and_artifact_fail_closed_without_publishing_state() {
    let fixture = Fixture::new("corruption");
    let mut manager = fixture.manager(3, 1_000);
    let database_generation = fixture.create_backup(&mut manager, 100);
    let database = database_generation.path.join("store.sqlite3");
    let mut bytes = std::fs::read(&database).unwrap();
    bytes[100] ^= 0xff;
    std::fs::write(&database, bytes).unwrap();
    let database_restore = fixture.root.join("corrupt-database-restore");
    assert!(matches!(
        manager.restore(
            &database_generation.name,
            &database_restore,
            101,
            env!("CARGO_PKG_VERSION")
        ),
        Err(BackupError::DigestMismatch("database"))
    ));
    assert!(!database_restore.exists());

    let artifact_generation = fixture.create_backup(&mut manager, 102);
    let artifact = artifact_file(&artifact_generation.path, fixture.artifact_id);
    std::fs::write(&artifact, b"corrupt").unwrap();
    let artifact_restore = fixture.root.join("corrupt-artifact-restore");
    assert!(matches!(
        manager.restore(
            &artifact_generation.name,
            &artifact_restore,
            103,
            env!("CARGO_PKG_VERSION")
        ),
        Err(BackupError::DigestMismatch("artifact object"))
    ));
    assert!(!artifact_restore.exists());
}

#[test]
fn generation_retention_is_exact() {
    let fixture = Fixture::new("retention");
    let mut manager = fixture.manager(3, 1_000);
    for now in 100..105 {
        fixture.create_backup(&mut manager, now);
    }
    let generations = manager.generations().unwrap();
    assert_eq!(generations.len(), 3);
    assert_eq!(
        generations
            .iter()
            .map(|generation| generation.created_at_unix_micros)
            .collect::<Vec<_>>(),
        vec![102, 103, 104]
    );
}

#[test]
fn shared_deleted_content_restores_only_before_advertised_expiry() {
    let fixture = Fixture::new("expiry");
    let mut manager = fixture.manager(2, 200);
    let generation = fixture.create_backup(&mut manager, 100);
    let before = fixture.root.join("before-expiry");
    manager
        .restore(&generation.name, &before, 199, env!("CARGO_PKG_VERSION"))
        .unwrap();
    assert_eq!(
        ArtifactStore::open(before.join("artifacts"))
            .unwrap()
            .open_bytes(fixture.artifact_id)
            .unwrap(),
        b"shared content awaiting physical deletion"
    );

    let at_boundary = fixture.root.join("at-expiry");
    assert!(matches!(
        manager.restore(
            &generation.name,
            &at_boundary,
            200,
            env!("CARGO_PKG_VERSION")
        ),
        Err(BackupError::BackupExpired(200))
    ));
    assert!(!at_boundary.exists());
}

#[test]
fn published_generations_remain_in_deletion_inventory_until_expiry() {
    let fixture = Fixture::new("inventory-expiry");
    let mut manager = fixture.manager(1, 200);
    let mut inventory = test_support::open_service_store(&fixture.database).unwrap();
    let mut names = Vec::new();
    for now in 100..103 {
        let generation = test_support::create_backup(&mut manager, now, &mut inventory).unwrap();
        names.push(generation.name);
    }
    assert_eq!(manager.generations().unwrap().len(), 1);

    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    for name in &names {
        let contents: i64 = connection
            .query_row(
                "SELECT count(*) FROM deletion_backup_contents WHERE generation_id = ?1",
                [name],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(contents, 1, "inventory missing for {name}");
    }
    drop(connection);

    assert_eq!(inventory.expire_backup_generations(199).unwrap(), 1);
    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    let active: i64 = connection
        .query_row(
            "SELECT count(*) FROM deletion_backup_generations
             WHERE generation_id LIKE 'generation-%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active, 3);
    drop(connection);

    assert_eq!(inventory.expire_backup_generations(200).unwrap(), 3);
    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    let active: i64 = connection
        .query_row(
            "SELECT count(*) FROM deletion_backup_generations
             WHERE generation_id LIKE 'generation-%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(active, 0);
}

#[test]
fn startup_verifies_generations_and_reconciles_missing_inventory() {
    let fixture = Fixture::new("startup-reconcile");
    let mut initial = fixture.manager(3, 200);
    let generations = (100..103)
        .map(|now| fixture.create_backup(&mut initial, now))
        .collect::<Vec<_>>();
    let latest = generations.last().unwrap();
    rusqlite::Connection::open(&fixture.database)
        .unwrap()
        .execute(
            "DELETE FROM deletion_backup_generations WHERE generation_id LIKE 'generation-%'",
            [],
        )
        .unwrap();
    let mut restarted = fixture.manager(1, 200);
    let generations = test_support::reconcile_backup_generations(&mut restarted, 103).unwrap();
    let mut inventory = test_support::open_service_store(&fixture.database).unwrap();
    inventory
        .reconcile_backup_generations(&generations, 103)
        .unwrap();
    test_support::prune_backup_generations(&mut restarted).unwrap();

    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    let contents: i64 = connection
        .query_row(
            "SELECT count(*) FROM deletion_backup_contents
             WHERE generation_id LIKE 'generation-%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(contents, 3);
    assert_eq!(restarted.generations().unwrap().len(), 1);
    assert_eq!(
        restarted.health(125).current_generation.as_deref(),
        Some(latest.name.as_str())
    );
}

#[test]
fn failed_inventory_registration_is_not_published_and_updates_health() {
    let fixture = Fixture::new("inventory-failure-health");
    let mut manager = fixture.manager(2, 200);
    let mut inventory = test_support::open_service_store(&fixture.database).unwrap();
    rusqlite::Connection::open(&fixture.database)
        .unwrap()
        .execute_batch("DROP TABLE deletion_backup_contents")
        .unwrap();
    let error = test_support::create_backup(&mut manager, 100, &mut inventory).unwrap_err();
    assert!(matches!(error, BackupError::Inventory(_)));
    assert!(manager.generations().unwrap().is_empty());

    let health = manager.health(110);
    assert!(health.last_success.is_none());
    assert!(health.current_generation.is_none());
    assert_eq!(health.last_failure.unwrap().at_unix_micros, 100);
}

#[test]
fn manager_uses_artifacts_registered_after_open() {
    let fixture = Fixture::new("post-open-artifact");
    let mut manager = fixture.manager(2, 1_000);
    let store = ArtifactStore::open(&fixture.artifacts).unwrap();
    let artifact = store
        .put(
            b"published after backup manager startup",
            ArtifactMetadata::new(
                "text/plain",
                ArtifactClass::File,
                "principal_backup",
                "project_backup",
                ArtifactRetention::Forever,
                20,
            )
            .unwrap(),
        )
        .unwrap();
    let record_id = ArtifactId::generate().unwrap();
    rusqlite::Connection::open(&fixture.database)
        .unwrap()
        .execute(
            "INSERT INTO deletion_objects
             (object_key, object_kind, principal_id, project_id, stored_at_unix_micros,
              archived, artifact_digest, policy_json)
             VALUES (?1, 'artifact', 'principal_backup', 'project_backup', 20, 0, ?2, ?3)",
            rusqlite::params![
                format!("artifact:{record_id}"),
                artifact.id().to_string(),
                br#"{"event":"forever","transcript":"forever","terminal":"forever","artifact":"forever","experiment":"forever","backup":"forever"}"#.as_slice(),
            ],
        )
        .unwrap();

    let generation = fixture.create_backup(&mut manager, 100);
    let restored = fixture.root.join("post-open-restored");
    let report = manager
        .restore(&generation.name, &restored, 101, env!("CARGO_PKG_VERSION"))
        .unwrap();
    assert_eq!(report.artifact_count, 2);
    assert_eq!(
        ArtifactStore::open(restored.join("artifacts"))
            .unwrap()
            .open_bytes(artifact.id())
            .unwrap(),
        b"published after backup manager startup"
    );
}

#[test]
fn opaque_references_and_verification_reports_restore_with_independent_metadata() {
    let fixture = Fixture::new("opaque-references");
    let store = ArtifactStore::open(&fixture.artifacts).unwrap();
    let principal_one = PrincipalId::generate().unwrap();
    let project_one = ProjectId::generate().unwrap();
    let principal_two = PrincipalId::generate().unwrap();
    let project_two = ProjectId::generate().unwrap();
    let (authenticated_one, _, _) =
        test_support::trusted_verification_context(principal_one, project_one);
    let (authenticated_two, _, _) =
        test_support::trusted_verification_context(principal_two, project_two);
    let first = store
        .put(
            b"equal content",
            ArtifactMetadata::new(
                "text/plain",
                ArtifactClass::File,
                principal_one.to_string(),
                project_one.to_string(),
                ArtifactRetention::UntilUnixMicros(500),
                20,
            )
            .unwrap(),
        )
        .unwrap();
    let second = store
        .put(
            b"equal content",
            ArtifactMetadata::new(
                "application/octet-stream",
                ArtifactClass::Report,
                principal_two.to_string(),
                project_two.to_string(),
                ArtifactRetention::Forever,
                21,
            )
            .unwrap(),
        )
        .unwrap();
    assert_eq!(first.digest(), second.digest());
    assert_ne!(first.reference(), second.reference());

    let mut manager = fixture.manager(2, 1_000);
    let generation = fixture.create_backup(&mut manager, 100);
    let restored = fixture.root.join("opaque-references-restored");
    manager
        .restore(&generation.name, &restored, 101, env!("CARGO_PKG_VERSION"))
        .unwrap();
    let restored = ArtifactStore::open(restored.join("artifacts")).unwrap();
    let restored_first = restored
        .resolve_reference(&authenticated_one, first.reference())
        .unwrap();
    let restored_second = restored
        .resolve_reference(&authenticated_two, second.reference())
        .unwrap();
    assert_eq!(
        restored_first.manifest().principal,
        principal_one.to_string()
    );
    assert_eq!(
        restored_first.manifest().retention,
        ArtifactRetention::UntilUnixMicros(500)
    );
    assert_eq!(
        restored_second.manifest().principal,
        principal_two.to_string()
    );
    assert_eq!(
        restored_second.manifest().retention,
        ArtifactRetention::Forever
    );
    assert_eq!(
        restored.open_bytes(restored_first.digest()).unwrap(),
        b"equal content"
    );
}

#[test]
fn policy_hold_and_physical_deletion_changes_are_read_per_generation() {
    let fixture = Fixture::new("live-policy");
    let mut manager = fixture.manager(3, 1_000);
    let first = fixture.create_backup(&mut manager, 100);
    let first_artifact = &generation_manifest(&first)["artifacts"][0];
    assert_eq!(first_artifact["policy_retention"], "forever");
    assert_eq!(first_artifact["legal_hold"], true);

    let connection = rusqlite::Connection::open(&fixture.database).unwrap();
    connection
        .execute(
            "UPDATE deletion_objects SET policy_json = ?1
             WHERE object_key = 'artifact:backup-fixture'",
            [br#"{"event":"forever","transcript":"forever","terminal":"forever","artifact":{"for_micros":90},"experiment":"forever","backup":"forever"}"#.as_slice()],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE deletion_legal_holds SET released_at_unix_micros = 101 WHERE hold_id = '1'",
            [],
        )
        .unwrap();
    let second = fixture.create_backup(&mut manager, 101);
    let second_artifact = &generation_manifest(&second)["artifacts"][0];
    assert_eq!(second_artifact["policy_retention"], "for:90");
    assert_eq!(second_artifact["legal_hold"], false);

    connection
        .execute(
            "UPDATE deletion_objects SET physically_deleted = 1
             WHERE object_key = 'artifact:backup-fixture'",
            [],
        )
        .unwrap();
    let third = fixture.create_backup(&mut manager, 102);
    let third_manifest = generation_manifest(&third);
    assert_eq!(third_manifest["artifacts"].as_array().unwrap().len(), 1);
    assert_eq!(
        third_manifest["artifacts"][0]["policy_retention"],
        serde_json::Value::Null
    );
    assert_eq!(third_manifest["artifacts"][0]["reachable"], false);
    assert_eq!(third_manifest["references"].as_array().unwrap().len(), 1);
}
