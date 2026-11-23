//! Result capture for legacy routes and insertion-correlated draft v2 prompts.
use std::collections::BTreeSet;

use agent_client_protocol::v2::schema::{MessageId, StopReason};
use tokio::time::Instant;

use super::{ChildError, ChildOutput, protocol::Foreground};

pub(super) enum Receipt {
    Accepted,
    EchoOverflow,
}

pub(super) enum Boundary {
    Waiting,
    UnverifiedIdle(Instant),
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
        match self {
            Self::Legacy(turn) => turn.idle().map_or(Boundary::Waiting, Boundary::Settled),
            Self::Correlated(correlation) => correlation.boundary(),
        }
    }

    pub(super) fn echo(&mut self, id: &str) -> bool {
        match self {
            Self::Legacy(_) => false,
            Self::Correlated(correlation) => correlation.echo(id),
        }
    }

    pub(super) fn record_thought(&mut self) -> bool {
        match self {
            Self::Legacy(_) => false,
            Self::Correlated(correlation) => correlation.progress(Progress::Thought),
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
        let changed = match self {
            Self::Legacy(_) => false,
            Self::Correlated(correlation) => correlation.progress(Progress::Answer),
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
    echoes: BTreeSet<String>,
    echo_bytes: usize,
    turn: Turn,
    answer_observed: bool,
    thought: Thought,
    unverified_idle: Option<Instant>,
    output_overflow: bool,
}

enum Progress {
    Thought,
    Answer,
}

#[derive(Default, PartialEq, Eq)]
enum Thought {
    #[default]
    Unseen,
    Observed,
    ClearedIdle,
}

// A receipt may follow the entire matching turn. Keep bounded provisional turn
// captures, never a shared output buffer spanning distinct Running/Idle pairs.
const MAX_UNCONFIRMED_ECHOES: usize = 64;
const MAX_UNCONFIRMED_ECHO_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROVISIONAL_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Correlation {
    receipt: Option<(String, Instant)>,
    idle_without_match: Option<Instant>,
    matched: bool,
    current: Candidate,
    completed: Vec<Candidate>,
    echo_count: usize,
    echo_bytes: usize,
    output_bytes: usize,
    echo_overflow: bool,
}

impl Correlation {
    fn boundary(&self) -> Boundary {
        let Some((_, accepted)) = self.receipt else {
            return Boundary::Waiting;
        };
        let idle = if self.matched {
            if let Some(reason) = self.current.turn.idle() {
                return Boundary::Settled(reason);
            }
            self.current.unverified_idle
        } else {
            self.idle_without_match
        };
        // An Idle may precede the receipt. Give correlation a full window from
        // acceptance, without letting unrelated traffic extend that window.
        idle.map_or(Boundary::Waiting, |idle| {
            Boundary::UnverifiedIdle(idle.max(accepted))
        })
    }

    fn echo(&mut self, id: &str) -> bool {
        if self.matched {
            return false;
        }
        if let Some((receipt, _)) = &self.receipt {
            if receipt == id {
                self.match_current();
                return true;
            }
        } else if !self.current.echoes.contains(id) {
            if self.echo_count == MAX_UNCONFIRMED_ECHOES
                || id.len() > MAX_UNCONFIRMED_ECHO_BYTES - self.echo_bytes
            {
                self.echo_overflow = true;
                return false;
            }
            self.echo_count += 1;
            self.echo_bytes += id.len();
            self.current.echo_bytes += id.len();
            self.current.echoes.insert(id.to_owned());
        }
        false
    }

    fn match_current(&mut self) {
        self.matched = true;
        // A late echo proves insertion, not that a previously observed Idle
        // ended this turn. Without other evidence, keep the same deadline.
        if self.current.turn.foreground == Foreground::Waiting
            && self.current.thought == Thought::Unseen
            && !self.current.answer_observed
        {
            self.current.unverified_idle = self.idle_without_match;
        }
    }

    fn progress(&mut self, progress: Progress) -> bool {
        if !self.matched && self.current.echoes.is_empty() {
            return false;
        }
        let turn = &mut self.current;
        match progress {
            Progress::Answer if !turn.answer_observed => turn.answer_observed = true,
            Progress::Thought if turn.thought != Thought::ClearedIdle => {
                turn.thought = if turn.unverified_idle.is_some() {
                    Thought::ClearedIdle
                } else {
                    Thought::Observed
                };
            }
            _ => return false,
        }
        // Reasoning can disprove one stale Idle even if the stream began before
        // the snapshot. Repeated thoughts cannot keep rearming that grace period.
        // Neither kind of progress retroactively makes the prior Idle terminal.
        turn.unverified_idle.take().is_some() && self.matched
    }

    fn receipt(&mut self, id: MessageId) -> Receipt {
        let id = id.to_string();
        if let Some(index) = self
            .completed
            .iter()
            .position(|turn| turn.echoes.contains(&id))
        {
            self.current = self.completed.swap_remove(index);
            self.match_current();
        } else if self.current.echoes.contains(&id) {
            self.match_current();
        } else if self.echo_overflow {
            return Receipt::EchoOverflow;
        }
        self.completed.clear();
        // The current turn may have multiple insertions. Keep its output
        // until a foreground boundary, even if only a different ID has echoed.
        self.output_bytes = self.current.turn.output.retained_bytes();
        self.current.echoes.clear();
        self.current.echo_bytes = 0;
        self.echo_count = 0;
        self.echo_bytes = 0;
        self.receipt = Some((id, Instant::now()));
        Receipt::Accepted
    }

    // Return whether a waiting prompt must recheck its boundary.
    fn state(&mut self, state: Foreground, repeated_idle: bool) -> bool {
        let idle = matches!(state, Foreground::Idle(_));
        let newly_unverified = if idle && !self.matched && self.idle_without_match.is_none() {
            self.idle_without_match = Some(Instant::now());
            self.receipt.is_some()
        } else {
            false
        };
        // An echo only establishes insertion. A repeated Idle carries no turn
        // identifier, and even its stop reason may be from an earlier turn.
        if repeated_idle
            && !self.current.answer_observed
            && self.current.turn.foreground == Foreground::Waiting
        {
            if (self.matched || !self.current.echoes.is_empty())
                && self.current.unverified_idle.is_none()
            {
                self.current.unverified_idle = Some(Instant::now());
                return self.matched || newly_unverified;
            }
            return newly_unverified;
        }
        if self.current.turn.foreground == Foreground::Waiting && idle {
            if !self.matched && self.current.echoes.is_empty() {
                // An Idle without an echo cannot identify this request's turn.
                self.discard_current();
                return newly_unverified;
            }
            self.current.turn.foreground = state;
        } else {
            let active = state == Foreground::Running;
            if !self.current.turn.state(state) {
                return active && self.current.unverified_idle.take().is_some() && self.matched;
            }
        }
        self.current.unverified_idle = None;
        if self.matched {
            return true;
        }
        if self.current.echoes.is_empty() {
            self.discard_current();
        } else {
            self.completed.push(std::mem::take(&mut self.current));
        }
        newly_unverified
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

#[cfg(test)]
#[allow(clippy::disallowed_macros)]
mod tests {
    use super::*;

    #[test]
    fn uncorrelated_idle_survives_wrong_echoes_and_a_late_matching_echo_alone() {
        let mut capture = Capture::new(true);
        capture.state(Foreground::Idle(Some(StopReason::Refusal)), false);
        assert!(matches!(capture.boundary(), Boundary::Waiting));
        assert!(matches!(
            capture.receipt(MessageId::new("ours")),
            Receipt::Accepted
        ));
        let Boundary::UnverifiedIdle(observed) = capture.boundary() else {
            panic!("acceptance must bound the wait after an uncorrelated Idle");
        };
        assert!(!capture.echo("theirs"));
        assert!(!capture.record_thought());
        assert!(!capture.state(Foreground::Idle(None), true));
        assert!(capture.echo("ours"));
        assert!(matches!(capture.boundary(), Boundary::UnverifiedIdle(same) if same == observed));
        assert!(capture.record_thought());
        assert!(matches!(capture.boundary(), Boundary::Waiting));
    }

    #[test]
    fn a_thought_stream_can_clear_one_stale_snapshot_but_does_not_settle_an_idle() {
        let mut capture = Capture::new(true);
        capture.receipt(MessageId::new("ours"));
        capture.echo("ours");
        assert!(!capture.record_thought());
        assert!(capture.state(Foreground::Idle(Some(StopReason::Refusal)), true));
        assert!(capture.record_thought());
        assert!(matches!(capture.boundary(), Boundary::Waiting));
        assert!(capture.state(Foreground::Idle(Some(StopReason::Refusal)), true));
        assert!(!capture.record_thought());
        assert!(matches!(capture.boundary(), Boundary::UnverifiedIdle(_)));
        assert!(capture.state(Foreground::Running, false));
        assert!(capture.state(Foreground::Idle(None), false));
        assert!(matches!(capture.boundary(), Boundary::Settled(None)));
    }
}
