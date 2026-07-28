mod attempt;
mod ownership;
mod run;

use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

pub use attempt::{
    AttemptLifecycle, AttemptRecord, AttemptState, AttemptTransitionCommand, AttemptTransitionError,
};
pub use ownership::{
    AttemptOwned, AttemptOwnership, CasConflict, FencingToken, LocalChildAgentOwnership,
    ModelCallOwnership, ProcessClaim, ProcessOwnership, StateVersion, TerminalOwnership,
    ToolCallOwnership, Transition, TurnOwnership, WorkspaceWriterOwnership,
};
pub use run::{RunLifecycle, RunRecord, RunState, RunTransitionCommand, RunTransitionError};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct TransitionWire<S> {
    from: S,
    to: S,
}

macro_rules! lifecycle_transition {
    ($name:ident, $state:ty) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
        pub struct $name {
            from: $state,
            to: $state,
        }

        impl $name {
            pub fn new(from: $state, to: $state) -> Result<Self, TransitionError> {
                from.can_transition_to(to)
                    .then_some(Self { from, to })
                    .ok_or(TransitionError)
            }

            pub const fn from(self) -> $state {
                self.from
            }

            pub const fn to(self) -> $state {
                self.to
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = TransitionWire::<$state>::deserialize(deserializer)?;
                Self::new(value.from, value.to).map_err(D::Error::custom)
            }
        }
    };
}

lifecycle_transition!(RunTransition, RunState);
lifecycle_transition!(AttemptTransition, AttemptState);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransitionError;

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid lifecycle transition")
    }
}

impl std::error::Error for TransitionError {}
