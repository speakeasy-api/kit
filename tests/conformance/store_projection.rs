use std::path::{Path, PathBuf};

use kit::domain::commands::ExpectedVersion;
use kit::domain::events::{EntityId, EventType, SchemaVersion, TraceId, UtcDateTime};
use kit::domain::ids::{CommandId, EventId, PrincipalId, RunId};
use kit::store::sqlite::append::StoredEvent;
use kit::store::sqlite::append::{AppendCommand, ExpectedStreamVersion, NewEvent};
use kit::store::sqlite::idempotency::{CanonicalRequestDigest, IdempotencyKey, IdempotencyScope};
use kit::store::sqlite::projection::{ProjectionCrashPoint, ProjectionError};
use kit::test_support;
use rusqlite::{Connection, params};

struct TestDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "kit-projection-{name}-{}",
            EventId::generate().unwrap()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("store.sqlite3");
        test_support::open_sqlite_store(&path).unwrap();
        Self { directory, path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn append(path: &Path, stream: RunId, principal: PrincipalId, first: usize, count: usize) {
    let mut store = test_support::open_sqlite_store(path).unwrap();
    let expected = store
        .events()
        .unwrap()
        .iter()
        .filter(|event| event.event.stream == EntityId::Run(stream))
        .count() as u64;
    let events = (first..first + count)
        .map(|seed| NewEvent {
            id: EventId::generate().unwrap(),
            stream: EntityId::Run(stream),
            event_type: EventType::parse("run.projected").unwrap(),
            schema_version: SchemaVersion::V1,
            occurred_at: UtcDateTime::parse("2026-07-21T12:00:00Z").unwrap(),
            causation_id: CommandId::generate().unwrap(),
            correlation_id: EntityId::Run(stream),
            attempt_id: None,
            trace_id: TraceId::parse(&format!("projection-{seed:016x}")).unwrap(),
            payload: format!("payload-{seed:04}").into_bytes(),
            artifacts: b"[]".to_vec(),
        })
        .collect();
    test_support::append(
        &mut store,
        AppendCommand {
            idempotency_scope: IdempotencyScope::new(
                principal,
                "projection.append",
                EntityId::Run(stream),
            )
            .unwrap(),
            idempotency_key: IdempotencyKey::parse(&format!("projection-{first}-{count}")).unwrap(),
            request_digest: CanonicalRequestDigest::new([first as u8; 32]),
            claim: None,
            driver_claim: None,
            allow_quiescent_driver_claim: false,
            expected_versions: vec![ExpectedStreamVersion {
                stream: EntityId::Run(stream),
                version: ExpectedVersion::new(expected),
            }],
            events,
            response: Vec::new(),
        },
    )
    .unwrap();
}

fn reduce(bytes: &mut Vec<u8>, event: &StoredEvent) -> Result<(), ProjectionError> {
    bytes.extend_from_slice(&event.canonical_bytes());
    Ok(())
}

#[test]
fn replay_is_byte_identical_across_twenty_restarts() {
    let database = TestDatabase::new("restarts");
    append(
        database.path(),
        RunId::generate().unwrap(),
        PrincipalId::generate().unwrap(),
        0,
        40,
    );
    let mut expected = None;
    for _ in 0..20 {
        let mut store = test_support::open_projection_store(database.path()).unwrap();
        let snapshot =
            test_support::rebuild_projection(&mut store, "events", b"projection-v1", reduce)
                .unwrap();
        if let Some((bytes, digest)) = &expected {
            assert_eq!(&snapshot.canonical_bytes, bytes);
            assert_eq!(&snapshot.digest, digest);
        } else {
            expected = Some((snapshot.canonical_bytes, snapshot.digest));
        }
    }
}

#[test]
fn incremental_update_equals_full_rebuild() {
    let database = TestDatabase::new("incremental");
    let stream = RunId::generate().unwrap();
    let principal = PrincipalId::generate().unwrap();
    append(database.path(), stream, principal, 0, 10);
    let mut store = test_support::open_projection_store(database.path()).unwrap();
    test_support::update_projection(&mut store, "incremental", b"projection-v1", reduce).unwrap();
    drop(store);
    append(database.path(), stream, principal, 10, 10);
    let mut store = test_support::open_projection_store(database.path()).unwrap();
    let incremental =
        test_support::update_projection(&mut store, "incremental", b"projection-v1", reduce)
            .unwrap();
    let rebuilt =
        test_support::rebuild_projection(&mut store, "rebuilt", b"projection-v1", reduce).unwrap();
    assert_eq!(incremental.canonical_bytes, rebuilt.canonical_bytes);
    assert_eq!(incremental.digest, rebuilt.digest);
    assert_eq!(incremental.checkpoint, rebuilt.checkpoint);
}

#[test]
fn projection_crash_rolls_back_state_checkpoint_and_clock() {
    let database = TestDatabase::new("crash");
    let stream = RunId::generate().unwrap();
    let principal = PrincipalId::generate().unwrap();
    append(database.path(), stream, principal, 0, 1);
    let mut store = test_support::open_projection_store(database.path()).unwrap();
    let before =
        test_support::update_projection(&mut store, "events", b"projection-v1", reduce).unwrap();
    drop(store);
    append(database.path(), stream, principal, 1, 1);
    let mut store = test_support::open_projection_store(database.path()).unwrap();
    let error = test_support::update_projection_with_hook(
        &mut store,
        "events",
        b"projection-v1",
        false,
        reduce,
        |point| point == ProjectionCrashPoint::AfterWrite,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        ProjectionError::InjectedCrash(ProjectionCrashPoint::AfterWrite)
    ));
    assert_eq!(store.load("events").unwrap().unwrap(), before);
}

#[test]
fn every_migration_round_trips_without_touching_events() {
    let database = TestDatabase::new("migration-roundtrip");
    append(
        database.path(),
        RunId::generate().unwrap(),
        PrincipalId::generate().unwrap(),
        0,
        1,
    );
    let store = test_support::open_projection_store(database.path()).unwrap();
    drop(store);
    for version in test_support::projection_migration_versions().rev() {
        assert_eq!(
            test_support::rollback_latest_projection_migration(database.path()).unwrap(),
            Some(version)
        );
    }
    let connection = Connection::open(database.path()).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM events", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'projection_state'",
        [], |row| row.get::<_, i64>(0),
    ).unwrap(), 0);
    drop(connection);
    test_support::open_projection_store(database.path()).unwrap();
}

#[test]
fn checksum_drift_and_unknown_newer_migration_are_rejected() {
    let drift = TestDatabase::new("migration-drift");
    test_support::open_projection_store(drift.path()).unwrap();
    let connection = Connection::open(drift.path()).unwrap();
    connection
        .execute(
            "UPDATE kit_sqlite_migrations SET checksum = ?1 WHERE version = 1",
            [[7_u8; 32].as_slice()],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        test_support::open_projection_store(drift.path()),
        Err(ProjectionError::MigrationDrift(1))
    ));

    let newer = TestDatabase::new("migration-newer");
    test_support::open_projection_store(newer.path()).unwrap();
    let connection = Connection::open(newer.path()).unwrap();
    connection.execute(
        "INSERT INTO kit_sqlite_migrations (version, name, checksum) VALUES (999, 'future', ?1)",
        [[9_u8; 32].as_slice()],
    ).unwrap();
    drop(connection);
    assert!(matches!(
        test_support::open_projection_store(newer.path()),
        Err(ProjectionError::UnknownMigration(999))
    ));
}

#[test]
fn checkpoint_cannot_outrun_committed_prefix() {
    let database = TestDatabase::new("watermark");
    test_support::open_projection_store(database.path()).unwrap();
    let connection = Connection::open(database.path()).unwrap();
    let error = connection
        .execute(
            "INSERT INTO projection_state (name, canonical_bytes, digest, checkpoint, updated_at)
         VALUES ('invalid', X'', zeroblob(32), 1, '2026-07-21T00:00:00.000Z')",
            [],
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("checkpoint exceeds committed prefix")
    );
}

#[test]
fn store_time_is_nondecreasing_and_consumed_in_its_transaction() {
    let database = TestDatabase::new("store-time");
    let mut store = test_support::open_projection_store(database.path()).unwrap();
    test_support::with_projection_store_time(&mut store, |transaction, now| {
        transaction.execute_batch(
            "CREATE TABLE lease_probe (at_micros INTEGER NOT NULL, at_rfc3339 TEXT NOT NULL)",
        )?;
        transaction.execute(
            "INSERT INTO lease_probe (at_micros, at_rfc3339) VALUES (?1, ?2)",
            params![now.unix_micros(), now.as_rfc3339()],
        )?;
        Ok(())
    })
    .unwrap();
    let first = test_support::projection_store_time(&mut store).unwrap();
    let second = test_support::projection_store_time(&mut store).unwrap();
    assert!(second.unix_micros() >= first.unix_micros());
    UtcDateTime::parse(first.as_rfc3339()).unwrap();
    let connection = Connection::open(database.path()).unwrap();
    let recorded: (i64, String) = connection
        .query_row("SELECT at_micros, at_rfc3339 FROM lease_probe", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert!(recorded.0 <= first.unix_micros());
    UtcDateTime::parse(&recorded.1).unwrap();
}
