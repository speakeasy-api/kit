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

pub(crate) mod child;
pub(crate) mod terminal;

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod diff_tests;

const CAPACITY: usize = 256;
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

fn subscribers() -> usize {
    bus().receiver_count() + v2_bus().receiver_count()
}

// Separate ingress queues: v1 must never lag because of v2-only traffic,
// including when clients of both protocol versions coexist.
struct Buses {
    v1: broadcast::Sender<Update>,
    v2: broadcast::Sender<Update>,
}

impl Buses {
    fn new() -> Self {
        Self {
            v1: broadcast::channel(CAPACITY).0,
            v2: broadcast::channel(CAPACITY).0,
        }
    }

    fn publish(&self, update: Update) {
        if !update.v2_only() {
            let _ = self.v1.send(update.clone());
        }
        let _ = self.v2.send(update);
    }
}

fn buses() -> &'static Buses {
    static BUSES: OnceLock<Buses> = OnceLock::new();
    BUSES.get_or_init(Buses::new)
}

fn v2_bus() -> &'static broadcast::Sender<Update> {
    &buses().v2
}
fn bus() -> &'static broadcast::Sender<Update> {
    &buses().v1
}
fn publish(update: Update) {
    buses().publish(update);
}

/// An invocation owns its terminal update, including cancellation/unwind.
pub(crate) struct Invocation(Update);

impl Invocation {
    pub(crate) fn start(request: &ToolRequest, root: Option<&Path>) -> Option<Self> {
        if subscribers() == 0
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
            "tool_schema" => ("Reading tool schema", "read"),
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
        publish(update.clone());
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
        publish(self.0.clone());
    }
}

/// Publish a location established by the tool itself, not a guessed source line.
pub(crate) fn location(request: &ToolRequest, path: &Path, line: u32) {
    if subscribers() == 0
        || request.session_id.0.len() > MAX_ID
        || request.call_id.0.len() > MAX_ID
        || !request.call_id.0.contains(":compose:")
    {
        return;
    }
    let Some(locations) = location_value(path, Some(line)) else {
        return;
    };
    publish(Update {
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

/// Maximum combined UTF-8 before/after bytes retained per diff. Larger diffs are
/// omitted, never truncated into a misleading file replacement. JSON escaping
/// expands this by at most six, in addition to bounded path/identity overhead.
const MAX_DIFF_TEXT: usize = 16 * 1024;

/// Capture a deletion without making unreadable, binary, or large files fail an
/// otherwise valid delete. Reads stop at the bound even if the file grows.
pub(crate) fn deletion_text(path: &Path) -> Option<String> {
    use std::io::Read;
    if subscribers() == 0 {
        return None;
    }
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_DIFF_TEXT as u64 {
        return None;
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take((MAX_DIFF_TEXT + 1) as u64)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() <= MAX_DIFF_TEXT).then_some(text)
}

/// Publish only committed file changes. None denotes an absent file, not empty
/// text. The patch contains both wire shapes; each protocol's typed decoder
/// retains only its own fields. No renderable v2 git patch is synthesized.
pub(crate) fn diff(request: &ToolRequest, path: &Path, old: Option<&str>, new: Option<&str>) {
    if subscribers() == 0
        || request.session_id.0.len() > MAX_ID
        || request.call_id.0.len() > MAX_ID
        || !request.call_id.0.contains(":compose:")
    {
        return;
    }
    let Some(content) = diff_content(path, old, new) else {
        return;
    };
    publish(Update {
        session: request.session_id.0.clone(),
        call: request.call_id.0.clone(),
        start: None,
        patch: Some(Value::Object(Map::from_iter([
            ("toolCallId".into(), Value::from(request.call_id.0.clone())),
            ("content".into(), Value::Array(vec![content])),
        ]))),
        ok: false,
    });
}

/// Shared conversion for local committed edits and captured v1 child snapshots.
fn diff_content(path: &Path, old: Option<&str>, new: Option<&str>) -> Option<Value> {
    if old
        .map_or(0, str::len)
        .saturating_add(new.map_or(0, str::len))
        > MAX_DIFF_TEXT
        || location_value(path, None).is_none()
    {
        return None;
    }
    let operation = match (old, new) {
        (None, Some(_)) => "add",
        (Some(_), None) => "delete",
        (Some(_), Some(_)) => "modify",
        (None, None) => return None,
    };
    Some(Value::Object(Map::from_iter([
        ("type".into(), Value::from("diff")),
        ("path".into(), Value::from(path.to_str())),
        ("oldText".into(), Value::from(old)),
        ("newText".into(), Value::from(new.unwrap_or_default())),
        (
            "changes".into(),
            Value::Array(vec![Value::Object(Map::from_iter([
                ("operation".into(), Value::from(operation)),
                ("path".into(), Value::from(path.to_str())),
                ("fileType".into(), Value::from("text")),
            ]))]),
        ),
    ])))
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
        Self::with_receiver(bus().subscribe(), session, send)
    }

    pub(super) fn start_v2(
        session: String,
        send: impl Fn(Update) -> bool + Send + 'static,
    ) -> Self {
        Self::with_receiver(v2_bus().subscribe(), session, send)
    }

    fn with_receiver(
        receiver: broadcast::Receiver<Update>,
        session: String,
        send: impl Fn(Update) -> bool + Send + 'static,
    ) -> Self {
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
    let mut active = HashMap::new();
    let mut budget = terminal::Budget::default();
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
                    result = forward_event(
                        event,
                        &mut receiver,
                        &session,
                        &mut active,
                        &mut budget,
                        &send,
                    );
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
                if forward_event(
                    event,
                    &mut receiver,
                    &session,
                    &mut active,
                    &mut budget,
                    &send,
                )
                .is_err()
                {
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
    active: &mut HashMap<String, HashMap<String, terminal::State>>,
    budget: &mut terminal::Budget,
    send: &impl Fn(Update) -> bool,
) -> Result<(), agentkit_acp::AcpRuntimeError> {
    match event {
        Ok(mut update) if update.session == session => {
            if update.start.is_some() {
                if active.len() >= CAPACITY || active.contains_key(&update.call) {
                    return Ok(());
                }
                active.insert(update.call.clone(), HashMap::new());
            } else if let Some(patch) = &update.patch {
                let Some(terminals) = active.get_mut(&update.call) else {
                    return Ok(());
                };
                let kind = patch.get("sessionUpdate").and_then(Value::as_str);
                if matches!(kind, Some("terminal_update" | "terminal_output_chunk")) {
                    let Some(id) = patch
                        .get("terminalId")
                        .and_then(Value::as_str)
                        .filter(|id| id.len() <= MAX_ID)
                    else {
                        return Ok(());
                    };
                    if !terminals.contains_key(id)
                        && (terminals.len() >= CAPACITY / 4
                            || kind == Some("terminal_output_chunk"))
                    {
                        return Ok(());
                    }
                    let state = terminals
                        .entry(id.to_owned())
                        .or_insert_with(terminal::State::running);
                    if kind == Some("terminal_update")
                        && let Some(exit) = patch.get("exitStatus")
                    {
                        // Omission preserves the last state; null explicitly clears
                        // an exit, and only an object declares the terminal exited.
                        if exit.is_null() {
                            state.running = true;
                        } else if exit.is_object() {
                            state.running = false;
                        }
                    }
                    if !budget.admit(&mut update, state) {
                        return Ok(());
                    }
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
            for (call, terminals) in active.drain() {
                // Child replay terminals have independent IDs. Invalidate each
                // affected stream, without asserting a child process has exited.
                for (id, _) in terminals.into_iter().filter(|(_, state)| state.running) {
                    let mut patch = Map::from_iter([
                        ("sessionUpdate".into(), Value::from("terminal_update")),
                        ("terminalId".into(), Value::from(id.clone())),
                        (
                            "_meta".into(),
                            Value::Object(Map::from_iter([(
                                "kit/outputIncomplete".into(),
                                Value::from(true),
                            )])),
                        ),
                    ]);
                    if id == call {
                        patch.insert("exitStatus".into(), Value::Object(Map::new()));
                    }
                    if !send(Update {
                        session: session.into(),
                        call: call.clone(),
                        start: None,
                        patch: Some(Value::Object(patch)),
                        ok: false,
                    }) {
                        return Err(delivery_error());
                    }
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
