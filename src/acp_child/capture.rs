//! Result capture for legacy routes and insertion-correlated draft v2 prompts.
use std::collections::BTreeSet;

use agent_client_protocol::v2::schema::MessageId;

use super::{ChildError, ChildOutput, protocol::Foreground};

pub(super) enum Receipt {
    Waiting,
    Settled(Foreground),
    EchoOverflow,
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

    pub(super) fn state(&mut self, state: Foreground) -> Option<Foreground> {
        match self {
            Self::Legacy(turn) => turn.state(state),
            Self::Correlated(correlation) => correlation.state(state),
        }
    }

    pub(super) fn echo(&mut self, id: &str) {
        if let Self::Correlated(correlation) = self {
            correlation.echo(id);
        }
    }

    pub(super) fn receipt(&mut self, id: MessageId) -> Receipt {
        match self {
            Self::Legacy(_) => Receipt::Waiting,
            Self::Correlated(correlation) => correlation.receipt(id),
        }
    }

    pub(super) fn record_output(&mut self, record: impl FnOnce(&mut ChildOutput)) {
        match self {
            Self::Legacy(turn) => record(&mut turn.output),
            Self::Correlated(correlation) => correlation.record_output(record),
        }
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
    fn state(&mut self, state: Foreground) -> Option<Foreground> {
        if self.foreground.advance(state) {
            self.idle()
        } else {
            None
        }
    }

    fn idle(&self) -> Option<Foreground> {
        matches!(self.foreground, Foreground::Idle(_)).then(|| self.foreground.clone())
    }
}

#[derive(Default)]
struct Candidate {
    echoes: BTreeSet<String>,
    echo_bytes: usize,
    turn: Turn,
    output_overflow: bool,
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
        } else if !self.current.echoes.contains(id) {
            if self.echo_count == MAX_UNCONFIRMED_ECHOES
                || id.len() > MAX_UNCONFIRMED_ECHO_BYTES - self.echo_bytes
            {
                self.echo_overflow = true;
                return;
            }
            self.echo_count += 1;
            self.echo_bytes += id.len();
            self.current.echo_bytes += id.len();
            self.current.echoes.insert(id.to_owned());
        }
    }

    fn receipt(&mut self, id: MessageId) -> Receipt {
        let id = id.to_string();
        if let Some(index) = self
            .completed
            .iter()
            .position(|turn| turn.echoes.contains(&id))
        {
            self.current = self.completed.swap_remove(index);
            self.matched = true;
        } else if self.current.echoes.contains(&id) {
            self.matched = true;
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
        if self.matched
            && let Some(idle) = self.current.turn.idle()
        {
            Receipt::Settled(idle)
        } else {
            Receipt::Waiting
        }
    }

    fn state(&mut self, state: Foreground) -> Option<Foreground> {
        let idle = if self.current.turn.foreground == Foreground::Waiting
            && matches!(state, Foreground::Idle(_))
        {
            if !self.matched && self.current.echoes.is_empty() {
                // Registration can miss an unrelated turn's Running. Its Idle
                // still marks a boundary, but cannot settle this route without an echo.
                self.discard_current();
                return None;
            }
            self.current.turn.foreground = state;
            self.current.turn.idle()?
        } else {
            self.current.turn.state(state)?
        };
        if self.matched {
            return Some(idle);
        }
        if self.current.echoes.is_empty() {
            self.discard_current();
        } else {
            self.completed.push(std::mem::take(&mut self.current));
        }
        None
    }

    fn discard_current(&mut self) {
        let discarded = std::mem::take(&mut self.current);
        self.echo_count -= discarded.echoes.len();
        self.echo_bytes -= discarded.echo_bytes;
        self.output_bytes -= discarded.turn.output.retained_bytes();
    }

    fn record_output(&mut self, record: impl FnOnce(&mut ChildOutput)) {
        if self.current.output_overflow
            || self.current.turn.idle().is_some()
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
