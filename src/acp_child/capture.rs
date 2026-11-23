//! Result capture for legacy routes and insertion-correlated draft v2 prompts.
use std::collections::BTreeSet;

use agent_client_protocol::v2::schema::MessageId;
use serde_json::Value;

use super::{ChildOutput, protocol::Foreground};

pub(super) fn echo_id(update: &Value) -> Option<&str> {
    if matches!(
        update["sessionUpdate"].as_str(),
        Some("user_message" | "user_message_chunk")
    ) {
        return update["messageId"].as_str();
    }
    None
}

pub(super) enum Receipt {
    Waiting,
    Settled(Foreground),
    Overflow,
}

pub(super) enum Capture {
    Legacy(Turn),
    Correlated(Box<Correlation>),
}

impl Capture {
    pub(super) fn new(correlate: bool) -> Self {
        // Only an agent advertising steer can create a rollover turn for a steer
        // we sent. Older v2 agents may not echo their ordinary prompt IDs at all.
        if correlate {
            Self::Correlated(Box::default())
        } else {
            Self::Legacy(Turn::default())
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

    pub(super) fn output_mut(&mut self, update: &Value) -> Option<&mut ChildOutput> {
        match self {
            Self::Legacy(turn) => Some(&mut turn.output),
            Self::Correlated(correlation) => correlation.output_mut(update),
        }
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
    turn: Turn,
}

// A receipt may follow the entire matching turn. Keep bounded provisional turn
// captures, never a shared output buffer spanning distinct Running/Idle pairs.
const MAX_UNCONFIRMED_ECHOES: usize = 64;
const MAX_UNCONFIRMED_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Correlation {
    receipt: Option<String>,
    matched: bool,
    current: Candidate,
    completed: Vec<Candidate>,
    echo_count: usize,
    buffered_bytes: usize,
    overflow: bool,
}

impl Correlation {
    fn echo(&mut self, id: &str) {
        if self.matched || self.overflow {
            return;
        }
        if let Some(receipt) = &self.receipt {
            self.matched = receipt == id;
        } else if !self.current.echoes.contains(id) {
            if self.echo_count == MAX_UNCONFIRMED_ECHOES || !self.reserve(id.len()) {
                self.discard_unconfirmed();
                return;
            }
            self.echo_count += 1;
            self.current.echoes.insert(id.to_owned());
        }
    }

    fn receipt(&mut self, id: MessageId) -> Receipt {
        if self.overflow {
            return Receipt::Overflow;
        }
        let id = id.to_string();
        if let Some(turn) = self
            .completed
            .drain(..)
            .find(|turn| turn.echoes.contains(&id))
        {
            self.current = turn;
            self.matched = true;
        } else {
            self.matched = self.current.echoes.contains(&id);
            if !self.matched {
                self.current.turn.output = ChildOutput::default();
            }
        }
        self.current.echoes.clear();
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
        if !self.matched
            && matches!(self.current.turn.foreground, Foreground::Waiting)
            && matches!(state, Foreground::Idle(_))
        {
            // Registration can miss an unrelated turn's Running. Its Idle
            // still marks a boundary, but cannot settle or contaminate this route.
            self.current = Candidate::default();
            return None;
        }
        let idle = self.current.turn.state(state)?;
        if self.matched {
            return Some(idle);
        }
        let finished = std::mem::take(&mut self.current);
        if !finished.echoes.is_empty() {
            self.completed.push(finished);
        }
        None
    }

    fn output_mut(&mut self, update: &Value) -> Option<&mut ChildOutput> {
        if !(self.matched || !self.current.echoes.is_empty()) || self.current.turn.idle().is_some()
        {
            return None;
        }
        // Only the provisional buffer needs an aggregate limit. A normal
        // confirmed result keeps the existing child output limits.
        if self.receipt.is_none() && !self.reserve(update.to_string().len()) {
            self.discard_unconfirmed();
            return None;
        }
        Some(&mut self.current.turn.output)
    }

    fn reserve(&mut self, bytes: usize) -> bool {
        self.buffered_bytes = self.buffered_bytes.saturating_add(bytes);
        self.buffered_bytes <= MAX_UNCONFIRMED_BYTES
    }

    fn discard_unconfirmed(&mut self) {
        self.overflow = true;
        self.completed.clear();
        self.current = Candidate::default();
    }
}
