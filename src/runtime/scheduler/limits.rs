use std::fmt;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Resource {
    CostMicrousd,
    Tokens,
    Turns,
    Tools,
    Processes,
}

impl Resource {
    pub const ALL: [Self; 5] = [
        Self::CostMicrousd,
        Self::Tokens,
        Self::Turns,
        Self::Tools,
        Self::Processes,
    ];
}

impl fmt::Display for Resource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::CostMicrousd => "cost_microusd",
            Self::Tokens => "tokens",
            Self::Turns => "turns",
            Self::Tools => "tools",
            Self::Processes => "processes",
        };
        formatter.write_str(name)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Spend {
    cost_microusd: u64,
    tokens: u64,
    turns: u64,
    tools: u64,
    processes: u64,
}

impl Spend {
    pub const ZERO: Self = Self::new(0, 0, 0, 0, 0);

    pub const fn new(
        cost_microusd: u64,
        tokens: u64,
        turns: u64,
        tools: u64,
        processes: u64,
    ) -> Self {
        Self {
            cost_microusd,
            tokens,
            turns,
            tools,
            processes,
        }
    }

    pub const fn cost_microusd(self) -> u64 {
        self.cost_microusd
    }

    pub const fn tokens(self) -> u64 {
        self.tokens
    }

    pub const fn turns(self) -> u64 {
        self.turns
    }

    pub const fn tools(self) -> u64 {
        self.tools
    }

    pub const fn processes(self) -> u64 {
        self.processes
    }

    pub const fn get(self, resource: Resource) -> u64 {
        match resource {
            Resource::CostMicrousd => self.cost_microusd,
            Resource::Tokens => self.tokens,
            Resource::Turns => self.turns,
            Resource::Tools => self.tools,
            Resource::Processes => self.processes,
        }
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self::new(
            self.cost_microusd.checked_add(other.cost_microusd)?,
            self.tokens.checked_add(other.tokens)?,
            self.turns.checked_add(other.turns)?,
            self.tools.checked_add(other.tools)?,
            self.processes.checked_add(other.processes)?,
        ))
    }

    pub fn checked_sub(self, other: Self) -> Option<Self> {
        Some(Self::new(
            self.cost_microusd.checked_sub(other.cost_microusd)?,
            self.tokens.checked_sub(other.tokens)?,
            self.turns.checked_sub(other.turns)?,
            self.tools.checked_sub(other.tools)?,
            self.processes.checked_sub(other.processes)?,
        ))
    }
}
