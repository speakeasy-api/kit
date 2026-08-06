#![cfg(debug_assertions)]

mod artifact_crash;
#[path = "../integration/backup_restore.rs"]
mod backup_restore;
mod broker_auth_interrupt;
#[path = "../fixtures/clock/mod.rs"]
mod clock;
#[path = "../fixtures/crashpoints/mod.rs"]
mod crashpoints;
mod edit_recovery;
mod exec_cancel;
mod fencing;
mod lifecycle_cas;
mod loop_restart;
mod lsp_fencing;
#[path = "../conformance/cap_invoke.rs"]
mod mcp_invocation_broker_crash;
mod mcp_listchange_storm;
mod model_intent_outcome;
mod process_reap;
mod provider_interrupt;
mod sched_crash;
mod store_append_crash;
#[path = "../fixtures/storefault/mod.rs"]
mod storefault;
mod tool_learning_crash;

#[test]
fn deterministic_crash_schedule_replays_named_occurrences() {
    use crashpoints::{
        ACP_CHILD_MID_TOOL_CALL, AFTER_WAL_COMMIT, BEFORE_PROJECTION_UPDATE, CrashAction,
        CrashSchedule, CrashTrigger, ISOLATION_BACKEND_UNAVAILABLE,
    };

    fn replay() -> Vec<Option<crashpoints::InjectedCrash>> {
        let mut schedule = CrashSchedule::new(
            0x51,
            vec![
                CrashTrigger {
                    name: AFTER_WAL_COMMIT.to_owned(),
                    occurrence: 2,
                    action: CrashAction::Terminate,
                },
                CrashTrigger {
                    name: BEFORE_PROJECTION_UPDATE.to_owned(),
                    occurrence: 1,
                    action: CrashAction::ReturnUnavailable,
                },
                CrashTrigger {
                    name: ISOLATION_BACKEND_UNAVAILABLE.to_owned(),
                    occurrence: 1,
                    action: CrashAction::ReturnUnavailable,
                },
                CrashTrigger {
                    name: ACP_CHILD_MID_TOOL_CALL.to_owned(),
                    occurrence: 1,
                    action: CrashAction::Disconnect,
                },
            ],
        );
        [
            AFTER_WAL_COMMIT,
            AFTER_WAL_COMMIT,
            AFTER_WAL_COMMIT,
            BEFORE_PROJECTION_UPDATE,
            ISOLATION_BACKEND_UNAVAILABLE,
            ACP_CHILD_MID_TOOL_CALL,
        ]
        .map(|point| schedule.hit(point))
        .into()
    }

    let first = replay();
    assert_eq!(first, replay());
    assert!(first[0].is_none());
    let wal_crash = first[1].as_ref().unwrap();
    assert_eq!(wal_crash.seed, 0x51);
    assert_eq!(wal_crash.name, AFTER_WAL_COMMIT);
    assert_eq!(wal_crash.occurrence, 2);
    assert_eq!(wal_crash.action, CrashAction::Terminate);
    assert_ne!(wal_crash.fingerprint, 0);
    assert!(first[2].is_none());
    assert_eq!(
        first[3].as_ref().unwrap().action,
        CrashAction::ReturnUnavailable
    );
    assert_eq!(
        first[4].as_ref().unwrap().name,
        ISOLATION_BACKEND_UNAVAILABLE
    );
    assert_eq!(first[5].as_ref().unwrap().action, CrashAction::Disconnect);
}

#[test]
fn deterministic_store_schedule_recovers_committed_wal() {
    use storefault::{
        AppendOutcome, ScheduledFault, StoreFault, StoreFaultSchedule, StoreHarness, StorePoint,
    };

    fn replay() -> (Vec<AppendOutcome>, Vec<String>, Vec<String>) {
        let mut store = StoreHarness::new(
            0x52,
            vec![
                ScheduledFault {
                    point: StorePoint::AfterWalCommit,
                    occurrence: 1,
                    fault: StoreFault::Crash,
                },
                ScheduledFault {
                    point: StorePoint::BeforeProjectionUpdate,
                    occurrence: 1,
                    fault: StoreFault::Crash,
                },
            ],
        );
        let outcomes = vec![store.append("event-a"), store.append("event-b")];
        assert!(store.projection.is_empty());
        store.recover_projection();
        (outcomes, store.wal, store.projection)
    }

    let first = replay();
    assert_eq!(first, replay());
    assert_eq!(
        first.0,
        vec![
            AppendOutcome::Faulted(StoreFault::Crash),
            AppendOutcome::Faulted(StoreFault::Crash),
        ]
    );
    assert_eq!(first.1, ["event-a", "event-b"]);
    assert_eq!(first.2, first.1);

    let mut clean = StoreHarness::new(0x54, vec![]);
    assert_eq!(clean.append("committed"), AppendOutcome::Committed);

    let mut before_commit = StoreHarness::new(
        0x55,
        vec![ScheduledFault {
            point: StorePoint::BeforeWalCommit,
            occurrence: 1,
            fault: StoreFault::Crash,
        }],
    );
    assert_eq!(
        before_commit.append("not-committed"),
        AppendOutcome::Faulted(StoreFault::Crash)
    );
    assert!(before_commit.wal.is_empty());

    let mut bytes = StoreFaultSchedule::new(
        0x56,
        vec![
            ScheduledFault {
                point: StorePoint::AfterUploadConfirm,
                occurrence: 1,
                fault: StoreFault::CorruptBytes,
            },
            ScheduledFault {
                point: StorePoint::BeforeHashVerification,
                occurrence: 1,
                fault: StoreFault::WithholdBytes,
            },
            ScheduledFault {
                point: StorePoint::BackupRead,
                occurrence: 1,
                fault: StoreFault::CorruptBytes,
            },
            ScheduledFault {
                point: StorePoint::CommitSerialization,
                occurrence: 1,
                fault: StoreFault::Partition,
            },
        ],
    );
    assert_eq!(bytes.seed, 0x56);
    let corruption = bytes.at(StorePoint::AfterUploadConfirm).unwrap();
    assert_ne!(
        bytes.apply_to_bytes(&corruption, b"artifact"),
        Some(b"artifact".to_vec())
    );
    let withheld = bytes.at(StorePoint::BeforeHashVerification).unwrap();
    assert_eq!(bytes.apply_to_bytes(&withheld, b"artifact"), None);
    assert_eq!(
        bytes.at(StorePoint::BackupRead),
        Some(StoreFault::CorruptBytes)
    );
    let partition = bytes.at(StorePoint::CommitSerialization).unwrap();
    assert_eq!(
        bytes.apply_to_bytes(&partition, b"event"),
        Some(b"event".to_vec())
    );
}

#[test]
fn deterministic_clock_schedule_expires_and_fences_stale_lease() {
    fn replay() -> (u64, u64, bool, bool) {
        let mut controller = clock::LeaseController::new(0x53);
        assert_eq!(controller.clock.seed, 0x53);
        let stale = controller.issue("node-a", 10);
        assert_eq!(stale.owner, "node-a");
        let renewed = controller.renew(&stale, 20).unwrap();
        assert_eq!(renewed.fence, stale.fence);
        controller.clock.advance_ms(21);
        let expired_commit = controller.can_commit(&stale);
        let current = controller.issue("node-b", 10);
        (
            stale.fence,
            current.fence,
            expired_commit,
            controller.can_commit(&current),
        )
    }

    let first = replay();
    assert_eq!(first, replay());
    assert!(first.1 > first.0);
    assert!(!first.2);
    assert!(first.3);
}
