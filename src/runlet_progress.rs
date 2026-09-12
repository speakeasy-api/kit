//! Bounded, value-free adapter. Ownership is captured from the bridge envelope,
//! never from thread-local operation context. The consumer lives inside execute.
use crate::events::RuntimeEvent;
pub(crate) mod transport;
use agentkit_tool_compose::{
    BackendRun, ComposeOutcome, RunletBackend, RunletProgress, RunletProgressEnd,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{num::NonZeroUsize, time::Duration};
use transport::Transport;

pub const MAX_SOURCE: usize = 64 * 1024;
#[cfg(feature = "tui")]
pub const MAX_NODES: usize = 256;
#[cfg(feature = "tui")]
pub const MAX_RUNS: usize = 32;
const MAX_ID: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub call: bool,
    pub start: usize,
    pub end: usize,
    pub state: State,
    pub attempt: u32,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Planned,
    Blocked,
    Ready,
    WaitingForCapacity,
    Running,
    Succeeded,
    Failed,
    Cancelling,
    Cancelled,
    Pruned,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Change {
    Started {
        digest: String,
        healed: bool,
    },
    /// Every upstream sequence is carried, even relationships we do not render.
    Step {
        node: Option<Node>,
    },
    Finished {
        complete: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Progress {
    pub owner: String,
    pub incarnation: u64,
    pub sequence: u64,
    pub change: Change,
}
impl Progress {
    pub fn bounded(&self) -> bool {
        self.owner.len() <= MAX_ID
            && self.incarnation != 0
            && match &self.change {
                Change::Started { digest, .. } => {
                    digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
                }
                Change::Step { node: Some(n) } => {
                    n.id.len() <= MAX_ID && n.start <= n.end && n.end <= MAX_SOURCE
                }
                _ => true,
            }
    }
}

/// Pollable diagnostic adapter; each poll returns at most one bounded event.
/// Dropping an unfinished adapter emits invalidation for its captured owner.
pub(crate) struct Active {
    receiver: RunletProgress,
    transport: Transport,
    sequence: u64,
    started: bool,
    ended: bool,
}
impl Active {
    pub(crate) fn new(receiver: RunletProgress, transport: Transport) -> Self {
        Self {
            receiver,
            transport,
            sequence: 0,
            started: false,
            ended: false,
        }
    }
    fn event(&self, change: Change) -> RuntimeEvent {
        RuntimeEvent::RunletProgress {
            progress: Progress {
                owner: self.receiver.parent_call_id.0.clone(),
                incarnation: self.receiver.incarnation,
                sequence: self.sequence,
                change,
            },
        }
    }
    pub(crate) fn poll(&mut self) -> Option<RuntimeEvent> {
        if self.ended {
            return None;
        }
        if !self.started {
            self.started = true;
            return Some(self.event(Change::Started {
                digest: self.receiver.source_digest.clone(),
                healed: self.receiver.healed,
            }));
        }
        let change = match self.receiver.try_recv() {
            Ok(Some(event)) => {
                self.sequence = event.sequence;
                let node = match event.change {
                    runlet::ProgressChange::NodeAdded(node)
                    | runlet::ProgressChange::NodeUpdated(node) => Some(Node::from(node)),
                    runlet::ProgressChange::EdgeAdded(_) => None,
                    // The bridge suppresses raw runtime completion. Never treat
                    // it as host-authoritative completion if the contract changes.
                    runlet::ProgressChange::Finished(_) => {
                        self.ended = true;
                        return Some(self.event(Change::Finished { complete: false }));
                    }
                };
                if node
                    .as_ref()
                    .is_some_and(|n| n.id.len() > MAX_ID || n.end > MAX_SOURCE)
                {
                    self.ended = true;
                    Change::Finished { complete: false }
                } else {
                    Change::Step { node }
                }
            }
            Ok(None) => return None,
            Err(end) => {
                self.ended = true;
                Change::Finished {
                    complete: matches!(
                        end,
                        RunletProgressEnd::Succeeded | RunletProgressEnd::Failed
                    ),
                }
            }
        };
        Some(self.event(change))
    }
}
impl From<runlet::ProgressNode> for Node {
    fn from(node: runlet::ProgressNode) -> Self {
        Self {
            id: node.id,
            call: matches!(node.kind, runlet::NodeKind::Call),
            start: node.span.start,
            end: node.span.end,
            state: node.state.into(),
            attempt: node.attempt,
        }
    }
}
impl From<runlet::ProgressState> for State {
    fn from(state: runlet::ProgressState) -> Self {
        match state {
            runlet::ProgressState::Planned => Self::Planned,
            runlet::ProgressState::Blocked => Self::Blocked,
            runlet::ProgressState::Ready => Self::Ready,
            runlet::ProgressState::WaitingForCapacity => Self::WaitingForCapacity,
            runlet::ProgressState::Running => Self::Running,
            runlet::ProgressState::Succeeded => Self::Succeeded,
            runlet::ProgressState::Failed => Self::Failed,
            runlet::ProgressState::Cancelling => Self::Cancelling,
            runlet::ProgressState::Cancelled => Self::Cancelled,
            runlet::ProgressState::Pruned => Self::Pruned,
        }
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        if self.started
            && !self.ended
            && let RuntimeEvent::RunletProgress { progress } =
                self.event(Change::Finished { complete: false })
        {
            self.transport.publish(progress);
        }
    }
}

pub async fn execute(run: BackendRun) -> Result<Value, ComposeOutcome> {
    let Some(transport) = transport::global() else {
        use agentkit_tool_compose::ComposeBackend;
        return RunletBackend.execute(run).await;
    };
    execute_observed(run, transport).await
}

pub(crate) async fn execute_observed(
    run: BackendRun,
    transport: &Transport,
) -> Result<Value, ComposeOutcome> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(2);
    let execute = RunletBackend.execute_with_progress(
        run,
        tx,
        NonZeroUsize::new(1024).unwrap_or(NonZeroUsize::MIN),
    );
    let consume = async move {
        let mut active: Vec<Active> = Vec::new();
        let mut closed = false;
        let mut tick = tokio::time::interval(Duration::from_millis(25));
        loop {
            if closed {
                tick.tick().await;
            } else {
                use futures_util::future::{Either, select};
                match select(std::pin::pin!(rx.recv()), std::pin::pin!(tick.tick())).await {
                    Either::Left((envelope, _)) => {
                        match envelope {
                            Some(receiver) => {
                                if active.len() == 2 {
                                    active.remove(0);
                                }
                                active.push(Active::new(receiver, transport.clone()));
                            }
                            None => closed = true,
                        }
                        continue;
                    }
                    Either::Right(_) => {}
                }
            }
            for run in &mut active {
                for _ in 0..512 {
                    let Some(event) = run.poll() else {
                        break;
                    };
                    if let RuntimeEvent::RunletProgress { progress } = event {
                        transport.publish(progress);
                    }
                }
            }
            active.retain(|run| !run.ended);
            if closed && active.is_empty() {
                break;
            }
        }
    };
    let (result, ()) = futures_util::future::join(execute, consume).await;
    result
}

/// Retain bounded source bytes even when an ACP caller submits oversized input.
#[cfg(feature = "tui")]
pub(crate) fn bounded_source(source: String) -> String {
    if source.len() <= MAX_SOURCE {
        return source;
    }
    let mut end = MAX_SOURCE - 32;
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… source truncated", &source[..end])
}
