#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AcpEvent {
    ChildStarted,
    ToolCallStarted,
    CancelRequested,
    CancelAcknowledged,
    ChildExited,
    QuiescenceConfirmed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChildState {
    Running,
    Interrupted,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpOutcome {
    pub trace_id: u64,
    pub state: ChildState,
    pub events: Vec<AcpEvent>,
}

#[derive(Clone, Debug)]
pub struct AcpSimulator {
    seed: u64,
}

impl AcpSimulator {
    pub fn new(seed: u64) -> Self {
        Self { seed }
    }

    pub fn replay(&self, events: &[AcpEvent]) -> AcpOutcome {
        let acknowledged = events.contains(&AcpEvent::CancelAcknowledged);
        let quiescent = events.contains(&AcpEvent::QuiescenceConfirmed);
        let exited = events.contains(&AcpEvent::ChildExited);
        let state = if acknowledged || quiescent {
            ChildState::Cancelled
        } else if exited {
            ChildState::Interrupted
        } else {
            ChildState::Running
        };

        AcpOutcome {
            trace_id: self.seed.rotate_left(29) ^ 0x4143_5000,
            state,
            events: events.to_vec(),
        }
    }
}
