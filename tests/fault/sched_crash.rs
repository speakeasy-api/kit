use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use kit::test_support;
use kit::{
    domain::{
        ids::{AttemptId, PrincipalId, RunId},
        lifecycle::{AttemptOwnership, FencingToken},
    },
    runtime::scheduler::{
        AdmissionKind, DurableScheduler, ReservationRequest, SchedulerConfig, SchedulerError,
        budget::RunBudget,
        limits::Spend,
        reserve::{ReservationId, ReservationStatus},
    },
};

struct TestDb {
    root: PathBuf,
    path: PathBuf,
}

impl TestDb {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "kit-scheduler-crash-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("state.sqlite3");
        drop(test_support::open_service_store(&path).unwrap());
        Self { root, path }
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn open_scheduler(path: &PathBuf) -> DurableScheduler {
    DurableScheduler::open_with_config(
        path,
        SchedulerConfig {
            run_budget: RunBudget::new(100, 100, 100, 100, 100),
            ..SchedulerConfig::default()
        },
    )
    .unwrap()
}

fn request(
    id: u128,
    run_id: RunId,
    principal_id: PrincipalId,
    attempt: Option<AttemptOwnership>,
) -> ReservationRequest {
    ReservationRequest {
        id: ReservationId::new(id),
        run_id,
        principal_id,
        attempt,
        idempotency_key: format!("reservation-{id}"),
        kind: AdmissionKind::Tool,
        spend: Spend::new(11, 7, 0, 2, 0),
    }
}

#[test]
fn real_sqlite_restart_releases_only_safe_reservations_and_flags_dispatched_unknown() {
    let db = TestDb::new();
    let principal = PrincipalId::generate().unwrap();
    let runs = [(); 4].map(|_| RunId::generate().unwrap());
    let scheduler = open_scheduler(&db.path);
    for (index, run) in runs.into_iter().enumerate() {
        scheduler
            .register_run(run, principal, &format!("run-{index}"))
            .unwrap();
        scheduler.admit_run(run).unwrap();
        scheduler
            .reserve(&request(index as u128 + 1, run, principal, None))
            .unwrap();
    }
    scheduler.mark_dispatched(ReservationId::new(2)).unwrap();
    scheduler.debit(ReservationId::new(3)).unwrap();
    scheduler.cancel(ReservationId::new(4)).unwrap();
    drop(scheduler);

    let restarted = open_scheduler(&db.path);
    let report = restarted.reconcile_startup().unwrap();
    assert_eq!(report.released, 1);
    assert_eq!(report.dispatched_unknown, 1);
    assert_eq!(
        restarted.snapshot(ReservationId::new(1)).unwrap().status(),
        ReservationStatus::Released
    );
    assert_eq!(
        restarted.snapshot(ReservationId::new(2)).unwrap().status(),
        ReservationStatus::Reconciled
    );
    assert_eq!(
        restarted.snapshot(ReservationId::new(3)).unwrap().status(),
        ReservationStatus::Debited
    );
    assert_eq!(
        restarted.snapshot(ReservationId::new(4)).unwrap().status(),
        ReservationStatus::Released
    );
    assert_eq!(
        restarted.totals(runs[1]).unwrap().committed,
        Spend::new(11, 7, 0, 2, 0)
    );
    assert_eq!(
        restarted.totals(runs[2]).unwrap().committed,
        Spend::new(11, 7, 0, 2, 0)
    );

    drop(restarted);
    let second_restart = open_scheduler(&db.path);
    assert_eq!(
        second_restart.reconcile_startup().unwrap(),
        Default::default()
    );
    assert_eq!(
        second_restart
            .snapshot(ReservationId::new(2))
            .unwrap()
            .status(),
        ReservationStatus::Reconciled
    );
}

#[test]
fn restart_preserves_attempt_fence_and_prevents_double_debit() {
    let db = TestDb::new();
    let principal = PrincipalId::generate().unwrap();
    let run = RunId::generate().unwrap();
    let attempt = AttemptId::generate().unwrap();
    let owner = AttemptOwnership::new(attempt, principal, FencingToken::new(9));
    let scheduler = open_scheduler(&db.path);
    scheduler.register_run(run, principal, "run").unwrap();
    scheduler.admit_run(run).unwrap();
    let reservation = request(40, run, principal, Some(owner));
    scheduler.reserve(&reservation).unwrap();
    scheduler.mark_dispatched(reservation.id).unwrap();
    scheduler.debit(reservation.id).unwrap();
    drop(scheduler);

    let restarted = open_scheduler(&db.path);
    assert_eq!(
        restarted.debit(reservation.id).unwrap().status(),
        ReservationStatus::Debited
    );
    assert_eq!(restarted.totals(run).unwrap().committed, reservation.spend);
    let stale = request(
        41,
        run,
        principal,
        Some(AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal,
            FencingToken::new(8),
        )),
    );
    assert!(matches!(
        restarted.reserve(&stale),
        Err(SchedulerError::StaleFence)
    ));
    assert_eq!(restarted.totals(run).unwrap().committed, reservation.spend);
}
