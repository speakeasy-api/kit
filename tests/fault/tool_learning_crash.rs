use kit::{
    api::service::AttemptDriverClaim,
    domain::{
        events::{TraceId, UtcDateTime},
        ids::{AttemptId, EventId, PrincipalId, ProjectId, RunId},
        lifecycle::{AttemptOwnership, FencingToken},
    },
    store::sqlite::append::CrashPoint,
    telemetry::tool_learning::{
        self, LearningCommon, LearningOperation, LearningSurface, PointerDomain,
        ProjectPointerHasher, ToolLearningEvent,
    },
    test_support,
};

const CRASH_POINTS: [CrashPoint; 10] = [
    CrashPoint::AfterTransactionBegin,
    CrashPoint::AfterIdempotencyCheck,
    CrashPoint::AfterExpectedVersionCheck,
    CrashPoint::AfterEventInsert,
    CrashPoint::AfterStreamHeadsUpdate,
    CrashPoint::AfterWatermarkUpdate,
    CrashPoint::BeforeIdempotencyTerminal,
    CrashPoint::AfterIdempotencyTerminal,
    CrashPoint::BeforeCommit,
    CrashPoint::AfterCommit,
];

#[test]
fn every_learning_append_crash_boundary_recovers_without_duplication() {
    for point in CRASH_POINTS {
        let root = std::env::temp_dir().join(format!(
            "kit-tool-learning-crash-{point:?}-{}",
            EventId::generate().unwrap()
        ));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("events.sqlite3");
        let principal = PrincipalId::generate().unwrap();
        let project = ProjectId::generate().unwrap();
        let run = RunId::generate().unwrap();
        let owner = AttemptOwnership::new(
            AttemptId::generate().unwrap(),
            principal,
            FencingToken::new(1),
        );
        let mut store = test_support::open_sqlite_store(&database).unwrap();
        let claim = store
            .install_driver_claim_for_test(AttemptDriverClaim {
                run_id: run,
                attempt_id: owner.attempt_id,
                principal_id: principal,
                fence: owner.fencing_token,
                lease_version: 1,
                expires_at_unix_micros: 0,
            })
            .unwrap();
        let hasher = ProjectPointerHasher::new(project, &[12; 32]);
        let event = ToolLearningEvent::Opportunity {
            common: LearningCommon::new(
                &hasher,
                run,
                1,
                LearningOperation::Projection,
                LearningSurface::Discovery,
                b"crash-opportunity",
                None,
                None,
                None,
            ),
            offered: 4,
            eager: 4,
            deferred: 0,
            generic_available: true,
            projection: hasher.pointer(PointerDomain::Schema, b"projection"),
            candidates: Vec::new(),
            detail_artifact: None,
        };
        let mut injected = false;
        let result = tool_learning::append_many_with_hook(
            &mut store,
            owner,
            claim,
            &hasher,
            UtcDateTime::parse("2026-08-05T12:00:00Z").unwrap(),
            TraceId::parse("learning-crash").unwrap(),
            std::slice::from_ref(&event),
            |candidate| {
                if !injected && candidate == point {
                    injected = true;
                    true
                } else {
                    false
                }
            },
        );
        assert!(result.is_err(), "{point:?} did not inject");
        drop(store);

        let mut store = test_support::open_sqlite_store(&database).unwrap();
        assert!(tool_learning::records(&store, run, &hasher).unwrap().len() <= 1);
        tool_learning::append(
            &mut store,
            owner,
            claim,
            &hasher,
            UtcDateTime::parse("2026-08-05T12:00:01Z").unwrap(),
            TraceId::parse("learning-crash").unwrap(),
            &event,
        )
        .unwrap();
        assert_eq!(
            tool_learning::records(&store, run, &hasher).unwrap().len(),
            1
        );
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
