//! Best-effort projection at the evaluated hidden-tool invocation boundary.
//! No execution waits or accumulated source/input/output retention. The compose
//! budget bounds cards per run; the bus and receiver bound queued/active cards.
//! Rich-content patches carry bounded payloads only for the lifetime of delivery.
use std::{collections::HashMap, path::Path, sync::OnceLock};

use agentkit_tools_core::ToolRequest;
use serde_json::{Map, Value};
use tokio::sync::broadcast;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests;

pub(crate) mod terminal;

const CAPACITY: usize = crate::runlet_progress::MAX_NODES;
const MAX_ID: usize = 256;
const MAX_PATH: usize = 4096;

#[derive(Clone, Debug)]
pub(crate) struct Update {
    session: String,
    call: String,
    start: Option<Value>,
    patch: Option<Value>,
    ok: bool,
}

fn bus() -> &'static broadcast::Sender<Update> {
    static BUS: OnceLock<broadcast::Sender<Update>> = OnceLock::new();
    BUS.get_or_init(|| broadcast::channel(CAPACITY).0)
}

/// An invocation owns its terminal update, including cancellation/unwind.
pub(crate) struct Invocation(Update);

impl Invocation {
    pub(crate) fn start(request: &ToolRequest, root: Option<&Path>) -> Option<Self> {
        if bus().receiver_count() == 0
            || request.session_id.0.len() > MAX_ID
            || request.call_id.0.len() > MAX_ID
        {
            return None;
        }
        let (owner, _) = request.call_id.0.rsplit_once(":compose:")?;
        let name = request.tool_name.0.as_str();
        if name.len() > 128 {
            return None;
        }
        let (title, kind) = match name {
            "shell" => ("Running shell command", "execute"),
            "edit" => ("Editing file", "edit"),
            "read_file" => ("Reading image", "read"),
            "tool_search" => ("Searching available tools", "search"),
            "docs" => ("Fetching Kit documentation", "fetch"),
            "subagent" => ("Starting subagent", "other"),
            "prompt" => ("Prompting subagent", "other"),
            "fork" => ("Forking session", "other"),
            _ => ("Calling tool", "other"),
        };
        let mut start = Map::from_iter([
            ("toolCallId".into(), Value::from(request.call_id.0.clone())),
            ("name".into(), Value::from(name)),
            ("title".into(), Value::from(title)),
            ("kind".into(), Value::from(kind)),
            ("status".into(), Value::from("in_progress")),
            (
                "_meta".into(),
                Value::Object(Map::from_iter([(
                    "kit/parentToolCallId".into(),
                    Value::from(owner),
                )])),
            ),
        ]);
        if matches!(name, "edit" | "read_file")
            && let Some(path) = request.input.get("path").and_then(Value::as_str)
            && path.len() <= MAX_PATH
        {
            let path = Path::new(path);
            let absolute = if path.is_absolute() {
                Some(path.to_path_buf())
            } else {
                root.map(|root| root.join(path))
            };
            if let Some(path) = absolute
                && let Some(locations) = location_value(&path, None)
            {
                start.insert("locations".into(), locations);
            }
        }
        let update = Update {
            session: request.session_id.0.clone(),
            call: request.call_id.0.clone(),
            start: Some(Value::Object(start)),
            patch: None,
            ok: false,
        };
        let _ = bus().send(update.clone());
        Some(Self(Update {
            start: None,
            ..update
        }))
    }

    pub(crate) fn finish(mut self, ok: bool) {
        self.0.ok = ok;
    }
}

impl Drop for Invocation {
    fn drop(&mut self) {
        let _ = bus().send(self.0.clone());
    }
}

/// Publish a location established by the tool itself, not a guessed source line.
pub(crate) fn location(request: &ToolRequest, path: &Path, line: u32) {
    if bus().receiver_count() == 0
        || request.session_id.0.len() > MAX_ID
        || request.call_id.0.len() > MAX_ID
        || !request.call_id.0.contains(":compose:")
    {
        return;
    }
    let Some(locations) = location_value(path, Some(line)) else {
        return;
    };
    let _ = bus().send(Update {
        session: request.session_id.0.clone(),
        call: request.call_id.0.clone(),
        start: None,
        patch: Some(Value::Object(Map::from_iter([
            ("toolCallId".into(), Value::from(request.call_id.0.clone())),
            ("locations".into(), locations),
        ]))),
        ok: false,
    });
}

fn location_value(path: &Path, line: Option<u32>) -> Option<Value> {
    if !path.is_absolute() || path.as_os_str().len() > MAX_PATH {
        return None;
    }
    let mut location = Map::from_iter([("path".into(), Value::from(path.to_str()?))]);
    if let Some(line) = line {
        location.insert("line".into(), Value::from(line));
    }
    Some(Value::Array(vec![Value::Object(location)]))
}

impl Update {
    fn value(&self) -> Value {
        self.start
            .clone()
            .or_else(|| self.patch.clone())
            .unwrap_or_else(|| {
                Value::Object(Map::from_iter([
                    ("toolCallId".into(), Value::from(self.call.clone())),
                    (
                        "status".into(),
                        Value::from(if self.ok { "completed" } else { "failed" }),
                    ),
                ]))
            })
    }

    pub(crate) fn v2_only(&self) -> bool {
        self.patch
            .as_ref()
            .is_some_and(|patch| patch.get("sessionUpdate").is_some())
    }

    pub(crate) fn v1(&self) -> Result<agentkit_acp::SessionUpdate, serde_json::Error> {
        if self.start.is_some() {
            serde_json::from_value(self.value()).map(agentkit_acp::SessionUpdate::ToolCall)
        } else {
            serde_json::from_value(self.value()).map(agentkit_acp::SessionUpdate::ToolCallUpdate)
        }
    }

    pub(crate) fn v2(&self) -> Result<agentkit_acp::v2::wire::SessionUpdate, serde_json::Error> {
        if self.v2_only() {
            serde_json::from_value(self.value())
        } else {
            serde_json::from_value(self.value())
                .map(agentkit_acp::v2::wire::SessionUpdate::ToolCallUpdate)
        }
    }
}

/// The session activity owns this subscription; its last clone aborts delivery.
/// Slow consumers invalidate active cards on loss instead of leaving them running.
pub(super) struct Subscription {
    task: tokio::task::JoinHandle<()>,
    drains: tokio::sync::mpsc::Sender<Drain>,
}

type Drain = tokio::sync::oneshot::Sender<Result<(), agentkit_acp::AcpRuntimeError>>;

impl Subscription {
    pub(super) fn start(session: String, send: impl Fn(Update) -> bool + Send + 'static) -> Self {
        let receiver = bus().subscribe();
        let (drains, commands) = tokio::sync::mpsc::channel(1);
        Self {
            task: tokio::spawn(forward(receiver, session, send, commands)),
            drains,
        }
    }

    /// Fence already-published frames, not running tools. Only session finalization
    /// waits here; publication from an invocation stays synchronous and bounded.
    pub(super) async fn drain(&self) -> Result<(), agentkit_acp::AcpRuntimeError> {
        let (reply, response) = tokio::sync::oneshot::channel();
        self.drains
            .send(reply)
            .await
            .map_err(|_| delivery_error())?;
        response.await.map_err(|_| delivery_error())?
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn delivery_error() -> agentkit_acp::AcpRuntimeError {
    agentkit_acp::AcpRuntimeError::Loop("inner tool projection delivery failed".into())
}

async fn forward(
    mut receiver: broadcast::Receiver<Update>,
    session: String,
    send: impl Fn(Update) -> bool,
    mut drains: tokio::sync::mpsc::Receiver<Drain>,
) {
    use futures_util::future::{Either, select};
    // The bool tracks whether this card has a live v2 terminal.
    let mut active = HashMap::new();
    let mut drains_open = true;
    loop {
        let next = if drains_open {
            match select(
                std::pin::pin!(drains.recv()),
                std::pin::pin!(receiver.recv()),
            )
            .await
            {
                Either::Left((Some(reply), _)) => Either::Left(reply),
                Either::Left((None, _)) => {
                    drains_open = false;
                    continue;
                }
                Either::Right((event, _)) => Either::Right(event),
            }
        } else {
            Either::Right(receiver.recv().await)
        };
        match next {
            Either::Left(reply) => {
                // Snapshot the pending work: concurrent background tools must not
                // turn this fence into an unbounded wait for future publication.
                let mut result = Ok(());
                for _ in 0..receiver.len().min(CAPACITY) {
                    let event = match receiver.try_recv() {
                        Ok(update) => Ok(update),
                        Err(broadcast::error::TryRecvError::Empty) => break,
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            Err(broadcast::error::RecvError::Lagged(n))
                        }
                        Err(broadcast::error::TryRecvError::Closed) => {
                            Err(broadcast::error::RecvError::Closed)
                        }
                    };
                    result = forward_event(event, &mut receiver, &session, &mut active, &send);
                    if result.is_err() {
                        break;
                    }
                }
                let failed = result.is_err();
                let _ = reply.send(result);
                if failed {
                    return;
                }
            }
            Either::Right(event) => {
                if forward_event(event, &mut receiver, &session, &mut active, &send).is_err() {
                    return;
                }
            }
        }
    }
}

fn forward_event(
    event: Result<Update, broadcast::error::RecvError>,
    receiver: &mut broadcast::Receiver<Update>,
    session: &str,
    active: &mut HashMap<String, bool>,
    send: &impl Fn(Update) -> bool,
) -> Result<(), agentkit_acp::AcpRuntimeError> {
    match event {
        Ok(update) if update.session == session => {
            if update.start.is_some() {
                if active.len() >= CAPACITY || active.contains_key(&update.call) {
                    return Ok(());
                }
                active.insert(update.call.clone(), false);
            } else if let Some(patch) = &update.patch {
                let Some(terminal_running) = active.get_mut(&update.call) else {
                    return Ok(());
                };
                if patch.get("sessionUpdate").and_then(Value::as_str) == Some("terminal_update") {
                    *terminal_running = patch.get("exitStatus").is_none();
                }
            } else if active.remove(&update.call).is_none() {
                return Ok(());
            }
            if !send(update) {
                return Err(delivery_error());
            }
        }
        Ok(_) => {}
        Err(error) => {
            for (call, terminal_running) in active.drain() {
                // Loss invalidates the stream as well as its card. Do not leave
                // an editor waiting for a terminal exit frame that was dropped.
                if terminal_running
                    && !send(Update {
                        session: session.into(),
                        call: call.clone(),
                        start: None,
                        patch: Some(Value::Object(Map::from_iter([
                            ("sessionUpdate".into(), Value::from("terminal_update")),
                            ("terminalId".into(), Value::from(call.clone())),
                            ("exitStatus".into(), Value::Object(Map::new())),
                            (
                                "_meta".into(),
                                Value::Object(Map::from_iter([(
                                    "kit/outputIncomplete".into(),
                                    Value::from(true),
                                )])),
                            ),
                        ]))),
                        ok: false,
                    })
                {
                    return Err(delivery_error());
                }
                if !send(Update {
                    session: session.into(),
                    call,
                    start: None,
                    patch: None,
                    ok: false,
                }) {
                    return Err(delivery_error());
                }
            }
            if matches!(error, broadcast::error::RecvError::Closed) {
                return Err(delivery_error());
            }
            // Discard the stale tail after a gap; fresh starts re-establish cards.
            *receiver = receiver.resubscribe();
        }
    }
    Ok(())
}
