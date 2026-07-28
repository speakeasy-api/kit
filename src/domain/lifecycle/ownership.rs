use serde::{Deserialize, Serialize};

use super::super::ids::{
    AgentLinkId, AttemptId, DaemonServiceId, ModelCallId, PrincipalId, ProcessId, TerminalId,
    ToolCallId, TurnId, WorkspaceId,
};

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct StateVersion(u64);

impl StateVersion {
    pub const INITIAL: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn increment(self) -> Option<Self> {
        self.0.checked_add(1).map(Self)
    }
}

#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct FencingToken(u64);

impl FencingToken {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct AttemptOwnership {
    pub attempt_id: AttemptId,
    pub principal_id: PrincipalId,
    pub fencing_token: FencingToken,
}

impl AttemptOwnership {
    pub const fn new(
        attempt_id: AttemptId,
        principal_id: PrincipalId,
        fencing_token: FencingToken,
    ) -> Self {
        Self {
            attempt_id,
            principal_id,
            fencing_token,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct AttemptOwned<T> {
    pub resource_id: T,
    pub owner: AttemptOwnership,
}

impl<T> AttemptOwned<T> {
    pub const fn new(resource_id: T, owner: AttemptOwnership) -> Self {
        Self { resource_id, owner }
    }
}

pub type WorkspaceWriterOwnership = AttemptOwned<WorkspaceId>;
pub type TerminalOwnership = AttemptOwned<TerminalId>;
pub type ModelCallOwnership = AttemptOwned<ModelCallId>;
pub type LocalChildAgentOwnership = AttemptOwned<AgentLinkId>;
pub type TurnOwnership = AttemptOwned<TurnId>;
pub type ToolCallOwnership = AttemptOwned<ToolCallId>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", content = "owner", rename_all = "snake_case")]
pub enum ProcessOwnership {
    Attempt(AttemptOwnership),
    DaemonService(DaemonServiceId),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ProcessClaim {
    pub process_id: ProcessId,
    pub owner: ProcessOwnership,
}

impl ProcessClaim {
    pub const fn new(process_id: ProcessId, owner: ProcessOwnership) -> Self {
        Self { process_id, owner }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CasConflict {
    Version {
        expected: StateVersion,
        actual: StateVersion,
    },
    Fence {
        expected: FencingToken,
        actual: FencingToken,
    },
    Owner {
        expected: AttemptId,
        actual: AttemptId,
    },
    Principal {
        expected: PrincipalId,
        actual: PrincipalId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Transition<S> {
    pub from: S,
    pub to: S,
    pub version: StateVersion,
    pub owner: AttemptOwnership,
}

pub(crate) fn check_cas(
    actual_version: StateVersion,
    actual_owner: AttemptOwnership,
    expected_version: StateVersion,
    expected_owner: AttemptOwnership,
) -> Result<(), CasConflict> {
    if expected_version != actual_version {
        return Err(CasConflict::Version {
            expected: expected_version,
            actual: actual_version,
        });
    }
    if expected_owner.fencing_token != actual_owner.fencing_token {
        return Err(CasConflict::Fence {
            expected: expected_owner.fencing_token,
            actual: actual_owner.fencing_token,
        });
    }
    if expected_owner.attempt_id != actual_owner.attempt_id {
        return Err(CasConflict::Owner {
            expected: expected_owner.attempt_id,
            actual: actual_owner.attempt_id,
        });
    }
    if expected_owner.principal_id != actual_owner.principal_id {
        return Err(CasConflict::Principal {
            expected: expected_owner.principal_id,
            actual: actual_owner.principal_id,
        });
    }
    Ok(())
}
