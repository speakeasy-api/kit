//! Side channel that carries nested tool activity from `kit serve` to the
//! terminal client, along with the id of each persisted ACP session it opens.
//!
//! ACP reports the model-visible `compose` call, but every interesting thing
//! Kit does happens *inside* that call: the Runlet program dispatches shell,
//! edit, subagent, and A2A children concurrently. The terminal client renders
//! that as a live runtime graph, so it needs the child lifecycle.
//!
//! Rather than fork the ACP surface, the events ride on stderr — Kit's
//! diagnostics channel — as single JSON lines behind a control-character
//! marker. The terminal client owns the `serve` child process and pipes its
//! stderr, so marked lines become graph updates and everything else becomes
//! log output. Emission is opt-in through `KIT_RUNTIME_EVENTS` so ordinary
//! ACP hosts never see the extra chatter.

use std::{
    io::Write,
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Prefix that distinguishes an event line from a diagnostic line. The
/// leading control character cannot appear in ordinary log text.
pub const EVENT_MARKER: &str = "\u{1}kit-runtime\u{1}";

/// Environment variable that turns emission on for a `serve` process.
pub const EVENTS_ENV: &str = "KIT_RUNTIME_EVENTS";

/// One runtime event sent privately to the terminal client.
///
/// `call` is the compose child call id, shaped `<parent>:compose:<operation>`,
/// so a client can attribute every child to the ACP tool call it belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// A persisted ACP session was opened by the child runtime.
    SessionStarted { session_id: String },
    /// A nested tool call started running.
    ChildStarted {
        call: String,
        tool: String,
        summary: String,
        at: u64,
    },
    /// A nested tool call finished, successfully or not.
    ChildFinished {
        call: String,
        tool: String,
        ok: bool,
        summary: String,
        millis: u64,
    },
    /// Automatic transcript compaction started.
    CompactionStarted { reason: String, at: u64 },
    /// Automatic transcript compaction finished.
    CompactionFinished {
        reason: String,
        ok: bool,
        compacted: bool,
        millis: u64,
    },
}

impl RuntimeEvent {
    /// The ACP tool call this child belongs to, when the id carries one.
    #[must_use]
    pub fn parent_call(&self) -> Option<&str> {
        let call = match self {
            Self::ChildStarted { call, .. } | Self::ChildFinished { call, .. } => call,
            Self::SessionStarted { .. }
            | Self::CompactionStarted { .. }
            | Self::CompactionFinished { .. } => return None,
        };
        call.rsplit_once(":compose:").map(|(parent, _)| parent)
    }
}

/// Whether this process should emit runtime events.
#[must_use]
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os(EVENTS_ENV).is_some())
}

/// Writes one event to stderr when emission is enabled.
pub fn emit(event: &RuntimeEvent) {
    if !enabled() {
        return;
    }
    if let Ok(line) = serde_json::to_string(event) {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{EVENT_MARKER}{line}");
    }
}

/// Parses one stderr line, returning an event when the line carries one.
#[must_use]
pub fn parse(line: &str) -> Option<RuntimeEvent> {
    serde_json::from_str(line.strip_prefix(EVENT_MARKER)?).ok()
}

/// Milliseconds since the Unix epoch, saturating at zero on a broken clock.
#[must_use]
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// One short line describing what a nested call was asked to do.
#[must_use]
pub fn summarize_input(input: &Value) -> String {
    subject(
        input,
        &[
            "command", "path", "file", "prompt", "task", "query", "url", "message",
        ],
    )
}

/// One short line describing what a nested call produced.
#[must_use]
pub fn summarize_output(output: &Value) -> String {
    subject(
        output,
        &["stdout", "text", "message", "summary", "path", "error"],
    )
}

/// One short line describing a tool payload.
///
/// Tool inputs and outputs are small JSON objects whose most descriptive field
/// differs per tool, so the first field that reads like a subject wins, and
/// anything unexpected falls back to compact JSON.
fn subject(value: &Value, keys: &[&str]) -> String {
    let named = value.as_object().and_then(|fields| {
        keys.iter()
            .filter_map(|key| fields.get(*key))
            .find(|field| !matches!(field, Value::String(text) if text.trim().is_empty()))
            .map(render_value)
    });
    truncate(&named.unwrap_or_else(|| render_value(value)), 160)
}

fn render_value(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{EVENT_MARKER, RuntimeEvent, parse, summarize_input, summarize_output};

    #[test]
    fn reads_back_an_emitted_event_line() {
        let event = RuntimeEvent::ChildStarted {
            call: "call-1:compose:abcdef".into(),
            tool: "shell".into(),
            summary: "ls".into(),
            at: 7,
        };
        let line = format!("{EVENT_MARKER}{}", serde_json::to_string(&event).unwrap());
        let parsed = parse(&line).expect("event round trips");
        assert_eq!(parsed.parent_call(), Some("call-1"));
    }

    #[test]
    fn session_started_carries_one_durable_identity() {
        let event = RuntimeEvent::SessionStarted {
            session_id: "s-123-4-5".into(),
        };
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({"event": "session_started", "session_id": "s-123-4-5"})
        );
    }

    #[test]
    fn reads_back_a_compaction_event() {
        let event = RuntimeEvent::CompactionStarted {
            reason: "TokenThreshold".into(),
            at: 7,
        };
        let line = format!("{EVENT_MARKER}{}", serde_json::to_string(&event).unwrap());
        let parsed = parse(&line).expect("event round trips");
        assert!(matches!(parsed, RuntimeEvent::CompactionStarted { .. }));
        assert_eq!(parsed.parent_call(), None);
    }

    #[test]
    fn ignores_ordinary_diagnostics() {
        assert!(parse("listening on 127.0.0.1:7331").is_none());
    }

    #[test]
    fn summarizes_by_the_most_descriptive_field() {
        assert_eq!(
            summarize_input(&json!({ "timeout_seconds": 30, "command": "cargo test" })),
            "cargo test"
        );
        assert_eq!(
            summarize_output(&json!({ "success": true, "stdout": "ok\nrest" })),
            "ok"
        );
        assert_eq!(
            summarize_output(&json!({ "success": true, "stdout": "" })),
            "{\"success\":true,\"stdout\":\"\"}"
        );
    }
}
