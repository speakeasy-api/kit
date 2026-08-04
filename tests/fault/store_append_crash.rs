use std::path::{Path, PathBuf};

use kit::domain::commands::ExpectedVersion;
use kit::domain::events::{EntityId, EventType, SchemaVersion, TraceId, UtcDateTime};
use kit::domain::ids::{CommandId, EventId, PrincipalId, RunId};
use kit::store::sqlite::append::{
    AppendCommand, AppendOutcome, CrashPoint, ExpectedStreamVersion, NewEvent,
    PendingArtifactPublication, StoreError,
};
use kit::store::sqlite::idempotency::{
    CanonicalRequestDigest, ClaimOutcome, IdempotencyKey, IdempotencyScope, IdempotencyStatus,
};
use kit::test_support;

const CRASH_CASES: [(CrashPoint, usize); 11] = [
    (CrashPoint::AfterTransactionBegin, 1),
    (CrashPoint::AfterIdempotencyCheck, 1),
    (CrashPoint::AfterExpectedVersionCheck, 1),
    (CrashPoint::AfterEventInsert, 1),
    (CrashPoint::AfterEventInsert, 2),
    (CrashPoint::AfterStreamHeadsUpdate, 1),
    (CrashPoint::AfterWatermarkUpdate, 1),
    (CrashPoint::BeforeIdempotencyTerminal, 1),
    (CrashPoint::AfterIdempotencyTerminal, 1),
    (CrashPoint::BeforeCommit, 1),
    (CrashPoint::AfterCommit, 1),
];

struct TestDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new(point: CrashPoint, occurrence: usize) -> Self {
        let unique = EventId::generate().unwrap().to_string();
        let directory =
            std::env::temp_dir().join(format!("kit-crash-{point:?}-{occurrence}-{unique}"));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("store.sqlite3");
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

fn event(stream: RunId, payload: &[u8]) -> NewEvent {
    NewEvent {
        id: EventId::generate().unwrap(),
        stream: EntityId::Run(stream),
        event_type: EventType::parse("run.crash_tested").unwrap(),
        schema_version: SchemaVersion::V1,
        occurred_at: UtcDateTime::parse("2026-07-21T12:00:00Z").unwrap(),
        causation_id: CommandId::generate().unwrap(),
        correlation_id: EntityId::Run(stream),
        attempt_id: None,
        trace_id: TraceId::parse("trace-crash-matrix").unwrap(),
        payload: payload.to_vec(),
        artifacts: b"[]".to_vec(),
    }
}

fn command(key: &str, principal: PrincipalId, first: RunId, second: RunId) -> AppendCommand {
    AppendCommand {
        idempotency_scope: IdempotencyScope::new(principal, "run.crash_test", EntityId::Run(first))
            .unwrap(),
        idempotency_key: IdempotencyKey::parse(key).unwrap(),
        request_digest: CanonicalRequestDigest::new([42; 32]),
        claim: None,
        driver_claim: None,
        allow_quiescent_driver_claim: false,
        expected_versions: vec![
            ExpectedStreamVersion {
                stream: EntityId::Run(first),
                version: ExpectedVersion::new(0),
            },
            ExpectedStreamVersion {
                stream: EntityId::Run(second),
                version: ExpectedVersion::new(0),
            },
        ],
        events: vec![
            NewEvent {
                artifacts: br#"["blake3:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]"#.to_vec(),
                ..event(first, b"first")
            },
            event(second, b"second"),
        ],
        response: b"terminal-response".to_vec(),
    }
}

#[test]
fn every_transaction_boundary_recovers_as_all_or_nothing_without_invented_success() {
    for (point, occurrence) in CRASH_CASES {
        let database = TestDatabase::new(point, occurrence);
        let first = RunId::generate().unwrap();
        let second = RunId::generate().unwrap();
        let principal = PrincipalId::generate().unwrap();
        let key = format!("crash-{point:?}");
        let mut append = command(&key, principal, first, second);
        let expected_events = append.events.clone();
        let scope = append.idempotency_scope.clone();
        let idempotency_key = append.idempotency_key.clone();

        let mut store = test_support::open_sqlite_store(database.path()).unwrap();
        let publication = PendingArtifactPublication {
            reference: format!("artifact-ref:{}", "b".repeat(64)),
            digest: format!("blake3:{}", "a".repeat(64)),
            purpose: "mcp_invocation_result".to_owned(),
            subject_id: "invocation_crash_test".to_owned(),
            principal_id: principal.to_string(),
            project_id: "project_crash_test".to_owned(),
            run_id: first.to_string(),
        };
        test_support::arm_artifact_publication(&mut store, publication.clone()).unwrap();
        let ClaimOutcome::Claimed(claim) = test_support::claim(
            &mut store,
            scope.clone(),
            idempotency_key.clone(),
            append.request_digest,
        )
        .unwrap() else {
            panic!("fresh crash request must be claimed")
        };
        append.claim = Some(claim);
        let mut visits = 0;
        let result = test_support::append_with_hook(&mut store, append.clone(), |visited| {
            if visited != point {
                return false;
            }
            visits += 1;
            visits == occurrence
        });
        assert!(matches!(result, Err(StoreError::InjectedCrash(actual)) if actual == point));
        drop(store);

        let mut recovered = test_support::open_sqlite_store(database.path()).unwrap();
        let committed = point == CrashPoint::AfterCommit;
        let journaled: usize = rusqlite::Connection::open(database.path())
            .unwrap()
            .query_row(
                "SELECT count(*) FROM artifact_publication_journal",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(journaled, usize::from(committed));
        assert_eq!(
            recovered.events().unwrap().len(),
            if committed { 2 } else { 0 }
        );
        assert_eq!(
            recovered.committed_through().unwrap(),
            if committed { 2 } else { 0 }
        );
        let status = recovered
            .idempotency_status(&scope, &idempotency_key)
            .unwrap();
        assert_eq!(
            matches!(&status, IdempotencyStatus::Terminal { .. }),
            committed,
            "terminal idempotency state at {point:?}"
        );
        assert!(
            committed || matches!(&status, IdempotencyStatus::Pending { .. }),
            "pre-commit crash did not preserve the durable pending claim at {point:?}"
        );

        let mut conflicting = append.clone();
        conflicting.claim = None;
        conflicting.request_digest = CanonicalRequestDigest::new([43; 32]);
        assert!(matches!(
            test_support::append(&mut recovered, conflicting),
            Err(StoreError::IdempotencyConflict(_))
        ));

        append.claim = None;
        test_support::arm_artifact_publication(&mut recovered, publication).unwrap();
        let retry = test_support::append(&mut recovered, append).unwrap();
        if committed {
            assert!(matches!(retry, AppendOutcome::Replayed(_)));
        } else {
            assert!(matches!(retry, AppendOutcome::Committed(_)));
        }
        let events = recovered.events().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(
            events
                .iter()
                .map(|event| event.event.clone())
                .collect::<Vec<_>>(),
            expected_events
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.commit_position.get())
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(recovered.committed_through().unwrap(), 2);
    }
}
