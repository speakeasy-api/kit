//! Best-effort projection at the evaluated hidden-tool invocation boundary.
//! No source/input/output retention and no execution waits. The compose call
//! budget bounds cards per run; the bus and receiver bound queued/active cards.
use std::{collections::HashSet, path::Path, sync::OnceLock};

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

    pub(crate) fn v1(&self) -> Result<agentkit_acp::SessionUpdate, serde_json::Error> {
        if self.start.is_some() {
            serde_json::from_value(self.value()).map(agentkit_acp::SessionUpdate::ToolCall)
        } else {
            serde_json::from_value(self.value()).map(agentkit_acp::SessionUpdate::ToolCallUpdate)
        }
    }

    pub(crate) fn v2(&self) -> Result<agentkit_acp::v2::wire::SessionUpdate, serde_json::Error> {
        serde_json::from_value(self.value())
            .map(agentkit_acp::v2::wire::SessionUpdate::ToolCallUpdate)
    }
}

/// The observer owns this subscription; dropping its last clone aborts delivery.
/// Slow consumers invalidate active cards on loss instead of leaving them running.
pub(super) struct Subscription(tokio::task::JoinHandle<()>);

impl Subscription {
    pub(super) fn start(session: String, send: impl Fn(Update) -> bool + Send + 'static) -> Self {
        let receiver = bus().subscribe();
        Self(tokio::spawn(forward(receiver, session, send)))
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn forward(
    mut receiver: broadcast::Receiver<Update>,
    session: String,
    send: impl Fn(Update) -> bool,
) {
    let mut active = HashSet::new();
    loop {
        match receiver.recv().await {
            Ok(update) if update.session == session => {
                if update.start.is_some() {
                    if active.len() >= CAPACITY || !active.insert(update.call.clone()) {
                        continue;
                    }
                } else if update.patch.is_some() {
                    if !active.contains(&update.call) {
                        continue;
                    }
                } else if !active.remove(&update.call) {
                    continue;
                }
                if !send(update) {
                    break;
                }
            }
            Ok(_) => {}
            Err(error) => {
                for call in active.drain() {
                    if !send(Update {
                        session: session.clone(),
                        call,
                        start: None,
                        patch: None,
                        ok: false,
                    }) {
                        break;
                    }
                }
                if matches!(error, broadcast::error::RecvError::Closed) {
                    break;
                }
                // Discard the stale tail after a gap. Unknown completions are
                // ignored; subsequent starts can establish fresh cards.
                receiver = receiver.resubscribe();
            }
        }
    }
}
