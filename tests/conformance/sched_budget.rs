use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use kit::test_support;
use kit::{
    domain::{
        config::{ConfigLayer, LayerStack, RunConfigContext},
        ids::{AttemptId, PrincipalId, ProjectId, RunId},
        lifecycle::{AttemptOwnership, FencingToken},
    },
    runtime::scheduler::{
        AdmissionKind, AnchoredConsumptionReceipt, AnchoredConsumptionVerifier, DurableScheduler,
        PendingStatisticalTrial, ReservationRequest, SchedulerConfig, SchedulerError,
        TrialAdmissionToken, TrialAdmissionVerifier, TrialRunBinding,
        budget::RunBudget,
        limits::Spend,
        reserve::{ReservationId, ReservationStatus},
    },
};
use std::collections::BTreeSet;

struct TestDb {
    root: PathBuf,
    path: PathBuf,
}

#[test]
fn statistical_trial_admission_is_verified_and_consumed_once_before_run_admission() {
    struct Verifier;
    impl TrialAdmissionVerifier for Verifier {
        fn verify(&self, token: &TrialAdmissionToken) -> bool {
            token.authentication == "authority-signature" && token.token_digest == "token-digest"
        }
    }
    impl AnchoredConsumptionVerifier for Verifier {
        fn verify(
            &self,
            pending: &PendingStatisticalTrial,
            receipt: &AnchoredConsumptionReceipt,
        ) -> bool {
            receipt.scheduler_run_id == pending.run_id.to_string()
                && receipt.authentication_tag == "authority-receipt"
                && receipt.anchor_identity == "authority-1"
        }
    }

    let db = TestDb::new();
    let principal = PrincipalId::generate().unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        principal,
        FencingToken::new(1),
    );
    let scheduler = DurableScheduler::open(&db.path).unwrap();
    let first = RunId::generate().unwrap();
    scheduler
        .register_run(first, principal, "statistical-first")
        .unwrap();
    let admission = TrialAdmissionToken {
        authority_id: "authority-1".to_owned(),
        authority_position: 2,
        registration_sequence: 1,
        preregistration_digest: "plan-digest".to_owned(),
        schedule_index: 0,
        trial_id: "trial-1".to_owned(),
        pair_id: "pair-1".to_owned(),
        task_id: "task-1".to_owned(),
        dataset_member_id: "member-1".to_owned(),
        task_manifest_digest: "task-manifest-digest".to_owned(),
        seed: 7,
        arm: "baseline".to_owned(),
        nonce: "nonce-1".to_owned(),
        token_digest: "token-digest".to_owned(),
        authentication: "authority-signature".to_owned(),
    };
    let binding = TrialRunBinding {
        trial_id: admission.trial_id.clone(),
        trial_digest: "trial-digest".to_owned(),
        task_digest: "task-digest".to_owned(),
        model_digest: "model-digest".to_owned(),
        config_digest: "legacy".to_owned(),
        attempt: owner,
        admission: Some(admission),
    };
    assert!(matches!(
        scheduler.admit_trial_run(first, &binding),
        Err(SchedulerError::Conflict)
    ));
    let pending = scheduler
        .admit_statistical_trial_run(first, &binding, &Verifier)
        .unwrap();
    let receipt = AnchoredConsumptionReceipt {
        authority_id: "authority-1".to_owned(),
        scheduler_run_id: first.to_string(),
        admission_token_digest: pending.admission_token_digest.clone(),
        admission_nonce: pending.admission_nonce.clone(),
        scheduler_consumption_position: pending.consumption_position,
        scheduler_consumption_digest: pending.consumption_digest.clone(),
        ledger_position: 3,
        ledger_head_digest: format!("sha256:{}", "1".repeat(64)),
        anchor_source: "production-anchor".to_owned(),
        anchor_identity: "authority-1".to_owned(),
        anchor_counter: 3,
        anchor_signature: "anchor-signature".to_owned(),
        authentication_algorithm: "hmac-sha256".to_owned(),
        authentication_key_id: "authority-key".to_owned(),
        authentication_tag: "authority-receipt".to_owned(),
    };
    let mut forged = receipt.clone();
    forged.scheduler_consumption_digest = format!("sha256:{}", "2".repeat(64));
    assert!(matches!(
        scheduler.finalize_statistical_trial_anchor(&pending, &forged, &Verifier),
        Err(SchedulerError::Conflict)
    ));
    scheduler
        .finalize_statistical_trial_anchor(&pending, &receipt, &Verifier)
        .unwrap();
    scheduler
        .finalize_statistical_trial_anchor(&pending, &receipt, &Verifier)
        .unwrap();
    scheduler.rollback_run_admission(first).unwrap();

    let second = RunId::generate().unwrap();
    scheduler
        .register_run(second, principal, "statistical-second")
        .unwrap();
    assert!(matches!(
        scheduler.admit_statistical_trial_run(second, &binding, &Verifier),
        Err(SchedulerError::Conflict)
    ));
}

impl TestDb {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "kit-durable-scheduler-{}-{nonce}",
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

fn request(id: u128, run_id: RunId, principal_id: PrincipalId, spend: Spend) -> ReservationRequest {
    ReservationRequest {
        id: ReservationId::new(id),
        run_id,
        principal_id,
        attempt: None,
        idempotency_key: format!("reservation-{id}"),
        kind: AdmissionKind::Run,
        spend,
    }
}

#[test]
fn trial_binding_is_admitted_once_and_terminal_runs_reject_late_effects() {
    let db = TestDb::new();
    let run = RunId::generate().unwrap();
    let principal = PrincipalId::generate().unwrap();
    let owner = AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        principal,
        FencingToken::new(1),
    );
    let scheduler = DurableScheduler::open(&db.path).unwrap();
    scheduler.register_run(run, principal, "trial-run").unwrap();
    let binding = TrialRunBinding {
        trial_id: "trial-1".to_owned(),
        trial_digest: "trial-digest".to_owned(),
        task_digest: "task-digest".to_owned(),
        model_digest: "model-digest".to_owned(),
        config_digest: "legacy".to_owned(),
        attempt: owner,
        admission: None,
    };
    scheduler.admit_trial_run(run, &binding).unwrap();
    scheduler.admit_trial_run(run, &binding).unwrap();
    let mut substituted = binding.clone();
    substituted.model_digest = "other-model".to_owned();
    assert!(matches!(
        scheduler.admit_trial_run(run, &substituted),
        Err(SchedulerError::Conflict)
    ));

    let reservation = ReservationRequest {
        id: ReservationId::new(99),
        run_id: run,
        principal_id: principal,
        attempt: Some(owner),
        idempotency_key: "trial-model".to_owned(),
        kind: AdmissionKind::Model,
        spend: Spend::new(1, 1, 1, 0, 0),
    };
    scheduler.reserve(&reservation).unwrap();
    scheduler.release(reservation.id).unwrap();
    scheduler.finish_run(run, false).unwrap();
    scheduler.finish_run(run, false).unwrap();
    assert!(matches!(
        scheduler.reserve(&ReservationRequest {
            id: ReservationId::new(100),
            idempotency_key: "late-model".to_owned(),
            ..reservation
        }),
        Err(SchedulerError::InvalidTransition)
    ));
    assert!(
        rusqlite::Connection::open(&db.path)
            .unwrap()
            .execute(
                "UPDATE run_to_trial SET model_digest = 'tampered' WHERE run_id = ?1",
                [run.to_string()],
            )
            .is_err()
    );
}

#[test]
fn scheduler_persists_the_exact_effective_snapshot_budget() {
    let db = TestDb::new();
    let principal = PrincipalId::generate().unwrap();
    let project = ProjectId::generate().unwrap();
    let run = RunId::generate().unwrap();
    let mut built_in = ConfigLayer::safe_defaults();
    built_in.budgets.max_cost_microusd = Some(123);
    built_in.budgets.max_tokens = Some(456);
    built_in.budgets.max_turns = Some(7);
    built_in.concurrency.max_tools = Some(8);
    built_in.concurrency.max_runs = Some(9);
    let snapshot = LayerStack {
        built_in,
        user: None,
        project: None,
        run: None,
        experiment: None,
    }
    .materialize(
        RunConfigContext {
            principal_id: principal,
            project_id: project,
            run_id: run,
        },
        &BTreeSet::from([
            kit::domain::config::Grant::ModelCall,
            kit::domain::config::Grant::WorkspaceRead,
            kit::domain::config::Grant::WorkspaceWrite,
            kit::domain::config::Grant::ProcessSpawn,
            kit::domain::config::Grant::NetworkEgress,
        ]),
    )
    .unwrap();
    let scheduler = DurableScheduler::open(&db.path).unwrap();
    scheduler
        .register_run_with_snapshot(run, principal, "snapshot", &snapshot)
        .unwrap();
    assert_eq!(
        scheduler.run_budget(run).unwrap().limits(),
        RunBudget::from_effective_config(snapshot.effective()).limits()
    );
    drop(scheduler);
    let restarted = DurableScheduler::open(&db.path).unwrap();
    assert_eq!(
        restarted.run_budget(run).unwrap().limits(),
        RunBudget::from_effective_config(snapshot.effective()).limits()
    );
}

#[test]
fn ten_thousand_adversarial_overspend_attempts_have_zero_successes() {
    const ATTEMPTS: usize = 10_000;
    const WORKERS: usize = 32;
    let db = TestDb::new();
    let principal = PrincipalId::generate().unwrap();
    let run = RunId::generate().unwrap();
    let scheduler = Arc::new(
        DurableScheduler::open_with_config(
            &db.path,
            SchedulerConfig {
                run_budget: RunBudget::new(1, 1, 1, 1, 1),
                ..SchedulerConfig::default()
            },
        )
        .unwrap(),
    );
    scheduler.register_run(run, principal, "run").unwrap();
    scheduler.admit_run(run).unwrap();
    let full = Spend::new(1, 1, 1, 1, 1);
    scheduler
        .reserve(&request(1, run, principal, full))
        .unwrap();
    scheduler.debit(ReservationId::new(1)).unwrap();

    let barrier = Arc::new(Barrier::new(WORKERS));
    let successes = Arc::new(AtomicUsize::new(0));
    let handles = (0..WORKERS)
        .map(|worker| {
            let scheduler = Arc::clone(&scheduler);
            let barrier = Arc::clone(&barrier);
            let successes = Arc::clone(&successes);
            thread::spawn(move || {
                barrier.wait();
                for attempt in (worker..ATTEMPTS).step_by(WORKERS) {
                    if scheduler
                        .reserve(&request((attempt + 2) as u128, run, principal, full))
                        .is_ok()
                    {
                        successes.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(successes.load(Ordering::Relaxed), 0);
    let totals = scheduler.totals(run).unwrap();
    assert_eq!(totals.committed, full);
    assert_eq!(totals.reserved, Spend::ZERO);
}

#[test]
fn durable_reservations_are_atomic_idempotent_and_use_the_persisted_budget() {
    let db = TestDb::new();
    let principal = PrincipalId::generate().unwrap();
    let run = RunId::generate().unwrap();
    let scheduler = DurableScheduler::open_with_config(
        &db.path,
        SchedulerConfig {
            run_budget: RunBudget::new(20, 10, 2, 3, 4),
            ..SchedulerConfig::default()
        },
    )
    .unwrap();
    scheduler.register_run(run, principal, "run").unwrap();
    let spend = Spend::new(20, 10, 1, 1, 1);
    let reservation = request(7, run, principal, spend);
    let reserved = scheduler.reserve(&reservation).unwrap();
    assert_eq!(scheduler.reserve(&reservation).unwrap(), reserved);
    assert_eq!(reserved.status(), ReservationStatus::Reserved);
    let mut conflict = reservation.clone();
    conflict.spend = Spend::new(1, 0, 0, 0, 0);
    assert!(matches!(
        scheduler.reserve(&conflict),
        Err(SchedulerError::Conflict)
    ));
    assert_eq!(
        scheduler.debit(reservation.id).unwrap().status(),
        ReservationStatus::Debited
    );
    drop(scheduler);

    let restarted = DurableScheduler::open_with_config(
        &db.path,
        SchedulerConfig {
            run_budget: RunBudget::new(100, 100, 100, 100, 100),
            ..SchedulerConfig::default()
        },
    )
    .unwrap();
    assert_eq!(
        restarted.snapshot(reservation.id).unwrap().status(),
        ReservationStatus::Debited
    );
    assert!(matches!(
        restarted.reserve(&request(8, run, principal, Spend::new(1, 0, 0, 0, 0))),
        Err(SchedulerError::Exhausted(exhaustion))
            if exhaustion.maximum == 20 && exhaustion.committed == 20
    ));
}

#[test]
fn bounded_fifo_and_durable_principal_global_caps_gate_run_admission() {
    let db = TestDb::new();
    let principal = PrincipalId::generate().unwrap();
    let first = RunId::generate().unwrap();
    let second = RunId::generate().unwrap();
    let third = RunId::generate().unwrap();
    let scheduler = DurableScheduler::open_with_config(
        &db.path,
        SchedulerConfig {
            queue_capacity: 2,
            global_concurrency: 1,
            principal_concurrency: 1,
            ..SchedulerConfig::default()
        },
    )
    .unwrap();
    scheduler.register_run(first, principal, "first").unwrap();
    scheduler.register_run(second, principal, "second").unwrap();
    assert!(matches!(
        scheduler.register_run(third, principal, "third"),
        Err(SchedulerError::QueueFull { capacity: 2 })
    ));
    assert!(matches!(
        scheduler.admit_run(second),
        Err(SchedulerError::NotQueueHead)
    ));
    scheduler.admit_run(first).unwrap();
    assert!(matches!(
        scheduler.admit_run(second),
        Err(SchedulerError::GlobalCap { limit: 1 })
    ));
    scheduler.finish_run(first, false).unwrap();
    scheduler.admit_run(second).unwrap();
}
