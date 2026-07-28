use std::sync::{Arc, Barrier};
use std::thread;

use kit::domain::ids::{AttemptId, PrincipalId, RunId};
use kit::domain::lifecycle::{
    AttemptOwnership, CasConflict, FencingToken, RunLifecycle, RunState, RunTransitionCommand,
    RunTransitionError, StateVersion,
};

fn owner(fence: u64) -> AttemptOwnership {
    AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        PrincipalId::generate().unwrap(),
        FencingToken::new(fence),
    )
}

#[test]
fn one_of_more_than_one_hundred_identical_cas_commands_wins() {
    const RACERS: usize = 128;
    let owner = owner(7);
    let run = Arc::new(RunLifecycle::new(RunId::generate().unwrap(), owner));
    let barrier = Arc::new(Barrier::new(RACERS));
    let threads: Vec<_> = (0..RACERS)
        .map(|_| {
            let run = Arc::clone(&run);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                run.transition(RunTransitionCommand::new(
                    StateVersion::INITIAL,
                    owner,
                    RunState::AcquiringWorkspace,
                ))
            })
        })
        .collect();

    let results: Vec<_> = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(RunTransitionError::Conflict(CasConflict::Version { .. }))
            ))
            .count(),
        RACERS - 1
    );
    assert_eq!(run.snapshot().version, StateVersion::new(1));
    assert_eq!(run.snapshot().state, RunState::AcquiringWorkspace);
}

#[test]
fn stale_version_and_fence_are_typed_conflicts_without_mutation() {
    let owner = owner(11);
    let run = RunLifecycle::new(RunId::generate().unwrap(), owner);

    let before = run.snapshot();
    assert_eq!(
        run.transition(RunTransitionCommand::new(
            StateVersion::new(99),
            owner,
            RunState::AcquiringWorkspace,
        )),
        Err(RunTransitionError::Conflict(CasConflict::Version {
            expected: StateVersion::new(99),
            actual: StateVersion::INITIAL,
        }))
    );
    assert_eq!(run.snapshot(), before);

    let stale_owner =
        AttemptOwnership::new(owner.attempt_id, owner.principal_id, FencingToken::new(10));
    assert_eq!(
        run.transition(RunTransitionCommand::new(
            StateVersion::INITIAL,
            stale_owner,
            RunState::AcquiringWorkspace,
        )),
        Err(RunTransitionError::Conflict(CasConflict::Fence {
            expected: FencingToken::new(10),
            actual: FencingToken::new(11),
        }))
    );
    assert_eq!(run.snapshot(), before);
}

#[test]
fn stale_owner_cannot_commit_after_retry_reassigns_the_run() {
    let old_owner = owner(20);
    let run = RunLifecycle::new(RunId::generate().unwrap(), old_owner);
    for target in [
        RunState::AcquiringWorkspace,
        RunState::Starting,
        RunState::Running,
        RunState::Interrupted,
    ] {
        run.transition(RunTransitionCommand::new(
            run.snapshot().version,
            old_owner,
            target,
        ))
        .unwrap();
    }
    let interrupted = run.snapshot();
    let current_owner = AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        old_owner.principal_id,
        FencingToken::new(21),
    );
    run.transition(RunTransitionCommand::retry(
        interrupted.version,
        old_owner,
        current_owner,
    ))
    .unwrap();
    let reassigned = run.snapshot();

    for _ in 0..100 {
        assert!(matches!(
            run.transition(RunTransitionCommand::new(
                reassigned.version,
                old_owner,
                RunState::AcquiringWorkspace,
            )),
            Err(RunTransitionError::Conflict(CasConflict::Fence { .. }))
        ));
        assert_eq!(run.snapshot(), reassigned);
    }
}
