use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use kit::domain::commands::ExpectedVersion;
use kit::domain::events::{EntityId, EventType, SchemaVersion, TraceId, UtcDateTime};
use kit::domain::ids::{CommandId, EventId, PrincipalId, RunId};
use kit::store::sqlite::append::{
    AppendCommand, AppendOutcome, ExpectedStreamVersion, NewEvent, StoreError,
};
use kit::store::sqlite::idempotency::{
    CanonicalRequestDigest, ClaimOutcome, IdempotencyKey, IdempotencyScope,
};
use kit::test_support;

const REPRESENTATIVE_CONNECTIONS: usize = 16;
const STRESS_CONNECTIONS: usize = 64;

struct TestDatabase {
    directory: PathBuf,
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let unique = EventId::generate().unwrap().to_string();
        let directory = std::env::temp_dir().join(format!("kit-{name}-{unique}"));
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

fn digest(seed: u8) -> CanonicalRequestDigest {
    CanonicalRequestDigest::new([seed; 32])
}

fn scope(principal: PrincipalId, command: &str, target: RunId) -> IdempotencyScope {
    IdempotencyScope::new(principal, command, EntityId::Run(target)).unwrap()
}

fn event(stream: RunId, seed: usize) -> NewEvent {
    NewEvent {
        id: EventId::generate().unwrap(),
        stream: EntityId::Run(stream),
        event_type: EventType::parse("run.tested").unwrap(),
        schema_version: SchemaVersion::V1,
        occurred_at: UtcDateTime::parse("2026-07-21T12:00:00Z").unwrap(),
        causation_id: CommandId::generate().unwrap(),
        correlation_id: EntityId::Run(stream),
        attempt_id: None,
        trace_id: TraceId::parse(&format!("trace-{seed:016x}")).unwrap(),
        payload: format!("payload-{seed}").into_bytes(),
        artifacts: b"[]".to_vec(),
    }
}

fn command(
    idempotency_scope: IdempotencyScope,
    key: &str,
    request_digest: CanonicalRequestDigest,
    expected: Vec<(RunId, u64)>,
    events: Vec<NewEvent>,
) -> AppendCommand {
    AppendCommand {
        idempotency_scope,
        idempotency_key: IdempotencyKey::parse(key).unwrap(),
        request_digest,
        claim: None,
        driver_claim: None,
        allow_quiescent_driver_claim: false,
        expected_versions: expected
            .into_iter()
            .map(|(stream, version)| ExpectedStreamVersion {
                stream: EntityId::Run(stream),
                version: ExpectedVersion::new(version),
            })
            .collect(),
        events,
        response: format!("response-{key}").into_bytes(),
    }
}

#[test]
fn one_command_atomically_appends_multiple_streams_and_checks_versions() {
    let database = TestDatabase::new("multi-stream");
    let mut store = test_support::open_sqlite_store(database.path()).unwrap();
    let principal = PrincipalId::generate().unwrap();
    let first = RunId::generate().unwrap();
    let second = RunId::generate().unwrap();
    let original_events = vec![event(first, 1), event(second, 2), event(first, 3)];
    let outcome = test_support::append(
        &mut store,
        command(
            scope(principal, "run.multi_stream", first),
            "multi-stream",
            digest(1),
            vec![(first, 0), (second, 0)],
            original_events.clone(),
        ),
    )
    .unwrap();
    let AppendOutcome::Committed(result) = outcome else {
        panic!("first append must commit")
    };
    assert_eq!(
        result
            .commit_positions
            .iter()
            .map(|position| position.get())
            .collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let events = store.events().unwrap();
    assert_eq!(events[0].sequence.get(), 1);
    assert_eq!(events[1].sequence.get(), 1);
    assert_eq!(events[2].sequence.get(), 2);
    assert_eq!(
        events
            .iter()
            .map(|stored| stored.event.clone())
            .collect::<Vec<_>>(),
        original_events
    );

    let before = events;
    let error = test_support::append(
        &mut store,
        command(
            scope(principal, "run.stale", first),
            "stale-version",
            digest(2),
            vec![(first, 0)],
            vec![event(first, 4)],
        ),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        StoreError::ExpectedVersion {
            expected: 0,
            actual: 2,
            ..
        }
    ));
    assert_eq!(store.events().unwrap(), before);
    assert_eq!(store.committed_through().unwrap(), 3);
}

#[test]
fn idempotency_replays_only_the_same_canonical_request_and_exposes_pending() {
    let database = TestDatabase::new("idempotency");
    let mut store = test_support::open_sqlite_store(database.path()).unwrap();
    let principal = PrincipalId::generate().unwrap();
    let stream = RunId::generate().unwrap();
    let original_scope = scope(principal, "run.append", stream);
    let original = command(
        original_scope.clone(),
        "same-command",
        digest(3),
        vec![(stream, 0)],
        vec![event(stream, 1)],
    );
    let replay = original.clone();
    let first = test_support::append(&mut store, original).unwrap();
    let second = test_support::append(&mut store, replay).unwrap();
    let (AppendOutcome::Committed(first), AppendOutcome::Replayed(second)) = (first, second) else {
        panic!("same request must replay its terminal result")
    };
    assert_eq!(first, second);
    assert_eq!(store.events().unwrap().len(), 1);

    let conflict = test_support::append(
        &mut store,
        command(
            original_scope.clone(),
            "same-command",
            digest(4),
            vec![(stream, 1)],
            vec![event(stream, 2)],
        ),
    )
    .unwrap_err();
    assert!(matches!(conflict, StoreError::IdempotencyConflict(_)));

    let pending_key = IdempotencyKey::parse("pending-command").unwrap();
    let pending_scope = scope(principal, "run.pending", stream);
    let ClaimOutcome::Claimed(_) = test_support::claim(
        &mut store,
        pending_scope.clone(),
        pending_key.clone(),
        digest(5),
    )
    .unwrap() else {
        panic!("new request must be claimed")
    };
    assert_eq!(
        test_support::claim(
            &mut store,
            pending_scope.clone(),
            pending_key.clone(),
            digest(5),
        )
        .unwrap(),
        ClaimOutcome::Pending
    );
    drop(store);

    let mut store = test_support::open_sqlite_store(database.path()).unwrap();
    let pending_append = command(
        pending_scope,
        "pending-command",
        digest(5),
        vec![(stream, 1)],
        vec![event(stream, 3)],
    );
    assert!(matches!(
        test_support::append(&mut store, pending_append),
        Ok(AppendOutcome::Committed(_))
    ));

    let other_scope = scope(PrincipalId::generate().unwrap(), "run.append", stream);
    assert!(matches!(
        test_support::append(
            &mut store,
            command(
                other_scope,
                "same-command",
                digest(4),
                vec![(stream, 2)],
                vec![event(stream, 4)],
            )
        ),
        Ok(AppendOutcome::Committed(_))
    ));
}

#[test]
fn sixteen_real_connections_allocate_one_gapless_committed_prefix() {
    concurrent_connections_allocate_one_gapless_committed_prefix(REPRESENTATIVE_CONNECTIONS);
}

#[test]
#[ignore = "exact opt-in SQLite WAL contention stress; run serially with --ignored --exact --test-threads=1"]
fn sixty_four_real_connections_allocate_one_gapless_committed_prefix() {
    concurrent_connections_allocate_one_gapless_committed_prefix(STRESS_CONNECTIONS);
}

fn concurrent_connections_allocate_one_gapless_committed_prefix(connections: usize) {
    let database = TestDatabase::new("concurrent");
    test_support::open_sqlite_store(database.path()).unwrap();
    let barrier = Arc::new(Barrier::new(connections));
    let path = Arc::new(database.path().to_owned());
    let handles: Vec<_> = (0..connections)
        .map(|index| {
            let barrier = Arc::clone(&barrier);
            let path = Arc::clone(&path);
            thread::spawn(move || {
                let stream = RunId::generate().unwrap();
                let principal = PrincipalId::generate().unwrap();
                let mut store = test_support::open_sqlite_store(path.as_path()).unwrap();
                let append = command(
                    scope(principal, "run.concurrent", stream),
                    &format!("concurrent-{index}"),
                    digest(index as u8),
                    vec![(stream, 0)],
                    vec![event(stream, index)],
                );
                barrier.wait();
                test_support::append(&mut store, append).unwrap()
            })
        })
        .collect();
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, AppendOutcome::Committed(_)))
            .count(),
        connections
    );

    let store = test_support::open_sqlite_store(database.path()).unwrap();
    let events = store.events().unwrap();
    let positions: Vec<_> = events
        .iter()
        .map(|event| event.commit_position.get())
        .collect();
    assert_eq!(positions, (1..=connections as u64).collect::<Vec<_>>());
    assert_eq!(
        positions.iter().copied().collect::<BTreeSet<_>>().len(),
        connections
    );
    assert_eq!(
        events
            .iter()
            .map(|event| event.event.id)
            .collect::<BTreeSet<_>>()
            .len(),
        connections
    );
    assert!(events.iter().all(|event| event.sequence.get() == 1));
    assert_eq!(store.committed_through().unwrap(), connections as u64);
}
