use agentkit_loop::ModelSession;

use crate::{
    agent::driver::attempt::{AttemptDriver, CommitError},
    api::service::AttemptProjection,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DispatchState {
    PreDispatch,
    Dispatched,
    OutcomeCommitted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancellationOutcome {
    Cancelled,
    OutcomeUnknown,
    AlreadyCommitted,
}

pub const fn cancellation_outcome(state: DispatchState) -> CancellationOutcome {
    match state {
        DispatchState::PreDispatch => CancellationOutcome::Cancelled,
        DispatchState::Dispatched => CancellationOutcome::OutcomeUnknown,
        DispatchState::OutcomeCommitted => CancellationOutcome::AlreadyCommitted,
    }
}

pub async fn commit_cancellation<S, T, E>(
    driver: &mut AttemptDriver<S>,
    projection: &AttemptProjection,
    state: DispatchState,
    append: impl FnOnce(CancellationOutcome) -> Result<T, E>,
) -> Result<T, CommitError<E>>
where
    S: ModelSession,
{
    let outcome = cancellation_outcome(state);
    driver
        .commit_cancellation(projection, || append(outcome))
        .await
}
