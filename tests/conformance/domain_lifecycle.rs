use kit::domain::ids::{AttemptId, PrincipalId, RunId};
use kit::domain::lifecycle::{
    AttemptLifecycle, AttemptOwnership, AttemptState, AttemptTransitionCommand, FencingToken,
    RunLifecycle, RunState, RunTransitionCommand, RunTransitionError, StateVersion,
};

fn owner(fence: u64) -> AttemptOwnership {
    AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        PrincipalId::generate().unwrap(),
        FencingToken::new(fence),
    )
}

fn run_at(state: RunState) -> (RunLifecycle, AttemptOwnership) {
    let owner = owner(1);
    let run = RunLifecycle::new(RunId::generate().unwrap(), owner);
    let path: &[RunState] = match state {
        RunState::Queued => &[],
        RunState::AcquiringWorkspace => &[RunState::AcquiringWorkspace],
        RunState::Starting => &[RunState::AcquiringWorkspace, RunState::Starting],
        RunState::Running => &[
            RunState::AcquiringWorkspace,
            RunState::Starting,
            RunState::Running,
        ],
        RunState::WaitingForInput => &[
            RunState::AcquiringWorkspace,
            RunState::Starting,
            RunState::Running,
            RunState::WaitingForInput,
        ],
        RunState::WaitingForApproval => &[
            RunState::AcquiringWorkspace,
            RunState::Starting,
            RunState::Running,
            RunState::WaitingForApproval,
        ],
        RunState::WaitingForAuth => &[
            RunState::AcquiringWorkspace,
            RunState::Starting,
            RunState::Running,
            RunState::WaitingForAuth,
        ],
        RunState::Completed => &[
            RunState::AcquiringWorkspace,
            RunState::Starting,
            RunState::Running,
            RunState::Completed,
        ],
        RunState::Failed => &[
            RunState::AcquiringWorkspace,
            RunState::Starting,
            RunState::Running,
            RunState::Failed,
        ],
        RunState::Cancelling => &[RunState::Cancelling],
        RunState::Cancelled => &[RunState::Cancelling, RunState::Cancelled],
        RunState::Interrupted => &[
            RunState::AcquiringWorkspace,
            RunState::Starting,
            RunState::Running,
            RunState::Interrupted,
        ],
    };
    for target in path {
        let version = run.snapshot().version;
        run.transition(RunTransitionCommand::new(version, owner, *target))
            .unwrap();
    }
    (run, owner)
}

fn attempt_at(state: AttemptState) -> (AttemptLifecycle, AttemptOwnership) {
    let owner = owner(1);
    let attempt = AttemptLifecycle::new(RunId::generate().unwrap(), owner);
    let path: &[AttemptState] = match state {
        AttemptState::Leased => &[],
        AttemptState::Executing => &[AttemptState::Executing],
        AttemptState::Quiescing => &[AttemptState::Executing, AttemptState::Quiescing],
        AttemptState::Succeeded => &[
            AttemptState::Executing,
            AttemptState::Quiescing,
            AttemptState::Succeeded,
        ],
        AttemptState::Failed => &[
            AttemptState::Executing,
            AttemptState::Quiescing,
            AttemptState::Failed,
        ],
        AttemptState::Interrupted => &[
            AttemptState::Executing,
            AttemptState::Quiescing,
            AttemptState::Interrupted,
        ],
    };
    for target in path {
        let version = attempt.snapshot().version;
        attempt
            .transition(AttemptTransitionCommand::new(version, owner, *target))
            .unwrap();
    }
    (attempt, owner)
}

#[test]
fn run_transition_matrix_matches_the_rfc() {
    for from in RunState::ALL {
        for to in RunState::ALL {
            let expected = from.can_transition_to(to);
            let (run, current_owner) = run_at(from);
            let before = run.snapshot();
            let command = if from == RunState::Interrupted && to == RunState::Queued {
                RunTransitionCommand::retry(
                    before.version,
                    current_owner,
                    AttemptOwnership::new(
                        AttemptId::generate().unwrap(),
                        current_owner.principal_id,
                        FencingToken::new(2),
                    ),
                )
            } else {
                RunTransitionCommand::new(before.version, current_owner, to)
            };
            let result = run.transition(command);
            assert_eq!(
                result.is_ok(),
                expected,
                "unexpected {from:?} -> {to:?} result: {result:?}"
            );
            let after = run.snapshot();
            if expected {
                assert_eq!(after.version.get(), before.version.get() + 1);
                assert_eq!(after.state, to);
            } else {
                assert_eq!(after, before);
            }
        }
    }
}

#[test]
fn attempt_transition_matrix_matches_the_rfc() {
    for from in AttemptState::ALL {
        for to in AttemptState::ALL {
            let expected = from.can_transition_to(to);
            let (attempt, owner) = attempt_at(from);
            let before = attempt.snapshot();
            let result =
                attempt.transition(AttemptTransitionCommand::new(before.version, owner, to));
            assert_eq!(
                result.is_ok(),
                expected,
                "unexpected {from:?} -> {to:?} result: {result:?}"
            );
            let after = attempt.snapshot();
            if expected {
                assert_eq!(after.version.get(), before.version.get() + 1);
                assert_eq!(after.state, to);
            } else {
                assert_eq!(after, before);
            }
        }
    }
}

#[test]
fn waiting_runs_resume_and_terminal_states_are_monotonic() {
    for waiting in [
        RunState::WaitingForInput,
        RunState::WaitingForApproval,
        RunState::WaitingForAuth,
    ] {
        assert!(waiting.is_waiting());
        assert!(waiting.can_transition_to(RunState::Running));
        assert!(waiting.can_transition_to(RunState::Cancelling));
    }
    for terminal in [RunState::Completed, RunState::Failed, RunState::Cancelled] {
        assert!(terminal.is_terminal());
        assert!(
            RunState::ALL
                .into_iter()
                .all(|target| !terminal.can_transition_to(target))
        );
    }
    for terminal in [
        AttemptState::Succeeded,
        AttemptState::Failed,
        AttemptState::Interrupted,
    ] {
        assert!(terminal.is_terminal());
        assert!(
            AttemptState::ALL
                .into_iter()
                .all(|target| !terminal.can_transition_to(target))
        );
    }
}

#[test]
fn retry_replaces_attempt_and_advances_fence() {
    let (run, old_owner) = run_at(RunState::Interrupted);
    let before = run.snapshot();

    let same_attempt = AttemptOwnership::new(
        old_owner.attempt_id,
        old_owner.principal_id,
        FencingToken::new(2),
    );
    assert_eq!(
        run.transition(RunTransitionCommand::retry(
            before.version,
            old_owner,
            same_attempt,
        )),
        Err(RunTransitionError::RetryRequiresNewAttempt)
    );
    assert_eq!(run.snapshot(), before);

    let stale_fence = owner(1);
    assert_eq!(
        run.transition(RunTransitionCommand::retry(
            before.version,
            old_owner,
            stale_fence,
        )),
        Err(RunTransitionError::RetryFenceNotAdvanced)
    );
    assert_eq!(run.snapshot(), before);

    let replacement = AttemptOwnership::new(
        AttemptId::generate().unwrap(),
        old_owner.principal_id,
        FencingToken::new(2),
    );
    let transition = run
        .transition(RunTransitionCommand::retry(
            before.version,
            old_owner,
            replacement,
        ))
        .unwrap();
    assert_eq!(transition.owner, replacement);
    assert_eq!(
        transition.version,
        StateVersion::new(before.version.get() + 1)
    );
    assert_eq!(run.snapshot().owner, replacement);
}
