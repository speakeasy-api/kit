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
    collections::HashMap,
    io::Write,
    sync::{Mutex, OnceLock},
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
        model: Option<String>,
        created_at_unix_ms: u64,
        generation_started_at_unix_ms: u64,
        generation_finished_at_unix_ms: Option<u64>,
    },
    /// Every observable strict descendant of an ancestor should be removed.
    SubagentDescendantsRemoved { ancestor_id: String },
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
    /// Whether a parent Kit runtime should forward this child event payload.
    pub(crate) fn forward_from_child(&self) -> bool {
        matches!(
            self,
            Self::ChildStarted { .. }
                | Self::ChildFinished { .. }
                | Self::SubagentStateChanged { .. }
                | Self::SubagentDescendantsRemoved { .. }
        )
    }

    /// The ACP tool call this child belongs to, when the id carries one.
    #[must_use]
    pub fn parent_call(&self) -> Option<&str> {
        let call = match self {
            Self::ChildStarted { call, .. } | Self::ChildFinished { call, .. } => call,
            Self::StorageStatus { .. }
            | Self::SessionStarted { .. }
            | Self::CompactionStarted { .. }
            | Self::CompactionFinished { .. }
            | Self::SubagentStateChanged { .. }
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

/// Private, ephemeral stderr envelope. Persisted/replayed `RuntimeEvent` shapes
/// are unchanged. Legacy lines decode without an activation and cannot establish
/// live routing authority; historical replay still reads the unchanged event.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticEvent {
    #[serde(flatten)]
    pub event: RuntimeEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<DiagnosticActivation>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticActivation {
    pub session_id: String,
    pub epoch: u64,
}

pub const ACTIVATION_META_KEY: &str = "kitRuntimeActivation";

#[derive(Default)]
struct DiagnosticRoutes {
    next_epoch: u64,
    sessions: HashMap<String, u64>,
    active: Option<DiagnosticActivation>,
}

fn diagnostic_routes() -> &'static Mutex<DiagnosticRoutes> {
    static ROUTES: OnceLock<Mutex<DiagnosticRoutes>> = OnceLock::new();
    ROUTES.get_or_init(Mutex::default)
}

/// Commit a new activation only after session admission has succeeded. The ACP
/// response returns this exact epoch; stderr alone is never activation authority.
pub fn activate_diagnostics(session_id: &str) -> u64 {
    let mut routes = diagnostic_routes().lock().expect("diagnostic routes lock");
    routes.next_epoch = routes
        .next_epoch
        .checked_add(1)
        .expect("activation epoch overflow");
    let epoch = routes.next_epoch;
    routes.sessions.insert(session_id.to_owned(), epoch);
    routes.active = Some(DiagnosticActivation {
        session_id: session_id.to_owned(),
        epoch,
    });
    write_diagnostic_marker(&routes);
    epoch
}

#[cfg(test)]
pub(crate) fn diagnostic_epoch(session_id: &str) -> Option<u64> {
    diagnostic_routes()
        .lock()
        .expect("diagnostic routes lock")
        .sessions
        .get(session_id)
        .copied()
}

/// Restore an actor's route without minting a new activation on each turn.
pub fn restore_diagnostics(session_id: &str) {
    let mut routes = diagnostic_routes().lock().expect("diagnostic routes lock");
    routes.active = routes
        .sessions
        .get(session_id)
        .map(|epoch| DiagnosticActivation {
            session_id: session_id.to_owned(),
            epoch: *epoch,
        });
    write_diagnostic_marker(&routes);
}

tokio::task_local! {
    static DIAGNOSTIC_ACTIVATION: Option<DiagnosticActivation>;
}

/// Preserve the turn's activation even if another actor restores its route, or
/// a loaded-session resume reactivates this durable ID while the turn is live.
/// Capture synchronously at construction, before the future can be spawned.
pub fn scope_diagnostics<F: std::future::Future>(
    session_id: &str,
    future: F,
) -> impl std::future::Future<Output = F::Output> + use<F> {
    let activation = {
        let routes = diagnostic_routes().lock().expect("diagnostic routes lock");
        routes
            .sessions
            .get(session_id)
            .map(|epoch| DiagnosticActivation {
                session_id: session_id.to_owned(),
                epoch: *epoch,
            })
    };
    DiagnosticScope(activation).scope(future)
}

fn write_diagnostic_marker(routes: &DiagnosticRoutes) {
    if enabled()
        && let Some(activation) = &routes.active
    {
        write_diagnostic(
            &mut std::io::stderr().lock(),
            &DiagnosticEvent {
                event: RuntimeEvent::SessionStarted {
                    session_id: activation.session_id.clone(),
                },
                activation: Some(activation.clone()),
            },
        );
    }
}

/// Immutable producer identity. Capture before constructing a spawned task, not
/// when that task is first polled. Unscoped producers never borrow the active UI
/// route, including after a durable session ID is reactivated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DiagnosticScope(Option<DiagnosticActivation>);

impl DiagnosticScope {
    pub(crate) fn capture() -> Self {
        Self(DIAGNOSTIC_ACTIVATION.try_with(Clone::clone).unwrap_or(None))
    }

    pub(crate) fn scope<F: std::future::Future>(
        &self,
        future: F,
    ) -> impl std::future::Future<Output = F::Output> + use<F> {
        DIAGNOSTIC_ACTIVATION.scope(self.0.clone(), future)
    }

    pub(crate) fn emit(&self, event: &RuntimeEvent) {
        if !enabled() {
            return;
        }
        write_diagnostic(
            &mut std::io::stderr().lock(),
            &DiagnosticEvent {
                event: event.clone(),
                activation: if matches!(event, RuntimeEvent::StorageStatus { .. }) {
                    None
                } else {
                    self.0.clone()
                },
            },
        );
    }
}

/// Inherit the caller's scope synchronously, before the returned future can be
/// moved into another task. Tokio task locals do not propagate through spawn.
pub(crate) fn inherit_diagnostics<F: std::future::Future>(
    future: F,
) -> impl std::future::Future<Output = F::Output> + use<F> {
    DiagnosticScope::capture().scope(future)
}

/// Writes with the originating producer's scope. Truly unscoped events remain
/// unscoped; process-wide StorageStatus is global even inside a session scope.
pub fn emit(event: &RuntimeEvent) {
    DiagnosticScope::capture().emit(event);
}

fn write_diagnostic(writer: &mut impl Write, event: &DiagnosticEvent) {
    if let Ok(line) = serde_json::to_string(event) {
        let _ = writeln!(writer, "{EVENT_MARKER}{line}");
    }
}

#[cfg(test)]
fn write_event(writer: &mut impl Write, event: &RuntimeEvent) {
    write_diagnostic(
        writer,
        &DiagnosticEvent {
            event: event.clone(),
            activation: None,
        },
    );
}

/// Decode both current activation-aware and historical unscoped stderr lines.
#[must_use]
pub fn parse_diagnostic(line: &str) -> Option<DiagnosticEvent> {
    serde_json::from_str(line.strip_prefix(EVENT_MARKER)?).ok()
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
    use std::io::{self, Write};

    use serde_json::json;

    use super::{
        DIAGNOSTIC_ACTIVATION, DiagnosticActivation, DiagnosticEvent, EVENT_MARKER,
        GenerationOutcome, RuntimeEvent, SubagentStatus, activate_diagnostics, parse,
        parse_diagnostic, restore_diagnostics, scope_diagnostics, summarize_input,
        summarize_output, write_diagnostic, write_event,
    };

    #[test]
    fn diagnostic_envelope_preserves_legacy_replay_and_rejects_malformed_identity() {
        let legacy = format!(
            "{EVENT_MARKER}{}",
            r#"{"event":"child_started","call":"p:compose:c","tool":"shell","summary":"ok","at":1}"#
        );
        let event = parse(&legacy).unwrap();
        assert_eq!(parse_diagnostic(&legacy).unwrap().activation, None);
        let current = DiagnosticEvent {
            event: event.clone(),
            activation: Some(DiagnosticActivation {
                session_id: "source".into(),
                epoch: 42,
            }),
        };
        let mut bytes = Vec::new();
        write_diagnostic(&mut bytes, &current);
        let line = String::from_utf8(bytes).unwrap();
        assert_eq!(parse_diagnostic(line.trim_end()), Some(current));
        // Old child-forwarding/replay readers retain exactly their event shape.
        assert_eq!(parse(line.trim_end()), Some(event));
        assert!(line.contains(r#""activation":{"session_id":"source","epoch":42}"#));
        for activation in [
            r#"{"session_id":"source","epoch":"42"}"#,
            r#"{"session_id":"source","epoch":-1}"#,
            r#"{"epoch":42}"#,
        ] {
            let malformed = format!(
                "{EVENT_MARKER}{{\"event\":\"session_started\",\"session_id\":\"source\",\"activation\":{activation}}}"
            );
            assert!(parse_diagnostic(&malformed).is_none());
        }
    }

    #[tokio::test]
    async fn diagnostic_epochs_change_on_reactivation_not_turn_restoration() {
        let session_id = "event-epoch-test";
        let first = activate_diagnostics(session_id);
        restore_diagnostics(session_id);
        scope_diagnostics(session_id, async {
            assert_eq!(
                DIAGNOSTIC_ACTIVATION.with(|activation| activation.as_ref().unwrap().epoch),
                first
            );
            let second = activate_diagnostics(session_id);
            assert!(second > first);
            // Already-running futures keep their original activation identity.
            assert_eq!(
                DIAGNOSTIC_ACTIVATION.with(|activation| activation.as_ref().unwrap().epoch),
                first
            );
            restore_diagnostics(session_id);
            scope_diagnostics(session_id, async {
                assert_eq!(
                    DIAGNOSTIC_ACTIVATION.with(|activation| activation.as_ref().unwrap().epoch),
                    second
                );
            })
            .await;
        })
        .await;
    }

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
