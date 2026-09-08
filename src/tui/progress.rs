//! Single-writer transient source observations. Missing information is unknown.
use crate::runlet_progress::{Change, MAX_NODES, MAX_SOURCE, Node, Progress, State};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

#[derive(Default)]
pub(super) struct ScriptProgress {
    incarnation: u64,
    sequence: u64,
    digest: String,
    started: Option<Change>,
    last: Option<(u64, Change)>,
    terminal: Option<(u64, bool)>,
    known: bool,
    ended: bool,
    nodes: HashMap<String, Node>,
}
impl ScriptProgress {
    pub fn parent_finished(&mut self) {
        if !self.ended {
            self.invalidate();
        }
    }
    pub fn invalidate(&mut self) {
        self.known = false;
        self.nodes = HashMap::new();
    }
    pub fn retained(&self) -> bool {
        !self.nodes.is_empty()
    }
    pub fn apply_terminal(&mut self, event: &Progress, source: &str) {
        if !self.ended || event.incarnation != self.incarnation {
            self.invalidate();
            return;
        }
        self.apply(event, source);
    }
    pub fn apply(&mut self, event: &Progress, source: &str) {
        if !event.bounded() {
            self.invalidate();
            return;
        }
        if event.incarnation < self.incarnation {
            self.invalidate();
            return;
        }
        if let Change::Started { digest, healed } = &event.change {
            if event.incarnation == self.incarnation {
                if event.sequence != 0 || self.started.as_ref() != Some(&event.change) {
                    self.invalidate();
                }
                return;
            }
            self.invalidate();
            self.incarnation = event.incarnation;
            self.sequence = 0;
            self.started = Some(event.change.clone());
            self.last = None;
            self.terminal = None;
            self.ended = false;
            self.digest.clone_from(digest);
            self.known = event.sequence == 0
                && !healed
                && source.len() <= MAX_SOURCE
                && source_digest(source) == *digest;
            return;
        }
        if event.incarnation != self.incarnation {
            self.invalidate();
            self.incarnation = event.incarnation;
            return;
        }
        if matches!(event.change, Change::Finished { complete: false }) {
            self.invalidate();
            self.ended = true;
            return;
        }
        if !self.known {
            return;
        }
        if self.ended {
            let duplicate = match &event.change {
                Change::Step { .. } => {
                    self.last.as_ref() == Some(&(event.sequence, event.change.clone()))
                }
                Change::Finished { complete } => self.terminal == Some((event.sequence, *complete)),
                Change::Started { .. } => false,
            };
            if !duplicate {
                self.invalidate();
            }
            return;
        }
        if source.len() > MAX_SOURCE {
            self.invalidate();
            return;
        }
        match &event.change {
            Change::Step { node } => {
                // Duplicate delivery is idempotent. Any other reordering or gap
                // permanently invalidates this incarnation (there is no replay).
                if event.sequence == self.sequence {
                    if self.last.as_ref() != Some(&(event.sequence, event.change.clone())) {
                        self.invalidate();
                    }
                    return;
                }
                if event.sequence != self.sequence.saturating_add(1) {
                    self.invalidate();
                    return;
                }
                self.sequence = event.sequence;
                self.last = Some((event.sequence, event.change.clone()));
                if let Some(node) = node {
                    if let Some(previous) = self.nodes.get(&node.id)
                        && previous.call
                        && matches!(
                            previous.state,
                            State::Succeeded | State::Failed | State::Cancelled | State::Pruned
                        )
                        && previous.state != node.state
                    {
                        self.invalidate();
                        return;
                    }
                    if source.get(node.start..node.end).is_none() {
                        self.invalidate();
                        return;
                    }
                    if !self.nodes.contains_key(&node.id) && self.nodes.len() == MAX_NODES {
                        self.invalidate();
                        return;
                    }
                    self.nodes.insert(node.id.clone(), node.clone());
                }
            }
            Change::Finished { complete } => {
                self.ended = true;
                self.terminal = Some((event.sequence, *complete));
                if !complete || event.sequence != self.sequence {
                    self.invalidate();
                }
            }
            Change::Started { .. } => {}
        }
    }
    /// Annotations keyed by one-based source start line. Columns count Unicode
    /// scalar values, not UTF-8 bytes; ranges have an exclusive end position.
    pub fn labels(&self, source: &str) -> BTreeMap<usize, Vec<String>> {
        if !self.known || source.len() > MAX_SOURCE || source_digest(source) != self.digest {
            return BTreeMap::new();
        }
        let mut groups: BTreeMap<(usize, usize), BTreeMap<State, usize>> = BTreeMap::new();
        for node in self.nodes.values() {
            // Structural activity cannot establish that a call has been spawned.
            if !node.call {
                continue;
            }
            if self.ended
                && !matches!(
                    node.state,
                    State::Succeeded | State::Failed | State::Cancelled | State::Pruned
                )
            {
                continue;
            }
            *groups
                .entry((node.start, node.end))
                .or_default()
                .entry(node.state)
                .or_default() += 1;
        }
        let mut labels: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for ((start, end), states) in groups {
            let (line, column) = source_position(source, start);
            let (end_line, end_column) = source_position(source, end);
            let counts = states
                .into_iter()
                .map(|(state, count)| {
                    let label = match state {
                        State::Planned => "planned",
                        State::Blocked => "blocked",
                        State::Ready => "ready",
                        State::WaitingForCapacity => "waiting for capacity",
                        State::Running => "running",
                        State::Succeeded => "succeeded",
                        State::Failed => "failed",
                        State::Cancelling => "cancelling",
                        State::Cancelled => "cancelled",
                        State::Pruned => "pruned",
                    };
                    format!("{count} {label}")
                })
                .collect::<Vec<_>>()
                .join(", ");
            labels.entry(line).or_default().push(format!(
                "# call @L{line}:C{column}..L{end_line}:C{end_column}: {counts}"
            ));
        }
        labels
    }
}
pub(super) fn source_digest(source: &str) -> String {
    Sha256::digest(source.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn source_position(source: &str, byte: usize) -> (usize, usize) {
    // Accepted nodes have already been checked against these exact source bytes.
    let prefix = &source[..byte];
    (
        prefix.bytes().filter(|b| *b == b'\n').count() + 1,
        prefix
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .chars()
            .count()
            + 1,
    )
}
