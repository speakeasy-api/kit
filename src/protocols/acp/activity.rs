use agentkit_loop::AgentEvent;

/// Authoritative activity state for an attached session. A drive/wake-up is
/// not itself activity: only a logical turn started by the loop enters Running.
/// A finished logical turn remains Settling until the actor has drained steering
/// and structured background work. Those continuations belong to the same client-visible
/// activity interval, rather than producing an Idle/Running flicker.
#[derive(Default)]
pub(super) enum SessionActivity {
    #[default]
    Idle,
    Running,
    Settling,
}

impl SessionActivity {
    pub(super) fn observe(&mut self, event: &AgentEvent) -> Option<bool> {
        match event {
            AgentEvent::TurnStarted { .. } => {
                let was_idle = matches!(self, Self::Idle);
                *self = Self::Running;
                was_idle.then_some(true)
            }
            AgentEvent::TurnFinished(_) if !matches!(self, Self::Idle) => {
                *self = Self::Settling;
                None
            }
            _ => None,
        }
    }

    pub(super) fn settle(&mut self) -> Option<bool> {
        if matches!(self, Self::Idle) {
            return None;
        }
        *self = Self::Idle;
        Some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentkit_core::{FinishReason, MetadataMap, SessionId};

    #[test]
    fn session_activity_coalesces_continuations_and_settles_once() {
        for reason in [
            FinishReason::Completed,
            FinishReason::Cancelled,
            FinishReason::MaxTokens,
            FinishReason::Blocked,
            FinishReason::Error,
        ] {
            let mut state = SessionActivity::default();
            assert!(state.settle().is_none());
            let started = AgentEvent::TurnStarted {
                session_id: SessionId::new("state-loop"),
                turn_id: agentkit_core::TurnId::new("first"),
            };
            assert!(matches!(state.observe(&started), Some(true)));
            assert!(state.observe(&started).is_none());
            let finished = AgentEvent::TurnFinished(agentkit_loop::TurnResult {
                turn_id: agentkit_core::TurnId::new("first"),
                finish_reason: reason,
                items: Vec::new(),
                usage: None,
                metadata: MetadataMap::new(),
            });
            assert!(state.observe(&finished).is_none());
            assert!(matches!(state, SessionActivity::Settling));
            // A queued steer or structured synthesis continues before actor settlement.
            assert!(
                state
                    .observe(&AgentEvent::TurnStarted {
                        session_id: SessionId::new("state-loop"),
                        turn_id: agentkit_core::TurnId::new("continuation"),
                    })
                    .is_none()
            );
            assert!(matches!(state.settle(), Some(false)));
            assert!(state.settle().is_none());
            assert!(state.observe(&finished).is_none());
            assert!(matches!(state, SessionActivity::Idle));
            assert!(matches!(state.observe(&started), Some(true)));
        }
    }
}
