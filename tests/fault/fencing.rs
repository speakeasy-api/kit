use std::path::{Path, PathBuf};
use std::time::Duration;

use kit::test_support::open_lease_runtime;
use kit::{
    agent::driver::restart::{
        BoundarySnapshot, EffectJournal, EffectJournalAppend, LoopRecord, SafeBoundary,
    },
    api::service::AttemptDriverClaim,
    domain::{
        events::{TraceId, UtcDateTime},
        ids::{ArtifactId, AttemptId, CommandId, DaemonServiceId, EventId, PrincipalId, RunId},
        lifecycle::{AttemptOwnership, FencingToken},
    },
    runtime::lease::{
        LeaseError, LeaseKey, LeaseLoss, LeaseOwner, ReconciliationAction, ShutdownPhase,
        StateRootLockError,
    },
    store::sqlite::{append::StoreError, idempotency::IdempotencyKey},
};

struct StateRoot(PathBuf);

impl StateRoot {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "kit-lease-{label}-{}-{}",
            std::process::id(),
            AttemptId::generate().unwrap()
        ));
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for StateRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn ten_thousand_stale_fence_commits_are_rejected() {
    let root = StateRoot::new("stale");
    let mut runtime = open_lease_runtime(root.path()).unwrap();
    let key = LeaseKey::new("run:stale-fence").unwrap();
    let stale = runtime
        .leases()
        .acquire(
            key.clone(),
            LeaseOwner::Attempt(AttemptId::generate().unwrap()),
            Duration::from_secs(60),
        )
        .unwrap();
    let renewed = runtime
        .leases()
        .renew(&stale, Duration::from_secs(60))
        .unwrap();
    assert_eq!(renewed.fence, stale.fence);
    runtime.leases().release(&renewed).unwrap();
    let current = runtime
        .leases()
        .acquire(
            key,
            LeaseOwner::Attempt(AttemptId::generate().unwrap()),
            Duration::from_secs(60),
        )
        .unwrap();
    assert!(current.fence > stale.fence);

    let mut accepted = 0;
    for _ in 0..10_000 {
        let result = runtime.leases().guarded_commit(&stale, |_| {
            accepted += 1;
            Ok(())
        });
        assert!(matches!(
            result,
            Err(LeaseError::LeaseLost {
                reason: LeaseLoss::OwnerChanged | LeaseLoss::FenceChanged,
                ..
            })
        ));
    }
    assert_eq!(accepted, 0);
}

#[test]
fn ten_thousand_stale_journal_appends_commit_zero_events() {
    let root = StateRoot::new("stale-journal");
    let runtime = open_lease_runtime(root.path()).unwrap();
    let database = root.path().join("state.sqlite3");
    let run_id = RunId::generate().unwrap();
    let principal_id = PrincipalId::generate().unwrap();
    let old_owner = AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        principal_id,
        FencingToken::new(1),
    );
    let old = AttemptDriverClaim {
        run_id,
        attempt_id: old_owner.attempt_id,
        principal_id,
        fence: old_owner.fencing_token,
        lease_version: 1,
        expires_at_unix_micros: 0,
    };
    let mut store = kit::test_support::open_sqlite_store(&database).unwrap();
    store.install_driver_claim_for_test(old).unwrap();
    store
        .install_driver_claim_for_test(AttemptDriverClaim {
            attempt_id: AttemptId::generate().unwrap(),
            fence: FencingToken::new(2),
            lease_version: 2,
            ..old
        })
        .unwrap();
    let before = store.events().unwrap().len();
    let record = LoopRecord::Boundary(BoundarySnapshot {
        boundary: SafeBoundary::TurnEnd,
        transcript: Vec::new(),
        resume_index: None,
        model_outcome: None,
    });
    let key = IdempotencyKey::parse("stale-journal").unwrap();
    let command_id = CommandId::generate().unwrap();
    let event_id = EventId::generate().unwrap();
    let occurred_at = UtcDateTime::parse("2026-07-23T00:00:00Z").unwrap();
    let trace_id = TraceId::parse("stale-journal").unwrap();
    for _ in 0..10_000 {
        assert!(matches!(
            store.append_effect(EffectJournalAppend {
                owner: old_owner,
                claim: Some(old),
                idempotency_key: key.clone(),
                command_id,
                event_id,
                occurred_at: occurred_at.clone(),
                trace_id: trace_id.clone(),
                artifacts: Vec::new(),
                record: record.clone(),
            }),
            Err(StoreError::StaleDriverClaim | StoreError::StaleDriverClaimDetail(_))
        ));
    }
    assert_eq!(store.events().unwrap().len(), before);
    runtime.shutdown().unwrap();
}

#[test]
fn second_daemon_gets_typed_lock_error_one_hundred_times() {
    let root = StateRoot::new("dual-lock");
    let first = open_lease_runtime(root.path()).unwrap();
    for _ in 0..100 {
        assert!(matches!(
            open_lease_runtime(root.path()),
            Err(LeaseError::StateRoot(
                StateRootLockError::AlreadyLocked { .. }
            ))
        ));
    }
    first.shutdown().unwrap();
    open_lease_runtime(root.path()).unwrap().shutdown().unwrap();
}

#[test]
fn startup_reconciliation_only_returns_explicit_safe_actions() {
    let root = StateRoot::new("reconcile");
    let mut runtime = open_lease_runtime(root.path()).unwrap();
    let lease = runtime
        .leases()
        .acquire(
            LeaseKey::new("attempt:reconcile").unwrap(),
            LeaseOwner::Attempt(AttemptId::generate().unwrap()),
            Duration::from_secs(1),
        )
        .unwrap();
    runtime
        .leases()
        .record_intent(&lease, "safe-intent", true, true)
        .unwrap();
    runtime
        .leases()
        .record_intent(&lease, "ambiguous-intent", false, false)
        .unwrap();
    let artifact = ArtifactId::generate().unwrap();
    runtime
        .leases()
        .record_prepared_artifact(&lease, artifact)
        .unwrap();
    std::thread::sleep(Duration::from_millis(1_100));

    let actions = runtime.leases().reconcile_startup().unwrap();
    assert!(actions.iter().any(|action| matches!(
        action,
        ReconciliationAction::ExpiredLease { owner, .. } if *owner == lease.owner
    )));
    assert!(actions.iter().any(|action| matches!(
        action,
        ReconciliationAction::RetryIntent { intent_id, .. } if intent_id == "safe-intent"
    )));
    assert!(actions.iter().any(|action| matches!(
        action,
        ReconciliationAction::OutcomeUnknown { intent_id, .. } if intent_id == "ambiguous-intent"
    )));
    assert!(actions.iter().any(|action| matches!(
        action,
        ReconciliationAction::InspectPreparedArtifact { artifact_id, .. } if *artifact_id == artifact
    )));
}

#[test]
fn graceful_shutdown_closes_admission_before_flush_and_unlock() {
    let root = StateRoot::new("shutdown");
    let runtime = open_lease_runtime(root.path()).unwrap();
    let effect = runtime.admit_local_effect().unwrap();
    runtime.begin_shutdown();
    assert_eq!(runtime.phase(), ShutdownPhase::Quiescing);
    assert!(matches!(
        runtime.admit_local_effect(),
        Err(LeaseError::AdmissionClosed)
    ));
    drop(effect);
    runtime.shutdown().unwrap();

    let mut next = open_lease_runtime(root.path()).unwrap();
    let service = LeaseOwner::Service(DaemonServiceId::generate().unwrap());
    next.leases()
        .acquire(
            LeaseKey::new("service:after-shutdown").unwrap(),
            service,
            Duration::from_secs(1),
        )
        .unwrap();
    next.shutdown().unwrap();
}
