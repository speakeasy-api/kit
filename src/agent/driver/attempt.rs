use std::fmt;

use crate::{
    api::service::{AttemptDriverClaim, AttemptProjection},
    domain::lifecycle::{AttemptOwnership, AttemptState},
    store::sqlite::append::SqliteStore,
};
use agentkit_core::{CancellationController, ToolCallId};
use agentkit_loop::{LoopDriver, LoopError, LoopInterrupt, LoopStep, ModelSession};
use agentkit_tools_core::ApprovalDecision;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptDriverError {
    AttemptMismatch,
    OwnerMismatch,
    StaleProjection,
    InactiveAttempt(AttemptState),
    ActiveAttempt(AttemptState),
    StaleOwner,
}

impl fmt::Display for AttemptDriverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AttemptMismatch => f.write_str("attempt projection does not match the driver"),
            Self::OwnerMismatch => f.write_str("attempt projection owner does not match"),
            Self::StaleProjection => f.write_str("attempt projection is older than the live owner"),
            Self::InactiveAttempt(state) => {
                write!(f, "attempt in state {state:?} cannot own a loop driver")
            }
            Self::ActiveAttempt(state) => {
                write!(
                    f,
                    "attempt in state {state:?} has not durably revoked its claim"
                )
            }
            Self::StaleOwner => f.write_str("loop driver owner token is stale"),
        }
    }
}

impl std::error::Error for AttemptDriverError {}

#[derive(Debug)]
pub enum PollError {
    Ownership(AttemptDriverError),
    Loop(LoopError),
}

impl fmt::Display for PollError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ownership(error) => error.fmt(f),
            Self::Loop(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for PollError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ownership(error) => Some(error),
            Self::Loop(error) => Some(error),
        }
    }
}

#[derive(Debug)]
pub enum CommitError<E> {
    Ownership(AttemptDriverError),
    Commit(E),
}

impl<E: fmt::Display> fmt::Display for CommitError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ownership(error) => error.fmt(f),
            Self::Commit(error) => error.fmt(f),
        }
    }
}

impl<E> std::error::Error for CommitError<E>
where
    E: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Ownership(error) => Some(error),
            Self::Commit(error) => Some(error),
        }
    }
}

/// The sole polling and commit authority for one durable attempt owner.
///
/// Claims are serialized with polls and commits. A live durable attempt/fence
/// may be claimed once; replacement requires a newly leased attempt owner.
pub struct AttemptDriver<S>
where
    S: ModelSession,
{
    driver: LoopDriver<S>,
    cancellation: CancellationController,
    owner: AttemptOwnership,
    claim: AttemptDriverClaim,
    store: Option<SqliteStore>,
}

impl<S> AttemptDriver<S>
where
    S: ModelSession,
{
    pub async fn claim(
        projection: &AttemptProjection,
        claim: AttemptDriverClaim,
        mut store: SqliteStore,
        driver: LoopDriver<S>,
        cancellation: CancellationController,
    ) -> Result<Self, AttemptDriverError> {
        if !matches!(
            projection.state,
            AttemptState::Leased | AttemptState::Executing
        ) {
            return Err(AttemptDriverError::InactiveAttempt(projection.state));
        }
        if projection.id != projection.owner.attempt_id {
            return Err(AttemptDriverError::AttemptMismatch);
        }

        if claim.run_id != projection.run_id || claim.owner() != projection.owner {
            return Err(AttemptDriverError::OwnerMismatch);
        }
        store
            .verify_driver_claim(claim)
            .map_err(|_| AttemptDriverError::StaleOwner)?;

        Ok(Self {
            driver,
            cancellation,
            owner: projection.owner,
            claim,
            store: Some(store),
        })
    }

    #[cfg(debug_assertions)]
    pub(crate) fn unclaimed_for_test(
        projection: &AttemptProjection,
        driver: LoopDriver<S>,
        cancellation: CancellationController,
    ) -> Result<Self, AttemptDriverError> {
        if !matches!(
            projection.state,
            AttemptState::Leased | AttemptState::Executing
        ) {
            return Err(AttemptDriverError::InactiveAttempt(projection.state));
        }
        Ok(Self {
            driver,
            cancellation,
            owner: projection.owner,
            claim: AttemptDriverClaim {
                run_id: projection.run_id,
                attempt_id: projection.id,
                principal_id: projection.owner.principal_id,
                fence: projection.owner.fencing_token,
                lease_version: 0,
                expires_at_unix_micros: 0,
            },
            store: None,
        })
    }

    pub const fn owner(&self) -> AttemptOwnership {
        self.owner
    }

    pub const fn claim_token(&self) -> AttemptDriverClaim {
        self.claim
    }

    pub fn cancellation(&self) -> &CancellationController {
        &self.cancellation
    }

    pub async fn poll(&mut self, projection: &AttemptProjection) -> Result<LoopStep, PollError> {
        validate_projection(projection, self.owner, 0).map_err(PollError::Ownership)?;
        if let Some(store) = &mut self.store {
            store
                .verify_driver_claim(self.claim)
                .map_err(|_| PollError::Ownership(AttemptDriverError::StaleOwner))?;
        }
        self.driver.next().await.map_err(PollError::Loop)
    }

    pub async fn restore_approved_tool(
        &mut self,
        projection: &AttemptProjection,
        call_id: ToolCallId,
    ) -> Result<(), PollError> {
        loop {
            match self.poll(projection).await? {
                LoopStep::Interrupt(LoopInterrupt::ApprovalRequest(found))
                    if found.request.call_id.as_ref() == Some(&call_id) =>
                {
                    self.driver
                        .resolve_approval_for(call_id, ApprovalDecision::Approve)
                        .map_err(PollError::Loop)?;
                    return Ok(());
                }
                LoopStep::Interrupt(LoopInterrupt::AfterToolResult(_)) => {}
                _ => {
                    return Err(PollError::Loop(LoopError::InvalidState(
                        "approved tool could not be reconstructed in AgentKit".to_owned(),
                    )));
                }
            }
        }
    }

    /// Checks durable ownership immediately before the guarded append.
    pub async fn commit<T, E>(
        &mut self,
        projection: &AttemptProjection,
        append: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, CommitError<E>> {
        validate_projection(projection, self.owner, 0).map_err(CommitError::Ownership)?;
        if let Some(store) = &mut self.store {
            store
                .verify_driver_claim(self.claim)
                .map_err(|_| CommitError::Ownership(AttemptDriverError::StaleOwner))?;
        }
        append().map_err(CommitError::Commit)
    }

    /// Makes cancellation visible to AgentKit only after its durable event commits.
    pub async fn commit_cancellation<T, E>(
        &mut self,
        projection: &AttemptProjection,
        append: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, CommitError<E>> {
        let result = self.commit(projection, append).await?;
        self.cancellation.interrupt();
        Ok(result)
    }

    /// Releases a live claim after its durable run has entered a waiting state.
    pub async fn suspend(
        mut self,
        projection: &AttemptProjection,
    ) -> Result<(), AttemptDriverError> {
        validate_projection(projection, self.owner, 0)?;
        match &mut self.store {
            Some(store) => store
                .quiesce_driver_claim(self.claim)
                .map_err(|_| AttemptDriverError::StaleOwner),
            None => Ok(()),
        }
    }

    /// Releases the run claim only after the durable attempt projection is inactive.
    pub async fn revoke(
        mut self,
        projection: &AttemptProjection,
    ) -> Result<(), AttemptDriverError> {
        if projection.id != self.owner.attempt_id {
            return Err(AttemptDriverError::AttemptMismatch);
        }
        if projection.owner != self.owner {
            return Err(AttemptDriverError::OwnerMismatch);
        }
        if matches!(
            projection.state,
            AttemptState::Leased | AttemptState::Executing
        ) {
            return Err(AttemptDriverError::ActiveAttempt(projection.state));
        }
        match &mut self.store {
            Some(store) => store
                .quiesce_driver_claim(self.claim)
                .map_err(|_| AttemptDriverError::StaleOwner),
            None => Ok(()),
        }
    }
}

fn validate_projection(
    projection: &AttemptProjection,
    owner: AttemptOwnership,
    minimum_version: u64,
) -> Result<(), AttemptDriverError> {
    if projection.id != owner.attempt_id {
        Err(AttemptDriverError::AttemptMismatch)
    } else if projection.owner != owner {
        Err(AttemptDriverError::OwnerMismatch)
    } else if projection.version < minimum_version {
        Err(AttemptDriverError::StaleProjection)
    } else if !matches!(
        projection.state,
        AttemptState::Leased | AttemptState::Executing
    ) {
        Err(AttemptDriverError::InactiveAttempt(projection.state))
    } else {
        Ok(())
    }
}
