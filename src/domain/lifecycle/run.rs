use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::super::ids::RunId;
use super::ownership::{AttemptOwnership, CasConflict, StateVersion, Transition, check_cas};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    AcquiringWorkspace,
    Starting,
    Running,
    WaitingForInput,
    WaitingForApproval,
    WaitingForAuth,
    Completed,
    Failed,
    Cancelling,
    Cancelled,
    Interrupted,
}

impl RunState {
    pub const ALL: [Self; 12] = [
        Self::Queued,
        Self::AcquiringWorkspace,
        Self::Starting,
        Self::Running,
        Self::WaitingForInput,
        Self::WaitingForApproval,
        Self::WaitingForAuth,
        Self::Completed,
        Self::Failed,
        Self::Cancelling,
        Self::Cancelled,
        Self::Interrupted,
    ];

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub const fn is_waiting(self) -> bool {
        matches!(
            self,
            Self::WaitingForInput | Self::WaitingForApproval | Self::WaitingForAuth
        )
    }

    pub const fn can_transition_to(self, target: Self) -> bool {
        match self {
            Self::Queued => matches!(target, Self::AcquiringWorkspace | Self::Cancelling),
            Self::AcquiringWorkspace => matches!(target, Self::Starting | Self::Cancelling),
            Self::Starting => matches!(target, Self::Running | Self::Cancelling),
            Self::Running => matches!(
                target,
                Self::WaitingForInput
                    | Self::WaitingForApproval
                    | Self::WaitingForAuth
                    | Self::Completed
                    | Self::Failed
                    | Self::Cancelling
                    | Self::Interrupted
            ),
            Self::WaitingForInput | Self::WaitingForApproval | Self::WaitingForAuth => {
                matches!(target, Self::Running | Self::Cancelling)
            }
            Self::Cancelling => {
                matches!(target, Self::Cancelled | Self::Failed | Self::Interrupted)
            }
            Self::Interrupted => {
                matches!(target, Self::Queued | Self::Failed | Self::Cancelling)
            }
            Self::Completed | Self::Failed | Self::Cancelled => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunRecord {
    pub run_id: RunId,
    pub state: RunState,
    pub version: StateVersion,
    pub owner: AttemptOwnership,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunTransitionCommand {
    pub expected_version: StateVersion,
    pub expected_owner: AttemptOwnership,
    pub target: RunState,
    pub replacement_owner: Option<AttemptOwnership>,
}

impl RunTransitionCommand {
    pub const fn new(
        expected_version: StateVersion,
        expected_owner: AttemptOwnership,
        target: RunState,
    ) -> Self {
        Self {
            expected_version,
            expected_owner,
            target,
            replacement_owner: None,
        }
    }

    pub const fn retry(
        expected_version: StateVersion,
        expected_owner: AttemptOwnership,
        replacement_owner: AttemptOwnership,
    ) -> Self {
        Self {
            expected_version,
            expected_owner,
            target: RunState::Queued,
            replacement_owner: Some(replacement_owner),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunTransitionError {
    Conflict(CasConflict),
    InvalidTransition { from: RunState, to: RunState },
    RetryRequiresNewAttempt,
    OwnerChangeRequiresRetry,
    RetryFenceNotAdvanced,
    RetryPrincipalChanged,
    VersionOverflow,
}

impl From<CasConflict> for RunTransitionError {
    fn from(conflict: CasConflict) -> Self {
        Self::Conflict(conflict)
    }
}

#[derive(Debug)]
pub struct RunLifecycle {
    inner: Mutex<RunRecord>,
}

impl RunLifecycle {
    pub const fn new(run_id: RunId, owner: AttemptOwnership) -> Self {
        Self {
            inner: Mutex::new(RunRecord {
                run_id,
                state: RunState::Queued,
                version: StateVersion::INITIAL,
                owner,
            }),
        }
    }

    pub fn snapshot(&self) -> RunRecord {
        *self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn transition(
        &self,
        command: RunTransitionCommand,
    ) -> Result<Transition<RunState>, RunTransitionError> {
        let mut record = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        check_cas(
            record.version,
            record.owner,
            command.expected_version,
            command.expected_owner,
        )?;

        if !record.state.can_transition_to(command.target) {
            return Err(RunTransitionError::InvalidTransition {
                from: record.state,
                to: command.target,
            });
        }

        let is_retry = record.state == RunState::Interrupted && command.target == RunState::Queued;
        let next_owner = match (is_retry, command.replacement_owner) {
            (true, None) => return Err(RunTransitionError::RetryRequiresNewAttempt),
            (false, Some(_)) => return Err(RunTransitionError::OwnerChangeRequiresRetry),
            (false, None) => record.owner,
            (true, Some(owner)) => {
                if owner.attempt_id == record.owner.attempt_id {
                    return Err(RunTransitionError::RetryRequiresNewAttempt);
                }
                if owner.fencing_token <= record.owner.fencing_token {
                    return Err(RunTransitionError::RetryFenceNotAdvanced);
                }
                if owner.principal_id != record.owner.principal_id {
                    return Err(RunTransitionError::RetryPrincipalChanged);
                }
                owner
            }
        };
        let next_version = record
            .version
            .increment()
            .ok_or(RunTransitionError::VersionOverflow)?;
        let transition = Transition {
            from: record.state,
            to: command.target,
            version: next_version,
            owner: next_owner,
        };

        record.state = command.target;
        record.version = next_version;
        record.owner = next_owner;
        Ok(transition)
    }
}
