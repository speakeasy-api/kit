#![cfg(debug_assertions)]

use std::{collections::BTreeMap, path::PathBuf};

use kit::test_support;
use kit::{
    api::{
        auth::{
            contract::{Authenticator, GrantSnapshot, ScopedAuthorizer},
            local_peer::{LocalPeerAuthenticator, LocalPeerObservation},
        },
        service::{
            ArtifactService, CapabilityService, Command, EventCursor, LeaseService, Query,
            RequestContext, RetentionPeriod, RetentionPolicy, Scheduler, ServiceError,
        },
    },
    domain::{
        config::Grant,
        events::{ArtifactRef, SchemaVersion, TraceId},
        ids::{EventId, PrincipalId, ProjectId, RunId, ThreadId},
    },
    store::sqlite::idempotency::IdempotencyKey,
};

struct TestDatabase {
    root: PathBuf,
    path: PathBuf,
}

#[derive(Clone, Copy)]
struct TestRuntime;

impl Scheduler for TestRuntime {}
impl CapabilityService for TestRuntime {}
impl LeaseService for TestRuntime {}

impl ArtifactService for TestRuntime {
    fn commit_verified<T>(
        &self,
        _principal_id: PrincipalId,
        _project_id: ProjectId,
        _command: &Command,
        commit: impl FnOnce() -> Result<T, ServiceError>,
    ) -> Result<T, ServiceError> {
        commit()
    }
}

impl TestDatabase {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kit-erasure-{}-{}",
            std::process::id(),
            EventId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("store.sqlite3");
        Self { root, path }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn context(principal: PrincipalId, project: ProjectId, serial: usize) -> RequestContext {
    let authenticator = LocalPeerAuthenticator::new(BTreeMap::from([(
        1000,
        GrantSnapshot::new(
            principal,
            project,
            [
                Grant::WorkspaceRead,
                Grant::WorkspaceWrite,
                Grant::ModelCall,
            ],
        ),
    )]));
    RequestContext::authenticated(
        authenticator.authenticate(&LocalPeerObservation::from_transport(1000, 1, 1000)),
        Some(IdempotencyKey::parse(&format!("erase-{serial}")).unwrap()),
        TraceId::parse(&format!("erase-trace-{serial:016x}")).unwrap(),
    )
    .unwrap()
}

#[test]
fn completed_deletion_erases_payload_projection_and_replay_source() {
    let database = TestDatabase::new();
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let thread = ThreadId::generate().unwrap();
    let run = RunId::generate().unwrap();
    let run_content = format!("blake3:{}", "e".repeat(64));
    let mut service = test_support::service_with_runtime(
        test_support::open_service_store(&database.path).unwrap(),
        ScopedAuthorizer,
        TestRuntime,
    );
    service
        .execute(
            &context(principal, project, 1),
            Command::CreateProject {
                schema_version: SchemaVersion::CURRENT,
                project_id: project,
            },
        )
        .unwrap();
    service
        .execute(
            &context(principal, project, 2),
            Command::SetProjectRetention {
                schema_version: SchemaVersion::CURRENT,
                project_id: project,
                policy: RetentionPolicy {
                    transcript: RetentionPeriod::ForMicros(0),
                    ..RetentionPolicy::FOREVER
                },
                expected_version: 1,
            },
        )
        .unwrap();
    service
        .execute(
            &context(principal, project, 3),
            Command::CreateThread {
                schema_version: SchemaVersion::CURRENT,
                thread_id: thread,
                project_id: project,
            },
        )
        .unwrap();
    service
        .execute(
            &context(principal, project, 4),
            Command::StartRun {
                schema_version: SchemaVersion::CURRENT,
                run_id: run,
                thread_id: thread,
                input: ArtifactRef::parse(&run_content).unwrap(),
                run_config: None,
                experiment_config: None,
                effective_config: None,
            },
        )
        .unwrap();
    service
        .execute(
            &context(principal, project, 5),
            Command::InitiateThreadDeletion {
                schema_version: SchemaVersion::CURRENT,
                thread_id: thread,
                expected_version: 1,
            },
        )
        .unwrap();
    let report = service
        .store_mut()
        .run_deletion_jobs("test-worker", 4, |_| Ok(()))
        .unwrap();
    assert_eq!(report.completed, 1);
    assert!(matches!(
        service.query(
            &context(principal, project, 5),
            Query::GetThread { thread_id: thread }
        ),
        Err(ServiceError::NotFound)
    ));
    assert!(matches!(
        service.execute(
            &context(principal, project, 7),
            Command::CreateThread {
                schema_version: SchemaVersion::CURRENT,
                thread_id: thread,
                project_id: project,
            }
        ),
        Err(ServiceError::Conflict(_))
    ));
    drop(service);

    let connection = rusqlite::Connection::open(&database.path).unwrap();
    let payloads = connection
        .prepare("SELECT payload, artifacts FROM events ORDER BY commit_position")
        .unwrap()
        .query_map([], |row| {
            let mut bytes = row.get::<_, Vec<u8>>(0)?;
            bytes.extend_from_slice(&row.get::<_, Vec<u8>>(1)?);
            Ok(bytes)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .concat();
    assert!(
        !payloads
            .windows(thread.to_string().len())
            .any(|bytes| { bytes == thread.to_string().as_bytes() })
    );
    let tombstones: i64 = connection
        .query_row("SELECT count(*) FROM deletion_tombstones", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(tombstones, 1);
    drop(connection);
    for path in [&database.path, &database.path.with_extension("sqlite3-wal")] {
        if let Ok(bytes) = std::fs::read(path) {
            assert!(
                !bytes
                    .windows(run_content.len())
                    .any(|bytes| bytes == run_content.as_bytes())
            );
        }
    }

    let mut projections = test_support::open_projection_store(&database.path).unwrap();
    let (rebuilt, _) = test_support::rebuild_domain_projection(&mut projections).unwrap();
    assert!(rebuilt.thread(thread).is_none());
    drop(projections);
    let mut restarted = test_support::service_with_runtime(
        test_support::open_service_store(&database.path).unwrap(),
        ScopedAuthorizer,
        TestRuntime,
    );
    assert!(matches!(
        restarted.query(
            &context(principal, project, 6),
            Query::GetThread { thread_id: thread }
        ),
        Err(ServiceError::NotFound)
    ));
}

#[test]
fn domain_projection_size_is_bounded_by_state_not_event_count() {
    let database = TestDatabase::new();
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let thread = ThreadId::generate().unwrap();
    let mut service = test_support::service_with_runtime(
        test_support::open_service_store(&database.path).unwrap(),
        ScopedAuthorizer,
        TestRuntime,
    );
    for (serial, command) in [
        Command::CreateProject {
            schema_version: SchemaVersion::CURRENT,
            project_id: project,
        },
        Command::CreateThread {
            schema_version: SchemaVersion::CURRENT,
            thread_id: thread,
            project_id: project,
        },
    ]
    .into_iter()
    .enumerate()
    {
        service
            .execute(&context(principal, project, serial), command)
            .unwrap();
    }
    let before = std::fs::metadata(&database.path).unwrap().len();
    for version in 1..=200 {
        service
            .execute(
                &context(principal, project, version as usize + 10),
                Command::SetThreadArchived {
                    schema_version: SchemaVersion::CURRENT,
                    thread_id: thread,
                    archived: version % 2 == 0,
                    expected_version: version,
                },
            )
            .unwrap();
    }
    let snapshot = service.store_mut().projection_digest().unwrap();
    assert_ne!(snapshot, [0; 32]);
    let connection = rusqlite::Connection::open(&database.path).unwrap();
    let projection_size: i64 = connection
        .query_row(
            "SELECT length(canonical_bytes) FROM projection_state WHERE name = 'domain'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(projection_size < 8_192);
    assert!(std::fs::metadata(&database.path).unwrap().len() >= before);
}

#[test]
fn compaction_respects_active_cursor_and_persists_rebuild_gap() {
    let database = TestDatabase::new();
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let thread = ThreadId::generate().unwrap();
    let mut service = test_support::service_with_runtime(
        test_support::open_service_store(&database.path).unwrap(),
        ScopedAuthorizer,
        TestRuntime,
    );
    for (serial, command) in [
        Command::CreateProject {
            schema_version: SchemaVersion::CURRENT,
            project_id: project,
        },
        Command::SetProjectRetention {
            schema_version: SchemaVersion::CURRENT,
            project_id: project,
            policy: RetentionPolicy {
                event: RetentionPeriod::ForMicros(0),
                ..RetentionPolicy::FOREVER
            },
            expected_version: 1,
        },
        Command::CreateThread {
            schema_version: SchemaVersion::CURRENT,
            thread_id: thread,
            project_id: project,
        },
    ]
    .into_iter()
    .enumerate()
    {
        service
            .execute(&context(principal, project, serial + 100), command)
            .unwrap();
    }
    let blocked = service
        .store_mut()
        .compact_retained_events(i64::MAX, &BTreeMap::from([(project, EventCursor::START)]))
        .unwrap();
    assert_eq!(blocked.erased, 0);
    let compacted = service
        .store_mut()
        .compact_retained_events(
            i64::MAX,
            &BTreeMap::from([(project, EventCursor::new(u64::MAX))]),
        )
        .unwrap();
    assert_eq!(compacted.erased, 3);
    drop(service);

    let connection = rusqlite::Connection::open(&database.path).unwrap();
    let gap: (i64, i64, Vec<u8>) = connection
        .query_row(
            "SELECT first_available_position, compacted_through, cursor_expiry_snapshot
             FROM retention_event_gaps WHERE project_id = ?1 AND event_class = 'event'",
            [project.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(gap.0, 4);
    assert_eq!(gap.1, 3);
    assert!(!gap.2.is_empty());
    drop(connection);

    let mut projections = test_support::open_projection_store(&database.path).unwrap();
    let (rebuilt, _) = test_support::rebuild_domain_projection(&mut projections).unwrap();
    assert!(rebuilt.thread(thread).is_some());
}
