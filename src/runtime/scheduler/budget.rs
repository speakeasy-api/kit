use super::limits::{Resource, Spend};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RunBudget {
    limits: Spend,
}

impl RunBudget {
    pub const MAX_CUMULATIVE_TOOL_CALLS: u64 = 256;
    pub const MAX_CUMULATIVE_PROCESSES: u64 = 256;

    pub const fn new(
        cost_microusd: u64,
        tokens: u64,
        turns: u64,
        tools: u64,
        processes: u64,
    ) -> Self {
        Self {
            limits: Spend::new(cost_microusd, tokens, turns, tools, processes),
        }
    }

    pub const fn limits(self) -> Spend {
        self.limits
    }

    pub fn from_effective_config(config: &crate::domain::config::EffectiveConfig) -> Self {
        Self::new(
            config.max_cost_microusd,
            config.max_tokens,
            u64::from(config.max_turns),
            Self::MAX_CUMULATIVE_TOOL_CALLS,
            Self::MAX_CUMULATIVE_PROCESSES,
        )
    }

    pub const fn limit(self, resource: Resource) -> u64 {
        self.limits.get(resource)
    }

    pub fn remaining(self, committed: Spend, reserved: Spend) -> Spend {
        Spend::new(
            self.remaining_for(Resource::CostMicrousd, committed, reserved),
            self.remaining_for(Resource::Tokens, committed, reserved),
            self.remaining_for(Resource::Turns, committed, reserved),
            self.remaining_for(Resource::Tools, committed, reserved),
            self.remaining_for(Resource::Processes, committed, reserved),
        )
    }

    pub(crate) fn check(
        self,
        committed: Spend,
        reserved: Spend,
        requested: Spend,
    ) -> Result<(), Exhaustion> {
        for resource in Resource::ALL {
            let maximum = self.limit(resource);
            let committed = committed.get(resource);
            let reserved = reserved.get(resource);
            let requested = requested.get(resource);
            let used = committed.saturating_add(reserved);
            if used > maximum || requested > maximum - used {
                return Err(Exhaustion {
                    resource,
                    maximum,
                    committed,
                    reserved,
                    requested,
                });
            }
        }
        Ok(())
    }

    fn remaining_for(self, resource: Resource, committed: Spend, reserved: Spend) -> u64 {
        self.limit(resource)
            .saturating_sub(committed.get(resource))
            .saturating_sub(reserved.get(resource))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Exhaustion {
    pub resource: Resource,
    pub maximum: u64,
    pub committed: u64,
    pub reserved: u64,
    pub requested: u64,
}
