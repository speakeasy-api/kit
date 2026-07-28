use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::super::ids::{AttemptId, RunId};
use super::ownership::{AttemptOwnership, CasConflict, StateVersion, Transition, check_cas};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Leased,
    Executing,
    Quiescing,
    Succeeded,
    Failed,
    Interrupted,
}

impl AttemptState {
    pub const ALL: [Self; 6] = [
        Self::Leased,
        Self::Executing,
        Self::Quiescing,
        Self::Succeeded,
        Self::Failed,
        Self::Interrupted,
    ];

    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Interrupted)
    }

    pub const fn can_transition_to(self, target: Self) -> bool {
        match self {
            Self::Leased => matches!(target, Self::Executing | Self::Quiescing),
            Self::Executing => matches!(target, Self::Quiescing),
            Self::Quiescing => {
                matches!(target, Self::Succeeded | Self::Failed | Self::Interrupted)
            }
            Self::Succeeded | Self::Failed | Self::Interrupted => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttemptRecord {
    pub attempt_id: AttemptId,
    pub run_id: RunId,
    pub state: AttemptState,
    pub version: StateVersion,
    pub owner: AttemptOwnership,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptTransitionCommand {
    pub expected_version: StateVersion,
    pub expected_owner: AttemptOwnership,
    pub target: AttemptState,
}

impl AttemptTransitionCommand {
    pub const fn new(
        expected_version: StateVersion,
        expected_owner: AttemptOwnership,
        target: AttemptState,
    ) -> Self {
        Self {
            expected_version,
            expected_owner,
            target,
        }
    }

    pub const fn lease_lost(
        expected_version: StateVersion,
        expected_owner: AttemptOwnership,
    ) -> Self {
        Self::new(expected_version, expected_owner, AttemptState::Quiescing)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptTransitionError {
    Conflict(CasConflict),
    InvalidTransition {
        from: AttemptState,
        to: AttemptState,
    },
    VersionOverflow,
}

impl From<CasConflict> for AttemptTransitionError {
    fn from(conflict: CasConflict) -> Self {
        Self::Conflict(conflict)
    }
}

#[derive(Debug)]
pub struct AttemptLifecycle {
    inner: Mutex<AttemptRecord>,
}

impl AttemptLifecycle {
    pub const fn new(run_id: RunId, owner: AttemptOwnership) -> Self {
        Self {
            inner: Mutex::new(AttemptRecord {
                attempt_id: owner.attempt_id,
                run_id,
                state: AttemptState::Leased,
                version: StateVersion::INITIAL,
                owner,
            }),
        }
    }

    pub fn snapshot(&self) -> AttemptRecord {
        *self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn transition(
        &self,
        command: AttemptTransitionCommand,
    ) -> Result<Transition<AttemptState>, AttemptTransitionError> {
        let mut record = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        check_cas(
            record.version,
            record.owner,
            command.expected_version,
            command.expected_owner,
        )?;
        if !record.state.can_transition_to(command.target) {
            return Err(AttemptTransitionError::InvalidTransition {
                from: record.state,
                to: command.target,
            });
        }
        let next_version = record
            .version
            .increment()
            .ok_or(AttemptTransitionError::VersionOverflow)?;
        let transition = Transition {
            from: record.state,
            to: command.target,
            version: next_version,
            owner: record.owner,
        };

        record.state = command.target;
        record.version = next_version;
        Ok(transition)
    }
}
