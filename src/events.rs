//! Side channel that carries nested tool activity from `kit serve` to the
//! terminal client, along with the id of each persisted ACP session it opens.
//!
//! ACP reports the model-visible `compose` call, but every interesting thing
//! Kit does happens *inside* that call: the Runlet program dispatches shell,
//! edit, subagent, and A2A children concurrently. The terminal client renders
//! that as inline live script state, so it needs the child lifecycle.
//!
//! Rather than fork the ACP surface, the events ride on stderr — Kit's
//! diagnostics channel — as single JSON lines behind a control-character
//! marker. The terminal client owns the `serve` child process and pipes its
//! stderr, so marked lines become script-state updates and everything else
//! becomes log output. Emission is opt-in through `KIT_RUNTIME_EVENTS` so ordinary
//! ACP hosts never see the extra chatter.

use std::{
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
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// Process-wide progress transport lease/reset, not a source execution.
    RunletTransport { available: bool },
    /// Authoritative, value-free observations owned by an exact compose call.
    RunletProgress {
        progress: crate::runlet_progress::Progress,
    },
    /// Process-wide durability state, independent of the active ACP session.
    StorageStatus { pending: bool, exhausted: bool },
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
    /// A direct subagent registry lifecycle transition was committed.
    SubagentStateChanged {
        id: String,
        name: String,
        status: SubagentStatus,
        outcome: Option<GenerationOutcome>,
        generation: u64,
        task: String,
        parent_id: Option<String>,
        parent_name: Option<String>,
        harness: String,
        #[serde(default)]
        vendor: HarnessVendor,
        model: Option<String>,
        created_at_unix_ms: u64,
        generation_started_at_unix_ms: u64,
        generation_finished_at_unix_ms: Option<u64>,
    },
    /// A subagent's ACP session reported its context window occupancy.
    SubagentUsage { id: String, used: u64, size: u64 },
    /// Every observable strict descendant of an ancestor should be removed.
    SubagentDescendantsRemoved { ancestor_id: String },
}

/// The product behind an ACP harness, inferred from its launch command.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HarnessVendor {
    Kit,
    Claude,
    Codex,
    OpenCode,
    Copilot,
    Cursor,
    Pi,
    Antigravity,
    #[default]
    Unknown,
}

impl HarnessVendor {
    /// Classifies an argv-only launch line by its command basename and args.
    ///
    /// Tokens are matched whole so `copilot` never reads as `pi`, and
    /// `@agentclientprotocol/codex-acp@1.4.0` still yields `codex`.
    #[must_use]
    pub fn detect(command: &str, args: &[String]) -> Self {
        let basename = std::path::Path::new(command)
            .file_name()
            .map_or(command, |name| name.to_str().unwrap_or(command));
        let tokens = std::iter::once(basename)
            .chain(args.iter().map(String::as_str))
            .flat_map(|word| word.split(|c: char| !c.is_ascii_alphanumeric()))
            .filter(|token| !token.is_empty())
            .map(str::to_ascii_lowercase);
        for token in tokens {
            let vendor = match token.as_str() {
                "kit" => Self::Kit,
                "claude" => Self::Claude,
                "codex" => Self::Codex,
                "opencode" => Self::OpenCode,
                "copilot" => Self::Copilot,
                "cursor" => Self::Cursor,
                "pi" => Self::Pi,
                "antigravity" | "agy" => Self::Antigravity,
                _ => continue,
            };
            return vendor;
        }
        Self::Unknown
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Starting,
    Working,
    Idle,
    Removed,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GenerationOutcome {
    Success,
    Failed,
}

impl RuntimeEvent {
    /// Whether a parent Kit runtime should forward this child event unchanged.
    pub(crate) fn forward_from_child(&self) -> bool {
        matches!(
            self,
            Self::ChildStarted { .. }
                | Self::ChildFinished { .. }
                | Self::SubagentStateChanged { .. }
                | Self::SubagentUsage { .. }
                | Self::SubagentDescendantsRemoved { .. }
        )
    }

    /// The ACP tool call this child belongs to, when the id carries one.
    #[must_use]
    pub fn parent_call(&self) -> Option<&str> {
        let call = match self {
            Self::RunletProgress { progress } => return Some(&progress.owner),
            Self::ChildStarted { call, .. } | Self::ChildFinished { call, .. } => call,
            Self::RunletTransport { .. }
            | Self::StorageStatus { .. }
            | Self::SessionStarted { .. }
            | Self::CompactionStarted { .. }
            | Self::CompactionFinished { .. }
            | Self::SubagentStateChanged { .. }
            | Self::SubagentUsage { .. }
            | Self::SubagentDescendantsRemoved { .. } => return None,
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

/// Enqueues one event without waiting for stderr. Loss disables the transport;
/// its explicit reset (or the client lease on a stalled sink) hides source state.
pub fn emit(event: &RuntimeEvent) {
    if !enabled() {
        return;
    }
    if let Some(transport) = crate::runlet_progress::transport::global() {
        transport.publish_event(event);
    }
}

/// Parses one stderr line, returning an event when the line carries one.
#[must_use]
pub fn parse(line: &str) -> Option<RuntimeEvent> {
    let body = line.strip_prefix(EVENT_MARKER)?;
    if body.len() > 64 * 1024 {
        return None;
    }
    // Existing diagnostic events retain their historical parser shape. The new
    // bounded payload is checked before it can reach retained UI state.
    let event: RuntimeEvent = serde_json::from_str(body).ok()?;
    if let RuntimeEvent::RunletProgress { progress } = &event
        && (body.len() > 4096 || !progress.bounded())
    {
        return None;
    }
    Some(event)
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
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::disallowed_methods,
    clippy::disallowed_macros
)]
mod tests {
    use std::io::{self, Write};

    use serde_json::json;

    use super::{
        EVENT_MARKER, GenerationOutcome, HarnessVendor, RuntimeEvent, SubagentStatus, parse,
        summarize_input, summarize_output, test_support::write_event,
    };

    #[test]
    fn storage_status_events_round_trip_without_session_affinity() {
        for (pending, exhausted) in [(true, false), (false, false), (true, true)] {
            let event = RuntimeEvent::StorageStatus { pending, exhausted };
            let mut wire = Vec::new();
            write_event(&mut wire, &event);
            let wire = String::from_utf8(wire).unwrap();
            let parsed = parse(wire.trim_end()).unwrap();
            assert_eq!(parsed, event);
            assert_eq!(parsed.parent_call(), None);
        }
    }

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
    fn subagent_lifecycle_events_round_trip_with_stable_wire_names() {
        let changed = RuntimeEvent::SubagentStateChanged {
            id: "s-child".into(),
            name: "Scout".into(),
            status: SubagentStatus::Idle,
            outcome: Some(GenerationOutcome::Success),
            generation: 2,
            task: "Trace lifecycle events".into(),
            parent_id: None,
            parent_name: None,
            harness: "acp.kit".into(),
            vendor: crate::events::HarnessVendor::Kit,
            model: Some("test-model".into()),
            created_at_unix_ms: 1_000,
            generation_started_at_unix_ms: 2_000,
            generation_finished_at_unix_ms: Some(3_000),
        };
        let changed_json = serde_json::to_value(&changed).unwrap();
        assert_eq!(changed_json["event"], "subagent_state_changed");
        assert_eq!(changed_json["status"], "idle");
        assert_eq!(changed_json["outcome"], "success");
        assert_eq!(changed_json["created_at_unix_ms"], 1_000);
        assert_eq!(changed_json["generation_started_at_unix_ms"], 2_000);
        assert_eq!(changed_json["generation_finished_at_unix_ms"], 3_000);

        let removed = RuntimeEvent::SubagentDescendantsRemoved {
            ancestor_id: "s-parent".into(),
        };
        for event in [changed, removed] {
            let line = format!("{EVENT_MARKER}{}", serde_json::to_string(&event).unwrap());
            assert_eq!(parse(&line), Some(event.clone()));
            assert_eq!(event.parent_call(), None);
        }
    }

    #[test]
    fn nested_roster_events_round_trip_as_child_forwarding_payloads() {
        let changed = RuntimeEvent::SubagentStateChanged {
            id: "s-child".into(),
            name: "Scout".into(),
            status: SubagentStatus::Working,
            outcome: None,
            generation: 2,
            task: "inspect".into(),
            parent_id: Some("s-parent".into()),
            parent_name: Some("偵察 🦀".into()),
            harness: "acp.kit".into(),
            vendor: crate::events::HarnessVendor::Kit,
            model: None,
            created_at_unix_ms: 10,
            generation_started_at_unix_ms: 20,
            generation_finished_at_unix_ms: None,
        };
        let removed = RuntimeEvent::SubagentDescendantsRemoved {
            ancestor_id: "s-parent".into(),
        };

        for event in [changed, removed] {
            let line = format!("{EVENT_MARKER}{}", serde_json::to_string(&event).unwrap());
            let parsed = parse(&line).expect("nested roster event parses");
            assert_eq!(parsed, event);
            assert!(parsed.forward_from_child());
        }
    }

    #[test]
    fn subagent_usage_round_trips_and_forwards_from_children() {
        let usage = RuntimeEvent::SubagentUsage {
            id: "s-child".into(),
            used: 12_345,
            size: 200_000,
        };
        let json = serde_json::to_value(&usage).unwrap();
        assert_eq!(json["event"], "subagent_usage");
        let line = format!("{EVENT_MARKER}{}", serde_json::to_string(&usage).unwrap());
        let parsed = parse(&line).unwrap();
        assert_eq!(parsed, usage);
        assert!(parsed.forward_from_child());
        assert_eq!(parsed.parent_call(), None);
    }

    #[test]
    fn subagent_state_without_vendor_reads_as_unknown() {
        let line = format!(
            "{EVENT_MARKER}{}",
            json!({
                "event": "subagent_state_changed",
                "id": "s-old",
                "name": "Scout",
                "status": "working",
                "outcome": null,
                "generation": 1,
                "task": "inspect",
                "parent_id": null,
                "parent_name": null,
                "harness": "acp.kit",
                "model": null,
                "created_at_unix_ms": 1,
                "generation_started_at_unix_ms": 2,
                "generation_finished_at_unix_ms": null
            })
        );
        let Some(RuntimeEvent::SubagentStateChanged { vendor, .. }) = parse(&line) else {
            panic!("older roster line still parses");
        };
        assert_eq!(vendor, HarnessVendor::Unknown);
    }

    #[test]
    fn vendor_detection_reads_whole_tokens_from_command_and_args() {
        let args = |list: &[&str]| {
            list.iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            HarnessVendor::detect(
                "npx",
                &args(&["-y", "@agentclientprotocol/claude-agent-acp@0.69.0"])
            ),
            HarnessVendor::Claude
        );
        assert_eq!(
            HarnessVendor::detect(
                "npx",
                &args(&["-y", "@agentclientprotocol/codex-acp@1.4.0"])
            ),
            HarnessVendor::Codex
        );
        assert_eq!(
            HarnessVendor::detect("cursor-agent", &args(&["acp"])),
            HarnessVendor::Cursor
        );
        assert_eq!(
            HarnessVendor::detect("/usr/local/bin/opencode", &args(&["acp"])),
            HarnessVendor::OpenCode
        );
        assert_eq!(
            HarnessVendor::detect("copilot", &args(&["--acp"])),
            HarnessVendor::Copilot
        );
        assert_eq!(
            HarnessVendor::detect("pi", &args(&["--mode", "acp"])),
            HarnessVendor::Pi
        );
        assert_eq!(
            HarnessVendor::detect("npx", &args(&["pi-acp"])),
            HarnessVendor::Pi
        );
        assert_eq!(
            HarnessVendor::detect("agy", &args(&["acp"])),
            HarnessVendor::Antigravity
        );
        assert_eq!(
            HarnessVendor::detect("antigravity", &[]),
            HarnessVendor::Antigravity
        );
        assert_eq!(
            HarnessVendor::detect("/opt/kit/bin/kit", &args(&["acp"])),
            HarnessVendor::Kit
        );
        assert_eq!(
            HarnessVendor::detect("python3", &args(&["mock-acp.py"])),
            HarnessVendor::Unknown
        );
        assert_eq!(
            HarnessVendor::detect("Copilot.exe", &[]),
            HarnessVendor::Copilot
        );
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
    fn event_write_failures_are_observational() {
        struct BrokenWriter;
        impl Write for BrokenWriter {
            fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("broken event transport"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("broken event transport"))
            }
        }

        write_event(
            &mut BrokenWriter,
            &RuntimeEvent::SubagentDescendantsRemoved {
                ancestor_id: "s-parent".into(),
            },
        );
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

#[cfg(test)]
pub(crate) mod test_support;
