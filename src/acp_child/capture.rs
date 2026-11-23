//! Result capture for legacy routes and insertion-correlated draft v2 prompts.
use std::collections::BTreeMap;

use agent_client_protocol::v2::schema::{MessageId, StopReason};

use super::{ChildError, ChildOutput, protocol::Foreground};

pub(super) enum Receipt {
    Accepted,
    EchoOverflow,
}

pub(super) enum Boundary {
    Waiting,
    AmbiguousIdle,
    Settled(Option<StopReason>),
}

pub(super) enum Capture {
    Legacy(Box<Turn>),
    Correlated(Box<Correlation>),
}

impl Capture {
    pub(super) fn new(correlate: bool) -> Self {
        // Only an agent advertising steer can create a rollover turn for a steer
        // we sent. Older v2 agents may not echo their ordinary prompt IDs at all.
        if correlate {
            Self::Correlated(Box::default())
        } else {
            Self::Legacy(Box::default())
        }
    }

    pub(super) fn state(&mut self, state: Foreground, repeated_idle: bool) -> bool {
        match self {
            Self::Legacy(turn) => turn.state(state),
            Self::Correlated(correlation) => correlation.state(state, repeated_idle),
        }
    }

    pub(super) fn boundary(&self) -> Boundary {
        let turn = match self {
            Self::Legacy(turn) => turn,
            Self::Correlated(correlation) if correlation.matched => {
                if correlation.current.ambiguous_idle {
                    return Boundary::AmbiguousIdle;
                }
                &correlation.current.turn
            }
            Self::Correlated(_) => return Boundary::Waiting,
        };
        turn.idle().map_or(Boundary::Waiting, Boundary::Settled)
    }

    pub(super) fn echo(&mut self, id: &str) {
        if let Self::Correlated(correlation) = self {
            correlation.echo(id);
        }
    }

    pub(super) fn receipt(&mut self, id: MessageId) -> Receipt {
        match self {
            Self::Legacy(_) => Receipt::Accepted,
            Self::Correlated(correlation) => correlation.receipt(id),
        }
    }

    pub(super) fn record_output(&mut self, record: impl FnOnce(&mut ChildOutput)) {
        match self {
            Self::Legacy(turn) => record(&mut turn.output),
            Self::Correlated(correlation) => correlation.record_output(record),
        }
    }

    pub(super) fn record_message(
        &mut self,
        update: agent_client_protocol::v2::schema::SessionUpdate,
    ) -> bool {
        let changed = if let Self::Correlated(correlation) = self
            && (correlation.matched || !correlation.current.echoes.is_empty())
        {
            // Only the first answer update can lift the ambiguity deadline.
            let first = !std::mem::replace(&mut correlation.current.answer_observed, true);
            first && correlation.current.clear_ambiguous_idle() && correlation.matched
        } else {
            false
        };
        self.record_output(|output| output.record_message(update));
        changed
    }

    pub(super) fn take_result(&mut self) -> Result<ChildOutput, ChildError> {
        if let Self::Correlated(correlation) = self
            && correlation.matched
            && correlation.current.output_overflow
        {
            return Err(ChildError::Failed(
                "nested agent exceeded the provisional output limit".into(),
            ));
        }
        Ok(self.take_output())
    }

    pub(super) fn take_output(&mut self) -> ChildOutput {
        match self {
            Self::Legacy(turn) => std::mem::take(&mut turn.output),
            Self::Correlated(correlation) if correlation.matched => {
                std::mem::take(&mut correlation.current.turn.output)
            }
            Self::Correlated(_) => ChildOutput::default(),
        }
    }
}

pub(super) struct Turn {
    foreground: Foreground,
    output: ChildOutput,
}

impl Default for Turn {
    fn default() -> Self {
        Self {
            foreground: Foreground::Waiting,
            output: ChildOutput::default(),
        }
    }
}

impl Turn {
    fn state(&mut self, state: Foreground) -> bool {
        self.foreground.advance(state) && matches!(self.foreground, Foreground::Idle(_))
    }

    fn idle(&self) -> Option<Option<StopReason>> {
        match &self.foreground {
            Foreground::Idle(reason) => Some(reason.clone()),
            _ => None,
        }
    }
}

#[derive(Default)]
struct Candidate {
    echoes: BTreeMap<String, bool>,
    echo_bytes: usize,
    turn: Turn,
    answer_observed: bool,
    ambiguous_idle: bool,
    output_overflow: bool,
}

impl Candidate {
    fn clear_ambiguous_idle(&mut self) -> bool {
        self.echoes.values_mut().for_each(|idle| *idle = false);
        std::mem::take(&mut self.ambiguous_idle)
    }
}

// A receipt may follow the entire matching turn. Keep bounded provisional turn
// captures, never a shared output buffer spanning distinct Running/Idle pairs.
const MAX_UNCONFIRMED_ECHOES: usize = 64;
const MAX_UNCONFIRMED_ECHO_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROVISIONAL_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Correlation {
    receipt: Option<String>,
    matched: bool,
    current: Candidate,
    completed: Vec<Candidate>,
    echo_count: usize,
    echo_bytes: usize,
    output_bytes: usize,
    echo_overflow: bool,
}

impl Correlation {
    fn echo(&mut self, id: &str) {
        if self.matched {
            return;
        }
        if let Some(receipt) = &self.receipt {
            self.matched = receipt == id;
        } else if !self.current.echoes.contains_key(id) {
            if self.echo_count == MAX_UNCONFIRMED_ECHOES
                || id.len() > MAX_UNCONFIRMED_ECHO_BYTES - self.echo_bytes
            {
                self.echo_overflow = true;
                return;
            }
            self.echo_count += 1;
            self.echo_bytes += id.len();
            self.current.echo_bytes += id.len();
            self.current.echoes.insert(id.to_owned(), false);
        }
    }

    fn receipt(&mut self, id: MessageId) -> Receipt {
        let id = id.to_string();
        if let Some(index) = self
            .completed
            .iter()
            .position(|turn| turn.echoes.contains_key(&id))
        {
            self.current = self.completed.swap_remove(index);
            self.matched = true;
        } else if let Some(ambiguous_idle) = self.current.echoes.get(&id) {
            self.matched = true;
            self.current.ambiguous_idle = *ambiguous_idle;
        } else if self.echo_overflow {
            return Receipt::EchoOverflow;
        }
        self.completed.clear();
        // Keep the current Running candidate even without an echo: its echo
        // can still arrive later. All other candidates have been discarded.
        self.output_bytes = self.current.turn.output.retained_bytes();
        self.current.echoes.clear();
        self.current.echo_bytes = 0;
        self.echo_count = 0;
        self.echo_bytes = 0;
        self.receipt = Some(id);
        Receipt::Accepted
    }

    // Return whether a waiting prompt must recheck its boundary.
    fn state(&mut self, state: Foreground, repeated_idle: bool) -> bool {
        // An echo only establishes insertion. A repeated Idle carries no turn
        // identifier, and even its stop reason may be from an earlier turn.
        if repeated_idle && !self.current.answer_observed {
            if self.current.turn.foreground == Foreground::Waiting {
                if self.matched {
                    return !std::mem::replace(&mut self.current.ambiguous_idle, true);
                }
                self.current
                    .echoes
                    .values_mut()
                    .for_each(|idle| *idle = true);
            }
            return false;
        }
        let active = state == Foreground::Running;
        if self.current.turn.foreground == Foreground::Waiting
            && matches!(state, Foreground::Idle(_))
        {
            if !self.matched && self.current.echoes.is_empty() {
                // Registration can miss an unrelated turn's Running. Its Idle
                // still marks a boundary, but cannot settle this route without an echo.
                self.discard_current();
                return false;
            }
            self.current.turn.foreground = state;
        } else if !self.current.turn.state(state) {
            // Resume the ordinary wait once the peer reports active work.
            return active && self.current.clear_ambiguous_idle() && self.matched;
        }
        self.current.ambiguous_idle = false;
        if self.matched {
            return true;
        }
        if self.current.echoes.is_empty() {
            self.discard_current();
        } else {
            self.completed.push(std::mem::take(&mut self.current));
        }
        false
    }

    fn discard_current(&mut self) {
        let discarded = std::mem::take(&mut self.current);
        self.echo_count -= discarded.echoes.len();
        self.echo_bytes -= discarded.echo_bytes;
        self.output_bytes -= discarded.turn.output.retained_bytes();
    }

    fn record_output(&mut self, record: impl FnOnce(&mut ChildOutput)) {
        if self.current.output_overflow
            || matches!(self.current.turn.foreground, Foreground::Idle(_))
            || !(self.matched
                || self.current.turn.foreground == Foreground::Running
                || !self.current.echoes.is_empty())
        {
            return;
        }
        let output = &mut self.current.turn.output;
        if self.matched {
            record(output);
            return;
        }
        let other_bytes = self.output_bytes - output.retained_bytes();
        record(output);
        // Charge what the recorder kept, not the wire frame. Rich updates can
        // truncate, and message replacements can release previously held bytes.
        let retained = output.retained_bytes();
        if retained > MAX_PROVISIONAL_OUTPUT_BYTES - other_bytes {
            // Only this candidate's payload overflowed; its echo and state can
            // still settle, without affecting any other candidates.
            *output = ChildOutput::default();
            self.current.output_overflow = true;
            self.output_bytes = other_bytes;
        } else {
            self.output_bytes = other_bytes + retained;
        }
    }
}
